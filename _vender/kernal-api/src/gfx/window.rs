//! Window description.

use super::{Capabilities, GfxError, GfxErrorKind, Support, Theme};

/// Where the window first appears.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
#[non_exhaustive]
pub enum Placement {
    /// Wherever the windowing system puts a new window.
    #[default]
    Default,
    /// Centred on the primary monitor.
    Centered,
    /// Outer top-left corner at this position, in logical pixels of the
    /// virtual desktop.
    At {
        /// Horizontal position.
        x: f32,
        /// Vertical position.
        y: f32,
    },
    /// On the monitor with this enumeration index.
    Monitor {
        /// Zero-based monitor index.
        index: u32,
    },
    /// A dock (panel, bar) along one screen edge: it spans that edge, is
    /// `thickness` logical pixels deep, and the desktop reserves the space so
    /// ordinary windows do not cover it. [`WindowOptions::size`] is ignored
    /// for the window (it still sizes a [`snapshot`](super::snapshot)).
    /// Requires [`Capabilities::dock`].
    Dock {
        /// The screen edge the dock spans.
        edge: Edge,
        /// Depth away from the edge, in logical pixels.
        thickness: f32,
    },
}

/// A drop-down panel: a borderless, always-on-top surface that the running
/// dock opens beside itself, anchored to a screen edge. Shown with
/// [`Frame::show_panel`](super::Frame::show_panel) on every frame it should
/// stay visible and drawn by [`App::panel`](super::App::panel). The desktop
/// keeps it clear of the dock's reserved space, so a panel on the dock's own
/// edge opens directly below (or beside) the dock. Requires
/// [`Capabilities::panels`](super::Capabilities::panels).
#[derive(Clone, Copy, Debug, PartialEq)]
#[must_use]
pub struct PanelOptions {
    pub(crate) edge: Edge,
    pub(crate) offset: f32,
    pub(crate) size: (f32, f32),
}

impl PanelOptions {
    /// A `width` x `height` panel (logical pixels) on the top edge, at the
    /// start of that edge.
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            edge: Edge::Top,
            offset: 0.0,
            size: (width, height),
        }
    }

    /// The screen edge the panel hangs from.
    pub fn edge(mut self, edge: Edge) -> Self {
        self.edge = edge;
        self
    }

    /// Distance along the edge from the start of the dock (its left end on
    /// a top or bottom edge, its top end on a left or right edge), in
    /// logical pixels. For a dock spanning the edge
    /// ([`DockLength::Full`]) that is the start of the edge; for a shorter
    /// one the backend adds where the dock begins, so an app positions a
    /// panel under one of its widgets without knowing the screen size.
    pub fn offset(mut self, offset: f32) -> Self {
        self.offset = offset;
        self
    }

    /// The requested size in logical pixels.
    #[must_use]
    pub fn size(&self) -> (f32, f32) {
        self.size
    }

    pub(crate) fn validate(&self) -> Result<(), GfxError> {
        let (width, height) = self.size;
        if !(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0) {
            return Err(GfxError::new(
                GfxErrorKind::InvalidOptions,
                format!("panel size must be positive and finite, got {width}x{height}"),
            ));
        }
        if !(self.offset.is_finite() && self.offset >= 0.0) {
            return Err(GfxError::new(
                GfxErrorKind::InvalidOptions,
                format!(
                    "panel offset must be finite and not negative, got {}",
                    self.offset
                ),
            ));
        }
        Ok(())
    }
}

/// Which monitor (output) a [`Placement::Dock`] and its panels appear on.
///
/// Set with [`WindowOptions::monitor`]. Requires
/// [`Capabilities::dock_monitor`](super::Capabilities::dock_monitor) for
/// anything but [`Monitor::Primary`]. The monitor the dock actually landed
/// on is reported to the app as [`Event::Monitor`](super::Event::Monitor).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Monitor {
    /// The desktop's choice: its primary monitor where it has that notion,
    /// otherwise the one it considers active.
    #[default]
    Primary,
    /// The monitor whose connector name is this (`HDMI-A-1`, `DP-2`,
    /// `eDP-1`), as the desktop names outputs.
    ///
    /// When no such monitor is connected at start-up the dock falls back to
    /// [`Monitor::Primary`] rather than failing, and the first
    /// [`Event::Monitor`](super::Event::Monitor) says so with
    /// `fallback: true`: a bar that refuses to start while its monitor
    /// sleeps is worse than one on the wrong monitor.
    Named(String),
}

