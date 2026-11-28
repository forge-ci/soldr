//! Windows per-user directory placement for product runtime artifacts.

use std::fs::File;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

#[cfg(test)]
#[path = "fs_acl_unsupported_tests.rs"]
mod acl_unsupported_tests;

/// This host does not yet implement held-file ACL inspection.
pub fn ensure_no_extended_acl(_file: &std::fs::File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "held-file ACL verification is unavailable on this host",
    ))
}


#[path = "fs/private_directory.rs"]
mod private_directory;

pub use private_directory::{create_dir_all_private, ensure_dir_private};

/// Open an append-only file without truncating the bytes already in it.
///
/// "Shared" is the Windows half of the contract: the default share mode on
/// this host is exclusive, so a second appender -- or an operator's `type` --
/// would be refused while the first handle lives. The Linux and macOS trees
/// need no equivalent, because a POSIX open never claims that exclusion.
pub fn open_shared_append(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use winapi::um::winnt::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
}

/// Directory for `product`'s ephemeral runtime artifacts (pid files, run data).
///
/// `LOCALAPPDATA` is per-user and non-roaming, which is what machine-local
/// artifacts want: a roaming profile would carry another machine's state here.
/// Sockets are not placed by this — Windows named pipes live in a kernel
/// namespace with no directory at all.
pub fn user_runtime_dir(product: &str) -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join(product)
}

/// Directory for `product`'s persistent state (databases that outlive a boot).
///
/// Windows draws no line between runtime and state locations; both are
/// per-user under `LOCALAPPDATA`.
pub fn user_state_dir(product: &str) -> PathBuf {
    user_runtime_dir(product)
}

/// Root under which `product` keeps per-run scratch data.
pub fn user_run_data_root(product: &str) -> PathBuf {
    user_runtime_dir(product)
}

/// Stable identity of an open file on this host.
///
/// Two paths that resolve to the same bytes on disk report the same identity,
/// which is what lets a caller notice that the file it opened has since been
/// replaced. The two fields are whatever this host uses to say that: a device
/// and inode, a volume serial and file index, or an equivalent pair. Callers
/// compare them; they do not interpret them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    /// Device, volume, or platform-equivalent file namespace.
    pub device: u64,
    /// Inode, file index, or platform-equivalent file number.
    pub file: u64,
}

/// Identity of an already-open file.
pub fn file_identity(file: &File) -> io::Result<Option<FileIdentity>> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use winapi::um::fileapi::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
    use winapi::um::winnt::HANDLE;

    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    let result =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, info.as_mut_ptr()) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }

    let info = unsafe { info.assume_init() };
    Ok(Some(FileIdentity {
        device: info.dwVolumeSerialNumber as u64,
        file: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    }))
}

/// Identity of the file a path currently names.
///
/// Windows answers this from an open handle, so the file is opened here. The
/// share mode is permissive on purpose: asking who a file is must not evict a
/// writer that already holds it.
pub fn path_identity(path: &Path) -> io::Result<Option<FileIdentity>> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use winapi::um::winnt::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)?;
    file_identity(&file)
}

/// Open `path` for use as an advisory lock file, creating it if absent.
///
/// The share mode is permissive on purpose: exclusion must come from the lock,
/// not from the open, or a second opener fails before it can even ask.
pub fn open_lock_file(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use winapi::um::winnt::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Never truncate: an existing lock file may be held right now,
        // and its contents are not ours to clear.
        .truncate(false)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
}

/// The offset of the one byte every lock in this module is taken on.
///
/// A `LockFileEx` range is *mandatory*: the kernel denies `ReadFile` and
/// `WriteFile` from every other handle inside it, not just competing lock
/// requests. Locking the whole file would therefore make this module's
/// advisory promise false on this host alone -- a second process that opened
/// the lock file only to read who holds it would fail with
/// `ERROR_LOCK_VIOLATION`, and a shared holder would block every
/// non-participating writer. Confining the lock to a single byte far outside
/// any real file's data keeps the exclusion between lock holders exactly as
/// it was and leaves the file body readable and writable, which is what the
/// facade means by "advisory" and the same trick SQLite uses for its
/// cross-platform locking.
///
/// `1 << 62` is the offset because it is past anything a filesystem can hold
/// while still positive when the kernel reads it as the signed
/// `LARGE_INTEGER` that `NtLockFile` takes underneath `LockFileEx`. Locking a
/// range beyond end-of-file is legal and does not extend the file.
const LOCK_BYTE_OFFSET: u64 = 1 << 62;

