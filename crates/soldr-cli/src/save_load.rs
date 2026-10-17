//! `soldr save` / `soldr load` CLI surface — thin wrappers around the
//! soldr-save crate's `save`/`load` functions.
//!
//! These commands consume and produce single-file `.tar.zst` archives
//! that bundle a build cache directory plus a content-verified snapshot
//! of source-file mtimes. The intent is to give any project that builds
//! from a fresh checkout (CI, hermetic sandboxes) a portable way to
//! preserve the fingerprint stability Cargo expects without resorting
//! to mtime-rewrite tricks that risk underbuilds.

use std::path::{Component, Path, PathBuf};

use crate::cache_lib::save::{
    load, read_manifest_file, read_manifest_from_archive, save_delta_with_projection,
    save_with_projection, write_manifest_file, LoadOptions, SaveDeltaOptions, SaveOptions,
    SaveProfile, DEFAULT_ZSTD_LEVEL, SAVE_PROFILE_ENV,
};
use clap::Args;

#[path = "save_load_snapshot.rs"]
mod snapshot;

#[derive(Debug, Args)]
pub struct SaveArgs {
    /// Build-cache directory whose contents should be bundled. Omit
    /// when `--mtimes-only` is set (manifest-only archive).
    #[arg(long, value_name = "DIR")]
    pub cache_dir: Option<PathBuf>,

    /// Workspace whose tracked source files should be snapshotted.
    /// Omit to produce a cache-only archive (no mtime snapshot — load
    /// will still restore the cache but skip the replay). Required when
    /// `--mtimes-only` is set.
    #[arg(long, value_name = "DIR")]
    pub workspace: Option<PathBuf>,

    /// Destination archive path. Suggest `.tar.zst` suffix.
    #[arg(long, value_name = "FILE")]
    pub out: PathBuf,

    /// zstd compression level (1..=22). Default 3 — higher levels hurt
    /// wall-clock far more than they shrink transfer.
    #[arg(long, default_value_t = DEFAULT_ZSTD_LEVEL, value_name = "N")]
    pub zstd_level: i32,

    /// Thread count for parallel hashing + zstd. Default uses all
    /// available cores.
    #[arg(long, value_name = "N")]
    pub threads: Option<usize>,

    /// Emit a machine-readable JSON line summarising the save. Stable
    /// schema includes `profile`, `excluded_files`, and `excluded_bytes`.
    #[arg(long)]
    pub json: bool,

    /// Use the CI/minimal payload profile. Alias: `--minimal`.
    /// Excludes logs, sockets, locks, runtime scratch, and soldr-managed
    /// binary/toolchain trees from cache archives while reporting the
    /// omitted file count and byte total. If absent, `SOLDR_SAVE_PROFILE`
    /// may select the same profile.
    #[arg(long, visible_alias = "minimal")]
    pub ci: bool,

    /// Produce a manifest-only archive — only the source-file mtime
    /// snapshot, no cache payload. Requires `--workspace`; mutually
    /// exclusive with `--cache-dir`. This is the setup-soldr
    /// `preserve-source-mtimes` feature, promoted into the soldr CLI
    /// so any wrapper can produce the same sidecar.
    #[arg(long = "mtimes-only")]
    pub mtimes_only: bool,

