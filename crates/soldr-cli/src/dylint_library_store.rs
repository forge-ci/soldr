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
//! declared library (`Cargo.toml`, resolved dependency closure, `rust-toolchain[.toml]`), so
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
const KEY_FILES: &[&str] = &["Cargo.toml", "rust-toolchain", "rust-toolchain.toml"];

/// The store directory for `workspace_root`'s declared libraries under
/// `channel`, or `None` when the workspace declares no path libraries.
pub(crate) fn store_dir(
    store_root: &Path,
    workspace_root: &Path,
    channel: &str,
) -> Option<PathBuf> {
    store_dir_for_target(
        store_root,
        workspace_root,
        channel,
        crate::pyo3_detect::host_triple(),
    )
}

fn store_dir_for_target(
    store_root: &Path,
    workspace_root: &Path,
    channel: &str,
    triple: &str,
) -> Option<PathBuf> {
    let libraries = crate::dylint_libraries::library_directories(workspace_root).ok()?;
    if libraries.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(b"soldr-dylint-libraries-v2\0");
    hasher.update(channel.as_bytes());
    hasher.update(b"\0target\0");
    hasher.update(triple.as_bytes());
    let closure = metadata::dependency_closure(workspace_root, &libraries).ok()?;
    hasher.update(closure.as_bytes());
    for library in &libraries {
        // The key names the library by its directory name, not by its path
        // inside this checkout: a subtree worker roots the repository one
        // level down (`sub/dylints/one` instead of `dylints/one`), and the
        // bytes are identical, so keying on the path made it rebuild the
        // whole 350 MB tree for a layout difference alone. Two libraries in
        // one workspace can never share a directory name.
        let name = library.file_name().unwrap_or_else(|| library.as_os_str());
        hasher.update(b"\0lib\0");
        hasher.update(name.to_string_lossy().replace('\\', "/").as_bytes());
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
    Some(store_root.join(format!("{channel}-{triple}-{key}")))
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
    args: &[String],
) {
    let Some(target_dir) = target_dir else {
        return;
    };
    if crate::dylint_libraries::library_directories(workspace_root)
        .is_ok_and(|libraries| libraries.is_empty())
    {
        return;
    }
    let store_root = paths.root.join("dylint").join("libraries");
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let _lock = gc::lock_store(&store_root)?;
        let targets = invocation_targets(workspace_root, args)?;
        for requested in std::iter::once(None).chain(targets.into_iter().map(Some)) {
            let host = requested.is_none();
            let triple = requested
                .as_deref()
                .unwrap_or(crate::pyo3_detect::host_triple());
            let target = if host {
                target_dir.to_path_buf()
            } else {
                target_dir.join(triple)
            };
            let store = if host {
                store_dir(&store_root, workspace_root, channel)
            } else {
                store_dir_for_target(&store_root, workspace_root, channel, triple)
            };
            let Some(store) = store else { continue };
            link_libraries_dir(&target, &store)?;
            gc::register_link(&target, &store)?;
        }
        drop(_lock);
        gc::prune(&store_root, 4, false)?;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!(
            "soldr: could not share Dylint library builds ({error}); building locally instead"
        );
    }
}

/// Cargo CLI targets override configuration; always retain the host build above.
fn invocation_targets(
    root: &Path,
    args: &[String],
) -> Result<Vec<String>, crate::core::SoldrError> {
    // Reuse Dylint's parser, considering targets on either side of its separator.
    let flattened: Vec<_> = args
        .iter()
        .filter(|arg| arg.as_str() != "--")
        .cloned()
        .collect();
    let mut forwarded = vec!["--".to_string()];
    forwarded.extend(flattened);
    let explicit = crate::dylint_target::requested_targets(&forwarded)?;
    if !explicit.is_empty() {
        return Ok(explicit);
    }
    if let Ok(target) = std::env::var("CARGO_BUILD_TARGET") {
        if !target.is_empty() {
            return Ok(vec![target]);
        }
    }
    for ancestor in root.ancestors() {
        let config = ["config", "config.toml"]
            .into_iter()
            .map(|name| ancestor.join(".cargo").join(name))
            .find(|path| path.is_file());
        if let Some(config) = config {
            let text = std::fs::read_to_string(&config)
                .map_err(|e| crate::core::SoldrError::Other(e.to_string()))?;
            let value: toml::Value =
                toml::from_str(&text).map_err(|e| crate::core::SoldrError::Other(e.to_string()))?;
            if let Some(target) = value.get("build").and_then(|v| v.get("target")) {
                return Ok(if let Some(target) = target.as_str() {
                    vec![target.to_string()]
                } else {
                    target
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                });
            }
        }
    }
    Ok(Vec::new())
}

#[path = "dylint_library_store_gc.rs"]
mod gc;
#[path = "dylint_library_store_metadata.rs"]
mod metadata;
pub(crate) use gc::run;

#[cfg(test)]
#[path = "dylint_library_store_tests.rs"]
mod tests;
