use crate::platform::terminal::InputQueue;
#[cfg(windows)]
use std::fs::OpenOptions;
#[cfg(windows)]
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use thiserror::Error;

/// Environment variable name for the trace file path.
pub const NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV: &str =
    "KERNAL_API_NATIVE_TERMINAL_INPUT_TRACE_PATH";

// ── Error type ──

/// Errors returned by native terminal input capture.
#[derive(Debug, Error)]
pub enum TerminalInputError {
    #[error("terminal capture failed: {0}")]
    Capture(#[from] TerminalCaptureFailure),
    /// Terminal input capture has already closed.
    #[error("terminal input is closed")]
    Closed,
    /// No terminal input arrived before the requested timeout.
    #[error("no terminal input available before timeout")]
    Timeout,
    /// An operating-system I/O operation failed.
    #[error("terminal input I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A terminal input error that does not fit a narrower variant.
    #[error("terminal input error: {0}")]
    Other(String),
}

// ── Pure-Rust data types ──

/// A translated terminal input event ready to forward to a PTY.
#[derive(Clone)]
pub struct TerminalInputEventRecord {
    /// Bytes to write to the PTY for this event.
    pub data: Vec<u8>,
    /// Whether this event represents an unmodified Enter submit action.
    pub submit: bool,
    /// Whether Shift was active when the event was captured.
    pub shift: bool,
    /// Whether Ctrl was active when the event was captured.
    pub ctrl: bool,
    /// Whether Alt was active when the event was captured.
    pub alt: bool,
    /// Virtual-key code for the source key event.
    pub virtual_key_code: u16,
    /// Number of key repeats represented by this event.
    pub repeat_count: u16,
}

/// Shared queue state for captured terminal input events.
pub struct TerminalInputState {
    /// Queued translated terminal input events.
    events: InputQueue<TerminalInputEventRecord>,
    failure: Option<TerminalCaptureFailure>,
    /// Whether the capture stream has been closed.
    pub closed: bool,
}

/// Capture stops and reports failure instead of silently dropping excess input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum TerminalCaptureFailure {
    #[error("terminal input exceeded queue or repeat limits")]
    Overflow,
    #[error("native terminal input failed")]
    Native,
}

#[cfg(windows)]
/// Windows console state saved while native input capture is active.
pub struct ActiveTerminalInputCapture {
    _input_lease: crate::platform::terminal::InputLease,
    /// Raw Windows console input handle as an integer.
    pub input_handle: usize,
    /// Console mode to restore when capture stops.
    pub original_mode: u32,
    /// Console mode installed while capture is active.
    pub active_mode: u32,
}

#[cfg(windows)]
/// Result of waiting for a Windows terminal input event.
#[derive(Debug, PartialEq)]
pub enum TerminalInputWaitOutcome {
    Failed(TerminalCaptureFailure),
    /// A translated input event was received.
    Event(TerminalInputEventRecord),
    /// Terminal input capture closed before an event arrived.
    Closed,
    /// No event arrived before the timeout.
    Timeout,
}

#[cfg(windows)]
impl std::fmt::Debug for TerminalInputEventRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalInputEventRecord")
            .field("data", &self.data)
            .field("submit", &self.submit)
            .field("shift", &self.shift)
            .field("ctrl", &self.ctrl)
            .field("alt", &self.alt)
            .field("virtual_key_code", &self.virtual_key_code)
            .field("repeat_count", &self.repeat_count)
            .finish()
    }
}

#[cfg(windows)]
impl PartialEq for TerminalInputEventRecord {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
            && self.submit == other.submit
            && self.shift == other.shift
            && self.ctrl == other.ctrl
            && self.alt == other.alt
            && self.virtual_key_code == other.virtual_key_code
            && self.repeat_count == other.repeat_count
    }
}

// ── Utility functions ──

/// Returns the current Unix timestamp as fractional seconds.
pub fn unix_now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(windows)]
/// Returns the configured native input trace target, if tracing is enabled.
pub fn native_terminal_input_trace_target() -> Option<String> {
    std::env::var(NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(windows)]
/// Appends one native input trace line to the configured target.
pub fn append_native_terminal_input_trace_line(line: &str) {
    let Some(target) = native_terminal_input_trace_target() else {
        return;
    };
    if target == "-" {
        eprintln!("{line}");
        return;
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&target) else {
        return;
    };
    let _ = writeln!(file, "{line}");
}

#[cfg(windows)]
/// Formats bytes as lowercase hexadecimal values for trace output.
pub fn format_terminal_input_bytes(data: &[u8]) -> String {
    if data.is_empty() {
        return "[]".to_string();
    }
    let parts: Vec<String> = data.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("[{}]", parts.join(" "))
}

// ── Console mode / key translation helpers ──

#[cfg(windows)]
/// Builds the Windows console mode used during native input capture.
pub fn native_terminal_input_mode(original_mode: u32) -> u32 {
    use winapi::um::wincon::{
        ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_QUICK_EDIT_MODE, ENABLE_WINDOW_INPUT,
    };

    (original_mode | ENABLE_EXTENDED_FLAGS | ENABLE_WINDOW_INPUT)
        & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT | ENABLE_QUICK_EDIT_MODE)
}

#[cfg(windows)]
/// Returns the VT modifier parameter for the active modifier keys.
pub fn terminal_input_modifier_parameter(shift: bool, alt: bool, ctrl: bool) -> Option<u8> {
    let value = 1 + u8::from(shift) + (u8::from(alt) * 2) + (u8::from(ctrl) * 4);
    (value > 1).then_some(value)
}

#[cfg(windows)]
/// Repeats translated bytes according to a Windows key repeat count.
pub fn repeat_terminal_input_bytes(chunk: &[u8], repeat_count: u16) -> Vec<u8> {
    let repeat = usize::from(repeat_count.max(1));
    let mut output = Vec::with_capacity(chunk.len() * repeat);
    for _ in 0..repeat {
        output.extend_from_slice(chunk);
    }
    output
}

#[cfg(windows)]
/// Adds a VT CSI modifier parameter to a base sequence when needed and repeats it.
pub fn repeated_modified_sequence(base: &[u8], modifier: Option<u8>, repeat_count: u16) -> Vec<u8> {
    if let Some(value) = modifier {
        let base_text = std::str::from_utf8(base).expect("VT sequence literal must be utf-8");
        let body = base_text
            .strip_prefix("\x1b[")
            .expect("VT sequence literal must start with CSI");
        let sequence = format!("\x1b[1;{value}{body}");
        repeat_terminal_input_bytes(sequence.as_bytes(), repeat_count)
    } else {
        repeat_terminal_input_bytes(base, repeat_count)
    }
}