/// Fill the `OVERLAPPED` offset pair that names [`LOCK_BYTE_OFFSET`].
///
/// Both `LockFileEx` and `UnlockFileEx` take the range's start this way and
/// its length in the two `DWORD` arguments, so the pair has to agree between
/// them or a release silently fails with `ERROR_NOT_LOCKED`.
fn lock_byte_overlapped() -> winapi::um::minwinbase::OVERLAPPED {
    use std::mem;
    use winapi::um::minwinbase::OVERLAPPED;

    let mut overlapped: OVERLAPPED = unsafe { mem::zeroed() };
    let offsets = unsafe { overlapped.u.s_mut() };
    offsets.Offset = LOCK_BYTE_OFFSET as u32;
    offsets.OffsetHigh = (LOCK_BYTE_OFFSET >> 32) as u32;
    overlapped
}

/// Take or wait for a lock on [`LOCK_BYTE_OFFSET`], with the given
/// `LockFileEx` flags. Private: every public lock entry point below is one
/// flag combination of this call.
fn lock_file(file: &File, flags: winapi::shared::minwindef::DWORD) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use winapi::um::fileapi::LockFileEx;
    use winapi::um::winnt::HANDLE;

    let mut overlapped = lock_byte_overlapped();
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle() as HANDLE,
            flags,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Take an exclusive advisory lock without waiting.
///
/// This host refuses an exclusive request that overlaps a range the *same*
/// handle already locked, so a caller that still holds a shared lock on this
/// file gets a conflict here rather than an upgrade. That is the one place
/// the hosts genuinely differ, and the facade documents it as a rule callers
/// keep: one lock guard per open file.
pub fn try_lock_exclusive(file: &File) -> io::Result<()> {
    use winapi::um::minwinbase::{LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY};

    lock_file(file, LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY)
}

/// Take an exclusive advisory lock, waiting until it is available.
///
/// The waiting form of the same refusal [`try_lock_exclusive`] describes:
/// asked to upgrade a lock the same handle already holds, this waits for a
/// release that only this caller could perform, and so never returns.
pub fn lock_exclusive(file: &File) -> io::Result<()> {
    use winapi::um::minwinbase::LOCKFILE_EXCLUSIVE_LOCK;

    lock_file(file, LOCKFILE_EXCLUSIVE_LOCK)
}

/// Take a shared advisory lock, waiting until it is available.
///
/// A `LockFileEx` call without `LOCKFILE_EXCLUSIVE_LOCK` set is what this
/// host spells "shared" -- there is no separate shared-lock flag.
pub fn lock_shared(file: &File) -> io::Result<()> {
    lock_file(file, 0)
}

/// Take a shared advisory lock without waiting.
///
/// Returns immediately when an exclusive holder has it; the caller decides
/// whether that is a conflict worth retrying, via [`is_lock_conflict`].
pub fn try_lock_shared(file: &File) -> io::Result<()> {
    use winapi::um::minwinbase::LOCKFILE_FAIL_IMMEDIATELY;

    lock_file(file, LOCKFILE_FAIL_IMMEDIATELY)
}

/// Release a lock taken by [`try_lock_exclusive`], [`try_lock_shared`],
/// [`lock_exclusive`], or [`lock_shared`].
///
/// The range must be the one `lock_file` took, byte for byte: this host
/// releases a named range, not "whatever this handle holds", and a mismatched
/// range fails with `ERROR_NOT_LOCKED` while the lock stays held.
pub fn unlock(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use winapi::um::fileapi::UnlockFileEx;
    use winapi::um::winnt::HANDLE;

    let mut overlapped = lock_byte_overlapped();
    let result = unsafe { UnlockFileEx(file.as_raw_handle() as HANDLE, 0, 1, 0, &mut overlapped) };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Whether `error` means "someone else holds it", rather than a real failure.
pub fn is_lock_conflict(error: &io::Error) -> bool {
    use winapi::shared::winerror::ERROR_LOCK_VIOLATION;

    error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32)
}

