//! `soldr logs` — discoverable logs API tracked in
//! [issue #820](https://github.com/zackees/soldr/issues/820).
//!
//! This module implements `paths`, `list`, and `show`. The follow-up
//! verbs in the issue (`view`, `prune`) ride on the same
//! `Commands::Logs` arm.
//!
//! Goal: 15-minute-grep-for-the-right-journal becomes one command.
//! On a vanilla `~/.soldr/` install with no `SOLDR_CACHE_DIR`
//! override, the directories printed today are the same ones the
//! issue's repro mentioned (`logs/last-session.{log,jsonl,stats.json}`,
//! `daemon-{lifecycle,spawn}-*.log`, trash mailboxes, runtime state).
//! `logs list/show` read the durable history; `logs paths` names the
//! live directories.
//!
//! ## JSON shape (stable contract)
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "root": "/home/user/.soldr",
//!   "paths": [
//!     {
//!       "name": "soldr-root",
//!       "path": "/home/user/.soldr",
//!       "description": "User-level soldr install root...",
//!       "exists": true
//!     },
//!     ...
//!   ]
//! }
//! ```
//!
//! `schema_version` bumps on a breaking change. Field additions are
//! additive (consumer code reading by name is forward-compatible).

use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};
use crate::daemon::db::{Event, EventKind};
use crate::daemon::protocol::{BuildCacheSummary, BuildLogPaths, BuildMissReason, BuildRecord};

#[cfg(test)]
use crate::daemon::db;

const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize, Debug, Clone)]
pub(crate) struct LogPathEntry {
    /// Short stable identifier (snake_case). JSON consumers filter by
    /// this. Human output prefixes the line with `[name]`.
    pub name: String,
    pub path: PathBuf,
    pub description: String,
    pub exists: bool,
}

#[derive(Serialize, Debug)]
pub(crate) struct LogPathsOutput {
    pub schema_version: u32,
    pub root: PathBuf,
    pub paths: Vec<LogPathEntry>,
}

#[derive(Serialize, Debug)]
pub(crate) struct LogsListOutput {
    pub schema_version: u32,
    pub command: &'static str,
    pub root: PathBuf,
    pub db_path: PathBuf,
    pub launches: Vec<LogLaunchSummary>,
    pub notes: Vec<String>,
}

#[derive(Serialize, Debug)]
pub(crate) struct LogsShowOutput {
    pub schema_version: u32,
    pub command: &'static str,
    pub root: PathBuf,
    pub db_path: PathBuf,
    pub launch: LogLaunchSummary,
    pub slow_compiles: Vec<LogCompileEvent>,
    pub events: Vec<LogEvent>,
    pub notes: Vec<String>,
}

#[derive(Serialize, Debug, Clone)]
pub(crate) struct LogLaunchSummary {
    pub id: String,
    pub short_id: String,
    pub repo_root: String,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub duration_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub crate_count: u32,
    pub cache: Option<LogCacheSummary>,
    pub slowest_compile: Option<LogCompileEvent>,
    pub miss_reasons: Vec<LogMissReason>,
    pub logs: Option<LogBuildPaths>,
}

#[derive(Serialize, Debug, Clone)]
pub(crate) struct LogCacheSummary {
    pub hits: u64,
    pub misses: u64,
    pub non_cacheable: u64,
    pub errors: u64,
    pub compilations: u64,
    pub time_saved_ms: u64,
    pub hit_rate: Option<f64>,
}

#[derive(Serialize, Debug, Clone)]
pub(crate) struct LogCompileEvent {
    pub crate_name: Option<String>,
    pub duration_us: Option<u64>,
    pub duration_ms: Option<f64>,
    pub target_dir: Option<String>,
    pub started_at_ms: Option<i64>,
}

#[derive(Serialize, Debug, Clone)]
pub(crate) struct LogMissReason {
    pub reason: String,
    pub count: u64,
}

