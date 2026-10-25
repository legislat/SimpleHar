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

use crate::budget::{self, AdmissionError, Gibibytes};
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

/// Failures reported to swap/unload/cancel callers, with the numbers used (G13).
#[derive(Debug, Clone, PartialEq, Error)]
pub enum SchedulerError {
    /// No catalog entry for the requested name.
    #[error("unknown model `{0}`")]
    UnknownModel(String),
    /// No job with this id (already finished and pruned, or never existed).
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
    /// Memory did not settle to the expected baseline within `settle_timeout_s`
    /// after a confirmed release; nothing was double-spawned (§0.3).
    #[error(
        "settle timeout: {used_gib:.1} GiB used vs {expected_gib:.1} GiB expected after release"
    )]
    SettleTimeout {
        /// Witness-used at the timeout, in GiB.
        used_gib: f64,
        /// Baseline (remaining ledger + externals + tolerance), in GiB.
        expected_gib: f64,
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
    /// Admission cost of the occupant in GiB.
    pub need_gib: Option<f64>,
    /// Job currently running for this slot, if any.
    pub loading_job: Option<JobId>,
}

/// One job ledger entry (Step 12: this is what `loading: bool` always pretended to be).
#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    /// Job id.
    pub id: JobId,
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
    /// Admission cost in GiB, once computed.
    pub need_gib: Option<f64>,
    /// Free VRAM at admit time in GiB (the number that was used).
    pub free_at_admit_gib: Option<f64>,
    /// Step timings (§0.3).
    pub timings: JobTimings,
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
        SchedulerHandle { tx, events }
    }
}

enum ActorMsg {
    Command {
        cmd: Command,
        reply: Waiter,
    },
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
    /// Readiness loop finished (terminal tri-state only).
    Probed(Probe),
    /// Witness sample for admission returned (Err = unavailable, ledger-only).
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
}

