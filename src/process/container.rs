//! The container seam of process supervision: the WSL/docker command builders
//! and probes (`docker rm -f`, `docker port`, `docker ps`, `docker logs`), the
//! claims and custody ledger homes, the boot-reclaim and R5 stranded plans,
//! and the pure verdict cores the kill/release paths judge with. The verdict
//! functions (`table_verdict`, `reap_action`, `release_ready`) are the pure
//! decision cores; their behavior pins stay in the process test module.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::engine::wsl_prefix;
use crate::ownership::{boot_reap_decision, ContainerClaims, ReapDecision};

use super::control::{run_control_command, ControlOutcome};

#[cfg(test)]
use std::sync::{Arc, Mutex};

/// Defensive ceiling on the `docker logs` text we retain (bytes, tail). A second
/// belt behind docker's own `--tail` ([`CONTAINER_LOG_TAIL_LINES`]): pathological
/// multi-megabyte single lines still cannot blow up the harness. Bounded upstream by
/// `--log-opt max-file` for containers the harness itself started.
const CONTAINER_LOG_FETCH_CAP: usize = 8 * 1024 * 1024;
/// The line bound handed to `docker logs --tail`, so the daemon — not the harness's
/// memory — enforces the fetch bound (see [`container_logs`]). Comfortably above any
/// plausible post-startup chatter while the startup `MemorySummary` record (written
/// at startup, re-read only for a container this harness claimed) stays in view.
const CONTAINER_LOG_TAIL_LINES: usize = 20_000;

/// What the engine table says about a kill/release request for a port.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TableVerdict {
    /// No entry: we never owned anything on that port, so there is nothing to kill and
    /// nothing to confirm away.
    Unowned,
    /// An entry exists, but under a newer spawn: this ticket is stale and the child it
    /// named is already gone.
    Stale {
        /// The generation the table holds for that port.
        found: u64,
    },
    /// This ticket is the live owner of the port.
    Ours,
}

/// The one identity rule behind `kill` and `release_confirmed`: a port is a slot in the
/// port space, not a process, so ownership is `(port, generation)` — never the port
/// alone. A stale ticket must not be able to terminate, or "confirm the release" of, the
/// successor that inherited the port.
pub(super) fn table_verdict(held_generation: Option<u64>, ticket_generation: u64) -> TableVerdict {
    match held_generation {
        None => TableVerdict::Unowned,
        Some(found) if found != ticket_generation => TableVerdict::Stale { found },
        Some(_) => TableVerdict::Ours,
    }
}

/// Test seam (R2-08-01 pin): a two-way gate that parks a `release_confirmed`
/// poller in the await gap between reading the table verdict and re-judging it
/// under the final table lock — the window in which a concurrent same-ticket
/// poller plus the scheduler's Dispose->Spawn gate used to be able to install
/// a successor that the parked poller then removed unguarded. A pin holds one
/// poller at the gate, installs the successor, and releases it, making the
/// interleaving deterministic instead of timing-dependent. Inert in
/// production: no field value exists there, so no await is ever inserted.
#[cfg(test)]
#[derive(Default)]
pub(super) struct ReleaseGate {
    /// Notified by `release_confirmed` when it reaches the gap.
    pub(super) arrived: tokio::sync::Notify,
    /// Notified by the test to let the parked poller re-judge and finish.
    pub(super) departed: tokio::sync::Notify,
}

/// Recorded reap-verdict seam for the REV7 pins (test-only): one scripted
/// verdict per `kill`-carried container reap, plus the names it was asked to
/// reap. `kill` consumes the script front-first and falls through to the real
/// [`reap_container`] once exhausted, so a pin that scripts nothing keeps the
/// real (coordinate-refusing) path untouched. Parallel in spirit to
/// `docker_probe`, but narrower: it replaces only the WSL/docker call inside
/// the reap duty, so the duty rule, the claim gate, the generation guard, and
/// the entry recording under test stay the production ones (REV7-03 lets a
/// pin drive `kill` writing BOTH verdicts onto `reap_confirmed`).
#[cfg(test)]
#[derive(Default)]
pub(super) struct ReapSeam {
    /// One verdict per scripted reap, consumed front-first.
    pub(super) scripted: Mutex<std::collections::VecDeque<bool>>,
    /// The container name each scripted verdict answered for, in order. A
    /// passive `release_confirmed` poll must never extend this list (REV7-01).
    pub(super) asked: Mutex<Vec<String>>,
}

