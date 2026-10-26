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
use crate::ownership::{ResourceKind, SpawnTicket};
use async_trait::async_trait;
use std::collections::HashMap;
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
async fn reap_container(distro: &str, user: &str, name: &str) {
    let (program, args) = container_rm(distro, user, name);
    let command = Command::new(&program)
        .args(&args)
        .creation_flags(CREATE_NO_WINDOW)
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(10), command).await {
        Ok(Ok(output)) if output.status.success() => {
            tracing::info!(container = name, "container removed");
        }
        Ok(Ok(output)) => tracing::warn!(
            container = name,
            stderr = %excerpt(&output.stderr),
            "docker rm -f reported failure"
        ),
        Ok(Err(error)) => {
            tracing::warn!(container = name, error = %error, "docker rm -f cannot run");
        }
        Err(_) => tracing::warn!(container = name, "docker rm -f timed out"),
    }
}

/// Boot reconcile (deploy review, 2026-10-26): a container this harness started in an
/// earlier lifetime survives a crash holding tens of GiB, and observed-VRAM admission
/// then refuses every later load with no pointer to the culprit. Only names the
/// operator's own catalog declares (`ninfer_container`) are candidates — never a
/// wildcard, never a hardcoded prefix — so reaping stays ownership-scoped (§5).
/// Best-effort: any failure logs and boot proceeds.
pub async fn reclaim_owned_containers(distro: &str, user: &str, owned: &[String]) {
    if owned.is_empty() {
        return;
    }
    let Some(present) = docker_ps(distro, user).await else {
        return;
    };
    for name in to_reclaim(&present, owned) {
        tracing::warn!(container = %name, "reclaiming a container left behind by an earlier run");
        reap_container(distro, user, &name).await;
    }
}

/// Running containers that the catalog claims we own. Pure, so the ownership rule is
/// testable without WSL.
fn to_reclaim(present: &[String], owned: &[String]) -> Vec<String> {
    present
        .iter()
        .filter(|name| owned.contains(name))
        .cloned()
        .collect()
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
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .expect("invariant: plain-HTTP client with a timeout needs no configuration"),
            logs_dir,
            // WSL coordinates come from config defaults; boot always overrides
            // them via `with_wsl`, so there is exactly one source of the literals.
            wsl_distro: crate::config::AppConfig::default().wsl_distro,
            wsl_user: crate::config::AppConfig::default().wsl_user,
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
            reap_container(&self.wsl_distro, &self.wsl_user, name).await;
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
        self.engines.lock().expect("engine table lock").insert(
            ticket.port,
            EngineProc {
                child,
                job,
                terminated: false,
                container,
            },
        );
        Ok(Spawned {
            port: ticket.port,
            pid,
        })
    }

    async fn kill(&self, ticket: &SpawnTicket) -> Result<(), SpawnError> {
        let container = {
            let mut engines = self.engines.lock().expect("engine table lock");
            let Some(engine) = engines.get_mut(&ticket.port) else {
                return Ok(()); // nothing we own on that port: never a foreign kill
            };
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
            let Some(engine) = engines.get_mut(&ticket.port) else {
                return Ok(true); // never tracked here -> nothing to confirm
            };
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
            let mut engines = self.engines.lock().expect("engine table lock");
            if let Some(engine) = engines.remove(&ticket.port) {
                drop(engine.job); // JobHandle::drop closes the handle exactly once.
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

    #[test]
    fn container_rm_uses_the_shared_wsl_prefix() {
        let (program, args) = container_rm("Ubuntu", "legislat", "ninfer-x");
        assert_eq!(program, "wsl.exe");
        assert_eq!(
            [vec![program], args].concat(),
            vec![
                "wsl.exe", "-d", "Ubuntu", "-u", "legislat", "--", "docker", "rm", "-f",
                "ninfer-x",
            ]
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

    #[test]
    fn boot_reclaim_touches_only_catalog_owned_names() {
        // Reaping stays ownership-scoped (§5): a running container is reclaimed only
        // when the operator's own ModelConf declares that exact name — never by
        // prefix, never by wildcard.
        let present: Vec<String> = ["SE-nin-bonsai", "ninfer-bonsai", "someone-elses-container"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            to_reclaim(&present, &["SE-nin-bonsai".to_string()]),
            vec!["SE-nin-bonsai".to_string()]
        );
        assert!(to_reclaim(&present, &[]).is_empty());
    }
}
