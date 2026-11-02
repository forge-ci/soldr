//! Central content-addressed toolchain store (soldr#4027).
//!
//! The same Rust toolchain used to sit on disk as full copies in several
//! places: the caller's `~/.rustup/toolchains`, soldr's managed
//! `<root>/rustup/toolchains`, and sometimes twice inside one home (a bare
//! `nightly-2026-08-15` next to `nightly-2026-08-15-<triple>`). Every copy
//! is gigabytes.
//!
//! The store keeps one copy of every distinct file under
//! `<root>/toolchain-store/objects/<aa>/<blake3>[-x]` and makes each
//! toolchain file a hardlink to it. rustup still sees ordinary files in
//! ordinary directories, so nothing that reads a toolchain changes; only
//! the duplicate bytes go away. A file on another filesystem cannot be
//! hardlinked and is left as it is, and counted.
//!
//! Hardlinks are safe here because rustup never edits an installed file
//! in place: install, update and uninstall each create or remove whole
//! files, which leaves the other links untouched.
//!
//! The store also keeps a last-use stamp per toolchain under
//! `<root>/toolchain-store/used/<name>`, which
//! `soldr gc purge --kind rustup_toolchain --unused-days N` reads to pick
//! only toolchains no pin has used within N days. An object whose only
//! remaining link is the store's own is unreferenced and
//! [`gc_unreferenced_objects`] deletes it.

use serde::Serialize;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::core::SoldrPaths;

/// Stamps newer than this are not rewritten, so recording use on every
/// cargo invocation costs a `stat`, not a write.
const USE_STAMP_REFRESH: Duration = Duration::from_secs(60 * 60);

/// The store root for a soldr home.
pub(crate) fn store_root(paths: &SoldrPaths) -> PathBuf {
    paths.root.join("toolchain-store")
}

/// What linking one or more toolchain directories into the store did.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LinkStats {
    /// Regular files examined.
    pub(crate) files: u64,
    /// Files that were replaced by a link to an existing object.
    pub(crate) deduplicated_files: u64,
    /// Bytes those replaced files held, now freed.
    pub(crate) deduplicated_bytes: u64,
    /// Files that became new objects in the store.
    pub(crate) new_objects: u64,
    /// Files that could not be linked (another filesystem, a file in use).
    pub(crate) skipped_files: u64,
}

impl LinkStats {
    fn add(&mut self, other: &LinkStats) {
        self.files += other.files;
        self.deduplicated_files += other.deduplicated_files;
        self.deduplicated_bytes += other.deduplicated_bytes;
        self.new_objects += other.new_objects;
        self.skipped_files += other.skipped_files;
    }
}

/// Link every real toolchain directory of `rustup_home` into the store.
/// Symlinked entries (`rustup toolchain link`) are not rustup's copies and
/// are left alone.
pub(crate) fn link_rustup_home(store: &Path, rustup_home: &Path) -> LinkStats {
    let mut stats = LinkStats::default();
    let Ok(entries) = std::fs::read_dir(rustup_home.join("toolchains")) else {
        return stats;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            stats.add(&link_toolchain_dir(store, &entry.path()));
        }
    }
    stats
}

/// Link every regular file under `toolchain_dir` into the store.
pub(crate) fn link_toolchain_dir(store: &Path, toolchain_dir: &Path) -> LinkStats {
    let mut stats = LinkStats::default();
    let mut pending = vec![toolchain_dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                stats.files += 1;
                match link_file(store, &path) {
                    Ok(LinkOutcome::Deduplicated(bytes)) => {
                        stats.deduplicated_files += 1;
                        stats.deduplicated_bytes += bytes;
                    }
                    Ok(LinkOutcome::NewObject) => stats.new_objects += 1,
                    Ok(LinkOutcome::AlreadyLinked) => {}
                    Err(_) => stats.skipped_files += 1,
                }
            }
        }
    }
    stats
}

#[derive(Debug, PartialEq, Eq)]
enum LinkOutcome {
    AlreadyLinked,
    NewObject,
    Deduplicated(u64),
}

