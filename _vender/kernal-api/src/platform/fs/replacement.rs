//! Native replacement operations for publishing staged artifacts.
//!
//! Paths and their parent directories must be caller-controlled, and source
//! and destination must share a filesystem. These are not secure-open
//! transactions or cross-filesystem copy fallbacks, and none adds Unix fsync
//! durability; pair them with [`super::sync_directory_if_supported`].
//!
//! On Windows every file operation here retries antivirus-scanner and
//! indexer interference: after an access-denied (5), sharing-violation (32),
//! or any `PermissionDenied`-kind failure it waits 50, 100, 250, then 500 ms,
//! for five attempts in all, and returns the last error when they are
//! exhausted. Any other error returns immediately.

use std::io;
use std::path::Path;

use crate::native_fs_materialize as native;

/// Replace `destination` with `source` by native rename.
///
/// Unix `rename`. Windows `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING |
/// MOVEFILE_WRITE_THROUGH` on extended-length paths (see
/// [`super::native_call_path`]), with the retry ladder. Success consumes the
/// source path; on failure both paths keep their state where the host
/// guarantees it.
///
/// # Errors
///
/// Returns the final native error.
pub fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    native::atomic_replace(source, destination)
}

/// Rename a new generation into place with host-native collision behavior.
///
/// Windows refuses an existing destination and applies the retry ladder.
/// Unix uses `rename` and **may replace** an existing destination. This is
/// not a portable no-clobber rename.
///
/// # Errors
///
/// Returns the final native error.
pub fn rename_generation(source: &Path, destination: &Path) -> io::Result<()> {
    native::rename_generation(source, destination)
}

/// Replace `destination`, deleting it first on Windows if replacement fails.
///
/// Unix `rename`. Windows first tries [`atomic_replace`]; after *any* error,
/// when the destination exists it deletes that file and retries with
/// [`rename_generation`]. That opt-in compatibility path is not atomic and
/// may lose the old destination even when the final rename fails (including
/// when the source is missing).
///
/// # Errors
///
/// Returns the replacement error when the destination does not exist, the
/// deletion error, or the final rename error.
pub fn replace_with_delete_fallback(source: &Path, destination: &Path) -> io::Result<()> {
    native::replace_with_delete_fallback(source, destination)
}

/// Install the staged directory tree `staged` at `requested`, which may
/// already exist. Creates `requested`'s parent. On success `staged` no
/// longer exists.
///
/// A missing destination is a plain rename. An existing Linux or macOS
/// destination is exchanged in place (`renameat2` `RENAME_EXCHANGE`,
/// `renamex_np` `RENAME_SWAP`), so the requested path never goes missing,
/// and the old tree is then removed; a filesystem without exchange support
/// returns its error. Windows has no directory exchange: it renames the old
/// tree to a backup sibling, installs the new tree, and removes the backup,
/// attempting to restore the backup if installation fails. Rollback is
/// best-effort, and a cleanup failure can return an error after the new tree
/// is installed. No crash durability or concurrent-installer isolation is
/// promised.
///
/// # Errors
///
/// Returns the first native error from creation, exchange/rename, or
/// cleanup.
pub fn install_directory(staged: &Path, requested: &Path) -> io::Result<()> {
    native::install_directory(staged, requested)
}

/// Whether `error` is a Windows access-denied (5) or sharing-violation (32)
/// failure that may clear when retried. Always `false` on Linux and macOS.
///
/// Narrower than the internal retry rule: an error synthesized with
/// `PermissionDenied` kind but no native code is `false`.
#[must_use]
pub fn is_transient_share_error(error: &io::Error) -> bool {
    native::is_transient_share_error(error)
}

/// Whether `error` is a Windows lock violation (33). Always `false` on Linux
/// and macOS.
///
/// A narrow classifier for byte-range lock contention, distinct from
/// [`super::is_lock_conflict`], which answers for this crate's own advisory
/// lock operations on every host.
#[must_use]
pub fn is_lock_contention(error: &io::Error) -> bool {
    native::is_lock_contention(error)
}
