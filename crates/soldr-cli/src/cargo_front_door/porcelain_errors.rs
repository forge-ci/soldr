//! Failure/error identities and ANSI stripping for the porcelain digest.
//!
//! Split out of `porcelain.rs` for the 1,000-line production ceiling
//! (soldr#2493); the behaviour is unchanged and `porcelain` re-exports
//! nothing that moved -- callers import from here.

use super::porcelain::first_line;

/// Upper bound on the number of error identities carried in `summary`.
/// `summary.error_count` always reports the true count.
pub(super) const MAX_ERROR_IDENTITIES: usize = 20;

/// Upper bound on the distinct error lines remembered for de-duplication.
/// Past it, repeated occurrences count as they arrive rather than growing the
/// set without limit.
pub(super) const MAX_DEDUPE_ENTRIES: usize = 4096;

/// Count and name the failures/errors in a child stream.
pub(super) fn extract_errors(output: &str) -> (usize, Vec<String>) {
    if let Some(result) = extract_cargo_json_errors(output) {
        return result;
    }
    let mut errors: Vec<String> = Vec::new();
    // A single failure is often reported more than once: cargo-nextest prints
    // the `FAIL ...` line in the progress stream AND again in its summary, so a
    // raw occurrence count told a caller "3 errors" for ONE failing test. Count
    // and list each distinct line once, in first-seen order.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut count = 0usize;
    for line in output.lines() {
        let line = without_ansi(line);
        let trimmed = line.trim();
        if !is_error_line(trimmed) {
            continue;
        }
        // Bound the dedupe set so a pathological stream cannot grow it without
        // limit; past the cap, occurrences are counted as they arrive.
        if seen.len() < MAX_DEDUPE_ENTRIES && !seen.insert(trimmed.to_string()) {
            continue;
        }
        count += 1;
        if errors.len() < MAX_ERROR_IDENTITIES {
            errors.push(trimmed.to_string());
        }
    }
    (count, errors)
}

/// Strip ANSI escapes from one captured line.
///
/// Cargo colours `error[E0308]: ...` whenever the caller forces colour
/// (`CARGO_TERM_COLOR=always`, `--color always`, `CLICOLOR_FORCE`). The escape
/// bytes then sit in front of `error[`, so a plain `starts_with` scan reports
/// `status: "failed"` with `error_count: 0, errors: []` -- the empty digest this
/// mode exists to prevent. Borrow when there is nothing to strip, which is the
/// common (piped, uncoloured) case.
pub(super) fn without_ansi(line: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if !line.contains('\u{1b}') {
        return Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            // CSI: parameters/intermediates, then one final byte in `@..=~`.
            Some('[') => {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            // OSC: consume until BEL or ST (`ESC \`).
            Some(']') => {
                for next in chars.by_ref() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' {
                        let _ = chars.next();
                        break;
                    }
                }
            }
            // Two-byte escape, or a truncated sequence at end of line.
            Some(_) | None => {}
        }
    }
    Cow::Owned(out)
}

pub(super) fn is_error_line(trimmed: &str) -> bool {
    trimmed.starts_with("error[")
        || trimmed.starts_with("error:")
        // cargo-nextest's per-test failure line, e.g. `FAIL [ 0.1s] pkg::t`.
        || trimmed.starts_with("FAIL ")
        || trimmed.starts_with("FAILED ")
        || is_c_compiler_error(trimmed)
}

/// A GCC/Clang diagnostic: `path:line:col: error: ...` (or `fatal error:`).
pub(super) fn is_c_compiler_error(trimmed: &str) -> bool {
    let Some(at) = trimmed
        .find(": error: ")
        .or_else(|| trimmed.find(": fatal error: "))
    else {
        return false;
    };
    let mut parts = trimmed[..at].rsplitn(3, ':');
    let col = parts.next().unwrap_or_default();
    let line = parts.next().unwrap_or_default();
    [line, col]
        .iter()
        .all(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// Errors from cargo's `--message-format=json` stream, when this stream is
/// one. `None` means "not a JSON stream" so the plain scan takes over.
///
/// The shape mirrors `strip_diagnostics::merge_cargo_json_diagnostics`
/// (`reason == "compiler-message"`, `/message/rendered`), extended to the
/// error level so a failure has an identity rather than just a count.
/// Returns `(true_count, capped_identities)`.
pub(super) fn extract_cargo_json_errors(output: &str) -> Option<(usize, Vec<String>)> {
    let mut saw_message = false;
    let mut errors: Vec<String> = Vec::new();
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
            != Some("error")
        {
            continue;
        }
        total += 1;
        if errors.len() < MAX_ERROR_IDENTITIES {
            errors.push(json_error_identity(&value));
        }
    }
    saw_message.then_some((total, errors))
}

/// Short identity for one cargo JSON `compiler-message` error: its rustc
/// error code when present (`E0308`), else the first line of the message.
pub(super) fn json_error_identity(value: &serde_json::Value) -> String {
    let code = value
        .pointer("/message/code/code")
        .and_then(serde_json::Value::as_str);
    let message = value
        .pointer("/message/message")
        .and_then(serde_json::Value::as_str)
        .map(first_line)
        .unwrap_or_else(|| "error".to_string());
    match code {
        Some(code) => format!("{code}: {message}"),
        None => message,
    }
}
