//! Awaiting another process's exit (Windows).

use std::io;
use std::ptr;
use std::sync::Arc;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetExitCodeProcess, OpenProcess, SetEvent, WaitForMultipleObjects,
    WaitForSingleObject, INFINITE, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::platform::process::{
    ProcessExitObservation, ProcessId, ProcessInspectError, ProcessInspectErrorKind,
    ProcessSessionExit,
};

/// The right to wait on a kernel object.
///
/// Spelled out here rather than imported: `windows-sys` files it under the
/// filesystem namespace, which is a generator artefact and would read at this
/// call site as though a file were involved.
pub(crate) const SYNCHRONIZE: u32 = 0x0010_0000;

/// `WaitForMultipleObjects` reports this when the wait itself failed.
const WAIT_FAILED: u32 = 0xFFFF_FFFF;

/// A subscription to one process's exit, held open until it arrives.
///
/// The open handle *is* the reuse safety. Windows will not reissue a PID
/// while any handle to that process remains open, so a handle taken once
/// keeps naming the same process -- including after it exits, when it becomes
/// a handle to a known-dead process rather than a stale number.
///
/// That same property is why acquisition needs a second question here. On the
/// Unix hosts a process that has been waited for is simply gone, and the
/// acquiring syscall says so. On this one the process *object* outlives the
/// process for as long as anybody holds a handle -- its own parent's `Child`
/// will do -- and `OpenProcess` keeps handing out handles to it. Opening is
/// therefore not by itself evidence of a live target, and [`Self::open`] asks.
///
/// This host is the generous one about status: `GetExitCodeProcess` answers
/// through any handle with query rights, parent or not, so the exit code is
/// reported where Linux and macOS have nothing to give.
pub struct ProcessExitWatch {
    pid: ProcessId,
    process: Arc<KernelHandle>,
}

impl std::fmt::Debug for ProcessExitWatch {
    /// Names the process, not the handle.
    ///
    /// The handle value is an artefact of this process's own table; printing
    /// it invites a reader to compare two values that were never comparable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessExitWatch")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl ProcessExitWatch {
    /// Subscribe to `pid`'s exit, failing if it is not running now.
    ///
    /// Synchronous and runtime-free: acquisition is the part that must happen
    /// at a known moment, and making it `async` would put a scheduling delay
    /// between the caller's decision and the kernel pinning the target.
    ///
    /// `SYNCHRONIZE` is requested alongside query rights because a handle
    /// opened for queries alone cannot be waited on -- the access mask is
    /// checked when the wait starts, not when it would have completed. It is
    /// also what lets acquisition ask whether the target is still running.
    ///
    /// An exited process that somebody still holds a handle to is reported
    /// gone, like one this host has finished with entirely. The two are
    /// different situations for the kernel and the same one for the caller:
    /// there is no exit left to wait for, and a watch that returned
    /// immediately would be a subscription to something that already
    /// happened, which is not what asking for one means.
    pub fn open(pid: ProcessId) -> Result<Self, ProcessInspectError> {
        // SAFETY: the call takes access flags, an inherit flag, and a PID by
        // value; the returned handle is checked before use.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
                0,
                pid.get(),
            )
        };
        if handle.is_null() {
            let source = io::Error::last_os_error();
            let kind = match source.raw_os_error() {
                Some(code) if code == ERROR_INVALID_PARAMETER as i32 => {
                    ProcessInspectErrorKind::NotFound
                }
                _ => ProcessInspectErrorKind::Host,
            };
            return Err(ProcessInspectError { kind, source });
        }

        // Owned before the liveness question, so every path below closes it.
        let process = KernelHandle(handle);
        if has_already_exited(&process) {
            return Err(ProcessInspectError::stated(
                ProcessInspectErrorKind::NotFound,
                "no such process",
            ));
        }
        Ok(Self {
            pid,
            process: Arc::new(process),
        })
    }

    /// The process this watch was opened for.
    #[must_use]
    pub fn pid(&self) -> ProcessId {
        self.pid
    }

    /// Wait until that process exits.
    ///
    /// This host has no readiness descriptor to hand a reactor, so the wait
    /// happens on a blocking worker. The second waited-on object is a cancel
    /// event signalled when this future is dropped, so giving up on the wait
    /// releases the worker instead of parking it until the target happens to
    /// exit.
    ///
    /// Waiting is not reaping: a `PlatformChild` or `std::process::Child` for
    /// the same process still waits successfully afterwards.
    pub async fn exited(&self) -> Result<ProcessExitObservation, ProcessInspectError> {
        let cancel = Arc::new(create_cancel_event()?);
        let process = Arc::clone(&self.process);
        let waiter = Arc::clone(&cancel);
        let release = CancelOnDrop(cancel);
        let outcome =
            crate::async_engine::launch_blocking(move || wait_for_exit(&process, &waiter))
                .await
                .map_err(|error| ProcessInspectError {
                    kind: ProcessInspectErrorKind::Host,
                    source: io::Error::other(error.to_string()),
                })?;
        drop(release);
        outcome
    }
}

