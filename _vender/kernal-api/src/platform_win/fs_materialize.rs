//! Windows mechanics for cache materialization: replacement, link and
//! reparse classification, the readonly attribute, path identity, USN change
//! markers, and volume facts.
//!
//! Every operation here is the host half of a neutral operation in
//! `crate::platform::fs`; the documentation that states the contract lives
//! there.

use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

use crate::platform::fs::{LinkKind, WriterWait};

#[path = "fs_extent.rs"]
mod extent;
pub use extent::extent_sharing;

#[path = "fs_replacement.rs"]
mod replacement;
pub use replacement::{
    atomic_replace, install_directory, is_lock_contention, is_transient_share_error,
    rename_generation, replace_with_delete_fallback,
};

#[path = "fs/write_seal.rs"]
mod write_seal;

/// Every share mode, so an observation never evicts or blocks a writer,
/// renamer, or deleter that already holds the file.
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// UTF-16, NUL-terminated spelling of `path`; `None` when the path itself
/// contains NUL and so cannot name a file.
fn wide_path(path: &Path) -> Option<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return None;
    }
    wide.push(0);
    Some(wide)
}

/// Open `path` for `access` with every share mode, or `None` when it
/// cannot be opened. The handle is closed when the returned guard drops.
fn open_shared(path: &Path, access: u32, flags: u32) -> Option<OwnedHandle> {
    let wide = wide_path(path)?;
    // SAFETY: `wide` is a live NUL-terminated UTF-16 path; the returned
    // handle is owned by the guard and closed exactly once.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    (handle != INVALID_HANDLE_VALUE).then_some(OwnedHandle(handle))
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the guard owns this valid handle and closes it once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

// ---------------------------------------------------------------------------
// Path-observed identity
// ---------------------------------------------------------------------------

/// Volume serial plus the native 128-bit file ID. ReFS does not guarantee
/// the legacy 64-bit index is unique, so `FILE_ID_INFO` is preferred and the
/// legacy index (zero-extended) is the fallback.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PathFileIdentity {
    volume_serial: u64,
    identifier: [u8; 16],
}

fn observe_identity(path: &Path) -> Option<PathFileIdentity> {
    // Metadata-only access; backup semantics admits directories.
    let handle = open_shared(path, 0, FILE_FLAG_BACKUP_SEMANTICS)?;
    // SAFETY: both structures are plain integer data, and the handle is live
    // for the duration of both queries.
    unsafe {
        let mut native: FILE_ID_INFO = std::mem::zeroed();
        if GetFileInformationByHandleEx(
            handle.0,
            FileIdInfo,
            (&raw mut native).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        ) != 0
        {
            return Some(native_identity(&native));
        }
        let mut legacy: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        (GetFileInformationByHandle(handle.0, &mut legacy) != 0)
            .then(|| legacy_identity(&legacy))
    }
}

fn native_identity(info: &FILE_ID_INFO) -> PathFileIdentity {
    PathFileIdentity {
        volume_serial: info.VolumeSerialNumber,
        identifier: info.FileId.Identifier,
    }
}

fn legacy_identity(info: &BY_HANDLE_FILE_INFORMATION) -> PathFileIdentity {
    let mut identifier = [0_u8; 16];
    identifier[..4].copy_from_slice(&info.nFileIndexLow.to_ne_bytes());
    identifier[4..8].copy_from_slice(&info.nFileIndexHigh.to_ne_bytes());
    PathFileIdentity {
        volume_serial: u64::from(info.dwVolumeSerialNumber),
        identifier,
    }
}

pub fn path_file_identity(path: &Path) -> io::Result<PathFileIdentity> {
    observe_identity(path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("cannot obtain file identity for {}", path.display()),
        )
    })
}

pub fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
    Ok(observe_identity(a)
        .zip(observe_identity(b))
        .is_some_and(|(a, b)| a == b))
}

