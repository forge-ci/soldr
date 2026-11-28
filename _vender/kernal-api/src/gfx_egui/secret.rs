//! Append-only masked editing: never hand plaintext to egui's TextEdit.
use crate::gfx::Event;
use zeroize::{Zeroize, Zeroizing};

const LIMIT: usize = 65_536;

pub(super) fn field(
    ui: &mut egui::Ui,
    events: &mut Vec<Event>,
    value: &mut String,
    multiline: bool,
) -> bool {
    prepare(value);
    let masked: String = value
        .chars()
        .map(|c| if c == '\n' { '\n' } else { '•' })
        .collect();
    let field = egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_min_size(egui::vec2(
            ui.available_width(),
            if multiline { 90.0 } else { 28.0 },
        ));
        ui.label(egui::RichText::new(masked).monospace());
    });
    let response = ui.interact(
        field.response.rect,
        field.response.id.with("secret"),
        egui::Sense::click(),
    );
    if ui.is_enabled() && (response.clicked() || ui.ctx().memory(|m| m.focused().is_none())) {
        response.request_focus();
    }
    let focused = ui.is_enabled() && response.has_focus();
    let mut changed = false;
    ui.ctx().input_mut(|input| {
        for event in &mut input.events {
            match event {
                egui::Event::Text(text)
                | egui::Event::Paste(text)
                | egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                    if focused && value.len() + text.len() <= LIMIT {
                        for c in text
                            .chars()
                            .filter(|c| !c.is_control() || (*c == '\n' && multiline))
                        {
                            value.push(c);
                            changed = true;
                        }
                    }
                    text.zeroize();
                }
                egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) => text.zeroize(),
                egui::Event::Key {
                    key: egui::Key::Backspace,
                    pressed: true,
                    ..
                } if focused => {
                    if let Some((index, _)) = value.char_indices().last() {
                        let old = Zeroizing::new(value.clone());
                        value.zeroize();
                        value.push_str(&old[..index]);
                        changed = true;
                    }
                }
                egui::Event::Key {
                    key: egui::Key::Enter,
                    pressed: true,
                    ..
                } if focused && multiline && value.len() < LIMIT => {
                    value.push('\n');
                    changed = true;
                }
                _ => {}
            }
        }
        for event in &mut input.raw.events {
            scrub(event);
        }
        // Field-owned keys must never also activate a button, including on retry.
        if focused {
            input.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::Enter | egui::Key::Space | egui::Key::Backspace,
                        ..
                    }
                )
            });
        }
    });
    scrub_facade(events);
    if changed {
        ui.ctx().request_repaint();
    }
    changed
}

// Move the old allocation into a zeroizing owner before growing a prefilled field.
fn prepare(value: &mut String) {
    if value.capacity() < LIMIT {
        let old = Zeroizing::new(std::mem::replace(value, String::with_capacity(LIMIT)));
        value.push_str(&old);
    }
}

fn scrub_facade(events: &mut Vec<Event>) {
    for event in events.iter_mut() {
        if let Event::Text(text) = event {
            text.zeroize();
        }
    }
    events.retain(|e| !matches!(e, Event::Text(_)));
}

fn scrub(event: &mut egui::Event) {
    if let egui::Event::Text(text)
    | egui::Event::Paste(text)
    | egui::Event::Ime(egui::ImeEvent::Commit(text))
    | egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) = event
    {
        text.zeroize();
    }
}