/// A kernel handle this crate owns and closes exactly once.
pub(crate) struct KernelHandle(pub(crate) HANDLE);

// SAFETY: a process or event handle is a kernel object usable from any
// thread; the value is opaque here and never dereferenced.
unsafe impl Send for KernelHandle {}
unsafe impl Sync for KernelHandle {}

impl Drop for KernelHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from OpenProcess or CreateEventW and is
        // closed once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Signals the cancel event when the awaiting future goes away.
struct CancelOnDrop(Arc<KernelHandle>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        // SAFETY: the event handle is live for as long as this Arc is.
        unsafe {
            SetEvent(self.0 .0);
        }
    }
}

/// Whether this handle already names a finished process.
///
/// A process handle is signalled from the moment the process exits, and a
/// zero-length wait reads that without blocking. `GetExitCodeProcess` against
/// `STILL_ACTIVE` would answer the same question wrongly: 259 is a legal exit
/// code, so a process that chose it would be called alive forever.
///
/// The handle must have been opened with [`SYNCHRONIZE`]. Without that right
/// the wait fails rather than answering, and a failure is not a signal, so
/// this reports `false` -- which a caller reads as "still running". Callers
/// that may hold a query-only handle must know which one they have; see
/// [`ProcessLiveness`](crate::ProcessLiveness).
pub(crate) fn has_already_exited(process: &KernelHandle) -> bool {
    // SAFETY: the handle is live and was opened with SYNCHRONIZE; a zero
    // timeout makes this a question rather than a wait.
    let waited = unsafe { WaitForSingleObject(process.0, 0) };
    waited == WAIT_OBJECT_0
}

/// A manual-reset event, so a cancellation set once stays set.
fn create_cancel_event() -> Result<KernelHandle, ProcessInspectError> {
    // SAFETY: default security, manual reset, initially unsignalled, unnamed;
    // the returned handle is checked before use.
    let handle = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if handle.is_null() {
        return Err(ProcessInspectError::last_os_error(
            ProcessInspectErrorKind::Host,
        ));
    }
    Ok(KernelHandle(handle))
}

fn wait_for_exit(
    process: &KernelHandle,
    cancel: &KernelHandle,
) -> Result<ProcessExitObservation, ProcessInspectError> {
    let handles = [process.0, cancel.0];
    // SAFETY: two live handles are described, `bWaitAll` is false, and the
    // wait is released either by the process exiting or by cancellation.
    let waited = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
    if waited == WAIT_OBJECT_0 {
        return Ok(exit_status(process));
    }
    if waited == WAIT_FAILED {
        return Err(ProcessInspectError::last_os_error(
            ProcessInspectErrorKind::Host,
        ));
    }
    Err(ProcessInspectError::stated(
        ProcessInspectErrorKind::Host,
        "the exit wait was cancelled",
    ))
}

fn exit_status(process: &KernelHandle) -> ProcessExitObservation {
    let mut code = 0_u32;
    // SAFETY: the handle is live and the out-parameter is an initialised u32.
    let ok = unsafe { GetExitCodeProcess(process.0, &mut code) };
    if ok == 0 {
        return ProcessExitObservation::Unreported;
    }
    // The DWORD is kept whole in the native status; the signed convenience
    // code is the same bits read the way a caller expects to compare them.
    ProcessExitObservation::Reported(ProcessSessionExit::from_native(
        Some(code as i32),
        None,
        code,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process that is not there cannot be subscribed to, which is the
    /// acquisition half of reuse safety: no handle, so nothing to retarget.
    ///
    /// Asserted twice on purpose. While the `Child` is still held, the process
    /// object is alive and `OpenProcess` succeeds -- so the first assertion is
    /// the one that pins this host to the same contract the Unix hosts get for
    /// free. The second is the ordinary case that follows once every handle is
    /// closed.
    #[test]
    fn a_reaped_process_cannot_be_watched() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "exit 0"])
            .spawn()
            .expect("spawn");
        let pid = ProcessId::new(child.id()).expect("child pid is in range");
        child.wait().expect("reap");

        let retained = ProcessExitWatch::open(pid).expect_err("an exited process has no exit left");
        assert_eq!(retained.kind, ProcessInspectErrorKind::NotFound);

        drop(child);
        let released = ProcessExitWatch::open(pid).expect_err("a reaped pid is gone");
        assert_eq!(released.kind, ProcessInspectErrorKind::NotFound);
    }

    /// A watch names the process it was opened for, and this host will open
    /// one for any process it can query rather than only for its children.
    #[test]
    fn a_watch_can_be_opened_for_a_process_this_one_did_not_spawn() {
        let watch = ProcessExitWatch::open(ProcessId::current()).expect("open self");
        assert_eq!(watch.pid(), ProcessId::current());
    }
}
