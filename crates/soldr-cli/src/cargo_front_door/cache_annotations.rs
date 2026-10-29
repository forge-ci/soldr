//! Where soldr's own per-unit cache annotations go.
//!
//! soldr#2302 prints a `soldr[cache] <crate> [HIT!/MISS]` line to stderr as
//! each compile resolves. That is the right surface for a human and for a CI
//! log. A `--porcelain` run (soldr#3924) asked for exactly one JSON line on
//! stdout and nothing else, so its annotations are *deferred* into a bounded
//! buffer and appended to the durable run log afterwards
//! ([`super::porcelain::append_deferred_annotations`]) -- moved, never
//! suppressed, which is the rule soldr#3929 sets for anything taken off the
//! agent's stream. Interactive and CI defaults keep [`CacheLineSink::Stderr`].
//!
//! Lives in its own file so `cache_states.rs` stays under the 1000-line
//! production ceiling (house style, post-#339).

use std::sync::Arc;
use std::time::Instant;

use soldr_platform::sync::Mutex;

use super::cache_states::CacheStateTail;

/// Where a per-unit annotation line is written.
///
/// stderr is the surface an interactive run and a CI log see, and it stays the
/// default. A `--porcelain` run asked for exactly one JSON line on stdout and
/// nothing else, so its annotations are *deferred*: buffered here and appended
/// to the durable porcelain run log afterwards
/// ([`super::porcelain::append_deferred_annotations`]). They are moved, never
/// suppressed -- the log is the artifact that carries them, which is the rule
/// soldr#3929 sets for anything taken off the agent's stream.
#[derive(Clone)]
pub(crate) enum CacheLineSink {
    Stderr,
    Deferred(DeferredCacheLines),
}

impl CacheLineSink {
    /// The sink a run in this capture mode uses.
    pub(crate) fn for_capture(porcelain: bool) -> Self {
        if porcelain {
            Self::Deferred(DeferredCacheLines::default())
        } else {
            Self::Stderr
        }
    }

    /// The buffer behind a deferred sink, for the caller that will flush it.
    pub(crate) fn deferred(&self) -> Option<DeferredCacheLines> {
        match self {
            Self::Stderr => None,
            Self::Deferred(buffer) => Some(buffer.clone()),
        }
    }

    fn write(&self, line: String) {
        match self {
            Self::Stderr => eprintln!("{line}"),
            Self::Deferred(buffer) => buffer.push(line),
        }
    }
}

/// Bound on the annotations one run holds in memory before it starts counting
/// omissions. A cold workspace build emits one line per compiled unit, so this
/// is far above any realistic build; it exists so a pathological journal cannot
/// make soldr grow without bound. The compile journal keeps the complete
/// record, and the log section states how many lines were left out.
const MAX_DEFERRED_LINES: usize = 5_000;

/// The annotations a porcelain run kept off stderr, in journal order.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DeferredCacheDrain {
    pub(crate) lines: Vec<String>,
    pub(crate) omitted: usize,
}

/// The shared buffer a deferred sink writes into.
#[derive(Clone, Default)]
pub(crate) struct DeferredCacheLines {
    inner: Arc<Mutex<DeferredBuffer>>,
}

#[derive(Default)]
struct DeferredBuffer {
    lines: Vec<String>,
    omitted: usize,
}

impl DeferredCacheLines {
    fn push(&self, line: String) {
        let mut buffer = self.inner.lock();
        if buffer.lines.len() < MAX_DEFERRED_LINES {
            buffer.lines.push(line);
        } else {
            buffer.omitted += 1;
        }
    }

    /// Take everything buffered so far. Called once, after the tail is joined.
    pub(crate) fn drain(&self) -> DeferredCacheDrain {
        let mut buffer = self.inner.lock();
        DeferredCacheDrain {
            lines: std::mem::take(&mut buffer.lines),
            omitted: std::mem::take(&mut buffer.omitted),
        }
    }
}

/// Render one annotation line -- with the elapsed-seconds prefix the relayed
/// cargo output uses, when that prefix is on -- and hand it to `sink`.
pub(super) fn emit(line: &str, stamp_from: Option<Instant>, sink: &CacheLineSink) {
    let rendered = match stamp_from {
        Some(t0) => format!("{:>8.2} {line}", t0.elapsed().as_nanos() as f64 / 1e9),
        None => line.to_string(),
    };
    sink.write(rendered);
}

/// A started tail plus the buffer its lines were deferred into, if any.
///
/// Kept beside the sink rather than in `cache_states.rs`, which has a
/// 1000-line production ceiling.
pub(crate) struct CacheStateTailStart {
    pub(crate) tail: Option<CacheStateTail>,
    pub(crate) deferred: Option<DeferredCacheLines>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default surfaces are unchanged: a human or CI run keeps the live
    /// stderr stream, which is the whole point of soldr#2302.
    #[test]
    fn a_default_run_keeps_the_stderr_sink() {
        assert!(CacheLineSink::for_capture(false).deferred().is_none());
        assert!(CacheLineSink::for_capture(true).deferred().is_some());
    }

    /// The porcelain half of the contract, end to end through the real tail:
    /// the same journal that renders annotations on the stderr sink lands all
    /// of them in the deferred buffer instead, in journal order.
    #[test]
    fn a_porcelain_run_defers_its_annotations_instead_of_printing_them() {
        use super::super::cache_states::{BuildScope, CacheStateTail};

        let dir = tempfile::tempdir().expect("tempdir");
        let journal = dir.path().join("compile_journal.jsonl");
        std::fs::write(
            &journal,
            concat!(
                r#"{"outcome":"hit","args":["--crate-name","serde"]}"#,
                "\n",
                r#"{"outcome":"miss","args":["--crate-name","soldr_cli"]}"#,
                "\n",
            ),
        )
        .expect("write journal");
        let sink = CacheLineSink::for_capture(true);
        let buffer = sink.deferred().expect("porcelain defers");
        let tail = CacheStateTail::start(journal, 0, None, BuildScope::default(), sink)
            .expect("tail started");
        tail.stop_and_join();
        let drained = buffer.drain();
        // Colour is on by design in CI (soldr#3437), so assert the text with
        // the escape sequences removed rather than pinning a colour decision.
        let plain: Vec<String> = drained
            .lines
            .iter()
            .map(|line| super::super::porcelain::without_ansi(line).into_owned())
            .collect();
        assert_eq!(
            plain,
            vec![
                "soldr[cache] serde [HIT!]".to_string(),
                "soldr[cache] soldr_cli [MISS]".to_string(),
            ]
        );
        assert_eq!(drained.omitted, 0);
    }

    /// The buffer is bounded, and what it leaves out is counted rather than
    /// silently forgotten, so the log section can say so.
    #[test]
    fn the_deferred_buffer_is_bounded_and_counts_its_omissions() {
        let buffer = DeferredCacheLines::default();
        for index in 0..MAX_DEFERRED_LINES + 3 {
            buffer.push(format!("soldr[cache] crate{index} [MISS]"));
        }
        let drained = buffer.drain();
        assert_eq!(drained.lines.len(), MAX_DEFERRED_LINES);
        assert_eq!(drained.omitted, 3);
        assert_eq!(
            buffer.drain(),
            DeferredCacheDrain::default(),
            "draining twice must not replay lines"
        );
    }
}
