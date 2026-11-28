//! A counter window drawn through `kernal_api::gfx` only.
//!
//! ```console
//! soldr cargo run --example gfx-window --features gfx-egui
//! ```
//!
//! Escape closes the window. No egui, wgpu or winit name appears here: the
//! backend is chosen by the `gfx-egui` feature, not by this code.

use kernal_api::gfx::{self, App, Event, Frame, Key, Theme, ThemeMode, WindowOptions};

struct Counter {
    count: i64,
    step: f64,
    dark: bool,
    name: String,
}

impl App for Counter {
    fn frame(&mut self, frame: &mut Frame<'_>) {
        let escape = frame.events().iter().any(|event| {
            matches!(
                event,
                Event::Key {
                    key: Key::Escape,
                    pressed: true,
                    ..
                }
            )
        });
        if escape {
            frame.close();
        }

        frame.heading("kernal-api gfx");
        frame.label(&format!(
            "Hello, {}! The count is {}.",
            self.display_name(),
            self.count
        ));
        frame.horizontal(|row| {
            if row.button("-") {
                self.count -= self.step_size();
            }
            if row.button("+") {
                self.count += self.step_size();
            }
            if row.button("Reset") {
                self.count = 0;
            }
        });
        frame.slider(&mut self.step, 1.0..=10.0, "step");
        frame.separator();
        frame.horizontal(|row| {
            row.label("Name:");
            row.text_field(&mut self.name);
        });
        if frame.toggle(&mut self.dark, "Dark theme") {
            let mode = if self.dark {
                ThemeMode::Dark
            } else {
                ThemeMode::Light
            };
            frame.set_theme(Theme::with_mode(mode));
        }
    }
}

impl Counter {
    fn step_size(&self) -> i64 {
        // The slider bounds the step to 1..=10, so this is exact.
        #[allow(clippy::cast_possible_truncation, reason = "slider range is 1..=10")]
        let step = self.step.round() as i64;
        step
    }

    fn display_name(&self) -> &str {
        match self.name.trim() {
            "" => "world",
            name => name,
        }
    }
}

fn main() -> Result<(), gfx::GfxError> {
    let caps = gfx::capabilities();
    println!("gfx backend: {:?}", caps.backend);
    let options = WindowOptions::new("kernal-api gfx counter")
        .size(360.0, 240.0)
        .resizable(true);
    gfx::run(
        options,
        Counter {
            count: 0,
            step: 1.0,
            dark: true,
            name: String::new(),
        },
    )
}