/// One monitor (output) a running dock can be shown on, as listed by
/// [`Frame::monitors`](super::Frame::monitors).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MonitorInfo {
    /// The connector name (`HDMI-A-1`), the same string
    /// [`Monitor::Named`] and [`Event::Monitor`](super::Event::Monitor)
    /// use; empty when the desktop does not name its outputs.
    pub name: String,
    /// The dock is on this monitor now.
    pub current: bool,
}

impl MonitorInfo {
    #[cfg_attr(
        not(feature = "gfx-egui"),
        allow(dead_code, reason = "built by a backend")
    )]
    pub(crate) fn new(name: String, current: bool) -> Self {
        Self { name, current }
    }
}

/// How much of its edge a [`Placement::Dock`] spans. Set with
/// [`WindowOptions::dock_length`] and changed while running with
/// [`Frame::set_dock_length`](super::Frame::set_dock_length).
///
/// Either way the desktop reserves exactly the dock's thickness along the
/// whole edge: a shorter dock does not let maximised windows slide under
/// the free part of the edge, and does not reserve the space twice.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
#[non_exhaustive]
pub enum DockLength {
    /// The whole edge.
    #[default]
    Full,
    /// This many logical pixels along the edge (an app sizing itself to its
    /// content measures it with [`Frame::text_width`](super::Frame::text_width)).
    /// Clamped to the edge's length once the backend knows the monitor.
    Fixed(f32),
}

impl DockLength {
    pub(crate) fn validate(self) -> Result<(), GfxError> {
        match self {
            Self::Full => Ok(()),
            Self::Fixed(length) if length.is_finite() && length > 0.0 => Ok(()),
            Self::Fixed(length) => Err(GfxError::new(
                GfxErrorKind::InvalidOptions,
                format!("dock length must be positive and finite, got {length}"),
            )),
        }
    }
}

/// Where a [`DockLength::Fixed`] dock sits along its edge. Set with
/// [`WindowOptions::dock_align`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum DockAlign {
    /// At the start of the edge (left of a top or bottom edge, top of a
    /// side edge).
    Start,
    /// Centred on the edge.
    #[default]
    Center,
    /// At the end of the edge.
    End,
}

/// A screen edge, for [`Placement::Dock`] and [`PanelOptions`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Edge {
    /// The top edge.
    Top,
    /// The bottom edge.
    Bottom,
    /// The left edge.
    Left,
    /// The right edge.
    Right,
}

/// Everything [`run`](super::run) needs to create the window.
#[derive(Clone, Debug, PartialEq)]
#[must_use]
pub struct WindowOptions {
    pub(crate) title: String,
    pub(crate) size: (f32, f32),
    pub(crate) resizable: bool,
    pub(crate) decorations: bool,
    pub(crate) always_on_top: bool,
    pub(crate) transparent: bool,
    pub(crate) click_through: bool,
    pub(crate) placement: Placement,
    pub(crate) monitor: Monitor,
    pub(crate) dock_length: DockLength,
    pub(crate) dock_align: DockAlign,
    pub(crate) theme: Theme,
}

