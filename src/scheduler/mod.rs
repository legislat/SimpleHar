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
mod jobs;
mod measured;
mod occupant;
mod spawn;
mod status;
mod vocab;

pub use jobs::{
    booking_gibs, fault_of, headline_fault, AdmissionPath, JobKind, JobStatus, JobTimings,
    SwapReceipt,
};
pub use status::{SchedulerStatus, SlotStatus};
pub use vocab::{transition_allowed, Command, JobId, SwapState};

use crate::budget::{AdmissionError, Gibibytes};
use crate::config::{AppConfig, CategoryId};
use crate::engine::{EngineAdapter, EngineRuntime, Probe, SpawnError, Spawned};
use crate::model::scan::{Catalog, ScanError, ScanKind};
use crate::model::ModelEntry;
use crate::ownership::SpawnTicket;
use fit::FitEntry;
use jobs::{Job, Victim, Waiter};
use measured::{MeasuredAnchor, MeasuredData, Reading};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

#[cfg(test)]
use crate::ownership::ResourceKind;
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
/// What a parked load is answered with when the job it waited behind leaves the slot
/// occupied (FINDINGS-02 [3]). `Cancelled` is the variant that fits: the API maps it
/// to 409 (a conflict the caller can retry once the slot is free), the ledger records
/// it under outcome `cancelled` so `fault_of` never pins a red headline on a load the
/// card was simply busy for, and the message names the fact the client can act on —
/// unload the model still in that slot, then load again.
const PARKED_SLOT_NOT_FREED: &str = "the load it was waiting on did not free the slot";

/// Failures reported to swap/unload/cancel callers, with the numbers used (G13).
#[derive(Debug, Clone, PartialEq, Error)]
pub enum SchedulerError {
    /// No catalog entry for the requested name.
    #[error("unknown model `{0}`")]
    UnknownModel(String),
    /// No job with this id: the job ledger lives only for this process's lifetime
    /// (it is never persisted), so a restart or a mistyped id both land here.
    #[error("unknown job {0}")]
    UnknownJob(JobId),
    /// The budget gate refused the load; carries need/free/reserve (Step 6 contract).
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    /// The adapter refused to build a launch spec.
    #[error(transparent)]
    Engine(#[from] crate::engine::EngineError),
    /// The engine could not be started.
    #[error("spawn failed: {0}")]
    Spawn(String),
    /// Readiness was never reached inside `load_timeout_s`.
    #[error("engine failed to become ready: {0}")]
    EngineFailed(String),
    /// Release could not be proven in time; the named holder is still alive and
    /// nothing was double-spawned (G12).
    #[error("release timeout: {holder} still holds the slot")]
    ReleaseTimeout { holder: String },
    /// The job was cancelled by request or superseded by a newer load.
    #[error("cancelled: {0}")]
    Cancelled(String),
    /// The card had not physically freed the gate target within `settle_timeout_s`
    /// after a confirmed release; nothing was double-spawned (§0.3). The reason is
    /// the witness's own numbers, composed once at the emit site and carried
    /// structurally — the gate never re-parses a human string.
    #[error("settle timeout: {reason}")]
    SettleTimeout {
        /// What the settle gate saw, e.g. `12.4 GiB free after release, 34.0 GiB
        /// required incl. 2.0 GiB reserve (used 58.2 of 72.0)`.
        reason: String,
    },
    /// The actor task is gone (shutting down).
    #[error("scheduler is not running")]
    ActorGone,
    /// The command is real but lands with a later milestone.
    #[error("unsupported in this milestone: {0}")]
    Unsupported(String),
}

/// Successful outcome of a command, echoed to every attached waiter.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Reply {
    /// The model is loaded and serving.
    Loaded(SwapReceipt),
    /// The model was already serving in that category (idempotent double-click,
    /// Step 12).
    Already {
        /// Harness name of the serving model.
        model: String,
        /// Category it serves in.
        category: CategoryId,
        /// Port it serves on.
        port: u16,
        /// The engine's self-reported id (may rewrite differently, F-4).
        serve_id: Option<String>,
    },
    /// The command was accepted with nothing further to report (cancel, reload).
    Done,
    /// An accepted unload command, reporting what the actor actually did at accept
    /// (FINDINGS-07 \[5]): never a blanket `Done` while nothing moved. The release
    /// itself stays job-verified — this is an acceptance ledger, not release proof.
    Unloaded {
        /// Slots whose occupant the actor committed to free: unload jobs started
        /// now, plus occupants armed to unload behind the in-flight job this
        /// command cancelled. A commitment counts only when it is new — a
        /// repeat command that would re-arm an already-armed slot reports `0`
        /// (FINDINGS-09 \[1]).
        unloading: usize,
        /// In-flight LOADS cancelled and parked loads dropped by this command.
        /// Cancelling an in-flight Unload job is not this: it removes an
        /// unloading commitment rather than a load, and rides `unloading`
        /// instead (FINDINGS-09 \[2]). A re-entered cancel of an already
        /// cancel-requested job counts nowhere: it moved nothing.
        jobs_cancelled: usize,
    },
}