/// The USN of the file's latest change-journal record (record versions
/// 2 and 3). `ChangeTime` is deliberately not a fallback: `SetFileTime` can
/// restore it along with the mtime and hide an ABA mutation.
pub fn file_change_marker(path: &Path) -> Option<i128> {
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::System::Ioctl::{FSCTL_READ_FILE_USN_DATA, READ_FILE_USN_DATA};
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let handle = open_shared(path, GENERIC_READ, FILE_FLAG_BACKUP_SEMANTICS)?;
    let query = READ_FILE_USN_DATA {
        MinMajorVersion: 2,
        MaxMajorVersion: 4,
    };
    let mut record = [0_u8; 512];
    let mut returned = 0_u32;
    // SAFETY: the query and output buffers are live and sized as passed; the
    // handle is valid for the call.
    let read = unsafe {
        DeviceIoControl(
            handle.0,
            FSCTL_READ_FILE_USN_DATA,
            (&raw const query).cast(),
            std::mem::size_of::<READ_FILE_USN_DATA>() as u32,
            record.as_mut_ptr().cast(),
            record.len() as u32,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if read == 0 {
        return None;
    }
    record
        .get(..returned as usize)
        .and_then(decode_usn)
        .map(i128::from)
}

fn decode_usn(record: &[u8]) -> Option<i64> {
    use windows_sys::Win32::System::Ioctl::{USN_RECORD_V2, USN_RECORD_V3};

    if record.len() < 8 {
        return None;
    }
    let major = u16::from_ne_bytes([record[4], record[5]]);
    // SAFETY: each arm checks the complete native structure size, and the
    // unaligned read copies the value without creating an aligned reference.
    unsafe {
        match major {
            2 if record.len() >= std::mem::size_of::<USN_RECORD_V2>() => {
                Some(std::ptr::read_unaligned(record.as_ptr().cast::<USN_RECORD_V2>()).Usn)
            }
            3 if record.len() >= std::mem::size_of::<USN_RECORD_V3>() => {
                Some(std::ptr::read_unaligned(record.as_ptr().cast::<USN_RECORD_V3>()).Usn)
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------

/// Writers are not observed here. A Windows child inherits only handles
/// marked inheritable, so a spawn cannot extend a write handle's lifetime the
/// way a POSIX fork does.
pub fn await_no_writers(path: &Path, _timeout: std::time::Duration) -> io::Result<WriterWait> {
    std::fs::metadata(path)?;
    Ok(WriterWait::Unobservable)
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

pub fn symlink_file(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

pub fn hard_link_count(path: &Path) -> io::Result<u64> {
    let wide = wide_path(path).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL")
    })?;
    // SAFETY: `wide` is a live NUL-terminated path, the information
    // structure is plain integer data, and the handle is closed once.
    unsafe {
        let handle = CreateFileW(
            wide.as_ptr(),
            0,
            SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        let got_info = GetFileInformationByHandle(handle, &mut info);
        // Capture this before CloseHandle can overwrite the thread error.
        let info_error = (got_info == 0).then(io::Error::last_os_error);
        let closed = CloseHandle(handle);
        if let Some(error) = info_error {
            return Err(error);
        }
        if closed == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(u64::from(info.nNumberOfLinks))
    }
}

/// Name-surrogate symbolic-link reparse points are `Symlink`; every other
/// reparse point (junction, mount point, cloud placeholder, ...) is
/// `Reparse`, including one whose tag cannot be read.
pub fn classify(path: &Path) -> io::Result<LinkKind> {
    use std::os::windows::fs::MetadataExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    /// Reparse tag of a name-surrogate symbolic link.
    const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        return Ok(LinkKind::Regular);
    }
    Ok(match reparse_tag(path) {
        Some(IO_REPARSE_TAG_SYMLINK) => LinkKind::Symlink,
        Some(_) | None => LinkKind::Reparse,
    })
}

/// The tag is the first `u32` of every reparse data buffer.
fn reparse_tag(path: &Path) -> Option<u32> {
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;
    use windows_sys::Win32::System::IO::DeviceIoControl;

    /// `MAXIMUM_REPARSE_DATA_BUFFER_SIZE`. A header-sized buffer makes the
    /// query fail with `ERROR_MORE_DATA` for any symlink with a real target
    /// name, which would misreport ordinary symlinks as opaque reparses.
    const MAXIMUM_REPARSE_DATA_BUFFER_SIZE: usize = 16 * 1024;

    let handle = open_shared(
        path,
        0,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )?;
    let mut buffer = vec![0_u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE];
    let mut returned = 0_u32;
    // SAFETY: the output buffer is live and sized as passed; the handle is
    // valid for the call.
    let read = unsafe {
        DeviceIoControl(
            handle.0,
            FSCTL_GET_REPARSE_POINT,
            std::ptr::null(),
            0,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    (read != 0 && returned >= 4)
        .then(|| u32::from_ne_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]))
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

pub fn metadata_mode(metadata: &std::fs::Metadata) -> u32 {
    u32::from(metadata.permissions().readonly())
}

/// Windows has no per-file executable bit.
pub fn metadata_executable(_metadata: &std::fs::Metadata) -> bool {
    false
}

pub fn apply_metadata_mode(path: &Path, mode: u32) -> io::Result<()> {
    set_readonly(path, mode != 0)
}

pub fn set_readonly(path: &Path, readonly: bool) -> io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    if permissions.readonly() == readonly {
        return Ok(());
    }
    permissions.set_readonly(readonly);
    std::fs::set_permissions(path, permissions)
}

/// Deny in-place writes for every trustee while keeping
/// `Permissions::readonly()` false and rename-over-replace working; the
/// contract lives on [`crate::platform::fs::deny_in_place_writes`].
///
/// A legacy seal that set the `READONLY` attribute is cleared first, so a
/// blob staged by an older build upgrades to the ACE seal the next time it
/// is sealed instead of staying unreadable to a `check_file_is_writeable`
/// caller.
pub fn deny_in_place_writes(path: &Path) -> io::Result<()> {
    set_readonly(path, false)?;
    write_seal::deny_in_place_writes(path)
}

/// Remove a seal this module applied; see
/// [`crate::platform::fs::allow_in_place_writes`]. Clears a legacy
/// `READONLY` attribute alongside the deny ACE, so either generation of
/// seal restores in-place writes.
pub fn allow_in_place_writes(path: &Path) -> io::Result<()> {
    write_seal::allow_in_place_writes(path)?;
    set_readonly(path, false)
}

/// Whether in-place writes are denied — by deny ACE **or** a legacy
/// `READONLY` attribute; see
/// [`crate::platform::fs::in_place_writes_denied`].
pub fn in_place_writes_denied(path: &Path) -> io::Result<bool> {
    if write_seal::in_place_writes_denied(path)? {
        return Ok(true);
    }
    Ok(std::fs::metadata(path)?.permissions().readonly())
}

/// Windows has no per-file executable bit.
pub fn make_owner_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Windows has no per-file executable bit.
pub fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Volume facts
// ---------------------------------------------------------------------------

pub fn volume_identity_u128(path: &Path) -> Option<u128> {
    let native = native_call_path(path).unwrap_or_else(|_| path.to_path_buf());
    observe_identity(&native).map(|identity| u128::from(identity.volume_serial))
}

pub const fn file_id_width() -> u32 {
    128
}

pub fn allocated_bytes(path: &Path, metadata: &std::fs::Metadata) -> u64 {
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;

    let native = native_call_path(path).unwrap_or_else(|_| path.to_path_buf());
    let Some(wide) = wide_path(&native) else {
        return metadata.len();
    };
    let mut high = 0_u32;
    // SAFETY: `wide` is a live NUL-terminated path and `high` a live output.
    unsafe {
        SetLastError(0);
        let low = GetCompressedFileSizeW(wide.as_ptr(), &mut high);
        allocated_size_result(low, high, GetLastError(), metadata.len())
    }
}

/// Combine the words `GetCompressedFileSizeW` reports. `u32::MAX` is a valid
/// low word when last-error stays zero; it signals failure only alongside a
/// nonzero error, and then the logical length is the answer.
fn allocated_size_result(low: u32, high: u32, error: u32, fallback: u64) -> u64 {
    if low == u32::MAX && error != 0 {
        fallback
    } else {
        (u64::from(high) << 32) | u64::from(low)
    }
}

// ---------------------------------------------------------------------------
// Paths and durability
// ---------------------------------------------------------------------------

/// A byte stream cannot carry native UTF-16 losslessly, so accept UTF-8 only.
pub fn path_from_raw_bytes(bytes: &[u8]) -> Option<PathBuf> {
    std::str::from_utf8(bytes).ok().map(PathBuf::from)
}

/// Canonicalize the existing parent and append the unchanged file name,
/// yielding the extended-length absolute form direct Win32 calls need to
/// escape `MAX_PATH`, without requiring the final file to exist.
pub fn native_call_path(path: &Path) -> io::Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path has no filename: {}", path.display()),
        )
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(std::fs::canonicalize(parent)?.join(name))
}

/// Windows exposes no directory handle to flush; `MOVEFILE_WRITE_THROUGH`
/// covers the rename side. Deliberately does not resolve `directory`.
pub fn sync_directory_if_supported(_directory: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_fallback_preserves_both_words_and_zero_extends_identifier() {
        // SAFETY: this Win32 metadata structure contains integer fields only.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        info.dwVolumeSerialNumber = u32::MAX;
        info.nFileIndexLow = 0x1234_5678;
        info.nFileIndexHigh = 0x9abc_def0;
        let identity = legacy_identity(&info);
        assert_eq!(identity.volume_serial, u64::from(u32::MAX));
        assert_eq!(&identity.identifier[..4], &0x1234_5678_u32.to_ne_bytes());
        assert_eq!(&identity.identifier[4..8], &0x9abc_def0_u32.to_ne_bytes());
        assert_eq!(&identity.identifier[8..], &[0_u8; 8]);
    }

    #[test]
    fn upper_identifier_and_volume_bits_participate_in_identity() {
        // SAFETY: FILE_ID_INFO consists of integer fields and a byte array.
        let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
        info.VolumeSerialNumber = 1_u64 << 48;
        info.FileId.Identifier[15] = 1;
        let first = native_identity(&info);
        assert_eq!(first.volume_serial, 1_u64 << 48);
        info.FileId.Identifier[15] = 2;
        assert_ne!(first, native_identity(&info));
        info.FileId.Identifier[15] = 1;
        info.VolumeSerialNumber = 0;
        assert_ne!(first, native_identity(&info));
    }

    #[test]
    fn decodes_supported_usn_versions_without_alignment_assumptions() {
        use windows_sys::Win32::System::Ioctl::{USN_RECORD_V2, USN_RECORD_V3};

        for (version, size, offset) in [
            (
                2_u16,
                std::mem::size_of::<USN_RECORD_V2>(),
                std::mem::offset_of!(USN_RECORD_V2, Usn),
            ),
            (
                3_u16,
                std::mem::size_of::<USN_RECORD_V3>(),
                std::mem::offset_of!(USN_RECORD_V3, Usn),
            ),
        ] {
            let mut bytes = vec![0_u8; size + 1];
            let record = &mut bytes[1..];
            record[4..6].copy_from_slice(&version.to_ne_bytes());
            record[offset..offset + 8].copy_from_slice(&1234_i64.to_ne_bytes());
            assert_eq!(decode_usn(record), Some(1234));
            assert_eq!(decode_usn(&record[..7]), None);
            assert_eq!(decode_usn(&record[..size - 1]), None);
            record[4..6].copy_from_slice(&4_u16.to_ne_bytes());
            assert_eq!(decode_usn(record), None);
        }
    }

    #[test]
    fn allocated_size_combines_high_and_low_words_and_falls_back() {
        assert_eq!(allocated_size_result(7, 1, 0, 99), (1_u64 << 32) | 7);
        assert_eq!(allocated_size_result(u32::MAX, 0, 0, 99), u64::from(u32::MAX));
        assert_eq!(allocated_size_result(u32::MAX, 0, 5, 99), 99);
    }
}
