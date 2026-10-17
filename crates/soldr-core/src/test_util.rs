//! Test-support helpers shared across the workspace.
//!
//! Per-test timeouts are NOT here. They used to be: `timed_test!` wrapped every
//! test body in a worker thread and called `std::process::abort()` when the
//! budget expired. soldr#2493 removed it in favour of cargo-nextest's
//! `slow-timeout` / `terminate-after`, configured in `.config/nextest.toml`.
//!
//! The macro had two failures that tuning could not fix: its diagnostic went
//! through `eprintln!`, which libtest captures and only prints when it reports
//! a result -- which `abort()` guarantees never happens -- and the abort tore
//! down every other test in the binary. nextest runs each test in its own
//! process, so a timeout is attributable, its output survives, and it kills
//! only the test that earned it.
//!
//! What remains here is the Windows leaked-daemon diagnostic (issue #780) and
//! the shared process-env lock.
// ---------------------------------------------------------------------------
// Windows leaked-daemon diagnostic (issue #780).
//
// During PR #779 validation, the soldr-cli lib test binary on Windows
// exited with STATUS_STACK_BUFFER_OVERRUN (0xc0000409) AFTER all
// individual tests reported success. The working hypothesis is that
// one of soldr's spawned subprocesses (zccache-daemon, soldr-daemon)
// keeps a stdio handle inherited from the captured test process
// alive past test teardown, and Windows' fast-fail mechanism trips
// at DLL_PROCESS_DETACH. Issue #692 documents an analogous earlier
// instance with `cli_cargo_native_cc` skips still in place.
//
// `tasklist_snapshot_soldr_daemons` is the diagnostic primitive #780's
// acceptance criteria asked for: a CI-callable helper that surfaces
// any leftover soldr-managed daemon process so a `0xc0000409` exit is
// actionable instead of opaque. The parser is deliberately tolerant
// of `tasklist` quirks ("INFO: No tasks…" header, CSV quoting,
// trailing CR/LF) so a CI snippet like
//
//   if: failure()
//   run: cargo test ... ; powershell -Command "tasklist /FI 'IMAGENAME eq zccache-daemon.exe'"
//
// remains a backstop while the in-process call gives Rust tests the
// same data without shelling out themselves.

/// One row from a `tasklist /FO CSV /NH` enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeakedDaemonInfo {
    pub image_name: String,
    pub pid: u32,
}

/// Names this helper considers "soldr-managed daemons" for the purposes
/// of #780 leak detection. Matched case-insensitively; the `.exe`
/// suffix is required because `tasklist` always reports it on Windows
/// and reporting a Unix-style name here would silently miss real
/// matches.
pub const SOLDR_DAEMON_IMAGE_NAMES: &[&str] = &["zccache-daemon.exe", "soldr-daemon.exe"];

/// Parse the output of `tasklist /FO CSV /NH /FI "IMAGENAME eq <name>"`
/// into a structured list. Tolerant of:
///
/// * the `INFO: No tasks are running which match the specified criteria.`
///   stderr/stdout line that ships when zero matches are found,
/// * extra surrounding whitespace + trailing CRLF,
/// * the standard 5-column CSV (`"name","pid","session","sessno","memuse"`).
///
/// Returns an empty Vec for empty or no-match output rather than
/// panicking so callers can use `.is_empty()` as the "all clean"
/// signal in their assertions.
pub fn parse_tasklist_csv(stdout: &str) -> Vec<LeakedDaemonInfo> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("INFO:") {
            continue;
        }
        // Expected: "name","pid","session","sessno","memuse"
        let mut fields = Vec::new();
        let mut current = String::new();
        let mut in_quote = false;
        for ch in trimmed.chars() {
            match ch {
                '"' => in_quote = !in_quote,
                ',' if !in_quote => {
                    fields.push(std::mem::take(&mut current));
                }
                _ => current.push(ch),
            }
        }
        fields.push(current);
        if fields.len() < 2 {
            continue;
        }
        let image_name = fields[0].trim().to_string();
        let Ok(pid) = fields[1].trim().parse::<u32>() else {
            continue;
        };
        if SOLDR_DAEMON_IMAGE_NAMES
            .iter()
            .any(|known| image_name.eq_ignore_ascii_case(known))
        {
            out.push(LeakedDaemonInfo { image_name, pid });
        }
    }
    out
}

/// Windows-only: enumerate leftover soldr-managed daemons by shelling
/// out to `tasklist`. Returns an empty Vec on non-Windows so callers
/// can write platform-independent diagnostic code that only fires
/// when actually relevant.
pub fn tasklist_snapshot_soldr_daemons() -> Vec<LeakedDaemonInfo> {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
        return Vec::new();
    }
    let mut all = Vec::new();
    for name in SOLDR_DAEMON_IMAGE_NAMES {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("IMAGENAME eq {name}"), "/FO", "CSV", "/NH"])
            .output();
        let Ok(output) = output else { continue };
        let stdout = String::from_utf8_lossy(&output.stdout);
        all.extend(parse_tasklist_csv(&stdout));
    }
    all
}

/// Render a list of leaked daemons in a CI-friendly multi-line block
/// that points the operator at #780 and #692. Returns `None` when the
/// list is empty so callers can write
/// `if let Some(diag) = format_leaked_daemons(&snap) { panic!("{diag}") }`.
pub fn format_leaked_daemons(daemons: &[LeakedDaemonInfo]) -> Option<String> {
    if daemons.is_empty() {
        return None;
    }
    let mut out = String::from(
        "Detected leftover soldr-managed daemon process(es) after test teardown. \
         This is the leading hypothesis for the Windows lib-test \
         STATUS_STACK_BUFFER_OVERRUN (0xc0000409) fast-fail described in #780. \
         Likely related to #692 stdio-handle inheritance. Suspects:\n",
    );
    for d in daemons {
        out.push_str(&format!("  - {} pid={}\n", d.image_name, d.pid));
    }
    Some(out)
}

