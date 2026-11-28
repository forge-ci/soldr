//! What Windows says about running out of things.

use std::io;
use std::path::Path;

use crate::platform::resources::InodeCapacity;

/// WSAEMFILE -- a socket call that could not get a descriptor.
const WSAEMFILE: i32 = 10024;
/// ERROR_TOO_MANY_OPEN_FILES.
const ERROR_TOO_MANY_OPEN_FILES: i32 = 4;
/// ERROR_NO_SYSTEM_RESOURCES -- the system-wide form of the same wall.
const ERROR_NO_SYSTEM_RESOURCES: i32 = 1450;

/// ERROR_HANDLE_DISK_FULL.
const ERROR_HANDLE_DISK_FULL: i32 = 39;
/// ERROR_DISK_FULL.
const ERROR_DISK_FULL: i32 = 112;

/// Whether this error means the process or the system is out of descriptors.
pub fn signals_fd_exhaustion(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(WSAEMFILE | ERROR_TOO_MANY_OPEN_FILES | ERROR_NO_SYSTEM_RESOURCES)
    )
}

/// Whether this error means the filesystem is out of space.
pub fn signals_storage_exhaustion(error: &io::Error) -> bool {
    if matches!(error.kind(), io::ErrorKind::StorageFull) {
        return true;
    }
    matches!(
        error.raw_os_error(),
        Some(ERROR_HANDLE_DISK_FULL | ERROR_DISK_FULL)
    )
}

/// One error this host would report for descriptor exhaustion.
pub fn fd_exhaustion_error() -> io::Error {
    io::Error::from_raw_os_error(WSAEMFILE)
}

/// One error this host would report for storage exhaustion.
pub fn storage_exhaustion_error() -> io::Error {
    io::Error::from_raw_os_error(ERROR_DISK_FULL)
}

/// Windows filesystems have no fixed inode table, so there is nothing to
/// report -- and reporting invented numbers would be worse than saying so.
pub fn inode_capacity(path: &Path) -> io::Result<Option<InodeCapacity>> {
    let _ = path;
    Ok(None)
}

/// Total capacity, in bytes, of the volume containing `path`.
pub fn total_space(path: &Path) -> io::Result<u64> {
    Ok(disk_free_space(path)?.1)
}

/// Space available to this (unprivileged, possibly quota-limited) caller, in
/// bytes, on the volume containing `path`.
///
/// This is `lpFreeBytesAvailableToCaller`, not `lpTotalNumberOfFreeBytes`: the
/// two differ under a per-user disk quota, and a caller sizing a write cares
/// about what it can actually use, not what is free on the volume overall.
pub fn available_space(path: &Path) -> io::Result<u64> {
    Ok(disk_free_space(path)?.0)
}

/// `GetDiskFreeSpaceExW` for the volume containing `path`, returning
/// `(available_to_caller, total)` in bytes.
fn disk_free_space(path: &Path) -> io::Result<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut available_to_caller: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;
    // SAFETY: `wide` is a NUL-terminated UTF-16 path alive for the call, and
    // the three out-pointers reference valid, appropriately sized storage.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available_to_caller,
            &mut total,
            &mut total_free,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok((available_to_caller, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not applicable is the answer, and it must stay an answer rather than
    /// becoming an error or an invented zero-of-zero: a caller presenting
    /// inode pressure has to be able to tell "this host does not have that"
    /// from "the probe failed".
    #[test]
    fn inode_usage_is_not_applicable_on_windows() {
        let probed =
            inode_capacity(&std::env::temp_dir()).expect("the probe never fails on Windows");
        assert_eq!(probed, None);
    }
}
