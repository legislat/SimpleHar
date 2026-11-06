//! The scheduler actor (DESIGN Step 2, choice C LOCKED): one task owns all slot and
//! budget state; everyone else sends commands and holds a oneshot waiter.
//!
//! Invariants encoded here:
//! - single writer: only the actor task mutates slots, jobs, and the ledger;
//! - the actor never performs engine I/O inline — every step runs as a spawned task
//!   whose result returns as an `Event`, so control-plane calls (`status`, `cancel`,
//!   `unload`) never queue behind a load (defect A1);
//! - a dropped waiter is harmless (F-1) and a second identical load attaches to the
//!   in-flight job instead of racing it (F-6);
//! - a same-category load with a different model supersedes: the running job is
//!   cancelled and the new one starts when the slot returns to `Idle` ("rapid A->B
//!   ends with exactly B", T3.4);
//! - every exit path installs an occupant or reports an error — never neither (F-6
//!   funnel) — enforced by walking only [`transition_allowed`] edges.

mod admission;
mod catalog;
mod commands;
mod events;
mod fit;
mod handle;
mod jobs;
mod measured;
mod messages;
mod occupant;
mod spawn;
mod status;
mod vocab;

pub use handle::{Reply, SchedulerError, SchedulerHandle};
pub use jobs::{
    booking_gibs, fault_of, headline_fault, AdmissionPath, JobKind, JobStatus, JobTimings,
    SwapReceipt,
};
pub use status::{SchedulerStatus, SlotStatus};
pub use vocab::{transition_allowed, Command, JobId, SwapState};

use crate::budget::Gibibytes;
use crate::config::{AppConfig, CategoryId};
use crate::engine::{EngineAdapter, EngineRuntime};
use crate::model::scan::{Catalog, ScanError, ScanKind};
use crate::model::ModelEntry;
use crate::ownership::SpawnTicket;
use admission::ReleaseSamples;
use catalog::{gather_drift_stats, scan_with_binary_check, DriftStats};
use fit::FitEntry;
use jobs::{Job, Reclaim, Victim, Waiter};
use measured::{MeasuredAnchor, MeasuredData, Reading};
use messages::{ActorMsg, Event, ReconcileRelease, ServeIdProbeRelease, SnapshotRelease};
use occupant::state_last_used;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[cfg(test)]
use crate::budget::AdmissionError;
#[cfg(test)]
use crate::ownership::ResourceKind;
#[cfg(test)]
use admission::RELEASE_SAMPLE_CAP;
#[cfg(test)]
use catalog::missing_binary_error;
#[cfg(test)]
use fit::FIT_VALIDATE_SLACK_GIB;
#[cfg(test)]
use measured::serve_model_warning;

/// Pause between readiness attempts; a fallback, never the mechanism (§0.3).
const PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// Pause between G12 release-evidence polls while inside the release deadline.
const RELEASE_POLL: Duration = Duration::from_millis(50);
/// Idle sweep cadence (Step 5): thresholds are seconds, so a 2 s tick is ample.
const IDLE_TICK: Duration = Duration::from_secs(2);
/// Wall-clock bound on every serve sampler (one day in seconds): a sampler is not a
/// promise of eternity, so each refresh loop stops after this even if its serve runs
/// on. Bound by wall clock, not by tick count — a day of ticks at the 60 s default
/// cadence would otherwise run for sixty days. Shared by the card stand-in
/// (`events.rs`), the elastic card ladder, and the RAM ladder (`occupant.rs`).
const SAMPLER_LIFETIME_BOUND: Duration = Duration::from_secs(86_400);
/// What a parked load is answered with when the job it waited behind leaves the slot
/// occupied (FINDINGS-02 [3]). `Cancelled` is the variant that fits: the API maps it
/// to 409 (a conflict the caller can retry once the slot is free), the ledger records
/// it under outcome `cancelled` so `fault_of` never pins a red headline on a load the
/// card was simply busy for, and the message names the fact the client can act on —
/// unload the model still in that slot, then load again.
const PARKED_SLOT_NOT_FREED: &str = "the load it was waiting on did not free the slot";

/// Starts the actor; the only constructor (Step 2: one scheduler per process).
pub struct Scheduler;

impl Scheduler {
    /// Spawns the actor task and returns its handle, without touching the catalog:
    /// the scan of `models_dir` (§0.2 step 6, re-run by [`Command::Reload`]) happens
    /// **on the actor task, never on the boot thread** — `start()` returns while it
    /// is still pending, so the boot thread reaches `TcpListener::bind` unblocked
    /// (FINDINGS-14 \[1]: an operator-chosen `modelconf_dir` can live on a sleeping
    /// USB or a disconnected `\\NAS` volume, and a wedged pre-bind scan would stall
    /// the listeners and `/har/status` while the already-held single-instance guard
    /// refuses the operator's relaunch — the same stall the A6 precedent in
    /// `main.rs` moved the banner's stat loop off the critical path for). Command
    /// ordering is identical to a pre-bind scan: the channel is unbounded and the
    /// scan is the first thing the actor does, so everything sent before it lands
    /// queues behind it.
    pub fn start(
        cfg: AppConfig,
        models_dir: PathBuf,
        adapters: BTreeMap<String, Arc<dyn EngineAdapter>>,
        runtime: Arc<dyn EngineRuntime>,
        witness: Arc<dyn crate::budget::VramWitness>,
    ) -> SchedulerHandle {
        let (tx, rx) = mpsc::unbounded_channel();
        let state = ActorState::new(cfg, models_dir, adapters, runtime, witness, tx.clone());
        let (events, _) = tokio::sync::broadcast::channel::<SchedulerStatus>(64);
        tokio::spawn(actor_loop(
            ActorState {
                events: events.clone(),
                ..state
            },
            rx,
        ));
        // Idle sweep tick (Step 5): cheap actor-side scan into the same queue, so
        // the single-writer rule holds.
        let tick_tx = tx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(IDLE_TICK);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if tick_tx.send(ActorMsg::Tick).is_err() {
                    return;
                }
            }
        });
        SchedulerHandle { tx, events }
    }
}

