//! Disk-budget scanning and eviction for staged v2 artifact generations.

use super::{
    is_staged_link_or_reparse, open_store_lock, pointer_path, remove_registered_blob,
    remove_staged_link_or_reparse, staged_key_supported, staged_root,
    validate_staged_artifact_root, STORE_LOCK,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::time::SystemTime;

pub(crate) struct StagedDiskArtifact {
    pub(crate) key: String,
    #[cfg(test)]
    pub(crate) total_size: u64,
    pub(crate) mtime: SystemTime,
    /// Committed generation observed while holding the shared store lock.
    pub(crate) generation: Option<String>,
}

fn staged_tree_stats(path: &Path) -> io::Result<(u64, SystemTime)> {
    let metadata = fs::symlink_metadata(path)?;
    if is_staged_link_or_reparse(path) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing staged artifact scan through linked/reparse path: {}",
                path.display()
            ),
        ));
    }
    let mut size = metadata.len();
    let mut mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    if metadata.is_dir() {
        for entry in fs::read_dir(path)?.flatten() {
            let (child_size, child_mtime) = staged_tree_stats(&entry.path())?;
            size = size.saturating_add(child_size);
            mtime = mtime.max(child_mtime);
        }
    }
    Ok((size, mtime))
}

pub(crate) fn scan_staged_disk_artifacts(
    artifact_dir: &Path,
) -> io::Result<Vec<StagedDiskArtifact>> {
    let root = staged_root(artifact_dir);
    if !validate_staged_artifact_root(artifact_dir)? {
        return Ok(Vec::new());
    }
    let store_lock = open_store_lock(&root)?;
    let _store_lock_guard = crate::wait::lock_shared_for(&store_lock, crate::wait::SHORT_LOCK)?;
    scan_staged_disk_artifacts_locked(artifact_dir, root.as_path())
}

fn scan_staged_disk_artifacts_locked(
    artifact_dir: &Path,
    root: &Path,
) -> io::Result<Vec<StagedDiskArtifact>> {
    let mut artifacts = Vec::new();
    for entry in fs::read_dir(root)?.flatten() {
        let key_root = entry.path();
        let key_metadata = fs::symlink_metadata(&key_root)?;
        if is_staged_link_or_reparse(&key_root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing staged artifact scan through linked/reparse key root: {}",
                    key_root.display()
                ),
            ));
        }
        if !key_metadata.is_dir() {
            continue;
        }
        let key = entry.file_name().to_string_lossy().into_owned();
        if !staged_key_supported(&key) {
            continue;
        }
        let (mut _total_size, mut mtime) = staged_tree_stats(&key_root)?;
        let pointer = pointer_path(artifact_dir, &key);
        let generation = if let Ok(metadata) = fs::symlink_metadata(&pointer) {
            if is_staged_link_or_reparse(&pointer) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing staged artifact scan through linked/reparse pointer: {}",
                        pointer.display()
                    ),
                ));
            }
            _total_size = _total_size.saturating_add(metadata.len());
            mtime = mtime.max(metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH));
            fs::read_to_string(&pointer)
                .ok()
                .map(|value| value.trim().to_string())
        } else {
            None
        };
        artifacts.push(StagedDiskArtifact {
            key,
            #[cfg(test)]
            total_size: _total_size,
            mtime,
            generation,
        });
    }
    Ok(artifacts)
}

#[cfg(test)]
pub(crate) fn evict_staged_artifact_keys(
    artifact_dir: &Path,
    keys: &HashSet<String>,
) -> io::Result<u64> {
    if keys.is_empty() {
        return Ok(0);
    }
    let root = staged_root(artifact_dir);
    if !validate_staged_artifact_root(artifact_dir)? {
        return Ok(0);
    }
    let store_lock = open_store_lock(&root)?;
    let _store_lock_guard = crate::wait::lock_exclusive_for(&store_lock, crate::wait::SHORT_LOCK)?;
    evict_staged_artifact_keys_locked(artifact_dir, root.as_path(), keys)
}

/// Delete only staged generations that still match the maintenance snapshot.
/// The exclusive lock makes compare-and-delete atomic with respect to staged
/// publication without blocking writers during legacy scans or planning.
pub(crate) fn evict_staged_artifact_keys_if_unchanged(
    artifact_dir: &Path,
    expected: &HashMap<String, Option<String>>,
) -> io::Result<HashSet<String>> {
    if expected.is_empty() {
        return Ok(HashSet::new());
    }
    let root = staged_root(artifact_dir);
    if !validate_staged_artifact_root(artifact_dir)? {
        return Ok(HashSet::new());
    }
    let store_lock = open_store_lock(&root)?;
    #[cfg(test)]
    super::hook::pause(
        artifact_dir,
        super::StagedHookPoint::MaintenanceStoreLockPending,
    );
    // Bounded: the caller holds the publication write barrier, and a reader
    // (`zccache warm`, a stopped process) can hold this lock shared for as
    // long as it likes. Waiting without a deadline froze every compile's
    // publication behind maintenance (soldr#3836 audit). A busy store
    // defers eviction to the next pass instead.
    let Some(_store_lock_guard) = lock_exclusive_within(&store_lock, MAINTENANCE_STORE_LOCK_DEADLINE)? else {
        tracing::warn!(
            root = %root.display(),
            "staged store lock busy for {:?}; eviction deferred to the next maintenance pass",
            MAINTENANCE_STORE_LOCK_DEADLINE
        );
        return Ok(HashSet::new());
    };
    let mut removed = HashSet::new();
    for (key, expected_generation) in expected.iter().filter(|(key, _)| staged_key_supported(key)) {
        let pointer = pointer_path(artifact_dir, key);
        let current = match fs::symlink_metadata(&pointer) {
            Ok(_) if is_staged_link_or_reparse(&pointer) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing staged artifact eviction through linked/reparse pointer: {}",
                        pointer.display()
                    ),
                ));
            }
            Ok(_) => fs::read_to_string(&pointer)
                .ok()
                .map(|value| value.trim().to_string()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if &current != expected_generation {
            continue;
        }
        // Detach under the locks (one rename per path); the recursive delete
        // runs later in `sweep_staged_trash`, outside the store flock and the
        // publication barrier, so publishers are not stalled for it.
        move_to_staged_trash(&root, &root.join(key))?;
        move_to_staged_trash(&root, &pointer_path(artifact_dir, key))?;
        removed.insert(key.clone());
    }
    Ok(removed)
}

