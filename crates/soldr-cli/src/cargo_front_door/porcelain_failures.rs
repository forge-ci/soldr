//! Failing-test detail for the porcelain summary (soldr#3961).
//!
//! A failing `cargo test` / `cargo nextest` run used to surface only
//! `error: test failed, to rerun pass ...`; the test name and its panic
//! lived only in the durable log, costing the agent another tool call.
//! This module lifts a bounded digest out of the captured stream: the
//! failing test names, each one's panic `file:line:col` and message lines,
//! and the per-binary `test result: FAILED` lines.

use serde::Serialize;

/// Failing tests named in the summary; the rest are counted in `failed_tests_more`.
pub(super) const MAX_FAILED_TESTS: usize = 10;
/// Message lines kept per failing test.
const MAX_MESSAGE_LINES: usize = 20;
/// Longest single message line kept (bytes, cut on a char boundary).
const MAX_LINE_BYTES: usize = 300;
/// `test result: FAILED` lines kept.
const MAX_RESULT_LINES: usize = 10;

/// One failing test and its panic detail.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub(super) struct FailedTest {
    pub(super) name: String,
    /// Panic `file:line:col`, when the stream carried one.
    pub(super) location: Option<String>,
    /// Message / assertion lines following the panic location.
    pub(super) message: Vec<String>,
}

/// Bounded failing-test digest of one stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct FailureDigest {
    pub(super) tests: Vec<FailedTest>,
    /// Failing tests beyond [`MAX_FAILED_TESTS`].
    pub(super) more: usize,
    pub(super) results: Vec<String>,
}

fn clip(line: &str) -> String {
    let line = line.trim_end();
    if line.len() <= MAX_LINE_BYTES {
        return line.to_string();
    }
    let mut end = MAX_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &line[..end])
}

/// Name from a section header: libtest `---- NAME stdout ----` or
/// nextest `--- STDERR:              crate bin NAME ---`.
fn section_name(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("---- ") {
        let name = rest.strip_suffix(" stdout ----")?;
        return Some(name.trim().to_string());
    }
    let rest = line.strip_prefix("--- STD")?;
    let rest = rest.strip_suffix(" ---")?;
    let (_, tail) = rest.split_once(':')?;
    tail.split_whitespace().last().map(str::to_string)
}

/// Does this line end a failure section?
fn ends_section(line: &str) -> bool {
    line == "failures:"
        || line.starts_with("test result:")
        || line.starts_with("FAIL ")
        || line.starts_with("error: test failed")
        || line.starts_with("------------")
        || section_name(line).is_some()
}

fn is_noise(line: &str) -> bool {
    line.is_empty()
        || line.starts_with("note: run with `RUST_BACKTRACE")
        || line == "stack backtrace:"
}

/// `file:line:col` from `thread 'x' panicked at FILE:L:C:` (old and new rustc forms).
fn panic_location(line: &str) -> Option<String> {
    let (_, rest) = line.split_once("panicked at ")?;
    let loc = rest.trim().trim_end_matches(':').trim();
    (!loc.is_empty()).then(|| loc.to_string())
}

fn parse_section(name: String, body: &[String]) -> FailedTest {
    let mut test = FailedTest {
        name,
        ..FailedTest::default()
    };
    let mut after_panic = false;
    for line in body {
        let trimmed = line.trim();
        if test.location.is_none() {
            if let Some(loc) = panic_location(trimmed) {
                test.location = Some(loc);
                after_panic = true;
                // Older rustc prints the message on the same line after `, `.
            }
            continue;
        }
        if is_noise(trimmed) || !after_panic {
            continue;
        }
        if test.message.len() == MAX_MESSAGE_LINES {
            break;
        }
        test.message.push(clip(line));
    }
    test
}

/// Extract the digest from a captured stream (ANSI already stripped per line).
pub(super) fn digest(lines: &[String]) -> FailureDigest {
    let mut tests: Vec<FailedTest> = Vec::new();
    let mut results: Vec<String> = Vec::new();
    let mut named: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end();
        if let Some(name) = section_name(line.trim()) {
            let start = i + 1;
            let mut end = start;
            while end < lines.len() && !ends_section(lines[end].trim()) {
                end += 1;
            }
            let test = parse_section(name, &lines[start..end]);
            // nextest prints a STDOUT and a STDERR block per test: keep the one with a panic.
            match tests.iter_mut().find(|t| t.name == test.name) {
                Some(existing) if existing.location.is_none() => *existing = test,
                Some(_) => {}
                None => tests.push(test),
            }
            i = end;
            continue;
        }
        let trimmed = line.trim();
        if let Some(name) = trimmed
            .strip_prefix("test ")
            .and_then(|r| r.strip_suffix(" ... FAILED"))
        {
            named.push(name.to_string());
        } else if trimmed.starts_with("test result: FAILED") && results.len() < MAX_RESULT_LINES {
            results.push(clip(trimmed));
        }
        i += 1;
    }
    for name in named {
        if !tests.iter().any(|t| t.name == name) {
            tests.push(FailedTest {
                name,
                ..FailedTest::default()
            });
        }
    }
    let more = tests.len().saturating_sub(MAX_FAILED_TESTS);
    tests.truncate(MAX_FAILED_TESTS);
    FailureDigest {
        tests,
        more,
        results,
    }
}
