fn nightly_toolchain_available() -> bool {
    let Ok(output) = crate::platform::wait::output_for(
        std::process::Command::new("rustup").args(["toolchain", "list"]),
        crate::platform::wait::PROBE,
    )
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .any(|line| line.trim_start().starts_with("nightly"))
}

/// Enumerate every soldr-owned path for the auto-GC orchestrator.
/// Synchronous last-chance reclaim, run when the disk watchdog is about
/// to abort a build (soldr#2134).
///
/// The reclaim mechanism already existed and already worked — the
/// reporter watched it free 118 GiB — but it lives in the detached,
/// five-minute-throttled sweeper, so it ran *after* the build had
/// already failed. The developer ate a failed command for a condition
/// soldr was about to resolve on its own.
///
/// This runs the **same** Tier-2 pass with the same guards, floors and
/// ordering, so nothing becomes eligible for deletion that was not
/// already eligible; the only change is that it happens in time to
/// matter. Returns bytes reclaimed.
///
/// Targets on other volumes are excluded (they cannot help), as is the
/// target directory of the build being blocked — deleting the cache the
/// current command is about to populate is the single most expensive
/// choice available, and it is the one the reporter actually observed.
pub(crate) fn reclaim_target_dirs_for_block(build_volume: &std::path::Path) -> u64 {
    use crate::cache_lib::auto_gc::{AutoGcPathKind, VolumeProbe};

    let Ok(paths) = SoldrPaths::new() else {
        return 0;
    };
    if auto_gc_env_disabled() {
        return 0;
    }
    let Ok(config) = paths.load_config() else {
        return 0;
    };
    if !config.auto_gc.enabled {
        return 0;
    }
    let probe = SystemVolumeProbe;
    let Some(build_key) = probe.volume_key(build_volume) else {
        return 0;
    };
    let targets: Vec<std::path::PathBuf> = enumerate_auto_gc_paths(&paths)
        .into_iter()
        .filter(|entry| matches!(entry.kind, AutoGcPathKind::WorkspaceTarget))
        .map(|entry| entry.path)
        .filter(|path| !is_same_build_tree(path, build_volume))
        .filter(|path| probe.volume_key(path).as_deref() == Some(build_key.as_str()))
        .collect();
    if targets.is_empty() {
        return 0;
    }
    let (validated, _) = crate::cache_lib::auto_gc::validate_config(&config.auto_gc);
    // soldr#2700 deliberately does NOT add the filesystem discovery walk
    // here. This path runs synchronously in front of a build that is
    // already blocked on disk space, under `BLOCK_TIER_PRUNE_BUDGET`; a
    // sized walk of every dev root costs far more than the budget it is
    // trying to spend on deleting. The background sweeper's tier 2 does
    // the discovery, so a blocked build still benefits from it on the
    // next firing.
    run_soldr_target_purge_background(&paths, &targets, Vec::new(), validated.min_age_secs).reclaimed
}

/// Whether `candidate` is the build's own `target/` (or contains it).
///
/// The watchdog probes the target dir when it exists and the project
/// CWD when it does not, so both containment directions matter.
fn is_same_build_tree(candidate: &std::path::Path, build_volume: &std::path::Path) -> bool {
    candidate == build_volume
        || build_volume.starts_with(candidate)
        || candidate.starts_with(build_volume)
}

fn enumerate_auto_gc_paths(paths: &SoldrPaths) -> Vec<crate::cache_lib::auto_gc::AutoGcPath> {
    let mut out: Vec<crate::cache_lib::auto_gc::AutoGcPath> = Vec::new();
    if let Some(cargo_home) = crate::core::resolve_cargo_home() {
        out.push(crate::cache_lib::auto_gc::AutoGcPath {
            kind: crate::cache_lib::auto_gc::AutoGcPathKind::CargoHome,
            path: cargo_home,
        });
    }
    if let Some(rustup_home) = crate::core::resolve_rustup_home() {
        out.push(crate::cache_lib::auto_gc::AutoGcPath {
            kind: crate::cache_lib::auto_gc::AutoGcPathKind::RustupHome,
            path: rustup_home,
        });
    }
    out.push(crate::cache_lib::auto_gc::AutoGcPath {
        kind: crate::cache_lib::auto_gc::AutoGcPathKind::SoldrCache,
        path: paths.cache.clone(),
    });
    if let Ok(rows) = super::daemon_registry_rows(paths) {
        for row in rows {
            if row.path.exists() {
                out.push(crate::cache_lib::auto_gc::AutoGcPath {
                    kind: crate::cache_lib::auto_gc::AutoGcPathKind::WorkspaceTarget,
                    path: row.path,
                });
            }
        }
    }
    out
}

/// System volume probe — Windows uses the drive letter (`C`, `D`),
/// Unix uses the device id from `stat()`. Falls back to the canonical
/// path's root component when neither is available.
struct SystemVolumeProbe;

impl crate::cache_lib::auto_gc::DiskFreeProbe for SystemVolumeProbe {
    fn free_bytes(&self, path: &std::path::Path) -> Option<u64> {
        let probe = existing_filesystem_probe_path(path);
        available_space(&probe).ok()
    }
}

impl crate::cache_lib::auto_gc::VolumeProbe for SystemVolumeProbe {
    fn volume_key(&self, path: &std::path::Path) -> Option<String> {
        let probe = existing_filesystem_probe_path(path);
        volume_key_for_path(&probe)
    }
}

fn volume_key_for_path(path: &std::path::Path) -> Option<String> {
    crate::platform::fs::volume::identity(path)
}

fn append_auto_gc_log_line(log_path: &std::path::Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    use std::io::Write as _;
    writeln!(file, "{ts} {line}")?;
    Ok(())
}

fn rotate_auto_gc_log_if_needed(log_path: &std::path::Path, max_bytes: u64) -> std::io::Result<()> {
    let meta = match std::fs::metadata(log_path) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    if meta.len() < max_bytes {
        return Ok(());
    }
    let archive = log_path.with_extension("log.old");
    let _ = std::fs::remove_file(&archive);
    std::fs::rename(log_path, &archive)?;
    Ok(())
}
