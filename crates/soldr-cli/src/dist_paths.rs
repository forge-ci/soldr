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
pub(crate) fn portable_path_remaps(cwd: &Path) -> Vec<String> {
    let dist_client = std::env::var_os("SOLDR_DIST_PEERS").is_some_and(|v| !v.is_empty());
    if !(dist_client || env_flag::flag(PORTABLE_PATHS_ENV)) {
        return vec![];
    }
    remaps_for(cargo_home().as_deref(), cwd)
}

fn remaps_for(cargo_home: Option<&Path>, cwd: &Path) -> Vec<String> {
    match cargo_home {
        // A workspace inside CARGO_HOME (e.g. a git checkout being built) keeps
        // its own workspace remap; an identity remap there would be redundant.
        Some(home) if home.is_absolute() && !cwd.starts_with(home) => {
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

#[cfg(all(test, unix))]
mod tests {
    use super::remaps_for;
    use std::path::Path;

    #[test]
    fn identity_remap_covers_cargo_home_outside_the_workspace() {
        let home = Path::new("/home/u/.cargo");
        assert_eq!(
            remaps_for(Some(home), Path::new("/home/u/dev/app")),
            ["--remap-path-prefix=/home/u/.cargo=/home/u/.cargo"]
        );
        assert!(remaps_for(Some(home), Path::new("/home/u/.cargo/git/checkouts/x")).is_empty());
        assert!(remaps_for(Some(Path::new("relative")), Path::new("/w")).is_empty());
        assert!(remaps_for(None, Path::new("/w")).is_empty());
    }
}
