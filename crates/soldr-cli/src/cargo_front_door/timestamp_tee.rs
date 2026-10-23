//! Line-aware elapsed-time prefixing for relayed child output (#1802).
//!
//! Prefixes every line soldr relays from cargo with seconds since the
//! build session started, so per-line cost is visible at a glance:
//!
//! ```text
//! # t0=1784950000.123
//!     0.01 -> Run soldr cargo build -p soldr-cli --release
//!     0.42     Updating crates.io index
//!     1.90  Downloading crates ...
//! ```
//!
//! # Why a byte-level tee rather than a line reader
//!
//! The prefix is plain text inserted only at column 0. Everything
//! between line terminators — including ANSI colour escapes — is copied
//! verbatim, so colour survives without this code having to understand
//! it. A line-buffering reader would have to re-emit escapes, and would
//! stall progress bars until their newline arrived.
//!
//! A carriage return counts as a line start alongside a newline for
//! that reason: cargo redraws `Downloading`/`Building` progress with a
//! bare CR, and stamping each redraw is what makes a slow download
//! legible. A CRLF pair stamps once, not twice.
//!
//! # Diagnostic lines stay byte-exact (soldr#3850)
//!
//! A line that opens a rustc/cargo diagnostic (`error:`, `error[E..]:`,
//! `warning:`, `note:`, `help:`, or an indented ` --> ` location) is
//! relayed without a prefix, so `grep '^error'` and IDE problem matchers
//! still match when stderr is piped. Deciding needs only the first few
//! visible bytes, so the tee holds back at most a short line head (ANSI
//! colour escapes skipped) and never stalls a progress redraw.
//!
//! # What must NOT be stamped
//!
//! Only the copy going to the user's terminal. The bytes forwarded on
//! the capture channel stay raw, because the diagnostic scanner and the
//! cargo-JSON parser both match on cargo's exact output — a prefix
//! would break them. That separation is the whole reason this wraps the
//! *write* side inside the pipe readers rather than the reader itself.

use std::io::Write;
use std::time::Instant;

const LF: u8 = 0x0a;
const CR: u8 = 0x0d;

/// `SOLDR_TIMESTAMP_LINES` — `1`/`true`/`on` forces stamping on, and
/// `0`/`false`/`off` forces it off, overriding the TTY default.
pub(crate) const TIMESTAMP_LINES_ENV_VAR: &str = "SOLDR_TIMESTAMP_LINES";

/// Emitted once so absolute wall-clock times are derivable from the
/// elapsed offsets that follow.
/// Whether to print the `# t0=` anchor at all.
///
/// The anchor exists so elapsed stamps can be turned back into wall-clock
/// time. GitHub Actions already prefixes every log line with a UTC
/// timestamp, so there the anchor is one more line per invocation that
/// says nothing the log does not.
pub(crate) fn epoch_anchor_wanted(github_actions: bool) -> bool {
    !github_actions
}

pub(crate) fn epoch_anchor_line(now_unix_ms: i64) -> String {
    format!("# t0={}.{:03}\n", now_unix_ms / 1_000, now_unix_ms % 1_000)
}

/// Whether to stamp, given the env override and whether the stream is a
/// terminal.
///
/// Default is **on for non-TTY, off for TTY**. A CI log is read after
/// the fact, where "which line cost 40 seconds" is the whole question;
/// an interactive terminal already shows progress live, and stamping a
/// redrawing progress bar is noise. The env var overrides both ways so
/// neither default is a trap.
pub(crate) fn should_timestamp(env_value: Option<&str>, is_terminal: bool) -> bool {
    if let Some(v) = env_value {
        if crate::core::flag_value(v) {
            return true;
        }
        if crate::core::is_off_value(v) {
            return false;
        }
    }
    // Empty or unrecognised means "not configured".
    !is_terminal
}

/// Wraps a writer and inserts an elapsed-seconds prefix at each line
/// start.
pub(crate) struct TimestampedTee<W: Write> {
    inner: W,
    t0: Instant,
    at_line_start: bool,
    /// Suppresses a second stamp for the newline of a CRLF pair.
    last_was_cr: bool,
    /// Line-head bytes held back until [`classify_line_head`] decides
    /// whether the line is a diagnostic.
    pending: Vec<u8>,
    holding: bool,
}

/// Longest line head held back before giving up and stamping.
const MAX_PENDING: usize = 64;

/// Visible words that open a rustc/cargo diagnostic line; each must be
/// followed by `:` or `[` to count.
const DIAGNOSTIC_WORDS: [&str; 4] = ["error", "warning", "note", "help"];

#[derive(Debug, PartialEq, Eq)]
enum LineHead {
    Undecided,
    Diagnostic,
    Ordinary,
}