/// Cloneable handle: the only way to talk to the actor (AGENTS.md: no globals).
#[derive(Debug, Clone)]
pub struct SchedulerHandle {
    tx: mpsc::UnboundedSender<ActorMsg>,
    events: tokio::sync::broadcast::Sender<SchedulerStatus>,
}

impl SchedulerHandle {
    /// Test-only handle over a dummy channel: for unit tests that need an
    /// `ApiState` but never issue scheduler commands (no actor is spawned).
    #[cfg(test)]
    pub(crate) fn test_dummy() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        SchedulerHandle {
            tx,
            events: tokio::sync::broadcast::channel(1).0,
        }
    }

    /// Runtime switch for the decision-level log lines (page checkbox, off at
    /// boot): admit comparisons, settle evidence, full request ring.
    pub fn set_extended_logging(&self, on: bool) {
        crate::observability::set_extended_logging(on);
    }

    /// Whether extended logging is currently on.
    #[must_use]
    pub fn extended_logging(&self) -> bool {
        crate::observability::extended_logging()
    }
    /// Loads `model` into `category` (supersede policy for same-category loads).
    ///
    /// # Errors
    ///
    /// [`SchedulerError`] variants; a dropped future (client abort) never cancels the
    /// load — the actor replies into a closed channel and continues (F-1).
    pub async fn swap(&self, model: &str, category: CategoryId) -> Result<Reply, SchedulerError> {
        self.call(Command::Swap {
            model: model.to_string(),
            category,
        })
        .await
    }

    /// Cancels a job; cleanup completes (kill + release confirmation) before the slot
    /// returns to `Idle` (Step 3). Cancelling a finished job is `Done` (idempotent).
    ///
    /// # Errors
    ///
    /// [`SchedulerError::UnknownJob`] or [`SchedulerError::ActorGone`].
    pub async fn cancel(&self, job: JobId) -> Result<Reply, SchedulerError> {
        self.call(Command::Cancel { job }).await
    }

    /// Unloads every occupant of `category`; idempotent (an empty category
    /// answers [`Reply::Unloaded`] with both counts zero).
    ///
    /// # Errors
    ///
    /// [`SchedulerError::ActorGone`] when the actor is no longer running.
    pub async fn unload(&self, category: CategoryId) -> Result<Reply, SchedulerError> {
        self.call(Command::Unload { category }).await
    }

    /// Restarts `model` in `category`: kills the (dead) occupant and loads it
    /// fresh — the T10.1 crash-retry primitive. `failed_port` is the engine port
    /// the caller's request refused to connect on (the proxy knows it): the
    /// identity of the corpse. A Restart naming a port the slot no longer serves
    /// describes a crash a previous retry already recovered, and is answered
    /// against the live occupant instead of reaping it (R2-06-01).
    ///
    /// # Errors
    ///
    /// [`SchedulerError`] variants as for [`SchedulerHandle::swap`].
    pub async fn restart(
        &self,
        model: &str,
        category: CategoryId,
        failed_port: u16,
    ) -> Result<Reply, SchedulerError> {
        self.call(Command::Restart {
            model: model.to_string(),
            category,
            port: failed_port,
        })
        .await
    }

    /// Unloads one model's slot (T12.1). A loading swap for it is cancelled
    /// first (cleanup still completes, Step 3).
    ///
    /// # Errors
    ///
    /// [`SchedulerError::ActorGone`] when the actor is no longer running.
    pub async fn unload_model(&self, model: &str) -> Result<Reply, SchedulerError> {
        self.call(Command::UnloadModel {
            model: model.to_string(),
        })
        .await
    }

    /// Unloads every occupied slot across all categories (T12.4). Loading jobs
    /// are cancelled; cleanup completes.
    ///
    /// # Errors
    ///
    /// [`SchedulerError::ActorGone`] when the actor is no longer running.
    pub async fn unload_all(&self) -> Result<Reply, SchedulerError> {
        self.call(Command::UnloadAll).await
    }

    /// Records a use of `model` (fire-and-forget): the proxy paths call this on
    /// every request so the actor's idle check measures real activity (Step 5).
    /// A dropped actor makes this a no-op.
    pub fn touch(&self, model: &str) {
        let _ = self.tx.send(ActorMsg::Touch {
            model: model.to_string(),
        });
    }

    /// Re-scans `ModelConf` (hot reload, Step 9): the catalog and its error report
    /// refresh atomically from the actor's single writer.
    ///
    /// # Errors
    ///
    /// [`SchedulerError::ActorGone`] when the actor is no longer running.
    pub async fn reload(&self) -> Result<Reply, SchedulerError> {
        self.call(Command::Reload).await
    }

    /// Catalog snapshot (Step 2: reads are snapshots owned by the actor).
    ///
    /// # Errors
    ///
    /// [`SchedulerError::ActorGone`] when the actor is no longer running.
    pub async fn catalog(&self) -> Result<crate::model::scan::Catalog, SchedulerError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorMsg::Catalog { reply: tx })
            .map_err(|_| SchedulerError::ActorGone)?;
        rx.await.map_err(|_| SchedulerError::ActorGone)
    }

    /// Subscribes to status pushes: the actor publishes a snapshot after every
    /// state mutation (Step 12: the events stream, so the page polls nothing).
    ///
    /// # Returns
    ///
    /// A broadcast receiver; laggy subscribers miss snapshots, never block the actor.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<SchedulerStatus> {
        self.events.subscribe()
    }

    /// Snapshot of slots and jobs; never probes, never mutates (Step 2).
    ///
    /// # Errors
    ///
    /// [`SchedulerError::ActorGone`] when the actor is no longer running.
    pub async fn status(&self) -> Result<SchedulerStatus, SchedulerError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorMsg::Status { reply: tx })
            .map_err(|_| SchedulerError::ActorGone)?;
        rx.await.map_err(|_| SchedulerError::ActorGone)
    }

    /// Tells the actor that a harness-owned writer just rewrote this model's own
    /// yaml, carrying the post-write fingerprint (Axis-2 interlock 5: the measured
    /// fold and the API's flag edit must never amber their own model). Send it
    /// **before** `reload()` when one follows: the actor's mailbox orders the
    /// re-baseline ahead of the reload's verdict refresh. Fire-and-forget — a gone
    /// actor means no resident left to mark.
    pub fn note_model_file_written(
        &self,
        model: &str,
        path: &std::path::Path,
        fingerprint: crate::model::FileFingerprint,
    ) {
        let _ = self.tx.send(ActorMsg::ModelFileWritten {
            model: model.to_string(),
            path: path.to_path_buf(),
            fingerprint,
        });
    }

    async fn call(&self, cmd: Command) -> Result<Reply, SchedulerError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorMsg::Command { cmd, reply: tx })
            .map_err(|_| SchedulerError::ActorGone)?;
        match rx.await {
            Ok(inner) => inner,
            Err(_) => Err(SchedulerError::ActorGone),
        }
    }
}

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

