//! Diagnostics for a dispatched compile that failed unhelpfully.
//!
//! Extracted from `compile_dispatch.rs` for soldr#1969: that file is already
//! well past the 1,500-line ceiling, and the ratchet added in soldr#1966
//! correctly refused to let it grow further. This is the module the ratchet
//! asked for -- a new file rather than a rename of a hot one, so no in-flight
//! branch gets a modify/delete conflict.

use std::io::Write;

/// Wraps the caller's stderr sink and records whether the dispatched
/// compile wrote anything to it.
///
/// A rustc failure never surfaces as a [`DispatchError`] — it arrives as
/// a non-zero compile exit and is propagated as a bare exit code
/// (see [`client_error_indicates_daemon_unavailable`]). When the daemon
/// also relays no stderr, Cargo prints `error: could not compile <crate>`
/// with an empty cause and the user has nothing to act on.
///
/// That is the shape of soldr#1857, where ~2.8% of dispatched compiles on
/// Windows fail this way while the same workload through
/// `soldr --no-cache` is clean. Detecting "failed **and** said nothing" is
/// what separates that fault from an ordinary compile error, so it is
/// worth the one counter.
pub(crate) struct SilenceDetectingWriter<W> {
    inner: W,
    bytes_written: u64,
    /// soldr#1969: did the failure output name a path inside soldr's own
    /// cache?
    saw_cache_path: bool,
    /// soldr#2188: longest cache-owned path seen in the failure output. A
    /// path over `MAX_PATH` is a different fault with a different remedy.
    longest_cache_path: usize,
    /// Trailing bytes of the previous chunk, so a marker split across a
    /// stream boundary is still found. Streamed stderr arrives in arbitrary
    /// slices, so scanning each chunk in isolation would miss the exact case
    /// this exists to catch.
    tail: Vec<u8>,
}

/// The path segment that identifies soldr's own compile-cache storage.
///
/// Nothing in a user's build legitimately references this, so seeing it in a
/// *failure* message is strong evidence the cache is implicated rather than
/// the code being compiled.
const CACHE_OWNED_PATH_MARKER: &[u8] = b"daemon-state";

/// Windows' legacy `MAX_PATH`. A cache-owned path at least this long in a
/// *failure* message is the soldr#2188 shape rather than the soldr#1969 one.
const LEGACY_MAX_PATH: usize = 260;

/// Fixed structure the embedded store puts between `SoldrPaths::cache` and a
/// staged artifact's filename, measured from a real failing build
/// (soldr#2188):
///
/// ```text
/// zccache\daemon-state\embedded-v1\v1.13.0\staging\<session>\.compile-<pid>-<n>\
/// ```
///
/// `<session>` is ~27 characters and `.compile-<pid>-<n>` ~18, so this is
/// dominated by parts no user controls.
///
/// The number carries a few characters of slack over that measurement on
/// purpose. Erring high warns slightly early; erring low lets the failure
/// through, which is the outcome this exists to prevent.
const STAGING_PREFIX_BUDGET: usize = 110;

/// Room a rustc-generated artifact name needs, e.g.
/// `build_script_build-52378a44826b4cb2.exe` (39). Rounded up rather than
/// fitted, because the point is to warn before the tightest case, not the
/// average one.
const STAGED_ARTIFACT_NAME_BUDGET: usize = 60;

/// Warn when `cache_root` leaves no room for a staged artifact path
/// (soldr#2188).
///
/// The reactive diagnostic added in soldr#2190 explains an `LNK1104` after it
/// happens, which is a real improvement over a bare linker error. This is the
/// half that costs nothing: the arithmetic is knowable before the first crate
/// compiles, and a build that is going to fail this way fails every time, so
/// there is no reason to spend the build first.
///
/// Returns the message rather than printing it, so the caller decides the
/// channel and tests do not have to capture stderr.
pub(crate) fn maxpath_headroom_warning(cache_root: &std::path::Path) -> Option<String> {
    // The MAX_PATH ceiling is a Windows fact; hosts without one skip the
    // projection entirely.
    let limit = crate::platform::host::facts::max_path()?;
    let root_len = cache_root.as_os_str().len();
    let projected = root_len + STAGING_PREFIX_BUDGET + STAGED_ARTIFACT_NAME_BUDGET;
    if projected <= limit {
        return None;
    }
    Some(format!(
        "soldr warning: soldr's cache directory path is {root_len} characters, which \
         leaves no room for staged compile paths under Windows' {limit}-character MAX_PATH \
         limit (projected {projected}). Linking can fail with LNK1104 naming a file \
         inside soldr's cache -- see soldr#2188. Point SOLDR_CACHE_DIR at a shorter \
         root, or run `ZCCACHE_DISABLE=1 soldr cargo ...`."
    ))
}

