//! Shared broker-SESSION compile dispatch.
//!
//! Both the soldr `RUSTC_WRAPPER` entry and the `zccache-soldr` multicall
//! entry terminate here. The broker is the only daemon-acquisition front
//! door; this module has no direct daemon socket, spawn, or rustc fallback.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};
use crate::daemon::protocol::CompileRequest;

/// Build the wire request used by wrapper-performance and env-contract tests.
pub fn build_compile_request(rustc_argv: &[String]) -> CompileRequest {
    let cwd = std::env::current_dir()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_default();
    crate::daemon::compile_request::build_compile_request_from(rustc_argv, cwd, std::env::vars())
}

/// Re-export the shared environment predicate for existing contract tests.
pub use crate::daemon::compile_request::is_compile_env_var;

/// Historical fallback journal path retained for status/doctor compatibility.
///
/// The mandatory broker cutover does not append new records. Existing records
/// remain readable so upgrading soldr does not erase diagnostic history.
pub(crate) fn compile_daemon_fallback_log_path(paths: &SoldrPaths) -> PathBuf {
    paths
        .root
        .join("logs")
        .join("compile-daemon-fallbacks.jsonl")
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CompileFallbackCursor {
    len: u64,
    tail_anchor: Vec<u8>,
}

/// Capture a cheap cursor for the historical fallback journal.
pub(crate) fn compile_daemon_fallback_cursor(paths: &SoldrPaths) -> CompileFallbackCursor {
    const ANCHOR_BYTES: u64 = 512;

    let path = compile_daemon_fallback_log_path(paths);
    let Ok(mut file) = std::fs::File::open(path) else {
        return CompileFallbackCursor::default();
    };
    let Ok(len) = file.metadata().map(|metadata| metadata.len()) else {
        return CompileFallbackCursor::default();
    };
    let anchor_start = len.saturating_sub(ANCHOR_BYTES);
    if file.seek(SeekFrom::Start(anchor_start)).is_err() {
        return CompileFallbackCursor::default();
    }
    let mut tail_anchor = Vec::with_capacity((len - anchor_start) as usize);
    if file.read_to_end(&mut tail_anchor).is_err() {
        return CompileFallbackCursor::default();
    }
    CompileFallbackCursor { len, tail_anchor }
}

/// Count historical fallback records appended during a front-door session.
///
/// This normally returns zero after the mandatory broker cutover. Keeping the
/// reader avoids changing the existing status/log JSON shape in the same PR.
pub(crate) fn compile_daemon_fallback_count_since(
    paths: &SoldrPaths,
    cursor: &CompileFallbackCursor,
    session_id: u64,
) -> std::io::Result<(usize, PathBuf)> {
    let path = compile_daemon_fallback_log_path(paths);
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, path)),
        Err(error) => return Err(error),
    };
    let len = file.metadata()?.len();
    let anchor_start = cursor.len.saturating_sub(cursor.tail_anchor.len() as u64);
    let anchor_matches = if len < cursor.len {
        false
    } else {
        file.seek(SeekFrom::Start(anchor_start))?;
        let mut current_anchor = vec![0; cursor.tail_anchor.len()];
        file.read_exact(&mut current_anchor)?;
        current_anchor == cursor.tail_anchor
    };
    file.seek(SeekFrom::Start(if anchor_matches { cursor.len } else { 0 }))?;
    let mut appended = String::new();
    file.read_to_string(&mut appended)?;
    let mut count = 0;
    for (index, line) in appended.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: serde_json::Value = serde_json::from_str(line).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "malformed fallback record {} in {}: {error}",
                    index + 1,
                    path.display()
                ),
            )
        })?;
        if event["event"] == "compile_daemon_fallback"
            && event["session_id"].as_u64() == Some(session_id)
        {
            count += 1;
        }
    }
    Ok((count, path))
}

/// Dispatch one rustc-style invocation through the broker SESSION service.
pub fn dispatch_compile<O, E>(
    rustc_argv: &[String],
    _stdout: O,
    _stderr: E,
) -> Result<i32, SoldrError>
where
    O: Write,
    E: Write,
{
    match crate::session_transport::session_hot_path(rustc_argv) {
        crate::session_transport::SessionHotPathOutcome::Served(exit_code) => Ok(exit_code),
        crate::session_transport::SessionHotPathOutcome::HardFail(error) => {
            Err(SoldrError::Other(match fatal_refusal_message(&error) {
                Some(message) => message,
                None => session_failure_message(&error),
            }))
        }
    }
}

