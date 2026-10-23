//! Keep compiler snapshots portable without moving live daemon directories.

use std::path::{Path, PathBuf};

use crate::cache_lib::save::CacheProjection;
use soldr_daemon::zccache_embedded_snapshot as backend;

pub(super) struct PreparedSnapshot {
    _temporary: tempfile::TempDir,
    source: PathBuf,
    archive_prefix: PathBuf,
    exclude_prefix: PathBuf,
}

impl PreparedSnapshot {
    pub(super) fn projection(&self) -> CacheProjection<'_> {
        CacheProjection {
            source: &self.source,
            archive_prefix: &self.archive_prefix,
            exclude_prefix: &self.exclude_prefix,
        }
    }
}

fn archive_cache_prefix(
    cache_dir: &Path,
    paths: &crate::core::SoldrPaths,
) -> Result<PathBuf, String> {
    let cache = super::path_for_containment(cache_dir)?;
    let owned = super::path_for_containment(&paths.cache)?;
    owned
        .strip_prefix(&cache)
        .map(Path::to_path_buf)
        .map_err(|_| {
            "compiler snapshot transport requires the Soldr cache directory or its parent".into()
        })
}

pub(super) fn prepare(cache_dir: Option<&Path>) -> Result<Option<PreparedSnapshot>, String> {
    let Some(cache_dir) = cache_dir else {
        return Ok(None);
    };
    let paths = crate::core::SoldrPaths::new().map_err(|error| error.to_string())?;
    if !super::archive_contains_embedded_cache(
        cache_dir,
        &crate::zccache_embedded::embedded_cache_root(&paths),
    )? {
        return Ok(None);
    }
    let prefix = archive_cache_prefix(cache_dir, &paths)?;
    let temporary = tempfile::Builder::new()
        .prefix("soldr-compiler-snapshot-")
        .tempdir()
        .map_err(|error| error.to_string())?;
    let source = temporary.path().join("snapshot");
    let receipt = backend::export(&paths, &source).map_err(|error| error.to_string())?;
    let Some(receipt) = receipt else {
        return Ok(None);
    };
    eprintln!(
        "soldr save: compiler snapshot entries={} outputs={}",
        receipt.entries, receipt.outputs
    );
    Ok(Some(PreparedSnapshot {
        _temporary: temporary,
        source,
        archive_prefix: prefix.join(backend::ARCHIVE_PREFIX),
        exclude_prefix: prefix.join(backend::PRIVATE_PREFIX),
    }))
}

pub(super) fn restore(cache_dir: Option<&Path>) -> Result<(), String> {
    let Some(cache_dir) = cache_dir else {
        return Ok(());
    };
    let paths = crate::core::SoldrPaths::new().map_err(|error| error.to_string())?;
    if !super::archive_contains_embedded_cache(
        cache_dir,
        &crate::zccache_embedded::embedded_cache_root(&paths),
    )? {
        return Ok(());
    }
    let source = cache_dir
        .join(archive_cache_prefix(cache_dir, &paths)?)
        .join(backend::ARCHIVE_PREFIX);
    if !source.exists() {
        return Ok(()); // Historical archives have no portable backend snapshot.
    }
    match backend::import(&paths, &source).map_err(|error| error.to_string())? {
        Some(receipt) => eprintln!(
            "soldr load: compiler snapshot entries={} outputs={}",
            receipt.entries, receipt.outputs
        ),
        None => eprintln!("soldr load: compiler snapshot kept the existing store"),
    }
    Ok(())
}