/// A slot occupant as owned by the actor.
struct Occupant {
    model: String,
    need: Gibibytes,
    /// Host RAM booked by a CPU-resident resident (`Some` exactly for
    /// [`crate::model::Resource::Ram`] rows, operator ruling 2026-11-05).
    /// The VRAM `need` of such a resident is 0: it holds no VRAM booking,
    /// is never a VRAM ledger summand or victim, and its host RAM enters
    /// only `ram_ledger_sum` and the host's own `ram_used` reading.
    ram_need: Option<Gibibytes>,
    port: u16,
    ticket: SpawnTicket,
    loaded_at: Instant,
    /// Wall-clock counterpart of `loaded_at`, taken at the same install (the moment
    /// the engine started serving — not the spawn: a cold start runs seconds to
    /// minutes before anyone is served, and the operator's question is when this
    /// model became reachable). `Instant` has no wall-clock meaning outside the
    /// process, so the human-facing launch stamp needs `SystemTime`
    /// (Axis-2 ruling 2026-10-29: "the operator can always see ... launch model
    /// time").
    launched_at_wall: std::time::SystemTime,
    /// The model's yaml as this launch came from: the fold anchor's file paired with
    /// its install-time stat — the spawn-side half of the `stale_config` verdict
    /// (Axis 2 = B). Composed with the anchor, not copied from it: the fold keeps
    /// writing through its own handle. `None` = no backing file, or the stat was
    /// unreadable at install: no capture, and the verdict never accuses on absence
    /// of evidence (same posture as the queued row's KEEP legs).
    spawn_config: Option<(PathBuf, crate::model::FileFingerprint)>,
    /// The cached verdict the snapshot reads (zero fs on the status path,
    /// FINDINGS2-04 [2]): refreshed at install, on the reload turn
    /// ([`ActorState::refresh_stale_markers`]) and on the harness's own
    /// [`ActorState::on_model_file_written`] — never anywhere else.
    stale_config: bool,
    /// The engine's self-reported model id (discovered at readiness, F-4 fix);
    /// proxy rewriting targets this, falling back to the harness name.
    serve_id: Option<String>,
    /// Serving-window witness sampler folding this occupant's total-diff into the
    /// model's measured range; aborted the moment the slot empties (§0.4).
    measure: Option<tokio::task::JoinHandle<()>>,
    /// The largest VRAM footprint this spawn's own fold witnessed
    /// (TASK-GREEDY-SWAP-CREDIT, operator ruling 2026-11-06). `None` until this
    /// spawn folds: never seeded from disk, so it can never be a stale pair —
    /// the same-slot release credit may only spend what THIS process proved it
    /// holds (DOCTRINE 12's provenance rule, projected not booked).
    live_measured_gb: Option<f64>,
}

struct ActorState {
    cfg: AppConfig,
    models_dir: PathBuf,
    catalog: Vec<ModelEntry>,
    catalog_errors: Vec<crate::model::scan::ScanError>,
    adapters: BTreeMap<String, Arc<dyn EngineAdapter>>,
    runtime: Arc<dyn EngineRuntime>,
    witness: Arc<dyn crate::budget::VramWitness>,
    /// The host-RAM witness the CPU-resident gate leg is judged against
    /// (operator ruling 2026-11-05): the RAM ladder's twin of `witness`.
    /// A `None` sample refuses fail-closed — there is no ledger-only mode
    /// for RAM.
    ram_witness: Arc<dyn crate::witness::RamWitness>,
    /// The per-pid working-set reader the CPU-resident serve sampler drives
    /// (W2 step 3): the tree probe behind the `measured_ram_gb` pair. The
    /// live default is `GetProcessMemoryInfo`; tests swap it in place, so
    /// no gate run ever calls the real API.
    working_set_probe: Arc<dyn crate::process::WorkingSetProbe>,
    events: tokio::sync::broadcast::Sender<SchedulerStatus>,
    tx: mpsc::UnboundedSender<ActorMsg>,
    /// Containers with a detached failed-launch log snapshot in flight (R9 C1).
    /// The task must survive `finish`'s `job.tasks` abort to land the evidence the
    /// next launch's pre-reap destroys, so its bound is this set — one fetch per
    /// container, never a pile of concurrent `docker logs` clients from a
    /// crash-looping load — rather than a job-scoped handle.
    snapshot_in_flight: HashSet<String>,
    /// `(port, generation)` of every `serve_id` re-discovery in flight
    /// (FINDINGS-06 [8]): one probe per spawn at a time — the next resolve of
    /// an id-less occupant retries, but never piles concurrent `GET /v1/models`
    /// calls behind a slow engine.
    serve_id_refresh: HashSet<(u16, u64)>,
    /// `(port, generation)` of every occupant whose re-discovery silence has
    /// already been named (FINDINGS-11 [2]): the warn marks the CHANGE into
    /// silence, the same doctrine as the R5 reconcile dedupe — a permanently
    /// silent engine hammered by resolves logs one warn per spawn, never one
    /// per probe. A landed id clears the mark (the silence state ended, so a
    /// return to silence is news again), and every new generation is a fresh
    /// mark; `clear_occupant` retires it with the occupant.
    serve_id_silence_warned: HashSet<(u16, u64)>,
    /// R5 report-only reconcile: a probe per catalog application, never two at
    /// once (the R9 in-flight bound) — a rescan landing mid-probe is answered
    /// by the next application, never by a pile of concurrent `docker ps`.
    reconcile_in_flight: bool,
    /// The stranded-container set the reconcile last warned about (R5 dedupe).
    /// `None` = no successful probe yet — absence of a report is not an empty
    /// report. Warn fires only when the set CHANGES, so repeated rescans of the
    /// same stranded container never spam the ring.
    last_reported_stranded: Option<BTreeSet<String>>,
    /// R11 fit cache (in-process only, never persisted): `(fit tool, probe argv
    /// hash)` → the projection and the model-file fingerprint it was computed
    /// against. A figure is valid precisely while its argv AND the calculator
    /// that minted it are unchanged — the version epoch and fingerprint checks
    /// are the two halves of that identity. The tool joins the key (FR-13):
    /// rows served by different `llama_server_exe` binaries never share a
    /// figure, because each binary's sibling calculator minted its own.
    fit_cache: HashMap<(PathBuf, u64), FitEntry>,
    /// The version line each fit tool answered when its current cache was
    /// minted (FR-13: one entry per tool, keyed by the tool itself). A tool
    /// absent from the map is never confirmed; a version change voids that
    /// tool's cache (a figure belongs to the calculator that minted it).
    fit_versions: HashMap<PathBuf, String>,
    /// The catalog epoch the fit work is stamped with: bumped on every catalog
    /// application, so a warm pass or an admit refresh born under a superseded
    /// catalog is dropped at the handler instead of patching rows that may no
    /// longer describe it.
    fit_epoch: Arc<AtomicU64>,
    /// R11 session cross-check verdicts (display-only): model → (fit epoch the
    /// verdict was earned under, whether the witness fell inside the session
    /// tolerance). A verdict counts only while its epoch stands, so a rescan
    /// retires every session stamp; a calculator swap clears the map outright.
    /// Never persisted, never a gate input — the folded measured pair is the
    /// only booking consequence a witness reading may ever have.
    fit_stamps: BTreeMap<String, (u64, bool)>,
    slots: Vec<Slot>,
    jobs: BTreeMap<JobId, Job>,
    /// Last proxy-side use per model (idle sweep input, Step 5).
    last_used: HashMap<String, Instant>,
    /// Slots holding a queued (parked) load, T3.4, mapped to the ledger row registered
    /// when it parks (FINDINGS-02 [3]): its model, waiters and state all live on that
    /// row, and [`ActorState::start_pending`] arms the same row rather than creating
    /// a second one.
    pending: HashMap<usize, JobId>,
    /// Slots whose occupant an operator's unload must still free once the in-flight
    /// job it cancelled terminalizes (FINDINGS-02 [2]: unload = cancel *and* unload).
    unload_after_finish: HashSet<usize>,
    /// Waiters attached to rescans whose off-thread scan has not landed yet, each
    /// stamped with the dispatch sequence of the scan they wait behind.
    pending_reload: Vec<(u64, Waiter)>,
    /// Rolling median source for refusal wait estimates: every driver-confirmed
    /// release appends its accept-to-confirmed duration here (memory-only,
    /// [`ReleaseSamples`]); a refusal quotes the median or nothing.
    release_samples: ReleaseSamples,
    /// Dispatch counter for reload scans; the apply side records the newest
    /// sequence it honored, so an out-of-order completion never replaces the
    /// catalog with an older filesystem snapshot.
    next_reload: u64,
    last_reload_applied: u64,
    next_job: u64,
    next_gen: u64,
}