/// Recorded docker-probe seam for the R5 pins (test-only): a canned `docker ps`
/// answer plus the log of docker verbs it was asked to run.
#[cfg(test)]
#[derive(Clone)]
pub(super) struct DockerProbeSeam {
    /// `Some` = docker answered with these running names; `None` = the command
    /// could not run (the silent path).
    pub(super) outcome: Option<Vec<String>>,
    /// One entry per probe, in order. The reconcile reaching `docker rm` or any
    /// other verb would leave its mark here.
    pub(super) calls: Arc<Mutex<Vec<String>>>,
}

/// `wsl.exe -d <distro> -u <user> -- docker rm -f <name>` (v3.3 kill_container
/// parity): program and args as pure data, through the shared `wsl_prefix` so
/// launch and teardown can never address different WSL coordinates.
pub(super) fn container_rm(distro: &str, user: &str, name: &str) -> (String, Vec<String>) {
    let mut argv = wsl_prefix(distro, user);
    argv.extend([
        "docker".to_string(),
        "rm".to_string(),
        "-f".to_string(),
        name.to_string(),
    ]);
    let (program, rest) = argv
        .split_first()
        .unwrap_or_else(|| unreachable!("invariant: wsl_prefix is never empty"));
    (program.clone(), rest.to_vec())
}

/// `wsl.exe -d <distro> -u <user> -- docker port <name>`: the read-only
/// publish-evidence query of ruling 1(b), through the same [`wsl_prefix`] as
/// launch and teardown so the diagnosis can never address different WSL
/// coordinates than the spawn it explains.
pub(super) fn container_port(distro: &str, user: &str, name: &str) -> (String, Vec<String>) {
    let mut argv = wsl_prefix(distro, user);
    argv.extend(["docker".to_string(), "port".to_string(), name.to_string()]);
    let (program, rest) = argv
        .split_first()
        .unwrap_or_else(|| unreachable!("invariant: wsl_prefix is never empty"));
    (program.clone(), rest.to_vec())
}

/// Parse of `docker port` stdout for [`EngineRuntime::published_elsewhere`]
/// (ruling 1(b)): lines `<internal>/<proto> -> <bind>:<host-port>` (the IPv6
/// bind spells `[::]:18112`). `None` when the harness-offered port is among the
/// host mappings — the rendered script honored the `$3` contract, so there is
/// nothing to report — and `None` when no usable mapping is present: a diagnosis
/// may annotate a failure, never invent evidence from docker's error text.
pub(super) fn published_elsewhere_evidence(
    container: &str,
    offered: u16,
    text: &str,
) -> Option<String> {
    let mut others: Vec<String> = Vec::new();
    for line in text.lines() {
        let Some((internal, target)) = line.split_once("->") else {
            continue;
        };
        let target = target.trim();
        let Some((_, host_port)) = target.rsplit_once(':') else {
            continue;
        };
        let Ok(port) = host_port.trim().parse::<u16>() else {
            continue;
        };
        if port == offered {
            return None;
        }
        others.push(format!("{} -> {target}", internal.trim()));
    }
    if others.is_empty() {
        return None;
    }
    Some(format!(
        "published-port evidence: docker publishes {container} as {} \
         (not the harness-offered port {offered})",
        others.join(", ")
    ))
}

