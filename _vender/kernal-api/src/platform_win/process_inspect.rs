//! Asking this host about another process (Windows).

use std::io;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};

use super::process_exit_watch::{has_already_exited, KernelHandle, SYNCHRONIZE};
use crate::platform::process::{ProcessId, ProcessInspectError, ProcessInspectErrorKind};

/// `GetExitCodeProcess` reports this while a process is still running.
///
/// It is also a perfectly legal exit code, so a process that exits with 259
/// is indistinguishable from a running one by this call alone. That is why
/// the compare below is only the fallback: where the target can be waited on,
/// [`has_already_exited`] answers without the ambiguity.
const STILL_ACTIVE: u32 = 259;

/// A live reference to another process, good for as long as it is held.
///
/// The open handle is the identity. Windows will not reissue a PID while any
/// handle to that process remains open, so a handle taken once keeps naming
/// the same process -- including after it exits, when it becomes a handle to
/// a known-dead process rather than a stale number.
pub struct ProcessLiveness {
    pid: u32,
    process: KernelHandle,
    /// Whether the handle carries `SYNCHRONIZE`, and so can be asked.
    waitable: bool,
}

impl std::fmt::Debug for ProcessLiveness {
    /// Names the process, not the handle.
    ///
    /// The underlying descriptor or handle value is an artefact of this
    /// process's own table; printing it invites a reader to compare two
    /// numbers that were never comparable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessLiveness")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl ProcessLiveness {
    /// Take a reference to `pid`, failing if no such process is running.
    ///
    /// A refusal is not an absence. This host reports a PID it has never
    /// issued, or has finished with, as `ERROR_INVALID_PARAMETER`; every other
    /// failure -- most of all `ERROR_ACCESS_DENIED` for a process this one may
    /// not query -- describes a process that exists and is reported as a host
    /// failure, so a caller cannot read "I was refused" as "it is gone".
    ///
    /// `SYNCHRONIZE` is asked for on top of the query right because it is what
    /// makes [`Self::is_alive`] unambiguous, and it is asked for *as well as*
    /// rather than *instead of*: a caller allowed to query a process but not
    /// to wait on it still deserves the best answer this host can give, so a
    /// refused wide open falls back to the query-only open that has always
    /// been made here. The narrow attempt is therefore the deciding one --
    /// every open that succeeds without this right still succeeds, and a
    /// failure is the same failure, mapped the same way.
    pub fn open(pid: u32) -> Result<Self, ProcessInspectError> {
        // This host has no signed-PID trap of its own, but it shares the range
        // rule so a PID written down here means the same thing when a Unix
        // host reads it back. See `ProcessId`.
        let pid = ProcessId::new(pid)?.get();
        // SAFETY: the call takes access flags, an inherit flag, and a PID by
        // value; the returned handle is checked before use.
        let wide =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
        if !wide.is_null() {
            return Ok(Self {
                pid,
                process: KernelHandle(wide),
                waitable: true,
            });
        }
        // SAFETY: as above.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            let source = io::Error::last_os_error();
            if source.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                return Err(ProcessInspectError::stated(
                    ProcessInspectErrorKind::NotFound,
                    "no such process",
                ));
            }
            return Err(ProcessInspectError {
                kind: ProcessInspectErrorKind::Host,
                source,
            });
        }
        Ok(Self {
            pid,
            process: KernelHandle(handle),
            waitable: false,
        })
    }

    /// The process ID this handle was opened for.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether that process is still running.
    ///
    /// A process handle is signalled from the moment the process exits, so
    /// where [`Self::open`] was granted `SYNCHRONIZE` the answer comes from a
    /// zero-length wait and is exact for every exit code.
    ///
    /// Where that right was refused, the only question left to ask is
    /// `GetExitCodeProcess`, which cannot tell a running process from one that
    /// exited with 259 -- `STILL_ACTIVE` is that number. On that path alone a
    /// process that chose 259 is reported alive for as long as this handle is
    /// held. The narrower answer is still the better one available: it is
    /// wrong for one exit code out of four billion, where refusing to answer
    /// would be useless for all of them.
    pub fn is_alive(&self) -> bool {
        if self.waitable {
            return !has_already_exited(&self.process);
        }
        let mut exit_code = 0_u32;
        // SAFETY: the handle is live for this value's lifetime and the
        // out-parameter is a valid initialised u32.
        let ok = unsafe { GetExitCodeProcess(self.process.0, &mut exit_code) };
        ok != 0 && exit_code == STILL_ACTIVE
    }

    /// Whether that process has exited, or why the host could not tell.
    ///
    /// Where [`Self::is_alive`] folds every failed observation into "not
    /// alive", this reports it: `Err` means the question itself failed, and
    /// says nothing about the process. The retained handle is asked with a
    /// zero-length wait where it carries `SYNCHRONIZE`, and through
    /// `GetExitCodeProcess` otherwise -- where, as for [`Self::is_alive`], an
    /// exit with code 259 is indistinguishable from running and is reported
    /// as not exited.
    pub fn has_exited(&self) -> io::Result<bool> {
        if self.waitable {
            // SAFETY: the handle is live for this value's lifetime and was
            // opened with SYNCHRONIZE; a zero timeout makes this a question
            // rather than a wait.
            let waited = unsafe { WaitForSingleObject(self.process.0, 0) };
            return match waited {
                WAIT_OBJECT_0 => Ok(true),
                WAIT_TIMEOUT => Ok(false),
                WAIT_FAILED => Err(io::Error::last_os_error()),
                other => Err(io::Error::other(format!(
                    "unexpected process wait result {other:#x}"
                ))),
            };
        }
        let mut exit_code = 0_u32;
        // SAFETY: the handle is live for this value's lifetime and the
        // out-parameter is a valid initialised u32.
        if unsafe { GetExitCodeProcess(self.process.0, &mut exit_code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(exit_code != STILL_ACTIVE)
    }
}

/// Resolve the on-disk image a running process was started from.
pub fn process_executable_path(pid: u32) -> Result<PathBuf, io::Error> {
    // SAFETY: see `ProcessLiveness::open`.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }

    let mut path = vec![0_u16; 32768];
    let mut len = path.len() as u32;
    // SAFETY: `path` is valid for `len` wide characters, and `len` is updated
    // in place to the number actually written.
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut len) };
    let source = io::Error::last_os_error();
    // SAFETY: the handle came from OpenProcess above and is closed once.
    unsafe {
        CloseHandle(handle);
    }
    if ok == 0 {
        return Err(source);
    }

    path.truncate(len as usize);
    Ok(PathBuf::from(String::from_utf16_lossy(&path)))
}

