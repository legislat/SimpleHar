//! R11 fit-probe cluster: the warm-pass deferral and session-slack constants, the
//! cached projection, the row and pass plans, the two off-actor probe legs, and the
//! actor-side fit cache, epoch, stamps, and probe lifecycle.
//!
//! The data and probe legs stay pure: they probe off-thread and answer back
//! through the actor channel. The `impl ActorState` block at the end of this
//! module owns the actor's fit cache, epoch, and stamps on the actor's turn.

use crate::engine::EngineRuntime;
use crate::model::ModelEntry;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use super::{ActorMsg, ActorState};

/// R11: the deferral before a warm fit pass starts probing, so the queue never
/// competes with the boot window (first bind, autostart loads). Long enough to
/// stay clear of boot, short enough that the figures are warm before an
/// operator's first click.
pub(super) const FIT_WARM_DELAY: Duration = Duration::from_millis(1500);

/// R11 session cross-check: the slack added to `fit_margin_gb` to form the
/// tolerance a live witness reading must fall inside to confirm a Fit booking
/// for the page. A witness total-diff carries noise the deterministic
/// projection does not (other tenants' sampling instants, KV growth under
/// load), so the cross-check window is wider than the booking margin.
///
/// Calibration decision (TASK-R11-FIT, C1 capture 2026-11-02): margin 3,
/// surcharge 2 and this 2.0 slack STAND - the capture booked conservative
/// against the live witness figure on the single sample the card holds, and
/// any numeric change needs multi-sample card-window evidence first. The C1
/// fact set (rows, constants, figures, headroom) lives in exactly one home:
/// the calibration pin in `src/engine/fit.rs` - cite it, do not restate it.
/// A config knob (`engines.llama.fit_validate_slack_gib`) is DEFERRED: a
/// fifth `engines.llama` key owes the llama row's `config_knobs` claim and
/// the parity-test literal in `src/engine/policy.rs`.
pub(super) const FIT_VALIDATE_SLACK_GIB: f64 = 2.0;

/// One cached R11 fit projection: the booked GiB and the model file's
/// fingerprint at the moment it was computed. The cache is keyed by the probe
/// argv hash (the void identity — a config change is a different key), and a
/// figure survives on a row only while that fingerprint is unproven-changed:
/// both sides known and differing voids it (absence of evidence is never
/// evidence of change, the KEEP doctrine). In-process only — this figure never
/// persists, so a restart re-derives it.
#[derive(Debug, Clone)]
pub(super) struct FitEntry {
    pub(super) gib: f64,
    pub(super) fingerprint: Option<crate::model::FileFingerprint>,
}

/// One row a fit pass projects: the argv identity, the row it belongs to, and
/// what the booking formula needs that argv does not carry (the vision
/// surcharge flag, the model file to fingerprint).
#[derive(Debug, Clone)]
pub(super) struct FitRowPlan {
    pub(super) model: String,
    pub(super) key: u64,
    pub(super) args: Vec<String>,
    pub(super) has_mmproj: bool,
    pub(super) model_file: Option<PathBuf>,
}

/// A whole fit pass resolved once off the actor: the config-sibling tool, the
/// calibration constants, the rows to project.
#[derive(Debug, Clone)]
pub(super) struct FitProbePlan {
    pub(super) program: PathBuf,
    pub(super) constants: crate::engine::fit::FitConstants,
    pub(super) rows: Vec<FitRowPlan>,
}

/// The version leg shared by the warm pass and the admit refresh: report what
/// the calculator says it is, then answer whether rows may run at all — no
/// answer is silence for the whole pass (a tool that will not say its version
/// will not project either; one warn, not a probe per doomed row). A CHANGED
/// answer does not stop the pass: the version message lands before every row
/// message, so rows computed under the new tool are stamped under the new
/// epoch the handler just minted.
pub(super) async fn run_fit_version_leg(
    runtime: &Arc<dyn EngineRuntime>,
    tx: &mpsc::UnboundedSender<ActorMsg>,
    gen: u64,
    plan: &FitProbePlan,
) -> bool {
    // Absence is checked and NAMED here (off the actor, immediately before the
    // first probe), never guessed past: a missing calculator is one warn and
    // the whole ladder stands — no version message, no rows, no silence about
    // why. Async stat: no synchronous fs on the worker either, the
    // FINDINGS2-04 [2] discipline this whole file keeps for its actor turn.
    let tool_present = match tokio::fs::try_exists(&plan.program).await {
        Ok(present) => present,
        Err(error) => {
            tracing::warn!(
                path = %plan.program.display(),
                error = %error,
                "cannot stat the fit tool; the fit pass stays silent and the ladder stands"
            );
            false
        }
    };
    if !tool_present {
        tracing::warn!(
            error = %crate::engine::fit::FitError::ToolMissing {
                path: plan.program.display().to_string(),
            },
            "fit pass skipped"
        );
        return false;
    }
    let answer = runtime
        .run_probe_command(
            &plan.program,
            &crate::engine::fit::version_args(),
            plan.constants.probe_timeout,
        )
        .await;
    let established = answer.is_some();
    let _ = tx.send(ActorMsg::FitVersionDone {
        gen,
        version: answer,
    });
    established
}