#[cfg(windows)]
/// Builds and repeats a VT CSI tilde sequence with an optional modifier.
pub fn repeated_tilde_sequence(number: u8, modifier: Option<u8>, repeat_count: u16) -> Vec<u8> {
    if let Some(value) = modifier {
        let sequence = format!("\x1b[{number};{value}~");
        repeat_terminal_input_bytes(sequence.as_bytes(), repeat_count)
    } else {
        let sequence = format!("\x1b[{number}~");
        repeat_terminal_input_bytes(sequence.as_bytes(), repeat_count)
    }
}

#[cfg(windows)]
/// Maps a Unicode code unit to the corresponding Ctrl-key control byte.
pub fn control_character_for_unicode(unicode: u16) -> Option<u8> {
    let upper = char::from_u32(u32::from(unicode))?.to_ascii_uppercase();
    match upper {
        '@' | ' ' => Some(0x00),
        'A'..='Z' => Some((upper as u8) - b'@'),
        '[' => Some(0x1B),
        '\\' => Some(0x1C),
        ']' => Some(0x1D),
        '^' => Some(0x1E),
        '_' => Some(0x1F),
        _ => None,
    }
}

#[cfg(windows)]
/// Writes a trace record for a translated key event and returns the event unchanged.
pub(crate) fn trace_translated_console_key_event(
    record: &winapi::um::wincontypes::KEY_EVENT_RECORD,
    event: TerminalInputEventRecord,
) -> TerminalInputEventRecord {
    append_native_terminal_input_trace_line(&format!(
        "[{:.6}] native_terminal_input raw bKeyDown={} vk={:#06x} scan={:#06x} unicode={:#06x} control={:#010x} repeat={} translated bytes={} submit={} shift={} ctrl={} alt={}",
        unix_now_seconds(),
        record.bKeyDown,
        record.wVirtualKeyCode,
        record.wVirtualScanCode,
        unsafe { *record.uChar.UnicodeChar() },
        record.dwControlKeyState,
        record.wRepeatCount.max(1),
        format_terminal_input_bytes(&event.data),
        event.submit,
        event.shift,
        event.ctrl,
        event.alt,
    ));
    event
}

