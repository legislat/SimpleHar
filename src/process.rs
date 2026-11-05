//! Process supervision (DESIGN Step 8, LOCKED C+D, item 6 approved).
//!
//! One Win32 Job Object per owned engine tree with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: if the harness dies for any reason, the
//! kernel kills the whole tree — the crash-proof orphan guarantee for *process*
//! engines. Detached WSL containers (ninfer) are the documented carve-out: the
//! job only reaps the `wsl.exe` client; the container itself is reaped
//! cooperatively (`docker rm -f` in `kill`) and reconciled pre-spawn by removing
//! any stale container of the same name — a harness crash can orphan one until the
//! model loads again.
//! Spawns are hidden (`CREATE_NO_WINDOW`); engine children additionally start
//! suspended and run only after the kill job has claimed them (R2-08-04), so no
//! engine descendant can be born outside the job. stdout/stderr are captured to
//! `logs/<slug>-<epoch>.log`, the stem sanitized by `sanitize_slug` — the one
//! forensic pool key shared with the container snapshots (R2-08-03). Engine
//! children receive exactly an OS baseline plus
//! `LaunchSpec.env` (§5, E3): harness secrets (`SIMPLEHARN_TOKEN`) and Python
//! import-hijack vars (`PYTHONPATH`, `VIRTUAL_ENV`) never leak into them. The
//! harness-internal control commands spawned here (`wsl.exe` docker teardown and
//! `docker ps`) are scrubbed the same way, because they execute *inside the
//! operator's distro*: inheriting there would carry the plaintext token across that
//! very boundary (FINDINGS2-0910 \[4], closing the asymmetry E3 left). Control
//! commands owned by other modules (`netsh` in `main.rs`, the `nvidia-smi` fallback
//! in `witness.rs`, the Web UI opener in `tray.rs`) still inherit — none of them
//! crosses into WSL — and are the smaller remaining half of that rule.
//! G12 release evidence: the child handle is signaled *and* the port refuses TCP
//! (for containers, the closed port after reaping is the evidence).
//!
//! This module holds the project's documented `unsafe` for Win32 interop: every
//! block states its invariant. The single-instance guard (A10, §0.2 step 3) lives
//! here too: a named mutex refuses a second harness.

use crate::custody::{self, CustodyLedger};
use crate::engine::{
    http_probe, wsl_prefix, EngineRuntime, LaunchSpec, Probe, SpawnError, Spawned,
};
use crate::ownership::{
    boot_reap_decision, valid_container_name, ContainerClaims, ReapDecision, ResourceKind,
    SpawnTicket,
};
use async_trait::async_trait;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

/// `CREATE_NO_WINDOW`: hidden children (Step 8).
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Poll pause between release-evidence checks inside the deadline.
const RELEASE_POLL: Duration = Duration::from_millis(100);
/// Per-model forensic retention (R9, operator 2026-10-31): the newest this many
/// launches' client captures **and** this many container snapshots survive, per model.
/// Replaces the old global-50 budget, which let a swap-heavy model evict another
/// model's forensic log — a failed launch's log must survive the second launch.
const ENGINE_LOG_KEEP: usize = 5;
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

/// Windows env var names (case-insensitive) an engine child may keep (§5 locked
/// decision, E3): external engines obey only harness-owned configuration, so a
/// child's environment is this baseline plus what [`LaunchSpec`] declares — never
/// the harness's own environment with its plaintext `SIMPLEHARN_TOKEN`, its
/// `PYTHONPATH`/`VIRTUAL_ENV` (strata is a Python child: import-hijack risk), and
/// every other stray var.
const OS_ENV_BASELINE: &[&str] = &[
    "SystemRoot",
    "windir",
    "COMSPEC",
    "Path",
    "PATHEXT",
    "TEMP",
    "TMP",
];

/// Builds the child baseline by copying the [`OS_ENV_BASELINE`] names out of
/// `source` (the harness env at the spawn site), matching names case-insensitively:
/// Windows env names are case-insensitive and [`std::env::vars`] preserves whatever
/// spelling the parent carried, so lookups go through the uppercased key while the
/// output uses the baseline's canonical spelling. A name supplied under several
/// spellings (e.g. `PATH` and `Path`) collapses to one entry, first occurrence
/// wins. `SystemRoot`, `windir`, `COMSPEC` and `PATHEXT` are never invented: absent
/// from `source` they stay absent and the spawn fails loudly if Windows demands
/// one. `Path`, `TEMP` and `TMP` are always present via [`baseline_fallback`].
fn child_env_baseline_from<I>(source: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    let wanted: HashSet<String> = OS_ENV_BASELINE
        .iter()
        .map(|name| name.to_uppercase())
        .collect();
    let mut found: HashMap<String, String> = HashMap::new();
    for (name, value) in source {
        let upper = name.to_uppercase();
        if wanted.contains(&upper) {
            found.entry(upper).or_insert(value);
        }
    }
    OS_ENV_BASELINE
        .iter()
        .filter_map(|name| {
            let upper = name.to_uppercase();
            let value = found
                .get(&upper)
                .cloned()
                .or_else(|| baseline_fallback(&upper))?;
            Some(((*name).to_string(), value))
        })
        .collect()
}

/// The always-present baseline values the harness env may lack: `Path` falls back
/// to the harness's own directory and `TEMP`/`TMP` to [`std::env::temp_dir`];
/// every other name returns `None` and is omitted rather than invented.
///
/// # Panics
///
/// Only when `name_upper` is `"PATH"` and the harness executable path cannot be
/// resolved — there is no honest substitute for `Path`.
fn baseline_fallback(name_upper: &str) -> Option<String> {
    match name_upper {
        "PATH" => Some(
            std::env::current_exe()
                .expect("invariant: the harness exe path resolves; Path has no other fallback")
                .parent()
                .expect("invariant: the harness exe lives in a directory")
                .to_string_lossy()
                .into_owned(),
        ),
        "TEMP" | "TMP" => Some(std::env::temp_dir().to_string_lossy().into_owned()),
        _ => None,
    }
}

/// The complete environment for one engine child: the baseline copied from
/// `source` (the harness env at the spawn site), then every `spec_env` entry
/// applied last so a declared var overrides its baseline twin. Names are
/// case-insensitive on Windows, so a spec entry first removes any baseline pair
/// whose uppercased key equals its own, then appends under its own spelling —
/// the assembly never carries two spellings of one name.
///
/// Applies to `LaunchSpec`-driven engine spawns and, with an empty `spec_env`, to
/// the harness-internal WSL control commands built by [`run_control_command`] —
/// nothing spawned by this module inherits the harness environment any more
/// (FINDINGS2-0910 [4]). Shared by the spawn site and the tests so the tests
/// exercise the real production assembly.
fn engine_child_env<I>(source: I, spec_env: &BTreeMap<String, String>) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut env = child_env_baseline_from(source);
    for (key, value) in spec_env {
        let upper = key.to_uppercase();
        env.retain(|(name, _)| name.to_uppercase() != upper);
        env.push((key.clone(), value.clone()));
    }
    env
}

/// What a harness-internal control command may carry: exactly the OS baseline
/// ([`OS_ENV_BASELINE`]) and nothing else — no `SIMPLEHARN_TOKEN`, no `PYTHONPATH`,
/// no stray harness name (FINDINGS2-0910 [4]).
///
/// The earlier ruling here was "control commands run harness code, not operator
/// config, so they keep inheriting"; that justified the *inbound* direction only.
/// The two WSL control commands execute *outbound* — their whole purpose is to run
/// inside the operator's distro, the very boundary E3 called worse than a Windows
/// child environment — so inheriting was an outbound secret leak. They need nothing
/// but `SystemRoot`/`windir`/`Path`/`PATHEXT`/`COMSPEC`/`TEMP`/`TMP`, i.e. what
/// `wsl.exe` itself requires, so no separate control-only list was invented: one
/// baseline, one rule, nothing to drift.
fn control_child_env() -> Vec<(String, String)> {
    engine_child_env(std::env::vars(), &BTreeMap::new())
}

/// How one harness-internal control command finished.
#[derive(Debug)]
enum ControlOutcome {
    /// The command ran and reported; the caller judges the exit status and stdout.
    Ran(std::process::Output),
    /// The command could not be started at all (no `wsl.exe`, spawn refused).
    Failed(String),
    /// The command outlived [`CONTROL_TIMEOUT`].
    TimedOut,
}

/// The deadline every control command gets: a wedged `wsl.exe`/docker call must not
/// park a boot reconcile or an unload indefinitely.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

/// The ONE site through which a harness-internal control command is built and run:
/// hidden (`CREATE_NO_WINDOW`), `kill_on_drop` so a timed-out client cannot
/// accumulate as an orphan, and with the scrubbed [`control_child_env`] rather than
/// the inherited environment (FINDINGS2-0910 [4]). Both WSL control commands go
/// through here; neither may build its own [`Command`], which is what made the
/// asymmetry between engine children and control commands possible in the first
/// place.
async fn run_control_command(program: &str, args: &[String]) -> ControlOutcome {
    run_control_command_with_stdin(program, args, &[]).await
}

/// [`run_control_command`] with an explicit deadline — the ONE control-command
/// builder shared by the WSL commands and the R11 fit probes, so a probe gets
/// the same hidden / kill-on-drop / scrubbed-environment posture and only the
/// timeout is the caller's (the fit probes outlive the 10 s WSL default only
/// by operator calibration, never by a new spawn path).
async fn run_control_command_timeout(
    program: &str,
    args: &[String],
    timeout: Duration,
) -> ControlOutcome {
    run_control_command_with_stdin_timeout(program, args, &[], timeout).await
}

/// [`run_control_command`] with a stdin payload. The ONLY legitimate use is the
/// staging driver: a program that must reach the distro shell *as data*, never as
/// argv — the login-shell re-tokenization GOTCHAS `[ps]` warns about cannot touch
/// bytes that ride a pipe, and no path (Windows or otherwise) ever appears in the
/// command line. Writing the payload to the child's stdin to EOF precedes waiting,
/// so a child that never reads cannot wedge the control timeout into `TimedOut`
/// without also being reaped by `kill_on_drop`.
async fn run_control_command_with_stdin(
    program: &str,
    args: &[String],
    stdin: &[u8],
) -> ControlOutcome {
    run_control_command_with_stdin_timeout(program, args, stdin, CONTROL_TIMEOUT).await
}

/// The body behind [`run_control_command`], [`run_control_command_timeout`] and
/// [`run_control_command_with_stdin`]: the ONE place a control [`Command`] is
/// built, with the deadline as the only varying parameter.
async fn run_control_command_with_stdin_timeout(
    program: &str,
    args: &[String],
    stdin: &[u8],
    timeout: Duration,
) -> ControlOutcome {
    let mut command = Command::new(program);
    command
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .kill_on_drop(true)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in control_child_env() {
        command.env(key, value);
    }
    let run = async {
        let mut child = command.spawn().map_err(|error| error.to_string())?;
        let mut pipe = child
            .stdin
            .take()
            .unwrap_or_else(|| unreachable!("invariant: stdin is piped above"));
        // A stdin write that fails because the child already died (a `bash` that
        // rejects the invocation outright) is not the failure to report: the exit
        // status and stderr below name the real one.
        let _ = pipe.write_all(stdin).await;
        drop(pipe);
        child
            .wait_with_output()
            .await
            .map_err(|error| error.to_string())
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(Ok(output)) => ControlOutcome::Ran(output),
        Ok(Err(error)) => ControlOutcome::Failed(error),
        Err(_) => ControlOutcome::TimedOut,
    }
}

/// The directory every harness log file lands in: `logs/` beside the current
/// executable, falling back to the working directory. Shared by the captured engine
/// children and [`crate::observability`]'s boot log, so the two can never disagree
/// about where the operator's `logs/` lives.
#[must_use]
pub fn harness_logs_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("logs")))
        .unwrap_or_else(|| PathBuf::from("logs"))
}

