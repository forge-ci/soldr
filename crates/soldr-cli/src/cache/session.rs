//! `soldr session start/end`, `soldr cache shutdown`, and `soldr cache flush` —
//! lifecycle commands for soldr-daemon's embedded zccache service and its
//! per-session journal.

use crate::core::{SoldrError, SoldrPaths};
use crate::daemon::protocol::{CacheFlushInfo, CompileStatsInfo};
use crate::zccache::managed_zccache_cache_dir;
use crate::JSON_SCHEMA_VERSION;
use serde::Serialize;

#[derive(Serialize)]
struct SessionStartOutput {
    schema_version: u32,
    command: &'static str,
    session_id: String,
    log_path: String,
    journal_path: String,
    stats_path: String,
    cache_dir: String,
    /// True when the caller already had a session in
    /// `ZCCACHE_SESSION_ID` and we did not contact the daemon. soldr#379
    /// requires session-start be idempotent for the calling process.
    reused: bool,
}

#[derive(Serialize)]
struct SessionEndOutput {
    schema_version: u32,
    command: &'static str,
    session_id: String,
    /// Embedded compile-counter delta since session start. `null` when the
    /// session was already gone (a second session-end is a no-op per the
    /// soldr#379 idempotency contract).
    stats: Option<serde_json::Value>,
    /// True when this call was a no-op because the named session had
    /// already been finalized.
    already_ended: bool,
    /// True when the caller passed `--clear` and journal/log/stats
    /// files were removed from disk.
    cleared: bool,
}

#[derive(Serialize)]
struct CacheShutdownOutput {
    schema_version: u32,
    command: &'static str,
    cache_dir: String,
    /// The session that was finalized (if any) before stopping the
    /// daemon.
    session_id: Option<String>,
    /// Whether a verified PID or a successful IPC reply proved a daemon was
    /// running when this command started.
    daemon_was_running: bool,
    /// Structured result of the explicit pre-shutdown cache checkpoint.
    flush: Option<CacheFlushInfo>,
    /// Whether the daemon acknowledged the wire Shutdown request.
    shutdown_requested: bool,
    /// Whether the acknowledged daemon generation or a signal-safe,
    /// verified-PID compatibility fallback was observed to stop. False when
    /// it was already absent.
    daemon_stopped: bool,
    /// Whether the daemon process was observed to have actually exited
    /// after the Soldr wire shutdown request (the acknowledgement can
    /// precede process exit, so Soldr observes the exact responder generation).
    /// `false` when the daemon was never running, when polling was
    /// disabled with `--no-wait`, or when polling timed out.
    daemon_exited: bool,
    /// Where session logs were archived to, if `--archive-logs` was
    /// supplied.
    archive_dir: Option<String>,
    /// Diagnostic notes for the human-facing print path.
    notes: Vec<String>,
}

#[derive(Serialize)]
struct CacheFlushOutput {
    schema_version: u32,
    command: &'static str,
    cache_dir: String,
    /// True only when every embedded checkpoint phase completed.
    flushed: bool,
    /// Structured embedded-cache persistence report.
    stats: Option<CacheFlushInfo>,
    /// Diagnostic notes for the human-facing print path.
    notes: Vec<String>,
}

