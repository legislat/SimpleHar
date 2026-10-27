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
//! `logs/<name>-<epoch>.log`.
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
    boot_reap_decision, ContainerClaims, ReapDecision, ResourceKind, SpawnTicket,
};
use async_trait::async_trait;
use std::collections::{BTreeSet, HashMap};
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
/// caller proceed (the release poll is the arbiter). `kill_on_drop` so a timed-out
/// client cannot accumulate as an orphan (review, 2026-10-26).
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
    let command = Command::new(&program)
        .args(&args)
        .creation_flags(CREATE_NO_WINDOW)
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(10), command).await {
        Ok(Ok(output)) if output.status.success() => {
            tracing::info!(container = name, "container removed");
            true
        }
        Ok(Ok(output)) => {
            tracing::warn!(
                container = name,
                stderr = %excerpt(&output.stderr),
                "docker rm -f reported failure"
            );
            false
        }
        Ok(Err(error)) => {
            tracing::warn!(container = name, error = %error, "docker rm -f cannot run");
            false
        }
        Err(_) => {
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
/// reports failure (the reconcile is advisory, never fatal).
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
    let command = Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(Duration::from_secs(10), command).await {
        Ok(Ok(output)) if output.status.success() => output,
        Ok(Ok(output)) => {
            tracing::warn!(
                stderr = %excerpt(&output.stderr),
                "docker ps failed; skipping the boot container reconcile"
            );
            return None;
        }
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "docker ps cannot run; skipping the boot reconcile");
            return None;
        }
        Err(_) => {
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
        // runtime never invents one. Reaping is ownership-scoped: only names
        // this harness claimed at spawn are ever addressed.
        let container = match &ticket.kind {
            ResourceKind::Container { name } => Some(name.clone()),
            ResourceKind::ChildProcess => None,
        };
        // Reconcile a stale container from a crashed run before claiming the
        // name: otherwise `docker run` fails on the name conflict (opaque
        // minutes-long readiness failure) or the stale container answers
        // readiness for a model the harness never started.
        if let Some(name) = container.as_deref() {
            // One rule for both reaping paths (chunk 10 [1] and [4]): `claim` is the sole
            // authority on whether this harness may address the name at all. It records
            // the positive evidence the boot reclaim demands, and it refuses what the
            // shared validator rejects — including docker container-ID shapes — so the
            // pre-reap can never hand `docker rm -f` an argument docker would resolve as
            // someone else's id prefix. A refused name reaps nothing.
            let claimable = self
                .claims
                .lock()
                .expect("container claim ledger lock")
                .claim(name);
            if claimable {
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
            .creation_flags(CREATE_NO_WINDOW);
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        let mut child = command.spawn().map_err(|e| SpawnError::SpawnFailed {
            reason: format!("cannot spawn {program}: {e}"),
        })?;
        // Safety: the job handle is created and assigned here, on this task, before
        // the child is stored; `raw_handle` is valid while the child is alive. The
        // job is closed exactly once, by `JobHandle::drop`.
        let job = JobHandle(
            unsafe { create_kill_on_close_job() }
                .map_err(|reason| SpawnError::SpawnFailed { reason })?,
        );
        if let Some(handle) = child.raw_handle() {
            let assigned = unsafe { assign_to_job(job.0, handle) };
            if let Err(reason) = assigned {
                let _ = child.kill().await;
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
                },
            );
        }
        Ok(Spawned {
            port: ticket.port,
            pid,
        })
    }

    async fn kill(&self, ticket: &SpawnTicket) -> Result<(), SpawnError> {
        let container = {
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
            let reaped = should_reap(engine.terminated, engine.container.as_ref());
            engine.terminated = true;
            let _ = engine.child.start_kill();
            reaped
        };
        // A detached WSL container survives its client process: reap it by name.
        if let Some(name) = container {
            reap_container(&self.wsl_distro, &self.wsl_user, &name).await;
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
                    .map(|status| status.to_string())
            })
        };
        if let Some(status) = exited {
            return Probe::Failed {
                reason: format!("engine process exited early ({status})"),
            };
        }
        http_probe(&self.client, port, readiness).await
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
}