#[derive(Serialize, Debug, Clone)]
pub(crate) struct LogBuildPaths {
    pub zccache_session_id: Option<String>,
    pub cache_dir: Option<String>,
    pub session_log_path: Option<String>,
    pub journal_path: Option<String>,
    pub session_stats_path: Option<String>,
    pub compile_journal_path: Option<String>,
    pub archived_session_log_path: Option<String>,
    pub archived_journal_path: Option<String>,
    pub archived_session_stats_path: Option<String>,
    pub archived_compile_journal_path: Option<String>,
    pub private_daemon_name: Option<String>,
}

#[derive(Serialize, Debug)]
pub(crate) struct LogEvent {
    pub ts_ms: i64,
    pub kind: &'static str,
    pub crate_name: Option<String>,
    pub duration_us: Option<u64>,
    pub target_dir: Option<String>,
    pub exit_code: Option<i32>,
}

/// Implementation of `soldr logs paths`. Returns the soldr exit code:
/// always `0` on success — the command's job is informational, not
/// diagnostic. JSON mode emits a stable `schema_version: 1` payload.
pub(crate) fn run_logs_paths(json: bool) -> Result<i32, SoldrError> {
    let paths = SoldrPaths::new()?;
    let output = build_log_paths_output(&paths);

    if json {
        emit_json(&output)?;
    } else {
        emit_human(&output);
    }
    Ok(0)
}

pub(crate) fn run_logs_list(limit: u32, json: bool) -> Result<i32, SoldrError> {
    let paths = SoldrPaths::new()?;
    let output = collect_logs_list_output_for_paths(&paths, limit)?;

    if json {
        emit_json(&output)?;
    } else {
        emit_logs_list_human(&output);
    }
    Ok(0)
}

pub(crate) fn run_logs_show(launch_id: &str, json: bool) -> Result<i32, SoldrError> {
    let paths = SoldrPaths::new()?;
    let output = collect_logs_show_output_for_paths(&paths, launch_id)?;

    if json {
        emit_json(&output)?;
    } else {
        emit_logs_show_human(&output);
    }
    Ok(0)
}

/// Pure-function constructor for the [`LogPathsOutput`] used by both
/// the JSON and human emit paths. Lets unit tests drive the shape
/// without doing filesystem I/O for emission.
pub(crate) fn build_log_paths_output(paths: &SoldrPaths) -> LogPathsOutput {
    let entries = collect_log_path_entries(paths);
    LogPathsOutput {
        schema_version: SCHEMA_VERSION,
        root: paths.root.clone(),
        paths: entries,
    }
}

pub(crate) fn collect_logs_list_output_for_paths(
    paths: &SoldrPaths,
    limit: u32,
) -> Result<LogsListOutput, SoldrError> {
    let db_path = crate::cache_lib::data_db_path(paths);
    let records = daemon_list_builds(paths, limit)?;
    Ok(LogsListOutput {
        schema_version: SCHEMA_VERSION,
        command: "logs list",
        root: paths.root.clone(),
        db_path,
        launches: records.iter().map(log_launch_summary).collect(),
        notes: Vec::new(),
    })
}

pub(crate) fn collect_logs_show_output_for_paths(
    paths: &SoldrPaths,
    launch_id: &str,
) -> Result<LogsShowOutput, SoldrError> {
    let db_path = crate::cache_lib::data_db_path(paths);
    // The daemon owns these tables. Prefixes resolve through its list query;
    // the selected session's events come from its exact-session query.
    let (record, events) = daemon_logs_show_inputs(paths, launch_id)?;
    let mut notes = launch_notes(&record);
    let slow_compiles = slow_compile_events(&events, 10);
    if slow_compiles.is_empty() && record.slowest_crate_us.is_none() {
        notes.push(
            "slow_compiles: no per-compile durations were recorded for this launch".to_string(),
        );
    }
    Ok(LogsShowOutput {
        schema_version: SCHEMA_VERSION,
        command: "logs show",
        root: paths.root.clone(),
        db_path,
        launch: log_launch_summary(&record),
        slow_compiles,
        events: events.iter().map(log_event).collect(),
        notes,
    })
}