/// Releases a detached failed-launch snapshot's in-flight mark on drop (R9 C1):
/// constructed inside the spawned fetch, dropped by every exit path (normal end,
/// early return, panic unwind), so a wedged mark can never permanently blind the
/// next failure of the same container name (review 2026-10-31, L9).
struct SnapshotRelease {
    tx: mpsc::UnboundedSender<ActorMsg>,
    container: String,
}

impl Drop for SnapshotRelease {
    fn drop(&mut self) {
        let _ = self.tx.send(ActorMsg::SnapshotDone(self.container.clone()));
    }
}

/// R5 reconcile release: every exit of the probe task — normal end, panic,
/// cancellation — lands exactly one [`ActorMsg::ReconcileDone`], so the
/// in-flight mark can never wedge. An outcome the probe never filled reports
/// as a failed probe (silence), never as an empty set.
struct ReconcileRelease {
    tx: mpsc::UnboundedSender<ActorMsg>,
    outcome: Option<Option<BTreeSet<String>>>,
}

impl Drop for ReconcileRelease {
    fn drop(&mut self) {
        let outcome = self.outcome.take().unwrap_or(None);
        let _ = self.tx.send(ActorMsg::ReconcileDone(outcome));
    }
}

/// Serve-id re-discovery release (FINDINGS-06 [8]): every exit of the probe
/// task — normal end, panic, cancellation — lands exactly one
/// [`ActorMsg::ServeIdRefreshed`], so the in-flight mark can never wedge the
/// next resolve into silence without a retry. An answer the probe never
/// produced reports as silence (`None`), the same posture as the reconcile
/// release above.
struct ServeIdProbeRelease {
    tx: mpsc::UnboundedSender<ActorMsg>,
    port: u16,
    generation: u64,
    outcome: Option<Option<String>>,
}