pub(crate) async fn run_session_start_command(
    id: Option<String>,
    log: Option<std::path::PathBuf>,
    journal: Option<std::path::PathBuf>,
    json: bool,
) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;
    std::fs::create_dir_all(&zccache_dir)?;
    std::fs::create_dir_all(zccache_dir.join("logs"))?;

    let session_log_path = log.unwrap_or_else(|| crate::cache_lib::session_log_path(&zccache_dir));
    let journal_path =
        journal.unwrap_or_else(|| crate::cache_lib::session_journal_path(&zccache_dir));
    let session_stats_path = crate::cache_lib::session_stats_path(&zccache_dir);

    // Idempotent path: if ZCCACHE_SESSION_ID is already set and the
    // caller did not pass an explicit --id, reuse the existing session
    // without contacting the daemon. soldr#379 requires this so
    // setup-soldr can call session-start repeatedly without spawning
    // orphan sessions.
    if id.is_none() {
        if let Ok(existing) = std::env::var(crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR) {
            let trimmed = existing.trim();
            if !trimmed.is_empty() {
                validate_session_id(trimmed)?;
                let output = SessionStartOutput {
                    schema_version: JSON_SCHEMA_VERSION,
                    command: "session-start",
                    session_id: trimmed.to_string(),
                    log_path: session_log_path.display().to_string(),
                    journal_path: journal_path.display().to_string(),
                    stats_path: session_stats_path.display().to_string(),
                    cache_dir: zccache_dir.display().to_string(),
                    reused: true,
                };
                emit_session_start(&output, json)?;
                return Ok(());
            }
        }
    }

    // soldr#1368: sessions are no longer external zccache-daemon sessions.
    // Mint a local id and capture a baseline of the soldr-daemon embedded
    // zccache service's cumulative compile counters so `soldr session end`
    // can diff against it. Best-effort: if the daemon isn't up yet, record
    // a zero baseline (this session's compiles all count against it).
    let session_id = id.unwrap_or_else(mint_session_id);
    validate_session_id(&session_id)?;
    let baseline = embedded_compile_stats(&paths).unwrap_or_default();
    write_session_baseline(&zccache_dir, &session_id, &baseline)?;

    let output = SessionStartOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "session-start",
        session_id,
        log_path: session_log_path.display().to_string(),
        journal_path: journal_path.display().to_string(),
        stats_path: session_stats_path.display().to_string(),
        cache_dir: zccache_dir.display().to_string(),
        reused: false,
    };
    emit_session_start(&output, json)?;
    Ok(())
}

/// Generate a local build-session id (soldr#1368). No longer a zccache
/// daemon session id — a blake3 of (pid, monotonic nanos) suffices.
fn mint_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = zccache::hash::StreamHasher::new();
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&nanos.to_le_bytes());
    hasher.finalize().to_hex()[..24].to_string()
}

const MAX_SESSION_ID_LEN: usize = 128;

/// Validate a session identifier before using it in any filesystem path.
///
/// IDs are intentionally portable across Unix and Windows archive paths.
fn validate_session_id(session_id: &str) -> Result<(), SoldrError> {
    let valid = !session_id.is_empty()
        && session_id.len() <= MAX_SESSION_ID_LEN
        && session_id != "."
        && session_id != ".."
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(SoldrError::Other(format!(
            "invalid session id {session_id:?}: use 1-{MAX_SESSION_ID_LEN} ASCII letters, \
             digits, '.', '_', or '-', excluding '.' and '..'"
        )))
    }
}

/// Query the soldr-daemon embedded zccache service for its cumulative
/// compile counters. `None` when the daemon is not reachable.
fn embedded_compile_stats(paths: &SoldrPaths) -> Option<CompileStatsInfo> {
    let sock = crate::daemon::server::server_sock_path(paths);
    crate::daemon::client::compile_stats(&sock).ok()
}

fn session_baseline_path(
    zccache_dir: &std::path::Path,
    session_id: &str,
) -> Result<std::path::PathBuf, SoldrError> {
    validate_session_id(session_id)?;
    Ok(zccache_dir
        .join("logs")
        .join(format!("session-{session_id}.baseline.json")))
}

fn write_session_baseline(
    zccache_dir: &std::path::Path,
    session_id: &str,
    stats: &CompileStatsInfo,
) -> Result<(), SoldrError> {
    let path = session_baseline_path(zccache_dir, session_id)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(stats)
        .map_err(|e| SoldrError::Other(format!("failed to serialize session baseline: {e}")))?;
    std::fs::write(&path, json)?;
    Ok(())
}