struct Slot {
    category: CategoryId,
    index: usize,
    /// Internal engine port for this slot, from the config port plan (Step 8/11).
    engine_port: u16,
    occupant: Option<Occupant>,
    job: Option<JobId>,
}

/// The actor's message loop. The initial catalog scan is the **first thing this task
/// does**, before it takes any message: commands, ticks and reads queue behind it on
/// the unbounded channel, exactly as when the scan ran synchronously in
/// [`ActorState::new`] — but its filesystem work runs on a blocking thread
/// (FINDINGS-13 [6]) while the actor task still *awaits* it before entering the
/// loop (the 4389a0a ordering: scan first, everything queues behind). What a
/// stalled `modelconf_dir` volume can no longer delay: the listener bind, the
/// tray's start, and boot readiness. What still queues behind the first scan:
/// every actor command, `/har/status` polls included (`api`: `handle.status`
/// round-trips this loop) and the tray's refresh (which `block_on`s that status) —
/// bounded by the volume answering, the same exposure [`Command::Reload`] used to
/// have inline. A reload, by contrast, never blocks this loop: its scan lands back
/// here as [`ActorMsg::CatalogLoaded`] and is applied with the same wholesale
/// replace.
async fn actor_loop(mut state: ActorState, mut rx: mpsc::UnboundedReceiver<ActorMsg>) {
    let boot_dir = state.models_dir.clone();
    let (scan, binary) = state.scan_blocking().await;
    let catalog = scan.unwrap_or_else(|reason| Catalog {
        models: Vec::new(),
        errors: vec![ScanError {
            kind: ScanKind::Other,
            file: boot_dir.display().to_string(),
            reason: format!("cannot scan ModelConf: {reason}"),
        }],
    });
    // Boot carries no drift evidence to gather: no resident has captured a spawn
    // file and no row can be queued yet, so the empty map is the complete answer.
    state.apply_catalog(catalog, binary, &DriftStats::default());
    while let Some(msg) = rx.recv().await {
        match msg {
            ActorMsg::Command { cmd, reply } => state.on_command(cmd, reply),
            ActorMsg::CatalogLoaded {
                catalog,
                seq,
                binary,
                drift,
            } => {
                if seq > state.last_reload_applied {
                    state.apply_catalog(catalog, binary, &drift);
                    state.last_reload_applied = seq;
                    state.publish();
                }
                state.drain_reload_waiters(seq);
            }
            ActorMsg::ReloadFailed { seq, reason } => {
                tracing::error!(reason = %reason, "rescan task failed; the previous catalog stays in place");
                state.record_reload_failure(&reason);
                state.publish();
                state.drain_reload_waiters(seq);
            }
            ActorMsg::Touch { model } => state.touch_model(&model),
            ActorMsg::Measured {
                model,
                ticket,
                total_diff,
                own,
                total_gib,
                used_gib,
            } => state.on_measured(
                &model,
                &ticket,
                Reading {
                    total_diff,
                    own,
                    total_gib,
                    used_gib,
                },
            ),
            ActorMsg::MeasuredEngineReport {
                model,
                ticket,
                required_to_free_gb,
                total_gib,
            } => state.on_engine_report(&model, &ticket, required_to_free_gb, total_gib),
            ActorMsg::MeasuredProcessReport {
                model,
                ticket,
                measurement,
                total_gib,
            } => state.on_process_measurement(&model, &ticket, measurement, total_gib),
            ActorMsg::MeasuredRam {
                model,
                ticket,
                ram_gib,
                attribution,
            } => state.on_measured_ram(&model, &ticket, ram_gib, attribution),
            ActorMsg::SnapshotDone(container) => {
                state.snapshot_in_flight.remove(&container);
            }
            ActorMsg::ReconcileDone(outcome) => {
                state.reconcile_in_flight = false;
                state.on_reconcile_done(outcome);
            }
            ActorMsg::ServeIdRefreshed {
                port,
                generation,
                discovered,
            } => {
                state.serve_id_refresh.remove(&(port, generation));
                if state.on_serve_id_refreshed(port, generation, discovered.as_deref()) {
                    state.publish();
                }
            }
            ActorMsg::FitVersionDone {
                gen,
                program,
                version,
            } => {
                state.on_fit_version(gen, &program, version);
            }
            ActorMsg::FitRowDone {
                gen,
                program,
                model,
                key,
                outcome,
            } => state.on_fit_row_done(gen, &program, &model, key, outcome),
            ActorMsg::MeasuredPersistFailed { model, data, file } => {
                state.on_measured_persist_failed(&model, &data, file);
            }
            ActorMsg::ModelFileWritten {
                model,
                path,
                fingerprint,
            } => {
                state.on_model_file_written(&model, &path, fingerprint);
                state.publish();
            }
            ActorMsg::Tick => {
                state.on_tick();
                state.publish();
            }
            ActorMsg::Event { job, event } => state.on_event(job, event),
            ActorMsg::Status { reply } => {
                let _ = reply.send(state.snapshot());
            }
            ActorMsg::Catalog { reply } => {
                let _ = reply.send(crate::model::scan::Catalog {
                    models: state.catalog.clone(),
                    errors: state.catalog_errors.clone(),
                });
            }
        }
    }
}

impl ActorState {
    /// Builds the actor state **without scanning**: the initial catalog scan belongs
    /// to [`actor_loop`], never to the thread that calls [`Scheduler::start`]
    /// (FINDINGS-14 [1]). Identical to [`ActorState::empty`]; kept as the canonical
    /// constructor the boot path funnels through.
    fn new(
        cfg: AppConfig,
        models_dir: PathBuf,
        adapters: BTreeMap<String, Arc<dyn EngineAdapter>>,
        runtime: Arc<dyn EngineRuntime>,
        witness: Arc<dyn crate::budget::VramWitness>,
        tx: mpsc::UnboundedSender<ActorMsg>,
    ) -> Self {
        Self::empty(cfg, models_dir, adapters, runtime, witness, tx)
    }

