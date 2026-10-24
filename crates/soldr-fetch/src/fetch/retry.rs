//! Shared retry-with-exponential-backoff for network fetches (soldr#2132).
//!
//! This crate already retried transient network errors in three places
//! (`fetch::mod`, `fetch::llvm`, `fetch::zig`), each with its own copy of the
//! same loop and its own `ATTEMPTS` / `INITIAL_BACKOFF` pair set to the same
//! values. Seven other fetchers had none. That gap failed two lanes of the
//! v0.8.30 release build:
//!
//! ```text
//! soldr build: managed cmake unavailable for host aarch64-unknown-linux-gnu:
//!   network error: error decoding response body
//! → error[E0463]: can't find crate for `core` / `std`
//! ```
//!
//! One truncated response body, and the build failed with an error naming the
//! wrong thing entirely.
//!
//! # What is and is not retried
//!
//! [`is_transient`] deliberately matches the predicate the existing loops
//! already used: [`SoldrError::Network`] and [`SoldrError::ToolNotFound`]
//! (a release asset that is not published *yet*). Everything else is fatal on
//! the first try -- including [`SoldrError::ReleaseNotFound`], GitHub's 404 for
//! a release that does not exist (soldr#3911) — in particular a sha256 mismatch is
//! [`SoldrError::Other`]/[`SoldrError::Archive`] and must stay fatal. Retrying
//! an integrity failure would turn a hard stop into three more chances to
//! accept a bad artifact, which is worse than the flake this module exists to
//! absorb.

use std::future::Future;
use std::time::Duration;

use crate::core::SoldrError;

/// Total attempts, including the first. Matches the value the three
/// pre-existing loops independently converged on.
pub(crate) const FETCH_ATTEMPTS: u32 = 4;

/// Delay before the second attempt; doubles thereafter (5s, 10s, 20s).
pub(crate) const FETCH_INITIAL_BACKOFF: Duration = Duration::from_secs(5);

/// Ceiling on the *total* time spent retrying control-plane requests, backoff
/// included. Archive data-plane requests use [`with_asset_backoff`] instead:
/// their per-attempt stream watchdog bounds a stall, while their transfer time
/// must not consume eligibility for a later retry.
///
/// Attempts alone do not bound wall-clock, because an attempt can itself be a
/// long timeout. `manifest_lookup`'s catalogue fetch uses a 30s
/// `MANIFEST_FETCH_TIMEOUT`, and a timeout maps to `SoldrError::Network`,
/// which [`is_transient`] retries -- so 4 attempts plus 5s/10s/20s backoff is
/// up to **155 seconds** for one call that used to fail once at 30s.
///
/// That is not theoretical: it pushed `cli_build_fetch_overlap` tests past the
/// 120s nextest budget on CI, where they are terminated and reported as TIMEOUT
/// and look like a hang rather than a slow fetch.
///
/// With this budget the worst case is one in-flight attempt plus the budget,
/// and a fetch that is merely slow still gets its retries.
pub(crate) const FETCH_TOTAL_BUDGET: Duration = Duration::from_secs(45);

// A single attempt would make the backoff dead code and silently turn this
// module into a no-op wrapper.
const _: () = assert!(FETCH_ATTEMPTS >= 2);

/// Whether `err` is worth another attempt.
///
/// `Network` covers connect/read/decode failures and non-success HTTP. Any
/// integrity or layout error is excluded on purpose — see the module docs.
pub(crate) fn is_transient(err: &SoldrError) -> bool {
    matches!(err, SoldrError::ToolNotFound(_) | SoldrError::Network(_))
}

/// Run `operation`, retrying transient failures with exponential backoff.
///
/// `what` names the thing being fetched and appears in the retry log, so an
/// operator reading CI output can tell *which* fetch is struggling rather than
/// just that something is.
///
/// The last error is returned unchanged once attempts are exhausted, so the
/// caller's own context (and the original message) survive.
pub(crate) async fn with_backoff<T, F, Fut>(what: &str, operation: F) -> Result<T, SoldrError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, SoldrError>>,
{
    with_backoff_params(
        what,
        FETCH_ATTEMPTS,
        FETCH_INITIAL_BACKOFF,
        FETCH_TOTAL_BUDGET,
        operation,
    )
    .await
}