fn read_session_baseline(
    zccache_dir: &std::path::Path,
    session_id: &str,
) -> Option<CompileStatsInfo> {
    let path = session_baseline_path(zccache_dir, session_id).ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// soldr#1538: number of rustc-wrapper invocations (cache hits + misses +
/// non-cacheable) the daemon has recorded for this session since
/// [`capture_build_baseline`] snapshotted the baseline, without consuming
/// (removing) the baseline file — unlike [`finalize_build_session_stats`],
/// which is still the sole owner of that cleanup and runs later in the
/// invocation. Used by the rust-plan save-tail (issue #1538) to prove that
/// a just-finished cargo invocation could not have written anything new
/// into `target/`: `Some(0)` only when the daemon was reachable at both
/// baseline and now, so a real zero was observed rather than assumed.
/// `None` when the daemon was unreachable at either end — callers must
/// treat that as "unproven" and never skip on it.
pub(crate) fn compilations_since_baseline(
    zccache_dir: &std::path::Path,
    session_id: &str,
) -> Option<u64> {
    let paths = SoldrPaths::new().ok()?;
    let current = embedded_compile_stats(&paths)?;
    let baseline = read_session_baseline(zccache_dir, session_id)?;
    compilation_delta(&baseline, &current)
}

/// Return a trustworthy cumulative-counter delta. A counter that moved
/// backwards means the daemon restarted (or its state was reset) between
/// snapshots, so zero is not a valid conclusion and callers must fall back to
/// a real save.
fn compilation_delta(baseline: &CompileStatsInfo, current: &CompileStatsInfo) -> Option<u64> {
    current
        .total_compilations
        .checked_sub(baseline.total_compilations)
}

/// soldr#1368 observability restore — build-start half. Snapshot the
/// embedded zccache compile counters (via the soldr-daemon) so
/// [`finalize_build_session_stats`] can diff them into per-build
/// hit/miss figures. Best-effort: if the daemon isn't up yet no baseline
/// is written, and the finalize step then treats the baseline as all
/// zero — correct because a daemon that starts fresh for this build has
/// cumulative counters equal to this build's stats.
pub(crate) fn capture_build_baseline(zccache_dir: &std::path::Path, session_id: &str) {
    if validate_session_id(session_id).is_err() {
        return;
    }
    let Ok(paths) = SoldrPaths::new() else {
        return;
    };
    if let Some(baseline) = embedded_compile_stats(&paths) {
        let _ = write_session_baseline(zccache_dir, session_id, &baseline);
    }
}

/// soldr#1368 observability restore — build-end half. Diff the embedded
/// zccache compile counters against the build-start baseline and write
/// the per-build hit/miss summary to `last-session-stats.json`, the
/// artifact `soldr cache report` (and the perf harness) read. Restores
/// the reporting the pre-#1368 managed `zccache session-end` path used
/// to produce. A missing baseline is treated as all-zero (fresh daemon).
/// No-op when the daemon is unreachable at end (nothing to report) — which
/// leaves the *previous* invocation's file in place, so what is written is
/// keyed with [`key_session_stats`] and readers reject a file from another
/// session (soldr#3540).
pub(crate) fn finalize_build_session_stats(zccache_dir: &std::path::Path, session_id: &str) {
    if validate_session_id(session_id).is_err() {
        return;
    }
    let Ok(paths) = SoldrPaths::new() else {
        return;
    };
    let Some(current) = embedded_compile_stats(&paths) else {
        return;
    };
    let baseline = read_session_baseline(zccache_dir, session_id).unwrap_or_default();
    if let Some(stats) = compute_session_stats(Some(&baseline), Some(&current)) {
        let path = crate::cache_lib::session_stats_path(zccache_dir);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let stats = key_session_stats(stats, session_id);
        if let Ok(json) = serde_json::to_string(&stats) {
            let _ = std::fs::write(&path, json);
        }
    }
    if let Ok(path) = session_baseline_path(zccache_dir, session_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// soldr#3540: stamp the baseline-diff with the session that produced it.
///
/// `last-session-stats.json` is a fixed name shared by every front-door
/// invocation, so the counters alone cannot say *which* build they came from.
/// When [`finalize_build_session_stats`] no-ops — the daemon is unreachable
/// at end of build — the file still holds the previous invocation's numbers;
/// keying it lets `read_build_cache_summary` tell that stale file from this
/// session's own and report "no stats for this session" instead of repeating
/// the prior build's figures verbatim.
fn key_session_stats(mut stats: serde_json::Value, session_id: &str) -> serde_json::Value {
    stats["session_id"] = serde_json::json!(session_id);
    stats
}

/// Diff a session-end stats snapshot against the session-start baseline
/// into per-session `hits`/`misses`/`hit_rate`/… JSON. `None` when the
/// daemon was unreachable at end (no current snapshot to diff).
fn compute_session_stats(
    baseline: Option<&CompileStatsInfo>,
    current: Option<&CompileStatsInfo>,
) -> Option<serde_json::Value> {
    let (base, cur) = (baseline?, current?);
    let hits = cur.cache_hits.saturating_sub(base.cache_hits);
    let misses = cur.cache_misses.saturating_sub(base.cache_misses);
    let non_cacheable = cur.non_cacheable.saturating_sub(base.non_cacheable);
    let errors = cur.compile_errors.saturating_sub(base.compile_errors);
    let compilations = cur
        .total_compilations
        .saturating_sub(base.total_compilations);
    let time_saved_ms = cur.time_saved_ms.saturating_sub(base.time_saved_ms);
    let denom = hits + misses;
    let hit_rate = if denom > 0 {
        hits as f64 / denom as f64
    } else {
        0.0
    };
    let mut stats = serde_json::json!({
        "status": "ok",
        "hits": hits,
        "misses": misses,
        "non_cacheable": non_cacheable,
        "errors": errors,
        "compilations": compilations,
        "time_saved_ms": time_saved_ms,
        "hit_rate": hit_rate,
    });
    if let Some(staged) =
        staged_profile_delta(base.staged_profile.as_ref(), cur.staged_profile.as_ref())
    {
        stats["phase_profile"] = serde_json::json!({"staged": staged});
    }
    Some(stats)
}

fn staged_profile_delta(
    baseline: Option<&crate::daemon::protocol::StagedProfileInfo>,
    current: Option<&crate::daemon::protocol::StagedProfileInfo>,
) -> Option<crate::daemon::protocol::StagedProfileInfo> {
    let current = current?;
    let delta = |current: &std::collections::BTreeMap<String, u64>,
                 baseline: Option<&std::collections::BTreeMap<String, u64>>| {
        current
            .iter()
            .map(|(key, value)| {
                let baseline = baseline
                    .and_then(|values| values.get(key))
                    .copied()
                    .unwrap_or(0);
                (key.clone(), value.saturating_sub(baseline))
            })
            .collect()
    };
    Some(crate::daemon::protocol::StagedProfileInfo {
        counters: delta(&current.counters, baseline.map(|profile| &profile.counters)),
        timings_ns: delta(
            &current.timings_ns,
            baseline.map(|profile| &profile.timings_ns),
        ),
        bytes: delta(&current.bytes, baseline.map(|profile| &profile.bytes)),
        failures: delta(&current.failures, baseline.map(|profile| &profile.failures)),
    })
}

fn emit_session_start(output: &SessionStartOutput, json: bool) -> Result<(), SoldrError> {
    if json {
        // Single-line JSON keeps the output greppable by shell consumers
        // and `actions/core` parsers; see soldr#379 contract.
        let line = serde_json::to_string(output)
            .map_err(|e| SoldrError::Other(format!("failed to serialize session-start: {e}")))?;
        println!("{line}");
    } else {
        println!("ZCCACHE_SESSION_ID={}", output.session_id);
        println!("log: {}", output.log_path);
        println!("journal: {}", output.journal_path);
        println!("stats: {}", output.stats_path);
        if output.reused {
            println!("(reused existing session via ZCCACHE_SESSION_ID env var)");
        }
    }
    Ok(())
}

pub(crate) fn run_session_end_command(
    id: Option<String>,
    clear: bool,
    json: bool,
) -> Result<(), SoldrError> {
    let session_id = match id.or_else(|| {
        std::env::var(crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }) {
        Some(value) => value,
        None => {
            return Err(SoldrError::Other(format!(
                "session-end requires --id or ${} to be set",
                crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR
            )));
        }
    };
    validate_session_id(&session_id)?;

    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;

    // soldr#1368: diff the embedded zccache compile counters against the
    // baseline captured at `session start`. A missing baseline means the
    // session was never started here (or was already ended — the baseline
    // is consumed on end), so this is a no-op idempotent second call.
    let baseline = read_session_baseline(&zccache_dir, &session_id);
    let already_ended = baseline.is_none();
    let stats = if already_ended {
        None
    } else {
        let current = embedded_compile_stats(&paths);
        compute_session_stats(baseline.as_ref(), current.as_ref())
    };
    // Consume the baseline so a second `session end` is a clean no-op
    // (soldr#379 idempotency contract).
    let _ = std::fs::remove_file(session_baseline_path(&zccache_dir, &session_id)?);

    let cleared = if clear {
        clear_session_artifacts(&zccache_dir)?
    } else {
        false
    };

    let output = SessionEndOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "session-end",
        session_id: session_id.clone(),
        stats,
        already_ended,
        cleared,
    };

    if json {
        let line = serde_json::to_string(&output)
            .map_err(|e| SoldrError::Other(format!("failed to serialize session-end: {e}")))?;
        println!("{line}");
    } else {
        println!("session-end: {}", session_id);
        if output.already_ended {
            println!("  status: already-ended (no-op)");
        } else if let Some(stats) = &output.stats {
            if let Some(hits) = stats.get("hits").and_then(|v| v.as_u64()) {
                let misses = stats.get("misses").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("  hits/misses: {hits}/{misses}");
            }
            if let Some(rate) = stats.get("hit_rate").and_then(|v| v.as_f64()) {
                println!("  hit rate: {:.1}%", rate * 100.0);
            }
        }
        if cleared {
            println!("  cleared: journal/log/stats removed");
        }
    }
    Ok(())
}

fn clear_session_artifacts(zccache_dir: &std::path::Path) -> Result<bool, SoldrError> {
    let mut removed_any = false;
    for path in [
        crate::cache_lib::session_journal_path(zccache_dir),
        crate::cache_lib::session_log_path(zccache_dir),
        crate::cache_lib::session_stats_path(zccache_dir),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => removed_any = true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(SoldrError::Other(format!(
                    "failed to remove {}: {err}",
                    path.display()
                )));
            }
        }
    }
    Ok(removed_any)
}

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub(crate) async fn run_cache_shutdown_command(
    archive_logs: Option<std::path::PathBuf>,
    no_depgraph_save: bool,
    shutdown_timeout_seconds: u64,
    wait: bool,
    json: bool,
) -> Result<(), SoldrError> {
    if archive_logs.is_some() && !wait {
        return Err(SoldrError::Other(
            "--archive-logs requires daemon quiescence and cannot be combined with --no-wait"
                .into(),
        ));
    }
    if wait && shutdown_timeout_seconds == 0 {
        return Err(SoldrError::Other(
            "--shutdown-timeout-seconds must be greater than zero".into(),
        ));
    }
    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;
    let mut notes: Vec<String> = Vec::new();
    let daemon_pid = crate::daemon::lifecycle::claimed_daemon_occupies_route(&paths);

    let mut output = CacheShutdownOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache shutdown",
        cache_dir: zccache_dir.display().to_string(),
        session_id: None,
        daemon_was_running: daemon_pid.is_some(),
        flush: None,
        shutdown_requested: false,
        daemon_stopped: false,
        daemon_exited: false,
        archive_dir: None,
        notes: Vec::new(),
    };
    let mut failure = None;

    let env_session_id = std::env::var(crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    if let Some(session_id) = env_session_id.as_deref() {
        validate_session_id(session_id)?;
    }

    // End the active session (consume its stats baseline) so a following
    // `session end` is a clean no-op.
    if let Some(session_id) = env_session_id.as_deref() {
        let _ = std::fs::remove_file(session_baseline_path(&zccache_dir, session_id)?);
        output.session_id = Some(session_id.to_string());
    }

    let sock = crate::daemon::server::server_sock_path(&paths);
    if no_depgraph_save {
        notes.push(
            "explicit pre-shutdown cache checkpoint skipped by --no-depgraph-save; \
             graceful daemon shutdown still runs its internal flush"
                .into(),
        );
    } else if daemon_pid.is_some() {
        match crate::daemon::client::flush_caches(&sock) {
            Ok(report) => {
                output.daemon_was_running = true;
                if report.is_complete() {
                    notes.push("embedded zccache checkpoint completed".into());
                } else {
                    failure = Some(incomplete_flush_message(&report));
                    notes.push("embedded zccache checkpoint was incomplete".into());
                }
                output.flush = Some(report);
            }
            Err(crate::daemon::client::ClientError::NotRunning) => {}
            Err(err) => {
                let message = format!("embedded zccache checkpoint failed: {err:?}");
                notes.push(message.clone());
                failure = Some(message);
            }
        }
    }

    if daemon_pid.is_none() {
        output.daemon_exited = true;
        notes.push("soldr-daemon was already stopped".into());
    } else {
        match crate::daemon::client::shutdown(&sock) {
            Ok(responder) => {
                output.daemon_was_running = true;
                output.shutdown_requested = true;
                let timeout = if wait {
                    std::time::Duration::from_secs(shutdown_timeout_seconds)
                } else {
                    std::time::Duration::ZERO
                };
                let outcome = crate::daemon::lifecycle::wait_for_shutdown_responder(
                    &paths, &sock, responder, timeout,
                );
                output.daemon_exited = outcome.is_complete();
                output.daemon_stopped = output.daemon_exited;
                if wait && !output.daemon_exited {
                    failure.get_or_insert_with(|| {
                        format!(
                        "soldr-daemon generation {} (pid {}) acknowledged shutdown but is still \
                         completing its graceful flush after {shutdown_timeout_seconds}s; it was \
                         not force-killed",
                        responder.generation, responder.pid,
                    )
                    });
                }
            }
            Err(crate::daemon::client::ClientError::NotRunning) => {
                output.daemon_exited = true;
                notes.push("soldr-daemon was already stopped".into());
            }
            Err(err) => {
                notes.push(format!(
                    "wire shutdown failed ({err:?}); attempting verified-PID displacement"
                ));
                if daemon_pid.is_some()
                    && crate::daemon::lifecycle::displace_stale_daemon(
                        &paths,
                        Some(crate::daemon::lifecycle::LifecycleSource::Cli),
                    )
                {
                    output.daemon_stopped = true;
                    output.daemon_exited = true;
                    notes.push("soldr-daemon stopped through verified-PID fallback".into());
                } else {
                    failure.get_or_insert_with(|| {
                        format!(
                            "daemon shutdown failed without a trusted acknowledgement or \
                         signal-safe PID: {err:?}"
                        )
                    });
                }
            }
        }
    }

    // Archive only after the daemon generation is proven quiescent. Copying
    // earlier races the final event/index flush and produces partial logs.
    if let Some(archive_root) = archive_logs.as_ref() {
        if output.daemon_exited {
            let session_id = output
                .session_id
                .clone()
                .or_else(|| env_session_id.clone())
                .unwrap_or_else(|| "no-session".to_string());
            validate_session_id(&session_id)?;
            let target = archive_root.join(&session_id);
            std::fs::create_dir_all(&target)?;
            let mut copied = 0u32;
            for path in [
                crate::cache_lib::session_log_path(&zccache_dir),
                crate::cache_lib::session_journal_path(&zccache_dir),
                crate::cache_lib::session_stats_path(&zccache_dir),
            ] {
                if !path.exists() {
                    continue;
                }
                if let Some(filename) = path.file_name() {
                    std::fs::copy(&path, target.join(filename))?;
                    copied += 1;
                } else {
                    failure.get_or_insert_with(|| {
                        format!("session artifact has no filename: {}", path.display())
                    });
                }
            }
            output.archive_dir = Some(target.display().to_string());
            notes.push(format!(
                "archived {copied} session file(s) to {} after daemon quiescence",
                target.display()
            ));
        } else {
            failure.get_or_insert_with(|| {
                "refusing to archive session logs before the daemon is quiescent".into()
            });
        }
    }

    output.notes = notes;

    if json {
        let line = serde_json::to_string(&output)
            .map_err(|e| SoldrError::Other(format!("failed to serialize cache shutdown: {e}")))?;
        println!("{line}");
    } else {
        println!("soldr cache shutdown");
        if let Some(session_id) = &output.session_id {
            println!("  session-end: {session_id}");
        }
        if let Some(archive) = &output.archive_dir {
            println!("  archive: {archive}");
        }
        println!(
            "  soldr-daemon: {}",
            if output.daemon_exited {
                "stopped"
            } else if output.shutdown_requested {
                "shutdown requested"
            } else {
                "already absent"
            }
        );
        for note in &output.notes {
            println!("  note: {note}");
        }
    }
    if let Some(message) = failure {
        return Err(SoldrError::Other(message));
    }
    Ok(())
}

pub(crate) async fn run_cache_flush_command(json: bool) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;
    let mut output = CacheFlushOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache flush",
        cache_dir: zccache_dir.display().to_string(),
        flushed: false,
        stats: None,
        notes: Vec::new(),
    };

    // soldr#1368: flush the soldr-daemon embedded zccache state to disk
    // (artifact index, depgraph snapshot, metadata cache) so archives taken
    // afterwards restore with warm rustc hits. There is no separate managed
    // zccache daemon to flush any more.
    let sock = crate::daemon::server::server_sock_path(&paths);
    let failure = match crate::daemon::client::flush_caches(&sock) {
        Ok(report) => {
            output.flushed = report.is_complete();
            let failure = if report.is_complete() {
                output
                    .notes
                    .push("embedded zccache checkpoint completed".into());
                None
            } else {
                output
                    .notes
                    .push("embedded zccache checkpoint was incomplete".into());
                Some(incomplete_flush_message(&report))
            };
            output.stats = Some(report);
            failure
        }
        Err(err) => {
            let message = format!("soldr-daemon embedded flush unavailable: {err:?}");
            output.notes.push(message.clone());
            Some(message)
        }
    };

    if json {
        let line = serde_json::to_string(&output)
            .map_err(|e| SoldrError::Other(format!("failed to serialize cache flush: {e}")))?;
        println!("{line}");
    } else {
        println!("soldr cache flush");
        let status = match output.stats.as_ref() {
            Some(report) if report.is_complete() => "completed",
            Some(_) => "incomplete",
            None => "unavailable",
        };
        println!("  status: {}", status);
        for note in &output.notes {
            println!("  note: {note}");
        }
    }
    if let Some(message) = failure {
        return Err(SoldrError::Other(message));
    }
    Ok(())
}

