use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle as StdOwnedHandle, RawHandle};
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[cfg(test)]
#[path = "../../tests/support/readiness_marker.rs"]
mod readiness_marker;

use winapi::shared::minwindef::{BOOL, DWORD, FALSE, TRUE};
use winapi::um::fileapi::{CreateFileW, OPEN_EXISTING};
use winapi::um::handleapi::{CloseHandle, DuplicateHandle, INVALID_HANDLE_VALUE};
use winapi::um::jobapi2::{AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject};
#[cfg(test)]
use windows_sys::Win32::Foundation::HANDLE as WindowsHandle;
#[cfg(test)]
use windows_sys::Win32::System::JobObjects::IsProcessInJob;
use winapi::um::minwinbase::SECURITY_ATTRIBUTES;
use winapi::um::namedpipeapi::CreateNamedPipeW;
use winapi::um::processenv::GetStdHandle;
use winapi::um::processthreadsapi::{
    CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess, GetCurrentProcessId,
    GetExitCodeProcess, InitializeProcThreadAttributeList, ResumeThread, TerminateProcess,
    UpdateProcThreadAttribute, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
};
#[cfg(test)]
use winapi::um::processthreadsapi::OpenProcess;
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, DETACHED_PROCESS, EXTENDED_STARTUPINFO_PRESENT,
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, INFINITE, PIPE_ACCESS_INBOUND,
    PIPE_ACCESS_OUTBOUND, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_WAIT, STARTF_USESTDHANDLES, STARTUPINFOEXW, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE, WAIT_OBJECT_0,
};
use winapi::um::winnt::{
    JobObjectExtendedLimitInformation, DUPLICATE_SAME_ACCESS, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GENERIC_READ, GENERIC_WRITE, HANDLE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY,
};
#[cfg(test)]
use winapi::um::winnt::SYNCHRONIZE;

// PROC_THREAD_ATTRIBUTE_HANDLE_LIST = ProcThreadAttributeValue(2, FALSE,
// TRUE, FALSE) = 0x00020002.
const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x00020002;
const STILL_ACTIVE: u32 = 259;
/// A containment cleanup must not wedge the host if Windows fails to honor a
/// Job close or process termination. Worker-process escalation owns any later
/// retry/reaping policy.
#[allow(dead_code)] // exercised by the native containment regression; worker wiring follows in #28.
const STRICT_WORKER_REAP_TIMEOUT_MS: DWORD = 5_000;
#[allow(dead_code)]
const WAIT_TIMEOUT_RESULT: DWORD = 0x0000_0102;
#[cfg(test)]
static STRICT_WORKER_FORCE_REAP_TIMEOUT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static STRICT_WORKER_FORCE_ASSIGN_DENIED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub struct OwnedHandle(HANDLE);

impl OwnedHandle {
    pub fn as_raw(&self) -> HANDLE {
        self.0
    }

    /// Take the raw handle without dropping it (caller owns it now).
    pub fn into_raw(self) -> HANDLE {
        let h = self.0;
        std::mem::forget(self);
        h
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

// ── Typed pipe handles ──────────────────────────────────────────────────────
//
// The bug fixed in issue #115 was: an `OwnedHandle` produced by `CreatePipe`
// (synchronous-only) was passed to `ChildStdout::from(...)` (which uses
// overlapped/alertable I/O via `alertable_io_internal`). The mismatch
// silently drops every write after the first.
//
// The fix below makes that mismatch a compile error:
//
// - `OverlappedHandle` — a `HANDLE` *known* to support overlapped I/O.
//   Construction is restricted to [`create_pipe_pair`], which creates the
//   parent end via `CreateNamedPipeW(... FILE_FLAG_OVERLAPPED ...)`.
//   `ChildStdin` / `ChildStdout` / `ChildStderr` can ONLY be built from
//   an `OverlappedHandle`.
//
// - `SyncHandle` — a `HANDLE` *known* to be opened without
//   `FILE_FLAG_OVERLAPPED`. Safe to give a spawned child as a stdio slot.
//
// Both wrap a raw [`OwnedHandle`] (so they own & close it on Drop) and
// expose no public conversion that would let a caller bypass the
// invariant.

/// A handle known to support overlapped (asynchronous) I/O.
///
/// The only constructor is [`create_pipe_pair`]; the only consumers are
/// [`OverlappedHandle::into_child_stdin`] / `..._stdout` / `..._stderr`.
/// This guarantees that any `ChildStdin` / `ChildStdout` / `ChildStderr`
/// we hand back to a caller wraps a handle that Rust stdlib's
/// `alertable_io_internal` reader can actually drive without silently
/// dropping writes (issue #115).
pub struct OverlappedHandle(OwnedHandle);

impl OverlappedHandle {
    pub fn into_child_stdin(self) -> ChildStdin {
        let raw = self.0.into_raw() as RawHandle;
        let owned = unsafe { StdOwnedHandle::from_raw_handle(raw) };
        ChildStdin::from(owned)
    }

    pub fn into_child_stdout(self) -> ChildStdout {
        let raw = self.0.into_raw() as RawHandle;
        let owned = unsafe { StdOwnedHandle::from_raw_handle(raw) };
        ChildStdout::from(owned)
    }

    pub fn into_child_stderr(self) -> ChildStderr {
        let raw = self.0.into_raw() as RawHandle;
        let owned = unsafe { StdOwnedHandle::from_raw_handle(raw) };
        ChildStderr::from(owned)
    }
}

/// A handle known to be opened **without** `FILE_FLAG_OVERLAPPED`.
///
/// Suitable for the child end of a pipe: the child uses synchronous
/// `ReadFile` / `WriteFile` via plain `println!` / `stdin.read_line(..)`.
/// Passing a `SyncHandle` to anything that expects overlapped I/O
/// (Rust's `ChildStdin` / `ChildStdout` / `ChildStderr`, for instance)
/// is impossible by construction — there is no public conversion.
pub struct SyncHandle(OwnedHandle);

impl SyncHandle {
    /// Drop the `SyncHandle` claim and recover the raw owner.
    ///
    /// Used internally when the child-side handle is about to be passed
    /// to `CreateProcessW` via `STARTUPINFOEXW.hStdInput` / `hStdOutput`
    /// / `hStdError` — at that point we no longer need the type-level
    /// proof, only the inheritable raw `HANDLE`.
    fn into_owned(self) -> OwnedHandle {
        self.0
    }
}

/// Direction parameter for [`create_pipe_pair`].
#[derive(Clone, Copy)]
enum PipeDir {
    /// stdin: parent writes, child reads.
    ParentWritesChildReads,
    /// stdout/stderr: child writes, parent reads.
    ChildWritesParentReads,
}

fn open_nul(write: bool) -> io::Result<OwnedHandle> {
    let path: Vec<u16> = OsStr::new("NUL")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut sa: SECURITY_ATTRIBUTES = unsafe { std::mem::zeroed() };
    sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as DWORD;
    sa.bInheritHandle = TRUE as BOOL;
    let access = if write { GENERIC_WRITE } else { GENERIC_READ };
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &mut sa as *mut SECURITY_ATTRIBUTES,
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(h))
}

/// Create a pipe pair whose parent end supports overlapped I/O (so Rust's
/// `ChildStdin` / `ChildStdout` / `ChildStderr` — which use `ReadFileEx` +
/// alertable `SleepEx` via `alertable_io_internal` — can drive it without
/// silently dropping writes after the first transfer) and whose child end
/// is synchronous + inheritable.
///
/// Returns `(parent_end, child_end)` with the proof carried in the types:
/// [`OverlappedHandle`] for the parent side (only thing that can be
/// turned into a `ChildStdin/Stdout/Stderr`), [`SyncHandle`] for the
/// child side (which goes into `STARTUPINFOEXW`).
fn create_pipe_pair(dir: PipeDir) -> io::Result<(OverlappedHandle, SyncHandle)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static PIPE_COUNTER: AtomicU64 = AtomicU64::new(0);

