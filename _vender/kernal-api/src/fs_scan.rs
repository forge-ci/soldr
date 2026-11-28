//! Streaming native disk accounting, independent of the `fs` feature.
//!
//! `fs` selects dirs, globset, jwalk, reflink-copy and tempfile. A scanner
//! needs none of those: `fs-scan` uses standard threads and collections and
//! the existing private libc/windows-sys bindings, adding no dependency.
//!
//! Allocation is Linux blocks times 512, Darwin bulk ALLOCSIZE (with metadata
//! fallback), or Windows physical allocation for sparse/compressed files.
//! Hardlinks count once by volume and file identity; duplicate file entries
//! have zero sizes. Directories count their own allocation, not subtree sums.
//! Descendant failures increment [`Summary::errors`]; root failures are fatal.
//! This is a live observation: pending Windows sparse writes can change physical
//! allocation until the writer is synced and closed. See `docs/fs-scan.md`.
//!
//! Callbacks run on the calling thread, may borrow non-Send state, and stop
//! delivery by returning false. Worker buffers, batches and directory queues
//! are bounded; visited identities and queued directory paths grow with the
//! scan. The whole-volume Windows fast path retains its record graph.
#![deny(
    clippy::unwrap_used,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(test)]
pub(crate) mod test_support;
mod walk;

// The pure parsers live beside their native users. Compile their unchanged
// synthetic tests on every host, without compiling foreign ABI code.
#[cfg(test)]
#[allow(
    dead_code,
    reason = "Foreign parser fields are consumed by its native scanner"
)]
#[path = "platform_macos/fs_scan_bulk.rs"]
pub(crate) mod bulk_parser_tests;
#[cfg(test)]
#[allow(
    dead_code,
    reason = "Foreign parser fields are consumed by its native scanner"
)]
#[path = "platform_win/fs_scan/mft.rs"]
pub(crate) mod mft_parser_tests;
#[cfg(test)]
#[path = "platform_win/fs_scan/records.rs"]
pub(crate) mod windows_record_tests;

/// Selection policy; automatic preserves the native scanner's fallback order.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ScanCapability {
    #[default]
    Automatic,
    /// Skip whole-volume MFT/USN optimizations and enumerate directories.
    DirectoryWalk,
}

#[derive(Clone, Debug, Default)]
pub struct ScanOptions {
    pub follow_symlinks: bool,
    pub one_filesystem: bool,
    /// None uses min(available CPUs, 8); explicit counts must be 1..=256.
    pub threads: Option<usize>,
    pub capability: ScanCapability,
}

/// Opaque object identity used to connect a child to its parent directory.
/// Hardlinked names share an identity; IDs are observations, not open handles.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct EntryId(FileIdentity);

/// Volume-scoped native file identity, with no binding or handle vocabulary.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct FileIdentity {
    volume: u64,
    file: u64,
}

impl FileIdentity {
    pub const fn entry_id(self) -> EntryId {
        EntryId(self)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    #[default]
    Other,
}

/// Borrowed discovery event. Ordering is unspecified, including parents.
#[derive(Clone, Copy, Debug)]
pub struct ScanEntry<'a> {
    /// Lossless path relative to the requested root; empty for the root itself.
    /// May contain several components. The borrow lasts only for the callback.
    pub relative_name: &'a OsStr,
    /// Root uses its own identity as parent. A parent may be delivered later.
    pub parent: EntryId,
    pub allocated: u64,
    pub apparent: u64,
    pub kind: EntryKind,
    pub links: u64,
    pub identity: FileIdentity,
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub allocated: u64,
    pub apparent: u64,
    pub entries: u64,
    pub errors: u64,
    pub cancelled: bool,
    /// Reserved NTFS metadata, excluded from the user-visible totals.
    pub metadata_allocated: u64,
}

/// Failure to initialize a scan; descendant errors are counted in the summary.
#[derive(Debug)]
pub struct ScanError(pub io::Error);

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for ScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}
impl From<io::Error> for ScanError {
    fn from(error: io::Error) -> Self {
        Self(error)
    }
}

/// Host support, not a promise about a particular filesystem or account.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    pub raw_mft_supported: bool,
    /// Raw volume access ordinarily needs elevation; directory walking does not.
    pub requires_elevation: bool,
}

pub fn capabilities() -> Capabilities {
    crate::native_fs_scan::capabilities()
}

/// Scan without retaining a file tree. False cancels further callback delivery;
/// the summary includes the last delivered entry. Workers finish and release
/// their resources before returning. Call from a blocking thread.
pub fn scan_stream(
    root: &Path,
    options: &ScanOptions,
    mut sink: impl FnMut(ScanEntry<'_>) -> bool,
) -> Result<Summary, ScanError> {
    walk::scan_stream(root, options, |entry| sink(entry.borrowed()))
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Metadata {
    pub device: u64,
    pub inode: u64,
    pub mount: Option<u64>,
    pub allocated: u64,
    pub apparent: u64,
    pub is_dir: bool,
    pub links: u64,
    pub kind: EntryKind,
}

impl Metadata {
    pub fn identity(self) -> (u64, u64) {
        (self.device, self.inode)
    }
    pub fn file_identity(self) -> FileIdentity {
        FileIdentity {
            volume: self.device,
            file: self.inode,
        }
    }
    pub fn same_filesystem(self, other: Self) -> bool {
        match (self.mount, other.mount) {
            (Some(a), Some(b)) => a == b,
            _ => self.device == other.device,
        }
    }
}

// Private transport between workers; native buffers never cross the facade.
pub(crate) struct OwnedEntry {
    path: PathBuf,
    meta: Metadata,
    parent: EntryId,
}

impl OwnedEntry {
    pub(crate) fn new(path: PathBuf, meta: Metadata, parent: FileIdentity) -> Self {
        Self {
            path,
            meta,
            parent: parent.entry_id(),
        }
    }
    fn borrowed(&self) -> ScanEntry<'_> {
        ScanEntry {
            relative_name: self.path.as_os_str(),
            parent: self.parent,
            allocated: self.meta.allocated,
            apparent: self.meta.apparent,
            kind: self.meta.kind,
            links: self.meta.links,
            identity: self.meta.file_identity(),
        }
    }
}

fn skipped(system_root: bool, name: &OsStr, root: Metadata, child: Metadata) -> bool {
    root.device != child.device
        && system_root
        && matches!(name.to_str(), Some("proc" | "sys" | "dev"))
}
