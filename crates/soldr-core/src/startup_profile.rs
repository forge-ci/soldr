//! Per-phase startup timing emitted to stderr when
//! `SOLDR_PROFILE_STARTUP=1` is set. The hot user-visible path —
//! `RUSTC_WRAPPER=soldr` invoked by cargo 100+ times per build —
//! pays ~47 ms per invocation post-zccache-1.9.0 (issue #440). To
//! pick which lever (process startup, ensure_dirs, registry upsert,
//! IPC connect, …) actually matters, the wrapper code marks each
//! phase boundary; this module prints the deltas at exit.
//!
//! Zero cost when the env var is unset: `WrapperProfile::new()`
//! returns a `disabled` instance whose `mark()` and `finish()`
//! short-circuit immediately. No allocation, no `Instant::now()` —
//! we don't want the diagnostic itself to skew measurements.
//!
//! The output goes to stderr (where cargo's own progress lines
//! live) and is prefixed with `soldr-profile:` so downstream
//! scrapers can grep for it without false matches against ordinary
//! soldr log lines:
//!
//! ```text
//! soldr-profile: wrapper invocation breakdown (process_pid=12345)
//! soldr-profile:                 main_entry  +    87 µs
//! soldr-profile:           args_collected  +    21 µs
//! soldr-profile:         relocate_checked  +    18 µs
//! soldr-profile:     wrapper_dispatch_in  +     3 µs
//! soldr-profile:    record_target_dir_done  +   412 µs
//! soldr-profile:        before_zccache_exec  +    72 µs
//! soldr-profile:                       total       613 µs
//! ```
//!
//! See issue #440 acceptance criterion #1 — "A measurement of
//! soldr's per-invocation overhead on Linux, broken into: process
//! startup, arg parse, env read, IPC connect, request marshal,
//! response decode, exit."

use std::time::Instant;

/// Env var that enables per-phase timing output. Any non-empty value
/// turns it on. Soldr-internal callers ignore the variable's content
/// (only presence matters); future versions may key off specific
/// values (`per-invocation`, `summary-only`) for selective output.
pub(crate) const SOLDR_PROFILE_STARTUP_ENV_VAR: &str = "SOLDR_PROFILE_STARTUP";

/// Accumulates per-phase wall-clock marks. When the env var is set,
/// the wrapper hot path calls `mark("phase_name")` at each boundary
/// and `finish()` right before the exec'/exit. When unset, every
/// method is a no-op so we don't perturb the measurement of the
/// non-instrumented path.
pub struct WrapperProfile {
    enabled: bool,
    start: Option<Instant>,
    /// Pre-allocated to avoid `Vec::push` reallocs in the hot path
    /// when enabled. 16 phases is comfortably more than any
    /// realistic wrapper invocation marks today.
    phases: Vec<(&'static str, Instant)>,
}

impl Default for WrapperProfile {
    fn default() -> Self {
        Self::new()
    }
}

impl WrapperProfile {
    /// Construct a profile guard. Captures `Instant::now()` only
    /// when the env var is set, so the disabled path adds at most
    /// one cheap branch + one `var_os` syscall.
    pub fn new() -> Self {
        let enabled = std::env::var_os(SOLDR_PROFILE_STARTUP_ENV_VAR)
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        Self {
            enabled,
            start: enabled.then(process_start),
            phases: if enabled {
                Vec::with_capacity(16)
            } else {
                Vec::new()
            },
        }
    }

    /// True iff profiling is on. Lets callers skip expensive
    /// instrumentation-only work (sub-phase splits) entirely.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Record a phase boundary. `name` is the boundary that JUST
    /// completed — e.g. `mark("ensure_dirs_done")` is called right
    /// after `paths.ensure_dirs()` returns. The reported delta in
    /// the rendered output is from the previous mark (or `start`
    /// for the first one).
    ///
    /// No-op when profiling is disabled.
    pub fn mark(&mut self, name: &'static str) {
        if self.enabled {
            self.phases.push((name, Instant::now()));
        }
    }

    /// Render the accumulated marks to stderr as a sequence of
    /// `+delta µs` lines plus a `total` line. Should be called
    /// immediately before the wrapper's terminal `exec()` (or
    /// `std::process::exit`) so the measurement reflects all
    /// in-process work.
    ///
    /// On Unix `exec()` replaces the process so this is the LAST
    /// soldr-side code that runs — calling it after the spawn for
    /// the eventual zccache process is too late.
    pub fn finish(self, last_phase: &'static str) {
        self.finish_labeled("wrapper invocation", last_phase);
    }

