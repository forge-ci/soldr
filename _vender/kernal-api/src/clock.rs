//! Wall-clock time, in the units the C libraries underneath this estate use.
//!
//! `libobs` reads the wall clock through `time()` and compares whole seconds;
//! a process that wants to record "when did this frame/event happen" wants the
//! same value, not a formatting library and not a locale-aware calendar. This
//! facade is therefore deliberately narrow: epoch seconds and epoch
//! milliseconds, saturating at the epoch, with no time zones and no calendar
//! conversion.
//!
//! Calendar conversion (`localtime`, weekday names, DST) is intentionally *not*
//! here yet. It is a much larger surface — the C call is not thread-safe and
//! its behaviour depends on the process locale — and it will arrive as its own
//! capability with its own tests rather than as a silently-assumed part of
//! "tell me the time".
//!
//! The system clock can move backwards (NTP, a VM restore, a user changing it),
//! so these values order events only as well as the host clock does. Callers
//! that need "how long did this take" want a monotonic source, which is a
//! different capability.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch.
///
/// Saturates to `0` for a clock set before the epoch (a container built with a
/// bogus date, a test harness pinning `SOURCE_DATE_EPOCH`), rather than
/// panicking or wrapping into a huge positive number.
#[must_use]
pub fn unix_seconds() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// Milliseconds since the Unix epoch.
///
/// The same saturation rules as [`unix_seconds`]. Use this when whole-second
/// resolution is too coarse to order two events; it is still a wall clock, so
/// it is not a substitute for measuring elapsed time.
#[must_use]
pub fn unix_millis() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sanity window rather than an exact value: this test must not fail on a
    /// machine whose clock is merely a little fast or slow.
    const EARLIEST_PLAUSIBLE: i64 = 1_600_000_000; // 2020-09-13
    const LATEST_PLAUSIBLE: i64 = 4_102_444_800; // 2100-01-01

    #[test]
    fn unix_seconds_is_a_plausible_wall_clock_reading() {
        let now = unix_seconds();
        assert!(
            now >= EARLIEST_PLAUSIBLE,
            "clock reads {now}, earlier than {EARLIEST_PLAUSIBLE}"
        );
        assert!(
            now <= LATEST_PLAUSIBLE,
            "clock reads {now}, later than {LATEST_PLAUSIBLE}"
        );
    }

    #[test]
    fn unix_seconds_never_decreases_across_calls() {
        let first = unix_seconds();
        let second = unix_seconds();
        // The wall clock can step backwards, so allow one second of tolerance
        // rather than asserting monotonicity the host does not guarantee.
        assert!(
            second >= first - 1,
            "clock moved backwards by more than a second: {first} -> {second}"
        );
    }

    #[test]
    fn unix_millis_agrees_with_unix_seconds() {
        let seconds = unix_seconds();
        let millis = unix_millis();
        let derived = i64::try_from(millis / 1000).expect("millis within i64 seconds");
        assert!(
            (derived - seconds).abs() <= 2,
            "millis {millis} implies {derived}s but seconds() said {seconds}s"
        );
    }
}