    /// Produce a delta archive by comparing `--cache-dir` against this
    /// protobuf manifest from a previously restored base archive.
    #[arg(long, value_name = "FILE")]
    pub delta_from_manifest: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct LoadArgs {
    /// Archive produced by `soldr save`.
    #[arg(long, value_name = "FILE")]
    pub archive: PathBuf,

    /// Destination cache directory. Created if it doesn't exist. Omit
    /// when `--mtimes-only` is set.
    #[arg(long, value_name = "DIR")]
    pub cache_dir: Option<PathBuf>,

    /// Workspace where source-file mtimes should be replayed. Omit
    /// to restore the cache only. Required when `--mtimes-only` is set.
    #[arg(long, value_name = "DIR")]
    pub workspace: Option<PathBuf>,

    /// Thread count for parallel mtime replay (file hashing). Default
    /// uses all available cores.
    #[arg(long, value_name = "N")]
    pub threads: Option<usize>,

    /// Emit a machine-readable JSON line summarising the load. Stable
    /// schema: see `soldr save --json`'s counterpart fields.
    #[arg(long)]
    pub json: bool,

    /// Treat the archive as a manifest-only snapshot — apply mtimes
    /// only, refuse to extract any cache entries. Requires
    /// `--workspace`; mutually exclusive with `--cache-dir`.
    #[arg(long = "mtimes-only")]
    pub mtimes_only: bool,

    /// Write the archive's protobuf manifest to this path after a
    /// successful load, for later `soldr save --delta-from-manifest`.
    #[arg(long, value_name = "FILE")]
    pub manifest_out: Option<PathBuf>,

    /// Emit a per-phase profile line to stderr after the load (zstd
    /// decode time, tar parse + dispatch time, total extract time, per-
    /// worker job count, per-file extract latency percentiles). Useful
    /// for tuning the parallel-extract worker count. Also enabled when
    /// `SOLDR_PROFILE_EXTRACT=1` is set in the environment. (#575)
    #[arg(long = "profile-extract")]
    pub profile_extract: bool,

    /// On Windows, when the current process is admin, briefly add the
    /// `--cache-dir` to the Defender exclusion list for the duration of
    /// the load. No-op on non-Windows or non-admin contexts — never
    /// triggers a UAC prompt. Default off. (#575)
    #[arg(long = "auto-defender-exclude")]
    pub auto_defender_exclude: bool,
}

fn absolute_lexical_path(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("current_dir: {error}"))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(format!(
                        "path escapes its filesystem root: {}",
                        path.display()
                    ));
                }
            }
        }
    }
    Ok(normalized)
}

fn path_for_containment(path: &Path) -> Result<PathBuf, String> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => absolute_lexical_path(path),
        Err(error) => Err(format!("failed to resolve {}: {error}", path.display())),
    }
}

fn archive_contains_embedded_cache(cache_dir: &Path, embedded_root: &Path) -> Result<bool, String> {
    let archive_root = path_for_containment(cache_dir)?;
    let embedded_root = path_for_containment(embedded_root)?;
    Ok(embedded_root.starts_with(archive_root))
}

