//! Engine adapters and launch specs (DESIGN Step 7 — data-first, code as escape hatch).
//!
//! The primary artifact is an engine *template* resolved from data; the trait stays
//! because the mock engine in tests is exactly such an adapter. `LaunchSpec` must be
//! constructible from data alone — no adapter may hide a decision a template could
//! express. Readiness is typed, never a string heuristic (defect D7/E1 class).

pub mod mock;

use crate::model::ModelEntry;
use crate::ownership::SpawnTicket;
use std::collections::BTreeMap;
use std::path::PathBuf;
use thiserror::Error;

/// Why an adapter could not produce a [`LaunchSpec`].
#[derive(Debug, Clone, PartialEq, Error)]
pub enum EngineError {
    /// The model entry lacks a key the engine requires to launch.
    #[error("model `{model}` is missing required field `{field}`")]
    MissingField {
        /// Harness name of the model.
        model: String,
        /// Missing key.
        field: String,
    },
    /// The adapter rejected the model for an engine-specific reason.
    #[error("engine `{engine}` rejected model `{model}`: {reason}")]
    Rejected {
        /// Engine id.
        engine: String,
        /// Harness name of the model.
        model: String,
        /// Engine-specific explanation (G6).
        reason: String,
    },
}

/// Readiness as typed data: one HTTP expectation, or two in sequence (Step 7 sketch).
///
/// Placeholders in `path` are confined to argv-style substitution — there is no string
/// mini-language for liveness or kill here (typed `kill`/`liveness` arrive with the
/// template schema, item 9).
#[derive(Debug, Clone, PartialEq)]
pub enum ReadinessSpec {
    /// `GET path` must answer `expect_status`.
    Http { path: String, expect_status: u16 },
    /// Both specs in order (e.g. `/health` then `/v1/models` for ninfer).
    Then(Box<ReadinessSpec>, Box<ReadinessSpec>),
}

/// Everything the engine-blind spawn/kill/reap machinery needs (Step 7).
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchSpec {
    /// Argv, fully substituted.
    pub argv: Vec<String>,
    /// Working directory for the child.
    pub cwd: PathBuf,
    /// Extra environment for the child.
    pub env: BTreeMap<String, String>,
    /// Log file base name; the process module appends the epoch (Step 8).
    pub log_name: String,
    /// Readiness contract.
    pub readiness: ReadinessSpec,
}

/// Tri-state probe outcome (Step 7): the scheduler decides what a missing field means
/// per engine, in one place — with the reason surfaced in the job record (G6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Ready to serve.
    Ready,
    /// Not yet; keep waiting inside the deadline.
    NotYet { reason: String },
    /// Terminal failure; the reason goes to the job record.
    Failed { reason: String },
}

/// Produces launch specs for one engine id (Step 7).
///
/// Probing is not part of the trait yet: it lands with the scheduler milestone, on
/// blocking tasks with deadlines (Step 2), driven by [`ReadinessSpec`] data.
pub trait EngineAdapter: Send + Sync {
    /// The `engine:` name this adapter serves.
    fn engine_id(&self) -> &str;

    /// Builds the launch spec for `model`.
    ///
    /// # Errors
    ///
    /// [`EngineError::MissingField`] / [`EngineError::Rejected`] as documented; these
    /// become `Refused` job records, never panics.
    fn spec(&self, model: &ModelEntry) -> Result<LaunchSpec, EngineError>;
}

/// Why a spawn/kill/release step failed at the process layer (Step 8).
#[derive(Debug, Clone, PartialEq, Error)]
pub enum SpawnError {
    /// The engine could not be started.
    #[error("spawn failed: {reason}")]
    SpawnFailed {
        /// What went wrong.
        reason: String,
    },
    /// The kill path failed for a resource the harness owns.
    #[error("kill failed for {holder}: {reason}")]
    KillFailed {
        /// Human name of the resource (model or container).
        holder: String,
        /// What went wrong.
        reason: String,
    },
}

/// A started engine instance; the runtime assigned `port` (Step 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spawned {
    /// Port the engine now serves on.
    pub port: u16,
}

/// Process-layer operations, driven by the scheduler actor but never executed inline
/// in the actor loop (Step 2: no I/O in the actor; Step 8: one implementation).
///
/// The production implementation (hidden spawn, Job Objects, log files, container
/// labels) is the future `process.rs`; the mock implementation used by tests lives in
/// `tests/mock_runtime.rs` (Step 19).
#[async_trait::async_trait]
pub trait EngineRuntime: Send + Sync {
    /// Starts an engine for `spec`; the ticket stamps provenance on the result.
    ///
    /// # Errors
    ///
    /// [`SpawnError::SpawnFailed`] when the engine cannot be started.
    async fn spawn(&self, spec: &LaunchSpec, ticket: SpawnTicket) -> Result<Spawned, SpawnError>;

    /// Stops an owned resource. Only ever called with an owned ticket (Step 1).
    ///
    /// # Errors
    ///
    /// [`SpawnError::KillFailed`] when the stop command itself fails.
    async fn kill(&self, ticket: &SpawnTicket) -> Result<(), SpawnError>;

    /// G12 release evidence: `true` only when death is *proven* (handle signaled /
    /// job tree empty / container gone + port closed).
    ///
    /// # Errors
    ///
    /// [`SpawnError::KillFailed`] when the evidence check cannot be performed.
    async fn release_confirmed(&self, ticket: &SpawnTicket) -> Result<bool, SpawnError>;

    /// One readiness attempt against `readiness` (Step 7 tri-state).
    async fn probe(&self, port: u16, readiness: &ReadinessSpec) -> Probe;
}
