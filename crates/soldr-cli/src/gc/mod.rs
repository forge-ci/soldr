//! Garbage-collect stale Cargo `target/` directories and orchestrate
//! the auto-GC tiers. Extracted from `main.rs` as part of issue #339;
//! split into sub-modules in 2026-05 once the production surface
//! crossed 2k LOC.
//!
//! Sub-module map:
//!
//! - [`walks`] — filesystem walkers for `cargo_registry_src` and
//!   `cargo_git_checkouts` plus the shared directory sizer.
//! - [`purge`] — confirmation prompt, parallel deletion worker pool,
//!   `gc purge --registry-src` / `--git-checkouts` commands, and the
//!   per-summary helpers used by the top-level dispatch entry.
//! - [`rustup_toolchain`] — `gc purge --kind rustup_toolchain`
//!   (soldr#3507): enumeration of the caller + managed rustup homes,
//!   the protected-name selection rails, and the delegation to
//!   `rustup toolchain uninstall`.
//! - [`cargo_native`] — `gc cargo`, `gc locations`, `gc sweep` and
//!   the nightly-cargo wrapper `invoke_cargo_native_gc`.
//! - [`auto`] — auto-GC background machinery driven by the cargo
//!   front door (`maybe_kick_auto_gc`).

use crate::cache::print_json;
use crate::core::{SoldrError, SoldrPaths};
use serde::Serialize;

mod auto;
mod cargo_native;
mod delete_diagnosis;
mod discovery;
pub(crate) mod disk;
mod holding_process;
mod purge;
mod rustup_toolchain;
mod stale_versions;
pub(crate) mod target_dedupe;
pub(crate) use target_dedupe::run_gc_dedupe_command;
pub(crate) mod target_walker;
mod walks;

// Re-export the public CLI surface so `crate::gc::*` keeps matching
// the call sites in `main.rs` and `cargo_front_door.rs`.
pub(crate) use auto::{maybe_spawn_auto_gc_sweeper, run_gc_auto_sweep_command};
pub(crate) use cargo_native::{
    run_gc_cargo_command, run_gc_locations_command, run_gc_sweep_command,
};
pub(crate) use purge::{
    run_gc_purge_git_checkouts_command, run_gc_purge_registry_src_command,
    run_gc_purge_target_subtree_command,
};
pub(crate) use rustup_toolchain::{run_gc_purge_rustup_toolchain_command, rustup_homes_for_store};

// Items the tests file reaches through `super::*`. Keeping these
// `use`d inside `mod.rs` makes the visibility explicit and survives
// future renames without spelunking through three sub-modules.
#[cfg(test)]
use purge::{gc_purge_worker_count_for, parse_gc_purge_answer};
#[cfg(test)]
use walks::{
    resolve_git_checkout_last_used, resolve_registry_src_last_used, walk_cargo_git_checkouts,
    walk_cargo_registry_src,
};

use purge::{
    gc_candidate_output, gc_largest_candidates, gc_total_reclaimable_bytes, print_gc_purge_result,
    print_gc_purge_scan, print_gc_summary, resolve_gc_dev_roots, run_gc_purge_candidates,
};

pub(super) fn daemon_registry_rows(
    paths: &SoldrPaths,
) -> Result<Vec<crate::cache_lib::target_registry::TargetRow>, SoldrError> {
    let sock = crate::daemon::client::default_sock_path(paths);
    let rows = match crate::daemon::client::list_target_registry(&sock) {
        Ok(rows) => rows,
        Err(daemon_error) => match offline_registry_rows(paths)? {
            Some(rows) => return Ok(rows),
            None => {
                return Err(SoldrError::Other(format!(
                    "daemon target registry unavailable while the daemon owns this root: {daemon_error:?}"
                )));
            }
        },
    };
    Ok(rows
        .into_iter()
        .map(|row| crate::cache_lib::target_registry::TargetRow {
            path: std::path::PathBuf::from(row.path),
            last_used: row.last_used,
        })
        .collect())
}

/// Read the registry only when no daemon owns this Soldr root.
///
/// `RootOwnershipGuard` is the same OS lock the daemon holds for its full
/// lifetime. Acquiring it makes this an explicit offline operation: no daemon
/// can start, and a live daemon cannot be mistaken for an unavailable one
/// between the failed IPC probe and the redb open.
fn offline_registry_rows(
    paths: &SoldrPaths,
) -> Result<Option<Vec<crate::cache_lib::target_registry::TargetRow>>, SoldrError> {
    let Some(_owner) =
        crate::daemon::lifecycle::RootOwnershipGuard::try_acquire(paths).map_err(|error| {
            SoldrError::Other(format!("acquire offline registry ownership: {error}"))
        })?
    else {
        return Ok(None);
    };
    let db_path = crate::cache_lib::data_db_path(paths);
    // soldr-state-db: offline-root-owner
    let registry = crate::cache_lib::target_registry::TargetRegistry::open(&db_path)
        .map_err(|error| SoldrError::Other(format!("open offline target registry: {error}")))?;
    let rows = registry
        .list()
        .map_err(|error| SoldrError::Other(format!("list offline target registry: {error}")))?;
    Ok(Some(rows))
}