/// Quiesce the embedded cache only when the requested archive actually
/// contains it.
///
/// A point-in-time flush is not a snapshot barrier: new compile publications
/// can land between the reply and tar traversal. After a complete checkpoint,
/// request graceful daemon shutdown and wait for the exact acknowledged
/// generation. An unrelated `--cache-dir` never depends on the ambient daemon.
fn quiesce_embedded_state_before_save(cache_dir: &Path) -> Result<(), String> {
    let paths = crate::core::SoldrPaths::new().map_err(|error| error.to_string())?;
    let embedded_root = crate::zccache_embedded::embedded_cache_root(&paths);
    if !archive_contains_embedded_cache(cache_dir, &embedded_root)? {
        return Ok(());
    }

    let sock = crate::daemon::server::server_sock_path(&paths);
    match crate::daemon::client::flush_caches(&sock) {
        Ok(report) if report.is_complete() => {}
        Ok(report) => Err(format!(
            "embedded zccache checkpoint incomplete: {}",
            report.incomplete_reason()
        ))?,
        Err(crate::daemon::client::ClientError::NotRunning) => return Ok(()),
        Err(error) => return Err(format!("embedded zccache checkpoint failed: {error:?}")),
    }

    let responder = match crate::daemon::client::shutdown(&sock) {
        Ok(responder) => responder,
        Err(crate::daemon::client::ClientError::NotRunning) => return Ok(()),
        Err(error) => {
            return Err(format!(
                "embedded zccache snapshot quiescence failed: {error:?}"
            ))
        }
    };
    let outcome = crate::daemon::lifecycle::wait_for_shutdown_responder(
        &paths,
        &sock,
        responder,
        crate::daemon::lifecycle::GRACEFUL_SHUTDOWN_WAIT_TIMEOUT,
    );
    if !outcome.is_complete() {
        return Err(format!(
            "daemon generation {} (pid {}) acknowledged shutdown but did not become quiescent \
             within {}s; it was not force-killed",
            responder.generation,
            responder.pid,
            crate::daemon::lifecycle::GRACEFUL_SHUTDOWN_WAIT_TIMEOUT.as_secs(),
        ));
    }
    eprintln!(
        "soldr save: embedded zccache state checkpointed and daemon generation {} quiesced",
        responder.generation
    );
    Ok(())
}

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub fn run_save(args: SaveArgs) -> i32 {
    let args = match apply_private_session_cache_dir_default(
        args,
        crate::zccache::private_session_requested(),
        std::env::current_dir,
    ) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("soldr save: {err}");
            return 1;
        }
    };
    let profile = match resolve_save_profile(args.ci) {
        Ok(profile) => profile,
        Err(err) => {
            eprintln!("soldr save: {err}");
            return 1;
        }
    };
    if let Some(cache_dir) = args.cache_dir.as_deref() {
        if let Err(error) = quiesce_embedded_state_before_save(cache_dir) {
            eprintln!("soldr save: refusing to archive a non-quiescent cache: {error}");
            return 1;
        }
    }
    let prepared_snapshot = match snapshot::prepare(args.cache_dir.as_deref()) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("soldr save: compiler snapshot export failed: {error}");
            return 1;
        }
    };
    let projection = prepared_snapshot
        .as_ref()
        .map(snapshot::PreparedSnapshot::projection);
    if let Some(base_manifest_path) = args.delta_from_manifest.as_deref() {
        let Some(cache_dir) = args.cache_dir.as_deref() else {
            eprintln!("soldr save: --delta-from-manifest requires --cache-dir");
            return 1;
        };
        if args.mtimes_only {
            eprintln!("soldr save: --delta-from-manifest cannot be combined with --mtimes-only");
            return 1;
        }
        let base_manifest = match read_manifest_file(base_manifest_path) {
            Ok(manifest) => manifest,
            Err(err) => {
                eprintln!("soldr save: failed to read base manifest: {err}");
                return 1;
            }
        };
        let opts = SaveDeltaOptions {
            workspace: args.workspace.as_deref(),
            cache_dir,
            base_manifest: &base_manifest,
            out: &args.out,
            zstd_level: args.zstd_level,
            threads: args.threads,
            profile,
        };
        let report = match save_delta_with_projection(&opts, projection.as_ref()) {
            Ok(r) => r,
            Err(err) => {
                eprintln!("soldr save: {err}");
                return 1;
            }
        };
        if args.json {
            println!(
                "{{\"profile\":\"{}\",\"source_files\":{},\"cache_files\":{},\"deleted_cache_files\":{},\"excluded_files\":{},\"excluded_bytes\":{},\"archive_bytes\":{},\"elapsed_ms\":{},\"delta\":true}}",
                report.profile.as_str(),
                report.source_files,
                report.cache_files,
                report.deleted_cache_files,
                report.excluded_files,
                report.excluded_bytes,
                report.archive_bytes,
                report.elapsed_ms,
            );
        } else {
            println!(
                "soldr save: profile={} source_files={} cache_files={} deleted_cache_files={} excluded_files={} excluded_bytes={} archive_bytes={} elapsed_ms={} delta=true out={}",
                report.profile.as_str(),
                report.source_files,
                report.cache_files,
                report.deleted_cache_files,
                report.excluded_files,
                report.excluded_bytes,
                report.archive_bytes,
                report.elapsed_ms,
                args.out.display(),
            );
        }
        return 0;
    }

    let opts = SaveOptions {
        workspace: args.workspace.as_deref(),
        cache_dir: args.cache_dir.as_deref(),
        out: &args.out,
        zstd_level: args.zstd_level,
        threads: args.threads,
        mtimes_only: args.mtimes_only,
        profile,
    };
    let report = match save_with_projection(&opts, projection.as_ref()) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("soldr save: {err}");
            return 1;
        }
    };
    if args.json {
        println!(
            "{{\"profile\":\"{}\",\"source_files\":{},\"cache_files\":{},\"excluded_files\":{},\"excluded_bytes\":{},\"archive_bytes\":{},\"elapsed_ms\":{},\"mtimes_only\":{}}}",
            report.profile.as_str(),
            report.source_files,
            report.cache_files,
            report.excluded_files,
            report.excluded_bytes,
            report.archive_bytes,
            report.elapsed_ms,
            args.mtimes_only,
        );
    } else {
        println!(
            "soldr save: profile={} source_files={} cache_files={} excluded_files={} excluded_bytes={} archive_bytes={} elapsed_ms={} mtimes_only={} out={}",
            report.profile.as_str(),
            report.source_files,
            report.cache_files,
            report.excluded_files,
            report.excluded_bytes,
            report.archive_bytes,
            report.elapsed_ms,
            args.mtimes_only,
            args.out.display(),
        );
    }
    0
}

