// Age-based eviction of idle workspace `target/` directories (soldr#4031).
//
// Disk-pressure tier 2 only reclaims targets once a volume is already short
// on space, so a box with plenty of headroom kept every idle agent
// worktree's `target/` forever (one was 86G). `auto_gc.stale_target_days`
// opts into an unconditional pass: any registered or discovered target
// unused for that many days is deleted through the same guarded purge
// path tier 2 uses (allowlist roots, active-build guards, 256M floor,
// bounded budget). The filesystem discovery walk is costly, so the pass
// runs at most once per `STALE_TARGET_THROTTLE_SECONDS`.

/// How often the stale-target pass may run (once a day).
const STALE_TARGET_THROTTLE_SECONDS: u64 = 24 * 60 * 60;

fn stale_target_marker_path(paths: &SoldrPaths) -> std::path::PathBuf {
    paths.root.join(".auto_gc_stale_targets_marker")
}

/// Eviction age in seconds for `stale_target_days`, or `None` when the
/// pass is disabled. Never below the configured `min_age_secs` floor.
fn stale_target_age_secs(config: &crate::core::AutoGcConfig) -> Option<u64> {
    if config.stale_target_days == 0 {
        return None;
    }
    Some(
        config
            .stale_target_days
            .saturating_mul(24 * 60 * 60)
            .max(config.min_age_secs),
    )
}

fn run_stale_target_eviction(
    paths: &SoldrPaths,
    config: &crate::core::AutoGcConfig,
    log_path: &std::path::Path,
) {
    let Some(age_secs) = stale_target_age_secs(config) else {
        return;
    };
    let marker = stale_target_marker_path(paths);
    if !auto_gc_throttle_expired(&marker, STALE_TARGET_THROTTLE_SECONDS) {
        return;
    }
    let _ = touch_auto_gc_marker(&marker);

    let registered: Vec<std::path::PathBuf> = enumerate_auto_gc_paths(paths)
        .into_iter()
        .filter(|entry| {
            matches!(
                entry.kind,
                crate::cache_lib::auto_gc::AutoGcPathKind::WorkspaceTarget
            )
        })
        .map(|entry| entry.path)
        .collect();
    let known: Vec<_> = registered
        .iter()
        .map(|path| crate::cache_lib::target_registry::TargetRow {
            path: path.clone(),
            last_used: 0,
        })
        .collect();
    let roots = resolve_gc_dev_roots(paths).unwrap_or_default();
    let discovered = super::discovery::discovered_target_rows(
        &roots,
        super::discovery::AUTO_GC_DISCOVERY_MAX_DEPTH,
        &known,
    );
    let mut targets = registered;
    targets.extend(discovered.iter().map(|row| row.path.clone()));
    if targets.is_empty() {
        let _ = append_auto_gc_log_line(
            log_path,
            "auto-gc stale-targets skipped=true reason=no_workspace_targets",
        );
        return;
    }
    let outcome = run_soldr_target_purge_background(paths, &targets, discovered, age_secs);
    let _ = append_auto_gc_log_line(
        log_path,
        &format!(
            "auto-gc stale-targets days={} reclaimed_bytes={} candidates={} skipped={}",
            config.stale_target_days, outcome.reclaimed, outcome.candidates, outcome.skipped,
        ),
    );
}
