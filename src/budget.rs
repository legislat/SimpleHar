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

/// Refusal reasons of the admission gate, with the numbers used (G13).
#[derive(Debug, Clone, PartialEq, Error)]
pub enum AdmissionError {
    /// Neither `vram_gb` nor a GGUF size is known; the message says so explicitly
    /// (v3.3's F-7 lied about the reason).
    #[error("under-specified model: no `vram_gb` and no GGUF size to estimate from")]
    UnderSpecified,
    /// The gate rejected the load; the numbers travel with the refusal (Step 6 contract).
    #[error("insufficient VRAM: need {need}, free {free}, reserve {reserve}")]
    Insufficient {
        /// Declared or estimated cost of the wanted load.
        need: Gibibytes,
        /// Budget remaining at decision time.
        free: Gibibytes,
        /// Reserve subtracted before the gate.
        reserve: Gibibytes,
    },
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
        })
    }
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
            } => {
                assert!(close(need.0, 50.0));
                assert!(close(free.0, 40.0));
                assert!(close(reserve.0, 2.0));
            }
            other => panic!("expected Insufficient, got {other:?}"),
        }
    }
}