    fn empty(
        cfg: AppConfig,
        models_dir: PathBuf,
        adapters: BTreeMap<String, Arc<dyn EngineAdapter>>,
        runtime: Arc<dyn EngineRuntime>,
        witness: Arc<dyn crate::budget::VramWitness>,
        tx: mpsc::UnboundedSender<ActorMsg>,
    ) -> Self {
        let mut slots = Vec::new();
        let port_plan = cfg.effective_engine_ports();
        for cat in &cfg.categories {
            let ports = port_plan.get(&cat.id).cloned().unwrap_or_default();
            for (index, engine_port) in ports.iter().enumerate().take(cat.max_models) {
                slots.push(Slot {
                    category: cat.id,
                    index,
                    engine_port: *engine_port,
                    occupant: None,
                    job: None,
                });
            }
        }
        ActorState {
            cfg,
            models_dir,
            catalog: Vec::new(),
            catalog_errors: Vec::new(),
            adapters,
            runtime,
            witness,
            // The RAM ladder's default witness is the live host reading (one
            // `GlobalMemoryStatusEx` per admit). Tests swap it in place — the
            // scripted pins drive `finish_admit` with their own sample and
            // never need a different default.
            ram_witness: Arc::new(crate::witness::GlobalRamWitness),
            // The RAM tree probe's default is the live working-set query; the
            // sampler only ever runs it for a CPU-resident occupant, and every
            // pin that seats one swaps this default for its counting fake —
            // structurally, not by cadence — so no gate test reaches the real
            // API (W2 step 3, made structural by the W23-02 fold).
            working_set_probe: Arc::new(crate::process::Win32WorkingSetProbe),
            events: tokio::sync::broadcast::channel(64).0,
            tx,
            snapshot_in_flight: HashSet::new(),
            serve_id_refresh: HashSet::new(),
            serve_id_silence_warned: HashSet::new(),
            reconcile_in_flight: false,
            last_reported_stranded: None,
            fit_cache: HashMap::new(),
            fit_versions: HashMap::new(),
            fit_epoch: Arc::new(AtomicU64::new(0)),
            fit_stamps: BTreeMap::new(),
            slots,
            jobs: BTreeMap::new(),
            last_used: HashMap::new(),
            pending: HashMap::new(),
            unload_after_finish: HashSet::new(),
            pending_reload: Vec::new(),
            release_samples: ReleaseSamples::new(),
            // Reloads are sequenced from 1: `last_reload_applied` starts at 0 and
            // the newest-wins gate is `seq > last`, so the very first reload (the
            // only scan whose completion may replace the boot catalog) must clear
            // it.
            next_reload: 1,
            last_reload_applied: 0,
            next_job: 0,
            next_gen: 0,
        }
    }

    fn on_command(&mut self, cmd: Command, reply: Waiter) {
        match cmd {
            Command::Swap { model, category } => {
                self.on_swap(model, category, reply);
                self.publish();
            }
            Command::Cancel { job } => {
                self.on_cancel(job, reply);
                self.publish();
            }
            Command::Unload { category } => {
                self.on_unload(category, reply);
                self.publish();
            }
            Command::UnloadModel { model } => {
                self.on_unload_model(&model, reply);
                self.publish();
            }
            Command::UnloadAll => {
                self.on_unload_all(reply);
                self.publish();
            }
            Command::Restart {
                model,
                category,
                port,
            } => {
                self.on_restart(model, category, port, reply);
                self.publish();
            }
            Command::Reload => {
                // FINDINGS-13 [6]: dispatch the scan off-thread and park the
                // waiter; the actor keeps answering ticks, events and every other
                // command against the old catalog until the result lands as
                // `ActorMsg::CatalogLoaded`, which is also what resolves the
                // waiter — so `/har/rescan` still means "new catalog applied".
                // FINDINGS-07 [8] finishes the half: the marker/queue round used
                // to stat its drift evidence inline on the apply turn; it now
                // travels in the same message, stat'ed on this task's blocking
                // legs (`drift_capture_files`/`queued_load_candidates` read only
                // actor state here — never the filesystem).
                // Accepted edge (sibling of 28P [P-2]): a harness write landing
                // strictly between this dispatch's capture gather and the
                // re-baseline note's processing reads as a change — the resident
                // ambers once, and the next reload clears it.
                let tx = self.tx.clone();
                let dir = self.models_dir.clone();
                let exe = self.cfg.llama_server_exe.clone();
                let seq = self.next_reload;
                self.next_reload += 1;
                let capture_files = self.drift_capture_files();
                let queued_at_dispatch: HashSet<JobId> =
                    self.queued_load_candidates().into_iter().collect();
                let queued_models = self.queued_models_for(&queued_at_dispatch);
                tokio::spawn(async move {
                    let captures =
                        tokio::task::spawn_blocking(move || gather_drift_stats(&capture_files));
                    let (scan, binary) = scan_with_binary_check(dir, exe).await;
                    let mut drift = captures.await.unwrap_or_else(|error| {
                        tracing::error!(
                            error = %error,
                            "drift capture stat task failed; residents keep their verdicts"
                        );
                        DriftStats::default()
                    });
                    if let Ok(catalog) = &scan {
                        let row_files: Vec<PathBuf> = catalog
                            .models
                            .iter()
                            .filter(|entry| queued_models.contains(&entry.name))
                            .filter_map(|entry| entry.file.clone())
                            .collect();
                        if !row_files.is_empty() {
                            match tokio::task::spawn_blocking(move || {
                                gather_drift_stats(&row_files)
                            })
                            .await
                            {
                                Ok(rows) => drift.stats.extend(rows.stats),
                                Err(error) => tracing::error!(
                                    error = %error,
                                    "queued-row drift stat task failed; parked rows keep their place"
                                ),
                            }
                        }
                    }
                    drift.queued_at_dispatch = queued_at_dispatch;
                    match scan {
                        Ok(catalog) => {
                            let _ = tx.send(ActorMsg::CatalogLoaded {
                                catalog,
                                seq,
                                binary,
                                drift,
                            });
                        }
                        Err(reason) => {
                            let _ = tx.send(ActorMsg::ReloadFailed { seq, reason });
                        }
                    }
                });
                self.pending_reload.push((seq, reply));
            }
        }
    }