    /// Same as [`finish`](Self::finish) but names the scope being
    /// measured. The wrapper hot path is not the only place worth
    /// attributing: `soldr cargo ...` pays a fixed front-door cost
    /// before Cargo is ever spawned, and on a fully warm no-op that
    /// cost — not compilation — dominates the wall clock (#1843).
    /// Reusing this type keeps one output format for both scopes.
    pub fn finish_labeled(self, scope: &str, last_phase: &'static str) {
        if !self.enabled {
            return;
        }
        let Some(start) = self.start else {
            return;
        };

        let mut buf = String::with_capacity(64 * self.phases.len() + 128);
        buf.push_str(&format!(
            "soldr-profile: {scope} breakdown (pid={})\n",
            std::process::id()
        ));

        let mut prev = start;
        for (name, at) in &self.phases {
            let delta = at.duration_since(prev);
            buf.push_str(&format!(
                "soldr-profile: {:>26}  +{:>6} µs\n",
                name,
                delta.as_micros(),
            ));
            prev = *at;
        }

        // The last_phase delta is `now - prev`. We record `now`
        // here so callers don't have to thread a final `mark()`
        // through every exit path.
        let now = Instant::now();
        let last_delta = now.duration_since(prev);
        buf.push_str(&format!(
            "soldr-profile: {:>26}  +{:>6} µs\n",
            last_phase,
            last_delta.as_micros(),
        ));

        let total = now.duration_since(start);
        buf.push_str(&format!(
            "soldr-profile: {:>26}   {:>6} µs\n",
            "total",
            total.as_micros(),
        ));

        // Single `write_all` to avoid interleaving with parallel
        // wrapper invocations from cargo's `--jobs N` worker pool.
        use std::io::Write;
        let _ = std::io::stderr().lock().write_all(buf.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// Per-invocation compile timing (soldr#3800 PR 1). Same gate as
// `WrapperProfile`, same `soldr-profile:` stderr prefix, one line per compile.
// ---------------------------------------------------------------------------

/// Marker rustc's JSON diagnostics carry on the line that announces the
/// `.rmeta` is ready (`--json=artifacts`).
const METADATA_EMIT_MARKER: &[u8] = b"\"emit\":\"metadata\"";

/// True iff `SOLDR_PROFILE_STARTUP` is set non-empty in this process.
pub fn profiling_enabled() -> bool {
    std::env::var_os(SOLDR_PROFILE_STARTUP_ENV_VAR)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// True iff a forwarded `(key, value)` environment (a compile request's env)
/// turns profiling on. The shim forwards its whole environment to the daemon,
/// so this works against an already-running daemon whose own env is unset.
pub fn profiling_enabled_in<'a>(env: impl IntoIterator<Item = &'a (String, String)>) -> bool {
    env.into_iter()
        .any(|(k, v)| k == SOLDR_PROFILE_STARTUP_ENV_VAR && !v.is_empty())
}

/// Wall-clock milliseconds since the Unix epoch, for lining shim lines up
/// with the daemon's.
pub fn unix_ms_now() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Does `bytes` contain rustc's `"emit":"metadata"` artifact notification?
/// Read-only scan; callers forward the bytes untouched.
pub fn contains_metadata_emit(bytes: &[u8]) -> bool {
    let needle = METADATA_EMIT_MARKER;
    let Some((&first, rest)) = needle.split_first() else {
        return true;
    };
    let mut from = 0;
    while let Some(i) = bytes[from..].iter().position(|&b| b == first) {
        let at = from + i;
        if bytes[at + 1..].starts_with(rest) {
            return true;
        }
        from = at + 1;
    }
    false
}

/// `--crate-name <name>` / `--crate-name=<name>` from a rustc argv.
pub fn crate_name_from_args(args: &[String]) -> Option<&str> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--crate-name" {
            return it.next().map(String::as_str);
        }
        if let Some(name) = arg.strip_prefix("--crate-name=") {
            return Some(name);
        }
    }
    None
}

static PROCESS_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Earliest `Instant` this process recorded for profiling. `WrapperProfile::new`
/// seeds it at wrapper entry; a path that never built one falls back to the
/// first call.
pub fn process_start() -> Instant {
    *PROCESS_START.get_or_init(Instant::now)
}

/// Shim-side timings for one wrapper invocation, all milliseconds since
/// [`process_start`] unless named `unix_ms`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompileRelayTimings {
    pub crate_name: String,
    pub req_sent_ms: u128,
    pub first_frame_ms: Option<u128>,
    pub metadata_frame_ms: Option<u128>,
    pub exit_ms: u128,
    pub req_sent_unix_ms: u128,
    pub exit_unix_ms: u128,
}