impl WindowOptions {
    /// A decorated, resizable 640x480 window with the default theme.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            size: (640.0, 480.0),
            resizable: true,
            decorations: true,
            always_on_top: false,
            transparent: false,
            click_through: false,
            placement: Placement::Default,
            monitor: Monitor::Primary,
            dock_length: DockLength::Full,
            dock_align: DockAlign::Center,
            theme: Theme::default(),
        }
    }

    /// Inner size in logical pixels.
    pub fn size(mut self, width: f32, height: f32) -> Self {
        self.size = (width, height);
        self
    }

    /// Whether the user may resize the window.
    pub fn resizable(mut self, resizable: bool) -> Self {
        self.resizable = resizable;
        self
    }

    /// Whether the window has a title bar and border.
    pub fn decorations(mut self, decorations: bool) -> Self {
        self.decorations = decorations;
        self
    }

    /// Keep the window above ordinary windows.
    pub fn always_on_top(mut self, always_on_top: bool) -> Self {
        self.always_on_top = always_on_top;
        self
    }

    /// Composite the window with a transparent background.
    ///
    /// The surface is then cleared to the theme's background colour
    /// ([`Theme::with_background`](super::Theme::with_background), and the
    /// panel colour for panels) *including its alpha*, so a background of
    /// `Color::rgba(r, g, b, 179)` shows the desktop through it at 70%
    /// opacity while text and widgets stay opaque. Without a theme
    /// background the window is fully clear behind its widgets.
    pub fn transparent(mut self, transparent: bool) -> Self {
        self.transparent = transparent;
        self
    }

    /// Let pointer input pass through the window to whatever is below.
    pub fn click_through(mut self, click_through: bool) -> Self {
        self.click_through = click_through;
        self
    }

    /// Initial placement.
    pub fn placement(mut self, placement: Placement) -> Self {
        self.placement = placement;
        self
    }

    /// The monitor a [`Placement::Dock`] (and its panels) appears on.
    /// Ignored for other placements.
    pub fn monitor(mut self, monitor: Monitor) -> Self {
        self.monitor = monitor;
        self
    }

    /// How much of its edge a [`Placement::Dock`] spans at start-up
    /// (default [`DockLength::Full`]). Ignored for other placements.
    pub fn dock_length(mut self, length: DockLength) -> Self {
        self.dock_length = length;
        self
    }

    /// Where a [`DockLength::Fixed`] dock sits along its edge (default
    /// [`DockAlign::Center`]). Ignored for other placements.
    pub fn dock_align(mut self, align: DockAlign) -> Self {
        self.dock_align = align;
        self
    }

    /// Initial theme; [`Frame::set_theme`](super::Frame::set_theme) changes it later.
    pub fn theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    pub(crate) fn validate(&self, caps: &Capabilities) -> Result<(), GfxError> {
        let (width, height) = self.size;
        if !(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0) {
            return Err(GfxError::new(
                GfxErrorKind::InvalidOptions,
                format!("window size must be positive and finite, got {width}x{height}"),
            ));
        }
        if let Placement::Dock { thickness, .. } = self.placement {
            if !(thickness.is_finite() && thickness > 0.0) {
                return Err(GfxError::new(
                    GfxErrorKind::InvalidOptions,
                    format!("dock thickness must be positive and finite, got {thickness}"),
                ));
            }
            self.dock_length.validate()?;
        }
        let requested = [
            (self.transparent, caps.transparent, "transparent"),
            (self.click_through, caps.click_through, "click_through"),
            (self.always_on_top, caps.always_on_top, "always_on_top"),
            (!self.decorations, caps.undecorated, "decorations(false)"),
            (
                matches!(self.placement, Placement::At { .. }),
                caps.absolute_placement,
                "Placement::At",
            ),
            (
                matches!(self.placement, Placement::Monitor { .. }),
                caps.monitor_placement,
                "Placement::Monitor",
            ),
            (
                matches!(self.placement, Placement::Dock { .. }),
                caps.dock,
                "Placement::Dock",
            ),
            (
                matches!(self.placement, Placement::Dock { .. })
                    && self.monitor != Monitor::Primary,
                caps.dock_monitor,
                "Monitor::Named",
            ),
        ];
        for (wanted, support, name) in requested {
            if wanted && support == Support::Unsupported {
                return Err(GfxError::new(
                    GfxErrorKind::Unsupported,
                    format!(
                        "{name} is not supported by the {:?} gfx backend",
                        caps.backend
                    ),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx::BackendKind;

    fn all_best_effort() -> Capabilities {
        Capabilities {
            backend: BackendKind::Egui,
            transparent: Support::BestEffort,
            click_through: Support::BestEffort,
            always_on_top: Support::BestEffort,
            undecorated: Support::Supported,
            absolute_placement: Support::BestEffort,
            monitor_placement: Support::Unsupported,
            dock: Support::Unsupported,
            panels: Support::Unsupported,
            dock_monitor: Support::Unsupported,
            dock_relocation: Support::Unsupported,
        }
    }

    #[test]
    fn a_named_monitor_needs_its_capability_only_on_a_dock() {
        let named = |placement| {
            WindowOptions::new("bar")
                .placement(placement)
                .monitor(Monitor::Named("HDMI-A-1".into()))
        };
        let dock = Placement::Dock {
            edge: Edge::Top,
            thickness: 36.0,
        };
        let caps = Capabilities {
            dock: Support::Supported,
            ..all_best_effort()
        };
        let err = named(dock).validate(&caps).expect_err("no dock_monitor");
        assert_eq!(err.kind(), GfxErrorKind::Unsupported);
        assert!(err.to_string().contains("Monitor::Named"), "{err}");
        let caps = Capabilities {
            dock_monitor: Support::Supported,
            ..caps
        };
        assert!(named(dock).validate(&caps).is_ok());
        assert!(
            named(Placement::Default)
                .validate(&all_best_effort())
                .is_ok(),
            "ignored off a dock"
        );
        assert_eq!(WindowOptions::new("t").monitor, Monitor::Primary);
    }

    #[test]
    fn a_fixed_dock_length_must_be_positive() {
        let caps = Capabilities {
            dock: Support::Supported,
            ..all_best_effort()
        };
        let dock = |length| {
            WindowOptions::new("bar")
                .placement(Placement::Dock {
                    edge: Edge::Top,
                    thickness: 36.0,
                })
                .dock_length(length)
                .dock_align(DockAlign::End)
        };
        assert!(dock(DockLength::Full).validate(&caps).is_ok());
        assert!(dock(DockLength::Fixed(900.0)).validate(&caps).is_ok());
        for bad in [0.0, -3.0, f32::NAN, f32::INFINITY] {
            let err = dock(DockLength::Fixed(bad))
                .validate(&caps)
                .expect_err("rejected");
            assert_eq!(err.kind(), GfxErrorKind::InvalidOptions);
        }
        let plain = WindowOptions::new("t");
        assert_eq!(
            (plain.dock_length, plain.dock_align),
            (DockLength::Full, DockAlign::Center)
        );
    }

    #[test]
    fn a_panel_needs_a_positive_size_and_offset() {
        assert!(PanelOptions::new(320.0, 200.0)
            .offset(40.0)
            .validate()
            .is_ok());
        for bad in [
            PanelOptions::new(0.0, 10.0),
            PanelOptions::new(10.0, f32::INFINITY),
            PanelOptions::new(10.0, 10.0).offset(-1.0),
            PanelOptions::new(10.0, 10.0).offset(f32::NAN),
        ] {
            let err = bad.validate().expect_err("must be rejected");
            assert_eq!(err.kind(), GfxErrorKind::InvalidOptions);
        }
        let panel = PanelOptions::new(5.0, 6.0).edge(Edge::Bottom);
        assert_eq!(panel.size(), (5.0, 6.0));
        assert_eq!(panel.edge, Edge::Bottom);
    }

    #[test]
    fn rejects_a_degenerate_size() {
        let err = WindowOptions::new("t")
            .size(0.0, 10.0)
            .validate(&all_best_effort())
            .expect_err("zero width must be rejected");
        assert_eq!(err.kind(), GfxErrorKind::InvalidOptions);
        let err = WindowOptions::new("t")
            .size(f32::NAN, 10.0)
            .validate(&all_best_effort())
            .expect_err("NaN width must be rejected");
        assert_eq!(err.kind(), GfxErrorKind::InvalidOptions);
    }

    #[test]
    fn rejects_an_unsupported_capability_and_accepts_best_effort() {
        let caps = all_best_effort();
        let ok = WindowOptions::new("t")
            .transparent(true)
            .click_through(true)
            .always_on_top(true)
            .decorations(false)
            .placement(Placement::At { x: 1.0, y: 2.0 });
        assert!(ok.validate(&caps).is_ok());
        let err = WindowOptions::new("t")
            .placement(Placement::Monitor { index: 1 })
            .validate(&caps)
            .expect_err("monitor placement is unsupported here");
        assert_eq!(err.kind(), GfxErrorKind::Unsupported);
    }

    #[test]
    fn a_dock_needs_the_capability_and_a_positive_thickness() {
        let dock = |thickness| {
            WindowOptions::new("bar").placement(Placement::Dock {
                edge: Edge::Top,
                thickness,
            })
        };
        let err = dock(36.0)
            .validate(&all_best_effort())
            .expect_err("dock is unsupported here");
        assert_eq!(err.kind(), GfxErrorKind::Unsupported);
        let caps = Capabilities {
            dock: Support::Supported,
            ..all_best_effort()
        };
        assert!(dock(36.0).validate(&caps).is_ok());
        let err = dock(0.0).validate(&caps).expect_err("zero thickness");
        assert_eq!(err.kind(), GfxErrorKind::InvalidOptions);
    }
}
