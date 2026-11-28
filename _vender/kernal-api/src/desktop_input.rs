//! Synthesized keyboard and pointer input without exposing uinput, Win32 or Quartz types.
//!
//! Windows (`SendInput`) and macOS (`CGEventPost`) inject in-process; macOS drops events
//! silently until the user grants Accessibility. Linux never opens `/dev/uinput` in the caller:
//! requests go to the privileged `kernal-inputd` daemon over an owner/group-only socket
//! (forge-ci/kernal-api#537), so only that daemon can inject keystrokes.

use crate::native_desktop_input;

/// Longest text accepted by one [`type_text`] call, in chars.
pub const MAX_TEXT_CHARS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputError {
    InvalidInput,
    /// This host has no injection facility.
    Unsupported,
    /// The facility exists but is unreachable (Linux: `kernal-inputd` not running).
    Unavailable,
    /// The host refused the request (permission, policy).
    Denied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Type `text` at the focused control as Unicode, independent of keyboard layout.
pub async fn type_text(text: &str) -> Result<(), InputError> {
    if text.is_empty() || text.contains('\0') || text.chars().count() > MAX_TEXT_CHARS {
        return Err(InputError::InvalidInput);
    }
    native_desktop_input::type_text(text).await
}

/// Move the pointer by a relative offset in pixels (each axis within ±10 000).
pub async fn pointer_move_by(dx: i32, dy: i32) -> Result<(), InputError> {
    if dx.abs() > 10_000 || dy.abs() > 10_000 {
        return Err(InputError::InvalidInput);
    }
    native_desktop_input::pointer_move_by(dx, dy).await
}

/// Press and release one mouse button at the current pointer position.
pub async fn click(button: MouseButton) -> Result<(), InputError> {
    native_desktop_input::click(button).await
}

/// The `kernal-inputd` daemon side; only Linux runs it, other hosts report `Unsupported`.
#[cfg(feature = "desktop-input-daemon")]
pub mod daemon {
    pub use crate::native_input_daemon::run;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_invalid_requests_before_touching_the_host() {
        assert_eq!(type_text("").await, Err(InputError::InvalidInput));
        assert_eq!(type_text("a\0b").await, Err(InputError::InvalidInput));
        let long = "x".repeat(MAX_TEXT_CHARS + 1);
        assert_eq!(type_text(&long).await, Err(InputError::InvalidInput));
        assert_eq!(
            pointer_move_by(10_001, 0).await,
            Err(InputError::InvalidInput)
        );
    }
}
