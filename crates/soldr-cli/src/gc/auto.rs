//! Auto-GC under disk pressure (issue #323).
//!
//! Hook lives at the soldr cargo front door. On every cargo invocation
//! the wrapper consults a throttle marker and, if the throttle has
//! expired and the user hasn't opted out, spawns a detached background
//! thread that:
//!
//!   1. enumerates soldr-relevant paths and groups them by volume;
//!   2. probes free space per volume;
//!   3. runs the tiered GC plan only against volumes below the trigger;
//!   4. appends a structured line to ~/.soldr/logs/auto-gc.log.
//!
//! We deliberately spawn instead of running inline so the wrapper never
//! blocks the build. cargo's `.package-cache` mutex handles concurrent
//! invocations of `cargo clean gc` cleanly for us.

use crate::cargo_front_door::{available_space, existing_filesystem_probe_path};
use crate::core::SoldrPaths;
use crate::GcCargoArgs;

use super::cargo_native::invoke_cargo_native_gc;
use super::purge::resolve_gc_dev_roots;

const AUTO_GC_THROTTLE_SECONDS: u64 = 5 * 60;
const AUTO_GC_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
const AUTO_GC_DISABLE_ENV_VAR: &str = "SOLDR_AUTO_GC_DISABLED";
/// Retain daemon event rows for 30 days when no daemon owns this root.
const DAEMON_EVENT_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// soldr#1900: age at which an entry under `<cache>/tmp` is reclaimed.
///
/// Scratch is pinned to the cache volume so it can be swept from one place
/// instead of leaking into the OS temp dir forever. A day is comfortably
/// longer than any single build, so this never races an in-flight download
/// or an active test; anything older belongs to a process that is gone.
///
/// Reclaiming a still-wanted entry is cheap by construction -- everything
/// under scratch is either genuinely temporary or content-addressed and
/// re-derivable (e.g. the wrapper's stdin source file).
const SCRATCH_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// Issue #1286 (F5): spawn the auto-GC sweep as a DETACHED PROCESS at
/// build end instead of an in-process thread at build start.
///
/// The previous design (`maybe_kick_auto_gc`, a detached thread kicked
/// from the front door right after `build_active::set(true)`) could
/// never actually sweep: the thread woke up inside an active build,
/// deferred with `reason=build_active`, and re-armed the marker — on a
/// machine where soldr only runs during builds, the log showed days of
/// continuous deferrals and a 36 GB cache. A post-build thread doesn't
/// work either: the wrapper process exits right after cargo does,
/// killing the sweep mid-flight. A detached `soldr gc auto-sweep`
/// child survives the wrapper's exit and starts with
/// `build_active == false` in its own process.
///
/// The 5-minute throttle marker still bounds the spawn frequency, so
/// steady-state builds pay one `stat` here and at most one process
/// spawn per throttle window.
// reason: detached sweeper outlives this process; it writes its own
// diagnostics to `auto-gc.log`, so the inherited streams carry nothing.
#[cfg_attr(
    dylint_lib = "ban_swallowed_child_stdio",
    allow(ban_swallowed_child_stdio)
)]
pub(crate) fn maybe_spawn_auto_gc_sweeper(paths: &SoldrPaths) {
    if auto_gc_env_disabled() {
        return;
    }
    let cfg = match paths.load_config() {
        Ok(cfg) => cfg,
        Err(error) => {
            tracing::error!(%error, "not spawning auto-GC with invalid soldr config");
            return;
        }
    };
    let disk_pressure_enabled = cfg.auto_gc.enabled;
    let cook_enabled = cfg.cook.max_total_gb > 0 || cfg.cook.max_age_days > 0;
    let stale_targets_enabled = cfg.auto_gc.stale_target_days > 0;
    if !disk_pressure_enabled && !cook_enabled && !stale_targets_enabled {
        return;
    }
    let marker = crate::cache_lib::auto_gc_throttle_marker_path(paths);
    if !auto_gc_throttle_expired(&marker, AUTO_GC_THROTTLE_SECONDS) {
        return;
    }
    // Touch the marker before spawning so a crashing sweeper doesn't
    // cause us to immediately rerun on the next invocation.
    let _ = touch_auto_gc_marker(&marker);

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = sweeper_command(exe);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Detached background spawn: own process group, no console window —
    // the platform crate owns the flag mapping.
    let _ = crate::platform::process::spawn::spawn_detached(&mut cmd);
}

