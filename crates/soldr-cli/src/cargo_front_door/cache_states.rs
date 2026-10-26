//! soldr#2302 — surface compile-cache HIT/MISS in the build stream, plus a
//! one-line automatic cache-stats summary at the build tail.
//!
//! # Two surfaces
//!
//! - **Per-unit lines.** While cargo builds, a background thread tails the
//!   embedded zccache compile journal (`compile_journal.jsonl`) from the byte
//!   offset captured at build start and prints a `soldr[cache] <crate> [HIT!]`
//!   / `[MISS]` line as each compile resolves — or `[NC]` when zccache
//!   refused the compile by design rather than failing to serve it — so you
//!   can watch the cache kick in and see *for what*. This is the achievable
//!   form of "annotate the Compiling/Checking line": cargo prints
//!   `Compiling X` *before* rustc runs, so the outcome is not yet known at
//!   that point — the honest signal is a soldr-owned line emitted once the
//!   compile resolves.
//! - **Stats summary.** At the tail, one line reports hits/misses/hit-rate and
//!   the time the cache saved, read from the session baseline-diff stats (not
//!   the journal), so it is precisely scoped to this build. The stats file is
//!   fixed-name and shared by every invocation, so it is keyed to the session
//!   that wrote it: a file from an earlier build prints
//!   `no stats for this session` instead of the previous build's numbers
//!   (soldr#3540).
//!
//! # Correlation
//!
//! Embedded compiles are journaled in the *ephemeral* shape: the `session_id`
//! field is always `null` and there is no `crate_name` field (the name is
//! derived from `--crate-name` in `args`). The journal is daemon-wide, so the
//! only correlation available is the **byte-offset window** from build start
//! to end, and that window also holds every other build sharing the daemon: a
//! second repository on a dev box, or `soldr ci-test`'s Dylint stages running
//! beside `nextest-compile`. Left unfiltered those foreign lines made the
//! per-unit stream disagree with the build-scoped summary, which read as the
//! summary miscounting (soldr#3143). Each record's rustc `--out-dir` lies under
//! the target directory of the build that ran it, so lines are scoped to this
//! build's target root -- see [`BuildScope`] and [`ScopedLines`] for why a
//! wrong resolution can delay a line but never hide one.
//!
//! # Color on CI (deliberate)
//!
//! Unlike the dim log-paths summary (which used to never colorize under
//! GitHub Actions), the HIT/MISS annotations *do* colorize on CI: a GitHub
//! Actions log renders ANSI, and green/yellow is the whole point of the
//! feature there. That soldr#2302 carve-out became the general rule in
//! soldr#3437 — see [`crate::color_choice`] — which is why every surface now
//! resolves color the same way.
//!
//! Lives in its own file so `cargo_front_door/mod.rs` does not grow further
//! (house style, post-#339).