/// Whether a path is the harness's own boot log rather than a captured engine child.
/// Matched on the file stem, so the rolled `harness-<epoch>.log.1`, `.log.2`, … (whose
/// extension is a generation number) still count as harness logs and cannot leak out of
/// the harness budget into the engine one.
fn is_harness_log(path: &std::path::Path) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.starts_with(crate::observability::HARNESS_LOG_PREFIX))
}

fn is_engine_log(path: &std::path::Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("log") && !is_harness_log(path)
}

/// Deletes the oldest files beyond `keep` in `dir` that `wants` claims. `kind` names
/// them in the one warning this can emit, so a failure says which budget broke.
fn prune_matching(
    dir: &std::path::Path,
    keep: usize,
    kind: &str,
    wants: impl Fn(&std::path::Path) -> bool,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| wants(path))
        .filter_map(|path| {
            let modified = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .ok()?;
            Some((modified, path))
        })
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort_by_key(|(modified, _)| *modified); // oldest first
    let excess = files.len() - keep;
    for (_, path) in files.into_iter().take(excess) {
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!(error = %error, path = %path.display(), "cannot prune old {kind} log");
        }
    }
}

/// Whether a path is one of the R9 container-log snapshots (`<model>-<epoch>.container.log`),
/// as opposed to a client capture (`<model>-<epoch>.log`).
fn is_container_log(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".container.log"))
        && !is_harness_log(path)
}

/// Per-model forensic retention (R9): keep the newest `keep` client captures **and**
/// the newest `keep` container snapshots for one model, deleting the older ones of
/// each class. Scoped to one model so a swap-heavy model can never evict another
/// model's forensic log — the defect of the old global budget. The scope is the exact
/// minted shape `{model}-{epoch}[.container].log` — epoch all digits — NOT a bare
/// `{model}-` prefix: model names legally contain `-`, so a prefix match let model
/// `swift`'s keep-N claim (and delete) `swift-7b`'s files. Harness boot logs are
/// excluded from both classes by [`is_harness_log`]. The caller passes the ONE
/// forensic pool key — [`sanitize_slug`] of the model, the same stem both mint
/// sites use (R2-08-03) — never a raw name or a private spelling.
fn prune_model_logs(dir: &std::path::Path, prefix: &str, keep: usize) {
    let owned_prefix = format!("{prefix}-");
    let matches_model = |path: &std::path::Path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                let Some(rest) = name.strip_prefix(owned_prefix.as_str()) else {
                    return false;
                };
                // `{epoch}.log` or `{epoch}.container.log`, epoch all digits.
                let epoch = rest
                    .strip_suffix(".container.log")
                    .or_else(|| rest.strip_suffix(".log"))
                    .unwrap_or(rest);
                !epoch.is_empty() && epoch.chars().all(|c| c.is_ascii_digit())
            })
    };
    prune_matching(dir, keep, "engine", |path| {
        is_engine_log(path) && !is_container_log(path) && matches_model(path)
    });
    prune_matching(dir, keep, "container", |path| {
        is_container_log(path) && matches_model(path)
    });
}

/// Keeps the newest `keep` harness boot logs (rotations included). Called by
/// [`crate::observability::attach_file_sink`] at boot.
pub fn prune_harness_log_files(dir: &std::path::Path, keep: usize) {
    prune_matching(dir, keep, "harness", is_harness_log);
}

