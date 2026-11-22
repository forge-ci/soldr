//! Portable dependency paths for distributed compiles.
//!
//! rustc records a source path differently when a `--remap-path-prefix` matched
//! it than when none did: the file-name variant enters every source-file and
//! span hash, so an rlib or rmeta changes even though the recorded text is the
//! same. A dist worker reads its inputs from a sandbox mirror and must remap
//! them back to the client's paths, so a dependency outside the workspace (the
//! Cargo registry and git checkouts under `CARGO_HOME`) only reproduces its
//! local bytes if the local compile remapped it too. An identity remap of
//! `CARGO_HOME` makes both sides record the same remapped name. Workspace paths
//! are already remapped on both sides by `ZCCACHE_PATH_REMAP=auto`.
use soldr_core::core::env_flag;
use std::path::{Path, PathBuf};

/// Explicit switch; dist client mode (`SOLDR_DIST_PEERS`) enables it too.
pub(crate) const PORTABLE_PATHS_ENV: &str = "SOLDR_DIST_PORTABLE_PATHS";

/// Remap arguments to put *before* the crate's own arguments. rustc lets a
/// later remap win, so zccache's workspace remap and any user remap still
/// take precedence where they overlap.
pub(crate) fn portable_path_remaps() -> Vec<String> {
    let dist_client = std::env::var_os("SOLDR_DIST_PEERS").is_some_and(|v| !v.is_empty());
    if !(dist_client || env_flag::flag(PORTABLE_PATHS_ENV)) {
        return vec![];
    }
    remaps_for(cargo_home().as_deref())
}

/// `argv` (`[compiler, args...]`) with the portable remaps inserted after the
/// compiler, or `None` when there is nothing to add or it is not a rustc call.
pub(crate) fn with_portable_paths(argv: &[String]) -> Option<Vec<String>> {
    if !argv.iter().any(|arg| arg == "--crate-name") {
        return None;
    }
    let remaps = portable_path_remaps();
    if remaps.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(argv.len() + remaps.len());
    out.push(argv.first()?.clone());
    out.extend(remaps);
    out.extend_from_slice(&argv[1..]);
    Some(out)
}

/// Independent of the working directory on purpose: Cargo compiles a registry
/// crate with its cwd *inside* `CARGO_HOME`, which is exactly the case this
/// remap exists for. A workspace under `CARGO_HOME` still gets its own remap,
/// because rustc applies the last matching `--remap-path-prefix`.
fn remaps_for(cargo_home: Option<&Path>) -> Vec<String> {
    match cargo_home {
        Some(home) if home.is_absolute() => {
            vec![format!("--remap-path-prefix={0}={0}", home.display())]
        }
        _ => vec![],
    }
}

fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
}

#[cfg(test)]
mod tests {
    use super::remaps_for;
    use std::path::Path;

    #[test]
    fn identity_remap_covers_cargo_home_whatever_the_cwd() {
        // Registry crates compile with their cwd inside CARGO_HOME; the remap
        // must not depend on it (an earlier cwd guard silently disabled it).
        let home = std::env::temp_dir().join(".cargo");
        assert_eq!(
            remaps_for(Some(&home)),
            [format!("--remap-path-prefix={0}={0}", home.display())]
        );
        assert!(remaps_for(Some(Path::new("relative"))).is_empty());
        assert!(remaps_for(None).is_empty());
    }
}
