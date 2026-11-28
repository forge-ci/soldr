//! Owner-death containment for this process (macos).


use crate::platform::process::{LifetimeEnforcement, OwnerDeathCleanup, OwnerDeathCleanupError};

/// macOS has no parent-death signal and no job objects.
///
/// Reporting that plainly is the useful answer: the caller must spawn a
/// supervisor that watches the owner and does the reaping itself. Returning
/// `Unsupported` here would say "nothing can be done", which is false and
/// would lose the containment the supervisor provides.
pub fn install_owner_death_cleanup() -> Result<OwnerDeathCleanup, OwnerDeathCleanupError> {
    Ok(OwnerDeathCleanup::SupervisorRequired)
}

/// What this host will attempt, without attempting it.
pub fn owner_death_cleanup_target() -> OwnerDeathCleanup {
    OwnerDeathCleanup::SupervisorRequired
}

/// What binding a child to *this* process's lifetime obtains here.
///
/// The spawn path forks a kqueue supervisor before `exec` and waits for its
/// owner and child watches to be registered before reporting spawn success,
/// so the containment is real -- but it is a user-space process doing the
/// reaping, and nothing reaps if that supervisor is killed too. This host has
/// no parent-death signal and no job objects, and saying so is the useful
/// answer: a caller needing a guarantee that survives losing every user-space
/// participant has to refuse here rather than assume.
pub fn spawner_lifetime_enforcement() -> LifetimeEnforcement {
    LifetimeEnforcement::Watcher
}