fn resolve_save_profile(cli_ci: bool) -> Result<SaveProfile, String> {
    if cli_ci {
        return Ok(SaveProfile::Ci);
    }
    let Ok(raw) = std::env::var(SAVE_PROFILE_ENV) else {
        return Ok(SaveProfile::Full);
    };
    SaveProfile::parse(&raw).ok_or_else(|| {
        format!(
            "{SAVE_PROFILE_ENV} must be one of: full, default, complete, ci, minimal, cook (got {raw:?})"
        )
    })
}

pub fn run_load(args: LoadArgs) -> i32 {
    let args = match apply_private_session_cache_dir_default_load(
        args,
        crate::zccache::private_session_requested(),
        std::env::current_dir,
    ) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("soldr load: {err}");
            return 1;
        }
    };
    let env_profile = std::env::var("SOLDR_PROFILE_EXTRACT")
        .ok()
        .map(|v| crate::core::flag_value(&v))
        .unwrap_or(false);
    let opts = LoadOptions {
        archive: &args.archive,
        cache_dir: args.cache_dir.as_deref(),
        workspace: args.workspace.as_deref(),
        threads: args.threads,
        mtimes_only: args.mtimes_only,
        profile_extract: args.profile_extract || env_profile,
        auto_defender_exclude: args.auto_defender_exclude,
    };
    let report = match load(&opts) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("soldr load: {err}");
            return 1;
        }
    };
    if let Err(error) = snapshot::restore(args.cache_dir.as_deref()) {
        eprintln!("soldr load: compiler snapshot import failed: {error}");
        return 1;
    }
    if let Some(out) = args.manifest_out.as_deref() {
        let manifest = match read_manifest_from_archive(&args.archive) {
            Ok(manifest) => manifest,
            Err(err) => {
                eprintln!("soldr load: failed to read manifest for --manifest-out: {err}");
                return 1;
            }
        };
        if let Err(err) = write_manifest_file(out, &manifest) {
            eprintln!("soldr load: failed to write --manifest-out: {err}");
            return 1;
        }
    }
    if args.json {
        println!(
            "{{\"cache_files_restored\":{},\"source_files_in_manifest\":{},\"mtimes_applied\":{},\"mtimes_skipped_missing\":{},\"mtimes_skipped_size_mismatch\":{},\"mtimes_skipped_modified\":{},\"elapsed_ms\":{}}}",
            report.cache_files_restored,
            report.source_files_in_manifest,
            report.mtimes_applied,
            report.mtimes_skipped_missing,
            report.mtimes_skipped_size_mismatch,
            report.mtimes_skipped_modified,
            report.elapsed_ms,
        );
    } else {
        println!(
            "soldr load: cache_files_restored={} source_files_in_manifest={} mtimes_applied={} mtimes_skipped_missing={} mtimes_skipped_size_mismatch={} mtimes_skipped_modified={} elapsed_ms={}",
            report.cache_files_restored,
            report.source_files_in_manifest,
            report.mtimes_applied,
            report.mtimes_skipped_missing,
            report.mtimes_skipped_size_mismatch,
            report.mtimes_skipped_modified,
            report.elapsed_ms,
        );
    }
    0
}