/// Set the modification time of the file at `path`, without disturbing its
/// access or creation time.
///
/// Opened with only `FILE_WRITE_ATTRIBUTES`, not the read/write access a
/// plain open would request: Windows denies an attribute-only change to a
/// handle opened for full write access to a file this process has marked
/// read-only, but grants it to a handle that only asked for attributes --
/// and content-addressed cache entries are commonly left read-only. From
/// there this is the same stable `File::set_modified` every caller of this
/// crate could use directly; the access mode is the only part that needed
/// deciding per host.
pub fn set_file_mtime(
    path: &Path,
    seconds_since_unix_epoch: i64,
    nanoseconds: u32,
) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use winapi::um::winbase::FILE_FLAG_BACKUP_SEMANTICS;
    use winapi::um::winnt::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
    };

    let time = unix_time_to_system_time(seconds_since_unix_epoch, nanoseconds)?;
    // `FILE_FLAG_BACKUP_SEMANTICS` is what makes `CreateFileW` willing to
    // return a handle to a directory; without it this call is
    // `ERROR_ACCESS_DENIED` on every directory, while the Unix hosts stamp
    // one happily. The flag does not change how a regular file is opened.
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    file.set_modified(time)
}

/// Convert a Unix-epoch second/nanosecond pair to [`std::time::SystemTime`],
/// reporting out-of-range input rather than silently wrapping it.
fn unix_time_to_system_time(
    seconds_since_unix_epoch: i64,
    nanoseconds: u32,
) -> io::Result<std::time::SystemTime> {
    use std::time::{Duration, SystemTime};

    let out_of_range = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "modification time is out of range for this host's clock",
        )
    };

    if seconds_since_unix_epoch >= 0 {
        SystemTime::UNIX_EPOCH
            .checked_add(Duration::new(seconds_since_unix_epoch as u64, nanoseconds))
            .ok_or_else(out_of_range)
    } else {
        let magnitude = seconds_since_unix_epoch
            .checked_neg()
            .ok_or_else(out_of_range)? as u64;
        SystemTime::UNIX_EPOCH
            .checked_sub(Duration::new(magnitude, 0))
            .and_then(|time| time.checked_add(Duration::new(0, nanoseconds)))
            .ok_or_else(out_of_range)
    }
}

/// Encode `path` as the bytes this host uses to spell it.
///
/// Faithful, not canonical: this is the encoding a path is carried in so the
/// other end can reconstruct exactly the path that was named. It is
/// deliberately not `ipc::endpoint_scope_bytes`, which folds away differences
/// a host considers meaningless in order to hash two spellings to one identity.
/// Round-tripping through the pair here must return the original path;
/// round-tripping through that one need not.
pub fn encode_path_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt as _;

    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// Reconstruct a path from [`encode_path_bytes`] output produced on this host.
///
/// Windows paths are UTF-16, so an odd byte count cannot have come from this
/// encoder and is rejected rather than silently truncated.
pub fn decode_path_bytes(bytes: &[u8]) -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;

    if !bytes.len().is_multiple_of(2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows path bytes must be little-endian UTF-16",
        ));
    }
    let wide = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| u16::from_le_bytes(*chunk))
        .collect::<Vec<_>>();
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&wide)))
}

/// Directory for `product`'s shared application data.
///
/// Distinct from [`user_state_dir`]: state is this machine's private
/// bookkeeping, while this is the data a user expects to follow their account.
/// That is exactly the roaming/local split, so this uses the roaming root
/// while the state and runtime roles use the local one.
pub fn user_data_dir(product: &str) -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join(product)
}

/// The directory this host keeps a product's *configuration* in.
///
/// Distinct from [`user_data_dir`] on this host: Windows separates roaming
/// configuration from local application data, and a setting a user expects to
/// follow them between machines belongs in the former.
pub fn user_config_dir(product: &str) -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join(product)
}

