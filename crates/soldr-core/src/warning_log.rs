//! Warnings soldr emitted earlier in a run, so a later failure can repeat them.
//!
//! soldr#1999 rule 1: *a diagnostic emitted before a failure must be attached
//! to that failure*, not left upstream in the log.
//!
//! The failure mode it addresses: soldr warns during a build — the fast linker
//! was unavailable, a restore was partial, a fallback could not be persisted —
//! and then the build fails much later. The warning is still in the log, but
//! hundreds of Cargo progress lines above the error, so the person reading the
//! failure never connects the two and starts debugging the wrong subsystem.
//! soldr#1992 is the worked example: the rust-lld retry notice sat unlinked
//! while the user read a bare `could not compile`.
//!
//! Deliberately a *record-and-replay*, not a buffer. Warnings are still
//! printed the moment they happen — someone watching a live build must not
//! have to wait for a failure to learn something went wrong, and a build that
//! succeeds should still show them. Replay only adds a second copy where it is
//! needed, next to the error.

use soldr_platform::sync::Mutex;
use std::sync::OnceLock;

/// Upper bound on retained warnings.
///
/// A failure summary that repeats two hundred lines is noise, and noise is the
/// same disease as silence — the reader still cannot find the cause. A build
/// that produced more warnings than this has a bigger problem than the tail of
/// the list.
const MAX_RETAINED: usize = 20;

fn store() -> &'static Mutex<Vec<String>> {
    static STORE: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Print a warning and retain it for replay at a later failure.
///
/// Use in place of a bare `eprintln!("soldr warning: …")` anywhere the warning
/// could precede a failure the user has to diagnose.
pub fn warn(message: impl Into<String>) {
    let message = message.into();
    eprintln!("{message}");
    record(message);
}

/// Retain a warning that was printed by other means.
///
/// Poisoning is recovered by `soldr_platform::sync` rather than propagated:
/// panicking here would turn a warning into a crash.
pub fn record(message: impl Into<String>) {
    let mut guard = store().lock();
    if guard.len() >= MAX_RETAINED {
        return;
    }
    guard.push(message.into());
}

/// Everything retained so far, oldest first.
pub fn recorded() -> Vec<String> {
    store().lock().clone()
}

/// Drop every retained warning. Exists for tests, which share one process.
pub fn clear() {
    store().lock().clear();
}

/// The block to append to a failure report, or `None` when nothing was warned.
///
/// Returns `None` rather than an empty string so callers cannot accidentally
/// print a header with nothing under it.
pub fn replay_block() -> Option<String> {
    let warnings = recorded();
    if warnings.is_empty() {
        return None;
    }
    let mut out = String::from(
        "soldr: this build also produced the following warning(s) earlier, \
         which may be the cause:\n",
    );
    for warning in &warnings {
        out.push_str("  ");
        out.push_str(warning.trim());
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes these tests against each other.
    ///
    /// The store is process-wide, and a crate's unit tests share one process,
    /// so without this they would clobber each other's records. Module-local
    /// rather than the shared env barrier: per soldr#1896 a module guarding a
    /// resource nobody else touches keeps its own lock, and this is not an
    /// environment variable.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn fresh() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear();
        guard
    }

    #[test]
    fn nothing_warned_yields_no_block() {
        let _g = fresh();
        assert!(
            replay_block().is_none(),
            "an empty block would print a bare header"
        );
    }

    #[test]
    fn a_recorded_warning_is_replayed() {
        let _g = fresh();
        record("soldr warning: fast linker was unavailable");
        let block = replay_block().expect("a warning was recorded");
        assert!(block.contains("fast linker was unavailable"), "{block}");
        assert!(
            block.contains("may be the cause"),
            "the reader must be told why this is being repeated: {block}"
        );
    }

    #[test]
    fn warnings_replay_oldest_first() {
        let _g = fresh();
        record("soldr warning: first");
        record("soldr warning: second");
        let block = replay_block().expect("recorded");
        let first = block.find("first").expect("first present");
        let second = block.find("second").expect("second present");
        assert!(first < second, "order is the causal hint: {block}");
    }

    // Repeating two hundred lines at the failure is the same disease as
    // silence: the reader still cannot find the cause.
    #[test]
    fn retention_is_bounded() {
        let _g = fresh();
        for i in 0..(MAX_RETAINED * 3) {
            record(format!("soldr warning: {i}"));
        }
        assert_eq!(recorded().len(), MAX_RETAINED);
    }
}
