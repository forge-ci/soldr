//! Runtime-artifact identity, permissions, secure-open, replacement, and
//! directory primitives, plus advisory locking, modification-time, reflink
//! copy, parallel directory walking, and glob matching.
//!
//! Callers name the *role* a directory plays for their product -- ephemeral
//! runtime artifacts, persistent state, per-run scratch. Which location on this
//! host plays that role, and how two accounts are kept apart there, is decided
//! here. Callers still own their own layout beneath it: leaf names, extensions,
//! and subdirectories are product conventions, not host mechanics.
//!
//! The rest of this module is one capability, not several, even though it
//! groups locking, timestamps, copying, walking, and matching: every one of
//! them is a place the supported hosts genuinely diverge (advisory-lock
//! semantics, reflink support, filesystem-specific mtime granularity) and a
//! wrapper crate exists only to paper over that divergence. Locking and
//! modification-time setting are implemented natively per host, matching the
//! rest of this file; reflink copy, parallel walking, and glob matching wrap
//! a maintained backend privately, because there is no std equivalent and a
//! hand-rolled reflink ioctl is not something to get wrong silently.

#[cfg(feature = "fs")]
mod temporary;
#[cfg(feature = "fs")]
pub use temporary::{TemporaryDirectory, MAX_TEMP_PREFIX_BYTES};

/// A descriptor the caller already owns and has asked us to write to.
///
/// Deliberately opaque. Callers hold host-specific things -- a `RawFd` on
/// Unix, a `RawHandle` on Windows -- and there is no honest neutral spelling
/// for *what they hold*, so the conversion into this type is host-specific
/// and stays at the caller's edge. What is not host-specific is everything
/// after: writing all of a buffer to it, retrying the partial writes and the
/// interruptions that every host has in its own dialect.
///
/// This borrows. It does not close the descriptor, and it does not extend its
/// lifetime: the caller who opened it still decides when it goes away, and
/// using this after that is the same mistake as using the raw value would be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawDescriptor(usize);

impl RawDescriptor {
    /// Wrap a host descriptor value. Host trees call this; callers do not.
    pub(crate) fn from_value(value: usize) -> Self {
        Self(value)
    }

    /// The underlying host value, for the host tree that will use it.
    pub(crate) fn value(self) -> usize {
        self.0
    }
}

pub use crate::fs_write_all_to_descriptor as write_all_to_descriptor;

/// Private ownership of a scratch directory, anchored independently of its
/// pathname. Cleanup remains attached when an ancestor is renamed. Concurrent
/// renames during removal are not guaranteed atomic by the private backend.
#[cfg(feature = "wasm-sketch-worker")]
pub(crate) struct OwnedScratchDirectory {
    directory: Option<crate::ScratchDirectoryAnchor>,
    path: std::path::PathBuf,
}

#[cfg(feature = "wasm-sketch-worker")]
impl OwnedScratchDirectory {
    pub(crate) fn create_in(parent: &std::path::Path) -> std::io::Result<Self> {
        let temporary = tempfile::Builder::new()
            .prefix(".kernal-worker-output-")
            .tempdir_in(parent)?;
        let directory = crate::ScratchDirectoryAnchor::open(temporary.path())?;
        // Transfer cleanup ownership only after acquiring the handle. Never
        // let TempDir's pathname-based Drop remove a replacement directory.
        let path = temporary.keep();
        Ok(Self {
            directory: Some(directory),
            path,
        })
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub(crate) fn close(mut self) -> std::io::Result<()> {
        self.directory
            .take()
            .expect("scratch owner is live")
            .remove()
    }
}

#[cfg(feature = "wasm-sketch-worker")]
impl Drop for OwnedScratchDirectory {
    fn drop(&mut self) {
        if let Some(directory) = self.directory.take() {
            let _ = directory.remove();
        }
    }
}

#[cfg(feature = "fs")]
mod async_io;
#[cfg(feature = "fs")]
pub use async_io::AsyncFileIo;

#[cfg(feature = "fs")]
mod materialize;
#[cfg(feature = "fs")]
pub use materialize::{
    allocated_bytes, allow_in_place_writes, apply_metadata_mode, await_no_writers, classify,
    deny_in_place_writes, extent_sharing, file_change_marker, file_id_width, hard_link_count,
    in_place_writes_denied, make_executable, make_owner_executable, metadata_mode,
    native_call_path, path_from_raw_bytes, set_readonly, symlink_file, sync_directory_if_supported,
    volume_identity_u128, ExtentSharing, FileChangeMarker, LinkKind, WriterWait,
};
/// Path-observed file identity with the native identifier width.
#[cfg(feature = "fs")]
pub mod path_file;
/// Native replacement for publishing staged files and directories.
#[cfg(feature = "fs")]
pub mod replacement;

/// Resolve the current user's home directory using native account conventions.
///
/// On Unix, a nonempty `HOME` overrides the account database; missing or empty
/// `HOME` falls back to the account's home. On Windows, uses the user-profile
/// known folder. Returns `None` when the host cannot resolve a home; does not
/// create directories or invent a temporary fallback. Unlike
/// [`super::host::home_dir`], this is not an environment-only host fact.
#[cfg(feature = "fs")]
pub fn user_home_dir() -> Option<std::path::PathBuf> {
    dirs::home_dir()
}

#[cfg(feature = "fs")]
pub use crate::{
    fs_create_dir_all_private as create_dir_all_private,
    fs_create_private_file as create_private_file, fs_decode_path_bytes as decode_path_bytes,
    fs_encode_path_bytes as encode_path_bytes, fs_ensure_dir_private as ensure_dir_private,
    fs_file_identity as file_identity, fs_is_lock_conflict as is_lock_conflict,
    fs_open_lock_file as open_lock_file, fs_open_shared_append as open_shared_append,
    fs_path_identity as path_identity, fs_replace_file as replace_file,
    fs_sync_directory as sync_directory, fs_user_config_dir as user_config_dir,
    fs_user_data_dir as user_data_dir, fs_user_run_data_root as user_run_data_root,
    fs_user_runtime_dir as user_runtime_dir, fs_user_state_dir as user_state_dir,
    FsFileIdentity as FileIdentity,
};

/// Largest byte limit accepted by [`read_private_regular_file_bounded`].
///
/// This is deliberately a hard ceiling as well as a caller-selected limit:
/// this convenience operation returns one allocation rather than a stream.
#[cfg(feature = "fs")]
pub const MAX_PRIVATE_REGULAR_FILE_BYTES: usize = 64 * 1024 * 1024;

/// Largest byte limit accepted by [`read_context_regular_file_bounded`].
///
/// Context collection returns one allocation, so this is a hard facade cap as
/// well as a caller-selected bound.
#[cfg(feature = "fs")]
pub const MAX_CONTEXT_REGULAR_FILE_BYTES: usize = 64 * 1024 * 1024;

/// The final path component's kind, observed without following a link.
#[cfg(feature = "fs")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextPathKind {
    /// An ordinary regular file.
    RegularFile,
    /// A directory.
    Directory,
    /// A symbolic link or Windows reparse point.
    Symlink,
    /// A FIFO, device, socket, or another non-regular native object.
    Other,
}

/// Portable metadata for a context path or a coherently-read regular file.
///
/// `identity` is absent for a standalone non-following path observation:
/// obtaining a portable identity there would require opening the path and can
/// change its meaning. A successful bounded read always supplies the identity
/// of its final open handle.
#[cfg(feature = "fs")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextPathMetadata {
    /// The final component's observed kind.
    pub kind: ContextPathKind,
    /// Length for a regular file; absent for all other kinds.
    pub len: Option<u64>,
    /// Last modification time when the filesystem reports one.
    pub modified: Option<SystemTime>,
    /// Stable identity when observed from an open regular-file handle.
    pub identity: Option<FileIdentity>,
    /// A regular file with any Unix execute bit set. Always `false` for other
    /// kinds, and on Windows, which has no per-file execute bit.
    pub executable: bool,
}

/// A bounded regular-file read together with its final-handle observation.
///
/// The native implementation compares identity, length, and modification time
/// before and after reading, then re-identifies the final path without
/// following links. It rejects changes it can observe. This is not an atomic
/// filesystem-tree snapshot: trusted ancestors remain the caller's
/// responsibility, and filesystems with coarse or mutable timestamps can hide
/// an in-place content change. Native open/read calls are synchronous and
/// cannot be forcibly interrupted by this facade.
#[cfg(feature = "fs")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextFileObservation {
    /// The bytes, bounded by the caller's limit.
    pub bytes: Vec<u8>,
    /// Metadata derived from the final open handle after the read.
    pub metadata: ContextPathMetadata,
}

#[cfg(feature = "fs")]
pub(crate) fn context_path_kind(metadata: &std::fs::Metadata) -> ContextPathKind {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if metadata.file_attributes() & 0x400 != 0 {
            return ContextPathKind::Symlink;
        }
    }
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        ContextPathKind::Symlink
    } else if file_type.is_file() {
        ContextPathKind::RegularFile
    } else if file_type.is_dir() {
        ContextPathKind::Directory
    } else {
        ContextPathKind::Other
    }
}

#[cfg(feature = "fs")]
pub(crate) fn context_regular_file_metadata(
    metadata: &std::fs::Metadata,
    identity: FileIdentity,
) -> io::Result<ContextPathMetadata> {
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "context input is not a regular file",
        ));
    }
    Ok(ContextPathMetadata {
        kind: ContextPathKind::RegularFile,
        len: Some(metadata.len()),
        modified: Some(metadata.modified()?),
        identity: Some(identity),
        executable: crate::native_fs_materialize::metadata_executable(metadata),
    })
}

/// Observe the final path component without following a symbolic link.
///
/// This is a point-in-time classification only. It does not open the path,
/// establish a sandbox boundary, or make a later operation race-free.
#[cfg(feature = "fs")]
pub fn context_path_metadata_no_follow(path: &Path) -> io::Result<ContextPathMetadata> {
    let metadata = std::fs::symlink_metadata(path)?;
    let kind = context_path_kind(&metadata);
    Ok(ContextPathMetadata {
        kind,
        len: (kind == ContextPathKind::RegularFile).then_some(metadata.len()),
        modified: metadata.modified().ok(),
        identity: None,
        executable: crate::native_fs_materialize::metadata_executable(&metadata),
    })
}

/// Return a symbolic link's raw target without resolving it.
///
/// The returned path is inert text. It may be relative, dangling, or outside
/// a caller's selected root; the caller owns any policy before following it.
#[cfg(feature = "fs")]
pub fn read_context_link(path: &Path) -> io::Result<PathBuf> {
    std::fs::read_link(path)
}

/// Resolve a path using the host's canonicalization rules.
///
/// This follows links and therefore is separate from
/// [`context_path_metadata_no_follow`] and bounded regular-file reading.
#[cfg(feature = "fs")]
pub fn canonical_context_path(path: &Path) -> io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

/// Read an ordinary user-authorized regular file with a bounded allocation.
///
/// The final component is opened without following a link. Unix also opens
/// nonblocking so a FIFO can be rejected from handle metadata without waiting.
/// Ancestors are not protected from replacement or link traversal: callers
/// must trust them. At most `max_bytes + 1` bytes are allocated/read, and a
/// successful result includes a coherent final-handle observation.
#[cfg(feature = "fs")]
pub fn read_context_regular_file_bounded(
    path: &Path,
    max_bytes: usize,
) -> io::Result<ContextFileObservation> {
    if max_bytes > MAX_CONTEXT_REGULAR_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "context regular-file limit {max_bytes} exceeds the {} byte facade cap",
                MAX_CONTEXT_REGULAR_FILE_BYTES
            ),
        ));
    }
    crate::fs_read_context_regular_file_bounded(path, max_bytes)
}

// ---------------------------------------------------------------------------
// Demand-driven directory cursor
// ---------------------------------------------------------------------------

/// One owned entry observed by a [`DirectoryCursor`].
///
/// `kind` is a point-in-time, non-following classification of the entry's
/// final component. It is not a handle observation and does not make a later
/// open race-free.
#[cfg(feature = "fs")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryCursorEntry {
    path: PathBuf,
    file_name: OsString,
    kind: ContextPathKind,
}

#[cfg(feature = "fs")]
impl DirectoryCursorEntry {
    /// The entry path formed from the cursor's supplied directory path and
    /// this entry's name. It is owned but is not canonicalized or guaranteed
    /// to be absolute.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The entry's final component, owned independently of the native handle.
    pub fn file_name(&self) -> &OsStr {
        &self.file_name
    }

    /// The entry's observed final-component kind without following a link.
    pub fn kind(&self) -> ContextPathKind {
        self.kind
    }
}

