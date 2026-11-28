//! What Linux says about running out of things.

use std::io;
use std::path::Path;

use crate::platform::resources::InodeCapacity;

/// Whether this error means the process or the system is out of descriptors.
///
/// `EMFILE` is this process's limit and `ENFILE` the system's. A caller sheds
/// load for either, so both answer the same question.
pub fn signals_fd_exhaustion(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE))
}

/// Whether this error means the filesystem is out of space.
///
/// `EDQUOT` is a quota rather than a full disk, but the caller can do nothing
/// different about it: the write will not succeed until something is freed.
pub fn signals_storage_exhaustion(error: &io::Error) -> bool {
    if matches!(error.kind(), io::ErrorKind::StorageFull) {
        return true;
    }
    matches!(error.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT))
}

/// One error this host would report for descriptor exhaustion.
pub fn fd_exhaustion_error() -> io::Error {
    io::Error::from_raw_os_error(libc::EMFILE)
}

/// One error this host would report for storage exhaustion.
pub fn storage_exhaustion_error() -> io::Error {
    io::Error::from_raw_os_error(libc::ENOSPC)
}

/// Probe inode capacity for the filesystem containing `path`.
///
/// Inode exhaustion matters here in a way it does not on Windows: a filesystem
/// with a fixed inode table (ext4 most prominently) can fail writes with
/// `ENOSPC` while plenty of bytes remain free. A filesystem that reports an
/// empty table -- btrfs, and others that allocate inodes dynamically -- has
/// nothing to run out of, and is reported as not applicable rather than as
/// zero of zero.
pub fn inode_capacity(path: &Path) -> io::Result<Option<InodeCapacity>> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    // SAFETY: an all-zero `statvfs` is a valid one; the call fills it.
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a NUL-terminated path alive for the call, and
    // `stats` is valid writable storage of exactly the expected type.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if stats.f_files == 0 {
        return Ok(None);
    }
    // fsfilcnt_t is u64 on Linux but u32 on macOS; keep explicit casts.
    #[allow(clippy::unnecessary_cast)]
    Ok(Some(InodeCapacity {
        total: stats.f_files as u64,
        free: stats.f_favail as u64,
    }))
}

/// Total capacity, in bytes, of the filesystem containing `path`.
pub fn total_space(path: &Path) -> io::Result<u64> {
    let stats = probe_statvfs(path)?;
    // fsblkcnt_t and the block-size fields are u64 on Linux but narrower on
    // macOS; keep explicit casts so both hosts share this body.
    #[allow(clippy::unnecessary_cast)]
    Ok(stats.f_blocks as u64 * stats.f_frsize as u64)
}

/// Space available to this (unprivileged) caller, in bytes, on the
/// filesystem containing `path`.
///
/// This is `f_bavail`, not `f_bfree`: the two differ by the blocks a host
/// reserves for its superuser, and a caller sizing a write cares about what
/// it can actually use, not what is free in the abstract.
pub fn available_space(path: &Path) -> io::Result<u64> {
    let stats = probe_statvfs(path)?;
    #[allow(clippy::unnecessary_cast)]
    Ok(stats.f_bavail as u64 * stats.f_frsize as u64)
}

/// `statvfs` the filesystem containing `path`, for [`total_space`] and
/// [`available_space`].
fn probe_statvfs(path: &Path) -> io::Result<libc::statvfs> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    // SAFETY: an all-zero `statvfs` is a valid one; the call fills it.
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a NUL-terminated path alive for the call, and
    // `stats` is valid writable storage of exactly the expected type.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stats)
}
