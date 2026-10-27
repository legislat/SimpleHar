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
use crate::engine::{EngineAdapter, EngineRuntime, Probe, ReadinessSpec, SpawnError, Spawned};
use crate::model::ModelEntry;
use crate::ownership::{ResourceKind, SpawnTicket};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
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
    /// Spawns the actor task and returns its handle. The catalog is scanned from
    /// `models_dir` at construction (§0.2 step 6) and re-scanned by [`Command::Reload`].
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
    /// The engine's self-reported model id (discovered at readiness, F-4 fix);
    /// proxy rewriting targets this, falling back to the harness name.
    serve_id: Option<String>,
    /// Serving-window witness sampler folding this occupant's total-diff into the
    /// model's measured range; aborted the moment the slot empties (§0.4).
    measure: Option<tokio::task::JoinHandle<()>>,
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
}

/// A load parked until its slot returns to `Idle` (supersede, T3.4).
struct PendingSwap {
    model: String,
    waiters: Vec<Waiter>,
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
    pending: HashMap<usize, PendingSwap>,
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

async fn actor_loop(mut state: ActorState, mut rx: mpsc::UnboundedReceiver<ActorMsg>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            ActorMsg::Command { cmd, reply } => state.on_command(cmd, reply),
            ActorMsg::Touch { model } => state.touch_model(&model),
            ActorMsg::Measured {
                model,
                ticket,
                total_diff,
                own,
                total_gib,
            } => state.on_measured(&model, &ticket, total_diff, own, total_gib),
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
    fn new(
        cfg: AppConfig,
        models_dir: PathBuf,
        adapters: BTreeMap<String, Arc<dyn EngineAdapter>>,
        runtime: Arc<dyn EngineRuntime>,
        witness: Arc<dyn crate::budget::VramWitness>,
        tx: mpsc::UnboundedSender<ActorMsg>,
    ) -> Self {
        let mut state = Self::empty(cfg, models_dir, adapters, runtime, witness, tx);
        state.refresh_catalog();
        state
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
            next_job: 0,
            next_gen: 0,
        }
    }