use std::io::{IsTerminal, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::CargoCachePlan;
use crate::color_choice::{paint, DIM, GREEN, YELLOW};
use crate::core::SoldrPaths;
use crate::daemon::protocol::BuildCacheSummary;

/// `--no-cache-states` / `SOLDR_NO_CACHE_STATES=1` opt-out, following the
/// `SOLDR_NO_*` convention (`SOLDR_NO_LOG_SUMMARY`, `SOLDR_NO_TRAMPOLINE`).
pub(crate) const NO_CACHE_STATES_ENV_VAR: &str = "SOLDR_NO_CACHE_STATES";

/// How often the tail thread re-reads the journal for new records.
const TAIL_POLL: Duration = Duration::from_millis(100);

/// Whether the cache-states surfaces are enabled for this run.
///
/// On unless explicitly disabled. Unlike the elapsed-seconds prefix
/// (`should_timestamp`), a per-unit HIT/MISS line does not fight cargo's
/// progress redraw, so there is no reason to default it off on a terminal — the
/// user asked to *see* the cache work. Being on for both TTY and non-TTY also
/// means the CI log the feature is validated on always carries the signal.
pub(crate) fn enabled() -> bool {
    !super::env_flag_truthy(NO_CACHE_STATES_ENV_VAR)
}

/// The one-line automatic cache-stats summary, or `None` when nothing cacheable
/// resolved this build (so a cache-disabled or empty session prints nothing).
///
/// Pure so it is unit-asserted directly without capturing stderr.
pub(crate) fn cache_stats_message(summary: &BuildCacheSummary, use_color: bool) -> Option<String> {
    let decided = summary.hits + summary.misses;
    if decided == 0 {
        return None;
    }
    let rate = (summary.hits as f64 / decided as f64) * 100.0;
    let hits = paint(&format!("{} HIT", summary.hits), GREEN, use_color);
    let misses = paint(&format!("{} MISS", summary.misses), YELLOW, use_color);
    let saved = summary.time_saved_ms as f64 / 1000.0;
    Some(format!(
        "soldr: cache {hits}, {misses} ({rate:.0}% hit rate, saved {saved:.1}s)"
    ))
}

/// Read the per-build cache summary from the session stats file (the
/// baseline-diff `last-session-stats.json`). Lives here, beside the
/// annotations that consume it, rather than in the already-oversized front
/// door (soldr#2302 / the per-file line ceiling).
///
/// soldr#3540: the file name is fixed and shared by every front-door
/// invocation, and `finalize_build_session_stats` is a no-op when the daemon
/// is unreachable at end of build — so the counters on disk are frequently
/// *some earlier build's*. The session key stamped into the body is
/// therefore checked, and a file from another session is reported as
/// [`SessionStatsRead::Stale`] rather than returned as this build's numbers.
pub(crate) fn read_build_cache_summary(stats_path: &Path, session_id: &str) -> SessionStatsRead {
    let Ok(raw) = std::fs::read_to_string(stats_path) else {
        return SessionStatsRead::Missing;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
        return SessionStatsRead::Missing;
    };
    if json.get("status").and_then(serde_json::Value::as_str) != Some("ok") {
        return SessionStatsRead::Missing;
    }
    // soldr#3540: no key, or another session's key, means these numbers are
    // not this build's — including a body written before the key existed,
    // which is exactly the unkeyed file the bug report describes.
    if json.get("session_id").and_then(serde_json::Value::as_str) != Some(session_id) {
        return SessionStatsRead::Stale;
    }
    let hits = json_u64(&json, "hits").unwrap_or(0);
    let misses = json_u64(&json, "misses").unwrap_or(0);
    let non_cacheable = json_u64(&json, "non_cacheable").unwrap_or(0);
    let errors = json_u64(&json, "errors").unwrap_or(0);
    SessionStatsRead::Fresh(BuildCacheSummary {
        hits,
        misses,
        non_cacheable,
        errors,
        compilations: json_u64(&json, "compilations").unwrap_or(hits + misses),
        time_saved_ms: json_u64(&json, "time_saved_ms").unwrap_or(0),
    })
}

/// What [`read_build_cache_summary`] found in the stats file (soldr#3540).
pub(crate) enum SessionStatsRead {
    /// The file was keyed to the session reading it, so the numbers are
    /// this build's own.
    Fresh(BuildCacheSummary),
    /// No usable stats file: absent, unreadable, or `status != "ok"`.
    Missing,
    /// The file exists but belongs to an earlier session (or carries no
    /// session key at all). Reporting it as this build's is the stale-
    /// summary bug.
    Stale,
}

impl SessionStatsRead {
    /// The summary, only when it provably belongs to the reading session.
    pub(crate) fn fresh(self) -> Option<BuildCacheSummary> {
        match self {
            Self::Fresh(summary) => Some(summary),
            Self::Missing | Self::Stale => None,
        }
    }
}

fn json_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
    value.get(key).and_then(serde_json::Value::as_u64)
}

/// Start the live per-unit tail for a cache-enabled build. Returns `None`
/// (printing nothing) for a `--no-cache` run with no session or when the
/// surface is disabled — so the front door integrates in one line.
pub(crate) fn start_tail(
    cache_plan: &CargoCachePlan,
    paths: &SoldrPaths,
    journal_start_offset: u64,
    args: &[String],
) -> Option<CacheStateTail> {
    cache_plan.zccache_session()?;
    // Stamp with the same elapsed-seconds prefix as the relayed cargo output
    // when that prefix is on, so the two read as one stream in a CI log.
    let stamp_from = super::timestamp_tee::should_timestamp(
        std::env::var(super::timestamp_tee::TIMESTAMP_LINES_ENV_VAR)
            .ok()
            .as_deref(),
        std::io::stderr().is_terminal(),
    )
    .then(Instant::now);
    CacheStateTail::start(
        super::embedded_compile_journal_path(paths),
        journal_start_offset,
        stamp_from,
        BuildScope::resolve(args),
    )
}

