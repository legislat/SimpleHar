//! VRAM admission arithmetic (DESIGN Step 4, choice D — ledger authoritative, NVML a
//! reconciling witness).
//!
//! This module owns the need formula and the budget arithmetic. The NVML witness and
//! the settle-only-after-release gate land with the scheduler milestone; item 5 is
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
    /// The gate rejected the load and no sufficient eviction set exists; the
    /// numbers, the candidates considered, and the named blockers all travel with
    /// the refusal (Step 6 contract, G6).
    #[error("insufficient VRAM: need {need}, free {free}, reserve {reserve}{tail}", tail = insufficient_tail(.victims_considered, .blockers))]
    Insufficient {
        /// Declared or estimated cost of the wanted load.
        need: Gibibytes,
        /// Budget remaining at decision time.
        free: Gibibytes,
        /// Reserve subtracted before the gate.
        reserve: Gibibytes,
        /// Candidates the planner considered (may be empty when none exist).
        victims_considered: Vec<VictimInfo>,
        /// Models that cannot be evicted, with reasons.
        blockers: Vec<BlockerInfo>,
    },
    /// The model's launch floor was not met: actual free VRAM (externals counted)
    /// at decision time sits below `launch_only_with_at_least_vram_free`, so the
    /// load is refused BEFORE spawn — launching a greedy-offload quant onto a
    /// busy card thrashes RAM/SSD for minutes (operator design note 2026-10-26).
    #[error("launch floor not met: at least {min_free} free required, free {free}, reserve {reserve}{tail}", tail = insufficient_tail(.victims_considered, .blockers))]
    BelowLaunchFloor {
        /// Configured floor in GiB (model key, or the greedy global default).
        min_free: Gibibytes,
        /// Budget remaining at decision time.
        free: Gibibytes,
        /// Reserve subtracted before the gate.
        reserve: Gibibytes,
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

/// Budget remaining for a new load (Step 4 rule 2): total, minus reserve, minus the
/// ledger of live occupants, minus observed external reservations.
///
/// The result may be negative — that is information (over-commit), not an error; the
/// gate compares it against the need and refuses with both numbers.
pub fn free_gb(
    total: Gibibytes,
    reserve: Gibibytes,
    ledger_sum: Gibibytes,
    external_reserved: Gibibytes,
) -> Gibibytes {
    Gibibytes(total.0 - reserve.0 - ledger_sum.0 - external_reserved.0)
}

/// Decides a load against the budget.
///
/// # Errors
///
/// - [`AdmissionError::UnderSpecified`] / [`AdmissionError::Insufficient`] as above.
pub fn admit(
    need: Gibibytes,
    total: Gibibytes,
    reserve: Gibibytes,
    ledger_sum: Gibibytes,
    external_reserved: Gibibytes,
) -> Result<(), AdmissionError> {
    let free = free_gb(total, reserve, ledger_sum, external_reserved);
    if need.0 <= free.0 {
        Ok(())
    } else {
        Err(AdmissionError::Insufficient {
            need,
            free,
            reserve,
            victims_considered: Vec::new(),
            blockers: Vec::new(),
        })
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
}

impl MemorySample {
    /// Usage not attributable to `own_pids` — the external reservation the budget
    /// must subtract (Step 4 rule 2).
    pub fn external_gib(&self, own_pids: &[u32]) -> f64 {
        self.processes
            .iter()
            .filter(|(pid, _)| !own_pids.contains(pid))
            .map(|(_, used)| *used)
            .sum()
    }

    /// Whether memory settled to `baseline + tolerance` after a release (§0.3: the
    /// settle window is only legal after a release).
    pub fn settled(&self, baseline_gib: f64, tolerance_gib: f64) -> bool {
        self.used_gib <= baseline_gib + tolerance_gib
    }
}

/// The VRAM witness: NVML in-process when the driver library is present,
/// `nvidia-smi` as the fallback, and a caller-visible error when neither answers
/// (the admission gate then runs ledger-only, logged loudly — never a stale guess).
#[async_trait::async_trait]
pub trait VramWitness: Send + Sync {
    /// Samples the card. Implementations may cache briefly (status polling must
    /// not spawn a process storm); errors mean "unavailable", not zero.
    async fn sample(&self) -> Result<MemorySample, String>;
}

/// A witness that never answers: admission runs ledger-only and settle is skipped
/// (tests and machines without a GPU — the actor treats Err as "unavailable").
pub struct LedgerOnlyWitness;

#[async_trait::async_trait]
impl VramWitness for LedgerOnlyWitness {
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
    fn accepted_never_exceeds_budget_and_refusal_carries_numbers() {
        let total = Gibibytes(72.0);
        let reserve = Gibibytes(2.0);
        // 26 need, 30 already on the ledger, 0 external -> 40 free: accept.
        assert!(admit(
            Gibibytes(26.0),
            total,
            reserve,
            Gibibytes(30.0),
            Gibibytes(0.0)
        )
        .is_ok());
        // 50 need against the same state: refuse with need/free/reserve in the error.
        let err = admit(
            Gibibytes(50.0),
            total,
            reserve,
            Gibibytes(30.0),
            Gibibytes(0.0),
        )
        .expect_err("must refuse");
        match err {
            AdmissionError::Insufficient {
                need,
                free,
                reserve,
                ..
            } => {
                assert!(close(need.0, 50.0));
                assert!(close(free.0, 40.0));
                assert!(close(reserve.0, 2.0));
            }
            other => panic!("expected Insufficient, got {other:?}"),
        }
    }
}