    fn cancel_job(&mut self, job_id: JobId, reason: &str) {
        let error = SchedulerError::Cancelled(reason.to_string());
        let queued = match self.jobs.get(&job_id) {
            Some(job) if job.terminal => return,
            Some(job) => job.state == SwapState::Idle,
            None => return,
        };
        if queued {
            // A load still queued — registered at park time, never armed — spawned
            // nothing and owes no release, so the `Reap -> Idle` edges the graph
            // requires of a working state do not apply: answer it from `Idle` and let
            // go of its queue place, or it could still start on a later finish (FINDINGS-02 [3]).
            self.finish_queued_load(job_id, Err(error), "cancelled");
            return;
        }
        let spawned;
        {
            let Some(job) = self.jobs.get_mut(&job_id) else {
                return;
            };
            for task in job.tasks.drain(..) {
                task.abort();
            }
            job.error = Some(error.clone());
            // The reap decision belongs to the job as it was when the cancel landed,
            // so it is taken before the state moves: this abort just destroyed the only
            // task that could confirm the victim's release, and an unconfirmed victim
            // stays installed in its slot (chunk 4 [1]).
            spawned = job.spawned.is_some() || job.victim_release_pending();
            if vocab::transition_allowed(job.state, SwapState::CancelRequested) {
                job.state = SwapState::CancelRequested;
            }
        }
        if spawned {
            // Kill what this job spawned (or was releasing), confirm, then Idle.
            self.spawn_kill_confirm(job_id);
        } else {
            self.finish_cancelled(job_id);
        }
    }

    fn fail_job(&mut self, job_id: JobId, error: SchedulerError, note: &str) {
        if let Some(job) = self.jobs.get_mut(&job_id) {
            job.error = Some(error.clone());
        }
        self.step(job_id, SwapState::Failed);
        let has_spawn = self.jobs[&job_id].spawned.is_some();
        if has_spawn {
            self.spawn_kill_confirm(job_id);
        } else {
            self.step(job_id, SwapState::Reap);
            self.step(job_id, SwapState::Idle);
            self.finish(job_id, Err(error), note);
        }
    }

    /// Reap -> Idle -> `finish(Cancelled)`, keeping the error the cancel carried: the
    /// one shape every cancel exit takes, whether or not a release was confirmed first.
    fn finish_cancelled(&mut self, job_id: JobId) {
        self.step(job_id, SwapState::Reap);
        self.step(job_id, SwapState::Idle);
        let error = self.jobs[&job_id]
            .error
            .clone()
            .unwrap_or_else(|| SchedulerError::Cancelled("cancelled".to_string()));
        self.finish(job_id, Err(error), "cancelled");
    }

    /// Terminal funnel: every exit path funnels through here exactly once (Step 3).
    fn finish(&mut self, job_id: JobId, result: Result<Reply, SchedulerError>, note: &str) {
        let (waiters, slot) = {
            let Some(job) = self.jobs.get_mut(&job_id) else {
                return;
            };
            if job.terminal {
                return;
            }
            job.terminal = true;
            job.outcome = Some(note.to_string());
            if job.error.is_none() {
                if let Err(error) = &result {
                    job.error = Some(error.clone());
                }
            }
            (std::mem::take(&mut job.waiters), job.slot)
        };
        if let Some(job) = self.jobs.get_mut(&job_id) {
            for task in job.tasks.drain(..) {
                task.abort();
            }
        }
        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
        let owned = slot.filter(|&s| self.slots[s].job == Some(job_id));
        if let Some(s) = owned {
            self.slots[s].job = None;
        }
        // Both the parked-load drain and the unload that follows a cancel belong to the
        // job that owned this slot: a row that never armed (a queued load terminalizing)
        // must not drive either.
        if let Some(slot) = owned {
            if let Some(queued) = self.pending.remove(&slot) {
                if self.slots[slot].occupant.is_none() {
                    self.start_pending(slot, queued);
                } else {
                    // The predecessor did not free the slot, so the queued load will
                    // never start on its own: answer it and take its queue place, or it
                    // would fire unbidden on some later finish of this slot
                    // (FINDINGS-02 [3]).
                    self.finish_queued_load(
                        queued,
                        Err(SchedulerError::Cancelled(PARKED_SLOT_NOT_FREED.to_string())),
                        "cancelled",
                    );
                }
            }
            // The unload the operator asked for while this job was in flight takes the
            // occupant the cancel could not free (FINDINGS-02 [2]). The cancel's own
            // release may already have done that, in which case the flag is only
            // consumed: a slot is never unloaded twice.
            if self.unload_after_finish.remove(&slot) && self.slots[slot].occupant.is_some() {
                self.begin_unload_slot(slot, "unloaded");
            }
        }
        // FR-01: this is the one funnel every job exits through (the only writer of
        // `terminal = true`), so one prune here bounds the ledger and, with it, the
        // `jobs` array every `/har/status` publish re-serializes.
        self.prune_terminal_ledger();
    }

    /// FR-01 job-ledger retention, run at the terminal funnel: keep every live
    /// (non-terminal) row — the single writer is mid-swap on it — and every
    /// terminal row still **in service**: the load row whose spawn is an
    /// installed occupant's ticket. The in-service row is not history: the
    /// serve-window measured fold reads its [`MeasuredAnchor`] by that
    /// ticket on every sample (`measured.rs`), and `container_reported_gb` reads
    /// the container report off it — pruning it under churn would silently stop
    /// the fold for a long-lived resident. It joins ordinary history the moment
    /// its slot empties. Everything else that is terminal is retired history:
    /// the newest [`jobs::TERMINAL_ROW_KEEP`] stand, older ones leave, oldest
    /// first (`BTreeMap` order is `JobId` order, and ids mint monotonically).
    /// Pruning never strands a reader: `apply_event` returns on a missing row,
    /// the EVID-01 victim-row rewrite is a `get`/skip, and idempotency answers
    /// `Already` from the slot table and `on_swap`'s in-flight attach reads only
    /// live rows (`commands.rs`) — no decision consults a retired row.
    fn prune_terminal_ledger(&mut self) {
        let in_service: Vec<SpawnTicket> = self
            .slots
            .iter()
            .filter_map(|slot| slot.occupant.as_ref())
            .map(|occupant| occupant.ticket.clone())
            .collect();
        let mut droppable: Vec<JobId> = Vec::new();
        for (id, job) in &self.jobs {
            if !job.terminal {
                continue;
            }
            if job
                .spawned
                .as_ref()
                .is_some_and(|ticket| in_service.iter().any(|held| held == ticket))
            {
                continue;
            }
            droppable.push(*id);
        }
        for id in jobs::terminal_rows_to_prune(&droppable) {
            self.jobs.remove(id);
        }
    }

