//! Key codes to text for dock panels, with the compositor's own keymap.
//!
//! `wl_keyboard` delivers evdev key codes and, once, the keymap that gives
//! them meaning (an XKB text keymap in a file descriptor). libxkbcommon turns
//! a code plus the current modifier state into the character the user's
//! layout types; it is loaded at run time (as winit loads it for ordinary
//! windows), so a host without it still gets named keys, just no text.

use std::ffi::c_char;
use std::fs::File;
use std::os::fd::OwnedFd;

use xkbcommon_dl::{
    xkb_context, xkb_context_flags, xkb_keymap, xkb_keymap_compile_flags, xkb_keymap_format,
    xkb_state, xkb_state_component, XkbCommon, XKB_MOD_NAME_ALT, XKB_MOD_NAME_CTRL,
    XKB_MOD_NAME_LOGO, XKB_MOD_NAME_SHIFT,
};

/// Evdev codes are XKB key codes minus this.
const EVDEV_OFFSET: u32 = 8;

/// A compiled keymap and its modifier state. Single-threaded, like the
/// Wayland queue that feeds it.
pub(crate) struct Keymap {
    xkb: &'static XkbCommon,
    context: *mut xkb_context,
    keymap: *mut xkb_keymap,
    state: *mut xkb_state,
}

/// Which modifiers are held, for key events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "one flag per modifier")]
pub(crate) struct Mods {
    pub(crate) shift: bool,
    pub(crate) ctrl: bool,
    pub(crate) alt: bool,
    pub(crate) logo: bool,
}

impl Keymap {
    /// Compile the XKB text keymap of `size` bytes in `fd`. `None` when
    /// libxkbcommon is not installed or the keymap does not compile.
    pub(crate) fn from_fd(fd: OwnedFd, size: u32) -> Option<Self> {
        let xkb = xkbcommon_dl::xkbcommon_option()?;
        let file = File::from(fd);
        let len = usize::try_from(size).ok()?;
        // SAFETY: a private, read-only copy-on-write map of a file the
        // compositor sent for exactly this; nothing writes through it.
        let map = unsafe { memmap2::MmapOptions::new().len(len).map_copy_read_only(&file) }.ok()?;
        // The keymap is a NUL-terminated string; hand over the text alone.
        let text = map.split(|byte| *byte == 0).next().unwrap_or(&map);
        // SAFETY: plain libxkbcommon calls on pointers it returned, each
        // checked for NULL before use; `text` outlives the call that copies it.
        unsafe {
            let context = (xkb.xkb_context_new)(xkb_context_flags::XKB_CONTEXT_NO_FLAGS);
            if context.is_null() {
                return None;
            }
            let keymap = (xkb.xkb_keymap_new_from_buffer)(
                context,
                text.as_ptr().cast::<c_char>(),
                text.len(),
                xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,
                xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
            );
            if keymap.is_null() {
                (xkb.xkb_context_unref)(context);
                return None;
            }
            let state = (xkb.xkb_state_new)(keymap);
            if state.is_null() {
                (xkb.xkb_keymap_unref)(keymap);
                (xkb.xkb_context_unref)(context);
                return None;
            }
            Some(Self {
                xkb,
                context,
                keymap,
                state,
            })
        }
    }

    /// `wl_keyboard.modifiers`, as sent.
    pub(crate) fn set_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        // SAFETY: `state` is live for `self`'s lifetime.
        unsafe {
            (self.xkb.xkb_state_update_mask)(self.state, depressed, latched, locked, 0, 0, group);
        }
    }

    pub(crate) fn mods(&self) -> Mods {
        let active = |name: &[u8]| {
            // SAFETY: `name` is one of libxkbcommon's NUL-terminated names.
            unsafe {
                (self.xkb.xkb_state_mod_name_is_active)(
                    self.state,
                    name.as_ptr().cast::<c_char>(),
                    xkb_state_component::XKB_STATE_MODS_EFFECTIVE,
                ) > 0
            }
        };
        Mods {
            shift: active(XKB_MOD_NAME_SHIFT),
            ctrl: active(XKB_MOD_NAME_CTRL),
            alt: active(XKB_MOD_NAME_ALT),
            logo: active(XKB_MOD_NAME_LOGO),
        }
    }

    /// What evdev key `code` types under the current modifiers, if it types
    /// anything printable. Shortcuts (Ctrl, Alt, Super held) type nothing.
    pub(crate) fn text(&self, code: u32) -> Option<String> {
        let mods = self.mods();
        if mods.ctrl || mods.alt || mods.logo {
            return None;
        }
        let mut buf = [0u8; 64];
        // SAFETY: libxkbcommon writes at most `buf.len()` bytes, NUL included.
        let written = unsafe {
            (self.xkb.xkb_state_key_get_utf8)(
                self.state,
                code + EVDEV_OFFSET,
                buf.as_mut_ptr().cast::<c_char>(),
                buf.len(),
            )
        };
        let written = usize::try_from(written).ok().filter(|n| *n > 0 && *n < buf.len())?;
        printable(std::str::from_utf8(&buf[..written]).ok()?)
    }
}

/// `text` unless it is empty or a control character (Enter, Tab, Backspace
/// and Escape type those; they arrive as named keys instead).
fn printable(text: &str) -> Option<String> {
    (!text.is_empty() && !text.chars().any(char::is_control)).then(|| text.to_owned())
}

impl Drop for Keymap {
    fn drop(&mut self) {
        // SAFETY: each pointer was returned by libxkbcommon and is released once.
        unsafe {
            (self.xkb.xkb_state_unref)(self.state);
            (self.xkb.xkb_keymap_unref)(self.keymap);
            (self.xkb.xkb_context_unref)(self.context);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::printable;

    #[test]
    fn control_characters_are_not_text() {
        assert_eq!(printable("a").as_deref(), Some("a"));
        assert_eq!(printable("É").as_deref(), Some("É"));
        assert_eq!(printable(" ").as_deref(), Some(" "));
        for control in ["", "\r", "\t", "\u{8}", "\u{1b}", "\u{7f}"] {
            assert_eq!(printable(control), None, "{control:?}");
        }
    }
}
