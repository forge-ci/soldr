//! Path-observed file identity, preserving the native identifier width.
//!
//! These observations follow links and retain no handles. An identity can be
//! reused after deletion, equality is not a content or immutability
//! guarantee, and separate lookups may race with path replacement.
//!
//! This is distinct from [`super::FileIdentity`], which observes an open
//! [`std::fs::File`] and reports `None` where a host cannot answer. Use this
//! one when the question is whether two *paths* currently name the same
//! object, for example whether a materialized output is a hard link to its
//! cache blob.

use std::io;
use std::path::Path;

/// Opaque host identity: the Unix device/inode pair, or the Windows volume
/// serial plus the 128-bit `FILE_ID_INFO` identifier (the legacy 64-bit
/// index, zero-extended, where that is unavailable).
///
/// Compare and hash values from the same host only.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FileIdentity(crate::native_fs_materialize::PathFileIdentity);

/// Observe the identity of the object `path` currently names.
///
/// Windows opens the path metadata-only with every share mode, so a live
/// writer is never disturbed, and admits directories.
///
/// # Errors
///
/// Unix propagates the metadata error. Windows reports `NotFound` whenever
/// the identity cannot be observed.
pub fn file_identity(path: &Path) -> io::Result<FileIdentity> {
    crate::native_fs_materialize::path_file_identity(path).map(FileIdentity)
}

/// Whether `a` and `b` currently name the same object (hard links, or two
/// spellings of one path).
///
/// # Errors
///
/// Unix propagates either metadata error. Windows never errors: it returns
/// `Ok(false)` when either identity is unavailable, including both.
pub fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
    crate::native_fs_materialize::same_file(a, b)
}
