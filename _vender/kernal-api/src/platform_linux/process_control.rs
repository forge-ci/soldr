//! Linux child-control, standard-stream, and jobserver mechanics.
//!
//! The neutral contracts live in `crate::platform::process`; this file only
//! answers them for Linux.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::platform::process::{
    ProcessId, ProcessIdentity, ProcessIdentityAction, ProcessIdentityActionError,
    ProcessIdentityCapture,
};

/// Make a synchronous child the leader of a new session (`setsid`).
pub fn configure_session_leader_command(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs between fork and exec and calls only the
    // async-signal-safe `setsid`, touching no Rust-owned state.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

/// Force-kill the process group led by an unreaped child of this process.
pub fn force_terminate_child_process_group(child: &std::process::Child) -> io::Result<()> {
    let pid = ProcessId::new(child.id())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    ensure_unreaped_child(pid)?;
    // SAFETY: `killpg` receives a validated, positive group id.
    if unsafe { libc::killpg(pid.native_signed(), libc::SIGKILL) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

/// Prove `pid` is still a child this process has not reaped.
///
/// `WNOWAIT` leaves a zombie waitable, so the owner's own `wait` still sees
/// it. While the child is unreaped the kernel cannot reissue its PID, and so
/// cannot reissue the process-group id it leads either.
fn ensure_unreaped_child(pid: ProcessId) -> io::Result<()> {
    loop {
        // SAFETY: zeroed POD out-parameter, written by the kernel only.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: scalar id and flags; `info` outlives the call.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid.get() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::ECHILD) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "child was already reaped; its process group can no longer be proven",
                ))
            }
            _ => return Err(error),
        }
    }
}

/// Apply a scheduling band to exactly `identity`.
pub fn set_priority_identity(
    identity: ProcessIdentity,
    priority: crate::ProcessPriority,
) -> Result<ProcessIdentityAction, ProcessIdentityActionError> {
    let nice = match priority {
        crate::ProcessPriority::Normal => None,
        crate::ProcessPriority::Low => Some(10),
        crate::ProcessPriority::Idle => Some(19),
        crate::ProcessPriority::High => Some(-5),
    };
    match crate::platform_imp::capture_process_identity(identity.pid()) {
        ProcessIdentityCapture::Found(current) if current == identity => {}
        ProcessIdentityCapture::Found(_) => return Err(ProcessIdentityActionError::StaleIdentity),
        ProcessIdentityCapture::Exited => return Ok(ProcessIdentityAction::AlreadyExited),
        ProcessIdentityCapture::Unavailable(reason) => {
            return Err(ProcessIdentityActionError::Unavailable(reason))
        }
        ProcessIdentityCapture::Error(error) => return Err(ProcessIdentityActionError::Host(error)),
    }
    // `Normal` preserves whatever policy the process already has.
    let Some(nice) = nice else {
        return Ok(ProcessIdentityAction::Performed);
    };
    let pid = ProcessId::new(identity.pid()).map_err(|error| {
        ProcessIdentityActionError::Host(io::Error::new(io::ErrorKind::InvalidInput, error))
    })?;
    // SAFETY: scalar arguments only.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, pid.get(), nice) } == 0 {
        return Ok(ProcessIdentityAction::Performed);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(ProcessIdentityAction::AlreadyExited)
    } else {
        Err(ProcessIdentityActionError::Host(error))
    }
}

/// Replace this process's three standard streams with `/dev/null`.
pub fn detach_standard_streams() {
    // SAFETY: a constant NUL-terminated path.
    let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if null < 0 {
        return;
    }
    for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: both descriptors are valid; `dup2` clears CLOEXEC on target.
        let _ = unsafe { libc::dup2(null, target) };
    }
    if null > libc::STDERR_FILENO {
        // SAFETY: `null` is owned here and closed exactly once.
        let _ = unsafe { libc::close(null) };
    }
}

/// Redirect stdin to `/dev/null` and stdout/stderr to an append-only log.
pub fn redirect_standard_streams_to_log(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a constant NUL-terminated path.
    let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if null < 0 {
        return false;
    }
    // SAFETY: both descriptors are valid.
    let _ = unsafe { libc::dup2(null, libc::STDIN_FILENO) };
    if null > libc::STDERR_FILENO {
        // SAFETY: `null` is owned here and closed exactly once.
        let _ = unsafe { libc::close(null) };
    }
    // SAFETY: `path` is NUL-terminated and outlives the call.
    let log = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND | libc::O_CLOEXEC,
            0o644,
        )
    };
    if log < 0 {
        return false;
    }
    // SAFETY: both descriptors are valid.
    let _ = unsafe { libc::dup2(log, libc::STDOUT_FILENO) };
    let _ = unsafe { libc::dup2(log, libc::STDERR_FILENO) };
    if log > libc::STDERR_FILENO {
        // SAFETY: `log` is owned here and closed exactly once.
        let _ = unsafe { libc::close(log) };
    }
    true
}

/// Linux offers the GNU make `R,W` descriptor-pair jobserver.
pub fn native_jobserver_supported() -> bool {
    true
}

/// A host-owned GNU make jobserver pipe pair.
#[derive(Debug)]
pub struct NativeJobserver {
    read: OwnedFd,
    write: OwnedFd,
}

impl NativeJobserver {
    /// Construct and prime a jobserver with `capacity` tokens.
    pub fn create(capacity: usize) -> io::Result<Self> {
        if capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "jobserver capacity must be greater than zero",
            ));
        }
        let mut fds = [0_i32; 2];
        // SAFETY: `fds` is a writable two-element array.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `pipe2` succeeded, so both descriptors are fresh and owned.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let tokens = vec![b'+'; capacity];
        // SAFETY: `tokens` is readable for its full length.
        let written =
            unsafe { libc::write(write.as_raw_fd(), tokens.as_ptr().cast(), tokens.len()) };
        if written < 0 {
            return Err(io::Error::last_os_error());
        }
        if written as usize != tokens.len() {
            return Err(io::Error::other(format!(
                "jobserver pipe priming wrote {written} of {} bytes",
                tokens.len()
            )));
        }
        Ok(Self { read, write })
    }

    /// The GNU make `--jobserver-auth` descriptor pair, `R,W`.
    pub fn auth_string(&self) -> String {
        format!("{},{}", self.read.as_raw_fd(), self.write.as_raw_fd())
    }
}