/// Move `tmp` onto `target`, replacing it, without a window where neither is
/// readable.
///
/// `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` gives that guarantee here,
/// and gives it whether or not `target` already exists, so this is one call
/// rather than a create branch and a replace branch. There is nothing for a
/// prior `target.exists()` test to decide -- and a test like that could only
/// be answering about a moment that has already passed.
///
/// `MOVEFILE_WRITE_THROUGH` is the durability half: the call does not return
/// until the move has reached the disk, which is what lets [`sync_directory`]
/// have nothing left to do -- on *both* paths, where `ReplaceFileW` plus a
/// plain rename only covered the one where the target already existed.
///
/// `MOVEFILE_COPY_ALLOWED` is deliberately absent. A cross-volume move is a
/// copy and a delete, which is neither atomic nor what a caller committing a
/// file asked for; refusing it is the same answer Unix gives with `EXDEV`.
///
/// What `ReplaceFileW` did and this does not is carry the *target's* ACL,
/// attributes, and creation time onto the replacement. A move keeps the
/// replacement's own, which is what a Unix rename does, and what
/// [`create_private_file`] already tells callers to expect on this host:
/// protection comes from the directory the file was created in.
pub fn replace_file(tmp: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let tmp_w = wide(tmp);
    let target_w = wide(target);
    let ok = unsafe {
        MoveFileExW(
            tmp_w.as_ptr(),
            target_w.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Make a directory entry created by [`replace_file`] durable.
///
/// The flush already happened, on whichever path the caller took:
/// [`replace_file`] passes `MOVEFILE_WRITE_THROUGH`, which does not return
/// until the move is on the disk, and Windows exposes no directory handle to
/// flush separately. So the entry is durable before a caller gets here.
///
/// The directory is still resolved rather than ignored. Every host owes the
/// same answer to "was this a directory I could have flushed?", and returning
/// `Ok(())` for a path that does not exist would make this host's answer
/// differ from the Unix `File::open` that reports it.
pub fn sync_directory(directory: &Path) -> io::Result<()> {
    std::fs::metadata(directory)?;
    Ok(())
}

/// Create a new file that only its owner can read, failing if it exists.
///
/// `create_new` is part of the contract, not a convenience: a private file
/// opened over one that already exists inherits whatever that one allowed.
///
/// Windows carries no mode bits here. The file inherits its directory's ACL,
/// then records the current token user as its owner. That second step matters
/// for elevated tokens, whose default owner can be the Administrators group:
/// `OW` would otherwise grant every group member the owner-rights ACE. If
/// ownership cannot be set, the exclusively-created file is marked for
/// handle-bound deletion and the operation fails closed. Callers must still
/// create the file in a verified private directory: assigning an owner alone
/// does not remove access inherited from a permissive parent.
pub fn create_private_file(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use winapi::um::winnt::{DELETE, GENERIC_WRITE, WRITE_DAC, WRITE_OWNER};

    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        // Ownership is set on this exact handle below. DELETE makes cleanup
        // handle-bound too, so a rename cannot redirect it to another file.
        // `access_mode` replaces, rather than augments, `.write(true)` on
        // this host, so retain generic data-write access explicitly.
        .access_mode(GENERIC_WRITE | WRITE_OWNER | WRITE_DAC | DELETE)
        .open(path)?;
    if let Err(error) = super::ipc_private_dir::apply_current_user_owner(&file) {
        let _ = delete_file_on_close(&file);
        return Err(error);
    }
    if let Err(error) = super::ipc_private_dir::apply_current_user_private_file_dacl(&file) {
        let _ = delete_file_on_close(&file);
        return Err(error);
    }
    Ok(file)
}

/// Mark the already-open file for deletion, never resolving a pathname.
fn delete_file_on_close(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
    };

    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: `file` owns a live handle opened with DELETE access and
    // `disposition` is the exact buffer FileDispositionInfo requires.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as _,
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Secure bounded read rooted in a protected owner-private directory.
///
/// Windows permissions are ACLs rather than mode bits. The existing
/// owner-private-directory verifier checks the trusted parent for the
/// protected owner-and-SYSTEM DACL. Independently, the opened file must be
/// owned by the current user and have exactly the private owner-rights-and-
/// SYSTEM full-control DACL, either direct or inherited. We open the final
/// component's reparse point itself, reject it, and compare the opened handle
/// identity to that final path. Ancestor paths remain caller-trusted; this is
/// not a filesystem sandbox.
pub fn read_private_regular_file_bounded(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use winapi::um::winbase::FILE_FLAG_OPEN_REPARSE_POINT;
    use winapi::um::winnt::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "private file path has no parent")
    })?;
    if !super::ipc_private_dir::owner_private_directory(parent)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private file parent does not have the protected owner-private DACL",
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle as _;
    use winapi::um::fileapi::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
    use winapi::um::winnt::{FILE_ATTRIBUTE_REPARSE_POINT, HANDLE};
    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: the file handle remains live for this call and `information` is
    // an initialized writable out-buffer of the exact native type.
    if unsafe {
        GetFileInformationByHandle(file.as_raw_handle() as HANDLE, information.as_mut_ptr())
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the API above returned success and initialized every field.
    let information = unsafe { information.assume_init() };
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private input is not a regular non-reparse file",
        ));
    }
    if !super::ipc_private_dir::opened_file_is_current_user_private(&file)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private input is not current-user owner/SYSTEM DACL private",
        ));
    }
    let bound_plus_one = max_bytes.checked_add(1).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "private file limit overflows")
    })?;
    if metadata.len() > max_bytes as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "private input exceeds limit"));
    }
    let mut bytes = Vec::with_capacity(bound_plus_one);
    (&mut &file)
        .take(bound_plus_one as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "private input exceeds limit"));
    }
    if path_identity(path)? != file_identity(&file)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private input path changed while it was read",
        ));
    }
    Ok(bytes)
}

