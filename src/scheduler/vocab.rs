//! Scheduler vocabulary: commands, job ids, and the swap state machine graph
//! (DESIGN Steps 2-3).

use crate::config::CategoryId;
use serde::Serialize;

/// Identity of one swap job, observable at `GET /har/jobs` (Step 2/12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct JobId(pub u64);

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "job-{}", self.0)
    }
}

/// Commands into the scheduler actor (Step 2). A dropped waiter (client disconnect)
/// drops the oneshot, never the in-flight work — that is the F-1 fix.
///
/// Register/unregister (externals display list) are v0-deferred (§0.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Load `model` into `category`; same-slot policy is supersede, queue is FIFO
    /// otherwise (Step 2).
    Swap { model: String, category: CategoryId },
    /// Cooperative cancel: observed inside the current step, then cleanup completes
    /// (Step 3 — never a dropped task).
    Cancel { job: JobId },
    /// Unload every occupant of a category.
    Unload { category: CategoryId },
    /// Re-scan `ModelConf` (hot reload, Step 9).
    Reload,
}

/// Swap lifecycle states (Step 3, LOCKED graph).
///
/// `Dispose -> Spawn` is a *confirmation gate*, not a delay: the edge is legal only
/// once release is proven (G12); the spawn fires immediately on that event. A release
/// that cannot be proven inside `release_timeout_s` takes `Failed -> Reap` and refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum SwapState {
    /// No job on the slot.
    Idle,
    /// Budget gate against the ledger (Step 4).
    Admit,
    /// Admission refused; terminal until the slot returns to `Idle`.
    AdmitFail,
    /// Victims selected and being released, one at a time with re-settle (Step 6).
    Quiesce,
    /// Waiting for release confirmation of the previous occupant (§0.3).
    Dispose,
    /// Engine process/container starting.
    Spawn,
    /// Readiness probing (`Ready | NotYet | Failed`, Step 7).
    Ready,
    /// Installing the occupant into the slot table.
    Install,
    /// Occupant installed and serving; terminal-success for the job.
    Serving,
    /// Any step failed; cleanup pending. Every exit sets exactly one of
    /// {slot installed, last_error + reaped} — never neither (F-6 funnel).
    Failed,
    /// Killing what this job spawned; then `Idle`.
    Reap,
    /// Cancel observed; finish the current step, then `Reap` (Step 3).
    CancelRequested,
}

/// Whether the `from -> to` edge exists in the Step 3 graph.
///
/// Rules encoded:
/// - the happy path `Idle -> Admit -> Quiesce -> Dispose -> Spawn -> Ready -> Install
///   -> Serving` with no skips (a swap may never jump straight to `Spawn` — that is
///   how double spawns happened, F-6/F-3);
/// - every working step may fail into `Failed` (the exit funnel);
/// - any non-idle state may observe `CancelRequested`, and cancel always ends in
///   `Reap`, never in `Spawn`;
/// - `AdmitFail`, `Failed`, and `Reap` return the slot to `Idle`.
pub fn transition_allowed(from: SwapState, to: SwapState) -> bool {
    use SwapState::*;
    let happy_or_fail = matches!(
        (from, to),
        (Idle, Admit)
            | (Admit, Quiesce)
            | (Admit, AdmitFail)
            | (AdmitFail, Idle)
            | (Quiesce, Dispose)
            | (Quiesce, Failed)
            | (Dispose, Spawn)
            | (Dispose, Failed)
            | (Spawn, Ready)
            | (Spawn, Failed)
            | (Ready, Install)
            | (Ready, Failed)
            | (Install, Serving)
            | (Install, Failed)
            | (Failed, Reap)
            | (Reap, Idle)
            | (CancelRequested, Reap)
            // Unload jobs (2026-10-25 amendment): a deliberate unload ends with its
            // release confirmed in Dispose and teardown complete in Reap — the one
            // edge the swap graph shares with unload.
            | (Dispose, Reap)
    );
    let cancel =
        to == CancelRequested && !matches!(from, Idle | CancelRequested | Reap | AdmitFail);
    happy_or_fail || cancel
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_walks_every_edge_in_order() {
        use SwapState::*;
        let path = [
            Idle, Admit, Quiesce, Dispose, Spawn, Ready, Install, Serving,
        ];
        for pair in path.windows(2) {
            assert!(
                transition_allowed(pair[0], pair[1]),
                "edge {:?} -> {:?} must be legal",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn no_skip_to_spawn_or_serving() {
        use SwapState::*;
        // Skipping quiesce/dispose is how double spawns happened (F-6, F-3).
        assert!(!transition_allowed(Idle, Spawn));
        assert!(!transition_allowed(Admit, Spawn));
        assert!(!transition_allowed(Idle, Serving));
        assert!(!transition_allowed(Ready, Serving));
    }

    #[test]
    fn every_working_step_may_fail_into_the_funnel() {
        use SwapState::*;
        // Admission refuses have their own terminal (AdmitFail), not Failed.
        assert!(transition_allowed(Admit, AdmitFail));
        for from in [Quiesce, Dispose, Spawn, Ready, Install] {
            assert!(
                transition_allowed(from, Failed),
                "{from:?} -> Failed must be legal"
            );
            assert!(transition_allowed(Failed, Reap));
            assert!(transition_allowed(Reap, Idle));
        }
    }

    #[test]
    fn cancel_is_observable_from_any_active_state_and_never_spawns() {
        use SwapState::*;
        for from in [Admit, Quiesce, Dispose, Spawn, Ready, Install, Serving] {
            assert!(
                transition_allowed(from, CancelRequested),
                "{from:?} cancel must be legal"
            );
        }
        assert!(!transition_allowed(Idle, CancelRequested));
        assert!(!transition_allowed(CancelRequested, Spawn));
        assert!(transition_allowed(CancelRequested, Reap));
    }

    #[test]
    fn admission_refusal_is_a_terminal_of_its_own() {
        use SwapState::*;
        assert!(transition_allowed(Admit, AdmitFail));
        assert!(transition_allowed(AdmitFail, Idle));
        assert!(!transition_allowed(AdmitFail, Spawn));
    }

    #[test]
    fn job_ids_display_stably() {
        assert_eq!(JobId(7).to_string(), "job-7");
    }

    #[test]
    fn unload_completes_dispose_to_reap_to_idle() {
        use SwapState::*;
        assert!(transition_allowed(Dispose, Reap));
        assert!(transition_allowed(Reap, Idle));
    }
}
