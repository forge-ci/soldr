//! Desktop-wide key chords without exposing portal, Win32 or Carbon types.

use crate::native_global_hotkey;
use std::time::Duration;

/// Modifier keys held with the chord's key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Super / Windows / Command.
    pub meta: bool,
}

/// The non-modifier key of a chord.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// An ASCII letter (stored lowercase) or digit.
    Char(char),
    Space,
    Enter,
    Escape,
    Tab,
    /// F1 through F12.
    Function(u8),
}

/// A validated key chord such as `Meta+Space`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub(crate) modifiers: Modifiers,
    pub(crate) key: Key,
}

impl Chord {
    /// Parse `Mod+Mod+Key`. Modifiers: `Ctrl`, `Alt`, `Shift`, `Meta` (aliases `Super`, `Win`,
    /// `Cmd`), case-insensitive. A chord needs at least one modifier unless the key is `F1`–`F12`.
    pub fn parse(text: &str) -> Result<Self, HotkeyError> {
        let mut modifiers = Modifiers::default();
        let parts: Vec<&str> = text.split('+').map(str::trim).collect();
        let (key, mods) = parts.split_last().ok_or(HotkeyError::InvalidOptions)?;
        for part in mods {
            let flag = match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => &mut modifiers.ctrl,
                "alt" | "option" => &mut modifiers.alt,
                "shift" => &mut modifiers.shift,
                "meta" | "super" | "win" | "cmd" | "logo" => &mut modifiers.meta,
                _ => return Err(HotkeyError::InvalidOptions),
            };
            if *flag {
                return Err(HotkeyError::InvalidOptions);
            }
            *flag = true;
        }
        let key = parse_key(key).ok_or(HotkeyError::InvalidOptions)?;
        if modifiers == Modifiers::default() && !matches!(key, Key::Function(_)) {
            return Err(HotkeyError::InvalidOptions);
        }
        Ok(Self { modifiers, key })
    }

    pub fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    pub fn key(&self) -> Key {
        self.key
    }
}

fn parse_key(text: &str) -> Option<Key> {
    let lower = text.to_ascii_lowercase();
    Some(match lower.as_str() {
        "space" => Key::Space,
        "enter" | "return" => Key::Enter,
        "escape" | "esc" => Key::Escape,
        "tab" => Key::Tab,
        _ => {
            if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                return (1..=12).contains(&n).then_some(Key::Function(n));
            }
            let mut chars = lower.chars();
            let c = chars.next()?;
            if chars.next().is_some() || !c.is_ascii_alphanumeric() {
                return None;
            }
            Key::Char(c)
        }
    })
}

/// A chord plus the identity the desktop shows when it asks the user to confirm it.
#[derive(Clone, Debug)]
pub struct HotkeyOptions {
    pub(crate) id: String,
    pub(crate) description: String,
    pub(crate) chord: Chord,
}

impl HotkeyOptions {
    /// Validate before contacting the desktop. `id` is `[A-Za-z0-9._-]{1,255}`.
    pub fn new(id: &str, description: &str, chord: Chord) -> Result<Self, HotkeyError> {
        if id.is_empty()
            || id.len() > 255
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || description.is_empty()
            || description.len() > 255
            || description.contains('\0')
        {
            return Err(HotkeyError::InvalidOptions);
        }
        Ok(Self {
            id: id.into(),
            description: description.into(),
            chord,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn description(&self) -> &str {
        &self.description
    }
}

/// Registration is optional; callers keep a fallback such as a launcher binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyError {
    InvalidOptions,
    /// This host has no global-shortcut facility.
    Unsupported,
    /// The facility exists but is not reachable right now (no portal, no session bus).
    Unavailable,
    /// Another application already owns the chord, or the user declined it.
    Conflict,
}

/// One activation. Hosts that only report presses (Windows) never send `Released`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyEvent {
    Pressed,
    Released,
}

/// Owned registration. Dropping it releases the chord.
pub struct HotkeyHandle {
    native: native_global_hotkey::Handle,
    events: tokio::sync::mpsc::Receiver<HotkeyEvent>,
}

impl HotkeyHandle {
    /// Register the chord. The desktop may ask the user to confirm it, so this waits up to a minute.
    pub async fn register(options: HotkeyOptions) -> Result<Self, HotkeyError> {
        let (tx, events) = tokio::sync::mpsc::channel(16);
        let native = tokio::time::timeout(
            Duration::from_secs(60),
            native_global_hotkey::register(options, tx),
        )
        .await
        .map_err(|_| HotkeyError::Unavailable)??;
        Ok(Self { native, events })
    }

    /// Take one buffered event; at most 16 are buffered.
    pub fn try_event(&mut self) -> Option<HotkeyEvent> {
        self.events.try_recv().ok()
    }

    /// Wait up to `budget` for the next event. `None` on timeout or when the backend stopped.
    pub async fn next_event_within(&mut self, budget: Duration) -> Option<HotkeyEvent> {
        tokio::time::timeout(budget, self.events.recv())
            .await
            .ok()
            .flatten()
    }

    /// Whether the backend is still delivering events.
    pub fn is_active(&self) -> bool {
        self.native.is_active()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_meta_space_and_aliases() {
        let chord = Chord::parse("Meta+Space").unwrap();
        assert!(chord.modifiers().meta && chord.key() == Key::Space);
        assert_eq!(Chord::parse("super + SPACE").unwrap(), chord);
        assert_eq!(Chord::parse("Ctrl+Shift+K").unwrap().key(), Key::Char('k'));
        assert_eq!(Chord::parse("F9").unwrap().key(), Key::Function(9));
    }

    #[test]
    fn rejects_bare_keys_repeats_and_unknowns() {
        for bad in [
            "Space",
            "a",
            "Meta+Meta+a",
            "Hyper+a",
            "Ctrl+",
            "Ctrl+F13",
            "Ctrl+ab",
            "",
        ] {
            assert_eq!(Chord::parse(bad), Err(HotkeyError::InvalidOptions), "{bad}");
        }
    }
}
