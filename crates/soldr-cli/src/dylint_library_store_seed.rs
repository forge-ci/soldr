//! Seed changed lint sets without sharing writable fingerprints or outputs.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use fs2::FileExt;

const LIBRARIES: &str = ".soldr-libraries";

pub(super) fn record_libraries(store: &Path, libraries: &[PathBuf]) -> std::io::Result<()> {
    let inventory = store.join(LIBRARIES);
    std::fs::create_dir_all(&inventory)?;
    for name in library_names(libraries) {
        std::fs::write(inventory.join(name), [])?;
    }
    Ok(())
}

fn library_names(libraries: &[PathBuf]) -> BTreeSet<String> {
    libraries
        .iter()
        .filter_map(|p| p.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

fn candidate_names(store: &Path, toolchain: &str) -> BTreeSet<String> {
    if let Ok(entries) = std::fs::read_dir(store.join(LIBRARIES)) {
        return entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
    }
    // Existing stores predate the manifest. Their stamped cdylibs identify lints.
    std::fs::read_dir(store.join(toolchain).join("release"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let (name, _) = name.split_once('@')?;
            Some(name.strip_prefix("lib").unwrap_or(name).to_owned())
        })
        .collect()
}

pub(super) fn seed_store(
    root: &Path,
    store: &Path,
    channel: &str,
    triple: &str,
    libraries: &[PathBuf],
) -> std::io::Result<()> {
    if store.exists() {
        return Ok(());
    }
    let toolchain = format!("{channel}-{triple}");
    let prefix = format!("{toolchain}-");
    let legacy_prefix = format!("{channel}-");
    let names = library_names(libraries);
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let legacy = name
            .strip_prefix(&legacy_prefix)
            .is_some_and(|key| key.len() == 16 && key.bytes().all(|byte| byte.is_ascii_hexdigit()));
        if !entry.file_type()?.is_dir() || !(name.starts_with(&prefix) || legacy) {
            continue;
        }
        let path = entry.path();
        if !path.join(&toolchain).is_dir() {
            continue;
        }
        let shared = candidate_names(&path, &toolchain)
            .intersection(&names)
            .count();
        let modified = std::fs::metadata(path.join(".last-used"))
            .or_else(|_| path.metadata())?
            .modified()?;
        candidates.push((shared, modified, path));
    }
    candidates.sort_by(|a, b| b.cmp(a));
    for (_, _, source) in candidates {
        // Never snapshot a live build writer. This is deliberately nonblocking.
        let lock_path = source.join(&toolchain).join("release/.cargo-lock");
        let Ok(lock) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock_path)
        else {
            continue;
        };
        if lock.try_lock_exclusive().is_err() {
            continue;
        }
        let staging = tempfile::Builder::new().prefix(".seed-").tempdir_in(root)?;
        copy_tree(&source, staging.path(), &source, store)?;
        std::fs::rename(staging.path(), store)?;
        return Ok(());
    }
    Ok(())
}

fn copy_tree(
    source: &Path,
    target: &Path,
    source_root: &Path,
    final_root: &Path,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        // Script output can contain absolute donor paths (for example native
        // link-search directories). Reuse the executable, but rerun the script
        // so its output describes this store before dependent units are reused.
        if source
            .parent()
            .is_some_and(|parent| parent.file_name() == Some(std::ffi::OsStr::new(".fingerprint")))
            && name
                .to_str()
                .is_some_and(|name| name.starts_with("run-build-script-"))
        {
            continue;
        }
        // Backlinks/recency belong to the new key; locks and incremental
        // state cannot be reused. Links must stay inside the source store.
        if matches!(
            name.to_str(),
            Some(
                ".soldr-links" | ".soldr-libraries" | ".last-used" | ".cargo-lock" | "incremental"
            )
        ) {
            continue;
        }
        let destination = target.join(name);
        let kind = entry.file_type()?;
        if crate::platform::fs::links::is_link_or_reparse(&std::fs::symlink_metadata(entry.path())?)
        {
            copy_link(&entry.path(), &destination, source_root, final_root)?;
        } else if kind.is_dir() {
            std::fs::create_dir(&destination)?;
            copy_tree(&entry.path(), &destination, source_root, final_root)?;
        } else if kind.is_file() {
            if soldr_platform::fs::reflink::reflink_file(&entry.path(), &destination).is_err() {
                std::fs::copy(entry.path(), &destination)?;
            }
            // Fresh timestamps would make byte-identical fingerprints dirty.
            let metadata = entry.metadata()?;
            filetime::set_file_times(
                &destination,
                filetime::FileTime::from_last_access_time(&metadata),
                filetime::FileTime::from_last_modification_time(&metadata),
            )?;
            std::fs::set_permissions(&destination, metadata.permissions())?;
        }
    }
    Ok(())
}

fn copy_link(
    source: &Path,
    target: &Path,
    source_root: &Path,
    final_root: &Path,
) -> std::io::Result<()> {
    let resolved = source.canonicalize()?;
    let root = source_root.canonicalize()?;
    let relative = resolved.strip_prefix(&root).map_err(|error| {
        std::io::Error::other(format!(
            "cannot seed external build-output link {}: {error}",
            source.display()
        ))
    })?;
    let link = std::fs::read_link(source)?;
    let link = if link.is_absolute() {
        final_root.join(relative)
    } else {
        link
    };
    crate::platform::fs::links::create(&link.to_string_lossy(), target, resolved.is_dir())
}

#[cfg(test)]
#[path = "dylint_library_store_seed_tests.rs"]
mod tests;