pub(super) fn daemon_remove_registry_rows(
    paths: &SoldrPaths,
    rows: Vec<std::path::PathBuf>,
) -> Result<usize, SoldrError> {
    let sock = crate::daemon::client::default_sock_path(paths);
    let encoded_paths = rows
        .into_iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    match crate::daemon::client::remove_target_registry(&sock, encoded_paths.clone()) {
        Ok(removed) => Ok(removed as usize),
        Err(daemon_error) => {
            let Some(_owner) = crate::daemon::lifecycle::RootOwnershipGuard::try_acquire(paths)
                .map_err(|error| {
                    SoldrError::Other(format!("acquire offline registry ownership: {error}"))
                })?
            else {
                return Err(SoldrError::Other(format!(
                    "daemon target registry unavailable while the daemon owns this root: {daemon_error:?}"
                )));
            };
            let db_path = crate::cache_lib::data_db_path(paths);
            // soldr-state-db: offline-root-owner
            let registry = crate::cache_lib::target_registry::TargetRegistry::open(&db_path)
                .map_err(|error| {
                    SoldrError::Other(format!("open offline target registry: {error}"))
                })?;
            registry
                .remove_many(
                    &encoded_paths
                        .into_iter()
                        .map(std::path::PathBuf::from)
                        .collect::<Vec<_>>(),
                )
                .map_err(|error| {
                    SoldrError::Other(format!("remove offline target registry rows: {error}"))
                })
        }
    }
}

pub(super) fn daemon_gc_scan(
    paths: &SoldrPaths,
    options: &crate::cache_lib::gc::GcOptions,
) -> Result<crate::cache_lib::gc::GcReport, SoldrError> {
    let rows = daemon_registry_rows(paths)?;
    let (live, missing): (Vec<_>, Vec<_>) = rows.into_iter().partition(|row| row.path.exists());
    let dropped_missing =
        daemon_remove_registry_rows(paths, missing.into_iter().map(|row| row.path).collect())?;
    crate::cache_lib::gc::scan_daemon_snapshot(live, dropped_missing, options)
        .map_err(|error| SoldrError::Other(format!("gc scan failed: {error}")))
}

// ---------------------------------------------------------------------------
// soldr gc — garbage-collect stale Cargo target/ directories.
// ---------------------------------------------------------------------------

/// Delete one `target/` for `gc target --purge`.
///
/// A directory that is already gone is **success**, not a failure. GC walks a
/// plan built earlier in the run, so another process — or the user, or an
/// earlier entry whose parent contained this one — can legitimately have
/// removed it in between. Counting that as a failure both inflates the
/// failure count and makes the command exit non-zero for work that is done
/// (soldr#2199).
fn remove_target_dir(dir: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

// Slice 1 of #323: scaffold the kind/purge_safety discriminator on gc JSON
// entries. Every entry emitted today is a workspace target/ dir, so every
// entry is tagged `cargo_target` / `derived`. Later slices add walkers that
// emit entries with other kinds (registry-src, git-checkouts, in-target
// subtrees, …) without further schema churn.
pub(super) const KIND_CARGO_TARGET: &str = "cargo_target";
pub(super) const KIND_CARGO_TARGET_INCREMENTAL: &str = "cargo_target_incremental";
pub(super) const KIND_CARGO_TARGET_BUILD_SCRIPT_BINARIES: &str =
    "cargo_target_build_script_binaries";
pub(super) const KIND_CARGO_TARGET_DOC: &str = "cargo_target_doc";
pub(super) const KIND_CARGO_TARGET_SUBCOMMAND_CACHES: &str = "cargo_target_subcommand_caches";
// Slice 2 of #323: `$CARGO_HOME/registry/src/<reg>/<crate>-<vers>/` extracted
// crate sources. `purge_safety: derived` — cargo regenerates from the
// matching `.crate` tarball in `registry/cache/` on demand.
pub(super) const KIND_CARGO_REGISTRY_SRC: &str = "cargo_registry_src";
pub(super) const KIND_CARGO_REGISTRY_CACHE: &str = "cargo_registry_cache";
// Slice 3 of #323: `$CARGO_HOME/git/checkouts/<repo>/<commit>/` git-source
// crate checkouts. `purge_safety: derived` — cargo regenerates by re-checking
// out the bare repo in `$CARGO_HOME/git/db/<repo>/` on demand.
pub(super) const KIND_CARGO_GIT_CHECKOUTS: &str = "cargo_git_checkouts";
pub(super) const KIND_CARGO_GIT_DB: &str = "cargo_git_db";
pub(super) const KIND_CARGO_INSTALLED_BINARIES: &str = "cargo_installed_binaries";
pub(super) const KIND_RUSTUP_TOOLCHAIN: &str = "rustup_toolchain";
pub(super) const PURGE_SAFETY_DERIVED: &str = "derived";
pub(super) const PURGE_SAFETY_PRIMARY: &str = "primary";

/// Taxonomy kinds accepted by `gc list --kind` / `gc purge --kind`
/// (#323 slice 2). The CLI's clap `ValueEnum` converts into this so the
/// gc module owns its own taxonomy without re-exporting clap types.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum GcListKindFilter {
    CargoTarget,
    CargoTargetIncremental,
    CargoTargetBuildScriptBinaries,
    CargoTargetDoc,
    CargoTargetSubcommandCaches,
    CargoRegistrySrc,
    CargoRegistryCache,
    CargoGitCheckouts,
    CargoGitDb,
    CargoInstalledBinaries,
    RustupToolchain,
}

