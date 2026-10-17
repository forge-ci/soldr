//! Shared metadata helpers for the always-on per-build XML log
//! (issue #1790).
//!
//! The build-log writer names each log file after a sanitized slug of
//! the invoking working directory plus a compact UTC timestamp, and
//! embeds timing data for any tool/artifact fetches that happened
//! during the build. This module owns the three small, dependency-free
//! primitives that both the writer and the fetch subsystem need:
//!
//! * [`sanitize_cwd_slug`] — turn an arbitrary filesystem path into a
//!   filename-safe slug.
//! * [`utc_compact_timestamp`] — render a unix-ms timestamp as a
//!   compact `YYYYMMDDTHHMMSSZ` string without pulling in a date/time
//!   crate.
//! * [`fetch_timing`] — a process-global collector the fetch subsystem
//!   records into and the build-log writer drains when it flushes a
//!   log entry.
//!
//! Deliberately std-only: no new dependencies are introduced for this
//! module.

use std::path::Path;

/// Turn a filesystem path into a lowercase, filename-safe slug.
///
/// Rules:
/// * lossy-display the path, then lowercase it,
/// * map every byte/char not in `[a-z0-9]` to `-`,
/// * collapse consecutive `-` runs into a single `-`,
/// * trim leading/trailing `-`,
/// * cap the result at 80 characters (never leaving a trailing `-`
///   after truncation),
/// * fall back to `"root"` when the result would otherwise be empty.
///
/// ```
/// use std::path::Path;
/// use soldr_core::build_log_meta::sanitize_cwd_slug;
///
/// assert_eq!(
///     sanitize_cwd_slug(Path::new(r"C:\Users\niteris\dev\soldr2")),
///     "c-users-niteris-dev-soldr2"
/// );
/// ```
pub fn sanitize_cwd_slug(path: &Path) -> String {
    const MAX_LEN: usize = 80;

    let lossy = path.to_string_lossy().to_lowercase();

    // Map every non [a-z0-9] char to '-', collapsing runs as we go.
    let mut collapsed = String::with_capacity(lossy.len());
    let mut last_was_dash = false;
    for ch in lossy.chars() {
        let mapped = if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            ch
        } else {
            '-'
        };
        if mapped == '-' {
            if !last_was_dash {
                collapsed.push('-');
            }
            last_was_dash = true;
        } else {
            collapsed.push(mapped);
            last_was_dash = false;
        }
    }

    let trimmed = collapsed.trim_matches('-');

    if trimmed.is_empty() {
        return "root".to_string();
    }

    let mut capped: String = trimmed.chars().take(MAX_LEN).collect();
    while capped.ends_with('-') {
        capped.pop();
    }

    if capped.is_empty() {
        "root".to_string()
    } else {
        capped
    }
}

/// Number of whole days between the civil epoch (1970-01-01) and
/// `unix_ms`, plus the remaining milliseconds within that UTC day.
/// Negative `unix_ms` is clamped to 0 — the build log never needs to
/// represent pre-epoch times, and clamping keeps the calendar math
/// (which assumes non-negative days) simple and panic-free.
fn split_days_and_ms_of_day(unix_ms: i64) -> (i64, i64) {
    let unix_ms = unix_ms.max(0);
    let ms_per_day: i64 = 24 * 60 * 60 * 1000;
    let days = unix_ms / ms_per_day;
    let ms_of_day = unix_ms % ms_per_day;
    (days, ms_of_day)
}