    let counter = PIPE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = unsafe { GetCurrentProcessId() };
    let name = format!(r"\\.\pipe\running-process-{pid}-{counter}");
    let name_w: Vec<u16> = OsStr::new(&name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let (parent_open_mode, child_access) = match dir {
        PipeDir::ParentWritesChildReads => (PIPE_ACCESS_OUTBOUND, GENERIC_READ),
        PipeDir::ChildWritesParentReads => (PIPE_ACCESS_INBOUND, GENERIC_WRITE),
    };

    // Parent end: overlapped/async, not inheritable (default SA = NULL).
    let parent = unsafe {
        CreateNamedPipeW(
            name_w.as_ptr(),
            parent_open_mode | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,         // nMaxInstances: only one client
            64 * 1024, // nOutBufferSize
            64 * 1024, // nInBufferSize
            0,         // nDefaultTimeOut: use default (50 ms)
            std::ptr::null_mut(),
        )
    };
    if parent.is_null() || parent == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let parent = OverlappedHandle(OwnedHandle(parent));

    // Child end: synchronous, inheritable.
    let mut child_sa: SECURITY_ATTRIBUTES = unsafe { std::mem::zeroed() };
    child_sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as DWORD;
    child_sa.bInheritHandle = TRUE as BOOL;
    child_sa.lpSecurityDescriptor = std::ptr::null_mut();

    let child = unsafe {
        CreateFileW(
            name_w.as_ptr(),
            child_access,
            0, // no sharing
            &mut child_sa as *mut SECURITY_ATTRIBUTES,
            OPEN_EXISTING,
            0, // synchronous (no FILE_FLAG_OVERLAPPED)
            std::ptr::null_mut(),
        )
    };
    if child.is_null() || child == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let child = SyncHandle(OwnedHandle(child));

    Ok((parent, child))
}

/// Duplicate a handle into an inheritable copy in the current process.
fn dup_inheritable(src: HANDLE) -> io::Result<OwnedHandle> {
    let current = unsafe { GetCurrentProcess() };
    let mut out: HANDLE = std::ptr::null_mut();
    let ok = unsafe {
        DuplicateHandle(
            current,
            src,
            current,
            &mut out as *mut HANDLE,
            0,
            TRUE as BOOL,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(out))
}

/// One stdio slot resolved to a child-inheritable handle plus an
/// optional parent-side pipe end the caller will receive.
struct ResolvedSlot {
    /// The handle the child will see in this stdio slot. Inheritable.
    /// Generic `OwnedHandle` because the three non-pipe variants
    /// (Null/Parent/Handle) produce ordinary inheritable handles whose
    /// overlapped-ness we don't track.
    child_handle: OwnedHandle,
    /// Set only for [`crate::platform::process::StdioSource::Pipe`]: the parent-side end
    /// of a freshly-created pipe. Typed as [`OverlappedHandle`] so it
    /// can ONLY be turned into a `ChildStdin/Stdout/Stderr` (issue #115).
    parent_end: Option<OverlappedHandle>,
}

enum SlotDir {
    Stdin,
    Stdout,
    Stderr,
}

fn resolve_slot(
    slot: &crate::platform::process::StdioSource<'_>,
    dir: SlotDir,
) -> io::Result<ResolvedSlot> {
    match slot {
        crate::platform::process::StdioSource::Null => {
            let write = !matches!(dir, SlotDir::Stdin);
            Ok(ResolvedSlot {
                child_handle: open_nul(write)?,
                parent_end: None,
            })
        }
        crate::platform::process::StdioSource::Parent => {
            let std_handle = match dir {
                SlotDir::Stdin => STD_INPUT_HANDLE,
                SlotDir::Stdout => STD_OUTPUT_HANDLE,
                SlotDir::Stderr => STD_ERROR_HANDLE,
            };
            let src = unsafe { GetStdHandle(std_handle) };
            if src.is_null() || src == INVALID_HANDLE_VALUE {
                // No real parent handle (e.g. detached parent). Fall
                // back to NUL — child still gets a valid slot.
                let write = !matches!(dir, SlotDir::Stdin);
                return Ok(ResolvedSlot {
                    child_handle: open_nul(write)?,
                    parent_end: None,
                });
            }
            Ok(ResolvedSlot {
                child_handle: dup_inheritable(src)?,
                parent_end: None,
            })
        }
        crate::platform::process::StdioSource::File(file) => {
            let raw = file.as_raw_handle() as HANDLE;
            Ok(ResolvedSlot {
                child_handle: dup_inheritable(raw)?,
                parent_end: None,
            })
        }
        crate::platform::process::StdioSource::Pipe => {
            // Use the typed pipe constructor: parent end is provably
            // `OverlappedHandle` (the only thing that can be turned into
            // a Rust `ChildStdin/Stdout/Stderr`); child end is provably
            // `SyncHandle`. This makes the issue-#115 mismatch (sync
            // handle → ChildStdout reader using alertable_io_internal,
            // silent multi-write loss) a compile-time impossibility.
            let pipe_dir = match dir {
                SlotDir::Stdin => PipeDir::ParentWritesChildReads,
                SlotDir::Stdout | SlotDir::Stderr => PipeDir::ChildWritesParentReads,
            };
            let (parent_end, child_end) = create_pipe_pair(pipe_dir)?;
            Ok(ResolvedSlot {
                child_handle: child_end.into_owned(),
                parent_end: Some(parent_end),
            })
        }
    }
}

fn resolve_daemon_slot(
    slot: &crate::platform::process::DaemonStdioSource<'_>,
    dir: SlotDir,
) -> io::Result<OwnedHandle> {
    match slot {
        crate::platform::process::DaemonStdioSource::Null => {
            open_nul(!matches!(dir, SlotDir::Stdin))
        }
        crate::platform::process::DaemonStdioSource::File(file) => {
            dup_inheritable(file.as_raw_handle() as HANDLE)
        }
    }
}

pub struct SpawnedInner {
    process: Option<OwnedHandle>,
    job: Option<OwnedHandle>,
    // Held so the watcher knows what to close after drain_timeout.
    // None after watcher has been kicked.
    _drain_keepalive: Option<Arc<()>>,
}

impl SpawnedInner {
    pub fn kill(&self) -> io::Result<()> {
        if let Some(h) = self.process.as_ref() {
            let ok = unsafe { TerminateProcess(h.as_raw(), 1) };
            if ok == FALSE {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub fn wait(&self) -> io::Result<i32> {
        let Some(h) = self.process.as_ref() else {
            return Err(io::Error::other("child handle absent"));
        };
        wait_inner(h)
    }

    pub fn try_wait(&self) -> io::Result<Option<i32>> {
        let Some(h) = self.process.as_ref() else {
            return Ok(None);
        };
        try_wait_inner(h)
    }

    /// Called from Drop: close the Job Object handle, which (with
    /// KILL_ON_JOB_CLOSE) terminates the child and any descendants
    /// still in the job.
    pub fn shutdown(&mut self) {
        // Drop the Job Object handle first; KILL_ON_JOB_CLOSE will
        // terminate every assigned process.
        drop(self.job.take());
        // Then drop our process handle.
        drop(self.process.take());
    }
}

impl crate::platform::process::SpawnedChildControl for SpawnedInner {
    fn kill(&mut self) -> io::Result<()> {
        SpawnedInner::kill(self)
    }

    fn wait(&mut self) -> io::Result<i32> {
        SpawnedInner::wait(self)
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        SpawnedInner::try_wait(self)
    }

    fn shutdown(&mut self) {
        SpawnedInner::shutdown(self);
    }
}

impl Drop for SpawnedInner {
    fn drop(&mut self) {
        // Also covers failed setup before ownership reaches SpawnedChild.
        self.shutdown();
    }
}

fn resume_contained_thread(thread: &OwnedHandle) -> io::Result<()> {
    // SAFETY: the caller owns the primary thread handle until this returns.
    if unsafe { ResumeThread(thread.as_raw()) } == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static SYNC_SPAWN_FAILURE: std::cell::RefCell<(&'static str, Option<OwnedHandle>)> =
        const { std::cell::RefCell::new(("", None)) };
}

#[cfg(test)]
fn sync_spawn_checkpoint(stage: &str, process: HANDLE) -> io::Result<()> {
    SYNC_SPAWN_FAILURE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.0 != stage {
            return Ok(());
        }
        slot.0 = "";
        slot.1 = Some(dup_inheritable(process)?);
        Err(io::Error::other(format!("injected sync spawn {stage} failure")))
    })
}

pub fn spawn_sync_daemon(
    command: &mut Command,
    stdio: crate::platform::process::DaemonStdio<'_>,
    environment: crate::platform::process::SyncEnvironment,
    breakaway: bool,
) -> io::Result<crate::platform::process::DaemonChild> {
    let stdin = open_nul(false)?;
    let stdout = resolve_daemon_slot(&stdio.stdout, SlotDir::Stdout)?;
    let stderr = resolve_daemon_slot(&stdio.stderr, SlotDir::Stderr)?;
    let (handle, _thread, pid) = create_process_inner(
        command,
        &stdin,
        &stdout,
        &stderr,
        CreateMode::Daemon { breakaway },
        environment,
    )?;
    Ok(crate::platform::process::DaemonChild {
        pid,
        inner: Box::new(OwnedHandle(handle)),
    })
}

pub fn spawn_sync(
    command: &mut Command,
    stdio: crate::platform::process::SpawnStdio<'_>,
    environment: crate::platform::process::SyncEnvironment,
) -> io::Result<crate::platform::process::SpawnedChild> {
    let stdin_slot = resolve_slot(&stdio.stdin, SlotDir::Stdin)?;
    let stdout_slot = resolve_slot(&stdio.stdout, SlotDir::Stdout)?;
    let stderr_slot = resolve_slot(&stdio.stderr, SlotDir::Stderr)?;

    // No child exists if configuring the kill-on-close job fails.
    let job = create_job_object()?;

    let (process, thread, pid) = create_process_inner(
        command,
        &stdin_slot.child_handle,
        &stdout_slot.child_handle,
        &stderr_slot.child_handle,
        CreateMode::Contained {
            show_console: stdio.show_console,
        },
        environment,
    )?;

    // Assign to the per-spawn Job Object BEFORE ResumeThread so
    // the child cannot spawn grandchildren outside the job.
    let process = OwnedHandle(process);
    let thread = OwnedHandle(thread);
    let assignment = (|| -> io::Result<()> {
        #[cfg(test)]
        sync_spawn_checkpoint("assign", process.as_raw())?;
        // SAFETY: both owned handles remain live throughout assignment.
        if unsafe { AssignProcessToJobObject(job.as_raw(), process.as_raw()) } == FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    if let Err(err) = assignment {
        // The child is not in our job yet, so terminate it explicitly.
        // SAFETY: we still own the just-created process handle.
        unsafe {
            TerminateProcess(process.as_raw(), 1);
        }
        return Err(err);
    }

    // Own containment before any later fallible operation. Dropping this
    // guard closes the job (terminating the tree), then the process handle.
    let process_handle = process.as_raw();
    let mut inner = SpawnedInner {
        process: Some(process),
        job: Some(job),
        _drain_keepalive: None,
    };

    // Now safe to start the child.
    #[cfg(test)]
    sync_spawn_checkpoint("resume", process_handle)?;
    resume_contained_thread(&thread)?;
    drop(thread);

    // Convert the parent-side pipe ends, if any, into Rust ChildStdin
    // etc.  The kernel keeps duplicates of the child-side handles via
    // CreateProcessW, so dropping `stdin_slot.child_handle` etc.
    // below is fine. The OverlappedHandle::into_child_* methods are
    // the only conversion paths to a Rust ChildStd* — see #115.
    let stdin_pipe = stdin_slot
        .parent_end
        .map(OverlappedHandle::into_child_stdin);
    let stdout_pipe = stdout_slot
        .parent_end
        .map(OverlappedHandle::into_child_stdout);
    let stderr_pipe = stderr_slot
        .parent_end
        .map(OverlappedHandle::into_child_stderr);

    // Optional drain watcher: wait on process exit, then sleep
    // `drain_timeout`, then close our wrapper-held copies (none on
    // Windows after this point — Rust's ChildStdin/Stdout/Stderr own
    // them).  We still spawn the watcher so callers reading from the
    // pipes see EOF in bounded time after child exit; the watcher's
    // job here is purely the bounded sleep + signaling on drop.
    let drain_keepalive = if let Some(timeout) = stdio.drain_timeout {
        #[cfg(test)]
        sync_spawn_checkpoint("duplicate", process_handle)?;
        // The setup guard owns process_handle until success or error return.
        let process_handle = dup_inheritable(process_handle)?;
        let keep = Arc::new(());
        let keep_watcher = Arc::clone(&keep);
        // SAFETY: process_handle is moved into the thread.  We dup it
        // so closing the outer one from shutdown() doesn't break the
        // watcher's wait.
        thread::Builder::new().spawn(move || {
            drain_watcher(process_handle, timeout, keep_watcher);
        })?;
        Some(keep)
    } else {
        None
    };

    inner._drain_keepalive = drain_keepalive;
    Ok(crate::platform::process::SpawnedChild {
        stdin: stdin_pipe,
        stdout: stdout_pipe,
        stderr: stderr_pipe,
        pid,
        inner: Box::new(inner),
    })
}

/// Private containment stages for the hostile Wasm worker.  This is separate
/// from generic `spawn_sync`: that path deliberately supports daemon/breakaway
/// compatibility and must not silently acquire stricter semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum StrictWorkerSpawnStage {
    Pipe,
    CreateSuspended,
    CreateJob,
    ConfigureJob,
    AssignJob,
    Resume,
    Terminate,
    Reap,
}

/// Facade-owned bounds for a hostile Wasm worker Job Object. `None` leaves a
/// particular Job limit unset; the default still enables KILL_ON_JOB_CLOSE.
/// Memory values are bytes and must be nonzero and representable as a Windows
/// `SIZE_T` on the target process.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) struct StrictWorkerLimits {
    pub(crate) active_processes: Option<u32>,
    pub(crate) process_memory_bytes: Option<u64>,
    pub(crate) job_memory_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
enum StrictWorkerLimitError {
    ZeroActiveProcesses,
    ZeroProcessMemory,
    ZeroJobMemory,
    ProcessMemoryTooLarge,
    JobMemoryTooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
struct StrictWorkerJobConfiguration {
    limit_flags: DWORD,
    active_processes: Option<u32>,
    process_memory_bytes: Option<usize>,
    job_memory_bytes: Option<usize>,
}

impl StrictWorkerLimits {
    fn job_configuration(self) -> Result<StrictWorkerJobConfiguration, StrictWorkerLimitError> {
        let active_processes = match self.active_processes {
            Some(0) => return Err(StrictWorkerLimitError::ZeroActiveProcesses),
            value => value,
        };
        let process_memory_bytes = match self.process_memory_bytes {
            Some(0) => return Err(StrictWorkerLimitError::ZeroProcessMemory),
            Some(value) => Some(
                usize::try_from(value).map_err(|_| StrictWorkerLimitError::ProcessMemoryTooLarge)?,
            ),
            None => None,
        };
        let job_memory_bytes = match self.job_memory_bytes {
            Some(0) => return Err(StrictWorkerLimitError::ZeroJobMemory),
            Some(value) => Some(
                usize::try_from(value).map_err(|_| StrictWorkerLimitError::JobMemoryTooLarge)?,
            ),
            None => None,
        };

        let mut limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if active_processes.is_some() {
            limit_flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        }
        if process_memory_bytes.is_some() {
            limit_flags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        }
        if job_memory_bytes.is_some() {
            limit_flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
        }
        Ok(StrictWorkerJobConfiguration {
            limit_flags,
            active_processes,
            process_memory_bytes,
            job_memory_bytes,
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("strict worker spawn failed at {stage:?}: {cause}")]
#[allow(dead_code)]
pub(crate) struct StrictWorkerSpawnError {
    stage: StrictWorkerSpawnStage,
    cause: io::Error,
    cleanup: Option<StrictWorkerCleanupError>,
}
#[derive(Debug, thiserror::Error)]
#[error("strict worker cleanup failed at {stage:?}: {cause}")]
#[allow(dead_code)]
pub(crate) struct StrictWorkerCleanupError {
    stage: StrictWorkerSpawnStage,
    cause: io::Error,
}
#[allow(dead_code)]
impl StrictWorkerSpawnError {
    pub(crate) fn stage(&self) -> StrictWorkerSpawnStage { self.stage }
    pub(crate) fn cleanup(&self) -> Option<&StrictWorkerCleanupError> { self.cleanup.as_ref() }
}
#[allow(dead_code)]
impl StrictWorkerCleanupError {
    pub(crate) fn stage(&self) -> StrictWorkerSpawnStage { self.stage }
    pub(crate) fn message(&self) -> String { self.cause.to_string() }
    pub(crate) fn raw_os_error(&self) -> Option<i32> { self.cause.raw_os_error() }
}
/// Opaque crate-private worker child. Unlike generic `SpawnedChild`, Drop is
/// a containment operation: it closes the strict Job and waits/reaps the
/// direct worker. This covers ordinary CreateProcess descendants; processes
/// created by nonstandard escape mechanisms remain a platform-policy gap.
#[allow(dead_code)]
pub(crate) struct StrictWorkerChild {
    pub(crate) stdin: Option<ChildStdin>,
    pub(crate) stdout: Option<ChildStdout>,
    pub(crate) stderr: Option<ChildStderr>,
    pub(crate) pid: u32,
    inner: StrictWorkerInner,
}
#[allow(dead_code)]
struct StrictWorkerInner { process: Option<OwnedHandle>, job: Option<OwnedHandle> }
#[allow(dead_code)]
impl StrictWorkerChild {
    pub(crate) fn pid(&self) -> u32 { self.pid }
    pub(crate) fn try_wait(&self) -> io::Result<Option<i32>> { self.inner.process.as_ref().map_or(Ok(None), try_wait_inner) }
    pub(crate) fn wait(&self) -> io::Result<i32> { self.inner.process.as_ref().ok_or_else(|| io::Error::other("worker handle absent")).and_then(wait_inner) }
    /// Idempotent strict shutdown. Closing the Job is the tree termination
    /// action; `OwnedHandle::Drop` cannot report CloseHandle failure. Reaping
    /// is bounded, so Drop remains best-effort rather than wedging the host.
    pub(crate) fn terminate_and_reap(&mut self) -> Option<StrictWorkerCleanupError> {
        self.shutdown(true)
    }
    fn shutdown(&mut self, retry_termination: bool) -> Option<StrictWorkerCleanupError> {
        // KILL_ON_JOB_CLOSE must happen before waiting: it terminates the
        // worker and ordinary CreateProcess descendants as one containment
        // action. A failed/timed-out reap deliberately retains the process
        // handle so an explicit later call can retry it.
        let job = self.inner.job.take();
        let closed_job = job.is_some();
        // Closing the strict Job is the normal tree termination action.
        drop(job);
        let cleanup = self.inner.process.as_ref().and_then(|process| {
            if retry_termination && !closed_job {
                terminate_and_reap(process)
            } else {
                reap_only(process)
            }
        });
        if cleanup.is_none() {
            drop(self.inner.process.take());
        }
        cleanup
    }
}
impl Drop for StrictWorkerChild { fn drop(&mut self) { let _ = self.shutdown(false); } }

#[cfg(test)]
mod strict_worker_test_hook {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};

    type Hook = fn(HANDLE, HANDLE);

    static HOOK: OnceLock<Mutex<Option<Hook>>> = OnceLock::new();
    static MARKER: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    static OBSERVED: AtomicBool = AtomicBool::new(false);

    fn hook_slot() -> &'static Mutex<Option<Hook>> { HOOK.get_or_init(|| Mutex::new(None)) }
    fn marker_slot() -> &'static Mutex<Option<PathBuf>> { MARKER.get_or_init(|| Mutex::new(None)) }

    pub(super) struct Guard;

    pub(super) fn install(marker: PathBuf) -> Guard {
        OBSERVED.store(false, Ordering::SeqCst);
        *marker_slot().lock().unwrap() = Some(marker);
        *hook_slot().lock().unwrap() = Some(assert_assigned_while_suspended);
        Guard
    }

    pub(super) fn was_observed() -> bool { OBSERVED.load(Ordering::SeqCst) }

    impl Drop for Guard {
        fn drop(&mut self) {
            *hook_slot().lock().unwrap() = None;
            *marker_slot().lock().unwrap() = None;
        }
    }

    pub(super) fn run(process: HANDLE, job: HANDLE) {
        if let Some(hook) = *hook_slot().lock().unwrap() {
            hook(process, job);
        }
    }

    fn assert_assigned_while_suspended(process: HANDLE, job: HANDLE) {
        let marker = marker_slot().lock().unwrap().clone().expect("test marker installed");
        assert!(
            !marker.exists(),
            "worker reached its entry marker before strict spawn resumed it"
        );
        let mut contained = FALSE;
        assert_ne!(
            unsafe {
                IsProcessInJob(process as WindowsHandle, job as WindowsHandle, &mut contained)
            },
            FALSE,
            "IsProcessInJob failed: {}",
            io::Error::last_os_error()
        );
        assert_ne!(contained, FALSE, "suspended worker was not assigned to strict Job");
        OBSERVED.store(true, Ordering::SeqCst);
    }
}

/// Spawn a worker suspended, attach it to a strict kill-on-close Job Object
/// without BREAKAWAY_OK, then resume. `ERROR_ACCESS_DENIED` while assigning
/// (including hostile nested-job policy) is an AssignJob failure, never a
/// successful "already contained" fallback.
#[allow(dead_code)]
pub(crate) fn spawn_strict_contained_worker(
    command: &mut Command,
    stdio: crate::platform::process::SpawnStdio<'_>,
    environment: crate::platform::process::SyncEnvironment,
    limits: StrictWorkerLimits,
) -> Result<StrictWorkerChild, StrictWorkerSpawnError> {
    let stdin = resolve_slot(&stdio.stdin, SlotDir::Stdin).map_err(|source| StrictWorkerSpawnError { stage: StrictWorkerSpawnStage::Pipe, cause: source, cleanup: None })?;
    let stdout = resolve_slot(&stdio.stdout, SlotDir::Stdout).map_err(|source| StrictWorkerSpawnError { stage: StrictWorkerSpawnStage::Pipe, cause: source, cleanup: None })?;
    let stderr = resolve_slot(&stdio.stderr, SlotDir::Stderr).map_err(|source| StrictWorkerSpawnError { stage: StrictWorkerSpawnStage::Pipe, cause: source, cleanup: None })?;
    // Job setup is intentionally before CreateProcessW: failure here leaves
    // no suspended hostile child to clean up.
    let job = create_strict_worker_job(limits)
        .map_err(|(stage, source)| StrictWorkerSpawnError { stage, cause: source, cleanup: None })?;
    let (process, thread, pid) = create_process_inner(command, &stdin.child_handle, &stdout.child_handle, &stderr.child_handle, CreateMode::Contained { show_console: false }, environment)
        .map_err(|source| StrictWorkerSpawnError { stage: StrictWorkerSpawnStage::CreateSuspended, cause: source, cleanup: None })?;
    let process = OwnedHandle(process);
    let thread = OwnedHandle(thread);
    if let Err(source) = assign_strict_worker_to_job(job.as_raw(), process.as_raw()) {
        let cleanup = terminate_and_reap(&process);
        return Err(StrictWorkerSpawnError { stage: StrictWorkerSpawnStage::AssignJob, cause: source, cleanup });
    }
    #[cfg(test)]
    strict_worker_test_hook::run(process.as_raw(), job.as_raw());
    let resumed = unsafe { ResumeThread(thread.as_raw()) };
    if !strict_resume_count_is_valid(resumed) {
        let source = if resumed == u32::MAX { io::Error::last_os_error() } else { io::Error::other("strict worker suspend count was not one") };
        // Assignment succeeded, so Job close kills the complete contained
        // tree before direct-worker reaping.
        drop(job);
        let cleanup = reap_only(&process);
        return Err(StrictWorkerSpawnError { stage: StrictWorkerSpawnStage::Resume, cause: source, cleanup });
    }
    drop(thread);
    Ok(StrictWorkerChild {
        stdin: stdin.parent_end.map(OverlappedHandle::into_child_stdin),
        stdout: stdout.parent_end.map(OverlappedHandle::into_child_stdout),
        stderr: stderr.parent_end.map(OverlappedHandle::into_child_stderr),
        pid,
        inner: StrictWorkerInner { process: Some(process), job: Some(job) },
    })
}

/// Adapt the strict Job launcher to the private cross-platform worker facade.
#[allow(dead_code)] // Phase-A foundation; the phase-B supervisor owns it.
pub(crate) fn spawn_contained_worker(
    command: &mut Command,
    limits: crate::platform::process::WorkerLimits,
) -> Result<crate::platform::process::WorkerChild, crate::platform::process::WorkerError> {
    let mut strict = spawn_strict_contained_worker(
        command,
        crate::platform::process::SpawnStdio {
            stdin: crate::platform::process::StdioSource::Pipe,
            stdout: crate::platform::process::StdioSource::Pipe,
            stderr: crate::platform::process::StdioSource::Null,
            drain_timeout: None,
            show_console: false,
        },
        crate::platform::process::SyncEnvironment::Explicit(Vec::new()),
        StrictWorkerLimits {
            active_processes: limits.active_processes,
            process_memory_bytes: limits.process_memory_bytes,
            job_memory_bytes: limits.job_memory_bytes,
        },
    )
    .map_err(map_strict_worker_spawn_error)?;
    let stdin = strict.stdin.take();
    let stdout = strict.stdout.take();
    let pid = strict.pid();
    Ok(crate::platform::process::WorkerChild::new(
        stdin,
        stdout,
        pid,
        Box::new(StrictWorkerControl { child: Some(strict) }),
    ))
}

#[allow(dead_code)] // Phase-A foundation; the phase-B supervisor owns it.
struct StrictWorkerControl { child: Option<StrictWorkerChild> }

impl crate::platform::process::WorkerChildControl for StrictWorkerControl {
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        self.child.as_ref().map_or(Ok(None), StrictWorkerChild::try_wait)
    }

    fn force_and_reap(&mut self, _timeout: std::time::Duration) -> Result<(), crate::platform::process::WorkerError> {
        let Some(child) = self.child.as_mut() else { return Ok(()); };
        if let Some(error) = child.terminate_and_reap() {
            return Err(crate::platform::process::WorkerError::new(map_strict_stage(error.stage()), io::Error::other(error.to_string())));
        }
        self.child = None;
        Ok(())
    }

    fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() { let _ = child.terminate_and_reap(); }
    }
}

#[allow(dead_code)] // Phase-A foundation; the phase-B supervisor owns it.
fn map_strict_worker_spawn_error(error: StrictWorkerSpawnError) -> crate::platform::process::WorkerError {
    crate::platform::process::WorkerError::new(map_strict_stage(error.stage()), io::Error::other(error.to_string()))
}

#[allow(dead_code)] // Phase-A foundation; the phase-B supervisor owns it.
fn map_strict_stage(stage: StrictWorkerSpawnStage) -> crate::platform::process::WorkerStage {
    use crate::platform::process::WorkerStage;
    match stage {
        StrictWorkerSpawnStage::Pipe => WorkerStage::Pipe,
        StrictWorkerSpawnStage::CreateSuspended => WorkerStage::Create,
        StrictWorkerSpawnStage::CreateJob | StrictWorkerSpawnStage::ConfigureJob => WorkerStage::ConfigureContainment,
        StrictWorkerSpawnStage::AssignJob => WorkerStage::AssignContainment,
        StrictWorkerSpawnStage::Resume => WorkerStage::Resume,
        StrictWorkerSpawnStage::Terminate => WorkerStage::Terminate,
        StrictWorkerSpawnStage::Reap => WorkerStage::Reap,
    }
}

#[allow(dead_code)]
fn strict_resume_count_is_valid(previous: u32) -> bool { previous == 1 }

fn assign_strict_worker_to_job(job: HANDLE, process: HANDLE) -> io::Result<()> {
    #[cfg(test)]
    if STRICT_WORKER_FORCE_ASSIGN_DENIED.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "injected AssignProcessToJobObject access denied",
        ));
    }
    if unsafe { AssignProcessToJobObject(job, process) } == FALSE {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[allow(dead_code)]
fn terminate_and_reap(process: &OwnedHandle) -> Option<StrictWorkerCleanupError> {
    let terminate = if unsafe { TerminateProcess(process.as_raw(), 1) } == FALSE {
        Some(StrictWorkerCleanupError { stage: StrictWorkerSpawnStage::Terminate, cause: io::Error::last_os_error() })
    } else { None };
    let reap = reap_only(process);
    // A process that already exited between Job close and this retry can
    // reject TerminateProcess; successful reaping is still success.
    reap.as_ref()?;
    terminate.or(reap)
}

#[allow(dead_code)]
fn reap_only(process: &OwnedHandle) -> Option<StrictWorkerCleanupError> {
    #[cfg(test)]
    if STRICT_WORKER_FORCE_REAP_TIMEOUT.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return Some(StrictWorkerCleanupError {
            stage: StrictWorkerSpawnStage::Reap,
            cause: io::Error::new(io::ErrorKind::TimedOut, "injected strict worker reap timeout"),
        });
    }
    match unsafe { WaitForSingleObject(process.as_raw(), STRICT_WORKER_REAP_TIMEOUT_MS) } {
        WAIT_OBJECT_0 => None,
        WAIT_TIMEOUT_RESULT => Some(StrictWorkerCleanupError {
            stage: StrictWorkerSpawnStage::Reap,
            cause: io::Error::new(io::ErrorKind::TimedOut, "strict worker did not exit after containment cleanup"),
        }),
        _ => Some(StrictWorkerCleanupError {
            stage: StrictWorkerSpawnStage::Reap,
            cause: io::Error::last_os_error(),
        }),
    }
}

fn drain_watcher(process_handle: OwnedHandle, timeout: Duration, keep: Arc<()>) {
    // `WAIT_TIMEOUT` (0x102): a single wait slice elapsed without the child
    // exiting. Declared inline to avoid pinning an extra winapi import.
    const WAIT_TIMEOUT: u32 = 0x0000_0102;
    // Wait for the child to exit, but in bounded 1-second slices instead of
    // a single `INFINITE` wait (issue #590, cluster K): a child that never
    // exits — e.g. a descendant that broke away from the Job Object via
    // JOB_OBJECT_LIMIT_BREAKAWAY_OK — would otherwise park this detached
    // watcher thread forever. Bail out promptly once the owning
    // `SpawnedChild` has been dropped (our clone is then the only `Arc`
    // left), since there is nothing left to drain for.
    loop {
        let r = unsafe { WaitForSingleObject(process_handle.as_raw(), 1000) };
        if r != WAIT_TIMEOUT {
            // WAIT_OBJECT_0 (child exited) or a wait failure — stop looping
            // and fall through to the post-mortem drain below.
            break;
        }
        if Arc::strong_count(&keep) == 1 {
            return;
        }
    }
    // Give the pipes a chance to drain post-mortem.
    //
    // #199: intentional — same post-mortem drain semantic as the
    // Unix watcher; this sleep lets the reader threads pick up the
    // last bytes the kernel still has buffered.
    thread::sleep(timeout);
    // process_handle is closed here.  The Rust ChildStdin/Stdout/Stderr
    // pipes owned by the SpawnedChild caller are not in our hands; the
    // child has exited so any reads on those pipes will EOF naturally
    // once the kernel ref-counts the write-ends to zero.
}

enum CreateMode {
    Daemon {
        /// Whether to request `CREATE_BREAKAWAY_FROM_JOB`.
        ///
        /// Opt-in, because "detached daemon" and "escapes my caller's Job
        /// Object" are genuinely different properties and callers want them
        /// independently. A build daemon (zccache/sccache) started beneath an
        /// agent session needs to escape or the kernel kills it when that
        /// session's job closes. A child spawned as a daemon merely to obtain
        /// sanitized handle inheritance must NOT escape — see
        /// `testbins/src/bin/spawner.rs`, which relies on its sleepers staying
        /// in the caller's job so `containment_test` can prove they die with
        /// it.
        breakaway: bool,
    },
    Contained {
        show_console: bool,
    },
}

/// Returns (process_handle, pid). For `Contained` mode the process is
/// Returns `(process_handle, thread_handle, pid)`.
///
/// For [`CreateMode::Daemon`]: `thread_handle` is `null_mut()` (already
/// closed; child is running). For [`CreateMode::Contained`]: child is
/// still suspended; the caller must assign it to a Job Object then call
/// `ResumeThread(thread_handle)` and `CloseHandle(thread_handle)`.
#[expect(clippy::too_many_lines, reason = "Existing Windows process creation is 121 lines")]
fn create_process_inner(
    command: &mut Command,
    stdin: &OwnedHandle,
    stdout: &OwnedHandle,
    stderr: &OwnedHandle,
    mode: CreateMode,
    environment: crate::platform::process::SyncEnvironment,
) -> io::Result<(HANDLE, HANDLE, u32)> {
    let mut cmdline = build_command_line(command.get_program(), command.get_args());

    let envs: Vec<(OsString, Option<OsString>)> = command
        .get_envs()
        .map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string())))
        .collect();
    let env_block = if envs.is_empty()
        && matches!(
            &environment,
            crate::platform::process::SyncEnvironment::Inherit
        ) {
        // No overrides AND no clear → let the kernel inherit the
        // parent's env block (lpEnvironment=NULL).
        None
    } else {
        // Either we have overrides, or the caller selected a non-inherited
        // base. In both cases build and pass an explicit Unicode block.
        Some(build_env_block(envs, environment)?)
    };

    let cwd_w: Option<Vec<u16>> = command.get_current_dir().map(|p| {
        OsStr::new(p)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    });

    // Initialize the proc-thread attribute list.
    let mut size: usize = 0;
    unsafe {
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
    }
    let mut attr_buf: Vec<u8> = vec![0; size];
    let attr_list = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;

    let ok = unsafe { InitializeProcThreadAttributeList(attr_list, 1, 0, &mut size) };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }

    let handle_list: [HANDLE; 3] = [stdin.as_raw(), stdout.as_raw(), stderr.as_raw()];
    let ok = unsafe {
        UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            handle_list.as_ptr() as *mut _,
            std::mem::size_of::<[HANDLE; 3]>(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == FALSE {
        let err = io::Error::last_os_error();
        unsafe { DeleteProcThreadAttributeList(attr_list) };
        return Err(err);
    }

    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as DWORD;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = stdin.as_raw();
    si.StartupInfo.hStdOutput = stdout.as_raw();
    si.StartupInfo.hStdError = stderr.as_raw();
    si.lpAttributeList = attr_list;

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    let mut flags: DWORD = EXTENDED_STARTUPINFO_PRESENT;
    match mode {
        CreateMode::Daemon { breakaway } => {
            // Daemons are detached from every console and placed in a new
            // process group so Ctrl-C / Ctrl-Break delivered to the parent's
            // console group never reaches them. CREATE_NO_WINDOW can still
            // allocate a hidden conhost; DETACHED_PROCESS is the flag that
            // guarantees the daemon owns no console at all. The two flags
            // must not be combined.
            //
            // CREATE_BREAKAWAY_FROM_JOB is what actually makes a daemon
            // outlive its spawner. Job Object membership is inherited by
            // every descendant at any depth, and our contained jobs carry
            // KILL_ON_JOB_CLOSE — so without breakaway, a daemon spawned
            // anywhere beneath a contained process is terminated by the
            // kernel the moment that job's handle drops, no matter how
            // detached it made itself. Breakaway is the only escape; the
            // job must permit it via JOB_OBJECT_LIMIT_BREAKAWAY_OK, which
            // every job this crate creates now sets.
            flags = daemon_creation_flags(flags, breakaway);
        }
        CreateMode::Contained { show_console } => {
            // We need to assign to a Job Object before the child runs.
            flags |= CREATE_SUSPENDED;
            // Default: no console. Caller opts in via show_console to let
            // the child inherit / allocate one (interactive subprocess).
            if !show_console {
                flags |= CREATE_NO_WINDOW;
            }
        }
    }
    if env_block.is_some() {
        flags |= CREATE_UNICODE_ENVIRONMENT;
    }

    let cwd_ptr = cwd_w
        .as_ref()
        .map(|v| v.as_ptr())
        .unwrap_or(std::ptr::null());
    let env_ptr = env_block
        .as_ref()
        .map(|v| v.as_ptr() as *mut winapi::ctypes::c_void)
        .unwrap_or(std::ptr::null_mut());

    // `CREATE_BREAKAWAY_FROM_JOB` is a request, not a guarantee: if the
    // spawner sits inside a Job Object that does *not* set
    // JOB_OBJECT_LIMIT_BREAKAWAY_OK, CreateProcessW fails outright with
    // ERROR_ACCESS_DENIED rather than silently ignoring the flag. Outer
    // jobs we don't control are common (CI runners, container supervisors,
    // debuggers), so retry once without the flag: a daemon that stays
    // contained is strictly better than a daemon that fails to spawn.
    const ERROR_ACCESS_DENIED_CODE: i32 = 5;
    let mut spawn_flags = flags;
    let err = loop {
        let ok = unsafe {
            CreateProcessW(
                std::ptr::null(),
                cmdline.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                TRUE as BOOL,
                spawn_flags,
                env_ptr,
                cwd_ptr,
                &mut si.StartupInfo,
                &mut pi,
            )
        };
        if ok != FALSE {
            break None;
        }
        let err = io::Error::last_os_error();
        if spawn_flags & CREATE_BREAKAWAY_FROM_JOB != 0
            && err.raw_os_error() == Some(ERROR_ACCESS_DENIED_CODE)
        {
            spawn_flags &= !CREATE_BREAKAWAY_FROM_JOB;
            continue;
        }
        break Some(err);
    };
    unsafe {
        DeleteProcThreadAttributeList(attr_list);
    }
    if let Some(err) = err {
        return Err(err);
    }

    // For Contained mode we leave the child suspended and return the
    // thread handle to the caller; the caller assigns to the Job Object
    // then resumes. For Daemon mode (not CREATE_SUSPENDED) the thread is
    // already running and we just close the thread handle here.
    if matches!(mode, CreateMode::Daemon { .. }) {
        unsafe {
            CloseHandle(pi.hThread);
        }
        Ok((pi.hProcess, std::ptr::null_mut(), pi.dwProcessId))
    } else {
        Ok((pi.hProcess, pi.hThread, pi.dwProcessId))
    }
}

/// Creation flags for a detached daemon, layered onto `base`.
///
/// Split out from the spawn path so the flag composition is unit-testable
/// without actually creating a process. `breakaway` adds
/// `CREATE_BREAKAWAY_FROM_JOB`; see [`CreateMode::Daemon`] for why it is
/// opt-in rather than implied by "daemon".
fn daemon_creation_flags(base: DWORD, breakaway: bool) -> DWORD {
    let flags = base | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    if breakaway {
        flags | CREATE_BREAKAWAY_FROM_JOB
    } else {
        flags
    }
}

#[cfg(test)]
mod daemon_flag_tests {
    use super::*;

    static STRICT_WORKER_NATIVE_TEST_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
        std::sync::OnceLock::new();

    fn strict_worker_native_test_guard() -> std::sync::MutexGuard<'static, ()> {
        STRICT_WORKER_NATIVE_TEST_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Breakaway is what lets a daemon outlive the job its spawner sits in.
    /// Without it, KILL_ON_JOB_CLOSE reaps the daemon when the job closes.
    #[test]
    fn daemon_flags_request_breakaway_when_opted_in() {
        let flags = daemon_creation_flags(EXTENDED_STARTUPINFO_PRESENT, true);
        assert_ne!(flags & CREATE_BREAKAWAY_FROM_JOB, 0);
        assert_ne!(flags & CREATE_NEW_PROCESS_GROUP, 0);
        assert_ne!(flags & DETACHED_PROCESS, 0);
        assert_eq!(flags & CREATE_NO_WINDOW, 0);
    }

    /// The default must NOT break away: `testbin-spawner` spawns sleepers as
    /// daemons purely for handle sanitization and relies on them staying in
    /// the caller's job, which `containment_test` asserts.
    #[test]
    fn daemon_flags_stay_in_job_by_default() {
        let flags = daemon_creation_flags(EXTENDED_STARTUPINFO_PRESENT, false);
        assert_eq!(
            flags & CREATE_BREAKAWAY_FROM_JOB,
            0,
            "breakaway must be opt-in"
        );
        assert_ne!(flags & CREATE_NEW_PROCESS_GROUP, 0);
        assert_ne!(flags & DETACHED_PROCESS, 0);
        assert_eq!(flags & CREATE_NO_WINDOW, 0);
    }

    /// The daemon path must never suspend: nothing resumes it, unlike the
    /// contained path where the caller resumes after assigning the job.
    #[test]
    fn daemon_flags_never_suspend() {
        for breakaway in [true, false] {
            let flags = daemon_creation_flags(EXTENDED_STARTUPINFO_PRESENT, breakaway);
            assert_eq!(flags & CREATE_SUSPENDED, 0);
            assert_ne!(flags & EXTENDED_STARTUPINFO_PRESENT, 0, "base preserved");
        }
    }

    /// The ERROR_ACCESS_DENIED retry must clear only the breakaway bit, so
    /// the fallback spawn is otherwise identical to the first attempt.
    #[test]
    fn breakaway_fallback_clears_only_that_bit() {
        let flags = daemon_creation_flags(EXTENDED_STARTUPINFO_PRESENT, true);
        let fallback = flags & !CREATE_BREAKAWAY_FROM_JOB;
        assert_eq!(fallback & CREATE_BREAKAWAY_FROM_JOB, 0);
        assert_eq!(
            fallback,
            daemon_creation_flags(EXTENDED_STARTUPINFO_PRESENT, false),
            "fallback must equal the non-breakaway flag set"
        );
    }

    #[test]
    fn strict_worker_limits_default_to_kill_on_close_without_breakaway() {
        let configuration = StrictWorkerLimits::default().job_configuration().unwrap();
        let flags = configuration.limit_flags;
        assert_ne!(flags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, 0);
        assert_eq!(flags & JOB_OBJECT_LIMIT_BREAKAWAY_OK, 0);
        assert_eq!(flags & JOB_OBJECT_LIMIT_ACTIVE_PROCESS, 0);
        assert_eq!(flags & JOB_OBJECT_LIMIT_PROCESS_MEMORY, 0);
        assert_eq!(flags & JOB_OBJECT_LIMIT_JOB_MEMORY, 0);
    }

    #[test]
    fn strict_worker_limits_map_every_requested_job_limit() {
        let configuration = StrictWorkerLimits {
            active_processes: Some(3),
            process_memory_bytes: Some(4096),
            job_memory_bytes: Some(8192),
        }
        .job_configuration()
        .unwrap();
        assert_eq!(configuration.active_processes, Some(3));
        assert_eq!(configuration.process_memory_bytes, Some(4096));
        assert_eq!(configuration.job_memory_bytes, Some(8192));
        assert_ne!(configuration.limit_flags & JOB_OBJECT_LIMIT_ACTIVE_PROCESS, 0);
        assert_ne!(configuration.limit_flags & JOB_OBJECT_LIMIT_PROCESS_MEMORY, 0);
        assert_ne!(configuration.limit_flags & JOB_OBJECT_LIMIT_JOB_MEMORY, 0);
        assert_eq!(configuration.limit_flags & JOB_OBJECT_LIMIT_BREAKAWAY_OK, 0);
    }

    #[test]
    fn strict_worker_limits_reject_zero_values_before_process_creation() {
        assert_eq!(
            StrictWorkerLimits { active_processes: Some(0), ..StrictWorkerLimits::default() }
                .job_configuration(),
            Err(StrictWorkerLimitError::ZeroActiveProcesses)
        );
        assert_eq!(
            StrictWorkerLimits { process_memory_bytes: Some(0), ..StrictWorkerLimits::default() }
                .job_configuration(),
            Err(StrictWorkerLimitError::ZeroProcessMemory)
        );
        assert_eq!(
            StrictWorkerLimits { job_memory_bytes: Some(0), ..StrictWorkerLimits::default() }
                .job_configuration(),
            Err(StrictWorkerLimitError::ZeroJobMemory)
        );
    }

    #[test]
    fn strict_worker_accepts_exactly_one_suspension() {
        assert!(strict_resume_count_is_valid(1));
        assert!(!strict_resume_count_is_valid(0));
        assert!(!strict_resume_count_is_valid(2));
        assert!(!strict_resume_count_is_valid(u32::MAX));
    }

    /// This is invoked only by the native containment test below. It is an
    /// ignored test so normal test discovery can never park a helper process.
    #[test]
    #[ignore]
    fn strict_worker_native_descendant_helper() {
        if std::env::var_os("KERNAL_API_STRICT_WORKER_DESCENDANT").is_some() {
            std::thread::park();
        }
    }

    /// The direct helper spawns an ordinary CreateProcess descendant, reports
    /// its PID through a marker file, and stays alive until Job close kills it.
    #[test]
    #[ignore]
    #[allow(clippy::zombie_processes)] // closing the strict Job, not Child::wait, proves tree reaping.
    fn strict_worker_native_helper() {
        let Some(marker) = std::env::var_os("KERNAL_API_STRICT_WORKER_MARKER") else {
            return;
        };
        if let Some(breakaway_marker) = std::env::var_os("KERNAL_API_STRICT_WORKER_BREAKAWAY") {
            use std::os::windows::process::CommandExt;

            let mut breakaway = Command::new(std::env::current_exe().expect("test executable"));
            breakaway
                .args([
                    "--exact",
                    "platform_win::sync_spawn::daemon_flag_tests::strict_worker_native_descendant_helper",
                    "--ignored",
                ])
                .env("KERNAL_API_STRICT_WORKER_DESCENDANT", "1")
                .creation_flags(CREATE_BREAKAWAY_FROM_JOB);
            let outcome = match breakaway.spawn() {
                Ok(mut child) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    format!("unexpected-success:{}", child.id())
                }
                Err(error) => format!("denied:{}", error.raw_os_error().unwrap_or_default()),
            };
            super::readiness_marker::publish(std::path::Path::new(&breakaway_marker), &outcome)
                .expect("publish breakaway result");
        }
        let mut descendant = Command::new(std::env::current_exe().expect("test executable"));
        descendant
            .args([
                "--exact",
                "platform_win::sync_spawn::daemon_flag_tests::strict_worker_native_descendant_helper",
                "--ignored",
            ])
            .env("KERNAL_API_STRICT_WORKER_DESCENDANT", "1");
        let outcome = match descendant.spawn() {
            Ok(descendant) => format!("ready:{}", descendant.id()),
            Err(error) => format!("denied:{}", error.raw_os_error().unwrap_or_default()),
        };
        super::readiness_marker::publish(std::path::Path::new(&marker), &outcome)
            .expect("publish descendant result");
        std::thread::park();
    }

    /// Deliberately uncontained RED helper. Unlike the strict fixture it has
    /// no descendant: its sole purpose is proving user entry after an ordinary
    /// CreateProcess return and before any later Job assignment could occur.
    #[test]
    #[ignore]
    fn ordinary_post_spawn_red_helper() {
        if let Some(marker) = std::env::var_os("KERNAL_API_ORDINARY_SPAWN_MARKER") {
            super::readiness_marker::publish(std::path::Path::new(&marker), "ready")
                .expect("publish ordinary entry marker");
            std::thread::park();
        }
    }

    #[test]
    fn strict_worker_assigns_before_resume_and_job_close_reaps_tree() {
        const TEST_WAIT_MS: DWORD = 5_000;
        let _native_test_guard = strict_worker_native_test_guard();
        let marker = StrictWorkerTestMarker::new();
        let _hook = strict_worker_test_hook::install(marker.path.clone());
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "platform_win::sync_spawn::daemon_flag_tests::strict_worker_native_helper",
                "--ignored",
            ])
            .env("KERNAL_API_STRICT_WORKER_MARKER", &marker.path);
        let mut worker = spawn_strict_contained_worker(
            &mut command,
            crate::platform::process::SpawnStdio::default(),
            crate::platform::process::SyncEnvironment::Inherit,
            StrictWorkerLimits::default(),
        )
        .expect("strict worker launch");
        assert!(
            strict_worker_test_hook::was_observed(),
            "the strict pre-resume membership hook was not reached"
        );
        let direct = dup_inheritable(
            worker
                .inner
                .process
                .as_ref()
                .expect("live worker process")
                .as_raw(),
        )
        .expect("duplicate direct worker handle");
        let descendant_pid = marker.wait_for_pid();
        let descendant = unsafe { OpenProcess(SYNCHRONIZE, FALSE, descendant_pid) };
        assert!(
            !descendant.is_null(),
            "open descendant for bounded wait: {}",
            io::Error::last_os_error()
        );
        let descendant = OwnedHandle(descendant);