fn incomplete_flush_message(report: &CacheFlushInfo) -> String {
    format!(
        "embedded zccache checkpoint incomplete: {}",
        report.incomplete_reason()
    )
}

#[cfg(test)]
mod tests {
    use super::super::report::zccache_analyze_failure_note;
    use super::super::{output_snippet, ANALYZE_NOTE_LIMIT};
    use super::{
        clear_session_artifacts, compilation_delta, compute_session_stats, key_session_stats,
        session_baseline_path, validate_session_id,
    };

    fn compile_stats(total_compilations: u64) -> crate::daemon::protocol::CompileStatsInfo {
        crate::daemon::protocol::CompileStatsInfo {
            total_compilations,
            ..Default::default()
        }
    }

    #[test]
    fn session_ids_cannot_escape_baseline_or_archive_directories() {
        for invalid in [
            "",
            ".",
            "..",
            "../escape",
            "nested/path",
            r"nested\path",
            "contains space",
        ] {
            assert!(
                validate_session_id(invalid).is_err(),
                "{invalid:?} must be rejected"
            );
        }
        let root = std::path::Path::new("/cache");
        assert_eq!(
            session_baseline_path(root, "build-1.main_ok").expect("valid id"),
            root.join("logs/session-build-1.main_ok.baseline.json")
        );
    }