/// Render the `compile-relay` line (no trailing newline). A missing first or
/// metadata frame prints `-` (no metadata line seen: the metadata moment is
/// then `exit_ms`, which always follows).
pub fn format_compile_relay_line(t: &CompileRelayTimings) -> String {
    let opt = |v: Option<u128>| v.map_or_else(|| "-".to_string(), |v| v.to_string());
    let metadata = t
        .metadata_frame_ms
        .map_or_else(|| "-".to_string(), |v| v.to_string());
    format!(
        "soldr-profile: compile-relay crate={} req_sent_ms={} first_frame_ms={} \
         metadata_frame_ms={} exit_ms={} req_sent_unix_ms={} exit_unix_ms={}",
        if t.crate_name.is_empty() {
            "-"
        } else {
            &t.crate_name
        },
        t.req_sent_ms,
        opt(t.first_frame_ms),
        metadata,
        t.exit_ms,
        t.req_sent_unix_ms,
        t.exit_unix_ms,
    )
}

/// Daemon-side wall-clock marks for one compile (Unix milliseconds).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonCompileTimings {
    pub crate_name: String,
    pub recv_unix_ms: u128,
    pub compile_start_unix_ms: u128,
    pub compile_end_unix_ms: u128,
    pub reply_sent_unix_ms: u128,
}

/// Render the `daemon-compile` line (no trailing newline). zccache takes its
/// compile permit inside `compile`, so the permit wait is part of
/// `compile_end - compile_start`, not visible separately from soldr.
pub fn format_daemon_compile_line(t: &DaemonCompileTimings) -> String {
    format!(
        "soldr-profile: daemon-compile crate={} recv_unix_ms={} compile_start_unix_ms={} \
         compile_end_unix_ms={} reply_sent_unix_ms={}",
        if t.crate_name.is_empty() {
            "-"
        } else {
            &t.crate_name
        },
        t.recv_unix_ms,
        t.compile_start_unix_ms,
        t.compile_end_unix_ms,
        t.reply_sent_unix_ms,
    )
}