        assert!(worker.terminate_and_reap().is_none(), "strict shutdown failed");
        assert_eq!(
            unsafe { WaitForSingleObject(direct.as_raw(), TEST_WAIT_MS) },
            WAIT_OBJECT_0,
            "direct worker did not exit after Job close"
        );
        assert_eq!(
            unsafe { WaitForSingleObject(descendant.as_raw(), TEST_WAIT_MS) },
            WAIT_OBJECT_0,
            "ordinary CreateProcess descendant escaped Job close"
        );
        assert!(worker.terminate_and_reap().is_none(), "second shutdown was not harmless");
    }

    #[test]
    fn strict_worker_timeout_retains_process_for_explicit_retry() {
        let _native_test_guard = strict_worker_native_test_guard();
        let marker = StrictWorkerTestMarker::new();
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "platform_win::sync_spawn::daemon_flag_tests::strict_worker_native_helper",
                "--ignored",
            ])
            .env("KERNAL_API_STRICT_WORKER_MARKER", &marker.path);
        let mut worker = spawn_strict_contained_worker(
            &mut command,
            crate::platform::process::SpawnStdio::default(),
            crate::platform::process::SyncEnvironment::Inherit,
            StrictWorkerLimits::default(),
        )
        .expect("strict worker launch");
        STRICT_WORKER_FORCE_REAP_TIMEOUT.store(true, std::sync::atomic::Ordering::SeqCst);
        let timeout = worker.terminate_and_reap().expect("injected timeout");
        assert_eq!(timeout.stage(), StrictWorkerSpawnStage::Reap);
        assert!(worker.inner.process.is_some(), "timeout must retain retryable process handle");
        assert!(worker.terminate_and_reap().is_none(), "explicit retry must reap worker");
        assert!(worker.inner.process.is_none(), "successful reap consumes process handle");
    }

    #[test]
    fn injected_assign_denial_is_semantic_and_never_reaches_worker_entry() {
        let _native_test_guard = strict_worker_native_test_guard();
        let marker = StrictWorkerTestMarker::new();
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "platform_win::sync_spawn::daemon_flag_tests::strict_worker_native_helper",
                "--ignored",
            ])
            .env("KERNAL_API_STRICT_WORKER_MARKER", &marker.path);
        STRICT_WORKER_FORCE_ASSIGN_DENIED.store(true, std::sync::atomic::Ordering::SeqCst);
        let error = match spawn_strict_contained_worker(
            &mut command,
            crate::platform::process::SpawnStdio::default(),
            crate::platform::process::SyncEnvironment::Inherit,
            StrictWorkerLimits::default(),
        ) {
            Err(error) => error,
            Ok(_) => panic!("injected assignment denial unexpectedly spawned a worker"),
        };
        assert_eq!(error.stage(), StrictWorkerSpawnStage::AssignJob);
        assert!(
            !marker.path.exists(),
            "suspended worker entered despite failed Job assignment"
        );
    }

    #[test]
    fn strict_job_rejects_breakaway_creation() {
        let _native_test_guard = strict_worker_native_test_guard();
        let marker = StrictWorkerTestMarker::new();
        let breakaway = StrictWorkerTestMarker::new();
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "platform_win::sync_spawn::daemon_flag_tests::strict_worker_native_helper",
                "--ignored",
            ])
            .env("KERNAL_API_STRICT_WORKER_MARKER", &marker.path)
            .env("KERNAL_API_STRICT_WORKER_BREAKAWAY", &breakaway.path);
        let mut worker = spawn_strict_contained_worker(
            &mut command,
            crate::platform::process::SpawnStdio::default(),
            crate::platform::process::SyncEnvironment::Inherit,
            StrictWorkerLimits::default(),
        )
        .expect("strict worker launch");
        assert!(
            breakaway.wait_for_text().starts_with("denied:"),
            "strict Job unexpectedly permitted breakaway creation"
        );
        let _ = marker.wait_for_pid();
        assert!(worker.terminate_and_reap().is_none(), "strict shutdown failed");
    }

    #[test]
    fn strict_active_process_limit_rejects_worker_descendant() {
        let _native_test_guard = strict_worker_native_test_guard();
        let marker = StrictWorkerTestMarker::new();
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "platform_win::sync_spawn::daemon_flag_tests::strict_worker_native_helper",
                "--ignored",
            ])
            .env("KERNAL_API_STRICT_WORKER_MARKER", &marker.path);
        let mut worker = spawn_strict_contained_worker(
            &mut command,
            crate::platform::process::SpawnStdio::default(),
            crate::platform::process::SyncEnvironment::Inherit,
            StrictWorkerLimits {
                active_processes: Some(1),
                ..StrictWorkerLimits::default()
            },
        )
        .expect("strict limited worker launch");
        assert!(
            marker.wait_for_text().starts_with("denied:"),
            "active-process Job limit admitted a worker descendant"
        );
        assert!(worker.terminate_and_reap().is_none(), "strict shutdown failed");
    }

    /// RED contrast for the strict launch sequence: a normal CreateProcess
    /// child can publish its entry marker before its parent has any
    /// post-spawn opportunity to assign it to a Job Object.
    #[test]
    fn ordinary_spawn_can_enter_before_post_spawn_job_assignment() {
        let _native_test_guard = strict_worker_native_test_guard();
        let marker = StrictWorkerTestMarker::new();
        let mut helper = Command::new(std::env::current_exe().expect("test executable"));
        helper
            .args([
                "--exact",
                "platform_win::sync_spawn::daemon_flag_tests::ordinary_post_spawn_red_helper",
                "--ignored",
            ])
            .env("KERNAL_API_ORDINARY_SPAWN_MARKER", &marker.path);
        let mut helper = TestChildGuard::new(helper.spawn().expect("ordinary helper spawn"));
        assert_eq!(marker.wait_for_text(), "ready");
        assert!(
            helper.child_mut().try_wait().expect("observe helper").is_none(),
            "ordinary helper exited before publishing its entry marker"
        );
        helper.terminate_and_reap();
    }

    /// Narrow panic-safe owner for an intentionally uncontained native test
    /// helper. StrictWorkerChild itself already owns strict-Job cleanup.
    struct TestChildGuard(Option<std::process::Child>);

    impl TestChildGuard {
        fn new(child: std::process::Child) -> Self { Self(Some(child)) }

        fn child_mut(&mut self) -> &mut std::process::Child {
            self.0.as_mut().expect("live test helper")
        }

        fn terminate_and_reap(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    impl Drop for TestChildGuard {
        fn drop(&mut self) { self.terminate_and_reap(); }
    }

    struct StrictWorkerTestMarker {
        path: std::path::PathBuf,
    }

    impl StrictWorkerTestMarker {
        fn new() -> Self {
            let unique = format!(
                "kernal-api-strict-worker-{}-{}.marker",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock")
                    .as_nanos(),
            );
            Self { path: std::env::temp_dir().join(unique) }
        }

        fn wait_for_pid(&self) -> u32 {
            let text = self.wait_for_text();
            let pid = text.strip_prefix("ready:").expect("ready descendant marker");
            pid.parse().expect("descendant pid marker")
        }

        fn wait_for_text(&self) -> String {
            // A nested libtest process can take longer than the usual process
            // startup budget when GitHub's Windows workers are contended. This
            // is a readiness bound only; the containment assertions retain
            // their tighter five-second shutdown bounds.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                if let Ok(text) = std::fs::read_to_string(&self.path) {
                    return text;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "strict worker did not publish descendant readiness"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }

    impl Drop for StrictWorkerTestMarker {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn create_job_object() -> io::Result<OwnedHandle> {
    let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if job.is_null() || job == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == FALSE {
        let err = io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        return Err(err);
    }
    Ok(OwnedHandle(job))
}

#[allow(dead_code)]
fn create_strict_worker_job(
    limits: StrictWorkerLimits,
) -> Result<OwnedHandle, (StrictWorkerSpawnStage, io::Error)> {
    let configuration = limits.job_configuration().map_err(|error| {
        (
            StrictWorkerSpawnStage::ConfigureJob,
            io::Error::new(io::ErrorKind::InvalidInput, format!("invalid strict worker limit: {error:?}")),
        )
    })?;
    let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if job.is_null() || job == INVALID_HANDLE_VALUE {
        return Err((StrictWorkerSpawnStage::CreateJob, io::Error::last_os_error()));
    }
    let job = OwnedHandle(job);
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    // Deliberately no JOB_OBJECT_LIMIT_BREAKAWAY_OK: this worker executes
    // hostile threaded Wasm and every descendant must remain killable.
    info.BasicLimitInformation.LimitFlags = configuration.limit_flags;
    if let Some(active_processes) = configuration.active_processes {
        info.BasicLimitInformation.ActiveProcessLimit = active_processes;
    }
    if let Some(process_memory_bytes) = configuration.process_memory_bytes {
        info.ProcessMemoryLimit = process_memory_bytes;
    }
    if let Some(job_memory_bytes) = configuration.job_memory_bytes {
        info.JobMemoryLimit = job_memory_bytes;
    }
    if unsafe {
        SetInformationJobObject(
            job.as_raw(),
            JobObjectExtendedLimitInformation,
            (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == FALSE {
        return Err((StrictWorkerSpawnStage::ConfigureJob, io::Error::last_os_error()));
    }
    Ok(job)
}

pub fn terminate(handle: &OwnedHandle) -> io::Result<()> {
    let ok = unsafe { TerminateProcess(handle.as_raw(), 1) };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn wait(handle: &OwnedHandle) -> io::Result<i32> {
    wait_inner(handle)
}

pub fn try_wait(handle: &OwnedHandle) -> io::Result<Option<i32>> {
    try_wait_inner(handle)
}

impl crate::platform::process::DaemonChildControl for OwnedHandle {
    fn kill(&mut self) -> io::Result<()> {
        terminate(self)
    }

    fn wait(&mut self) -> io::Result<i32> {
        wait(self)
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        try_wait(self)
    }
}

fn wait_inner(handle: &OwnedHandle) -> io::Result<i32> {
    let rc = unsafe { WaitForSingleObject(handle.as_raw(), INFINITE) };
    if rc != WAIT_OBJECT_0 {
        return Err(io::Error::last_os_error());
    }
    let mut code: DWORD = 0;
    let ok = unsafe { GetExitCodeProcess(handle.as_raw(), &mut code as *mut DWORD) };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }
    Ok(code as i32)
}

fn try_wait_inner(handle: &OwnedHandle) -> io::Result<Option<i32>> {
    let mut code: DWORD = 0;
    let ok = unsafe { GetExitCodeProcess(handle.as_raw(), &mut code as *mut DWORD) };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }
    if code == STILL_ACTIVE {
        Ok(None)
    } else {
        Ok(Some(code as i32))
    }
}

fn build_command_line<'a>(program: &OsStr, args: impl Iterator<Item = &'a OsStr>) -> Vec<u16> {
    let program_str = program.to_string_lossy().into_owned();
    let is_cmd = is_cmd_exe(&program_str);

    let arg_strs: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();

    let mut s = String::new();
    s.push_str(&quote(&program_str));

    // Special case for cmd.exe: when an arg of `/C` or `/K`
    // (case-insensitive) is followed by what we treat as the script,
    // do NOT apply CRT-style escaping to the script. cmd.exe parses
    // `\"` literally as backslash + quote, so the CRT's `"` → `\"`
    // escape rule corrupts paths inside the script (the redirect
    // target `"C:\path"` becomes `\"C:\path\"` which is no longer a
    // valid filename to cmd). With `/S`, cmd strips the outermost
    // pair of quotes around the whole script; everything else we
    // pass through untouched. See PR #116 for the diagnostic trail.
    let mut i = 0;
    while i < arg_strs.len() {
        let a = &arg_strs[i];
        s.push(' ');
        if is_cmd && is_cmd_script_switch(a) {
            // Emit the switch itself unquoted (it has no special chars),
            // then the remaining args concatenated with spaces, wrapped
            // in a single pair of outer quotes that `/S` will strip.
            s.push_str(a);
            let script = arg_strs[i + 1..].join(" ");
            if !script.is_empty() {
                s.push(' ');
                s.push('"');
                s.push_str(&script);
                s.push('"');
            }
            break;
        }
        s.push_str(&quote(a));
        i += 1;
    }

    OsStr::new(&s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn is_cmd_exe(program: &str) -> bool {
    // Match `cmd`, `cmd.exe`, or any path ending in those (case-insensitive).
    let lower = program.to_ascii_lowercase();
    let tail = std::path::Path::new(&lower)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or(lower);
    tail == "cmd" || tail == "cmd.exe"
}

fn is_cmd_script_switch(arg: &str) -> bool {
    matches!(arg.to_ascii_lowercase().as_str(), "/c" | "/k")
}

fn quote(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '"'))
    {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let chars: Vec<char> = arg.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let mut nbs = 0;
        while i < chars.len() && chars[i] == '\\' {
            nbs += 1;
            i += 1;
        }
        if i == chars.len() {
            for _ in 0..(nbs * 2) {
                out.push('\\');
            }
            break;
        } else if chars[i] == '"' {
            for _ in 0..(nbs * 2 + 1) {
                out.push('\\');
            }
            out.push('"');
        } else {
            for _ in 0..nbs {
                out.push('\\');
            }
            out.push(chars[i]);
        }
        i += 1;
    }
    out.push('"');
    out
}

fn build_env_block(
    overrides: Vec<(OsString, Option<OsString>)>,
    environment: crate::platform::process::SyncEnvironment,
) -> io::Result<Vec<u16>> {
    use std::collections::BTreeMap;
    // Windows env var names are case-INSENSITIVE at the kernel level
    // (CreateProcessW + the env block accept any case but
    // `GetEnvironmentVariable` lookups uppercase the name). If we dedup
    // case-sensitively, an inherited "Path" and a caller override of
    // "PATH" (or vice versa) end up as TWO entries in the block; the
    // kernel picks one (whichever sorts first) and the override is
    // silently dropped.
    //
    // Use the uppercased UTF-16 form as the canonical key. Preserve
    // the original case of the most recent insert for emit.
    let upper_key = |k: &OsStr| -> Vec<u16> {
        // Simple ASCII upper-fold via OsStr→u16 chain. Windows
        // CompareStringOrdinal uses a locale-independent uppercase
        // fold; for env-var names (overwhelmingly ASCII) the simple
        // version suffices and avoids a Win32 round-trip. Non-ASCII
        // keys still dedup as long as their bytes match exactly.
        k.encode_wide()
            .map(|c| {
                if (b'a' as u16..=b'z' as u16).contains(&c) {
                    c - (b'a' as u16 - b'A' as u16)
                } else {
                    c
                }
            })
            .collect()
    };

    let base = match environment.into_base()? {
        None => std::env::vars_os().collect(),
        Some(base) => base,
    };

    let mut env: BTreeMap<Vec<u16>, (OsString, OsString)> = BTreeMap::new();
    for (k, v) in base {
        env.insert(upper_key(&k), (k, v));
    }

    for (k, v) in overrides {
        let ck = upper_key(&k);
        match v {
            Some(val) => {
                env.insert(ck, (k, val));
            }
            None => {
                env.remove(&ck);
            }
        }
    }
    let mut block: Vec<u16> = Vec::new();
    for (_ck, (k, v)) in env {
        block.extend(k.encode_wide());
        block.push(b'=' as u16);
        block.extend(v.encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contained_spawn_failures_terminate_the_child() {
        for stage in ["assign", "resume", "duplicate"] {
            SYNC_SPAWN_FAILURE.with(|slot| *slot.borrow_mut() = (stage, None));
            let mut command = Command::new("ping.exe");
            command.args(["-n", "30", "127.0.0.1"]);
            let result = spawn_sync(
                &mut command,
                crate::platform::process::SpawnStdio::default(),
                crate::platform::process::SyncEnvironment::Inherit,
            );
            let error = match result {
                Ok(_) => panic!("injected {stage} failure must reject startup"),
                Err(error) => error,
            };
            assert!(error.to_string().contains(stage), "{error}");
            let observed = SYNC_SPAWN_FAILURE.with(|slot| slot.borrow_mut().1.take())
                .expect("fault checkpoint must observe a created child");
            assert_eq!(
                unsafe { WaitForSingleObject(observed.as_raw(), 5_000) },
                WAIT_OBJECT_0,
                "child survived {stage} startup failure"
            );
        }
    }

    #[test]
    fn failed_native_resume_is_reported() {
        let invalid = OwnedHandle(INVALID_HANDLE_VALUE);
        assert!(resume_contained_thread(&invalid).is_err());
    }

    struct EnvRestore {
        key: String,
        value: Option<OsString>,
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match &self.value {
                Some(value) => std::env::set_var(&self.key, value),
                None => std::env::remove_var(&self.key),
            }
        }
    }

    #[test]
    fn empty_environment_block_has_double_nul_terminator() {
        let block = build_env_block(
            Vec::new(),
            crate::platform::process::SyncEnvironment::Explicit(Vec::new()),
        )
        .unwrap();
        assert_eq!(block, vec![0, 0]);
    }

    #[test]
    fn shell_command_round_trips_through_sanitized_spawn() {
        let output = std::env::temp_dir().join(format!(
            "running-process shell command {}.txt",
            std::process::id()
        ));
        let command_text = format!("> \"{}\" echo alpha beta ^& gamma", output.display());
        let mut command = crate::platform_win::shell_command(&command_text);

        let mut child = spawn_sync(
            &mut command,
            crate::platform::process::SpawnStdio::default(),
            crate::platform::process::SyncEnvironment::Inherit,
        )
        .expect("sanitized shell spawn");
        assert_eq!(child
                .wait_within(std::time::Duration::from_secs(5))
                .expect("wait for sanitized shell spawn"), 0);
        assert_eq!(
            std::fs::read_to_string(&output).expect("read shell output"),
            "alpha beta & gamma\r\n"
        );

        let _ = std::fs::remove_file(output);
    }

    #[test]
    fn reused_command_inherits_a_fresh_environment_after_explicit_spawn() {
        let key = format!("RUNNING_PROCESS_REUSE_ENV_{}", std::process::id());
        let _restore = EnvRestore {
            value: std::env::var_os(&key),
            key: key.clone(),
        };
        let output = std::env::temp_dir().join(format!(
            "running-process-reused-command-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        let shell = std::env::var_os("COMSPEC")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"));
        let mut command = Command::new(shell);
        command
            .arg("/D")
            .arg("/C")
            .arg(format!("> \"{}\" echo %{}%", output.display(), key));

        std::env::remove_var(&key);
        let mut first = spawn_sync(
            &mut command,
            crate::platform::process::SpawnStdio::default(),
            crate::platform::process::SyncEnvironment::Explicit(vec![(
                OsString::from(&key),
                OsString::from("stale-value"),
            )]),
        )
        .expect("explicit spawn");
        assert_eq!(first
                .wait_within(std::time::Duration::from_secs(5))
                .expect("wait for explicit spawn"), 0);
        assert!(std::fs::read_to_string(&output)
            .expect("read explicit environment output")
            .contains("stale-value"));

        std::env::set_var(&key, "fresh-value");
        let mut second = spawn_sync(
            &mut command,
            crate::platform::process::SpawnStdio::default(),
            crate::platform::process::SyncEnvironment::Inherit,
        )
        .expect("inherited spawn");
        assert_eq!(second
                .wait_within(std::time::Duration::from_secs(5))
                .expect("wait for inherited spawn"), 0);
        assert!(std::fs::read_to_string(&output)
            .expect("read inherited environment output")
            .contains("fresh-value"));

        let _ = std::fs::remove_file(output);
    }
}