    /// FR-03 rescan-failure retention: a failed rescan **replaces** the previous
    /// failure row for this models directory carrying the same reason, so a
    /// disconnected volume retried forever leaves exactly one honest row in
    /// `catalog_errors`, not one per attempt. A different reason is different
    /// news and stands beside it (both are bounded by the reason set, never by
    /// retry count). A successful rescan replaces the whole error set through
    /// [`ActorState::apply_catalog`] — a stale failure row never outlives the
    /// scan that proves the directory readable again, and the per-file errors
    /// that successful scan reported publish unchanged. This turn only edits
    /// memory; no filesystem read, no masking of per-file rows (their `file`
    /// names a yaml, never the directory).
    fn record_reload_failure(&mut self, reason: &str) {
        let row = ScanError {
            kind: ScanKind::Other,
            file: self.models_dir.display().to_string(),
            reason: format!("rescan failed: {reason}"),
        };
        self.catalog_errors.retain(|seen| *seen != row);
        self.catalog_errors.push(row);
    }

    /// Arms the load queued on `slot` (T3.4), reusing the ledger row registered at
    /// park time. A model that left the catalog since it parked is answered and
    /// terminalized like any other load that never started.
    fn start_pending(&mut self, slot: usize, job_id: JobId) {
        let Some(model) = self.jobs.get(&job_id).map(|job| job.model.clone()) else {
            return;
        };
        match self.catalog.iter().find(|m| m.name == model).cloned() {
            Some(entry) => self.start_queued_load(job_id, slot, entry),
            None => self.finish_queued_load(
                job_id,
                Err(SchedulerError::UnknownModel(model)),
                "unknown model",
            ),
        }
    }

    /// Terminalizes a load row that never left `Idle`: parked, then superseded,
    /// unloaded, cancelled, refused as an unknown model, or stranded by a predecessor
    /// that kept the occupant. Its waiters are answered and the row recorded with the
    /// note the caller gives, so `/har/jobs` shows what became of a queued load
    /// (FINDINGS-02 [3]). `finish` performs no state steps, which is what keeps this
    /// legal from `Idle`: the reap edges belong to jobs that actually started.
    ///
    /// Removes the slot's queue place when it still points at this row, so a queued
    /// load cannot be started after it has been answered.
    fn finish_queued_load(
        &mut self,
        job_id: JobId,
        result: Result<Reply, SchedulerError>,
        note: &str,
    ) {
        if let Some(slot) = self.jobs.get(&job_id).and_then(|job| job.slot) {
            if self.pending.get(&slot) == Some(&job_id) {
                self.pending.remove(&slot);
            }
        }
        self.finish(job_id, result, note);
    }

    /// Publishes a snapshot to event subscribers after every mutation (Step 12).
    /// Send errors are laggy or gone subscribers — never the actor's problem.
    fn publish(&self) {
        let _ = self.events.send(self.snapshot());
    }

    /// A refusal is the product's headline event (G6): it reaches the ring whatever
    /// the extended-logging switch says, because a load that never spawned would
    /// otherwise leave no trace there at all (operator 2026-10-26: five refusals in
    /// one session, zero lines at `/har/log`).
    fn log_refusal(&self, job_id: JobId, error: &SchedulerError, stage: &str) {
        let job = &self.jobs[&job_id];
        tracing::warn!(
            %job_id,
            model = %job.model,
            category = ?job.category,
            stage,
            need_gib = job.need.map(|need| need.0),
            free_gib = job.free_at_admit.map(|free| free.0),
            observed_at_admit_gib = job.observed_at_admit,
            floor_gib = job.vram_gate.floor(),
            gate_gib = job.gate_target,
            error = %error,
            "admission refused"
        );
    }

    fn ledger_sum(&self) -> Gibibytes {
        Gibibytes(
            self.slots
                .iter()
                .filter_map(|s| s.occupant.as_ref())
                // A CPU-resident resident books host RAM, not the card
                // (operator ruling 2026-11-05): it is invisible to every VRAM
                // budget, victim plan, and holder list.
                .filter(|o| o.ram_need.is_none())
                .map(|o| o.need.0)
                .sum(),
        )
    }

    /// GiB booked on the host RAM by installed CPU-resident residents — the
    /// RAM ladder's twin of [`Self::ledger_sum`], read only by the RAM gate
    /// leg and its refusal's `booked` figure. GPU residents never enter it
    /// (their host RAM rides the witness's `ram_used`, never the ledger).
    fn ram_ledger_sum(&self) -> Gibibytes {
        Gibibytes(
            self.slots
                .iter()
                .filter_map(|s| s.occupant.as_ref())
                .filter_map(|o| o.ram_need)
                .map(|need| need.0)
                .sum(),
        )
    }

    /// GiB committed to *other* loads that have passed admission but are not yet
    /// installed occupants. The card cannot account for them yet (their engines are
    /// still spawning), so without this debit two concurrent loads in different
    /// categories both spend the same free bytes — the retired ledger never covered
    /// it either, it summed installed occupants only (operator 2026-10-26).
    ///
    /// A `greedy` load still in `Admit` has no `need` yet and contributes 0; it is
    /// protected by its launch floor, and once the greedy booking is recorded it is
    /// debited like any other.
    ///
    /// CPU-resident rows never appear: they debit the RAM ladder
    /// ([`Self::ram_in_flight_need`]), never the card — the two ladders spend
    /// separate resources, and one resource's in-flight debit must never
    /// shrink the other's free (operator ruling 2026-11-05).
    fn in_flight_need(&self, except: JobId) -> f64 {
        self.jobs
            .iter()
            .filter(|(id, job)| {
                **id != except
                    && job.kind == JobKind::Load
                    && !job.terminal
                    && job.ram_need.is_none()
                    && job.state.reserves_vram()
            })
            .map(|(_, job)| job.need.map_or(0.0, |need| need.0))
            .sum()
    }

    /// GiB of host RAM committed to *other* loads admitted but not yet
    /// installed — the RAM ladder's debit, read only by the RAM gate leg
    /// (operator ruling 2026-11-05). The mirror-image rule to
    /// [`Self::in_flight_need`]: a VRAM row never spends RAM bytes here, a
    /// RAM row never spends card bytes there.
    fn ram_in_flight_need(&self, except: JobId) -> f64 {
        self.jobs
            .iter()
            .filter(|(id, job)| {
                **id != except
                    && job.kind == JobKind::Load
                    && !job.terminal
                    && job.state.reserves_vram()
            })
            .filter_map(|(_, job)| job.ram_need)
            .map(|need| need.0)
            .sum()
    }

    fn step(&mut self, job_id: JobId, to: SwapState) {
        let job = self
            .jobs
            .get_mut(&job_id)
            .expect("invariant: callers hold live job ids");
        debug_assert!(
            vocab::transition_allowed(job.state, to),
            "illegal transition {:?} -> {:?} on {job_id}",
            job.state,
            to
        );
        job.state = to;
    }

    fn set_timing<F>(&mut self, job_id: JobId, field: F)
    where
        F: FnOnce(&mut JobTimings) -> &mut Option<u64>,
    {
        let job = self
            .jobs
            .get_mut(&job_id)
            .expect("invariant: callers hold live job ids");
        let elapsed = job.t0.elapsed().as_millis() as u64;
        *field(&mut job.timings) = Some(elapsed);
    }
}