/// Stop and join a tail started by [`start_tail`], draining its final records.
pub(crate) fn stop_tail(tail: Option<CacheStateTail>) {
    if let Some(tail) = tail {
        tail.stop_and_join();
    }
}

/// Emit the automatic cache-stats summary for a finished build's session.
pub(crate) fn emit_build_stats(cache_plan: &CargoCachePlan) {
    let Some(session) = cache_plan.zccache_session() else {
        return;
    };
    match read_build_cache_summary(&session.session_stats_path, &session.session_id) {
        SessionStatsRead::Fresh(summary) => emit_cache_stats(Some(&summary)),
        // Nothing was ever written for this cache directory: an empty
        // invocation keeps printing nothing.
        SessionStatsRead::Missing => {}
        // soldr#3540: the finalize at the end of this build was a no-op
        // (daemon unreachable), so the file still holds an earlier build's
        // numbers. Say that instead of repeating them as this build's.
        SessionStatsRead::Stale => emit_stale_stats_notice(),
    }
}

/// Print the soldr#3540 stale-stats notice, unless the surface is disabled.
fn emit_stale_stats_notice() {
    if enabled() {
        eprintln!("{}", stale_stats_message());
    }
}

/// The line printed when the stats file belongs to an earlier session.
///
/// Pure so the stale branch is unit-asserted without capturing stderr.
fn stale_stats_message() -> String {
    "soldr: no stats for this session (cache summary file is from an earlier build)".to_string()
}

/// Print the automatic cache-stats summary to stderr, unless the surface is
/// disabled or nothing cacheable resolved. Called at the build tail beside the
/// log-paths summary.
pub(crate) fn emit_cache_stats(summary: Option<&BuildCacheSummary>) {
    if !enabled() {
        return;
    }
    let Some(summary) = summary else {
        return;
    };
    if let Some(message) = cache_stats_message(summary, crate::color_choice::stderr_enabled()) {
        eprintln!("{message}");
    }
}

/// The hit/miss classification a journal record carries, if it is one we
/// annotate. `error` / `cached_error` records are neither and are skipped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Outcome {
    Hit,
    Miss,
    /// A miss zccache refused to cache by design. zccache journals these as
    /// a plain `miss` (zackees/zccache#1865: PGO inputs and other
    /// passthroughs land there), distinguished only by
    /// `miss_reason: "uncacheable_input"` — counting them as `[MISS]` reads
    /// as a failing cache when it is working exactly as intended.
    NotCacheable,
}

impl Outcome {
    /// Classify a journal record. `miss_reason` is read because zccache
    /// emits only `hit`/`miss`/`error`/`cached_error`/`link_hit`/`link_miss`
    /// as outcomes; the reason is what separates a genuine miss from a
    /// deliberate passthrough. Unknown outcome strings stay `None` (never
    /// rendered as a miss).
    fn parse(outcome: &str, miss_reason: Option<&str>) -> Option<Self> {
        match outcome {
            "hit" | "link_hit" => Some(Self::Hit),
            "miss" | "link_miss" => Some(if miss_reason == Some(MISS_REASON_UNCACHEABLE_INPUT) {
                Self::NotCacheable
            } else {
                Self::Miss
            }),
            _ => None,
        }
    }
}

/// zccache's `miss_reason` bucket for an invocation that is intrinsically
/// uncacheable — `compile_journal::miss_reason::UNCACHEABLE_INPUT` upstream.
/// The one label zccache stamps on the passthroughs soldr#3540 was counting
/// as misses; the closed set of reasons lives there, so this mirrors it.
const MISS_REASON_UNCACHEABLE_INPUT: &str = "uncacheable_input";

/// `FLAG X` / `FLAG=X` out of a rustc argv.
fn rustc_arg_value(args: &[serde_json::Value], flag: &str) -> Option<String> {
    let mut iter = args.iter().filter_map(serde_json::Value::as_str);
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().map(str::to_string);
        }
        if let Some(value) = arg
            .strip_prefix(flag)
            .and_then(|rest| rest.strip_prefix('='))
        {
            return Some(value.to_string());
        }
    }
    None
}

/// One hit/miss compile record from the journal. The journal's ephemeral shape
/// omits the derived `crate_name` field, so the name comes from `--crate-name`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct JournalUnit {
    crate_name: String,
    outcome: Outcome,
    /// rustc's `--out-dir`, which lies under the target directory of the build
    /// that ran this compile.
    out_dir: Option<PathBuf>,
}

