//! The Win32 seam of process supervision: the hidden-spawn flag, the Job
//! Object owner type, the kill-on-close job helpers, the Toolhelp descendant
//! walk, and the single-instance guard. This is the documented-unsafe cluster
//! and it moves as one unit so reviewers read one file for all Win32
//! invariants.
//!
//! This module holds the project's documented `unsafe` for Win32 interop: every
//! block states its invariant. The single-instance guard (A10, §0.2 step 3) lives
//! here too: a named mutex refuses a second harness.

/// `CREATE_NO_WINDOW`: hidden children (Step 8).
pub(super) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Owned Job Object handle. `Send` because the runtime owns it exclusively: it is
/// created on one task, stored once, and closed exactly once — by [`Drop`].
pub(super) struct JobHandle(pub(super) windows::Win32::Foundation::HANDLE);

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

/// The hard bound on [`descendant_pids_of`]'s answer: a healthy engine
/// family is a handful of pids, so any reachable tree this large is a
/// pathological one and the walk stops rather than spin.
pub(super) const DESCENDANT_CAP: usize = 4_096;

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
pub(super) fn descendant_pids_of(root: u32) -> Vec<u32> {
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
pub(super) unsafe fn create_kill_on_close_job() -> Result<windows::Win32::Foundation::HANDLE, String>
{
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
pub(super) unsafe fn assign_to_job(
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
pub(super) fn resume_primary_thread(pid: u32) -> Result<(), String> {
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
pub(super) unsafe fn terminate_job(job: windows::Win32::Foundation::HANDLE) {
    use windows::Win32::System::JobObjects::TerminateJobObject;

    let _ = TerminateJobObject(job, 1);
}