/// Visible text of `head` with ANSI CSI escapes removed, or `None` when
/// the head ends inside an escape.
fn visible_text(head: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(head.len());
    let mut i = 0;
    while i < head.len() {
        if head[i] == 0x1b {
            if i + 1 >= head.len() {
                return None;
            }
            if head[i + 1] != b'[' {
                i += 2;
                continue;
            }
            let mut j = i + 2;
            while j < head.len() && !(0x40..=0x7e).contains(&head[j]) {
                j += 1;
            }
            if j >= head.len() {
                return None;
            }
            i = j + 1;
            continue;
        }
        out.push(head[i]);
        i += 1;
    }
    Some(out)
}

fn classify_line_head(head: &[u8]) -> LineHead {
    let Some(text) = visible_text(head) else {
        return LineHead::Undecided;
    };
    let rest = text.trim_ascii_start();
    if rest.len() != text.len() {
        // Indented: only a ` --> file:line` location is a diagnostic.
        return if rest.starts_with(b"-->") {
            LineHead::Diagnostic
        } else if b"-->".starts_with(rest) {
            LineHead::Undecided
        } else {
            LineHead::Ordinary
        };
    }
    let mut undecided = text.is_empty();
    for word in DIAGNOSTIC_WORDS {
        let word = word.as_bytes();
        if text.len() > word.len() && text.starts_with(word) {
            if matches!(text[word.len()], b':' | b'[') {
                return LineHead::Diagnostic;
            }
        } else if word.starts_with(&text) {
            undecided = true;
        }
    }
    if undecided {
        LineHead::Undecided
    } else {
        LineHead::Ordinary
    }
}

impl<W: Write> TimestampedTee<W> {
    pub(crate) fn new(inner: W, t0: Instant) -> Self {
        Self {
            inner,
            t0,
            at_line_start: true,
            last_was_cr: false,
            pending: Vec::new(),
            holding: false,
        }
    }

    /// Emits the held line head, stamped unless it is a diagnostic.
    fn release(&mut self, diagnostic: bool) -> std::io::Result<()> {
        if !diagnostic {
            let prefix = self.prefix();
            self.inner.write_all(prefix.as_bytes())?;
        }
        self.inner.write_all(&self.pending)?;
        self.pending.clear();
        self.holding = false;
        Ok(())
    }

    fn prefix(&self) -> String {
        format!("{:>8.2} ", self.t0.elapsed().as_nanos() as f64 / 1e9)
    }
}

impl<W: Write> Write for TimestampedTee<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for &byte in buf {
            let is_lf = byte == LF;
            let is_cr = byte == CR;
            if self.at_line_start && !(is_lf && self.last_was_cr) {
                self.holding = true;
            }
            if self.holding {
                self.pending.push(byte);
                let head = if is_lf || is_cr {
                    let body = &self.pending[..self.pending.len() - 1];
                    match classify_line_head(body) {
                        LineHead::Undecided => LineHead::Ordinary,
                        decided => decided,
                    }
                } else if self.pending.len() >= MAX_PENDING {
                    LineHead::Ordinary
                } else {
                    classify_line_head(&self.pending)
                };
                if head != LineHead::Undecided {
                    self.release(head == LineHead::Diagnostic)?;
                }
            } else {
                self.inner.write_all(&[byte])?;
            }