/// Retry an archive-sized data-plane fetch without a response-wide wall-clock
/// budget. Attempts and cumulative exponential-backoff sleep remain bounded;
/// each individual body read is bounded by the stream downloader's idle
/// watchdog. This means a late truncation after a healthy long transfer still
/// receives the configured retries.
pub(crate) async fn with_asset_backoff<T, F, Fut>(what: &str, operation: F) -> Result<T, SoldrError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, SoldrError>>,
{
    with_asset_backoff_params(what, FETCH_ATTEMPTS, FETCH_INITIAL_BACKOFF, operation).await
}

/// [`with_backoff`] with the schedule spelled out. Separate so callers with
/// their own tuning (and tests) can drive it without sleeping for real
/// seconds -- including `total_budget`, so a test can prove the deadline
/// without waiting out the production one.
pub(crate) async fn with_backoff_params<T, F, Fut>(
    what: &str,
    attempts: u32,
    initial_backoff: Duration,
    total_budget: Duration,
    operation: F,
) -> Result<T, SoldrError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, SoldrError>>,
{
    retry_with_backoff(
        what,
        attempts,
        initial_backoff,
        Some(total_budget),
        operation,
    )
    .await
}

/// Parameterized data-plane retry helper. Kept separate from
/// [`with_backoff_params`] so timing tests can prove that a long, progressing
/// transfer which fails transiently still gets another attempt.
pub(crate) async fn with_asset_backoff_params<T, F, Fut>(
    what: &str,
    attempts: u32,
    initial_backoff: Duration,
    operation: F,
) -> Result<T, SoldrError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, SoldrError>>,
{
    retry_with_backoff(what, attempts, initial_backoff, None, operation).await
}

