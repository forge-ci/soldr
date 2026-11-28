//! egui input to facade [`Event`]s.

use crate::gfx::{Event, Key, Modifiers, PointerButton};

/// Logical pixels per wheel line, matching egui's own line-to-point scale.
const LINE_HEIGHT: f32 = 24.0;

pub(crate) fn translate(events: &[egui::Event], out: &mut Vec<Event>) {
    out.extend(events.iter().filter_map(translate_one));
}

fn translate_one(event: &egui::Event) -> Option<Event> {
    Some(match event {
        egui::Event::Key {
            key,
            pressed,
            repeat,
            modifiers,
            ..
        } => Event::Key {
            key: key_of(*key),
            pressed: *pressed,
            repeat: *repeat,
            modifiers: modifiers_of(*modifiers),
        },
        egui::Event::Text(text) => Event::Text(text.clone()),
        egui::Event::PointerMoved(pos) => Event::PointerMoved { x: pos.x, y: pos.y },
        egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers,
        } => Event::PointerButton {
            button: button_of(*button),
            pressed: *pressed,
            x: pos.x,
            y: pos.y,
            modifiers: modifiers_of(*modifiers),
        },
        egui::Event::PointerGone => Event::PointerLeft,
        egui::Event::MouseWheel { unit, delta, .. } => {
            let scale = match unit {
                egui::MouseWheelUnit::Point => 1.0,
                egui::MouseWheelUnit::Line => LINE_HEIGHT,
                egui::MouseWheelUnit::Page => LINE_HEIGHT * 20.0,
            };
            Event::Scroll {
                dx: delta.x * scale,
                dy: delta.y * scale,
            }
        }
        egui::Event::WindowFocused(focused) => Event::Focused(*focused),
        _ => return None,
    })
}

fn modifiers_of(modifiers: egui::Modifiers) -> Modifiers {
    Modifiers {
        shift: modifiers.shift,
        ctrl: modifiers.ctrl,
        alt: modifiers.alt,
        command: modifiers.command,
    }
}

fn button_of(button: egui::PointerButton) -> PointerButton {
    match button {
        egui::PointerButton::Primary => PointerButton::Primary,
        egui::PointerButton::Secondary => PointerButton::Secondary,
        egui::PointerButton::Middle => PointerButton::Middle,
        _ => PointerButton::Other,
    }
}

fn key_of(key: egui::Key) -> Key {
    use egui::Key as K;
    match key {
        K::Enter => Key::Enter,
        K::Escape => Key::Escape,
        K::Tab => Key::Tab,
        K::Backspace => Key::Backspace,
        K::Delete => Key::Delete,
        K::Space => Key::Space,
        K::ArrowUp => Key::ArrowUp,
        K::ArrowDown => Key::ArrowDown,
        K::ArrowLeft => Key::ArrowLeft,
        K::ArrowRight => Key::ArrowRight,
        K::Home => Key::Home,
        K::End => Key::End,
        K::PageUp => Key::PageUp,
        K::PageDown => Key::PageDown,
        other => named_key(other.name()),
    }
}

/// Function keys by name (`"F7"`), printable keys by their one-char symbol.
fn named_key(name: &str) -> Key {
    if let Some(number) = name.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()) {
        return Key::F(number);
    }
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Key::Char(c.to_ascii_lowercase()),
        _ => Key::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_to_neutral_names() {
        assert_eq!(key_of(egui::Key::Enter), Key::Enter);
        assert_eq!(key_of(egui::Key::A), Key::Char('a'));
        assert_eq!(key_of(egui::Key::Num1), Key::Char('1'));
        assert_eq!(key_of(egui::Key::F7), Key::F(7));
    }

    #[test]
    fn line_scroll_is_converted_to_pixels() {
        let mut out = Vec::new();
        translate(
            &[egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: egui::vec2(0.0, -2.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
                source: egui::MouseWheelSource::Unknown,
            }],
            &mut out,
        );
        assert_eq!(out, [Event::Scroll { dx: 0.0, dy: -48.0 }]);
    }
}