impl From<crate::GcListKind> for GcListKindFilter {
    fn from(value: crate::GcListKind) -> Self {
        match value {
            crate::GcListKind::CargoTarget => GcListKindFilter::CargoTarget,
            crate::GcListKind::CargoTargetIncremental => GcListKindFilter::CargoTargetIncremental,
            crate::GcListKind::CargoTargetBuildScriptBinaries => {
                GcListKindFilter::CargoTargetBuildScriptBinaries
            }
            crate::GcListKind::CargoTargetDoc => GcListKindFilter::CargoTargetDoc,
            crate::GcListKind::CargoTargetSubcommandCaches => {
                GcListKindFilter::CargoTargetSubcommandCaches
            }
            crate::GcListKind::CargoRegistrySrc => GcListKindFilter::CargoRegistrySrc,
            crate::GcListKind::CargoRegistryCache => GcListKindFilter::CargoRegistryCache,
            crate::GcListKind::CargoGitCheckouts => GcListKindFilter::CargoGitCheckouts,
            crate::GcListKind::CargoGitDb => GcListKindFilter::CargoGitDb,
            crate::GcListKind::CargoInstalledBinaries => GcListKindFilter::CargoInstalledBinaries,
            crate::GcListKind::RustupToolchain => GcListKindFilter::RustupToolchain,
        }
    }
}

impl GcListKindFilter {
    pub(crate) fn kind_name(self) -> &'static str {
        match self {
            GcListKindFilter::CargoTarget => KIND_CARGO_TARGET,
            GcListKindFilter::CargoTargetIncremental => KIND_CARGO_TARGET_INCREMENTAL,
            GcListKindFilter::CargoTargetBuildScriptBinaries => {
                KIND_CARGO_TARGET_BUILD_SCRIPT_BINARIES
            }
            GcListKindFilter::CargoTargetDoc => KIND_CARGO_TARGET_DOC,
            GcListKindFilter::CargoTargetSubcommandCaches => KIND_CARGO_TARGET_SUBCOMMAND_CACHES,
            GcListKindFilter::CargoRegistrySrc => KIND_CARGO_REGISTRY_SRC,
            GcListKindFilter::CargoRegistryCache => KIND_CARGO_REGISTRY_CACHE,
            GcListKindFilter::CargoGitCheckouts => KIND_CARGO_GIT_CHECKOUTS,
            GcListKindFilter::CargoGitDb => KIND_CARGO_GIT_DB,
            GcListKindFilter::CargoInstalledBinaries => KIND_CARGO_INSTALLED_BINARIES,
            GcListKindFilter::RustupToolchain => KIND_RUSTUP_TOOLCHAIN,
        }
    }

    pub(crate) fn is_target_subtree(self) -> bool {
        matches!(
            self,
            GcListKindFilter::CargoTargetIncremental
                | GcListKindFilter::CargoTargetBuildScriptBinaries
                | GcListKindFilter::CargoTargetDoc
                | GcListKindFilter::CargoTargetSubcommandCaches
        )
    }
}

// gc list / gc summary entries follow their own schema version,
// independent of the global JSON_SCHEMA_VERSION used by other commands.
// Bumped from 1 to 2 in #323 slice 1 when kind + purge_safety were added.
// Bumped to 3 in soldr#2134 when `in_worktree` was added, following the
// same convention slice 1 used for `kind` + `purge_safety`.
pub(super) const GC_JSON_SCHEMA_VERSION: u32 = 3;