    #[test]
    fn compilation_delta_accepts_monotonic_zero_and_nonzero_counts() {
        assert_eq!(
            compilation_delta(&compile_stats(41), &compile_stats(41)),
            Some(0)
        );
        assert_eq!(
            compilation_delta(&compile_stats(41), &compile_stats(44)),
            Some(3)
        );
    }

    #[test]
    fn compilation_delta_rejects_daemon_counter_reset() {
        assert_eq!(
            compilation_delta(&compile_stats(41), &compile_stats(0)),
            None,
            "a daemon restart must be unproven, never misreported as zero compiles"
        );
    }

    /// soldr#3540: whatever a finalize writes must carry the id of the
    /// session that finalized it — the file name is fixed, so the key is the
    /// only thing that lets a later front door refuse these numbers as stale.
    #[test]
    fn session_stats_are_keyed_to_the_session_that_wrote_them() {
        let mut baseline = compile_stats(10);
        baseline.cache_hits = 400;
        baseline.cache_misses = 9;
        let mut current = compile_stats(460);
        current.cache_hits = 442;
        current.cache_misses = 9;
        let stats = compute_session_stats(Some(&baseline), Some(&current)).expect("stats");
        let keyed = key_session_stats(stats, "build-session-7f3a");
        assert_eq!(keyed["session_id"], serde_json::json!("build-session-7f3a"));
        assert_eq!(keyed["status"], serde_json::json!("ok"));
        assert_eq!(keyed["hits"], serde_json::json!(42));
        assert_eq!(keyed["misses"], serde_json::json!(0));
    }