/// Walk the known list of paths soldr writes runtime logs into
/// and stamp each with an `exists` boolean based on the live
/// filesystem. The fixed list mirrors the issue #820 repro's
/// "non-obvious tour" and the paths `cache_lib` / `daemon` /
/// `gc` actually use.
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn collect_log_path_entries(paths: &SoldrPaths) -> Vec<LogPathEntry> {
    let root = &paths.root;
    let cache = &paths.cache;
    let bin = &paths.bin;
    let zccache = cache.join("zccache");
    let zccache_embedded = crate::zccache_embedded::embedded_cache_root(paths);
    let zccache_embedded_normalized: zccache::core::NormalizedPath =
        zccache_embedded.clone().into();
    let zccache_embedded_versioned =
        zccache::core::config::effective_cache_root_from_top_level(&zccache_embedded_normalized);
    let zccache_embedded_logs = zccache_embedded_versioned.join("logs");
    let zccache_history = zccache.join("history");
    let soldr_daemon_state = crate::cache_lib::soldr_daemon_dir(paths);
    let embedded_zccache_warning_logs = soldr_daemon_state.join("logs");
    let runtime = root.join("runtime");
    let runtime_daemon = runtime.join("soldr-daemon");
    let runtime_self = runtime.join("soldr-self");
    let daemon_spawn_log = root.join("daemon-spawn.log");
    // The broker's directory is derived from the *installed* broker image, not
    // from `paths.root` — under `SOLDR_CACHE_DIR` the two differ. Resolve it
    // for real, and fall back to the conventional location if the broker image
    // cannot be resolved (nothing has spawned one yet), so the inventory names
    // a path either way.
    let broker_dir = crate::broker_identity::ResolvedBrokerEndpoint::resolve()
        .ok()
        .and_then(|endpoint| endpoint.executable_path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| root.join("broker"));
    let cargo_abort_log = paths.cargo_abort_log();
    let compile_daemon_fallback_log = paths
        .root
        .join("logs")
        .join("compile-daemon-fallbacks.jsonl");
    // soldr#1857 — daemon-owned, so it lives beside the daemon's other
    // state rather than under the client-written `<root>/logs/` tree.
    let compile_delivery_log = soldr_daemon_state
        .join("logs")
        .join("compile-delivery.jsonl");

    let entries = [
        (
            "soldr-root",
            root.to_path_buf(),
            "Selected soldr state root. The concrete path is shown above and may come from \
             the default home location or `SOLDR_CACHE_DIR`.",
        ),
        (
            "soldr-bin",
            bin.to_path_buf(),
            "Managed tool install directory. Fetched tools such as crgx live in per-version \
             subdirectories; zccache is embedded in soldr and does not live here.",
        ),
        (
            "zccache-cache-root",
            zccache.clone(),
            "Top-level owner for soldr's embedded zccache data and per-build history.",
        ),
        (
            "zccache-embedded-cache-root",
            zccache_embedded,
            "Stable top-level root passed to the in-process zccache service. Persistent \
             service state is versioned beneath this directory.",
        ),
        (
            "zccache-embedded-logs",
            zccache_embedded_logs.as_path().to_path_buf(),
            "Versioned embedded-zccache log directory. Contains the global \
             `compile_journal.jsonl` used for build miss-reason history.",
        ),
        (
            "zccache-build-history",
            zccache_history,
            "Per-build archived session logs, journals, stats, and compile-journal tails.",
        ),
        (
            "soldr-daemon-state",
            soldr_daemon_state,
            "Live soldr-daemon endpoint, PID, lifecycle JSONL, database, and daemon-owned logs.",
        ),
        (
            "embedded-zccache-warning-logs",
            embedded_zccache_warning_logs,
            "Daily rolling `embedded-zccache.warn.log.YYYY-MM-DD` files emitted by the \
             in-process cache service.",
        ),
        (
            "soldr-cargo-abort-log",
            cargo_abort_log,
            "Durable JSONL record of cargo front-door aborts and timeouts. Includes the \
             build-session id, elapsed time, cleanup counts, and cache-bypass recovery hints.",
        ),
        (
            "soldr-porcelain-logs",
            crate::cargo_front_door::porcelain::log_dir(paths),
            "Full stdout+stderr of `--porcelain` / `SOLDR_PORCELAIN` runs: the machine-readable \
             front-door mode suppresses the child relay and names one of these files as the JSON \
             payload's `log_path`. Swept by the auto-GC pass on the same schedule as the build logs.",
        ),
        (
            "soldr-compile-daemon-fallback-log",
            compile_daemon_fallback_log,
            "Historical JSONL record of pre-cutover compile-daemon cache-bypass \
             fallbacks, including build-session correlation and the terminal \
             startup failure. Nothing appends here any more (soldr#2424).",
        ),
        (
            "soldr-compile-delivery-log",
            compile_delivery_log,
            "Durable JSONL record of compiles the daemon ran but could not hand back to the \
             wrapper — a mid-compile client disconnect, or a reply the connection refused. \
             A row with `exit_code: 0` is soldr#1857's signature: a compile that succeeded \
             and still surfaced as a bare `exit 1` with no diagnostics.",
        ),
        (
            "soldr-daemon-runtime",
            runtime_daemon,
            "Relocated per-build soldr-daemon executable images. Runtime state and logs live \
             under the `soldr-daemon-state` entry instead.",
        ),
        (
            "soldr-daemon-spawn-log",
            daemon_spawn_log,
            "Fallback diagnostics recorded when detached soldr-daemon spawn or readiness fails.",
        ),
        (
            "soldr-broker-spawn-log",
            broker_dir.join("broker-spawn.log"),
            "Stdout and stderr of every detached broker this machine has spawned, appended \
             across runs. The first place to look when a build stalls waiting for a broker.",
        ),
        (
            "soldr-broker-bringup-log",
            broker_dir.join("broker-bringup.jsonl"),
            "Per-phase broker cold-start timings (JSONL). One record per completed bringup \
             phase — securing directories, Tokio runtime, peer policy, instance id, broker \
             state, bind. A broker that stalled during startup is identified by the phase \
             that has no record.",
        ),
        (
            "soldr-self-trampoline-dir",
            runtime_self,
            "Windows-only: relocated copies of `soldr.exe` made when soldr is run from a \
             disposable worktree. The trampoline (issue #1064 era) ensures `RUSTC_WRAPPER` \
             doesn't point at a doomed worktree path.",
        ),
    ];

    entries
        .into_iter()
        .map(|(name, path, description)| {
            let exists = path.exists();
            LogPathEntry {
                name: name.to_string(),
                path,
                description: description.to_string(),
                exists,
            }
        })
        .collect()
}

