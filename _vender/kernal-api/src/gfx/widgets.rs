//! Portable utility-panel widgets and explicit column layout.
use super::{Color, Frame, GfxError, GfxErrorKind, Theme, ThemeMode};

/// Meaning of text or a status indicator; resolved through the active theme.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum TextRole {
    /// Ordinary body text.
    #[default]
    Primary,
    /// Supporting information, still readable at small sizes.
    Secondary,
    /// A successful or ready state.
    Success,
    /// A state needing attention.
    Warning,
    /// A failed operation.
    Danger,
    /// Activity or emphasis.
    Accent,
}

/// Per-label styling, without exposing a font or backend style tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextStyle {
    /// Semantic colour, unless `color` is supplied.
    pub role: TextRole,
    /// Explicit sRGB override. The caller owns its contrast.
    pub color: Option<Color>,
    /// Strong emphasis.
    pub strong: bool,
    /// Three quarters of the theme body size.
    pub small: bool,
}

/// Horizontal alignment within a grid column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColumnAlign {
    /// Align to the left edge.
    #[default]
    Left,
    /// Align to the right edge.
    Right,
}

/// A fixed logical-pixel column. Explicit widths stay stable across frames.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Column {
    /// Positive finite width, excluding the theme's gap between columns.
    pub width: f32,
    /// Horizontal alignment of cell contents.
    pub align: ColumnAlign,
}

/// Grid contents. Each `cell` advances one column, automatically starting a
/// new row after the last column. An incomplete last row is allowed.
pub struct Grid<'a> {
    pub(crate) frame: Frame<'a>,
    pub(crate) columns: &'a [Column],
    pub(crate) column: usize,
}

impl Grid<'_> {
    /// Draw one cell. Text wraps within its column; keep controls within the
    /// requested width. Rows grow to the height of their tallest cell.
    pub fn cell<R>(&mut self, contents: impl FnOnce(&mut Frame<'_>) -> R) -> R {
        let mut contents = Some(contents);
        let mut result = None;
        self.frame
            .imp
            .grid_cell(self.columns[self.column], &mut |imp| {
                result = Some(contents.take().expect("one cell invocation")(&mut Frame {
                    imp,
                }));
            });
        self.column += 1;
        if self.column == self.columns.len() {
            self.column = 0;
            self.frame.imp.end_row();
        }
        result.expect("the backend runs the cell body exactly once")
    }
}