    #[test]
    fn session_stats_diff_phase_profile_counters() {
        let mut baseline = compile_stats(10);
        baseline.staged_profile = Some(crate::daemon::protocol::StagedProfileInfo {
            counters: [("published".to_string(), 3)].into(),
            timings_ns: [("publish".to_string(), 100)].into(),
            bytes: [("copied".to_string(), 20)].into(),
            failures: Default::default(),
        });
        let mut current = compile_stats(15);
        current.staged_profile = Some(crate::daemon::protocol::StagedProfileInfo {
            counters: [("published".to_string(), 7), ("salvaged".to_string(), 1)].into(),
            timings_ns: [("publish".to_string(), 160)].into(),
            bytes: [("copied".to_string(), 45)].into(),
            failures: [("copy".to_string(), 1)].into(),
        });

        let stats = compute_session_stats(Some(&baseline), Some(&current)).expect("stats");
        assert_eq!(stats["phase_profile"]["staged"]["counters"]["published"], 4);
        assert_eq!(stats["phase_profile"]["staged"]["counters"]["salvaged"], 1);
        assert_eq!(
            stats["phase_profile"]["staged"]["timings_ns"]["publish"],
            60
        );
        assert_eq!(stats["phase_profile"]["staged"]["bytes"]["copied"], 25);
        assert_eq!(stats["phase_profile"]["staged"]["failures"]["copy"], 1);
    }