/// One row leg: probe, strict-parse, book — a failure answers as the named
/// `Err` the handler warns, never as a number. The fingerprint is taken AFTER
/// the probe answered, so the figure and the file it describes are certified
/// against the same reading (a rewrite during the probe shows as proven-change
/// on the NEXT check, the safe direction).
pub(super) async fn run_fit_row_leg(
    runtime: &Arc<dyn EngineRuntime>,
    plan: &FitProbePlan,
    row: &FitRowPlan,
) -> Result<(f64, Option<crate::model::FileFingerprint>), String> {
    match runtime
        .run_probe_command(&plan.program, &row.args, plan.constants.probe_timeout)
        .await
    {
        Some(stdout) => {
            crate::engine::fit::parse_and_book(&stdout, row.has_mmproj, &plan.constants)
                .map(|gib| {
                    (
                        gib,
                        row.model_file
                            .as_deref()
                            .and_then(crate::model::fingerprint),
                    )
                })
                .map_err(|error| error.to_string())
        }
        None => Err("the probe did not answer".to_string()),
    }
}

impl ActorState {
    /// The current fit epoch: a probe's answer counts only while its stamped
    /// epoch still stands.
    #[must_use]
    pub(super) fn fit_gen(&self) -> u64 {
        self.fit_epoch.load(Ordering::Relaxed)
    }

    /// The probe identity of one catalog row, recomputed from the row itself: a
    /// fit result only patches the row whose OWN argv still hashes to the key
    /// the probe ran with (a rescan that edited the yaml mid-probe is not that
    /// row anymore).
    #[must_use]
    pub(super) fn fit_row_key(entry: &ModelEntry) -> Option<u64> {
        crate::engine::fit::probe_args(&entry.raw)
            .ok()
            .map(|args| crate::engine::fit::args_hash(&args))
    }

    /// The model file a fit row projects — the `model:` GGUF whose fingerprint
    /// guards the cache. `None` when the row has none (a `scheme://` sentinel
    /// row has nothing local to have changed).
    #[must_use]
    fn fit_model_file(entry: &ModelEntry) -> Option<PathBuf> {
        entry
            .raw
            .get("model")
            .map(|v| v.trim())
            .filter(|v| !v.is_empty() && !v.contains("://"))
            .map(PathBuf::from)
    }