fn link_file(store: &Path, file: &Path) -> io::Result<LinkOutcome> {
    let metadata = std::fs::metadata(file)?;
    let object = object_path(store, &hash_file(file)?, is_executable(file));
    if let Some(parent) = object.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::metadata(&object) {
        Ok(existing) => {
            if same_inode(&metadata, &existing) {
                return Ok(LinkOutcome::AlreadyLinked);
            }
            // Link the object next to the file, then rename over it so the
            // path is never missing.
            let temp = file.with_file_name(format!(
                ".{}.soldr-store-{}",
                file.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                std::process::id()
            ));
            let _ = std::fs::remove_file(&temp);
            std::fs::hard_link(&object, &temp)?;
            if let Err(error) = std::fs::rename(&temp, file) {
                let _ = std::fs::remove_file(&temp);
                return Err(error);
            }
            Ok(LinkOutcome::Deduplicated(metadata.len()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::hard_link(file, &object)?;
            Ok(LinkOutcome::NewObject)
        }
        Err(error) => Err(error),
    }
}

fn object_path(store: &Path, hash: &str, executable: bool) -> PathBuf {
    let name = if executable {
        format!("{hash}-x")
    } else {
        hash.to_string()
    };
    store.join("objects").join(&hash[..2]).join(name)
}

fn hash_file(file: &Path) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(std::fs::File::open(file)?)?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// The executable bit is part of the object key: hardlinks share
/// permissions, so a script and a data file with the same bytes must not
/// become one inode. Hosts without mode bits never set it.
fn is_executable(path: &Path) -> bool {
    crate::platform::fs::permissions::mode(path).is_some_and(|mode| mode & 0o111 != 0)
}

fn same_inode(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use crate::platform::fs::identity::hardlink_identity;
    match (hardlink_identity(a), hardlink_identity(b)) {
        (Some(a), Some(b)) => a.dev == b.dev && a.index == b.index,
        _ => false,
    }
}

/// Delete objects nothing links to any more. Returns `(objects, bytes)`.
pub(crate) fn gc_unreferenced_objects(store: &Path) -> (u64, u64) {
    let mut removed = (0, 0);
    let Ok(shards) = std::fs::read_dir(store.join("objects")) else {
        return removed;
    };
    for shard in shards.flatten() {
        let Ok(objects) = std::fs::read_dir(shard.path()) else {
            continue;
        };
        for object in objects.flatten() {
            let Ok(metadata) = object.metadata() else {
                continue;
            };
            if link_count(&object.path()) == Some(1) && std::fs::remove_file(object.path()).is_ok()
            {
                removed.0 += 1;
                removed.1 += metadata.len();
            }
        }
        let _ = std::fs::remove_dir(shard.path());
    }
    removed
}

fn link_count(path: &Path) -> Option<u64> {
    let file = std::fs::File::open(path).ok()?;
    crate::platform::fs::links::hard_link_count(&file).ok()
}

/// The stamp name for a toolchain: the channel without a host-triple
/// suffix, so `nightly-2026-08-15` and `nightly-2026-08-15-<triple>`
/// share one stamp.
fn stamp_name(toolchain: &str) -> String {
    let host = crate::pyo3_detect::host_triple();
    toolchain
        .strip_suffix(host)
        .and_then(|rest| rest.strip_suffix('-'))
        .unwrap_or(toolchain)
        .replace(['/', '\\'], "_")
}

/// Record a use of `toolchain` in the soldr home's store and return the
/// store root.
pub(crate) fn note_use(paths: &SoldrPaths, toolchain: &str) -> PathBuf {
    let store = store_root(paths);
    record_use(&store, toolchain);
    store
}

/// Link a freshly installed toolchain into the store so a copy already on
/// disk under another rustup home is shared. No-op when not `fresh`.
pub(crate) fn link_fresh(store: &Path, fresh: bool, toolchain_dir: &Path) {
    if fresh {
        link_toolchain_dir(store, toolchain_dir);
    }
}

/// Record that `toolchain` was used now. Best effort.
pub(crate) fn record_use(store: &Path, toolchain: &str) {
    if toolchain.is_empty() {
        return;
    }
    let stamp = store.join("used").join(stamp_name(toolchain));
    let fresh = std::fs::metadata(&stamp)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < USE_STAMP_REFRESH);
    if fresh {
        return;
    }
    if let Some(parent) = stamp.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&stamp, []);
}

/// When `toolchain` was last used: its stamp, or, for a toolchain installed
/// before stamps existed, the modification time of its directory.
pub(crate) fn last_used(store: &Path, toolchain: &str, toolchain_dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(store.join("used").join(stamp_name(toolchain)))
        .or_else(|_| std::fs::metadata(toolchain_dir))
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// Whether a toolchain last used at `last_used` is older than `days`.
/// An unknown time counts as unused.
pub(crate) fn unused_for(last_used: Option<SystemTime>, days: u64, now: SystemTime) -> bool {
    let window = Duration::from_secs(days.saturating_mul(24 * 60 * 60));
    match last_used {
        Some(time) => now.duration_since(time).is_ok_and(|age| age >= window),
        None => true,
    }
}

/// `soldr toolchain dedupe`: link the caller's and the managed rustup
/// homes into the store, then drop unreferenced objects.
pub(crate) fn run_toolchain_dedupe(json: bool) -> Result<i32, crate::core::SoldrError> {
    let paths = SoldrPaths::new()?;
    let store = store_root(&paths);
    let mut homes = Vec::new();
    let mut total = LinkStats::default();
    for home in crate::gc::rustup_homes_for_store(Some(&paths)) {
        let stats = link_rustup_home(&store, &home);
        total.add(&stats);
        if !json {
            eprintln!(
                "soldr toolchain dedupe: {}: {} files, {} deduplicated ({}), {} new objects, {} skipped",
                home.display(),
                stats.files,
                stats.deduplicated_files,
                crate::cache_lib::target_registry::human_size(stats.deduplicated_bytes),
                stats.new_objects,
                stats.skipped_files
            );
        }
        homes.push(serde_json::json!({ "rustup_home": home, "stats": stats }));
    }
    let (removed_objects, removed_bytes) = gc_unreferenced_objects(&store);
    if json {
        let report = serde_json::json!({
            "schema_version": 1,
            "store": store,
            "homes": homes,
            "total": total,
            "removed_objects": removed_objects,
            "removed_bytes": removed_bytes,
        });
        println!("{report}");
    } else {
        eprintln!(
            "soldr toolchain dedupe: freed {} across {} homes; store {}",
            crate::cache_lib::target_registry::human_size(total.deduplicated_bytes),
            homes.len(),
            store.display()
        );
    }
    Ok(0)
}

#[cfg(test)]
#[path = "toolchain_store_tests.rs"]
mod tests;
