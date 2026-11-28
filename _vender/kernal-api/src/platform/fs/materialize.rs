//! Link classification, permission bits, change markers, volume facts, and
//! native path spellings for callers that materialize cached artifacts.
//!
//! These are path-based compatibility operations: they follow links unless
//! stated otherwise, retain no handles, and are not secure-open or ACL
//! isolation primitives. The caller controls the paths involved.

use std::io;
use std::path::{Path, PathBuf};

use crate::native_fs_materialize as native;

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

/// A no-follow classification of one directory entry, without native
/// attribute bits.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LinkKind {
    /// An ordinary file or directory.
    Regular,
    /// A symbolic link: on Windows, a name-surrogate symlink reparse point.
    Symlink,
    /// A reparse point that is not a symbolic link (a Windows junction,
    /// mount point, cloud placeholder, ...). Callers must not traverse it as
    /// a regular directory entry. Never reported on Linux or macOS.
    Reparse,
}

/// Classify the entry at `path` without following its final component.
///
/// On Windows a reparse point whose tag cannot be read classifies as
/// [`LinkKind::Reparse`], never as a symlink.
///
/// # Errors
///
/// Returns the no-follow metadata error, including `NotFound` for a missing
/// entry. A dangling symbolic link is still classified.
pub fn classify(path: &Path) -> io::Result<LinkKind> {
    native::classify(path)
}

/// Number of hard links to the object `path` currently names.
///
/// Follows symbolic links, so a dangling link reports `NotFound`. On Windows
/// the handle is opened metadata-only with every share mode, so observing a
/// count never evicts an existing writer.
///
/// # Errors
///
/// Returns the metadata or native query error.
pub fn hard_link_count(path: &Path) -> io::Result<u64> {
    native::hard_link_count(path)
}

/// Result of [`await_no_writers`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriterWait {
    /// No process held the file open for writing when last observed.
    /// `waited` is how long other writers kept it open (zero when none did).
    Clear {
        /// Time spent waiting for other writers to close.
        waited: std::time::Duration,
    },
    /// Some process still held the file open for writing at the deadline.
    TimedOut,
    /// This host or file system cannot report writers. Hosts that execute a
    /// file regardless of writers report this without waiting.
    Unobservable,
}

/// Wait up to `timeout` until no process (this one or any other) holds the
/// file at `path` open for writing.
///
/// Publishing an executable is only safe to `exec` once every write
/// descriptor on its inode is closed. A child forked while the publisher's
/// write descriptor was open inherits that descriptor until its own `exec`,
/// so closing the publisher's copy is not enough: Linux refuses to execute
/// the file (`ETXTBSY`) for the child's fork-to-exec window. Call this after
/// closing the file and before handing it to anything that will execute it.
/// The file is opened read-only and no handle is retained.
///
/// # Errors
///
/// Returns the error from opening `path`.
pub fn await_no_writers(path: &Path, timeout: std::time::Duration) -> io::Result<WriterWait> {
    native::await_no_writers(path, timeout)
}

/// Whether a file's storage blocks are shared with another file.
///
/// Reflinks (Linux `FICLONE`, macOS `clonefile`, Windows ReFS block
/// cloning) and snapshots let two files share blocks while each has one
/// hard link, so a link count alone cannot prove that deleting a file frees
/// its space.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ExtentSharing {
    /// No block of the file is shared: removing its last hard link frees
    /// its allocation. Also reported on volumes that cannot share blocks at
    /// all (ext4, tmpfs, HFS+, FAT, NTFS).
    Exclusive,
    /// At least one block is shared with another file or snapshot.
    Shared,
    /// The volume can share blocks but this host cannot tell whether this
    /// file does (APFS, ReFS, and Linux volumes without `FIEMAP`). Callers
    /// that need proof must treat this as possibly shared.
    Unknown,
}

/// Report whether the regular file at `path` shares storage blocks with
/// another file. Follows symbolic links and retains no handle.
///
/// Linux asks the file system for the file's extent map (`FIEMAP`) and
/// looks for the shared-extent flag; a volume without an extent map is
/// answered from its type. macOS and Windows answer from the volume's file
/// system: one that cannot clone blocks is `Exclusive`, and APFS or ReFS is
/// `Unknown`.
///
/// # Errors
///
/// Returns the metadata or native query error, including `NotFound`.
pub fn extent_sharing(path: &Path) -> io::Result<ExtentSharing> {
    native::extent_sharing(path)
}

