//! Portable transport adapter. The backend owns its snapshot format and locks.

use std::{io, path::Path};

use crate::core::SoldrPaths;
use zccache::artifact::snapshot::{export_store_snapshot, import_snapshot, SnapshotReceipt};

/// Stable logical archive path, independent of the broker's execution route.
pub const ARCHIVE_PREFIX: &str = "zccache/compiler-snapshot-v1";
pub const PRIVATE_PREFIX: &str = "zccache/daemon-state";

fn compatibility() -> String {
    // Store format compatibility is separate from compiler compatibility:
    // the backend still checks each compiler context when replaying an object.
    let identity = format!(
        "soldr-embedded-snapshot-v1:{}",
        zccache::core::config::versioned_subdir()
    );
    zccache::hash::hash_bytes(identity.as_bytes())
        .to_hex()
        .to_string()
}

/// The dep-graph file under a store version root.
///
/// Perf Matrix `cold-tar-untar-warm` (0% rustc hits since the per-route
/// private caches of soldr#3568): the backend's store snapshot carries the
/// artifacts and their index but not the dep-graph, and a rustc lookup needs
/// the unit's dep-graph *context* before it consults the index. A restored
/// store therefore missed every unit with `context_not_found`. The adapter
/// carries the dep-graph beside the store snapshot. Its compatibility is the
/// same store version [`compatibility`] already pins.
const DEPGRAPH: &str = "depgraph/depgraph.bin";

/// The caller must checkpoint and gracefully stop the selected daemon first.
pub fn export(paths: &SoldrPaths, destination: &Path) -> io::Result<Option<SnapshotReceipt>> {
    let source = crate::zccache_embedded::embedded_version_root(paths);
    if !source.exists() {
        return Ok(None);
    }
    let receipt = export_store_snapshot(&source, &compatibility(), destination)?;
    copy_if_present(&source.join(DEPGRAPH), &destination.join(DEPGRAPH))?;
    Ok(Some(receipt))
}

/// Import before daemon startup. Existing destination stores are never
/// replaced, and neither is an existing dep-graph: a repeated restore into a
/// populated root keeps that store and returns `None`.
pub fn import(paths: &SoldrPaths, source: &Path) -> io::Result<Option<SnapshotReceipt>> {
    let destination = crate::zccache_embedded::embedded_version_root(paths);
    if destination.symlink_metadata().is_ok() {
        return Ok(None);
    }
    let receipt = import_snapshot(source, &compatibility(), &destination)?;
    let target = destination.join(DEPGRAPH);
    if !target.exists() {
        copy_if_present(&source.join(DEPGRAPH), &target)?;
    }
    Ok(Some(receipt))
}

fn copy_if_present(from: &Path, to: &Path) -> io::Result<()> {
    if !from.is_file() {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(from, to).map(|_| ())
}