/// Print one profile line to stderr with a single locked write.
pub fn emit_line(line: &str) {
    use std::io::Write;
    let mut s = String::with_capacity(line.len() + 1);
    s.push_str(line);
    s.push('\n');
    let _ = std::io::stderr().lock().write_all(s.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Test-wide lock that serializes the env-var mutations below.
    /// Without it, parallel test execution can clobber
    /// `SOLDR_PROFILE_STARTUP` between `EnvGuard::set` and the
    /// next-line `WrapperProfile::new()` read — caught by CI Linux
    /// x64 where `mark_records_phase_when_enabled` flaked with
    /// `phases.len() == 0` instead of `2`. Reproes locally too if
    /// you re-run the suite a few times.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Local RAII guard that sets / restores the profile env var
    /// AND holds `ENV_LOCK` for the lifetime of the guard.
    struct EnvGuard {
        key: &'static str,
        prior: Option<std::ffi::OsString>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let prior = std::env::var_os(key);
            std::env::set_var(key, value);
            Self {
                key,
                prior,
                _guard: guard,
            }
        }

        fn unset(key: &'static str) -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let prior = std::env::var_os(key);
            std::env::remove_var(key);
            Self {
                key,
                prior,
                _guard: guard,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn disabled_when_env_var_unset() {
        let _g = EnvGuard::unset(SOLDR_PROFILE_STARTUP_ENV_VAR);
        let profile = WrapperProfile::new();
        assert!(!profile.is_enabled());
        assert!(profile.start.is_none());
    }

    #[test]
    fn disabled_when_env_var_empty() {
        let _g = EnvGuard::set(SOLDR_PROFILE_STARTUP_ENV_VAR, "");
        let profile = WrapperProfile::new();
        assert!(
            !profile.is_enabled(),
            "empty value should not enable profiling",
        );
    }

    #[test]
    fn enabled_when_env_var_set_to_anything_non_empty() {
        let _g = EnvGuard::set(SOLDR_PROFILE_STARTUP_ENV_VAR, "1");
        let profile = WrapperProfile::new();
        assert!(profile.is_enabled());
        assert!(profile.start.is_some());
    }

    #[test]
    fn mark_records_phase_when_enabled() {
        let _g = EnvGuard::set(SOLDR_PROFILE_STARTUP_ENV_VAR, "1");
        let mut profile = WrapperProfile::new();
        profile.mark("first_phase");
        profile.mark("second_phase");
        assert_eq!(profile.phases.len(), 2);
        assert_eq!(profile.phases[0].0, "first_phase");
        assert_eq!(profile.phases[1].0, "second_phase");
        // Monotonic — second mark must be at or after the first.
        assert!(profile.phases[1].1 >= profile.phases[0].1);
    }

    #[test]
    fn mark_is_no_op_when_disabled() {
        let _g = EnvGuard::unset(SOLDR_PROFILE_STARTUP_ENV_VAR);
        let mut profile = WrapperProfile::new();
        for i in 0..1000 {
            profile.mark(if i % 2 == 0 { "even" } else { "odd" });
        }
        assert!(
            profile.phases.is_empty(),
            "disabled profile must not accumulate marks",
        );
    }

    #[test]
    fn finish_is_no_op_when_disabled() {
        let _g = EnvGuard::unset(SOLDR_PROFILE_STARTUP_ENV_VAR);
        let profile = WrapperProfile::new();
        // No panic, no stderr noise — this is the dominant production
        // path so we lock in that it stays cheap.
        profile.finish("would_have_been_total");
    }

    #[test]
    fn metadata_detector_finds_marker_and_ignores_lookalikes() {
        let hit = br#"{"artifact":"/t/libfoo.rmeta","emit":"metadata"}"#;
        assert!(contains_metadata_emit(hit));
        assert!(contains_metadata_emit(br#"x"x"emit":"metadata""#));
        assert!(!contains_metadata_emit(br#"{"emit":"link"}"#));
        assert!(!contains_metadata_emit(br#""emit":"metada"#));
        assert!(!contains_metadata_emit(b""));
    }

    #[test]
    fn crate_name_handles_both_spellings() {
        let a = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            crate_name_from_args(&a(&["--crate-name", "syn", "-C"])),
            Some("syn")
        );
        assert_eq!(
            crate_name_from_args(&a(&["--crate-name=quote"])),
            Some("quote")
        );
        assert_eq!(crate_name_from_args(&a(&["-C", "x"])), None);
    }

    #[test]
    fn env_forwarding_gate() {
        let on = vec![(SOLDR_PROFILE_STARTUP_ENV_VAR.to_string(), "1".to_string())];
        let empty = vec![(SOLDR_PROFILE_STARTUP_ENV_VAR.to_string(), String::new())];
        assert!(profiling_enabled_in(&on));
        assert!(!profiling_enabled_in(&empty));
        assert!(!profiling_enabled_in(&[]));
    }

    #[test]
    fn relay_line_format() {
        let mut t = CompileRelayTimings {
            crate_name: "syn".into(),
            req_sent_ms: 12,
            first_frame_ms: Some(40),
            metadata_frame_ms: Some(41),
            exit_ms: 42,
            req_sent_unix_ms: 1000,
            exit_unix_ms: 1030,
        };
        assert_eq!(
            format_compile_relay_line(&t),
            "soldr-profile: compile-relay crate=syn req_sent_ms=12 first_frame_ms=40 \
             metadata_frame_ms=41 exit_ms=42 req_sent_unix_ms=1000 exit_unix_ms=1030"
        );
        t.first_frame_ms = None;
        t.metadata_frame_ms = None;
        let line = format_compile_relay_line(&t);
        assert!(line.contains("first_frame_ms=- metadata_frame_ms=- exit_ms=42"));
    }

    #[test]
    fn daemon_line_format() {
        let t = DaemonCompileTimings {
            crate_name: String::new(),
            recv_unix_ms: 1,
            compile_start_unix_ms: 2,
            compile_end_unix_ms: 3,
            reply_sent_unix_ms: 4,
        };
        assert_eq!(
            format_daemon_compile_line(&t),
            "soldr-profile: daemon-compile crate=- recv_unix_ms=1 compile_start_unix_ms=2 \
             compile_end_unix_ms=3 reply_sent_unix_ms=4"
        );
    }
}