/// If `private_requested` and `--cache-dir` is omitted, default the
/// save side to `<cwd>/.zccache`. Mtimes-only mode is unaffected: the
/// `cache_dir` must stay `None` in that mode and the save side already
/// rejects the combination, so we leave it alone.
///
/// `cwd_provider` lets tests inject a synthetic working directory
/// without mutating the global process env.
fn apply_private_session_cache_dir_default<F>(
    mut args: SaveArgs,
    private_requested: bool,
    cwd_provider: F,
) -> Result<SaveArgs, String>
where
    F: FnOnce() -> std::io::Result<PathBuf>,
{
    if args.cache_dir.is_none() && !args.mtimes_only && private_requested {
        let cwd = cwd_provider().map_err(|e| format!("current_dir: {e}"))?;
        args.cache_dir = Some(cwd.join(crate::zccache::PRIVATE_SESSION_CACHE_DIR_NAME));
    }
    Ok(args)
}

fn apply_private_session_cache_dir_default_load<F>(
    mut args: LoadArgs,
    private_requested: bool,
    cwd_provider: F,
) -> Result<LoadArgs, String>
where
    F: FnOnce() -> std::io::Result<PathBuf>,
{
    if args.cache_dir.is_none() && !args.mtimes_only && private_requested {
        let cwd = cwd_provider().map_err(|e| format!("current_dir: {e}"))?;
        args.cache_dir = Some(cwd.join(crate::zccache::PRIVATE_SESSION_CACHE_DIR_NAME));
    }
    Ok(args)
}

