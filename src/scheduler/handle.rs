//! The caller half: the handle everyone else holds to talk to the actor, the
//! accepted reply, and the refusal vocabulary with numbers (G13).
//!
//! Owns no state: the handle is the only way to reach the actor (AGENTS.md: no
//! globals). Commands travel as messages; answers return as oneshot replies.

use crate::budget::AdmissionError;
use crate::config::CategoryId;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use super::{ActorMsg, Command, JobId, SchedulerStatus, SwapReceipt};

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
    /// The card (or, for a CPU-resident row, the host RAM — W2 ruling
    /// 2026-11-05) had not physically freed the gate target within
    /// `settle_timeout_s` after a confirmed release; nothing was double-spawned
    /// (§0.3). The reason is the witness's own numbers, composed once at the emit
    /// site and carried structurally — the gate never re-parses a human string.
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
    pub(super) tx: mpsc::UnboundedSender<ActorMsg>,
    pub(super) events: tokio::sync::broadcast::Sender<SchedulerStatus>,
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