/// A synchronous, demand-driven, nonrecursive directory enumeration.
///
/// The cursor owns one native directory-enumeration handle. Each
/// [`next_entry`](Self::next_entry) call asks the host for at most the next
/// entry; it neither descends into child directories nor collects, sorts, or
/// prefetches the rest of the directory. Native APIs can still buffer entries
/// internally, so this is not a claim that the operating system reads exactly
/// one directory record per call.
///
/// Dropping the cursor deterministically releases its native handle, which
/// lets a bounded consumer stop without enumerating the remainder. Calls are
/// synchronous native operations and may block. Opening a directory by path
/// follows the final link where the host does so. This cursor is not a hostile
/// path sandbox or an atomic directory snapshot: callers must trust and police
/// the root and its ancestors, and make separate observations before any
/// security-sensitive operation.
#[cfg(feature = "fs")]
pub struct DirectoryCursor {
    // Kept private so callers cannot retain a native entry/handle. `ReadDir`
    // is the kernel's native directory cursor on every supported host.
    native: std::fs::ReadDir,
}

#[cfg(feature = "fs")]
impl DirectoryCursor {
    /// Open `directory` for demand-driven, nonrecursive enumeration.
    ///
    /// Missing paths, non-directories, and inaccessible directories return the
    /// host error. The supplied spelling is retained in each entry path; it is
    /// not canonicalized.
    pub fn open(directory: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            native: std::fs::read_dir(directory)?,
        })
    }

    /// Yield the next entry, or `None` after the directory is exhausted.
    ///
    /// An error from native enumeration or entry classification is returned to
    /// the caller; it is not hidden or converted into end-of-directory.
    pub fn next_entry(&mut self) -> io::Result<Option<DirectoryCursorEntry>> {
        let Some(entry) = self.native.next() else {
            return Ok(None);
        };
        let entry = entry?;
        // Copy every facade value before `DirEntry` is dropped. On Unix a
        // retained `DirEntry` can retain the directory descriptor.
        let path = entry.path();
        let file_name = entry.file_name();
        let kind = context_path_kind(&std::fs::symlink_metadata(&path)?);
        Ok(Some(DirectoryCursorEntry {
            path,
            file_name,
            kind,
        }))
    }
}

/// Read a current-user-private regular file, rejecting links and oversized
/// input.
///
/// The final path component is opened without following a link, and all file
/// security checks are made from that same open handle before its contents are
/// read. The immediate parent must also be a current-user-private directory.
/// The path is re-identified after reading where the host can do so, so a
/// replacement of that final component during the operation is rejected rather
/// than silently read. This is not a filesystem sandbox: callers must supply a
/// trusted parent and ancestor path, and must prevent races that could replace
/// or redirect those path components while this operation runs.
///
/// `max_bytes` is both the returned-data limit and the allocation/read bound:
/// at most `max_bytes + 1` bytes are read in order to distinguish an exact
/// limit from an oversized file. Limits above
/// [`MAX_PRIVATE_REGULAR_FILE_BYTES`] are rejected.
///
/// On Unix, private means owned by the effective uid with no group or other
/// permission bits. On Windows, the trusted parent must have the protected
/// owner-and-SYSTEM DACL, and the opened file must be owned by the current user
/// with exactly the private owner-rights-and-SYSTEM full-control DACL, either
/// direct or inherited. The final component's reparse point is rejected; no
/// claim is made about reparse points in ancestors.
/// Callers that create staging files must still create them under an
/// owner-private directory (for example the runtime-directory helper's
/// result); this operation does not repair insecure paths.
#[cfg(feature = "fs")]
pub fn read_private_regular_file_bounded(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    if max_bytes > MAX_PRIVATE_REGULAR_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "private regular-file limit {max_bytes} exceeds the {} byte facade cap",
                MAX_PRIVATE_REGULAR_FILE_BYTES
            ),
        ));
    }
    crate::fs_read_private_regular_file_bounded(path, max_bytes)
}

#[cfg(feature = "fs")]
use std::ffi::{OsStr, OsString};
/// The regular-file handle type returned by the facade's openers and append seams.
///
/// Every handle-returning accessor on this module ([`open_read_regular_file`],
/// [`create_file`], [`open_read_write_file`], [`open_shared_append`],
/// [`open_lock_file`]) hands back this type. Re-exporting it lets consumers name
/// the handle in their own public signatures without reaching for
/// `std::fs::File` directly, which the platform-boundary lint bans outside the
/// adapters. The type is the host's `std::fs::File`, not a wrapper: locking,
/// metadata and I/O semantics are unchanged.
#[cfg(feature = "fs")]
pub use std::fs::File;
#[cfg(feature = "fs")]
use std::io;
#[cfg(feature = "fs")]
use std::path::{Path, PathBuf};
#[cfg(feature = "fs")]
use std::sync::Arc;
#[cfg(feature = "fs")]
use std::time::{Duration, SystemTime};

// ---------------------------------------------------------------------------
// Advisory locking
// ---------------------------------------------------------------------------

/// An advisory lock held on an open file, released automatically when this
/// guard drops.
///
/// Holding this guard *is* what "the lock is held" means in this module:
/// there is deliberately no bare `lock`/`unlock` pair here that a caller
/// could call out of balance and get wrong. Release is best-effort -- a
/// failure releasing an
/// advisory lock cannot be reported from a destructor, and on every supported
/// host closing the underlying file descriptor also releases the lock, so a
/// failed explicit unlock here is not a stuck lock, just a redundant release
/// that did not need to happen.
///
/// Advisory locks exclude other advisory-lock holders only, never a process
/// that opens and reads or writes the file without locking it -- that is true
/// on every supported host and is what "advisory" means throughout this
/// module. Unix gets it from `flock`, which is advisory by construction;
/// Windows byte-range locks are mandatory, so the Windows implementation
/// takes its lock on a single byte far past any file's data, leaving the body
/// unobstructed. The exclusion between holders is identical either way; only
/// the bytes the kernel guards differ, and no caller reads or writes those.
///
/// # One guard per open file
///
/// A guard borrows the file shared, so nothing stops a caller from holding
/// two guards on the *same* [`File`] -- and every host answers that
/// differently, so it is a programming error rather than a portable
/// operation:
///
/// - Unix `flock` converts the lock in place, so a shared guard followed by
///   an exclusive one is an upgrade, and *either* guard's drop releases the
///   file's lock outright, including the one the other guard still thinks it
///   holds.
/// - Windows refuses an exclusive request that overlaps a range the same
///   handle already locked: [`try_lock_exclusive`] reports a conflict (see
///   [`is_lock_conflict`]) and [`lock_exclusive`] waits forever, because the
///   only holder that could release it is the caller that is waiting.
///
/// Hold one guard per open file. A second lock on the same path needs a
/// second [`open_lock_file`] handle, which is also what makes the exclusion
/// between the two meaningful.
#[cfg(feature = "fs")]
#[derive(Debug)]
pub struct FileLock<'file> {
    file: &'file File,
}

#[cfg(feature = "fs")]
impl<'file> FileLock<'file> {
    fn new(file: &'file File) -> Self {
        Self { file }
    }
}

#[cfg(feature = "fs")]
impl Drop for FileLock<'_> {
    fn drop(&mut self) {
        let _ = crate::fs_unlock(self.file);
    }
}

/// Take an exclusive advisory lock, waiting until it is available.
///
/// The returned guard releases the lock when dropped.
///
/// # Errors
///
/// Returns an error if the host lock call fails for a reason other than
/// waiting for the lock.
#[cfg(feature = "fs")]
#[deprecated(note = "waits forever for a stuck holder; use `lock_exclusive_for` (#433)")]
pub fn lock_exclusive(file: &File) -> io::Result<FileLock<'_>> {
    crate::fs_lock_exclusive(file)?;
    Ok(FileLock::new(file))
}

/// Take a shared advisory lock, waiting until it is available.
///
/// The returned guard releases the lock when dropped.
///
/// # Errors
///
/// Returns an error if the host lock call fails for a reason other than
/// waiting for the lock.
#[cfg(feature = "fs")]
#[deprecated(note = "waits forever for a stuck holder; use `lock_shared_for` (#433)")]
pub fn lock_shared(file: &File) -> io::Result<FileLock<'_>> {
    crate::fs_lock_shared(file)?;
    Ok(FileLock::new(file))
}

/// Take an exclusive advisory lock without waiting.
///
/// Returns immediately when another holder has it; the caller decides
/// whether that is a conflict worth retrying, via [`is_lock_conflict`].
///
/// # Errors
///
/// Returns an error if another holder has the lock (see
/// [`is_lock_conflict`]), or if the host lock call fails for another reason.
#[cfg(feature = "fs")]
pub fn try_lock_exclusive(file: &File) -> io::Result<FileLock<'_>> {
    crate::fs_try_lock_exclusive(file)?;
    Ok(FileLock::new(file))
}

/// Take a shared advisory lock without waiting.
///
/// Returns immediately when an exclusive holder has it; the caller decides
/// whether that is a conflict worth retrying, via [`is_lock_conflict`].
///
/// # Errors
///
/// Returns an error if an exclusive holder has the lock (see
/// [`is_lock_conflict`]), or if the host lock call fails for another reason.
#[cfg(feature = "fs")]
pub fn try_lock_shared(file: &File) -> io::Result<FileLock<'_>> {
    crate::fs_try_lock_shared(file)?;
    Ok(FileLock::new(file))
}

/// A held advisory lock that owns its file handle.
///
/// [`FileLock`] borrows, which is right when the lock and the handle live in
/// one scope. It cannot express a lock that outlives the function taking it:
/// a struct cannot hold a `File` and a guard borrowing that same `File`, and
/// a function cannot return one without returning the other. Both are
/// ordinary -- a lock file held for the lifetime of a working directory, a
/// fetch lock returned to a caller -- so this owns the handle instead.
///
/// Dropping releases the lock and closes the handle, in that order.
#[cfg(feature = "fs")]
#[derive(Debug)]
pub struct OwnedFileLock {
    file: Option<File>,
}

#[cfg(feature = "fs")]
impl OwnedFileLock {
    /// Borrow the locked handle, to read or write the file the lock guards.
    pub fn file(&self) -> &File {
        self.file
            .as_ref()
            .expect("the handle is taken only by unlock, which consumes self")
    }

    /// Release the lock and hand the handle back, still open.
    ///
    /// Use this to unlock at a point of your choosing rather than at the end
    /// of the scope, or to keep reading the file afterwards.
    ///
    /// # Errors
    ///
    /// Returns the handle alongside the error if the host refused to unlock,
    /// since a caller that cannot unlock generally still needs to close.
    pub fn unlock(mut self) -> Result<File, (File, io::Error)> {
        let file = self
            .file
            .take()
            .expect("the handle is taken only here, and this consumes self");
        match crate::fs_unlock(&file) {
            Ok(()) => Ok(file),
            Err(error) => Err((file, error)),
        }
    }
}

#[cfg(feature = "fs")]
impl Drop for OwnedFileLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.as_ref() {
            let _ = crate::fs_unlock(file);
        }
    }
}

/// Take an exclusive advisory lock that owns `file`, waiting for it.
///
/// # Errors
///
/// Returns an error if the host lock call fails. `file` is closed in that
/// case; a caller that needs the handle back on failure should use
/// [`lock_exclusive_for`] and keep its own handle.
#[cfg(feature = "fs")]
#[deprecated(note = "waits forever for a stuck holder; use `lock_exclusive_owned_for` (#433)")]
pub fn lock_exclusive_owned(file: File) -> io::Result<OwnedFileLock> {
    crate::fs_lock_exclusive(&file)?;
    Ok(OwnedFileLock { file: Some(file) })
}

/// Take a shared advisory lock that owns `file`, waiting for it.
///
/// # Errors
///
/// As [`lock_exclusive_owned`].
#[cfg(feature = "fs")]
#[deprecated(note = "waits forever for a stuck holder; use `lock_shared_owned_for` (#433)")]
pub fn lock_shared_owned(file: File) -> io::Result<OwnedFileLock> {
    crate::fs_lock_shared(&file)?;
    Ok(OwnedFileLock { file: Some(file) })
}

/// Take an exclusive advisory lock that owns `file`, without waiting.
///
/// # Errors
///
/// Returns an error if another holder has the lock (see
/// [`is_lock_conflict`]), or if the host lock call fails. `file` is closed in
/// either case.
#[cfg(feature = "fs")]
pub fn try_lock_exclusive_owned(file: File) -> io::Result<OwnedFileLock> {
    crate::fs_try_lock_exclusive(&file)?;
    Ok(OwnedFileLock { file: Some(file) })
}

/// Take a shared advisory lock that owns `file`, without waiting.
///
/// # Errors
///
/// As [`try_lock_exclusive_owned`].
#[cfg(feature = "fs")]
pub fn try_lock_shared_owned(file: File) -> io::Result<OwnedFileLock> {
    crate::fs_try_lock_shared(&file)?;
    Ok(OwnedFileLock { file: Some(file) })
}

