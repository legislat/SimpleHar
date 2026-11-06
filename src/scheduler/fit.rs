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

use super::measured::MeasuredAnchor;
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
/// evidence of change, the KEEP doctrine). The "now" side of that compare is
/// the scan-carried value (FR-02) — a map read, never an actor-turn stat.
/// In-process only — this figure never persists, so a restart re-derives it.
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
    /// row has nothing local to have changed). The which-row-has-a-guard-file
    /// rule is [`ModelEntry::fit_model_key`], shared with the scan's
    /// fingerprint carrier so record and lookup can never disagree.
    #[must_use]
    fn fit_model_file(entry: &ModelEntry) -> Option<PathBuf> {
        entry.fit_model_key().map(PathBuf::from)
    }

    /// FR-02: the fingerprint carried for this row by the scan (or refreshed
    /// by a landed fit probe) — the value the cache guard compares against
    /// WITHOUT touching the filesystem: the admit turn reads a map, never a
    /// stat. `None` is the KEEP doctrine's absence of evidence (synthetic row,
    /// sentinel, or a scan that could not read the bytes), never evidence of
    /// change. A rescan refreshes the value from its existence-trace stat; a
    /// probe answer refreshes it off-actor.
    #[must_use]
    fn fit_carried_fingerprint(entry: &ModelEntry) -> Option<crate::model::FileFingerprint> {
        let yaml = entry.file.as_deref()?;
        let raw = entry.fit_model_key()?;
        crate::model::scan_fingerprint(yaml, raw)
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
    ///
    /// FR-02: the "not proven-changed" question is answered from the
    /// scan-carried fingerprint ([`Self::fit_carried_fingerprint`], a map
    /// read) — the actor turn makes NO filesystem stat. The change that voids
    /// the cache is a fresh fingerprint observation disagreeing with the
    /// cached one: a rescan (its existence-trace stat) or a landed probe
    /// refreshes the carried value, so a rewritten file is caught at the next
    /// rescan or the next projection, and an unrescanned silent rewrite keeps
    /// its figure exactly as the KEEP doctrine keeps everything unobserved —
    /// the physical gate remains the witness either way.
    pub(super) fn refresh_fit_on_admit(&mut self, entry: &mut ModelEntry) {
        if !crate::engine::policy::fit_capable(&entry.engine) || !self.cfg.llama_fit_probe_enabled()
        {
            return;
        }
        let Some(key) = Self::fit_row_key(entry) else {
            return;
        };
        let fingerprint = Self::fit_carried_fingerprint(entry);
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
    ///
    /// FR-02: the probe's fingerprint was taken off-actor after the answer, so
    /// it is the freshest reading of the row's model file — it refreshes the
    /// scan-carried value for that identity too, keeping cache and carried
    /// value in step after a change (without this, the next admit would miss
    /// forever on the stale scan observation until a rescan).
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
        if let (Some(fingerprint), Some(entry)) = (
            fingerprint,
            self.catalog.iter().find(|m| {
                m.name == model
                    && crate::engine::policy::fit_capable(&m.engine)
                    && Self::fit_row_key(m) == Some(key)
            }),
        ) {
            if let (Some(yaml), Some(raw)) = (entry.file.as_deref(), entry.fit_model_key()) {
                crate::model::note_scan_fingerprint(yaml, raw, fingerprint);
            }
        }
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

    /// The session cross-check's tolerance: `fit_margin_gb` plus
    /// [`FIT_VALIDATE_SLACK_GIB`]. An absent or non-finite margin means NO
    /// defined tolerance ([`None`] - the row never validates, silently, the
    /// same degradation an unavailable reading gets). Split out of
    /// [`Self::maybe_validate_fit`] so the composition carries a direct test
    /// pin (TASK-R11-FIT: tolerance = margin + slack, default slack 2.0).
    pub(super) fn fit_validation_tolerance(margin_gb: Option<f64>) -> Option<f64> {
        margin_gb
            .filter(|margin| margin.is_finite())
            .map(|margin| margin + FIT_VALIDATE_SLACK_GIB)
    }

    /// R11 session cross-check (rule 2026-11-01, display-only, never a gate
    /// input): when a witness reading has survived every fold gate for a
    /// Fit-booked load, compare it against the projection that booked the
    /// spawn. The tolerance is [`Self::fit_validation_tolerance`] -
    /// `fit_margin_gb` + [`FIT_VALIDATE_SLACK_GIB`]; an uncalibrated margin
    /// (constant absent) is NO defined tolerance, hence never-validated,
    /// degrading silently like an unavailable reading. A hit
    /// stamps the model under the current fit epoch; the stamp is in-memory
    /// only, retires with the epoch (a rescan re-stamps it away), and a
    /// calculator swap clears the map. The first refusal at an epoch warns
    /// once — the numbers the operator needs if the projection was wrong —
    /// and a later reading that lands inside tolerance still stamps. Returns
    /// whether the verdict map changed, so the caller publishes only when the
    /// page gained something to read.
    pub(super) fn maybe_validate_fit(
        &mut self,
        model: &str,
        anchor: &MeasuredAnchor,
        observed: f64,
    ) -> bool {
        let Some(fit) = anchor.fit_gb else {
            return false;
        };
        let Some(tolerance) = Self::fit_validation_tolerance(
            self.cfg.llama_fit().and_then(|llama| llama.fit_margin_gb),
        ) else {
            return false;
        };
        let gen = self.fit_gen();
        let seen = self.fit_stamps.get(model).copied();
        if seen == Some((gen, true)) {
            return false;
        }
        if (observed - fit).abs() > tolerance {
            if seen == Some((gen, false)) {
                return false;
            }
            tracing::warn!(
                model,
                observed,
                fit_gb = fit,
                tolerance,
                "witness cross-check kept the Fit row amber: the live reading sits \
                 outside the projection's session tolerance"
            );
            self.fit_stamps.insert(model.to_string(), (gen, false));
            return true;
        }
        if seen == Some((gen, false)) {
            tracing::warn!(
                model,
                observed,
                fit_gb = fit,
                "a later witness reading fell back inside the Fit projection's session \
                 tolerance: the row turns green for the session"
            );
        }
        self.fit_stamps.insert(model.to_string(), (gen, true));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{ActorState, FitEntry};
    use crate::engine::EngineRuntime;
    use crate::model::{self, FileFingerprint, ModelEntry};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::{Duration, UNIX_EPOCH};

    /// The fit row the pins book from: probeable keys, its yaml on disk, and
    /// `file` set the way `scan_dir` sets it. Unique names per test keep the
    /// scan-fingerprint table (a process-wide carrier) collision-free while
    /// the suite runs in parallel.
    fn fit_row(dir: &std::path::Path, name: &str) -> ModelEntry {
        let yaml = dir.join(format!("{name}.yaml"));
        std::fs::write(
            &yaml,
            format!("engine: llama\nvram_gb: 24\nmodel: {name}.gguf\ncontext: 4096\nngl: 99\n"),
        )
        .expect("seed yaml");
        let text = std::fs::read_to_string(&yaml).expect("readable");
        let (map, problems) = crate::model::scan::parse_flat_yaml(&text);
        assert!(problems.is_empty(), "{problems:?}");
        let mut entry = ModelEntry::from_flat(&map, name).expect("valid row");
        entry.file = Some(yaml);
        entry
    }

    fn fp(size: u64) -> FileFingerprint {
        FileFingerprint {
            mtime: UNIX_EPOCH + Duration::from_secs(1),
            size,
        }
    }

    /// State with the probe switched on and the calibration left unset: the
    /// refresh gate opens (probe enabled) while `fit_probe_plan` resolves to
    /// `None`, so the pins drive the cache decision without any spawn.
    fn fit_probe_state() -> ActorState {
        let mut adapters: BTreeMap<String, Arc<dyn crate::engine::EngineAdapter>> = BTreeMap::new();
        adapters.insert(
            "llama".to_string(),
            Arc::new(crate::engine::mock::MockAdapter) as Arc<dyn crate::engine::EngineAdapter>,
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ActorState::empty(
            crate::config::AppConfig::parse(r#"{"engines": {"llama": {"fit_probe": true}}}"#)
                .expect("probe on, margin unset"),
            crate::testutil::temp_dir("fit-fr02-state"),
            adapters,
            Arc::new(SilentRuntime) as Arc<dyn EngineRuntime>,
            Arc::new(crate::budget::LedgerOnlyWitness),
            tx,
        )
    }

    struct SilentRuntime;

    #[async_trait::async_trait]
    impl EngineRuntime for SilentRuntime {
        async fn spawn(
            &self,
            _spec: &crate::engine::LaunchSpec,
            _ticket: crate::ownership::SpawnTicket,
        ) -> Result<crate::engine::Spawned, crate::engine::SpawnError> {
            panic!("the FR-02 pins never spawn")
        }
        async fn kill(
            &self,
            _ticket: &crate::ownership::SpawnTicket,
        ) -> Result<(), crate::engine::SpawnError> {
            panic!("the FR-02 pins never kill")
        }
        async fn release_confirmed(
            &self,
            _ticket: &crate::ownership::SpawnTicket,
        ) -> Result<bool, crate::engine::SpawnError> {
            panic!("the FR-02 pins never reach release evidence")
        }
        async fn probe(
            &self,
            _port: u16,
            _readiness: &crate::engine::ReadinessSpec,
        ) -> crate::engine::Probe {
            panic!("the FR-02 pins never reach readiness")
        }
        async fn discover_model_id(&self, _port: u16) -> Option<String> {
            panic!("the FR-02 pins never reach readiness")
        }
    }

    /// FR-02 counting probe: the admit-turn cache hit books the certified
    /// figure and the current thread's sync-stat counter stays exactly where
    /// the reset left it — zero. Falsifier: restore the pre-fix
    /// `fingerprint(fit_model_file(entry))` on the refresh and the counter
    /// reads one, turning this pin red.
    #[test]
    fn the_admit_turn_hit_books_the_projection_without_one_single_stat() {
        let dir = crate::testutil::temp_dir("fit-fr02-hit");
        let mut state = fit_probe_state();
        let mut entry = fit_row(&dir, "fr02hit-row");
        let yaml = entry.file.clone().expect("seeded");
        let raw = entry.fit_model_key().expect("a local model").to_string();
        let carried = fp(1024);
        model::note_scan_fingerprint(&yaml, &raw, carried);
        let key = ActorState::fit_row_key(&entry).expect("probeable row");
        state.catalog = vec![entry.clone()];
        state.fit_cache.insert(
            key,
            FitEntry {
                gib: 30.0,
                fingerprint: Some(carried),
            },
        );
        entry.fit_gb = None;
        model::reset_fingerprint_stat_calls();
        state.refresh_fit_on_admit(&mut entry);
        assert_eq!(
            model::fingerprint_stat_calls(),
            0,
            "FR-02: the actor turn must not stat the model file"
        );
        assert_eq!(
            entry.fit_gb,
            Some(30.0),
            "the certified figure still books the load"
        );
        assert_eq!(
            state.catalog[0].fit_gb,
            Some(30.0),
            "the page row keeps the same figure the gate booked"
        );
    }

    /// FR-02 invalidation, both legs: a rescan that re-read the model file
    /// leaves a carried fingerprint differing from the cached one, and the
    /// next admit misses to the ladder; an unchanged carried value keeps the
    /// hit. Neither leg stats. Falsifiers: drop the fingerprint compare from
    /// the hit filter and the void leg keeps booking; treat an absent side as
    /// change and the keep leg drops to the ladder.
    #[test]
    fn a_changed_fingerprint_voids_the_cache_and_an_unchanged_one_keeps_it() {
        let dir = crate::testutil::temp_dir("fit-fr02-void");
        let mut state = fit_probe_state();
        model::reset_fingerprint_stat_calls();

        let mut kept = fit_row(&dir, "fr02keep-row");
        let kept_yaml = kept.file.clone().expect("seeded");
        let kept_raw = kept.fit_model_key().expect("a local model").to_string();
        let key = ActorState::fit_row_key(&kept).expect("probeable row");
        state.catalog = vec![kept.clone()];
        state.fit_cache.insert(
            key,
            FitEntry {
                gib: 30.0,
                fingerprint: Some(fp(1024)),
            },
        );
        model::note_scan_fingerprint(&kept_yaml, &kept_raw, fp(1024));
        kept.fit_gb = None;
        state.refresh_fit_on_admit(&mut kept);
        assert_eq!(
            kept.fit_gb,
            Some(30.0),
            "an unchanged file keeps the certified figure"
        );

        let mut changed = fit_row(&dir, "fr02void-row");
        let changed_yaml = changed.file.clone().expect("seeded");
        let changed_raw = changed.fit_model_key().expect("a local model").to_string();
        let key = ActorState::fit_row_key(&changed).expect("probeable row");
        state.catalog = vec![changed.clone()];
        state.fit_cache.insert(
            key,
            FitEntry {
                gib: 30.0,
                fingerprint: Some(fp(1024)),
            },
        );
        model::note_scan_fingerprint(&changed_yaml, &changed_raw, fp(1024));
        // The rescan re-read the bytes: the carried value moves, the cached
        // figure was certified against the old ones — proven-changed, void.
        model::note_scan_fingerprint(&changed_yaml, &changed_raw, fp(2048));
        changed.fit_gb = Some(30.0);
        state.refresh_fit_on_admit(&mut changed);
        assert_eq!(
            changed.fit_gb, None,
            "a changed model file must not book a projection it cannot certify"
        );
        assert_eq!(
            model::fingerprint_stat_calls(),
            0,
            "both invalidation legs are map reads, never stats"
        );
    }

    /// FR-02 refresh: a landed probe carries its own off-actor fingerprint and
    /// updates the carried value for that identity, so the next admit hits on
    /// the fresh pair instead of missing forever against the stale scan
    /// reading. Falsifier: drop the carrier update in `on_fit_row_done` and
    /// the second admit misses (stale carried value) — pin red.
    #[test]
    fn a_landed_probe_refreshes_the_carried_value_so_the_next_hit_holds() {
        let dir = crate::testutil::temp_dir("fit-fr02-probe");
        let mut state = fit_probe_state();
        let mut entry = fit_row(&dir, "fr02probe-row");
        let yaml = entry.file.clone().expect("seeded");
        let raw = entry.fit_model_key().expect("a local model").to_string();
        // File rewritten after the scan: carried moved to B while no figure
        // was ever certified for B (cold cache) — a probe is the only way.
        model::note_scan_fingerprint(&yaml, &raw, fp(2048));
        let key = ActorState::fit_row_key(&entry).expect("probeable row");
        state.catalog = vec![entry.clone()];
        model::reset_fingerprint_stat_calls();
        entry.fit_gb = Some(9.0);
        state.refresh_fit_on_admit(&mut entry);
        assert_eq!(entry.fit_gb, None, "cold cache: the ladder books");
        state.on_fit_row_done(
            state.fit_gen(),
            "fr02probe-row",
            key,
            Ok((31.0, Some(fp(2048)))),
        );
        assert_eq!(
            model::scan_fingerprint(&yaml, &raw),
            Some(fp(2048)),
            "the probe's reading is the freshest carried value"
        );
        entry.fit_gb = None;
        state.refresh_fit_on_admit(&mut entry);
        assert_eq!(
            entry.fit_gb,
            Some(31.0),
            "cache and carried value agree again: the next admit hits"
        );
        assert_eq!(
            model::fingerprint_stat_calls(),
            0,
            "the whole loop never stats on this thread"
        );
    }

    /// FR-02 KEEP doctrine survives the move: a row whose carried value cannot
    /// be known (no `file:` — a synthetic entry) is never punished for it:
    /// one side unknown is absence of evidence and the figure stands.
    #[test]
    fn an_unknown_carried_fingerprint_is_absence_never_change() {
        let dir = crate::testutil::temp_dir("fit-fr02-keep");
        let mut state = fit_probe_state();
        let mut entry = fit_row(&dir, "fr02keep-none-row");
        entry.file = None; // synthetic: nothing to carry
        let key = ActorState::fit_row_key(&entry).expect("probeable row");
        state.catalog = vec![entry.clone()];
        state.fit_cache.insert(
            key,
            FitEntry {
                gib: 28.0,
                fingerprint: Some(fp(1024)),
            },
        );
        entry.fit_gb = None;
        model::reset_fingerprint_stat_calls();
        state.refresh_fit_on_admit(&mut entry);
        assert_eq!(
            entry.fit_gb,
            Some(28.0),
            "absence of evidence must not retire a certified figure"
        );
        assert_eq!(model::fingerprint_stat_calls(), 0, "no stat either");
    }

    /// FR-02 structural tripwire: the admit refresh's own body holds no
    /// filesystem call. The counting probe catches `model::fingerprint`; this
    /// leg catches a reintroduction through any other sync-fs door.
    /// Falsifier: put any of the forbidden calls back into the body.
    #[test]
    fn the_admit_refresh_body_holds_no_filesystem_call() {
        let source = include_str!("fit.rs");
        let body = source
            .split("fn refresh_fit_on_admit(")
            .nth(1)
            .expect("the refresh stays")
            .split("\n    }\n")
            .next()
            .expect("the refresh body closes");
        for forbidden in [
            "model::fingerprint(",
            "std::fs",
            "fs::metadata",
            "try_exists",
            "symlink_metadata",
            "read_dir",
        ] {
            assert!(
                !body.contains(forbidden),
                "FR-02: the admit turn must not touch the filesystem: {forbidden}"
            );
        }
    }
}