/// Render one per-unit annotation line for a resolved compile.
fn render_line(crate_name: &str, outcome: Outcome, use_color: bool) -> String {
    let tag = match outcome {
        Outcome::Hit => paint("[HIT!]", GREEN, use_color),
        Outcome::Miss => paint("[MISS]", YELLOW, use_color),
        Outcome::NotCacheable => paint("[NC]", DIM, use_color),
    };
    format!("soldr[cache] {crate_name} {tag}")
}

/// Parse a chunk of compile-journal JSONL into the hit/miss units that carry a
/// derivable crate name.
///
/// Pure over its input so it is unit-tested with no daemon: malformed lines,
/// non-hit/miss outcomes (`error`, `cached_error`, and any outcome string
/// outside zccache's set), and records with no `--crate-name` (version probes
/// and other uncacheable inputs) are all skipped rather than rendered as
/// `? [MISS]`. A `miss` whose `miss_reason` says the input is intrinsically
/// uncacheable renders `[NC]`, not `[MISS]` (soldr#3540).
fn parse_journal_chunk(chunk: &str) -> Vec<JournalUnit> {
    let mut out = Vec::new();
    for line in chunk.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let outcome = value
            .get("outcome")
            .and_then(serde_json::Value::as_str)
            .and_then(|outcome| {
                Outcome::parse(
                    outcome,
                    value.get("miss_reason").and_then(serde_json::Value::as_str),
                )
            });
        let Some(outcome) = outcome else {
            continue;
        };
        let Some(args) = value.get("args").and_then(serde_json::Value::as_array) else {
            continue;
        };
        let Some(crate_name) = rustc_arg_value(args, "--crate-name") else {
            continue;
        };
        out.push(JournalUnit {
            crate_name,
            outcome,
            out_dir: rustc_arg_value(args, "--out-dir").map(PathBuf::from),
        });
    }
    out
}

/// The target root this build writes into (soldr#3143), from the shared
/// resolver (soldr#3203). Should Cargo still disagree -- a `--config <file>`
/// it does not model, say -- [`ScopedLines`]' fallback keeps every line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BuildScope {
    roots: Vec<PathBuf>,
}

impl BuildScope {
    pub(crate) fn resolve(args: &[String]) -> Self {
        let roots = std::env::current_dir()
            .ok()
            .and_then(|cwd| {
                crate::core::cargo_target_dir::resolve_cargo_target_dir(
                    &crate::core::cargo_target_dir::CargoTargetDirInputs::from_process(&cwd, args),
                )
            })
            .into_iter()
            .collect();
        Self { roots }
    }

    fn contains(&self, out_dir: &Path) -> bool {
        self.roots.iter().any(|root| out_dir.starts_with(root))
    }
}

/// Bound on lines held while the scope is unconfirmed.
const MAX_HELD_LINES: usize = 4096;

/// The per-unit lines of one build, filtered by [`BuildScope`] without ever
/// losing output to a wrong resolution (soldr#3143).
///
/// A unit whose `--out-dir` lies under a scope root is this build's and prints
/// at once. One that does not is *held*, not dropped, until some unit matches:
/// that match proves the scope found this build's real target, and only then
/// are held and later non-matching units discarded as another build's. If
/// nothing ever matches, the scope guessed wrong -- a `target-dir` set
/// somewhere it does not look -- and [`Self::finish`] releases everything held,
/// which is exactly what printed before the filter existed.
struct ScopedLines {
    scope: BuildScope,
    confirmed: bool,
    held: Vec<String>,
}

impl ScopedLines {
    fn new(scope: BuildScope) -> Self {
        Self {
            scope,
            confirmed: false,
            held: Vec::new(),
        }
    }

    /// The line to print now for `unit`, if any.
    fn accept(&mut self, unit: &JournalUnit, use_color: bool) -> Option<String> {
        let line = render_line(&unit.crate_name, unit.outcome, use_color);
        let Some(out_dir) = unit.out_dir.as_deref() else {
            // Cannot be attributed either way: print it, as before.
            return Some(line);
        };
        if self.scope.roots.is_empty() || self.scope.contains(out_dir) {
            if !self.confirmed {
                self.confirmed = true;
                self.held.clear();
            }
            return Some(line);
        }
        if !self.confirmed && self.held.len() < MAX_HELD_LINES {
            self.held.push(line);
        }
        None
    }

    /// Lines still owed once the build ends: the held ones, only if the scope
    /// was never confirmed.
    fn finish(self) -> Vec<String> {
        if self.confirmed {
            Vec::new()
        } else {
            self.held
        }
    }
}