/// Retry a non-blocking lock attempt on the same handle until it succeeds,
/// fails for a reason other than contention, or `timeout` elapses.
#[cfg(feature = "fs")]
fn lock_within(
    file: &File,
    timeout: Duration,
    attempt: fn(&File) -> io::Result<()>,
) -> io::Result<()> {
    let started = std::time::Instant::now();
    let mut pause = Duration::from_millis(1);
    loop {
        match attempt(file) {
            Err(error) if is_lock_conflict(&error) => {
                let elapsed = started.elapsed();
                if elapsed >= timeout {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "advisory file lock was not acquired within its timeout",
                    ));
                }
                std::thread::sleep(pause.min(timeout - elapsed));
                pause = (pause * 2).min(Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

/// Take an exclusive advisory lock, waiting at most `timeout`.
///
/// # Errors
///
/// Returns [`io::ErrorKind::TimedOut`] if another holder keeps the lock for
/// the whole timeout, or the host error if the lock call fails otherwise.
#[cfg(feature = "fs")]
pub fn lock_exclusive_for(file: &File, timeout: Duration) -> io::Result<FileLock<'_>> {
    lock_within(file, timeout, crate::fs_try_lock_exclusive)?;
    Ok(FileLock::new(file))
}

/// Take a shared advisory lock, waiting at most `timeout`.
///
/// # Errors
///
/// As [`lock_exclusive_for`].
#[cfg(feature = "fs")]
pub fn lock_shared_for(file: &File, timeout: Duration) -> io::Result<FileLock<'_>> {
    lock_within(file, timeout, crate::fs_try_lock_shared)?;
    Ok(FileLock::new(file))
}

/// Take an exclusive advisory lock that owns `file`, waiting at most
/// `timeout`. Unlike retrying [`try_lock_exclusive_owned`], the handle is
/// kept across attempts.
///
/// # Errors
///
/// As [`lock_exclusive_for`]; `file` is closed on error.
#[cfg(feature = "fs")]
pub fn lock_exclusive_owned_for(file: File, timeout: Duration) -> io::Result<OwnedFileLock> {
    lock_within(&file, timeout, crate::fs_try_lock_exclusive)?;
    Ok(OwnedFileLock { file: Some(file) })
}

/// Take a shared advisory lock that owns `file`, waiting at most `timeout`.
///
/// # Errors
///
/// As [`lock_exclusive_owned_for`].
#[cfg(feature = "fs")]
pub fn lock_shared_owned_for(file: File, timeout: Duration) -> io::Result<OwnedFileLock> {
    lock_within(&file, timeout, crate::fs_try_lock_shared)?;
    Ok(OwnedFileLock { file: Some(file) })
}

// ---------------------------------------------------------------------------
// Modification time
// ---------------------------------------------------------------------------

/// A filesystem modification time, independent of any host's on-disk
/// representation.
///
/// This is a Unix-epoch second and a nanosecond remainder, not
/// [`SystemTime`]: an on-disk mtime is fundamentally that pair (a filesystem
/// can legitimately record a time before 1970 as a negative second count),
/// and going through `SystemTime`'s own epoch arithmetic for every
/// construction and every host's native call would risk losing precision or
/// failing on values a filesystem can actually hold. A value only ever moves
/// between [`Metadata`](std::fs::Metadata) (via
/// [`from_last_modification_time`](FileTime::from_last_modification_time))
/// and a file (via [`set_file_mtime`]). A client that persists mtimes rather
/// than only copying them between a file and its metadata reads the value
/// back through [`unix_seconds`](FileTime::unix_seconds) and
/// [`nanoseconds`](FileTime::nanoseconds), which are the same two fields the
/// constructors take.
#[cfg(feature = "fs")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileTime {
    seconds_since_unix_epoch: i64,
    nanoseconds: u32,
}

#[cfg(feature = "fs")]
impl FileTime {
    /// Construct a modification time from a Unix-epoch second count and a
    /// nanosecond remainder.
    ///
    /// `seconds_since_unix_epoch` may be negative, for a time before 1970,
    /// matching what an on-disk mtime can legitimately encode.
    pub fn from_unix_time(seconds_since_unix_epoch: i64, nanoseconds: u32) -> Self {
        Self {
            seconds_since_unix_epoch,
            nanoseconds,
        }
    }

    /// The whole seconds since the Unix epoch, negative before 1970.
    ///
    /// With [`nanoseconds`](FileTime::nanoseconds) this is the exact pair
    /// [`from_unix_time`](FileTime::from_unix_time) accepts, so a persisted
    /// mtime round-trips without going through `SystemTime` -- which is the
    /// point, since `SystemTime` cannot represent every value a filesystem
    /// can hold.
    pub const fn unix_seconds(self) -> i64 {
        self.seconds_since_unix_epoch
    }

    /// The sub-second remainder, in nanoseconds.
    ///
    /// This is a remainder, not a total: it is always in `0..1_000_000_000`
    /// and is *not* signed for times before 1970. A pre-epoch mtime has a
    /// negative [`unix_seconds`](FileTime::unix_seconds) and a positive
    /// nanosecond remainder counting forward from it, which is how both the
    /// host field and `from_unix_time` encode it.
    pub const fn nanoseconds(self) -> u32 {
        self.nanoseconds
    }

    /// Construct a modification time from [`SystemTime`], such as
    /// [`Metadata::modified`](std::fs::Metadata::modified).
    pub fn from_system_time(time: SystemTime) -> Self {
        match time.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(duration) => Self {
                seconds_since_unix_epoch: duration.as_secs() as i64,
                nanoseconds: duration.subsec_nanos(),
            },
            Err(before_epoch) => {
                let duration = before_epoch.duration();
                let subsec = duration.subsec_nanos();
                let (seconds, nanoseconds) = if subsec == 0 {
                    (-(duration.as_secs() as i64), 0)
                } else {
                    (-(duration.as_secs() as i64) - 1, 1_000_000_000 - subsec)
                };
                Self {
                    seconds_since_unix_epoch: seconds,
                    nanoseconds,
                }
            }
        }
    }

    /// The current modification time, as this host's clock reports it now.
    pub fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    /// The modification time already recorded in `metadata`, such as from
    /// [`std::fs::metadata`] or [`std::fs::symlink_metadata`].
    ///
    /// Infallible: unlike [`Metadata::modified`](std::fs::Metadata::modified),
    /// this reads the raw host field directly rather than going through a
    /// conversion that can report the host as not supporting mtimes at all,
    /// which none of Linux, macOS, or Windows fail to.
    pub fn from_last_modification_time(metadata: &std::fs::Metadata) -> Self {
        last_modification_time(metadata)
    }
}

#[cfg(all(feature = "fs", unix))]
fn last_modification_time(metadata: &std::fs::Metadata) -> FileTime {
    use std::os::unix::fs::MetadataExt as _;

    FileTime {
        seconds_since_unix_epoch: metadata.mtime(),
        nanoseconds: metadata.mtime_nsec().clamp(0, 999_999_999) as u32,
    }
}

#[cfg(all(feature = "fs", windows))]
fn last_modification_time(metadata: &std::fs::Metadata) -> FileTime {
    use std::os::windows::fs::MetadataExt as _;

    windows_file_time_to_unix(metadata.last_write_time())
}

/// Windows-epoch (1601-01-01) 100-nanosecond ticks, converted to a Unix-epoch
/// second/nanosecond pair. Shared by [`last_modification_time`]; kept here
/// rather than duplicated per host because the conversion itself does not
/// diverge -- only which raw field a host hands it does.
#[cfg(all(feature = "fs", windows))]
fn windows_file_time_to_unix(ticks: u64) -> FileTime {
    /// 100-nanosecond ticks between the Windows epoch (1601-01-01) and the
    /// Unix epoch (1970-01-01).
    const WINDOWS_TO_UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;

    let ticks = ticks as i64 - WINDOWS_TO_UNIX_EPOCH_TICKS;
    let seconds_since_unix_epoch = ticks.div_euclid(10_000_000);
    let remainder_ticks = ticks.rem_euclid(10_000_000);
    FileTime {
        seconds_since_unix_epoch,
        nanoseconds: (remainder_ticks * 100) as u32,
    }
}

/// Set the modification time of the file at `path`, without disturbing its
/// access time.
///
/// `path` may name a directory as well as a regular file. Directories carry
/// an mtime on every host this crate supports, and a client restoring a
/// stored tree has to put it back.
///
/// # Errors
///
/// Returns an error if the caller lacks permission to change the file's
/// timestamps, if `path` does not exist, or if `time` is out of range for
/// this host's clock.
#[cfg(feature = "fs")]
pub fn set_file_mtime(path: &Path, time: FileTime) -> io::Result<()> {
    crate::fs_set_file_mtime(path, time.seconds_since_unix_epoch, time.nanoseconds)
}

// ---------------------------------------------------------------------------
// Copy
// ---------------------------------------------------------------------------

/// How [`copy_file`] moved the bytes.
#[cfg(feature = "fs")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyOutcome {
    /// The destination shares the source's data blocks (copy-on-write); no
    /// file content was duplicated on disk.
    Reflinked,
    /// The filesystem, or this particular pair of paths, does not support a
    /// reflink here, so this many bytes were duplicated instead.
    Copied {
        /// The number of bytes written to `destination`.
        bytes: u64,
    },
}

/// Copy `source` to `destination`, preferring a reflink over a byte-for-byte
/// copy.
///
/// A reflink shares data blocks copy-on-write instead of duplicating them, so
/// it is near-instant and free of disk space until either copy is modified --
/// but only some filesystems support it (btrfs, XFS with `reflink=1`, APFS,
/// ReFS on Windows Server, and a few others), and both paths generally need
/// to be on the same volume. Where a reflink is not possible this transparently
/// falls back to a full byte copy, and [`CopyOutcome`] tells the caller which
/// one actually happened -- a caller that specifically needs the cheap path
/// can detect the expensive one rather than assume.
///
/// # Errors
///
/// Returns an error if neither a reflink nor a byte copy could complete, such
/// as `source` not existing or a permissions failure on `destination`.
#[cfg(feature = "fs")]
pub fn copy_file(source: &Path, destination: &Path) -> io::Result<CopyOutcome> {
    match reflink_copy::reflink_or_copy(source, destination)? {
        None => Ok(CopyOutcome::Reflinked),
        Some(bytes) => Ok(CopyOutcome::Copied { bytes }),
    }
}

/// Reflink `source` to `destination`, failing rather than falling back.
///
/// [`copy_file`] is the right call when the goal is that the bytes arrive.
/// This is for the two cases where a silent byte copy is the wrong answer:
///
/// - **Probing a filesystem.** "Does this volume support reflinks?" is
///   answered by attempting one and seeing whether it works. Asked through
///   [`copy_file`], every volume answers yes, because the fallback always
///   succeeds -- so a caller that stores the result as a capability would
///   record a capability the filesystem does not have.
/// - **Taking the cheap path or none.** A caller that reflinks to avoid
///   duplicating a large artifact, and does something else entirely when it
///   cannot, must not have a full copy performed on its behalf first. With
///   [`copy_file`] it would pay for the copy and then, on discovering
///   [`CopyOutcome::Copied`], have to undo it.
///
/// `destination` must not already exist.
///
/// # Errors
///
/// Returns an error when the filesystem does not support reflinking this
/// pair of paths -- typically because they are on different volumes or the
/// filesystem has no copy-on-write support -- and for the ordinary reasons a
/// copy fails, such as `source` not existing. The two are deliberately not
/// distinguished: a caller that cannot reflink here does the same thing
/// whichever it is.
#[cfg(feature = "fs")]
pub fn reflink_file(source: &Path, destination: &Path) -> io::Result<()> {
    reflink_copy::reflink(source, destination)
}

// ---------------------------------------------------------------------------
// Parallel directory walk
// ---------------------------------------------------------------------------

/// One entry yielded by a [`DirectoryWalk`].
#[cfg(feature = "fs")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryEntry {
    path: PathBuf,
    depth: usize,
    is_directory: bool,
    is_file: bool,
    is_symbolic_link: bool,
    followed_symbolic_links: bool,
}

#[cfg(feature = "fs")]
impl DirectoryEntry {
    /// The absolute path of this entry.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Consume the entry, keeping only its path.
    pub fn into_path(self) -> PathBuf {
        self.path
    }

    /// The final component of this entry's path.
    ///
    /// For a root that has no final component (`/`, `C:\`), this is the
    /// whole root path, so it is never empty.
    pub fn file_name(&self) -> &OsStr {
        self.path
            .file_name()
            .unwrap_or_else(|| self.path.as_os_str())
    }

    /// Depth of this entry relative to the walk's root, which is depth `0`.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Whether this entry is a directory.
    pub fn is_directory(&self) -> bool {
        self.is_directory
    }

    /// Whether this entry is a regular file.
    pub fn is_file(&self) -> bool {
        self.is_file
    }

    /// Whether this entry is a symbolic link that was not followed.
    ///
    /// Never true when the walk that produced it followed symbolic links --
    /// in that case this entry describes the link's target instead.
    pub fn is_symbolic_link(&self) -> bool {
        self.is_symbolic_link
    }

    /// Re-read this entry's metadata from disk, describing the same thing
    /// the entry does: the link itself when the walk did not follow
    /// symbolic links, its target when it did.
    ///
    /// # Errors
    ///
    /// Returns an error if the entry no longer exists or cannot be read.
    pub fn metadata(&self) -> io::Result<std::fs::Metadata> {
        if self.followed_symbolic_links {
            std::fs::metadata(&self.path)
        } else {
            std::fs::symlink_metadata(&self.path)
        }
    }
}