/// The detached sweeper's argv, with the two facts a self-spawn must state.
///
/// `current_exe()` is not always the `soldr` front door: under `soldr
/// ci-test`'s Dylint stages, and under any `tool-shims`/`toolchain link`
/// setup, this process was entered through a cargo-named multicall
/// hardlink. Spawning that path bare re-enters as `cargo gc auto-sweep`,
/// which the cargo front door runs in whatever cwd it inherited: on run
/// 33925584046 that was `dylints/ban_raw_env_flag`, where it created a
/// lint-local `target/` (with soldr's scrub marker) and died with exit 101
/// on cargo's "no such command", and the shared-tree guard then failed the
/// whole host validation. The argv0 override pins the identity to `soldr`;
/// the edge marker is what every first-party self-spawn carries (soldr#2739).
fn sweeper_command(exe: std::path::PathBuf) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["gc", "auto-sweep"]);
    cmd.env(crate::multicall::SHIM_ARGV0_ENV, "soldr");
    cmd.env(soldr_core::self_relocate::SELF_SPAWN_EDGE_ENV_VAR, "1");
    cmd
}

/// Issue #1286 (F5): synchronous entry point behind the hidden
/// `soldr gc auto-sweep` verb. The spawner above already consumed the
/// throttle marker, so this runs the sweep unconditionally (the
/// `build_active` preamble check still applies inside).
pub(crate) fn run_gc_auto_sweep_command() -> Result<(), crate::core::SoldrError> {
    let paths = SoldrPaths::new()?;
    let log_path = crate::cache_lib::auto_gc_log_path(&paths);
    // soldr#1790: best-effort prune of the always-on per-build XML logs
    // (plus any legacy `.json` files from interim builds before the
    // JSON->XML conversion) to the newest `BUILD_LOG_KEEP` entries.
    // Rides the same 5-minute throttle + detached-sweeper design as the
    // rest of this pass — no new throttle is introduced.
    let deleted = crate::build_log::prune_build_logs(
        &crate::build_log::build_logs_dir(&paths),
        crate::build_log::BUILD_LOG_KEEP,
    );
    tracing::debug!(deleted, "pruned per-build XML logs");
    // Porcelain: same pass, same throttle. `expires_at` in the JSON is this
    // TTL, and it is enforced here rather than by a new sweeper.
    let porcelain_deleted = crate::cargo_front_door::porcelain::prune(&paths);
    tracing::debug!(porcelain_deleted, "pruned porcelain run logs");
    run_auto_gc_background(paths.root.clone(), log_path);
    Ok(())
}

fn auto_gc_env_disabled() -> bool {
    match std::env::var(AUTO_GC_DISABLE_ENV_VAR) {
        Ok(value) => crate::core::flag_value(&value),
        Err(_) => false,
    }
}

fn auto_gc_throttle_expired(marker: &std::path::Path, throttle_seconds: u64) -> bool {
    let Ok(meta) = std::fs::metadata(marker) else {
        return true;
    };
    let Ok(modified) = meta.modified() else {
        return true;
    };
    let elapsed = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or(std::time::Duration::ZERO);
    elapsed.as_secs() >= throttle_seconds
}

fn touch_auto_gc_marker(marker: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(marker, "")
}

/// Issue #980 L7: re-arm the throttle marker so the next wrapper
/// invocation can rerun the deferred sweep. Called when the background
/// thread bails out because a `cargo build` is active in this process.
fn rearm_auto_gc_marker(paths: &SoldrPaths) {
    let marker = crate::cache_lib::auto_gc_throttle_marker_path(paths);
    // Best-effort: remove the marker so the next call sees the
    // throttle as expired. A failure to remove is silent — the
    // throttle window is 5 minutes, the worst case is one missed
    // sweep window.
    let _ = std::fs::remove_file(&marker);
}

fn build_activity_active(paths: &SoldrPaths) -> bool {
    if crate::cache_lib::build_active::is_active() {
        return true;
    }
    match crate::cache_lib::build_active::any_active(paths) {
        Ok(active) => active,
        Err(error) => {
            tracing::warn!(%error, "auto-GC lease probe failed closed");
            true
        }
    }
}

fn defer_for_active_build(paths: &SoldrPaths, log_path: &std::path::Path, stage: &str) {
    let _ = append_auto_gc_log_line(
        log_path,
        &format!("auto-gc status=deferred reason=build_active stage={stage}"),
    );
    rearm_auto_gc_marker(paths);
}