/// Ask a process to stop.
///
/// This host has no signal that asks. Terminating without asking is a
/// different operation with different consequences for the target, so it is
/// reported as unsupported rather than quietly substituted.
#[allow(dead_code)] // PID-only mutation remains private and is not a facade operation.
pub fn process_signal_terminate(_pid: u32) -> Result<(), ProcessInspectError> {
    Err(ProcessInspectError::stated(
        ProcessInspectErrorKind::Unsupported,
        "this host has no graceful terminate signal",
    ))
}

/// Stop a process without asking.
#[allow(dead_code)] // PID-only mutation remains private and is not a facade operation.
pub fn process_force_kill(pid: u32) -> Result<(), ProcessInspectError> {
    // SAFETY: see `ProcessLiveness::open`.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        return Err(ProcessInspectError::stated(
            ProcessInspectErrorKind::NotFound,
            "no such process",
        ));
    }
    // SAFETY: `handle` is live and was opened with PROCESS_TERMINATE.
    let ok = unsafe { TerminateProcess(handle, 1) };
    let source = io::Error::last_os_error();
    // SAFETY: the handle came from OpenProcess above and is closed once.
    unsafe {
        CloseHandle(handle);
    }
    if ok == 0 {
        Err(ProcessInspectError {
            kind: ProcessInspectErrorKind::Host,
            source,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PID zero names no process this host will open, and is refused by the
    /// shared range rule before the kernel is asked.
    #[test]
    fn pid_zero_is_never_valid() {
        let error = ProcessLiveness::open(0).expect_err("pid 0");
        assert_eq!(error.kind, ProcessInspectErrorKind::InvalidPid);
    }

    /// This process is alive, and knows where it was started from.
    #[test]
    fn this_process_is_alive_and_locatable() {
        let me = std::process::id();
        let handle = ProcessLiveness::open(me).expect("open self");
        assert_eq!(handle.pid(), me);
        assert!(handle.is_alive());
        assert_eq!(
            process_executable_path(me).expect("exe"),
            std::env::current_exe().expect("current_exe")
        );
    }

    /// A handle keeps naming the process it was opened for, and reports it
    /// dead once it exits rather than failing to find it.
    #[test]
    fn a_dead_process_reports_dead() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "exit 0"])
            .spawn()
            .expect("spawn");
        let handle = ProcessLiveness::open(child.id()).expect("open child");
        child.wait().expect("wait");
        assert!(!handle.is_alive(), "an exited child must report dead");
        assert!(handle.has_exited().expect("observe exit"));
    }

    /// The fallible question agrees with the infallible one on both paths.
    #[test]
    fn has_exited_answers_through_both_handle_kinds() {
        let me = ProcessLiveness::open(std::process::id()).expect("open self");
        assert!(!me.has_exited().expect("observe self"));

        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "exit 3"])
            .spawn()
            .expect("spawn");
        let waitable = ProcessLiveness::open(child.id()).expect("open child");
        let mut query_only = ProcessLiveness::open(child.id()).expect("open child");
        query_only.waitable = false;
        child.wait().expect("wait");
        assert!(waitable.has_exited().expect("wait-based observation"));
        assert!(query_only.has_exited().expect("exit-code observation"));
    }

    /// 259 is a legal exit code, and an exit is an exit whichever number it
    /// carried.
    ///
    /// This is the one exit code `GetExitCodeProcess` cannot report, because
    /// it is the value that call also uses for "still running". A handle
    /// opened with `SYNCHRONIZE` is asked instead, and the wait knows the
    /// difference.
    #[test]
    fn a_child_that_exits_with_the_still_active_code_reports_dead() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "exit 259"])
            .spawn()
            .expect("spawn");
        let handle = ProcessLiveness::open(child.id()).expect("open child");
        let status = child.wait().expect("wait");
        assert_eq!(
            status.code(),
            Some(STILL_ACTIVE as i32),
            "the child must actually have exited with the ambiguous code"
        );
        assert!(
            !handle.is_alive(),
            "an exit code of 259 is an exit, not a running process"
        );
    }

    /// The fallback answers as well as it can, and no better.
    ///
    /// A query-only handle is what a caller gets when this host grants the
    /// query right and withholds `SYNCHRONIZE`. That combination cannot be
    /// arranged for a child here, so the handle is downgraded by hand -- the
    /// rights are a superset, so the fallback call is the same call it would
    /// make. It reports the 259 exit as alive, which is the documented limit
    /// of that path rather than an accident.
    #[test]
    fn without_synchronize_the_still_active_code_is_ambiguous() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "exit 259"])
            .spawn()
            .expect("spawn");
        let mut handle = ProcessLiveness::open(child.id()).expect("open child");
        handle.waitable = false;
        child.wait().expect("wait");
        assert!(
            handle.is_alive(),
            "the fallback cannot tell 259 from STILL_ACTIVE, and says so"
        );
        assert!(!handle.has_exited().expect("exit-code observation"));
    }

    /// Asking politely is not silently upgraded to terminating.
    #[test]
    fn graceful_terminate_is_reported_unsupported() {
        let error = process_signal_terminate(std::process::id()).expect_err("unsupported");
        assert_eq!(error.kind, ProcessInspectErrorKind::Unsupported);
    }
}