#[derive(Clone, Copy)]
pub(crate) enum GcMode {
    Summary,
    Purge { all: bool },
}

pub(crate) struct GcInvocation {
    pub(crate) mode: GcMode,
    pub(crate) older_than: String,
    pub(crate) larger_than: String,
    pub(crate) json: bool,
}

#[derive(Serialize)]
pub(super) struct GcCandidateOutput {
    pub(super) path: String,
    pub(super) size_bytes: u64,
    pub(super) size_human: String,
    pub(super) age_seconds: i64,
    pub(super) age_human: String,
    pub(super) eligible: bool,
    pub(super) reason: Option<String>,
    /// Taxonomy discriminator (#323 slice 1). Always `cargo_target` for
    /// now — later slices emit other kinds from new walkers.
    pub(super) kind: &'static str,
    /// Safety class for purge (#323 slice 1). Always `derived` for now.
    pub(super) purge_safety: &'static str,
}

#[derive(Serialize)]
struct GcOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    dry_run: bool,
    registry_path: String,
    candidate_count: usize,
    skipped_count: usize,
    total_reclaimable_bytes: u64,
    total_reclaimable_human: String,
    candidates: Vec<GcCandidateOutput>,
    largest_candidates: Vec<GcCandidateOutput>,
    skipped: Vec<GcCandidateOutput>,
    dropped_missing: usize,
    deleted_paths: Vec<String>,
    selected_count: usize,
    succeeded_count: usize,
    failed_count: usize,
    reclaimed_bytes: u64,
    reclaimed_human: String,
    error_log_path: Option<String>,
}

pub(crate) fn run_gc_command(invocation: GcInvocation) -> Result<(), SoldrError> {
    use crate::cache_lib::gc::{
        cleanup_old_gc_logs, parse_duration, parse_size, write_gc_error_log, GcOptions,
        GcPurgeSummary,
    };

    let older_than = parse_duration(&invocation.older_than).map_err(SoldrError::Other)?;
    let larger_than = parse_size(&invocation.larger_than).map_err(SoldrError::Other)?;
    let purge_all = match invocation.mode {
        GcMode::Summary => false,
        GcMode::Purge { all } => all,
    };
    let is_summary = matches!(invocation.mode, GcMode::Summary);

    let paths = SoldrPaths::new()?;
    let dev_roots = resolve_gc_dev_roots(&paths)?;
    let db_path = crate::cache_lib::data_db_path(&paths);
    let gc_log_dir = crate::cache_lib::gc_log_dir(&paths);
    cleanup_old_gc_logs(&gc_log_dir)
        .map_err(|e| SoldrError::Other(format!("failed to clean old gc logs: {e}")))?;

    let options = GcOptions {
        older_than_seconds: older_than,
        larger_than_bytes: larger_than,
        dev_roots,
        dry_run: is_summary,
    };

    // Snapshot-then-release: the sizing walk below, the per-candidate
    // prompt, and the deletion pool all run with no handle open (#1681).
    let report = daemon_gc_scan(&paths, &options)?;
    let total_reclaimable_bytes = gc_total_reclaimable_bytes(&report.candidates);

    let mut deleted_paths: Vec<String> = Vec::new();
    let mut purge_summary = GcPurgeSummary::default();
    let mut error_log_path: Option<std::path::PathBuf> = None;

    if is_summary {
        if !invocation.json {
            print_gc_summary(&db_path, &report, total_reclaimable_bytes);
        }
    } else if !invocation.json {
        print_gc_purge_scan(&db_path, &report, total_reclaimable_bytes);
    }

    if !is_summary {
        purge_summary =
            run_gc_purge_candidates(&paths, &report.candidates, purge_all, invocation.json)?;
        deleted_paths = purge_summary
            .deleted_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        if !purge_summary.failures.is_empty() {
            let args = std::env::args().collect::<Vec<_>>();
            let path = write_gc_error_log(&gc_log_dir, &args, &purge_summary.failures)
                .map_err(|e| SoldrError::Other(format!("failed to write gc error log: {e}")))?;
            error_log_path = Some(path);
        }
        if !invocation.json {
            print_gc_purge_result(&purge_summary, error_log_path.as_deref());
        }
    }

    if invocation.json {
        let output = GcOutput {
            schema_version: GC_JSON_SCHEMA_VERSION,
            command: "gc",
            mode: if is_summary { "summary" } else { "purge" },
            dry_run: is_summary,
            registry_path: db_path.display().to_string(),
            candidate_count: report.candidates.len(),
            skipped_count: report.skipped.len(),
            total_reclaimable_bytes,
            total_reclaimable_human: crate::cache_lib::target_registry::human_size(
                total_reclaimable_bytes,
            ),
            largest_candidates: gc_largest_candidates(&report.candidates, 5)
                .into_iter()
                .map(gc_candidate_output)
                .collect(),
            candidates: report
                .candidates
                .into_iter()
                .map(gc_candidate_output)
                .collect(),
            skipped: report
                .skipped
                .into_iter()
                .map(gc_candidate_output)
                .collect(),
            dropped_missing: report.dropped_missing,
            deleted_paths,
            selected_count: purge_summary.selected_count,
            succeeded_count: purge_summary.succeeded_count,
            failed_count: purge_summary.failed_count,
            reclaimed_bytes: purge_summary.reclaimed_bytes,
            reclaimed_human: crate::cache_lib::target_registry::human_size(
                purge_summary.reclaimed_bytes,
            ),
            error_log_path: error_log_path.map(|p| p.display().to_string()),
        };
        print_json(&output)?;
    }
    Ok(())
}

