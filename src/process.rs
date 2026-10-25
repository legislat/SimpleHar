//! Process supervision (DESIGN Step 8, LOCKED C+D, item 6 approved).
//!
//! One Win32 Job Object per owned engine tree with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: if the harness dies for any reason, the
//! kernel kills the whole tree — the crash-proof orphan guarantee. Spawns are hidden
//! (`CREATE_NO_WINDOW`) with stdout/stderr captured to `logs/<name>-<epoch>.log`.
//! G12 release evidence: the child handle is signaled *and* the port refuses TCP.
//!
//! This module holds the project's documented `unsafe` for Win32 interop: every
//! block states its invariant. The single-instance guard (A10, §0.2 step 3) lives
//! here too: a named mutex refuses a second harness.

use crate::engine::{http_probe, EngineRuntime, LaunchSpec, Probe, SpawnError, Spawned};
use crate::ownership::SpawnTicket;
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

/// One tracked engine tree: the child plus its Job Object.
struct EngineProc {
    child: Child,
    /// Win32 job handle; closing/terminating it kills the whole tree.
    job: JobHandle,
    /// Set once the kill path ran; the entry stays until release is confirmed.
    terminated: bool,
}

/// Owned Job Object handle. `Send` because the runtime owns it exclusively: it is
/// created on one task, stored once, and closed exactly once by kill/cleanup.
struct JobHandle(windows::Win32::Foundation::HANDLE);

// Safety: exclusive ownership as documented on the struct; Win32 job handles are
// usable from any thread and we never duplicate or share this one.
unsafe impl Send for JobHandle {}

/// The production `EngineRuntime`: real hidden child processes under Job Objects.
#[derive(Clone)]
pub struct ProcessRuntime {
    engines: Arc<Mutex<HashMap<u16, EngineProc>>>,
    client: reqwest::Client,
    logs_dir: PathBuf,
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
        if let Err(error) = tokio::fs::create_dir_all(&self.logs_dir).await {
            return Err(SpawnError::SpawnFailed {
                reason: format!("cannot create {}: {error}", self.logs_dir.display()),
            });
        }
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
        // job outlives the child entry and is closed only in `kill`/cleanup.
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
            },
        );
        Ok(Spawned {
            port: ticket.port,
            pid,
        })
    }

    async fn kill(&self, ticket: &SpawnTicket) -> Result<(), SpawnError> {
        let mut engines = self.engines.lock().expect("engine table lock");
        let Some(engine) = engines.get_mut(&ticket.port) else {
            return Ok(()); // nothing we own on that port: never a foreign kill
        };
        // Safety: the job handle is valid (created in spawn, closed only here or in
        // release cleanup) and terminating a job is safe from any thread.
        unsafe { terminate_job(engine.job.0) };
        engine.terminated = true;
        let _ = engine.child.start_kill();
        Ok(())
    }

    async fn release_confirmed(&self, ticket: &SpawnTicket) -> Result<bool, SpawnError> {
        let exited = {
            let mut engines = self.engines.lock().expect("engine table lock");
            let Some(engine) = engines.get_mut(&ticket.port) else {
                return Ok(true); // never tracked here -> nothing to confirm
            };
            if !engine.terminated {
                return Ok(false);
            }
            engine
                .child
                .try_wait()
                .map(|status| status.is_some())
                .map_err(|e| SpawnError::KillFailed {
                    holder: format!("port {}", ticket.port),
                    reason: e.to_string(),
                })?
        };
        // Handle signaled; now the port must refuse TCP (G12 evidence pair).
        let port_closed = tokio::net::TcpStream::connect(("127.0.0.1", ticket.port))
            .await
            .is_err();
        if exited && port_closed {
            let mut engines = self.engines.lock().expect("engine table lock");
            if let Some(engine) = engines.remove(&ticket.port) {
                // Safety: last owner of the handle; the tree is already dead.
                unsafe { close_handle(engine.job.0) };
            }
            tracing::info!(
                port = ticket.port,
                "release confirmed: child exited, port closed"
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
        let exited = {
            let mut engines = self.engines.lock().expect("engine table lock");
            engines.get_mut(&port).and_then(|engine| {
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
    #[allow(dead_code)] // the handle itself is the lock; Drop closes it
    handle: windows::Win32::Foundation::HANDLE,
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

/// Closes a handle exactly once.
///
/// # Safety
///
/// The handle must be valid and owned by the caller.
unsafe fn close_handle(handle: windows::Win32::Foundation::HANDLE) {
    let _ = windows::Win32::Foundation::CloseHandle(handle);
}