/// A release target during Quiesce/Dispose (Step 6 calls these victims).
#[derive(Clone)]
struct Victim {
    model: String,
    ticket: SpawnTicket,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum JobKind {
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
    free_at_admit: Option<Gibibytes>,
    /// External VRAM observed at admit (victim still owned); the settle gate holds
    /// this constant so a dead own engine's leftover memory cannot masquerade as a
    /// foreign holder (§0.3).
    external_at_admit: Option<f64>,
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

/// Splits the settle task's "U GiB used vs E GiB expected" payload back into
/// numbers for the typed error (kept simple: two f64s, space-separated).
fn parse_settle_numbers(numbers: &str) -> (f64, f64) {
    let values: Vec<f64> = numbers
        .split_whitespace()
        .filter_map(|token| token.parse::<f64>().ok())
        .collect();
    (
        values.first().copied().unwrap_or(f64::NAN),
        values.get(1).copied().unwrap_or(f64::NAN),
    )
}

async fn actor_loop(mut state: ActorState, mut rx: mpsc::UnboundedReceiver<ActorMsg>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            ActorMsg::Command { cmd, reply } => state.on_command(cmd, reply),
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

    fn on_unload(&mut self, category: CategoryId, reply: Waiter) {
        let targets: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.category == category && s.occupant.is_some() && s.job.is_none())
            .map(|(i, _)| i)
            .collect();
        for slot in targets {
            let job_id = JobId(self.next_job);
            self.next_job += 1;
            let victim = {
                let occupant = self.slots[slot]
                    .occupant
                    .as_ref()
                    .expect("invariant: filtered on occupant.is_some");
                Victim {
                    model: occupant.model.clone(),
                    ticket: occupant.ticket.clone(),
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
                free_at_admit: None,
                external_at_admit: None,
                timings: JobTimings::default(),
                t0: Instant::now(),
                waiters: Vec::new(),
                tasks: Vec::new(),
                spawned: None,
                readiness: None,
                victim: Some(victim),
                slot: Some(slot),
            };
            self.jobs.insert(job_id, job);
            self.slots[slot].job = Some(job_id);
            self.step(job_id, SwapState::Admit);
            self.step(job_id, SwapState::Quiesce);
            self.step(job_id, SwapState::Dispose);
            self.spawn_kill_confirm(job_id);
        }
        let _ = reply.send(Ok(Reply::Done));
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
            external_at_admit: None,
            timings: JobTimings::default(),
            t0,
            waiters,
            tasks: Vec::new(),
            spawned: None,
            readiness: None,
            victim: None,
            slot: Some(slot),
        };
        let need = budget::need_gb(entry.vram_gb.map(Gibibytes), None);
        let error = match need {
            Ok(need) => {
                job.need = Some(need);
                None
            }
            Err(error) => Some(SchedulerError::Admission(error)),
        };
        if let Some(error) = error {
            job.error = Some(error.clone());
            self.jobs.insert(job_id, job);
            self.slots[slot].job = Some(job_id);
            self.step(job_id, SwapState::Admit);
            self.step(job_id, SwapState::AdmitFail);
            self.finish(job_id, Err(error), "under-specified");
            return;
        }
        let victim = self.slots[slot].occupant.as_ref().map(|o| Victim {
            model: o.model.clone(),
            ticket: o.ticket.clone(),
        });
        job.victim = victim.clone();
        self.jobs.insert(job_id, job);
        self.slots[slot].job = Some(job_id);
        self.set_timing(job_id, |t| &mut t.queue_ms);
        self.step(job_id, SwapState::Admit);
        // Step 4: the gate is evaluated against the ledger with the witness as a
        // fresh, reconciling sample — sampled on a task, never inline in the actor.
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

    /// The Admit decision, once the witness sample returns: external reservations
    /// subtract (rule 2), then the ledger gate decides (rules 1-4).
    fn finish_admit(&mut self, job_id: JobId, sample: Result<crate::budget::MemorySample, String>) {
        let (total_gib, external_gib) = match &sample {
            Ok(sample) => (sample.total_gib, sample.external_gib(&self.own_pids())),
            Err(reason) => {
                tracing::warn!(reason = %reason,
                    "witness unavailable; admission runs ledger-only (no stale guesses, F-3/F-7)");
                (self.cfg.vram_total_gb, 0.0)
            }
        };
        let need = self.jobs[&job_id].need.unwrap_or_default();
        let victim_need = self.jobs[&job_id]
            .slot
            .map(|slot| self.slots[slot].occupant.as_ref().map_or(0.0, |o| o.need.0))
            .unwrap_or(0.0);
        let ledger = self.ledger_sum();
        let free = budget::free_gb(
            Gibibytes(total_gib),
            Gibibytes(self.cfg.vram_reserve_gb),
            ledger,
            Gibibytes(external_gib),
        );
        let external_at_admit = match &sample {
            Ok(sample) => Some(sample.external_gib(&self.own_pids())),
            Err(_) => None,
        };
        if let Some(job) = self.jobs.get_mut(&job_id) {
            job.free_at_admit = Some(free);
            job.external_at_admit = external_at_admit;
        }
        self.set_timing(job_id, |t| &mut t.admit_ms);
        if need.0 > free.0 + victim_need {
            self.step(job_id, SwapState::AdmitFail);
            self.finish(
                job_id,
                Err(SchedulerError::Admission(AdmissionError::Insufficient {
                    need,
                    free,
                    reserve: Gibibytes(self.cfg.vram_reserve_gb),
                })),
                "admission refused",
            );
            return;
        }
        self.step(job_id, SwapState::Quiesce);
        self.step(job_id, SwapState::Dispose);
        if self.jobs[&job_id].victim.is_some() {
            self.spawn_kill_confirm(job_id);
        } else {
            self.begin_spawn(job_id);
        }
    }

    /// PIDs of everything this harness spawned (occupants + in-flight spawns), for
    /// witness attribution of external usage (Step 4 rule 2).
    fn own_pids(&self) -> Vec<u32> {
        let mut pids: Vec<u32> = self
            .slots
            .iter()
            .filter_map(|slot| slot.occupant.as_ref())
            .map(|occupant| occupant.ticket.pid)
            .collect();
        pids.extend(
            self.jobs
                .values()
                .filter_map(|job| job.spawned.as_ref())
                .map(|ticket| ticket.pid),
        );
        pids
    }

    /// §0.3 memory evidence: after a confirmed release, wait until the witness sees
    /// `remaining ledger + externals + tolerance` before the spawn fires.
    fn spawn_settle_task(&mut self, job_id: JobId) {
        let baseline = self.ledger_sum().0;
        let tolerance = self.cfg.witness_tolerance_gb;
        // Externals are frozen at admit: a leftover from our own dead engine must
        // show up as "used above expected", not as a foreign holder.
        let external = self.jobs[&job_id].external_at_admit.unwrap_or(0.0);
        let deadline = Duration::from_secs(self.cfg.settle_timeout_s);
        let witness = Arc::clone(&self.witness);
        let tx = self.tx.clone();
        let handle = tokio::spawn(async move {
            let started = Instant::now();
            loop {
                match witness.sample().await {
                    Ok(sample) => {
                        let expected = baseline + external + tolerance;
                        if sample.used_gib <= expected {
                            let _ = tx.send(ActorMsg::Event {
                                job: job_id,
                                event: Event::Settled(Ok(())),
                            });
                            return;
                        }
                    }
                    Err(reason) => {
                        tracing::warn!(reason = %reason,
                            "witness unavailable during settle; proceeding (release was confirmed)");
                        let _ = tx.send(ActorMsg::Event {
                            job: job_id,
                            event: Event::Settled(Ok(())),
                        });
                        return;
                    }
                }
                if started.elapsed() >= deadline {
                    let numbers = match witness.sample().await {
                        Ok(sample) => format!(
                            "{:.1} GiB used vs {:.1} GiB expected",
                            sample.used_gib,
                            baseline + external + tolerance
                        ),
                        Err(_) => "no witness sample".to_string(),
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
        let ticket = SpawnTicket {
            generation: self.next_gen,
            kind: ResourceKind::ChildProcess,
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
        let deadline = Duration::from_secs(self.cfg.load_timeout_s);
        let handle = tokio::spawn(async move {
            let started = Instant::now();
            let terminal = loop {
                match runtime.probe(port, &readiness).await {
                    Probe::Ready => break Probe::Ready,
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
                event: Event::Probed(terminal),
            });
        });
        self.jobs
            .get_mut(&job_id)
            .expect("invariant: caller holds a live job id")
            .tasks
            .push(handle);
    }

    /// Kill the job's victim (or its own spawn on failure/cancel) and confirm release
    /// with evidence, inside `release_timeout_s` (G12).
    fn spawn_kill_confirm(&mut self, job_id: JobId) {
        let victim = {
            let job = &self.jobs[&job_id];
            let ticket = job
                .victim
                .as_ref()
                .map(|v| v.ticket.clone())
                .or_else(|| job.spawned.clone())
                .expect("invariant: kill targets exist for jobs in release phases");
            Victim {
                model: job.model.clone(),
                ticket,
            }
        };
        let tx = self.tx.clone();
        let runtime = Arc::clone(&self.runtime);
        let timeout = Duration::from_secs(self.cfg.release_timeout_s);
        let handle = tokio::spawn(async move {
            let outcome = match tokio::time::timeout(timeout, async {
                runtime.kill(&victim.ticket).await?;
                loop {
                    if runtime.release_confirmed(&victim.ticket).await? {
                        break Ok(());
                    }
                    tokio::time::sleep(RELEASE_POLL).await;
                }
            })
            .await
            {
                Ok(inner) => inner,
                Err(_) => Err(SpawnError::KillFailed {
                    holder: victim.model.clone(),
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
                let slot = self.jobs[&job_id].slot;
                if let Some(slot) = slot {
                    self.slots[slot].occupant = None;
                }
                if self.jobs[&job_id].kind == JobKind::Load {
                    // §0.3: release is proven; now the memory evidence must settle
                    // before the spawn fires (settle is only legal after a release).
                    self.spawn_settle_task(job_id);
                } else {
                    self.step(job_id, SwapState::Reap);
                    self.step(job_id, SwapState::Idle);
                    self.finish(job_id, Ok(Reply::Done), "unloaded");
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
            (SwapState::Ready, Event::Probed(Probe::Ready)) => {
                self.install(job_id);
            }
            (SwapState::Ready, Event::Probed(Probe::Failed { reason })) => {
                self.fail_job(
                    job_id,
                    SchedulerError::EngineFailed(reason),
                    "engine not ready",
                );
            }
            (SwapState::Ready, Event::Probed(Probe::NotYet { .. })) => {
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
            (SwapState::Dispose, Event::Settled(Err(numbers))) => {
                let (used, expected) = parse_settle_numbers(&numbers);
                self.fail_job(
                    job_id,
                    SchedulerError::SettleTimeout {
                        used_gib: used,
                        expected_gib: expected,
                    },
                    "memory did not settle after release",
                );
            }
            (other_state, other_event) => {
                tracing::warn!(state = ?other_state, event = ?other_event, %job_id, "unexpected event ignored");
            }
        }
    }

    /// `Ready -> Install -> Serving`: the one place an occupant is created (F-6 funnel).
    fn install(&mut self, job_id: JobId) {
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
        if let Some(slot) = slot {
            let job = &self.jobs[&job_id];
            self.slots[slot].occupant = Some(Occupant {
                model: job.model.clone(),
                need: job.need.unwrap_or_default(),
                port: port.port,
                ticket: job
                    .spawned
                    .clone()
                    .expect("invariant: install only happens after a spawn"),
                loaded_at: Instant::now(),
            });
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

    fn ledger_sum(&self) -> Gibibytes {
        Gibibytes(
            self.slots
                .iter()
                .filter_map(|s| s.occupant.as_ref())
                .map(|o| o.need.0)
                .sum(),
        )
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
                    need_gib: s.occupant.as_ref().map(|o| o.need.0),
                    loading_job: s.job,
                })
                .collect(),
            jobs: self
                .jobs
                .iter()
                .map(|(id, j)| JobStatus {
                    id: *id,
                    model: j.model.clone(),
                    category: j.category,
                    state: j.state,
                    terminal: j.terminal,
                    outcome: j.outcome.clone(),
                    error: j.error.as_ref().map(|e| e.to_string()),
                    need_gib: j.need.map(|n| n.0),
                    free_at_admit_gib: j.free_at_admit.map(|n| n.0),
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
