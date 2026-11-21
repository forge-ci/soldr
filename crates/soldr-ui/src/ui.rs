//! Small layout helpers using only the neutral facade.
use crate::{
    data,
    model::{Action, ActionState, DaemonState, State},
};
use kernal_api::gfx::{Color, Frame, Theme, ThemeMode};

pub const BACKGROUND: Color = Color::rgb(23, 28, 33);
pub fn status(state: &State) -> String {
    match state.daemon() {
        DaemonState::Ready => "• Ready".into(),
        DaemonState::Building { n } => format!("• Building · {n} compiles"),
        DaemonState::Stopped => "Stopped".into(),
        DaemonState::Starting => "Starting…".into(),
        DaemonState::Connecting => "Connecting…".into(),
        DaemonState::NeedsAttention { .. } => "• Needs attention".into(),
    }
}
pub fn foreground(state: &State) -> Color {
    if matches!(state.action, ActionState::Failed { .. }) {
        return Color::rgb(255, 166, 166);
    }
    match state.daemon() {
        DaemonState::Ready => Color::rgb(171, 230, 193),
        DaemonState::Building { .. } => Color::rgb(165, 205, 255),
        DaemonState::NeedsAttention { .. } => Color::rgb(255, 216, 151),
        _ => Color::rgb(224, 232, 238),
    }
}
pub fn theme(state: &State, scale: f32) -> Theme {
    let mut theme = Theme::with_mode(ThemeMode::Dark)
        .with_background(BACKGROUND)
        .with_foreground(foreground(state))
        .with_accent(Color::rgb(55, 78, 100));
    debug_assert!(contrast(foreground(state), BACKGROUND) >= 4.5);
    theme.text_size = 14.0 * scale;
    theme.spacing = 8.0 * scale;
    theme
}
pub fn contrast(a: Color, b: Color) -> f64 {
    fn luminance(c: Color) -> f64 {
        let channel = |n: u8| {
            let n = f64::from(n) / 255.0;
            if n <= 0.04045 {
                n / 12.92
            } else {
                ((n + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b)
    }
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}
pub fn fit(frame: &Frame<'_>, text: &str, size: f32, width: f32) -> String {
    if frame.text_width(text, size) <= width {
        return text.into();
    }
    let mut result = String::new();
    for c in text.chars() {
        let candidate = format!("{result}{c}…");
        if frame.text_width(&candidate, size) > width {
            break;
        }
        result.push(c);
    }
    format!("{result}…")
}
pub fn row_buttons(frame: &mut Frame<'_>, labels: &[&str], scale: f32) -> Option<usize> {
    let width = frame.available_width();
    let needed = labels
        .iter()
        .map(|s| frame.text_width(s, 14.0 * scale) + 28.0 * scale)
        .sum::<f32>();
    let mut clicked = None;
    if width >= needed {
        frame.horizontal(|row| {
            for (i, label) in labels.iter().enumerate() {
                if row.button(label) {
                    clicked = Some(i);
                }
            }
        });
    } else {
        for (i, label) in labels.iter().enumerate() {
            if frame.button(label) {
                clicked = Some(i);
            }
        }
    }
    clicked
}
pub fn status_row(frame: &mut Frame<'_>, state: &State, scale: f32) {
    let text = status(state);
    let version = data::version(state).map(|v| format!("v{v}"));
    if let Some(version) = version.filter(|v| {
        frame.text_width(v, 14.0 * scale) + frame.text_width(&text, 14.0 * scale) + 24.0
            < frame.available_width()
    }) {
        frame.horizontal(|row| {
            let width = row.available_width();
            status_badge(row, state);
            row.space(
                (width
                    - row.text_width(&text, 14.0 * scale)
                    - row.text_width(&version, 14.0 * scale)
                    - 16.0)
                    .max(0.0),
            );
            row.label(&version);
        });
    } else {
        status_badge(frame, state);
    }
}
pub fn align(frame: &mut Frame<'_>, start: f32, column: f32) {
    frame.space((column - (start - frame.available_width())).max(0.0));
}

/// A small measured text plot: the bundled font lacks block-element glyphs.
/// Keep actual values visible and use only printable ASCII for its markers.
pub fn sparkline(frame: &mut Frame<'_>, rates: &[f64], scale: f32) {
    if rates.len() < 2 {
        return;
    }
    let low = rates.iter().copied().fold(f64::INFINITY, f64::min);
    let high = rates.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    frame.label(&format!(
        "Recent hit rate · {:.0}% to {:.0}%",
        low * 100.0,
        high * 100.0
    ));
    let bands: Vec<usize> = rates
        .iter()
        .map(|value| {
            if high > low {
                ((value - low) / (high - low) * 3.0).round() as usize
            } else {
                1
            }
        })
        .collect();
    let cell = frame.text_width("000", 14.0 * scale) + 12.0 * scale;
    for band in (0..4).rev() {
        frame.horizontal(|row| {
            let start = row.available_width();
            let mut drawn = false;
            for (index, point) in bands.iter().enumerate() {
                if *point == band {
                    align(row, start, index as f32 * cell);
                    row.label("*");
                    drawn = true;
                }
            }
            if !drawn {
                row.label(" ");
            }
        });
    }
}

/// Compatibility boundary for the forthcoming semantic status widget.
pub fn status_badge(frame: &mut Frame<'_>, state: &State) {
    frame.label(&status(state));
}

/// Compatibility boundary for facade tables; fractions retain measured columns.
pub fn grid_row(
    frame: &mut Frame<'_>,
    cells: &[(f32, &str)],
    first_button: bool,
    scale: f32,
) -> bool {
    let mut clicked = false;
    frame.horizontal(|row| {
        let start = row.available_width();
        for (index, (column, text)) in cells.iter().enumerate() {
            if index > 0 {
                align(row, start, start * column);
            }
            if index == 0 {
                let text = fit(row, text, 14.0 * scale, start * 0.35);
                if first_button {
                    clicked = row.button(&text);
                } else {
                    row.label(&text);
                }
            } else {
                row.label(text);
            }
        }
    });
    clicked
}

/// Until disabled widgets are released, omit ineligible actions in one place.
pub fn action_buttons(
    frame: &mut Frame<'_>,
    state: &State,
    choices: &[(&str, Action)],
    scale: f32,
) -> Option<Action> {
    let eligible: Vec<_> = choices
        .iter()
        .filter(|(_, a)| state.can_submit(*a))
        .collect();
    let labels: Vec<_> = eligible.iter().map(|(label, _)| *label).collect();
    row_buttons(frame, &labels, scale).map(|index| eligible[index].1)
}

/// Pending controls replace conflicting buttons until disabled state is available.
pub fn pending_action(frame: &mut Frame<'_>, state: &State) -> bool {
    if let ActionState::Pending { action } = state.action {
        frame.label(&format!("{}…", action.label()));
        true
    } else {
        false
    }
}

/// Single-control counterpart; keep its original layout when hiding it.
pub fn action_button(frame: &mut Frame<'_>, state: &State, label: &str, action: Action) -> bool {
    state.can_submit(action) && frame.button(label)
}