/// Whether this kill must reap a container. A CONFIRMED reap retires the duty
/// for good: a re-kill never re-runs `docker rm` for someone else's container
/// (review, 2026-10-26). An UNCONFIRMED reap keeps the duty alive (REV7-01):
/// the entry is still ours and the container may still hold the card, so the
/// next explicit kill carries the retry. Only `kill` reaches this rule --
/// `release_confirmed` polls are passive and never reap -- and every attempt
/// stays gated by the (port, generation) verdict and the claim ledger
/// ([`reap_action`]), so a re-arm can never address an unclaimed name.
pub(super) fn should_reap(
    terminated: bool,
    reap_confirmed: bool,
    container: Option<&String>,
) -> Option<String> {
    if terminated && reap_confirmed {
        return None;
    }
    container.cloned()
}

/// What the kill path may do about a detached container (FINDINGS2-0910 [3]).
///
/// Two independent facts decide it: the ticket says this kill carries the *duty*,
/// and the ledger says this harness *owns* the name. The kill used to reap straight
/// from the ticket's name without consulting the ledger, so any future adapter that
/// invented a container name (chunk 10's templates) would have had a reap authority
/// the boot reclaim rightly refuses. Pure, so the rule is testable without WSL.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ReapAction {
    /// The duty fell to this kill and the ledger holds the claim.
    Reap(String),
    /// The duty fell to this kill but no claim backs the name: leave the container.
    SkipUnclaimed(String),
    /// Nothing to do (no container, or a re-kill whose reap already confirmed).
    None,
}

pub(super) fn reap_action(duty: Option<String>, claimed: bool) -> ReapAction {
    match duty {
        Some(name) if claimed => ReapAction::Reap(name),
        Some(name) => ReapAction::SkipUnclaimed(name),
        None => ReapAction::None,
    }
}

/// G12 release evidence: process engines must show exit + closed port; a
/// detached container's client exit proves nothing — the closed port after
/// reaping is the evidence (v3.3 `Proc::Container` parity).
pub(super) fn release_ready(is_container: bool, exited: bool, port_closed: bool) -> bool {
    (exited || is_container) && port_closed
}

/// Bounded stderr excerpt for warn lines (WSL/docker startup errors are verbose;
/// the ring buffer and `/har/log` stay readable).
pub(super) fn excerpt(bytes: &[u8]) -> String {
    const CAP: usize = 300;
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= CAP {
        return text.into_owned();
    }
    let mut end = CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// Best-effort `docker rm -f` inside WSL; every failure path warns and lets the
/// caller proceed (the release poll is the arbiter). Built and run by
/// [`run_control_command`]: hidden, kill-on-drop, and scrubbed to the OS baseline so
/// the harness's `SIMPLEHARN_TOKEN` never rides into the operator's distro
/// (FINDINGS2-0910 [4]).
///
/// # Returns
///
/// `true` only when docker confirmed the removal — both ownership paths drop
/// the claim for a name only on that evidence: the boot reclaim, and (since
/// R2-08-02) the live release, whose `kill` records the verdict on the entry
/// and whose `release_confirmed` demands it, so a failed reap stays
/// reclaimable at the next boot instead of silently orphaning.
pub(super) async fn reap_container(distro: &str, user: &str, name: &str) -> bool {
    if user.trim().is_empty() || distro.trim().is_empty() {
        // Neither coordinate has a shipped default (`wsl_user` names a personal account);
        // never hand `wsl.exe` an empty `-d`/`-u` and call the result a reaping.
        tracing::warn!(
            container = name,
            distro = distro,
            user = user,
            "WSL coordinates missing from the config: refusing to address this container"
        );
        return false;
    }
    let (program, args) = container_rm(distro, user, name);
    match run_control_command(&program, &args).await {
        ControlOutcome::Ran(output) if output.status.success() => {
            tracing::info!(container = name, "container removed");
            true
        }
        ControlOutcome::Ran(output) => {
            tracing::warn!(
                container = name,
                stderr = %excerpt(&output.stderr),
                "docker rm -f reported failure"
            );
            false
        }
        ControlOutcome::Failed(error) => {
            tracing::warn!(container = name, error = %error, "docker rm -f cannot run");
            false
        }
        ControlOutcome::TimedOut => {
            tracing::warn!(container = name, "docker rm -f timed out");
            false
        }
    }
}

/// One catalog-declared container candidate for the boot reclaim: the declaring
/// model's `engine:` plus its `ninfer_container:` value. Both travel together so
/// [`boot_reap_decision`] can apply the engine gate and the name gate at boot,
/// while the claim gate reads the ledger the spawn path wrote (chunk 10 \[1]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerCandidate {
    /// The declaring model's `engine:` value.
    pub engine: String,
    /// The declared container name (shape-unvalidated until the decision says so).
    pub name: String,
}

