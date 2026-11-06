//! Job ledger types: the in-flight job row, its `/har/status` projection, and the
//! pure rules over the ledger (the fault rule, the headline pick, the booked GiB).
//!
//! Data and pure functions only: this module never probes, never mutates, and
//! never touches the card, the filesystem, or an engine. The actor owns the values.

use crate::budget::Gibibytes;
use crate::config::CategoryId;
use crate::engine::ReadinessSpec;
use crate::ownership::SpawnTicket;
use serde::Serialize;
use std::time::Instant;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::{JobId, MeasuredAnchor, Reply, SchedulerError, SwapState};

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

/// One job ledger entry (Step 12: this is what `loading: bool` always pretended to be).
#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    /// Job id.
    pub id: JobId,
    /// Load or unload (A3: the headline fault only counts loads).
    pub kind: JobKind,
    /// The admission rung the gate decided for this load (B6 cost ladder).
    /// The rung names the decision, not the outcome: a load a later step
    /// refused still names the rung the gate took. A load the gate refused
    /// never reached a rung, and an unload row never made an admission
    /// decision, so both omit the field on the wire.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<AdmissionPath>,
    /// Model the job acts on.
    pub model: String,
    /// Category the job targets.
    pub category: CategoryId,
    /// Current machine state.
    pub state: SwapState,
    /// Whether the job reached a terminal state.
    pub terminal: bool,
    /// Terminal outcome note ("loaded", "unloaded", "evicted", ...). A
    /// confirmed release rewrites a released resident's terminal load row from
    /// "loaded" to the word that release earned (the scheduler's single commit
    /// point), so a gone model never reads as serving on `/har/jobs`.
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

/// FR-01 ledger retention (code constant, never a config key): the newest
/// retired terminal rows the ledger keeps. Live (non-terminal) rows and the
/// terminal rows still in service — an installed occupant's own load row, which
/// the serve-window measured fold and `container_reported_gb` read by its spawn
/// ticket — stand outside this bound; see `ActorState::prune_terminal_ledger`.
pub(super) const TERMINAL_ROW_KEEP: usize = 200;

/// The pure retention rule behind FR-01: given the ids of the **droppable**
/// terminal rows in ledger order (oldest first — `BTreeMap` order is `JobId`
/// order, and ids mint monotonically), the ids that leave the ledger so at most
/// [`TERMINAL_ROW_KEEP`] retired terminal rows stand. Never called with live or
/// in-service rows: those are not droppable (FR-01).
#[must_use]
pub(super) fn terminal_rows_to_prune(droppable: &[JobId]) -> &[JobId] {
    let drop_upto = droppable.len().saturating_sub(TERMINAL_ROW_KEEP);
    &droppable[..drop_upto]
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

pub(super) type Waiter = oneshot::Sender<Result<Reply, SchedulerError>>;

/// A release target during Quiesce/Dispose (Step 6 calls these victims).
#[derive(Clone)]
pub(super) struct Victim {
    pub(super) model: String,
    pub(super) ticket: SpawnTicket,
    /// The slot the victim occupies, cleared when its release is confirmed.
    pub(super) slot: usize,
    /// Admission cost freed by this release, in the resource the drain plans
    /// (RAM booking on the RAM ladder, W21-01 ruling; ledger arithmetic, Step 6
    /// re-check).
    pub(super) cost: f64,
    /// True when this victim was a cross-category eviction (vs a same-slot
    /// replacement); recorded in the job ledger (G13).
    pub(super) evicted: bool,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize)]
pub enum JobKind {
    Load,
    Unload,
}

/// The expert-offload RAM reclaim (operator ruling 2026-11-06) planned for a
/// `cost: greedy` load admitted on an `expert_offload` engine: the CPU-resident
/// rows queued for drain (largest booked footprint first), the CPU-resident
/// rows above the threshold that the drain must skip (model, reason), and the
/// host RAM in GiB the queued rows book. These victims are not the deficit
/// planner's `victims`: a GPU row has no host-RAM gate, so this queue is the
/// posture the ruling ordered, drained after the same-slot replacement and the
/// deficit victims, each through the unchanged confirm-and-settle release path.
#[derive(Default, Clone)]
pub(super) struct Reclaim {
    /// CPU residents above the threshold, drained largest first, in release
    /// order. `Victim::cost` is `0.0` by design: a CPU release books no card
    /// bytes, and crediting host GiB to the card's drain re-check would be a
    /// figure the gate cannot honestly state.
    pub(super) victims: Vec<Victim>,
    /// Rows above the threshold the drain skips instead of draining (a
    /// `min_turn` guard or the `evictable_by` matrix): name and reason, never
    /// a failure of the incoming load.
    pub(super) skipped: Vec<(String, String)>,
    /// Sum of the queued victims' booked host RAM in GiB (the log's reclaimed
    /// figure, planned at decision time).
    pub(super) gib: f64,
}

