//! R11 fit-probe cluster: the warm-pass deferral and session-slack constants, the
//! cached projection, the row and pass plans, and the two off-actor probe legs.
//!
//! Data and probe legs only: this module never touches actor state. The actor owns
//! the cache, the epoch, and the stamps; the legs probe off-thread and answer back
//! through the actor channel.

use crate::engine::EngineRuntime;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use super::ActorMsg;

/// R11: the deferral before a warm fit pass starts probing, so the queue never
/// competes with the boot window (first bind, autostart loads). Long enough to
/// stay clear of boot, short enough that the figures are warm before an
/// operator's first click.
pub(super) const FIT_WARM_DELAY: Duration = Duration::from_millis(1500);

/// R11 session cross-check: the slack added to `fit_margin_gb` to form the
/// tolerance a live witness reading must fall inside to confirm a Fit booking
/// for the page. A witness total-diff carries noise the deterministic
/// projection does not (other tenants' sampling instants, KV growth under
/// load), so the cross-check window is wider than the booking margin.
///
/// Calibration decision (TASK-R11-FIT, C1 capture 2026-11-02): margin 3,
/// surcharge 2 and this 2.0 slack STAND - the capture booked conservative
/// against the live witness figure on the single sample the card holds, and
/// any numeric change needs multi-sample card-window evidence first. The C1
/// fact set (rows, constants, figures, headroom) lives in exactly one home:
/// the calibration pin in `src/engine/fit.rs` - cite it, do not restate it.
/// A config knob (`engines.llama.fit_validate_slack_gib`) is DEFERRED: a
/// fifth `engines.llama` key owes the llama row's `config_knobs` claim and
/// the parity-test literal in `src/engine/policy.rs`.
pub(super) const FIT_VALIDATE_SLACK_GIB: f64 = 2.0;

/// One cached R11 fit projection: the booked GiB and the model file's
/// fingerprint at the moment it was computed. The cache is keyed by the probe
/// argv hash (the void identity — a config change is a different key), and a
/// figure survives on a row only while that fingerprint is unproven-changed:
/// both sides known and differing voids it (absence of evidence is never
/// evidence of change, the KEEP doctrine). In-process only — this figure never
/// persists, so a restart re-derives it.
#[derive(Debug, Clone)]
pub(super) struct FitEntry {
    pub(super) gib: f64,
    pub(super) fingerprint: Option<crate::model::FileFingerprint>,
}

/// One row a fit pass projects: the argv identity, the row it belongs to, and
/// what the booking formula needs that argv does not carry (the vision
/// surcharge flag, the model file to fingerprint).
#[derive(Debug, Clone)]
pub(super) struct FitRowPlan {
    pub(super) model: String,
    pub(super) key: u64,
    pub(super) args: Vec<String>,
    pub(super) has_mmproj: bool,
    pub(super) model_file: Option<PathBuf>,
}

/// A whole fit pass resolved once off the actor: the config-sibling tool, the
/// calibration constants, the rows to project.
#[derive(Debug, Clone)]
pub(super) struct FitProbePlan {
    pub(super) program: PathBuf,
    pub(super) constants: crate::engine::fit::FitConstants,
    pub(super) rows: Vec<FitRowPlan>,
}

/// The version leg shared by the warm pass and the admit refresh: report what
/// the calculator says it is, then answer whether rows may run at all — no
/// answer is silence for the whole pass (a tool that will not say its version
/// will not project either; one warn, not a probe per doomed row). A CHANGED
/// answer does not stop the pass: the version message lands before every row
/// message, so rows computed under the new tool are stamped under the new
/// epoch the handler just minted.
pub(super) async fn run_fit_version_leg(
    runtime: &Arc<dyn EngineRuntime>,
    tx: &mpsc::UnboundedSender<ActorMsg>,
    gen: u64,
    plan: &FitProbePlan,
) -> bool {
    // Absence is checked and NAMED here (off the actor, immediately before the
    // first probe), never guessed past: a missing calculator is one warn and
    // the whole ladder stands — no version message, no rows, no silence about
    // why. Async stat: no synchronous fs on the worker either, the
    // FINDINGS2-04 [2] discipline this whole file keeps for its actor turn.
    let tool_present = match tokio::fs::try_exists(&plan.program).await {
        Ok(present) => present,
        Err(error) => {
            tracing::warn!(
                path = %plan.program.display(),
                error = %error,
                "cannot stat the fit tool; the fit pass stays silent and the ladder stands"
            );
            false
        }
    };
    if !tool_present {
        tracing::warn!(
            error = %crate::engine::fit::FitError::ToolMissing {
                path: plan.program.display().to_string(),
            },
            "fit pass skipped"
        );
        return false;
    }
    let answer = runtime
        .run_probe_command(
            &plan.program,
            &crate::engine::fit::version_args(),
            plan.constants.probe_timeout,
        )
        .await;
    let established = answer.is_some();
    let _ = tx.send(ActorMsg::FitVersionDone {
        gen,
        version: answer,
    });
    established
}

/// One row leg: probe, strict-parse, book — a failure answers as the named
/// `Err` the handler warns, never as a number. The fingerprint is taken AFTER
/// the probe answered, so the figure and the file it describes are certified
/// against the same reading (a rewrite during the probe shows as proven-change
/// on the NEXT check, the safe direction).
pub(super) async fn run_fit_row_leg(
    runtime: &Arc<dyn EngineRuntime>,
    plan: &FitProbePlan,
    row: &FitRowPlan,
) -> Result<(f64, Option<crate::model::FileFingerprint>), String> {
    match runtime
        .run_probe_command(&plan.program, &row.args, plan.constants.probe_timeout)
        .await
    {
        Some(stdout) => {
            crate::engine::fit::parse_and_book(&stdout, row.has_mmproj, &plan.constants)
                .map(|gib| {
                    (
                        gib,
                        row.model_file
                            .as_deref()
                            .and_then(crate::model::fingerprint),
                    )
                })
                .map_err(|error| error.to_string())
        }
        None => Err("the probe did not answer".to_string()),
    }
}