/// The pure core of [`ProcessRuntime::reclaim_at_boot`]: what to destroy and what
/// to leave running with a named reason. Pure, so the ownership rule is testable
/// without WSL. A candidate is judged only when a running container already has
/// its exact name — presence is re-checked, never assumed.
pub(super) fn reclaim_plan(
    present: &[String],
    candidates: &[ContainerCandidate],
    claims: &ContainerClaims,
) -> Vec<(ContainerCandidate, ReapDecision)> {
    candidates
        .iter()
        .filter(|candidate| present.iter().any(|name| name == &candidate.name))
        .map(|candidate| {
            let decision = boot_reap_decision(
                &candidate.engine,
                &candidate.name,
                claims.contains(&candidate.name),
            );
            (candidate.clone(), decision)
        })
        .collect()
}

/// Home of the persisted container-claim ledger (chunk 10 [1]): beside the
/// engine logs, next to the exe. Spawn, release, and the boot reclaim all
/// address this one file through [`ContainerClaims`].
pub(super) fn container_claims_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("ninfer-claims.json")))
        .unwrap_or_else(|| PathBuf::from("ninfer-claims.json"))
}

/// The custody sidecar (slice 10b): recorded strata launches, beside the
/// claim ledger the harness already keeps next to its executable — the same
/// state home for the same reason: one microsecond of local read at boot,
/// never a stall on a volume that could delay the bind.
pub(super) fn strata_custody_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("strata-custody.json")))
        .unwrap_or_else(|| PathBuf::from("strata-custody.json"))
}

/// `wsl.exe -d <distro> -u <user> -- docker ps --format {{.Names}}` as pure data:
/// the one bounded command every container reconcile lists running names with,
/// through the shared [`wsl_prefix`] so the two reconciles can never address
/// docker with different commands.
pub(super) fn container_ps_command(distro: &str, user: &str) -> (String, Vec<String>) {
    let mut argv = wsl_prefix(distro, user);
    argv.extend([
        "docker".to_string(),
        "ps".to_string(),
        "--format".to_string(),
        "{{.Names}}".to_string(),
    ]);
    let (program, rest) = argv
        .split_first()
        .unwrap_or_else(|| unreachable!("invariant: wsl_prefix is never empty"));
    (program.clone(), rest.to_vec())
}