/// The rung of the making-room ladder an admitted load actually took.
///
/// Shape borrowed from the Berth broker's making-room ladder (fits-now,
/// beside-residual, lend, park, drain, cheapest first); the gate implements
/// the three rungs below, derived at the one admission-decision site
/// (`finish_admit` in `admission.rs`) — one derivation, no second truth.
/// The rungs `beside-residual`, `lend`, and `park` are reserved for a
/// future step: no code path derives them.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize)]
pub enum AdmissionPath {
    /// The gate passed the load with no release performed to make room. A
    /// same-slot replacement belongs here: releasing the occupant the
    /// request itself named is the request, not a rung. The gate shape
    /// (numeric need, launch floor, `whole`) never changes the rung.
    FitsNow,
    /// The same-slot release-first door: no planned victim set covered the
    /// deficit, a release could still fix it, and the load released its own
    /// slot's occupant before re-gating on the physically re-sampled card.
    ReleaseFirst,
    /// The eviction planner queued cross-category victims and the load
    /// admitted to drain them.
    Drain,
}

pub(super) struct Job {
    pub(super) kind: JobKind,
    /// The rung `finish_admit` recorded when this load passed the gate (B6
    /// cost ladder). `None` until that decision and for every unload row:
    /// the rung names a gate decision the row has not made. One derivation
    /// (in `admission.rs`); the `/har/jobs` projection reads this field and
    /// nothing else, so the ledger and the wire cannot disagree.
    pub(super) path: Option<AdmissionPath>,
    pub(super) model: String,
    pub(super) engine: String,
    pub(super) category: CategoryId,
    pub(super) state: SwapState,
    pub(super) terminal: bool,
    pub(super) outcome: Option<String>,
    pub(super) error: Option<SchedulerError>,
    pub(super) need: Option<Gibibytes>,
    /// The resource this row reserves against — derived from the catalog
    /// entry at registration (operator ruling 2026-11-05), frozen like the
    /// other derived keys: a reload must not flip a row mid-swap between
    /// ladders.
    pub(super) resource: crate::model::Resource,
    /// Host RAM booked for a CPU-resident load, armed in `start_queued_load`
    /// exactly where a VRAM row arms `need`; `None` for every VRAM row. A
    /// RAM row's `need` is 0 (it books no card bytes).
    pub(super) ram_need: Option<Gibibytes>,
    /// Cost class from the model yaml (fixed|greedy|exact) — drives the gate shape.
    pub(super) cost: crate::model::CostClass,
    /// The row's VRAM gate, frozen at registration with the other derived
    /// keys (`launch_only_with_at_least_vram_free:`): a numeric floor in GiB
    /// or the `whole` keyword.
    pub(super) vram_gate: crate::model::VramGate,
    /// Model's per-load readiness deadline (`load_timeout:`); `None` = config default.
    pub(super) load_timeout_s: Option<u64>,
    /// The `available` value (free_at_admit + released, each credit counted once) the
    /// release-minimality re-check must reach before spawning: `max(need, floor)` at
    /// plan time (floor alone for greedy). Without it, a greedy job whose booked need
    /// was clamped small would skip floor victims after the first release (review
    /// CRITICAL, 2026-10-26). The physical settle gate in
    /// [`ActorState::spawn_settle_task`] compares this same figure plus
    /// `vram_reserve_gb` against the card, so both halves apply one guard band
    /// (R2-01-01).
    pub(super) gate_target: Option<f64>,
    pub(super) free_at_admit: Option<Gibibytes>,
    /// Observed physical free (total - used) at admit; `None` when the witness
    /// was down. The release re-check and the settle gate take the more
    /// conservative figure against this (operator 2026-10-26).
    pub(super) observed_at_admit: Option<f64>,
    /// Ruling 2026-11-01 (revisable): this same-slot swap already took the
    /// release-first path — it destroyed its victim on the booked credit and
    /// re-gates on the physically re-sampled card instead of dying on paper.
    pub(super) release_first: bool,
    /// Detached WSL container backing the spawn (ninfer): suppresses the
    /// pid-attributed measured footprint, which is meaningless for a client
    /// process that exits before the container is ready (review, 2026-10-26).
    pub(super) container: Option<String>,
    pub(super) timings: JobTimings,
    pub(super) t0: Instant,
    pub(super) waiters: Vec<Waiter>,
    pub(super) tasks: Vec<JoinHandle<()>>,
    /// The engine this job spawned, once started (kill target on failure/cancel).
    pub(super) spawned: Option<SpawnTicket>,
    /// Readiness contract of the running spawn.
    pub(super) readiness: Option<ReadinessSpec>,
    /// Spawn-era measured anchor of the running spawn (`None` before `begin_spawn`).
    /// It rides the row exactly like `readiness` does: both describe what was
    /// launched, and readiness can run for minutes (a strata cold start) while a
    /// `reload()` replaces the catalog underneath them.
    pub(super) measured: Option<MeasuredAnchor>,
    /// The previous occupant this job must release before spawning.
    pub(super) victim: Option<Victim>,
    /// Cross-category victims queued by the eviction planner (Step 6), released
    /// one at a time after the same-slot replacement (if any).
    pub(super) victims: Vec<Victim>,
    /// The expert-offload RAM reclaim planned at admission (operator ruling
    /// 2026-11-06): CPU residents drained after the deficit queue, the rows
    /// skipped with their reasons, and the booked host GiB. Empty on every
    /// load the ruling does not address.
    pub(super) reclaim: Reclaim,
    /// Cross-category victims whose release is confirmed (observability, G13).
    pub(super) victims_released: Vec<String>,
    /// Ledger cost freed by confirmed releases so far (Step 6 minimality check).
    pub(super) released_cost: f64,
    /// The part of [`Self::released_cost`] that came from the non-evicted same-slot
    /// holder. Its booked need is already inside `free_at_admit`, so the ledger leg of
    /// the release re-check subtracts it and counts it once (R2-03-01). The observed
    /// leg keeps the full figure: a physical reading never held the paper credit.
    pub(super) holder_released_cost: f64,
    /// Ledger need booked for *other* occupants when this engine was admitted. The
    /// measured diff subtracts only what appeared after this point, so a tenant that
    /// was already resident is not double-counted (review, 2026-10-27).
    pub(super) other_need_at_admit: f64,
    /// Outcome wording for unload jobs ("unloaded" / "idle timeout").
    pub(super) note: Option<String>,
    /// Non-fatal observations recorded during the job (serve_model mismatch...).
    pub(super) warnings: Vec<String>,
    pub(super) slot: Option<usize>,
    /// The model's yaml as it was when this row **queued** (reload-drift Axis 1,
    /// operator ruling 2026-10-29): the bytes the parked intent was taken on, for
    /// [`ActorState::invalidate_queued_loads`] to compare when a rescan lands.
    /// `None` = a synthetic entry, or the stat was unreadable at registration:
    /// nothing to compare, nothing to retire on.
    pub(super) queue_fingerprint: Option<crate::model::FileFingerprint>,
}