            // A CRLF pair is one terminator: the CR sets the flag, and
            // the newline must not draw a second prefix on the way to
            // the same new line.
            self.at_line_start = is_lf || is_cr;
            self.last_was_cr = is_cr;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Write> Drop for TimestampedTee<W> {
    /// A line head still held at end of stream is an ordinary partial
    /// line; it must reach the terminal, not vanish.
    fn drop(&mut self) {
        if self.holding {
            let _ = self.release(false);
            let _ = self.inner.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamped(chunks: &[&[u8]]) -> String {
        let mut out = Vec::new();
        {
            let mut tee = TimestampedTee::new(&mut out, Instant::now());
            for chunk in chunks {
                tee.write_all(chunk).expect("write");
            }
        }
        String::from_utf8(out).expect("utf8")
    }

    /// Replace each `{:>8.2} ` prefix with a literal `<T> ` so
    /// assertions describe structure rather than elapsed time, which
    /// would otherwise make every one of these a clock race.
    fn shape(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = String::new();
        let mut i = 0;
        let mut at_line_start = true;
        while i < bytes.len() {
            if at_line_start {
                if let Some(consumed) = consume_prefix(&bytes[i..]) {
                    out.push_str("<T> ");
                    i += consumed;
                    at_line_start = false;
                    continue;
                }
            }
            let b = bytes[i];
            out.push(b as char);
            at_line_start = b == LF || b == CR;
            i += 1;
        }
        out
    }

    /// Length of a leading `[space]*<digits>.<digits><space>` run.
    fn consume_prefix(rest: &[u8]) -> Option<usize> {
        let mut i = 0;
        while rest.get(i) == Some(&b' ') {
            i += 1;
        }
        let digits_start = i;
        while rest.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == digits_start || rest.get(i) != Some(&b'.') {
            return None;
        }
        i += 1;
        let frac_start = i;
        while rest.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == frac_start || rest.get(i) != Some(&b' ') {
            return None;
        }
        Some(i + 1)
    }

    #[test]
    fn each_line_gets_one_prefix() {
        let text = stamped(&[b"alpha\nbeta\n"]);
        assert_eq!(shape(&text), "<T> alpha\n<T> beta\n");
    }

    #[test]
    fn a_chunk_split_mid_line_does_not_double_stamp() {
        // The pipe reader hands over arbitrary 8 KB slices, so a line
        // routinely spans two writes. Only a real line start counts.
        let text = stamped(&[b"al", b"pha\nbe", b"ta\n"]);
        assert_eq!(shape(&text), "<T> alpha\n<T> beta\n");
    }

    #[test]
    fn carriage_return_starts_a_line_so_redraws_are_stamped() {
        let text = stamped(&[b"Downloading 1\rDownloading 2\r"]);
        assert_eq!(shape(&text), "<T> Downloading 1\r<T> Downloading 2\r");
    }

    #[test]
    fn crlf_stamps_once_not_twice() {
        // Windows cargo emits CRLF. Treating both bytes as line starts
        // would put an empty stamped line between every real one.
        let text = stamped(&[b"alpha\r\nbeta\r\n"]);
        assert_eq!(shape(&text), "<T> alpha\r\n<T> beta\r\n");
    }

    #[test]
    fn ansi_escapes_pass_through_untouched() {
        // Colour must survive: the prefix is only ever inserted at
        // column 0, so escapes inside the line are copied verbatim.
        let text = stamped(&[b"\x1b[32mgreen\x1b[0m\n"]);
        assert!(
            text.contains("\x1b[32mgreen\x1b[0m"),
            "escape sequence must survive verbatim, got {text:?}",
        );
        assert_eq!(shape(&text), "<T> \u{1b}[32mgreen\u{1b}[0m\n");
    }

    #[test]
    fn trailing_partial_line_is_stamped_once() {
        let text = stamped(&[b"partial"]);
        assert_eq!(shape(&text), "<T> partial");
    }

    #[test]
    fn diagnostic_lines_stay_unprefixed_for_line_anchored_filters() {
        // soldr#3850: `grep '^error'` on piped output must still match.
        let text = stamped(&[
            b"   Compiling demo v0.1.0\n",
            b"error[E0308]: mismatched types\n",
            b" --> src/lib.rs:1:5\n",
            b"warning: unused\nnote: x\nhelp: y\n",
            b"error: could not compile `demo`\n",
        ]);
        assert_eq!(
            shape(&text),
            "<T>    Compiling demo v0.1.0\n\
             error[E0308]: mismatched types\n \
             --> src/lib.rs:1:5\n\
             warning: unused\nnote: x\nhelp: y\n\
             error: could not compile `demo`\n",
        );
    }

    #[test]
    fn diagnostic_detection_survives_split_writes_and_colour() {
        let text = stamped(&[
            b"er",
            b"ror: boom\n",
            b"\x1b[1m\x1b[31merr",
            b"or\x1b[0m: red\n",
        ]);
        assert_eq!(
            shape(&text),
            "error: boom\n\u{1b}[1m\u{1b}[31merror\u{1b}[0m: red\n"
        );
    }

    #[test]
    fn diagnostic_lookalikes_are_still_stamped() {
        let text = stamped(&[b"errors everywhere\nhelper\nerror\n   |\n"]);
        assert_eq!(
            shape(&text),
            "<T> errors everywhere\n<T> helper\n<T> error\n<T>    |\n"
        );
    }

    #[test]
    fn a_held_partial_line_is_emitted_on_drop() {
        let text = stamped(&[b"err"]);
        assert_eq!(shape(&text), "<T> err");
    }

    #[test]
    fn timestamping_defaults_on_for_ci_and_off_for_a_terminal() {
        assert!(should_timestamp(None, false), "non-TTY (CI) defaults on");
        assert!(!should_timestamp(None, true), "TTY defaults off");
    }

    #[test]
    fn env_override_wins_in_both_directions() {
        for on in ["1", "true", "on", "ON", " true ", "yes", "YES"] {
            assert!(should_timestamp(Some(on), true), "{on:?} must force on");
        }
        for off in ["0", "false", "off", "OFF", "no", "NO", " no "] {
            assert!(
                !should_timestamp(Some(off), false),
                "{off:?} must force off"
            );
        }
    }

    #[test]
    fn unrecognised_env_value_falls_back_to_the_default() {
        // Garbage must not silently mean "off" on CI, where the default
        // is the useful one.
        assert!(should_timestamp(Some("yes-please"), false));
        assert!(should_timestamp(Some(""), false));
    }

    #[test]
    fn anchor_line_carries_the_absolute_epoch() {
        assert_eq!(
            epoch_anchor_line(1_784_950_000_123),
            "# t0=1784950000.123\n"
        );
        assert_eq!(epoch_anchor_line(1_000), "# t0=1.000\n");
    }

    #[test]
    fn anchor_is_skipped_where_the_runner_already_stamps_lines() {
        assert!(epoch_anchor_wanted(false));
        assert!(!epoch_anchor_wanted(true));
    }
}
