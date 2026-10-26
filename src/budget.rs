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
        if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join(", "))
        }
    }
}

/// How far apart two GiB figures must be before the refusal bothers naming both.
const DISAGREE_GIB: f64 = 0.05;

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
        facts: BudgetFacts,
        /// Candidates the planner considered (may be empty when none exist).
        victims_considered: Vec<VictimInfo>,
        /// Models that cannot be evicted, with reasons.
        blockers: Vec<BlockerInfo>,
    },
    /// The model's launch floor was not met: the physical free VRAM seen at decision
    /// time sits below `launch_only_with_at_least_vram_free`, so the
    /// load is refused BEFORE spawn — launching a greedy-offload quant onto a
    /// busy card thrashes RAM/SSD for minutes (operator design note 2026-10-26).
    #[error("launch floor not met: at least {min_free} free required, free {free}, reserve {reserve}{detail}{tail}",
        detail = .facts.detail(*free),
        tail = insufficient_tail(.victims_considered, .blockers)
    )]
    BelowLaunchFloor {
        /// Configured floor in GiB (model key, or the greedy global default).
        min_free: Gibibytes,
        /// The figure the gate actually compared.
        free: Gibibytes,
        /// Reserve subtracted before the gate.
        reserve: Gibibytes,
        /// The inputs `free` was derived from (ledger / observed).
        facts: BudgetFacts,
        /// Candidates the planner considered (may be empty when none exist).
        victims_considered: Vec<VictimInfo>,
        /// Models that cannot be evicted, with reasons.
        blockers: Vec<BlockerInfo>,
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

/// One witness observation of the card (Step 4 rules 2-4).
#[derive(Debug, Clone, PartialEq)]
pub struct MemorySample {
    /// Total VRAM in GiB.
    pub total_gib: f64,
    /// Used VRAM in GiB (all processes).
    pub used_gib: f64,
    /// Per-process usage in GiB: `(pid, used)`.
    pub processes: Vec<(u32, f64)>,
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
            Ok(mib) => mib / 1024.0,
            Err(_) => continue,
        };
        processes.push((pid, used));
    }
    Ok(MemorySample {
        total_gib,
        used_gib,
        processes,
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
        let s = |procs: Vec<(u32, f64)>| MemorySample {
            total_gib: 72.0,
            used_gib: 40.0,
            processes: procs,
        };
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
            facts: BudgetFacts::default(),
            victims_considered: Vec::new(),
            blockers: Vec::new(),
        }
        .to_string();
        assert!(
            floor.contains("launch floor") && floor.contains('6') && floor.contains('4'),
            "{floor}"
        );
        assert!(AdmissionError::ExactUndeclared
            .to_string()
            .contains("exact"));
    }

    #[test]
    fn refusal_names_the_truth_that_clamped_the_budget() {
        // The 2026-10-26 review: a card holding 1.8 GiB was refused with
        // "free 69.7 GiB" because the message printed the ledger figure.
        let clamped = AdmissionError::Insufficient {
            need: Gibibytes(34.0),
            free: Gibibytes(1.8),
            reserve: Gibibytes(2.0),
            facts: BudgetFacts {
                budget: Some(Gibibytes(69.7)),
                observed_free: Some(Gibibytes(1.8)),
            },
            victims_considered: Vec::new(),
            blockers: Vec::new(),
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
            facts: BudgetFacts {
                budget: Some(Gibibytes(30.0)),
                observed_free: Some(Gibibytes(30.0)),
            },
            victims_considered: Vec::new(),
            blockers: Vec::new(),
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
}
