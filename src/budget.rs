//! VRAM admission arithmetic (DESIGN Step 4, as amended by the operator
//! 2026-10-26: the **card** is authoritative, and the ledger survives only as the
//! declared source of truth for a `ledger_only` witness).
//!
//! This module owns the need formula and the budget arithmetic. The NVML witness and
//! the settle-only-after-release gate live with the scheduler milestone; item 5 is
//! approved (`nvml-wrapper` in-process, `nvidia-smi` fallback when the DLL is absent).

use thiserror::Error;

/// A VRAM quantity in GiB (Step 17 newtype: never mix a GiB with a raw float).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct Gibibytes(pub f64);

impl std::fmt::Display for Gibibytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.1} GiB", self.0)
    }
}

/// A candidate victim the eviction planner considered (Step 6 refusal contract).
#[derive(Debug, Clone, PartialEq)]
pub struct VictimInfo {
    /// Harness name of the candidate.
    pub model: String,
    /// Category it occupies.
    pub category: String,
    /// Its admission cost.
    pub need_gib: f64,
}

/// A model that blocks the load and cannot be evicted (Step 6 hard filters).
#[derive(Debug, Clone, PartialEq)]
pub struct BlockerInfo {
    /// Harness name of the blocker.
    pub model: String,
    /// Category it occupies.
    pub category: String,
    /// Why it is not evictable ("only WORK can evict WORK").
    pub reason: String,
}

/// The budgets a refusal was measured against, carried so the message can name
/// which one bound. `free` in the refusal is the figure the gate compared; these
/// are the inputs behind it, each printed only where it disagrees (G13).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BudgetFacts {
    /// Ledger budget at decision time: `total - reserve - Σ(occupant need)`. Quoted
    /// in refusals so an operator can see when the card and the bookings disagree.
    /// `None` in tests that assert the compared figure alone.
    pub budget: Option<Gibibytes>,
    /// Physical free the witness saw at decision time; `None` when it was down.
    pub observed_free: Option<Gibibytes>,
    /// R3 (operator GO 2026-11-01): the share of the card that no eviction the
    /// harness may perform can reclaim — foreign holders, non-evictable occupants
    /// and the driver's own baseline. `None` when the witness was down.
    pub unreleasable: Option<Gibibytes>,
    /// R3: what the card physically held at decision time (the witness's own
    /// `used_gib`), quoted beside `unreleasable` so the split is checkable.
    pub used_gib: Option<Gibibytes>,
    /// R3: the harness's whole ledger (Σ occupant bookings) beside the physical
    /// reading, so a paper deficit between the two is visible in the refusal.
    pub booked_total: Option<Gibibytes>,
}

impl BudgetFacts {
    /// The parenthetical naming every truth that differs from the compared figure.
    /// Empty when they all agree, so a plain refusal stays one clean sentence
    /// (review 2026-10-26: `free 69.7 GiB` printed while the card held 1.8 GiB).
    #[must_use]
    pub fn detail(&self, compared: Gibibytes) -> String {
        let mut parts = Vec::new();
        if let Some(budget) = self
            .budget
            .filter(|b| (b.0 - compared.0).abs() > DISAGREE_GIB)
        {
            parts.push(format!("budget {budget}"));
        }
        if let Some(observed) = self
            .observed_free
            .filter(|o| (o.0 - compared.0).abs() > DISAGREE_GIB)
        {
            parts.push(format!("observed free {observed}"));
        }
        let sentence = if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join(", "))
        };
        // R3: the refusal must name what the operator cannot release, so the
        // dead end ("free 30, need 40 — but WHO holds the 30?") is diagnosable
        // from the message alone. Printed only when the number is real.
        match self.unreleasable {
            Some(u) if u.0 > DISAGREE_GIB => {
                let mut names = Vec::new();
                if let Some(used) = self.used_gib {
                    names.push(format!("card holds {used}"));
                }
                if let Some(booked) = self.booked_total {
                    names.push(format!("booked {booked}"));
                }
                let beside = if names.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", names.join(", "))
                };
                format!("{sentence}; {u} unreleasable{beside}")
            }
            _ => sentence,
        }
    }
}

