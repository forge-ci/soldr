//! Mutual exclusion between staged-file writes and child spawns
//! (soldr#3098; mirrors zackees/zccache#1562's `spawn_exclusion`).
//!
//! # The race
//!
//! soldr restores files by writing a unique sibling temporary and renaming
//! it over the destination (`load_extract::extract_one`, #1909). The write
//! holds a write descriptor on the temporary's inode. The same process
//! spawns children throughout a build -- cargo, the broker, the gc sweeper,
//! installers -- and on POSIX every `fork` duplicates the parent's whole
//! descriptor table: a child forked while that write descriptor is open
//! inherits it and keeps it until its own `execve` closes it (`O_CLOEXEC`
//! closes on exec, not on fork).
//!
//! `rename(2)` does not change the inode. After the writer closes its own
//! descriptor and publishes the path, the inherited copy is still a
//! *writable descriptor on the published inode*. `execve` evaluates
//! `ETXTBSY` against the inode, so cargo running the restored build script
//! fails with `Text file busy` for exactly the child's fork-to-exec window.
//! Inspecting the writer's own `/proc/self/fd` cannot see the descriptor:
//! it lives in the child's table.
//!
//! # The fix
//!
//! A process-wide `RwLock<()>`. Every child spawn holds the shared guard for
//! the duration of the spawn call (the parent returns from `spawn` only
//! after the child has exec'd or failed, so the shared guard brackets the
//! whole fork-to-exec window). A staged write holds the exclusive guard from
//! before the temporary is opened for writing until after it is closed, so
//! no child can be forked while a staged write descriptor is open, and no
//! such descriptor can be opened while a child is between fork and exec.
//! Spawns still overlap each other; only a spawn and an in-flight write
//! serialize, for the microseconds a write takes.
//!
//! Critical sections must contain no `.await` and nothing that can re-enter
//! the lock (a spawn inside a staged write would deadlock). The lock lives
//! in this dependency-leaf crate so the platform spawn primitives can take
//! it; `soldr_core::spawn_exclusion` re-exports it for everything above.

use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

// Raw `std` lock on purpose (soldr#3836): the exclusive side is held across a
// whole staged-file write, which for a large restored entry on a slow disk can
// outlast the `sync` critical-section watchdog and must not abort the process.
#[cfg_attr(dylint_lib = "ban_raw_sync_primitive", allow(ban_raw_sync_primitive))]
static SPAWN_WRITE_LOCK: RwLock<()> = RwLock::new(());

/// Shared guard for one child spawn. Hold it across the spawn call only --
/// never across `wait()`.
#[cfg_attr(dylint_lib = "ban_raw_sync_primitive", allow(ban_raw_sync_primitive))]
pub fn spawn_shared() -> RwLockReadGuard<'static, ()> {
    SPAWN_WRITE_LOCK
        .read()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Exclusive guard for one staged-file write. Hold it from before the file
/// is opened for writing until after the descriptor is closed; the rename
/// that publishes it may happen outside the guard (the inode is only busy
/// while a write descriptor exists).
#[cfg_attr(dylint_lib = "ban_raw_sync_primitive", allow(ban_raw_sync_primitive))]
pub fn write_exclusive() -> RwLockWriteGuard<'static, ()> {
    SPAWN_WRITE_LOCK
        .write()
        .unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawns_share_and_writes_exclude() {
        let a = spawn_shared();
        let b = spawn_shared();
        assert!(
            SPAWN_WRITE_LOCK.try_write().is_err(),
            "readers block a writer"
        );
        drop((a, b));
        let w = write_exclusive();
        assert!(
            SPAWN_WRITE_LOCK.try_read().is_err(),
            "a writer blocks readers"
        );
        drop(w);
        drop(spawn_shared());
    }
}