/// Bounded ordinary regular-file observation for the public context facade.
pub fn read_context_regular_file_bounded(
    path: &Path,
    max_bytes: usize,
) -> io::Result<crate::platform::fs::ContextFileObservation> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use winapi::um::winbase::FILE_FLAG_OPEN_REPARSE_POINT;
    use winapi::um::winnt::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    let bound_plus_one = max_bytes.checked_add(1).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "context file limit overflows")
    })?;
    // Reject currently-special final components before opening them. The open
    // below still verifies the handle, so this precheck is not advertised as a
    // race-free authorization decision.
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "context input is not a regular file",
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let before = file.metadata()?;
    use std::os::windows::fs::MetadataExt as _;
    if before.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !before.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "context input is not a regular file",
        ));
    }
    let identity = file_identity(&file)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::Unsupported, "context file identity is unavailable")
    })?;
    let before_modified = before.modified()?;
    if before.len() > max_bytes as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "context input exceeds limit",
        ));
    }
    let mut bytes = Vec::with_capacity(bound_plus_one);
    (&mut &file)
        .take(bound_plus_one as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "context input exceeds limit",
        ));
    }
    let after = file.metadata()?;
    if after.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "context input final component is a reparse point",
        ));
    }
    let after_identity = file_identity(&file)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::Unsupported, "context file identity is unavailable")
    })?;
    if after_identity != identity || after.len() != before.len() || after.modified()? != before_modified || bytes.len() as u64 != after.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "context input changed while it was read",
        ));
    }
    let path_file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let path_metadata = path_file.metadata()?;
    if path_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !path_metadata.is_file()
        || file_identity(&path_file)? != Some(identity)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "context input path changed while it was read",
        ));
    }
    Ok(crate::platform::fs::ContextFileObservation {
        bytes,
        metadata: crate::platform::fs::context_regular_file_metadata(&after, identity)?,
    })
}

#[cfg(test)]
mod identity_tests {
    #[test]
    fn running_executable_identity_needs_no_write_access() {
        let path = std::env::current_exe().expect("running test executable");
        let file = std::fs::File::open(&path).expect("read-only executable handle");
        assert_eq!(
            super::path_identity(&path).expect("identity while executable is running"),
            super::file_identity(&file).expect("opened executable identity")
        );
    }
}
