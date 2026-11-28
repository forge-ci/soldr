//! Theme tokens.

/// An sRGB colour with straight (unpremultiplied) alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Alpha; 0 is fully transparent.
    pub a: u8,
}

impl Color {
    /// An opaque colour.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// A colour with alpha.
    #[must_use]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Fully transparent.
    pub const TRANSPARENT: Self = Self::rgba(0, 0, 0, 0);
}

/// Light or dark base palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ThemeMode {
    /// Dark backgrounds, light text.
    #[default]
    Dark,
    /// Light backgrounds, dark text.
    Light,
    /// Track the window backend's system preference; dark when unavailable.
    FollowSystem,
}

/// The portable styling tokens. A backend maps each token onto its own
/// style system; anything finer-grained is deliberately not portable.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct Theme {
    /// Base palette.
    pub mode: ThemeMode,
    /// Selection, focus and active-widget colour.
    pub accent: Color,
    /// Window background; `None` keeps the palette default. Use
    /// [`Color::TRANSPARENT`] with a transparent window.
    pub background: Option<Color>,
    /// Body text colour; `None` keeps the palette default.
    pub foreground: Option<Color>,
    /// Fill of drop-down panels ([`Frame::show_panel`](super::Frame::show_panel));
    /// `None` uses `background`. See [`Theme::for_panel`].
    pub panel: Option<Color>,
    /// Body text size in logical pixels.
    pub text_size: f32,
    /// Spacing between widgets in logical pixels.
    pub spacing: f32,
    /// Widget corner radius in logical pixels.
    pub corner_radius: f32,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            mode: ThemeMode::Dark,
            accent: Color::rgb(0x3d, 0x8b, 0xfd),
            background: None,
            foreground: None,
            panel: None,
            text_size: 14.0,
            spacing: 8.0,
            corner_radius: 4.0,
        }
    }
}

impl Theme {
    /// The default tokens with `mode`.
    #[must_use]
    pub fn with_mode(mode: ThemeMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    /// These tokens with `accent`.
    #[must_use]
    pub const fn with_accent(mut self, accent: Color) -> Self {
        self.accent = accent;
        self
    }

    /// These tokens with a window `background`.
    #[must_use]
    pub const fn with_background(mut self, background: Color) -> Self {
        self.background = Some(background);
        self
    }

    /// These tokens with a body text colour.
    #[must_use]
    pub const fn with_foreground(mut self, foreground: Color) -> Self {
        self.foreground = Some(foreground);
        self
    }

    /// These tokens with a drop-down panel fill.
    #[must_use]
    pub const fn with_panel(mut self, panel: Color) -> Self {
        self.panel = Some(panel);
        self
    }

    /// The tokens a drop-down panel draws with: `background` replaced by
    /// `panel` when one is set. Backends apply this to panel surfaces; an app
    /// rendering panel content offscreen can pass it to a snapshot.
    #[must_use]
    pub fn for_panel(self) -> Self {
        Self {
            background: self.panel.or(self.background),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_set_only_their_token() {
        let red = Color::rgb(255, 0, 0);
        let theme = Theme::with_mode(ThemeMode::Light).with_accent(red);
        assert_eq!(theme.mode, ThemeMode::Light);
        assert_eq!(theme.accent, red);
        assert_eq!(theme.background, None);
        let full = theme
            .with_background(Color::rgb(1, 2, 3))
            .with_foreground(Color::rgb(4, 5, 6))
            .with_panel(Color::rgb(7, 8, 9));
        assert_eq!(full.background, Some(Color::rgb(1, 2, 3)));
        assert_eq!(full.foreground, Some(Color::rgb(4, 5, 6)));
        assert_eq!(full.panel, Some(Color::rgb(7, 8, 9)));
    }

    #[test]
    fn a_panel_uses_its_own_fill_else_the_background() {
        let bg = Theme::default().with_background(Color::rgb(1, 2, 3));
        assert_eq!(bg.for_panel().background, Some(Color::rgb(1, 2, 3)));
        let panel = bg.with_panel(Color::rgb(9, 9, 9)).for_panel();
        assert_eq!(panel.background, Some(Color::rgb(9, 9, 9)));
        assert_eq!(Theme::default().for_panel(), Theme::default());
    }
}
