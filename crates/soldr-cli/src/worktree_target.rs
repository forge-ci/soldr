//! `soldr doctor`: does this worktree build into another checkout's
//! `target/`? (soldr#3092)
//!
//! Cargo walks up from the build directory for `.cargo/config.toml`, so a
//! git worktree nested under another checkout inherits that checkout's
//! `[build] target-dir` and builds into it. Nothing warns, and N agents end
//! up queued on one `.cargo-lock` and unifying features against each other's
//! incremental state. The probe resolves the target dir Cargo would use from
//! the current directory (the shared resolver, soldr#3203). It reports
//! sharing when that dir lies inside a *different* git checkout than the
//! current one.
//!
//! It is stateless and spawns nothing: both checkout roots come from the
//! nearest `.git` file or directory, the same rule `ZCCACHE_PATH_REMAP=auto`
//! uses. A target dir outside every checkout (a deliberate per-worktree
//! `CARGO_TARGET_DIR` under a cache root) is not reported.

use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct WorktreeTarget {
    /// The checkout the current directory belongs to.
    pub worktree_root: PathBuf,
    /// The target dir Cargo would build into from here.
    pub target_dir: PathBuf,
    /// The other checkout that owns `target_dir`, when it is not this one.
    pub shared_with: Option<PathBuf>,
}

/// Pure classifier: `target_owner` is the checkout containing the target dir.
pub(crate) fn classify(
    worktree_root: &Path,
    target_dir: &Path,
    target_owner: Option<&Path>,
) -> WorktreeTarget {
    let shared_with = target_owner
        .filter(|owner| *owner != worktree_root)
        .map(Path::to_path_buf);
    WorktreeTarget {
        worktree_root: worktree_root.to_path_buf(),
        target_dir: target_dir.to_path_buf(),
        shared_with,
    }
}

/// The checkout containing `path`, judged from its nearest existing ancestor.
fn checkout_root(path: &Path) -> Option<PathBuf> {
    let existing = path.ancestors().find(|candidate| candidate.exists())?;
    crate::zccache::find_git_worktree_root(existing)
}

/// Probe from `cwd` under this process's environment; `None` outside a git
/// checkout.
pub(crate) fn probe(cwd: &Path) -> Option<WorktreeTarget> {
    probe_with(&crate::core::cargo_target_dir::CargoTargetDirInputs::from_process(cwd, &[]))
}

/// [`probe`] over explicit resolver inputs.
pub(crate) fn probe_with(
    inputs: &crate::core::cargo_target_dir::CargoTargetDirInputs,
) -> Option<WorktreeTarget> {
    let worktree_root = crate::zccache::find_git_worktree_root(&inputs.cwd)?;
    let target_dir = crate::core::cargo_target_dir::resolve_cargo_target_dir(inputs)
        .unwrap_or_else(|| worktree_root.join("target"));
    let owner = checkout_root(&target_dir);
    Some(classify(&worktree_root, &target_dir, owner.as_deref()))
}

pub(crate) fn print_human(probe: Option<&WorktreeTarget>) {
    let Some(probe) = probe else {
        return;
    };
    println!();
    println!("worktree target dir (soldr#3092):");
    println!("  worktree:   {}", probe.worktree_root.display());
    println!("  target dir: {}", probe.target_dir.display());
    if let Some(other) = &probe.shared_with {
        println!(
            "  WARNING: this worktree builds into the target dir of another checkout ({}).",
            other.display()
        );
        println!(
            "  Parallel builds there serialize on one .cargo-lock and invalidate each \
             other's incremental state. Give this worktree its own target dir (a \
             `.cargo/config.toml` with `[build] target-dir`, or CARGO_TARGET_DIR); \
             soldr's cache still shares artifacts across worktrees (docs/FILESYSTEMS.md)."
        );
    } else {
        println!("  ok: the target dir belongs to this worktree (or to no checkout)");
    }
}

#[cfg(test)]
#[path = "worktree_target_tests.rs"]
mod tests;