/// Who holds the card, graded from the **movement** of unaccounted memory across
/// the sample window (R3, ruling 10). A static foreign holder is not the
/// problem — the total-diff rides a baseline that already discounts it; the
/// problem is foreign memory *moving* while the reading is taken, which turns a
/// free-memory delta into someone else's story. This grades *trust in a
/// `total_diff` figure* — it never touches the physical gate, which reads the
/// raw card either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardOccupancy {
    /// Unaccounted memory did not move: a `total_diff` taken across the window
    /// is the engine's real footprint.
    Quiescent,
    /// Unaccounted memory moved, but less than the engine's own growth could
    /// explain: the `total_diff` is noisy-but-usable, keep it with its label.
    OwnDominated,
    /// Unaccounted memory moved by more than the engine's own growth: the
    /// window's delta is a foreign story, not a footprint.
    ForeignDominated,
}

/// Grades one sample window: `foreign_growth_gib` is how much further the
/// unaccounted remainder of the card drifted between admit and sample (it may
/// be negative — shrinking foreign memory is as unaccounted as growing), and
/// `own_growth_gib` is the footprint the fold itself attributes to the engine.
#[must_use]
pub fn grade_card_occupancy(
    foreign_growth_gib: f64,
    own_growth_gib: f64,
    tolerance_gib: f64,
) -> CardOccupancy {
    if foreign_growth_gib <= tolerance_gib {
        CardOccupancy::Quiescent
    } else if foreign_growth_gib <= own_growth_gib.max(0.0) {
        CardOccupancy::OwnDominated
    } else {
        CardOccupancy::ForeignDominated
    }
}

/// How far apart two GiB figures must be before the refusal bothers naming both.
const DISAGREE_GIB: f64 = 0.05;

/// What the refusal knows about WAITING, beside the named holders: an estimate
/// and a queue place, each `None` when the harness has no basis to quote it.
/// The two memory refusals carry this struct; the API renders `Some` figures
/// as structured JSON fields. Neither figure is a promise: the harness runs no
/// wait queue, so both describe the decision moment only (operator ruling
/// 2026-11-04: "an estimate, never a promise").
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WaitHint {
    /// Rolling median of the harness's own measured release durations in
    /// seconds (accept to driver-confirmed), one decimal when rendered. `None`
    /// when no release has been confirmed since boot: absent, never invented.
    pub expected_release_s: Option<f64>,
    /// One-based place behind the loads already ahead in the refused category,
    /// per the rule documented on `ActorState::wait_hint`. `None` when nothing
    /// is ahead and this refusal got no queue place — null, never a lie.
    pub queue_position: Option<u32>,
}