#[derive(Serialize)]
pub(super) struct GcListEntryOutput {
    pub(super) path: String,
    pub(super) last_used_unix: i64,
    pub(super) age_seconds: i64,
    pub(super) age_human: String,
    pub(super) size_bytes: u64,
    pub(super) size_human: String,
    pub(super) file_count: u64,
    /// Taxonomy discriminator (#323 slice 1). `cargo_target` is the
    /// only kind today; slice 2 also emits `cargo_registry_src`.
    pub(super) kind: &'static str,
    /// Safety class for purge (#323 slice 1). Always `derived` for now.
    pub(super) purge_safety: &'static str,
    /// Whether this target belongs to a linked git worktree (soldr#2134).
    /// Eviction takes these before primary checkouts, so surfacing it is
    /// what makes the resulting order explainable rather than arbitrary.
    /// Omitted for entry kinds that have no workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) in_worktree: Option<bool>,
    /// `<name>@<version>` parsed from the directory name. Present on
    /// `cargo_registry_src` entries and omitted (via `skip_serializing_if`)
    /// on `cargo_target` entries that lack the concept (#323 slice 2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) owner_crate: Option<String>,
    /// Workspace owning a target-derived entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) owner_workspace: Option<String>,
    /// Repository-ish owner for Cargo git database entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) owner_repo: Option<String>,
    /// Binary name for Cargo-installed executable entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) owner_binary: Option<String>,
    /// Toolchain name for rustup toolchain entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) owner_toolchain: Option<String>,
    /// Provenance of `last_used_unix` (#349 for registry-src, #1544
    /// for git checkouts). Present on `cargo_registry_src` and
    /// `cargo_git_checkouts` entries; omitted for `cargo_target` where
    /// only the soldr registry timestamp is available today. Values:
    ///
    /// - `"global_cache"` — cargo's own `$CARGO_HOME/.global-cache`
    ///   SQLite tracker produced the timestamp.
    /// - `"fs_mtime"` — the directory mtime, used when the tracker is
    ///   missing / locked / schema-drift / has no row for this entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) last_used_source: Option<&'static str>,
}