    /// Rescans `ModelConf` and cross-checks `default_model`s (§0.2 step 6, Step 9).
    fn refresh_catalog(&mut self) {
        let catalog = crate::model::scan::scan_dir(&self.models_dir);
        let mut errors = catalog.errors;
        for category in &self.cfg.categories {
            if let Some(default) = &category.default_model {
                if !catalog.models.iter().any(|m| &m.name == default) {
                    errors.push(crate::model::scan::ScanError {
                        file: "config".to_string(),
                        reason: format!(
                            "default_model `{default}` for {} is not in the catalog",
                            category.id
                        ),
                    });
                }
            }
        }
        tracing::info!(
            models = catalog.models.len(),
            errors = errors.len(),
            "catalog scanned"
        );
        self.catalog = catalog.models;
        self.catalog_errors = errors;
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
                self.refresh_catalog();
                self.publish();
                let _ = reply.send(Ok(Reply::Done));
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
                if let Some(old) = self.pending.insert(
                    target,
                    PendingSwap {
                        model: entry.name.clone(),
                        waiters: vec![reply],
                    },
                ) {
                    for waiter in old.waiters {
                        let _ = waiter.send(Err(SchedulerError::Cancelled(
                            "superseded by a newer load before it started".to_string(),
                        )));
                    }
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

    /// T12.1: unload one model's slot; a loading swap for it is cancelled first.
    /// A loading slot has no occupant yet — the in-flight job's model is the
    /// only way to find it (T8: unload during load = cancel, cleanup completes).
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
        self.unload_slots(targets, reply);
    }

    /// T12.4: unload every occupied slot; loading jobs are cancelled.
    fn on_unload_all(&mut self, reply: Waiter) {
        let targets: Vec<usize> = (0..self.slots.len())
            .filter(|&i| self.slots[i].occupant.is_some() || self.slots[i].job.is_some())
            .collect();
        self.unload_slots(targets, reply);
    }

    /// Shared unload path: free slots start an unload job; slots with an
    /// in-flight job get that job cancelled (T8: unload during load = cancel,
    /// and the cancellation still runs kill + release confirmation).
    fn unload_slots(&mut self, slots: Vec<usize>, reply: Waiter) {
        for slot in slots {
            match self.slots[slot].job {
                Some(job_id) => self.cancel_job(job_id, "unloaded by request"),
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
            victim: Some(victim),
            victims: Vec::new(),
            victims_released: Vec::new(),
            released_cost: 0.0,
            other_need_at_admit: 0.0,
            note: Some(note.to_string()),
            warnings: Vec::new(),
            slot: Some(slot),
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

    /// Folds one serving-window reading into the model's measured range and writes it
    /// back to the yaml (§0.4): `measured_gb` keeps the high end — what `fixed`
    /// admission consults — and `measured_gb_min` the low end the page shows, with the
    /// `cpu_offload` setting they were taken under so a later flip voids them at scan.
    /// Four guards keep the number honest: the sampled engine must still be the one in
    /// the slot; no other job may be in flight; a tenant that appeared after admit is
    /// subtracted, or the driver's own per-process attribution is used when the card
    /// reports it; and a figure outside the card is refused, not persisted
    /// (measurement verifies, it never guesses).
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
        let at_admit = self
            .jobs
            .values()
            .find(|job| job.model == model && job.spawned.as_ref() == Some(ticket))
            .map_or(0.0, |job| job.other_need_at_admit);
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
        let mut persist: Option<(std::path::PathBuf, f64, f64, bool)> = None;
        let mut changed = false;
        if let Some(entry) = self.catalog.iter_mut().find(|m| m.name == model) {
            // Stamped with the engine's own truth set, the reading the page and the
            // adapter use, so the invalidation cannot disagree with them (HANDOFF §6).
            let offload = entry
                .raw
                .get("cpu_offload")
                .is_some_and(|v| crate::engine::llama::jinja_on(v));
            let high = entry.measured_gb.unwrap_or(rounded).max(rounded);
            let low = entry
                .measured_min_gb
                .unwrap_or(rounded)
                .min(rounded)
                .min(high);
            if entry.measured_gb != Some(high)
                || entry.measured_min_gb != Some(low)
                || entry.measured_offload != Some(offload)
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
                entry.measured_offload = Some(offload);
                if let Some(path) = entry.file.clone() {
                    persist = Some((path, high, low, offload));
                }
                changed = true;
            }
        }
        if let Some((path, high, low, offload)) = persist {
            let name = model.to_string();
            tokio::task::spawn_blocking(move || {
                // All three keys in ONE rewrite: written separately, a failure between
                // them could leave an orphan or inverted pair, and the scanner rejects
                // that pair outright — dropping the whole model (review, 2026-10-27).
                let high_text = format!("{high:.1}");
                let low_text = format!("{low:.1}");
                let stamp = offload.to_string();
                if let Err(error) = crate::model::writeback::set_flags_on_file(
                    &path,
                    &[
                        ("measured_gb", high_text.as_str()),
                        ("measured_gb_min", low_text.as_str()),
                        ("measured_offload", stamp.as_str()),
                    ],
                ) {
                    tracing::warn!(
                        error = %error,
                        model = %name,
                        "cannot persist the measured range"
                    );
                }
            });
        }
        if changed {
            self.publish();
        }
    }
    fn begin_load(&mut self, slot: usize, entry: ModelEntry, waiters: Vec<Waiter>) {
        let job_id = JobId(self.next_job);
        self.next_job += 1;
        let t0 = Instant::now();
        let mut job = Job {
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
            t0,
            waiters,
            tasks: Vec::new(),
            spawned: None,
            readiness: None,
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
        };
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
        let error = match need {
            Ok(need) => {
                job.need = Some(need);
                None
            }
            // A greedy load's nominal size never binds — it offloads instead —
            // so an unknown size is allowed (booking uses the room at admit).
            Err(_) if entry.cost == crate::model::CostClass::Greedy => None,
            Err(error) => Some(SchedulerError::Admission(error)),
        };
        if let Some(error) = error {
            job.error = Some(error.clone());
            self.jobs.insert(job_id, job);
            self.slots[slot].job = Some(job_id);
            self.step(job_id, SwapState::Admit);
            self.step(job_id, SwapState::AdmitFail);
            self.log_refusal(job_id, &error, "need");
            self.finish(job_id, Err(error), "under-specified");
            return;
        }
        let victim = self.slots[slot].occupant.as_ref().map(|o| Victim {
            model: o.model.clone(),
            ticket: o.ticket.clone(),
            slot,
            evicted: false,
            cost: o.need.0,
        });
        job.victim = victim.clone();
        self.jobs.insert(job_id, job);
        self.slots[slot].job = Some(job_id);
        self.set_timing(job_id, |t| &mut t.queue_ms);
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

    /// `Dispose -> Spawn` edge (G12): only called after release is confirmed, or when
    /// there was nothing to release.
    fn begin_spawn(&mut self, job_id: JobId) {
        self.step(job_id, SwapState::Spawn);
        let (engine, model) = {
            let job = &self.jobs[&job_id];
            (job.engine.clone(), job.model.clone())
        };
        let spawn_error = |reason: String| Event::Spawned(Err(SpawnError::SpawnFailed { reason }));
        let Some(adapter) = self.adapters.get(&engine).cloned() else {
            self.on_event(
                job_id,
                spawn_error(format!("no adapter registered for engine `{engine}`")),
            );
            return;
        };
        let Some(entry) = self.catalog.iter().find(|m| m.name == model).cloned() else {
            self.on_event(
                job_id,
                spawn_error(format!("model `{model}` vanished from the catalog mid-job")),
            );
            return;
        };
        let engine_port = self
            .jobs
            .get(&job_id)
            .and_then(|job| job.slot)
            .map(|slot| self.slots[slot].engine_port)
            .unwrap_or_default();
        let spec = match adapter.spec(&entry, engine_port) {
            Ok(spec) => spec,
            Err(error) => {
                self.on_event(job_id, spawn_error(error.to_string()));
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
                // Clear the released victim's own slot (same-slot replacement or a
                // cross-category evictee — never the job's slot blindly).
                let released = self.jobs[&job_id].victim.clone();
                if let Some(victim) = &released {
                    if self.slots[victim.slot]
                        .occupant
                        .as_ref()
                        .map(|o| o.ticket.clone())
                        == Some(victim.ticket.clone())
                    {
                        self.clear_occupant(victim.slot);
                    }
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
            (SwapState::CancelRequested, Event::Released(_)) => {
                self.step(job_id, SwapState::Reap);
                self.step(job_id, SwapState::Idle);
                let error = self.jobs[&job_id]
                    .error
                    .clone()
                    .unwrap_or_else(|| SchedulerError::Cancelled("cancelled".to_string()));
                self.finish(job_id, Err(error), "cancelled");
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
        // warning at load time, and the engine's real id wins for rewriting.
        let serve_id = discovered_id;
        if let Some(job) = self.jobs.get_mut(&job_id) {
            let declared = self
                .catalog
                .iter()
                .find(|m| m.name == job.model)
                .and_then(|m| m.serve_model.clone());
            if let (Some(declared), Some(actual)) = (&declared, &serve_id) {
                if declared != actual {
                    let warning = format!(
                        "serve_model `{declared}` is not reported by the engine; using its real id `{actual}`"
                    );
                    tracing::warn!(%job_id, model = %job.model, "{warning}");
                    job.warnings.push(warning);
                }
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
            self.slots[slot].occupant = Some(Occupant {
                model: job_model,
                need: job_need,
                port: port.port,
                ticket: job_ticket.clone(),
                loaded_at: Instant::now(),
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
        let spawned;
        {
            let Some(job) = self.jobs.get_mut(&job_id) else {
                return;
            };
            if job.terminal {
                return;
            }
            for task in job.tasks.drain(..) {
                task.abort();
            }
            job.error = Some(error.clone());
            if vocab::transition_allowed(job.state, SwapState::CancelRequested) {
                job.state = SwapState::CancelRequested;
            }
            spawned = job.spawned.is_some() || job.victim_release_pending();
        }
        if spawned {
            // Kill what this job spawned (or was releasing), confirm, then Idle.
            self.spawn_kill_confirm(job_id);
        } else {
            self.step(job_id, SwapState::Reap);
            self.step(job_id, SwapState::Idle);
            self.finish(job_id, Err(error), "cancelled");
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
        if let Some(slot) = slot {
            if self.slots[slot].job == Some(job_id) {
                self.slots[slot].job = None;
            }
        }
        // Supersede: a parked load starts when its slot is free again (T3.4).
        if let Some(slot) = slot {
            if self.slots[slot].job.is_none() && self.slots[slot].occupant.is_none() {
                if let Some(pending) = self.pending.remove(&slot) {
                    self.start_pending(slot, pending);
                }
            }
        }
    }

    fn start_pending(&mut self, slot: usize, pending: PendingSwap) {
        match self
            .catalog
            .iter()
            .find(|m| m.name == pending.model)
            .cloned()
        {
            Some(entry) => self.begin_load(slot, entry, pending.waiters),
            None => {
                for waiter in pending.waiters {
                    let _ = waiter.send(Err(SchedulerError::UnknownModel(pending.model.clone())));
                }
            }
        }
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
    /// Whether a release task is in flight for this job's victim (Quiesce/Dispose).
    fn victim_release_pending(&self) -> bool {
        matches!(self.state, SwapState::Quiesce | SwapState::Dispose) && self.victim.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::{headline_fault, JobId, JobKind, JobStatus, SwapState};
    use crate::config::CategoryId;

    #[test]
    fn a_stale_reap_target_does_not_abort_the_rest() {
        // Chunk 2: aborting the reap on a stale victim ticket stranded the job's own live
        // engine — the very orphan the multi-target reap exists to prevent.
        use super::ActorState;
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