/// Fetch `soldr logs show` inputs from the daemon (soldr#1814 slice 2e).
///
/// Prefix matching uses the daemon's list operation, followed by its exact
/// build-log operation for the selected session. The CLI never opens the
/// daemon-owned state database itself.
fn daemon_logs_show_inputs(
    paths: &SoldrPaths,
    launch_id: &str,
) -> Result<(BuildRecord, Vec<Event>), SoldrError> {
    let sock = crate::daemon::client::default_sock_path(paths);
    if let Ok(session_id) = launch_id.trim().parse::<u64>() {
        let (events, record) = crate::daemon::client::build_log_inputs(&sock, session_id)
            .map_err(|error| daemon_query_error("logs show", error))?;
        if let Some(record) = record {
            return Ok((*record, events));
        }
    }

    let record = resolve_launch_record(daemon_list_builds(paths, 10_000)?, launch_id)?;
    let (events, daemon_record) = crate::daemon::client::build_log_inputs(&sock, record.session_id)
        .map_err(|error| daemon_query_error("logs show", error))?;
    Ok((
        daemon_record.map(|record| *record).unwrap_or(record),
        events,
    ))
}

fn daemon_list_builds(paths: &SoldrPaths, limit: u32) -> Result<Vec<BuildRecord>, SoldrError> {
    let sock = crate::daemon::client::default_sock_path(paths);
    crate::daemon::client::list_builds(&sock, limit, None)
        .map_err(|error| daemon_query_error("logs list", error))
}

