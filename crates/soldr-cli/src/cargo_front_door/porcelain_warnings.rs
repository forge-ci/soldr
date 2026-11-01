//! Warning identities for the porcelain digest (#3924).
//!
//! A build that exits zero can still emit `unused variable` / `unused import`
//! / `deprecated` diagnostics. The digest has to say so, or a caller that
//! wants to know whether there is anything to fix has to read the whole run
//! log back -- the context cost porcelain exists to remove.
//!
//! Split out of `porcelain.rs` to keep that file under the 1,000-line
//! production ceiling (#2493); the shape mirrors `porcelain_failures.rs`,
//! which does the same job for named test failures.

use super::porcelain::first_line;
use super::porcelain_errors::{without_ansi, MAX_DEDUPE_ENTRIES};

/// Upper bound on the distinct warning identities carried in the digest.
///
/// Warnings are not failures, so the digest only has to be enough for a
/// caller to decide whether to open `log_path` -- five distinct identities
/// covers the ordinary "one or two new lints" case without turning the
/// digest back into the relay this mode replaces.
pub(super) const MAX_WARNING_IDENTITIES: usize = 5;

/// Upper bound on one warning identity's length (message plus location).
pub(super) const MAX_WARNING_IDENTITY_CHARS: usize = 200;

/// Count and name the DISTINCT warnings in a child stream.
///
/// A porcelain success digest used to say only `status: "ok", error_count: 0`,
/// so a build that exited zero *with* warnings looked clean and the caller had
/// to open `log_path` to find out. The identity here is rustc's header line
/// plus the ` --> file:line:col` location when one is printed; the caret,
/// help and note lines below it stay in the durable log.
///
/// Cargo's trailing tally (``warning: `crate` (lib) generated 2 warnings``) is
/// not a warning, so it is not counted. Returns `(distinct_count,
/// capped_identities)`.
pub(super) fn extract(output: &str) -> (usize, Vec<String>) {
    if let Some(result) = extract_cargo_json_warnings(output) {
        return result;
    }
    let lines: Vec<&str> = output.lines().collect();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut count = 0usize;
    for (index, line) in lines.iter().enumerate() {
        let line = without_ansi(line);
        let trimmed = line.trim();
        if !is_warning_header(trimmed) || is_warning_summary(trimmed) {
            continue;
        }
        let identity = bounded_identity(&with_location(trimmed, &lines[index + 1..]));
        if seen.len() < MAX_DEDUPE_ENTRIES && !seen.insert(identity.clone()) {
            continue;
        }
        count += 1;
        if warnings.len() < MAX_WARNING_IDENTITIES {
            warnings.push(identity);
        }
    }
    (count, warnings)
}

/// Does this captured line open a rustc warning block?
///
/// `warning: <message>` is the common form; `warning[<lint>]: <message>` is the
/// structured spelling. `error: warnings emitted` is excluded because it is
/// built from the same word and is a tally.
fn is_warning_header(trimmed: &str) -> bool {
    trimmed.starts_with("warning:") || trimmed.starts_with("warning[")
}

/// Cargo's per-crate tally, in any of the shapes it prints:
///
/// * ``warning: `warny` (lib) generated 2 warnings``
/// * ``warning: `warny` (lib) generated 1 warning (run `cargo fix ...`)``
/// * ``warning: 3 warnings emitted``
///
/// All of them are built from the word `warning` and a count, so they are
/// matched by shape rather than by a suffix test (the suffix test missed the
/// `cargo fix` suggestion cargo appends to a single-warning tally).
fn is_warning_summary(trimmed: &str) -> bool {
    if let Some((_, rest)) = trimmed.split_once(" generated ") {
        let count_leads = rest.starts_with(|ch: char| ch.is_ascii_digit());
        return count_leads && rest.contains("warning");
    }
    trimmed.starts_with("warning: ") && trimmed.ends_with("emitted")
}

/// Append rustc's `--> file:line:col` location, when the block prints one
/// within the next few lines.
fn with_location(header: &str, following: &[&str]) -> String {
    for line in following.iter().take(4) {
        let line = without_ansi(line);
        let trimmed = line.trim();
        if let Some(location) = trimmed.strip_prefix("--> ") {
            return format!("{header} [{}]", location.trim());
        }
        // The caret block before the location would mean rustc printed no
        // location at all; stop rather than scanning unrelated output.
        if trimmed.starts_with('|') || trimmed.starts_with("= ") {
            break;
        }
    }
    header.to_string()
}

/// Cap one identity so a pathological lint message cannot re-inflate the digest.
fn bounded_identity(identity: &str) -> String {
    if identity.chars().count() <= MAX_WARNING_IDENTITY_CHARS {
        return identity.to_string();
    }
    let mut capped: String = identity.chars().take(MAX_WARNING_IDENTITY_CHARS).collect();
    capped.push_str("...");
    capped
}

/// Warnings from cargo's `--message-format=json` stream, when this stream is
/// one. `None` means "not a JSON stream" so the plain scan takes over.
fn extract_cargo_json_warnings(output: &str) -> Option<(usize, Vec<String>)> {
    let mut saw_message = false;
    let mut warnings: Vec<String> = Vec::new();
    let mut total = 0usize;
    for line in output.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(reason) = value.get("reason").and_then(serde_json::Value::as_str) else {
            continue;
        };
        saw_message = true;
        if reason != "compiler-message" {
            continue;
        }
        if value
            .pointer("/message/level")
            .and_then(serde_json::Value::as_str)
            != Some("warning")
        {
            continue;
        }
        total += 1;
        if warnings.len() < MAX_WARNING_IDENTITIES {
            warnings.push(bounded_identity(&json_warning_identity(&value)));
        }
    }
    saw_message.then_some((total, warnings))
}

/// Identity for one cargo JSON `compiler-message` warning: the message's first
/// line, with the primary span appended when cargo reports one.
fn json_warning_identity(value: &serde_json::Value) -> String {
    let message = value
        .pointer("/message/message")
        .and_then(serde_json::Value::as_str)
        .map(first_line)
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| "warning".to_string());
    let span = value.pointer("/message/spans/0");
    let location = span
        .and_then(|span| span.get("file_name"))
        .and_then(serde_json::Value::as_str)
        .map(|file| {
            let line = span
                .and_then(|span| span.get("line_start"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default();
            let column = span
                .and_then(|span| span.get("column_start"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default();
            format!("{file}:{line}:{column}")
        });
    match location {
        Some(location) => format!("warning: {message} [{location}]"),
        None => format!("warning: {message}"),
    }
}