impl Job {
    /// Whether this job still owes a confirmed release for its victim.
    ///
    /// `victim` is only ever assigned and never cleared (see `spawn_kill_confirm`), so
    /// this answers "a reap target exists", not "a task is parked on it". It is
    /// deliberately independent of `state`: its only caller (`cancel_job`) asks it while
    /// it is moving the job out of that state, and a cancel that arrives in `Dispose`
    /// must still kill and confirm the victim (chunk 4 [1]).
    pub(super) fn victim_release_pending(&self) -> bool {
        self.victim.is_some()
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;

    fn ids(count: u64) -> Vec<JobId> {
        (0..count).map(JobId).collect()
    }

    #[test]
    fn the_retention_bound_is_two_hundred_in_code() {
        // FR-01: the constant is code, not config — pin the figure itself so a
        // silent edit moves a test, not just every count downstream.
        assert_eq!(TERMINAL_ROW_KEEP, 200);
    }

    #[test]
    fn over_the_bound_prunes_the_oldest_first() {
        // 250 terminal rows + one fresh finish = 251 droppable: the 51 oldest
        // (ids 0..=50) leave, the newest 200 stand.
        let droppable = ids(251);
        let drop = terminal_rows_to_prune(&droppable);
        assert_eq!(drop.len(), 51);
        assert_eq!(drop.first(), Some(&JobId(0)));
        assert_eq!(drop.last(), Some(&JobId(50)));
    }

    #[test]
    fn at_and_under_the_bound_prunes_nothing() {
        assert!(terminal_rows_to_prune(&ids(TERMINAL_ROW_KEEP as u64)).is_empty());
        assert!(terminal_rows_to_prune(&ids(1)).is_empty());
        assert!(terminal_rows_to_prune(&[]).is_empty());
        // One past the bound drops exactly the oldest.
        let just_over = ids(TERMINAL_ROW_KEEP as u64 + 1);
        let drop = terminal_rows_to_prune(&just_over);
        assert_eq!(drop, [JobId(0)]);
    }
}