impl Frame<'_> {
    /// Scope interaction state for every nested control. Nested `true` cannot
    /// re-enable a disabled parent. The closure runs once and returns its value.
    /// Raw `events()` remain available: app-owned shortcuts must respect state.
    pub fn enabled<R>(&mut self, enabled: bool, contents: impl FnOnce(&mut Frame<'_>) -> R) -> R {
        let mut contents = Some(contents);
        let mut result = None;
        self.imp.enabled(enabled, &mut |imp| {
            result = Some(contents.take().expect("one scope invocation")(&mut Frame {
                imp,
            }));
        });
        result.expect("the backend runs the enabled body exactly once")
    }

    /// Show a non-interactive label with semantic colour, weight and size.
    pub fn styled_label(&mut self, text: &str, style: TextStyle) {
        self.imp.styled_label(text, style);
    }

    /// A non-interactive coloured dot followed by accessible text. Semantic
    /// colours meet 4.5:1 on opaque theme backgrounds, including custom fills.
    /// Transparent compositing requires the caller to check the final background.
    pub fn status(&mut self, text: &str, role: TextRole) {
        self.imp.status(text, role);
    }

    /// A determinate bar. Finite fractions clamp to 0..=1; non-finite values
    /// display as zero. Optional text replaces the default percentage.
    pub fn progress(&mut self, fraction: f32, text: Option<&str>) {
        self.imp.progress(fraction, text);
    }

    /// Attach hover text to the contents' bounding rectangle, including disabled
    /// controls. Scope runs once and returns its value. Draw one control per
    /// wrapper when the tooltip describes one control.
    pub fn with_tooltip<R>(&mut self, text: &str, contents: impl FnOnce(&mut Frame<'_>) -> R) -> R {
        let mut contents = Some(contents);
        let mut result = None;
        self.imp.with_tooltip(text, &mut |imp| {
            result = Some(contents.take().expect("one tooltip invocation")(
                &mut Frame { imp },
            ));
        });
        result.expect("the backend runs the tooltip body exactly once")
    }

    /// Lay out cells in fixed-width columns under a stable, caller-owned ID.
    /// IDs must be unique within the current layout. Rejects empty columns or
    /// non-positive/non-finite widths before invoking the closure.
    ///
    /// # Errors
    /// Returns `InvalidOptions` for invalid columns.
    pub fn grid<R>(
        &mut self,
        id: &str,
        columns: &[Column],
        contents: impl FnOnce(&mut Grid<'_>) -> R,
    ) -> Result<R, GfxError> {
        validate_columns(columns)?;
        let mut contents = Some(contents);
        let mut result = None;
        self.imp.grid(id, columns.len(), &mut |imp| {
            let mut grid = Grid {
                frame: Frame { imp },
                columns,
                column: 0,
            };
            result = Some(contents.take().expect("one grid invocation")(&mut grid));
        });
        Ok(result.expect("the backend runs the grid body exactly once"))
    }

    /// A non-interactive line mini-chart, layout-wide and `height` logical
    /// pixels tall. Auto-scales finite samples; missing samples break the line,
    /// constant samples are centred, and empty input draws an empty region.
    /// Invalid heights draw nothing. Add a text summary for accessibility.
    pub fn sparkline(&mut self, samples: &[f32], height: f32, role: TextRole) {
        self.imp.sparkline(samples, height, role);
    }
}

fn validate_columns(columns: &[Column]) -> Result<(), GfxError> {
    if columns.is_empty()
        || columns
            .iter()
            .any(|c| !c.width.is_finite() || c.width <= 0.0)
    {
        return Err(GfxError::new(
            GfxErrorKind::InvalidOptions,
            "grid needs positive finite column widths",
        ));
    }
    Ok(())
}

impl Theme {
    /// Window fill used by the portable palette. `FollowSystem` resolves the
    /// last reported preference, defaulting to dark before any report.
    #[must_use]
    pub fn background_color(&self) -> Color {
        self.background
            .unwrap_or_else(|| match self.resolved_mode() {
                ThemeMode::Light => Color::rgb(248, 248, 248),
                _ => Color::rgb(27, 27, 27),
            })
    }

    /// Semantic foreground with WCAG AA contrast (4.5:1) on the theme fill.
    /// Custom colours are adjusted toward black or white only when necessary.
    #[must_use]
    pub fn role_color(&self, role: TextRole) -> Color {
        let light = self.resolved_mode() == ThemeMode::Light;
        let color = match (role, light) {
            (TextRole::Primary, _) => self.foreground.unwrap_or(if light {
                Color::rgb(32, 32, 32)
            } else {
                Color::rgb(235, 235, 235)
            }),
            (TextRole::Secondary, true) => Color::rgb(95, 95, 95),
            (TextRole::Secondary, false) => Color::rgb(170, 170, 170),
            (TextRole::Success, true) => Color::rgb(25, 110, 55),
            (TextRole::Success, false) => Color::rgb(100, 210, 140),
            (TextRole::Warning, true) => Color::rgb(130, 80, 0),
            (TextRole::Warning, false) => Color::rgb(240, 195, 90),
            (TextRole::Danger, true) => Color::rgb(185, 35, 45),
            (TextRole::Danger, false) => Color::rgb(255, 135, 140),
            (TextRole::Accent, _) => self.accent,
        };
        contrasting(color, self.background_color())
    }

    fn resolved_mode(&self) -> ThemeMode {
        match self.mode {
            ThemeMode::FollowSystem => super::system_theme().unwrap_or(ThemeMode::Dark),
            mode => mode,
        }
    }
}

fn luminance(color: Color) -> f64 {
    let linear = |value: u8| {
        let v = f64::from(value) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}

fn contrast(a: Color, b: Color) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn contrasting(mut color: Color, background: Color) -> Color {
    color.a = 255;
    let black = Color::rgb(0, 0, 0);
    let white = Color::rgb(255, 255, 255);
    let toward_white = contrast(white, background) > contrast(black, background);
    while contrast(color, background) < 4.5 {
        let step = |c: u8| {
            if toward_white {
                c.saturating_add(1)
            } else {
                c.saturating_sub(1)
            }
        };
        color = Color::rgb(step(color.r), step(color.g), step(color.b));
    }
    color
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_semantic_role_meets_aa_in_both_themes_and_custom_fills() {
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            for background in [
                None,
                Some(Color::rgb(120, 120, 120)),
                Some(Color::rgb(0, 30, 80)),
            ] {
                let theme = Theme {
                    background,
                    ..Theme::with_mode(mode)
                };
                for role in [
                    TextRole::Primary,
                    TextRole::Secondary,
                    TextRole::Success,
                    TextRole::Warning,
                    TextRole::Danger,
                    TextRole::Accent,
                ] {
                    assert!(
                        contrast(theme.role_color(role), theme.background_color()) >= 4.5,
                        "{mode:?} {role:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn grid_rejects_invalid_widths() {
        assert!(validate_columns(&[]).is_err());
        for width in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert!(validate_columns(&[Column {
                width,
                align: ColumnAlign::Left
            }])
            .is_err());
        }
        assert!(validate_columns(&[Column {
            width: 100.0,
            align: ColumnAlign::Right
        }])
        .is_ok());
    }
}
