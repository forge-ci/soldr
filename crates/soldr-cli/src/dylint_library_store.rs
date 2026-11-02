//! One central store for built Dylint lint libraries (soldr#4030).
//!
//! `cargo-dylint` builds every declared lint library into
//! `<target>/dylint/libraries/<toolchain>-<host>/`. That tree holds the whole
//! `rustc_private` + `clippy_utils` dependency graph, roughly 350 MB, and it
//! was rebuilt from scratch in every worktree of the same repository.
//!
//! Soldr now points `<target>/dylint/libraries` at
//! `~/.soldr/dylint/libraries/<toolchain>-<key>/` with a directory link. The
//! key hashes the toolchain and the dependency-shaping sources of every
//! declared library (`Cargo.toml`, `Cargo.lock`, `rust-toolchain[.toml]`), so
//! worktrees whose lints share a dependency graph share one build. Edits to a
//! lint's own `src/` deliberately stay out of the key: Cargo rebuilds just
//! that small crate inside the shared tree instead of minting another 350 MB
//! copy per edit. Concurrent worktrees are serialized by Cargo's own build
//! directory lock.
//!
//! Everything here is best effort: when the link cannot be made, the run keeps
//! cargo-dylint's per-worktree default.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Files whose content decides what a lint library's build tree contains.
const KEY_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain",
    "rust-toolchain.toml",
];

/// The store directory for `workspace_root`'s declared libraries under
/// `channel`, or `None` when the workspace declares no path libraries.
pub(crate) fn store_dir(
    store_root: &Path,
    workspace_root: &Path,
    channel: &str,
) -> Option<PathBuf> {
    let libraries = crate::dylint_libraries::library_directories(workspace_root).ok()?;
    if libraries.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(b"soldr-dylint-libraries-v1\0");
    hasher.update(channel.as_bytes());
    for library in &libraries {
        let relative = library.strip_prefix(workspace_root).unwrap_or(library);
        hasher.update(b"\0lib\0");
        hasher.update(relative.to_string_lossy().replace('\\', "/").as_bytes());
        for name in KEY_FILES {
            hasher.update(b"\0file\0");
            hasher.update(name.as_bytes());
            if let Ok(bytes) = std::fs::read(library.join(name)) {
                hasher.update(b"\0present\0");
                hasher.update(&bytes);
            }
        }
    }
    let digest = hasher.finalize();
    let key: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    Some(store_root.join(format!("{channel}-{key}")))
}

/// Link `<target_dir>/dylint/libraries` to `store`. A pre-existing real
/// directory is moved into an empty store (keeping its warm build) or, when
/// the store is already populated, discarded as a redundant copy.
pub(crate) fn link_libraries_dir(target_dir: &Path, store: &Path) -> std::io::Result<()> {
    let link = target_dir.join("dylint").join("libraries");
    if std::fs::read_link(&link).is_ok_and(|current| current == store) && store.is_dir() {
        return Ok(());
    }
    if let Some(parent) = store.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::symlink_metadata(&link) {
        Ok(metadata) if crate::platform::fs::links::is_link_or_reparse(&metadata) => {
            crate::platform::fs::links::remove(&link)?;
        }
        Ok(metadata) if metadata.is_dir() => {
            if store.exists() || std::fs::rename(&link, store).is_err() {
                std::fs::remove_dir_all(&link)?;
            }
        }
        Ok(_) => std::fs::remove_file(&link)?,
        Err(_) => {}
    }
    std::fs::create_dir_all(store)?;
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::platform::fs::links::create(&store.to_string_lossy(), &link, true)
}

/// Front-door entry point: share the library build tree for this run.
pub(crate) fn share_library_build(
    paths: &crate::core::SoldrPaths,
    workspace_root: &Path,
    target_dir: Option<&Path>,
    channel: &str,
) {
    let Some(target_dir) = target_dir else {
        return;
    };
    let store_root = paths.root.join("dylint").join("libraries");
    let Some(store) = store_dir(&store_root, workspace_root, channel) else {
        return;
    };
    if let Err(error) = link_libraries_dir(target_dir, &store) {
        eprintln!(
            "soldr: could not share Dylint library builds via {} ({error}); \
             building them in this worktree instead",
            store.display()
        );
    }
}

#[cfg(test)]
#[path = "dylint_library_store_tests.rs"]
mod tests;
