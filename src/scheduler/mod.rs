//! The scheduler actor (DESIGN Step 2, choice C LOCKED): one task owns all slot and
//! budget state; everyone else sends commands and holds a oneshot waiter.
//!
//! Invariants encoded here:
//! - single writer: only the actor task mutates slots, jobs, and the ledger;
//! - the actor never performs engine I/O inline — every step runs as a spawned task
//!   whose result returns as an [`Event`], so control-plane calls (`status`, `cancel`,
//!   `unload`) never queue behind a load (defect A1);
//! - a dropped waiter is harmless (F-1) and a second identical load attaches to the
//!   in-flight job instead of racing it (F-6);
//! - a same-category load with a different model supersedes: the running job is
//!   cancelled and the new one starts when the slot returns to `Idle` ("rapid A->B
//!   ends with exactly B", T3.4);
//! - every exit path installs an occupant or reports an error — never neither (F-6
//!   funnel) — enforced by walking only [`transition_allowed`] edges.

mod vocab;

pub use vocab::{transition_allowed, Command, JobId, SwapState};

use crate::budget::{self, AdmissionError, BudgetFacts, Gibibytes};
use crate::config::{AppConfig, CategoryId};
use crate::engine::{
    EngineAdapter, EngineRuntime, LaunchSpec, Probe, ReadinessSpec, SpawnError, Spawned,
};
use crate::model::scan::{Catalog, ScanError, ScanKind};
use crate::model::ModelEntry;
use crate::ownership::{ResourceKind, SpawnTicket};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

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
        /// required (used 58.2 of 72.0)`.
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
    /// The command was accepted (cancel/unload).
    Done,
}

/// Terminal receipt of one load (Step 12 idempotency: a repeat returns this).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SwapReceipt {
    /// Job that performed the load.
    pub job: JobId,
    /// Harness name.
    pub model: String,
    /// Category slot.
    pub category: CategoryId,
    /// Engine port.
    pub port: u16,
    /// The engine's self-reported id (F-4); proxy rewriting targets this.
    pub serve_id: Option<String>,
}

/// Immutable status snapshot (Step 2: reads never probe and never mutate).
#[derive(Debug, Clone, Serialize)]
pub struct SchedulerStatus {
    /// Every materialized slot (AUX expands to `max_models` entries).
    pub slots: Vec<SlotStatus>,
    /// Job ledger, oldest first.
    pub jobs: Vec<JobStatus>,
    /// Catalog problems from the last scan (bad files, unresolved defaults).
    pub catalog_errors: Vec<crate::model::scan::ScanError>,
}

/// One slot as seen by `/har/status` (Step 12).
#[derive(Debug, Clone, Serialize)]
pub struct SlotStatus {
    /// Category of the slot.
    pub category: CategoryId,
    /// Index within its category (0-based).
    pub index: usize,
    /// Serving model, if any.
    pub model: Option<String>,
    /// Engine port, if occupied.
    pub port: Option<u16>,
    /// Child pid of the occupying engine (0 when the runtime reports none).
    pub pid: Option<u32>,
    /// Admission cost of the occupant in GiB.
    pub need_gib: Option<f64>,
    /// Seconds since the occupant was last used (None when empty; Step 5).
    pub idle_s: Option<u64>,
    /// The engine's self-reported id, when discovered (F-4).
    pub serve_id: Option<String>,
    /// Job currently running for this slot, if any.
    pub loading_job: Option<JobId>,
    /// Wall-clock launch of the occupant, server-formatted ISO-8601 UTC
    /// (Axis-2 ruling 2026-10-29: the operator sees when the model launched; the
    /// page prints this string verbatim — no date math in the browser). Empty slot:
    /// absent.
    pub launched_at: Option<String>,
    /// The server-computed `stale_config` verdict for the occupant (Axis 2 = B):
    /// `true` when the model's yaml differs — by the install-time fingerprint —
    /// from the bytes this engine launched from. Refreshed only at install, on the
    /// reload turn, and on harness write-backs; reading it performs no stat.
    /// Empty slot: absent.
    pub stale_config: Option<bool>,
    /// When the occupant was loaded (copy of the actor's `Occupant::loaded_at`).
    /// Empty slots carry the snapshot time, mirroring the `map_or(Instant::now(),
    /// |o| o.loaded_at)` policy of the scheduler's own LRU picks. Not serialized:
    /// an `Instant` has no wall-clock meaning outside the process, so `/har/status`
    /// keeps exposing `idle_s`. The no-`model` proxy fallback sorts untouched
    /// occupants by this (FINDINGS-06 [1]: never fabricate an instant instead).
    #[serde(skip)]
    pub loaded_at: Instant,
}

/// One job ledger entry (Step 12: this is what `loading: bool` always pretended to be).
#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    /// Job id.
    pub id: JobId,
    /// Load or unload (A3: the headline fault only counts loads).
    pub kind: JobKind,
    /// Model the job acts on.
    pub model: String,
    /// Category the job targets.
    pub category: CategoryId,
    /// Current machine state.
    pub state: SwapState,
    /// Whether the job reached a terminal state.
    pub terminal: bool,
    /// Terminal outcome note ("loaded", "unloaded", ...).
    pub outcome: Option<String>,
    /// Terminal error, human-readable with the numbers (G13).
    pub error: Option<String>,
    /// Whether this row is a genuine fault — a failed LOAD, deliberate cancels
    /// excluded. Derived once here (`fault_of`) so the row dot and the headline can
    /// never disagree (duplication review, 2026-10-26).
    pub is_fault: bool,
    /// Admission cost in GiB, once computed.
    pub need_gib: Option<f64>,
    /// Free VRAM at admit time in GiB (the number that was used).
    pub free_at_admit_gib: Option<f64>,
    /// Observed physical free (total - used) at admit; `None` when the witness
    /// was down (rehearsal forensics, 2026-10-26).
    pub observed_at_admit_gib: Option<f64>,
    /// The gate this job had to satisfy before spawn: max(need, launch floor).
    pub gate_target_gib: Option<f64>,
    /// Cross-category victims this job evicted (Step 6, G13 decision inputs).
    pub victims_released: Vec<String>,
    /// Non-fatal observations (serve_model mismatch, Step 10).
    pub warnings: Vec<String>,
    /// Step timings (§0.3).
    pub timings: JobTimings,
}

/// The single fault rule: a terminal **LOAD** whose outcome was not a deliberate
/// cancel and that carries an error. Unload failures are visible in the jobs strip
/// only; cancels (unload-during-load, supersede) are operator policy, not faults.
/// The headline, the tray and the per-row dot all read this — two surfaces deriving
/// the predicate separately is how a cancelled load came to pin a red dot
/// (duplication review, 2026-10-26).
#[must_use]
pub fn fault_of(terminal: bool, kind: JobKind, outcome: Option<&str>, has_error: bool) -> bool {
    terminal && kind == JobKind::Load && outcome != Some("cancelled") && has_error
}

/// The job that owns the sticky headline fault (A3) — the single derivation
/// shared by the page header and the tray tooltip so the surfaces cannot drift.
/// Rules: only LOAD jobs count (unload failures are visible in the jobs strip);
/// deliberate cancels — `outcome == "cancelled"`, e.g. unload-during-load or a
/// supersede — are operator policy, not faults (the API itself maps `Cancelled`
/// to 409); a fault stays pinned until **that same model in that same category**
/// completes a later load successfully, so unrelated loads and idle-sweep
/// unloads never mask a real failure.
#[must_use]
pub fn headline_fault(jobs: &[JobStatus]) -> Option<&JobStatus> {
    let fault = jobs.iter().rev().find(|job| {
        fault_of(
            job.terminal,
            job.kind,
            job.outcome.as_deref(),
            job.error.is_some(),
        )
    })?;
    let cleared = jobs.iter().any(|later| {
        later.id > fault.id
            && later.terminal
            && later.kind == JobKind::Load
            && later.outcome.as_deref() != Some("cancelled")
            && later.model == fault.model
            && later.category == fault.category
            && later.error.is_none()
    });
    (!cleared).then_some(fault)
}

/// GiB committed to loads that are admitted but not yet installed — the same set the
/// admission gate debits, so the page's "booked" figure can never disagree with the
/// number a refusal quotes (review, 2026-10-26).
#[must_use]
pub fn booking_gibs(jobs: &[JobStatus]) -> f64 {
    jobs.iter()
        .filter(|job| job.kind == JobKind::Load && !job.terminal && job.state.reserves_vram())
        .map(|job| job.need_gib.unwrap_or(0.0))
        .sum()
}

/// Per-job step timings in milliseconds (§0.3: measurable, not argued about).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct JobTimings {
    /// Submit to first actor handling.
    pub queue_ms: Option<u64>,
    /// Submit to admission decision.
    pub admit_ms: Option<u64>,
    /// Release of the previous occupant / victims.
    pub release_ms: Option<u64>,
    /// Engine start.
    pub spawn_ms: Option<u64>,
    /// Readiness confirmed.
    pub ready_ms: Option<u64>,
    /// Occupant installed.
    pub install_ms: Option<u64>,
}