/// The message for a fatal broker route refusal (soldr#3401), or `None` for any
/// other failure. The first refusal of a build prints the full explanation and
/// records a sentinel; every later one in that build prints one line pointing
/// back at it. The "SESSION compile failed" prefix is dropped: a refused route is
/// not a failed compile, and repeating the prefix per crate was the spew.
fn fatal_refusal_message(error: &std::io::Error) -> Option<String> {
    let paths = crate::core::SoldrPaths::new().ok();
    fatal_refusal_message_with(
        error,
        paths.as_ref(),
        crate::wrapper_target::read_build_session_id_env(),
    )
}

/// [`fatal_refusal_message`] with the sentinel location and build id supplied,
/// so tests do not touch the real cache or the process environment.
fn fatal_refusal_message_with(
    error: &std::io::Error,
    paths: Option<&crate::core::SoldrPaths>,
    build_id: Option<u64>,
) -> Option<String> {
    let refusal = crate::route_refusal::RouteRefusal::from_io(error).filter(|r| r.fatal)?;
    if refusal.repeat {
        return Some(refusal.text.clone());
    }
    let recorded = match paths {
        Some(paths) => crate::route_refusal_sentinel::record(paths, build_id),
        None => crate::route_refusal_sentinel::Recorded::Unkeyed,
    };
    Some(match recorded {
        crate::route_refusal_sentinel::Recorded::Repeat => {
            crate::route_refusal_sentinel::REPEAT_LINE.to_string()
        }
        crate::route_refusal_sentinel::Recorded::First
        | crate::route_refusal_sentinel::Recorded::Unkeyed => refusal.text.clone(),
    })
}

/// Does this error mean the peer went away mid-request, rather than refusing
/// the work on its merits?
///
/// All four are the same event seen from different points in the exchange: the
/// process on the other end of the socket stopped existing while a request was
/// in flight.
fn peer_vanished(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
    )
}

/// Explain a SESSION dispatch failure, and say when the unit being blamed is a
/// bystander.
///
/// soldr#2824: a `Nextest Cacheability` run failed with
///
/// ```text
/// warning: ring@0.17.14: zccache-soldr: dispatch failed:
///   broker SESSION compile failed: Connection reset by peer (os error 104)
/// error: failed to run custom build command for `ring v0.17.14`
/// ```
///
/// `ring` did nothing wrong. The daemon's own log shows what happened:
///
/// ```text
/// soldr-daemon: INFO: compiling sqlite3.c (9.1 MB) -- an amalgamated ...
/// soldr-daemon: compile concurrency = 2 (from SOLDR_JOBS)   <- a NEW generation
/// soldr-daemon: rejected broker connection handoff: Broken pipe (os error 32)
/// ```
///
/// with the previous daemon left `<defunct>` after 11 minutes of CPU. It was
/// killed while holding a 9.1 MB amalgamated translation unit, and every
/// in-flight dispatch reset with it. cargo then names whichever unit happened
/// to be dispatching, which is the one thing in the message guaranteed not to
/// be the cause -- and it is the name a reader chases.
///
/// A bare `os error 104` cannot be acted on. Saying the peer vanished, and
/// that the blamed unit is a bystander, points at the daemon log where the
/// real subject is recorded (soldr#2781).
fn session_failure_message(error: &std::io::Error) -> String {
    let mut message = format!("broker SESSION compile failed: {error}");
    if peer_vanished(error.kind()) {
        message.push_str(concat!(
            "
  the daemon serving this compile exited mid-request, so this ",
            "invocation was interrupted rather than failing on its own merits.",
            "
  cargo will blame whichever unit was dispatching; that unit is ",
            "the one that was interrupted, not necessarily the cause.",
            "
  the daemon's spawn log records what it was working on when it ",
            "went -- `soldr logs paths` -- and a large amalgamated C ",
            "translation unit under concurrent load is the known case ",
            "(soldr#2781).",
        ));
    }
    message
}