/// Whether two spellings name the same executable image on this host.
///
/// This host's paths are case-insensitive, and it reports long paths with a
/// `\\?\` prefix that the same file is equally reachable without. Comparing
/// the two spellings literally would call one image two different files.
///
/// Both sides are canonicalised first where the file is reachable; a path
/// that cannot be canonicalised is compared as written rather than treated as
/// a mismatch, because "the file moved" and "the caller lacks permission to
/// resolve it" arrive here identically.
pub fn process_same_executable_path(actual: &std::path::Path, expected: &std::path::Path) -> bool {
    comparable(actual) == comparable(expected)
}

fn comparable(path: &std::path::Path) -> String {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let path = path.to_string_lossy().replace('\\', "/");
    let path = path.strip_prefix("//?/").unwrap_or(&path);
    path.to_ascii_lowercase()
}

#[cfg(test)]
mod path_tests {
    use super::*;
    use std::path::Path;

    /// Case and the verbatim prefix are spelling, not identity.
    #[test]
    fn spelling_differences_do_not_make_two_images() {
        assert!(process_same_executable_path(
            Path::new(r"C:\Windows\System32\cmd.exe"),
            Path::new(r"c:\windows\system32\CMD.EXE"),
        ));
        assert!(process_same_executable_path(
            Path::new(r"\\?\C:\tmp\daemon.exe"),
            Path::new(r"C:\tmp\daemon.exe"),
        ));
    }

    /// Two genuinely different images still compare different.
    #[test]
    fn different_images_are_still_different() {
        assert!(!process_same_executable_path(
            Path::new(r"C:\tmp\daemon.exe"),
            Path::new(r"C:\tmp\other.exe"),
        ));
    }
}
