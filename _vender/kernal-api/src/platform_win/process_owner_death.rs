//! Owner-death containment for this process (Windows).

use std::io;
use std::sync::OnceLock;

use crate::platform::process::{
    LifetimeEnforcement, OwnerDeathCleanup, OwnerDeathCleanupError, OwnerDeathCleanupStage,
};

/// The job this process was placed in, kept alive for the process's lifetime.
///
/// The containment *is* the handle being open: a kill-on-close job destroys
/// its members when the last handle to it closes. Dropping this would
/// terminate the very process that installed it, so it is parked here and
/// never taken out again.
static JOB: OnceLock<JobHandle> = OnceLock::new();

/// Put this process in a job the kernel destroys along with its owner.
///
/// Idempotent: a second call reports the containment already in place rather
/// than building a second job, because a process can only be in one.
pub fn install_owner_death_cleanup() -> Result<OwnerDeathCleanup, OwnerDeathCleanupError> {
    if JOB.get().is_some() {
        return Ok(OwnerDeathCleanup::KillOnOwnerHandleClose);
    }

    let job = create_kill_on_close_job()?;
    match assign_current_process(job.raw()) {
        Ok(()) => match JOB.set(job) {
            Ok(()) => Ok(OwnerDeathCleanup::KillOnOwnerHandleClose),
            Err(job) => {
                // Two threads installed at once and the other won. Closing
                // this duplicate would close a handle to a job that now
                // contains us, terminating the process. Leaking one handle is
                // the cheaper of the two outcomes.
                std::mem::forget(job);
                Ok(OwnerDeathCleanup::AlreadyContained)
            }
        },
        // Already in a job someone else created -- a CI runner, a debugger,
        // a container shim. That is containment, just not ours.
        Err(error) if is_access_denied(&error) => Ok(OwnerDeathCleanup::AlreadyContained),
        Err(error) => Err(error),
    }
}

/// What this host will attempt, without attempting it.
pub fn owner_death_cleanup_target() -> OwnerDeathCleanup {
    OwnerDeathCleanup::KillOnOwnerHandleClose
}

/// What binding a child to *this* process's lifetime obtains here.
///
/// The spawn path assigns the child to a process-wide `KILL_ON_JOB_CLOSE` job
/// object, and a failure to do so is reported rather than swallowed -- so a
/// spawn that succeeded got this. It is the strongest of the three: it covers
/// descendants, and it holds however the owner dies, including a
/// `TerminateProcess` that runs no user code at all.
pub fn spawner_lifetime_enforcement() -> LifetimeEnforcement {
    LifetimeEnforcement::KernelContainer
}

struct JobHandle(winapi::um::winnt::HANDLE);

// SAFETY: a job handle is a kernel object usable from any thread; the raw
// pointer is opaque and never dereferenced.
unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

impl JobHandle {
    fn raw(&self) -> winapi::um::winnt::HANDLE {
        self.0
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from CreateJobObjectW and is closed once.
        unsafe {
            winapi::um::handleapi::CloseHandle(self.0);
        }
    }
}

fn create_kill_on_close_job() -> Result<JobHandle, OwnerDeathCleanupError> {
    use winapi::um::jobapi2::{CreateJobObjectW, SetInformationJobObject};
    use winapi::um::winnt::{
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    // SAFETY: an unnamed job with default security is what the null arguments
    // request; the returned handle is checked before use.
    let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if handle.is_null() {
        return Err(created(io::Error::last_os_error()));
    }
    let job = JobHandle(handle);

    // SAFETY: an all-zero extended-limit struct is a valid one; only the
    // kill-on-close flag is then set.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `info` is valid for the length passed, and the class matches it.
    let set = unsafe {
        SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if set == 0 {
        return Err(created(io::Error::last_os_error()));
    }
    Ok(job)
}

fn assign_current_process(
    job: winapi::um::winnt::HANDLE,
) -> Result<(), OwnerDeathCleanupError> {
    use winapi::um::jobapi2::AssignProcessToJobObject;
    use winapi::um::processthreadsapi::GetCurrentProcess;

    // SAFETY: the current-process pseudo-handle is owned by Windows and never
    // closed here; `job` is the live handle created above.
    let assigned = unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) };
    if assigned == 0 {
        return Err(joined(io::Error::last_os_error()));
    }
    Ok(())
}

fn created(source: io::Error) -> OwnerDeathCleanupError {
    OwnerDeathCleanupError { stage: OwnerDeathCleanupStage::CreateContainer, source }
}

fn joined(source: io::Error) -> OwnerDeathCleanupError {
    OwnerDeathCleanupError { stage: OwnerDeathCleanupStage::JoinContainer, source }
}

fn is_access_denied(error: &OwnerDeathCleanupError) -> bool {
    let error = &error.source;
    const ERROR_ACCESS_DENIED: i32 = 5;
    error.raw_os_error() == Some(ERROR_ACCESS_DENIED)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Installing twice reports containment both times rather than building a
    /// second job, which a process cannot be in anyway.
    #[test]
    fn installing_is_idempotent() {
        let first = install_owner_death_cleanup().expect("install");
        let second = install_owner_death_cleanup().expect("install again");
        assert!(matches!(
            first,
            OwnerDeathCleanup::KillOnOwnerHandleClose | OwnerDeathCleanup::AlreadyContained
        ));
        assert_eq!(first, second, "a second install must not change the answer");
    }

    /// Access-denied is the shape Windows uses to say "already in a job", and
    /// it must not be mistaken for a failure to contain.
    #[test]
    fn access_denied_is_recognised() {
        assert!(is_access_denied(&joined(io::Error::from_raw_os_error(5))));
        assert!(!is_access_denied(&joined(io::Error::from_raw_os_error(2))));
        assert!(!is_access_denied(&joined(io::Error::other("no os code"))));
    }
}
