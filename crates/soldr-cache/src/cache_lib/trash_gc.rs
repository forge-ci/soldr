//! Root-local reclamation for `trash-*` buckets.
//!
//! The release-worktree CLI owns creation of these buckets.  Cleanup lives in
//! `soldr-cache` so the long-lived daemon can retry deletions without reaching
//! up into the CLI crate.  The supplied [`SoldrPaths`] root is the entire
//! ownership boundary; sibling product roots are never enumerated.

use crate::core::SoldrPaths;
use serde::Serialize;

// Older sweepers only walk `trash-*` buckets and do not take our lock.
pub(super) const DETACHED_TARGETS_DIR: &str = "gc-detached-targets";

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct SweepReport {
    pub removed: u64,
    pub retained: u64,
    /// Why the retained entries stayed, newest first, capped at
    /// [`MAX_REPORTED_REASONS`] (soldr#2199).
    ///
    /// Counts alone are undiagnosable: a bucket that never drains reports
    /// `retained=N` forever without saying whether the cause is a running
    /// binary, a permission problem, or something transient. The OS error is
    /// the whole diagnosis and it used to be discarded here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
}

/// Cap on retained-reason strings kept in a [`SweepReport`].
///
/// A poisoned bucket can hold thousands of entries failing for the same
/// reason; the first few name the cause, the rest are noise in a log the
/// daemon writes on every maintenance pass.
pub const MAX_REPORTED_REASONS: usize = 5;