/// Wrapper/multicall convenience entry using the process standard streams.
pub fn compile_via_daemon(rustc_argv: &[String]) -> Result<i32, SoldrError> {
    dispatch_compile(rustc_argv, std::io::stdout(), std::io::stderr())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    /// The four ways the same event -- the peer stopped existing mid-request --
    /// surfaces depending on where in the exchange it was noticed.
    #[test]
    fn every_peer_vanished_kind_is_recognised() {
        for kind in [
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
            ErrorKind::UnexpectedEof,
        ] {
            assert!(peer_vanished(kind), "{kind:?}");
        }
    }

    /// A refusal on the merits must NOT be explained away as a dead daemon.
    /// Telling a reader to go look at the daemon log for a permission error
    /// sends them somewhere with nothing in it.
    #[test]
    fn an_ordinary_failure_is_not_treated_as_a_vanished_peer() {
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::NotFound,
            ErrorKind::InvalidData,
            ErrorKind::TimedOut,
        ] {
            assert!(!peer_vanished(kind), "{kind:?}");
        }
    }

    /// The exact error from the soldr#2824 run.
    #[test]
    fn a_connection_reset_says_the_unit_being_blamed_is_a_bystander() {
        let error = Error::new(
            ErrorKind::ConnectionReset,
            "Connection reset by peer (os error 104)",
        );
        let message = session_failure_message(&error);
        // The underlying error still leads: it is what a search will match on.
        assert!(
            message.starts_with("broker SESSION compile failed: "),
            "{message}"
        );
        assert!(message.contains("Connection reset by peer"), "{message}");
        // ...and then the part that makes it actionable.
        assert!(message.contains("exited mid-request"), "{message}");
        assert!(message.contains("not necessarily the cause"), "{message}");
        assert!(message.contains("soldr logs paths"), "{message}");
        assert!(message.contains("soldr#2781"), "{message}");
    }

    #[test]
    fn an_ordinary_failure_gets_no_daemon_explanation() {
        let error = Error::new(ErrorKind::PermissionDenied, "permission denied");
        let message = session_failure_message(&error);
        assert_eq!(message, "broker SESSION compile failed: permission denied");
    }
}

#[cfg(test)]
mod refusal_tests {
    use super::*;
    use crate::route_refusal::RouteRefusal;

    fn refusal(fatal: bool, repeat: bool) -> std::io::Error {
        std::io::Error::other(RouteRefusal {
            text: "the full explanatory block".into(),
            fatal,
            repeat,
        })
    }

    #[test]
    fn a_fatal_refusal_prints_the_block_once_per_build_then_one_line() {
        let temp = tempfile::tempdir().unwrap();
        let paths = crate::core::SoldrPaths::with_root(temp.path().join("root"));
        let error = refusal(true, false);

        let first = fatal_refusal_message_with(&error, Some(&paths), Some(42)).unwrap();
        assert_eq!(first, "the full explanatory block");
        for _ in 0..3 {
            let later = fatal_refusal_message_with(&error, Some(&paths), Some(42)).unwrap();
            assert_eq!(later, crate::route_refusal_sentinel::REPEAT_LINE);
        }
        // A different build starts over; a missing build id never dedupes.
        assert_eq!(
            fatal_refusal_message_with(&error, Some(&paths), Some(43)).unwrap(),
            "the full explanatory block"
        );
        for _ in 0..2 {
            assert_eq!(
                fatal_refusal_message_with(&error, Some(&paths), None).unwrap(),
                "the full explanatory block"
            );
        }
    }

    #[test]
    fn the_session_prefix_is_not_used_for_a_refusal_and_transient_ones_keep_it() {
        // A repeat carries only the pointer line.
        assert_eq!(
            fatal_refusal_message_with(&refusal(true, true), None, Some(1)).unwrap(),
            "the full explanatory block"
        );
        // A transient refusal, or any other error, is not handled here.
        assert!(fatal_refusal_message_with(&refusal(false, false), None, Some(1)).is_none());
        assert!(
            fatal_refusal_message_with(&std::io::Error::other("plain"), None, Some(1)).is_none()
        );
    }
}