impl Drop for ServeIdProbeRelease {
    fn drop(&mut self) {
        let discovered = self.outcome.take().flatten();
        let _ = self.tx.send(ActorMsg::ServeIdRefreshed {
            port: self.port,
            generation: self.generation,
            discovered,
        });
    }
}

#[derive(Debug)]
enum ActorMsg {
    Command {
        cmd: Command,
        reply: Waiter,
    },
    /// The off-thread scan dispatched by [`Command::Reload`] finished; apply it
    /// with the same wholesale replace as the boot scan and resolve the queued
    /// reload waiters — their reply means "the new catalog is applied", never
    /// "the scan was dispatched" (FINDINGS-13 [6] keeps the API-visible semantics).
    CatalogLoaded {
        catalog: Catalog,
        seq: u64,
        /// The `llama-server.exe` verdict gathered on the same blocking leg as the
        /// scan (FINDINGS2-04 [2]); `None` means nothing to report.
        binary: Option<ScanError>,
        /// The drift stats for the marker/queue rounds this application runs —
        /// gathered on the reload task's blocking legs so the apply turn never
        /// stats (FINDINGS-07 [8]).
        drift: DriftStats,
    },
    /// The blocking scan task itself failed (panic or cancel): the previous
    /// catalog stays in place, the failure is reported, and the reload waiters
    /// are still answered — a wedged scan thread must not strand callers.
    ReloadFailed {
        seq: u64,
        reason: String,
    },
    /// Last-use hint from the proxy paths (fire-and-forget; feeds the idle check).
    Touch {
        model: String,
    },
    /// One witness reading taken while `model` serves (§0.4 measured cost). The
    /// sampler reports only what the card shows; the actor decides what to charge —
    /// preferring the driver's per-process attribution and subtracting tenants that
    /// appeared after this engine was admitted. A reading whose engine has been
    /// replaced is rejected by `ticket`, so a sampler that outlives its engine cannot
    /// fold another model's memory into the old entry (review, 2026-10-27).
    Measured {
        model: String,
        ticket: SpawnTicket,
        total_diff: f64,
        own: Option<f64>,
        total_gib: f64,
        used_gib: f64,
    },
    /// The container's own startup `MemorySummary` figure (R9): one shot per launch,
    /// fetched from `docker logs` at readiness. The reserved footprint is static for
    /// the life of the engine, so it fixes the range outright instead of folding.
    MeasuredEngineReport {
        model: String,
        ticket: SpawnTicket,
        required_to_free_gb: f64,
        total_gib: f64,
    },
    /// A process engine's one-shot measured footprint (`TASK-ENGINE-MEASUREMENT`,
    /// ORCHESTRATION §3): the figure basalt/strata stated at their own status
    /// endpoint, or the witness's process-tree sum of the engine's pid plus its
    /// direct children. One shot per launch like `MeasuredEngineReport`; the
    /// measurement's `source` decides the provenance it folds under. `total_gib`
    /// is the card total of the live witness sample the figure was bounded
    /// against (doctrine 4: the witness is budget truth).
    MeasuredProcessReport {
        model: String,
        ticket: SpawnTicket,
        measurement: crate::engine::EngineMeasurement,
        total_gib: f64,
    },
    /// A detached failed-launch log snapshot (R9 C1) has finished; release its
    /// in-flight mark so a later failure of the same container name re-snapshots.
    SnapshotDone(String),
    /// An R5 report-only reconcile probe has finished. `Some` = docker answered
    /// with the stranded set (possibly empty); `None` = the probe could not run
    /// — silence, and the last-reported set stands untouched.
    ReconcileDone(Option<BTreeSet<String>>),
    /// A `serve_id` re-discovery (FINDINGS-06 [8]) for a loaded occupant that
    /// installed with no id has finished: `Some` = the id the engine now
    /// reports; `None` = still silence, and the next resolve retries.
    /// `(port, generation)` names the spawn the probe was launched for — an
    /// answer for a replaced occupant is dropped at the door.
    ServeIdRefreshed {
        port: u16,
        generation: u64,
        discovered: Option<String>,
    },
    /// The R11 fit version probe finished (warm pass and every fit admit lead
    /// with it): `Some` = the calculator's version line, `None` = the tool did
    /// not answer and the fit pass stays silent. `gen` is the catalog epoch the
    /// probe was born in — a stale epoch's answer is dropped at the door.
    FitVersionDone {
        gen: u64,
        version: Option<String>,
    },
    /// An R11 fit row probe finished: `Ok` = (booked GiB, the model file's
    /// fingerprint at computation); `Err` = why (warn + ladder, never a
    /// fabricated figure). `key` is the probe argv hash the figure was computed
    /// for — the cache identity.
    FitRowDone {
        gen: u64,
        model: String,
        key: u64,
        outcome: Result<(f64, Option<crate::model::FileFingerprint>), String>,
    },
    /// The blocking write-back of a measured figure was refused by the disk
    /// (Fork C, FINDINGS-13 [4] follow-through): the actor must stop booking in
    /// memory what disk rejected. Carries the exact figure — pair **and provenance
    /// stamps** — the failed write attempted, so a reading that has since been
    /// superseded never drains the newer (possibly persisted) values. `file` is the
    /// spawn-era row the write targeted: the drain may only land on it, never on a
    /// same-name twin.
    MeasuredPersistFailed {
        model: String,
        data: MeasuredData,
        file: Option<PathBuf>,
    },
    /// A harness-owned writer just rewrote a resident model's own yaml (the measured
    /// persist's success path, or `har_config_model`'s edit — the complete list):
    /// re-baseline that resident's spawn-side capture so the harness's own write is
    /// never a "config changed since launch" (Axis-2 interlock 5). Carries the
    /// fingerprint the writer took directly after its atomic rename, off-thread.
    ModelFileWritten {
        model: String,
        path: PathBuf,
        fingerprint: crate::model::FileFingerprint,
    },
    /// Periodic sweep from the tick task: idles out occupants per category policy.
    Tick,
    Event {
        job: JobId,
        event: Event,
    },
    Status {
        reply: oneshot::Sender<SchedulerStatus>,
    },
    Catalog {
        reply: oneshot::Sender<crate::model::scan::Catalog>,
    },
}

