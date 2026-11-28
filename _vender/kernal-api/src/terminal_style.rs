//! Foreground formatting without environment, color-choice or warning policy.
//! Callers choose whether to emit colors. Text is preserved verbatim, including
//! embedded escapes; this formatter is not a terminal-output sanitizer.

use std::{fmt, io};

/// Named colors in the ANSI 16-color palette (bright names match diagnostics).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Foreground {
    Black = 0,
    DarkRed = 1,
    DarkGreen = 2,
    DarkYellow = 3,
    DarkBlue = 4,
    DarkMagenta = 5,
    DarkCyan = 6,
    Grey = 7,
    DarkGrey = 8,
    Red = 9,
    Green = 10,
    Yellow = 11,
    Blue = 12,
    Magenta = 13,
    Cyan = 14,
    White = 15,
}

/// Borrowed, allocation-free text wrapper. Successful colored formatting
/// restores the default foreground, not a previously active foreground. If the
/// writer fails, its error is propagated; a reset cannot then be guaranteed.
#[derive(Clone, Copy, Debug)]
pub struct StyledText<'a> {
    text: &'a str,
    foreground: Foreground,
    enabled: bool,
}

impl<'a> StyledText<'a> {
    pub fn new(text: &'a str, foreground: Foreground, enabled: bool) -> Self {
        Self {
            text,
            foreground,
            enabled,
        }
    }
}

impl fmt::Display for StyledText<'_> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.enabled {
            write!(output, "\x1b[38;5;{}m", self.foreground as u8)?;
        }
        output.write_str(self.text)?;
        if self.enabled {
            output.write_str("\x1b[39m")?;
        }
        Ok(())
    }
}

/// Prepare stderr's native console to interpret ANSI output, without deciding
/// color policy. Unix needs no preparation and returns true; this is not a TTY
/// or TERM probe. Windows returns false for redirected/non-console handles or
/// consoles without VT support, and errors for other native failures.
///
/// Windows mode changes last for the attached console buffer's lifetime and
/// can affect other writers sharing that buffer. Existing mode bits are kept.
/// Applications should prepare once, then decide how to handle NO_COLOR,
/// redirected output, TERM, and native errors themselves.
pub fn prepare_stderr_ansi() -> io::Result<bool> {
    #[cfg(not(windows))]
    {
        Ok(true)
    }
    #[cfg(windows)]
    {
        use winapi::shared::winerror::{ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER};
        use winapi::um::consoleapi::{GetConsoleMode, SetConsoleMode};
        use winapi::um::handleapi::INVALID_HANDLE_VALUE;
        use winapi::um::processenv::GetStdHandle;
        use winapi::um::winbase::STD_ERROR_HANDLE;
        use winapi::um::wincon::{ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING};
        // SAFETY: GetStdHandle has no pointer arguments; the borrowed handle is
        // used only for mode calls and is never closed by this capability.
        let handle = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "stderr has no native handle",
            ));
        }
        let mut mode = 0;
        // SAFETY: mode is writable; native code validates the borrowed handle.
        if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_INVALID_HANDLE as i32) {
                Ok(false)
            } else {
                Err(error)
            };
        }
        let enabled = mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
        // SAFETY: the live console handle and documented output flags came from
        // the successful mode query above. Other existing bits are preserved.
        if mode != enabled && unsafe { SetConsoleMode(handle, enabled) } == 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                Ok(false)
            } else {
                Err(error)
            };
        }
        Ok(true)
    }
}