#[cfg(windows)]
/// Translates a Windows console key event into PTY input bytes.
#[expect(clippy::too_many_lines, reason = "Existing console-key translation is 221 lines")]
pub(crate) fn translate_console_key_event(
    record: &winapi::um::wincontypes::KEY_EVENT_RECORD,
) -> Option<TerminalInputEventRecord> {
    use winapi::um::wincontypes::{
        LEFT_ALT_PRESSED, LEFT_CTRL_PRESSED, RIGHT_ALT_PRESSED, RIGHT_CTRL_PRESSED, SHIFT_PRESSED,
    };
    use winapi::um::winuser::{
        VK_BACK, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_INSERT, VK_LEFT, VK_NEXT,
        VK_PRIOR, VK_RETURN, VK_RIGHT, VK_TAB, VK_UP,
    };

    if record.bKeyDown == 0 {
        append_native_terminal_input_trace_line(&format!(
            "[{:.6}] native_terminal_input raw bKeyDown=0 vk={:#06x} scan={:#06x} unicode={:#06x} control={:#010x} repeat={} translated=ignored",
            unix_now_seconds(),
            record.wVirtualKeyCode,
            record.wVirtualScanCode,
            unsafe { *record.uChar.UnicodeChar() },
            record.dwControlKeyState,
            record.wRepeatCount,
        ));
        return None;
    }

    let repeat_count = record.wRepeatCount.max(1);
    let modifiers = record.dwControlKeyState;
    let shift = modifiers & SHIFT_PRESSED != 0;
    let alt = modifiers & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0;
    let ctrl = modifiers & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0;
    let virtual_key_code = record.wVirtualKeyCode;
    let unicode = unsafe { *record.uChar.UnicodeChar() };

    // Shift+Enter: send CSI u escape sequence so downstream TUI apps
    // (e.g. Claude Code) can distinguish Shift+Enter (newline) from
    // plain Enter (submit).  Format: ESC [ 13 ; 2 u
    if shift && !ctrl && !alt && virtual_key_code as i32 == VK_RETURN {
        return Some(trace_translated_console_key_event(
            record,
            TerminalInputEventRecord {
                data: repeat_terminal_input_bytes(b"\x1b[13;2u", repeat_count),
                submit: false,
                shift,
                ctrl,
                alt,
                virtual_key_code,
                repeat_count,
            },
        ));
    }

    let mut data = if ctrl {
        control_character_for_unicode(unicode)
            .map(|byte| repeat_terminal_input_bytes(&[byte], repeat_count))
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    if data.is_empty() && unicode != 0 {
        if let Some(character) = char::from_u32(u32::from(unicode)) {
            let text: String = std::iter::repeat_n(character, usize::from(repeat_count)).collect();
            data = text.into_bytes();
        }
    }

    if data.is_empty() {
        let modifier = terminal_input_modifier_parameter(shift, alt, ctrl);
        let sequence = match virtual_key_code as i32 {
            VK_BACK => Some(b"\x08".as_slice()),
            VK_TAB if shift => Some(b"\x1b[Z".as_slice()),
            VK_TAB => Some(b"\t".as_slice()),
            VK_RETURN => Some(b"\r".as_slice()),
            VK_ESCAPE => Some(b"\x1b".as_slice()),
            VK_UP => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_modified_sequence(b"\x1b[A", modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_DOWN => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_modified_sequence(b"\x1b[B", modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_RIGHT => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_modified_sequence(b"\x1b[C", modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_LEFT => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_modified_sequence(b"\x1b[D", modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_HOME => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_modified_sequence(b"\x1b[H", modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_END => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_modified_sequence(b"\x1b[F", modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_INSERT => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_tilde_sequence(2, modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_DELETE => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_tilde_sequence(3, modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_PRIOR => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_tilde_sequence(5, modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_NEXT => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_tilde_sequence(6, modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            _ => None,
        };
        data = sequence.map(|chunk| repeat_terminal_input_bytes(chunk, repeat_count))?;
    }

    if alt && !data.starts_with(b"\x1b[") && !data.starts_with(b"\x1bO") {
        let mut prefixed = Vec::with_capacity(data.len() + 1);
        prefixed.push(0x1B);
        prefixed.extend_from_slice(&data);
        data = prefixed;
    }

    let event = TerminalInputEventRecord {
        data,
        submit: virtual_key_code as i32 == VK_RETURN && !shift,
        shift,
        ctrl,
        alt,
        virtual_key_code,
        repeat_count,
    };
    Some(trace_translated_console_key_event(record, event))
}

// ── Worker thread ──

#[cfg(windows)]
fn translate_captured_key_event(
    record: &winapi::um::wincon::KEY_EVENT_RECORD,
) -> Result<Option<TerminalInputEventRecord>, TerminalCaptureFailure> {
    // Releases carry no text. Ignore them before inspecting the repeat field.
    if record.bKeyDown == 0 {
        return Ok(None);
    }
    // Bound expansion before translation allocates repeated bytes.
    if record.wRepeatCount > 1024 {
        return Err(TerminalCaptureFailure::Overflow);
    }
    Ok(translate_console_key_event(record))
}

#[cfg(windows)]
/// Runs the Windows console input worker that queues translated key events.
pub fn native_terminal_input_worker(
    input_handle: usize,
    state: Arc<Mutex<TerminalInputState>>,
    condvar: Arc<Condvar>,
    stop: Arc<AtomicBool>,
    capturing: Arc<AtomicBool>,
) {
    use winapi::shared::minwindef::DWORD;
    use winapi::shared::winerror::WAIT_TIMEOUT;
    use winapi::um::consoleapi::ReadConsoleInputW;
    use winapi::um::synchapi::WaitForSingleObject;
    use winapi::um::winbase::WAIT_OBJECT_0;
    use winapi::um::wincontypes::{INPUT_RECORD, KEY_EVENT};
    use winapi::um::winnt::HANDLE;

    let handle = input_handle as HANDLE;
    let mut records: [INPUT_RECORD; 512] = unsafe { std::mem::zeroed() };
    append_native_terminal_input_trace_line(&format!(
        "[{:.6}] native_terminal_input worker_start handle={input_handle}",
        unix_now_seconds(),
    ));

    'capture: while !stop.load(Ordering::Acquire) {
        let wait_result = unsafe { WaitForSingleObject(handle, 50) };
        match wait_result {
            WAIT_OBJECT_0 => {
                let mut read_count: DWORD = 0;
                let ok = unsafe {
                    ReadConsoleInputW(
                        handle,
                        records.as_mut_ptr(),
                        records.len() as DWORD,
                        &mut read_count,
                    )
                };
                if ok == 0 {
                    state.lock().expect("terminal input mutex poisoned").failure =
                        Some(TerminalCaptureFailure::Native);
                    append_native_terminal_input_trace_line(&format!(
                        "[{:.6}] native_terminal_input read_console_input_failed handle={input_handle}",
                        unix_now_seconds(),
                    ));
                    break;
                }
                for record in records.iter().take(read_count as usize) {
                    if record.EventType != KEY_EVENT {
                        continue;
                    }
                    let key_event = unsafe { record.Event.KeyEvent() };
                    // Translation can write the optional diagnostic trace. Never
                    // hold the input-state mutex across that external I/O.
                    let translated = translate_captured_key_event(key_event);
                    let mut guard = state.lock().expect("terminal input mutex poisoned");
                    match translated {
                        Err(failure) => {
                            guard.failure = Some(failure);
                            break 'capture;
                        }
                        Ok(Some(event)) => {
                            let bytes = event.data.len();
                            if !guard.events.push(event, bytes) {
                                guard.failure = Some(TerminalCaptureFailure::Overflow);
                                break 'capture;
                            }
                            condvar.notify_all();
                        }
                        Ok(None) => {}
                    }
                }
            }
            WAIT_TIMEOUT => continue,
            _ => {
                state.lock().expect("terminal input mutex poisoned").failure =
                    Some(TerminalCaptureFailure::Native);
                append_native_terminal_input_trace_line(&format!(
                    "[{:.6}] native_terminal_input wait_result={wait_result} handle={input_handle}",
                    unix_now_seconds(),
                ));
                break;
            }
        }
    }

    capturing.store(false, Ordering::Release);
    let mut guard = state.lock().expect("terminal input mutex poisoned");
    guard.closed = true;
    if guard.failure.is_some() {
        guard.events.clear();
    }
    condvar.notify_all();
    drop(guard);
    append_native_terminal_input_trace_line(&format!(
        "[{:.6}] native_terminal_input worker_stop handle={input_handle}",
        unix_now_seconds(),
    ));
}

// ── Wait helper ──

#[cfg(windows)]
/// Waits for the next queued terminal input event, closure, or timeout.
pub fn wait_for_terminal_input_event(
    state: &Arc<Mutex<TerminalInputState>>,
    condvar: &Arc<Condvar>,
    timeout: Option<Duration>,
) -> TerminalInputWaitOutcome {
    let deadline = timeout.map(|limit| Instant::now() + limit);
    let mut guard = state.lock().expect("terminal input mutex poisoned");
    loop {
        if let Some(failure) = guard.failure {
            return TerminalInputWaitOutcome::Failed(failure);
        }
        if let Some(event) = guard.events.pop_front() {
            return TerminalInputWaitOutcome::Event(event);
        }
        if guard.closed {
            return TerminalInputWaitOutcome::Closed;
        }
        match deadline {
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    return TerminalInputWaitOutcome::Timeout;
                }
                let wait = deadline.saturating_duration_since(now);
                let result = condvar
                    .wait_timeout(guard, wait)
                    .expect("terminal input mutex poisoned");
                guard = result.0;
            }
            None => {
                guard = condvar.wait(guard).expect("terminal input mutex poisoned");
            }
        }
    }
}

// ── TerminalInputCore ──

/// Shared native terminal input capture core.
pub struct TerminalInputCore {
    /// Shared queue and closed flag for captured input.
    pub state: Arc<Mutex<TerminalInputState>>,
    /// Condition variable signaled when input state changes.
    pub condvar: Arc<Condvar>,
    /// Stop flag observed by the capture worker.
    pub stop: Arc<AtomicBool>,
    /// Whether native input capture is currently active.
    pub capturing: Arc<AtomicBool>,
    /// Worker thread handle for active capture.
    pub worker: Mutex<Option<thread::JoinHandle<()>>>,
    #[cfg(windows)]
    /// Saved Windows console capture state.
    pub console: Mutex<Option<ActiveTerminalInputCapture>>,
}

impl Default for TerminalInputCore {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalInputCore {
    pub fn supported() -> bool {
        true
    }

    /// Creates an idle terminal input core with capture stopped.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(TerminalInputState {
                events: InputQueue::new(),
                failure: None,
                closed: true,
            })),
            condvar: Arc::new(Condvar::new()),
            stop: Arc::new(AtomicBool::new(false)),
            capturing: Arc::new(AtomicBool::new(false)),
            worker: Mutex::new(None),
            #[cfg(windows)]
            console: Mutex::new(None),
        }
    }

    /// Pops the next queued terminal input event without blocking.
    pub fn next_event(&self) -> Option<TerminalInputEventRecord> {
        self.state
            .lock()
            .expect("terminal input mutex poisoned")
            .events
            .pop_front()
    }

    /// Returns whether at least one input event is queued.
    pub fn available(&self) -> bool {
        !self
            .state
            .lock()
            .expect("terminal input mutex poisoned")
            .events
            .is_empty()
    }

    /// Returns whether native terminal input capture is active.
    pub fn capturing(&self) -> bool {
        self.capturing.load(Ordering::Acquire)
    }

    /// Returns the saved original Windows console mode, if capture is active.
    pub fn original_console_mode(&self) -> Option<u32> {
        #[cfg(windows)]
        {
            return self
                .console
                .lock()
                .expect("terminal input console mutex poisoned")
                .as_ref()
                .map(|capture| capture.original_mode);
        }

        #[cfg(not(windows))]
        {
            None
        }
    }

    /// Returns the active Windows console mode, if capture is active.
    pub fn active_console_mode(&self) -> Option<u32> {
        #[cfg(windows)]
        {
            return self
                .console
                .lock()
                .expect("terminal input console mutex poisoned")
                .as_ref()
                .map(|capture| capture.active_mode);
        }

        #[cfg(not(windows))]
        {
            None
        }
    }

    /// Blocks until an input event, closure, or optional timeout.
    /// Wait at most `timeout` for the next event, returning
    /// [`TerminalInputError::Timeout`] on expiry.
    // Delegates to the deprecated optional form until it is removed (#433).
    #[allow(deprecated)]
    pub fn wait_for_event_within(
        &self,
        timeout: Duration,
    ) -> Result<TerminalInputEventRecord, TerminalInputError> {
        self.wait_for_event(Some(timeout.as_secs_f64()))
    }

    #[deprecated(note = "`None` waits forever; use `wait_for_event_within` (#433)")]
    pub fn wait_for_event(
        &self,
        timeout: Option<f64>,
    ) -> Result<TerminalInputEventRecord, TerminalInputError> {
        let state = Arc::clone(&self.state);
        let condvar = Arc::clone(&self.condvar);
        let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs_f64(secs));
        let mut guard = state.lock().expect("terminal input mutex poisoned");
        loop {
            if let Some(failure) = guard.failure {
                return Err(TerminalInputError::Capture(failure));
            }
            if let Some(event) = guard.events.pop_front() {
                return Ok(event);
            }
            if guard.closed {
                return Err(TerminalInputError::Closed);
            }
            match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(TerminalInputError::Timeout);
                    }
                    let wait = deadline.saturating_duration_since(now);
                    let result = condvar
                        .wait_timeout(guard, wait)
                        .expect("terminal input mutex poisoned");
                    guard = result.0;
                }
                None => {
                    guard = condvar.wait(guard).expect("terminal input mutex poisoned");
                }
            }
        }
    }

    /// Drains all queued terminal input events.
    pub fn drain_events(&self) -> Vec<TerminalInputEventRecord> {
        let mut guard = self.state.lock().expect("terminal input mutex poisoned");
        guard.events.drain().collect()
    }

    /// Stops native terminal input capture and restores console state.
    pub fn stop_impl(&self) -> Result<(), std::io::Error> {
        #[cfg(windows)]
        append_native_terminal_input_trace_line(&format!(
            "[{:.6}] native_terminal_input stop_requested",
            unix_now_seconds(),
        ));
        let mut worker_guard = self
            .worker
            .lock()
            .expect("terminal input worker mutex poisoned");
        self.stop.store(true, Ordering::Release);
        // Serialize restart until the old worker exits and its mode is restored.
        if let Some(worker) = worker_guard.take() {
            let _ = worker.join();
        }
        self.capturing.store(false, Ordering::Release);

        #[cfg(windows)]
        let restore_result = {
            use winapi::um::consoleapi::SetConsoleMode;
            use winapi::um::winnt::HANDLE;

            let console = self
                .console
                .lock()
                .expect("terminal input console mutex poisoned")
                .take();
            console.map(|capture| unsafe {
                SetConsoleMode(capture.input_handle as HANDLE, capture.original_mode)
            })
        };

        let mut guard = self.state.lock().expect("terminal input mutex poisoned");
        guard.closed = true;
        self.condvar.notify_all();
        drop(guard);

        #[cfg(windows)]
        if let Some(result) = restore_result {
            if result == 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    /// Starts native terminal input capture for the attached Windows console.
    pub fn start_impl(&self) -> Result<(), std::io::Error> {
        self.start_with_signal_keys(false)
    }

    #[cfg(feature = "terminal-input")]
    pub(crate) fn start_for_keys(&self) -> Result<(), std::io::Error> {
        self.start_with_signal_keys(true)
    }

    fn start_with_signal_keys(&self, preserve_signal_keys: bool) -> Result<(), std::io::Error> {
        use winapi::um::consoleapi::{GetConsoleMode, SetConsoleMode};
        use winapi::um::handleapi::INVALID_HANDLE_VALUE;
        use winapi::um::processenv::GetStdHandle;
        use winapi::um::winbase::STD_INPUT_HANDLE;

        let mut worker_guard = self
            .worker
            .lock()
            .expect("terminal input worker mutex poisoned");
        if worker_guard.is_some() {
            return Ok(());
        }

        let input_handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if input_handle.is_null() || input_handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }

        let input_lease = crate::platform::terminal::InputLease::acquire()?;
        let mut original_mode = 0u32;
        let got_mode = unsafe { GetConsoleMode(input_handle, &mut original_mode) };
        if got_mode == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "TerminalInputCore requires an attached Windows console stdin",
            ));
        }

        let mut active_mode = native_terminal_input_mode(original_mode);
        if preserve_signal_keys {
            active_mode |= original_mode & winapi::um::wincon::ENABLE_PROCESSED_INPUT;
        }
        let set_mode = unsafe { SetConsoleMode(input_handle, active_mode) };
        if set_mode == 0 {
            return Err(std::io::Error::last_os_error());
        }
        append_native_terminal_input_trace_line(&format!(
            "[{:.6}] native_terminal_input start handle={} original_mode={:#010x} active_mode={:#010x}",
            unix_now_seconds(),
            input_handle as usize,
            original_mode,
            active_mode,
        ));

        self.stop.store(false, Ordering::Release);
        self.capturing.store(true, Ordering::Release);
        {
            let mut state = self.state.lock().expect("terminal input mutex poisoned");
            state.events.clear();
            state.failure = None;
            state.closed = false;
        }
        *self
            .console
            .lock()
            .expect("terminal input console mutex poisoned") = Some(ActiveTerminalInputCapture {
            _input_lease: input_lease,
            input_handle: input_handle as usize,
            original_mode,
            active_mode,
        });

        let state = Arc::clone(&self.state);
        let condvar = Arc::clone(&self.condvar);
        let stop = Arc::clone(&self.stop);
        let capturing = Arc::clone(&self.capturing);
        let input_handle_raw = input_handle as usize;
        *worker_guard = Some(thread::spawn(move || {
            native_terminal_input_worker(input_handle_raw, state, condvar, stop, capturing);
        }));
        Ok(())
    }
}

