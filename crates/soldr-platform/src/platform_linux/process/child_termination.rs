//! Scoped cargo-front-door signal handling (soldr#4216, soldr#4013).
//! Deliberately mirrored across the concrete Unix boundaries.

use std::io;
use std::sync::atomic::{AtomicI32, Ordering};

static RECEIVED: AtomicI32 = AtomicI32::new(0);

extern "C" fn record_signal(signal: libc::c_int) {
    let _ = RECEIVED.compare_exchange(0, signal, Ordering::Relaxed, Ordering::Relaxed);
}

/// Own the dispositions for one supervised Cargo child. The handler only records
/// the request; the bounded child wait forwards it and escalates outside the
/// async-signal context. Dropping the guard restores the caller's dispositions.
pub struct ChildTerminationGuard {
    term: libc::sigaction,
    interrupt: libc::sigaction,
}

fn install_handler(signal: libc::c_int) -> io::Result<libc::sigaction> {
    // SAFETY: the handler only updates a lock-free atomic; sigaction buffers
    // remain valid for the call and the kernel copies their contents.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        let mut old: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = record_signal as extern "C" fn(libc::c_int) as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(signal, &action, &mut old) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(old)
    }
}

impl ChildTerminationGuard {
    /// Install before spawning Cargo so signals racing the spawn are retained.
    pub fn install() -> io::Result<Self> {
        RECEIVED.store(0, Ordering::Relaxed);
        let term = install_handler(libc::SIGTERM)?;
        match install_handler(libc::SIGINT) {
            Ok(interrupt) => Ok(Self { term, interrupt }),
            Err(err) => {
                // SAFETY: restore the disposition captured above.
                unsafe { libc::sigaction(libc::SIGTERM, &term, std::ptr::null_mut()) };
                Err(err)
            }
        }
    }
}

impl Drop for ChildTerminationGuard {
    fn drop(&mut self) {
        // SAFETY: restore the dispositions this guard captured at installation.
        unsafe {
            libc::sigaction(libc::SIGTERM, &self.term, std::ptr::null_mut());
            libc::sigaction(libc::SIGINT, &self.interrupt, std::ptr::null_mut());
        }
    }
}

/// Forward the first requested signal, reporting whether shutdown was requested.
/// An inherited group must not be signalled back into the Soldr parent itself.
pub fn forward_child_termination(pid: u32) -> io::Result<bool> {
    let signal = RECEIVED.load(Ordering::Relaxed);
    if signal == 0 {
        return Ok(false);
    }
    let pid = libc::pid_t::try_from(pid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid child pid"))?;
    // SAFETY: query the child group, then signal only its group or its own pid.
    let result = unsafe {
        let target = if libc::getpgid(pid) == pid { -pid } else { pid };
        libc::kill(target, signal)
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(true)
}

/// Kill a process group created and owned by the caller, even after its leader exits.
pub fn kill_owned_process_group(pid: u32) -> io::Result<()> {
    let pgid = libc::pid_t::try_from(pid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid owned group"))?;
    // SAFETY: the caller owns this positive group id; never signal our ambient group.
    if unsafe { libc::kill(-pgid, libc::SIGKILL) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}