/// One `docker ps` stdout into its container names (the parse shared by both
/// reconciles, so neither can invent a name the other would not see).
fn ps_names(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// `wsl.exe … docker ps --format {{.Names}}`; `None` when the call cannot run or
/// reports failure (the reconcile is advisory, never fatal). A control command like
/// [`reap_container`], so it goes through [`run_control_command`] and carries only
/// the OS baseline.
pub(super) async fn docker_ps(distro: &str, user: &str) -> Option<Vec<String>> {
    let (program, args) = container_ps_command(distro, user);
    let output = match run_control_command(&program, &args).await {
        ControlOutcome::Ran(output) if output.status.success() => output,
        ControlOutcome::Ran(output) => {
            tracing::warn!(
                stderr = %excerpt(&output.stderr),
                "docker ps failed; skipping the boot container reconcile"
            );
            return None;
        }
        ControlOutcome::Failed(error) => {
            tracing::warn!(error = %error, "docker ps cannot run; skipping the boot reconcile");
            return None;
        }
        ControlOutcome::TimedOut => {
            tracing::warn!("docker ps timed out; skipping the boot reconcile");
            return None;
        }
    };
    Some(ps_names(&output.stdout))
}

/// The R5 reconcile's probe: the very command of [`docker_ps`], but every
/// failure path is SILENT. Report-only doctrine: a probe that could not run is
/// not a warning — docker being off is the normal state of a machine the
/// harness never uses, and a rescan must never spam the operator for it.
pub(super) async fn docker_ps_quiet(distro: &str, user: &str) -> Option<Vec<String>> {
    let (program, args) = container_ps_command(distro, user);
    match run_control_command(&program, &args).await {
        ControlOutcome::Ran(output) if output.status.success() => Some(ps_names(&output.stdout)),
        ControlOutcome::Ran(_) | ControlOutcome::Failed(_) | ControlOutcome::TimedOut => None,
    }
}

/// The pure core of the R5 report-only reconcile: the running containers whose
/// exact name a catalog row declares as `ninfer_container` while the claim
/// ledger holds **no** claim — the stranded VRAM the ownership-scoped reaping
/// may never address (ruling 9: report it, never touch it). A claimed match is
/// a running engine this harness started and never appears; a running container
/// no catalog row names is foreign and equally out of report. Presence in
/// `docker ps` is re-checked, never assumed.
pub(super) fn stranded_plan(
    present: &[String],
    declared: &[String],
    claims: &ContainerClaims,
) -> BTreeSet<String> {
    present
        .iter()
        .filter(|name| {
            declared.iter().any(|candidate| candidate == *name) && !claims.contains(name)
        })
        .cloned()
        .collect()
}

/// `wsl.exe … docker logs --tail <n> <name>` (R9): the raw container stdout. The
/// line bound is imposed by **docker** (`--tail`), not by truncating a fully
/// buffered fetch after the fact — a container the operator never rotated (e.g. one
/// left from a manual run under a harness-shaped name) has an unbounded log, and a
/// cap that only applies after `output()` has buffered it is a cap on the file we
/// keep, not on the memory we spend. The byte ceiling stays as a belt for pathological
/// single lines. The engine-specific parser (ninfer) reads the startup `MemorySummary`
/// from this; `ProcessRuntime` stays engine-agnostic. A control command, so it runs
/// through [`run_control_command`] with only the scrubbed OS baseline (no token into
/// WSL).
pub(super) async fn container_logs(distro: &str, user: &str, name: &str) -> Option<String> {
    let mut argv = wsl_prefix(distro, user);
    argv.extend([
        "docker".to_string(),
        "logs".to_string(),
        format!("--tail={}", CONTAINER_LOG_TAIL_LINES),
        name.to_string(),
    ]);
    let (program, args) = argv
        .split_first()
        .unwrap_or_else(|| unreachable!("invariant: wsl_prefix is never empty"));
    match run_control_command(program, args).await {
        ControlOutcome::Ran(output) if output.status.success() => {
            // `docker logs` routes the container's stdout to the client's stdout and
            // the container's stderr to the client's stderr; the startup record is
            // the engine's, and an engine that logs it to stderr must not silently
            // cost the booking (the same reason the engine-child capture merges both
            // streams). A docker-side error line is inert to the parsers.
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            if !output.stderr.is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&String::from_utf8_lossy(&output.stderr));
            }
            let bytes = text.as_bytes();
            if bytes.len() <= CONTAINER_LOG_FETCH_CAP {
                Some(text)
            } else {
                let mut start = bytes.len() - CONTAINER_LOG_FETCH_CAP;
                while start < bytes.len() && !text.is_char_boundary(start) {
                    start += 1;
                }
                Some(text[start..].to_string())
            }
        }
        ControlOutcome::Ran(output) => {
            tracing::warn!(
                container = name,
                stderr = %excerpt(&output.stderr),
                "docker logs reported failure"
            );
            None
        }
        ControlOutcome::Failed(error) => {
            tracing::warn!(container = name, error = %error, "docker logs cannot run");
            None
        }
        ControlOutcome::TimedOut => {
            tracing::warn!(container = name, "docker logs timed out");
            None
        }
    }
}