#[cfg(feature = "fs")]
fn directory_entry_from_jwalk(
    entry: &jwalk::DirEntry<WalkState>,
    followed_symbolic_links: bool,
) -> DirectoryEntry {
    let file_type = entry.file_type();
    DirectoryEntry {
        path: entry.path(),
        depth: entry.depth(),
        is_directory: file_type.is_dir(),
        is_file: file_type.is_file(),
        is_symbolic_link: file_type.is_symlink(),
        followed_symbolic_links,
    }
}

/// Translate a walker error into the facade's error contract: a saturated
/// shared pool is [`io::ErrorKind::ResourceBusy`], a symbolic-link cycle is
/// [`io::ErrorKind::InvalidData`], and an I/O failure keeps its own kind. The
/// walker's error type is not carried along as the source.
#[cfg(feature = "fs")]
fn directory_walk_error(error: jwalk::Error) -> io::Error {
    if error.is_busy() {
        return io::Error::new(
            io::ErrorKind::ResourceBusy,
            "directory walk incomplete: the shared worker pool stayed busy, \
             so the walk ended before reading the whole tree",
        );
    }
    if let Some(ancestor) = error.loop_ancestor() {
        return symbolic_link_cycle(error.path().unwrap_or(ancestor), ancestor);
    }
    let message = error.to_string();
    match error.into_io_error() {
        Some(io_error) => io::Error::new(io_error.kind(), message),
        None => io::Error::other(message),
    }
}

/// The error item a followed walk yields in place of a directory link that
/// leads back to one of its own ancestors.
#[cfg(feature = "fs")]
fn symbolic_link_cycle(link: &Path, ancestor: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "symbolic link cycle: {} leads back to its ancestor {}",
            link.display(),
            ancestor.display()
        ),
    )
}

/// How long a shared-pool walk waits for a worker before giving up; the
/// walker's own long-standing default, stated so the documented behaviour of
/// [`DirectoryWalkParallelism::SharedPool`] cannot drift from it.
#[cfg(feature = "fs")]
const SHARED_POOL_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// The predicate a [`DirectoryWalk`] consults before descending into a
/// directory. Shared, because the walk hands it to every worker thread that
/// reads a directory, and named, because spelling it inline twice is the
/// kind of type that stops being readable.
#[cfg(feature = "fs")]
type PruneDirectories = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

/// The predicate a [`DirectoryWalk`] consults for every entry below the root.
#[cfg(feature = "fs")]
type FilterEntries = Arc<dyn Fn(&DirectoryEntry) -> bool + Send + Sync>;

/// Where a [`DirectoryWalk`] runs its directory reads.
///
/// Whatever the choice, a walk never ends short without saying so: an
/// incomplete walk always yields an error item before it ends. Only
/// [`DirectoryWalkParallelism::SharedPool`] can end a walk early for a reason
/// other than the tree itself, and it reports that as an
/// [`io::ErrorKind::ResourceBusy`] item.
#[cfg(feature = "fs")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DirectoryWalkParallelism {
    /// Read every directory on the thread that iterates the walk. No pool,
    /// no worker threads, nothing shared -- so nothing else on the machine
    /// can make the walk fail. The choice for a caller that skips error
    /// items or must not fail under load.
    ///
    /// Slower than a pool on a large tree on fast storage; the right choice
    /// whenever a walk's result feeds a fingerprint or an inventory that
    /// must be complete.
    Sequential,
    /// Share the process-wide worker pool with every other walk and parallel
    /// computation. Cheapest parallel option, and the default.
    ///
    /// Because that pool is shared, the walk cannot rely on getting a worker:
    /// when none picks it up within about one second -- the pool saturated by
    /// other work, or this walk started from inside that same pool -- the
    /// walk yields an [`io::ErrorKind::ResourceBusy`] error item and ends
    /// rather than risk a deadlock. On a busy machine that is a failure
    /// caused by load alone, so a caller that skips error items
    /// (`.flatten()`, `filter_map(Result::ok)`) must not use this mode.
    /// [`DirectoryWalkParallelism::Sequential`] is the drop-in for
    /// `walkdir`-style use.
    #[default]
    SharedPool,
    /// Give this walk its own worker pool, created when the walk starts and
    /// released when it ends.
    ///
    /// `threads` is the pool size; zero means one thread per available CPU.
    /// The walk never waits on other work for a worker, so it never ends
    /// early because the machine is busy. That costs one pool creation per
    /// walk.
    DedicatedPool {
        /// Worker threads; zero selects one per available CPU.
        threads: usize,
    },
}

/// A directory-tree walk, configured before it runs.
///
/// Every option here defaults to what [`std::fs::read_dir`] itself would do
/// for a single directory: hidden entries included, symbolic links not
/// followed, no ordering guarantee, no depth limit, and nothing filtered.
/// Reads run on the shared worker pool unless
/// [`DirectoryWalk::parallelism`] says otherwise.
///
/// # Errors and completeness
///
/// An entry the walk could not read (a permissions failure, a race with
/// something deleting the tree) is an error item, and the walk goes on. A
/// walk that ends before covering the tree always yields an error item
/// first; the only such case is a busy
/// [`DirectoryWalkParallelism::SharedPool`]. A stream with no error items is
/// therefore the whole tree, as filtered.
///
/// # Symbolic-link cycles
///
/// When [`DirectoryWalk::follow_symbolic_links`] is on and a directory link
/// points at one of its own ancestors, the link is yielded as an
/// [`io::ErrorKind::InvalidData`] error item naming the link and the
/// ancestor, in place of an entry; it is not descended into, and the rest of
/// the walk continues. Without following, a link is an ordinary entry with
/// [`DirectoryEntry::is_symbolic_link`] set and is never descended into, so
/// there is no cycle to report.
#[cfg(feature = "fs")]
pub struct DirectoryWalk {
    root: PathBuf,
    follow_symbolic_links: bool,
    include_hidden_entries: bool,
    sorted: bool,
    max_depth: Option<usize>,
    parallelism: DirectoryWalkParallelism,
    prune_directories: Option<PruneDirectories>,
    filter_entries: Option<FilterEntries>,
}

#[cfg(feature = "fs")]
impl DirectoryWalk {
    /// Start configuring a walk rooted at `root`.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            follow_symbolic_links: false,
            include_hidden_entries: true,
            sorted: false,
            max_depth: None,
            parallelism: DirectoryWalkParallelism::SharedPool,
            prune_directories: None,
            filter_entries: None,
        }
    }

    /// Whether to follow symbolic links into their targets. Default: `false`.
    pub fn follow_symbolic_links(mut self, follow: bool) -> Self {
        self.follow_symbolic_links = follow;
        self
    }

    /// Whether to include entries this host considers hidden. Default:
    /// `true`.
    pub fn include_hidden_entries(mut self, include: bool) -> Self {
        self.include_hidden_entries = include;
        self
    }

    /// Whether entries are yielded in a deterministic, sorted order.
    /// Default: `false`, which is faster.
    pub fn sorted(mut self, sorted: bool) -> Self {
        self.sorted = sorted;
        self
    }

    /// Yield nothing deeper than `depth`, and never read a directory whose
    /// children would be deeper. The root is depth `0`, so `max_depth(0)`
    /// yields only the root and `max_depth(1)` its direct children.
    /// Default: unlimited.
    pub fn max_depth(mut self, depth: usize) -> Self {
        self.max_depth = Some(depth);
        self
    }

    /// Where the walk's directory reads run. Default:
    /// [`DirectoryWalkParallelism::SharedPool`].
    ///
    /// Choose [`DirectoryWalkParallelism::Sequential`] (or a dedicated pool)
    /// when the walk must not fail merely because the shared pool is busy.
    pub fn parallelism(mut self, parallelism: DirectoryWalkParallelism) -> Self {
        self.parallelism = parallelism;
        self
    }

    /// Skip descending into (and yielding) any directory for which `keep`
    /// returns `false`.
    ///
    /// This is evaluated once per directory, before it is read, so a rejected
    /// directory's contents are never touched -- the reason to use this
    /// instead of filtering [`DirectoryWalk::walk`]'s output is exactly that
    /// short-circuit, on a tree where the pruned subtrees (`.git`, `target`,
    /// `node_modules`) can dwarf the rest.
    pub fn prune_directories<F>(mut self, keep: F) -> Self
    where
        F: Fn(&Path) -> bool + Send + Sync + 'static,
    {
        let keep: PruneDirectories = Arc::new(keep);
        self.prune_directories = Some(keep);
        self
    }

    /// Skip any entry below the root for which `keep` returns `false`:
    /// files, links and directories alike. A rejected directory is neither
    /// yielded nor descended into.
    ///
    /// Unlike [`DirectoryWalk::prune_directories`] the predicate sees the
    /// whole [`DirectoryEntry`], so it can match on kind, name or depth. The
    /// root itself is always yielded. When both are set, an entry is kept
    /// only if both keep it.
    pub fn filter_entries<F>(mut self, keep: F) -> Self
    where
        F: Fn(&DirectoryEntry) -> bool + Send + Sync + 'static,
    {
        let keep: FilterEntries = Arc::new(keep);
        self.filter_entries = Some(keep);
        self
    }

    /// Run the walk, yielding entries as they are discovered.
    ///
    /// See the type's documentation for which items are errors and the
    /// guarantee that an incomplete walk always says so.
    pub fn walk(self) -> impl Iterator<Item = io::Result<DirectoryEntry>> {
        let follow = self.follow_symbolic_links;
        let mut walker = WalkDirWithState::new(&self.root)
            .follow_links(follow)
            .skip_hidden(!self.include_hidden_entries)
            .sort(self.sorted)
            .parallelism(match self.parallelism {
                DirectoryWalkParallelism::Sequential => jwalk::Parallelism::Serial,
                DirectoryWalkParallelism::SharedPool => jwalk::Parallelism::RayonDefaultPool {
                    busy_timeout: SHARED_POOL_BUSY_TIMEOUT,
                },
                // A private pool has no busy-timeout: nothing else can hold
                // its workers, so the deadlock guard has nothing to guard.
                DirectoryWalkParallelism::DedicatedPool { threads } => {
                    jwalk::Parallelism::RayonNewPool(threads)
                }
            });
        if let Some(depth) = self.max_depth {
            // Applied after the parallelism: the walker drops to a serial
            // read on its own for a depth below two, which only removes the
            // shared-pool failure mode.
            walker = walker.max_depth(depth);
        }
        let select = SelectChildren {
            follow,
            prune_directories: self.prune_directories,
            filter_entries: self.filter_entries,
        };
        walker = walker.process_read_dir(move |_depth, directory, ancestors, children| {
            select.apply(directory, ancestors, children);
        });
        walker.into_iter().map(move |entry| match entry {
            Ok(mut entry) => match entry.client_state.take() {
                Some(ancestor) => Err(symbolic_link_cycle(&entry.path(), &ancestor)),
                None => Ok(directory_entry_from_jwalk(&entry, follow)),
            },
            Err(error) => Err(directory_walk_error(error)),
        })
    }
}

/// The real directories a followed walk has entered on the way to the one
/// being read, nearest first. Each read hands its children a copy, so a
/// directory sees exactly its own chain of ancestors.
#[cfg(feature = "fs")]
#[derive(Clone, Debug, Default)]
struct WalkAncestors(Option<Arc<WalkAncestor>>);

#[cfg(feature = "fs")]
#[derive(Debug)]
struct WalkAncestor {
    canonical: PathBuf,
    parent: WalkAncestors,
}

#[cfg(feature = "fs")]
impl WalkAncestors {
    fn find(&self, canonical: &Path) -> Option<&Path> {
        let mut next = self.0.as_deref();
        while let Some(ancestor) = next {
            if ancestor.canonical == canonical {
                return Some(&ancestor.canonical);
            }
            next = ancestor.parent.0.as_deref();
        }
        None
    }
}

/// Per-walk state: the ancestor chain of each read, and on an entry the
/// ancestor it loops back to, when it is a symbolic-link cycle.
#[cfg(feature = "fs")]
type WalkState = (WalkAncestors, Option<PathBuf>);

#[cfg(feature = "fs")]
type WalkDirWithState = jwalk::WalkDirGeneric<WalkState>;

/// What a walk decides about each directory's children, once per read.
#[cfg(feature = "fs")]
struct SelectChildren {
    follow: bool,
    prune_directories: Option<PruneDirectories>,
    filter_entries: Option<FilterEntries>,
}