fn daemon_query_error(operation: &str, error: crate::daemon::client::ClientError) -> SoldrError {
    SoldrError::Other(format!(
        "{operation} requires the running soldr-daemon to read daemon-owned build history; \
         start a cache-enabled build or run `soldr daemon start` ({error:?})"
    ))
}

fn resolve_launch_record(
    records: Vec<BuildRecord>,
    launch_id: &str,
) -> Result<BuildRecord, SoldrError> {
    let trimmed = launch_id.trim();
    if trimmed.is_empty() {
        return Err(SoldrError::Other("launch id is empty".to_string()));
    }
    let needle = trimmed.to_ascii_lowercase();
    let mut matches = records
        .into_iter()
        .filter(|record| {
            record.session_id.to_string().starts_with(&needle)
                || format!("{:016x}", record.session_id).starts_with(&needle)
        })
        .collect::<Vec<_>>();
    match matches.len() {
        0 => Err(SoldrError::Other(format!("launch id not found: {trimmed}"))),
        1 => Ok(matches.remove(0)),
        _ => Err(SoldrError::Other(format!(
            "launch id prefix is ambiguous: {trimmed}"
        ))),
    }
}

fn log_launch_summary(record: &BuildRecord) -> LogLaunchSummary {
    LogLaunchSummary {
        id: record.session_id.to_string(),
        short_id: short_launch_id(record.session_id),
        repo_root: record.repo_root.clone(),
        started_at_ms: record.started_at_ms,
        ended_at_ms: record.ended_at_ms,
        duration_ms: record.total_wall_ms,
        exit_code: record.exit_code,
        crate_count: record.crate_count,
        cache: record.cache_summary.as_ref().map(log_cache_summary),
        slowest_compile: record.slowest_crate_us.map(|duration_us| LogCompileEvent {
            crate_name: record.slowest_crate_name.clone(),
            duration_us: Some(duration_us),
            duration_ms: Some(duration_us as f64 / 1000.0),
            target_dir: None,
            started_at_ms: None,
        }),
        miss_reasons: record.miss_reasons.iter().map(log_miss_reason).collect(),
        logs: record.log_paths.as_ref().map(log_build_paths),
    }
}

fn log_cache_summary(summary: &BuildCacheSummary) -> LogCacheSummary {
    let denom = summary.hits + summary.misses;
    LogCacheSummary {
        hits: summary.hits,
        misses: summary.misses,
        non_cacheable: summary.non_cacheable,
        errors: summary.errors,
        compilations: summary.compilations,
        time_saved_ms: summary.time_saved_ms,
        hit_rate: (denom > 0).then_some(summary.hits as f64 / denom as f64),
    }
}

fn log_build_paths(paths: &BuildLogPaths) -> LogBuildPaths {
    LogBuildPaths {
        zccache_session_id: paths.zccache_session_id.clone(),
        cache_dir: paths.cache_dir.clone(),
        session_log_path: paths.session_log_path.clone(),
        journal_path: paths.journal_path.clone(),
        session_stats_path: paths.session_stats_path.clone(),
        compile_journal_path: paths.compile_journal_path.clone(),
        archived_session_log_path: paths.archived_session_log_path.clone(),
        archived_journal_path: paths.archived_journal_path.clone(),
        archived_session_stats_path: paths.archived_session_stats_path.clone(),
        archived_compile_journal_path: paths.archived_compile_journal_path.clone(),
        private_daemon_name: paths.private_daemon_name.clone(),
    }
}