#[cfg(test)]
mod private_session_default_tests {
    use super::*;
    use crate::platform::sync::Mutex;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    fn with_save_profile_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_MUTEX.lock();
        let prev = std::env::var_os(SAVE_PROFILE_ENV);
        match value {
            Some(value) => std::env::set_var(SAVE_PROFILE_ENV, value),
            None => std::env::remove_var(SAVE_PROFILE_ENV),
        }
        let result = f();
        match prev {
            Some(prev) => std::env::set_var(SAVE_PROFILE_ENV, prev),
            None => std::env::remove_var(SAVE_PROFILE_ENV),
        }
        result
    }

    fn fake_cwd() -> std::io::Result<PathBuf> {
        Ok(PathBuf::from("/fake/cwd"))
    }

    #[test]
    fn cache_snapshot_quiescence_is_scoped_to_archived_tree() {
        let temp = tempfile::tempdir().expect("tempdir");
        let managed = temp.path().join("managed");
        let embedded = managed.join("zccache/daemon-state/stable");
        let unrelated = temp.path().join("unrelated");
        std::fs::create_dir_all(&embedded).expect("embedded root");
        std::fs::create_dir_all(&unrelated).expect("unrelated root");

        assert!(archive_contains_embedded_cache(&managed, &embedded).expect("managed containment"));
        assert!(archive_contains_embedded_cache(&embedded, &embedded).expect("exact containment"));
        assert!(
            !archive_contains_embedded_cache(&unrelated, &embedded).expect("unrelated containment"),
            "an unrelated --cache-dir must not require an ambient daemon flush"
        );
    }

    fn base_save_args() -> SaveArgs {
        SaveArgs {
            cache_dir: None,
            workspace: None,
            out: PathBuf::from("/out.tar.zst"),
            zstd_level: DEFAULT_ZSTD_LEVEL,
            threads: None,
            json: false,
            ci: false,
            mtimes_only: false,
            delta_from_manifest: None,
        }
    }

    fn base_load_args() -> LoadArgs {
        LoadArgs {
            archive: PathBuf::from("/in.tar.zst"),
            cache_dir: None,
            workspace: None,
            threads: None,
            json: false,
            mtimes_only: false,
            manifest_out: None,
            profile_extract: false,
            auto_defender_exclude: false,
        }
    }

    #[test]
    fn save_default_routes_to_cwd_dot_zccache_when_private() {
        let args = apply_private_session_cache_dir_default(base_save_args(), true, fake_cwd)
            .expect("apply default");
        assert_eq!(
            args.cache_dir.as_deref(),
            Some(std::path::Path::new("/fake/cwd/.zccache")),
        );
    }

    #[test]
    fn save_default_no_op_when_private_off() {
        let args = apply_private_session_cache_dir_default(base_save_args(), false, fake_cwd)
            .expect("apply default");
        assert!(
            args.cache_dir.is_none(),
            "without the env var, --cache-dir must remain user-controlled",
        );
    }

    #[test]
    fn save_default_preserves_explicit_cache_dir() {
        let mut args = base_save_args();
        args.cache_dir = Some(PathBuf::from("/user/explicit"));
        let args =
            apply_private_session_cache_dir_default(args, true, fake_cwd).expect("apply default");
        assert_eq!(
            args.cache_dir.as_deref(),
            Some(std::path::Path::new("/user/explicit")),
            "explicit --cache-dir always wins over the private-session default",
        );
    }

    #[test]
    fn save_default_no_op_in_mtimes_only_mode() {
        let mut args = base_save_args();
        args.mtimes_only = true;
        let args =
            apply_private_session_cache_dir_default(args, true, fake_cwd).expect("apply default");
        assert!(
            args.cache_dir.is_none(),
            "mtimes-only forbids --cache-dir; default must not inject one",
        );
    }

    #[test]
    fn load_default_routes_to_cwd_dot_zccache_when_private() {
        let args = apply_private_session_cache_dir_default_load(base_load_args(), true, fake_cwd)
            .expect("apply default");
        assert_eq!(
            args.cache_dir.as_deref(),
            Some(std::path::Path::new("/fake/cwd/.zccache")),
        );
    }

    #[test]
    fn load_default_preserves_explicit_cache_dir() {
        let mut args = base_load_args();
        args.cache_dir = Some(PathBuf::from("/user/explicit"));
        let args = apply_private_session_cache_dir_default_load(args, true, fake_cwd)
            .expect("apply default");
        assert_eq!(
            args.cache_dir.as_deref(),
            Some(std::path::Path::new("/user/explicit")),
        );
    }

    #[test]
    fn load_default_no_op_in_mtimes_only_mode() {
        let mut args = base_load_args();
        args.mtimes_only = true;
        let args = apply_private_session_cache_dir_default_load(args, true, fake_cwd)
            .expect("apply default");
        assert!(args.cache_dir.is_none());
    }

    #[test]
    fn save_profile_defaults_to_full_when_env_unset() {
        with_save_profile_env(None, || {
            assert_eq!(resolve_save_profile(false).unwrap(), SaveProfile::Full);
        });
    }

    #[test]
    fn save_profile_env_accepts_ci_and_minimal_alias() {
        with_save_profile_env(Some("ci"), || {
            assert_eq!(resolve_save_profile(false).unwrap(), SaveProfile::Ci);
        });
        with_save_profile_env(Some("minimal"), || {
            assert_eq!(resolve_save_profile(false).unwrap(), SaveProfile::Ci);
        });
    }

    #[test]
    fn save_profile_invalid_env_errors_unless_cli_flag_wins() {
        with_save_profile_env(Some("tiny"), || {
            let err = resolve_save_profile(false).expect_err("invalid env must error");
            assert!(
                err.contains(SAVE_PROFILE_ENV) && err.contains("tiny"),
                "error should name env and bad value: {err}"
            );
            assert_eq!(resolve_save_profile(true).unwrap(), SaveProfile::Ci);
        });
    }
}