    /// R11 warm fit pass (boot scan and every rescan alike): re-stamp the epoch
    /// so probes born under a superseded catalog drop at the handler, then —
    /// only with the probe switched on — project every fit-capable row,
    /// serially and deferred, off-thread. Like R5's reconcile there is no I/O
    /// in the actor loop and nothing here can gate, kill or invent: the answer
    /// lands as a catalog patch through [`ActorState::on_fit_row_done`] or the
    /// row keeps its ladder.
    pub(super) fn spawn_fit_warm(&mut self) {
        self.fit_epoch.fetch_add(1, Ordering::Relaxed);
        // R11: the rescan's epoch retires every session cross-check stamp — a
        // green the page showed described the superseded catalog. Clearing keeps
        // the map bounded; the epoch equality is what makes stale entries read
        // as absent even before this line runs.
        self.fit_stamps.clear();
        let Some(plan) = self.fit_probe_plan() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return; // sync test constructors: live-actor work has nothing to do
        };
        let gen = self.fit_gen();
        let epoch = Arc::clone(&self.fit_epoch);
        let runtime = Arc::clone(&self.runtime);
        let tx = self.tx.clone();
        handle.spawn(async move {
            // Deferred, serial, cancellable-at-the-row-boundary: a projection
            // competes with nothing at boot, and a rescan mid-pass makes every
            // remaining row of THIS pass stale before its probe (the epoch
            // moved), while answers already sent are dropped by the handler's
            // own epoch guard.
            tokio::time::sleep(FIT_WARM_DELAY).await;
            if !run_fit_version_leg(&runtime, &tx, gen, &plan).await {
                return;
            }
            for row in plan.rows.iter() {
                if epoch.load(Ordering::Relaxed) != gen || tx.is_closed() {
                    return;
                }
                let outcome = run_fit_row_leg(&runtime, &plan, row).await;
                let _ = tx.send(ActorMsg::FitRowDone {
                    gen,
                    model: row.model.clone(),
                    key: row.key,
                    outcome,
                });
            }
        });
    }

    /// Resolves the warm plan, or `None` (with the reason warned once per
    /// catalog application) when the pass must stay silent: probe off is total
    /// silence — zero probes, ladder unchanged — while an uncalibrated
    /// constant is a named warn, never a silent zero pretending to be a
    /// calibration.
    pub(super) fn fit_probe_plan(&self) -> Option<FitProbePlan> {
        let llama = self.cfg.llama_fit()?;
        if !self.cfg.llama_fit_probe_enabled() {
            return None;
        }
        let constants = match crate::engine::fit::FitConstants::resolve(
            llama.fit_margin_gb,
            llama.mmproj_surcharge_gb,
            llama.fit_probe_timeout_s,
        ) {
            Ok(constants) => constants,
            Err(error) => {
                tracing::warn!(error = %error, "fit probe is on but cannot book: the rows keep their ladder");
                return None;
            }
        };
        let rows: Vec<FitRowPlan> = self
            .catalog
            .iter()
            .filter(|entry| crate::engine::policy::fit_capable(&entry.engine))
            .filter_map(|entry| {
                let args = match crate::engine::fit::probe_args(&entry.raw) {
                    Ok(args) => args,
                    Err(error) => {
                        tracing::warn!(model = %entry.name, error = %error, "fit row is not probeable");
                        return None;
                    }
                };
                let key = crate::engine::fit::args_hash(&args);
                Some(FitRowPlan {
                    model: entry.name.clone(),
                    key,
                    args,
                    has_mmproj: entry
                        .raw
                        .get("mmproj")
                        .is_some_and(|v| !v.trim().is_empty()),
                    model_file: Self::fit_model_file(entry),
                })
            })
            .collect();
        if rows.is_empty() {
            return None;
        }
        Some(FitProbePlan {
            program: crate::engine::fit::resolve_tool(&self.cfg.llama_server_exe),
            constants,
            rows,
        })
    }

    /// The R11 admit-time fit refresh, run on the actor BEFORE the booking
    /// decision. A cache hit (same argv, same tool epoch, model file not
    /// proven-changed) books the projection for this load AND still rides the
    /// version check fire-and-forget; a miss books the ladder now and fires
    /// the probe with the version leg first. Either way a replaced calculator
    /// is caught at every launch, not only at boot — the ruling words are
    /// "version checked at boot AND launch". The refresh never parks the
    /// admit: the physical gate is unchanged, and the next admit (and the
    /// page) get the figure.
    pub(super) fn refresh_fit_on_admit(&mut self, entry: &mut ModelEntry) {
        if !crate::engine::policy::fit_capable(&entry.engine) || !self.cfg.llama_fit_probe_enabled()
        {
            return;
        }
        let Some(key) = Self::fit_row_key(entry) else {
            return;
        };
        let fingerprint = Self::fit_model_file(entry)
            .as_deref()
            .and_then(crate::model::fingerprint);
        let hit = self
            .fit_cache
            .get(&key)
            .filter(|cell| match (&cell.fingerprint, &fingerprint) {
                (Some(have), Some(now)) => have == now,
                // One side unstat'able is the absence of EVIDENCE: the figure
                // stands (KEEP doctrine), not the row punished for a sleeping
                // volume.
                _ => true,
            })
            .map(|cell| cell.gib);
        if let Some(gib) = hit {
            entry.fit_gb = Some(gib);
            self.patch_catalog_fit(&entry.name, key, gib);
            // A booked hit still asks the tool who it is: an admit may not
            // park behind a process spawn, so the check is fire-and-forget.
            // A replaced calculator voids the cache and strips the row figures
            // the moment its answer lands, and the NEXT admit is a miss
            // (ladder); this admit books the figure it had already certified.
            // Nothing a mismatch could leave behind persists, and the physical
            // gate remains the witness.
            if let Some(plan) = self.fit_probe_plan() {
                let gen = self.fit_gen();
                let runtime = Arc::clone(&self.runtime);
                let tx = self.tx.clone();
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        run_fit_version_leg(&runtime, &tx, gen, &plan).await;
                    });
                }
            }
            return;
        }
        // The stale figure, if any, is dropped for THIS booking: the ladder —
        // declared > measured > estimate — books a projection it cannot certify.
        entry.fit_gb = None;
        let Some(plan) = self.fit_probe_plan() else {
            return;
        };
        let Some(row) = plan.rows.iter().find(|row| row.key == key).cloned() else {
            return;
        };
        let gen = self.fit_gen();
        let runtime = Arc::clone(&self.runtime);
        let tx = self.tx.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            if !run_fit_version_leg(&runtime, &tx, gen, &plan).await {
                return;
            }
            let outcome = run_fit_row_leg(&runtime, &plan, &row).await;
            let _ = tx.send(ActorMsg::FitRowDone {
                gen,
                model: row.model,
                key: row.key,
                outcome,
            });
        });
    }

    /// The answer leg of the version probe: a moved epoch voids the whole cache
    /// and strips every row's figure (a projection belongs to the calculator
    /// that minted it); no answer is silence about THIS pass only — the
    /// established epoch and its figures stand (a momentarily absent tool is
    /// no evidence the tool changed).
    pub(super) fn on_fit_version(&mut self, gen: u64, version: Option<String>) {
        if gen != self.fit_gen() {
            return;
        }
        let Some(version) = version else {
            tracing::warn!(
                "llama-fit-params did not answer --version: this fit pass stays silent, \
                 the rows keep their last certified state"
            );
            return;
        };
        if self.fit_version.as_deref() == Some(version.as_str()) {
            return;
        }
        if self.fit_version.is_some() {
            tracing::warn!(
                previous = %self.fit_version.as_deref().unwrap_or_default(),
                version = %version,
                "the fit calculator changed under us: every cached projection is void \
                 (a figure belongs to the tool that minted it)"
            );
        }
        self.fit_version = Some(version);
        self.fit_cache.clear();
        // R11: a replaced calculator voids the session cross-checks too — the
        // tolerance's own margin belongs to the tool that minted the projection,
        // so a verdict earned under the old one says nothing about the new.
        self.fit_stamps.clear();
        for entry in &mut self.catalog {
            if crate::engine::policy::fit_capable(&entry.engine) {
                entry.fit_gb = None;
            }
        }
        self.publish();
    }

    /// The answer leg of a row probe: Ok lands in the cache and patches the
    /// catalog row that still carries the probed identity, Err is a named warn
    /// and the ladder. A stale epoch's answer is dropped at the door — the
    /// superseded catalog is not the catalog this answer describes.
    pub(super) fn on_fit_row_done(
        &mut self,
        gen: u64,
        model: &str,
        key: u64,
        outcome: Result<(f64, Option<crate::model::FileFingerprint>), String>,
    ) {
        if gen != self.fit_gen() {
            return;
        }
        let Ok((gib, fingerprint)) = outcome else {
            tracing::warn!(
                model,
                error = %outcome.expect_err("checked non-Ok"),
                "fit projection refused: the row keeps its declared/estimate ladder"
            );
            return;
        };
        self.fit_cache.insert(key, FitEntry { gib, fingerprint });
        self.patch_catalog_fit(model, key, gib);
    }

    /// Patches the catalog row the probed identity still describes, then
    /// republishes so the page's Cost column sees the same figure the gate
    /// would book (one derivation — the duplication-review rule).
    fn patch_catalog_fit(&mut self, model: &str, key: u64, gib: f64) {
        let Some(entry) = self
            .catalog
            .iter_mut()
            .find(|m| m.name == model && crate::engine::policy::fit_capable(&m.engine))
        else {
            return;
        };
        if Self::fit_row_key(entry) != Some(key) {
            return;
        }
        if entry.fit_gb == Some(gib) {
            return;
        }
        entry.fit_gb = Some(gib);
        self.publish();
    }
}