/// The one process-wide barrier for environment mutation in tests.
///
/// soldr#1663 established that two mutexes guarding one variable provide no
/// mutual exclusion at all: a crate's unit tests share a process, so each
/// module's private mutex only excludes its own tests.
///
/// soldr#1994 showed the same failure across a *crate* boundary. `soldr-cli`
/// serialized `PATH` under its own `TEST_PROCESS_ENV_LOCK` while
/// `soldr-core`'s `cargo_path_check` mutated `PATH` under nothing — and
/// soldr-core cannot reach a barrier that lives downstream of it. This is the
/// only crate every other one depends on, so this is where the barrier has to
/// live for it to be genuinely shared.
///
/// Deliberately not `#[cfg(test)]`: that attribute applies when *this* crate
/// is under test, so a gated static would vanish for every downstream crate
/// that needs it. The rest of this module is un-gated for the same reason.
///
/// Deliberately a raw `std::sync::Mutex`: tests hold it for their whole body,
/// which the `soldr_platform::sync` critical-section watchdog (15 s) must not
/// bound, and every acquisition happens in test-harness builds, outside
/// `ban_raw_sync_primitive`'s scope (soldr#3836).
#[cfg_attr(dylint_lib = "ban_raw_sync_primitive", allow(ban_raw_sync_primitive))]
pub static TEST_PROCESS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Leaked-daemon diagnostic parser (issue #780).
    // -----------------------------------------------------------------

    #[test]
    fn tasklist_parser_extracts_zccache_daemon_row() {
        // Actual `tasklist /FO CSV /NH /FI "IMAGENAME eq zccache-daemon.exe"`
        // output shape from a Windows host with one running daemon.
        let stdout = r#""zccache-daemon.exe","12345","Console","1","8,192 K""#;
        let parsed = parse_tasklist_csv(stdout);
        assert_eq!(
            parsed,
            vec![LeakedDaemonInfo {
                image_name: "zccache-daemon.exe".to_string(),
                pid: 12345,
            }]
        );
    }

    #[test]
    fn tasklist_parser_extracts_soldr_daemon_row() {
        let stdout = r#""soldr-daemon.exe","987","Services","0","4,096 K""#;
        let parsed = parse_tasklist_csv(stdout);
        assert_eq!(
            parsed,
            vec![LeakedDaemonInfo {
                image_name: "soldr-daemon.exe".to_string(),
                pid: 987,
            }]
        );
    }

    #[test]
    fn tasklist_parser_returns_empty_for_no_match_banner() {
        // When zero processes match the filter, tasklist writes its
        // banner to stdout, NOT stderr. Treat it as "all clean".
        let stdout = "INFO: No tasks are running which match the specified criteria.\r\n";
        assert!(parse_tasklist_csv(stdout).is_empty());
    }

    #[test]
    fn tasklist_parser_returns_empty_for_blank_output() {
        assert!(parse_tasklist_csv("").is_empty());
        assert!(parse_tasklist_csv("   \r\n\r\n").is_empty());
    }

    #[test]
    fn tasklist_parser_ignores_unrelated_image_names() {
        // A user might have other long-running processes whose names
        // happen to contain "daemon". The diagnostic must only flag
        // soldr-managed ones to avoid false-positive CI failures.
        let stdout = r#""my-other-daemon.exe","42","Console","1","100 K""#;
        assert!(parse_tasklist_csv(stdout).is_empty());
    }

    #[test]
    fn tasklist_parser_is_case_insensitive_on_image_name() {
        // Some Windows versions report camel-case ImageName values.
        // The diagnostic should fire regardless of case.
        let stdout = r#""ZCcache-Daemon.EXE","7","Console","1","100 K""#;
        let parsed = parse_tasklist_csv(stdout);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].pid, 7);
    }

    #[test]
    fn tasklist_parser_handles_multiple_rows() {
        let stdout = "\"zccache-daemon.exe\",\"1\",\"Console\",\"1\",\"100 K\"\r\n\
                      \"zccache-daemon.exe\",\"2\",\"Console\",\"1\",\"200 K\"\r\n";
        let parsed = parse_tasklist_csv(stdout);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].pid, 1);
        assert_eq!(parsed[1].pid, 2);
    }

    #[test]
    fn tasklist_parser_skips_malformed_pid() {
        // Defensive: a row whose pid field is non-numeric should not
        // panic; it should be silently skipped so the CI signal stays
        // clean if tasklist's format ever changes.
        let stdout = r#""zccache-daemon.exe","not-a-number","Console","1","100 K""#;
        assert!(parse_tasklist_csv(stdout).is_empty());
    }

    #[test]
    fn format_leaked_daemons_returns_none_when_empty() {
        assert!(format_leaked_daemons(&[]).is_none());
    }

    #[test]
    fn format_leaked_daemons_mentions_780_and_692_for_actionability() {
        let formatted = format_leaked_daemons(&[LeakedDaemonInfo {
            image_name: "zccache-daemon.exe".to_string(),
            pid: 4321,
        }])
        .expect("non-empty diagnostic");
        // Acceptance criterion: future 0xc0000409 exits should be
        // actionable. The diagnostic must name the suspect AND point
        // at the open investigation issues so triage starts from a
        // known baseline.
        assert!(formatted.contains("0xc0000409"), "{formatted}");
        assert!(formatted.contains("#780"), "{formatted}");
        assert!(formatted.contains("#692"), "{formatted}");
        assert!(formatted.contains("zccache-daemon.exe"), "{formatted}");
        assert!(formatted.contains("pid=4321"), "{formatted}");
    }
}