impl Clone for ActorMsg {
    /// Only the persist-fail reconcile is ever cloned: it must leave the blocking
    /// write with the bytes it wrote, while a reply channel cannot be cloned and the
    /// rest of the variants never cross a thread boundary twice.
    fn clone(&self) -> Self {
        match self {
            Self::MeasuredPersistFailed { model, data, file } => Self::MeasuredPersistFailed {
                model: model.clone(),
                data: data.clone(),
                file: file.clone(),
            },
            _ => unreachable!("only the persist-fail reconcile is cloned"),
        }
    }
}

/// Result of an async step, sent back to the actor by the spawned step task.
#[derive(Debug)]
enum Event {
    /// Kill + release-evidence poll finished (G12). `Ok` carries the measured
    /// accept-to-confirmed duration of that release, stamped by
    /// [`ActorState::spawn_kill_confirm`] and elapsed inside the step task, so
    /// the actor records the sample without ever consulting wall clock itself.
    Released(Result<Duration, SpawnError>),
    /// Engine start finished.
    Spawned(Result<Spawned, SpawnError>),
    /// Readiness loop finished (terminal tri-state only) plus the engine's
    /// self-reported model id when ready and discoverable (Step 10, F-4).
    Probed(Probe, Option<String>),
    /// Witness sample for admission returned (Err = no reading, which refuses unless
    /// the witness declares `ledger_only`).
    Admitted(Result<crate::budget::MemorySample, String>),
    /// Post-release memory settle finished (Err carries the numbers).
    Settled(Result<(), String>),
}

/// A slot occupant as owned by the actor.
struct Occupant {
    model: String,
    need: Gibibytes,
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
}