type Waiter = oneshot::Sender<Result<Reply, SchedulerError>>;

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

    /// Unloads every occupant of `category`; idempotent (an empty category is `Done`).
    ///
    /// # Errors
    ///
    /// [`SchedulerError::ActorGone`] when the actor is no longer running.
    pub async fn unload(&self, category: CategoryId) -> Result<Reply, SchedulerError> {
        self.call(Command::Unload { category }).await
    }

    /// Restarts `model` in `category`: kills the (dead) occupant and loads it
    /// fresh — the T10.1 crash-retry primitive.
    ///
    /// # Errors
    ///
    /// [`SchedulerError`] variants as for [`SchedulerHandle::swap`].
    pub async fn restart(
        &self,
        model: &str,
        category: CategoryId,
    ) -> Result<Reply, SchedulerError> {
        self.call(Command::Restart {
            model: model.to_string(),
            category,
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
    /// (FINDINGS-14 [1]: an operator-chosen `modelconf_dir` can live on a sleeping
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
    },
    /// The blocking write-back of a measured triple was refused by the disk
    /// (Fork C, FINDINGS-13 [4] follow-through): the actor must stop booking in
    /// memory what disk rejected. Carries the exact triple the failed write
    /// attempted, so a reading that has since been superseded never drains the
    /// newer (possibly persisted) values. `offload` is `None` for engines outside
    /// `OFFLOAD_ENGINES` — their persist carried only the pair, and their memory
    /// never held a stamp to drain. `file` is the spawn-era row the write targeted:
    /// the drain may only land on it, never on a same-name twin.
    MeasuredPersistFailed {
        model: String,
        high: f64,
        low: f64,
        offload: Option<bool>,
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

/// Result of an async step, sent back to the actor by the spawned step task.
#[derive(Debug)]
enum Event {
    /// Kill + release-evidence poll finished (G12).
    Released(Result<(), SpawnError>),
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

/// What the measured fold is allowed to say about a running engine, and where it may
/// write it. Captured at `begin_spawn` from the catalog row the launch was built from,
/// never re-read at sample time (`research/reload-slot-drift-2026-10-28.md` §1 rows
/// 6-10): `reload()` replaces `self.catalog` wholesale while the process keeps the argv
/// it started with, so a fold that consulted the live catalog would stamp a range for a
/// launch that never happened — and because the flip-void rule compares that very stamp
/// with the same live bytes, it would also disarm the rule built to catch the lie.
#[derive(Debug, Clone, PartialEq)]
struct MeasuredAnchor {
    /// The yaml that owns this reading, by **file** identity. A reload that renames the
    /// row keeps the bytes and moves the name, and a re-minted twin steals the name, so
    /// the name is not an identity a fold may write to (FINDINGS-13 [3]). `None` = the
    /// row had no backing file when this engine spawned, so nothing on disk owns the
    /// reading and the fold stays closed.
    file: Option<PathBuf>,
    /// The `cpu_offload` the engine actually started under, already gated through
    /// [`crate::engine::llama::offload_live`]: an engine that never reads the key keeps
    /// `None` and mints no meaningless stamp (§20-9 Fork C knob-table row 1).
    offload: Option<bool>,
    /// The range folded across this spawn's samples, seeded from the row's persisted
    /// pair at spawn. Seeding is what keeps a mid-life reload — which drops the
    /// in-memory pair the moment a stamp no longer matches — from silently re-seeding
    /// an accumulated range from the one process still on the card.
    range: Option<(f64, f64)>,
    /// The `serve_model` the row declared when this engine was launched
    /// (FINDINGS-BATCH-2026-10-28L [L-2]). The install-time mismatch verdict is about
    /// the process on the card, so it is judged against these bytes: a reload landing
    /// inside the cold-start window must not manufacture or erase the warning, and a
    /// same-name twin must not lend this one its id.
    serve_model: Option<String>,
}

impl MeasuredAnchor {
    /// Captures the anchor from the entry a spawn is about to be built from. One
    /// derivation, called by [`ActorState::begin_spawn`] (and by the pins that seat a
    /// spawn), so the stamp can never be read from a different side of the fold than
    /// the engine's argv was.
    #[must_use]
    fn at_spawn(entry: &ModelEntry) -> Self {
        Self {
            file: entry.file.clone(),
            offload: crate::engine::llama::offload_live(&entry.engine).then(|| {
                entry
                    .raw
                    .get("cpu_offload")
                    .is_some_and(|value| crate::engine::llama::jinja_on(value))
            }),
            range: match (entry.measured_gb, entry.measured_min_gb) {
                (Some(high), Some(low)) => Some((high, low)),
                // A lone high end is still the floor of what this spawn may book; a
                // lone low end cannot exist (the scanner rejects an orphan pair).
                (Some(high), None) => Some((high, high)),
                (None, _) => None,
            },
            serve_model: entry.serve_model.clone(),
        }
    }
}

/// The serve_model mismatch verdict, decided against the SPAWN-era row
/// (FINDINGS-BATCH-2026-10-28L [L-2]).
///
/// Taking the anchor instead of the catalog is the whole point: the pre-fix site read
/// `self.catalog.iter().find(|m| m.name == job.model)` minutes after `begin_spawn`, so
/// under a mid-serve reload the warning compared the engine's real id against yaml that
/// arrived after the launch (a false warning, or a missed one), and under same-name
/// twins it could read the wrong twin's `serve_model`.
///
/// # Arguments
///
/// * `anchor` - the job's spawn-era anchor, `None` for a job that never spawned.
/// * `discovered` - the id the engine actually reported, `None` when discovery saw
///   nothing.
///
/// # Returns
///
/// The warning sentence, or `None` when there is nothing to report.
#[must_use]
fn serve_model_warning(
    anchor: Option<&MeasuredAnchor>,
    discovered: Option<&str>,
) -> Option<String> {
    let declared = anchor?.serve_model.as_deref()?;
    let actual = discovered?;
    (declared != actual).then(|| {
        format!(
            "serve_model `{declared}` is not reported by the engine; using its real id `{actual}`"
        )
    })
}

/// A release target during Quiesce/Dispose (Step 6 calls these victims).
#[derive(Clone)]
struct Victim {
    model: String,
    ticket: SpawnTicket,
    /// The slot the victim occupies, cleared when its release is confirmed.
    slot: usize,
    /// Admission cost freed by this release (ledger arithmetic, Step 6 re-check).
    cost: f64,
    /// True when this victim was a cross-category eviction (vs a same-slot
    /// replacement); recorded in the job ledger (G13).
    evicted: bool,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize)]
pub enum JobKind {
    Load,
    Unload,
}

struct Job {
    kind: JobKind,
    model: String,
    engine: String,
    category: CategoryId,
    state: SwapState,
    terminal: bool,
    outcome: Option<String>,
    error: Option<SchedulerError>,
    need: Option<Gibibytes>,
    /// Cost class from the model yaml (fixed|greedy|exact) — drives the gate shape.
    cost: crate::model::CostClass,
    /// Model's `launch_only_with_at_least_vram_free` in GiB, if set.
    launch_floor: Option<f64>,
    /// Model's per-load readiness deadline (`load_timeout:`); `None` = config default.
    load_timeout_s: Option<u64>,
    /// The `available` value (free_at_admit + released) the release-minimality
    /// re-check must reach before spawning: `max(need, floor)` at plan time
    /// (floor alone for greedy). Without it, a greedy job whose booked need was
    /// clamped small would skip floor victims after the first release (review
    /// CRITICAL, 2026-10-26).
    gate_target: Option<f64>,
    free_at_admit: Option<Gibibytes>,
    /// Observed physical free (total - used) at admit; `None` when the witness
    /// was down. The release re-check and the settle gate take the more
    /// conservative figure against this (operator 2026-10-26).
    observed_at_admit: Option<f64>,
    /// Detached WSL container backing the spawn (ninfer): suppresses the
    /// pid-attributed measured footprint, which is meaningless for a client
    /// process that exits before the container is ready (review, 2026-10-26).
    container: Option<String>,
    timings: JobTimings,
    t0: Instant,
    waiters: Vec<Waiter>,
    tasks: Vec<JoinHandle<()>>,
    /// The engine this job spawned, once started (kill target on failure/cancel).
    spawned: Option<SpawnTicket>,
    /// Readiness contract of the running spawn.
    readiness: Option<ReadinessSpec>,
    /// Spawn-era measured anchor of the running spawn (`None` before `begin_spawn`).
    /// It rides the row exactly like `readiness` does: both describe what was
    /// launched, and readiness can run for minutes (a strata cold start) while a
    /// `reload()` replaces the catalog underneath them.
    measured: Option<MeasuredAnchor>,
    /// The previous occupant this job must release before spawning.
    victim: Option<Victim>,
    /// Cross-category victims queued by the eviction planner (Step 6), released
    /// one at a time after the same-slot replacement (if any).
    victims: Vec<Victim>,
    /// Cross-category victims whose release is confirmed (observability, G13).
    victims_released: Vec<String>,
    /// Ledger cost freed by confirmed releases so far (Step 6 minimality check).
    released_cost: f64,
    /// Ledger need booked for *other* occupants when this engine was admitted. The
    /// measured diff subtracts only what appeared after this point, so a tenant that
    /// was already resident is not double-counted (review, 2026-10-27).
    other_need_at_admit: f64,
    /// Outcome wording for unload jobs ("unloaded" / "idle timeout").
    note: Option<String>,
    /// Non-fatal observations recorded during the job (serve_model mismatch...).
    warnings: Vec<String>,
    slot: Option<usize>,
    /// The model's yaml as it was when this row **queued** (reload-drift Axis 1,
    /// operator ruling 2026-10-29): the bytes the parked intent was taken on, for
    /// [`ActorState::invalidate_queued_loads`] to compare when a rescan lands.
    /// `None` = a synthetic entry, or the stat was unreadable at registration:
    /// nothing to compare, nothing to retire on.
    queue_fingerprint: Option<crate::model::FileFingerprint>,
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
    state.apply_catalog(catalog, binary);
    while let Some(msg) = rx.recv().await {
        match msg {
            ActorMsg::Command { cmd, reply } => state.on_command(cmd, reply),
            ActorMsg::CatalogLoaded {
                catalog,
                seq,
                binary,
            } => {
                if seq > state.last_reload_applied {
                    state.apply_catalog(catalog, binary);
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
            } => state.on_measured(&model, &ticket, total_diff, own, total_gib),
            ActorMsg::MeasuredPersistFailed {
                model,
                high,
                low,
                offload,
                file,
            } => state.on_measured_persist_failed(&model, high, low, offload, file),
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
            slots,
            jobs: BTreeMap::new(),
            last_used: HashMap::new(),
            pending: HashMap::new(),
            unload_after_finish: HashSet::new(),
            pending_reload: Vec::new(),
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

    /// Rescans `ModelConf` and cross-checks `default_model`s (§0.2 step 6, Step 9)
    /// off the actor's own thread: the whole synchronous filesystem walk
    /// (`read_dir`, one read per yaml, one stat per model) runs on a blocking
    /// thread so the single writer never parks on disk (FINDINGS-13 [6]). The
    /// caller applies the result with [`ActorState::apply_catalog`] — at boot
    /// before the first message, on reload when the result returns as
    /// [`ActorMsg::CatalogLoaded`].
    async fn scan_blocking(&self) -> (Result<Catalog, String>, Option<ScanError>) {
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
    /// FINDINGS2-04 [2]) is carried into the report, and every `default_model` must
    /// resolve.
    fn apply_catalog(&mut self, catalog: Catalog, binary: Option<ScanError>) {
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
        self.invalidate_queued_loads();
        self.refresh_stale_markers();
    }

    /// The resident half of reload (Axis 2 = B, operator ruling 2026-10-29): on the
    /// reload turn — and only here, at install, and on a harness write-back — the
    /// `stale_config` verdict is recomputed for every resident: the yaml it launched
    /// from, re-stat'ed, against the install-time capture. The status/SSE path reads
    /// this cache and performs zero fs syscalls (FINDINGS2-04 [2]'s one-stat-per-event
    /// rule extended to the marker; pinned by the api-side hidden-file counter-probe).
    ///
    /// Verdict rules, stated plainly:
    /// - no capture (synthetic row, or the stat failed at install): never accuses;
    ///   absence of evidence is not evidence of change (the queued rows' KEEP posture).
    /// - capture exists, current stat `None` (file deleted/volume ejected since
    ///   launch): accuses — the file demonstrably is not what it was.
    /// - capture exists, stat differs (either leg): accuses. The comparison is
    ///   against the SPAWN capture, so re-saving byte-identical content does not
    ///   clear the verdict (the mtime moved) — honest conservatism: the operator's
    ///   gesture that matters is unload + reload, and a respawned engine re-captures
    ///   cleanly.
    fn refresh_stale_markers(&mut self) {
        for slot in &mut self.slots {
            let Some(occupant) = slot.occupant.as_mut() else {
                continue;
            };
            let Some((file, then)) = occupant.spawn_config.as_ref() else {
                continue;
            };
            let then = *then;
            occupant.stale_config = crate::model::fingerprint(file) != Some(then);
        }
    }

    /// Re-baselines the spawn-side capture after a **harness-owned** write to a
    /// resident's own yaml (Axis-2 interlock 5: the measured fold and the API's flag
    /// edit must never amber their own model). The legitimate mutators are a CLOSED
    /// set — the measured persist (this file's fold success path) and
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
    fn on_model_file_written(
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
    /// either side (`fingerprint` = `None`: unreadable stat) is the absence of
    /// **evidence**, never evidence of change — the queue is kept (pinned by
    /// `a_rescan_keeps_a_queue_whose_fingerprint_cannot_show_a_change`).
    ///
    /// Budget no-leak: a never-armed row books nothing (`reserves_vram` excludes
    /// `Idle`), so retirement cannot strand a reservation; what it must not strand
    /// is the queue place — `finish_queued_load` clears it, so a later finish of
    /// the predecessor can never arm a retired row (verified by the integration
    /// pins' spawn arithmetic).
    fn invalidate_queued_loads(&mut self) {
        let candidates: Vec<JobId> = self
            .pending
            .values()
            .copied()
            .filter(|job_id| {
                self.jobs.get(job_id).is_some_and(|job| {
                    job.kind == JobKind::Load && !job.terminal && job.state == SwapState::Idle
                })
            })
            .collect();
        for job_id in candidates {
            let Some(job) = self.jobs.get(&job_id) else {
                continue;
            };
            let (model, category, queued) =
                (job.model.clone(), job.category, job.queue_fingerprint);
            let reason = match self.catalog.iter().find(|m| m.name == model) {
                None => Some("superseded by rescan (removed from the catalog)".to_string()),
                Some(entry) => {
                    let now = entry.file.as_deref().and_then(crate::model::fingerprint);
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
    fn drain_reload_waiters(&mut self, seq: u64) {
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
            Command::Restart { model, category } => {
                self.on_restart(model, category, reply);
                self.publish();
            }
            Command::Reload => {
                // FINDINGS-13 [6]: dispatch the scan off-thread and park the
                // waiter; the actor keeps answering ticks, events and every other
                // command against the old catalog until the result lands as
                // `ActorMsg::CatalogLoaded`, which is also what resolves the
                // waiter — so `/har/rescan` still means "new catalog applied".
                let tx = self.tx.clone();
                let dir = self.models_dir.clone();
                let exe = self.cfg.llama_server_exe.clone();
                let seq = self.next_reload;
                self.next_reload += 1;
                tokio::spawn(async move {
                    let (scan, binary) = scan_with_binary_check(dir, exe).await;
                    match scan {
                        Ok(catalog) => {
                            let _ = tx.send(ActorMsg::CatalogLoaded {
                                catalog,
                                seq,
                                binary,
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

    fn on_swap(&mut self, model: String, category: CategoryId, reply: Waiter) {
        let Some(entry) = self.catalog.iter().find(|m| m.name == model).cloned() else {
            let _ = reply.send(Err(SchedulerError::UnknownModel(model)));
            return;
        };
        let slots: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.category == category)
            .map(|(i, _)| i)
            .collect();
        // A4 fix: "already serving" matches name AND category.
        for &s in &slots {
            if let (Some(occupant), None) = (&self.slots[s].occupant, self.slots[s].job) {
                if occupant.model == model {
                    let _ = reply.send(Ok(Reply::Already {
                        model: occupant.model.clone(),
                        category,
                        port: occupant.port,
                        serve_id: occupant.serve_id.clone(),
                    }));
                    return;
                }
            }
        }
        // F-6 fix: an identical in-flight load gets the waiter attached, not a race.
        for &s in &slots {
            if let Some(job_id) = self.slots[s].job {
                if let Some(job) = self.jobs.get(&job_id) {
                    if job.kind == JobKind::Load && job.model == model && !job.terminal {
                        self.jobs
                            .get_mut(&job_id)
                            .expect("invariant: job exists when its id is on a slot")
                            .waiters
                            .push(reply);
                        return;
                    }
                }
            }
        }
        // Target: an empty slot first, else the LRU occupied one, else supersede.
        let idle = |s: usize| self.slots[s].job.is_none();
        let target = slots
            .iter()
            .copied()
            .find(|&s| idle(s) && self.slots[s].occupant.is_none())
            .or_else(|| {
                slots.iter().copied().filter(|&s| idle(s)).min_by_key(|&s| {
                    self.slots[s]
                        .occupant
                        .as_ref()
                        .map_or(Instant::now(), |o| o.loaded_at)
                })
            });
        match target {
            Some(target) => self.begin_load(target, entry, vec![reply]),
            None => {
                // Every slot runs a job: cancel the oldest, park this load (T3.4).
                let target = slots
                    .into_iter()
                    .min_by_key(|&s| {
                        self.slots[s]
                            .job
                            .and_then(|j| self.jobs.get(&j))
                            .map_or(Instant::now(), |j| j.t0)
                    })
                    .expect("invariant: the category has at least one slot");
                // The parked load gets its ledger row now, not when it starts: a
                // queued load must be visible at `/har/jobs` while its caller waits
                // (FINDINGS-02 [3]). One queue place per slot, so the load it
                // replaces is answered and terminalized rather than left stranded.
                let queued = self.register_queued_load(target, &entry, vec![reply]);
                if let Some(old) = self.pending.insert(target, queued) {
                    self.finish_queued_load(
                        old,
                        Err(SchedulerError::Cancelled(
                            "superseded by a newer load before it started".to_string(),
                        )),
                        "cancelled",
                    );
                }
                if let Some(job_id) = self.slots[target].job {
                    self.cancel_job(job_id, "superseded by a newer load");
                }
            }
        }
    }

    fn on_cancel(&mut self, job_id: JobId, reply: Waiter) {
        match self.jobs.get(&job_id) {
            Some(job) if job.terminal => {
                let _ = reply.send(Ok(Reply::Done));
            }
            Some(_) => {
                self.cancel_job(job_id, "cancelled by request");
                let _ = reply.send(Ok(Reply::Done));
            }
            None => {
                let _ = reply.send(Err(SchedulerError::UnknownJob(job_id)));
            }
        }
    }

    /// T10.1 crash-retry primitive: kill the (dead) occupant of `model`'s slot and
    /// load it fresh. Reuses the standard replacement path — the dead occupant
    /// becomes the victim, so release + settle + spawn all run normally.
    fn on_restart(&mut self, model: String, category: CategoryId, reply: Waiter) {
        let Some(entry) = self.catalog.iter().find(|m| m.name == model).cloned() else {
            let _ = reply.send(Err(SchedulerError::UnknownModel(model)));
            return;
        };
        let slot = self.slots.iter().position(|s| {
            s.category == category
                && s.job.is_none()
                && s.occupant.as_ref().is_some_and(|o| o.model == model)
        });
        match slot {
            Some(slot) => self.begin_load(slot, entry, vec![reply]),
            None => {
                // Already reaped (idle sweep) or never loaded: a plain load covers it.
                self.on_swap(model, category, reply);
            }
        }
    }

    fn on_unload(&mut self, category: CategoryId, reply: Waiter) {
        let targets: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.category == category)
            .map(|(i, _)| i)
            .collect();
        self.unload_slots(targets, reply);
    }

    /// T12.1: unload one model's slot; a loading swap for it is cancelled first and
    /// the slot is unloaded after that cancel terminalizes. A loading slot has no
    /// occupant yet — the in-flight job's model is the only way to find it (T8:
    /// unload during load = cancel, cleanup completes). A load queued on the model
    /// counts as a target too, so the row the operator can see is the row the command
    /// removes.
    fn on_unload_model(&mut self, model: &str, reply: Waiter) {
        let targets: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.occupant.as_ref().is_some_and(|o| o.model == model)
                    || s.job
                        .is_some_and(|j| self.jobs.get(&j).is_some_and(|job| job.model == model))
            })
            .map(|(i, _)| i)
            .collect();
        let queued: Vec<usize> = self
            .pending
            .iter()
            .filter(|(_, queued)| {
                self.jobs
                    .get(queued)
                    .is_some_and(|job| job.model == model && !job.terminal)
            })
            .map(|(slot, _)| *slot)
            .collect();
        let mut targets: Vec<usize> = targets.into_iter().chain(queued).collect();
        targets.sort_unstable();
        targets.dedup();
        self.unload_slots(targets, reply);
    }

    /// T12.4: unload every occupied slot; loading jobs are cancelled, and the slot
    /// they still leave occupied is unloaded when the cancel terminalizes.
    fn on_unload_all(&mut self, reply: Waiter) {
        let targets: Vec<usize> = (0..self.slots.len())
            .filter(|&i| self.slots[i].occupant.is_some() || self.slots[i].job.is_some())
            .collect();
        self.unload_slots(targets, reply);
    }

    /// Shared unload path: free slots start an unload job; a slot with an in-flight
    /// job has that job cancelled **and** — when it still holds an occupant — unloaded
    /// the moment the cancellation terminalizes (FINDINGS-02 [2]: an unload means
    /// cancel *and* unload, never either/or). The occupant survives a cancelled swap
    /// whenever the swap's release was never confirmed (chunk 4 [1] keeps an engine
    /// nobody proved dead installed), so cancelling alone can leave the model loaded
    /// and serving while the caller is told `Done`. A load parked on a targeted slot
    /// is dropped here too: an operator's unload must never be followed by the load
    /// they abandoned starting on the slot it just freed (FINDINGS-02 [3]).
    fn unload_slots(&mut self, slots: Vec<usize>, reply: Waiter) {
        for slot in slots {
            if let Some(queued) = self.pending.remove(&slot) {
                self.finish_queued_load(
                    queued,
                    Err(SchedulerError::Cancelled(
                        "unloaded by request before it started".to_string(),
                    )),
                    "cancelled",
                );
            }
            match self.slots[slot].job {
                Some(job_id) => {
                    // Flagged before the cancel: it may terminalize the job inline,
                    // and that finish is exactly where the flag is consumed.
                    let cancel_continues_into_unload =
                        self.jobs.get(&job_id).is_some_and(|job| !job.terminal)
                            && self.slots[slot].occupant.is_some();
                    if cancel_continues_into_unload {
                        self.unload_after_finish.insert(slot);
                    }
                    self.cancel_job(job_id, "unloaded by request");
                }
                None => {
                    if self.slots[slot].occupant.is_some() {
                        self.begin_unload_slot(slot, "unloaded");
                    }
                }
            }
        }
        let _ = reply.send(Ok(Reply::Done));
    }

    /// Starts one unload job for a specific slot (`note` becomes the job outcome:
    /// "unloaded" for operator/eviction paths, "idle timeout" for the sweep).
    fn begin_unload_slot(&mut self, slot: usize, note: &str) {
        let job_id = JobId(self.next_job);
        self.next_job += 1;
        let category = self.slots[slot].category;
        let victim = {
            let occupant = self.slots[slot]
                .occupant
                .as_ref()
                .expect("invariant: caller checks occupant.is_some");
            Victim {
                model: occupant.model.clone(),
                ticket: occupant.ticket.clone(),
                slot,
                evicted: false,
                cost: occupant.need.0,
            }
        };
        let job = Job {
            kind: JobKind::Unload,
            model: victim.model.clone(),
            engine: String::new(),
            category,
            state: SwapState::Idle,
            terminal: false,
            outcome: None,
            error: None,
            need: None,
            cost: crate::model::CostClass::Fixed,
            launch_floor: None,
            load_timeout_s: None,
            gate_target: None,
            free_at_admit: None,
            observed_at_admit: None,
            container: None,
            timings: JobTimings::default(),
            t0: Instant::now(),
            waiters: Vec::new(),
            tasks: Vec::new(),
            spawned: None,
            readiness: None,
            measured: None,
            victim: Some(victim),
            victims: Vec::new(),
            victims_released: Vec::new(),
            released_cost: 0.0,
            other_need_at_admit: 0.0,
            note: Some(note.to_string()),
            warnings: Vec::new(),
            slot: Some(slot),
            // Unload rows queue against an occupant, not a file: nothing to compare.
            queue_fingerprint: None,
        };
        self.jobs.insert(job_id, job);
        self.slots[slot].job = Some(job_id);
        self.step(job_id, SwapState::Admit);
        self.step(job_id, SwapState::Quiesce);
        self.step(job_id, SwapState::Dispose);
        self.spawn_kill_confirm(job_id);
    }

    /// The idle sweep (Step 5, tick-driven): every occupied, job-free slot whose
    /// category has `idle_timeout_s > 0` and whose model has been unused for the
    /// threshold starts an unload. WORK defaults to 0 (never idles); config is king.
    fn on_tick(&mut self) {
        let now = Instant::now();
        let due: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                let (Some(occupant), None) = (&slot.occupant, slot.job) else {
                    return false;
                };
                let Some(timeout) = self
                    .cfg
                    .categories
                    .iter()
                    .find(|cat| cat.id == slot.category)
                    .map(|cat| cat.idle_timeout_s)
                    .filter(|timeout| *timeout > 0)
                else {
                    return false;
                };
                now.duration_since(state_last_used(occupant, &self.last_used))
                    >= Duration::from_secs(timeout)
            })
            .map(|(index, _)| index)
            .collect();
        for slot in due {
            let model = self.slots[slot]
                .occupant
                .as_ref()
                .map(|o| o.model.clone())
                .unwrap_or_default();
            tracing::info!(slot = self.slots[slot].index, category = %self.slots[slot].category, %model,
                "idle timeout reached; unloading");
            self.begin_unload_slot(slot, "idle timeout");
        }
    }

    /// Records a proxy-side use of `model` (last-use input for the idle sweep).
    fn touch_model(&mut self, model: &str) {
        self.last_used.insert(model.to_string(), Instant::now());
    }

    /// The free memory the card had when this job's engine started: the admit reading
    /// corrected for everything the job's own release gave back. The admit reading
    /// alone still counts a victim's memory as occupied, which would under-measure
    /// every replacement load by the victim's whole footprint (review, 2026-10-27).
    fn post_release_free(&self, job_id: JobId) -> Option<f64> {
        let job = self.jobs.get(&job_id)?;
        job.observed_at_admit
            .map(|observed| observed + job.released_cost)
    }

    /// Ends a slot's residency and stops its sampler. The sampler belongs to the
    /// engine that was there, not to whatever loads into the slot next, so every path
    /// that drops an occupant has to come through here (review, 2026-10-27).
    fn clear_occupant(&mut self, slot: usize) {
        if let Some(occupant) = self.slots[slot].occupant.take() {
            if let Some(task) = occupant.measure {
                task.abort();
            }
        }
    }

    /// Clears a victim's own slot once its release is confirmed (same-slot replacement or
    /// a cross-category evictee — never the job's slot blindly), and only while the
    /// occupant there is still the one this ticket owns. Shared by the two arms that
    /// accept a confirmed release, so a cancelled swap cannot leave a reaped engine
    /// installed (chunk 4 [1]).
    ///
    /// # Arguments
    ///
    /// * `job_id` - the job whose `victim` has just been released.
    fn clear_released_victim_slot(&mut self, job_id: JobId) {
        let Some(victim) = self.jobs.get(&job_id).and_then(|job| job.victim.clone()) else {
            return;
        };
        if self.slots[victim.slot]
            .occupant
            .as_ref()
            .is_some_and(|o| o.ticket == victim.ticket)
        {
            self.clear_occupant(victim.slot);
        }
    }

    /// Folds one serving-window reading into the model's measured range and writes it
    /// back to the yaml (§0.4): `measured_gb` keeps the high end — what `fixed`
    /// admission consults — and `measured_gb_min` the low end the page shows, plus —
    /// for `OFFLOAD_ENGINES` owners only — the `cpu_offload` setting they were taken
    /// under, so a later flip voids them at scan (the pair is engine-agnostic
    /// witness truth; the stamp is a knob the engine must actually read).
    /// Everything it says comes from the spawn's [`MeasuredAnchor`], never from the
    /// live catalog: `reload()` replaces the catalog while the process keeps its argv,
    /// so a stamp read at sample time would describe a launch that never happened and
    /// — because the void rule compares that same stamp with the yaml — disarm the rule
    /// that catches it, and a range read at sample time would be re-seeded from the
    /// one live process. Six guards keep the number honest: the sampled engine must
    /// still be the one in the slot; no other job may be in flight; a tenant that
    /// appeared after admit is subtracted, or the driver's own per-process attribution
    /// is used when the card reports it; a figure outside the card is refused, not
    /// persisted (measurement verifies, it never guesses); the spawn must carry an
    /// anchor at all; and the row that owns the reading must still be the file this
    /// engine was launched from — if it has gone, nothing is booked, the reason is
    /// named once and the sampler is retired. A persist the disk refuses
    /// reconciles: the triple drains from memory (Fork C — see
    /// [`ActorState::on_measured_persist_failed`]).
    /// Takes and aborts the sampler of the serving occupant identified by
    /// `model`+`ticket`, so a reading with nowhere to go stops being produced rather
    /// than being discarded for 24 h. Both anchor classes that cannot persist call this
    /// (FINDINGS2-04 [3]); it is idempotent, and the ticket keeps a re-minted twin's
    /// sampler untouched.
    fn retire_sampler(&mut self, model: &str, ticket: &SpawnTicket) {
        let Some(slot) = self.slots.iter_mut().find(|slot| {
            slot.occupant
                .as_ref()
                .is_some_and(|o| o.model == model && &o.ticket == ticket)
        }) else {
            return;
        };
        if let Some(occupant) = slot.occupant.as_mut() {
            if let Some(sampler) = occupant.measure.take() {
                sampler.abort();
            }
        }
    }

    fn on_measured(
        &mut self,
        model: &str,
        ticket: &SpawnTicket,
        total_diff: f64,
        own: Option<f64>,
        total_gib: f64,
    ) {
        let serving = self.slots.iter().any(|slot| {
            slot.occupant
                .as_ref()
                .is_some_and(|o| o.model == model && &o.ticket == ticket)
        });
        if !serving {
            return;
        }
        let swapping = self
            .jobs
            .values()
            .any(|job| !job.terminal && job.model != model);
        if swapping {
            return;
        }
        let booked_others: f64 = self
            .slots
            .iter()
            .filter_map(|slot| slot.occupant.as_ref())
            .filter(|o| o.model != model)
            .map(|o| o.need.0)
            .sum();
        // One lookup for the spawn this reading belongs to, by the `(model, ticket)`
        // the sampler carries: the same row answers for the tenant subtraction above
        // (`other_need_at_admit`) and for the measured anchor below. A reading whose
        // spawn row cannot be found — or was never anchored at spawn — is not folded:
        // the harness cannot say what a launch it cannot identify ran under.
        let Some((spawned_job, at_admit, anchor)) = self
            .jobs
            .iter()
            .find(|(_, job)| job.model == model && job.spawned.as_ref() == Some(ticket))
            .map(|(id, job)| (*id, job.other_need_at_admit, job.measured.clone()))
        else {
            return;
        };
        let Some(anchor) = anchor else { return };
        let attributed = own.unwrap_or(total_diff - (booked_others - at_admit).max(0.0));
        if !attributed.is_finite() || attributed < 0.0 || attributed > total_gib {
            // No footprint this engine could have — a wedged driver, or a subtraction
            // that overshot. Absence, never a fake number.
            return;
        }
        let rounded = (attributed * 10.0).round() / 10.0;
        if rounded < 0.05 {
            // Sub-quantum attribution is absence, not a measurement: persisting it
            // would write the exact token the scanner's reader rule rejects.
            return;
        }
        let mut persist: Option<(std::path::PathBuf, f64, f64, Option<bool>)> = None;
        let mut changed = false;
        // The row that owns this reading is the spawn-era FILE, never whichever entry
        // answers to the name now: a reload that renames the yaml keeps the bytes and
        // moves the name, and a re-minted twin steals it, so only the file the engine
        // was launched from may receive the write (FINDINGS-13 [3]).
        let Some(path) = anchor.file.clone() else {
            // Synthetic row at spawn: no operator file exists, so nothing may be
            // written and nothing may be booked against a name nobody owns. The sampler
            // still has to be retired here (FINDINGS2-04 [3]): without this line the
            // task keeps walking the witness every `measured_sample_s` until its 24 h
            // deadline while the actor discards every reading it returns — the same
            // orphan class the row-gone branch below closes, reached from the other
            // unreadable anchor shape.
            tracing::warn!(
                model,
                "measured reading has no operator file: the spawn-era row was synthetic, \
                 so the sampler is retired instead of sampling into a discard"
            );
            self.retire_sampler(model, ticket);
            return;
        };
        let Some(entry) = self
            .catalog
            .iter_mut()
            .find(|m| m.file.as_deref() == Some(path.as_path()))
        else {
            // The row went away while its engine kept serving (renamed file, a new
            // problem line, an unknown engine). Nothing here can own the reading: say
            // so once and retire the sampler, which would otherwise walk the witness
            // every `measured_sample_s` for a day with nowhere to put the answer.
            tracing::warn!(
                model,
                file = %path.display(),
                "measured reading has no catalog row: its yaml went away mid-serve, so the range is neither booked nor persisted"
            );
            self.retire_sampler(model, ticket);
            return;
        };
        // Folded against the SPAWN's range, not the entry's live pair: `read_measured`
        // drops the in-memory copy the moment a stamp stops matching, and a reload
        // that voids it must not silently re-seed an accumulated range from the one
        // process still on the card.
        let high = anchor.range.map_or(rounded, |(high, _)| high.max(rounded));
        let low = anchor
            .range
            .map_or(rounded, |(_, low)| low.min(rounded))
            .min(high);
        if entry.measured_gb != Some(high)
            || entry.measured_min_gb != Some(low)
            || entry.measured_offload != anchor.offload
        {
            if let Some(declared) = entry.vram_gb {
                let drift = (high - declared).abs();
                if drift > (declared * 0.25).max(2.0) {
                    tracing::warn!(
                        model,
                        declared,
                        measured = high,
                        "measured footprint diverges from the declared vram_gb"
                    );
                }
            }
            entry.measured_gb = Some(high);
            entry.measured_min_gb = Some(low);
            // The stamp is what this launch actually ran under — minted at spawn
            // through `offload_live` + the engine's truth set, so the flip-void rule
            // keeps a stamp that describes the process rather than one rewritten to
            // agree with the yaml that arrived after it.
            entry.measured_offload = anchor.offload;
            persist = Some((path, high, low, anchor.offload));
            changed = true;
        }
        if changed {
            // The spawn's own accumulator, so the next sample (and any reload in
            // between) still folds against the range this process produced.
            if let Some(job) = self.jobs.get_mut(&spawned_job) {
                job.measured = Some(MeasuredAnchor {
                    range: Some((high, low)),
                    ..anchor.clone()
                });
            }
            self.publish();
        }
        if let Some((path, high, low, offload)) = persist {
            let name = model.to_string();
            let tx = self.tx.clone();
            let anchor_file = path.clone();
            tokio::task::spawn_blocking(move || {
                // Every measured key in ONE rewrite: written separately, a failure
                // between them could leave an orphan or inverted pair, and the
                // scanner rejects that pair outright — dropping the whole model
                // (review, 2026-10-27). The stamp rides only when the engine reads
                // `cpu_offload`: a non-offload yaml never gains a meaningless key.
                let high_text = format!("{high:.1}");
                let low_text = format!("{low:.1}");
                let stamp_text = offload.map(|on| on.to_string());
                let mut edits: Vec<(&str, Option<&str>)> = vec![
                    ("measured_gb", Some(high_text.as_str())),
                    ("measured_gb_min", Some(low_text.as_str())),
                ];
                if let Some(stamp) = stamp_text.as_deref() {
                    edits.push(("measured_offload", Some(stamp)));
                }
                match crate::model::writeback::set_flags_on_file(&path, &edits) {
                    Ok(()) => {
                        // Axis-2 interlock 5: the fold's own bytes are not a config
                        // change since launch. The fingerprint rides from here —
                        // off-thread, directly after the atomic rename — so the
                        // resident's cached verdict never amber-self over a measurement.
                        if let Some(fingerprint) = crate::model::fingerprint(&path) {
                            let _ = tx.send(ActorMsg::ModelFileWritten {
                                model: name,
                                path,
                                fingerprint,
                            });
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            model = %name,
                            "cannot persist the measured range"
                        );
                        // Warn alone left memory booking a range the file never got
                        // (FINDINGS-13 [4]'s degraded tail): hand the exact refused
                        // triple back to the actor, which drains it from the catalog.
                        // The file rides with it so the drain can only land on the row
                        // this write targeted — never on a same-name twin.
                        let _ = tx.send(ActorMsg::MeasuredPersistFailed {
                            model: name,
                            high,
                            low,
                            offload,
                            file: Some(anchor_file),
                        });
                    }
                }
            });
        }
    }

    /// Reconciles a refused measured persist (Fork C): the in-memory measured pair
    /// (and, for offload engines, the stamp) is drained to its none state so the
    /// page and `fixed` admission never charge a footprint the disk refused. The
    /// drain only lands when the entry still holds the exact values that failed —
    /// a superseding reading (whose own persist will carry its own reconcile if it
    /// too fails) is left untouched.
    ///
    /// The row is found by the same key the fold used, the spawn-era FILE
    /// (FINDINGS2-04 [1]): keying this leg `name && file` while the fold is keyed
    /// file-only made the name clause the single divergence point — a reload that
    /// renames the yaml leaves the fold booking the file's row and this drain missing
    /// it, so memory keeps a triple the disk refused for the whole serve window
    /// (subsequent samples reproduce the same high/low against the updated anchor
    /// range, `changed` stays false, and no retry persist ever fires). `model` is the
    /// fallback only for the hand-built no-file messages; production persist failures
    /// always carry the file (the fold's own `anchor_file`).
    fn on_measured_persist_failed(
        &mut self,
        model: &str,
        high: f64,
        low: f64,
        offload: Option<bool>,
        file: Option<PathBuf>,
    ) {
        let Some(entry) = self.catalog.iter_mut().find(|m| {
            file.as_deref()
                .map_or(m.name == model, |path| m.file.as_deref() == Some(path))
        }) else {
            return;
        };
        if entry.measured_gb == Some(high)
            && entry.measured_min_gb == Some(low)
            && entry.measured_offload == offload
        {
            entry.measured_gb = None;
            entry.measured_min_gb = None;
            entry.measured_offload = None;
            self.publish();
        }
    }
    /// The ordinary load path: register the row and arm it in one step.
    fn begin_load(&mut self, slot: usize, entry: ModelEntry, waiters: Vec<Waiter>) {
        let job_id = self.register_queued_load(slot, &entry, waiters);
        self.start_queued_load(job_id, slot, entry);
    }

    /// Creates the ledger row of a load that has not started yet — `kind: Load`,
    /// non-terminal, state `Idle`, holding its waiters. Every load begins here, so a
    /// parked one is already on the ledger while its caller waits, instead of being a
    /// request with no row and no answer (FINDINGS-02 [3]).
    ///
    /// # Arguments
    ///
    /// * `slot` - the slot the load is queued on (its `pending` place).
    /// * `entry` - the catalog entry, whose keys pre-fill the booking fields.
    /// * `waiters` - the callers attached to this load (F-6: duplicates share one row).
    ///
    /// # Returns
    ///
    /// The id of the row just registered, which is what `pending` stores.
    fn register_queued_load(
        &mut self,
        slot: usize,
        entry: &ModelEntry,
        waiters: Vec<Waiter>,
    ) -> JobId {
        let job_id = JobId(self.next_job);
        self.next_job += 1;
        let job = Job {
            kind: JobKind::Load,
            model: entry.name.clone(),
            engine: entry.engine.clone(),
            category: entry.category,
            state: SwapState::Idle,
            terminal: false,
            outcome: None,
            error: None,
            need: None,
            free_at_admit: None,
            observed_at_admit: None,
            container: None,
            timings: JobTimings::default(),
            t0: Instant::now(),
            waiters,
            tasks: Vec::new(),
            spawned: None,
            readiness: None,
            measured: None,
            victim: None,
            victims: Vec::new(),
            victims_released: Vec::new(),
            released_cost: 0.0,
            other_need_at_admit: 0.0,
            note: None,
            warnings: Vec::new(),
            slot: Some(slot),
            cost: entry.cost,
            launch_floor: entry.launch_floor_gb,
            load_timeout_s: entry.load_timeout_s,
            gate_target: None,
            // The yaml as the catalog saw it, statted once per registration: the
            // same one-stat-per-event discipline `apply_catalog` keeps (the stat
            // rides a file the just-applied scan already read; adapters already
            // stat config files synchronously on the actor at preflight).
            queue_fingerprint: entry.file.as_deref().and_then(crate::model::fingerprint),
        };
        self.jobs.insert(job_id, job);
        self.set_timing(job_id, |t| &mut t.queue_ms);
        job_id
    }

    /// Arms a registered load row: takes over its slot, books the need, records the
    /// occupant it must release, and starts the witness sample that drives `Admit`.
    /// Arming clears any unload still queued behind a cancelled swap on that slot
    /// (FINDINGS-02 [2]): a load the operator asked for after pressing Unload is the
    /// later instruction.
    fn start_queued_load(&mut self, job_id: JobId, slot: usize, entry: ModelEntry) {
        self.unload_after_finish.remove(&slot);
        // One decision (`ModelEntry::cost_source`) drives the gate; the page's Cost
        // column reads the same one, so the two cannot drift.
        let need = match entry.cost_source() {
            // `exact` is a precision claim: the engine's number, not an estimate.
            // No `vram_gb`, no load — the size×1.15+2 fallback would be a guess.
            crate::model::CostSource::Undeclared => Err(AdmissionError::ExactUndeclared),
            source => budget::need_gb(
                entry
                    .vram_gb
                    .map(Gibibytes)
                    // `fixed` consults the harness-measured footprint of an earlier
                    // run before the size estimate. Greedy keeps nominal semantics: a
                    // post-spill measurement is not a nominal size and must not shrink
                    // the booking (review, 2026-10-26).
                    .or_else(|| {
                        (source == crate::model::CostSource::Measured)
                            .then(|| entry.measured_gb.map(Gibibytes))
                            .flatten()
                    }),
                entry.gguf_size_gb.map(Gibibytes),
            ),
        };
        // A greedy load's nominal size never binds — it offloads instead — so an
        // unknown size is allowed (booking uses the room at admit).
        let booking = match need {
            Ok(need) => Some(need),
            Err(_) if entry.cost == crate::model::CostClass::Greedy => None,
            Err(error) => {
                let error = SchedulerError::Admission(error);
                self.slots[slot].job = Some(job_id);
                self.step(job_id, SwapState::Admit);
                self.step(job_id, SwapState::AdmitFail);
                self.log_refusal(job_id, &error, "need");
                self.finish(job_id, Err(error), "under-specified");
                return;
            }
        };
        let victim = self.slots[slot].occupant.as_ref().map(|o| Victim {
            model: o.model.clone(),
            ticket: o.ticket.clone(),
            slot,
            evicted: false,
            cost: o.need.0,
        });
        if let Some(job) = self.jobs.get_mut(&job_id) {
            job.need = booking;
            job.victim = victim;
        }
        self.slots[slot].job = Some(job_id);
        self.step(job_id, SwapState::Admit);
        // Step 4 (as amended 2026-10-26): the gate is evaluated against the card's own
        // reading, sampled on a task — never inline in the actor, which must not stall
        // behind a witness call.
        let witness = Arc::clone(&self.witness);
        let tx = self.tx.clone();
        let handle = tokio::spawn(async move {
            let _ = tx.send(ActorMsg::Event {
                job: job_id,
                event: Event::Admitted(witness.sample().await),
            });
        });
        self.jobs
            .get_mut(&job_id)
            .expect("invariant: caller holds a live job id")
            .tasks
            .push(handle);
    }

    /// The Admit decision, once the witness sample returns. The card's own reading
    /// is the budget (operator 2026-10-26): `total - reserve - observed_used`,
    /// credited for the occupant this job is about to release and debited for what
    /// other loads have already committed. The ledger survives only as the `budget`
    /// figure a refusal quotes, and as the whole basis of `ledger_only` witness mode.
    fn finish_admit(&mut self, job_id: JobId, sample: Result<crate::budget::MemorySample, String>) {
        // A real witness that cannot answer is a refusal, never a fallback onto a
        // bookkeeping guess (F-3/F-7: this is exactly how v3.3 over-committed).
        if sample.is_err() && !self.witness.ledger_only() {
            let reason = sample
                .as_ref()
                .err()
                .map_or_else(|| "no sample".to_string(), String::clone);
            self.step(job_id, SwapState::AdmitFail);
            let error = SchedulerError::Admission(AdmissionError::WitnessUnavailable { reason });
            self.log_refusal(job_id, &error, "witness");
            self.finish(job_id, Err(error), "witness down");
            return;
        }
        let (total_gib, occupied, observed_free) = match &sample {
            Ok(sample) => (sample.total_gib, sample.used_gib, Some(sample.free_gib())),
            Err(_) => {
                // The declared mode of a witness that never answers (tests, a box
                // without a GPU): the ledger is then the only truth there is.
                tracing::debug!("witness never answers (ledger_only); budgeting the ledger");
                (self.cfg.vram_total_gb, self.ledger_sum().0, None)
            }
        };
        let declared = self.jobs[&job_id].need;
        let need = declared.unwrap_or_default();
        let victim_need = self.jobs[&job_id]
            .slot
            .map(|slot| self.slots[slot].occupant.as_ref().map_or(0.0, |o| o.need.0))
            .unwrap_or(0.0);
        // The bytes this job is about to reclaim are still physically on the card at
        // sample time, so they are credited; what other loads have committed but not
        // yet installed is debited, or two concurrent loads spend the same free GiB.
        let in_flight = self.in_flight_need(job_id);
        let free =
            Gibibytes(total_gib - self.cfg.vram_reserve_gb - occupied + victim_need - in_flight);
        let facts = BudgetFacts {
            budget: Some(budget::free_gb(
                Gibibytes(total_gib),
                Gibibytes(self.cfg.vram_reserve_gb),
                self.ledger_sum(),
            )),
            observed_free: observed_free.map(Gibibytes),
        };
        if let Some(job) = self.jobs.get_mut(&job_id) {
            job.free_at_admit = Some(free);
            job.observed_at_admit = observed_free;
        }
        self.set_timing(job_id, |t| &mut t.admit_ms);
        let (greedy, model_floor) = {
            let job = &self.jobs[&job_id];
            (
                job.cost == crate::model::CostClass::Greedy,
                job.launch_floor,
            )
        };
        // Launch floor: the model's key, else the global greedy default — a
        // greedy quant spawned onto a card already busy with foreign processes
        // thrashes RAM/SSD for minutes before serving (operator note 2026-10-26).
        let floor = model_floor.or(if greedy {
            Some(self.cfg.greedy_launch_min_free_gb)
        } else {
            None
        });
        // The card itself can be misread: `nvidia-smi` parses `NaN` as a number, and
        // every comparison against NaN is false, which would silently read as
        // "fits". An unreadable reading is named as one (fail closed, §0.3).
        if !free.0.is_finite() {
            self.step(job_id, SwapState::AdmitFail);
            let error = SchedulerError::Admission(AdmissionError::WitnessUnavailable {
                reason: format!("unreadable witness reading: {} GiB free", free.0),
            });
            self.log_refusal(job_id, &error, "witness");
            self.finish(job_id, Err(error), "witness unreadable");
            return;
        }
        // Defense in depth (review, 2026-10-26): the scan parser rejects
        // non-finite sizes, but no value may ever *silently* pass the gate —
        // every NaN comparison is false, which would read as "fits".
        if need.0.is_nan() || floor.is_some_and(|f| !f.is_finite()) {
            self.step(job_id, SwapState::AdmitFail);
            let error = SchedulerError::Admission(AdmissionError::UnderSpecified);
            self.log_refusal(job_id, &error, "bad-size");
            self.finish(job_id, Err(error), "under-specified");
            return;
        }
        // One truth now: `free` already is the physical reading, credited and
        // debited. `facts` carries the originals so a refusal can name which bound.
        let need_deficit = if greedy {
            0.0
        } else {
            (need.0 - free.0).max(0.0)
        };
        let floor_deficit = floor.map(|f| (f - free.0).max(0.0)).unwrap_or(0.0);
        let deficit = need_deficit.max(floor_deficit);
        // What `available` (free_at_admit + released) must reach before spawn.
        // Comparing only `need` re-admits the bug this review caught: greedy
        // books a clamped-small need, so the re-check would skip the very floor
        // victims the planner queued.
        let gate_target = if greedy {
            floor.unwrap_or(0.0)
        } else {
            need.0.max(floor.unwrap_or(0.0))
        };
        if let Some(job) = self.jobs.get_mut(&job_id) {
            job.gate_target = Some(gate_target);
        }
        if deficit > 0.0 {
            // Step 6: try to cover the deficit with evictable cross-category
            // victims before refusing — cheapest sufficient set, hard filters.
            match self.plan_victims(job_id, deficit) {
                Ok(victims) => {
                    if let Some(job) = self.jobs.get_mut(&job_id) {
                        job.victims = victims;
                    }
                    tracing::info!(
                        %job_id,
                        victims = ?self.jobs[&job_id].victims.iter().map(|v| &v.model).collect::<Vec<_>>(),
                        deficit_gib = deficit,
                        "admission short: evicting victims"
                    );
                }
                Err((considered, blockers)) => {
                    self.step(job_id, SwapState::AdmitFail);
                    let reserve = Gibibytes(self.cfg.vram_reserve_gb);
                    let error =
                        if floor_deficit > 0.0 && floor_deficit >= need_deficit {
                            AdmissionError::BelowLaunchFloor {
                                min_free: Gibibytes(floor.expect(
                                    "invariant: floor_deficit > 0 implies a configured floor",
                                )),
                                // G13: the figure the gate actually compared.
                                free,
                                reserve,
                                facts,
                                victims_considered: considered,
                                blockers,
                            }
                        } else {
                            AdmissionError::Insufficient {
                                need,
                                // G13: the compared figure, never the flattering one.
                                free,
                                reserve,
                                facts,
                                victims_considered: considered,
                                blockers,
                            }
                        };
                    self.log_refusal(job_id, &SchedulerError::Admission(error.clone()), "gate");
                    self.finish(
                        job_id,
                        Err(SchedulerError::Admission(error)),
                        "admission refused",
                    );
                    return;
                }
            }
        }
        // A greedy tenant books what it will actually grab — its nominal size if
        // that fits, else the room available *including* the victims this very
        // admit queued for release (their bytes are still counted as used but will
        // be gone before spawn). Never its offload-able nominal size (starves the
        // card) and never zero when it will hold real VRAM (under-books the gate).
        if greedy {
            let planned_room = free.0
                + self.jobs[&job_id]
                    .victims
                    .iter()
                    .map(|v| v.cost)
                    .sum::<f64>();
            let booked = declared
                .map_or(planned_room, |n| n.0.min(planned_room))
                .max(0.0);
            if let Some(job) = self.jobs.get_mut(&job_id) {
                job.need = Some(Gibibytes(booked));
            }
        }
        if crate::observability::extended_logging() {
            // Decision-level evidence for rehearsal post-checks (operator,
            // 2026-10-26): the exact numbers the gate compared on the pass path.
            let job = &self.jobs[&job_id];
            tracing::info!(
                %job_id,
                model = %job.model,
                cost = ?job.cost,
                need_gib = job.need.map(|n| n.0),
                floor_gib = floor,
                free_gib = free.0,
                observed_at_admit_gib = job.observed_at_admit,
                budget_gib = facts.budget.map(|budget| budget.0),
                booked_gib = self.in_flight_need(job_id),
                victim = ?job.victim.as_ref().map(|v| v.model.clone()),
                victims = ?job.victims.iter().map(|v| v.model.clone()).collect::<Vec<_>>(),
                gate_gib = gate_target,
                "admission decision (extended)"
            );
        }
        // Chunk 10 [2]: everything the adapter validates about a launch — required
        // keys, profile/interpreter existence, WSL word and container legality — must
        // be settled *before* the eviction. Run after Dispose it would destroy the
        // resident model and every victim, then refuse, leaving an empty slot and
        // work nobody can un-kill. Read-only, so a refusal here changes nothing.
        if let Err(error) = self.preflight_spec(job_id) {
            self.step(job_id, SwapState::AdmitFail);
            self.log_refusal(job_id, &error, "launch");
            self.finish(job_id, Err(error), "launch refused");
            return;
        }
        self.step(job_id, SwapState::Quiesce);
        self.step(job_id, SwapState::Dispose);
        // Ensure a release target: the same-slot replacement first, else the first
        // queued cross-category victim.
        if self.jobs[&job_id].victim.is_none() {
            self.dequeue_victim(job_id);
        }
        if self.jobs[&job_id].victim.is_some() {
            self.spawn_kill_confirm(job_id);
        } else {
            self.begin_spawn(job_id);
        }
    }

    /// Step 6 planner: among occupants evictable by this job's category, pick the
    /// cheapest sufficient set (exhaustive over the ≤9 slots; tie-break: fewer
    /// victims). Hard filters first — a non-evictable occupant becomes a named
    /// blocker, never a candidate. Release order: least-protected first, LRU
    /// within a category.
    fn plan_victims(
        &self,
        job_id: JobId,
        deficit: f64,
    ) -> Result<
        Vec<Victim>,
        (
            Vec<crate::budget::VictimInfo>,
            Vec<crate::budget::BlockerInfo>,
        ),
    > {
        let load_category = self.jobs[&job_id].category;
        let job_slot = self.jobs[&job_id].slot;
        let evictable_by = |category: CategoryId| {
            self.cfg
                .categories
                .iter()
                .find(|cat| cat.id == category)
                .map(|cat| cat.evictable_by.clone())
                .unwrap_or_default()
        };
        let mut candidates: Vec<(usize, f64, usize)> = Vec::new();
        let mut considered = Vec::new();
        let mut blockers = Vec::new();
        for (index, slot) in self.slots.iter().enumerate() {
            if job_slot == Some(index) {
                continue; // the same-slot replacement is already accounted
            }
            let Some(occupant) = &slot.occupant else {
                continue;
            };
            let allowed_list = evictable_by(slot.category);
            if allowed_list.contains(&load_category) {
                candidates.push((index, occupant.need.0, allowed_list.len()));
                considered.push(crate::budget::VictimInfo {
                    model: occupant.model.clone(),
                    category: slot.category.to_string(),
                    need_gib: occupant.need.0,
                });
            } else {
                let reason = if allowed_list.is_empty() {
                    format!("nothing may evict {}", slot.category)
                } else {
                    format!(
                        "only {} can evict {}",
                        allowed_list
                            .iter()
                            .map(|c| c.to_string())
                            .collect::<Vec<_>>()
                            .join(" and "),
                        slot.category
                    )
                };
                blockers.push(crate::budget::BlockerInfo {
                    model: occupant.model.clone(),
                    category: slot.category.to_string(),
                    reason,
                });
            }
        }
        let n = candidates.len();
        if n == 0 {
            return Err((considered, blockers));
        }
        // Exhaustive cheapest covering subset (n ≤ 9 slots → ≤512 masks).
        //
        // Ties break by disposability — how many categories may evict a victim — and
        // never by slot order. RP lists only WORK as an evictor while AUX lists WORK
        // and RP, so when two victims cost the same and either would do, the more
        // disposable category must be the one released. Comparing masks in iteration
        // order evicted RP, the costlier model to bring back, purely because its slot
        // index precedes AUX's — which contradicted the release ordering below (T6).
        let mut best: Option<u32> = None;
        let mut best_sum = f64::INFINITY;
        let mut best_count = usize::MAX;
        let mut best_disposability = usize::MIN;
        for mask in 1u32..(1 << n) {
            let (mut sum, mut count, mut disposability) = (0.0, 0, 0);
            for (bit, (_, cost, protection)) in candidates.iter().enumerate() {
                if mask & (1 << bit) != 0 {
                    sum += cost;
                    count += 1;
                    disposability += protection;
                }
            }
            if sum + 1e-9 >= deficit
                && (sum < best_sum
                    || (sum <= best_sum + 1e-9
                        && (count < best_count
                            || (count == best_count && disposability > best_disposability))))
            {
                best_sum = sum;
                best_count = count;
                best_disposability = disposability;
                best = Some(mask);
            }
        }
        let Some(mask) = best else {
            return Err((considered, blockers));
        };
        // Release order: least-protected first (more categories may evict it),
        // LRU within the same protection level.
        let mut chosen: Vec<usize> = (0..n).filter(|bit| mask & (1 << bit) != 0).collect();
        chosen.sort_by(|&a, &b| {
            let (slot_a, slot_b) = (candidates[a].0, candidates[b].0);
            let prot = |index: usize| evictable_by(self.slots[index].category).len();
            prot(slot_b).cmp(&prot(slot_a)).then_with(|| {
                let lru = |index: usize| {
                    self.slots[index]
                        .occupant
                        .as_ref()
                        .map_or(Instant::now(), |o| o.loaded_at)
                };
                lru(slot_a).cmp(&lru(slot_b))
            })
        });
        let victims = chosen
            .into_iter()
            .map(|bit| {
                let (index, _, _) = candidates[bit];
                let occupant = self.slots[index]
                    .occupant
                    .as_ref()
                    .expect("invariant: candidates come from live occupants");
                Victim {
                    model: occupant.model.clone(),
                    ticket: occupant.ticket.clone(),
                    slot: index,
                    evicted: true,
                    cost: occupant.need.0,
                }
            })
            .collect();
        Ok(victims)
    }

    /// Moves the next queued victim into the release slot; `false` when the queue
    /// is empty (Step 6: one at a time).
    fn dequeue_victim(&mut self, job_id: JobId) -> bool {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return false;
        };
        if job.victims.is_empty() {
            return false;
        }
        let next = job.victims.remove(0);
        job.victim = Some(next);
        true
    }

    /// §0.3 memory evidence: after a confirmed release, wait until the card has
    /// *physically* freed the gate target (`max(need, launch floor)`, within the
    /// witness tolerance) before the spawn fires. This is where a replacement load
    /// proves the card truly emptied (operator 2026-10-26: the stopper is observed
    /// VRAM, judged after release). No ledger baseline is consulted — an
    /// unattributable desktop floor is not a leak.
    ///
    /// A witness that dies mid-window fails **closed**, exactly as at admission: a
    /// confirmed release proves the port closed, never that the VRAM came back, and
    /// for a same-slot replacement this window is the only physical check the load
    /// ever gets. Only a deliberately silent (`ledger_only`) witness is waved
    /// through (review, 2026-10-26).
    fn spawn_settle_task(&mut self, job_id: JobId) {
        let tolerance = self.cfg.witness_tolerance_gb;
        let gate = self.jobs[&job_id].gate_target.unwrap_or(0.0);
        let model = self.jobs[&job_id].model.clone();
        let deadline = Duration::from_secs(self.cfg.settle_timeout_s);
        let ledger_only = self.witness.ledger_only();
        let witness = Arc::clone(&self.witness);
        let tx = self.tx.clone();
        let handle = tokio::spawn(async move {
            let started = Instant::now();
            loop {
                match witness.sample().await {
                    Ok(sample) => {
                        let observed_free = sample.free_gib();
                        if sample.freed_for(gate, tolerance) {
                            if crate::observability::extended_logging() {
                                tracing::info!(
                                    used_gib = sample.used_gib,
                                    total_gib = sample.total_gib,
                                    free_gib = observed_free,
                                    gate_gib = gate,
                                    "settle evidence (extended)"
                                );
                            }
                            let _ = tx.send(ActorMsg::Event {
                                job: job_id,
                                event: Event::Settled(Ok(())),
                            });
                            return;
                        }
                    }
                    Err(reason) => {
                        if ledger_only {
                            tracing::debug!(reason = %reason,
                                "no reading to settle against (ledger-only witness); proceeding");
                            let _ = tx.send(ActorMsg::Event {
                                job: job_id,
                                event: Event::Settled(Ok(())),
                            });
                            return;
                        }
                        tracing::warn!(reason = %reason,
                            "witness lost during settle; holding the spawn until it answers");
                    }
                }
                if started.elapsed() >= deadline {
                    let numbers = match witness.sample().await {
                        Ok(sample) => {
                            let observed_free = sample.free_gib();
                            let msg = format!(
                                "{:.1} GiB free after release, {:.1} GiB required (used {:.1} of {:.1})",
                                observed_free, gate, sample.used_gib, sample.total_gib
                            );
                            // The load that never spawned must still be readable
                            // in the ring (§0.3 evidence, operator 2026-10-26).
                            tracing::warn!(
                                %job_id,
                                %model,
                                used_gib = sample.used_gib,
                                total_gib = sample.total_gib,
                                free_gib = observed_free,
                                gate_gib = gate,
                                "settle timeout: the card never freed the required space"
                            );
                            msg
                        }
                        Err(reason) => {
                            tracing::warn!(%job_id, %model, reason = %reason, gate_gib = gate,
                                "settle timeout: the witness never answered again");
                            format!("witness lost during settle: {reason}")
                        }
                    };
                    let _ = tx.send(ActorMsg::Event {
                        job: job_id,
                        event: Event::Settled(Err(numbers)),
                    });
                    return;
                }
                tokio::time::sleep(PROBE_INTERVAL).await;
            }
        });
        self.jobs
            .get_mut(&job_id)
            .expect("invariant: caller holds a live job id")
            .tasks
            .push(handle);
    }

    /// The engine port the slot plan assigned to this job's slot (Step 8/11).
    fn slot_engine_port(&self, job_id: JobId) -> u16 {
        self.jobs
            .get(&job_id)
            .and_then(|job| job.slot)
            .map_or(0, |slot| self.slots[slot].engine_port)
    }

    /// Read-only launch preparation for a live job: adapter lookup, catalog lookup,
    /// the slot's engine port, and the adapter's own validation of the entry.
    ///
    /// A pure function of state — it mutates nothing, kills nothing and spawns
    /// nothing — so the actor can settle every launch-time refusal (chunk 10 [2])
    /// while the resident model and the planned victims are still alive. `begin_spawn`
    /// calls the same helper, so the pre-eviction refusal and the spawn cannot drift.
    ///
    /// # Arguments
    ///
    /// * `job_id` - a live job that has passed admission, in `Admit` or later.
    ///
    /// # Returns
    ///
    /// The [`LaunchSpec`] to hand the runtime, with the slot's engine port embedded,
    /// and the [`ModelEntry`] it was built from — the pair, never the spec alone, so
    /// the spawn's `MeasuredAnchor` provably comes from the row the argv came from.
    ///
    /// # Errors
    ///
    /// * [`SchedulerError::Spawn`] - no adapter is registered for the job's engine, or
    ///   the model vanished from the catalog mid-job.
    /// * [`SchedulerError::Engine`] - the adapter refused the entry (`MissingField` for
    ///   a key it requires, `Rejected` for an illegal or absent value).
    fn preflight_spec(&self, job_id: JobId) -> Result<(LaunchSpec, ModelEntry), SchedulerError> {
        let (engine, model) = {
            let job = self
                .jobs
                .get(&job_id)
                .expect("invariant: callers hold live job ids");
            (job.engine.clone(), job.model.clone())
        };
        let Some(adapter) = self.adapters.get(&engine).cloned() else {
            return Err(SchedulerError::Spawn(format!(
                "no adapter registered for engine `{engine}`"
            )));
        };
        let Some(entry) = self.catalog.iter().find(|m| m.name == model).cloned() else {
            return Err(SchedulerError::Spawn(format!(
                "model `{model}` vanished from the catalog mid-job"
            )));
        };
        let engine_port = self.slot_engine_port(job_id);
        adapter
            .spec(&entry, engine_port)
            .map(|spec| (spec, entry))
            .map_err(SchedulerError::from)
    }

    /// `Dispose -> Spawn` edge (G12): only called after release is confirmed, or when
    /// there was nothing to release.
    fn begin_spawn(&mut self, job_id: JobId) {
        self.step(job_id, SwapState::Spawn);
        let engine_port = self.slot_engine_port(job_id);
        let spawn_error = |reason: String| Event::Spawned(Err(SpawnError::SpawnFailed { reason }));
        let (spec, entry) = match self.preflight_spec(job_id) {
            Ok(launch) => launch,
            Err(error) => {
                // The `Spawn` arm already reads like a spawn failure; anything else
                // (an adapter refusal) is quoted verbatim, as before.
                let reason = match &error {
                    SchedulerError::Spawn(reason) => reason.clone(),
                    other => other.to_string(),
                };
                self.on_event(job_id, spawn_error(reason));
                return;
            }
        };
        // The provenance type carries the container identity (review, 2026-10-26):
        // the runtime reaps by the ticket's label, never by re-reading the spec.
        let kind = match &spec.container {
            Some(name) => ResourceKind::Container { name: name.clone() },
            None => ResourceKind::ChildProcess,
        };
        let ticket = SpawnTicket {
            generation: self.next_gen,
            kind,
            pid: 0,
            port: engine_port,
        };
        self.next_gen += 1;
        {
            let job = self
                .jobs
                .get_mut(&job_id)
                .expect("invariant: caller holds a live job id");
            job.spawned = Some(ticket.clone());
            job.readiness = Some(spec.readiness.clone());
            job.container = spec.container.clone();
            // The measured anchor is taken from the same entry the argv above was
            // built from, in the same actor turn: what the fold may later say about
            // this process is fixed here, and no `reload()` can move it.
            job.measured = Some(MeasuredAnchor::at_spawn(&entry));
        }
        let tx = self.tx.clone();
        let runtime = Arc::clone(&self.runtime);
        let handle = tokio::spawn(async move {
            let _ = tx.send(ActorMsg::Event {
                job: job_id,
                event: Event::Spawned(runtime.spawn(&spec, ticket).await),
            });
        });
        self.jobs
            .get_mut(&job_id)
            .expect("invariant: caller holds a live job id")
            .tasks
            .push(handle);
    }

    /// Readiness loop as a task; sends exactly one terminal event (Step 7 + §0.3).
    fn spawn_probe_task(&mut self, job_id: JobId, port: u16) {
        let readiness = self.jobs[&job_id]
            .readiness
            .clone()
            .expect("invariant: readiness is set when a spawn exists");
        let tx = self.tx.clone();
        let runtime = Arc::clone(&self.runtime);
        let deadline = Duration::from_secs(
            self.jobs[&job_id]
                .load_timeout_s
                .unwrap_or(self.cfg.load_timeout_s),
        );
        let handle = tokio::spawn(async move {
            let started = Instant::now();
            let mut discovered = None;
            let terminal = loop {
                match runtime.probe(port, &readiness).await {
                    Probe::Ready => {
                        // Step 10 (F-4): the upstream id is discovered, not asserted.
                        discovered = runtime.discover_model_id(port).await;
                        break Probe::Ready;
                    }
                    Probe::Failed { reason } => break Probe::Failed { reason },
                    Probe::NotYet { reason } => {
                        if started.elapsed() >= deadline {
                            break Probe::Failed {
                                reason: format!(
                                    "readiness deadline {deadline:?} exceeded; last: {reason}"
                                ),
                            };
                        }
                    }
                }
                tokio::time::sleep(PROBE_INTERVAL).await;
            };
            let _ = tx.send(ActorMsg::Event {
                job: job_id,
                event: Event::Probed(terminal, discovered),
            });
        });
        self.jobs
            .get_mut(&job_id)
            .expect("invariant: caller holds a live job id")
            .tasks
            .push(handle);
    }

    /// Whether a reap error on one target should abort the whole reap.
    ///
    /// A stale ticket means the process this job meant is already gone — there is nothing
    /// left to reap for that target, and aborting here would strand the job's OTHER
    /// target, its own live engine (chunk 2, 2026-10-27). Any other failure is real and
    /// must surface.
    fn reap_should_continue(error: &SpawnError) -> bool {
        matches!(error, SpawnError::StaleTicket { .. })
    }

    /// Kill every process this job owns (its pending victim and its own spawn) and
    /// confirm release with evidence, inside `release_timeout_s` (G12).
    fn spawn_kill_confirm(&mut self, job_id: JobId) {
        // Both targets, never one chosen by preference. `job.victim` is only ever
        // assigned (when a victim is queued or dequeued) and never cleared, so the old
        // `match job.victim.as_ref()` re-killed a victim that had already been released
        // and never killed the engine this job had itself started — leaving it holding
        // the card and its port while the slot read empty (review CRITICAL, 2026-10-27).
        let targets: Vec<Victim> = {
            let job = &self.jobs[&job_id];
            let mut targets: Vec<Victim> = Vec::with_capacity(2);
            if let Some(victim) = job.victim.clone() {
                targets.push(victim);
            }
            if let Some(ticket) = job.spawned.clone() {
                if !targets.iter().any(|t| t.ticket == ticket) {
                    targets.push(Victim {
                        model: job.model.clone(),
                        ticket,
                        // Unused on this path: a job reaping its own spawn has its slot
                        // cleared by its own transition, not by the victim-slot check.
                        slot: job.slot.unwrap_or(0),
                        evicted: false,
                        cost: 0.0,
                    });
                }
            }
            targets
        };
        if targets.is_empty() {
            tracing::warn!(
                %job_id,
                "nothing to reap: release phase with neither a victim nor a spawn"
            );
            return;
        }
        let tx = self.tx.clone();
        let runtime = Arc::clone(&self.runtime);
        let timeout = Duration::from_secs(self.cfg.release_timeout_s);
        let handle = tokio::spawn(async move {
            let holder = targets
                .iter()
                .map(|t| t.model.clone())
                .collect::<Vec<_>>()
                .join("+");
            let outcome = match tokio::time::timeout(timeout, async {
                for target in &targets {
                    if let Err(error) = runtime.kill(&target.ticket).await {
                        if !Self::reap_should_continue(&error) {
                            return Err(error);
                        }
                        tracing::warn!(
                            port = target.ticket.port,
                            generation = target.ticket.generation,
                            model = %target.model,
                            "stale reap target: its process is already gone, reaping the rest"
                        );
                    }
                }
                for target in &targets {
                    loop {
                        match runtime.release_confirmed(&target.ticket).await {
                            Ok(true) => break,
                            Ok(false) => tokio::time::sleep(RELEASE_POLL).await,
                            Err(error) if Self::reap_should_continue(&error) => {
                                tracing::warn!(
                                    port = target.ticket.port,
                                    "stale reap target: nothing left to confirm"
                                );
                                break;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                }
                Ok(())
            })
            .await
            {
                Ok(inner) => inner,
                Err(_) => Err(SpawnError::KillFailed {
                    holder,
                    reason: "release not confirmed within release_timeout_s".to_string(),
                }),
            };
            let _ = tx.send(ActorMsg::Event {
                job: job_id,
                event: Event::Released(outcome),
            });
        });
        self.jobs
            .get_mut(&job_id)
            .expect("invariant: caller holds a live job id")
            .tasks
            .push(handle);
    }

    fn on_event(&mut self, job_id: JobId, event: Event) {
        self.apply_event(job_id, event);
        self.publish();
    }

    fn apply_event(&mut self, job_id: JobId, event: Event) {
        let Some(job) = self.jobs.get(&job_id) else {
            return;
        };
        if job.terminal {
            return; // a stale step result after cancel/supersede
        }
        let state = job.state;
        // The admission decision is witness-driven (Step 4); every other step
        // matches its expected state.
        if let Event::Admitted(sample) = event {
            if state == SwapState::Admit {
                self.finish_admit(job_id, sample);
            } else {
                tracing::warn!(%job_id, ?state, "stale admission sample ignored");
            }
            return;
        }
        match (state, event) {
            (SwapState::Dispose, Event::Released(Ok(()))) => {
                self.set_timing(job_id, |t| &mut t.release_ms);
                self.clear_released_victim_slot(job_id);
                if let Some(victim) = self.jobs[&job_id].victim.clone() {
                    if let Some(job) = self.jobs.get_mut(&job_id) {
                        job.released_cost += victim.cost;
                        if victim.evicted {
                            job.victims_released.push(victim.model.clone());
                        }
                    }
                }
                if self.jobs[&job_id].kind == JobKind::Load {
                    // Step 6: release cross victims one at a time; after each
                    // release re-check the gate and stop as soon as it is satisfied
                    // (minimality — never kill more than the load needs).
                    if self.dequeue_victim(job_id) {
                        // The planned gate value from finish_admit — max(need, floor),
                        // never the (greedy-clamped) booking alone, or the floor
                        // victims would be skipped after the first release
                        // (review CRITICAL, 2026-10-26).
                        let target = self.jobs[&job_id]
                            .gate_target
                            .unwrap_or_else(|| self.jobs[&job_id].need.map_or(0.0, |n| n.0));
                        // free_at_admit was computed with everything still in the
                        // ledger; every confirmed release has since freed its cost.
                        // The observed figure is the same arithmetic against the
                        // card as physically sampled at admit (never ledger-only).
                        let available = self.jobs[&job_id].free_at_admit.map_or(0.0, |f| f.0)
                            + self.jobs[&job_id].released_cost;
                        let available = self.jobs[&job_id]
                            .observed_at_admit
                            .map_or(available, |obs| {
                                available.min(obs + self.jobs[&job_id].released_cost)
                            });
                        if target <= available {
                            if let Some(job) = self.jobs.get_mut(&job_id) {
                                job.victims.clear();
                            }
                            tracing::info!(%job_id, "release sufficient; skipping remaining victims");
                            self.spawn_settle_task(job_id);
                        } else {
                            self.spawn_kill_confirm(job_id);
                        }
                    } else {
                        // §0.3: release is proven; now the memory evidence must
                        // settle before the spawn fires.
                        self.spawn_settle_task(job_id);
                    }
                } else {
                    let note = self.jobs[&job_id]
                        .note
                        .clone()
                        .unwrap_or_else(|| "unloaded".to_string());
                    self.step(job_id, SwapState::Reap);
                    self.step(job_id, SwapState::Idle);
                    self.finish(job_id, Ok(Reply::Done), &note);
                }
            }
            (SwapState::Dispose, Event::Released(Err(error))) => {
                let holder = self.jobs[&job_id]
                    .victim
                    .as_ref()
                    .map(|v| v.model.clone())
                    .unwrap_or_else(|| job_id.to_string());
                self.fail_job(
                    job_id,
                    SchedulerError::ReleaseTimeout { holder },
                    &error.to_string(),
                );
            }
            (SwapState::Spawn, Event::Spawned(Ok(spawned))) => {
                if let Some(ticket) = self.jobs.get_mut(&job_id).and_then(|j| j.spawned.as_mut()) {
                    ticket.port = spawned.port;
                    ticket.pid = spawned.pid;
                }
                self.set_timing(job_id, |t| &mut t.spawn_ms);
                self.step(job_id, SwapState::Ready);
                self.spawn_probe_task(job_id, spawned.port);
            }
            (SwapState::Spawn, Event::Spawned(Err(error))) => {
                self.fail_job(
                    job_id,
                    SchedulerError::Spawn(error.to_string()),
                    "spawn failed",
                );
            }
            (SwapState::Ready, Event::Probed(Probe::Ready, discovered)) => {
                self.install(job_id, discovered);
            }
            (SwapState::Ready, Event::Probed(Probe::Failed { reason }, _)) => {
                self.fail_job(
                    job_id,
                    SchedulerError::EngineFailed(reason),
                    "engine not ready",
                );
            }
            (SwapState::Ready, Event::Probed(Probe::NotYet { .. }, _)) => {
                tracing::warn!(%job_id, "probe task sent a non-terminal result; ignored");
            }
            (SwapState::Failed, Event::Released(_)) => {
                self.step(job_id, SwapState::Reap);
                self.step(job_id, SwapState::Idle);
                let error = self.jobs[&job_id]
                    .error
                    .clone()
                    .unwrap_or_else(|| SchedulerError::Spawn("unknown failure".to_string()));
                self.finish(job_id, Err(error), "failed");
            }
            (SwapState::CancelRequested, Event::Released(Ok(()))) => {
                // The release this job's own (aborted) reap task never proved: now the
                // victim's engine is gone, so its slot must go the way a completed
                // Dispose release takes it, or the ledger debits a corpse forever.
                self.clear_released_victim_slot(job_id);
                self.finish_cancelled(job_id);
            }
            (SwapState::CancelRequested, Event::Released(Err(_))) => {
                // An unconfirmed release leaves the engine possibly alive: the slot
                // stays installed rather than claiming a death nobody proved.
                self.finish_cancelled(job_id);
            }
            (SwapState::Dispose, Event::Settled(Ok(()))) => {
                self.begin_spawn(job_id);
            }
            (SwapState::Dispose, Event::Settled(Err(reason))) => {
                self.fail_job(
                    job_id,
                    SchedulerError::SettleTimeout { reason },
                    "memory did not settle after release",
                );
            }
            (other_state, other_event) => {
                tracing::warn!(state = ?other_state, event = ?other_event, %job_id, "unexpected event ignored");
            }
        }
    }

    /// `Ready -> Install -> Serving`: the one place an occupant is created (F-6 funnel).
    /// `discovered_id` is the engine's self-reported id (Step 10, F-4 fix).
    fn install(&mut self, job_id: JobId, discovered_id: Option<String>) {
        self.set_timing(job_id, |t| &mut t.ready_ms);
        self.step(job_id, SwapState::Install);
        let (slot, model, category, port) = {
            let job = &self.jobs[&job_id];
            let ticket = job
                .spawned
                .clone()
                .expect("invariant: install only happens after a spawn");
            (
                job.slot,
                job.model.clone(),
                job.category,
                Spawned {
                    port: ticket.port,
                    pid: 0,
                },
            )
        };
        // F-4: a yaml that claims an id the engine never reports becomes a visible
        // warning at load time, and the engine's real id wins for rewriting. Judged
        // against the spawn-era anchor, never the live catalog: this install runs
        // minutes after `begin_spawn`, and the process being judged was launched from
        // the bytes that anchor captured (FINDINGS-BATCH-2026-10-28L [L-2]).
        let serve_id = discovered_id;
        if let Some(job) = self.jobs.get_mut(&job_id) {
            if let Some(warning) = serve_model_warning(job.measured.as_ref(), serve_id.as_deref()) {
                tracing::warn!(%job_id, model = %job.model, "{warning}");
                job.warnings.push(warning);
            }
        }
        if let Some(slot) = slot {
            // Extract first: `clear_occupant` needs `&mut self`, and a replaced
            // occupant's sampler must die with it (review, 2026-10-27).
            let job = &self.jobs[&job_id];
            let (job_model, job_need, job_ticket) = (
                job.model.clone(),
                job.need.unwrap_or_default(),
                job.spawned
                    .clone()
                    .expect("invariant: install only happens after a spawn"),
            );
            self.clear_occupant(slot);
            let others_booked = self
                .slots
                .iter()
                .filter_map(|s| s.occupant.as_ref())
                .filter(|o| o.model != job_model)
                .map(|o| o.need.0)
                .sum();
            if let Some(job) = self.jobs.get_mut(&job_id) {
                job.other_need_at_admit = others_booked;
            }
            // Axis 2 (ruling 2026-10-29): capture the yaml this launch came from at
            // install — one stat, on the actor, for the one event that creates a
            // resident. The path is the fold anchor's own file (the identity the
            // measured persist writes), so the marker and the fold never drift onto
            // different files; an unreadable stat (or a synthetic row) yields no
            // capture, and the verdict then never accuses.
            let spawn_config = self
                .jobs
                .get(&job_id)
                .and_then(|job| job.measured.as_ref())
                .and_then(|anchor| {
                    anchor.file.as_ref().and_then(|file| {
                        crate::model::fingerprint(file).map(|fp| (file.clone(), fp))
                    })
                });
            self.slots[slot].occupant = Some(Occupant {
                model: job_model,
                need: job_need,
                port: port.port,
                ticket: job_ticket.clone(),
                loaded_at: Instant::now(),
                launched_at_wall: std::time::SystemTime::now(),
                spawn_config,
                stale_config: false,
                serve_id: serve_id.clone(),
                measure: None,
            });
        }
        // Measured footprint (§0.4, rehearsal item c): witness readings taken while the
        // model serves, folded into its yaml range. The baseline is the free memory
        // *after* this load's own release, so a replacement is not measured short by the
        // victim's whole footprint. The card's per-process attribution is preferred
        // when the driver reports it; the total-diff stands in otherwise, which is what
        // makes a container-backed engine measurable at all (its ticket pid is a
        // short-lived `wsl.exe` client). What to charge is the actor's call alone: it
        // knows who else is booked and which engine actually holds the slot.
        let ticket = self.jobs.get(&job_id).and_then(|job| job.spawned.clone());
        let own_pids: Vec<u32> = match (
            ticket.as_ref(),
            self.jobs
                .get(&job_id)
                .is_some_and(|job| job.container.is_none()),
        ) {
            (Some(ticket), true) => vec![ticket.pid],
            _ => Vec::new(),
        };
        let baseline_free = self.post_release_free(job_id);
        if let (Some(slot), Some(baseline_free), Some(ticket)) = (slot, baseline_free, ticket) {
            let witness = Arc::clone(&self.witness);
            let tx = self.tx.clone();
            let measured_model = model.clone();
            let every = std::time::Duration::from_secs(self.cfg.measured_sample_s.max(1));
            let handle = tokio::spawn(async move {
                let mut tick = tokio::time::interval(every);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // The first interval tick completes at once; consume it, because a
                // sample taken the instant the port opened measures the state the load
                // was admitted in — and it would spend a scripted witness sample.
                tick.tick().await;
                // Bounded by wall clock, not by tick count: a 86_400-iteration loop at
                // the 60 s default would run for sixty days.
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(86_400);
                loop {
                    tick.tick().await;
                    if tokio::time::Instant::now() >= deadline {
                        return;
                    }
                    match witness.sample().await {
                        Ok(sample) => {
                            // `free_gib()` clamps, so a NaN or nonsense reading would
                            // otherwise arrive as "the model took the whole card" and
                            // be persisted to the operator's file forever.
                            if !sample.total_gib.is_finite()
                                || !sample.used_gib.is_finite()
                                || sample.total_gib <= 0.0
                                || sample.used_gib < 0.0
                            {
                                continue;
                            }
                            let total_diff = baseline_free - (sample.total_gib - sample.used_gib);
                            if !total_diff.is_finite() {
                                continue;
                            }
                            if tx
                                .send(ActorMsg::Measured {
                                    model: measured_model.clone(),
                                    ticket: ticket.clone(),
                                    total_diff,
                                    own: sample.own_gib(&own_pids),
                                    total_gib: sample.total_gib,
                                })
                                .is_err()
                            {
                                return; // actor gone
                            }
                        }
                        // A witness that cannot answer yields no measurement, never a
                        // false one (§0.3): stop rather than guess or hammer it.
                        Err(_) => return,
                    }
                }
            });
            if let Some(occupant) = self.slots.get_mut(slot).and_then(|s| s.occupant.as_mut()) {
                occupant.measure = Some(handle);
            }
        }
        self.set_timing(job_id, |t| &mut t.install_ms);
        self.step(job_id, SwapState::Serving);
        self.finish(
            job_id,
            Ok(Reply::Loaded(SwapReceipt {
                job: job_id,
                model,
                category,
                port: port.port,
                serve_id,
            })),
            "loaded",
        );
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
            floor_gib = job.launch_floor,
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

    fn snapshot(&self) -> SchedulerStatus {
        let now = Instant::now();
        SchedulerStatus {
            catalog_errors: self.catalog_errors.clone(),
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
                    launched_at: s
                        .occupant
                        .as_ref()
                        .map(|o| crate::model::iso8601_utc(o.launched_at_wall)),
                    stale_config: s.occupant.as_ref().map(|o| o.stale_config),
                    loaded_at: s.occupant.as_ref().map_or(now, |o| o.loaded_at),
                })
                .collect(),
            jobs: self
                .jobs
                .iter()
                .map(|(id, j)| JobStatus {
                    id: *id,
                    kind: j.kind,
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

impl Job {
    /// Whether this job still owes a confirmed release for its victim.
    ///
    /// `victim` is only ever assigned and never cleared (see `spawn_kill_confirm`), so
    /// this answers "a reap target exists", not "a task is parked on it". It is
    /// deliberately independent of `state`: its only caller (`cancel_job`) asks it while
    /// it is moving the job out of that state, and a cancel that arrives in `Dispose`
    /// must still kill and confirm the victim (chunk 4 [1]).
    fn victim_release_pending(&self) -> bool {
        self.victim.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        headline_fault, missing_binary_error, serve_model_warning, ActorMsg, ActorState, Gibibytes,
        Job, JobId, JobKind, JobStatus, JobTimings, MeasuredAnchor, Occupant, Reply, ResourceKind,
        ScanKind, Scheduler, Slot, SpawnTicket, SwapState,
    };
    use crate::budget::LedgerOnlyWitness;
    use crate::config::CategoryId;
    use crate::engine::mock::MockAdapter;
    use crate::engine::EngineAdapter;
    use crate::model::ModelEntry;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    /// Process-global warn capture for the persist-drift test: `tracing`'s default
    /// dispatcher is thread-local, and the persist warn fires on a blocking-pool
    /// thread, so the capturing layer goes in as the global default exactly once
    /// (no lib test installs one; `observability::init` runs only in `main`).
    type CapturedWarn = (String, BTreeMap<String, String>);
    static WARN_CAPTURE: OnceLock<Arc<Mutex<Vec<CapturedWarn>>>> = OnceLock::new();

    struct CaptureVisitor<'a> {
        message: &'a mut String,
        fields: &'a mut BTreeMap<String, String>,
    }

    impl tracing::field::Visit for CaptureVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                *self.message = format!("{value:?}");
            } else {
                self.fields
                    .insert(field.name().to_string(), format!("{value:?}"));
            }
        }
    }

    struct CaptureLayer(Arc<Mutex<Vec<CapturedWarn>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut message = String::new();
            let mut fields = BTreeMap::new();
            event.record(&mut CaptureVisitor {
                message: &mut message,
                fields: &mut fields,
            });
            self.0.lock().expect("capture lock").push((message, fields));
        }
    }

    fn install_warn_capture() -> Arc<Mutex<Vec<CapturedWarn>>> {
        WARN_CAPTURE
            .get_or_init(|| {
                let store = Arc::new(Mutex::new(Vec::new()));
                let _ = tracing_subscriber::registry()
                    .with(CaptureLayer(store.clone()))
                    .try_init();
                store
            })
            .clone()
    }

    /// Constructor-only stand-in for the never-called runtime half of an
    /// [`ActorState`]: the boot-thread pin builds state and reads its catalog field,
    /// it never spawns, kills or probes, so any call reaching here is a test bug.
    struct NeverRuntime;

    #[async_trait::async_trait]
    impl crate::engine::EngineRuntime for NeverRuntime {
        async fn spawn(
            &self,
            _spec: &crate::engine::LaunchSpec,
            _ticket: crate::ownership::SpawnTicket,
        ) -> Result<crate::engine::Spawned, crate::engine::SpawnError> {
            panic!("the boot-thread pin must never call the runtime");
        }
        async fn kill(
            &self,
            _ticket: &crate::ownership::SpawnTicket,
        ) -> Result<(), crate::engine::SpawnError> {
            panic!("the boot-thread pin must never call the runtime");
        }
        async fn release_confirmed(
            &self,
            _ticket: &crate::ownership::SpawnTicket,
        ) -> Result<bool, crate::engine::SpawnError> {
            panic!("the boot-thread pin must never call the runtime");
        }
        async fn probe(
            &self,
            _port: u16,
            _readiness: &crate::engine::ReadinessSpec,
        ) -> crate::engine::Probe {
            panic!("the boot-thread pin must never call the runtime");
        }
        async fn discover_model_id(&self, _port: u16) -> Option<String> {
            panic!("the boot-thread pin must never call the runtime");
        }
    }

    struct BootWiring {
        adapters: BTreeMap<String, Arc<dyn EngineAdapter>>,
        runtime: Arc<dyn crate::engine::EngineRuntime>,
        witness: Arc<dyn crate::budget::VramWitness>,
        tx: tokio::sync::mpsc::UnboundedSender<super::ActorMsg>,
    }

    fn boot_stub_wiring() -> BootWiring {
        let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
        adapters.insert(
            "mock".to_string(),
            Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
        );
        BootWiring {
            adapters,
            runtime: Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
            witness: Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
            tx: tokio::sync::mpsc::unbounded_channel().0,
        }
    }

    fn seed_one_model(dir: &std::path::Path, name: &str) {
        std::fs::write(
            dir.join(format!("{name}.yaml")),
            format!("name: {name}\nengine: mock\ncategory: work\n"),
        )
        .expect("model yaml");
    }

    #[test]
    fn actor_state_construction_never_scans_the_catalog() {
        // FINDINGS-14 [1], boot-thread pin: the constructor that `Scheduler::start`
        // funnels through — including the `ActorState::empty` path it delegates to —
        // must return against a dir holding a valid model with the catalog still
        // empty. Re-introducing the scan into the constructor (the pre-bind stall)
        // fails this assertion. Deterministic: no task has run, so nothing can have
        // scanned yet.
        let dir = crate::testutil::temp_dir("bootscan");
        seed_one_model(&dir, "boot-stub");
        let wiring = boot_stub_wiring();
        let state = ActorState::new(
            crate::config::AppConfig::default(),
            dir.clone(),
            wiring.adapters,
            wiring.runtime,
            wiring.witness,
            wiring.tx,
        );
        assert!(
            state.catalog.is_empty(),
            "the constructor must not scan: a pre-bind catalog read is the \
             FINDINGS-14 [1] stall; {:?}",
            state
                .catalog
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<Vec<_>>()
        );
        let wiring = boot_stub_wiring();
        let state = ActorState::empty(
            crate::config::AppConfig::default(),
            dir.clone(),
            wiring.adapters,
            wiring.runtime,
            wiring.witness,
            wiring.tx,
        );
        assert!(
            state.catalog.is_empty(),
            "ActorState::empty must not scan either; {:?}",
            state
                .catalog
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn actor_task_performs_the_initial_scan() {
        // FINDINGS-14 [1], actor-side pin: the scan `ActorState::new` no longer runs
        // must still happen — as the first thing the actor task does. A command sent
        // immediately after `start()` (here the catalog read) queues behind the scan
        // on the unbounded channel and observes its result; no sleep or timing
        // assumption is involved, only a bounded poll for the eventual occupancy of
        // the seeded model.
        let dir = crate::testutil::temp_dir("bootscan");
        seed_one_model(&dir, "boot-model");
        let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
        adapters.insert(
            "mock".to_string(),
            Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
        );
        let handle = Scheduler::start(
            crate::config::AppConfig::default(),
            dir.clone(),
            adapters,
            Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
            Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let catalog = handle.catalog().await.expect("actor alive");
            if catalog
                .models
                .iter()
                .any(|entry| entry.name == "boot-model")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the actor-side initial scan never populated the catalog: {:?}",
                catalog
                    .models
                    .iter()
                    .map(|entry| entry.name.clone())
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_command_during_an_in_flight_rescan_is_answered() {
        // FINDINGS-13 [6]: the reload scan parks on a test-only seam; while it is
        // in flight the actor must keep answering commands, and the reload reply
        // may resolve only when the new catalog is APPLIED (never when the scan
        // was dispatched — the /har/rescan contract). Pre-fix (the inline scan),
        // the parked scan holds the actor thread and the status poll below times
        // out — which is exactly why this test runs on a multi-thread runtime:
        // the timer has to survive a thread that a blocking scan has parked.
        let dir = crate::testutil::temp_dir("reloadresp");
        seed_one_model(&dir, "resident");
        let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
        adapters.insert(
            "mock".to_string(),
            Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
        );
        let handle = Scheduler::start(
            crate::config::AppConfig::default(),
            dir.clone(),
            adapters,
            Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
            Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        );
        // The boot scan must have landed before the seam is armed: the seam is
        // single-shot for this dir and belongs to the reload alone.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let catalog = handle.catalog().await.expect("actor alive");
            if catalog.models.iter().any(|entry| entry.name == "resident") {
                break;
            }
            assert!(Instant::now() < deadline, "the boot scan never landed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        std::fs::write(
            dir.join("late.yaml"),
            "engine: mock\ncategory: work\nvram_gb: 2\n",
        )
        .expect("seed late model");

        let (started, release) = crate::model::scan::arm_scan_pause(&dir);
        let client = handle.clone();
        let reload = tokio::spawn(async move { client.reload().await });
        let grabbed = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || started.recv().is_ok()),
        )
        .await;
        assert!(
            matches!(grabbed, Ok(Ok(true))),
            "the reload scan never parked on the seam (is scan_dir off-thread?)"
        );

        let answered = tokio::time::timeout(Duration::from_secs(2), handle.status()).await;
        assert!(
            matches!(answered, Ok(Ok(_))),
            "the actor stopped answering while a rescan was in flight"
        );
        assert!(
            !reload.is_finished(),
            "the reload reply resolved before the scan even completed"
        );

        drop(release);
        let reply = tokio::time::timeout(Duration::from_secs(10), reload)
            .await
            .expect("the reload resolves once released")
            .expect("join")
            .expect("reply");
        assert_eq!(reply, Reply::Done);
        let catalog = handle.catalog().await.expect("actor alive");
        assert!(
            catalog.models.iter().any(|entry| entry.name == "late"),
            "the reply meant applied: the new model must be in the catalog"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_unavailable_engine_is_dropped_and_named() {
        // The audit verdict shipped with this batch (FINDINGS-13 [3] follow-up): no
        // fixture publishes an engine its adapter map lacks, and such an entry can
        // only ever die later as `no adapter registered` — so the actor refuses it
        // at apply time, DROPPING it with a typed, page-visible reason.
        let dir = crate::testutil::temp_dir("engineunavail");
        std::fs::write(
            dir.join("ghost.yaml"),
            "engine: phantom\ncategory: work\nvram_gb: 2\n",
        )
        .expect("seed ghost");
        seed_one_model(&dir, "resident");
        let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
        adapters.insert(
            "mock".to_string(),
            Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
        );
        let handle = Scheduler::start(
            crate::config::AppConfig::default(),
            dir.clone(),
            adapters,
            Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
            Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let catalog = loop {
            let catalog = handle.catalog().await.expect("actor alive");
            let reported = catalog
                .errors
                .iter()
                .any(|e| e.kind == crate::model::scan::ScanKind::EngineUnavailable);
            let settled = catalog.models.iter().any(|entry| entry.name == "resident");
            assert!(
                Instant::now() < deadline,
                "scan never reported: {catalog:?}"
            );
            if reported && settled {
                break catalog;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(
            !catalog.models.iter().any(|entry| entry.name == "ghost"),
            "an engine with no adapter must be dropped from the catalog"
        );
        let error = catalog
            .errors
            .iter()
            .find(|e| e.kind == crate::model::scan::ScanKind::EngineUnavailable)
            .expect("the drop must be reported");
        assert_eq!(error.file, "ghost.yaml");
        assert!(
            error.reason == "engine \"phantom\" is not available to this harness (known: mock)",
            "{}",
            error.reason
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Seats a spawn exactly as the actor does: the terminal load row that owns the
    /// ticket — carrying the spawn-era [`MeasuredAnchor`] built by the very function
    /// [`ActorState::begin_spawn`] calls — plus the occupant that holds its sampler.
    /// A pin that folds a reading must go through this: with no spawn row there is no
    /// anchor, and an unanchored reading has neither an honest stamp to mint nor a
    /// file it may write to.
    fn seat_spawn(state: &mut ActorState, entry: &ModelEntry, ticket: &SpawnTicket, port: u16) {
        let job_id = JobId(state.next_job);
        state.next_job += 1;
        state.jobs.insert(
            job_id,
            Job {
                kind: JobKind::Load,
                model: entry.name.clone(),
                engine: entry.engine.clone(),
                category: CategoryId::Work,
                state: SwapState::Serving,
                terminal: true,
                outcome: Some("loaded".to_string()),
                error: None,
                need: Some(Gibibytes(2.0)),
                cost: entry.cost,
                launch_floor: entry.launch_floor_gb,
                load_timeout_s: entry.load_timeout_s,
                gate_target: Some(2.0),
                free_at_admit: Some(Gibibytes(20.0)),
                observed_at_admit: Some(20.0),
                container: None,
                timings: JobTimings::default(),
                t0: Instant::now(),
                waiters: Vec::new(),
                tasks: Vec::new(),
                spawned: Some(ticket.clone()),
                readiness: None,
                measured: Some(MeasuredAnchor::at_spawn(entry)),
                victim: None,
                victims: Vec::new(),
                victims_released: Vec::new(),
                released_cost: 0.0,
                other_need_at_admit: 0.0,
                note: Some("loaded".to_string()),
                warnings: Vec::new(),
                slot: Some(0),
                queue_fingerprint: None,
            },
        );
        state.slots = vec![Slot {
            category: CategoryId::Work,
            index: 0,
            engine_port: port,
            occupant: Some(Occupant {
                model: entry.name.clone(),
                need: Gibibytes(2.0),
                port,
                ticket: ticket.clone(),
                loaded_at: Instant::now(),
                launched_at_wall: std::time::SystemTime::now(),
                spawn_config: None,
                stale_config: false,
                serve_id: None,
                measure: None,
            }),
            job: Some(job_id),
        }];
    }

    /// A persist the disk refuses must not leave memory booking what disk denied
    /// (Fork C, closing FINDINGS-13 [4]'s warn-only tail): the warn fires AND the
    /// catalog entry's measured triple drains to its none state. The entry is a
    /// `llama` row so the persist really carries the stamped triple — non-offload
    /// engines never mint the stamp (`only_offload_engines_stamp_measured_offload`).
    /// The wedged-drive handle is Windows-only, so the test is gated like its
    /// writeback sibling (FINDINGS-BATCH-2026-10-28 [7]).
    #[cfg(windows)]
    #[tokio::test]
    async fn a_refused_measured_persist_drains_the_triple_from_memory() {
        let capture = install_warn_capture();
        let dir = crate::testutil::temp_dir("measured-persist");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let yaml = dir.join("drift.yaml");
        std::fs::write(&yaml, "engine: llama\ncategory: work\nvram_gb: 2\n").expect("write");

        // Refuse the validated rename the way a wedged drive does: hold the target
        // open with delete-sharing off. Reads still succeed (the batch must be
        // able to read the file), the replace cannot.
        let _lock = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(0x3) // FILE_SHARE_READ | FILE_SHARE_WRITE (no DELETE)
                .open(&yaml)
                .expect("exclusive handle")
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<ActorMsg>();
        let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
        adapters.insert(
            "mock".to_string(),
            Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
        );
        let mut state = ActorState::empty(
            crate::config::AppConfig::default(),
            dir.clone(),
            adapters,
            Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
            Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
            tx,
        );
        let (map, problems) =
            crate::model::scan::parse_flat_yaml("engine: llama\ncategory: work\nvram_gb: 2\n");
        assert!(problems.is_empty(), "{problems:?}");
        let mut entry = ModelEntry::from_flat(&map, "drift").expect("entry");
        entry.file = Some(yaml.clone());
        let ticket = SpawnTicket {
            generation: 1,
            kind: ResourceKind::ChildProcess,
            pid: 4242,
            port: 18_991,
        };
        state.catalog = vec![entry.clone()];
        seat_spawn(&mut state, &entry, &ticket, 18_991);

        state.on_measured("drift", &ticket, 3.7, Some(3.7), 24.0);
        assert_eq!(
            state.catalog[0].measured_gb,
            Some(3.7),
            "the reading books in memory first"
        );

        let msg = tokio::time::timeout(Duration::from_secs(15), rx.recv())
            .await
            .expect("the persist failure reaches the actor")
            .expect("channel open");
        let ActorMsg::MeasuredPersistFailed {
            model,
            high,
            low,
            offload,
            file,
        } = msg
        else {
            panic!("expected the persist-failed reconcile message");
        };
        assert_eq!(model, "drift");
        assert_eq!((high, low, offload), (3.7, 3.7, Some(false)));
        assert_eq!(
            file.as_deref(),
            Some(yaml.as_path()),
            "the reconcile names the spawn-era row it refused to write"
        );

        state.on_measured_persist_failed(&model, high, low, offload, file);
        let entry = &state.catalog[0];
        assert_eq!(entry.measured_gb, None);
        assert_eq!(entry.measured_min_gb, None);
        assert_eq!(entry.measured_offload, None);

        let warns = capture.lock().expect("capture lock");
        assert!(
            warns.iter().any(|(message, fields)| {
                message.contains("cannot persist the measured range")
                    && fields.get("model").is_some_and(|m| m.contains("drift"))
            }),
            "the warn must have fired: {warns:?}"
        );
        // And the file really never gained the keys the memory had booked.
        let text = std::fs::read_to_string(&yaml).expect("read back");
        assert!(
            !text.contains("measured_gb"),
            "disk refused the triple: {text}"
        );

        drop(_lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDINGS2-04 [1]: the fold books the row that owns the spawn-era FILE, while the
    /// drain keyed `name && file`. Wherever the file's row answers to a different name
    /// than the spawn-era string, the drain missed the very row it exists to clear:
    /// memory kept booking a footprint the disk refused for the whole serve window
    /// (later samples reproduce the same high/low against the updated anchor range, so
    /// `changed` stays false and no retry persist ever fires — it healed only on
    /// rescan). Scan derives identity from the file stem today, so the state is built by
    /// hand: it is the vocabulary the DEFERRED `name:`-key hard-reject opens up, and the
    /// drain must not be the line that turns it into a leak. Falsifier: restore the
    /// `name && file` key and the renamed row keeps `Some(3.7)` after the drain.
    #[cfg(windows)]
    #[tokio::test]
    async fn the_refused_persist_drain_follows_the_file_the_fold_booked() {
        let _capture = install_warn_capture();
        let dir = crate::testutil::temp_dir("measured-drain-file");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let yaml = dir.join("renamed.yaml");
        std::fs::write(&yaml, "engine: llama\ncategory: work\nvram_gb: 2\n").expect("write");
        let _lock = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(0x3)
                .open(&yaml)
                .expect("exclusive handle")
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<ActorMsg>();
        let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
        adapters.insert(
            "mock".to_string(),
            Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
        );
        let mut state = ActorState::empty(
            crate::config::AppConfig::default(),
            dir.clone(),
            adapters,
            Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
            Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
            tx,
        );
        let (map, problems) =
            crate::model::scan::parse_flat_yaml("engine: llama\ncategory: work\nvram_gb: 2\n");
        assert!(problems.is_empty(), "{problems:?}");
        let mut spawn_era = ModelEntry::from_flat(&map, "renamed").expect("entry");
        spawn_era.file = Some(yaml.clone());
        let ticket = SpawnTicket {
            generation: 1,
            kind: ResourceKind::ChildProcess,
            pid: 4243,
            port: 18_997,
        };
        state.catalog = vec![spawn_era.clone()];
        seat_spawn(&mut state, &spawn_era, &ticket, 18_997);

        // The reload the finding names: the same bytes on disk, a row that answers to a
        // different name. The engine is still serving the spawn-era string.
        state.catalog = vec![ModelEntry {
            name: "moved".to_string(),
            ..spawn_era.clone()
        }];

        state.on_measured("renamed", &ticket, 3.7, Some(3.7), 24.0);
        assert_eq!(
            state.catalog[0].measured_gb,
            Some(3.7),
            "the fold reaches the row by file, whatever it is called now"
        );

        let msg = tokio::time::timeout(Duration::from_secs(15), rx.recv())
            .await
            .expect("the persist failure reaches the actor")
            .expect("channel open");
        let ActorMsg::MeasuredPersistFailed {
            model,
            high,
            low,
            offload,
            file,
        } = msg
        else {
            panic!("expected the persist-failed reconcile message");
        };
        assert_eq!(
            model, "renamed",
            "the message carries the spawn-era name, which is what used to defeat the drain"
        );
        assert_eq!(file.as_deref(), Some(yaml.as_path()));

        state.on_measured_persist_failed(&model, high, low, offload, file);
        let entry = &state.catalog[0];
        assert_eq!(
            entry.measured_gb, None,
            "memory may not book what the disk refused, whichever name the row answers to"
        );
        assert_eq!(entry.measured_min_gb, None);
        assert_eq!(entry.measured_offload, None);

        drop(_lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDINGS2-04 [3]: a spawn whose anchor carries `file: None` passed the serving
    /// and anchor checks and returned early WITHOUT retiring the sampler — the one
    /// reading class that left the task walking the witness every `measured_sample_s`
    /// until its 24 h deadline while the actor discarded every reading it produced.
    /// `apply_catalog` keeps file-None rows whose engine has an adapter, so the door is
    /// config-inert, not type-inert. Falsifier: delete the `retire_sampler` call from
    /// the file-None arm and the sampler is still attached.
    #[tokio::test]
    async fn a_synthetic_anchor_retires_the_sampler_that_has_nowhere_to_write() {
        let capture = install_warn_capture();
        let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
        let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
        adapters.insert(
            "mock".to_string(),
            Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
        );
        let mut state = ActorState::empty(
            crate::config::AppConfig::default(),
            crate::testutil::temp_dir("synthetic-anchor"),
            adapters,
            Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
            Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
            tx,
        );
        let (map, problems) = crate::model::scan::parse_flat_yaml("engine: mock\n");
        assert!(problems.is_empty(), "{problems:?}");
        // `from_flat` is the only producer of a `file: None` row: the synthetic shape.
        let entry = ModelEntry::from_flat(&map, "synthetic").expect("entry");
        assert_eq!(entry.file, None, "the shape under test");
        let ticket = SpawnTicket {
            generation: 1,
            kind: ResourceKind::ChildProcess,
            pid: 4343,
            port: 18_998,
        };
        state.catalog = vec![entry.clone()];
        seat_spawn(&mut state, &entry, &ticket, 18_998);
        let sampler = tokio::spawn(tokio::time::sleep(Duration::from_secs(3600)));
        state
            .slots
            .first_mut()
            .and_then(|slot| slot.occupant.as_mut())
            .expect("a seated occupant")
            .measure = Some(sampler);

        state.on_measured("synthetic", &ticket, 3.7, Some(3.7), 24.0);

        let measure = &state
            .slots
            .first()
            .and_then(|slot| slot.occupant.as_ref())
            .expect("a seated occupant")
            .measure;
        assert!(
            measure.is_none(),
            "a reading with no operator file may not leave a sampler polling for a day"
        );
        let warns = capture.lock().expect("capture lock");
        assert!(
            warns
                .iter()
                .any(|(message, _)| message.contains("no operator file")),
            "the retirement has to be visible: {warns:?}"
        );
    }

    /// FINDINGS-BATCH-2026-10-28L [L-2]: the install-time `serve_model` verdict read the
    /// LIVE catalog by name, minutes after `begin_spawn`. Under a mid-serve reload that
    /// rewrites the row's `serve_model`, the warning was computed against yaml the
    /// running process was never launched from — a false warning, or a missed one — and
    /// under same-name twins it could quote the other twin's id. The verdict now rides
    /// the spawn-era anchor. Falsifier: judge from the live row instead of the anchor
    /// and the `declared-A`/`declared-B` legs fail.
    #[test]
    fn the_serve_model_verdict_is_judged_against_the_spawn_era_row() {
        let (map, problems) =
            crate::model::scan::parse_flat_yaml("engine: mock\nserve_model: declared-A\n");
        assert!(problems.is_empty(), "{problems:?}");
        let spawn_era = ModelEntry::from_flat(&map, "served").expect("entry");
        let anchor = MeasuredAnchor::at_spawn(&spawn_era);

        let warning = serve_model_warning(Some(&anchor), Some("actual"))
            .expect("the yaml claims an id the engine does not report");
        assert!(warning.contains("declared-A"), "{warning}");

        // The reload the finding names: the same model, a different id on disk now.
        let (reloaded_map, _) =
            crate::model::scan::parse_flat_yaml("engine: mock\nserve_model: declared-B\n");
        let reloaded = ModelEntry::from_flat(&reloaded_map, "served").expect("entry");
        assert_eq!(reloaded.serve_model.as_deref(), Some("declared-B"));
        assert!(
            !warning.contains("declared-B"),
            "yaml that arrived after the launch may not rewrite what counted as a \
             mismatch: {warning}"
        );

        assert!(
            serve_model_warning(Some(&anchor), Some("declared-A")).is_none(),
            "an id the engine really reports is not a mismatch"
        );
        assert!(
            serve_model_warning(Some(&anchor), None).is_none(),
            "no discovered id is nothing to compare against"
        );
        assert!(
            serve_model_warning(None, Some("actual")).is_none(),
            "a job that never spawned holds no spawn-era claim"
        );

        // And the site may not quietly go back to describing the process from live
        // catalog bytes.
        let source = include_str!("mod.rs");
        let site = source
            .split("let serve_id = discovered_id;")
            .nth(1)
            .expect("the install verdict site is still here")
            .split("if let Some(slot) = slot")
            .next()
            .expect("the site ends where the slot work begins");
        assert!(
            !site.contains("self.catalog"),
            "the install verdict must come from the anchor, not the live catalog: {site}"
        );
    }

    /// FINDINGS2-04 [2]: `apply_catalog` ran `self.cfg.llama_server_exe.exists()` inside
    /// the single writer's turn — the last synchronous `std::fs` touch on the actor.
    /// The exe path is operator config, so that stat can park the actor behind a
    /// sleeping USB or a disconnected NAS volume with the listener already bound, and
    /// every command, `/har/status` poll and tray tick queues behind it (the A1-class
    /// stall FINDINGS-14 [1]/13 [6] moved the scans off-thread for). The verdict now
    /// travels in from the blocking leg. Falsifier: restore the stat inside
    /// `apply_catalog` and the first leg fails (an absent binary reports itself even
    /// when the scan leg said nothing), and the file-wide count leg fails.
    #[test]
    fn the_actor_applies_the_binary_verdict_it_is_handed_and_never_stats_for_one() {
        let source = include_str!("mod.rs");
        // Counted over the production half only, and with a needle built at compile
        // time so this assertion's own text cannot feed on itself.
        let production = source.split("mod tests").next().unwrap_or(source);
        let needle = concat!(".exists", "()");
        assert_eq!(
            production.match_indices(needle).count(),
            1,
            "the actor half must hold exactly one synchronous stat — the one inside \
             `missing_binary_error`, which runs on a blocking thread"
        );

        let dir = crate::testutil::temp_dir("binary-verdict");
        let absent = dir.join("llama-server.exe");
        let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
        let config = crate::config::AppConfig {
            llama_server_exe: absent.clone(),
            ..crate::config::AppConfig::default()
        };
        let mut state = ActorState::empty(
            config,
            dir.clone(),
            BTreeMap::new(),
            Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
            Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
            tx,
        );

        assert!(
            missing_binary_error(&absent).is_some(),
            "the fixture binary really is absent"
        );
        state.apply_catalog(crate::model::scan::Catalog::empty(), None);
        assert!(
            !state
                .catalog_errors
                .iter()
                .any(|error| error.kind == ScanKind::EngineBinaryMissing),
            "the actor may not answer from a stat of its own: {:?}",
            state.catalog_errors
        );

        state.apply_catalog(
            crate::model::scan::Catalog::empty(),
            missing_binary_error(&absent),
        );
        let report = state
            .catalog_errors
            .iter()
            .find(|error| error.kind == ScanKind::EngineBinaryMissing)
            .expect("the carried verdict is published");
        assert!(
            report.file == "config"
                && report.reason.contains(&absent.display().to_string())
                && report.reason.contains("llama loads will fail"),
            "the page-visible sentence keeps naming the path and the consequence: {:?}",
            report.reason
        );

        // The other polarity: a binary that is there is nothing to report.
        let present = dir.join("present.exe");
        std::fs::write(&present, b"").expect("fixture binary");
        assert!(missing_binary_error(&present).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The reconcile must not drain a triple that a newer reading already replaced
    /// (the stale-failure race: two persist tasks land out of order), and the
    /// exact-match guard covers the `Option<bool>` shape dimension itself
    /// (FINDINGS-BATCH-2026-10-28F [2]): equal `(high, low)` with a mismatched
    /// stamp shape drains nothing, in either direction. Falsifier: neutering the
    /// `entry.measured_offload == offload` comparison fails the shape legs below.
    #[test]
    fn a_stale_persist_failure_never_drains_a_superseding_triple() {
        let dir = crate::testutil::temp_dir("measured-stale");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
        let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
        adapters.insert(
            "mock".to_string(),
            Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
        );
        let mut state = ActorState::empty(
            crate::config::AppConfig::default(),
            dir.clone(),
            adapters,
            Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
            Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
            tx,
        );
        let (map, _) = crate::model::scan::parse_flat_yaml("engine: llama\nvram_gb: 2\n");
        let mut entry = ModelEntry::from_flat(&map, "fresh").expect("entry");
        entry.measured_gb = Some(6.0);
        entry.measured_min_gb = Some(4.0);
        entry.measured_offload = Some(false);
        let fresh_yaml = dir.join("fresh.yaml");
        entry.file = Some(fresh_yaml.clone());
        state.catalog = vec![entry];

        state.on_measured_persist_failed("fresh", 5.0, 5.0, Some(false), Some(fresh_yaml.clone()));
        assert_eq!(
            state.catalog[0].measured_gb,
            Some(6.0),
            "the newer (possibly persisted) triple survives a stale failure"
        );
        state.on_measured_persist_failed("fresh", 6.0, 4.0, None, Some(fresh_yaml.clone()));
        assert_eq!(
            state.catalog[0].measured_gb,
            Some(6.0),
            "a None-shaped (strata-style) failure must not drain a stamped \
             (llama-style) triple: equal pair values with the wrong stamp shape are \
             not the triple that failed"
        );
        state.on_measured_persist_failed("fresh", 6.0, 4.0, Some(false), Some(fresh_yaml.clone()));
        assert_eq!(
            state.catalog[0].measured_gb, None,
            "the matching triple drains"
        );

        let (map2, _) = crate::model::scan::parse_flat_yaml("engine: strata\nvram_gb: 2\n");
        let mut paironly = ModelEntry::from_flat(&map2, "paironly").expect("entry");
        paironly.measured_gb = Some(6.0);
        paironly.measured_min_gb = Some(4.0);
        // measured_offload stays None: the shape a non-offload engine's persist carries.
        let paironly_yaml = dir.join("paironly.yaml");
        paironly.file = Some(paironly_yaml.clone());
        state.catalog.push(paironly);
        state.on_measured_persist_failed(
            "paironly",
            6.0,
            4.0,
            Some(false),
            Some(paironly_yaml.clone()),
        );
        assert_eq!(
            state.catalog[1].measured_gb,
            Some(6.0),
            "the reverse leg: a Some-shaped (llama-style) failure must not drain a \
             pair-only (strata-style) triple"
        );
        state.on_measured_persist_failed("paironly", 6.0, 4.0, None, Some(paironly_yaml.clone()));
        assert_eq!(
            state.catalog[1].measured_gb, None,
            "the shape-matching failure drains the pair"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Poll a persisted yaml until every needle appears. The measured persist
    /// runs on the blocking pool and lands through one atomic temp+rename, so a
    /// successful write publishes the whole batch in one step.
    async fn await_yaml_keys(path: &std::path::Path, keys: &[&str]) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let text = std::fs::read_to_string(path).expect("persisted yaml readable");
            if keys.iter().all(|k| text.contains(k)) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "keys {keys:?} never reached the disk: {text}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// D of the `cpu_offload` scoping ruling: the measured PAIR mints for every
    /// engine (witness truth is engine-agnostic), but the `measured_offload` stamp
    /// is minted only for `OFFLOAD_ENGINES` owners — in memory AND on disk: both
    /// entries carry a real backing yaml, so the persist runs to completion and
    /// the bytes are read back. Falsifier (disk leg): deleting the stamp push in
    /// the persist edits makes this the only failing test — a llama yaml that
    /// silently stops carrying `measured_offload` (killing the flip-void rule's
    /// stamp source) cannot ship unnoticed; the mock leg then stays green (its
    /// contract is pair-only). Also catches: dropping the `offload_live` gate in
    /// `on_measured` (stamps the mock row) or tightening it past llama (unmints).
    #[tokio::test]
    async fn only_offload_engines_stamp_measured_offload() {
        let dir = crate::testutil::temp_dir("measured-stamp");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
        let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
        adapters.insert(
            "mock".to_string(),
            Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
        );
        let mut state = ActorState::empty(
            crate::config::AppConfig::default(),
            dir.clone(),
            adapters,
            Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
            Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
            tx,
        );
        let ticket = SpawnTicket {
            generation: 1,
            kind: ResourceKind::ChildProcess,
            pid: 4242,
            port: 18_992,
        };
        let entry = |name: &str, engine: &str| -> ModelEntry {
            let (map, _) = crate::model::scan::parse_flat_yaml(&format!(
                "engine: {engine}\ncpu_offload: true\nvram_gb: 2\n"
            ));
            ModelEntry::from_flat(&map, name).expect("entry")
        };
        let mut stampy = entry("stampy", "llama");
        // `on` is in the engine truth set: the stamp must follow it, not a
        // third reading of the token (HANDOFF §6).
        stampy
            .raw
            .insert("cpu_offload".to_string(), "on".to_string());
        let mut nostamp = entry("nostamp", "mock");
        // Real backing files: the persist runs to completion (NOT the wedged-drive
        // sibling) so the stamped triple is pinned as written bytes, not just as
        // in-memory fields.
        let stampy_path = dir.join("stampy.yaml");
        std::fs::write(&stampy_path, "engine: llama\nvram_gb: 2\ncpu_offload: on\n").expect("seed");
        let nostamp_path = dir.join("nostamp.yaml");
        std::fs::write(
            &nostamp_path,
            "engine: mock\nvram_gb: 2\ncpu_offload: true\n",
        )
        .expect("seed");
        stampy.file = Some(stampy_path.clone());
        nostamp.file = Some(nostamp_path.clone());
        state.catalog = vec![stampy.clone(), nostamp.clone()];

        seat_spawn(&mut state, &stampy, &ticket, 18_992);
        state.on_measured("stampy", &ticket, 4.2, Some(4.2), 24.0);
        assert_eq!(
            (
                state.catalog[0].measured_gb,
                state.catalog[0].measured_min_gb
            ),
            (Some(4.2), Some(4.2)),
            "the pair mints on the offload engine"
        );
        assert_eq!(
            state.catalog[0].measured_offload,
            Some(true),
            "the stamp mints on the engine that reads the key"
        );
        let written = await_yaml_keys(
            &stampy_path,
            &[
                "measured_gb: 4.2",
                "measured_gb_min: 4.2",
                "measured_offload: true",
            ],
        )
        .await;
        assert!(
            written.contains("measured_offload: true"),
            "a genuine llama yaml must gain all three stamped keys on disk — if the \
             persist stops pushing the stamp, this is the only failing test: {written}"
        );

        seat_spawn(&mut state, &nostamp, &ticket, 18_992);
        state.on_measured("nostamp", &ticket, 4.3, Some(4.3), 24.0);
        assert_eq!(
            (
                state.catalog[1].measured_gb,
                state.catalog[1].measured_min_gb
            ),
            (Some(4.3), Some(4.3)),
            "witness truth is engine-agnostic: the pair still mints on mock"
        );
        assert_eq!(
            state.catalog[1].measured_offload, None,
            "no stamp for an engine whose adapter never reads cpu_offload"
        );
        let written =
            await_yaml_keys(&nostamp_path, &["measured_gb: 4.3", "measured_gb_min: 4.3"]).await;
        assert!(
            !written.contains("measured_offload"),
            "a non-offload yaml must not gain the meaningless stamp on disk: {written}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Actor state for the fold pins: the fold consults no adapter, runtime or
    /// witness (each pin drives [`ActorState::on_measured`] with its own reading), so
    /// the never-called stand-ins keep the setup to one line per test.
    fn fold_state(dir: &std::path::Path) -> ActorState {
        let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
        let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
        adapters.insert(
            "mock".to_string(),
            Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
        );
        ActorState::empty(
            crate::config::AppConfig::default(),
            dir.to_path_buf(),
            adapters,
            Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
            Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
            tx,
        )
    }

    /// The ticket a fold pin seats its spawn under. Any generation works: the fold's
    /// `(model, ticket)` guard is satisfied by `seat_spawn` writing the same value.
    fn fold_ticket(port: u16) -> SpawnTicket {
        SpawnTicket {
            generation: 7,
            kind: ResourceKind::ChildProcess,
            pid: 4242,
            port,
        }
    }

    /// Reads one yaml of the temp dir back through the real parser, as `reload()`
    /// would: `read_measured` runs here, so the flip-void rule fires exactly as it
    /// does on a rescan — the pins cannot fake the catalog into any shape the scan
    /// would not have produced.
    fn rescan_entry(path: &std::path::Path, name: &str) -> ModelEntry {
        let text = std::fs::read_to_string(path).expect("fixture yaml readable");
        let (map, problems) = crate::model::scan::parse_flat_yaml(&text);
        assert!(problems.is_empty(), "{problems:?}");
        let mut entry = ModelEntry::from_flat(&map, name).expect("entry");
        entry.file = Some(path.to_path_buf());
        entry
    }

    /// The `cpu_offload` flip that starts this whole investigation
    /// (`research/reload-slot-drift-2026-10-28.md` §1 rows 6, 7, 9): the knob changes
    /// in the yaml while the engine still serves under the old argv. The stamp must
    /// keep describing the launch that ran — a stamp rewritten to agree with the new
    /// bytes silently disarms the flip-void rule, which compares the range with those
    /// same live bytes — and the accumulated range must survive the rescan that drops
    /// the in-memory pair. Falsifiers: computing `offload` from `entry.raw` inside the
    /// fold (the pre-fix shape) flips the stamp leg to `Some(true)`; seeding the fold
    /// from the live entry's pair instead of the spawn's drops the low end to `6.0`;
    /// each fails here and nowhere else in the suite.
    #[tokio::test]
    async fn a_mid_serve_offload_flip_stamps_the_launch_that_ran() {
        let dir = crate::testutil::temp_dir("fold-flip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let yaml = dir.join("flip.yaml");
        std::fs::write(&yaml, "engine: llama\nvram_gb: 20\ncpu_offload: false\n").expect("seed");
        let mut state = fold_state(&dir);
        let spawned = rescan_entry(&yaml, "flip");
        let ticket = fold_ticket(19_001);
        state.catalog = vec![spawned.clone()];
        seat_spawn(&mut state, &spawned, &ticket, 19_001);

        state.on_measured("flip", &ticket, 5.0, Some(5.0), 24.0);
        await_yaml_keys(&yaml, &["measured_gb: 5.0", "measured_offload: false"]).await;

        // The operator flips the knob (one line, the measured keys stay) and the
        // catalog is rescanned underneath the serving engine.
        let flipped = std::fs::read_to_string(&yaml)
            .expect("read")
            .replace("cpu_offload: false", "cpu_offload: true");
        std::fs::write(&yaml, flipped).expect("flip");
        state.catalog = vec![rescan_entry(&yaml, "flip")];
        assert_eq!(
            state.catalog[0].measured_gb, None,
            "the rescan's void rule must have fired, or the leg below proves nothing"
        );

        state.on_measured("flip", &ticket, 6.0, Some(6.0), 24.0);
        let entry = &state.catalog[0];
        assert_eq!(
            entry.measured_offload,
            Some(false),
            "the stamp describes the spawn (`-ngl 99`), not the yaml that arrived after it"
        );
        assert_eq!(
            (entry.measured_gb, entry.measured_min_gb),
            (Some(6.0), Some(5.0)),
            "the range keeps the history this model built, not a re-seed from the one live process"
        );
        let written = await_yaml_keys(
            &yaml,
            &[
                "measured_gb: 6.0",
                "measured_gb_min: 5.0",
                "measured_offload: false",
            ],
        )
        .await;
        assert!(
            written.contains("cpu_offload: true"),
            "the flip really is on disk, so the contradiction below is real: {written}"
        );
        // And the rule that was supposed to catch it still works: a stamp that cannot
        // match its knob voids the pair, instead of agreeing with it forever.
        let after = rescan_entry(&yaml, "flip");
        assert_eq!(
            (after.measured_gb, after.measured_min_gb),
            (None, None),
            "the flip-void rule is restored, not disarmed by the fold"
        );
        assert_eq!(after.measured_offload, Some(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDINGS-13 [3]'s fold half: two rows answering to one identity. The reading
    /// belongs to the file the engine was spawned from, and the reconcile of a refused
    /// persist may only drain that same row — never the twin whose bytes the refused
    /// write never touched. The catalog order puts the twin FIRST so any name-keyed
    /// target (the pre-fix `find(|m| m.name == model)`, in the fold and in the drain)
    /// picks the wrong file and both legs below fail.
    #[tokio::test]
    async fn a_twin_name_folds_only_into_the_spawn_era_file() {
        let dir = crate::testutil::temp_dir("fold-twin");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mine = dir.join("twin.yaml");
        let twin = dir.join("twin .yaml");
        std::fs::write(&mine, "engine: llama\nvram_gb: 20\n").expect("seed mine");
        std::fs::write(&twin, "engine: llama\nvram_gb: 20\n").expect("seed twin");
        let mut state = fold_state(&dir);
        let spawned = rescan_entry(&mine, "twin");
        let ticket = fold_ticket(19_002);
        // Catalog order deliberately the other way round: the twin owns the first
        // slot, so a name lookup answers with the row this engine never came from.
        state.catalog = vec![rescan_entry(&twin, "twin"), spawned.clone()];
        seat_spawn(&mut state, &spawned, &ticket, 19_002);

        state.on_measured("twin", &ticket, 4.4, Some(4.4), 24.0);
        let folded = await_yaml_keys(&mine, &["measured_gb: 4.4"]).await;
        assert!(
            folded.contains("measured_gb_min: 4.4"),
            "the spawn-era file gains the pair: {folded}"
        );
        assert_eq!(
            (
                state.catalog[0].measured_gb,
                state.catalog[0].measured_min_gb,
                state.catalog[0].measured_offload
            ),
            (None, None, None),
            "the twin row books nothing for a launch it did not describe"
        );
        let untouched = std::fs::read_to_string(&twin).expect("read twin");
        assert!(
            !untouched.contains("measured_gb"),
            "the twin's bytes are not the resident's measurement sheet: {untouched}"
        );

        // The refused-persist reconcile is held to the same target: equal values on
        // both rows may not make the twin drainable.
        for index in 0..2 {
            state.catalog[index].measured_gb = Some(4.4);
            state.catalog[index].measured_min_gb = Some(4.4);
            state.catalog[index].measured_offload = Some(false);
        }
        state.on_measured_persist_failed("twin", 4.4, 4.4, Some(false), Some(mine.clone()));
        assert_eq!(
            state.catalog[1].measured_gb, None,
            "the row the refused write targeted drains"
        );
        assert_eq!(
            state.catalog[0].measured_gb,
            Some(4.4),
            "the same-name twin keeps the values the refused write never touched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The row that owned a serving engine vanishes mid-serve (renamed away, turned
    /// into a problem line, given an engine with no adapter). Nothing may be minted or
    /// extended without it, and the sampler that produced the reading is retired —
    /// left alone it walks the witness every `measured_sample_s` for a day with
    /// nowhere to put the answer, and says nothing. Falsifiers: dropping the detach
    /// `return` (the fold would carry on against whatever row the name still reaches)
    /// leaves the sampler armed and the reading booked; dropping the `anchor.file`
    /// guard writes a triple to no file at all and clears nothing.
    #[tokio::test]
    async fn a_vanished_row_fails_the_fold_closed_and_retires_the_sampler() {
        let capture = install_warn_capture();
        let dir = crate::testutil::temp_dir("fold-vanished");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let yaml = dir.join("gone.yaml");
        std::fs::write(&yaml, "engine: llama\nvram_gb: 20\n").expect("seed");
        let mut state = fold_state(&dir);
        let spawned = rescan_entry(&yaml, "gone");
        let ticket = fold_ticket(19_003);
        state.catalog = vec![spawned.clone()];
        seat_spawn(&mut state, &spawned, &ticket, 19_003);
        let sampler = tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
        state.slots[0]
            .occupant
            .as_mut()
            .expect("seated occupant")
            .measure = Some(sampler);

        // The operator turns the file into a problem line; Fork C never publishes it,
        // so the rescan leaves the serving engine with no row at all.
        std::fs::write(&yaml, "engine: llama\nbad key: x\n").expect("break the row");
        state.catalog = Vec::new();

        state.on_measured("gone", &ticket, 9.0, Some(9.0), 24.0);
        assert!(
            state.catalog.is_empty(),
            "no row may be booked, created or extended for a vanished launch"
        );
        assert!(
            state.slots[0]
                .occupant
                .as_ref()
                .and_then(|o| o.measure.as_ref())
                .is_none(),
            "the orphaned sampler is retired, not left polling the card"
        );
        let text = std::fs::read_to_string(&yaml).expect("read back");
        assert!(
            !text.contains("measured_gb"),
            "nothing is persisted for the reading that had no row: {text}"
        );
        let warns = capture.lock().expect("capture lock");
        assert!(
            warns.iter().any(|(message, fields)| {
                message.contains("measured reading has no catalog row")
                    && fields.get("model").is_some_and(|m| m.contains("gone"))
                    && fields.get("file").is_some_and(|f| f.contains("gone.yaml"))
            }),
            "the silence is the bug this leg pins: {warns:?}"
        );

        // Second leg: a spawn with no anchor at all (no `begin_spawn` ever ran for it)
        // folds nothing either — absence, never a number with no provenance.
        let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
        let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
        adapters.insert(
            "mock".to_string(),
            Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
        );
        let mut bare = ActorState::empty(
            crate::config::AppConfig::default(),
            dir.clone(),
            adapters,
            Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
            Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
            tx,
        );
        bare.catalog = vec![spawned];
        let unanchored = SpawnTicket {
            generation: 8,
            kind: ResourceKind::ChildProcess,
            pid: 4243,
            port: 19_004,
        };
        bare.slots = vec![Slot {
            category: CategoryId::Work,
            index: 0,
            engine_port: 19_004,
            occupant: Some(Occupant {
                model: "gone".to_string(),
                need: Gibibytes(2.0),
                port: 19_004,
                ticket: unanchored.clone(),
                loaded_at: Instant::now(),
                launched_at_wall: std::time::SystemTime::now(),
                spawn_config: None,
                stale_config: false,
                serve_id: None,
                measure: None,
            }),
            job: None,
        }];
        bare.on_measured("gone", &unanchored, 9.0, Some(9.0), 24.0);
        assert_eq!(
            bare.catalog[0].measured_gb, None,
            "an unanchored spawn has no stamp to mint and no file to write"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Axis 1 predicate table (ruling 2026-10-29), including the
    /// unreadable-stat windows the actor path cannot race into existence
    /// deterministically (the scan that publishes the bytes rides the same read
    /// the stat does), so the `None` legs are manufactured here directly: a
    /// missing fingerprint — at registration or at rescan — is the absence of
    /// evidence, never evidence of change. Falsifiers: treating `None` as change
    /// fails every KEEP leg; disabling the invalidation fails both RETIRE legs;
    /// an always-retire predicate fails KEEP 1 (whose actor-path twin is
    /// `an_unchanged_rescan_keeps_the_queue_and_the_parked_load_still_loads`).
    #[test]
    fn a_rescan_keeps_a_queue_whose_fingerprint_cannot_show_a_change() {
        let dir = crate::testutil::temp_dir("rescan-fp");
        let yaml = dir.join("m.yaml");
        let entry_on_disk = |bytes: &str| -> ModelEntry {
            std::fs::write(&yaml, bytes).expect("seed");
            let (map, _) = crate::model::scan::parse_flat_yaml("engine: mock\nvram_gb: 4\n");
            let mut entry = ModelEntry::from_flat(&map, "m").expect("entry");
            entry.file = Some(yaml.clone());
            entry
        };
        let fresh_state = || -> ActorState {
            let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
            let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
            adapters.insert(
                "mock".to_string(),
                Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
            );
            ActorState::empty(
                crate::config::AppConfig::default(),
                dir.clone(),
                adapters,
                Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
                Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
                tx,
            )
        };
        let queue_on = |state: &mut ActorState, entry: &ModelEntry| -> JobId {
            state.slots = vec![Slot {
                category: CategoryId::Work,
                index: 0,
                engine_port: 18_100,
                occupant: None,
                // The parked row does not own the slot: some other job is in
                // flight on it, exactly as parking requires.
                job: Some(JobId(9_999)),
            }];
            let id = state.register_queued_load(0, entry, Vec::new());
            state.pending.insert(0, id);
            id
        };
        let apply = |state: &mut ActorState, models: Vec<ModelEntry>| {
            state.apply_catalog(
                crate::model::scan::Catalog {
                    models,
                    errors: Vec::new(),
                },
                None,
            );
        };
        let queued_alive = |state: &ActorState, id: JobId| {
            state.jobs.get(&id).is_some_and(|j| !j.terminal)
                && state.pending.values().any(|p| *p == id)
        };

        // KEEP 1: same bytes, same clock — the no-change rescan must not drop
        // the queue (budget kept intact, place kept, row non-terminal).
        let entry = entry_on_disk("vram_gb: 4\n");
        let mut state = fresh_state();
        let id = queue_on(&mut state, &entry);
        apply(&mut state, vec![entry.clone()]);
        assert!(
            queued_alive(&state, id),
            "a no-change rescan must never drop a queue"
        );

        // KEEP 2: the file became unstat'able by apply time (bytes published
        // from the earlier read, the stat window lost the file) — a missing
        // rescan-side fingerprint is not evidence.
        let entry = entry_on_disk("vram_gb: 4\n");
        let mut state = fresh_state();
        let id = queue_on(&mut state, &entry);
        std::fs::remove_file(&yaml).expect("delete after the scan read");
        apply(&mut state, vec![entry.clone()]);
        assert!(
            queued_alive(&state, id),
            "an unreadable rescan-time stat must not retire"
        );

        // KEEP 3: the row queued on an unreadable file (no registration
        // baseline at all) — even readable-and-changed bytes now prove nothing.
        let entry = entry_on_disk("vram_gb: 4\n");
        let mut state = fresh_state();
        let id = queue_on(&mut state, &entry);
        state.jobs.get_mut(&id).expect("row").queue_fingerprint = None;
        std::fs::write(&yaml, "vram_gb: 400\n").expect("edit");
        apply(&mut state, vec![entry.clone()]);
        assert!(
            queued_alive(&state, id),
            "a row without a registration baseline survives the rescan"
        );

        // RETIRE: bytes moved against a real baseline — the named reason is on
        // the row and the queue place is released.
        let entry = entry_on_disk("vram_gb: 4\n");
        let mut state = fresh_state();
        let id = queue_on(&mut state, &entry);
        std::fs::write(&yaml, "vram_gb: 400\n").expect("edit");
        apply(&mut state, vec![entry]);
        assert!(
            state.jobs[&id].terminal,
            "moved bytes must retire the intent"
        );
        assert!(state.pending.is_empty(), "and release the queue place");
        let error = state.jobs[&id]
            .error
            .clone()
            .expect("the reason rides the ledger row")
            .to_string();
        assert!(
            error.contains("superseded by rescan")
                && error.contains("m.yaml")
                && error.contains("changed since queued")
                && error.contains("re-queue to apply"),
            "{error}"
        );

        // RETIRE: the model left the catalog.
        let entry = entry_on_disk("vram_gb: 4\n");
        let mut state = fresh_state();
        let id = queue_on(&mut state, &entry);
        apply(&mut state, Vec::new());
        assert!(
            state.jobs[&id].terminal,
            "a vanished model retires its intent"
        );
        assert!(
            state.jobs[&id].error.as_ref().is_some_and(|e| e
                .to_string()
                .contains("superseded by rescan (removed from the catalog)")),
            "{:?}",
            state.jobs[&id].error
        );
    }

    /// Shared fixture for the Axis-2 resident pins: one `ActorState` with one
    /// resident whose `spawn_config` is a real capture of a real yaml (the state
    /// `install` produces — same fields, taken from the same helper), the file's
    /// fingerprint, and the entry pair the fold would have written.
    fn resident_on_a_file() -> (
        ActorState,
        std::path::PathBuf,
        crate::model::FileFingerprint,
    ) {
        let dir = crate::testutil::temp_dir("stale-resident");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let yaml = dir.join("m.yaml");
        std::fs::write(
            &yaml,
            "engine: mock\ncost: fixed\nmeasured_gb: 10.0\nmeasured_gb_min: 8.0\n",
        )
        .expect("seed");
        let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
        let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
        adapters.insert(
            "mock".to_string(),
            Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
        );
        let mut state = ActorState::empty(
            crate::config::AppConfig::default(),
            dir.clone(),
            adapters,
            Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
            Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
            tx,
        );
        let capture = crate::model::fingerprint(&yaml).expect("seeded file fingerprints");
        let ticket = SpawnTicket {
            generation: 1,
            kind: ResourceKind::ChildProcess,
            pid: 4242,
            port: 18_431,
        };
        state.catalog = vec![entry_from(&yaml, "m")];
        state.slots = vec![Slot {
            category: CategoryId::Work,
            index: 0,
            engine_port: 18_431,
            occupant: Some(Occupant {
                model: "m".to_string(),
                need: Gibibytes(4.0),
                port: 18_431,
                ticket,
                loaded_at: Instant::now(),
                launched_at_wall: std::time::SystemTime::now(),
                spawn_config: Some((yaml.clone(), capture)),
                stale_config: false,
                serve_id: None,
                measure: None,
            }),
            job: None,
        }];
        (state, yaml, capture)
    }

    fn entry_from(path: &std::path::Path, name: &str) -> ModelEntry {
        let text = std::fs::read_to_string(path).expect("readable");
        let (map, _) = crate::model::scan::parse_flat_yaml(&text);
        let mut entry = ModelEntry::from_flat(&map, name).expect("entry");
        entry.file = Some(path.to_path_buf());
        entry
    }

    /// The reload-turn rescan the pins drive: re-parse the file the operator (or the
    /// harness) just wrote and land it through `apply_catalog` — the only judge
    /// between install and the next launch.
    fn rescan(state: &mut ActorState, yaml: &std::path::Path, name: &str) {
        state.apply_catalog(
            crate::model::scan::Catalog {
                models: vec![entry_from(yaml, name)],
                errors: Vec::new(),
            },
            None,
        );
    }

    /// Axis 2 = B (ruling 2026-10-29), reload leg: an edit only ambers when a rescan
    /// lands — between the edit and the rescan the status path must not notice
    /// (one-stat-per-event: the snapshot never stats). Falsifier: removing
    /// `refresh_stale_markers` from `apply_catalog` fails the amber assert; moving
    /// the verdict computation into the snapshot (or any status read) fails the
    /// pre-rescan assert.
    #[test]
    fn a_resident_ambers_on_the_reload_that_finds_newer_bytes_than_its_launch() {
        let (mut state, yaml, _) = resident_on_a_file();
        let stamp = state.snapshot().slots[0]
            .launched_at
            .clone()
            .expect("a resident carries its wall-clock launch");
        assert_eq!(state.snapshot().slots[0].stale_config, Some(false));
        let original = std::fs::read_to_string(&yaml).expect("read");
        std::fs::write(&yaml, format!("{original}# operator note\n")).expect("operator edit");
        assert_eq!(
            state.snapshot().slots[0].stale_config,
            Some(false),
            "an edit without a rescan must not move the verdict from a status read: \
             the snapshot never stats"
        );
        rescan(&mut state, &yaml, "m");
        let after = state.snapshot();
        assert_eq!(
            after.slots[0].stale_config,
            Some(true),
            "newer bytes than the spawn capture must amber on the reload turn"
        );
        assert_eq!(
            after.slots[0].launched_at.as_deref(),
            Some(stamp.as_str()),
            "the launch stamp rides the install, not the rescan"
        );
    }

    /// Interlock (4), stated plainly: the comparison rides the SPAWN capture
    /// (mtime + size), so re-saving byte-identical content does NOT clear the
    /// verdict — the clock moved. The apply gesture is unload + reload, and a
    /// respawned engine re-captures cleanly. Falsifier: a verdict that compares
    /// content, or that clears on size-equality, takes the final assert from true
    /// to false here (the size/mtime probes prove the leg is real either way).
    #[test]
    fn restoring_identical_bytes_does_not_clear_the_stale_verdict() {
        let (mut state, yaml, capture) = resident_on_a_file();
        let original = std::fs::read(&yaml).expect("read");
        std::fs::write(
            &yaml,
            "engine: mock\ncost: fixed\nmeasured_gb: 444.0\nmeasured_gb_min: 8.0\n",
        )
        .expect("diverge (size moves: no dependence on the clock tick)");
        rescan(&mut state, &yaml, "m");
        assert_eq!(state.snapshot().slots[0].stale_config, Some(true));
        std::fs::write(&yaml, &original).expect("restore the identical bytes");
        // A same-tick rewrite can leave the recorded mtime unmoved (Windows clock
        // granularity), so force the clock forward: the restore is byte-identical,
        // the timestamp factually newer — the state a re-save really produces.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&yaml)
            .expect("open for the clock bump");
        file.set_times(
            std::fs::FileTimes::new().set_modified(
                capture
                    .mtime
                    .checked_add(Duration::from_millis(60_000))
                    .expect("mtime in range"),
            ),
        )
        .expect("force the mtime forward");
        drop(file);
        let now = crate::model::fingerprint(&yaml).expect("stat");
        assert_eq!(
            now.size, capture.size,
            "byte-identical restore: the SIZE cannot tell it apart"
        );
        assert_ne!(
            now.mtime, capture.mtime,
            "and the clock moved (pre {:?} vs post {:?}) — this is the leg size-only would miss",
            capture.mtime, now.mtime
        );
        rescan(&mut state, &yaml, "m");
        assert_eq!(
            state.snapshot().slots[0].stale_config,
            Some(true),
            "byte-identical content restored later is STILL stale: honest conservatism, \
             the operator applies with unload + reload"
        );
    }

    /// Interlock (1): stale is INFO, not policy. An amber resident keeps its
    /// measured pair, its cost basis and its slot exactly as a clean one does —
    /// the marker never touches what admission books or who holds the card.
    /// Falsifier: gating `cost_source`/measured validity (or the slot) on
    /// `stale_config` fails this while the verdict pins stay green.
    #[test]
    fn a_stale_resident_keeps_serving_its_measured_range() {
        let (mut state, yaml, _) = resident_on_a_file();
        std::fs::write(
            &yaml,
            "engine: mock\ncost: fixed\nmeasured_gb: 10.0\nmeasured_gb_min: 8.0\n# operator note\n",
        )
        .expect("operator edit that keeps the measured keys intact");
        rescan(&mut state, &yaml, "m");
        let after = state.snapshot();
        assert_eq!(after.slots[0].stale_config, Some(true), "amber first");
        let entry = state
            .catalog
            .iter()
            .find(|m| m.name == "m")
            .expect("resident's row");
        assert_eq!(
            (entry.measured_gb, entry.measured_min_gb),
            (Some(10.0), Some(8.0)),
            "the measured pair is witness truth: an amber marker does not void it"
        );
        assert_eq!(
            entry.cost_source(),
            crate::model::CostSource::Measured,
            "and admission still books from it — stale never demotes the cost basis"
        );
        let occupant = after.slots[0].clone();
        assert_eq!(occupant.model.as_deref(), Some("m"));
        assert_eq!(occupant.port, Some(18_431), "the resident keeps its slot");
    }

    /// Interlock (5), actor side: a harness-owned write (the measured fold, the API
    /// flag edit) re-baselines the spawn capture — never through the verdict, never
    /// over a true one. Falsifiers: no-op-ing `on_model_file_written` makes the
    /// unchanged rescan after the write amber (L1 fails); re-baselining regardless
    /// of the verdict erases the operator's earned amber (L2's final assert fails).
    #[test]
    fn the_harness_own_write_rebaselines_a_clean_resident_captured_spawn_side() {
        // L1: clean resident + harness write + carried fingerprint -> the verdict
        // stays false AND survives the next unchanged rescan (the capture moved).
        let (mut state, yaml, _) = resident_on_a_file();
        std::fs::write(
            &yaml,
            "engine: mock\ncost: fixed\nmeasured_gb: 12.0\nmeasured_gb_min: 8.0\n",
        )
        .expect("fold write");
        let written = crate::model::fingerprint(&yaml).expect("stat");
        state.on_model_file_written("m", &yaml, written);
        rescan(&mut state, &yaml, "m");
        assert_eq!(
            state.snapshot().slots[0].stale_config,
            Some(false),
            "the harness's own write never ambers its own model"
        );

        // L2: an already-true verdict is never erased: neither by the message nor
        // by the unchanged rescan that follows it (which would go false had the
        // capture been moved under an earned amber).
        std::fs::write(
            &yaml,
            "engine: mock\ncost: fixed\nmeasured_gb: 120.0\nmeasured_gb_min: 8.0\n",
        )
        .expect("operator edit (size moves)");
        rescan(&mut state, &yaml, "m");
        assert_eq!(state.snapshot().slots[0].stale_config, Some(true));
        std::fs::write(
            &yaml,
            "engine: mock\ncost: fixed\nmeasured_gb: 120.0\nmeasured_gb_min: 8.0\n",
        )
        .expect("fold write over an amber resident");
        let written = crate::model::fingerprint(&yaml).expect("stat");
        state.on_model_file_written("m", &yaml, written);
        assert_eq!(
            state.snapshot().slots[0].stale_config,
            Some(true),
            "a harness write must not clear a verdict the operator's edit earned"
        );
        rescan(&mut state, &yaml, "m");
        assert_eq!(
            state.snapshot().slots[0].stale_config,
            Some(true),
            "and must not re-baseline the capture under it: the unchanged rescan would \
             go false if it had"
        );

        // L3: a resident with no capture stays unaccused through the same message.
        if let Some(occupant) = state.slots[0].occupant.as_mut() {
            occupant.spawn_config = None;
            occupant.stale_config = false;
        }
        state.on_model_file_written("m", &yaml, written);
        rescan(&mut state, &yaml, "m");
        assert_eq!(
            state.snapshot().slots[0].stale_config,
            Some(false),
            "absence of a capture is absence of evidence, never an accusation"
        );
    }

    /// The hard invariant's STRUCTURAL tier (established `include_str!` seam, same
    /// shape as the api's `swap_for_port` tripwire): the `snapshot` body — the one
    /// status-path reader of the marker — may never reach for the filesystem or the
    /// clock. The ban-list convicts SYNONYMS, not just the lazy spelling: bare
    /// `fs::` catches `std::fs::` and `tokio::fs::` alike, `metadata(` catches
    /// `std::fs::metadata`, `Path::metadata()` and the `MetadataExt` forms, and
    /// `SystemTime::now` is banned because the stamp is stored at install
    /// (`launched_at_wall`) for exactly that reason.
    ///
    /// The two-layer contract, stated honestly: what the NET cannot see is a stat
    /// whose result is DISCARDED — inert by definition, it cannot move a verdict —
    /// and a stat parked behind a helper name outside the scan. What it convicts is
    /// every spelling of a reaching fs/clock call inside the read path's own bodies.
    /// The BEHAVIOURAL backstop (the hidden-file probe below, and the api-side
    /// `/har/models` leg) catches anything whose result reaches the published
    /// verdict regardless of spelling: a stat feeding the verdict is convicted by
    /// BOTH tiers; a stat whose result is dropped is harmless to the verdict by
    /// construction, and the net still keeps the synonym out of the hot path.
    /// The tail sentinel asserts the slice actually covers the body's end (a
    /// future early column-4 `}` cannot silently shrink what is scanned).
    ///
    /// Falsifier: move the verdict computation into `snapshot` (any spelling) and
    /// the net plus both probes fire; inject an inert `Path::new(f).metadata()` and
    /// the net fires (FINDINGS-BATCH-2026-10-28P [P-1]'s M-E).
    #[test]
    fn the_snapshot_reads_the_cached_verdict_and_never_stats() {
        let source = include_str!("mod.rs");
        let start = source
            .find("    fn snapshot(&self) -> SchedulerStatus {")
            .expect("snapshot is still in this file");
        let body = &source[start
            ..start
                + source[start..]
                    .find("\n    }\n")
                    .expect("the function closes")];
        assert!(
            body.contains("loaded_at:"),
            "the scan slice must reach the body's tail (early-close fragility)"
        );
        for banned in [
            "fs::",
            "fingerprint(",
            "read_to_string(",
            "metadata(",
            "modified(",
            "try_exists",
            "File::",
            "FileTimes",
            "OpenOptions",
            "SystemTime::now",
            "read_dir",
        ] {
            assert!(
                !body.contains(banned),
                "the snapshot may only read cached state; found `{banned}` in its body"
            );
        }
        let (state, yaml, _) = resident_on_a_file();
        let before = state.snapshot();
        assert_eq!(before.slots[0].stale_config, Some(false));
        std::fs::remove_file(&yaml).expect("hide the file");
        let after = state.snapshot();
        assert_eq!(
            before.slots[0].stale_config, after.slots[0].stale_config,
            "hiding the file from under the status path changes nothing: the verdict \
             is a cache, not a stat (a statting snapshot would flip to Some(true))"
        );
        assert_eq!(before.slots[0].launched_at, after.slots[0].launched_at);
    }

    /// The production half of a source file: everything before its test module (if
    /// no `mod tests` boundary exists, the whole file counts as production — the
    /// safe direction).
    fn production_half(source: &str) -> &str {
        let cut = [
            "#[cfg(test)]\nmod tests {",
            "#[cfg(test)]\npub(crate) mod tests {",
        ]
        .iter()
        .filter_map(|marker| source.find(marker))
        .min()
        .unwrap_or(source.len());
        &source[..cut]
    }

    /// P-4 (FINDINGS-BATCH-2026-10-28P): the re-baseline table documented at
    /// [`ActorState::on_model_file_written`] claims a CLOSED set of legitimate model-
    /// yaml mutators — the measured persist (this file, its note riding the success
    /// path) and `har_config_model`'s flag edit (api, its note sent before the
    /// handler's reload). "True today" was grep-only; this pin makes the claim
    /// structural, the repo's established `include_str!` style: every scanned source
    /// keeps exactly its allowed count of writeback call sites in its production
    /// half, and each mutator file must also carry its note. A third caller must
    /// register the write in the re-baseline table (send `ModelFileWritten` /
    /// `note_model_file_written` with the write) or the amber blames the harness for
    /// its own write — the exact lie interlock 5 forbids.
    /// Falsifier: inject `let _ = crate::model::writeback::set_flags_on_file(
    /// std::path::Path::new("x"), &[]);` into any scanned production body and this
    /// is the sole failing test; add the call to api WITHOUT the note and the note
    /// leg fires too.
    #[test]
    fn the_writeback_mutator_set_stays_closed_to_the_rebaseline_table() {
        for (file, source, allowed) in [
            ("scheduler/mod.rs", include_str!("mod.rs"), 1usize),
            ("api/mod.rs", include_str!("../api/mod.rs"), 1),
            ("main.rs", include_str!("../main.rs"), 0),
            ("catalog.rs", include_str!("../catalog.rs"), 0),
            ("tray.rs", include_str!("../tray.rs"), 0),
            ("model/mod.rs", include_str!("../model/mod.rs"), 0),
            ("engine/mod.rs", include_str!("../engine/mod.rs"), 0),
        ] {
            let production = production_half(source);
            let calls = production.matches("set_flags_on_file(").count()
                + production.matches("set_flag_on_file(").count();
            assert_eq!(
                calls, allowed,
                "{file} carries {calls} writeback mutator call sites; the closed table \
                 allows {allowed} (the measured persist here, har_config_model's flag edit \
                 in api). A new writer must register in the re-baseline table — send the \
                 ModelFileWritten note with the write (see ActorState::on_model_file_written) \
                 — or the stale marker lies about the harness's own write: \
                 FINDINGS-BATCH-2026-10-28P [P-4]."
            );
            if allowed == 1 {
                let notes = if file.starts_with("api") {
                    production.contains("note_model_file_written(")
                } else {
                    production.contains("ActorMsg::ModelFileWritten")
                };
                assert!(
                    notes,
                    "{file} mutates a model yaml but never sends the re-baseline note: \
                     the write would self-amber its resident"
                );
            }
        }
    }

    #[test]
    fn a_stale_reap_target_does_not_abort_the_rest() {
        // Chunk 2: aborting the reap on a stale victim ticket stranded the job's own live
        // engine — the very orphan the multi-target reap exists to prevent.
        use crate::engine::SpawnError;
        assert!(
            ActorState::reap_should_continue(&SpawnError::StaleTicket {
                port: 18_109,
                expected: 3,
                found: 4,
            }),
            "a vanished victim leaves nothing to reap; the next target must still be tried"
        );
        assert!(
            !ActorState::reap_should_continue(&SpawnError::KillFailed {
                holder: "model".to_string(),
                reason: "TerminateProcess failed".to_string(),
            }),
            "a real kill failure must still fail the job"
        );
    }

    fn job(id: u64, kind: JobKind, model: &str, error: Option<&str>, outcome: &str) -> JobStatus {
        JobStatus {
            id: JobId(id),
            kind,
            model: model.to_string(),
            category: CategoryId::Work,
            state: SwapState::Serving,
            terminal: true,
            outcome: Some(outcome.to_string()),
            error: error.map(str::to_string),
            // Recomputed here independently of `fault_of`, so a broken rule cannot
            // quietly make its own fixture consistent.
            is_fault: kind == JobKind::Load && error.is_some() && outcome != "cancelled",
            need_gib: None,
            free_at_admit_gib: None,
            observed_at_admit_gib: None,
            gate_target_gib: None,
            victims_released: Vec::new(),
            warnings: Vec::new(),
            timings: Default::default(),
        }
    }

    #[test]
    fn only_a_failed_load_is_a_fault() {
        // The row dot and the headline share one rule: a supersede or
        // unload-during-load must not pin a red row (duplication review, 2026-10-26).
        let cancelled = job(
            7,
            JobKind::Load,
            "m",
            Some("cancelled by unload"),
            "cancelled",
        );
        assert!(!cancelled.is_fault, "a cancel is policy, not a fault");
        assert!(headline_fault(&[cancelled]).is_none());
        let failed = job(8, JobKind::Load, "m", Some("settle timeout"), "failed");
        assert!(failed.is_fault);
        assert!(headline_fault(&[failed]).is_some());
        let unload = job(9, JobKind::Unload, "m", Some("spawn failed"), "failed");
        assert!(
            !unload.is_fault,
            "unload failures live in the jobs strip only"
        );
    }

    fn failed(id: u64, model: &str) -> JobStatus {
        job(
            id,
            JobKind::Load,
            model,
            Some("engine not ready"),
            "engine not ready",
        )
    }

    fn ok(id: u64, model: &str) -> JobStatus {
        job(id, JobKind::Load, model, None, "loaded")
    }

    #[test]
    fn newest_failure_wins() {
        let jobs = vec![failed(1, "a"), failed(2, "b")];
        assert_eq!(headline_fault(&jobs).expect("fault").id, JobId(2));
    }

    #[test]
    fn cancels_and_unloads_are_not_faults() {
        // Review findings 1/2: an operator cancel (T8 unload-during-load, supersede)
        // and a failed unload never take the headline away from a real load failure.
        let jobs = vec![
            failed(1, "a"),
            job(
                2,
                JobKind::Load,
                "b",
                Some("cancelled: superseded"),
                "cancelled",
            ),
            job(3, JobKind::Unload, "c", Some("kill refused"), "unloaded"),
        ];
        assert_eq!(headline_fault(&jobs).expect("real fault").id, JobId(1));
    }

    #[test]
    fn only_a_later_same_model_success_clears() {
        assert!(headline_fault(&[failed(1, "a")]).is_some());
        assert!(
            headline_fault(&[failed(1, "a"), ok(2, "b")]).is_some(),
            "a different model's success does not mask the fault"
        );
        assert!(
            headline_fault(&[
                failed(1, "a"),
                job(2, JobKind::Unload, "a", None, "unloaded")
            ])
            .is_some(),
            "an unload never clears"
        );
        assert!(
            headline_fault(&[failed(1, "a"), ok(2, "a")]).is_none(),
            "the same model+category loading successfully clears"
        );
        assert_eq!(
            headline_fault(&[failed(1, "a"), ok(2, "a"), failed(3, "a")])
                .expect("re-faulted")
                .id,
            JobId(3)
        );
    }
}