/// A separate lock keeps the trash sweeper away from in-flight detachments.
/// Callers may hold the build lease, so contention must never block.
pub(super) fn try_trash_reclaim_lock(
    root: &std::path::Path,
    shared: bool,
) -> std::io::Result<Option<std::fs::File>> {
    use std::io::{Error, ErrorKind};

    super::path_safety::validate_owned_directory(root, root)?;
    let path = root.join("trash-reclaim.lock");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    let file = match options.clone().create_new(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(&path)?;
            if !metadata.is_file() || super::path_safety::is_link_or_reparse(&metadata) {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "invalid trash lock file",
                ));
            }
            options.open(&path)?
        }
        Err(error) => return Err(error),
    };
    let result = if shared {
        fs2::FileExt::try_lock_shared(&file)
    } else {
        fs2::FileExt::try_lock_exclusive(&file)
    };
    match result {
        Ok(()) => Ok(Some(file)),
        Err(error) if super::cargo_lock::lock_is_held(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn sweep_trash(paths: &SoldrPaths) -> std::io::Result<SweepReport> {
    let mut report = SweepReport::default();
    if !paths.root.exists() {
        return Ok(report);
    }
    let Some(_lease) = try_trash_reclaim_lock(&paths.root, false)? else {
        return Ok(report);
    };
    let entries = match std::fs::read_dir(&paths.root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(error) => return Err(error),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name != DETACHED_TARGETS_DIR && !name.to_string_lossy().starts_with("trash-") {
            continue;
        }
        let metadata = match std::fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.is_dir() || crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
            continue;
        }
        crate::cache_lib::path_safety::validate_owned_directory(&paths.root, &entry.path())?;
        let bucket_entries = match std::fs::read_dir(entry.path()) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        let mut bucket_retained = 0usize;
        for child in bucket_entries.flatten() {
            let path = child.path();
            let result = match std::fs::symlink_metadata(&path) {
                Ok(kind) if crate::cache_lib::path_safety::is_link_or_reparse(&kind) => {
                    report.retained += 1;
                    bucket_retained += 1;
                    continue;
                }
                Ok(kind) if kind.is_dir() => std::fs::remove_dir_all(&path),
                Ok(kind) if kind.is_file() => std::fs::remove_file(&path),
                _ => continue,
            };
            match result {
                Ok(()) => report.removed += 1,
                Err(err) => {
                    report.retained += 1;
                    bucket_retained += 1;
                    if report.reasons.len() < MAX_REPORTED_REASONS {
                        report.reasons.push(format!("{}: {err}", path.display()));
                    }
                }
            }
        }
        // An emptied bucket is removed rather than left behind. The buckets
        // are named per volume and recreated on demand, so a drained one is
        // pure residue -- and its continued presence reads as "trash is still
        // pending" to anyone looking (soldr#2199).
        // Per-bucket, not global: one poisoned bucket must not keep every
        // other drained bucket on disk.
        if bucket_retained == 0 {
            let _ = std::fs::remove_dir(entry.path());
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_is_exact_root_local_and_ignores_links() {
        let temp = tempfile::tempdir().unwrap();
        let owned = temp.path().join("owned");
        let sibling = temp.path().join("sibling");
        std::fs::create_dir_all(owned.join("trash-C/1")).unwrap();
        std::fs::create_dir_all(sibling.join("trash-C/keep")).unwrap();
        std::fs::write(owned.join("trash-C/1/file"), b"delete").unwrap();
        std::fs::write(sibling.join("trash-C/keep/sentinel"), b"keep").unwrap();

        let report = sweep_trash(&SoldrPaths::with_root(owned)).unwrap();
        assert_eq!(report.removed, 1);
        assert!(sibling.join("trash-C/keep/sentinel").is_file());
    }
}

#[cfg(test)]
mod sweep_reporting_tests {
    use super::*;

    fn paths_at(root: &std::path::Path) -> SoldrPaths {
        SoldrPaths::with_root(root.to_path_buf())
    }

    // soldr#2199: a drained bucket is residue. Leaving it reads as "trash
    // still pending" to anyone inspecting the root.
    #[test]
    fn trash_lock_does_not_create_a_dangling_link_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("soldr");
        std::fs::create_dir(&root).unwrap();
        let outside = temp.path().join("outside");
        let link = root.join("trash-reclaim.lock");
        if crate::platform::fs::links::create(&outside.to_string_lossy(), &link, false).is_err() {
            return;
        }

        assert!(try_trash_reclaim_lock(&root, true).is_err());
        assert!(!outside.exists());
    }

    #[test]
    fn sweep_reclaims_a_target_left_by_an_interrupted_detach() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_at(temp.path());
        let bucket = paths.root.join(DETACHED_TARGETS_DIR);
        let target = bucket.join("target-interrupted/target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("artifact"), b"stale").unwrap();

        let report = sweep_trash(&paths).unwrap();
        assert_eq!(report.removed, 1);
        assert!(!bucket.exists());
    }

    #[test]
    fn an_emptied_bucket_is_removed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bucket = temp.path().join("trash-C");
        std::fs::create_dir_all(&bucket).expect("mkdir");
        std::fs::write(bucket.join("stale.bin"), b"x").expect("write");

        let report = sweep_trash(&paths_at(temp.path())).expect("sweep");
        assert_eq!(report.removed, 1);
        assert_eq!(report.retained, 0);
        assert!(
            !bucket.exists(),
            "a fully drained bucket must not survive the sweep",
        );
    }

    // One poisoned bucket must not keep the others on disk -- the drain
    // check is per-bucket, not global.
    #[test]
    fn a_retained_bucket_does_not_block_draining_others() {
        let temp = tempfile::tempdir().expect("tempdir");
        let drained = temp.path().join("trash-C");
        std::fs::create_dir_all(&drained).expect("mkdir");
        std::fs::write(drained.join("gone.bin"), b"x").expect("write");

        // A bucket whose child is a link is retained by design (never
        // followed), which makes it a stable stand-in for "undeletable".
        let held = temp.path().join("trash-D");
        std::fs::create_dir_all(held.join("kept")).expect("mkdir");

        let report = sweep_trash(&paths_at(temp.path())).expect("sweep");
        assert!(
            !drained.exists(),
            "the drained bucket must go even though another bucket remains",
        );
        assert!(report.removed >= 1);
    }

    #[test]
    fn reported_reasons_are_capped() {
        assert_eq!(MAX_REPORTED_REASONS, 5);
        let report = SweepReport::default();
        assert!(
            report.reasons.is_empty(),
            "a clean sweep reports no reasons"
        );
    }
}