    #[test]
    fn clear_session_artifacts_removes_existing_files_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let zccache_dir = tmp.path();
        std::fs::create_dir_all(zccache_dir.join("logs")).expect("logs dir");

        // Only the log file exists; the journal and stats files are absent.
        let log = crate::cache_lib::session_log_path(zccache_dir);
        std::fs::write(&log, b"hello").expect("write log");

        let removed = clear_session_artifacts(zccache_dir).expect("clear");
        assert!(removed, "expected at least one file to be removed");
        assert!(!log.exists(), "log file should be gone");

        // Calling again on an empty state must succeed and report nothing
        // removed (idempotency contract for `session-end --clear`).
        let removed_again = clear_session_artifacts(zccache_dir).expect("clear-twice");
        assert!(!removed_again, "second clear should be a no-op");
    }

    #[test]
    fn output_snippet_omits_empty_output() {
        assert_eq!(output_snippet(b""), None);
        assert_eq!(output_snippet(b" \n\t "), None);
    }

    #[test]
    fn output_snippet_compacts_whitespace() {
        assert_eq!(
            output_snippet(b"  first line\n\nsecond\tline  ").as_deref(),
            Some("first line second line")
        );
    }

    #[test]
    fn output_snippet_truncates_long_output() {
        let output = "x".repeat(ANALYZE_NOTE_LIMIT + 10);
        let snippet = output_snippet(output.as_bytes()).unwrap();
        assert_eq!(snippet.chars().count(), ANALYZE_NOTE_LIMIT + 3);
        assert!(snippet.ends_with("..."));
    }

    #[test]
    fn zccache_analyze_failure_note_includes_stdout_and_stderr() {
        let note = zccache_analyze_failure_note(
            Some(1),
            br#"{"status":"error","error":"bad input"}"#,
            b"expected compile journal JSONL\n",
        );
        assert!(note.contains("rollups: zccache analyze exited with status Some(1)"));
        assert!(note.contains("stderr: expected compile journal JSONL"));
        assert!(note.contains(r#"stdout: {"status":"error","error":"bad input"}"#));
    }
}