const MAINTENANCE_STORE_LOCK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);
const CLEAR_STORE_LOCK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Take the exclusive store lock, giving up after `deadline` (backing off
/// from 1 ms to 50 ms between attempts). `None` means another holder kept it.
fn lock_exclusive_within(
    file: &std::fs::File,
    deadline: std::time::Duration,
) -> io::Result<Option<kernal_api::platform::fs::FileLock<'_>>> {
    let started = std::time::Instant::now();
    let mut backoff = std::time::Duration::from_millis(1);
    loop {
        match kernal_api::platform::fs::try_lock_exclusive(file) {
            Ok(guard) => return Ok(Some(guard)),
            Err(error) if kernal_api::platform::fs::is_lock_conflict(&error) => {
                let waited = started.elapsed();
                if waited >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(backoff.min(deadline - waited));
                backoff = (backoff * 2).min(std::time::Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Evicted staged trees awaiting deletion. Not a hex key, so every scanner
/// that walks the staged root skips it.
const STAGED_TRASH: &str = ".trash";

fn move_to_staged_trash(root: &Path, path: &Path) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    if is_staged_link_or_reparse(path) {
        // Keep the existing link/reparse removal semantics exactly.
        return remove_staged_tree(path).map(|_| ());
    }
    let trash = root.join(STAGED_TRASH);
    fs::create_dir_all(&trash)?;
    let name = format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    fs::rename(path, trash.join(name))
}

/// Delete everything evicted into the staged trash. Runs without the store
/// lock: trash entries are unreachable by every reader. Per-entry failures
/// are logged and left for the next sweep.
pub(crate) fn sweep_staged_trash(artifact_dir: &Path) -> u64 {
    let trash = staged_root(artifact_dir).join(STAGED_TRASH);
    let Ok(entries) = fs::read_dir(&trash) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        match remove_staged_tree(&entry.path()) {
            Ok(bytes) => removed += bytes,
            Err(error) => tracing::warn!(
                path = %entry.path().display(),
                %error,
                "staged trash entry not removed; the next sweep retries"
            ),
        }
    }
    removed
}

#[cfg(test)]
fn evict_staged_artifact_keys_locked(
    artifact_dir: &Path,
    root: &Path,
    keys: &HashSet<String>,
) -> io::Result<u64> {
    let mut bytes_removed: u64 = 0;
    for key in keys.iter().filter(|key| staged_key_supported(key)) {
        bytes_removed = bytes_removed.saturating_add(remove_staged_tree(&root.join(key))?);
        bytes_removed =
            bytes_removed.saturating_add(remove_staged_tree(&pointer_path(artifact_dir, key))?);
    }
    Ok(bytes_removed)
}

pub(super) fn remove_staged_tree(path: &Path) -> io::Result<u64> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    if is_staged_link_or_reparse(path) {
        remove_staged_link_or_reparse(path, &metadata)?;
        return Ok(0);
    }
    if metadata.is_dir() {
        let mut removed = 0;
        for entry in fs::read_dir(path)?.flatten() {
            removed += remove_staged_tree(&entry.path())?;
        }
        fs::remove_dir(path)?;
        return Ok(removed);
    }
    let size = metadata.len();
    remove_registered_blob(path)?;
    Ok(size)
}

pub(in crate::daemon_core::daemon::server) fn clear_staged_artifacts(artifact_dir: &Path) -> io::Result<u64> {
    let root = staged_root(artifact_dir);
    if !validate_staged_artifact_root(artifact_dir)? {
        return Ok(0);
    }
    let store_lock = open_store_lock(&root)?;
    #[cfg(test)]
    super::hook::pause(
        artifact_dir,
        super::StagedHookPoint::MaintenanceStoreLockPending,
    );
    // Bounded for the same reason as eviction (the caller holds the
    // publication write barrier); Clear waits longer for in-flight hit
    // deliveries, then fails loudly rather than hanging.
    let Some(_store_lock_guard) = lock_exclusive_within(&store_lock, CLEAR_STORE_LOCK_DEADLINE)? else {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "staged store lock {} still held after {:?} (a `zccache warm` or stopped \
                 process?); staged artifacts were not cleared",
                root.join(STORE_LOCK).display(),
                CLEAR_STORE_LOCK_DEADLINE
            ),
        ));
    };
    // Detach everything under the lock; `sweep_staged_trash` deletes it after
    // the caller releases the publication barrier.
    for entry in fs::read_dir(&root)?.flatten() {
        if entry.file_name() == STORE_LOCK || entry.file_name() == STAGED_TRASH {
            continue;
        }
        move_to_staged_trash(&root, &entry.path())?;
    }
    Ok(0)
}
