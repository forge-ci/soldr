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

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CLOSURE_CACHE_NAME: &str = ".soldr-dependency-closure-v1.json";

#[derive(Serialize, Deserialize)]
struct ClosureCache {
    fingerprint: String,
    closure: String,
}

/// A standalone lint with a lockfile containing only itself as a path package
/// has a stable dependency graph until one of its dependency inputs changes.
/// Other workspace shapes continue through graph resolution on every run.
fn closure_fingerprint(workspace_root: &Path, libraries: &[PathBuf]) -> Option<String> {
    let mut hasher = Sha256::new();
    hash_input(
        &mut hasher,
        b"workspace",
        workspace_root.as_os_str().as_encoded_bytes(),
    );
    for name in KEY_FILES {
        hash_input(
            &mut hasher,
            name.as_bytes(),
            &std::fs::read(workspace_root.join(name)).unwrap_or_default(),
        );
    }
    for library in libraries {
        let manifest_path = library.join(KEY_FILES[0]);
        let manifest = std::fs::read(&manifest_path).ok()?;
        let parsed: toml::Value = toml::from_str(std::str::from_utf8(&manifest).ok()?).ok()?;
        if parsed
            .get("workspace")
            .and_then(toml::Value::as_table)
            .is_none()
        {
            return None;
        }
        let lock = std::fs::read(manifest_path.with_extension("lock")).ok()?;
        let parsed_lock: toml::Value = toml::from_str(std::str::from_utf8(&lock).ok()?).ok()?;
        let packages = parsed_lock.get("package")?.as_array()?;
        let path_packages: Vec<_> = packages
            .iter()
            .filter(|package| package.get("source").is_none())
            .collect();
        if path_packages.len() != 1
            || path_packages[0].get("name")?.as_str()? != library.file_name()?.to_str()?
        {
            return None;
        }
        hash_input(
            &mut hasher,
            b"library",
            library.file_name()?.as_encoded_bytes(),
        );
        hash_input(&mut hasher, b"manifest", &manifest);
        hash_input(&mut hasher, b"lock", &lock);
        for ancestor in library.ancestors() {
            for name in [".cargo/config", ".cargo/config.toml"] {
                hash_cargo_config(&mut hasher, &ancestor.join(name))?;
            }
        }
        for name in &KEY_FILES[1..] {
            hash_input(
                &mut hasher,
                name.as_bytes(),
                &std::fs::read(library.join(name)).unwrap_or_default(),
            );
        }
    }
    for root in [
        std::env::var_os("CARGO_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")),
    ]
    .into_iter()
    .flatten()
    {
        for name in ["config", "config.toml"] {
            hash_cargo_config(&mut hasher, &root.join(name))?;
        }
    }
    let mut variables: Vec<_> = std::env::vars_os()
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            name.starts_with("CARGO_") || name.starts_with("RUSTUP_") || name == "RUSTFLAGS"
        })
        .collect();
    variables.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, value) in variables {
        hash_input(
            &mut hasher,
            name.as_encoded_bytes(),
            value.as_encoded_bytes(),
        );
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn write_closure_cache(path: &Path, cache: &ClosureCache) -> std::io::Result<()> {
    let parent = path.parent().expect("closure cache has a parent");
    std::fs::create_dir_all(parent)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut staged, cache)?;
    staged.persist(path)?;
    Ok(())
}

fn hash_cargo_config(hasher: &mut Sha256, path: &Path) -> Option<()> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(()),
        Err(_) => return None,
    };
    let config: toml::Table = toml::from_str(&content).ok()?;
    if ["include", "patch", "paths"]
        .iter()
        .any(|key| config.contains_key(*key))
    {
        return None;
    }
    hash_input(
        hasher,
        path.as_os_str().as_encoded_bytes(),
        content.as_bytes(),
    );
    Some(())
}

fn hash_input(hasher: &mut Sha256, name: &[u8], bytes: &[u8]) {
    hasher.update((name.len() as u64).to_le_bytes());
    hasher.update(name);
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

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
    let inputs = StoreInputs::resolve(workspace_root)?;
    Some(inputs.store_dir(store_root, channel, triple))
}

/// The target-independent half of a store key: the lint libraries and
/// their resolved dependency closure. Resolving the closure runs `cargo
/// metadata` once for the workspace and once per standalone library, so a
/// run that keys several targets resolves it once and reuses it.
struct StoreInputs {
    libraries: Vec<PathBuf>,
    closure: String,
}

impl StoreInputs {
    fn resolve(workspace_root: &Path) -> Option<Self> {
        let libraries = crate::dylint_libraries::library_directories(workspace_root).ok()?;
        if libraries.is_empty() {
            return None;
        }
        let closure = metadata::dependency_closure(workspace_root, &libraries).ok()?;
        Some(Self { libraries, closure })
    }

    fn resolve_cached(workspace_root: &Path, target_dir: &Path) -> Option<Self> {
        let libraries = crate::dylint_libraries::library_directories(workspace_root).ok()?;
        if libraries.is_empty() {
            return None;
        }
        let fingerprint = closure_fingerprint(workspace_root, &libraries);
        let cache_path = target_dir.join("dylint").join(CLOSURE_CACHE_NAME);
        let cached = fingerprint.as_ref().and_then(|fingerprint| {
            let bytes = std::fs::read(&cache_path).ok()?;
            let cache: ClosureCache = serde_json::from_slice(&bytes).ok()?;
            (cache.fingerprint == *fingerprint).then_some(cache)
        });
        if let Some(cache) = cached {
            return Some(Self {
                libraries,
                closure: cache.closure,
            });
        }
        let closure = metadata::dependency_closure(workspace_root, &libraries).ok()?;
        if let Some(fingerprint) = fingerprint.filter(|before| {
            closure_fingerprint(workspace_root, &libraries).as_ref() == Some(before)
        }) {
            let cache = ClosureCache {
                fingerprint,
                closure: closure.clone(),
            };
            let _ = write_closure_cache(&cache_path, &cache);
        }
        Some(Self { libraries, closure })
    }

    fn store_dir(&self, store_root: &Path, channel: &str, triple: &str) -> PathBuf {
        let Self { libraries, closure } = self;
        let mut hasher = Sha256::new();
        hasher.update(b"soldr-dylint-libraries-v2\0");
        hasher.update(channel.as_bytes());
        hasher.update(b"\0target\0");
        hasher.update(triple.as_bytes());
        hasher.update(closure.as_bytes());
        for library in libraries {
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
        store_root.join(format!("{channel}-{triple}-{key}"))
    }
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
        let Some(inputs) = StoreInputs::resolve_cached(workspace_root, target_dir) else {
            return Ok(());
        };
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
            let store = inputs.store_dir(&store_root, channel, triple);
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
