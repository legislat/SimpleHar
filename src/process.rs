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
//! Spawns are hidden (`CREATE_NO_WINDOW`) with stdout/stderr captured to
//! `logs/<name>-<epoch>.log`. Engine children receive exactly an OS baseline plus
//! `LaunchSpec.env` (§5, E3): harness secrets (`SIMPLEHAR_TOKEN`) and Python
//! import-hijack vars (`PYTHONPATH`, `VIRTUAL_ENV`) never leak into them. The
//! harness-internal control commands spawned here (`wsl.exe` docker teardown and
//! `docker ps`) are scrubbed the same way, because they execute *inside the
//! operator's distro*: inheriting there would carry the plaintext token across that
//! very boundary (FINDINGS2-0910 [4], closing the asymmetry E3 left). Control
//! commands owned by other modules (`netsh` in `main.rs`, the `nvidia-smi` fallback
//! in `witness.rs`, the Web UI opener in `tray.rs`) still inherit — none of them
//! crosses into WSL — and are the smaller remaining half of that rule.
//! G12 release evidence: the child handle is signaled *and* the port refuses TCP
//! (for containers, the closed port after reaping is the evidence).
//!
//! This module holds the project's documented `unsafe` for Win32 interop: every
//! block states its invariant. The single-instance guard (A10, §0.2 step 3) lives
//! here too: a named mutex refuses a second harness.

use crate::engine::{
    http_probe, wsl_prefix, EngineRuntime, LaunchSpec, Probe, SpawnError, Spawned,
};
use crate::ownership::{
    boot_reap_decision, valid_container_name, ContainerClaims, ReapDecision, ResourceKind,
    SpawnTicket,
};
use async_trait::async_trait;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::{Child, Command};

/// `CREATE_NO_WINDOW`: hidden children (Step 8).
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Poll pause between release-evidence checks inside the deadline.
const RELEASE_POLL: Duration = Duration::from_millis(100);
/// Log hygiene (Step 16): at most this many engine log files survive; the oldest
/// are deleted at spawn time (T14's `logs/` budget).
const LOG_FILE_CAP: usize = 50;

/// Windows env var names (case-insensitive) an engine child may keep (§5 locked
/// decision, E3): external engines obey only harness-owned configuration, so a
/// child's environment is this baseline plus what [`LaunchSpec`] declares — never
/// the harness's own environment with its plaintext `SIMPLEHAR_TOKEN`, its
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
/// ([`OS_ENV_BASELINE`]) and nothing else — no `SIMPLEHAR_TOKEN`, no `PYTHONPATH`,
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
    let mut command = Command::new(program);
    command
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .kill_on_drop(true)
        .env_clear();
    for (key, value) in control_child_env() {
        command.env(key, value);
    }
    match tokio::time::timeout(CONTROL_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => ControlOutcome::Ran(output),
        Ok(Err(error)) => ControlOutcome::Failed(error.to_string()),
        Err(_) => ControlOutcome::TimedOut,
    }
}

/// Deletes the oldest `*.log` files beyond `keep` in `dir`.
fn prune_log_files(dir: &std::path::Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("log"))
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
            tracing::warn!(error = %error, path = %path.display(), "cannot prune old engine log");
        }
    }
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
    client: reqwest::Client,
    logs_dir: PathBuf,
    wsl_distro: String,
    wsl_user: String,
    /// Test seam: fails the job-object step *after* `command.spawn()` succeeded, so
    /// the post-spawn failure path — the one that must retract a claim this call
    /// minted (FINDINGS2-0910 [3]) — is reachable without a wedged Win32 API.
    #[cfg(test)]
    fail_job_after_spawn: bool,
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

