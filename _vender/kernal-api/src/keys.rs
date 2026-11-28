//! Small terminal-key contract, not a complete terminal emulator.
//!
//! UTF-8 characters, Enter, Escape and unambiguous legacy control/Alt characters are
//! decoded. Other escape sequences and bracketed paste produce `Other`, never
//! their embedded characters. Unbracketed paste is indistinguishable from typed
//! text. Releases are absent from byte protocols; Windows capture ignores them
//! and expands bounded repeats. Shift cannot be recovered from legacy bytes.
//! Ambiguous Alt encodings (including Alt+Space) are conservatively parsed as
//! escape sequences, not text keys; incomplete sequences fail closed.

use std::io;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Character(char),
    Enter,
    Escape,
    Other,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyModifiers {
    pub control: bool,
    pub alt: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: Key,
    pub modifiers: KeyModifiers,
}

#[derive(Default)]
enum State {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Utf8 {
        length: usize,
        alt: bool,
    },
    Csi,
    Ss3,
    Paste {
        matched: usize,
    },
    String {
        bell: bool,
        escape: bool,
    },
    Failed,
}

/// Incremental decoder with at most 128 buffered bytes and 64 KiB per control
/// string/paste. Errors poison the decoder: trailing input cannot become keys.
/// Callers own timing; [`TerminalKeys`] also bounds incomplete-sequence time.
#[derive(Default)]
pub struct KeyDecoder {
    state: State,
    pending: Vec<u8>,
    consumed: usize,
}

impl KeyDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    fn failure(&mut self) -> io::Error {
        self.state = State::Failed;
        self.pending.clear();
        io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed or oversized terminal key sequence",
        )
    }

    fn event(&mut self, key: Key, control: bool, alt: bool) -> Option<KeyEvent> {
        self.state = State::Ground;
        self.pending.clear();
        self.consumed = 0;
        Some(KeyEvent {
            key,
            modifiers: KeyModifiers { control, alt },
        })
    }

    fn character(&mut self, byte: u8, alt: bool) -> io::Result<Option<KeyEvent>> {
        let key = match byte {
            b'\r' | b'\n' => Key::Enter,
            1..=26 => {
                return Ok(self.event(Key::Character(char::from(b'a' + byte - 1)), true, alt))
            }
            32..=126 => Key::Character(char::from(byte)),
            0 | 28..=31 | 127 => Key::Other,
            0xc2..=0xf4 => {
                let length = if byte < 0xe0 {
                    2
                } else if byte < 0xf0 {
                    3
                } else {
                    4
                };
                self.pending.push(byte);
                self.state = State::Utf8 { length, alt };
                return Ok(None);
            }
            _ => return Err(self.failure()),
        };
        Ok(self.event(key, false, alt))
    }

    /// Feed one byte. `None` means a sequence is incomplete, not a key release.
    pub fn push(&mut self, byte: u8) -> io::Result<Option<KeyEvent>> {
        self.consumed += 1;
        if self.consumed > 65_536 {
            return Err(self.failure());
        }
        match self.state {
            State::Failed => Err(self.failure()),
            State::Ground if byte == 27 => {
                self.state = State::Escape;
                Ok(None)
            }
            State::Ground => self.character(byte, false),
            State::Escape => match byte {
                b'[' => {
                    self.state = State::Csi;
                    Ok(None)
                }
                b'O' => {
                    self.state = State::Ss3;
                    Ok(None)
                }
                27 => {
                    let event = self.event(Key::Escape, false, false);
                    self.state = State::Escape;
                    self.consumed = 1;
                    Ok(event)
                }
                b']' | b'P' | b'_' | b'^' | b'X' => {
                    self.state = State::String {
                        bell: byte == b']',
                        escape: false,
                    };
                    Ok(None)
                }
                0x20..=0x2f => {
                    self.state = State::EscapeIntermediate;
                    Ok(None)
                }
                _ => self.character(byte, true),
            },
            State::EscapeIntermediate => match byte {
                0x20..=0x2f if self.consumed <= 128 => Ok(None),
                0x30..=0x7e => Ok(self.event(Key::Other, false, false)),
                _ => Err(self.failure()),
            },
            State::Utf8 { length, alt } => {
                self.pending.push(byte);
                if self.pending.len() < length {
                    return Ok(None);
                }
                let character = std::str::from_utf8(&self.pending)
                    .ok()
                    .and_then(|text| text.chars().next());
                match character {
                    Some(character) => Ok(self.event(Key::Character(character), false, alt)),
                    None => Err(self.failure()),
                }
            }
            State::Csi | State::Ss3 => {
                let csi = matches!(self.state, State::Csi);
                if self.pending.len() >= 128 {
                    return Err(self.failure());
                }
                self.pending.push(byte);
                match byte {
                    0x20..=0x3f => Ok(None),
                    0x40..=0x7e if csi && self.pending == b"200~" => {
                        self.pending.clear();
                        self.state = State::Paste { matched: 0 };
                        Ok(None)
                    }
                    // X10 mouse packets have a raw payload after this final.
                    // They are unsupported, not standalone keys followed by text.
                    b'M' if csi && self.pending == b"M" => Err(self.failure()),
                    0x40..=0x7e => Ok(self.event(Key::Other, false, false)),
                    _ => Err(self.failure()),
                }
            }
            State::Paste { matched } => {
                let marker = b"\x1b[201~";
                let matched = if byte == marker[matched] {
                    matched + 1
                } else {
                    usize::from(byte == 27)
                };
                if matched == marker.len() {
                    return Ok(self.event(Key::Other, false, false));
                }
                self.state = State::Paste { matched };
                Ok(None)
            }
            State::String { bell, escape } => {
                if (bell && byte == 7) || (escape && byte == b'\\') {
                    return Ok(self.event(Key::Other, false, false));
                }
                self.state = State::String {
                    bell,
                    escape: byte == 27,
                };
                Ok(None)
            }
        }
    }

    /// Resolve a lone Escape after the caller's inter-byte wait. Other partial
    /// sequences are errors, so their trailing spaces can never escape parsing.
    pub fn finish_pending(&mut self) -> io::Result<Option<KeyEvent>> {
        match self.state {
            State::Ground => Ok(None),
            State::Escape => Ok(self.event(Key::Escape, false, false)),
            _ => Err(self.failure()),
        }
    }
}