fn log_miss_reason(reason: &BuildMissReason) -> LogMissReason {
    LogMissReason {
        reason: reason.reason.clone(),
        count: reason.count,
    }
}

fn slow_compile_events(events: &[Event], limit: usize) -> Vec<LogCompileEvent> {
    let mut rows = events
        .iter()
        .filter(|event| matches!(&event.kind, EventKind::CompileEnd))
        .filter_map(|event| {
            let duration_us = event.duration_us?;
            Some(LogCompileEvent {
                crate_name: event.crate_name.clone(),
                duration_us: Some(duration_us),
                duration_ms: Some(duration_us as f64 / 1000.0),
                target_dir: event.target_dir.clone(),
                started_at_ms: Some(event.ts_ms),
            })
        })
        .collect::<Vec<_>>();
    rows.sort_by_key(|row| std::cmp::Reverse(row.duration_us.unwrap_or(0)));
    rows.truncate(limit);
    rows
}

fn log_event(event: &Event) -> LogEvent {
    LogEvent {
        ts_ms: event.ts_ms,
        kind: match &event.kind {
            EventKind::SessionStart => "session_start",
            EventKind::SessionEnd => "session_end",
            EventKind::CompileStart => "compile_start",
            EventKind::CompileEnd => "compile_end",
        },
        crate_name: event.crate_name.clone(),
        duration_us: event.duration_us,
        target_dir: event.target_dir.clone(),
        exit_code: event.exit_code,
    }
}

fn launch_notes(record: &BuildRecord) -> Vec<String> {
    let mut notes = Vec::new();
    if record.cache_summary.is_none() {
        notes.push(
            "cache_summary: unavailable for this launch; only newer builds persist hit/miss stats"
                .to_string(),
        );
    }
    if record.log_paths.is_none() {
        notes.push(
            "logs: unavailable for this launch; only newer builds persist log/archive paths"
                .to_string(),
        );
    }
    if record.miss_reasons.is_empty() {
        notes.push(
            "miss_reasons: unavailable; no archived zccache journal or log reason lines were found"
                .to_string(),
        );
    }
    notes
}

fn emit_logs_list_human(output: &LogsListOutput) {
    println!("soldr logs list");
    println!("root: {}", output.root.display());
    if output.launches.is_empty() {
        println!("no launches recorded");
    } else {
        println!(
            "{:<14} {:>12} {:>11} {:>10}  cwd",
            "LAUNCH-ID", "DURATION", "HITS/MISSES", "EXIT"
        );
        for launch in &output.launches {
            let hits = launch
                .cache
                .as_ref()
                .map(|cache| format!("{}/{}", cache.hits, cache.misses))
                .unwrap_or_else(|| "n/a".to_string());
            let exit = launch
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "running".to_string());
            println!(
                "{:<14} {:>12} {:>11} {:>10}  {}",
                launch.short_id,
                launch
                    .duration_ms
                    .map(format_duration_ms)
                    .unwrap_or_else(|| "running".to_string()),
                hits,
                exit,
                launch.repo_root
            );
        }
    }
    for note in &output.notes {
        println!("note: {note}");
    }
    println!("Run `soldr logs list --json` for a machine-readable form.");
}