/// Convert a day count since the Unix epoch (1970-01-01) into a
/// `(year, month, day)` civil date, using Howard Hinnant's
/// `civil_from_days` algorithm (public domain,
/// <http://howardhinnant.github.io/date_algorithms.html>). Works for
/// any non-negative proleptic-Gregorian day count without floating
/// point or a date/time dependency.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468; // shift epoch from 1970-01-01 to 0000-03-01
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Render a unix-millisecond timestamp as a compact UTC timestamp,
/// `YYYYMMDDTHHMMSSZ` (e.g. `20260723T141502Z`). Negative input is
/// clamped to the epoch. Implemented with hand-rolled calendar math
/// (no chrono/time dependency) per the #1790 std-only constraint.
///
/// ```
/// use soldr_core::build_log_meta::utc_compact_timestamp;
///
/// assert_eq!(utc_compact_timestamp(0), "19700101T000000Z");
/// ```
pub fn utc_compact_timestamp(unix_ms: i64) -> String {
    let (days, ms_of_day) = split_days_and_ms_of_day(unix_ms);
    let (year, month, day) = civil_from_days(days);

    let secs_of_day = ms_of_day / 1000;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// Process-global collector for fetch-timing entries.
///
/// The fetch subsystem calls [`record`] as each tool/artifact fetch
/// completes; the build-log writer calls [`drain`] once per build to
/// pull everything recorded so far (and empty the buffer for the next
/// build) before serializing the log entry.
pub mod fetch_timing {
    use soldr_platform::sync::Mutex;
    use std::sync::OnceLock;

    /// One recorded fetch, timed by the caller.
    #[derive(Debug, Clone)]
    pub struct FetchTiming {
        /// Tool/artifact name, e.g. `"cargo-nextest"`.
        pub name: String,
        /// Where it came from, e.g. `"github-release"`, `"crates-io"`,
        /// `"catalogue"`.
        pub source: String,
        /// Unix milliseconds when the fetch began.
        pub started_at_ms: i64,
        /// Wall-clock duration of the fetch, in milliseconds.
        pub duration_ms: u64,
    }

    fn buffer() -> &'static Mutex<Vec<FetchTiming>> {
        static BUFFER: OnceLock<Mutex<Vec<FetchTiming>>> = OnceLock::new();
        BUFFER.get_or_init(|| Mutex::new(Vec::new()))
    }

    /// Record a completed fetch. Never panics: `soldr_platform::sync`
    /// recovers a poisoned lock, and the `Vec` underneath is append-only so
    /// always safe to reuse.
    pub fn record(timing: FetchTiming) {
        let mut guard = buffer().lock();
        guard.push(timing);
    }

    /// Return every timing recorded since the last `drain` (or process
    /// start), in recording order, and empty the buffer.
    pub fn drain() -> Vec<FetchTiming> {
        let mut guard = buffer().lock();
        std::mem::take(&mut *guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_of_windows_path_with_drive_letter() {
        let slug = sanitize_cwd_slug(Path::new(r"C:\Users\niteris\dev\soldr2"));
        assert_eq!(slug, "c-users-niteris-dev-soldr2");
    }

    #[test]
    fn slug_handles_unicode_and_spaces() {
        let slug = sanitize_cwd_slug(Path::new(r"/home/user/My Café Projëct"));
        assert_eq!(slug, "home-user-my-caf-proj-ct");
    }

    #[test]
    fn slug_collapses_and_trims_dashes() {
        let slug = sanitize_cwd_slug(Path::new("///weird---path!!!name///"));
        assert_eq!(slug, "weird-path-name");
    }

    #[test]
    fn slug_of_empty_or_symbol_only_path_is_root() {
        assert_eq!(sanitize_cwd_slug(Path::new("")), "root");
        assert_eq!(sanitize_cwd_slug(Path::new("///!!!///")), "root");
    }

    #[test]
    fn slug_is_capped_at_80_chars_without_trailing_dash() {
        // 100 'a' characters separated so collapsing doesn't shrink it,
        // then capped to 80 with no trailing dash.
        let long = "a".repeat(100);
        let slug = sanitize_cwd_slug(Path::new(&long));
        assert_eq!(slug.chars().count(), 80);
        assert!(!slug.ends_with('-'));
        assert_eq!(slug, "a".repeat(80));
    }

    #[test]
    fn slug_cap_does_not_leave_trailing_dash_mid_word_boundary() {
        // Construct a path whose 80th character would land exactly on
        // a '-' after collapsing, to make sure the cap trims it off.
        let mut raw = "a".repeat(79);
        raw.push('!'); // becomes '-' at position 80 (1-indexed)
        raw.push_str("bbbb");
        let slug = sanitize_cwd_slug(Path::new(&raw));
        assert!(!slug.ends_with('-'));
        assert!(slug.len() <= 80);
    }

    #[test]
    fn timestamp_at_unix_epoch() {
        assert_eq!(utc_compact_timestamp(0), "19700101T000000Z");
    }

    #[test]
    fn timestamp_at_known_modern_value() {
        // 2026-07-23T14:15:02Z. Rather than hand-deriving the day
        // count (error-prone), this test computes it via an
        // independent civil-date-to-days reference (`days_from_civil`
        // below, Howard Hinnant's published inverse of
        // `civil_from_days`) so it doesn't simply call back into the
        // implementation it's checking.
        let days: i64 = days_from_civil(2026, 7, 23);
        let ms_of_day: i64 = ((14 * 60 + 15) * 60 + 2) * 1000;
        let unix_ms = days * 24 * 60 * 60 * 1000 + ms_of_day;
        assert_eq!(utc_compact_timestamp(unix_ms), "20260723T141502Z");
    }

    #[test]
    fn timestamp_clamps_negative_input_to_epoch() {
        assert_eq!(utc_compact_timestamp(-1_000_000), "19700101T000000Z");
    }

    /// Independent day-count reference (Howard Hinnant's
    /// `days_from_civil`), used only by the test above so the test
    /// doesn't simply call back into the implementation it's checking.
    fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = (y - era * 400) as u64; // [0, 399]
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as u64 + 2) / 5 + d as u64 - 1; // [0, 365]
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
        era * 146_097 + doe as i64 - 719_468
    }

    #[test]
    fn fetch_timing_record_and_drain_empties_buffer_and_preserves_order() {
        // Drain first so this test is independent of any other test's
        // recordings (the collector is process-global and tests may
        // interleave threads within the same binary).
        let _ = fetch_timing::drain();

        fetch_timing::record(fetch_timing::FetchTiming {
            name: "cargo-nextest".to_string(),
            source: "github-release".to_string(),
            started_at_ms: 1000,
            duration_ms: 250,
        });
        fetch_timing::record(fetch_timing::FetchTiming {
            name: "cargo-audit".to_string(),
            source: "crates-io".to_string(),
            started_at_ms: 1300,
            duration_ms: 400,
        });

        let drained = fetch_timing::drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].name, "cargo-nextest");
        assert_eq!(drained[0].source, "github-release");
        assert_eq!(drained[0].started_at_ms, 1000);
        assert_eq!(drained[0].duration_ms, 250);
        assert_eq!(drained[1].name, "cargo-audit");
        assert_eq!(drained[1].source, "crates-io");

        // Buffer is empty after drain.
        assert!(fetch_timing::drain().is_empty());
    }
}