/// A live tail over the compile journal that prints per-unit HIT/MISS lines to
/// stderr as records land, for the lifetime of one cargo run.
///
/// The thread writes whole lines via `eprintln!`; a rare interleave with the
/// relayed cargo stream is cosmetic and does not warrant sharing a lock with
/// the front door's output tee.
pub(crate) struct CacheStateTail {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl CacheStateTail {
    /// Start tailing `journal_path` from `start_offset`. When `stamp_from` is
    /// `Some`, each line is prefixed with elapsed seconds in the same format as
    /// the relayed cargo output (`should_timestamp`), so the two read as one
    /// stream in a CI log.
    ///
    /// Returns `None` — printing nothing — when the surface is disabled, so the
    /// caller can hold an `Option` without branching.
    pub(crate) fn start(
        journal_path: PathBuf,
        start_offset: u64,
        stamp_from: Option<Instant>,
        scope: BuildScope,
    ) -> Option<Self> {
        if !enabled() {
            return None;
        }
        let use_color = crate::color_choice::stderr_enabled();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("soldr-cache-states".into())
            .spawn(move || {
                tail_loop(
                    &journal_path,
                    start_offset,
                    use_color,
                    stamp_from,
                    &stop_thread,
                    ScopedLines::new(scope),
                )
            })
            .ok()?;
        Some(Self {
            stop,
            handle: Some(handle),
        })
    }

    /// Signal the tail to drain any final records and stop, then join it.
    pub(crate) fn stop_and_join(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = crate::platform::wait::join_for(handle, crate::platform::wait::HELPER_JOIN);
        }
    }
}

fn emit(line: &str, stamp_from: Option<Instant>) {
    match stamp_from {
        Some(t0) => eprintln!("{:>8.2} {line}", t0.elapsed().as_nanos() as f64 / 1e9),
        None => eprintln!("{line}"),
    }
}

fn tail_loop(
    journal_path: &Path,
    start_offset: u64,
    use_color: bool,
    stamp_from: Option<Instant>,
    stop: &AtomicBool,
    mut lines: ScopedLines,
) {
    let mut offset = start_offset;
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let stopping = stop.load(Ordering::SeqCst);
        offset = drain_new_records(
            journal_path,
            offset,
            &mut pending,
            use_color,
            stamp_from,
            &mut lines,
        );
        if stopping {
            // Cargo has exited, but the daemon writes journal records
            // asynchronously, so the final compile's line can land a few
            // milliseconds later. Give it one grace poll and drain once more so
            // the last crate is not silently dropped from the per-unit stream.
            std::thread::sleep(TAIL_POLL);
            let _ = drain_new_records(
                journal_path,
                offset,
                &mut pending,
                use_color,
                stamp_from,
                &mut lines,
            );
            for line in lines.finish() {
                emit(&line, stamp_from);
            }
            break;
        }
        std::thread::sleep(TAIL_POLL);
    }
}