/// Exclusive noncanonical, no-echo input owner. Existing native signal-key and
/// output processing are preserved; normally Ctrl+C remains a signal, not an
/// input event. Drop restores modes best-effort via the existing native session.
/// No additional background listener or channel is added.
pub struct TerminalKeys {
    input: crate::TerminalInputSession,
    decoder: KeyDecoder,
    bytes: Vec<u8>,
    cursor: usize,
    partial_since: Option<Instant>,
}

impl TerminalKeys {
    pub fn new() -> io::Result<Option<Self>> {
        crate::TerminalInputSession::new_for_keys().map(|input| {
            input.map(|input| Self {
                input,
                decoder: KeyDecoder::new(),
                bytes: Vec::new(),
                cursor: 0,
                partial_since: None,
            })
        })
    }

    /// Return one key with a maximum requested wait of 100 ms. A poll examines
    /// at most 64 KiB; queued bytes survive subsequent polls. Incomplete input
    /// expires after 250 ms, checked on each poll (a lone Escape becomes a key).
    pub fn poll(&mut self, wait: Duration) -> io::Result<Option<KeyEvent>> {
        if wait > Duration::from_millis(100) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal poll exceeds 100 ms",
            ));
        }
        if self.cursor == self.bytes.len()
            && self
                .partial_since
                .is_some_and(|start| start.elapsed() >= Duration::from_millis(250))
        {
            self.partial_since = None;
            return self.decoder.finish_pending();
        }
        let deadline = Instant::now() + wait;
        for _ in 0..65_536 {
            if self.cursor == self.bytes.len() {
                self.bytes.clear();
                self.cursor = 0;
                let Some(chunk) = self
                    .input
                    .read_chunk(deadline.saturating_duration_since(Instant::now()))?
                else {
                    return Ok(None);
                };
                self.bytes = chunk.data;
                if self.bytes.is_empty() {
                    return Ok(None);
                }
            }
            let byte = self.bytes[self.cursor];
            self.cursor += 1;
            match self.decoder.push(byte)? {
                Some(event) => {
                    self.partial_since =
                        (!matches!(self.decoder.state, State::Ground)).then(Instant::now);
                    return Ok(Some(event));
                }
                None => {
                    self.partial_since.get_or_insert_with(Instant::now);
                }
            }
        }
        Ok(None)
    }
}