struct ActorState {
    cfg: AppConfig,
    models_dir: PathBuf,
    catalog: Vec<ModelEntry>,
    catalog_errors: Vec<crate::model::scan::ScanError>,
    adapters: BTreeMap<String, Arc<dyn EngineAdapter>>,
    runtime: Arc<dyn EngineRuntime>,
    witness: Arc<dyn crate::budget::VramWitness>,
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
    /// R11 fit cache (in-process only, never persisted): probe argv hash → the
    /// projection and the model-file fingerprint it was computed against. A
    /// figure is valid precisely while its argv AND the calculator that minted
    /// it are unchanged — the version epoch and fingerprint checks are the two
    /// halves of that identity.
    fit_cache: HashMap<u64, FitEntry>,
    /// The fit-tool version line the current cache was minted under. `None` =
    /// never confirmed; no figure books before the first answer, and a version
    /// change voids the whole cache (a figure belongs to the calculator that
    /// minted it).
    fit_version: Option<String>,
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

/// How many recent confirmed releases the wait-estimate ring keeps. Bounded by
/// design (same posture as the log ring): the median answers a refusal, and 32
/// confirmed releases describe recent release behavior as well as a longer
/// window without letting one old swap dominate.
const RELEASE_SAMPLE_CAP: usize = 32;

/// Rolling ring of the harness's own measured release durations, in seconds
/// (accept to driver-confirmed). Memory-only by ruling: nothing is persisted,
/// so a fresh process has no sample and quotes no estimate — the honest
/// starting state, not a missing cache (operator ruling 2026-11-04).
struct ReleaseSamples {
    seconds: std::collections::VecDeque<f64>,
}

impl ReleaseSamples {
    fn new() -> Self {
        Self {
            seconds: std::collections::VecDeque::with_capacity(RELEASE_SAMPLE_CAP),
        }
    }

    /// Appends one confirmed release, dropping the oldest sample past the cap.
    fn push(&mut self, elapsed: Duration) {
        if self.seconds.len() == RELEASE_SAMPLE_CAP {
            self.seconds.pop_front();
        }
        self.seconds.push_back(elapsed.as_secs_f64());
    }