/// Whether this kill must reap a container: only a not-yet-terminated engine
/// carries the reap duty; re-kills (cancel/retry paths) must not re-run
/// `docker rm` for someone else's container (review, 2026-10-26).
fn should_reap(terminated: bool, container: Option<&String>) -> Option<String> {
    if terminated {
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
    /// Nothing to do (no container, or a re-kill that already carried the duty).
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
/// the harness's `SIMPLEHAR_TOKEN` never rides into the operator's distro
/// (FINDINGS2-0910 [4]).
///
/// # Returns
///
/// `true` only when docker confirmed the removal — the boot reclaim drops the
/// ownership claim for a name only on that evidence, so a failed reap stays
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

/// One catalog-declared container candidate for the boot reclaim: the declaring
/// model's `engine:` plus its `ninfer_container:` value. Both travel together so
/// [`boot_reap_decision`] can apply the engine gate and the name gate at boot,
/// while the claim gate reads the ledger the spawn path wrote (chunk 10 [1]).
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

/// `wsl.exe … docker ps --format {{.Names}}`; `None` when the call cannot run or
/// reports failure (the reconcile is advisory, never fatal). A control command like
/// [`reap_container`], so it goes through [`run_control_command`] and carries only
/// the OS baseline.
async fn docker_ps(distro: &str, user: &str) -> Option<Vec<String>> {
    let mut argv = wsl_prefix(distro, user);
    argv.extend([
        "docker".to_string(),
        "ps".to_string(),
        "--format".to_string(),
        "{{.Names}}".to_string(),
    ]);
    let (program, args) = argv
        .split_first()
        .unwrap_or_else(|| unreachable!("invariant: wsl_prefix is never empty"));
    let output = match run_control_command(program, args).await {
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
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect(),
    )
}

impl ProcessRuntime {
    /// Creates the runtime; log files land under `logs/` next to the current exe
    /// (falling back to the working directory).
    pub fn new() -> Self {
        let logs_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("logs")))
            .unwrap_or_else(|| PathBuf::from("logs"));
        ProcessRuntime {
            engines: Arc::new(Mutex::new(HashMap::new())),
            // One ledger for the whole process lifetime, shared by every clone:
            // claimed at spawn, cleared on confirmed release, consulted at boot.
            claims: Arc::new(Mutex::new(ContainerClaims::load(container_claims_path()))),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .expect("invariant: plain-HTTP client with a timeout needs no configuration"),
            logs_dir,
            // No WSL coordinates before boot: they name a machine-specific distro and a
            // personal account, so nothing is assumed. Boot wires both through `with_wsl`,
            // and the reaping guards refuse loudly rather than invoke `wsl.exe` without
            // them (review, 2026-10-26).
            wsl_distro: String::new(),
            wsl_user: String::new(),
            #[cfg(test)]
            fail_job_after_spawn: false,
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

    /// Fails the job-object step after the client started. Test-only; see the
    /// [`Self::fail_job_after_spawn`] field.
    #[cfg(test)]
    #[must_use]
    pub fn with_injected_job_failure(mut self) -> Self {
        self.fail_job_after_spawn = true;
        self
    }

    /// Creates the kill-on-close job for a freshly spawned child. The `#[cfg(test)]`
    /// seam exists so the post-spawn failure path (which must retract a claim) can be
    /// reached without a wedged Win32 API.
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

    /// Retracts the claim this spawn minted, when the spawn is nevertheless reported
    /// as failed after the client started but before it was ever recorded (a job-object
    /// failure): no later `kill`/`release_confirmed` can reach this resource, so the
    /// claim would be permanent evidence for something the harness has disowned
    /// (FINDINGS2-0910 [3]'s belt, on top of minting the claim only after a real
    /// spawn).
    fn retract_claim(&self, name: Option<&str>) {
        if let Some(name) = name {
            self.claims
                .lock()
                .expect("container claim ledger lock")
                .unclaim(name);
        }
    }

    /// Boot reconcile (deploy review, 2026-10-26; scoped by chunk 10 [1], 2026-10-27):
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
        prune_log_files(&self.logs_dir, LOG_FILE_CAP);
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let log_path = self.logs_dir.join(format!("{}-{epoch}.log", spec.log_name));
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
            .creation_flags(CREATE_NO_WINDOW)
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
        // The claim is the positive ownership evidence the boot reclaim demands, so it
        // is minted HERE — only once a client really started (FINDINGS2-0910 [3]).
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
        // From here the client exists but is not yet recorded: these two paths report
        // a failed spawn, so the claim just minted has to go back with the report.
        // Safety: the job handle is created and assigned here, on this task, before
        // the child is stored; `raw_handle` is valid while the child is alive, and the
        // job is closed exactly once, by `JobHandle::drop`.
        let job = match self.create_kill_job() {
            Ok(handle) => JobHandle(handle),
            Err(reason) => {
                self.retract_claim(container.as_deref());
                return Err(SpawnError::SpawnFailed { reason });
            }
        };
        if let Some(handle) = child.raw_handle() {
            let assigned = unsafe { assign_to_job(job.0, handle) };
            if let Err(reason) = assigned {
                let _ = child.kill().await;
                self.retract_claim(container.as_deref());
                return Err(SpawnError::SpawnFailed { reason });
            }
        }
        let pid = child.id().unwrap_or(0);
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
            // Only the first kill carries the reap duty; re-kills (cancel,
            // release-retry) must not re-run `docker rm` (review, 2026-10-26).
            let duty = should_reap(engine.terminated, engine.container.as_ref());
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
                reap_container(&self.wsl_distro, &self.wsl_user, &name).await;
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
        if release_ready(is_container, exited, port_closed) {
            let reaped_name = self
                .engines
                .lock()
                .expect("engine table lock")
                .remove(&ticket.port)
                .and_then(|engine| engine.container); // `engine` drops here:
                                                      // `JobHandle::drop` closes the
                                                      // handle exactly once.
            if let (true, Some(name)) = (is_container, reaped_name) {
                // G12 evidence held: the container is gone, so its ownership claim
                // ends — a later hand-started container reusing the name can never
                // inherit a claim this harness no longer needs.
                self.claims
                    .lock()
                    .expect("container claim ledger lock")
                    .unclaim(&name);
            }
            tracing::info!(
                port = ticket.port,
                "release confirmed: engine gone, port closed"
            );
            return Ok(true);
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

/// The argv positions that carry a model file to the engine: llama takes
/// `-m <path>`, strata takes its profile as `--config <path>` (its `-m` names the
/// Python module, which [`looks_like_local_path`] rejects as a path candidate).
/// A missing file at one of these slots turns the bare exit status of a fast-fail
/// into the file the operator must restore (FINDINGS-13 [2]'s load-path companion);
/// the catalog is never mutated from here (operator ruling — a transient deletion
/// must not silently rewrite the page).
const PATH_FLAGS: &[&str] = &["-m", "--config"];

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
fn early_exit_report(argv: &[String], status: &str) -> (String, Option<String>) {
    let missing = argv.windows(2).find_map(|window| {
        (PATH_FLAGS.contains(&window[0].as_str())
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
            let name = windows::core::w!("SimpleHarSE-SingleInstance");
            let handle = windows::Win32::System::Threading::CreateMutexW(None, false, name)
                .map_err(|e| format!("cannot create mutex: {e}"))?;
            if windows::Win32::Foundation::GetLastError()
                == windows::Win32::Foundation::ERROR_ALREADY_EXISTS
            {
                let _ = windows::Win32::Foundation::CloseHandle(handle);
                return Err("another SimpleHar SE instance is already running".to_string());
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
mod tests {
    use super::*;
    use crate::engine::ReadinessSpec;
    use crate::ownership::{SKIP_INVALID_NAME, SKIP_NOT_NINFER, SKIP_UNCLAIMED};

    #[test]
    fn a_stale_ticket_never_owns_the_port_it_renames() {
        assert_eq!(table_verdict(None, 7), TableVerdict::Unowned);
        assert_eq!(table_verdict(Some(7), 7), TableVerdict::Ours);
        assert_eq!(
            table_verdict(Some(8), 7),
            TableVerdict::Stale { found: 8 },
            "the previous ticket must not be able to kill, or confirm away, the successor"
        );
    }

    #[test]
    fn container_rm_uses_the_shared_wsl_prefix() {
        let (program, args) = container_rm("Ubuntu", "ops", "ninfer-x");
        assert_eq!(program, "wsl.exe");
        assert_eq!(
            [vec![program], args].concat(),
            vec!["wsl.exe", "-d", "Ubuntu", "-u", "ops", "--", "docker", "rm", "-f", "ninfer-x",]
        );
    }

    #[test]
    fn only_the_first_kill_carries_the_reap_duty() {
        let name = "ninfer-x".to_string();
        assert_eq!(should_reap(false, Some(&name)), Some(name.clone()));
        assert_eq!(
            should_reap(true, Some(&name)),
            None,
            "re-kill must not re-rm"
        );
        assert_eq!(
            should_reap(false, None),
            None,
            "process engines reap nothing"
        );
    }

    #[test]
    fn release_evidence_rules() {
        // Process engines: exit + closed port (G12 pair). Containers: the client
        // exit proves nothing; the closed port after reaping is the evidence.
        assert!(release_ready(false, true, true));
        assert!(!release_ready(false, true, false));
        assert!(!release_ready(false, false, true));
        assert!(release_ready(true, false, true));
        assert!(!release_ready(true, false, false));
    }

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn early_exit_names_the_missing_llama_model_file() {
        let dir = crate::testutil::temp_dir("early-exit");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let spec_argv = argv(&[
            "llama-server.exe",
            "-m",
            "Z:\\definitely-not-here\\doomed.gguf",
            "--host",
            "127.0.0.1",
            "--port",
            "18199",
        ]);
        let (reason, missing) = early_exit_report(&spec_argv, "exit code: 1");
        assert_eq!(
            reason,
            "model file not found: Z:\\definitely-not-here\\doomed.gguf (engine exited early: \
             exit code: 1)"
        );
        assert_eq!(
            missing.as_deref(),
            Some("Z:\\definitely-not-here\\doomed.gguf")
        );

        // A profile-shaped strata argv names its --config when the profile is gone,
        // and never the Python module token after `-m`.
        let strata = argv(&[
            "python.exe",
            "-m",
            "serve.server",
            "--engine",
            "strata",
            "--config",
            "Z:\\nope\\profile.json",
            "--port",
            "18199",
        ]);
        let (reason, missing) = early_exit_report(&strata, "exit code: 1");
        assert!(
            reason.starts_with("model file not found: Z:\\nope\\profile.json"),
            "{reason}"
        );
        assert!(
            !reason.contains("serve.server"),
            "the module is not a model path"
        );
        assert_eq!(missing.as_deref(), Some("Z:\\nope\\profile.json"));

        // An existing file must NOT be named: the reason stays the bare status.
        let real = dir.join("here.gguf");
        std::fs::write(&real, b"x").expect("seed");
        let ok_argv = argv(&["llama-server.exe", "-m", real.to_str().expect("utf8")]);
        let (reason, missing) = early_exit_report(&ok_argv, "exit code: 3");
        assert_eq!(reason, "engine process exited early (exit code: 3)");
        assert_eq!(missing, None);

        // `scheme://` sentinels name no local file: exempt, bare status.
        let sentinel = argv(&["llama-server.exe", "-m", "hf://org/repo/x.gguf"]);
        let (reason, missing) = early_exit_report(&sentinel, "0x1");
        assert_eq!(reason, "engine process exited early (0x1)");
        assert_eq!(missing, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stderr_excerpt_is_bounded_and_char_safe() {
        assert_eq!(excerpt(b"short"), "short");
        assert_eq!(excerpt("x".repeat(400).as_bytes()).len(), 303);
        let unicode = "あ".repeat(200); // 600 bytes: must cut on a char boundary
        let cut = excerpt(unicode.as_bytes());
        assert!(cut.chars().all(|c| c == 'あ' || c == '.'), "{cut}");
        assert!(cut.len() <= 303);
    }

    #[test]
    fn log_pruning_keeps_the_newest_files() {
        let dir = crate::testutil::temp_dir("logprune");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp logs dir");
        for i in 0..6 {
            let path = dir.join(format!("engine-{i}.log"));
            std::fs::write(&path, b"x").expect("seed log");
            // Distinct mtimes: force aging by touching the metadata via sleep is
            // flaky; instead write times backwards is not portable - use file
            // order by setting modified times explicitly on unix only. Here we
            // rely on the prune test using names as a proxy: oldest = lowest i.
            let _ = i;
            std::thread::sleep(Duration::from_millis(10));
        }
        prune_log_files(&dir, 3);
        let remaining: Vec<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(remaining.len(), 3, "cap enforced: {remaining:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn claims_with(names: &[&str]) -> ContainerClaims {
        let dir = crate::testutil::temp_dir("claims-plan");
        let mut claims = ContainerClaims::load(dir.join("ninfer-claims.json"));
        for name in names {
            assert!(claims.claim(name), "test seed name must be valid");
        }
        claims
    }

    fn candidate(engine: &str, name: &str) -> ContainerCandidate {
        ContainerCandidate {
            engine: engine.to_string(),
            name: name.to_string(),
        }
    }

    fn verdicts(plan: &[(ContainerCandidate, ReapDecision)]) -> Vec<(&str, Option<&'static str>)> {
        plan.iter()
            .map(|(c, d)| {
                (
                    c.name.as_str(),
                    match d {
                        ReapDecision::Reap => None,
                        ReapDecision::Skip { reason } => Some(*reason),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn boot_reclaim_touches_only_catalog_owned_names() {
        // Reaping stays ownership-scoped (§5): a running container is reclaimed only
        // when the operator's own ModelConf declares that exact name — never by
        // prefix, never by wildcard. `someone-elses-container` never appears.
        let present: Vec<String> = ["SE-nin-bonsai", "ninfer-bonsai", "someone-elses-container"]
            .map(String::from)
            .to_vec();
        let claims = claims_with(&["SE-nin-bonsai"]);
        let plan = reclaim_plan(&present, &[candidate("ninfer", "SE-nin-bonsai")], &claims);
        assert_eq!(verdicts(&plan), vec![("SE-nin-bonsai", None)]);
        assert!(reclaim_plan(&present, &[], &claims).is_empty());
    }

    #[test]
    fn boot_reclaim_skips_every_container_lacking_ownership_evidence() {
        // Chunk 10 [1] reproduced: a running container named by SOME yaml is still
        // untouchable without all three gates — each failure SKIPs with its reason.
        // (The invalid name is unclaimable, so the claim gate cannot even be reached:
        // the shape gate fires first and the container is left running.)
        let present: Vec<String> = [
            "ninfer-claimed",
            "ninfer-unclaimed",
            "llama-named",
            "_illegal",
        ]
        .map(String::from)
        .to_vec();
        let claims = claims_with(&["ninfer-claimed", "llama-named"]);
        let plan = reclaim_plan(
            &present,
            &[
                candidate("ninfer", "ninfer-claimed"),
                candidate("ninfer", "ninfer-unclaimed"),
                candidate("llama", "llama-named"),
                candidate("ninfer", "_illegal"),
            ],
            &claims,
        );
        assert_eq!(
            verdicts(&plan),
            vec![
                ("ninfer-claimed", None),
                ("ninfer-unclaimed", Some(SKIP_UNCLAIMED)),
                ("llama-named", Some(SKIP_NOT_NINFER)),
                ("_illegal", Some(SKIP_INVALID_NAME)),
            ]
        );
    }

    #[test]
    fn boot_reclaim_never_reaps_an_id_shaped_name_even_when_a_stale_ledger_holds_it() {
        // Chunk 10 [4] on the boot path: `docker rm -f 1234abc` deletes whichever
        // *unrelated* container's id merely starts with those characters. The shape gate
        // therefore has to outrank any claim an older ledger still carries, so the seed
        // is written behind `claim` (which rightly refuses such names).
        let dir = crate::testutil::temp_dir("claims-stale");
        let path = dir.join("ninfer-claims.json");
        let stale: BTreeSet<String> = ["1234abc".to_string()].into_iter().collect();
        let bytes = serde_json::to_vec(&stale).expect("seed a stale ledger");
        std::fs::write(&path, bytes).expect("write the stale ledger");
        let claims = ContainerClaims::load(&path);
        assert!(
            claims.contains("1234abc"),
            "the hazard is a ledger that does hold the name"
        );
        let present = vec!["1234abc".to_string()];
        let plan = reclaim_plan(&present, &[candidate("ninfer", "1234abc")], &claims);
        assert_eq!(
            verdicts(&plan),
            vec![("1234abc", Some(SKIP_INVALID_NAME))],
            "an ID-shaped name is never destroyed, claim or no claim"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn boot_reclaim_never_judges_a_container_that_is_not_running() {
        // Presence in `docker ps` is the precondition: an absent declared name
        // produces no verdict at all, not even a skip line.
        let claims = claims_with(&["ninfer-a"]);
        let plan = reclaim_plan(&[], &[candidate("ninfer", "ninfer-a")], &claims);
        assert!(plan.is_empty());
    }

    fn container_ticket(port: u16, name: &str) -> SpawnTicket {
        SpawnTicket {
            generation: 1,
            kind: ResourceKind::Container {
                name: name.to_string(),
            },
            pid: 0,
            port,
        }
    }

    /// A spec whose `argv[0]` is `program`; the container identity lives on the
    /// ticket, which is the only place the runtime reads it from.
    fn client_spec(program: &str, args: &[&str], log_name: &str) -> LaunchSpec {
        LaunchSpec {
            argv: std::iter::once(program.to_string())
                .chain(args.iter().map(|arg| arg.to_string()))
                .collect(),
            cwd: std::env::temp_dir(),
            env: BTreeMap::new(),
            log_name: log_name.to_string(),
            readiness: ReadinessSpec::Http {
                path: "/health".to_string(),
                expect_status: 200,
            },
            container: None,
        }
    }

    /// FINDINGS2-0910 [3] (the reviewer's F4): a claim minted at spawn *intent* is
    /// ownership evidence for a client that never existed, and the boot reclaim
    /// reads exactly that evidence as permission to `docker rm -f`. Blank WSL
    /// coordinates keep every reap path from invoking `wsl.exe` (the reapers refuse
    /// and warn), and the ledger is a temp file so the real one beside the test exe
    /// stays untouched. Falsifier: mint the claim before the pre-reap again and this
    /// pin fails on both the in-memory and the on-disk read.
    #[tokio::test]
    async fn a_failed_container_spawn_leaves_no_claim() {
        let dir = crate::testutil::temp_dir("claims-failed-spawn");
        let ledger = dir.join("ninfer-claims.json");
        let runtime = ProcessRuntime::new().with_claims_ledger(&ledger);
        let error = runtime
            .spawn(
                &client_spec("no-such-engine-program-se.exe", &[], "se-probe-1"),
                container_ticket(18_281, "se-probe-1"),
            )
            .await
            .expect_err("a program that does not exist cannot spawn");
        assert!(
            matches!(&error, SpawnError::SpawnFailed { reason }
                if reason.contains("no-such-engine-program-se.exe")),
            "{error}"
        );
        assert!(
            !runtime.claimed("se-probe-1"),
            "a spawn that never happened must leave no ownership evidence"
        );
        assert!(
            !ContainerClaims::load(&ledger).contains("se-probe-1"),
            "and nothing of it may reach the persisted ledger"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The belt on that suspenders: the client started, so the claim was minted —
    /// then the spawn is nevertheless reported failed (here: the job object could not
    /// be created). Nothing can ever `release_confirmed` a resource that was never
    /// recorded, so the claim has to leave with the failure report.
    #[tokio::test]
    async fn a_post_spawn_failure_retracts_the_claim_it_minted() {
        let dir = crate::testutil::temp_dir("claims-retract");
        let ledger = dir.join("ninfer-claims.json");
        let runtime = ProcessRuntime::new()
            .with_claims_ledger(&ledger)
            .with_injected_job_failure();
        let error = runtime
            .spawn(
                &client_spec("cmd.exe", &["/c", "exit 0"], "se-probe-2"),
                container_ticket(18_282, "se-probe-2"),
            )
            .await
            .expect_err("the injected job failure must fail the spawn");
        assert!(
            matches!(&error, SpawnError::SpawnFailed { reason }
                if reason.contains("injected job-object failure")),
            "{error}"
        );
        assert!(
            !runtime.claimed("se-probe-2"),
            "the claim this call minted must go back with the failure report"
        );
        assert!(
            !ContainerClaims::load(&ledger).contains("se-probe-2"),
            "including on disk, where the boot reclaim reads it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The control that keeps the two pins above from being satisfied by a claim
    /// path that never works at all: a client that really started mints the claim
    /// every reap depends on, in memory and on disk.
    #[tokio::test]
    async fn a_started_client_mints_the_claim_the_reaps_depend_on() {
        let dir = crate::testutil::temp_dir("claims-mint");
        let ledger = dir.join("ninfer-claims.json");
        let runtime = ProcessRuntime::new().with_claims_ledger(&ledger);
        let spawned = runtime
            .spawn(
                &client_spec("cmd.exe", &["/c", "exit 0"], "se-probe-3"),
                container_ticket(18_283, "se-probe-3"),
            )
            .await
            .expect("cmd.exe starts as a stand-in client");
        assert_eq!(spawned.port, 18_283);
        assert!(
            runtime.claimed("se-probe-3"),
            "a started client is exactly what the ledger records"
        );
        assert!(
            ContainerClaims::load(&ledger).contains("se-probe-3"),
            "and persists for the next boot to read"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// [3]'s other half: the ticket carries the reap *duty*, the ledger carries the
    /// *authority*, and only their conjunction reaps. The kill path used to reap
    /// straight from the ticket's name.
    #[test]
    fn the_kill_reap_requires_both_the_duty_and_the_claim() {
        let duty = Some("ninfer-a".to_string());
        assert_eq!(
            reap_action(duty.clone(), true),
            ReapAction::Reap("ninfer-a".to_string()),
            "duty plus claim is the only combination that addresses a container"
        );
        assert_eq!(
            reap_action(duty.clone(), false),
            ReapAction::SkipUnclaimed("ninfer-a".to_string()),
            "a duty with no claim behind it leaves the container running"
        );
        assert_eq!(reap_action(None, true), ReapAction::None);
        assert_eq!(
            reap_action(None, false),
            ReapAction::None,
            "a re-kill never reaps, claim or no claim"
        );
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn spec_map(list: &[(&str, &str)]) -> BTreeMap<String, String> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn upper_keys(env: &[(String, String)]) -> Vec<String> {
        env.iter().map(|(key, _)| key.to_uppercase()).collect()
    }

    /// A synthetic harness env carrying every baseline var (mixed spellings where
    /// Windows plausibly carries them) plus the vars that must never reach an
    /// engine child: the plaintext control secret and the Python hijack vars.
    fn secret_bearing_source() -> Vec<(String, String)> {
        pairs(&[
            ("SystemRoot", "C:\\Windows"),
            ("windir", "C:\\Windows"),
            ("COMSPEC", "C:\\Windows\\System32\\cmd.exe"),
            ("Path", "C:\\Windows\\System32"),
            ("PATHEXT", ".COM;.EXE"),
            ("TEMP", "C:\\Temp"),
            ("TMP", "C:\\Temp"),
            ("SIMPLEHAR_TOKEN", "plaintext-secret"),
            ("PYTHONPATH", "C:\\evil\\hijack"),
            ("VIRTUAL_ENV", "C:\\evil\\venv"),
            ("CARGO_MANIFEST_DIR", "C:\\src"),
        ])
    }

    fn count_child_var(dump: &str, key: &str) -> usize {
        let prefix = format!("{}=", key.to_uppercase());
        dump.lines()
            .filter(|line| line.trim_start().to_uppercase().starts_with(&prefix))
            .count()
    }

    fn has_child_var(dump: &str, key: &str) -> bool {
        count_child_var(dump, key) > 0
    }

    #[test]
    fn baseline_keeps_only_the_os_names_and_drops_every_secret() {
        let env = child_env_baseline_from(secret_bearing_source());
        let keys = upper_keys(&env);
        assert_eq!(
            keys.len(),
            OS_ENV_BASELINE.len(),
            "one entry per baseline name, no duplicates: {keys:?}"
        );
        let actual: BTreeSet<String> = keys.iter().cloned().collect();
        let expected: BTreeSet<String> = OS_ENV_BASELINE
            .iter()
            .map(|name| name.to_uppercase())
            .collect();
        assert_eq!(actual, expected, "baseline carries exactly the OS names");
        for secret in [
            "SIMPLEHAR_TOKEN",
            "PYTHONPATH",
            "VIRTUAL_ENV",
            "CARGO_MANIFEST_DIR",
        ] {
            assert!(
                !keys.contains(&secret.to_string()),
                "secret {secret} leaked into the baseline"
            );
        }
    }

    #[test]
    fn baseline_falls_back_for_path_temp_tmp_and_omits_the_rest() {
        let env = child_env_baseline_from(pairs(&[("COMSPEC", "C:\\Windows\\System32\\cmd.exe")]));
        let exe_dir = std::env::current_exe()
            .expect("exe path")
            .parent()
            .expect("exe directory")
            .to_string_lossy()
            .into_owned();
        let temp = std::env::temp_dir().to_string_lossy().into_owned();
        let got: BTreeMap<String, String> = env
            .into_iter()
            .map(|(k, v)| (k.to_uppercase(), v))
            .collect();
        assert_eq!(got.get("PATH").expect("Path always present"), &exe_dir);
        assert_eq!(got.get("TEMP").expect("TEMP always present"), &temp);
        assert_eq!(got.get("TMP").expect("TMP always present"), &temp);
        assert_eq!(
            got.get("COMSPEC").expect("COMSPEC copied"),
            "C:\\Windows\\System32\\cmd.exe"
        );
        for absent in ["SYSTEMROOT", "WINDIR", "PATHEXT"] {
            assert!(!got.contains_key(absent), "{absent} must never be invented");
        }
    }

    #[test]
    fn baseline_collapses_case_duplicate_names_to_one_entry() {
        let env = child_env_baseline_from(pairs(&[
            ("PATH", "C:\\first"),
            ("Path", "C:\\second"),
            ("temp", "C:\\t1"),
            ("TEMP", "C:\\t2"),
        ]));
        let keys = upper_keys(&env);
        assert_eq!(keys.len(), env.len(), "no case-duplicates: {keys:?}");
        let path = env
            .iter()
            .find(|(k, _)| k.to_uppercase() == "PATH")
            .expect("Path present");
        assert_eq!(path.0, "Path", "baseline emits its canonical spelling");
        assert_eq!(path.1, "C:\\first", "first occurrence wins");
    }

    #[test]
    fn spec_env_overrides_the_baseline_case_insensitively() {
        let assembled = engine_child_env(
            secret_bearing_source(),
            &spec_map(&[("PATH", "C:\\engine\\bin"), ("EXTRA_ONE", "1")]),
        );
        let keys = upper_keys(&assembled);
        assert_eq!(keys.len(), assembled.len(), "no case-duplicates: {keys:?}");
        let (name, value) = assembled
            .iter()
            .find(|(k, _)| k.to_uppercase() == "PATH")
            .expect("exactly one Path entry");
        assert_eq!(name, "PATH", "the spec entry keeps its own spelling");
        assert_eq!(
            value, "C:\\engine\\bin",
            "the spec entry replaces the baseline's"
        );
        assert!(
            assembled.contains(&("EXTRA_ONE".to_string(), "1".to_string())),
            "a fresh spec entry is appended"
        );
    }

    #[test]
    fn engine_child_env_keeps_baseline_and_spec_and_drops_everything_else() {
        let assembled = engine_child_env(
            secret_bearing_source(),
            &spec_map(&[("PROBE_MARKER", "seen")]),
        );
        let keys = upper_keys(&assembled);
        for name in OS_ENV_BASELINE {
            assert!(
                keys.contains(&name.to_uppercase()),
                "baseline {name} missing: {keys:?}"
            );
        }
        for secret in [
            "SIMPLEHAR_TOKEN",
            "PYTHONPATH",
            "VIRTUAL_ENV",
            "CARGO_MANIFEST_DIR",
        ] {
            assert!(
                !keys.contains(&secret.to_string()),
                "secret {secret} must never reach a child"
            );
        }
        assert!(
            assembled.contains(&("PROBE_MARKER".to_string(), "seen".to_string())),
            "spec entry must be present"
        );
        assert_eq!(keys.len(), assembled.len(), "no case-duplicates: {keys:?}");
    }

    #[test]
    fn assembled_env_reaches_a_real_child_without_inherited_secrets() {
        use std::os::windows::process::CommandExt as _;
        let mut source: Vec<(String, String)> = std::env::vars().collect();
        source.extend(pairs(&[
            ("SIMPLEHAR_TOKEN", "plaintext-secret"),
            ("PYTHONPATH", "C:\\evil\\hijack"),
        ]));
        let assembled = engine_child_env(source, &spec_map(&[("PROBE_MARKER", "seen")]));
        let dir = crate::testutil::temp_dir("childenv");
        let dump = dir.join("child-set.txt");
        let mut command = std::process::Command::new("cmd.exe");
        command
            .args(["/c", &format!("set > {}", dump.display())])
            .current_dir(&dir)
            .env_clear()
            .creation_flags(CREATE_NO_WINDOW);
        for (key, value) in &assembled {
            command.env(key, value);
        }
        let status = command.status().expect("cmd.exe runs");
        assert!(status.success(), "cmd /c set exited with {status}");
        let text = std::fs::read_to_string(&dump).expect("child wrote its env dump");
        assert!(
            has_child_var(&text, "PROBE_MARKER"),
            "spec entry missing from the child: {text}"
        );
        assert!(
            !has_child_var(&text, "SIMPLEHAR_TOKEN"),
            "secret reached the child: {text}"
        );
        assert!(
            !has_child_var(&text, "PYTHONPATH"),
            "python hijack var reached the child: {text}"
        );
        assert_eq!(
            count_child_var(&text, "Path"),
            1,
            "exactly one Path line in the child env: {text}"
        );
        if std::env::var_os("SystemRoot").is_some() || std::env::var_os("windir").is_some() {
            assert!(
                has_child_var(&text, "SystemRoot") || has_child_var(&text, "windir"),
                "OS locators must survive into the child: {text}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Injects harness-process vars for one test and removes them on scope exit,
    /// including through an assertion panic. Only names no other test in this
    /// binary reads may be injected: the crate reads just `SIMPLEHAR_LOG`,
    /// `SIMPLEHAR_TOKEN` and `SIMPLEHAR_CONFIG`, never `PYTHONPATH`/`VIRTUAL_ENV`,
    /// and integration suites are separate processes. `std::env::set_var` is a
    /// safe call in edition 2021 — an `unsafe` block here would deny the build
    /// with `unused_unsafe` under `-D warnings`.
    struct HarnessEnvGuard(Vec<String>);

    impl HarnessEnvGuard {
        fn set(list: &[(&str, &str)]) -> Self {
            let mut keys = Vec::with_capacity(list.len());
            for (key, value) in list {
                std::env::set_var(key, value);
                keys.push((*key).to_string());
            }
            Self(keys)
        }
    }

    impl Drop for HarnessEnvGuard {
        fn drop(&mut self) {
            for key in &self.0 {
                std::env::remove_var(key);
            }
        }
    }

    /// Polls until the child's redirected `set` output exists and stops changing
    /// (the file is created at redirect time, written when cmd flushes).
    async fn wait_for_child_env_dump(path: &std::path::Path) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut previous = String::new();
        loop {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            if !text.is_empty() && text == previous {
                return text;
            }
            previous = text;
            assert!(
                std::time::Instant::now() < deadline,
                "the child `set` dump never stabilized at {}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn engine_spawn_serves_only_the_declared_environment() {
        // The production path end to end: `ProcessRuntime::spawn` (the single
        // LaunchSpec-driven spawn site) must hand the child baseline + spec only.
        // Neutralising its `env_clear()` fails the PYTHONPATH/VIRTUAL_ENV/CARGO_
        // assertions below, because these markers genuinely live in the harness
        // process while the child spawns.
        let _markers = HarnessEnvGuard::set(&[
            ("PYTHONPATH", "C:\\evil\\hijack"),
            ("VIRTUAL_ENV", "C:\\evil\\venv"),
        ]);
        let dir = crate::testutil::temp_dir("enginespawn");
        let dump = dir.join("child-set.txt");
        let runtime = ProcessRuntime::new();
        let spec = LaunchSpec {
            argv: vec![
                "cmd.exe".to_string(),
                "/c".to_string(),
                format!("set > {}", dump.display()),
            ],
            cwd: dir.clone(),
            env: spec_map(&[("PROBE_MARKER", "seen")]),
            log_name: "engine-env-test".to_string(),
            readiness: ReadinessSpec::Http {
                path: "/health".to_string(),
                expect_status: 200,
            },
            container: None,
        };
        let tk = SpawnTicket {
            generation: 7,
            kind: ResourceKind::ChildProcess,
            pid: 0,
            port: 18_195,
        };
        let spawned = runtime
            .spawn(&spec, tk.clone())
            .await
            .expect("cmd.exe spawns as an engine child");
        assert_eq!(spawned.port, 18_195);
        let text = wait_for_child_env_dump(&dump).await;
        assert!(
            has_child_var(&text, "PROBE_MARKER"),
            "spec entry missing from the engine child: {text}"
        );
        assert!(
            has_child_var(&text, "Path"),
            "baseline Path missing from the engine child: {text}"
        );
        for secret in ["SIMPLEHAR_TOKEN", "PYTHONPATH", "VIRTUAL_ENV"] {
            assert!(
                !has_child_var(&text, secret),
                "engine child inherited harness {secret}: {text}"
            );
        }
        let inherited_toolchain: Vec<&str> = text
            .lines()
            .filter(|line| line.trim_start().to_uppercase().starts_with("CARGO_"))
            .collect();
        assert!(
            inherited_toolchain.is_empty(),
            "engine child inherited harness toolchain vars: {inherited_toolchain:?}"
        );
        if std::env::var_os("SystemRoot").is_some() || std::env::var_os("windir").is_some() {
            assert!(
                has_child_var(&text, "SystemRoot") || has_child_var(&text, "windir"),
                "OS locators must survive into the engine child: {text}"
            );
        }
        runtime.kill(&tk).await.expect("cleanup kill runs");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDINGS2-0910 [4], the assembly rule: a control command gets exactly the OS
    /// baseline. The test process genuinely carries `CARGO_*` (and a real harness run
    /// carries `SIMPLEHAR_TOKEN`), so "exactly the baseline names" is a filter that
    /// has to be working for this to hold.
    #[test]
    fn control_child_env_is_the_baseline_and_nothing_else() {
        let env = control_child_env();
        let keys = upper_keys(&env);
        let expected: BTreeSet<String> = OS_ENV_BASELINE
            .iter()
            .map(|name| name.to_uppercase())
            .collect();
        let actual: BTreeSet<String> = keys.iter().cloned().collect();
        assert_eq!(
            actual, expected,
            "one entry per baseline name, nothing else"
        );
        assert_eq!(keys.len(), env.len(), "no case-duplicates: {keys:?}");
        for name in &keys {
            assert!(
                !name.starts_with("SIMPLEHAR_") && !name.starts_with("CARGO_"),
                "secret/toolchain name {name} reached the control assembly"
            );
        }
    }

    /// FINDINGS2-0910 [4], the production site: `run_control_command` is the ONE
    /// builder both WSL control commands go through, so this drives it directly with
    /// a real child — exactly how the engine-child pin drives `ProcessRuntime::spawn`.
    /// The load-bearing marker is `CARGO_*`: cargo always sets those in the test
    /// process, they are not on the baseline, and reading them from the harness env
    /// is something no other test does (unlike `SIMPLEHAR_TOKEN`, which the api layer
    /// reads). Falsifier: drop the `env_clear()` here and the toolchain assertions
    /// below fail.
    #[tokio::test]
    async fn a_control_command_child_carries_no_harness_environment() {
        let dir = crate::testutil::temp_dir("controlenv");
        let dump = dir.join("child-set.txt");
        let args = vec!["/c".to_string(), format!("set > {}", dump.display())];
        match run_control_command("cmd.exe", &args).await {
            ControlOutcome::Ran(output) => {
                assert!(output.status.success(), "cmd /c set exited with {output:?}")
            }
            other => panic!("the control command could not run: {other:?}"),
        }
        let text = wait_for_child_env_dump(&dump).await;
        for forbidden in ["SIMPLEHAR_TOKEN", "PYTHONPATH", "VIRTUAL_ENV"] {
            assert!(
                !has_child_var(&text, forbidden),
                "control child inherited harness {forbidden}: {text}"
            );
        }
        let inherited_toolchain: Vec<String> = text
            .lines()
            .filter(|line| line.trim_start().to_uppercase().starts_with("CARGO_"))
            .map(|line| line.trim().to_string())
            .collect();
        assert!(
            inherited_toolchain.is_empty(),
            "control child inherited the harness toolchain environment: \
             {inherited_toolchain:?}"
        );
        assert_eq!(
            count_child_var(&text, "Path"),
            1,
            "exactly one Path line for `wsl.exe` to resolve itself: {text}"
        );
        if std::env::var_os("SystemRoot").is_some() || std::env::var_os("windir").is_some() {
            assert!(
                has_child_var(&text, "SystemRoot") || has_child_var(&text, "windir"),
                "OS locators must survive into the control child: {text}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