#[derive(Serialize)]
struct GcListOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    registry_path: String,
    entry_count: usize,
    pruned_missing: usize,
    entries: Vec<GcListEntryOutput>,
}

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub(crate) fn run_gc_list_command(
    json: bool,
    kind_filter: Option<GcListKindFilter>,
) -> Result<(), SoldrError> {
    use rayon::prelude::*;
    use walks::{absolute_path_string, fast_directory_size_and_files};

    let paths = SoldrPaths::new()?;
    let db_path = crate::cache_lib::data_db_path(&paths);
    // Scoped: the rayon sizing walk below visits every tracked target
    // tree, so the handle must not be held across it. The batched
    // `remove_many` reopens briefly once the walk is done (#1681).
    let rows = daemon_registry_rows(&paths)?;
    let now = crate::cache_lib::target_registry::current_unix_seconds()
        .map_err(|e| SoldrError::Other(format!("gc list clock error: {e}")))?;

    // Partition rows into those still on disk and those that have
    // disappeared since the registry was written. Missing rows are
    // never reported — they're swept out of the registry at the end
    // via a single batched delete.
    let (live_rows, missing_paths): (Vec<_>, Vec<_>) = rows.into_par_iter().partition_map(|row| {
        if row.path.exists() {
            rayon::iter::Either::Left(row)
        } else {
            rayon::iter::Either::Right(row.path)
        }
    });

    let include_targets =
        kind_filter.is_none() || matches!(kind_filter, Some(GcListKindFilter::CargoTarget));
    let include_registry_src =
        kind_filter.is_none() || matches!(kind_filter, Some(GcListKindFilter::CargoRegistrySrc));
    let include_git_checkouts =
        kind_filter.is_none() || matches!(kind_filter, Some(GcListKindFilter::CargoGitCheckouts));

    let mut entries: Vec<GcListEntryOutput> = if include_targets {
        live_rows
            .clone()
            .into_par_iter()
            .map(|row| {
                let (size_bytes, file_count) = fast_directory_size_and_files(&row.path);
                // soldr#2134: the same age eviction uses, not the raw
                // registry stamp. The stamp goes stale while a directory
                // stays hot (a repo built with bare `cargo` never updates
                // it), so reporting it made `soldr gc target` disagree
                // with the decision it exists to explain.
                let age_seconds =
                    crate::cache_lib::gc::effective_age_seconds(&row.path, row.last_used, now);
                GcListEntryOutput {
                    path: absolute_path_string(&row.path),
                    last_used_unix: row.last_used,
                    age_seconds,
                    age_human: crate::cache_lib::target_registry::human_age(age_seconds),
                    size_bytes,
                    size_human: crate::cache_lib::target_registry::human_size(size_bytes),
                    file_count,
                    kind: KIND_CARGO_TARGET,
                    purge_safety: PURGE_SAFETY_DERIVED,
                    in_worktree: Some(crate::cache_lib::gc::in_linked_git_worktree(&row.path)),
                    owner_crate: None,
                    owner_workspace: Some(
                        crate::cache_lib::target_registry::workspace_root_for_target(&row.path)
                            .display()
                            .to_string(),
                    ),
                    owner_repo: None,
                    owner_binary: None,
                    owner_toolchain: None,
                    last_used_source: None,
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    if include_registry_src {
        if let Some(cargo_home) = crate::core::resolve_cargo_home() {
            entries.extend(walks::walk_cargo_registry_src(&cargo_home, now));
        }
    }

    if include_git_checkouts {
        if let Some(cargo_home) = crate::core::resolve_cargo_home() {
            entries.extend(walks::walk_cargo_git_checkouts(&cargo_home, now));
        }
    }

    entries.extend(walks::walk_cargo_target_subtrees(
        &live_rows,
        now,
        kind_filter,
    ));

    if let Some(cargo_home) = crate::core::resolve_cargo_home() {
        entries.extend(walks::walk_cargo_report_only(&cargo_home, now, kind_filter));
    }

    // Both rustup homes the purge surface operates on (soldr#3507): the
    // caller's own `$RUSTUP_HOME` and the managed `<root>/rustup`, so
    // `gc list` can never show a toolchain the purge would not — the 20 GB
    // of #3507 lives in the managed home and used to be invisible here.
    for home in rustup_toolchain::homes_for_enumeration(Some(&paths)) {
        entries.extend(walks::walk_rustup_toolchains(&home.path, now, kind_filter));
    }

    // Bounded reopen for the batched removal (#1681).
    let pruned_missing = daemon_remove_registry_rows(&paths, missing_paths)?;

    if json {
        let output = GcListOutput {
            schema_version: GC_JSON_SCHEMA_VERSION,
            command: "gc",
            mode: "list",
            registry_path: db_path.display().to_string(),
            entry_count: entries.len(),
            pruned_missing,
            entries,
        };
        print_json(&output)?;
    } else {
        println!("soldr gc list: registry: {}", db_path.display());
        println!(
            "soldr gc list: {} tracked target dir{}",
            entries.len(),
            if entries.len() == 1 { "" } else { "s" }
        );
        for entry in &entries {
            println!(
                "  {}  size={}  files={}  age={}  kind={}",
                entry.path, entry.size_human, entry.file_count, entry.age_human, entry.kind,
            );
        }
        if pruned_missing > 0 {
            println!(
                "soldr gc list: pruned {pruned_missing} missing row{} from registry",
                if pruned_missing == 1 { "" } else { "s" }
            );
        }
    }
    Ok(())
}

pub(crate) fn emit_startup_target_warning_if_due() {
    let Ok(paths) = SoldrPaths::new() else { return };
    let marker = crate::cache_lib::gc_warning_marker_path(&paths);
    // Check the 24 h throttle before asking the daemon for a registry snapshot
    // (#1843).
    //
    // `maybe_build_startup_warning` evaluates this same condition, but only
    // after we have already opened the registry — and `TargetRegistry::open`
    // is not a read-only open: `init_schema` runs a durable write transaction
    // (with an fsync) on every open just to ensure the table exists. So on the
    // ~24-hours-out-of-24 where no warning is due, the front door paid a redb
    // create + commit, plus a `config.toml` read in `resolve_gc_dev_roots`,
    // purely to discover it had nothing to say. Measured at 37-42 ms per
    // `soldr cargo` invocation.
    //
    // `startup_warning_due` is a single `fs::metadata`, and treats a missing
    // or unreadable marker as due, so hoisting it preserves the cold-path
    // semantics exactly — `maybe_build_startup_warning` still re-checks and
    // still owns touching the marker.
    if !crate::cache_lib::gc::startup_warning_due(&marker).unwrap_or(true) {
        return;
    }
    let options = crate::cache_lib::gc::GcOptions {
        older_than_seconds: crate::cache_lib::target_registry::DEFAULT_STALE_AGE_SECONDS,
        larger_than_bytes: crate::cache_lib::target_registry::DEFAULT_STALE_SIZE_BYTES,
        dev_roots: resolve_gc_dev_roots(&paths).unwrap_or_default(),
        dry_run: true,
    };
    let Ok(report) = daemon_gc_scan(&paths, &options) else {
        return;
    };
    match crate::cache_lib::gc::startup_warning_from_report(&report, &options, &marker) {
        Ok(Some(message)) => eprintln!("{message}"),
        Ok(None) => {}
        Err(_) => {}
    }
}

// ---------------------------------------------------------------------------
// soldr gc target — cross-repo target/ reclamation (#574).
// ---------------------------------------------------------------------------

/// Env-var override for the walk root used by `soldr gc target`.
pub(crate) const SOLDR_GC_TARGET_ROOT_ENV_VAR: &str = "SOLDR_GC_TARGET_ROOT";

const GC_TARGET_JSON_SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
struct GcTargetEntryOutput {
    workspace_root: String,
    target_dir: String,
    size_bytes: u64,
    size_human: String,
    file_count: u64,
    last_modified_ms: i64,
    /// How this entry was discovered. `"manifest"` is the original
    /// sibling-`Cargo.toml` path; `"content"` is the cargo-shape
    /// heuristic added in #681. Additive JSON field — readers that
    /// don't know about it stay forward-compatible. Schema stays at
    /// version 1; this is a pure additive change.
    discovery: &'static str,
}

#[derive(Serialize)]
struct GcTargetOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    root: String,
    max_depth: usize,
    entry_count: usize,
    total_bytes: u64,
    total_human: String,
    entries: Vec<GcTargetEntryOutput>,
    purged_count: usize,
    failed_count: usize,
    purged_bytes: u64,
    purged_human: String,
    failures: Vec<GcTargetFailure>,
}

#[derive(Serialize)]
struct GcTargetFailure {
    target_dir: String,
    error: String,
}

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub(crate) fn run_gc_target_command(args: crate::cli_args::GcTargetArgs) -> Result<(), SoldrError> {
    use std::io::{IsTerminal, Write};
    use target_walker::TargetEntry;

    let root = resolve_gc_target_root(args.root.as_deref())?;
    let mut entries: Vec<TargetEntry> = target_walker::walk(&root, args.max_depth);
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.size_bytes));

    let total_bytes: u64 = entries.iter().map(|e| e.size_bytes).sum();
    let mode = if args.purge { "purge" } else { "report" };

    let entry_outputs: Vec<GcTargetEntryOutput> = entries
        .iter()
        .map(|e| GcTargetEntryOutput {
            workspace_root: e.workspace_root.display().to_string(),
            target_dir: e.target_dir.display().to_string(),
            size_bytes: e.size_bytes,
            size_human: crate::cache_lib::target_registry::human_size(e.size_bytes),
            file_count: e.file_count,
            last_modified_ms: e.last_modified_ms,
            discovery: match e.discovery {
                target_walker::TargetDiscovery::Manifest => "manifest",
                target_walker::TargetDiscovery::Content => "content",
            },
        })
        .collect();

    let mut purged_count = 0usize;
    let mut purged_bytes = 0u64;
    let mut failures: Vec<GcTargetFailure> = Vec::new();

    if args.purge {
        // Print human summary to stderr so JSON callers still get clean
        // stdout. The y/n prompt also lives on stderr/stdin so piping
        // stdout through `jq` doesn't break confirmation flow.
        if !args.json {
            print_target_report(&root, &entries, total_bytes, /*purge_plan=*/ true);
        }

        let proceed = if args.yes {
            true
        } else if !std::io::stdin().is_terminal() {
            // Why: refuse to silently delete in CI when --yes is missing.
            // The non-interactive caller must opt in explicitly.
            eprintln!(
                "soldr gc target: refusing to purge without --yes on a non-interactive stdin"
            );
            return Err(SoldrError::Other(
                "soldr gc target --purge requires --yes on non-interactive stdin".into(),
            ));
        } else {
            eprint!(
                "soldr gc target: purge {} target/ director{} totaling {} ? [y/N] ",
                entries.len(),
                if entries.len() == 1 { "y" } else { "ies" },
                crate::cache_lib::target_registry::human_size(total_bytes),
            );
            let _ = std::io::stderr().flush();
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .map_err(|e| SoldrError::Other(format!("failed to read prompt answer: {e}")))?;
            matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES")
        };

        if proceed {
            for entry in &entries {
                match remove_target_dir(&entry.target_dir) {
                    Ok(()) => {
                        purged_count += 1;
                        purged_bytes = purged_bytes.saturating_add(entry.size_bytes);
                    }
                    Err(err) => failures.push(GcTargetFailure {
                        target_dir: entry.target_dir.display().to_string(),
                        error: err.to_string(),
                    }),
                }
            }
        } else if !args.json {
            eprintln!("soldr gc target: aborted; no directories deleted");
        }
    } else if !args.json {
        print_target_report(&root, &entries, total_bytes, /*purge_plan=*/ false);
    }

    let failed_count = failures.len();

    if args.json {
        let output = GcTargetOutput {
            schema_version: GC_TARGET_JSON_SCHEMA_VERSION,
            command: "gc target",
            mode,
            root: root.display().to_string(),
            max_depth: args.max_depth,
            entry_count: entries.len(),
            total_bytes,
            total_human: crate::cache_lib::target_registry::human_size(total_bytes),
            entries: entry_outputs,
            purged_count,
            failed_count,
            purged_bytes,
            purged_human: crate::cache_lib::target_registry::human_size(purged_bytes),
            failures,
        };
        print_json(&output)?;
    } else if args.purge {
        eprintln!(
            "soldr gc target: purged {purged_count} target/ director{} ({}); {} failure{}",
            if purged_count == 1 { "y" } else { "ies" },
            crate::cache_lib::target_registry::human_size(purged_bytes),
            failed_count,
            if failed_count == 1 { "" } else { "s" },
        );
        // soldr#2199: the count alone is undiagnosable. The reporting user
        // saw "1 failure" with no path and no reason, and the cause only
        // surfaced after reproducing the delete by hand. The error was
        // already captured for `--json`; say it here too.
        for failure in &failures {
            eprintln!(
                "soldr gc target:   {}: {}",
                failure.target_dir, failure.error
            );
            // soldr#2199: that error comes from the *parent* ("the directory
            // is not empty") and names nothing. The refusal happened on some
            // leaf inside it, and which leaf -- and what is unusual about it
            // -- is the whole diagnosis. Collect it while the tree is still
            // in the failed state.
            if let Some(detail) =
                crate::gc::delete_diagnosis::describe(std::path::Path::new(&failure.target_dir))
            {
                eprintln!("soldr gc target:     {detail}");
            }
        }
    }

    if args.purge && failed_count > 0 {
        return Err(SoldrError::Other(format!(
            "soldr gc target: {failed_count} target/ purge failure{}",
            if failed_count == 1 { "" } else { "s" }
        )));
    }
    Ok(())
}

fn print_target_report(
    root: &std::path::Path,
    entries: &[target_walker::TargetEntry],
    total_bytes: u64,
    purge_plan: bool,
) {
    let heading = if purge_plan {
        "soldr gc target: purge plan"
    } else {
        "soldr gc target: report"
    };
    println!("{heading}");
    println!("  root: {}", root.display());
    println!("  entries: {}", entries.len());
    println!(
        "  total: {}",
        crate::cache_lib::target_registry::human_size(total_bytes)
    );
    for entry in entries {
        // Surface discovery=content prominently so users can
        // spot-check heuristic matches before accepting --yes (#681).
        let discovery = match entry.discovery {
            target_walker::TargetDiscovery::Manifest => "manifest",
            target_walker::TargetDiscovery::Content => "content",
        };
        println!(
            "    {}  size={}  files={}  discovery={discovery}  target={}",
            entry.workspace_root.display(),
            crate::cache_lib::target_registry::human_size(entry.size_bytes),
            entry.file_count,
            entry.target_dir.display(),
        );
    }
    if !purge_plan && !entries.is_empty() {
        println!("  run with --purge --yes to reclaim these directories");
    }
}

fn resolve_gc_target_root(cli: Option<&std::path::Path>) -> Result<std::path::PathBuf, SoldrError> {
    if let Some(p) = cli {
        return Ok(p.to_path_buf());
    }
    if let Some(raw) = std::env::var_os(SOLDR_GC_TARGET_ROOT_ENV_VAR) {
        let s = raw.to_string_lossy();
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Ok(crate::core::expand_user_home(trimmed));
        }
    }
    let home = crate::core::user_home_dir()
        .map_err(|e| SoldrError::Other(format!("failed to resolve $HOME: {e}")))?;
    Ok(home.join("dev"))
}

#[cfg(test)]
mod tests;