/// Refusal reasons of the admission gate, with the numbers used (G13).
#[derive(Debug, Clone, PartialEq, Error)]
pub enum AdmissionError {
    /// Neither `vram_gb` nor a GGUF size is known; the message says so explicitly
    /// (v3.3's F-7 lied about the reason).
    #[error("under-specified model: no `vram_gb` and no GGUF size to estimate from")]
    UnderSpecified,
    /// `cost: exact` with no `vram_gb`: the class asserts the engine's precise
    /// requirement, so falling back to a size estimate would be a guess (refused
    /// rather than booked on faith).
    #[error("cost `exact` requires an explicit `vram_gb` (the engine's computed requirement)")]
    ExactUndeclared,
    /// A ninfer row declared a harness- or manual-owned port number as its
    /// container-internal port (R10 separation, operator 2026-11-01: "8003-8005
    /// are manual only"): the container inside is its own namespace and must
    /// carry its own number — no docker collision is needed to make the
    /// confusion real (the 8004 post-mortem misread exactly such a number).
    #[error("`ninfer_internal_port` {port} claims {claim}: a container's internal port must live in its own namespace")]
    PortClaimed {
        /// The claimed number from the yaml (`ninfer_internal_port`, or the
        /// default the row would bind without the key).
        port: u16,
        /// The namespace that already owns it, named for the operator.
        claim: &'static str,
    },
    /// The card could not be read at all (NVML and the `nvidia-smi` fallback both
    /// failed) and this is a real GPU box, so there is no honest number to budget
    /// against. Refused rather than guessed: admission is the product, and a stale
    /// or absent reading is exactly how v3.3 over-committed onto a saturated card
    /// (F-3/F-7). A `ledger_only` witness is exempt — there the ledger is the
    /// declared source of truth (tests, machines without a GPU).
    #[error("witness unavailable, refusing to guess: {reason}")]
    WitnessUnavailable {
        /// Why the sample failed, verbatim from the witness.
        reason: String,
    },
    /// The gate rejected the load and no sufficient eviction set exists; the
    /// numbers, the candidates considered, and the named blockers all travel with
    /// the refusal (Step 6 contract, G6).
    #[error("insufficient VRAM: need {need}, free {free}, reserve {reserve}{detail}{tail}",
        detail = .facts.detail(*free),
        tail = insufficient_tail(.victims_considered, .blockers)
    )]
    Insufficient {
        /// Declared or estimated cost of the wanted load.
        need: Gibibytes,
        /// The figure the gate actually compared.
        free: Gibibytes,
        /// Reserve subtracted before the gate.
        reserve: Gibibytes,
        /// The inputs `free` was derived from (ledger / observed).
        facts: Box<BudgetFacts>,
        /// Candidates the planner considered (may be empty when none exist).
        victims_considered: Vec<VictimInfo>,
        /// Models that cannot be evicted, with reasons.
        blockers: Vec<BlockerInfo>,
        /// What the decision knows about waiting (estimate + place), computed
        /// at refusal time beside these numbers.
        wait_hint: WaitHint,
    },
    /// The model's launch floor was not met: the physical free VRAM seen at decision
    /// time sits below `launch_only_with_at_least_vram_free`, so the
    /// load is refused BEFORE spawn — launching a greedy-offload quant onto a
    /// busy card thrashes RAM/SSD for minutes (operator design note 2026-10-26).
    #[error(
        "launch floor not met: at least {min_free} free required, free {free}, reserve {reserve}{detail}{tail}{guard}",
        detail = .facts.detail(*free),
        tail = insufficient_tail(.victims_considered, .blockers),
        guard = if *no_releases_reach {
            "; No releases allow for the VRAM floor. Check foreign residents"
        } else {
            ""
        }
    )]
    BelowLaunchFloor {
        /// Configured floor in GiB (model key, or the greedy global default).
        min_free: Gibibytes,
        /// The figure the gate actually compared.
        free: Gibibytes,
        /// Reserve subtracted before the gate.
        reserve: Gibibytes,
        /// The inputs `free` was derived from (ledger / observed).
        facts: Box<BudgetFacts>,
        /// Candidates the planner considered (may be empty when none exist).
        victims_considered: Vec<VictimInfo>,
        /// Models that cannot be evicted, with reasons.
        blockers: Vec<BlockerInfo>,
        /// Row-10 guard (operator ruling 2026-11-01): even freeing everything the
        /// harness may release — its own booked same-slot occupant plus every
        /// evictable resident, never foreign occupancy — would not reach the floor.
        /// The refusal says so and the load was refused BEFORE any release.
        no_releases_reach: bool,
        /// What the decision knows about waiting (estimate + place), computed
        /// at refusal time beside these numbers.
        wait_hint: WaitHint,
    },
}