/// Create a symbolic link at `link` to the file `target`, which may not
/// exist yet.
///
/// Windows creates the file flavor of symbolic link, which may require the
/// Developer Mode or `SeCreateSymbolicLinkPrivilege` the host grants.
///
/// # Errors
///
/// Returns the native error, including `AlreadyExists` when `link` exists.
pub fn symlink_file(target: &Path, link: &Path) -> io::Result<()> {
    native::symlink_file(target, link)
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

/// Capture the host permission representation from already-fetched
/// metadata: the full Unix mode bits, or the Windows readonly attribute as
/// `0`/`1`. Values are host-specific, not portable archive modes; restore one
/// only with [`apply_metadata_mode`] on the same host.
pub fn metadata_mode(metadata: &std::fs::Metadata) -> u32 {
    native::metadata_mode(metadata)
}

/// Restore a value returned by [`metadata_mode`]. Unix applies the exact
/// mode; Windows treats any nonzero value as readonly and preserves every
/// other attribute.
///
/// # Errors
///
/// Returns the metadata or permission-change error.
pub fn apply_metadata_mode(path: &Path, mode: u32) -> io::Result<()> {
    native::apply_metadata_mode(path, mode)
}

/// Toggle write permission while retaining unrelated mode bits and
/// attributes.
///
/// Unix `readonly = true` clears every write bit; `false` adds only the
/// owner-write bit. Windows toggles its readonly attribute and skips the
/// write when it already has the requested value. Follows symbolic links.
///
/// # Errors
///
/// Returns the metadata error for a missing path (including a dangling
/// link), or the permission-change error.
pub fn set_readonly(path: &Path, readonly: bool) -> io::Result<()> {
    native::set_readonly(path, readonly)
}

/// Deny in-place writes to `path` for every trustee — including the owner —
/// while keeping `Permissions::readonly()` **false** and
/// rename-over-replace intact.
///
/// The two guarantees together are what a compiler that pre-checks its
/// outputs needs: rustc's `check_file_is_writeable` fails fatally when
/// `readonly()` is true even though rustc only ever writes a temp file and
/// renames it over the output (zccache#1791). A caller that wants "cannot be
/// overwritten at all" wants `set_readonly(path, true)` instead.
///
/// The two hosts reach the guarantee by different mechanics: Unix clears the
/// owner-write bit and keeps at least the group-write bit (the kernel
/// applies only the owner bits to the owner), Windows installs a deny ACE
/// for `FILE_WRITE_DATA | FILE_APPEND_DATA` on the file's DACL — the DACL
/// rides the file record, so every hardlink carries the seal, and a legacy
/// `READONLY` attribute is cleared so an older blob upgrades on its next
/// sealing. Idempotent on both hosts; a file this seal never touched is
/// left alone by the matching [`allow_in_place_writes`].
///
/// Follows symbolic links. Not a secure-open primitive: callers control the
/// path, as everywhere else in this module.
///
/// # Errors
///
/// Returns the metadata or permission-change error for a missing path; on
/// Windows, also when the filesystem cannot retain the deny ACE (no ACL
/// support), rather than reporting a seal that was silently dropped.
pub fn deny_in_place_writes(path: &Path) -> io::Result<()> {
    native::deny_in_place_writes(path)
}

/// Remove a seal applied by [`deny_in_place_writes`], restoring in-place
/// writes. A file without the seal is a no-op on both hosts.
///
/// Follows symbolic links.
///
/// # Errors
///
/// Returns the metadata or permission-change error for a missing path.
pub fn allow_in_place_writes(path: &Path) -> io::Result<()> {
    native::allow_in_place_writes(path)
}

/// Whether [`deny_in_place_writes`] currently applies to `path` — by deny
/// mechanism or by a legacy seal the host treats as equivalent (a
/// `READONLY` attribute on Windows).
///
/// Follows symbolic links.
///
/// # Errors
///
/// Returns the metadata error for a missing path.
pub fn in_place_writes_denied(path: &Path) -> io::Result<bool> {
    native::in_place_writes_denied(path)
}

/// Add every Unix execute bit, retaining the other mode bits. Windows has no
/// per-file execute bit and performs no filesystem operation.
///
/// # Errors
///
/// On Unix, returns the metadata or permission-change error.
pub fn make_executable(path: &Path) -> io::Result<()> {
    native::make_executable(path)
}

/// Add only the owner's Unix execute bit, retaining the other mode bits, so an
/// owner-private file (no group or other bits) stays private. Windows has no
/// per-file execute bit and performs no filesystem operation.
///
/// # Errors
///
/// On Unix, returns the metadata or permission-change error.
pub fn make_owner_executable(path: &Path) -> io::Result<()> {
    native::make_owner_executable(path)
}

// ---------------------------------------------------------------------------
// Change markers
// ---------------------------------------------------------------------------

/// An opaque change-journal observation of one file: its Windows USN.
///
/// Compare only observations of the same file within one journal epoch. It
/// is not a content hash, a file identity, or proof against a journal
/// reset, a concurrent write, or path replacement.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FileChangeMarker(i128);

/// Observe `path`'s change-journal position.
///
/// Windows reads the file's USN record (record versions 2 and 3). Missing
/// journals, query failures, and unsupported record versions return `None`,
/// never a timestamp substitute. Linux and macOS keep no such journal and
/// return `None` without probing the path. `None` means "cannot prove the
/// file is unchanged".
pub fn file_change_marker(path: &Path) -> Option<FileChangeMarker> {
    native::file_change_marker(path).map(FileChangeMarker)
}

// ---------------------------------------------------------------------------
// Volume facts
// ---------------------------------------------------------------------------

/// The raw identity of the volume hosting `path`: `st_dev` on Unix, the
/// volume serial number on Windows. For callers whose persisted keys need
/// the raw value; compare values from the same host only. Follows symbolic
/// links. `None` when the path cannot be inspected.
pub fn volume_identity_u128(path: &Path) -> Option<u128> {
    native::volume_identity_u128(path)
}

/// Width in bits of the host's native file identifier: 128 on Windows
/// (NTFS/ReFS `FILE_ID_INFO`), 64 (the inode) on Linux and macOS.
pub const fn file_id_width() -> u32 {
    native::file_id_width()
}

/// Bytes of storage the file described by `metadata` actually occupies.
///
/// Unix reports allocated 512-byte blocks, so a sparse file can report less
/// than its length. Windows reports the compressed size
/// (`GetCompressedFileSizeW` on `path`), falling back to the logical length
/// when the volume cannot answer. Never fails.
pub fn allocated_bytes(path: &Path, metadata: &std::fs::Metadata) -> u64 {
    native::allocated_bytes(path, metadata)
}

// ---------------------------------------------------------------------------
// Paths and durability
// ---------------------------------------------------------------------------

/// Convert tool-emitted path bytes to a host path.
///
/// Unix preserves arbitrary bytes. Windows accepts UTF-8 only, because a
/// byte stream cannot represent native UTF-16 losslessly, and returns `None`
/// otherwise rather than converting lossily. This does not validate the path
/// for filesystem use (an embedded NUL is retained).
pub fn path_from_raw_bytes(bytes: &[u8]) -> Option<PathBuf> {
    native::path_from_raw_bytes(bytes)
}

/// Prepare a file path for direct native calls.
///
/// Windows canonicalizes the existing parent and appends the unchanged file
/// name, yielding an extended-length absolute path that escapes `MAX_PATH`
/// without requiring the final file to exist. Linux and macOS return the
/// path unchanged. Follows parent links; this is not a secure-open or a
/// lexical-only operation.
///
/// # Errors
///
/// On Windows, `InvalidInput` when `path` has no file name, or the parent's
/// canonicalization error.
pub fn native_call_path(path: &Path) -> io::Result<PathBuf> {
    native::native_call_path(path)
}

/// Flush `directory`'s entries to stable storage where the host exposes a
/// directory flush.
///
/// Linux and macOS open and fsync the directory. Windows has no directory
/// handle to flush and succeeds without resolving `directory`; unlike
/// [`sync_directory`](super::sync_directory), a missing directory is not an
/// error there. Callers whose durability protocol can use directory fsync
/// where it exists, but must stay portable to hosts without it, use this.
///
/// # Errors
///
/// On Linux and macOS, the open or fsync error.
pub fn sync_directory_if_supported(directory: &Path) -> io::Result<()> {
    native::sync_directory_if_supported(directory)
}