#[cfg(feature = "fs")]
impl SelectChildren {
    fn apply(
        &self,
        directory: &Path,
        ancestors: &mut WalkAncestors,
        children: &mut Vec<Result<jwalk::DirEntry<WalkState>, jwalk::Error>>,
    ) {
        if self.follow {
            // Canonical, so a link is recognised by where it lands rather
            // than how it is spelled: relative (`..`) and absolute links to
            // an ancestor are the same cycle.
            if let Ok(canonical) = std::fs::canonicalize(directory) {
                *ancestors = WalkAncestors(Some(Arc::new(WalkAncestor {
                    canonical,
                    parent: std::mem::take(ancestors),
                })));
            }
        }
        children.retain_mut(|entry| {
            let Ok(entry) = entry else {
                return true;
            };
            if self.follow && entry.file_type().is_dir() && entry.path_is_symlink() {
                let looped = std::fs::canonicalize(entry.path())
                    .ok()
                    .and_then(|canonical| ancestors.find(&canonical).map(Path::to_path_buf));
                if let Some(ancestor) = looped {
                    entry.read_children_path = None;
                    entry.client_state = Some(ancestor);
                    return true;
                }
            }
            if let Some(keep) = &self.prune_directories {
                if entry.file_type().is_dir() && !keep(&entry.path()) {
                    return false;
                }
            }
            self.filter_entries
                .as_ref()
                .is_none_or(|keep| keep(&directory_entry_from_jwalk(entry, self.follow)))
        });
    }
}

// ---------------------------------------------------------------------------
// Glob matching
// ---------------------------------------------------------------------------

/// A compiled set of glob patterns, matched together against one candidate
/// path per call.
///
/// Build one with [`PatternSetBuilder`].
#[cfg(feature = "fs")]
#[derive(Clone, Debug)]
pub struct PatternSet(globset::GlobSet);

#[cfg(feature = "fs")]
impl PatternSet {
    /// Whether `path` matches any pattern in this set.
    ///
    /// Accepts anything path-like because callers match against whatever
    /// they already hold -- a `String` of a relative path built for display,
    /// an `OsStr` from a directory entry -- and requiring `&Path` only makes
    /// them write the conversion at every call site.
    pub fn is_match(&self, path: impl AsRef<Path>) -> bool {
        self.0.is_match(path.as_ref())
    }

    /// Whether this set contains no patterns.
    ///
    /// An empty set matches nothing, so a caller filtering with "exclude
    /// unless excluded" logic uses this to skip the walk's matching work
    /// entirely rather than calling [`is_match`](PatternSet::is_match) per
    /// candidate to be told no every time.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many patterns this set contains.
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// Builds a [`PatternSet`] from glob patterns.
///
/// Patterns are validated together at [`PatternSetBuilder::build`] rather
/// than one at a time as they are added, so a caller assembling a set from a
/// product's own configuration gets one place to report a bad pattern.
#[cfg(feature = "fs")]
#[derive(Debug, Default)]
pub struct PatternSetBuilder {
    patterns: Vec<String>,
}

#[cfg(feature = "fs")]
impl PatternSetBuilder {
    /// Start with no patterns.
    pub fn new() -> Self {
        Self {
            patterns: Vec::new(),
        }
    }

    /// Add one glob pattern to the set.
    pub fn add_pattern(mut self, pattern: &str) -> Self {
        self.patterns.push(pattern.to_owned());
        self
    }

    /// Compile the added patterns into a [`PatternSet`].
    ///
    /// # Errors
    ///
    /// Returns an error if any added pattern is not valid glob syntax.
    pub fn build(self) -> io::Result<PatternSet> {
        let mut builder = globset::GlobSetBuilder::new();
        for pattern in &self.patterns {
            let glob = globset::Glob::new(pattern)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            builder.add(glob);
        }
        let set = builder
            .build()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        Ok(PatternSet(set))
    }
}

// ---------------------------------------------------------------------------
// Plain file mutation
// ---------------------------------------------------------------------------
//
// The facade owns ordinary file mutation so that clients stop reaching for
// `std::fs` directly (zccache#1519's migration inventory, `disposition =
// "extend"`). These are deliberately the plain operations: a caller that needs
// a private directory, a staged replacement or an atomic publish uses the
// narrower helpers above, which carry those invariants. Nothing here repairs
// permissions or proves containment.

/// Replace `path`'s contents with `bytes`, creating the file if it is absent.
///
/// The write is not atomic: a concurrent reader can observe a partial file and
/// a crash can leave a truncated one. Callers that need an all-or-nothing
/// publication write a sibling temporary file and
/// [`rename`] it over the destination.
///
/// # Errors
///
/// Returns an error for the ordinary reasons a write fails: a missing parent
/// directory, a permission denial, a read-only file, or a full filesystem.
#[cfg(feature = "fs")]
pub fn write_file(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    std::fs::write(path, bytes)
}

/// Remove the file at `path`.
///
/// A missing file is an error, which keeps a caller from silently treating a
/// failed removal as success. Use [`std::fs::remove_file`]'s own tolerance via
/// the caller if that distinction does not matter to it.
///
/// # Errors
///
/// Returns an error when the file does not exist, when `path` is a directory,
/// or when the removal is denied.
#[cfg(feature = "fs")]
pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::remove_file(path)
}

/// Rename `from` to `to`, replacing `to` when it already exists.
///
/// This is the facade's publish primitive: on the same filesystem a rename is
/// atomic, so a reader sees either the old file or the new one. Across
/// filesystems the call fails rather than falling back to a copy.
///
/// # Errors
///
/// Returns an error when `from` is missing, when the two paths are on different
/// filesystems, or when the rename is denied.
#[cfg(feature = "fs")]
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    std::fs::rename(from, to)
}

/// Create `path` and every missing parent, like `mkdir -p`.
///
/// An existing directory is success. The created directories inherit the host's
/// ordinary permissions; a directory that must be private to this user goes
/// through [`create_dir_all_private`] instead.
///
/// # Errors
///
/// Returns an error when a path component exists as a file, when the path is
/// denied, or when the filesystem is full.
#[cfg(feature = "fs")]
pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

// ---------------------------------------------------------------------------
// Plain path inspection and directory/link mutation
// ---------------------------------------------------------------------------
//
// The second half of the ordinary-replacement surface. `write_file` /
// `remove_file` / `rename` / `create_dir_all` above cover the single-file
// mutations; these cover what a client still needs to stop reaching for
// `std::fs` while it walks, prunes or links a directory tree. As above, these
// are the plain operations: a caller that needs containment, privacy or
// atomicity uses the narrower helpers, which carry those invariants. Nothing
// here proves containment or repairs permissions.

/// Report `path`'s metadata, following a symbolic link at the final component.
///
/// This is the facade's ordinary, following `stat`: the answer describes the
/// object the path names after resolution. It deliberately does not report
/// containment, because the answer is about a path the caller already chose.
/// A caller that must know whether the object stays inside a root uses
/// [`context_path_metadata_no_follow`], which carries the containment answer
/// this call cannot.
///
/// # Errors
///
/// Returns an error when `path` does not exist, when a link in it is broken, or
/// when the metadata is denied.
#[cfg(feature = "fs")]
pub fn path_metadata(path: impl AsRef<Path>) -> io::Result<std::fs::Metadata> {
    std::fs::metadata(path)
}

/// Create the single directory `path`, like `mkdir`.
///
/// Unlike [`create_dir_all`], a missing parent is an error, and so is an
/// existing directory: the call reports the state the caller asked for rather
/// than accepting whatever was there. A directory that must be private to this
/// user goes through [`create_dir_all_private`] instead.
///
/// # Errors
///
/// Returns an error when a parent is missing, when `path` already exists, when
/// the creation is denied, or when the filesystem is full.
#[cfg(feature = "fs")]
pub fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::create_dir(path)
}

/// Remove the empty directory `path`.
///
/// Unlike [`remove_dir_all`], a directory with any entry left in it is an
/// error, which keeps a caller from silently deleting a tree it only meant to
/// prune.
///
/// # Errors
///
/// Returns an error when the directory does not exist, is not empty, names a
/// file, or when the removal is denied.
#[cfg(feature = "fs")]
pub fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::remove_dir(path)
}

/// Remove `path` and every entry beneath it.
///
/// The recursive call does not follow symbolic links inside the tree: a link is
/// removed as a link, never as the directory it names. Any other error while
/// descending is reported, so a partially removed tree surfaces as a failure
/// rather than a silent success.
///
/// # Errors
///
/// Returns an error when `path` does not exist, when it is not a directory, or
/// when an entry beneath it cannot be removed.
#[cfg(feature = "fs")]
pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::remove_dir_all(path)
}

/// Create `new` as a hard link to `existing`.
///
/// Both names then refer to one inode: the bytes are shared, the link count on
/// the source rises by one, and either name can be removed without affecting
/// the other. Because the object is shared, a caller that wants an independent
/// copy uses [`copy_file`] instead, and a caller that wants the sharing to be
/// optional falls back the same way when this call fails.
///
/// # Errors
///
/// Returns an error when `existing` is missing, when the two paths are on
/// different filesystems or volumes, when `new` already exists, or when the
/// host denies linking the object (some filesystems and network shares do).
#[cfg(feature = "fs")]
pub fn hard_link(existing: impl AsRef<Path>, new: impl AsRef<Path>) -> io::Result<()> {
    std::fs::hard_link(existing, new)
}

// ---------------------------------------------------------------------------
// Streaming regular-file handles
// ---------------------------------------------------------------------------
//
// The bounded reads above are the right shape for a file whose size the facade
// already knows: a lock file, a pointer, a small sidecar. They cannot stand in
// for a file whose size the *caller* chose. A 64 MiB convenience cap is a
// different operation, not a tighter spelling of the same one, so refusing or
// truncating a 200 MiB cache artifact is not a decision this facade may make on
// a client's behalf. What follows is the same operation the standard library
// offers -- open a stream and hand the handle back -- plus the facade ownership
// that lets a client stop calling `std::fs` directly. Every budget on top of
// these (how many bytes, how many files, how deep) belongs to the caller, and
// the doc comments say so rather than growing a second cap here. The whole-file
// read shapes are the `read_file` / `read_to_string` drop-ins below, so this
// block names no second spelling of them.

/// Open `path` for reading and hand back the streaming handle.
///
/// This is the facade's unbounded read path: unlike
/// [`read_context_regular_file_bounded`] it neither collects the bytes nor
/// enforces [`MAX_CONTEXT_REGULAR_FILE_BYTES`], so a caller hashing an input of
/// arbitrary size reads it in its own chunks. The facade owns the call because
/// "open this path for reading" is the one host operation every client
/// migration off `std::fs` needs and because the handle it returns is an
/// ordinary `File` the caller may also lock or stat.
///
/// A symbolic link at the final component is followed, exactly as
/// [`path_metadata`] follows it: the answer is about the object the caller
/// named, and containment questions go through
/// [`context_path_metadata_no_follow`] instead.
///
/// # Errors
///
/// Returns an error when `path` does not exist, when a link in it is broken,
/// when the caller lacks read access, or when the host denies the open.
#[cfg(feature = "fs")]
pub fn open_read_regular_file(path: impl AsRef<Path>) -> io::Result<File> {
    File::open(path)
}

/// Create `path`, or truncate it when it already exists, and return the write
/// handle.
///
/// Two neighbours are deliberately different operations. [`create_private_file`]
/// refuses an existing path, because it exists to prove the caller created it.
/// [`write_file`] takes the bytes up front, because most single-shot writes do
/// not need a handle. This is the third case: the caller writes in its own
/// chunks and accepts that it is replacing whatever was there.
///
/// The truncation is not atomic against a concurrent reader, and a crash leaves
/// a partial file; a publication that must be all-or-nothing writes a sibling
/// temporary and [`rename`]s it into place.
///
/// # Errors
///
/// Returns an error when a parent directory is missing, when `path` is a
/// directory, when the creation is denied, or when the filesystem is full.
#[cfg(feature = "fs")]
pub fn create_file(path: impl AsRef<Path>) -> io::Result<File> {
    File::create(path)
}

/// Open `path` for reading and writing, creating it when it is absent.
///
/// Truncation is deliberately *not* implied: a caller replacing the contents
/// uses [`create_file`], and a caller that seeks to chosen offsets and writes
/// there -- a journal, an index, a preallocated bundle -- uses this. The
/// equivalent standard-library spelling is
/// `OpenOptions::new().read(true).write(true).create(true)`; the facade owns it
/// so the shape is named once instead of being re-derived per client.
///
/// # Errors
///
/// Returns an error when a parent directory is missing, when `path` is a
/// directory, when the open is denied, or when the filesystem is full.
#[cfg(feature = "fs")]
pub fn open_read_write_file(path: impl AsRef<Path>) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Report the bytes `path` occupies on disk, in one call.
///
/// [`allocated_bytes`] answers the same question for a caller that already
/// holds a [`std::fs::Metadata`] -- a directory walk yields one per entry, and
/// re-statting then would double the syscalls. This form exists for the single
/// path: an accounting pass that asks about one file must not have to obtain a
/// metadata value through `std::fs` first just to hand it back.
///
/// # Errors
///
/// Returns an error when `path` does not exist, when a link in it is broken, or
/// when the metadata is denied.
#[cfg(feature = "fs")]
pub fn allocated_bytes_at(path: impl AsRef<Path>) -> io::Result<u64> {
    let path = path.as_ref();
    let metadata = std::fs::metadata(path)?;
    Ok(allocated_bytes(path, &metadata))
}

// ---------------------------------------------------------------------------
// Read-side drop-ins
// ---------------------------------------------------------------------------
//
// The facade already owns bounded and containable reads; these are the plain
// shapes clients reach for when they leave `std::fs`, so a migration is a
// rename rather than a rewrite of every call site. A caller that needs a byte
// ceiling or a containment answer uses `read_context_regular_file_bounded` /
// `context_path_metadata_no_follow` instead.