/// Falsifiable pins for the two 1.4 findings owned here: FR-01 job-ledger
/// retention and FR-03 `catalog_errors` growth. Self-contained fixtures: the
/// runtime is never reached, no scan, no engine, no channel read.
#[cfg(test)]
mod retention_tests {
    use super::jobs::TERMINAL_ROW_KEEP;
    use super::*;
    use crate::budget::LedgerOnlyWitness;
    use tokio::sync::oneshot;

    /// Never-called stand-in: every pin here drives actor-side memory paths
    /// only, and a call reaching the runtime would mean a fixture escaped its
    /// scope.
    struct NeverRuntime;

    #[async_trait::async_trait]
    impl EngineRuntime for NeverRuntime {
        async fn spawn(
            &self,
            _spec: &crate::engine::LaunchSpec,
            _ticket: SpawnTicket,
        ) -> Result<crate::engine::Spawned, crate::engine::SpawnError> {
            unimplemented!("retention pins never spawn")
        }
        async fn kill(&self, _ticket: &SpawnTicket) -> Result<(), crate::engine::SpawnError> {
            unimplemented!("retention pins never kill")
        }
        async fn release_confirmed(
            &self,
            _ticket: &SpawnTicket,
        ) -> Result<bool, crate::engine::SpawnError> {
            unimplemented!("retention pins never reap")
        }
        async fn probe(
            &self,
            _port: u16,
            _readiness: &crate::engine::ReadinessSpec,
        ) -> crate::engine::Probe {
            unimplemented!("retention pins never probe")
        }
        async fn discover_model_id(&self, _port: u16) -> Option<String> {
            unimplemented!("retention pins never discover")
        }
    }

    fn state() -> ActorState {
        let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
        ActorState::empty(
            AppConfig::default(),
            PathBuf::from("models-conf-under-test"),
            BTreeMap::new(),
            Arc::new(NeverRuntime) as Arc<dyn EngineRuntime>,
            Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
            tx,
        )
    }

    fn row(kind: JobKind, terminal: bool) -> Job {
        Job {
            kind,
            path: None,
            model: "ledger-row".to_string(),
            engine: String::new(),
            category: CategoryId::Work,
            state: SwapState::Idle,
            terminal,
            outcome: terminal.then(|| "loaded".to_string()),
            error: None,
            need: None,
            resource: crate::model::Resource::Gpu,
            ram_need: None,
            cost: crate::model::CostClass::Fixed,
            vram_gate: crate::model::VramGate::default(),
            load_timeout_s: None,
            gate_target: None,
            free_at_admit: None,
            observed_at_admit: None,
            release_first: false,
            container: None,
            timings: JobTimings::default(),
            t0: Instant::now(),
            waiters: Vec::new(),
            tasks: Vec::new(),
            spawned: None,
            readiness: None,
            measured: None,
            victim: None,
            victims: Vec::new(),
            reclaim: Reclaim::default(),
            victims_released: Vec::new(),
            released_cost: 0.0,
            holder_released_cost: 0.0,
            other_need_at_admit: 0.0,
            note: None,
            warnings: Vec::new(),
            slot: None,
            queue_fingerprint: None,
        }
    }

    /// Insert one terminal row under `id` — keeps the pin bodies to the counts
    /// that carry the finding.
    fn insert_terminal(state: &mut ActorState, id: u64) {
        state.jobs.insert(JobId(id), row(JobKind::Load, true));
    }

    fn occupant(name: &str, ticket: &SpawnTicket) -> Occupant {
        Occupant {
            model: name.to_string(),
            need: Gibibytes(2.0),
            ram_need: None,
            port: ticket.port,
            ticket: ticket.clone(),
            loaded_at: Instant::now(),
            launched_at_wall: std::time::SystemTime::now(),
            spawn_config: None,
            stale_config: false,
            // An id stands for the fixture: without it the resolve would arm
            // the FINDINGS-06 [8] re-discovery task, and the sync pins run with
            // no reactor.
            serve_id: Some(name.to_string()),
            measure: None,
            live_measured_gb: None,
        }
    }

    fn resident_ticket(port: u16) -> SpawnTicket {
        SpawnTicket {
            generation: 1,
            kind: ResourceKind::ChildProcess,
            pid: 4242,
            port,
        }
    }

    /// FR-01 pin: 250 terminal rows plus one fresh finish (251 terminal) leave
    /// exactly the newest 200 — through the terminal funnel, not the pure rule.
    #[test]
    fn two_hundred_fifty_terminal_rows_leave_the_newest_two_hundred() {
        assert_eq!(TERMINAL_ROW_KEEP, 200);
        let mut state = state();
        for id in 0..250u64 {
            insert_terminal(&mut state, id);
        }
        let closing = JobId(250);
        state.jobs.insert(closing, row(JobKind::Load, false));
        state.finish(closing, Ok(Reply::Done), "loaded");
        let ids: Vec<u64> = state.jobs.keys().map(|id| id.0).collect();
        assert_eq!(ids.len(), TERMINAL_ROW_KEEP, "ledger must cap at 200");
        assert_eq!(ids.first(), Some(&51), "the 51 oldest retired rows left");
        assert_eq!(ids.last(), Some(&250), "the newest row stands");
    }

    /// FR-01 pin: a live (non-terminal) row is never pruned, not even as the
    /// single oldest row while 251 terminal rows pile up behind it.
    #[test]
    fn a_live_row_is_never_pruned() {
        let mut state = state();
        let oldest_live = JobId(0);
        state.jobs.insert(oldest_live, row(JobKind::Load, false));
        for id in 1..=250u64 {
            insert_terminal(&mut state, id);
        }
        let closing = JobId(251);
        state.jobs.insert(closing, row(JobKind::Unload, false));
        state.finish(closing, Ok(Reply::Done), "unloaded");
        assert!(
            state.jobs.contains_key(&oldest_live),
            "the single writer is mid-swap on the oldest row: retention may not eat it"
        );
        assert!(!state.jobs[&oldest_live].terminal);
        let terminal = state.jobs.values().filter(|job| job.terminal).count();
        assert_eq!(terminal, TERMINAL_ROW_KEEP);
        assert_eq!(state.jobs.len(), TERMINAL_ROW_KEEP + 1);
    }

    /// FR-01 pin: the `/har/status` view IS the retained view — the published
    /// `jobs` array carries exactly the ledger rows, so every publish serializes
    /// the bounded set, never the pruned history.
    #[test]
    fn the_status_jobs_view_is_the_retained_view() {
        let mut state = state();
        for id in 0..250u64 {
            insert_terminal(&mut state, id);
        }
        let closing = JobId(250);
        state.jobs.insert(closing, row(JobKind::Load, false));
        state.finish(closing, Ok(Reply::Done), "loaded");
        let snapshot = state.snapshot();
        let snapshot_ids: Vec<u64> = snapshot.jobs.iter().map(|job| job.id.0).collect();
        let ledger_ids: Vec<u64> = state.jobs.keys().map(|id| id.0).collect();
        assert_eq!(snapshot.jobs.len(), TERMINAL_ROW_KEEP);
        assert_eq!(
            snapshot_ids, ledger_ids,
            "snapshot and ledger cannot disagree"
        );
    }