/// Reclaim stale entries under `<cache>/tmp` (soldr#1900).
///
/// Returns the number of entries removed. Best-effort throughout: a single
/// unremovable entry (still held open on Windows, or owned by another user)
/// must not abort the sweep or fail the GC pass.
fn sweep_stale_scratch(paths: &SoldrPaths, now_ms: i64) -> u64 {
    let root = crate::core::temp_root_for(paths);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return 0;
    };
    let mut removed = 0_u64;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let age_ok = metadata
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| now_ms.saturating_sub(d.as_millis() as i64) > SCRATCH_TTL_MS)
            .unwrap_or(false);
        if !age_ok {
            continue;
        }
        let path = entry.path();
        let outcome = if metadata.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if outcome.is_ok() {
            removed = removed.saturating_add(1);
        }
    }
    removed
}

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn run_auto_gc_background(paths_root: std::path::PathBuf, log_path: std::path::PathBuf) {
    use crate::cache_lib::auto_gc::DiskFreeProbe as _;
    use crate::cache_lib::auto_gc::VolumeProbe as _;
    let start = std::time::Instant::now();
    let paths = SoldrPaths::with_root(paths_root);

    // Issue #980 L7: cargo build is running in this same process. Yield
    // immediately so we don't compete for IO + the cargo
    // `.package-cache` mutex. Re-arm the marker so the post-build
    // wrapper invocation can pick up the deferred sweep.
    if build_activity_active(&paths) {
        tracing::debug!("auto-GC background tick deferred: build active");
        let _ = append_auto_gc_log_line(
            &log_path,
            "auto-gc status=deferred reason=build_active stage=preamble",
        );
        rearm_auto_gc_marker(&paths);
        return;
    }
    let _maintenance_lease =
        match crate::cache_lib::build_active::MaintenanceLease::try_acquire(&paths) {
            Ok(Some(lease)) => lease,
            Ok(None) => {
                defer_for_active_build(&paths, &log_path, "root_lease");
                return;
            }
            Err(error) => {
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!("auto-gc status=deferred reason=root_lease_failed error={error}"),
                );
                rearm_auto_gc_marker(&paths);
                return;
            }
        };

    // Issue #1286 (F5): record that a sweep actually STARTED. Before
    // this line the log could only ever contain deferrals and tier
    // actions, so "GC ran but found nothing to do" and "GC never ran"
    // were indistinguishable — which is how days of silent starvation
    // went unnoticed.
    let _ = append_auto_gc_log_line(&log_path, "auto-gc status=run stage=start");

    let cli_policy_config = paths.load_config().unwrap_or_default();
    let cli_policy_context = crate::cache_lib::gc_policy::GcContext {
        driver: crate::cache_lib::gc_policy::Driver::Cli,
        tick: crate::cache_lib::gc_policy::TickKind::Pressure,
        free_by_volume: Vec::new(),
        config: cli_policy_config,
        daemon_events_available: crate::cache_lib::data_db_path(&paths).exists(),
        daemon_live: crate::daemon::lifecycle::claimed_daemon_occupies_route(&paths).is_some(),
    };
    let cli_policy_actions = crate::cache_lib::gc_policy::plan(
        &crate::cache_lib::gc_policy::registry(),
        &cli_policy_context,
    );
    let has_policy_action = |id: &str| {
        cli_policy_actions
            .iter()
            .any(|action| action.category_id == id)
    };

    // Tier-0 (Phase 2): prune `daemon_events` rows older than 30 days
    // before any disk-pressure tiers run. Bounded, cheap, runs even when
    // the volume isn't below trigger so the event log can't grow
    // unbounded between auto-GC firings.
    // Scratch reclamation must NOT be gated on the state DB existing -- a
    // machine whose DB was never created (or was deleted) is exactly where
    // scratch accumulates unnoticed.
    let scratch_removed = if has_policy_action("scratch") {
        sweep_stale_scratch(
            &paths,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        )
    } else {
        0
    };
    if scratch_removed > 0 {
        let _ = append_auto_gc_log_line(
            &log_path,
            &format!("auto-gc tier=0 scratch_entries_reclaimed={scratch_removed}"),
        );
    }
    super::stale_versions::run_and_log(&paths, &log_path);

    // The daemon performs the same event-retention pass in its maintenance
    // loop. When it is stopped, run the pass only under the same root lock
    // the daemon would hold, never as an opportunistic second opener.
    let db_path = crate::cache_lib::data_db_path(&paths);
    if has_policy_action("daemon_events")
        && db_path.exists()
        && crate::daemon::lifecycle::claimed_daemon_occupies_route(&paths).is_none()
    {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0);
        match run_offline_daemon_event_prune(&paths, now_ms - DAEMON_EVENT_TTL_MS) {
            Ok(Some(removed)) if removed > 0 => {
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!("auto-gc tier=0 daemon_events_pruned={removed}"),
                );
            }
            Ok(_) => {}
            Err(error) => {
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!("auto-gc tier=0 daemon_events_deferred error={error}"),
                );
            }
        }
    }

    let full_config = match paths.load_config() {
        Ok(config) => config,
        Err(error) => {
            let _ = append_auto_gc_log_line(
                &log_path,
                &format!("auto-gc status=deferred reason=invalid_config error={error}"),
            );
            rearm_auto_gc_marker(&paths);
            return;
        }
    };
    let config = full_config.auto_gc.clone();
    let cook_config = full_config.cook.clone();

    // soldr#2310: install source cache is a disposable, content-addressed
    // category (extracted codeload zips + shallow clones) with an eager
    // 2-day TTL — the first value-bearing cache evicted, costing only a
    // network re-fetch to restore. Runs every throttle window regardless
    // of disk pressure, honoring SOLDR_INSTALL_SRC_TTL_DAYS /
    // SOLDR_NO_INSTALL_SRC_GC. Never opens state.sqlite3, so no daemon
    // ownership conflict.
    let install_source_removed = if has_policy_action("install_source") {
        crate::install::cache::sweep_with_config(&paths, full_config.install.source_ttl_days)
    } else {
        0
    };
    if install_source_removed > 0 {
        let _ = append_auto_gc_log_line(
            &log_path,
            &format!("auto-gc tier=0 install_source_entries_reclaimed={install_source_removed}"),
        );
    }
    let (validated, warnings) = crate::cache_lib::auto_gc::validate_config(&config);
    for warning in &warnings {
        let _ = append_auto_gc_log_line(&log_path, &format!("warning: {warning}"));
    }

    // `[cook]` auto-GC (issue #589). Independent of disk-pressure
    // tiering: cook artifacts can grow unbounded even on a volume that
    // is well above `trigger_free_gb`, so the eviction pass runs every
    // throttle window when the cook knobs are non-zero.
    if has_policy_action("cook") {
        // soldr#1814 slice 2b (criterion 2 — single owning process per file).
        // `cook_evict_pass` opens `state.sqlite3` via `cook_index`, so running it
        // here makes the CLI a second opener alongside the daemon.
        //
        // Skipping it when the daemon is up costs no coverage: the daemon runs
        // this exact pass in `maintenance::run_local_components`, driven by
        // `PRESSURE_INTERVAL` (5 min) — the same window as this sweeper's
        // `AUTO_GC_THROTTLE_SECONDS` (5 min). Its daily `Full` tick also adds
        // the absolute-age sweep, which this path never does. So the daemon's
        // coverage is a superset, not a delay.
        //
        // Use the version-blind route-claim occupancy check, NOT `is_live`.
        // `is_live` probes the optional broker and hashes the executable
        // identity — the #1832 note on `preflight_displace_stale_daemon`
        // records that costing tens of seconds — and CI caught exactly that
        // as a `PEP 517 daemon smoke (windows-x64)` failure here.
        //
        // Version-blind is also the semantically correct question: *any* live
        // soldr daemon owns state.sqlite3, whatever protocol it speaks.
        match crate::daemon::lifecycle::claimed_daemon_occupies_route(&paths) {
            Some(pid) => {
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!(
                        "cook-gc skipped: daemon pid={pid} owns state.sqlite3 and runs \
                         the same pass every 5 min (soldr#1814)"
                    ),
                );
            }
            None => match run_offline_cook_gc(&paths, &cook_config) {
                Ok(Some(report)) => log_cook_gc_report(&log_path, &report),
                Ok(None) => {
                    let _ = append_auto_gc_log_line(
                        &log_path,
                        "cook-gc deferred: daemon claimed root ownership during offline handoff",
                    );
                }
                Err(error) => {
                    let _ = append_auto_gc_log_line(
                        &log_path,
                        &format!("cook-gc deferred: offline ownership error={error}"),
                    );
                }
            },
        }
    }

    // `release-worktree` trash sweep (#710 follow-up). Runs every
    // throttle window so the per-volume `~/.soldr/trash-*/` buckets
    // get reclaimed without requiring the user to call
    // `soldr cache sweep-trash` manually. Tolerates per-entry failures
    // (Windows daemon may still hold handles); retries next pass.
    if has_policy_action("trash") {
        if let Ok(report) = crate::cache::sweep_trash(&paths) {
            if report.removed > 0 || report.retained > 0 {
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!(
                        "trash-sweep removed={} retained={}",
                        report.removed, report.retained,
                    ),
                );
            }
        }
    }

    run_stale_target_eviction(&paths, &validated, &log_path);

    if !validated.enabled {
        // Disk-pressure tiers off, but cook-gc above may still have
        // run. Done.
        return;
    }

    let auto_paths = enumerate_auto_gc_paths(&paths);
    let probe = SystemVolumeProbe;
    let plans = crate::cache_lib::auto_gc::plan_auto_gc(&validated, &auto_paths, &probe, &probe);
    if plans.is_empty() {
        return; // Either disabled or no volume is below trigger.
    }

    for plan in &plans {
        let line = format!(
            "auto-gc volume={} free_gib={:.2} trigger_gib={} target_gib={} paths={} status=detected",
            plan.volume_key,
            (plan.free_bytes as f64) / (crate::cache_lib::auto_gc::GIB as f64),
            validated.trigger_free_gb,
            validated.target_free_gb,
            plan.paths.len()
        );
        let _ = append_auto_gc_log_line(&log_path, &line);

        // Tier 1: conservative cargo GC (no explicit --max-*-age flags
        // so cargo uses its own conservative defaults). Only attempt
        // when the volume holds the cargo home.
        let mut last_tier = 0u8;
        let cargo_volume_paths = plan
            .paths
            .iter()
            .filter(|p| matches!(p.kind, crate::cache_lib::auto_gc::AutoGcPathKind::CargoHome))
            .count();
        if cargo_volume_paths > 0 {
            let outcome = run_conservative_cargo_gc_background(&log_path);
            last_tier = 1;
            let _ = append_auto_gc_log_line(
                &log_path,
                &format!(
                    "tier=1 volume={} exit_code={} skipped={} reason={}",
                    plan.volume_key,
                    outcome.exit_code,
                    outcome.skipped,
                    outcome.reason.as_deref().unwrap_or("ran")
                ),
            );
        }

        // Re-probe and decide whether to escalate.
        let mut free_bytes = probe.free_bytes(&plan.paths[0].path).unwrap_or(0);
        let target_bytes = validated
            .target_free_gb
            .saturating_mul(crate::cache_lib::auto_gc::GIB);

        // Tier 2: soldr target purge (only if the volume holds workspace
        // targets and we're still under target).
        if has_policy_action("workspace_targets")
            && crate::cache_lib::auto_gc::next_tier(free_bytes, target_bytes, last_tier).is_some()
        {
            let mut workspace_targets: Vec<_> = plan
                .paths
                .iter()
                .filter(|p| {
                    matches!(
                        p.kind,
                        crate::cache_lib::auto_gc::AutoGcPathKind::WorkspaceTarget
                    )
                })
                .map(|p| p.path.clone())
                .collect();
            let registry_rows_on_volume = workspace_targets.len();

            // soldr#2700: the registry alone is not a complete picture of
            // what is on disk. It is written only when a build goes
            // through soldr's cargo front door or the wrapper's
            // fire-and-forget daemon touch, so plain-`cargo` builds —
            // dylint drivers, vendored `.extern-repos/` checkouts, agent
            // worktrees — never appear, and tier 2 kept no-opping while
            // 106.4 GB of them filled the volume. Walk the allowlisted
            // dev roots for the rest.
            //
            // Only here, never in `enumerate_auto_gc_paths`: this is a
            // sized walk of the whole tree, and running it on every
            // sweeper firing would put that I/O in front of builds that
            // are not under pressure at all. Reaching tier 2 already
            // means tier 1 ran and the volume is still short.
            let discovery_roots = resolve_gc_dev_roots(&paths).unwrap_or_default();
            let known: Vec<_> = workspace_targets
                .iter()
                .map(|path| crate::cache_lib::target_registry::TargetRow {
                    path: path.clone(),
                    last_used: 0,
                })
                .collect();
            let discovered_rows: Vec<_> = super::discovery::discovered_target_rows(
                &discovery_roots,
                super::discovery::AUTO_GC_DISCOVERY_MAX_DEPTH,
                &known,
            )
            .into_iter()
            .filter(|row| probe.volume_key(&row.path).as_deref() == Some(&plan.volume_key))
            .collect();
            let discovered_on_volume = discovered_rows.len();
            workspace_targets.extend(discovered_rows.iter().map(|row| row.path.clone()));

            if workspace_targets.is_empty() {
                // soldr#2700: previously this branch wrote nothing at
                // all, so "tier 2 had no candidates" and "tier 2 never
                // ran" were the same empty log — the exact ambiguity
                // #1286 removed one level up. Always say it.
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!(
                        "tier=2 volume={} skipped=true reason=no_workspace_targets \
                         registry_rows_on_volume=0 discovered_on_volume=0 roots={}",
                        plan.volume_key,
                        discovery_roots.len(),
                    ),
                );
            } else {
                let tier2 = run_soldr_target_purge_background(
                    &paths,
                    &workspace_targets,
                    discovered_rows,
                    validated.min_age_secs,
                );
                last_tier = 2;
                free_bytes = probe.free_bytes(&plan.paths[0].path).unwrap_or(free_bytes);
                // Per-stage counts (#705) — distinguishes
                // "no candidates" vs "candidates filtered out
                // pre-delete" so the next time someone hits 0-byte
                // tier-2 reclaim, they can immediately see WHERE
                // candidates went. registry_rows_on_volume is the
                // pre-scan view; discovered_on_volume is what the
                // #2700 walk added on top of it; candidates is the
                // post-threshold + post-guard view; on_volume_matched
                // is the after-intersect-with-affected-volumes view.
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!(
                        "tier=2 volume={} reclaimed_bytes={} free_gib={:.2} \
                         registry_rows_on_volume={registry_rows_on_volume} \
                         discovered_on_volume={discovered_on_volume} \
                         candidates={} skipped={} \
                         dropped_missing={} on_volume_matched={}",
                        plan.volume_key,
                        tier2.reclaimed,
                        (free_bytes as f64) / (crate::cache_lib::auto_gc::GIB as f64),
                        tier2.candidates,
                        tier2.skipped,
                        tier2.dropped_missing,
                        tier2.on_volume_matched,
                    ),
                );
            }
        }

        // Tier 3: aggressive cargo GC (clamped to min_age_secs).
        // #705: probe for nightly before invoking — the unstable
        // `-Zgc` flag requires it, and without it every Tier-3
        // invocation exits non-zero silently. The probe lets us
        // record `skipped=true reason=no_nightly_toolchain` so the
        // user can see WHY tier 3 isn't helping and either install
        // nightly or accept that tier 3 won't fire on this box.
        if crate::cache_lib::auto_gc::next_tier(free_bytes, target_bytes, last_tier).is_some()
            && cargo_volume_paths > 0
        {
            if !nightly_toolchain_available() {
                last_tier = 3;
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!(
                        "tier=3 volume={} exit_code=0 skipped=true \
                         reason=no_nightly_toolchain free_gib={:.2}",
                        plan.volume_key,
                        (free_bytes as f64) / (crate::cache_lib::auto_gc::GIB as f64),
                    ),
                );
            } else {
                let ages =
                    crate::cache_lib::auto_gc::TIER3_AGES.clamped_seconds(validated.min_age_secs);
                let outcome = run_aggressive_cargo_gc_background(&log_path, &ages);
                last_tier = 3;
                free_bytes = probe.free_bytes(&plan.paths[0].path).unwrap_or(free_bytes);
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!(
                        "tier=3 volume={} exit_code={} skipped={} reason={} free_gib={:.2}",
                        plan.volume_key,
                        outcome.exit_code,
                        outcome.skipped,
                        outcome.reason.as_deref().unwrap_or("ran"),
                        (free_bytes as f64) / (crate::cache_lib::auto_gc::GIB as f64),
                    ),
                );
            }
        }

        if crate::cache_lib::auto_gc::next_tier(free_bytes, target_bytes, last_tier).is_none()
            && free_bytes < target_bytes
        {
            let _ = append_auto_gc_log_line(
                &log_path,
                &format!(
                    "auto-gc warning volume={} free_gib={:.2} target_gib={} \
                    tiers exhausted; run `soldr gc sweep --aggressive`",
                    plan.volume_key,
                    (free_bytes as f64) / (crate::cache_lib::auto_gc::GIB as f64),
                    validated.target_free_gb,
                ),
            );
        }
    }

    let _ = append_auto_gc_log_line(
        &log_path,
        &format!(
            "auto-gc done elapsed_ms={} volumes={}",
            start.elapsed().as_millis(),
            plans.len(),
        ),
    );
    let _ = rotate_auto_gc_log_if_needed(&log_path, AUTO_GC_LOG_MAX_BYTES);
}