/// Read the whole file at `path`.
///
/// Unbounded: the caller owns the size decision. For anything that may be large
/// or attacker-influenced use [`read_context_regular_file_bounded`].
///
/// # Errors
///
/// Returns an error when the file is missing, when it is a directory, or when
/// the read is denied.
#[cfg(feature = "fs")]
pub fn read_file(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    std::fs::read(path)
}

/// Read the whole file at `path` as UTF-8.
///
/// # Errors
///
/// Returns an error when the file cannot be read or is not valid UTF-8.
#[cfg(feature = "fs")]
pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    std::fs::read_to_string(path)
}

/// Report `path`'s metadata without following a symbolic link at the final
/// component.
///
/// This is the drop-in for `std::fs::symlink_metadata`: it answers about the
/// link itself. [`context_path_metadata_no_follow`] is the facade's own
/// containment-carrying form.
///
/// # Errors
///
/// Returns an error when `path` is missing or the stat is denied.
#[cfg(feature = "fs")]
pub fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<std::fs::Metadata> {
    std::fs::symlink_metadata(path)
}

/// Copy `from` to `to`, returning the bytes copied.
///
/// Plain byte copy, no reflink attempt: a caller that wants copy-on-write uses
/// [`copy_file`] and inspects its [`CopyOutcome`].
///
/// # Errors
///
/// Returns an error for the ordinary reasons a copy fails.
#[cfg(feature = "fs")]
pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    std::fs::copy(from, to)
}

/// Set `path`'s permissions from an already-built [`std::fs::Permissions`].
///
/// The portable accessors are [`set_readonly`] and [`apply_metadata_mode`];
/// this exists for call sites that carry a `Permissions` value around.
///
/// # Errors
///
/// Returns an error when `path` is missing or the change is denied.
#[cfg(feature = "fs")]
pub fn set_permissions(
    path: impl AsRef<Path>,
    permissions: std::fs::Permissions,
) -> io::Result<()> {
    std::fs::set_permissions(path, permissions)
}

/// List `path`'s entries as paths.
///
/// `std::fs::read_dir` yields `DirEntry` values; this yields the paths, which
/// is what almost every caller reduces them to. A caller that needs the entry's
/// own file type uses [`DirectoryCursor`].
///
/// # Errors
///
/// Returns an error when `path` is missing, is not a directory, or the listing
/// is denied.
#[cfg(feature = "fs")]
pub fn read_dir_paths(path: impl AsRef<Path>) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(path)? {
        paths.push(entry?.path());
    }
    Ok(paths)
}

#[cfg(all(test, feature = "fs"))]
mod tests {
    use super::*;

    const PRODUCT: &str = "rp-fs-facade-test";

    /// A directory this facade created is not writable by other users.
    ///
    /// Asserted through `ensure_dir_private`'s own report rather than against
    /// mode bits or a DACL: the property is "already private", and each host
    /// decides what that means. A caller that has just created the directory
    /// must never be told it had to be repaired.
    #[test]
    fn a_freshly_created_private_directory_needs_no_tightening() {
        let root = tempfile::tempdir().expect("temp root");
        let nested = root.path().join("outer").join("inner");

        create_dir_all_private(&nested).expect("create private directory");

        assert!(nested.is_dir(), "the directory and its parents exist");
        assert!(
            !ensure_dir_private(&nested).expect("inspect a fresh private directory"),
            "a directory this facade just created is already private"
        );
    }

    /// Creating an existing private directory again is not an error.
    ///
    /// The operation composes like `create_dir_all`, so callers that always
    /// create before use do not have to distinguish the first run from later
    /// ones.
    #[test]
    fn creating_a_private_directory_twice_succeeds() {
        let root = tempfile::tempdir().expect("temp root");
        let path = root.path().join("twice");

        create_dir_all_private(&path).expect("first create");
        create_dir_all_private(&path).expect("second create");

        assert!(path.is_dir());
    }

    /// Tightening reports a missing directory rather than inventing one.
    #[test]
    fn ensuring_a_missing_directory_is_private_reports_not_found() {
        let root = tempfile::tempdir().expect("temp root");

        let error = ensure_dir_private(&root.path().join("absent")).expect_err("missing directory");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    /// Appending keeps what an earlier handle wrote, and the bytes land in order.
    ///
    /// The shared-append contract is about not truncating and not excluding a
    /// second opener; both handles here are live at once, which is the part
    /// Windows would refuse under its default share mode.
    #[test]
    fn shared_append_preserves_earlier_bytes_and_admits_a_second_writer() {
        use std::io::Write as _;

        let root = tempfile::tempdir().expect("temp root");
        let path = root.path().join("log");

        let mut first = open_shared_append(&path).expect("first append handle");
        first.write_all(b"one\n").expect("first write");
        let mut second = open_shared_append(&path).expect("second append handle");
        second.write_all(b"two\n").expect("second write");
        drop((first, second));

        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "one\ntwo\n"
        );
    }

    /// Every role resolves to an absolute directory that names the product.
    ///
    /// Asserted as a property rather than against one host's spelling: the
    /// point of the facade is that callers cannot tell which host answered.
    #[test]
    fn every_role_is_an_absolute_product_scoped_directory() {
        for directory in [
            user_runtime_dir(PRODUCT),
            user_state_dir(PRODUCT),
            user_run_data_root(PRODUCT),
        ] {
            assert!(
                directory.is_absolute(),
                "{} must be absolute",
                directory.display()
            );
            assert!(
                directory.to_string_lossy().contains(PRODUCT),
                "{} must be scoped to the product",
                directory.display()
            );
        }
    }

    /// Two products never share a directory in any role.
    #[test]
    fn distinct_products_do_not_collide() {
        let other = "rp-fs-facade-other";
        assert_ne!(user_runtime_dir(PRODUCT), user_runtime_dir(other));
        assert_ne!(user_state_dir(PRODUCT), user_state_dir(other));
        assert_ne!(user_run_data_root(PRODUCT), user_run_data_root(other));
    }