/// How much trailing output to carry between chunks. Large enough that a
/// path around [`LEGACY_MAX_PATH`] can still be measured when the stream
/// splits it, with room for the surrounding quotes and prefix.
const MAX_MEASURED_PATH: usize = 1024;

/// Length of the whitespace-delimited token containing the marker at
/// `offset`.
///
/// Linkers quote paths inconsistently, so the token is bounded by whitespace
/// and by the quote characters that show up around `LNK1104` operands, and
/// the surrounding message text is not counted.
fn cache_path_token_len(window: &[u8], offset: usize) -> usize {
    let is_boundary = |b: u8| b.is_ascii_whitespace() || b == b'\'' || b == b'"';
    let start = window[..offset]
        .iter()
        .rposition(|&b| is_boundary(b))
        .map_or(0, |i| i + 1);
    let end = window[offset..]
        .iter()
        .position(|&b| is_boundary(b))
        .map_or(window.len(), |i| offset + i);
    end.saturating_sub(start)
}

impl<W: Write> SilenceDetectingWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self {
            inner,
            bytes_written: 0,
            saw_cache_path: false,
            longest_cache_path: 0,
            tail: Vec::new(),
        }
    }

    /// Record whether `chunk` (joined to the previous chunk's tail) names a
    /// cache-owned path, and how long the longest such path is.
    ///
    /// The length matters because it selects the remedy: see
    /// [`Self::longest_cache_path`].
    fn scan_for_cache_path(&mut self, chunk: &[u8]) {
        let mut window = std::mem::take(&mut self.tail);
        window.extend_from_slice(chunk);
        for (offset, _) in window
            .windows(CACHE_OWNED_PATH_MARKER.len())
            .enumerate()
            .filter(|(_, w)| *w == CACHE_OWNED_PATH_MARKER)
        {
            self.saw_cache_path = true;
            self.longest_cache_path = self
                .longest_cache_path
                .max(cache_path_token_len(&window, offset));
        }
        // Keep enough to bridge the next boundary *and* to measure a path
        // that straddles it. A MAX_PATH-length token is the thing being
        // measured, so the bridge has to be longer than one.
        let keep = MAX_MEASURED_PATH;
        if window.len() > keep {
            window.drain(..window.len() - keep);
        }
        self.tail = window;
    }

    /// Emit a diagnostic when the compile failed without explaining why.
    ///
    /// Deliberately hedged: a non-zero exit with no output is *usually*
    /// the #1857 fault, but claiming it always is would be wrong, so the
    /// message states the observation first and the likely cause second.
    pub(crate) fn report_if_silently_failed(&mut self, exit_code: i32) {
        if exit_code == 0 {
            return;
        }
        // soldr#2024: either the compile relayed output through this writer
        // or this function is about to speak for it. Both count, so the
        // top-level exit guard must not add a third voice.
        crate::exit_guard::mark_spoke();
        // soldr#1969: the failure *did* explain itself, but the file it names
        // belongs to soldr. A linker error citing a path nobody recognises
        // reads as a broken toolchain or broken code -- attributing it cost a
        // full diagnostic pass in the report. Say whose file it is.
        // soldr#1999 rule 1: a warning emitted earlier in this build belongs
        // *at* the failure, not hundreds of Cargo progress lines above it.
        // Printed a second time rather than only here, because someone
        // watching live must not have to wait for a failure to learn
        // something went wrong.
        if let Some(block) = soldr_core::warning_log::replay_block() {
            let _ = write!(self.inner, "{block}");
        }
        if self.bytes_written > 0 {
            if self.saw_cache_path {
                // soldr#2188: an over-MAX_PATH cache path is a *different*
                // fault, and the #1969 advice is actively wrong for it --
                // retrying a path that is too long fails identically every
                // time. Separate the two so the remedy matches the cause.
                if self.longest_cache_path >= LEGACY_MAX_PATH {
                    let _ = writeln!(
                        self.inner,
                        concat!(
                            "soldr: that error names a file inside soldr's own compile cache, ",
                            "not your project, and the path is over Windows' {limit}-character ",
                            "MAX_PATH limit ({len} characters).\n",
                            "soldr: the linker cannot open it at any length of retry -- see ",
                            "soldr#2188.\n",
                            "soldr: point SOLDR_CACHE_DIR at a shorter root (the default ",
                            "~/.soldr works because it is short), or run ",
                            "`ZCCACHE_DISABLE=1 soldr cargo ...` to bypass the cache entirely."
                        ),
                        limit = LEGACY_MAX_PATH,
                        len = self.longest_cache_path,
                    );
                } else {
                    let _ = writeln!(
                        self.inner,
                        concat!(
                            "soldr: that error names a file inside soldr's own compile cache, ",
                            "not your project.
",
                            "soldr: an intermediate can be reclaimed while a slow link is still ",
                            "reading it -- see soldr#1969.
",
                            "soldr: retrying usually succeeds; `ZCCACHE_DISABLE=1 soldr cargo ...` ",
                            "bypasses the cache entirely."
                        )
                    );
                }
                let _ = self.inner.flush();
            }
            return;
        }
        let _ = writeln!(
            self.inner,
            "soldr: rustc exited {exit_code} without emitting any diagnostics.\n\
             soldr: the compile was dispatched to soldr-daemon and failed before it \
             could report a reason — see soldr#1857.\n\
             soldr: retrying usually succeeds; `ZCCACHE_DISABLE=1 soldr cargo ...` \
             bypasses the daemon entirely."
        );
        let _ = self.inner.flush();
    }
}

