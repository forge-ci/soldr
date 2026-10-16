//! Rendering for `soldr daemon status`.
//!
//! Extracted from `soldr_main.rs` (soldr#2023) so that file could stop
//! growing — it is over the per-file ceiling and the ratchet correctly
//! refused the addition. The status surface is a self-contained unit: one
//! `StatusInfo` in, one JSON object or one human block out.

use crate::core::SoldrPaths;
use crate::daemon::protocol::StatusInfo;

/// Print the status block in whichever form was asked for.
pub(crate) fn render(info: &StatusInfo, paths: &SoldrPaths, json: bool) {
    // Cook-index aggregate stats (issue #576). Older daemons would emit
    // `cook_stats: None` — render as zero so the surface is stable.
    let cook = info.cook_stats_or_zero();
    // The broker route is version-partitioned, so a successful control tunnel
    // already proves that this CLI reached its own package-version route.
    let claimed_pkg = env!("CARGO_PKG_VERSION");
    // soldr#2023: the daemon's limit is the one it applied at startup, so
    // compare it against what this CLI would resolve now. A running daemon
    // keeps its startup limit for life, and that used to be invisible.
    let local = crate::core::jobs::resolve_compile_jobs(
        paths
            .load_config()
            .ok()
            .and_then(|c| c.jobs.max_parallel_compiles),
    );
    let jobs_match = info.compile_jobs as usize == local.jobs;

    if json {
        let payload = serde_json::json!({
            "running": true,
            "version": info.version,
            "pkg_version": claimed_pkg,
            "pkg_version_matches_cli": true,
            "cli_pkg_version": env!("CARGO_PKG_VERSION"),
            "pid": info.pid,
            "generation": info.generation,
            "uptime_secs": info.uptime_secs,
            "request_count": info.request_count,
            "in_flight_compiles": info.in_flight_compiles,
            "compile_jobs": {
                "daemon": info.compile_jobs,
                "daemon_source": info.compile_jobs_source,
                "cli_would_resolve": local.jobs,
                "cli_source": local.source.describe(),
                "matches": jobs_match,
            },
            "cook": {
                "entries": cook.entries,
                "total_bytes": cook.total_bytes,
                "hits_this_session": cook.hits_this_session,
            },
        });
        println!("{}", serde_json::to_string(&payload).unwrap_or_default());
        return;
    }

    println!(
        "soldr-daemon: pid={} generation={} uptime={}s requests={} protocol={}",
        info.pid, info.generation, info.uptime_secs, info.request_count, info.version
    );
    match info.in_flight_compiles {
        Some(count) => println!("  in-flight compiles: {count}"),
        None => println!("  in-flight compiles: unknown (daemon does not report it)"),
    }
    println!(
        "  pkg version: {} (this cli: {})",
        claimed_pkg,
        env!("CARGO_PKG_VERSION"),
    );
    println!(
        "  compile jobs: {} (from {}){}",
        info.compile_jobs,
        info.compile_jobs_source,
        if jobs_match {
            String::new()
        } else {
            format!(
                "  [MISMATCH — this cli resolves {} from {}; \
                 `soldr daemon stop` to apply it]",
                local.jobs,
                local.source.describe()
            )
        },
    );
    println!(
        "  cook: entries={} total_bytes={} hits_this_session={}",
        cook.entries, cook.total_bytes, cook.hits_this_session
    );
}

/// Print the `soldr daemon builds` / `daemon slow-builds` result.
///
/// Extracted from `soldr_main.rs` (soldr#2139) for the same reason `render`
/// was: that file is over the per-file ceiling, so a new dispatch arm has to
/// be paid for by moving a self-contained block out. This is one — a client
/// result in, one JSON object or one line per row out.
pub(crate) fn render_builds(
    result: Result<Vec<crate::daemon::protocol::BuildRecord>, crate::daemon::client::ClientError>,
    json: bool,
) -> Result<(), crate::core::SoldrError> {
    use crate::core::SoldrError;
    use crate::daemon::client::ClientError;
    match result {
        Ok(rows) => {
            if json {
                let payload = serde_json::json!({
                    "builds": rows.iter().map(|r| serde_json::json!({
                        "session_id": r.session_id,
                        "repo_root": r.repo_root,
                        "started_at_ms": r.started_at_ms,
                        "ended_at_ms": r.ended_at_ms,
                        "exit_code": r.exit_code,
                        "total_wall_ms": r.total_wall_ms,
                        "crate_count": r.crate_count,
                        "slowest_crate_us": r.slowest_crate_us,
                        "slowest_crate_name": r.slowest_crate_name,
                    })).collect::<Vec<_>>(),
                });
                println!("{}", serde_json::to_string(&payload).unwrap_or_default());
            } else if rows.is_empty() {
                println!("(no recorded builds)");
            } else {
                for r in rows {
                    let wall = r
                        .total_wall_ms
                        .map(|m| format!("{m}ms"))
                        .unwrap_or_else(|| "running".into());
                    let exit = r
                        .exit_code
                        .map(|c| format!("exit={c}"))
                        .unwrap_or_else(|| "exit=?".into());
                    let slowest = r.slowest_crate_name.as_deref().unwrap_or("(none)");
                    println!(
                        "session_id={} repo={} wall={} {} crates={} slowest={}",
                        r.session_id, r.repo_root, wall, exit, r.crate_count, slowest
                    );
                }
            }
            Ok(())
        }
        Err(ClientError::NotRunning) => {
            if json {
                println!("{}", serde_json::json!({"running": false, "builds": []}));
            } else {
                println!("soldr-daemon: not running");
            }
            Ok(())
        }
        Err(e) => Err(SoldrError::Other(format!("daemon builds failed: {e:?}"))),
    }
}