    /// The median release duration in seconds. Odd count: the middle value.
    /// Even count: the mean of the two middle values (the exact-even rule —
    /// it never invents a sample that was not measured). `None` on an empty
    /// ring: with no confirmed release there is no basis to quote an estimate.
    fn median_s(&self) -> Option<f64> {
        if self.seconds.is_empty() {
            return None;
        }
        let mut sorted: Vec<f64> = self.seconds.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        let mid = sorted.len() / 2;
        Some(if sorted.len() % 2 == 1 {
            sorted[mid]
        } else {
            (sorted[mid - 1] + sorted[mid]) / 2.0
        })
    }
}

/// The occupant's last use: the proxy-side hint when present, else load time.
fn state_last_used(occupant: &Occupant, last_used: &HashMap<String, Instant>) -> Instant {
    last_used
        .get(&occupant.model)
        .copied()
        .unwrap_or(occupant.loaded_at)
        .max(occupant.loaded_at)
}

/// Runs `scan_dir`'s synchronous filesystem work on a blocking thread (the
/// reactor-offload rule: `read_dir`, one read per yaml, one stat per model must
/// never park a task that answers the control plane). `Err` names the failed
/// blocking task itself (panic/cancel), not a scan report — scan failures travel
/// inside the [`Catalog`].
async fn scan_models_dir(dir: PathBuf) -> Result<Catalog, String> {
    let reported = dir.clone();
    tokio::task::spawn_blocking(move || crate::model::scan::scan_dir(&dir))
        .await
        .map_err(|error| format!("{error} scanning {}", reported.display()))
}

/// The filesystem half of a catalog refresh: the modelconf walk **and** the configured
/// `llama-server.exe` stat, on blocking threads, concurrent with each other.
///
/// The stat lives here rather than in [`ActorState::apply_catalog`] because that runs
/// during the single writer's turn: `llama_server_exe` is operator config, so a
/// `Path::exists()` there parks the actor behind a sleeping USB or a disconnected NAS
/// volume while the listener is already bound — every command, `/har/status` poll and
/// tray tick queues behind it, the A1-class stall FINDINGS-14 [1] and FINDINGS-13 [6]
/// moved the scans off-thread for. This was the last `std::fs` touch on the actor
/// (FINDINGS2-04 [2]).
///
/// # Arguments
///
/// * `dir` - the `ModelConf` directory to walk.
/// * `llama_server_exe` - the configured binary to check.
///
/// # Returns
///
/// The scan result, plus the page-visible error for a missing binary (`None` when there
/// is nothing to report: the binary is present, or the blocking task itself died, where
/// silence beats a false alarm).
async fn scan_with_binary_check(
    dir: PathBuf,
    llama_server_exe: PathBuf,
) -> (Result<Catalog, String>, Option<ScanError>) {
    let (scan, binary) = tokio::join!(
        scan_models_dir(dir),
        tokio::task::spawn_blocking(move || missing_binary_error(&llama_server_exe))
    );
    (scan, binary.unwrap_or(None))
}

/// The `EngineBinaryMissing` report for a binary the config points at but the volume
/// does not have. Pure apart from the one stat, which is why it runs inside
/// [`scan_with_binary_check`]'s blocking task and not on the actor.
#[must_use]
fn missing_binary_error(llama_server_exe: &std::path::Path) -> Option<ScanError> {
    (!llama_server_exe.exists()).then(|| ScanError {
        kind: ScanKind::EngineBinaryMissing,
        file: "config".to_string(),
        reason: format!(
            "llama-server.exe not found at {}; llama loads will fail",
            llama_server_exe.display()
        ),
    })
}

/// The filesystem evidence one catalog application judges: the resident captures
/// and the queued rows' files, stat'ed **off the actor** by [`gather_drift_stats`]
/// on the reload's blocking legs (FINDINGS-07 [8] — the scan half moved off-thread
/// in FINDINGS-13 [6]/FINDINGS2-04 [2]; these stats were the reload turn's last
/// inline `std::fs`).
///
/// Keyed by path, with a three-state answer the drift rounds must respect:
/// - absent → the path was not gathered for this round (state moved after the
///   reload was dispatched): absence of evidence, the verdict stays as it is;
/// - present with `None` → the gather ran and the file could not be stat'ed
///   (gone/ejected): for the resident marker this IS the accusation the rule
///   already made ("the file demonstrably is not what it was"), for a queued row
///   it never retires;
/// - present with `Some(fp)` → the current bytes' fingerprint.
#[derive(Debug, Default)]
struct DriftStats {
    stats: HashMap<PathBuf, Option<crate::model::FileFingerprint>>,
    /// The parked rows the reload dispatch saw. A row absent from this set
    /// registered after the dispatch: its fingerprint is by construction newer
    /// than any stat gathered before its birth, so judging it could only invent
    /// a "changed since queued" — late rows are kept unconditionally.
    queued_at_dispatch: HashSet<JobId>,
}

/// The blocking-thread half of a reload's drift round: one stat per named path.
/// Runs inside `spawn_blocking` on the reload task, never on the actor; a path
/// whose stat fails is inserted with `None` (evidence of absence), and only the
/// paths the actor named are ever inserted (so an absent key really means
/// "not observed this round").
fn gather_drift_stats(paths: &[PathBuf]) -> DriftStats {
    let mut stats = HashMap::with_capacity(paths.len());
    for path in paths {
        stats.insert(path.clone(), crate::model::fingerprint(path));
    }
    DriftStats {
        stats,
        ..Default::default()
    }
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
                state.catalog_errors.push(ScanError {
                    kind: ScanKind::Other,
                    file: state.models_dir.display().to_string(),
                    reason: format!("rescan failed: {reason}"),
                });
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
            ActorMsg::FitVersionDone { gen, version } => state.on_fit_version(gen, version),
            ActorMsg::FitRowDone {
                gen,
                model,
                key,
                outcome,
            } => state.on_fit_row_done(gen, &model, key, outcome),
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
            events: tokio::sync::broadcast::channel(64).0,
            tx,
            snapshot_in_flight: HashSet::new(),
            serve_id_refresh: HashSet::new(),
            serve_id_silence_warned: HashSet::new(),
            reconcile_in_flight: false,
            last_reported_stranded: None,
            fit_cache: HashMap::new(),
            fit_version: None,
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
                .map(|o| o.need.0)
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
    fn in_flight_need(&self, except: JobId) -> f64 {
        self.jobs
            .iter()
            .filter(|(id, job)| {
                **id != except
                    && job.kind == JobKind::Load
                    && !job.terminal
                    && job.state.reserves_vram()
            })
            .map(|(_, job)| job.need.map_or(0.0, |need| need.0))
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

    /// What the occupant's own container reported it reserves (R9), read off the
    /// spawn anchor of the job that produced this exact occupant (`model` + `ticket`
    /// — never the model name alone, which a re-minted twin can steal): Some only
    /// for a ninfer container spawn whose anchor folded an `engine_report` figure.
    /// The API's memory-bar sum derives from this so `/har/status` and the SSE push
    /// quote ONE source (review 2026-10-31: threading the sum only through the SSE
    /// build left the poll path — the only path a token-gated page has — forever
    /// null, and a fixture carrying the value hid it).
    fn container_reported_gb(&self, slot: &Slot) -> Option<f64> {
        let occupant = slot.occupant.as_ref()?;
        let job = self.jobs.iter().find_map(|(_, job)| {
            (job.model == occupant.model && job.spawned.as_ref() == Some(&occupant.ticket))
                .then_some(job)
        })?;
        let anchor = job.measured.as_ref()?;
        (job.engine == crate::ownership::NINFER_ENGINE
            && anchor.container
            && anchor.attribution == Some(crate::model::MeasuredAttribution::EngineReport))
        .then(|| anchor.range.map(|(high, _)| high))
        .flatten()
    }

    fn snapshot(&self) -> SchedulerStatus {
        let now = Instant::now();
        SchedulerStatus {
            catalog_errors: self.catalog_errors.clone(),
            // One accessor derives the payload from the dedupe state (empty means
            // absent, never an empty list that would keep the key alive).
            stranded_containers: self
                .last_reported_stranded
                .as_ref()
                .filter(|names| !names.is_empty())
                .cloned(),
            // R11: only stamps earned under the CURRENT epoch answer — the page
            // must never show a green carried across a rescan, and reading this
            // is pure memory (no fs, no clock), like every other snapshot field.
            fit_validated: self
                .fit_stamps
                .iter()
                .filter(|(_, (epoch, ok))| *ok && *epoch == self.fit_gen())
                .map(|(model, _)| model.clone())
                .collect(),
            slots: self
                .slots
                .iter()
                .map(|s| SlotStatus {
                    category: s.category,
                    index: s.index,
                    model: s.occupant.as_ref().map(|o| o.model.clone()),
                    port: s.occupant.as_ref().map(|o| o.port),
                    pid: s.occupant.as_ref().map(|o| o.ticket.pid),
                    need_gib: s.occupant.as_ref().map(|o| o.need.0),
                    idle_s: s.occupant.as_ref().map(|o| {
                        let last = state_last_used(o, &self.last_used);
                        now.duration_since(last).as_secs()
                    }),
                    serve_id: s.occupant.as_ref().and_then(|o| o.serve_id.clone()),
                    loading_job: s.job,
                    // The one derivation of the slot label (see
                    // `ActorState::slot_state`): a committed release reads
                    // `releasing` until the driver confirms it.
                    state: self.slot_state(s).to_string(),
                    launched_at: s
                        .occupant
                        .as_ref()
                        .map(|o| crate::model::iso8601_utc(o.launched_at_wall)),
                    stale_config: s.occupant.as_ref().map(|o| o.stale_config),
                    container_reported_gb: self.container_reported_gb(s),
                    loaded_at: s.occupant.as_ref().map_or(now, |o| o.loaded_at),
                })
                .collect(),
            jobs: self
                .jobs
                .iter()
                .map(|(id, j)| JobStatus {
                    id: *id,
                    kind: j.kind,
                    path: j.path,
                    model: j.model.clone(),
                    category: j.category,
                    state: j.state,
                    terminal: j.terminal,
                    outcome: j.outcome.clone(),
                    error: j.error.as_ref().map(|e| e.to_string()),
                    need_gib: j.need.map(|n| n.0),
                    free_at_admit_gib: j.free_at_admit.map(|n| n.0),
                    observed_at_admit_gib: j.observed_at_admit,
                    gate_target_gib: j.gate_target,
                    victims_released: j.victims_released.clone(),
                    warnings: j.warnings.clone(),
                    is_fault: fault_of(j.terminal, j.kind, j.outcome.as_deref(), j.error.is_some()),
                    timings: j.timings,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests;