/// Read the journal from `offset` to EOF, emit a line for every *complete*
/// (newline-terminated) record, and return the new offset. A trailing partial
/// line is buffered in `pending` until its newline arrives.
fn drain_new_records(
    journal_path: &Path,
    offset: u64,
    pending: &mut Vec<u8>,
    use_color: bool,
    stamp_from: Option<Instant>,
    lines: &mut ScopedLines,
) -> u64 {
    let Ok(mut file) = std::fs::File::open(journal_path) else {
        return offset;
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return offset;
    };
    if len <= offset {
        return offset;
    }
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return offset;
    }
    let mut buf = Vec::with_capacity((len - offset) as usize);
    if file.read_to_end(&mut buf).is_err() {
        return offset;
    }
    let read = buf.len() as u64;
    // Accumulate raw bytes and decode only whole lines: a multi-byte UTF-8
    // char (e.g. in a non-ASCII crate path) can straddle a poll boundary, so
    // converting per-read would corrupt it — converting per complete line
    // keeps both halves together.
    pending.extend_from_slice(&buf);
    if let Some(last_nl) = pending.iter().rposition(|&b| b == b'\n') {
        let complete: Vec<u8> = pending.drain(..=last_nl).collect();
        for unit in parse_journal_chunk(&String::from_utf8_lossy(&complete)) {
            if let Some(line) = lines.accept(&unit, use_color) {
                emit(&line, stamp_from);
            }
        }
    }
    offset + read
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(hits: u64, misses: u64) -> BuildCacheSummary {
        BuildCacheSummary {
            hits,
            misses,
            non_cacheable: 0,
            errors: 0,
            compilations: hits + misses,
            time_saved_ms: 12_300,
        }
    }

    #[test]
    fn stats_plain_lists_hits_misses_rate_saved() {
        let msg = cache_stats_message(&summary(42, 8), false).expect("stats present");
        assert!(msg.contains("42 HIT"), "got {msg:?}");
        assert!(msg.contains("8 MISS"), "got {msg:?}");
        assert!(msg.contains("84% hit"), "got {msg:?}");
        assert!(msg.contains("saved 12.3s"), "got {msg:?}");
        assert!(!msg.contains('\u{1b}'), "plain must carry no ANSI: {msg:?}");
    }

    #[test]
    fn stats_colors_hit_green_miss_yellow() {
        let msg = cache_stats_message(&summary(42, 8), true).expect("stats present");
        assert!(
            msg.contains("\u{1b}[32m42 HIT\u{1b}[0m"),
            "hit must be green: {msg:?}"
        );
        assert!(
            msg.contains("\u{1b}[33m8 MISS\u{1b}[0m"),
            "miss must be yellow: {msg:?}"
        );
    }

    #[test]
    fn stats_none_when_nothing_compiled() {
        assert!(cache_stats_message(&summary(0, 0), false).is_none());
    }

    /// soldr#3540 part 1: `last-session-stats.json` is a fixed name, and a
    /// finalize that no-ops (daemon unreachable at end of build) leaves the
    /// previous invocation's numbers on disk. A file keyed to another
    /// session must read as stale — never as this build's stats.
    #[test]
    fn stats_file_from_an_earlier_session_is_not_reported_as_this_sessions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("last-session-stats.json");
        std::fs::write(
            &path,
            r#"{"status":"ok","session_id":"session-previous","hits":451,"misses":0,"compilations":451,"time_saved_ms":12300}"#,
        )
        .expect("write stats");

        assert!(
            matches!(
                read_build_cache_summary(&path, "session-current"),
                SessionStatsRead::Stale
            ),
            "another session's file must be stale, not this build's stats"
        );
        assert!(
            read_build_cache_summary(&path, "session-current")
                .fresh()
                .is_none(),
            "a stale file must not hand a summary to the journal-tail wait"
        );
        assert!(
            stale_stats_message().contains("no stats for this session"),
            "stale stats must be reported as such: {}",
            stale_stats_message()
        );

        // The very same file is fresh for the session that wrote it.
        assert!(
            matches!(
                read_build_cache_summary(&path, "session-previous"),
                SessionStatsRead::Fresh(summary) if summary.hits == 451 && summary.misses == 0
            ),
            "the writing session's own read must be fresh"
        );
    }

    /// soldr#3540: a body written before the session key existed carries no
    /// attribution at all, so it is equally unreportable as this build's.
    #[test]
    fn an_unkeyed_stats_file_is_stale_not_this_sessions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("last-session-stats.json");
        std::fs::write(
            &path,
            r#"{"status":"ok","hits":451,"misses":0,"compilations":451}"#,
        )
        .expect("write stats");

        assert!(matches!(
            read_build_cache_summary(&path, "session-current"),
            SessionStatsRead::Stale
        ));
    }

    #[test]
    fn an_absent_stats_file_is_missing_not_stale() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("last-session-stats.json");
        assert!(matches!(
            read_build_cache_summary(&path, "session-current"),
            SessionStatsRead::Missing
        ));
        assert!(read_build_cache_summary(&path, "session-current")
            .fresh()
            .is_none());
    }

    /// A journal chunk carrying: a hit with `--crate-name X`, a miss with the
    /// `--crate-name=X` spelling, a `link_hit`, an `error` (skipped), a record
    /// with no crate name (skipped), and a malformed line (skipped).
    const CHUNK: &str = concat!(
        r#"{"outcome":"hit","args":["--crate-name","serde","src/lib.rs"],"session_id":null}"#,
        "\n",
        r#"{"outcome":"miss","args":["--crate-name=soldr_cli","src/lib.rs"],"cwd":"C:\\repo"}"#,
        "\n",
        r#"{"outcome":"link_hit","args":["--crate-name","tokio"]}"#,
        "\n",
        r#"{"outcome":"error","args":["--crate-name","broken"]}"#,
        "\n",
        r#"{"outcome":"miss","args":["--print","cfg"]}"#,
        "\n",
        "this is not json",
        "\n",
    );

    fn render_journal_chunk(chunk: &str, use_color: bool) -> Vec<String> {
        parse_journal_chunk(chunk)
            .iter()
            .map(|unit| render_line(&unit.crate_name, unit.outcome, use_color))
            .collect()
    }

    #[test]
    fn journal_chunk_renders_only_hit_miss_with_a_crate_name() {
        let lines = render_journal_chunk(CHUNK, false);
        assert_eq!(
            lines,
            vec![
                "soldr[cache] serde [HIT!]".to_string(),
                "soldr[cache] soldr_cli [MISS]".to_string(),
                "soldr[cache] tokio [HIT!]".to_string(),
            ],
            "error/no-crate-name/malformed rows must be skipped"
        );
    }

    #[test]
    fn journal_chunk_colorizes_tags() {
        let lines = render_journal_chunk(CHUNK, true);
        assert!(lines[0].contains("\u{1b}[32m[HIT!]\u{1b}[0m"), "{lines:?}");
        assert!(lines[1].contains("\u{1b}[33m[MISS]\u{1b}[0m"), "{lines:?}");
    }

    /// soldr#3540 part 2: zccache journals the compiles it refuses by design
    /// (PGO inputs, ...) as a plain `miss` whose
    /// `miss_reason` is `uncacheable_input` — zackees/zccache#1865. Printing
    /// those yellow `[MISS]` claimed the cache was failing on the 17 units of
    /// a `cargo test --no-run` when it was working as intended.
    #[test]
    fn uncacheable_passthrough_misses_render_as_not_cacheable_not_miss() {
        let chunk = concat!(
            r#"{"outcome":"miss","miss_reason":"uncacheable_input","args":["--crate-name","harness_one"]}"#,
            "\n",
            r#"{"outcome":"link_miss","miss_reason":"uncacheable_input","args":["--crate-name","harness_two"]}"#,
            "\n",
            r#"{"outcome":"miss","miss_reason":"context_not_found","args":["--crate-name","genuine_miss"]}"#,
            "\n",
        );
        let lines = render_journal_chunk(chunk, false);
        assert_eq!(
            lines,
            vec![
                "soldr[cache] harness_one [NC]".to_string(),
                "soldr[cache] harness_two [NC]".to_string(),
                "soldr[cache] genuine_miss [MISS]".to_string(),
            ],
            "a passthrough must render [NC]; only a real miss renders [MISS]"
        );
        assert!(
            !lines[0].contains("[MISS]"),
            "the passthrough line must not carry MISS: {:?}",
            lines[0]
        );
    }

    #[test]
    fn not_cacheable_is_dim_and_miss_stays_yellow() {
        let chunk = concat!(
            r#"{"outcome":"miss","miss_reason":"uncacheable_input","args":["--crate-name","passthrough"]}"#,
            "\n",
            r#"{"outcome":"miss","miss_reason":"input_fingerprint_mismatch","args":["--crate-name","changed"]}"#,
            "\n",
        );
        let lines = render_journal_chunk(chunk, true);
        assert!(
            lines[0].contains("\u{1b}[2m[NC]\u{1b}[0m"),
            "not-cacheable must be dim: {:?}",
            lines[0]
        );
        assert!(
            lines[1].contains("\u{1b}[33m[MISS]\u{1b}[0m"),
            "a real miss must stay yellow: {:?}",
            lines[1]
        );
    }

    /// soldr#3540 part 2: outcome strings outside zccache's hit/miss set are
    /// skipped, never folded into `[MISS]`.
    #[test]
    fn outcomes_outside_the_hit_miss_set_are_never_rendered_as_miss() {
        let chunk = concat!(
            r#"{"outcome":"cached_error","args":["--crate-name","cached_broken"]}"#,
            "\n",
            r#"{"outcome":"error","args":["--crate-name","failed"]}"#,
            "\n",
        );
        let lines = render_journal_chunk(chunk, false);
        assert!(lines.is_empty(), "nothing else may render: {lines:?}");
    }

    #[test]
    fn enabled_defaults_on_and_env_disables() {
        // The env var is process-global; assert both directions without
        // leaving it set for other tests in this binary.
        std::env::remove_var(NO_CACHE_STATES_ENV_VAR);
        assert!(enabled(), "on by default");
        std::env::set_var(NO_CACHE_STATES_ENV_VAR, "1");
        assert!(!enabled(), "disabled by SOLDR_NO_CACHE_STATES=1");
        std::env::remove_var(NO_CACHE_STATES_ENV_VAR);
    }

    #[test]
    fn a_partial_trailing_line_is_held_until_its_newline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("compile_journal.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"outcome":"hit","args":["--crate-name","alpha"]}"#,
                "\n",
                r#"{"outcome":"miss","args":["--crate-name","beta"]}"#, // no trailing \n
            ),
        )
        .expect("write journal");

        let mut pending: Vec<u8> = Vec::new();
        let mut lines = ScopedLines::new(BuildScope::default());
        let offset = drain_new_records(&path, 0, &mut pending, false, None, &mut lines);
        // Only the complete first record is consumed for rendering; the
        // partial `beta` line stays pending, but the offset still advances past
        // all bytes read so the next poll does not re-read them.
        assert_eq!(offset, std::fs::metadata(&path).unwrap().len());
        let held = String::from_utf8_lossy(&pending);
        assert!(held.contains("beta"), "partial line held: {held:?}");
        assert!(!held.contains("alpha"), "complete line drained: {held:?}");
    }

    fn unit(crate_name: &str, out_dir: Option<&str>) -> JournalUnit {
        JournalUnit {
            crate_name: crate_name.to_string(),
            outcome: Outcome::Miss,
            out_dir: out_dir.map(PathBuf::from),
        }
    }

    fn scope(roots: &[&str]) -> BuildScope {
        BuildScope {
            roots: roots.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn a_unit_carries_its_out_dir_in_either_spelling() {
        let units = parse_journal_chunk(concat!(
            r#"{"outcome":"miss","args":["--crate-name","a","--out-dir","/ws/target/debug/deps"]}"#,
            "\n",
            r#"{"outcome":"hit","args":["--crate-name=b","--out-dir=/ws/target/debug/build/b-1/out"]}"#,
            "\n",
        ));
        assert_eq!(
            units.iter().map(|u| u.out_dir.clone()).collect::<Vec<_>>(),
            vec![
                Some(PathBuf::from("/ws/target/debug/deps")),
                Some(PathBuf::from("/ws/target/debug/build/b-1/out")),
            ]
        );
    }

    /// soldr#3143, as measured: a cold two-crate build printed a third `[MISS]`
    /// for `running_process_platform_internal`, compiled by another repository
    /// into its own `target/` on the same daemon, while the summary correctly
    /// said 2 MISS.
    #[test]
    fn another_builds_units_are_dropped_once_this_builds_target_is_confirmed() {
        let mut lines = ScopedLines::new(scope(&["/ws/target"]));
        let foreign = unit(
            "running_process_platform_internal",
            Some("/other/target/debug/deps"),
        );
        assert_eq!(
            lines.accept(&foreign, false),
            None,
            "held before confirmation"
        );
        assert_eq!(
            lines.accept(&unit("a", Some("/ws/target/debug/deps")), false),
            Some("soldr[cache] a [MISS]".to_string())
        );
        assert_eq!(
            lines.accept(&foreign, false),
            None,
            "dropped after confirmation"
        );
        assert!(
            lines.finish().is_empty(),
            "held foreign lines are discarded"
        );
    }

    /// The guarantee that makes the filter safe: a scope that never matches is
    /// a wrong resolution, and every held line is released at the end.
    #[test]
    fn a_scope_that_never_matches_loses_no_lines() {
        let mut lines = ScopedLines::new(scope(&["/ws/target"]));
        assert_eq!(
            lines.accept(&unit("a", Some("/elsewhere/deps")), false),
            None
        );
        assert_eq!(
            lines.accept(&unit("b", Some("/elsewhere/deps")), false),
            None
        );
        assert_eq!(
            lines.finish(),
            vec![
                "soldr[cache] a [MISS]".to_string(),
                "soldr[cache] b [MISS]".to_string()
            ]
        );
    }

    #[test]
    fn an_unscoped_or_unattributable_unit_prints_immediately() {
        let mut unscoped = ScopedLines::new(BuildScope::default());
        assert!(unscoped
            .accept(&unit("a", Some("/anywhere/deps")), false)
            .is_some());
        let mut scoped = ScopedLines::new(scope(&["/ws/target"]));
        assert!(scoped.accept(&unit("b", None), false).is_some());
        assert!(
            !scoped.confirmed,
            "an unattributable unit proves nothing about the scope"
        );
    }
}
