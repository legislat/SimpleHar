//! Actor-side catalog and reload lifecycle: the off-thread rescan and its apply
//! turn, the drift rounds that judge only gathered evidence, the parked-row
//! helpers, the queued-load retirement, the reload-waiter drain, the harness
//! write-back re-baseline, and the R5 report-only unclaimed-container reconcile.
//! The state and its fields stay owned by `mod.rs`.

use crate::model::scan::{Catalog, ScanError, ScanKind};
use crate::model::ModelEntry;
use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use super::{
    scan_with_binary_check, ActorState, DriftStats, JobId, JobKind, ReconcileRelease, Reply,
    SchedulerError, SwapState,
};

impl ActorState {
    /// Rescans `ModelConf` and cross-checks `default_model`s (§0.2 step 6, Step 9)
    /// off the actor's own thread: the whole synchronous filesystem walk
    /// (`read_dir`, one read per yaml, one stat per model) runs on a blocking
    /// thread so the single writer never parks on disk (FINDINGS-13 [6]). The
    /// caller applies the result with [`ActorState::apply_catalog`] — at boot
    /// before the first message, on reload when the result returns as
    /// [`super::ActorMsg::CatalogLoaded`].
    pub(super) async fn scan_blocking(&self) -> (Result<Catalog, String>, Option<ScanError>) {
        scan_with_binary_check(self.models_dir.clone(), self.cfg.llama_server_exe.clone()).await
    }

