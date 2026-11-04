//! Status snapshot types for `/har/status` and the SSE document.
//!
//! These are pure data types. They never probe and never mutate. The actor owns
//! the values; this module only defines the shape.

use crate::config::CategoryId;
use crate::model::scan::ScanError;
use serde::Serialize;
use std::collections::BTreeSet;
use std::time::Instant;

use super::{jobs::JobStatus, JobId};

/// Immutable status snapshot (Step 2: reads never probe and never mutate).
#[derive(Debug, Clone, Serialize)]
pub struct SchedulerStatus {
    /// Every materialized slot (AUX expands to `max_models` entries).
    pub slots: Vec<SlotStatus>,
    /// Job ledger, oldest first.
    pub jobs: Vec<JobStatus>,
    /// Catalog problems from the last scan (bad files, unresolved defaults).
    pub catalog_errors: Vec<ScanError>,
    /// R5 report-only reconcile (ruling 9): running containers matching a catalog
    /// `ninfer_container` row while the claim ledger holds no claim — the SAME set
    /// the reconcile warned about, carried on this one snapshot so the page and the
    /// log can never disagree. Absent from `/har/status` until a probe reports a
    /// non-empty set (an empty last-reported set normalizes to `None` at the single
    /// snapshot accessor — the skip predicate's polarity stays `is_none`, the
    /// GOTCHAS inverted-predicate class); a failed probe keeps the last-reported
    /// set, exactly like the warn dedupe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stranded_containers: Option<BTreeSet<String>>,
    /// R11 session cross-check (rule 2026-11-01, display-only): the models whose
    /// Fit booking a usable witness reading confirmed inside the session
    /// tolerance while the current fit epoch stands — the page turns those rows
    /// green. In-memory actor stamps filtered to the epoch, never persisted and
    /// never a gate input; a rescan (epoch moves) or a calculator swap (version
    /// invalidation) empties it. Skip-serialized because the verdict rides the
    /// per-model rows (`/har/models` and the SSE document carry it), never as a
    /// top-level list of `/har/status`.
    #[serde(skip)]
    pub fit_validated: BTreeSet<String>,
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
    /// The slot lifecycle the server derives: `empty`, `loading`, `serving`,
    /// or `releasing` (operator ruling 2026-11-04). `releasing` names a
    /// committed release: the occupant is the target of a live unload or
    /// eviction and stays installed until the driver confirms the release, so
    /// the label covers the whole window from accept to confirmation. A
    /// release that timed out is terminal: the occupant stays on purpose and
    /// the slot reads `serving`, never a stuck `releasing`. The page renders
    /// this value instead of re-deriving the dot.
    pub state: String,
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
    /// What this container occupant's own startup `MemorySummary` said it reserves
    /// (R9), straight from its spawn anchor — `None` for a child-process spawn, a
    /// container that never reported, or a report the anchor refused. Carried on
    /// the snapshot so BOTH `/har/status` and SSE quote the same number (review
    /// 2026-10-31: threading it only through the SSE build made the memory-bar
    /// annotation unreachable for a token-gated, poll-only page); display only,
    /// never a gate input.
    pub container_reported_gb: Option<f64>,
    /// When the occupant was loaded (copy of the actor's `Occupant::loaded_at`).
    /// Empty slots carry the snapshot time, mirroring the `map_or(Instant::now(),
    /// |o| o.loaded_at)` policy of the scheduler's own LRU picks. Not serialized:
    /// an `Instant` has no wall-clock meaning outside the process, so `/har/status`
    /// keeps exposing `idle_s`. The no-`model` proxy fallback sorts untouched
    /// occupants by this (FINDINGS-06 \[1]: never fabricate an instant instead).
    #[serde(skip)]
    pub loaded_at: Instant,
}