impl<W: Write> Write for SilenceDetectingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.bytes_written += written as u64;
        self.scan_for_cache_path(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests that drive `soldr_core::warning_log`.
    ///
    /// That log is process-global, and both tests below `clear()` it, so run
    /// concurrently one wipes the entry the other just recorded and asserts
    /// on. The resulting flake predates soldr#2188 but is load-dependent, and
    /// the tests added there made it fire -- it is a real race in the tests,
    /// not in the code, so fix it rather than tolerate a rarer version of it.
    fn warning_log_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        // A panicking test must not wedge every later one.
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // soldr#1857 — a dispatched compile that fails *and* says nothing is
    // the fault signature worth calling out. These three cases pin the
    // boundary: only failure-with-silence gets the extra diagnostic.
    #[test]
    fn silent_failure_gets_an_explanatory_diagnostic() {
        let mut sink: Vec<u8> = Vec::new();
        let mut writer = SilenceDetectingWriter::new(&mut sink);
        writer.report_if_silently_failed(1);

        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            text.contains("without emitting any diagnostics"),
            "expected the silence to be named; got:
{text}"
        );
        assert!(
            text.contains("soldr#1857"),
            "expected a pointer to the tracking issue; got:
{text}"
        );
        assert!(
            text.contains("ZCCACHE_DISABLE=1"),
            "expected the documented bypass; got:
{text}"
        );
        // soldr#2424: `--no-cache` is deprecated and hidden, so a reader
        // cannot find it in `soldr --help`. Advice names the supported
        // kill-switch only.
        assert!(
            !text.contains("--no-cache"),
            "advice must not name the deprecated flag; got:
{text}"
        );
    }

    // soldr#1999 rule 1: a warning emitted earlier in the build must appear
    // *at* the failure. Left upstream it sits hundreds of Cargo progress lines
    // above the error and nobody connects the two -- #1992's rust-lld retry
    // notice is the worked example.
    #[test]
    fn an_earlier_warning_is_repeated_at_the_failure() {
        let _guard = warning_log_lock();
        soldr_core::warning_log::clear();
        soldr_core::warning_log::record("soldr warning: fast linker was unavailable");
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = SilenceDetectingWriter::new(&mut sink);
            let _ = writer.write_all(b"error[E0308]: mismatched types");
            writer.report_if_silently_failed(1);
        }
        soldr_core::warning_log::clear();
        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            text.contains("fast linker was unavailable"),
            "the earlier warning must be repeated at the failure, got: {text}"
        );
    }

    // A successful build must not be decorated with a warning replay -- the
    // warning was already printed once, when it happened.
    #[test]
    fn a_successful_build_does_not_replay_warnings() {
        let _guard = warning_log_lock();
        soldr_core::warning_log::clear();
        soldr_core::warning_log::record("soldr warning: something minor");
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = SilenceDetectingWriter::new(&mut sink);
            let _ = writer.write_all(b"   Compiling foo v0.1.0");
            writer.report_if_silently_failed(0);
        }
        soldr_core::warning_log::clear();
        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            !text.contains("something minor"),
            "exit 0 must stay quiet, got: {text}"
        );
    }

    // soldr#1969 — a linker error naming a file inside soldr's cache reads as
    // a broken toolchain. These pin that soldr says whose file it is.
    #[test]
    fn a_failure_naming_a_cache_path_is_attributed_to_soldr() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = SilenceDetectingWriter::new(&mut sink);
            let _ = writer.write_all(
                br"LNK1181: cannot open input file 'C:\c\zccache\daemon-state1\staging.natvis'",
            );
            writer.report_if_silently_failed(1);
        }
        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            text.contains("soldr's own compile cache"),
            "must attribute the file to soldr, got: {text}"
        );
        assert!(text.contains("1969"), "must cite the issue, got: {text}");
    }

    // The marker arrives split across two writes -- the exact case the rolling
    // window exists for, and the one a per-chunk scan would miss.
    #[test]
    fn a_cache_path_split_across_chunks_is_still_found() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = SilenceDetectingWriter::new(&mut sink);
            let _ = writer.write_all(br"LNK1181: cannot open 'C:\c\daemon");
            let _ = writer.write_all(br"-state1\staging.natvis'");
            writer.report_if_silently_failed(1);
        }
        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            text.contains("soldr's own compile cache"),
            "a marker split across writes must still be detected, got: {text}"
        );
    }

    /// A realistic soldr#2188 line: the real failure quotes a staging path
    /// built from a deep `SOLDR_CACHE_DIR`.
    fn lnk1104_over_max_path() -> Vec<u8> {
        let deep_root = format!(r"C:\{}", vec!["nested"; 24].join(r"\"));
        format!(
            concat!(
                r"LINK : fatal error LNK1104: cannot open file ",
                r"'{root}\cache\zccache\daemon-state\embedded-v1\v1.13.0\staging",
                r"\13352-0-1785588800636122100\.compile-13352-1",
                r"\build_script_build-52378a44826b4cb2.exe'"
            ),
            root = deep_root,
        )
        .into_bytes()
    }

    // soldr#2188: an over-MAX_PATH cache path is a different fault from the
    // soldr#1969 reclaim race, and "retrying usually succeeds" is wrong for
    // it -- the linker fails identically every time. These two pin that the
    // remedy follows the cause rather than the marker alone.
    #[test]
    fn an_over_max_path_cache_failure_names_the_length_not_a_retry() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = SilenceDetectingWriter::new(&mut sink);
            let payload = lnk1104_over_max_path();
            let _ = writer.write_all(&payload);
            writer.report_if_silently_failed(1);
        }
        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            text.contains("MAX_PATH"),
            "must name the length limit, got: {text}"
        );
        assert!(text.contains("2188"), "must cite the issue, got: {text}");
        assert!(
            text.contains("SOLDR_CACHE_DIR"),
            "must name the knob that fixes it, got: {text}"
        );
        assert!(
            !text.contains("retrying usually succeeds"),
            "must not advise a retry that cannot work, got: {text}"
        );
    }

    // The other side of the boundary: a short cache path keeps the #1969
    // reclaim-race advice, which is correct there.
    #[test]
    fn a_short_cache_path_failure_keeps_the_retry_advice() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = SilenceDetectingWriter::new(&mut sink);
            let _ = writer.write_all(
                br"LNK1181: cannot open input file 'C:\c\zccache\daemon-state1\staging.natvis'",
            );
            writer.report_if_silently_failed(1);
        }
        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            text.contains("retrying usually succeeds"),
            "a short cache path is the reclaim race, got: {text}"
        );
        assert!(
            !text.contains("MAX_PATH"),
            "must not blame path length for a short path, got: {text}"
        );
    }

    // The long path arrives split across writes, as streamed stderr does.
    // Measuring it requires bridging more than the marker itself.
    #[test]
    fn an_over_max_path_split_across_chunks_is_still_measured() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = SilenceDetectingWriter::new(&mut sink);
            let payload = lnk1104_over_max_path();
            let split = payload.len() / 2;
            let _ = writer.write_all(&payload[..split]);
            let _ = writer.write_all(&payload[split..]);
            writer.report_if_silently_failed(1);
        }
        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            text.contains("MAX_PATH"),
            "a long path split across writes must still be measured, got: {text}"
        );
    }

    // An ordinary compile error must not be blamed on the cache.
    #[test]
    fn an_ordinary_failure_is_not_attributed_to_the_cache() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = SilenceDetectingWriter::new(&mut sink);
            let _ = writer.write_all(b"error[E0308]: mismatched types");
            writer.report_if_silently_failed(1);
        }
        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            !text.contains("soldr's own compile cache"),
            "a normal compile error must not be blamed on soldr, got: {text}"
        );
    }

    // Success is never annotated, even if the output mentions the cache.
    #[test]
    fn a_successful_compile_is_never_attributed() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = SilenceDetectingWriter::new(&mut sink);
            let _ = writer.write_all(b"note: daemon-state path mentioned");
            writer.report_if_silently_failed(0);
        }
        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            !text.contains("1969"),
            "exit 0 must stay silent, got: {text}"
        );
    }

    #[test]
    fn failure_that_produced_output_is_left_alone() {
        let mut sink: Vec<u8> = Vec::new();
        let mut writer = SilenceDetectingWriter::new(&mut sink);
        // Whatever rustc already said about the failure.
        writer
            .write_all(
                b"error[E0308]: mismatched types
",
            )
            .expect("write");
        writer.report_if_silently_failed(1);

        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            !text.contains("soldr#1857"),
            "an ordinary compile error must not be blamed on #1857; got:
{text}"
        );
        assert_eq!(
            text,
            "error[E0308]: mismatched types
"
        );
    }

    #[test]
    fn successful_silent_compile_is_left_alone() {
        let mut sink: Vec<u8> = Vec::new();
        let mut writer = SilenceDetectingWriter::new(&mut sink);
        // The overwhelmingly common case: a cache hit says nothing and succeeds.
        writer.report_if_silently_failed(0);

        assert!(sink.is_empty(), "exit 0 must stay silent");
    }

    #[test]
    fn byte_count_tracks_partial_writes() {
        let mut sink: Vec<u8> = Vec::new();
        let mut writer = SilenceDetectingWriter::new(&mut sink);
        writer.write_all(b"abc").expect("write");
        writer.write_all(b"de").expect("write");

        assert_eq!(writer.bytes_written, 5);
    }

    #[test]
    fn maxpath_headroom_warns_only_when_the_root_leaves_no_room() {
        // soldr#2188 was found at ~160 characters and reproduced cleanly:
        // ~40 and ~99 character roots build fine, ~160 fails at link.
        // Forward slashes deliberately: the check measures length, not shape,
        // and this keeps the test free of escape noise on every platform.
        let short = std::path::PathBuf::from("C:/Users/me/.soldr");
        assert!(
            maxpath_headroom_warning(&short).is_none(),
            "the default-length root must not warn"
        );

        let deep = std::path::PathBuf::from(format!("C:/{}", "d".repeat(150)));
        let warning = maxpath_headroom_warning(&deep);
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            let warning = warning.expect("a 150-char root must warn");
            assert!(warning.contains("MAX_PATH"), "{warning}");
            assert!(
                warning.contains("2188"),
                "must point at the issue: {warning}"
            );
            assert!(
                warning.contains("SOLDR_CACHE_DIR") && warning.contains("ZCCACHE_DISABLE=1"),
                "must give both remedies: {warning}"
            );
        } else {
            // The limit is Windows-only; warning elsewhere would be noise.
            assert!(warning.is_none(), "must not warn off Windows");
        }
    }
}