    /// Applies a scanned catalog with the single-writer wholesale replace, adding
    /// the actor-side cross-checks the scan cannot see: every scan error gets a
    /// `tracing::warn!` (the scan module itself stays log-free — lib purity),
    /// entries naming an engine with no adapter are **dropped** and reported (the
    /// audit for that verdict shipped with this batch: no fixture publishes an
    /// engine its adapter map lacks, and a load for one could only ever die later
    /// as `no adapter registered`, so early refusal is strictly better), the
    /// configured `llama-server.exe` verdict (gathered off-thread by
    /// [`ActorState::scan_blocking`], never by a stat in this function —
    /// FINDINGS2-04 [2]) is carried into the report, every `default_model` must
    /// resolve, and the drift rounds ([`ActorState::invalidate_queued_loads`],
    /// [`ActorState::refresh_stale_markers`]) judge only the `drift` evidence
    /// gathered off-thread on the reload's blocking legs (FINDINGS-07 [8]) — this
    /// turn performs zero filesystem calls.
    pub(super) fn apply_catalog(
        &mut self,
        catalog: Catalog,
        binary: Option<ScanError>,
        drift: &DriftStats,
    ) {
        let mut errors = catalog.errors;
        let known: Vec<&str> = self.adapters.keys().map(String::as_str).collect();
        let models: Vec<ModelEntry> = catalog
            .models
            .into_iter()
            .filter(|entry| {
                if self.adapters.contains_key(&entry.engine) {
                    return true;
                }
                errors.push(ScanError {
                    kind: ScanKind::EngineUnavailable,
                    file: entry
                        .file
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| format!("<synthetic `{}`>", entry.name)),
                    reason: format!(
                        "engine \"{}\" is not available to this harness (known: {})",
                        entry.engine,
                        known.join(", ")
                    ),
                });
                false
            })
            .collect();
        for category in &self.cfg.categories {
            if let Some(default) = &category.default_model {
                if !models.iter().any(|m| &m.name == default) {
                    errors.push(ScanError {
                        kind: ScanKind::Other,
                        file: "config".to_string(),
                        reason: format!(
                            "default_model `{default}` for {} is not in the catalog",
                            category.id
                        ),
                    });
                }
            }
        }
        // The rescan twin of the R10 arm-time refusal (`start_queued_load`): the
        // page names a borrowed port number before the load dies, never after.
        for entry in &models {
            if let Some(port) = crate::engine::ninfer::declared_internal_port(entry) {
                if let Some(claim) = self.cfg.port_claim(port) {
                    errors.push(ScanError {
                        kind: ScanKind::Other,
                        file: entry
                            .file
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| format!("<synthetic `{}`>", entry.name)),
                        reason: format!(
                            "`ninfer_internal_port` {port} claims {claim}: a container's \
                             internal port must live in its own namespace (loads are refused)"
                        ),
                    });
                }
            }
        }
        // Page-visible twin of the boot banner's warn (`main.rs`): the banner says
        // it once, this says it on every scan, so a binary configured away mid-day
        // is visible on `/har/status` rather than only in the boot log.
        if let Some(missing) = binary {
            errors.push(missing);
        }
        for error in &errors {
            tracing::warn!(
                kind = ?error.kind,
                file = %error.file,
                reason = %error.reason,
                "catalog scan error"
            );
        }
        tracing::info!(
            models = models.len(),
            errors = errors.len(),
            "catalog scanned"
        );
        self.catalog = models;
        self.catalog_errors = errors;
        self.invalidate_queued_loads(drift);
        self.refresh_stale_markers(drift);
        // R5 report-only reconcile (ruling 9, 2026-11-01): every catalog
        // application — the boot scan and every rescan alike — re-checks which
        // declared container names are running with no claim. Off-thread like
        // every other engine touch; report-only by construction.
        self.spawn_unclaimed_reconcile();
        // R11 warm fit pass: the same per-application rhythm projects the
        // fit-capable rows (epoch-stamped, so the superseded catalog's probes
        // drop on arrival). Deferred and off-thread like everything that
        // touches a volume; a no-op when the probe is off.
        self.spawn_fit_warm();
    }

    /// The resident half of reload (Axis 2 = B, operator ruling 2026-10-29): on the
    /// reload turn — and only here, at install, and on a harness write-back — the
    /// `stale_config` verdict is recomputed for every resident: the yaml it launched
    /// from, re-stat'ed **off-thread** by [`super::gather_drift_stats`] on the reload's
    /// blocking legs, against the install-time capture. The status/SSE path reads
    /// this cache and performs zero fs syscalls (FINDINGS2-04 [2]'s one-stat-per-event
    /// rule extended to the marker; pinned by the api-side hidden-file counter-probe,
    /// and the reload turn's own stat-free shape by
    /// `the_drift_rounds_judge_only_pre_gathered_evidence` — FINDINGS-07 [8]).
    ///
    /// Verdict rules, stated plainly:
    /// - no capture (synthetic row): never accuses; absence of evidence is not
    ///   evidence of change (the queued rows' KEEP posture).
    /// - capture path not in the gathered map (the resident or its capture moved
    ///   after the reload was dispatched): this round observed nothing of it, and
    ///   an unobserved verdict is kept, never computed from a missing stat.
    /// - capture gathered, current stat `None` (file deleted/volume ejected since
    ///   launch): accuses — the file demonstrably is not what it was.
    /// - capture gathered, stat differs (either leg): accuses. The comparison is
    ///   against the SPAWN capture, so re-saving byte-identical content does not
    ///   clear the verdict (the mtime moved) — honest conservatism: the operator's
    ///   gesture that matters is unload + reload, and a respawned engine re-captures
    ///   cleanly.
    ///
    /// The yaml capture is the whole verdict: no engine ships a harness-tracked
    /// profile file any more (ruling 2026-11-02 row 19 made strata's profile
    /// harness-rendered L3 bytes, and ninfer's is the cksum-verified staged
    /// script — both drift-proof by construction), so `queue_fingerprint` →
    /// `spawn_config` is the entire `stale_config` story (FINDINGS-25). Only the
    /// yaml re-baselines on a harness write-back; a resident with no capture is
    /// never accused.
    pub(super) fn refresh_stale_markers(&mut self, drift: &DriftStats) {
        for slot in &mut self.slots {
            let Some(occupant) = slot.occupant.as_mut() else {
                continue;
            };
            let Some((file, then)) = occupant.spawn_config.as_ref() else {
                continue;
            };
            // The resident was judged only from gathered evidence; a path absent
            // from the map was never observed for this round (state moved after
            // dispatch) — an unobserved verdict is kept, never computed from a
            // missing stat.
            let Some(now) = drift.stats.get(file.as_path()) else {
                continue;
            };
            occupant.stale_config = *now != Some(*then);
        }
    }

    /// Re-baselines the spawn-side capture after a **harness-owned** write to a
    /// resident's own yaml (Axis-2 interlock 5: the measured fold and the API's flag
    /// edit must never amber their own model). The legitimate mutators are a CLOSED
    /// set — the measured persist (`measured.rs`'s fold success path) and
    /// `har_config_model`'s rewrite (api, note sent before its reload) — and the
    /// closure is pinned by `the_writeback_mutator_set_stays_closed_to_the_rebaseline_table`
    /// (FINDINGS-BATCH-2026-10-28P [P-4]): a new writer must register here with its
    /// note, or the amber accuses the harness of its own edit.
    ///
    /// A verdict already `true` is never erased: only a clean resident re-baselines.
    /// Known accepted edge (mirrors 28O [O-7]'s over-retirement in the opposite
    /// direction): an operator edit that was never seen by a rescan can be folded
    /// under a later harness write's re-baseline — the window closes on the next
    /// unload + reload, which is the operator's apply gesture anyway.
    pub(super) fn on_model_file_written(
        &mut self,
        model: &str,
        path: &std::path::Path,
        written: crate::model::FileFingerprint,
    ) {
        for slot in &mut self.slots {
            let Some(occupant) = slot.occupant.as_mut() else {
                continue;
            };
            if occupant.model == model
                && !occupant.stale_config
                && occupant
                    .spawn_config
                    .as_ref()
                    .is_some_and(|(file, _)| file == path)
            {
                occupant.spawn_config = Some((path.to_path_buf(), written));
            }
        }
    }

    /// The parked (queued, un-armed) Load intents — the same candidate set
    /// [`ActorState::invalidate_queued_loads`] judges, shared so the reload
    /// dispatch gathers stats for exactly the rows the apply turn will read and
    /// carries the ids back in [`DriftStats::queued_at_dispatch`] (FINDINGS-07 [8]).
    pub(super) fn queued_load_candidates(&self) -> Vec<JobId> {
        self.pending
            .values()
            .copied()
            .filter(|job_id| {
                self.jobs.get(job_id).is_some_and(|job| {
                    job.kind == JobKind::Load && !job.terminal && job.state == SwapState::Idle
                })
            })
            .collect()
    }

    /// The model names behind a parked-row candidate set: the rows whose
    /// new-catalog entry files the drift gather must stat.
    pub(super) fn queued_models_for(&self, candidates: &HashSet<JobId>) -> HashSet<String> {
        candidates
            .iter()
            .filter_map(|job_id| self.jobs.get(job_id))
            .map(|job| job.model.clone())
            .collect()
    }

    /// The capture files of every current resident — the paths the marker round
    /// compares, collected actor-side from slot state only (no fs); the stats
    /// themselves are off-thread ([`super::gather_drift_stats`], FINDINGS-07 [8]).
    pub(super) fn drift_capture_files(&self) -> Vec<PathBuf> {
        self.slots
            .iter()
            .filter_map(|slot| slot.occupant.as_ref())
            .filter_map(|occupant| occupant.spawn_config.as_ref().map(|(file, _)| file.clone()))
            .collect()
    }

    /// The queued-intent half of reload (Axis 1 = **1b**, operator ruling
    /// 2026-10-29, `research/reload-slot-drift-2026-10-28.md` rows 12-13): a
    /// rescan retires parked/queued load rows whose model bytes moved or whose
    /// model left the catalog — with the blessed `superseded` vocabulary (19[5]),
    /// named reason, and the same release path every other queued retirement
    /// takes. A **no-change rescan never drops a queue**.
    ///
    /// Candidate states (the registration vocabulary): [`ActorState::register_queued_load`]
    /// is called from two sites — [`ActorState::begin_load`], which arms the row in
    /// the same actor turn (a row never observable as queued), and the park path in
    /// [`ActorState::on_swap`]. What makes a row a candidate is **pending
    /// membership plus its removal discipline**: `pending.insert` lives only on
    /// the park path, [`ActorState::start_pending`] removes before arming, and
    /// [`ActorState::finish_queued_load`] removes before terminalizing — so an
    /// armed row (`Admit` and beyond, a **committed launch**: its witness sample
    /// is dispatched and the gate is pricing the very process it will spawn; the
    /// ruling keeps in-flight/spawning out of scope) is unreachable from here by
    /// construction. The `JobKind::Load`, `!terminal` and `SwapState::Idle`
    /// tests (the same one [`ActorState::cancel_job`] uses for "queued") are
    /// belt-and-braces guards: no exclusion has teeth today under that
    /// invariant, kept deliberately as defense-in-depth for future callers of
    /// registration. This never touches a live
    /// slot, an occupant, a ticket, `last_used`/MRU, or any other job — residents
    /// are the `stale_config` companion round's ground.
    ///
    /// The predicate is the yaml fingerprint captured at registration and nothing
    /// else: every fact a parked row freezes (`cost`, `launch_floor`, `engine`,
    /// `category`, `load_timeout_s`) is derived from that one file, and the only
    /// catalog re-read an armed row still consults ([`ActorState::start_pending`] →
    /// a fresh entry) derives its numbers from the same bytes — so bytes unchanged
    /// means the row is consistent *by construction*, and a per-field diff would
    /// only re-implement the fingerprint with more ways to lie. Bytes moved means a
    /// frozen fact can now contradict the re-read fact: the intent is stale, so it
    /// retires and the operator re-queues on the new file. A missing fingerprint on
    /// either side — no registration baseline, or the gathered stat `None`/absent
    /// (unreadable): is the absence of **evidence**, never evidence of change —
    /// the queue is kept (pinned by
    /// `a_rescan_keeps_a_queue_whose_fingerprint_cannot_show_a_change`). A row
    /// that entered the queue between the gather and the apply is never judged
    /// at all — it is not in `DriftStats::queued_at_dispatch`, and no stat
    /// gathered before its birth can ever speak for it (pinned there, KEEP 4).
    /// The stats ride in with the catalog application as [`DriftStats`], gathered
    /// off-thread on the reload's blocking legs (FINDINGS-07 [8]); this turn reads
    /// only the map.
    ///
    /// Budget no-leak: a never-armed row books nothing (`reserves_vram` excludes
    /// `Idle`), so retirement cannot strand a reservation; what it must not strand
    /// is the queue place — `finish_queued_load` clears it, so a later finish of
    /// the predecessor can never arm a retired row (verified by the integration
    /// pins' spawn arithmetic).
    fn invalidate_queued_loads(&mut self, drift: &DriftStats) {
        let candidates = self.queued_load_candidates();
        for job_id in candidates {
            if !drift.queued_at_dispatch.contains(&job_id) {
                continue;
            }
            let Some(job) = self.jobs.get(&job_id) else {
                continue;
            };
            let (model, category, queued) =
                (job.model.clone(), job.category, job.queue_fingerprint);
            let reason = match self.catalog.iter().find(|m| m.name == model) {
                None => Some("superseded by rescan (removed from the catalog)".to_string()),
                Some(entry) => {
                    let now = entry
                        .file
                        .as_deref()
                        .and_then(|file| drift.stats.get(file))
                        .copied()
                        .flatten();
                    match (queued, now) {
                        (Some(then), Some(now)) if then != now => Some(format!(
                            "superseded by rescan ({} changed since queued \u{2014} re-queue to apply)",
                            entry
                                .file
                                .as_ref()
                                .and_then(|p| p.file_name())
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "<model file>".to_string())
                        )),
                        _ => None,
                    }
                }
            };
            if let Some(reason) = reason {
                tracing::warn!(
                    model = %model,
                    category = ?category,
                    reason = %reason,
                    "queued load superseded by rescan"
                );
                self.finish_queued_load(
                    job_id,
                    Err(SchedulerError::Cancelled(reason)),
                    "cancelled",
                );
            }
        }
    }

    /// Resolves every reload waiter dispatched at or before `seq`: whichever
    /// landing covers a waiter's dispatch can answer it, even if a newer scan
    /// superseded its own (both are complete snapshots of the same disk).
    pub(super) fn drain_reload_waiters(&mut self, seq: u64) {
        let mut keep = Vec::new();
        for (waiter_seq, waiter) in std::mem::take(&mut self.pending_reload) {
            if waiter_seq <= seq {
                let _ = waiter.send(Ok(Reply::Done));
            } else {
                keep.push((waiter_seq, waiter));
            }
        }
        self.pending_reload = keep;
    }

    /// R5 report-only reconcile trigger (boot scan and every rescan alike): name
    /// the running containers whose exact name a catalog row declares as
    /// `ninfer_container` while the claim ledger holds no claim — the stranded
    /// VRAM the ownership-scoped reaping may never address. The probe runs
    /// off-thread (no I/O in the actor loop), one at a time, and its result is
    /// a warning through [`ActorState::on_reconcile_done`] and nothing else:
    /// never a kill, never an eviction, never a gate (G4; ruling 9 — foreign
    /// VRAM is never the harness's own and never a gate guess).
    fn spawn_unclaimed_reconcile(&mut self) {
        let declared: Vec<String> = self
            .catalog
            .iter()
            .filter_map(|entry| {
                entry
                    .raw
                    .get("ninfer_container")
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty())
            })
            .collect();
        if declared.is_empty() || self.reconcile_in_flight {
            return;
        }
        // The sync constructor pins drive `apply_catalog` without a reactor;
        // the reconcile is live-actor work and simply has nothing to do there.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        self.reconcile_in_flight = true;
        let runtime = Arc::clone(&self.runtime);
        let tx = self.tx.clone();
        handle.spawn(async move {
            // RAII release (the R9 C1 rule): the in-flight mark clears on EVERY
            // exit — a panic in the probe reports as a failed probe (silence),
            // never as a mark wedged for the process lifetime.
            let mut release = ReconcileRelease { tx, outcome: None };
            release.outcome = Some(runtime.unclaimed_container_names(&declared).await);
        });
    }

    /// The R5 report itself: one `warn` per stranded name — and only when the
    /// stranded SET changed, so repeated rescans of the same stranded container
    /// never spam the ring (the warn IS the ring event: the observability ring
    /// layer captures every tracing event for the UI log panel). A failed probe
    /// (`None`) is silence and leaves the last-reported set standing — a docker
    /// that was momentarily off must not re-warn what a later probe re-confirms.
    pub(super) fn on_reconcile_done(&mut self, outcome: Option<BTreeSet<String>>) {
        let Some(stranded) = outcome else {
            return;
        };
        if self.last_reported_stranded.as_ref() == Some(&stranded) {
            return;
        }
        self.last_reported_stranded = Some(stranded.clone());
        for name in &stranded {
            tracing::warn!(
                container = %name,
                "unclaimed running container matches a catalog `ninfer_container` row with no \
                 claim in the ledger: its VRAM stays occupied and the harness will never reap it \
                 (report only)"
            );
        }
    }
}