impl Drop for TerminalInputCore {
    fn drop(&mut self) {
        let _ = self.stop_impl();
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use winapi::um::wincon::{
        ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_QUICK_EDIT_MODE, ENABLE_WINDOW_INPUT,
    };
    use winapi::um::wincontypes::{
        KEY_EVENT_RECORD, LEFT_ALT_PRESSED, LEFT_CTRL_PRESSED, SHIFT_PRESSED,
    };
    use winapi::um::winuser::{VK_RETURN, VK_TAB, VK_UP};

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_locked_env_var<T>(
        key: &'static str,
        value: Option<&str>,
        f: impl FnOnce() -> T + std::panic::UnwindSafe,
    ) -> T {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os(key);
        unsafe {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        let result = std::panic::catch_unwind(f);
        unsafe {
            match previous {
                Some(previous) => std::env::set_var(key, previous),
                None => std::env::remove_var(key),
            }
        }
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

pub(crate) fn key_event(
    virtual_key_code: u16,
    unicode: u16,
    control_key_state: u32,
    repeat_count: u16,
) -> KEY_EVENT_RECORD {
    let mut event: KEY_EVENT_RECORD = unsafe { std::mem::zeroed() };
    event.bKeyDown = 1;
    event.wRepeatCount = repeat_count;
    event.wVirtualKeyCode = virtual_key_code;
    event.wVirtualScanCode = 0;
    event.dwControlKeyState = control_key_state;
    unsafe {
        *event.uChar.UnicodeChar_mut() = unicode;
    }
    event
}

#[test]
fn capture_repeat_limit_precedes_translation_but_ignores_releases() {
    let mut record = key_event(0, b' ' as u16, 0, 1025);
    record.bKeyDown = 0;
    assert_eq!(translate_captured_key_event(&record), Ok(None));
    record.bKeyDown = 1;
    assert_eq!(translate_captured_key_event(&record), Err(TerminalCaptureFailure::Overflow));
    record.wRepeatCount = 1024;
    assert_eq!(translate_captured_key_event(&record).unwrap().unwrap().data, vec![b' '; 1024]);
}

#[test]
fn capture_queue_overflow_is_reported_before_queued_input() {
    let core = TerminalInputCore::new();
    {
        let mut state = core.state.lock().unwrap();
        let record = key_event(0, b' ' as u16, 0, 1);
        for _ in 0..256 {
            let event = translate_captured_key_event(&record).unwrap().unwrap();
            assert!(state.events.push(event, 1));
        }
        let event = translate_captured_key_event(&record).unwrap().unwrap();
        assert!(!state.events.push(event, 1));
        state.failure = Some(TerminalCaptureFailure::Overflow);
        assert_eq!(state.failure, Some(TerminalCaptureFailure::Overflow));
    }
    assert_eq!(
        wait_for_terminal_input_event(&core.state, &core.condvar, Some(Duration::ZERO)),
        TerminalInputWaitOutcome::Failed(TerminalCaptureFailure::Overflow)
    );
    assert!(matches!(core.wait_for_event_within(Duration::ZERO), Err(TerminalInputError::Capture(TerminalCaptureFailure::Overflow))));
}

#[test]
fn native_terminal_input_mode_disables_cooked_console_flags() {
    let original_mode =
        ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT | ENABLE_QUICK_EDIT_MODE;

    let active_mode = native_terminal_input_mode(original_mode);

    assert_eq!(active_mode & ENABLE_ECHO_INPUT, 0);
    assert_eq!(active_mode & ENABLE_LINE_INPUT, 0);
    assert_eq!(active_mode & ENABLE_PROCESSED_INPUT, 0);
    assert_eq!(active_mode & ENABLE_QUICK_EDIT_MODE, 0);
    assert_ne!(active_mode & ENABLE_EXTENDED_FLAGS, 0);
    assert_ne!(active_mode & ENABLE_WINDOW_INPUT, 0);
}

#[test]
fn translate_terminal_input_preserves_submit_hint_for_enter() {
    let event = translate_console_key_event(&key_event(VK_RETURN as u16, '\r' as u16, 0, 1))
        .expect("enter should translate");
    assert_eq!(event.data, b"\r");
    assert!(event.submit);
}

#[test]
fn translate_terminal_input_keeps_shift_enter_non_submit() {
    let event =
        translate_console_key_event(&key_event(VK_RETURN as u16, '\r' as u16, SHIFT_PRESSED, 1))
            .expect("shift-enter should translate");
    // Shift+Enter emits CSI u sequence so downstream apps can
    // distinguish it from plain Enter.
    assert_eq!(event.data, b"\x1b[13;2u");
    assert!(!event.submit);
    assert!(event.shift);
}

#[test]
fn translate_terminal_input_encodes_shift_tab() {
    let event = translate_console_key_event(&key_event(VK_TAB as u16, 0, SHIFT_PRESSED, 1))
        .expect("shift-tab should translate");
    assert_eq!(event.data, b"\x1b[Z");
    assert!(!event.submit);
}

#[test]
fn translate_terminal_input_encodes_modified_arrows() {
    let event = translate_console_key_event(&key_event(
        VK_UP as u16,
        0,
        SHIFT_PRESSED | LEFT_CTRL_PRESSED,
        1,
    ))
    .expect("modified arrow should translate");
    assert_eq!(event.data, b"\x1b[1;6A");
}

#[test]
fn translate_terminal_input_encodes_alt_printable_with_escape_prefix() {
    let event =
        translate_console_key_event(&key_event(b'X' as u16, 'x' as u16, LEFT_ALT_PRESSED, 1))
            .expect("alt printable should translate");
    assert_eq!(event.data, b"\x1bx");
}

#[test]
fn translate_terminal_input_encodes_ctrl_printable_as_control_character() {
    let event =
        translate_console_key_event(&key_event(b'C' as u16, 'c' as u16, LEFT_CTRL_PRESSED, 1))
            .expect("ctrl-c should translate");
    assert_eq!(event.data, [0x03]);
}

#[test]
fn translate_terminal_input_ignores_keyup_events() {
    let mut event = key_event(VK_RETURN as u16, '\r' as u16, 0, 1);
    event.bKeyDown = 0;
    assert!(translate_console_key_event(&event).is_none());
}

#[test]
fn terminal_input_modifier_none() {
    assert!(terminal_input_modifier_parameter(false, false, false).is_none());
}

#[test]
fn terminal_input_modifier_shift() {
    assert_eq!(
        terminal_input_modifier_parameter(true, false, false),
        Some(2)
    );
}

#[test]
fn terminal_input_modifier_alt() {
    assert_eq!(
        terminal_input_modifier_parameter(false, true, false),
        Some(3)
    );
}

#[test]
fn terminal_input_modifier_ctrl() {
    assert_eq!(
        terminal_input_modifier_parameter(false, false, true),
        Some(5)
    );
}

#[test]
fn terminal_input_modifier_shift_ctrl() {
    assert_eq!(
        terminal_input_modifier_parameter(true, false, true),
        Some(6)
    );
}

#[test]
fn control_character_for_unicode_letters() {
    assert_eq!(control_character_for_unicode('A' as u16), Some(0x01));
    assert_eq!(control_character_for_unicode('C' as u16), Some(0x03));
    assert_eq!(control_character_for_unicode('Z' as u16), Some(0x1A));
}

#[test]
fn control_character_for_unicode_special() {
    assert_eq!(control_character_for_unicode('@' as u16), Some(0x00));
    assert_eq!(control_character_for_unicode('[' as u16), Some(0x1B));
}

#[test]
fn control_character_for_unicode_digit_returns_none() {
    assert!(control_character_for_unicode('1' as u16).is_none());
}

#[test]
fn format_terminal_input_bytes_empty() {
    assert_eq!(format_terminal_input_bytes(b""), "[]");
}

#[test]
fn format_terminal_input_bytes_multi() {
    assert_eq!(format_terminal_input_bytes(&[0x41, 0x42]), "[41 42]");
}

#[test]
fn repeated_tilde_sequence_no_modifier() {
    assert_eq!(repeated_tilde_sequence(3, None, 1), b"\x1b[3~");
}

#[test]
fn repeated_tilde_sequence_with_modifier() {
    assert_eq!(repeated_tilde_sequence(3, Some(2), 1), b"\x1b[3;2~");
}

#[test]
fn repeated_tilde_sequence_repeated() {
    let result = repeated_tilde_sequence(3, None, 3);
    assert_eq!(result, b"\x1b[3~\x1b[3~\x1b[3~");
}

#[test]
fn repeated_modified_sequence_no_modifier() {
    let result = repeated_modified_sequence(b"\x1b[A", None, 1);
    assert_eq!(result, b"\x1b[A");
}

#[test]
fn repeated_modified_sequence_with_modifier() {
    // Shift modifier (2) applied to Up arrow
    let result = repeated_modified_sequence(b"\x1b[A", Some(2), 1);
    assert_eq!(result, b"\x1b[1;2A");
}

#[test]
fn repeated_modified_sequence_repeated() {
    let result = repeated_modified_sequence(b"\x1b[A", None, 2);
    assert_eq!(result, b"\x1b[A\x1b[A");
}

#[test]
fn repeat_terminal_input_bytes_single() {
    let result = repeat_terminal_input_bytes(b"\r", 1);
    assert_eq!(result, b"\r");
}

#[test]
fn repeat_terminal_input_bytes_multiple() {
    let result = repeat_terminal_input_bytes(b"ab", 3);
    assert_eq!(result, b"ababab");
}

#[test]
fn repeat_terminal_input_bytes_zero_clamps_to_one() {
    let result = repeat_terminal_input_bytes(b"x", 0);
    assert_eq!(result, b"x");
}

// ── B1: Windows Console Key Translation (navigation keys) ──

#[test]
fn translate_console_key_home() {
    use winapi::um::winuser::VK_HOME;
    let event = translate_console_key_event(&key_event(VK_HOME as u16, 0, 0, 1))
        .expect("VK_HOME should translate");
    assert_eq!(event.data, b"\x1b[H");
    assert!(!event.submit);
}

#[test]
fn translate_console_key_end() {
    use winapi::um::winuser::VK_END;
    let event = translate_console_key_event(&key_event(VK_END as u16, 0, 0, 1))
        .expect("VK_END should translate");
    assert_eq!(event.data, b"\x1b[F");
    assert!(!event.submit);
}

#[test]
fn translate_console_key_insert() {
    use winapi::um::winuser::VK_INSERT;
    let event = translate_console_key_event(&key_event(VK_INSERT as u16, 0, 0, 1))
        .expect("VK_INSERT should translate");
    assert_eq!(event.data, b"\x1b[2~");
    assert!(!event.submit);
}

#[test]
fn translate_console_key_delete() {
    use winapi::um::winuser::VK_DELETE;
    let event = translate_console_key_event(&key_event(VK_DELETE as u16, 0, 0, 1))
        .expect("VK_DELETE should translate");
    assert_eq!(event.data, b"\x1b[3~");
    assert!(!event.submit);
}

#[test]
fn translate_console_key_page_up() {
    use winapi::um::winuser::VK_PRIOR;
    let event = translate_console_key_event(&key_event(VK_PRIOR as u16, 0, 0, 1))
        .expect("VK_PRIOR should translate");
    assert_eq!(event.data, b"\x1b[5~");
    assert!(!event.submit);
}

#[test]
fn translate_console_key_page_down() {
    use winapi::um::winuser::VK_NEXT;
    let event = translate_console_key_event(&key_event(VK_NEXT as u16, 0, 0, 1))
        .expect("VK_NEXT should translate");
    assert_eq!(event.data, b"\x1b[6~");
    assert!(!event.submit);
}

#[test]
fn translate_console_key_shift_home() {
    use winapi::um::winuser::VK_HOME;
    let event = translate_console_key_event(&key_event(VK_HOME as u16, 0, SHIFT_PRESSED, 1))
        .expect("Shift+Home should translate");
    assert_eq!(event.data, b"\x1b[1;2H");
    assert!(event.shift);
}

#[test]
fn translate_console_key_shift_end() {
    use winapi::um::winuser::VK_END;
    let event = translate_console_key_event(&key_event(VK_END as u16, 0, SHIFT_PRESSED, 1))
        .expect("Shift+End should translate");
    assert_eq!(event.data, b"\x1b[1;2F");
    assert!(event.shift);
}

#[test]
fn translate_console_key_ctrl_home() {
    use winapi::um::winuser::VK_HOME;
    let event = translate_console_key_event(&key_event(VK_HOME as u16, 0, LEFT_CTRL_PRESSED, 1))
        .expect("Ctrl+Home should translate");
    assert_eq!(event.data, b"\x1b[1;5H");
    assert!(event.ctrl);
}

#[test]
fn translate_console_key_shift_delete() {
    use winapi::um::winuser::VK_DELETE;
    let event = translate_console_key_event(&key_event(VK_DELETE as u16, 0, SHIFT_PRESSED, 1))
        .expect("Shift+Delete should translate");
    assert_eq!(event.data, b"\x1b[3;2~");
    assert!(event.shift);
}

#[test]
fn translate_console_key_ctrl_page_up() {
    use winapi::um::winuser::VK_PRIOR;
    let event = translate_console_key_event(&key_event(VK_PRIOR as u16, 0, LEFT_CTRL_PRESSED, 1))
        .expect("Ctrl+PageUp should translate");
    assert_eq!(event.data, b"\x1b[5;5~");
    assert!(event.ctrl);
}

#[test]
fn translate_console_key_backspace() {
    use winapi::um::winuser::VK_BACK;
    let event = translate_console_key_event(&key_event(VK_BACK as u16, 0x08, 0, 1))
        .expect("Backspace should translate");
    assert_eq!(event.data, b"\x08");
}

#[test]
fn translate_console_key_escape() {
    use winapi::um::winuser::VK_ESCAPE;
    let event = translate_console_key_event(&key_event(VK_ESCAPE as u16, 0x1b, 0, 1))
        .expect("Escape should translate");
    assert_eq!(event.data, b"\x1b");
}

#[test]
fn translate_console_key_tab() {
    let event = translate_console_key_event(&key_event(VK_TAB as u16, 0, 0, 1))
        .expect("Tab should translate");
    assert_eq!(event.data, b"\t");
}

#[test]
fn translate_console_key_plain_enter_is_submit() {
    let event = translate_console_key_event(&key_event(VK_RETURN as u16, '\r' as u16, 0, 1))
        .expect("Enter should translate");
    assert_eq!(event.data, b"\r");
    assert!(event.submit);
    assert!(!event.shift);
}

#[test]
fn translate_console_key_unicode_printable() {
    // Regular 'a' key
    let event = translate_console_key_event(&key_event(b'A' as u16, 'a' as u16, 0, 1))
        .expect("printable should translate");
    assert_eq!(event.data, b"a");
}

#[test]
fn translate_console_key_unicode_repeated() {
    let event = translate_console_key_event(&key_event(b'A' as u16, 'a' as u16, 0, 3))
        .expect("repeated printable should translate");
    assert_eq!(event.data, b"aaa");
}

#[test]
fn translate_console_key_down_arrow() {
    use winapi::um::winuser::VK_DOWN;
    let event = translate_console_key_event(&key_event(VK_DOWN as u16, 0, 0, 1))
        .expect("Down arrow should translate");
    assert_eq!(event.data, b"\x1b[B");
}

#[test]
fn translate_console_key_right_arrow() {
    use winapi::um::winuser::VK_RIGHT;
    let event = translate_console_key_event(&key_event(VK_RIGHT as u16, 0, 0, 1))
        .expect("Right arrow should translate");
    assert_eq!(event.data, b"\x1b[C");
}

#[test]
fn translate_console_key_left_arrow() {
    use winapi::um::winuser::VK_LEFT;
    let event = translate_console_key_event(&key_event(VK_LEFT as u16, 0, 0, 1))
        .expect("Left arrow should translate");
    assert_eq!(event.data, b"\x1b[D");
}

#[test]
fn translate_console_key_unknown_vk_no_unicode_returns_none() {
    // Unknown VK with no unicode char → should return None
    let result = translate_console_key_event(&key_event(0xFF, 0, 0, 1));
    assert!(result.is_none());
}

#[test]
fn translate_console_key_alt_escape_prefix() {
    // Alt+letter should prepend ESC byte to the character
    let event =
        translate_console_key_event(&key_event(b'A' as u16, 'a' as u16, LEFT_ALT_PRESSED, 1))
            .expect("Alt+a should translate");
    assert_eq!(event.data, b"\x1ba");
    assert!(event.alt);
}

#[test]
fn translate_console_key_ctrl_a() {
    let event =
        translate_console_key_event(&key_event(b'A' as u16, 'a' as u16, LEFT_CTRL_PRESSED, 1))
            .expect("Ctrl+A should translate");
    assert_eq!(event.data, [0x01]); // SOH
    assert!(event.ctrl);
}

#[test]
fn translate_console_key_ctrl_z() {
    let event =
        translate_console_key_event(&key_event(b'Z' as u16, 'z' as u16, LEFT_CTRL_PRESSED, 1))
            .expect("Ctrl+Z should translate");
    assert_eq!(event.data, [0x1A]); // SUB
    assert!(event.ctrl);
}

mod windows_additional_tests {
    use super::*;
    use winapi::um::winuser::VK_F1;

    // ── control_character_for_unicode tests ──

    #[test]
    fn control_char_at_sign() {
        assert_eq!(control_character_for_unicode('@' as u16), Some(0x00));
    }

    #[test]
    fn control_char_space() {
        assert_eq!(control_character_for_unicode(' ' as u16), Some(0x00));
    }

    #[test]
    fn control_char_a() {
        assert_eq!(control_character_for_unicode('a' as u16), Some(0x01));
    }

    #[test]
    fn control_char_z() {
        assert_eq!(control_character_for_unicode('z' as u16), Some(0x1A));
    }

    #[test]
    fn control_char_bracket() {
        assert_eq!(control_character_for_unicode('[' as u16), Some(0x1B));
    }

    #[test]
    fn control_char_backslash() {
        assert_eq!(control_character_for_unicode('\\' as u16), Some(0x1C));
    }

    #[test]
    fn control_char_close_bracket() {
        assert_eq!(control_character_for_unicode(']' as u16), Some(0x1D));
    }

    #[test]
    fn control_char_caret() {
        assert_eq!(control_character_for_unicode('^' as u16), Some(0x1E));
    }

    #[test]
    fn control_char_underscore() {
        assert_eq!(control_character_for_unicode('_' as u16), Some(0x1F));
    }

    #[test]
    fn control_char_digit_returns_none() {
        assert_eq!(control_character_for_unicode('0' as u16), None);
    }

    #[test]
    fn control_char_exclamation_returns_none() {
        assert_eq!(control_character_for_unicode('!' as u16), None);
    }

    // ── terminal_input_modifier_parameter tests ──

    #[test]
    fn modifier_param_no_modifiers_returns_none() {
        assert_eq!(terminal_input_modifier_parameter(false, false, false), None);
    }

    #[test]
    fn modifier_param_shift_only() {
        assert_eq!(
            terminal_input_modifier_parameter(true, false, false),
            Some(2)
        );
    }

    #[test]
    fn modifier_param_alt_only() {
        assert_eq!(
            terminal_input_modifier_parameter(false, true, false),
            Some(3)
        );
    }

    #[test]
    fn modifier_param_ctrl_only() {
        assert_eq!(
            terminal_input_modifier_parameter(false, false, true),
            Some(5)
        );
    }

    #[test]
    fn modifier_param_shift_ctrl() {
        assert_eq!(
            terminal_input_modifier_parameter(true, false, true),
            Some(6)
        );
    }

    #[test]
    fn modifier_param_shift_alt() {
        assert_eq!(
            terminal_input_modifier_parameter(true, true, false),
            Some(4)
        );
    }

    #[test]
    fn modifier_param_all_modifiers() {
        assert_eq!(terminal_input_modifier_parameter(true, true, true), Some(8));
    }

    // ── repeated_tilde_sequence tests ──

    #[test]
    fn tilde_sequence_no_modifier() {
        let result = repeated_tilde_sequence(3, None, 1);
        assert_eq!(result, b"\x1b[3~");
    }

    #[test]
    fn tilde_sequence_with_modifier() {
        let result = repeated_tilde_sequence(3, Some(2), 1);
        assert_eq!(result, b"\x1b[3;2~");
    }

    #[test]
    fn tilde_sequence_repeated() {
        let result = repeated_tilde_sequence(3, None, 3);
        assert_eq!(result, b"\x1b[3~\x1b[3~\x1b[3~");
    }

    // ── repeated_modified_sequence tests ──

    #[test]
    fn modified_sequence_no_modifier() {
        let result = repeated_modified_sequence(b"\x1b[A", None, 1);
        assert_eq!(result, b"\x1b[A");
    }

    #[test]
    fn modified_sequence_with_modifier() {
        let result = repeated_modified_sequence(b"\x1b[A", Some(2), 1);
        assert_eq!(result, b"\x1b[1;2A");
    }

    #[test]
    fn modified_sequence_repeated_with_modifier() {
        let result = repeated_modified_sequence(b"\x1b[A", Some(5), 2);
        assert_eq!(result, b"\x1b[1;5A\x1b[1;5A");
    }

    // ── format_terminal_input_bytes tests ──

    #[test]
    fn format_bytes_empty() {
        assert_eq!(format_terminal_input_bytes(&[]), "[]");
    }

    #[test]
    fn format_bytes_multiple() {
        assert_eq!(
            format_terminal_input_bytes(&[0x1B, 0x5B, 0x41]),
            "[1b 5b 41]"
        );
    }

    // ── native_terminal_input_trace_target tests ──

    #[test]
    fn trace_target_empty_env_returns_none() {
        with_locked_env_var(NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV, None, || {
            assert!(native_terminal_input_trace_target().is_none());
        });
    }

    #[test]
    fn trace_target_whitespace_env_returns_none() {
        with_locked_env_var(NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV, Some("   "), || {
            assert!(native_terminal_input_trace_target().is_none());
        });
    }

    #[test]
    fn trace_target_valid_env_returns_value() {
        with_locked_env_var(
            NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV,
            Some("/tmp/trace.log"),
            || {
                let result = native_terminal_input_trace_target();
                assert_eq!(result, Some("/tmp/trace.log".to_string()));
            },
        );
    }

    // ── translate_console_key_event: key-up ignored ──

    #[test]
    fn translate_key_up_event_returns_none() {
        let mut event: KEY_EVENT_RECORD = unsafe { std::mem::zeroed() };
        event.bKeyDown = 0;
        event.wVirtualKeyCode = VK_RETURN as u16;
        let result = translate_console_key_event(&event);
        assert!(result.is_none());
    }

    // ── translate: F1 returns None (unknown key) ──

    #[test]
    fn translate_f1_key_returns_none() {
        let event = key_event(VK_F1 as u16, 0, 0, 1);
        let result = translate_console_key_event(&event);
        assert!(result.is_none());
    }

    // ── translate: alt prefix ──

    #[test]
    fn translate_alt_a_has_escape_prefix() {
        let event = key_event('a' as u16, 'a' as u16, LEFT_ALT_PRESSED, 1);
        let result = translate_console_key_event(&event).unwrap();
        assert!(result.data.starts_with(b"\x1b"));
        assert!(result.alt);
    }

    // ── translate: Ctrl+character ──

    #[test]
    fn translate_ctrl_c_produces_etx() {
        let event = key_event('C' as u16, 'c' as u16, LEFT_CTRL_PRESSED, 1);
        let result = translate_console_key_event(&event).unwrap();
        assert_eq!(result.data, &[0x03]);
        assert!(result.ctrl);
    }
}


}