/// The human tail of the [`AdmissionError::Insufficient`] message: who was
/// considered and who blocks, so the refusal explains itself (G6).
fn insufficient_tail(victims_considered: &[VictimInfo], blockers: &[BlockerInfo]) -> String {
    let mut tail = String::new();
    if !victims_considered.is_empty() {
        tail.push_str("; considered evicting: ");
        tail.push_str(
            &victims_considered
                .iter()
                .map(|v| format!("{} ({}), {:.1} GiB", v.model, v.category, v.need_gib))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if !blockers.is_empty() {
        tail.push_str("; blocked by: ");
        tail.push_str(
            &blockers
                .iter()
                .map(|b| format!("{} ({})", b.model, b.reason))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    tail
}

/// GGUF-size fallback factor kept from v3.3 (DESIGN Step 4 rule 1).
const GGUF_FACTOR: f64 = 1.15;
/// Flat GiB added on top of the GGUF-size estimate (v3.3 formula).
const GGUF_OVERHEAD: f64 = 2.0;

/// Admission cost of a load: declared `vram_gb`, else GGUF size x 1.15 + 2, else
/// [`AdmissionError::UnderSpecified`] (Step 4 rule 1).
///
/// # Errors
///
/// - [`AdmissionError::UnderSpecified`] when both inputs are `None`; the refusal names
///   the reason instead of blaming a neighbor (F-7 lesson).
pub fn need_gb(
    declared: Option<Gibibytes>,
    gguf_size: Option<Gibibytes>,
) -> Result<Gibibytes, AdmissionError> {
    if let Some(d) = declared {
        return Ok(d);
    }
    gguf_size
        .map(|s| Gibibytes(s.0 * GGUF_FACTOR + GGUF_OVERHEAD))
        .ok_or(AdmissionError::UnderSpecified)
}

/// Budget remaining for a new load: total, minus the reserve band, minus what is
/// already committed to the card.
///
/// `occupied` is the physical reading (`used_gib`) whenever the witness can be
/// read — the stopper is observed VRAM, and it accounts for foreign tenants,
/// under-booked greedy engines and our own pending releases alike. The ledger
/// figure is passed only by a `ledger_only` witness (tests, machines without a
/// GPU), where nothing else exists.
///
/// The result may be negative — that is information (over-commit), not an error;
/// the gate compares it against the need and refuses with both numbers.
pub fn free_gb(total: Gibibytes, reserve: Gibibytes, occupied: Gibibytes) -> Gibibytes {
    Gibibytes(total.0 - reserve.0 - occupied.0)
}

/// Whether the per-process figures on a sample are usable for attribution (R1).
///
/// A pure, witness-local fact: `PerProcess` iff at least one entry reported a
/// concrete non-zero byte count; `Unavailable` covers the driver returning no list,
/// an empty list, or all-zero `UsedGpuMemory` (the WDDM degradation seen box-wide).
/// An **idle** card is `Unavailable` deliberately — attribution asks "could per-process
/// bytes be obtained", never "is anything running" (idleness is `used_gib`'s question).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributionQuality {
    /// At least one compute process reported a concrete `used > 0`.
    PerProcess,
    /// No usable per-process byte figure came back.
    Unavailable,
}

impl AttributionQuality {
    /// The payload token for `/har/status` (R1): the witness's own answer to "could
    /// per-process bytes be obtained", spelled for the page and the operator.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AttributionQuality::PerProcess => "per_process",
            AttributionQuality::Unavailable => "unavailable",
        }
    }
}

/// The card-global attribution quality for a per-process list.
#[must_use]
pub fn attribution_from_processes(processes: &[(u32, f64)]) -> AttributionQuality {
    if processes.iter().any(|(_, used)| *used > 0.0) {
        AttributionQuality::PerProcess
    } else {
        AttributionQuality::Unavailable
    }
}

/// One witness observation of the card (Step 4 rules 2-4).
#[derive(Debug, Clone, PartialEq)]
pub struct MemorySample {
    /// Total VRAM in GiB.
    pub total_gib: f64,
    /// Used VRAM in GiB (all processes).
    pub used_gib: f64,
    /// Per-process usage in GiB: `(pid, used)`.
    pub processes: Vec<(u32, f64)>,
    /// Whether `processes` can be trusted to attribute a footprint (R1).
    pub attribution: AttributionQuality,
}

impl MemorySample {
    /// Shared per-process aggregation: count and sum of entries whose pid is
    /// in (`include` true) or out of (false) `pids`.
    fn attributed(&self, pids: &[u32], include: bool) -> (usize, f64) {
        self.processes
            .iter()
            .filter(|(pid, _)| pids.contains(pid) == include)
            .map(|(_, used)| *used)
            .fold((0usize, 0f64), |(n, s), used| (n + 1, s + used))
    }

    /// Sum of ALL per-process figures (display: what NVML accounted in total).
    /// Foreign holders need no separate subtraction: the physical `used_gib` already
    /// includes them, so the budget would double-count them (and never pass an empty
    /// list to `attributed` meaning "all" — this accessor exists for that reason).
    pub fn process_total_gib(&self) -> f64 {
        self.attributed(&[], false).1
    }

    /// Footprint attributable to `own_pids` — the harness-measured engine usage.
    /// `None` when per-process attribution is unavailable (no entries, or all
    /// zeros under WDDM degradation): absence is reported, never a fake zero.
    pub fn own_gib(&self, own_pids: &[u32]) -> Option<f64> {
        let (count, total) = self.attributed(own_pids, true);
        (count > 0 && total > 0.0).then_some(total)
    }

    /// Physical free space the card reports, never negative.
    #[must_use]
    pub fn free_gib(&self) -> f64 {
        (self.total_gib - self.used_gib).max(0.0)
    }

    /// Whether the card has physically freed to `target_gib` of free space, within
    /// tolerance (§0.3: the settle window is only legal after a release). The
    /// tolerance absorbs driver lag after a kill, so a load may spawn up to
    /// `witness_tolerance_gb` under the gate — accepted by the operator 2026-10-26
    /// for greedy launch floors as well as plain need.
    ///
    /// This replaced the ledger-era form (`used <= ledger + external + tolerance`).
    /// On a box where per-process attribution reports nothing, that form compared a
    /// 2.0 GiB tolerance against a ~3.3 GiB desktop floor and timed out every swap
    /// that had anything else resident — a phantom leak, never a real one.
    #[must_use]
    pub fn freed_for(&self, target_gib: f64, tolerance_gib: f64) -> bool {
        self.free_gib() + tolerance_gib >= target_gib
    }
}

/// The VRAM witness: NVML in-process when the driver library is present,
/// `nvidia-smi` as the fallback, and a caller-visible error when neither answers —
/// which the admission gate treats as a **refusal** ([`AdmissionError::
/// WitnessUnavailable`]), never a stale guess or a silent ledger fallback
/// (F-3/F-7).
#[async_trait::async_trait]
pub trait VramWitness: Send + Sync {
    /// Samples the card. Implementations may cache briefly (status polling must
    /// not spawn a process storm); errors mean "unavailable", not zero.
    async fn sample(&self) -> Result<MemorySample, String>;

    /// Whose number it was: the label the page prints beside every figure, so an
    /// `nvidia-smi` fallback is never read as an in-process NVML reading (F-7 —
    /// never present a number without saying whose number it is).
    fn source(&self) -> String;

    /// Whether the ledger is the declared source of truth instead of the card.
    /// Only a witness that deliberately never answers sets this (tests, machines
    /// without a GPU); a real witness that fails is a refusal, not a fallback
    /// ([`AdmissionError::WitnessUnavailable`]).
    fn ledger_only(&self) -> bool {
        false
    }
}

/// The only witness for which the **ledger is authoritative**: it never answers, and
/// `ledger_only` tells the gate that this silence is deliberate (tests, a machine
/// without a GPU) rather than an outage to refuse on. Its settle window is skipped,
/// since there is nothing to measure.
pub struct LedgerOnlyWitness;

#[async_trait::async_trait]
impl VramWitness for LedgerOnlyWitness {
    fn source(&self) -> String {
        "ledger-only".to_string()
    }

    fn ledger_only(&self) -> bool {
        true
    }

    async fn sample(&self) -> Result<MemorySample, String> {
        Err("witness unavailable (ledger-only mode)".to_string())
    }
}

/// `nvidia-smi --query-gpu=...` CSV output -> sample (pure; unit-tested).
pub fn parse_smi_sample(gpu_csv: &str, apps_csv: &str) -> Result<MemorySample, String> {
    let parse_gib = |column: usize, line: &str| -> Result<f64, String> {
        let mib: f64 = line
            .split(',')
            .nth(column)
            .ok_or_else(|| format!("short --query-gpu line: {line:?}"))?
            .trim()
            .parse()
            .map_err(|_| format!("not a number in --query-gpu line: {line:?}"))?;
        // R2-01-03: `parse::<f64>` accepts "NaN"/"inf"/"-inf"; a half-wedged
        // nvidia-smi printing garbage must not hand it through as a figure.
        // Rejecting here fails the whole sample, which the gate treats as a
        // refusal (F-3/F-7) instead of resting on the downstream guards.
        if !mib.is_finite() {
            return Err(format!("not a finite number in --query-gpu line: {line:?}"));
        }
        Ok(mib / 1024.0)
    };
    let first = gpu_csv.lines().next().ok_or("empty --query-gpu output")?;
    let total_gib = parse_gib(0, first)?;
    let used_gib = parse_gib(1, first)?;
    let mut processes = Vec::new();
    for line in apps_csv.lines() {
        let Some((pid, used)) = line.split_once(',') else {
            continue;
        };
        let pid: u32 = match pid.trim().parse() {
            Ok(pid) => pid,
            Err(_) => continue,
        };
        let used: f64 = match used.trim().trim_end_matches(" MiB").parse::<f64>() {
            // R2-01-03: a garbage (`NaN`/`inf`) process figure is dropped like
            // any other unreadable entry — the totals still gate, the entry is
            // never booked.
            Ok(mib) if mib.is_finite() => mib / 1024.0,
            _ => continue,
        };
        processes.push((pid, used));
    }
    let attribution = attribution_from_processes(&processes);
    Ok(MemorySample {
        total_gib,
        used_gib,
        processes,
        attribution,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn own_gib_reports_attribution_or_none() {
        let s = |procs: Vec<(u32, f64)>| {
            let attribution = attribution_from_processes(&procs);
            MemorySample {
                total_gib: 72.0,
                used_gib: 40.0,
                processes: procs,
                attribution,
            }
        };
        assert_eq!(
            s(vec![]).attribution,
            AttributionQuality::Unavailable,
            "no entries: nothing to attribute"
        );
        assert_eq!(
            s(vec![(7, 0.0), (8, 0.0)]).attribution,
            AttributionQuality::Unavailable,
            "all-zero WDDM is not a measurement"
        );
        assert_eq!(
            s(vec![(7, 30.0), (8, 10.0)]).attribution,
            AttributionQuality::PerProcess,
            "a concrete byte count is attributable"
        );
        assert_eq!(s(vec![]).own_gib(&[7]), None, "no attribution is not zero");
        assert_eq!(
            s(vec![(7, 0.0), (8, 0.0)]).own_gib(&[7]),
            None,
            "WDDM-degraded zeros are never a measurement"
        );
        assert_eq!(s(vec![(7, 30.0), (8, 10.0)]).own_gib(&[7]), Some(30.0));
        assert_eq!(s(vec![(7, 30.0), (8, 10.0)]).own_gib(&[7, 8]), Some(40.0));
    }

    #[test]
    fn declared_cost_wins_over_estimate() {
        let need = need_gb(Some(Gibibytes(26.0)), Some(Gibibytes(23.0))).expect("declared");
        assert!(close(need.0, 26.0));
    }

    #[test]
    fn fallback_formula_is_gguf_times_1_15_plus_2() {
        let need = need_gb(None, Some(Gibibytes(23.0))).expect("gguf present");
        assert!(close(need.0, 23.0 * 1.15 + 2.0));
    }

    #[test]
    fn unknown_cost_refuses_as_under_specified() {
        assert_eq!(need_gb(None, None), Err(AdmissionError::UnderSpecified));
    }

    #[test]
    fn launch_floor_and_exact_refusals_explain_themselves() {
        // G13/G6: every refusal carries its numbers and names its reason.
        let floor = AdmissionError::BelowLaunchFloor {
            min_free: Gibibytes(6.0),
            free: Gibibytes(4.0),
            reserve: Gibibytes(2.0),
            facts: Box::default(),
            victims_considered: Vec::new(),
            blockers: Vec::new(),
            no_releases_reach: false,
            wait_hint: WaitHint::default(),
        }
        .to_string();
        assert!(
            floor.contains("launch floor") && floor.contains('6') && floor.contains('4'),
            "{floor}"
        );
        assert!(
            !floor.contains("No releases allow"),
            "a plain floor refusal must not claim the unreachable-floor guard: {floor}"
        );
        assert!(AdmissionError::ExactUndeclared
            .to_string()
            .contains("exact"));
        let claimed = AdmissionError::PortClaimed {
            port: 8004,
            claim: "in `protected_ports` (the manual namespace)",
        }
        .to_string();
        assert!(
            claimed.contains("ninfer_internal_port")
                && claimed.contains("8004")
                && claimed.contains("manual namespace"),
            "the R10 refusal must name key, number, and namespace: {claimed}"
        );
    }

    /// Row-10 guard (operator ruling 2026-11-01): when NO release the harness may
    /// perform reaches the floor, the refusal names it — the operator's diagnosis is
    /// foreign residents, not a numeric shrug. The sentence rides only the guarded
    /// refusal (the `false` leg above pins its absence).
    #[test]
    fn the_unreachable_floor_guard_names_the_ruling_sentence() {
        let unreachable = AdmissionError::BelowLaunchFloor {
            min_free: Gibibytes(60.0),
            free: Gibibytes(1.5),
            reserve: Gibibytes(2.0),
            facts: Box::default(),
            victims_considered: Vec::new(),
            blockers: Vec::new(),
            no_releases_reach: true,
            wait_hint: WaitHint::default(),
        }
        .to_string();
        assert!(
            unreachable.contains("No releases allow for the VRAM floor")
                && unreachable.contains("Check foreign residents"),
            "{unreachable}"
        );
    }

    #[test]
    fn refusal_names_the_truth_that_clamped_the_budget() {
        // The 2026-10-26 review: a card holding 1.8 GiB was refused with
        // "free 69.7 GiB" because the message printed the ledger figure.
        let clamped = AdmissionError::Insufficient {
            need: Gibibytes(34.0),
            free: Gibibytes(1.8),
            reserve: Gibibytes(2.0),
            facts: Box::new(BudgetFacts {
                budget: Some(Gibibytes(69.7)),
                observed_free: Some(Gibibytes(1.8)),
                ..Default::default()
            }),
            victims_considered: Vec::new(),
            blockers: Vec::new(),
            wait_hint: WaitHint::default(),
        }
        .to_string();
        assert!(clamped.contains("free 1.8 GiB"), "{clamped}");
        assert!(clamped.contains("budget 69.7 GiB"), "{clamped}");
        assert!(!clamped.contains("observed free"), "{clamped}");
        // Full agreement stays one clean sentence, no parenthetical.
        let plain = AdmissionError::Insufficient {
            need: Gibibytes(34.0),
            free: Gibibytes(30.0),
            reserve: Gibibytes(2.0),
            facts: Box::new(BudgetFacts {
                budget: Some(Gibibytes(30.0)),
                observed_free: Some(Gibibytes(30.0)),
                ..Default::default()
            }),
            victims_considered: Vec::new(),
            blockers: Vec::new(),
            wait_hint: WaitHint::default(),
        }
        .to_string();
        assert!(!plain.contains('('), "{plain}");
    }

    #[test]
    fn free_gb_subtracts_reserve_and_occupancy() {
        // The budget arithmetic the gate compares against. Negative is information
        // (over-commit already), not an error — the gate refuses with both numbers.
        assert!(close(
            free_gb(Gibibytes(72.0), Gibibytes(2.0), Gibibytes(30.0)).0,
            40.0
        ));
        assert!(close(
            free_gb(Gibibytes(72.0), Gibibytes(2.0), Gibibytes(80.0)).0,
            -10.0
        ));
    }

    /// R3: the refusal names the memory the operator cannot release, beside the
    /// physical reading and the ledger, so the dead end is diagnosable from the
    /// message alone. A zero or absent figure stays a clean sentence (the
    /// `plain`-refusal rule above).
    #[test]
    fn refusal_names_the_unreleasable_share_of_the_card() {
        let foreign_held = AdmissionError::Insufficient {
            need: Gibibytes(40.0),
            free: Gibibytes(30.0),
            reserve: Gibibytes(2.0),
            facts: Box::new(BudgetFacts {
                budget: Some(Gibibytes(30.0)),
                observed_free: Some(Gibibytes(30.0)),
                unreleasable: Some(Gibibytes(18.4)),
                used_gib: Some(Gibibytes(40.0)),
                booked_total: Some(Gibibytes(21.6)),
            }),
            victims_considered: Vec::new(),
            blockers: Vec::new(),
            wait_hint: WaitHint::default(),
        }
        .to_string();
        assert!(
            foreign_held.contains("18.4 GiB unreleasable (card holds 40.0 GiB, booked 21.6 GiB)"),
            "{foreign_held}"
        );
        // A quiet card must not grow a tail about unreleasable memory: no
        // figure, and a zero figure, both stay a clean sentence.
        let quiet = AdmissionError::Insufficient {
            need: Gibibytes(40.0),
            free: Gibibytes(30.0),
            reserve: Gibibytes(2.0),
            facts: Box::default(),
            victims_considered: Vec::new(),
            blockers: Vec::new(),
            wait_hint: WaitHint::default(),
        }
        .to_string();
        assert!(!quiet.contains("unreleasable"), "{quiet}");
        let zero = BudgetFacts {
            unreleasable: Some(Gibibytes(0.0)),
            ..Default::default()
        }
        .detail(Gibibytes(30.0));
        assert!(zero.is_empty(), "{zero}");
    }

    /// R3 ruling 10: the fold grades the window by how far UNACCOUNTED memory
    /// moved, not by how much sits there: a static foreign holder is already
    /// discounted by the baseline, and refusing those would blank every honest
    /// co-serve fold. The clamp legs matter: shrinking foreign memory grades
    /// quiescent, and foreign movement beyond the engine's own growth is a
    /// foreign story whatever the share.
    #[test]
    fn occupancy_grades_foreign_movement_across_the_window() {
        assert_eq!(
            grade_card_occupancy(0.1, 10.0, 0.3),
            CardOccupancy::Quiescent,
            "unaccounted memory that did not move keeps the fold trusted"
        );
        assert_eq!(
            grade_card_occupancy(-5.0, 10.0, 0.3),
            CardOccupancy::Quiescent,
            "shrinking foreign memory moves nothing the fold could blame itself"
        );
        assert_eq!(
            grade_card_occupancy(10.0, 20.0, 0.3),
            CardOccupancy::OwnDominated,
            "10 GiB foreign movement against 20 own growth is still own-explainable"
        );
        assert_eq!(
            grade_card_occupancy(30.0, 20.0, 0.3),
            CardOccupancy::ForeignDominated,
            "30 GiB moved that the engine's own growth cannot explain"
        );
        assert_eq!(
            grade_card_occupancy(1.0, 0.0, 0.3),
            CardOccupancy::ForeignDominated,
            "movement with nothing of its own to blame it on is foreign"
        );
    }

    /// R2-01-03: Rust's `parse::<f64>` succeeds on "NaN"/"inf"/"-inf"/"infinity"
    /// (documented behaviour), so a half-wedged `nvidia-smi` printing garbage in
    /// a memory column used to produce a "valid" `MemorySample` whose honesty
    /// rested entirely on the downstream guards. The gpu columns now reject it at
    /// the source — the sample fails, so the gate refuses (fail-closed) rather
    /// than comparing garbage. The guards stay as defence-in-depth.
    #[test]
    fn smi_parser_rejects_non_finite_gpu_figures() {
        for garbage in ["NaN", "inf", "-inf", "infinity"] {
            assert!(
                parse_smi_sample(&format!("{garbage}, 4096"), "").is_err(),
                "total {garbage:?} is driver garbage, not a figure"
            );
            assert!(
                parse_smi_sample(&format!("73728, {garbage}"), "").is_err(),
                "used {garbage:?} is driver garbage, not a figure"
            );
        }
        // The finite pair still parses (rejection is the garbage, not the rule).
        let s = parse_smi_sample("73728, 30720", "").expect("finite pair parses");
        assert!(close(s.total_gib, 72.0) && close(s.used_gib, 30.0));
    }

    /// Same rejection for the `--query-compute-apps` column: a garbage process
    /// figure is dropped like any other unreadable entry (the totals still gate),
    /// and all-garbage attribution reports absence, never a fake entry.
    #[test]
    fn smi_parser_drops_non_finite_process_figures() {
        for garbage in ["NaN", "inf", "-inf"] {
            let s = parse_smi_sample("73728, 30720", &format!("42, {garbage}\n7, 1024"))
                .expect("the totals are finite");
            assert_eq!(
                s.processes,
                vec![(7, 1.0)],
                "the {garbage:?} entry must be dropped, not booked"
            );
        }
        let s = parse_smi_sample("73728, 30720", "42, NaN").expect("the totals are finite");
        assert!(s.processes.is_empty(), "no readable entry survives");
        assert_eq!(s.attribution, AttributionQuality::Unavailable);
    }
}