/// What the engine table says about a kill/release request for a port.
#[derive(Debug, PartialEq, Eq)]
enum TableVerdict {
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
fn table_verdict(held_generation: Option<u64>, ticket_generation: u64) -> TableVerdict {
    match held_generation {
        None => TableVerdict::Unowned,
        Some(found) if found != ticket_generation => TableVerdict::Stale { found },
        Some(_) => TableVerdict::Ours,
    }
}

/// One tracked engine tree: the child plus its Job Object.
struct EngineProc {
    child: Child,
    /// Win32 job handle; closing/terminating it kills the whole tree.
    job: JobHandle,
    /// Set once the kill path ran; the entry stays until release is confirmed.
    terminated: bool,
    /// Detached WSL container backing this engine (ninfer): the job object only
    /// reaps the `wsl.exe` client, so `kill` must additionally `docker rm -f`
    /// this name or the container keeps holding the GPU.
    container: Option<String>,
    /// The generation of the [`SpawnTicket`] that created this entry. Kill and release
    /// must match it: a port names a slot in the port space, not a process, and the
    /// ticket that identified the original child can outlive it — a stale ticket must
    /// never terminate the successor that inherited the port.
    generation: u64,
    /// The launch argv, kept only so the probe fast-fail can say WHICH path the
    /// early-exited engine was launched against (FINDINGS-13 [2]'s companion: an
    /// engine that dies on a deleted model must name it, not just its exit code).
    argv: Vec<String>,
    /// The child's OS pid captured at spawn. `Child::id()` reports `None` once
    /// the child has been reaped, so the release-time custody bookkeeping cannot
    /// ask the child again — the value must ride on the entry.
    pid: u32,
    /// The custody sidecar token written at spawn for a custodied launch
    /// (chunk 10 slice 10b); `release_confirmed` may only clear the entry
    /// bearing this token, never one a later launch wrote under the same pid.
    custody_token: Option<u64>,
    /// The verdict of `kill`'s one-shot container reap duty (R2-08-02): a
    /// container-backed entry may only end its table slot and ownership claim
    /// on a *confirmed* `docker rm -f` — a refusing port alone does not prove
    /// the detached container is gone. Process engines never reap and never
    /// set this, so their release evidence stays exactly the G12 pair.
    reap_confirmed: bool,
}

/// Owned Job Object handle. `Send` because the runtime owns it exclusively: it is
/// created on one task, stored once, and closed exactly once — by [`Drop`].
struct JobHandle(windows::Win32::Foundation::HANDLE);

// Safety: exclusive ownership as documented on the struct; Win32 job handles are
// usable from any thread and we never duplicate or share this one.
unsafe impl Send for JobHandle {}

impl Drop for JobHandle {
    fn drop(&mut self) {
        // Safety: sole owner per the struct invariant. Closing an un-terminated
        // job also kills its tree (KILL_ON_JOB_CLOSE) — the same kernel mechanism
        // `kill` relies on, so dropped entries can never outlive their children.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// The production `EngineRuntime`: real hidden child processes under Job Objects.
#[derive(Clone)]
pub struct ProcessRuntime {
    engines: Arc<Mutex<HashMap<u16, EngineProc>>>,
    /// Shared so every clone of the runtime reads and writes one claim ledger:
    /// the boot reclaim and the spawn/release paths cannot disagree.
    claims: Arc<Mutex<ContainerClaims>>,
    /// The custody sidecar (slice 10b): recorded at spawn, cleared on
    /// confirmed release, swept at boot — one shared record per process
    /// lifetime, the same single-authority rule as the claim ledger.
    custody: Arc<Mutex<CustodyLedger>>,
    /// Test seam: per-engine overrides of the rendered-profile staging roots
    /// (`TASK-BASALT` stage 1 generalized the strata-only root of ruling 19;
    /// production always resolves the compiled-in row per launch).
    #[cfg(test)]
    profiles_root_overrides: BTreeMap<&'static str, PathBuf>,
    client: reqwest::Client,
    logs_dir: PathBuf,
    wsl_distro: String,
    wsl_user: String,
    /// Test seam: fails the job-object step *after* `command.spawn()` succeeded, so
    /// the post-spawn failure path — the one that must leave no ownership evidence
    /// behind (FINDINGS2-0910 [3]; since R2-08-04 it never has any, because the
    /// claim and custody writes happen only after the job seal succeeds) — is
    /// reachable without a wedged Win32 API.
    #[cfg(test)]
    fail_job_after_spawn: bool,
    /// Test seam: parks this runtime's `release_confirmed` pollers in the gap
    /// between the TCP probe and the final generation re-check of the table
    /// lock (see [`ReleaseGate`]).
    #[cfg(test)]
    release_gate: Option<Arc<ReleaseGate>>,
    /// Test seam: scripts the verdicts `kill`'s container reap duty reports
    /// (see [`ReapSeam`]).
    #[cfg(test)]
    reap_seam: Option<Arc<ReapSeam>>,
    /// Test seam (R5): the R5 reconcile probe goes through this docker door
    /// instead of `wsl.exe`, so pins can script `docker ps` and read back the
    /// docker verbs the reconcile actually asked for — the vocabulary bound the
    /// report-only doctrine promises.
    #[cfg(test)]
    docker_probe: Option<DockerProbeSeam>,
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
struct ReleaseGate {
    /// Notified by `release_confirmed` when it reaches the gap.
    arrived: tokio::sync::Notify,
    /// Notified by the test to let the parked poller re-judge and finish.
    departed: tokio::sync::Notify,
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
struct ReapSeam {
    /// One verdict per scripted reap, consumed front-first.
    scripted: Mutex<std::collections::VecDeque<bool>>,
    /// The container name each scripted verdict answered for, in order. A
    /// passive `release_confirmed` poll must never extend this list (REV7-01).
    asked: Mutex<Vec<String>>,
}

/// Recorded docker-probe seam for the R5 pins (test-only): a canned `docker ps`
/// answer plus the log of docker verbs it was asked to run.
#[cfg(test)]
#[derive(Clone)]
struct DockerProbeSeam {
    /// `Some` = docker answered with these running names; `None` = the command
    /// could not run (the silent path).
    outcome: Option<Vec<String>>,
    /// One entry per probe, in order. The reconcile reaching `docker rm` or any
    /// other verb would leave its mark here.
    calls: Arc<Mutex<Vec<String>>>,
}

/// `wsl.exe -d <distro> -u <user> -- docker rm -f <name>` (v3.3 kill_container
/// parity): program and args as pure data, through the shared `wsl_prefix` so
/// launch and teardown can never address different WSL coordinates.
fn container_rm(distro: &str, user: &str, name: &str) -> (String, Vec<String>) {
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
fn container_port(distro: &str, user: &str, name: &str) -> (String, Vec<String>) {
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
fn published_elsewhere_evidence(container: &str, offered: u16, text: &str) -> Option<String> {
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

/// The hard bound on [`descendant_pids_of`]'s answer: a healthy engine
/// family is a handful of pids, so any reachable tree this large is a
/// pathological one and the walk stops rather than spin.
const DESCENDANT_CAP: usize = 4_096;

/// ALL descendants of `root`, at every depth, from ONE Toolhelp32 process
/// snapshot (`TASK-ENGINE-MEASUREMENT` phase 5, card capture 2026-11-04):
/// both engines sit TWO levels below the harness child — python launcher ->
/// python server -> engine, plus the engine's own vision child — so a
/// one-generation sum misses the very process that holds the card. The
/// snapshot becomes a parent->children table and a breadth-first walk leaves
/// from the root with a visited set, so a parent cycle in the table cannot
/// spin the walk and the root itself never enters the answer. The walk reads
/// the snapshot only and touches no process. Every failure path — snapshot
/// refused, walk broken at any point — answers an empty Vec: absence is
/// reported, never a guess (doctrine 4).
#[must_use]
fn descendant_pids_of(root: u32) -> Vec<u32> {
    use std::collections::{HashMap, HashSet, VecDeque};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    // Safety: a fresh process-list snapshot; `CreateToolhelp32Snapshot`
    // answers Err for the invalid handle, and every entry buffer passed to
    // the walk is owned here, zeroed, and carries its exact size in `dwSize`.
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return Vec::new();
    };
    let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // Safety: `snapshot` is the live handle from above; `entry` is a writable,
    // correctly sized PROCESSENTRY32W. A failed `First` is an empty or broken
    // snapshot — the walk simply stops.
    let mut walking = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
    while walking {
        children_of
            .entry(entry.th32ParentProcessID)
            .or_default()
            .push(entry.th32ProcessID);
        // Safety: the same live handle and entry as the `First` call;
        // `Next` reports the end of the list (and any error) as a failed walk.
        walking = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
    }
    // Safety: sole owner of the snapshot handle from above; this single close
    // is the release on every path, mirroring the [`JobHandle`] discipline.
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(snapshot) };
    let mut seen: HashSet<u32> = HashSet::new();
    seen.insert(root); // the root is asked-about, never an answer of its own
    let mut queue: VecDeque<u32> = VecDeque::new();
    queue.push_back(root);
    let mut descendants: Vec<u32> = Vec::new();
    while let Some(parent) = queue.pop_front() {
        let Some(children) = children_of.get(&parent) else {
            continue;
        };
        for &child in children {
            if descendants.len() >= DESCENDANT_CAP {
                return descendants; // pathological tree: bounded, never unbounded work
            }
            if seen.insert(child) {
                descendants.push(child);
                queue.push_back(child);
            }
        }
    }
    descendants
}

/// Whether this kill must reap a container. A CONFIRMED reap retires the duty
/// for good: a re-kill never re-runs `docker rm` for someone else's container
/// (review, 2026-10-26). An UNCONFIRMED reap keeps the duty alive (REV7-01):
/// the entry is still ours and the container may still hold the card, so the
/// next explicit kill carries the retry. Only `kill` reaches this rule --
/// `release_confirmed` polls are passive and never reap -- and every attempt
/// stays gated by the (port, generation) verdict and the claim ledger
/// ([`reap_action`]), so a re-arm can never address an unclaimed name.
fn should_reap(
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
enum ReapAction {
    /// The duty fell to this kill and the ledger holds the claim.
    Reap(String),
    /// The duty fell to this kill but no claim backs the name: leave the container.
    SkipUnclaimed(String),
    /// Nothing to do (no container, or a re-kill whose reap already confirmed).
    None,
}

fn reap_action(duty: Option<String>, claimed: bool) -> ReapAction {
    match duty {
        Some(name) if claimed => ReapAction::Reap(name),
        Some(name) => ReapAction::SkipUnclaimed(name),
        None => ReapAction::None,
    }
}

/// G12 release evidence: process engines must show exit + closed port; a
/// detached container's client exit proves nothing — the closed port after
/// reaping is the evidence (v3.3 `Proc::Container` parity).
fn release_ready(is_container: bool, exited: bool, port_closed: bool) -> bool {
    (exited || is_container) && port_closed
}

/// Bounded stderr excerpt for warn lines (WSL/docker startup errors are verbose;
/// the ring buffer and `/har/log` stay readable).
fn excerpt(bytes: &[u8]) -> String {
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
async fn reap_container(distro: &str, user: &str, name: &str) -> bool {
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

/// The in-distro directory a harness-rendered autostart script is staged into.
/// Deliberately outside every model directory and outside the operator's script
/// tree: a staged file can never shadow or be confused with a hand-written one
/// (template adoption, operator ruling 2026-10-31). The value is READ from the
/// ninfer row's `custody_exec_roots` cell of the [`crate::engine::policy`]
/// table (slice 10b: the table cell is the single home, not a re-aliased
/// constant), so the compiled-in custody root and the staging path cannot
/// drift apart (§20-9 Fork C).
///
/// # Panics
///
/// Never: the compiled-in ninfer row always carries its staged-scripts
/// custody root (a row without it is a build-time bug).
#[must_use]
fn staged_scripts_root() -> &'static str {
    crate::engine::policy::engine_policy(crate::ownership::NINFER_ENGINE)
        .and_then(|row| row.custody_exec_roots.first())
        .copied()
        .expect("invariant: the compiled-in ninfer row ships its staged-scripts custody root")
}

/// How many staged renders of one model's slug survive in the staging directory
/// (the staging driver prunes older `<slug>-<hash>.sh` files past this many). A
/// model under active editing re-renders on every yaml change; the cap keeps the
/// directory bounded while retaining enough history to re-launch what the ledger
/// still names.
const STAGED_SCRIPT_KEEP: usize = 10;

/// The `tail -n +N` bound that realizes [`STAGED_SCRIPT_KEEP`]: the first
/// `KEEP`-newest files are kept, everything from `KEEP+1` on is pruned.
const STAGED_SCRIPT_KEEP_PLUS_ONE: usize = STAGED_SCRIPT_KEEP + 1;

/// FNV-1a over the script bytes: a content address, not a security primitive. Two
/// renders of one model collide only when their bytes are equal, which is exactly
/// the dedup the staging path wants; a hostile yaml cannot engineer a collision
/// against another model because every render names its own container, and the
/// script's *content* is validated at render time regardless.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// The content address a staged script carries in its name. Public so the adapter's
/// argv placeholder names exactly the file `stage_script` will have verified —
/// one hash, one naming rule, no drift between what the ledger shows and what runs.
#[must_use]
pub fn script_content_hash(script: &str) -> u64 {
    fnv1a64(script.as_bytes())
}

/// How many staged profile renders of one model survive in the rendered-profile
/// root (the keep-N twin of [`STAGED_SCRIPT_KEEP`], strata template mode,
/// operator ruling 2026-11-02 row 19).
const STAGED_PROFILE_KEEP: usize = 10;

/// The slug alphabet every staged artifact shares: the model or container name
/// with anything outside `[A-Za-z0-9._-]` replaced by `_`, so a staged file
/// name is always filesystem-bare and the keep-N prune matches one model's
/// pool literally.
#[must_use]
pub(crate) fn sanitize_slug(slug: &str) -> String {
    slug.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The staged file name a rendered profile carries: the sanitized model slug
/// plus the same content address the runtime verifies the bytes against before
/// it rewrites `--config` (one hash, one naming rule — the ledger, the argv
/// placeholder and the file on disk name the same bytes). Engine-blind: every
/// engine that stages a rendered profile names it the same way
/// ([`crate::engine::strata`] delegates here; [`crate::engine::basalt`] calls
/// through it), so the prune and the verify can never disagree with the writer.
#[must_use]
pub(crate) fn staged_profile_name(slug: &str, text: &str) -> String {
    format!(
        "{}-{:016x}.json",
        sanitize_slug(slug),
        script_content_hash(text)
    )
}

/// The absolute directory harness-rendered profiles of one engine stage into:
/// the first `custody_data_roots` cell of that engine's compiled-in row,
/// resolved under the executable's own directory — the exe-adjacent home
/// [`strata_custody_path`] keeps (operator ruling 2026-11-02 row 19 for strata;
/// `TASK-BASALT` stage 1 generalized the door, one root per engine).
///
/// # Panics
///
/// When the compiled-in table carries no row for `engine`, or the row ships no
/// data root — an adapter handing the runtime a rendered profile for an engine
/// with no custody root is a builder bug, not operator input.
#[must_use]
pub(crate) fn rendered_profiles_dir(engine: &str) -> PathBuf {
    let root = crate::engine::policy::engine_policy(engine)
        .and_then(|row| row.custody_data_roots.first())
        .copied()
        .unwrap_or_else(|| {
            panic!("invariant: the compiled-in `{engine}` row ships its rendered-profiles custody root")
        });
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(root)))
        .unwrap_or_else(|| PathBuf::from(root))
}

/// The strata spelling of [`rendered_profiles_dir`], kept for the strata call
/// sites and their pins (byte-identical tail: `.simplehar/strata-profiles`).
#[must_use]
pub(crate) fn strata_profiles_dir() -> PathBuf {
    rendered_profiles_dir("strata")
}

fn staged_script_path(slug: &str, script: &str) -> String {
    let slug = sanitize_slug(slug);
    let root = staged_scripts_root();
    format!("~/{root}/{slug}-{:016x}.sh", fnv1a64(script.as_bytes()))
}

/// Escape every POSIX ERE metacharacter so `find -regextype posix-extended -regex`
/// matches `literal` byte-for-byte. `staged_script_path` confines the slug it feeds
/// the prune regex to `[A-Za-z0-9._-]`, where only `.` is ERE-significant — but the
/// escape is total, so widening that charset upstream cannot silently hand a `.`- or
/// `[`-bearing slug the prune wildcard (review 2026-10-31: raw `har.a` pruned
/// `har-a`'s and `harxa`'s pools, measured on the distro's findutils).
///
/// # Arguments
///
/// * `literal` - text to be matched literally inside a POSIX ERE.
///
/// # Returns
///
/// The text with every metacharacter backslash-prefixed.
fn escape_posix_regex(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len() + 8);
    for c in literal.chars() {
        if matches!(
            c,
            '.' | '\\' | '[' | ']' | '{' | '}' | '(' | ')' | '*' | '+' | '?' | '|' | '^' | '$'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Point the script slot of a staged-script argv at `path`, or refuse. The slot is
/// the word after `bash`, and `bash` must be the word after the *last* `--` that
/// closes `wsl.exe`'s own options — anchoring on the first `bash` anywhere would
/// let a config whose distro or user is literally `bash` move the rewrite onto the
/// wrong slot, leaving the unwritten placeholder for `bash` to open relative to
/// cwd (review 2026-10-31, LOW; fail-closed today, pinned so it stays so). Pure,
/// so the anchor is testable without a distro.
///
/// # Arguments
///
/// * `argv` - the adapter's launch argv, placeholder in the script slot.
/// * `path` - the staged path (from [`staged_script_path`]) to install.
///
/// # Returns
///
/// The rewritten argv, or `None` when argv is not `… -- bash <slot> …`.
fn rewrite_script_slot(mut argv: Vec<String>, path: String) -> Option<Vec<String>> {
    let dashes = argv.iter().rposition(|arg| arg == "--")?;
    argv.get(dashes + 1).filter(|word| *word == "bash")?;
    let slot = argv.get_mut(dashes + 2)?;
    *slot = path;
    Some(argv)
}

/// Point the `--config` slot of a rendered-profile argv at `path`, or refuse.
/// The marker is the strata row's `path_markers` cell; a spec that carries a
/// rendered profile but no config slot is a builder bug, and the launch fails
/// closed rather than spawning a child pointed at the placeholder.
///
/// # Arguments
///
/// * `argv` - the adapter's launch argv, harness-minted placeholder in the slot.
/// * `path` - the staged absolute path to install as the flag's value.
///
/// # Returns
///
/// The rewritten argv, or `None` when no `--config` token carries a value.
fn rewrite_config_slot(mut argv: Vec<String>, path: &str) -> Option<Vec<String>> {
    let slot = argv
        .iter()
        .position(|arg| arg == "--config")?
        .checked_add(1)?;
    *argv.get_mut(slot)? = path.to_string();
    Some(argv)
}

/// Stage a harness-rendered profile into the exe-adjacent root named by its
/// engine's row (operator ruling 2026-11-02 row 19, engine-tagged in
/// `TASK-BASALT` stage 1). Three guarantees, one door:
///
/// * **name binds bytes** — the argv placeholder must equal
///   `<slug>-<fnv>.json` recomputed from `text`, so a hand-built spec cannot
///   stage one profile under another model's name;
/// * **atomic write** — hash-named temp plus `rename` (same-directory,
///   replace-on-publish), then the published bytes are read back and re-hashed:
///   what the child reads is what the adapter validated;
/// * **bounded** — keep-N prune of this slug's
///   `<sanitized-slug>-<16hex>.json` pool ([`prune_staged_profiles`]),
///   newest first (the in-distro script driver's [`STAGED_SCRIPT_KEEP`] rule
///   ported to the Windows filesystem).
///
/// # Returns
///
/// The absolute staged path on success, or the refusal sentence.
fn stage_rendered_profile(
    engine: &str,
    root: &Path,
    slug: &str,
    text: &str,
    placeholder: &str,
) -> Result<PathBuf, String> {
    let expected = staged_profile_name(slug, text);
    if placeholder != expected {
        return Err(format!(
            "the {engine} argv names `{placeholder}` but these bytes render `{expected}`: a \
             staged profile must carry its own content address"
        ));
    }
    std::fs::create_dir_all(root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    let target = root.join(&expected);
    let temp = root.join(format!(".staging.{expected}.tmp"));
    if let Err(e) = std::fs::write(&temp, text.as_bytes()) {
        // A partial temp the prune never matches (it only claims `.json`)
        // must not survive its own failure (review 26 [3]).
        let _ = std::fs::remove_file(&temp);
        return Err(format!("cannot write {}: {e}", temp.display()));
    }
    if let Err(error) = std::fs::rename(&temp, &target) {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("cannot publish {}: {error}", target.display()));
    }
    let landed = std::fs::read(&target)
        .map_err(|e| format!("cannot read back {}: {e}", target.display()))?;
    if script_content_hash(&String::from_utf8_lossy(&landed)) != script_content_hash(text) {
        return Err(format!(
            "the staged profile at {} is not the bytes the harness rendered",
            target.display()
        ));
    }
    prune_staged_profiles(root, slug);
    Ok(target)
}

/// Keep-N prune of one slug's staged-profile pool, newest by mtime. Entries are
/// matched exactly — `<sanitized-slug>-<16 hex>.json`, the SAME name
/// [`crate::engine::strata::staged_profile_name`] writes (review 25 [3]: raw
/// vs sanitized would prune zero of its own files and grow the pool without
/// bound) — so a dot-bearing slug (`har.a`) can never prune another model's
/// pool (the lesson the ninfer staging driver learned on the distro's
/// findutils, review 2026-10-31). Two names that sanitize identically
/// (`a b` / `a_b`) share one pool by construction: the name is the content
/// address's owner and a launch re-stages its own bytes anyway.
fn prune_staged_profiles(root: &Path, slug: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let prefix = format!("{}-", sanitize_slug(slug));
    let mut pool: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let address = name.strip_suffix(".json")?.strip_prefix(prefix.as_str())?;
            // Exactly the 16-hex content address, nothing else, marks the pool.
            let is_address = address.len() == 16
                && address
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
            if !is_address {
                return None;
            }
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            Some((mtime, entry.path()))
        })
        .collect();
    if pool.len() <= STAGED_PROFILE_KEEP {
        return;
    }
    pool.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, stale) in &pool[STAGED_PROFILE_KEEP..] {
        let _ = std::fs::remove_file(stale);
    }
}

/// The POSIX `cksum` value of `bytes` — CRC-32 with polynomial `0x04C11DB7`
/// (MSB-first, unreflected), zero init, then the message length folded in the same
/// way, then the complement — exactly what every POSIX `cksum` prints as its first
/// two fields. The staging driver re-computes this *in the distro* (`cksum < file`,
/// the two-field stdin form) and compares, so the staging evidence is the bytes,
/// not "some non-empty file exists". Chosen over awk arithmetic (a review measured
/// gawk and mawk disagreeing with each other and with any 64-bit hash) and over
/// zlib-style reflected CRC-32 (which is NOT what `cksum` computes — a review
/// caught the first cut confusing them): these are the algorithm's defining
/// constants, and the pin below is the operator's own coreutils 9.4 binary, whose
/// vectors this function reproduces byte-for-byte.
fn cksum_expectation(bytes: &[u8]) -> (u32, usize) {
    fn fold(mut crc: u32, byte: u8) -> u32 {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
        crc
    }
    let mut crc = 0u32;
    for byte in bytes {
        crc = fold(crc, *byte);
    }
    let mut len = bytes.len();
    loop {
        crc = fold(crc, (len & 0xFF) as u8);
        len /= 256;
        if len == 0 {
            break;
        }
    }
    (!crc, bytes.len())
}

/// The stdin staging program: base64 heredoc in, hash-named temp, double `cksum`
/// gate (before and after the atomic `mv -f`), and a bounded per-slug keep-N prune.
/// Pure — every interpolation is harness-internal (the hash, the blob, the cksum
/// expectation, the path-safe slug escaped for the prune regex), so the driver's
/// shape is pinned without a distro. `file_name` is the argv word `bash -s` binds
/// to `$1`; its `<slug>-<16hex>.sh` shape names the same content address the temp
/// and the prune carry.
fn staging_driver(script: &str, file_name: &str, blob: &str) -> String {
    let hash = fnv1a64(script.as_bytes());
    let slug = file_name
        .strip_suffix(&format!("-{:016x}.sh", hash))
        .unwrap_or(file_name);
    let (expected_crc, expected_len) = cksum_expectation(script.as_bytes());
    // `valid_container_name` admits `.`, which is the ERE any-character operator:
    // interpolated raw, a dot-bearing slug (`har.a`) would prune `har-a`'s and
    // `harxa`'s keep-N pool (review 2026-10-31, measured on the distro's findutils).
    // Escaping every metacharacter makes the regex match the slug LITERALLY.
    let slug_re = escape_posix_regex(slug);
    let root = staged_scripts_root();
    // The temp names the slug AND the hash, and both temp paths are `rm -f`-ed
    // before the redirects: shell `>` PRESERVES a pre-existing file's mode (umask
    // is creation-only), so a planted loose-mode temp must not propagate to the
    // published target, and two concurrent stagings of identical bytes must not
    // share one temp (review 2026-10-31, LOW; fail-closed either way — content is
    // cksum-gated — but a flake and a mode leak are both what staging must not do).
    format!(
        "#!/usr/bin/env bash\n\
         set -euo pipefail\n\
         umask 077\n\
         dir=\"$HOME/{root}\"\n\
         mkdir -p \"$dir\"\n\
         target=\"$dir/$1\"\n\
         tmp=\"$dir/.staging.{slug}-{hash:x}.tmp\"\n\
         trap 'rm -f -- \"$tmp\" \"$tmp.b64\"' EXIT\n\
         rm -f -- \"$tmp\" \"$tmp.b64\"\n\
         cat > \"$tmp.b64\" <<'SIMPLEHARN_B64'\n{blob}\nSIMPLEHARN_B64\n\
         base64 -d \"$tmp.b64\" > \"$tmp\"\n\
         rm -f -- \"$tmp.b64\"\n\
         test \"$(cksum < \"$tmp\")\" = \"{expected_crc} {expected_len}\" \\\n\
             || {{ echo \"staging: content mismatch before publish\" >&2; exit 3; }}\n\
         mv -f -- \"$tmp\" \"$target\"\n\
         test \"$(cksum < \"$target\")\" = \"{expected_crc} {expected_len}\" \\\n\
             || {{ echo \"staging: content mismatch after publish\" >&2; exit 4; }}\n\
         trap - EXIT\n\
         find \"$dir\" -maxdepth 1 -regextype posix-extended \\\n\
             -regex '.*/{slug_re}-[0-9a-f]{{16}}\\.sh' -printf '%T@\\t%p\\n' 2>/dev/null \\\n\
             | sort -rn | tail -n +{STAGED_SCRIPT_KEEP_PLUS_ONE} \\\n\
             | while IFS=$'\\t' read -r _ stale; do case \"$stale\" in \"$dir/\"*) rm -f -- \"$stale\";; esac; done\n",
    )
}

/// Stage `script`'s bytes at `path` (a `~/.simplehar/autostart/` path the wsl_user
/// owns) and prove the *content* landed. The distro rules (GOTCHAS `[ps]`) are "never
/// `wsl bash -c <string>` — the login shell re-tokenizes and expands the payload
/// before the inner shell sees it" and (same reason) never hand the inner shell a
/// Windows path: so the staging program arrives on **stdin** (`bash -s`), which the
/// login shell never sees at all, with only plain harness-minted words after it. The
/// script body never enters any command line — it rides a base64 blob inside the
/// driver's quoted heredoc, the one alphabet with no shell metacharacters — and
/// `$HOME` is resolved by the driver, inside the distro. Write is atomic
/// (slug-and-hash-named temp + `mv -f`; the temp is `rm -f`-ed up front because a
/// shell redirect preserves a pre-existing file's mode, while the *target* is
/// never pre-deleted, so a concurrent staging's published file is only ever
/// replaced by `mv` itself), a `trap` removes the temp on every exit path, and the
/// driver checks `cksum` of the staged bytes twice — before publish and after —
/// against the expectation computed here: a staging the distro did not confirm
/// byte-for-byte is reported, never spawned against.
///
/// # Returns
///
/// `Ok(())` only when the distro confirmed the staged file checksums to `script`'s
/// exact bytes.
async fn stage_script(distro: &str, user: &str, path: &str, script: &str) -> Result<(), String> {
    use base64::Engine as _;
    if distro.trim().is_empty() || user.trim().is_empty() {
        return Err(
            "WSL coordinates missing from the config: cannot stage an autostart script".to_string(),
        );
    }
    let blob = base64::engine::general_purpose::STANDARD.encode(script.as_bytes());
    let file_name = path.rsplit('/').next().unwrap_or(path);
    let driver = staging_driver(script, file_name, &blob);
    // The driver rides stdin; the argv words after `bash -s` are plain words
    // (`<slug>-<16hex>.sh` re-sanitized by `staged_script_path`; the prune regex
    // carries the slug ERE-escaped — literal, never a wildcard — AND demands the
    // 16-hex content address, so no container name, prefix-related or dot-bearing,
    // can aim the keep-N pool at another model's renders). The
    // cksum expectation is NOT an argument: the login shell would re-split a
    // space-bearing value, so it rides as a literal inside the stdin script, and
    // `cksum < file` takes the two-field stdin form (the file-argument form prints
    // a third path field a `test =` can never match).
    let mut argv = wsl_prefix(distro, user);
    argv.extend(["bash".to_string(), "-s".to_string(), file_name.to_string()]);
    let (program, rest) = argv
        .split_first()
        .unwrap_or_else(|| unreachable!("invariant: wsl_prefix is never empty"));
    match run_control_command_with_stdin(program, rest, driver.as_bytes()).await {
        ControlOutcome::Ran(output) if output.status.success() => Ok(()),
        ControlOutcome::Ran(output) => Err(format!(
            "distro rejected the staging driver: {}",
            excerpt(&output.stderr)
        )),
        ControlOutcome::Failed(error) => Err(format!("cannot run the staging driver: {error}")),
        ControlOutcome::TimedOut => Err("the staging driver timed out".to_string()),
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
fn reclaim_plan(
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
fn container_claims_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("ninfer-claims.json")))
        .unwrap_or_else(|| PathBuf::from("ninfer-claims.json"))
}

/// The custody sidecar (slice 10b): recorded strata launches, beside the
/// claim ledger the harness already keeps next to its executable — the same
/// state home for the same reason: one microsecond of local read at boot,
/// never a stall on a volume that could delay the bind.
fn strata_custody_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("strata-custody.json")))
        .unwrap_or_else(|| PathBuf::from("strata-custody.json"))
}

/// `wsl.exe -d <distro> -u <user> -- docker ps --format {{.Names}}` as pure data:
/// the one bounded command every container reconcile lists running names with,
/// through the shared [`wsl_prefix`] so the two reconciles can never address
/// docker with different commands.
fn container_ps_command(distro: &str, user: &str) -> (String, Vec<String>) {
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
async fn docker_ps(distro: &str, user: &str) -> Option<Vec<String>> {
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
async fn docker_ps_quiet(distro: &str, user: &str) -> Option<Vec<String>> {
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
fn stranded_plan(
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
async fn container_logs(distro: &str, user: &str, name: &str) -> Option<String> {
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

impl ProcessRuntime {
    /// Creates the runtime; log files land under `logs/` next to the current exe
    /// (falling back to the working directory).
    pub fn new() -> Self {
        ProcessRuntime {
            engines: Arc::new(Mutex::new(HashMap::new())),
            // One ledger for the whole process lifetime, shared by every clone:
            // claimed at spawn, cleared on confirmed release, consulted at boot.
            claims: Arc::new(Mutex::new(ContainerClaims::load(container_claims_path()))),
            // One custody sidecar per process lifetime, same single-authority
            // rule (slice 10b): recorded when a custodied launch really
            // started, cleared on confirmed release, swept at boot.
            custody: Arc::new(Mutex::new(CustodyLedger::load(strata_custody_path()))),
            #[cfg(test)]
            profiles_root_overrides: BTreeMap::new(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .expect("invariant: plain-HTTP client with a timeout needs no configuration"),
            logs_dir: harness_logs_dir(),
            // No WSL coordinates before boot: they name a machine-specific distro and a
            // personal account, so nothing is assumed. Boot wires both through `with_wsl`,
            // and the reaping guards refuse loudly rather than invoke `wsl.exe` without
            // them (review, 2026-10-26).
            wsl_distro: String::new(),
            wsl_user: String::new(),
            #[cfg(test)]
            fail_job_after_spawn: false,
            #[cfg(test)]
            release_gate: None,
            #[cfg(test)]
            reap_seam: None,
            #[cfg(test)]
            docker_probe: None,
        }
    }

    /// Sets the WSL coordinates used to address ninfer containers; the harness
    /// wires these from config.
    #[must_use]
    pub fn with_wsl(mut self, wsl_distro: impl Into<String>, wsl_user: impl Into<String>) -> Self {
        self.wsl_distro = wsl_distro.into();
        self.wsl_user = wsl_user.into();
        self
    }

    /// Points this runtime's ownership ledger at `path`. Test-only: the pins for
    /// [3] must neither touch the real `ninfer-claims.json` beside the test exe nor
    /// hand `wsl.exe` the operator's coordinates.
    #[cfg(test)]
    #[must_use]
    pub fn with_claims_ledger(mut self, path: impl Into<PathBuf>) -> Self {
        self.claims = Arc::new(Mutex::new(ContainerClaims::load(path)));
        self
    }

    /// Points this runtime's custody sidecar at `path`. Test-only, the very
    /// rule [`Self::with_claims_ledger`] states: pins must neither touch the
    /// real sidecar beside the test exe nor leave entries for the next run.
    #[cfg(test)]
    #[must_use]
    pub fn with_custody_ledger(mut self, path: impl Into<PathBuf>) -> Self {
        self.custody = Arc::new(Mutex::new(CustodyLedger::load(path)));
        self
    }

    /// Points this runtime's strata rendered-profile root at `path`. Test-only,
    /// the same rule as the ledger seams: pins must neither touch the directory
    /// beside the test exe nor the harness's release folder.
    #[cfg(test)]
    #[must_use]
    pub fn with_profiles_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.profiles_root_overrides.insert("strata", path.into());
        self
    }

    /// Points this runtime's rendered-profile root for `engine` at `path`.
    /// Test-only, the rule [`Self::with_profiles_root`] states, generalized to
    /// the per-engine roots of `TASK-BASALT` stage 1.
    #[cfg(test)]
    #[must_use]
    pub fn with_rendered_profiles_root(
        mut self,
        engine: &'static str,
        path: impl Into<PathBuf>,
    ) -> Self {
        self.profiles_root_overrides.insert(engine, path.into());
        self
    }

    /// Where rendered profiles of `engine` stage for this runtime: the
    /// compiled-in row's custody root, or the test seam's override for it.
    fn profiles_root(&self, engine: &str) -> PathBuf {
        #[cfg(test)]
        if let Some(path) = self.profiles_root_overrides.get(engine) {
            return path.clone();
        }
        rendered_profiles_dir(engine)
    }

    /// Fails the job-object step after the client started. Test-only; see the
    /// [`Self::fail_job_after_spawn`] field.
    #[cfg(test)]
    #[must_use]
    pub fn with_injected_job_failure(mut self) -> Self {
        self.fail_job_after_spawn = true;
        self
    }

    /// Routes this runtime's R5 reconcile probe through a recorded docker seam
    /// answering with `outcome` (`None` = docker cannot run). Test-only, the
    /// rule [`Self::with_claims_ledger`] states: pins must neither touch the
    /// operator's WSL nor their docker. The calls handle is shared, so the test
    /// reads back the docker verbs the reconcile asked for.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_docker_probe(
        mut self,
        outcome: Option<Vec<String>>,
        calls: &Arc<Mutex<Vec<String>>>,
    ) -> Self {
        self.docker_probe = Some(DockerProbeSeam {
            outcome,
            calls: Arc::clone(calls),
        });
        self
    }

    /// Parks this runtime's `release_confirmed` pollers in the await gap
    /// before the final generation re-check. Test-only; see [`ReleaseGate`].
    #[cfg(test)]
    #[must_use]
    fn with_release_gate(mut self, gate: Arc<ReleaseGate>) -> Self {
        self.release_gate = Some(gate);
        self
    }

    /// Scripts this runtime's `kill`-carried container reap verdicts (the
    /// seam also reports the names each scripted verdict answered for).
    /// Test-only; see [`ReapSeam`].
    #[cfg(test)]
    #[must_use]
    fn with_reap_seam(mut self, verdicts: &[bool]) -> (Self, Arc<ReapSeam>) {
        let seam = Arc::new(ReapSeam {
            scripted: Mutex::new(verdicts.iter().copied().collect()),
            asked: Mutex::new(Vec::new()),
        });
        self.reap_seam = Some(Arc::clone(&seam));
        (self, seam)
    }

    /// Creates the kill-on-close job for the freshly spawned, still-suspended
    /// child. The `#[cfg(test)]` seam exists so the post-spawn failure path
    /// (which must leave no ownership evidence behind) can be reached without a
    /// wedged Win32 API.
    ///
    /// # Errors
    ///
    /// The Win32 reason the job could not be created.
    fn create_kill_job(&self) -> Result<windows::Win32::Foundation::HANDLE, String> {
        #[cfg(test)]
        if self.fail_job_after_spawn {
            return Err("injected job-object failure (test)".to_string());
        }
        // Safety: the handle is created here and handed to exactly one `JobHandle`,
        // whose `Drop` closes it; job handles are usable from any thread.
        unsafe { create_kill_on_close_job() }
    }

    /// Clears this launch's custody sidecar entry, token-guarded: `None` (the
    /// launch was never recorded) forgets nothing, and a foreign token can
    /// never erase a later launch's record (slice 10b, engine `drop_record`
    /// rule). Used by the confirmed release.
    fn forget_custody(&self, pid: u32, token: Option<u64>) {
        if let Some(token) = token {
            self.custody
                .lock()
                .expect("custody sidecar lock")
                .forget(pid, token);
        }
    }

    /// Runs this kill's reap duty: the scripted verdict while the REV7 test
    /// seam ([`ReapSeam`]) still holds one for it, the real `docker rm -f`
    /// otherwise. The seam replaces only the WSL/docker call, so the duty
    /// rule, the claim gate, the generation guard, and the entry recording
    /// stay the production paths; a runtime without a seam, or once the
    /// script is exhausted, runs [`reap_container`] exactly as before.
    async fn run_reap_duty(&self, name: &str) -> bool {
        #[cfg(test)]
        if let Some(seam) = &self.reap_seam {
            let scripted = seam
                .scripted
                .lock()
                .expect("reap verdict seam lock")
                .pop_front();
            if let Some(verdict) = scripted {
                seam.asked
                    .lock()
                    .expect("reap verdict seam lock")
                    .push(name.to_string());
                return verdict;
            }
        }
        reap_container(&self.wsl_distro, &self.wsl_user, name).await
    }

    /// Boot reconcile (deploy review, 2026-10-26; scoped by chunk 10 \[1], 2026-10-27):
    /// a container this harness started in an earlier lifetime survives a crash holding
    /// tens of GiB, and observed-VRAM admission then refuses every later load with no
    /// pointer to the culprit. A running container is destroyed only when
    /// [`boot_reap_decision`] passes every ownership gate: declared by a **ninfer**
    /// model, docker-legal under the same validation the spawn path applies, and
    /// positively claimed by this harness's ledger at spawn. Any gate failing — above
    /// all a missing claim — logs a warning naming the container and SKIPs it: the
    /// harness never destroys on suspicion (operator policy). Best-effort: any failure
    /// logs and boot proceeds.
    pub async fn reclaim_at_boot(&self, candidates: &[ContainerCandidate]) {
        if candidates.is_empty() {
            return;
        }
        let declared: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
        if self.wsl_user.trim().is_empty() || self.wsl_distro.trim().is_empty() {
            // Name the containers the catalog declares: without them the operator's next
            // load only says "insufficient VRAM", and this line is the sole pointer to the
            // orphan holding the card (review, 2026-10-26).
            tracing::warn!(
                declared = ?declared,
                distro = %self.wsl_distro,
                user = %self.wsl_user,
                "WSL coordinates missing from the config: boot reclaim stands down, an orphaned \
                 container keeps the card occupied"
            );
            return;
        }
        let Some(present) = docker_ps(&self.wsl_distro, &self.wsl_user).await else {
            return;
        };
        let plan = {
            let claims = self.claims.lock().expect("container claim ledger lock");
            reclaim_plan(&present, candidates, &claims)
        };
        let mut reaped: BTreeSet<String> = BTreeSet::new();
        for (candidate, decision) in plan {
            match decision {
                ReapDecision::Skip { reason } => tracing::warn!(
                    container = %candidate.name,
                    engine = %candidate.engine,
                    reason,
                    declared = ?declared,
                    "boot reclaim skipped: no ownership evidence for this container; leaving it running"
                ),
                ReapDecision::Reap => {
                    if !reaped.insert(candidate.name.clone()) {
                        continue; // two catalog entries, one reap
                    }
                    // Re-check the ledger under its lock immediately before destroying:
                    // a release that completed while we deliberated withdraws permission.
                    if !self
                        .claims
                        .lock()
                        .expect("container claim ledger lock")
                        .contains(&candidate.name)
                    {
                        continue;
                    }
                    tracing::warn!(container = %candidate.name, "reclaiming a container left behind by an earlier run");
                    if reap_container(&self.wsl_distro, &self.wsl_user, &candidate.name).await {
                        self.claims
                            .lock()
                            .expect("container claim ledger lock")
                            .unclaim(&candidate.name);
                    }
                }
            }
        }
    }

    /// Test-only read of the ownership ledger behind [3]'s pins.
    #[cfg(test)]
    fn claimed(&self, name: &str) -> bool {
        self.claims
            .lock()
            .expect("container claim ledger lock")
            .contains(name)
    }

    /// Boot custody reconcile + orphan sweep (chunk 10 slice 10b, R8): for
    /// each custody sidecar entry, act only on a process that provably matches
    /// the recorded identity (pid alive **and** start-time token equal); a
    /// non-matching entry is dropped as stale and whatever lives at that pid
    /// now is never touched. The engine's own leftover probe is untrusted —
    /// the release log's `[strata] could not check for engines left by an
    /// earlier run: RuntimeError` proves it can fail while saying nothing
    /// about what still runs — and is never consulted here: the sweep is
    /// driven purely by this harness's own sidecar, through pid-ops that have
    /// no engine channel. A matching orphan is terminated once (identity
    /// re-verified at the instant of the signal) and waited out with the
    /// normal stop-grace; an unconfirmed reap keeps its claim for the next
    /// boot. Best-effort: every failure path warns and boot proceeds.
    pub async fn reclaim_custody_at_boot(&self, grace: Duration) -> custody::ReconcileReport {
        let ledger = Arc::clone(&self.custody);
        if ledger.lock().expect("custody sidecar lock").is_empty() {
            return custody::ReconcileReport::default();
        }
        custody::reconcile(&ledger, &custody::WinCustodyOs, grace).await
    }

    /// Test-only read of the custody sidecar behind slice 10b's pins.
    #[cfg(test)]
    fn custody_entries(&self) -> Vec<custody::CustodyEntry> {
        self.custody
            .lock()
            .expect("custody sidecar lock")
            .entries()
            .cloned()
            .collect()
    }

    /// Number of tracked engines (test/leak assertion helper).
    pub fn engine_count(&self) -> usize {
        self.engines.lock().expect("engine table lock").len()
    }
}

impl Default for ProcessRuntime {
    fn default() -> Self {
        ProcessRuntime::new()
    }
}

#[async_trait]
impl EngineRuntime for ProcessRuntime {
    async fn spawn(&self, spec: &LaunchSpec, ticket: SpawnTicket) -> Result<Spawned, SpawnError> {
        // Template adoption (operator ruling 2026-10-31): a spec carrying a rendered
        // script runs THAT, never whatever file argv names — the bytes are staged
        // content-addressed, verified on disk, and the script position in argv is
        // rewritten to the staged path before anything spawns. A failed staging is a
        // spawn failure, not a fallback to an uncontrolled script.
        let spec = match spec.script.as_deref() {
            None => std::borrow::Cow::Borrowed(spec),
            Some(script) => {
                let slug = match &ticket.kind {
                    ResourceKind::Container { name } => name.clone(),
                    ResourceKind::ChildProcess => spec.log_name.clone(),
                };
                let path = staged_script_path(&slug, script);
                if let Err(reason) =
                    stage_script(&self.wsl_distro, &self.wsl_user, &path, script).await
                {
                    return Err(SpawnError::SpawnFailed {
                        reason: format!(
                            "cannot stage the harness-rendered autostart script: {reason}"
                        ),
                    });
                }
                let argv =
                    match rewrite_script_slot(spec.argv.clone(), path) {
                        Some(rewritten) => rewritten,
                        None => return Err(SpawnError::SpawnFailed {
                            reason:
                                "a staged-script spec must run bash: no script position to rewrite"
                                    .to_string(),
                        }),
                    };
                std::borrow::Cow::Owned(LaunchSpec {
                    argv,
                    ..spec.clone()
                })
            }
        };
        // Rendered-profile mode (operator ruling 2026-11-02 row 19; per-engine
        // roots in `TASK-BASALT` stage 1): a spec carrying a rendered profile
        // reads THAT file — the bytes stage into the harness's own root for
        // the tag's engine, the placeholder in the `--config` slot is rewritten
        // to the staged absolute path, and the name's content address is
        // verified against the bytes before the child can read them. A failed
        // staging is a spawn failure, never a fallback.
        let spec = match spec.rendered_profile.as_ref() {
            None => spec,
            Some(profile) => {
                let staged = stage_rendered_profile(
                    profile.engine,
                    &self.profiles_root(profile.engine),
                    &spec.log_name,
                    &profile.text,
                    spec.argv
                        .iter()
                        .position(|arg| arg == "--config")
                        .and_then(|at| spec.argv.get(at + 1))
                        .map(String::as_str)
                        .unwrap_or(""),
                );
                match staged.and_then(|path| {
                    rewrite_config_slot(spec.argv.clone(), &path.to_string_lossy())
                        .ok_or_else(|| "a rendered-profile spec must carry --config".to_string())
                }) {
                    Ok(argv) => std::borrow::Cow::Owned(LaunchSpec {
                        argv,
                        ..spec.as_ref().clone()
                    }),
                    Err(reason) => {
                        return Err(SpawnError::SpawnFailed {
                            reason: format!(
                                "cannot stage the harness-rendered {} profile: {reason}",
                                profile.engine
                            ),
                        });
                    }
                }
            }
        };
        let Some((program, rest)) = spec.argv.split_first() else {
            return Err(SpawnError::SpawnFailed {
                reason: "empty argv in launch spec".to_string(),
            });
        };
        // Container identity travels on the ticket (the provenance type); the
        // runtime never invents one. Every reap stays ownership-scoped, in two
        // parts: the shared name rule gates the pre-reap below, and the claim
        // ledger — minted only on a spawn that really started — gates the kill
        // reap and the boot reclaim (FINDINGS2-0910 [3]).
        let container = match &ticket.kind {
            ResourceKind::Container { name } => Some(name.clone()),
            ResourceKind::ChildProcess => None,
        };
        // Reconcile a stale container from a crashed run before this spawn: otherwise
        // `docker run` fails on the name conflict (opaque minutes-long readiness
        // failure) or the stale container answers readiness for a model the harness
        // never started.
        if let Some(name) = container.as_deref() {
            // The stale-container pre-reap is gated on the SHARED name rule — the one
            // the adapter, the claim ledger and the boot reclaim all use — so a name
            // docker could resolve as a container-ID prefix can never reach
            // `docker rm -f` from here. It is deliberately NOT a ledger write: the
            // claim that authorizes future reaping is minted only once a client
            // actually started (FINDINGS2-0910 [3]), so a load that failed before the
            // spawn leaves no ownership evidence behind. A refused name reaps nothing.
            if valid_container_name(name) {
                reap_container(&self.wsl_distro, &self.wsl_user, name).await;
            } else {
                tracing::warn!(
                    container = name,
                    port = ticket.port,
                    "not a name this harness may address: the stale container is left alone"
                );
            }
        }
        if let Err(error) = tokio::fs::create_dir_all(&self.logs_dir).await {
            return Err(SpawnError::SpawnFailed {
                reason: format!("cannot create {}: {error}", self.logs_dir.display()),
            });
        }
        // One forensic naming rule for the whole pool (R2-08-03): the client
        // capture and the container snapshot share one key — `sanitize_slug`,
        // the same slug rule the staged profiles use (`prune_staged_profiles`).
        // `spec.log_name` stays RAW as the custody/instance identity; only the
        // file name is filesystem-bare, so keep-N bounds one model's pair of
        // classes and two models cannot prune each other through a raw-vs-clean
        // split. Names that sanitize identically (`a b` / `a_b`) share one pool
        // by design — the accepted rule of the staged profiles (review 25 [3]).
        let pool = sanitize_slug(&spec.log_name);
        prune_model_logs(&self.logs_dir, &pool, ENGINE_LOG_KEEP);
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let log_path = self.logs_dir.join(format!("{pool}-{epoch}.log"));
        let log_file = std::fs::File::create(&log_path).map_err(|e| SpawnError::SpawnFailed {
            reason: format!("cannot create {}: {e}", log_path.display()),
        })?;
        let mut command = Command::new(program);
        command
            .args(rest)
            .current_dir(&spec.cwd)
            .stdout(log_file.try_clone().map_err(|e| SpawnError::SpawnFailed {
                reason: format!("cannot duplicate log handle: {e}"),
            })?)
            .stderr(log_file)
            // R2-08-04: hidden AND suspended. The primary thread runs no
            // instruction until the seal below has put this child in its kill
            // job and resumed it, so no engine thread can ever run — and no
            // grandchild ever be born — outside the job.
            .creation_flags(
                CREATE_NO_WINDOW | windows::Win32::System::Threading::CREATE_SUSPENDED.0,
            )
            // §5 (E3): the child sees exactly the OS baseline plus `spec.env`,
            // never the harness environment. The control commands in this module
            // get the same baseline with no spec entries ([`control_child_env`]).
            .env_clear();
        for (key, value) in engine_child_env(std::env::vars(), &spec.env) {
            command.env(key, value);
        }
        let mut child = command.spawn().map_err(|e| SpawnError::SpawnFailed {
            reason: format!("cannot spawn {program}: {e}"),
        })?;
        // R2-08-04 (the escape window, doctrine: no engine descendant may ever
        // escape the job): a grandchild created between `CreateProcess` and
        // `AssignProcessToJobObject` escapes `KILL_ON_JOB_CLOSE` forever, and the
        // claim/custody writes used to sit inside that window and widen it. The
        // child therefore starts SUSPENDED and the order here is fixed: create
        // the kill job, assign the frozen child, resume its primary thread. The
        // first engine instruction runs only after the assignment, so every
        // descendant is born inside the job; a failed seal kills the still
        // frozen child, which never runs and never had a thread to fork.
        let pid = child.id().unwrap_or(0);
        // Safety: the job handle is created and assigned here, on this task,
        // while the child is still suspended and before it is stored;
        // `raw_handle` is valid while the child is alive, and the job is closed
        // exactly once, by `JobHandle::drop`.
        let job = match self.create_kill_job().map(JobHandle) {
            Ok(job) => job,
            Err(reason) => {
                let _ = child.kill().await;
                return Err(SpawnError::SpawnFailed { reason });
            }
        };
        let sealed = match (child.raw_handle(), pid) {
            (Some(handle), pid) if pid != 0 => {
                unsafe { assign_to_job(job.0, handle) }.and_then(|()| resume_primary_thread(pid))
            }
            _ => Err("the spawned client reported no usable handle or pid".to_string()),
        };
        if let Err(reason) = sealed {
            // Nothing is written as ownership evidence before the seal, so a
            // failed seal retracts nothing: the frozen child dies here with no
            // claim, no custody record, and no instruction executed (the
            // success-path-only rule of FINDINGS2-0910 [3], now enforced by
            // ordering alone).
            let _ = child.kill().await;
            return Err(SpawnError::SpawnFailed { reason });
        }
        // The claim is the positive ownership evidence the boot reclaim demands, so it
        // is minted HERE — only once a client really started and stands sealed in its
        // kill job (FINDINGS2-0910 [3]).
        // Minting it before the spawn let a load that failed at `create_dir_all`,
        // the log file or the spawn itself leave `ninfer-claims.json` holding a name
        // this harness never started, which then passed all three boot gates and
        // authorized `docker rm -f` against a container the operator had started by
        // hand under that name. `claim` keeps its own validity rule, so an ID-shaped
        // name is still never recorded.
        if let Some(name) = container.as_deref() {
            self.claims
                .lock()
                .expect("container claim ledger lock")
                .claim(name);
        }
        // Custody (slice 10b): a custodied launch's identity enters the sidecar
        // only on the success path — after every spawn-failure return — so a
        // load the harness reports as failed never leaves evidence for the
        // boot sweep to act on. A child whose start time cannot be read records
        // nothing (warn): an identity that cannot be proven must never
        // authorize a future kill, and a record that cannot be written never
        // stops a launch (the engine-side rule, ported).
        let custody_token = match (spec.custody.as_deref(), pid) {
            (Some(engine), pid) if pid != 0 => child
                .raw_handle()
                .and_then(|raw| custody::start_token_of(windows::Win32::Foundation::HANDLE(raw)))
                .inspect(|&token| {
                    self.custody.lock().expect("custody sidecar lock").record(
                        custody::CustodyEntry {
                            pid,
                            start_token: token,
                            port: ticket.port,
                            instance: spec.log_name.clone(),
                            engine: engine.to_string(),
                            claimed_at_ms: custody::now_ms(),
                        },
                    );
                }),
            (Some(_), _) => {
                tracing::warn!(
                    port = ticket.port,
                    instance = %spec.log_name,
                    "a custodied launch reported no pid: nothing recorded, this engine \
                     will not be reclaimed if this harness dies"
                );
                None
            }
            (None, _) => None,
        };
        tracing::info!(port = ticket.port, pid, log = %log_path.display(),
            "engine spawned under a kill-on-close job object");
        {
            let mut engines = self.engines.lock().expect("engine table lock");
            if let Some(stale) = engines.get(&ticket.port) {
                // Replacing a live entry closes its job handle, which reaps that tree by
                // design (a crash-retry reclaims its own port). Saying so keeps the one
                // case that must never happen — two live engines on one port — visible
                // instead of silent.
                tracing::warn!(
                    port = ticket.port,
                    replaced_generation = stale.generation,
                    new_generation = ticket.generation,
                    "replacing a tracked engine on this port; its job object reaps it"
                );
            }
            engines.insert(
                ticket.port,
                EngineProc {
                    child,
                    job,
                    terminated: false,
                    container,
                    generation: ticket.generation,
                    argv: spec.argv.clone(),
                    pid,
                    custody_token,
                    reap_confirmed: false,
                },
            );
        }
        Ok(Spawned {
            port: ticket.port,
            pid,
        })
    }

    async fn kill(&self, ticket: &SpawnTicket) -> Result<(), SpawnError> {
        let duty = {
            let mut engines = self.engines.lock().expect("engine table lock");
            match table_verdict(
                engines.get(&ticket.port).map(|entry| entry.generation),
                ticket.generation,
            ) {
                TableVerdict::Unowned => return Ok(()), // never ours: never a foreign kill
                TableVerdict::Stale { found } => {
                    return Err(SpawnError::StaleTicket {
                        port: ticket.port,
                        expected: ticket.generation,
                        found,
                    })
                }
                TableVerdict::Ours => {}
            }
            let engine = engines
                .get_mut(&ticket.port)
                .expect("invariant: the verdict above just read Ours for this port");
            // Safety: the job handle is valid (created in spawn, closed by Drop)
            // and terminating a job is safe from any thread.
            unsafe { terminate_job(engine.job.0) };
            // A confirmed reap retires the duty forever: re-kills (cancel,
            // release-retry) must not re-run `docker rm` (review, 2026-10-26).
            // An UNCONFIRMED reap re-arms on the next explicit kill (REV7-01),
            // so a failed `docker rm` cannot burn the slot until boot; the
            // ledger and the generation verdict keep every retry scoped.
            let duty = should_reap(
                engine.terminated,
                engine.reap_confirmed,
                engine.container.as_ref(),
            );
            engine.terminated = true;
            let _ = engine.child.start_kill();
            duty
        };
        // A detached WSL container survives its client process: reap it by name —
        // but the LEDGER, not the ticket, is the reap authority (FINDINGS2-0910 [3]).
        let action = match duty {
            Some(name) => {
                let claimed = self
                    .claims
                    .lock()
                    .expect("container claim ledger lock")
                    .contains(&name);
                reap_action(Some(name), claimed)
            }
            None => ReapAction::None,
        };
        match action {
            ReapAction::Reap(name) => {
                let reaped = self.run_reap_duty(&name).await;
                // R2-08-02: the reap verdict is the evidence `release_confirmed`
                // needs before it may end the claim, so it rides on the entry.
                // Generation-guarded: a successor that inherited this port
                // while docker deliberated owns the entry now, and no reap of
                // the retired lifetime may speak for it.
                if let Some(engine) = self
                    .engines
                    .lock()
                    .expect("engine table lock")
                    .get_mut(&ticket.port)
                {
                    if engine.generation == ticket.generation {
                        engine.reap_confirmed = reaped;
                    }
                }
            }
            ReapAction::SkipUnclaimed(name) => tracing::warn!(
                container = %name,
                port = ticket.port,
                "kill owed a container reap but the ledger holds no claim: leaving it running"
            ),
            ReapAction::None => {}
        }
        Ok(())
    }

    async fn release_confirmed(&self, ticket: &SpawnTicket) -> Result<bool, SpawnError> {
        let (exited, is_container) = {
            let mut engines = self.engines.lock().expect("engine table lock");
            match table_verdict(
                engines.get(&ticket.port).map(|entry| entry.generation),
                ticket.generation,
            ) {
                TableVerdict::Unowned => return Ok(true), // never ours: nothing to confirm
                TableVerdict::Stale { found } => {
                    return Err(SpawnError::StaleTicket {
                        port: ticket.port,
                        expected: ticket.generation,
                        found,
                    })
                }
                TableVerdict::Ours => {}
            }
            let engine = engines
                .get_mut(&ticket.port)
                .expect("invariant: the verdict above just read Ours for this port");
            if !engine.terminated {
                return Ok(false);
            }
            let exited = engine
                .child
                .try_wait()
                .map(|status| status.is_some())
                .map_err(|e| SpawnError::KillFailed {
                    holder: format!("port {}", ticket.port),
                    reason: e.to_string(),
                })?;
            (exited, engine.container.is_some())
        };
        // Handle signaled; now the port must refuse TCP (G12 evidence pair).
        let port_closed = tokio::net::TcpStream::connect(("127.0.0.1", ticket.port))
            .await
            .is_err();
        // R2-08-01 pin seam (test-only, inert in production): park this task in
        // the await gap between reading the verdict and re-judging it under the
        // final table lock — the window this function must survive.
        #[cfg(test)]
        if let Some(gate) = &self.release_gate {
            gate.arrived.notify_one();
            gate.departed.notified().await;
        }
        if release_ready(is_container, exited, port_closed) {
            // The verdict above was read under the lock, but the lock was
            // dropped for the TCP await. Two live jobs can poll the SAME victim
            // ticket (the eviction job's victim and a same-slot reload's own
            // reaper — board REV6-03); a sibling poller that completed its
            // removal in this window lets the Dispose->Spawn gate insert the
            // successor before this line runs. Re-judge under the final lock
            // (R2-08-01): only the entry still bearing OUR generation may be
            // removed, and — R2-08-02 — a container entry only on its recorded
            // reap verdict. The bookkeeping below then uses the REMOVED entry,
            // never a capture from before the gap.
            enum ReleaseOutcome {
                /// Our entry, ours to end: its ownership records ride with it.
                Removed(Box<EngineProc>),
                /// A sibling poller already removed our entry, or a newer
                /// spawn inherited the port: the release of OUR child is
                /// proven, and whoever holds the entry now is a different
                /// lifetime this retired ticket may not touch.
                Superseded,
                /// Our entry, but its container reap was never confirmed while
                /// the ledger still holds the claim: the container may still
                /// hold the card, so entry and claim stay (see the return
                /// contract of [`reap_container`]). Carries the held entry's
                /// container name (REV7-04: it is always present here).
                ReapUnconfirmed(String),
            }
            let outcome = {
                let mut engines = self.engines.lock().expect("engine table lock");
                match table_verdict(
                    engines.get(&ticket.port).map(|entry| entry.generation),
                    ticket.generation,
                ) {
                    TableVerdict::Unowned | TableVerdict::Stale { .. } => {
                        ReleaseOutcome::Superseded
                    }
                    TableVerdict::Ours => {
                        // REV7-04: one lookup for the held entry. REV7-02: a
                        // container entry holds the release only while the
                        // ledger still backs it with a claim AND its reap has
                        // not confirmed. With no claim there is no ownership
                        // evidence left to preserve (boot reclaim already
                        // refuses the name), so the plain G12 pair decides and
                        // the SkipUnclaimed shape must not burn the slot until
                        // boot; REV7-01's explicit re-kill is the live retry.
                        let entry = engines
                            .get(&ticket.port)
                            .expect("invariant: the verdict above just read Ours for this port");
                        let unconfirmed_name = match entry.container.as_deref() {
                            Some(name) if !entry.reap_confirmed => Some(name.to_string()),
                            _ => None,
                        };
                        let reap_held = unconfirmed_name.as_deref().is_some_and(|name| {
                            self.claims
                                .lock()
                                .expect("container claim ledger lock")
                                .contains(name)
                        });
                        match unconfirmed_name {
                            Some(name) if reap_held => ReleaseOutcome::ReapUnconfirmed(name),
                            _ => ReleaseOutcome::Removed(Box::new(
                                engines.remove(&ticket.port).expect(
                                    "invariant: the verdict above just read Ours for this port",
                                ),
                            )),
                        }
                    }
                }
            };
            match outcome {
                ReleaseOutcome::Removed(engine) => {
                    if let Some(name) = engine.container.as_ref() {
                        // G12 evidence held AND `kill`'s `docker rm -f`
                        // confirmed the removal: the container is gone, so its
                        // ownership claim ends -- a later hand-started
                        // container reusing the name can never inherit a claim
                        // this harness no longer needs. An entry the ledger
                        // does not hold (the SkipUnclaimed shape, REV7-02)
                        // gets nothing written to its name: `unclaim` persists
                        // only a claim it actually removes.
                        self.claims
                            .lock()
                            .expect("container claim ledger lock")
                            .unclaim(name);
                    }
                    // The custody entry ends with the same evidence, on the
                    // REMOVED entry's own pid and token: token-guarded, so a
                    // release can never erase a later launch's record written
                    // under the same pid (slice 10b) — and a ticket that
                    // removed nothing clears nobody's record (R2-08-01).
                    self.forget_custody(engine.pid, engine.custody_token);
                    // `engine` drops here: `JobHandle::drop` closes the
                    // handle exactly once.
                    tracing::info!(
                        port = ticket.port,
                        "release confirmed: engine gone, port closed"
                    );
                    return Ok(true);
                }
                ReleaseOutcome::Superseded => {
                    tracing::info!(
                        port = ticket.port,
                        "release confirmed for the retired ticket; whoever holds the port now is untouched"
                    );
                    return Ok(true);
                }
                ReleaseOutcome::ReapUnconfirmed(container) => {
                    // R2-08-02: for a detached container the client's exit and
                    // a refusing port prove nothing about the container itself
                    // — `kill`'s `docker rm -f` is that evidence, and it
                    // failed. Stay unconfirmed: the release timeout keeps the
                    // claim, and the next boot reclaim — which keeps it too
                    // until docker confirms — stays the path with authority
                    // over a possibly-live container.
                    tracing::debug!(
                        port = ticket.port,
                        container = %container,
                        "release held: the container reap was not confirmed; keeping the ownership claim, an explicit re-kill re-arms the duty (REV7-01)"
                    );
                }
            }
        }
        tokio::time::sleep(RELEASE_POLL).await;
        Ok(false)
    }

    async fn discover_model_id(&self, port: u16) -> Option<String> {
        let url = format!("http://127.0.0.1:{port}/v1/models");
        let response = self.client.get(&url).send().await.ok()?;
        let body: serde_json::Value = response.json().await.ok()?;
        body["data"][0]["id"].as_str().map(str::to_string)
    }

    async fn engine_status_json(&self, port: u16, path: &str) -> Option<serde_json::Value> {
        // Same posture and the same 2 s client as `discover_model_id`: a
        // loopback GET, JSON in, `None` for any failure. Engine-blind by
        // contract — the per-engine parse lives in the adapter and the
        // sampler decides what absence means (witness fallback, never a
        // guess; TASK-ENGINE-MEASUREMENT ORCHESTRATION §1).
        let url = format!("http://127.0.0.1:{port}{path}");
        let response = self.client.get(&url).send().await.ok()?;
        response.json::<serde_json::Value>().await.ok()
    }

    async fn descendant_pids(&self, pid: u32) -> Vec<u32> {
        descendant_pids_of(pid)
    }

    async fn container_report(&self, container: &str) -> Option<String> {
        if self.wsl_distro.trim().is_empty() || self.wsl_user.trim().is_empty() {
            return None;
        }
        // Ownership gate, same rule as the reap: never address a name this harness
        // could not have spawned (an ID-shaped or hostile name is refused).
        if !valid_container_name(container) {
            tracing::warn!(
                container,
                "not a name this harness may address: container_report skipped"
            );
            return None;
        }
        container_logs(&self.wsl_distro, &self.wsl_user, container).await
    }

    async fn published_elsewhere(&self, container: &str, offered: u16) -> Option<String> {
        if self.wsl_distro.trim().is_empty() || self.wsl_user.trim().is_empty() {
            return None;
        }
        // Ownership gate ruling 1(b) applies strictly: the readiness report only
        // needs a well-shaped name, but this query reads the claim ledger too —
        // a container this harness never claimed is never even asked.
        let claimed = self
            .claims
            .lock()
            .expect("container claim ledger lock")
            .contains(container);
        if !valid_container_name(container) || !claimed {
            return None;
        }
        let (program, args) = container_port(&self.wsl_distro, &self.wsl_user, container);
        match run_control_command(&program, &args).await {
            // Only docker's own success line is evidence; a not-running,
            // wedged, or refused `docker port` keeps the original refusal
            // exactly as the probe wrote it.
            ControlOutcome::Ran(output) if output.status.success() => published_elsewhere_evidence(
                container,
                offered,
                &String::from_utf8_lossy(&output.stdout),
            ),
            ControlOutcome::Ran(output) => {
                tracing::debug!(
                    container,
                    stderr = %excerpt(&output.stderr),
                    "docker port refused publish evidence"
                );
                None
            }
            ControlOutcome::Failed(error) => {
                tracing::debug!(container, error = %error, "docker port cannot run for evidence");
                None
            }
            ControlOutcome::TimedOut => {
                tracing::debug!(container, "docker port timed out; no publish evidence");
                None
            }
        }
    }

    async fn unclaimed_container_names(&self, declared: &[String]) -> Option<BTreeSet<String>> {
        #[cfg(test)]
        if let Some(seam) = &self.docker_probe {
            seam.calls
                .lock()
                .expect("docker probe lock")
                .push("ps".to_string());
            let present = seam.outcome.clone()?;
            let claims = self.claims.lock().expect("container claim ledger lock");
            return Some(stranded_plan(&present, declared, &claims));
        }
        // Blank coordinates never reach `wsl.exe` — and the R5 probe answers
        // them SILENTLY, unlike the boot reclaim's pointer line: report-only
        // doctrine, a failed probe is not a warning.
        if self.wsl_distro.trim().is_empty() || self.wsl_user.trim().is_empty() {
            return None;
        }
        let present = docker_ps_quiet(&self.wsl_distro, &self.wsl_user).await?;
        let claims = self.claims.lock().expect("container claim ledger lock");
        Some(stranded_plan(&present, declared, &claims))
    }

    async fn save_container_log(&self, model: &str, text: &str) {
        // A snapshot with no bytes is forensic noise, not evidence: a container
        // that never exec'd yields an empty `docker logs`, and rotating a model's
        // genuine snapshots against such a file would destroy history to record
        // nothing.
        if text.trim().is_empty() {
            return;
        }
        // One naming rule, the same `sanitize_slug` the capture mint uses at
        // `spawn` and the staged profiles use (R2-08-03): adapters hand
        // `log_name` RAW as the instance identity, so BOTH forensic classes are
        // keyed under the sanitized stem and one per-model keep-N bounds the
        // pair. Names that sanitize identically share one pool by design, the
        // accepted rule of the staged profiles (review 25 [3]).
        let prefix = sanitize_slug(model);
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let path = self
            .logs_dir
            .join(format!("{prefix}-{epoch}.container.log"));
        if let Err(error) = tokio::fs::create_dir_all(&self.logs_dir).await {
            tracing::warn!(error = %error, "cannot create logs dir for a container snapshot");
            return;
        }
        match tokio::fs::write(&path, text.as_bytes()).await {
            Ok(()) => {
                prune_model_logs(&self.logs_dir, &prefix, ENGINE_LOG_KEEP);
                tracing::debug!(path = %path.display(), "container log snapshot written");
            }
            Err(error) => tracing::warn!(
                path = %path.display(),
                error = %error,
                "cannot write container log snapshot"
            ),
        }
    }

    async fn run_probe_command(
        &self,
        program: &std::path::Path,
        args: &[String],
        timeout: Duration,
    ) -> Option<String> {
        // R11: the fit probes ride the ONE control-command builder — hidden,
        // kill-on-drop, scrubbed to the OS baseline — never an engine spawn:
        // no ticket, no port, nothing registered. Every failure path warns and
        // answers `None`; the caller falls down the ladder, there is never an
        // invented number.
        match run_control_command_timeout(&program.to_string_lossy(), args, timeout).await {
            ControlOutcome::Ran(output) if output.status.success() => {
                Some(String::from_utf8_lossy(&output.stdout).into_owned())
            }
            ControlOutcome::Ran(output) => {
                tracing::warn!(
                    program = %program.display(),
                    stderr = %excerpt(&output.stderr),
                    "fit probe command refused; the row keeps its declared/estimate ladder"
                );
                None
            }
            ControlOutcome::Failed(error) => {
                tracing::warn!(
                    program = %program.display(),
                    error = %error,
                    "fit probe command could not start; the row keeps its declared/estimate ladder"
                );
                None
            }
            ControlOutcome::TimedOut => {
                tracing::warn!(
                    program = %program.display(),
                    timeout_s = timeout.as_secs(),
                    "fit probe timed out; the row keeps its declared/estimate ladder"
                );
                None
            }
        }
    }

    async fn probe(&self, port: u16, readiness: &crate::engine::ReadinessSpec) -> Probe {
        // Fast-fail (Step 3 cancel/dead-engine lesson): a child that already exited
        // must not eat the whole readiness deadline — readiness is unreachable.
        // Container-backed engines are exempt: the engine is the detached
        // container, not the short-lived wsl.exe client.
        let exited = {
            let mut engines = self.engines.lock().expect("engine table lock");
            engines.get_mut(&port).and_then(|engine| {
                if engine.container.is_some() {
                    return None;
                }
                engine
                    .child
                    .try_wait()
                    .ok()
                    .flatten()
                    .map(|status| (status.to_string(), engine.argv.clone()))
            })
        };
        if let Some((status, argv)) = exited {
            let (reason, missing) = early_exit_report(&argv, &status);
            if let Some(path) = missing {
                tracing::error!(
                    port,
                    path = %path,
                    "engine process exited early against a model file that is not on disk"
                );
            }
            return Probe::Failed { reason };
        }
        http_probe(&self.client, port, readiness).await
    }
}

/// Whether an argv word is a local path a missing-file diagnosis may name:
/// `scheme://` sentinels name no local file, and a bare module token
/// (`serve.server`) has no path shape at all — checking it would misname the
/// strata module as a missing model.
fn looks_like_local_path(candidate: &str) -> bool {
    !candidate.contains("://")
        && (candidate.contains('/')
            || candidate.contains('\\')
            || candidate.as_bytes().get(1) == Some(&b':'))
}

/// The fast-fail verdict: `(reason, missing path)`. When the launched spec named
/// a local model/profile file that is gone, the reason names it; otherwise the
/// pre-existing text is preserved verbatim. Pure given the filesystem — the stat
/// is its only effect-free input — so the decision is testable without a spawn.
/// The argv positions that carry a model file come from the `EnginePolicy`
/// table's `path_markers` cells (llama `-m <path>`, strata `--config <path>`);
/// a missing file at one of these slots turns the bare exit status of a
/// fast-fail into the file the operator must restore (FINDINGS-13 [2]'s
/// load-path companion); the catalog is never mutated from here (operator
/// ruling — a transient deletion must not silently rewrite the page).
fn early_exit_report(argv: &[String], status: &str) -> (String, Option<String>) {
    let markers = crate::engine::policy::path_markers();
    let missing = argv.windows(2).find_map(|window| {
        (markers.contains(&window[0].as_str())
            && looks_like_local_path(&window[1])
            && std::path::Path::new(&window[1]).metadata().is_err())
        .then(|| window[1].clone())
    });
    match missing {
        Some(path) => (
            format!("model file not found: {path} (engine exited early: {status})"),
            Some(path),
        ),
        None => (format!("engine process exited early ({status})"), None),
    }
}

/// Holds the named mutex for the process lifetime (A10 single-instance guard).
pub struct SingleInstanceGuard {
    handle: windows::Win32::Foundation::HANDLE,
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        // Safety: sole owner per the struct invariant; the guard is process-lifetime.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

impl SingleInstanceGuard {
    /// Acquires the one-instance mutex.
    ///
    /// # Errors
    ///
    /// [`String`] naming the error; "already running" means a second harness exists.
    pub fn acquire() -> Result<Self, String> {
        // Safety: CreateMutexW with no security attributes and automatic ownership
        // reclaim; we only inspect the last error, never wait on the handle.
        unsafe {
            // The mutex name keeps the PRE-rename spelling on purpose
            // (TASK-RENAME-SIMPLEHARN, 2026-11-04): during the live-deployment
            // switch window the pre-rename binary and `simpleharn.exe` must still
            // see each other's lock, or two harnesses share one card and the
            // single-instance rule (AGENTS.md §6) is silently broken.
            // This is a compatibility shim: do not "clean up" the old name.
            let name = windows::core::w!("SimpleHarSE-SingleInstance");
            let handle = windows::Win32::System::Threading::CreateMutexW(None, false, name)
                .map_err(|e| format!("cannot create mutex: {e}"))?;
            if windows::Win32::Foundation::GetLastError()
                == windows::Win32::Foundation::ERROR_ALREADY_EXISTS
            {
                let _ = windows::Win32::Foundation::CloseHandle(handle);
                return Err("another SimpleHarn instance is already running".to_string());
            }
            Ok(SingleInstanceGuard { handle })
        }
    }
}

/// Creates a Job Object whose tree dies when the handle closes (or is terminated).
///
/// # Safety
///
/// The caller must close the returned handle exactly once (`CloseHandle`) and must
/// not use it after. Valid as long as the kernel object exists.
unsafe fn create_kill_on_close_job() -> Result<windows::Win32::Foundation::HANDLE, String> {
    use windows::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    let job = CreateJobObjectW(None, None).map_err(|e| format!("CreateJobObjectW: {e}"))?;
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let set = SetInformationJobObject(
        job,
        JobObjectExtendedLimitInformation,
        &limits as *const _ as *const core::ffi::c_void,
        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
    );
    if set.is_err() {
        let _ = windows::Win32::Foundation::CloseHandle(job);
        return Err("SetInformationJobObject failed".to_string());
    }
    Ok(job)
}

/// Assigns a child process to a job.
///
/// # Safety
///
/// `child` must be a live process handle owned by this harness; the job must not
/// already contain processes with conflicting limits.
unsafe fn assign_to_job(
    job: windows::Win32::Foundation::HANDLE,
    child: std::os::windows::io::RawHandle,
) -> Result<(), String> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::AssignProcessToJobObject;

    AssignProcessToJobObject(job, HANDLE(child as _))
        .map_err(|e| format!("AssignProcessToJobObject: {e}"))
}

/// Resumes the primary thread of a process created with `CREATE_SUSPENDED`
/// (R2-08-04): the engine spawn seals the child into its kill job while no
/// instruction has run, then resumes it exactly once. A suspended process has
/// precisely one thread — its primary — so the Toolhelp entry whose owner pid
/// matches is that thread, and it cannot exit while suspended. This helper
/// needs the id because `std`'s `Command` closes the `CreateProcessW` primary
/// thread handle it is given and does not expose it.
///
/// # Errors
///
/// The Win32 reason the primary thread could not be found, opened, or resumed.
fn resume_primary_thread(pid: u32) -> Result<(), String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    // Safety: a fresh thread-list snapshot; `CreateToolhelp32Snapshot` answers
    // Err for the invalid handle, and every entry buffer passed to the walk is
    // owned here, zeroed, and carries its exact size in `dwSize`. The thread
    // handle is opened by that snapshot's own id and closed on every path
    // below; resuming the one suspended thread of a child this harness just
    // created suspended affects nothing else.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)
            .map_err(|e| format!("CreateToolhelp32Snapshot: {e}"))?;
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut primary: Option<u32> = None;
        let mut walking = Thread32First(snapshot, &mut entry).is_ok();
        while walking {
            if entry.th32OwnerProcessID == pid {
                primary = Some(entry.th32ThreadID);
                break;
            }
            walking = Thread32Next(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
        let tid = primary.ok_or_else(|| format!("pid {pid} has no thread to resume"))?;
        let thread = OpenThread(THREAD_SUSPEND_RESUME, false, tid)
            .map_err(|e| format!("OpenThread({tid}): {e}"))?;
        let previous = ResumeThread(thread);
        let _ = CloseHandle(thread);
        if previous == u32::MAX {
            return Err(format!("ResumeThread({tid}) failed"));
        }
        if previous > 1 {
            // `CREATE_SUSPENDED` guarantees one suspend; more would mean the
            // child is still frozen after this resume. Fail closed: the seal
            // failure arm kills the child and mints no ownership evidence.
            return Err(format!(
                "ResumeThread({tid}) left the primary thread suspended (count {previous})"
            ));
        }
        Ok(())
    }
}

/// Terminates every process in the job.
///
/// # Safety
///
/// The handle must be valid and not yet closed.
unsafe fn terminate_job(job: windows::Win32::Foundation::HANDLE) {
    use windows::Win32::System::JobObjects::TerminateJobObject;

    let _ = TerminateJobObject(job, 1);
}

#[cfg(test)]
mod tests;