/// Run cook eviction only while holding the daemon's root-ownership lock.
///
/// This is the coordinated offline counterpart to daemon maintenance: once
/// the lock is held, a daemon cannot start between the liveness probe and
/// `cook_evict_pass` opening `state.sqlite3`.
fn run_offline_cook_gc(
    paths: &SoldrPaths,
    config: &crate::core::CookConfig,
) -> Result<Option<crate::cache_lib::cook_gc::CookEvictReport>, std::io::Error> {
    let Some(_owner) = crate::daemon::lifecycle::RootOwnershipGuard::try_acquire(paths)? else {
        return Ok(None);
    };
    // soldr-state-db: offline-root-owner
    Ok(Some(crate::cache_lib::cook_gc::cook_evict_pass(
        paths, config,
    )))
}

fn run_offline_daemon_event_prune(
    paths: &SoldrPaths,
    cutoff_ms: i64,
) -> Result<Option<u64>, String> {
    let Some(_owner) = crate::daemon::lifecycle::RootOwnershipGuard::try_acquire(paths)
        .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    // soldr-state-db: offline-root-owner
    crate::daemon::db::prune_events_older_than(&crate::cache_lib::data_db_path(paths), cutoff_ms)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn log_cook_gc_report(
    log_path: &std::path::Path,
    report: &crate::cache_lib::cook_gc::CookEvictReport,
) {
    if report.time_evicted > 0
        || report.size_evicted > 0
        || report.quarantine_evicted > 0
        || report.errors > 0
    {
        let _ = append_auto_gc_log_line(
            log_path,
            &format!(
                "cook-gc protected={} time_evicted={} size_evicted={} \
                 quarantine_evicted={} bytes_freed={} errors={}",
                report.protected,
                report.time_evicted,
                report.size_evicted,
                report.quarantine_evicted,
                report.bytes_freed,
                report.errors,
            ),
        );
    }
}

struct AutoGcCargoOutcome {
    exit_code: i32,
    skipped: bool,
    reason: Option<String>,
}

fn run_conservative_cargo_gc_background(log_path: &std::path::Path) -> AutoGcCargoOutcome {
    let args = GcCargoArgs {
        dry_run: false,
        toolchain: None,
        max_src_age: None,
        max_crate_age: None,
        max_index_age: None,
        max_git_co_age: None,
        max_git_db_age: None,
        max_download_age: None,
        max_src_size: None,
        max_crate_size: None,
        max_git_size: None,
        max_download_size: None,
        json: true,
    };
    match invoke_cargo_native_gc(&args, true) {
        Ok(outcome) => AutoGcCargoOutcome {
            exit_code: outcome.exit_code,
            skipped: outcome.skipped,
            reason: outcome.skipped_reason,
        },
        Err(e) => {
            let _ = append_auto_gc_log_line(log_path, &format!("tier=1 invoke_error={e}"));
            AutoGcCargoOutcome {
                exit_code: 1,
                skipped: true,
                reason: Some(format!("invoke_error: {e}")),
            }
        }
    }
}

fn aggressive_cargo_gc_args(ages: &crate::cache_lib::auto_gc::CargoGcAgeSeconds) -> GcCargoArgs {
    GcCargoArgs {
        dry_run: false,
        toolchain: None,
        max_src_age: Some(crate::cache_lib::auto_gc::cargo_gc_duration_arg(
            ages.max_src,
        )),
        max_crate_age: Some(crate::cache_lib::auto_gc::cargo_gc_duration_arg(
            ages.max_crate,
        )),
        max_index_age: None,
        max_git_co_age: Some(crate::cache_lib::auto_gc::cargo_gc_duration_arg(
            ages.max_git_co,
        )),
        max_git_db_age: None,
        max_download_age: None,
        max_src_size: None,
        max_crate_size: None,
        max_git_size: None,
        max_download_size: None,
        json: true,
    }
}

fn run_aggressive_cargo_gc_background(
    log_path: &std::path::Path,
    ages: &crate::cache_lib::auto_gc::CargoGcAgeSeconds,
) -> AutoGcCargoOutcome {
    let args = aggressive_cargo_gc_args(ages);
    match invoke_cargo_native_gc(&args, true) {
        Ok(outcome) => AutoGcCargoOutcome {
            exit_code: outcome.exit_code,
            skipped: outcome.skipped,
            reason: outcome.skipped_reason,
        },
        Err(e) => {
            let _ = append_auto_gc_log_line(log_path, &format!("tier=3 invoke_error={e}"));
            AutoGcCargoOutcome {
                exit_code: 1,
                skipped: true,
                reason: Some(format!("invoke_error: {e}")),
            }
        }
    }
}

/// Outcome of one Tier-2 purge pass. Surfaces per-stage counts so the
/// auto-GC log distinguishes "nothing to clean" from "couldn't see
/// anything to clean" — the diagnostic gap documented in #705.
#[derive(Debug, Clone, Copy, Default)]
struct Tier2Outcome {
    /// Bytes actually deleted.
    reclaimed: u64,
    /// Eligible candidates returned by `scan` (passed threshold + guards).
    candidates: usize,
    /// Skipped rows (under threshold or guard-rejected).
    skipped: usize,
    /// Registry rows whose path no longer existed and were dropped.
    dropped_missing: usize,
    /// Candidates that survived the eligibility scan AND matched a
    /// volume in `workspace_targets`. If `candidates > 0` but
    /// `on_volume_matched == 0`, the on-volume filter ate everything
    /// (a path-normalisation drift between the registry rows and the
    /// volume-grouped paths the orchestrator handed us).
    on_volume_matched: usize,
}

/// Wall-clock ceiling for the synchronous reclaim that runs in front of a
/// blocked build (soldr#2134).
///
/// 30s is chosen to be longer than any plausible single `target/` removal and
/// far shorter than the stall it replaces. The tier only ever deletes cold,
/// off-build-tree candidates larger than 256 MB, so the common case finishes
/// well inside it and the budget is invisible.
pub(super) const BLOCK_TIER_PRUNE_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

fn run_soldr_target_purge_background(
    paths: &SoldrPaths,
    workspace_targets: &[std::path::PathBuf],
    discovered_rows: Vec<crate::cache_lib::target_registry::TargetRow>,
    min_age_secs: u64,
) -> Tier2Outcome {
    use crate::cache_lib::gc::{parse_size, GcOptions};
    let larger_than_bytes = parse_size("256M").unwrap_or(256 * 1024 * 1024);
    // Auto-GC always honors at least the configured min-age floor.
    // We never go below 1h.
    let older_than_seconds = crate::cache_lib::auto_gc::clamp_age_to_floor(min_age_secs, 3600);
    let options = GcOptions {
        older_than_seconds,
        larger_than_bytes,
        dev_roots: match resolve_gc_dev_roots(paths) {
            Ok(roots) => roots,
            Err(error) => {
                tracing::error!(%error, "refusing target cleanup with invalid soldr config");
                return Tier2Outcome::default();
            }
        },
        dry_run: false,
    };
    // soldr#2700: scan the registry *and* the rows the caller discovered on
    // the filesystem. Both go through the same snapshot scan, so the size
    // and age thresholds and every safety guard — the `dev_roots`
    // allowlist included — apply to a discovered target exactly as they do
    // to a registered one.
    let report = match super::discovery::daemon_gc_scan_with_rows(paths, &options, discovered_rows)
    {
        Ok(r) => r,
        Err(_) => return Tier2Outcome::default(),
    };
    let candidates = report.candidates.len();
    let skipped = report.skipped.len();
    let dropped_missing = report.dropped_missing;
    // Filter to candidates that actually live on the affected volumes.
    let mut reclaimed = 0u64;
    let mut on_volume_matched = 0usize;
    let on_volume: std::collections::HashSet<&std::path::Path> =
        workspace_targets.iter().map(|p| p.as_path()).collect();
    // soldr#2134: bounded, because this runs *synchronously* in front of a
    // build that is already blocked. Without a deadline the stall is as long
    // as the volume is dirty -- on a machine with many large stale targets
    // that is minutes, spent deleting, with no output. Deleting is
    // best-effort by construction, so stopping early simply leaves the
    // remaining candidates for the next pass (or for the block message).
    let deadline = std::time::Instant::now() + BLOCK_TIER_PRUNE_BUDGET;
    let mut budget_exhausted = false;
    let mut removed_rows = Vec::new();
    for cand in report.candidates {
        if !on_volume.contains(cand.path.as_path()) {
            continue;
        }
        // Checked before the delete, not after: one more multi-gigabyte
        // removal past the deadline is exactly what the budget exists to
        // prevent.
        if std::time::Instant::now() >= deadline {
            budget_exhausted = true;
            break;
        }
        on_volume_matched += 1;
        let bytes = cand.size_bytes;
        let outcome = crate::cache_lib::gc::delete_candidate_dir(cand);
        if outcome.removed {
            reclaimed = reclaimed.saturating_add(bytes);
            removed_rows.push(outcome.candidate.path);
        }
    }
    if !removed_rows.is_empty() {
        let _ = super::daemon_remove_registry_rows(paths, removed_rows);
    }
    if budget_exhausted {
        eprintln!(
            "soldr: reclaim budget ({}s) reached with candidates remaining;              freed {} so far. Run `soldr gc target --purge` to finish.",
            BLOCK_TIER_PRUNE_BUDGET.as_secs(),
            crate::cache_lib::target_registry::human_size(reclaimed),
        );
    }
    Tier2Outcome {
        reclaimed,
        candidates,
        skipped,
        dropped_missing,
        on_volume_matched,
    }
}

// Probe whether a `nightly` rustup toolchain is installed. The
// Tier-3 aggressive `cargo clean gc` requires the unstable `-Zgc`
// flag, so without nightly the tier silently fails with `exit_code=1`
// (which is what #705 reported as 100% of the Tier-3 invocations in
// the user's log). We probe via `rustup toolchain list` because it's
// the cheapest and most-reliable check that doesn't actually spawn
// rustc — see the auto-GC `tier=3 skipped=true reason=…` branch
// below for how the outcome is reported.
include!("auto_reclaim.rs");
include!("stale_targets.rs");
#[cfg(test)]
#[path = "auto_tests.rs"]
mod tests;
