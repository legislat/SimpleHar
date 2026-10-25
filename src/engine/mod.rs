//! Engine adapters and launch specs (DESIGN Step 7 — data-first, code as escape hatch).
//!
//! The primary artifact is an engine *template* resolved from data; the trait stays
//! because the mock engine in tests is exactly such an adapter. `LaunchSpec` must be
//! constructible from data alone — no adapter may hide a decision a template could
//! express. Readiness is typed, never a string heuristic (defect D7/E1 class).

pub mod mock;

use crate::model::ModelEntry;
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