fn emit_logs_show_human(output: &LogsShowOutput) {
    let launch = &output.launch;
    println!("soldr logs show {}", launch.id);
    println!("short-id: {}", launch.short_id);
    println!("started-ms: {}", launch.started_at_ms);
    if let Some(duration_ms) = launch.duration_ms {
        println!("duration: {}", format_duration_ms(duration_ms));
    }
    println!("cwd: {}", launch.repo_root);
    println!(
        "exit-code: {}",
        launch
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "running".to_string())
    );
    println!("compilations: {}", launch.crate_count);
    if let Some(cache) = &launch.cache {
        println!(
            "cache: hits={} misses={} non-cacheable={} errors={} hit-rate={}",
            cache.hits,
            cache.misses,
            cache.non_cacheable,
            cache.errors,
            cache
                .hit_rate
                .map(|rate| format!("{:.1}%", rate * 100.0))
                .unwrap_or_else(|| "n/a".to_string())
        );
        println!("time-saved: {} ms", cache.time_saved_ms);
    } else {
        println!("cache: n/a");
    }

    if !launch.miss_reasons.is_empty() {
        println!("top miss reasons:");
        for reason in &launch.miss_reasons {
            println!("  {:>6}  {}", reason.count, reason.reason);
        }
    }
    if !output.slow_compiles.is_empty() {
        println!("slowest compiles:");
        for compile in &output.slow_compiles {
            println!(
                "  {:>10}  {}",
                compile
                    .duration_ms
                    .map(|ms| format!("{ms:.1} ms"))
                    .unwrap_or_else(|| "n/a".to_string()),
                compile.crate_name.as_deref().unwrap_or("<unknown>")
            );
        }
    } else if let Some(slowest) = &launch.slowest_compile {
        println!(
            "slowest compile: {} ({})",
            slowest.crate_name.as_deref().unwrap_or("<unknown>"),
            slowest
                .duration_ms
                .map(|ms| format!("{ms:.1} ms"))
                .unwrap_or_else(|| "n/a".to_string())
        );
    }
    if let Some(paths) = &launch.logs {
        println!("logs:");
        print_optional_path("  cache", &paths.cache_dir);
        print_optional_path("  session", &paths.archived_session_log_path);
        print_optional_path("  journal", &paths.archived_journal_path);
        print_optional_path("  compile-journal", &paths.archived_compile_journal_path);
        print_optional_path("  stats", &paths.archived_session_stats_path);
        print_optional_path("  original-session", &paths.session_log_path);
        print_optional_path("  original-journal", &paths.journal_path);
        print_optional_path("  original-compile-journal", &paths.compile_journal_path);
        print_optional_path("  original-stats", &paths.session_stats_path);
    }
    for note in &output.notes {
        println!("note: {note}");
    }
    println!(
        "Run `soldr logs show {} --json` for full event details.",
        launch.id
    );
}

fn print_optional_path(label: &str, path: &Option<String>) {
    if let Some(path) = path {
        println!("{label}: {path}");
    }
}

fn short_launch_id(session_id: u64) -> String {
    format!("{session_id:016x}")[..12].to_string()
}

fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let minutes = ms / 60_000;
        let seconds = (ms % 60_000) as f64 / 1000.0;
        format!("{minutes}m {seconds:.1}s")
    }
}

fn emit_json<T: Serialize>(output: &T) -> Result<(), SoldrError> {
    let s = serde_json::to_string_pretty(output)
        .map_err(|e| SoldrError::Other(format!("serialize logs JSON: {e}")))?;
    println!("{s}");
    Ok(())
}

fn emit_human(output: &LogPathsOutput) {
    println!("soldr logs paths — issue #820 phase 1");
    println!("schema_version: {}", output.schema_version);
    println!("root: {}", output.root.display());
    println!();
    for entry in &output.paths {
        let marker = if entry.exists { "✓" } else { "·" };
        println!("[{marker}] {}", entry.name);
        println!("    path: {}", entry.path.display());
        // Wrap the description at a soft 78 cols by walking spaces.
        for line in wrap_description(&entry.description, 74) {
            println!("    {line}");
        }
        println!();
    }
    println!("legend: ✓ = directory exists today | · = not yet created");
    println!("Run `soldr logs paths --json` for a machine-readable form.");
}

/// Hand-rolled greedy line wrapper so we don't pull in a textwrap dep
/// just for one info command. Splits on whitespace, packs words into
/// lines whose width is `<= width` (the first word always lands even
/// if it overflows alone — better one long line than dropped output).
fn wrap_description(s: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in s.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
            continue;
        }
        if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
#[path = "logs_cmd_tests.rs"]
mod tests;