async fn retry_with_backoff<T, F, Fut>(
    what: &str,
    attempts: u32,
    initial_backoff: Duration,
    total_budget: Option<Duration>,
    mut operation: F,
) -> Result<T, SoldrError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, SoldrError>>,
{
    let mut backoff = initial_backoff;
    let mut attempt: u32 = 1;
    let started = std::time::Instant::now();
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(err)
                if attempt < attempts
                    && is_transient(&err)
                    // Checked *before* sleeping, so the backoff itself cannot
                    // push past the budget and then start a fresh attempt.
                    && total_budget.is_none_or(|budget| {
                        started.elapsed().saturating_add(backoff) < budget
                    }) =>
            {
                eprintln!(
                    "soldr: transient error fetching {what} (attempt {attempt}/{attempts}): \
                     {err}; retrying in {backoff:?}"
                );
                if !backoff.is_zero() {
                    tokio::time::sleep(backoff).await;
                }
                backoff = backoff.saturating_mul(2);
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const NO_SLEEP: Duration = Duration::ZERO;
    /// Large enough that the deadline never fires in tests about attempts.
    const AMPLE_BUDGET: Duration = Duration::from_secs(3600);
    /// Small enough that the deadline test finishes in milliseconds.
    const TINY_BUDGET: Duration = Duration::from_millis(200);

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime")
            .block_on(future)
    }

    #[test]
    fn transient_failures_are_retried_until_one_succeeds() {
        let calls = Cell::new(0u32);
        let result: Result<&str, SoldrError> = block_on(with_backoff_params(
            "test-asset",
            4,
            NO_SLEEP,
            AMPLE_BUDGET,
            || {
                calls.set(calls.get() + 1);
                async {
                    if calls.get() < 3 {
                        Err(SoldrError::Network("error decoding response body".into()))
                    } else {
                        Ok("bundle")
                    }
                }
            },
        ));
        assert_eq!(
            result.expect("should succeed on the third attempt"),
            "bundle"
        );
        assert_eq!(calls.get(), 3, "should stop as soon as an attempt succeeds");
    }

    #[test]
    fn a_checksum_mismatch_is_never_retried() {
        // The case that must stay fatal: retrying an integrity failure would
        // turn one hard stop into several chances to accept a bad artifact.
        let calls = Cell::new(0u32);
        let result: Result<(), SoldrError> = block_on(with_backoff_params(
            "test-asset",
            4,
            NO_SLEEP,
            AMPLE_BUDGET,
            || {
                calls.set(calls.get() + 1);
                async {
                    Err(SoldrError::Other(
                        "sha256 mismatch: expected a, got b".into(),
                    ))
                }
            },
        ));
        assert!(result.is_err());
        assert_eq!(calls.get(), 1, "a non-transient error must not be retried");
    }

    /// soldr#3911: GitHub's 404 for a release lookup is permanent; retrying
    /// it spent 36 s before reporting a tool that does not exist.
    #[test]
    fn a_missing_release_is_not_retried() {
        let calls = Cell::new(0u32);
        let result: Result<(), SoldrError> = block_on(with_backoff_params(
            "test-release",
            4,
            NO_SLEEP,
            AMPLE_BUDGET,
            || {
                calls.set(calls.get() + 1);
                async {
                    Err(SoldrError::ReleaseNotFound(
                        "release lookup failed: HTTP 404 Not Found".into(),
                    ))
                }
            },
        ));
        assert!(matches!(result, Err(SoldrError::ReleaseNotFound(_))));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn attempts_are_bounded_and_the_last_error_survives() {
        let calls = Cell::new(0u32);
        let result: Result<(), SoldrError> = block_on(with_backoff_params(
            "test-asset",
            4,
            NO_SLEEP,
            AMPLE_BUDGET,
            || {
                calls.set(calls.get() + 1);
                // Capture the count by value: `async move` would otherwise move
                // the `Cell` itself into the coroutine, and the assertion below
                // still needs it.
                let attempt = calls.get();
                async move { Err(SoldrError::Network(format!("attempt {attempt}"))) }
            },
        ));
        assert_eq!(calls.get(), 4, "should stop at the attempt budget");
        // The caller sees the *last* failure, not a synthesized wrapper, so the
        // original cause is still in the log.
        match result {
            Err(SoldrError::Network(message)) => assert_eq!(message, "attempt 4"),
            other => panic!("expected the final Network error, got {other:?}"),
        }
    }

    #[test]
    fn a_slow_attempt_cannot_blow_the_total_budget() {
        // The regression this budget exists for: an attempt that is itself a
        // long timeout. With 4 attempts and no deadline, a 30s
        // `MANIFEST_FETCH_TIMEOUT` becomes 4x30s + 35s of backoff = 155s for
        // one call, which is past the 120s nextest budget.
        //
        // Simulated with a short "timeout" and a backoff scaled to match, so
        // the test itself stays fast: each attempt burns most of the budget,
        // so only the first retry can fit.
        let calls = Cell::new(0u32);
        let started = std::time::Instant::now();
        let result: Result<(), SoldrError> = block_on(with_backoff_params(
            "slow-asset",
            FETCH_ATTEMPTS,
            Duration::from_millis(1),
            TINY_BUDGET,
            || {
                calls.set(calls.get() + 1);
                async {
                    // Each attempt consumes a large slice of FETCH_TOTAL_BUDGET.
                    tokio::time::sleep(TINY_BUDGET / 2).await;
                    Err(SoldrError::Network("timed out".into()))
                }
            },
        ));
        assert!(result.is_err());
        assert!(
            calls.get() < FETCH_ATTEMPTS,
            "the budget must cut retries short of the attempt count; got {} attempts",
            calls.get()
        );
        assert!(
            started.elapsed() < TINY_BUDGET * 4,
            "total time must stay near the budget, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn late_asset_failure_is_retried_after_progress() {
        // This attempt is longer than the control-plane budget used above. An
        // asset transfer with a late truncation must still get a fresh attempt:
        // the per-attempt stream idle watchdog prevents an infinite hang.
        let calls = Cell::new(0u32);
        let result: Result<&str, SoldrError> = block_on(with_asset_backoff_params(
            "large asset",
            2,
            NO_SLEEP,
            || {
                calls.set(calls.get() + 1);
                async {
                    if calls.get() == 1 {
                        tokio::time::sleep(TINY_BUDGET + Duration::from_millis(20)).await;
                        Err(SoldrError::Network("truncated after progress".into()))
                    } else {
                        Ok("recovered")
                    }
                }
            },
        ));
        assert_eq!(result.expect("second attempt must run"), "recovered");
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn transient_predicate_covers_network_and_not_found_only() {
        assert!(is_transient(&SoldrError::Network("timeout".into())));
        assert!(is_transient(&SoldrError::ToolNotFound(
            "not published yet".into()
        )));
        assert!(!is_transient(&SoldrError::Other("sha256 mismatch".into())));
        assert!(!is_transient(&SoldrError::Archive("tar.zst unpack".into())));
        assert!(!is_transient(&SoldrError::UnsupportedPlatform(
            "wasm".into()
        )));
    }
}