    /// FR-01 in-service exception: the terminal row of an installed resident
    /// stands past the cap — its ticket match against the slot occupant keeps
    /// it, because the serve-window measured fold reads its anchor by exactly
    /// that ticket on every sample (`measured.rs` finds the row by
    /// `job.spawned == ticket`).
    #[test]
    fn an_in_service_terminal_row_survives_the_cap() {
        let mut state = state();
        let ticket = resident_ticket(18_999);
        let resident = JobId(0);
        let mut resident_row = row(JobKind::Load, true);
        resident_row.spawned = Some(ticket.clone());
        resident_row.state = SwapState::Serving;
        state.jobs.insert(resident, resident_row);
        state.slots[0].occupant = Some(occupant("resident", &ticket));
        for id in 1..=250u64 {
            insert_terminal(&mut state, id);
        }
        let closing = JobId(251);
        state.jobs.insert(closing, row(JobKind::Load, false));
        state.finish(closing, Ok(Reply::Done), "loaded");
        assert!(
            state.jobs.contains_key(&resident),
            "pruning the resident's row would silently stop its measured fold"
        );
        let terminal = state.jobs.values().filter(|job| job.terminal).count();
        assert_eq!(
            terminal,
            TERMINAL_ROW_KEEP + 1,
            "200 retired + 1 in service"
        );
    }

    /// FR-01 idempotency pin: a repeated load answers `Already` from the slot
    /// table, and keeps answering `Already` after the ledger row that once
    /// produced the resident is gone — the exact shape a pruned row leaves. The
    /// receipt never depends on a terminal ledger row surviving.
    #[test]
    fn a_repeat_load_answers_already_with_the_ledger_row_pruned() {
        let mut state = state();
        let (map, _) = crate::model::scan::parse_flat_yaml("engine: mock\nvram_gb: 4\n");
        let mut entry = ModelEntry::from_flat(&map, "res").expect("entry");
        entry.name = "res".to_string();
        state.catalog.push(entry);
        let category = state.slots[0].category;
        let ticket = resident_ticket(18_998);
        state.slots[0].occupant = Some(occupant("res", &ticket));

        let produced = JobId(7);
        let mut produced_row = row(JobKind::Load, true);
        produced_row.model = "res".to_string();
        produced_row.spawned = Some(ticket.clone());
        produced_row.state = SwapState::Serving;
        state.jobs.insert(produced, produced_row);
        let (reply, mut receipt) = oneshot::channel();
        state.on_swap("res".to_string(), category, reply);
        assert!(
            matches!(receipt.try_recv(), Ok(Ok(Reply::Already { .. }))),
            "the loaded resident answers Already while its row still stands"
        );

        // The FR-01 shape: retention removed the row. The decision reads the
        // slot table (`commands.rs`), so the repeat is answered identically.
        state.jobs.remove(&produced);
        let (reply, mut receipt) = oneshot::channel();
        state.on_swap("res".to_string(), category, reply);
        match receipt.try_recv() {
            Ok(Ok(Reply::Already { model, port, .. })) => {
                assert_eq!(model, "res");
                assert_eq!(port, ticket.port);
            }
            other => panic!("Already must not depend on a terminal row: {other:?}"),
        }
    }

    /// FR-03 pin: three identical failed rescans of the same models directory
    /// leave ONE row, not three.
    #[test]
    fn three_identical_failed_rescans_leave_one_row() {
        let mut state = state();
        for _ in 0..3 {
            state.record_reload_failure("os error 5: Access is denied. (os error 5)");
        }
        assert_eq!(
            state.catalog_errors.len(),
            1,
            "repeated identical failures must not grow the report: {:?}",
            state.catalog_errors
        );
        assert_eq!(state.catalog_errors[0].file, "models-conf-under-test");
        assert!(state.catalog_errors[0].reason.starts_with("rescan failed:"));
        let snapshot = state.snapshot();
        assert_eq!(snapshot.catalog_errors.len(), 1, "the published view too");
    }

    /// FR-03 pin: a changed failure reason is different news and stays visible
    /// beside the previous one; repeating either still never duplicates.
    #[test]
    fn a_changed_failure_reason_stays_visible() {
        let mut state = state();
        state.record_reload_failure("volume offline");
        state.record_reload_failure("read_dir denied");
        state.record_reload_failure("volume offline");
        assert_eq!(
            state.catalog_errors.len(),
            2,
            "one row per reason, not per try"
        );
        let reasons: Vec<&str> = state
            .catalog_errors
            .iter()
            .map(|row| row.reason.as_str())
            .collect();
        assert!(reasons.iter().any(|r| r.contains("volume offline")));
        assert!(reasons.iter().any(|r| r.contains("read_dir denied")));
    }

    /// FR-03 pin: a successful rescan replaces the failure rows — the scan that
    /// proves the directory readable again leaves no stale failure behind.
    #[test]
    fn a_successful_rescan_clears_the_failure_rows() {
        let mut state = state();
        state.record_reload_failure("volume offline");
        state.record_reload_failure("volume offline");
        assert_eq!(state.catalog_errors.len(), 1);
        state.apply_catalog(
            Catalog {
                models: Vec::new(),
                errors: Vec::new(),
            },
            None,
            &DriftStats::default(),
        );
        assert!(
            state.catalog_errors.is_empty(),
            "a clean rescan clears the failure rows: {:?}",
            state.catalog_errors
        );
    }

    /// FR-03 pin: a rescan failure never masks the per-file scan errors a
    /// successful scan reported — the replace is scoped to the identical
    /// directory-failure row only.
    #[test]
    fn a_failed_rescan_never_masks_per_file_scan_errors() {
        let mut state = state();
        let per_file = ScanError {
            kind: ScanKind::InvalidName,
            file: "bad.yaml".to_string(),
            reason: "name: contradicts the file stem".to_string(),
        };
        state.apply_catalog(
            Catalog {
                models: Vec::new(),
                errors: vec![per_file.clone()],
            },
            None,
            &DriftStats::default(),
        );
        for _ in 0..3 {
            state.record_reload_failure("volume offline");
        }
        assert_eq!(
            state.catalog_errors,
            vec![
                per_file,
                ScanError {
                    kind: ScanKind::Other,
                    file: "models-conf-under-test".to_string(),
                    reason: "rescan failed: volume offline".to_string(),
                }
            ],
            "the per-file error stands, the failure appears once"
        );
    }
}

#[cfg(test)]
mod tests;
