//! Reporting a fatal route refusal once per build (soldr#3401).
//!
//! A version-policy refusal is deterministic, yet cargo starts every in-flight
//! unit's wrapper before the first one fails, and each used to dial the broker,
//! be refused, and print the whole explanation: 125 near-identical lines for
//! one cause. The first wrapper to hit the refusal records a sentinel keyed by
//! the build; every later wrapper in that build sees it before dialing and
//! exits with one line pointing back at the first error.
//!
//! Keyed on `SOLDR_BUILD_SESSION_ID`, which the front door exports for the
//! whole build, so nothing needs clearing at build start: a new build has a new
//! id. Without that id (a bypassed front door, a hand-set `RUSTC_WRAPPER`) there
//! is no sentinel and each invocation reports for itself, as before. Stale
//! sentinels are swept on write.

use crate::core::SoldrPaths;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

const SENTINEL_DIR: &str = "route-refusals";
const SENTINEL_TTL: Duration = Duration::from_secs(60 * 60);

/// The one line every repeat refusal in a build prints.
pub(crate) const REPEAT_LINE: &str =
    "soldr: broker route refused earlier in this build -- see the first error above";

/// Whether this refusal was the first of its build to be recorded.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Recorded {
    /// Print the full explanation.
    First,
    /// Another wrapper already did; print [`REPEAT_LINE`].
    Repeat,
    /// No build id, or the sentinel could not be written: report normally.
    Unkeyed,
}

fn sentinel_path(paths: &SoldrPaths, build_id: u64) -> PathBuf {
    paths
        .cache
        .join(SENTINEL_DIR)
        .join(format!("build-{build_id}.refused"))
}

/// Has this build already had a route refused? Checked before dialing.
pub(crate) fn already_refused(paths: &SoldrPaths, build_id: Option<u64>) -> bool {
    build_id.is_some_and(|id| sentinel_path(paths, id).is_file())
}

/// Record a fatal refusal for this build. The first caller wins the atomic
/// create; the rest are told it is a repeat.
pub(crate) fn record(paths: &SoldrPaths, build_id: Option<u64>) -> Recorded {
    let Some(id) = build_id else {
        return Recorded::Unkeyed;
    };
    let path = sentinel_path(paths, id);
    let Some(dir) = path.parent() else {
        return Recorded::Unkeyed;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return Recorded::Unkeyed;
    }
    sweep_stale(dir);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(_) => Recorded::First,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Recorded::Repeat,
        Err(_) => Recorded::Unkeyed,
    }
}

fn sweep_stale(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.filter_map(Result::ok) {
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > SENTINEL_TTL);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (tempfile::TempDir, SoldrPaths) {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        (temp, paths)
    }

    #[test]
    fn the_first_refusal_of_a_build_is_first_and_the_rest_are_repeats() {
        let (_temp, paths) = paths();
        assert!(!already_refused(&paths, Some(7)));
        assert_eq!(record(&paths, Some(7)), Recorded::First);
        assert!(already_refused(&paths, Some(7)));
        assert_eq!(record(&paths, Some(7)), Recorded::Repeat);
        assert_eq!(record(&paths, Some(7)), Recorded::Repeat);
    }

    #[test]
    fn another_build_is_not_affected() {
        let (_temp, paths) = paths();
        assert_eq!(record(&paths, Some(7)), Recorded::First);
        assert!(!already_refused(&paths, Some(8)));
        assert_eq!(record(&paths, Some(8)), Recorded::First);
    }

    #[test]
    fn without_a_build_id_nothing_is_keyed() {
        let (_temp, paths) = paths();
        assert!(!already_refused(&paths, None));
        assert_eq!(record(&paths, None), Recorded::Unkeyed);
        assert_eq!(record(&paths, None), Recorded::Unkeyed);
    }

    #[test]
    fn a_stale_sentinel_is_swept_on_the_next_write() {
        let (_temp, paths) = paths();
        assert_eq!(record(&paths, Some(1)), Recorded::First);
        let old = sentinel_path(&paths, 1);
        let aged = SystemTime::now() - SENTINEL_TTL - Duration::from_secs(60);
        let file = std::fs::OpenOptions::new().write(true).open(&old).unwrap();
        file.set_modified(aged).unwrap();
        drop(file);
        assert_eq!(record(&paths, Some(2)), Recorded::First);
        assert!(!old.exists(), "a sentinel older than the TTL must be swept");
    }
}