    /// A file is the same file as itself, by whichever pair this host uses.
    #[test]
    fn a_file_has_one_identity_through_both_a_handle_and_its_path() {
        let dir = std::env::temp_dir().join(format!("rp-fs-identity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("subject");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("create subject");

        let by_handle = file_identity(&file).expect("identity by handle");
        let by_path = path_identity(&path).expect("identity by path");
        assert_eq!(by_handle, by_path);

        drop(file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two distinct files never share an identity, which is the property a
    /// caller relies on to notice its file was replaced underneath it.
    #[test]
    fn distinct_files_have_distinct_identities() {
        let dir = std::env::temp_dir().join(format!("rp-fs-identity2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let (first, second) = (dir.join("first"), dir.join("second"));
        std::fs::write(&first, b"a").expect("write first");
        std::fs::write(&second, b"b").expect("write second");

        let a = path_identity(&first).expect("identity a");
        let b = path_identity(&second).expect("identity b");
        if a.is_some() {
            assert_ne!(a, b);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An exclusive lock excludes a second holder, and releasing readmits one.
    ///
    /// Both handles are opened through the facade, so this exercises the open
    /// and the lock together -- on Windows the two interact, because a
    /// restrictive share mode would fail the second open before it could ask
    /// for the lock.
    #[test]
    fn an_exclusive_lock_excludes_a_second_holder_until_released() {
        let dir = std::env::temp_dir().join(format!("rp-fs-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("guard.lock");

        let first = open_lock_file(&path).expect("open first");
        let second = open_lock_file(&path).expect("open second");

        let first_lock = try_lock_exclusive(&first).expect("first acquires");
        let conflict = try_lock_exclusive(&second).expect_err("second must be refused");
        assert!(
            is_lock_conflict(&conflict),
            "refusal must classify as a conflict, got {conflict:?}"
        );

        // Dropping the guard is the only release mechanism this facade
        // exposes; the second holder succeeding proves the drop actually
        // released the lock, not just that the borrow ended.
        drop(first_lock);
        let second_lock = try_lock_exclusive(&second).expect("second acquires after release");
        drop(second_lock);

        drop((first, second));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A held lock obstructs no one who did not ask for a lock.
    ///
    /// This is the facade's advisory promise stated as a test, and it is the
    /// pidfile pattern [`open_lock_file`] exists for: a holder writes who it
    /// is, and anyone else reads that without participating in the locking.
    /// Deliberately not host-gated -- the claim is that every host answers
    /// the same way, so every host has to run it.
    #[test]
    fn a_lock_does_not_obstruct_a_process_that_never_locked() {
        use std::io::{Read as _, Write as _};

        let dir = std::env::temp_dir().join(format!("rp-fs-advisory-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("holder.lock");
        std::fs::write(&path, b"00000").expect("seed contents");

        let held = open_lock_file(&path).expect("open holder");

        // An exclusive holder must not stop a plain reader.
        let exclusive =
            lock_exclusive_for(&held, Duration::from_secs(1)).expect("holder takes it exclusively");
        let mut contents = Vec::new();
        std::fs::File::open(&path)
            .expect("a non-participant can open it")
            .read_to_end(&mut contents)
            .expect("a non-participant can read it");
        assert_eq!(contents, b"00000");
        drop(exclusive);

        // A shared holder must not stop a plain writer either. The write
        // does not truncate: a lock file's contents belong to whoever wrote
        // them, and replacing five bytes with five keeps this about the lock
        // rather than about file length.
        let shared =
            lock_shared_for(&held, Duration::from_secs(1)).expect("holder takes it shared");
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(false)
            .open(&path)
            .expect("a non-participant can open it for writing")
            .write_all(b"11111")
            .expect("a non-participant can write it");
        drop(shared);

        assert_eq!(std::fs::read(&path).expect("read back"), b"11111");

        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Asking one handle to upgrade its own lock is refused, not granted.
    ///
    /// Windows-only because it is the host whose answer differs: `flock`
    /// converts in place, while `LockFileEx` will not take an exclusive lock
    /// overlapping a range the same handle already holds. Written against
    /// the *try* form on purpose -- [`lock_exclusive`] would wait for a
    /// release only this caller could perform, which is a hung CI lane
    /// rather than a failing test.
    #[cfg(windows)]
    #[test]
    fn one_handle_cannot_upgrade_its_own_shared_lock() {
        let dir = std::env::temp_dir().join(format!("rp-fs-upgrade-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("upgrade.lock");

        let file = open_lock_file(&path).expect("open");
        let shared = lock_shared_for(&file, Duration::from_secs(1)).expect("take it shared");
        let conflict = try_lock_exclusive(&file).expect_err("an upgrade must be refused");
        assert!(
            is_lock_conflict(&conflict),
            "refusal must classify as a conflict, got {conflict:?}"
        );
        drop(shared);

        drop(file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A genuine failure is not reported as a conflict, so a caller does not
    /// retry forever on something waiting cannot fix.
    #[test]
    fn an_unrelated_error_is_not_a_lock_conflict() {
        let missing = std::env::temp_dir().join("rp-fs-lock-no-such-file");
        let _ = std::fs::remove_file(&missing);
        let error = std::fs::File::open(&missing).expect_err("must not exist");
        assert!(!is_lock_conflict(&error));
    }

    /// The pair round-trips, which is the only contract a wire encoding owes
    /// its decoder: the far end must reconstruct exactly the path that was
    /// named, not an equivalent one.
    #[test]
    fn a_path_survives_encoding_and_decoding_unchanged() {
        for original in [
            std::path::PathBuf::from("relative/leaf.log"),
            std::env::temp_dir()
                .join("rp path with spaces")
                .join("t.log"),
            std::env::current_exe().expect("current image"),
        ] {
            let decoded =
                decode_path_bytes(&encode_path_bytes(&original)).expect("decode what we encoded");
            assert_eq!(decoded, original);
        }
    }

    /// An empty path is a path, and must not become an error or a surprise.
    #[test]
    fn an_empty_path_round_trips_as_empty() {
        let empty = std::path::PathBuf::new();
        assert!(encode_path_bytes(&empty).is_empty());
        assert_eq!(
            decode_path_bytes(&encode_path_bytes(&empty)).expect("decode empty"),
            empty
        );
    }

    /// Replacing works whether or not the target already exists.
    ///
    /// Both cases matter, and they must be the same case: the no-target one
    /// is what a first write takes, and a host that reached it by a different
    /// call would owe that call's durability separately.
    #[test]
    fn a_file_is_replaced_whether_or_not_the_target_exists() {
        let dir = std::env::temp_dir().join(format!("rp-fs-replace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let target = dir.join("manifest");

        let first = dir.join("first.tmp");
        std::fs::write(&first, b"first").expect("write first");
        replace_file(&first, &target).expect("replace absent target");
        assert_eq!(std::fs::read(&target).expect("read"), b"first");

        let second = dir.join("second.tmp");
        std::fs::write(&second, b"second").expect("write second");
        replace_file(&second, &target).expect("replace existing target");
        assert_eq!(std::fs::read(&target).expect("read"), b"second");

        // The replaced-from paths are consumed by the move, not left behind.
        assert!(!first.exists());
        assert!(!second.exists());

        sync_directory(&dir).expect("sync the directory that records it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A first write and an overwrite are the same move, not two mechanisms.
    ///
    /// Windows-only because it is the host that had two: a create branch that
    /// took a plain rename and got no write-through, and a replace branch
    /// that did. The durability itself cannot be asserted from a test -- it
    /// only shows up across an unclean shutdown -- so this pins the property
    /// that stands in for it: both paths move the temporary file's own record
    /// into place, which a copy-and-replace would not do, so whatever the one
    /// path guarantees the other guarantees too.
    #[cfg(windows)]
    #[test]
    fn a_first_write_and_an_overwrite_move_the_same_way() {
        let dir = std::env::temp_dir().join(format!("rp-fs-replace-win-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let target = dir.join("manifest");

        for contents in [&b"first"[..], &b"second"[..]] {
            let tmp = dir.join("staged.tmp");
            std::fs::write(&tmp, contents).expect("stage");
            let staged = path_identity(&tmp).expect("identity of the staged file");

            replace_file(&tmp, &target).expect("replace");

            assert_eq!(std::fs::read(&target).expect("read"), contents);
            assert!(!tmp.exists(), "the staged path is consumed by the move");
            if staged.is_some() {
                assert_eq!(
                    path_identity(&target).expect("identity of the target"),
                    staged,
                    "the target must be the staged file itself, moved"
                );
            }
        }

        sync_directory(&dir).expect("sync the directory that records it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory that does not exist is not something to report as synced.
    ///
    /// Ungated: the answer must not depend on the host. Windows has nothing
    /// to flush, but "nothing to flush" is not the same as "yes", and a
    /// caller leaning on this as a cheap assertion gets the same answer
    /// everywhere.
    #[test]
    fn syncing_a_directory_that_does_not_exist_is_an_error() {
        let missing = std::env::temp_dir()
            .join(format!("rp-fs-no-such-dir-{}", std::process::id()))
            .join("nested");
        let _ = std::fs::remove_dir_all(&missing);
        sync_directory(&missing).expect_err("a missing directory cannot be synced");
    }

    /// Shared data and machine-local state are different roles, and a host
    /// that distinguishes them must not collapse the two.
    #[test]
    fn shared_data_is_its_own_role() {
        let data = user_data_dir(PRODUCT);
        assert!(data.is_absolute());
        assert!(data.to_string_lossy().contains(PRODUCT));
    }

    /// A private file is created, and refuses to open over an existing one.
    ///
    /// The refusal is the security-relevant half: opening over a file someone
    /// else made would inherit their permissions, so it must fail rather than
    /// succeed with weaker protection than the caller asked for.
    #[test]
    fn a_private_file_is_created_once_and_refuses_to_reopen() {
        let dir = std::env::temp_dir().join(format!("rp-fs-private-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("artifact.json");
        let _ = std::fs::remove_file(&path);

        {
            let mut file = create_private_file(&path).expect("create private file");
            use std::io::Write as _;
            file.write_all(b"payload").expect("write");
        }
        assert_eq!(std::fs::read(&path).expect("read back"), b"payload");

        let second = create_private_file(&path).expect_err("must not open over an existing file");
        assert_eq!(second.kind(), std::io::ErrorKind::AlreadyExists);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The roles are stable: asking twice gives the same answer, so a path
    /// derived at startup still names the same directory later.
    #[test]
    fn roles_are_stable_across_calls() {
        assert_eq!(user_runtime_dir(PRODUCT), user_runtime_dir(PRODUCT));
        assert_eq!(user_state_dir(PRODUCT), user_state_dir(PRODUCT));
        assert_eq!(user_run_data_root(PRODUCT), user_run_data_root(PRODUCT));
    }

    /// Any number of shared holders may coexist, but an exclusive request is
    /// refused while one is outstanding, and admitted once every shared
    /// holder has dropped.
    #[test]
    fn shared_locks_coexist_but_exclude_an_exclusive_request() {
        let dir = std::env::temp_dir().join(format!("rp-fs-lock-shared-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("guard.lock");

        let first = open_lock_file(&path).expect("open first");
        let second = open_lock_file(&path).expect("open second");
        let third = open_lock_file(&path).expect("open third");

        let first_shared = try_lock_shared(&first).expect("first shared holder admitted");
        let second_shared = try_lock_shared(&second).expect("second shared holder admitted");

        let conflict =
            try_lock_exclusive(&third).expect_err("exclusive must be refused while shared holds");
        assert!(is_lock_conflict(&conflict));

        drop(first_shared);
        let conflict = try_lock_exclusive(&third)
            .expect_err("exclusive must still be refused with one shared holder left");
        assert!(is_lock_conflict(&conflict));

        drop(second_shared);
        let exclusive =
            try_lock_exclusive(&third).expect("exclusive admitted once shared holders release");
        drop(exclusive);

        drop((first, second, third));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The blocking variants eventually acquire the lock a concurrent holder
    /// releases, rather than failing immediately the way the `try_` variants
    /// do -- this is the property that distinguishes the two families.
    #[test]
    // Exercises the deprecated unbounded form until it is removed (#433).
    #[allow(deprecated)]
    fn blocking_lock_acquires_once_the_holder_releases() {
        let dir = std::env::temp_dir().join(format!("rp-fs-lock-blocking-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("guard.lock");

        let first = open_lock_file(&path).expect("open first");
        let second = open_lock_file(&path).expect("open second");

        let held = try_lock_exclusive(&first).expect("first acquires");
        drop(held);

        // With no concurrent holder left, the blocking call must return
        // immediately rather than actually block this test.
        let acquired = lock_exclusive(&second).expect("blocking lock acquires");
        drop(acquired);

        drop((first, second));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A modification time survives the round trip from Unix seconds through
    /// [`set_file_mtime`] and back out through
    /// [`FileTime::from_last_modification_time`].
    #[test]
    fn a_unix_time_survives_the_round_trip_through_a_file() {
        let dir = std::env::temp_dir().join(format!("rp-fs-mtime-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("stamped");
        std::fs::write(&path, b"payload").expect("write");

        // A time with a sub-second remainder, comfortably before this test
        // ever runs, so no host's mtime resolution rounds it away.
        let stamped = FileTime::from_unix_time(1_700_000_000, 500_000_000);
        set_file_mtime(&path, stamped).expect("set mtime");

        let metadata = std::fs::metadata(&path).expect("read metadata back");
        let read_back = FileTime::from_last_modification_time(&metadata);
        assert_eq!(read_back, stamped);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory takes a modification time the same way a file does.
    ///
    /// Windows needs `FILE_FLAG_BACKUP_SEMANTICS` to open a directory handle
    /// at all, so without it this is `ERROR_ACCESS_DENIED` there while both
    /// Unix hosts pass. A client restoring a stored tree puts directory
    /// mtimes back, so the gap only shows up off Linux.
    #[test]
    fn a_unix_time_survives_the_round_trip_through_a_directory() {
        let dir = std::env::temp_dir().join(format!("rp-fs-dir-mtime-{}", std::process::id()));
        let stamped_dir = dir.join("nested");
        std::fs::create_dir_all(&stamped_dir).expect("create dir");

        let stamped = FileTime::from_unix_time(1_600_000_000, 250_000_000);
        set_file_mtime(&stamped_dir, stamped).expect("set directory mtime");

        let metadata = std::fs::metadata(&stamped_dir).expect("read metadata back");
        assert_eq!(FileTime::from_last_modification_time(&metadata), stamped);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// [`FileTime::from_system_time`] and [`FileTime::from_last_modification_time`]
    /// agree, since a file's metadata is read through [`SystemTime`] itself.
    #[test]
    fn from_system_time_and_from_metadata_agree() {
        let dir = std::env::temp_dir().join(format!("rp-fs-mtime-agree-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("stamped");
        std::fs::write(&path, b"payload").expect("write");

        let metadata = std::fs::metadata(&path).expect("read metadata");
        let modified = metadata.modified().expect("host supports mtime");

        assert_eq!(
            FileTime::from_last_modification_time(&metadata),
            FileTime::from_system_time(modified)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A time before the Unix epoch round-trips through [`SystemTime`]
    /// without silently landing on the wrong side of it.
    #[test]
    fn a_time_before_the_unix_epoch_round_trips() {
        // `epoch - 3600.25s` is `-3601` whole seconds plus a forward `0.75s`
        // remainder: nanoseconds always advance from the second mark, they
        // never subtract from it, so the second count floors.
        let before_epoch = SystemTime::UNIX_EPOCH - std::time::Duration::new(3600, 250_000_000);
        let converted = FileTime::from_system_time(before_epoch);
        assert_eq!(converted, FileTime::from_unix_time(-3601, 750_000_000));
    }

    /// `now()` reports a time in the current era, not a zeroed or default
    /// value -- a facade that silently returned the epoch would be a much
    /// harder bug to notice than one that fails loudly.
    #[test]
    fn now_reports_a_recent_time() {
        let now = FileTime::now();
        // 2020-01-01T00:00:00Z. Anything before this is not "now" by any
        // clock this crate supports.
        assert!(now > FileTime::from_unix_time(1_577_836_800, 0));
    }

    /// [`copy_file`] produces a byte-identical destination whether the
    /// filesystem gave it a reflink or fell back to a copy, and reports which
    /// one happened.
    #[test]
    fn copy_file_reproduces_the_source_and_reports_its_strategy() {
        let dir = std::env::temp_dir().join(format!("rp-fs-copy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let source = dir.join("source");
        let destination = dir.join("destination");
        let content = b"reflink or copy, the bytes must match";
        std::fs::write(&source, content).expect("write source");

        let outcome = copy_file(&source, &destination).expect("copy succeeds");
        assert_eq!(
            std::fs::read(&destination).expect("read destination"),
            content
        );
        match outcome {
            CopyOutcome::Reflinked => {}
            CopyOutcome::Copied { bytes } => assert_eq!(bytes, content.len() as u64),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// [`reflink_file`] must not silently substitute a byte copy.
    ///
    /// This is the property the probe callers depend on: they ask "can this
    /// volume reflink?" by attempting one, so an implementation that fell
    /// back would answer yes everywhere. The test cannot assume the temp
    /// filesystem supports reflinks, so it asserts the honest disjunction --
    /// either the reflink worked and the bytes match, or it reported failure
    /// and left no destination behind. What must never happen is a reported
    /// success that was actually a copy, which is exactly what
    /// [`copy_file`] is allowed to do and this is not.
    #[test]
    fn reflink_file_either_reflinks_or_reports_that_it_could_not() {
        let dir = std::env::temp_dir().join(format!("rp-fs-reflink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let source = dir.join("source");
        let destination = dir.join("destination");
        let content = b"a reflink is not a copy";
        std::fs::write(&source, content).expect("write source");

        match reflink_file(&source, &destination) {
            Ok(()) => assert_eq!(
                std::fs::read(&destination).expect("read destination"),
                content,
                "a successful reflink must expose the source's bytes"
            ),
            Err(_) => assert!(
                !destination.exists(),
                "a failed reflink must not leave a destination behind"
            ),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An owned lock excludes a second holder, and releasing it lets that
    /// holder in -- whether the release is a drop or an explicit `unlock`.
    ///
    /// This is the property the borrowed guard cannot provide to a caller
    /// that must store the lock or return it, so it is checked the same way:
    /// take the lock, prove a second handle is refused, release, prove it is
    /// then accepted.
    #[test]
    fn an_owned_lock_excludes_a_second_holder_until_released() {
        let dir = std::env::temp_dir().join(format!("rp-fs-owned-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("lockfile");
        std::fs::write(&path, b"").expect("create lock file");

        let open = || std::fs::File::open(&path).expect("open lock file");

        // Dropping the guard releases.
        {
            let held = lock_exclusive_owned_for(open(), Duration::from_secs(1))
                .expect("first holder takes the lock");
            let conflict =
                try_lock_exclusive(&open()).expect_err("a second holder must be refused");
            assert!(is_lock_conflict(&conflict));
            drop(held);
        }
        drop(try_lock_exclusive(&open()).expect("the lock is free once the owner drops"));

        // `unlock` releases at a chosen point and hands the handle back still
        // open, which is why it exists rather than only `drop`.
        let held =
            lock_exclusive_owned_for(open(), Duration::from_secs(1)).expect("retake the lock");
        assert!(is_lock_conflict(
            &try_lock_exclusive(&open()).expect_err("still exclusive")
        ));
        let returned = held.unlock().expect("unlock returns the handle");
        assert!(
            returned.metadata().is_ok(),
            "the handle must still be usable after unlocking"
        );
        drop(try_lock_exclusive(&open()).expect("the lock is free after unlock"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty set reports itself empty and matches nothing, and a set
    /// accepts any path-like argument.
    ///
    /// Both matter to a caller that filters a walk: it asks `is_empty` once
    /// to skip matching entirely, and matches against whatever string or
    /// path it already holds rather than converting at every candidate.
    #[test]
    fn an_empty_pattern_set_is_empty_and_matches_nothing() {
        let empty = PatternSetBuilder::new().build().expect("empty set builds");
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert!(!empty.is_match("anything"));
        assert!(!empty.is_match(Path::new("anything")));

        let populated = PatternSetBuilder::new()
            .add_pattern("*.rs")
            .add_pattern("src/**")
            .build()
            .expect("set builds");
        assert!(!populated.is_empty());
        assert_eq!(populated.len(), 2);
        // The same candidate as a `&str`, a `String` and a `&Path`.
        assert!(populated.is_match("main.rs"));
        assert!(populated.is_match(String::from("main.rs")));
        assert!(populated.is_match(Path::new("main.rs")));
        assert!(!populated.is_match("main.txt"));
    }

    /// A persisted mtime survives the round trip through its two fields.
    ///
    /// This is the property a client that stores mtimes depends on, and it
    /// is deliberately checked without `SystemTime` in the middle: the whole
    /// reason `FileTime` carries the raw pair is that `SystemTime` cannot
    /// represent every value a filesystem can hold, so a round trip through
    /// it would not prove this.
    #[test]
    fn a_file_time_round_trips_through_its_accessors() {
        for (seconds, nanoseconds) in [
            (0_i64, 0_u32),
            (1_700_000_000, 123_456_789),
            // Before 1970: the seconds go negative while the remainder stays
            // a positive count forward, which is the encoding that is easy
            // to get wrong when reading a value back.
            (-1, 999_999_999),
            (-86_400, 1),
        ] {
            let time = FileTime::from_unix_time(seconds, nanoseconds);
            assert_eq!(time.unix_seconds(), seconds);
            assert_eq!(time.nanoseconds(), nanoseconds);
            assert_eq!(
                FileTime::from_unix_time(time.unix_seconds(), time.nanoseconds()),
                time,
                "reconstructing from the accessors must yield the same value"
            );
        }
    }

    /// The mtime read back off a file is the one that was written, field for
    /// field -- not merely equal as a `SystemTime`.
    #[test]
    fn a_written_mtime_reads_back_through_the_accessors() {
        let dir = std::env::temp_dir().join(format!("rp-fs-mtime-acc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("stamped");
        std::fs::write(&path, b"x").expect("write file");

        let written = FileTime::from_unix_time(1_600_000_000, 500_000_000);
        set_file_mtime(&path, written).expect("set mtime");
        let metadata = std::fs::metadata(&path).expect("stat");
        let read_back = FileTime::from_last_modification_time(&metadata);

        assert_eq!(read_back.unix_seconds(), written.unix_seconds());
        assert_eq!(read_back.nanoseconds(), written.nanoseconds());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A walk finds every file under its root, and a pruned directory's
    /// contents never appear at all.
    #[test]
    fn a_walk_finds_files_and_honours_pruning() {
        let dir = std::env::temp_dir().join(format!("rp-fs-walk-{}", std::process::id()));
        let pruned = dir.join("pruned");
        let kept = dir.join("kept");
        std::fs::create_dir_all(&pruned).expect("create pruned dir");
        std::fs::create_dir_all(&kept).expect("create kept dir");
        std::fs::write(pruned.join("secret"), b"never seen").expect("write pruned file");
        std::fs::write(kept.join("visible"), b"seen").expect("write kept file");
        std::fs::write(dir.join("root_file"), b"seen too").expect("write root file");

        let entries: Vec<DirectoryEntry> = DirectoryWalk::new(dir.clone())
            .prune_directories(|path| {
                path.file_name().and_then(|name| name.to_str()) != Some("pruned")
            })
            .walk()
            .collect::<io::Result<Vec<_>>>()
            .expect("walk succeeds");

        let file_paths: Vec<&Path> = entries
            .iter()
            .filter(|entry| entry.is_file())
            .map(DirectoryEntry::path)
            .collect();
        assert!(file_paths.contains(&kept.join("visible").as_path()));
        assert!(file_paths.contains(&dir.join("root_file").as_path()));
        assert!(
            !file_paths.contains(&pruned.join("secret").as_path()),
            "a pruned directory's contents must never be yielded"
        );
        assert!(
            entries.iter().all(|entry| entry.path() != pruned.as_path()),
            "a pruned directory itself must not be yielded either"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A set of glob patterns matches any path that satisfies at least one of
    /// them, and rejects a path that satisfies none.
    #[test]
    fn a_pattern_set_matches_any_included_pattern() {
        let set = PatternSetBuilder::new()
            .add_pattern("*.rs")
            .add_pattern("Cargo.toml")
            .build()
            .expect("valid patterns compile");

        assert!(set.is_match(Path::new("src/lib.rs")));
        assert!(set.is_match(Path::new("Cargo.toml")));
        assert!(!set.is_match(Path::new("README.md")));
    }

    /// An invalid glob pattern is reported at build time, not accepted and
    /// silently never matched.
    #[test]
    fn an_invalid_pattern_is_rejected_at_build() {
        let error = PatternSetBuilder::new()
            .add_pattern("[unterminated")
            .build()
            .expect_err("malformed glob syntax must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    /// The plain mutation accessors round-trip the bytes they were given, so a
    /// client can replace `std::fs` with the facade without a behavior change.
    #[test]
    fn plain_mutation_accessors_round_trip() {
        let root = tempfile::tempdir().expect("temp root");
        let nested = root.path().join("a").join("b");
        create_dir_all(&nested).expect("create nested directories");
        assert!(nested.is_dir(), "create_dir_all makes every parent");

        let first = nested.join("first.txt");
        write_file(&first, b"one").expect("write");
        assert_eq!(std::fs::read(&first).expect("read back"), b"one");

        write_file(&first, b"two").expect("overwrite");
        assert_eq!(std::fs::read(&first).expect("read back"), b"two");

        let second = nested.join("second.txt");
        rename(&first, &second).expect("rename publishes the file");
        assert!(!first.exists(), "the old name is gone after a rename");
        assert_eq!(std::fs::read(&second).expect("read back"), b"two");

        remove_file(&second).expect("remove");
        assert!(!second.exists(), "the file is gone");
        assert_eq!(
            remove_file(&second)
                .expect_err("removing a missing file is an error")
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    /// The path and tree accessors answer over a real directory tree, so a
    /// client can replace `std::fs` for inspection, pruning and linking
    /// without a behavior change.
    #[test]
    fn path_and_tree_accessors_round_trip() {
        let root = tempfile::tempdir().expect("temp root");
        let outer = root.path().join("outer");

        create_dir(&outer).expect("create one directory");
        assert!(outer.is_dir(), "create_dir makes the named directory");
        assert!(
            create_dir(&outer).is_err(),
            "an existing directory is an error, not a silent success"
        );
        assert!(
            create_dir(root.path().join("absent").join("child")).is_err(),
            "create_dir does not create parents"
        );

        let original = outer.join("payload.txt");
        write_file(&original, b"payload").expect("write");
        let metadata = path_metadata(&original).expect("following stat");
        assert_eq!(metadata.len(), 7, "the stat describes the written bytes");
        assert!(metadata.is_file(), "the stat describes a regular file");
        assert_eq!(
            path_metadata(root.path().join("missing.txt"))
                .expect_err("a missing path is an error")
                .kind(),
            io::ErrorKind::NotFound
        );

        let linked = outer.join("linked.txt");
        hard_link(&original, &linked).expect("hard link");
        remove_file(&original).expect("remove the original name");
        assert_eq!(
            std::fs::read(&linked).expect("read through the surviving name"),
            b"payload",
            "a hard link keeps the bytes alive under either name"
        );

        assert!(
            remove_dir(&outer).is_err(),
            "a directory with entries is not removable by remove_dir"
        );
        remove_dir_all(&outer).expect("remove the tree");
        assert!(!outer.exists(), "the whole tree is gone");

        create_dir(&outer).expect("recreate an empty directory");
        remove_dir(&outer).expect("remove the empty directory");
        assert!(!outer.exists(), "the empty directory is gone");
    }

    /// `path_metadata` resolves the final component, unlike the no-follow
    /// containment stat, so a client inspecting a linked artifact sees the
    /// object rather than the link.
    #[test]
    fn path_metadata_follows_a_final_symlink() {
        let root = tempfile::tempdir().expect("temp root");
        let target = root.path().join("target.txt");
        let link = root.path().join("link.txt");
        write_file(&target, b"resolved").expect("write target");
        if symlink_file(&target, &link).is_err() {
            // A host that will not create the link (Windows without Developer
            // Mode or the symlink privilege) cannot answer the question here;
            // the plain stat above still covers the accessor's contract.
            return;
        }

        let metadata = path_metadata(&link).expect("following stat");
        assert!(
            metadata.is_file(),
            "the following stat describes the target"
        );
        assert_eq!(metadata.len(), 8, "the following stat sizes the target");
    }

    /// A write that cannot land reports the filesystem's own error rather than
    /// succeeding quietly, so a caller can distinguish it from a success.
    #[test]
    fn write_file_reports_a_missing_parent() {
        let root = tempfile::tempdir().expect("temp root");
        let orphan = root.path().join("no-such-directory").join("file.txt");

        assert_eq!(
            write_file(&orphan, b"x")
                .expect_err("writing under a missing parent is an error")
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    /// The handle forms exist so a caller can stream a file whose size the
    /// facade never learns. Round-tripping through `create_file` and the
    /// mainline `read_file` / `read_to_string` drop-ins proves the handle pair
    /// is the same object: what the create handle writes is what the read
    /// helpers return, and a second create truncates rather than appending.
    #[test]
    fn a_created_file_handle_round_trips_and_truncates() {
        let root = tempfile::tempdir().expect("temp root");
        let path = root.path().join("streamed.bin");

        {
            let mut file = create_file(&path).expect("create handle");
            std::io::Write::write_all(&mut file, b"first generation").expect("write first");
        }
        assert_eq!(read_file(&path).expect("read back"), b"first generation");
        assert_eq!(
            read_to_string(&path).expect("read back as text"),
            "first generation"
        );

        {
            let mut file = create_file(&path).expect("second create handle");
            std::io::Write::write_all(&mut file, b"second").expect("write second");
        }
        assert_eq!(
            read_file(&path).expect("read truncating write"),
            b"second",
            "creating over an existing path truncates it"
        );
    }

    /// A read/write handle creates a missing file and does not shorten one that
    /// already has contents, which is what separates it from `create_file`.
    #[test]
    fn a_read_write_handle_creates_without_truncating() {
        let root = tempfile::tempdir().expect("temp root");
        let path = root.path().join("journal.bin");

        let mut first = open_read_write_file(&path).expect("create read/write handle");
        std::io::Write::write_all(&mut first, b"0000").expect("seed contents");
        drop(first);

        let mut second = open_read_write_file(&path).expect("reopen read/write handle");
        let mut observed = [0u8; 4];
        std::io::Read::read_exact(&mut second, &mut observed).expect("read existing contents");
        assert_eq!(
            &observed, b"0000",
            "reopening a read/write handle must not truncate"
        );
    }

    /// A missing path is an error on the streaming open, not a silently empty
    /// read: a cache that cannot open its artifact must fail the lookup rather
    /// than serve a zero-length result. The unbounded read is not a cap the
    /// facade will quietly apply either -- a file larger than the context cap
    /// reads whole.
    #[test]
    fn the_streaming_open_reports_a_missing_path_and_reads_past_the_cap() {
        let root = tempfile::tempdir().expect("temp root");
        let missing = root.path().join("absent.bin");

        assert_eq!(
            open_read_regular_file(&missing)
                .expect_err("opening an absent file is an error")
                .kind(),
            io::ErrorKind::NotFound
        );

        let large = root.path().join("large.bin");
        let payload = vec![b'z'; MAX_CONTEXT_REGULAR_FILE_BYTES + 1];
        write_file(&large, &payload).expect("write over the context cap");
        assert_eq!(
            read_file(&large)
                .expect("read whole file above the context cap")
                .len(),
            MAX_CONTEXT_REGULAR_FILE_BYTES + 1,
            "the whole-file read is not capped by the context limit"
        );
    }

    /// The single-path allocation form agrees with the metadata-holding form for
    /// the same file, so an accounting pass does not change its answer by
    /// switching between them.
    #[test]
    fn the_single_path_allocation_agrees_with_the_metadata_form() {
        let root = tempfile::tempdir().expect("temp root");
        let path = root.path().join("accounted.bin");
        write_file(&path, vec![b'q'; 8192]).expect("write accounted file");

        let metadata = path_metadata(&path).expect("path metadata");
        assert_eq!(
            allocated_bytes_at(&path).expect("allocated bytes at path"),
            allocated_bytes(&path, &metadata)
        );
    }
}

/// Reject a held file with an extended ACL, or return `Unsupported` when the
/// host cannot inspect it. This checks ACLs only; callers must separately
/// verify ownership, mode, ancestry, and identity of their held handles.
/// ACL absence is a point-in-time observation; callers must control later
/// security metadata changes while relying on it.
#[cfg(feature = "fs")]
pub fn ensure_no_extended_acl(file: &std::fs::File) -> std::io::Result<()> {
    crate::fs_ensure_no_extended_acl(file)
}
