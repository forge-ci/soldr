//! Target-directory path helpers for [`super`]: resolve an explicit
//! `--target-dir` the way Cargo does, and compare paths that may not exist
//! yet. Split from `nested_cargo_guard.rs` for the 1000-line ceiling.

use std::path::{Component, Path, PathBuf};

/// Resolve an explicit `--target-dir` the way Cargo does: relative to `-C`,
/// which is itself relative to the process's working directory.
pub(super) fn resolve_target_dir(
    path: &str,
    change_dir: Option<&str>,
    cwd: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    let base = match change_dir.map(Path::new) {
        Some(dir) if dir.is_absolute() => dir.to_path_buf(),
        Some(dir) => cwd()?.join(dir),
        None => cwd()?,
    };
    Some(base.join(path))
}

/// Canonicalize the longest existing prefix of `path` (resolving symlinks and
/// junctions) and append the rest, so a target directory Cargo has not
/// created yet still compares against real paths.
pub(crate) fn canonicalish(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(mut resolved) = std::fs::canonicalize(existing) {
            for name in rest.iter().rev() {
                resolved.push(name);
            }
            return resolved;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return lexical_normalize(path),
        }
    }
}

pub(super) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
