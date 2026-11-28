//! Backend-neutral GUI facade.
//!
//! An application describes one window ([`WindowOptions`]), implements
//! [`App`], and hands both to [`run`], which owns the event loop on the
//! calling thread until the window closes. Every frame the backend calls
//! [`App::frame`] with a [`Frame`] carrying an immediate-mode widget subset
//! and the input events delivered since the previous frame.
//!
//! No backend type appears in this module's public surface; see
//! `docs/gfx-backend.md` for the vocabulary, what is deliberately not
//! portable, and how a second backend slots in. With only the `gfx` feature
//! enabled, [`run`] returns [`GfxErrorKind::NoBackend`]; `gfx-egui` supplies
//! the egui implementation.

mod frame;
mod input;
mod theme;
mod widgets;
mod window;

pub use frame::Frame;
pub use input::{Event, Key, Modifiers, PointerButton};
pub use theme::{Color, Theme, ThemeMode};
pub use widgets::{Column, ColumnAlign, Grid, TextRole, TextStyle};
pub use window::{
    DockAlign, DockLength, Edge, Monitor, MonitorInfo, PanelOptions, Placement, WindowOptions,
};

#[cfg(feature = "gfx-egui")]
use crate::gfx_egui as backend;

/// Private stand-in used when no backend feature is enabled.
#[cfg(not(feature = "gfx-egui"))]
mod backend {
    use super::{App, BackendKind, Capabilities, GfxError, GfxErrorKind, Support, WindowOptions};

    use std::convert::Infallible;
    use std::marker::PhantomData;
    use std::ops::RangeInclusive;

    use super::{DockLength, Event, Monitor, MonitorInfo, PanelOptions, PointerButton, Theme};

    /// Uninhabited: no frame is ever produced without a backend.
    pub(crate) struct FrameImp<'a>(Infallible, PhantomData<&'a mut ()>);

    #[allow(
        clippy::unused_self,
        reason = "uninhabited; mirrors the backend signatures"
    )]
    impl FrameImp<'_> {
        pub(crate) fn enabled(&mut self, _: bool, _: &mut dyn FnMut(FrameImp<'_>)) {
            match self.0 {}
        }
        pub(crate) fn styled_label(&mut self, _: &str, _: super::TextStyle) {
            match self.0 {}
        }
        pub(crate) fn status(&mut self, _: &str, _: super::TextRole) {
            match self.0 {}
        }
        pub(crate) fn progress(&mut self, _: f32, _: Option<&str>) {
            match self.0 {}
        }
        pub(crate) fn with_tooltip(&mut self, _: &str, _: &mut dyn FnMut(FrameImp<'_>)) {
            match self.0 {}
        }
        pub(crate) fn grid(&mut self, _: &str, _: usize, _: &mut dyn FnMut(FrameImp<'_>)) {
            match self.0 {}
        }
        pub(crate) fn grid_cell(&mut self, _: super::Column, _: &mut dyn FnMut(FrameImp<'_>)) {
            match self.0 {}
        }
        pub(crate) fn end_row(&mut self) {
            match self.0 {}
        }
        pub(crate) fn sparkline(&mut self, _: &[f32], _: f32, _: super::TextRole) {
            match self.0 {}
        }

        pub(crate) fn events(&self) -> &[Event] {
            match self.0 {}
        }
        pub(crate) fn label(&mut self, _: &str) {
            match self.0 {}
        }
        pub(crate) fn heading(&mut self, _: &str) {
            match self.0 {}
        }
        pub(crate) fn button(&mut self, _: &str) -> bool {
            match self.0 {}
        }
        pub(crate) fn toggle(&mut self, _: &mut bool, _: &str) -> bool {
            match self.0 {}
        }
        pub(crate) fn text_field(&mut self, _: &mut String) -> bool {
            match self.0 {}
        }
        pub(crate) fn clear_keyboard_focus(&mut self) {
            match self.0 {}
        }
        pub(crate) fn secret_field(&mut self, _: &mut String, _: bool) -> bool {
            match self.0 {}
        }
        pub(crate) fn text_area(&mut self, _: &mut String, _: usize) -> bool {
            match self.0 {}
        }
        pub(crate) fn slider(&mut self, _: &mut f64, _: RangeInclusive<f64>, _: &str) -> bool {
            match self.0 {}
        }
        pub(crate) fn separator(&mut self) {
            match self.0 {}
        }
        pub(crate) fn space(&mut self, _: f32) {
            match self.0 {}
        }
        pub(crate) fn horizontal(&mut self, _: &mut dyn FnMut(FrameImp<'_>)) {
            match self.0 {}
        }
        pub(crate) fn scroll(&mut self, _: f32, _: &mut dyn FnMut(FrameImp<'_>)) {
            match self.0 {}
        }
        pub(crate) fn list(&mut self, _: &mut dyn FnMut(FrameImp<'_>)) {
            match self.0 {}
        }
        pub(crate) fn selectable_row(&mut self, _: &str, _: bool) -> bool {
            match self.0 {}
        }
        pub(crate) fn set_theme(&mut self, _: Theme) {
            match self.0 {}
        }
        pub(crate) fn close(&mut self) {
            match self.0 {}
        }
        pub(crate) fn request_repaint(&mut self) {
            match self.0 {}
        }
        pub(crate) fn request_screenshot(&mut self) {
            match self.0 {}
        }
        pub(crate) fn request_repaint_after(&mut self, _: core::time::Duration) {
            match self.0 {}
        }
        pub(crate) fn text_width(&self, _: &str, _: f32) -> f32 {
            match self.0 {}
        }
        pub(crate) fn line_height(&self, _: f32) -> f32 {
            match self.0 {}
        }
        pub(crate) fn available_width(&self) -> f32 {
            match self.0 {}
        }
        pub(crate) fn available_height(&self) -> f32 {
            match self.0 {}
        }
        pub(crate) fn show_panel(&mut self, _: PanelOptions) -> Result<(), GfxError> {
            match self.0 {}
        }
        pub(crate) fn set_dock_length(&mut self, _: DockLength) -> Result<(), GfxError> {
            match self.0 {}
        }
        pub(crate) fn button_click(&mut self, _: &str) -> Option<PointerButton> {
            match self.0 {}
        }
        pub(crate) fn monitors(&self) -> &[MonitorInfo] {
            match self.0 {}
        }
        pub(crate) fn set_monitor(&mut self, _: Monitor) -> Result<(), GfxError> {
            match self.0 {}
        }
    }

    pub(crate) fn capabilities() -> Capabilities {
        Capabilities {
            backend: BackendKind::None,
            transparent: Support::Unsupported,
            click_through: Support::Unsupported,
            always_on_top: Support::Unsupported,
            undecorated: Support::Unsupported,
            absolute_placement: Support::Unsupported,
            monitor_placement: Support::Unsupported,
            dock: Support::Unsupported,
            panels: Support::Unsupported,
            dock_monitor: Support::Unsupported,
            dock_relocation: Support::Unsupported,
        }
    }

    const NO_BACKEND: &str =
        "kernal-api was built with `gfx` but no backend feature (enable `gfx-egui`)";

    pub(crate) fn run(_options: WindowOptions, _app: Box<dyn App>) -> Result<(), GfxError> {
        Err(GfxError::new(GfxErrorKind::NoBackend, NO_BACKEND))
    }

    pub(crate) fn snapshot(
        _options: &WindowOptions,
        _app: &mut dyn App,
    ) -> Result<super::Snapshot, GfxError> {
        Err(GfxError::new(GfxErrorKind::NoBackend, NO_BACKEND))
    }
}

/// An application driven by [`run`].
pub trait App {
    /// Build this frame's UI and react to [`Frame::events`].
    fn frame(&mut self, frame: &mut Frame<'_>);

    /// Build this frame's UI for the panel requested with
    /// [`Frame::show_panel`]. Called only while a panel is shown, with the
    /// panel's own events (pointer positions relative to the panel). The
    /// default draws nothing.
    fn panel(&mut self, frame: &mut Frame<'_>) {
        let _ = frame;
    }

    /// The window was asked to close. Return `false` to keep it open.
    fn close_requested(&mut self) -> bool {
        true
    }
}

/// Which backend implements [`run`] in this build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackendKind {
    /// No backend feature is enabled.
    None,
    /// The `gfx-egui` immediate-mode backend.
    Egui,
}

/// How far a backend honours a non-portable request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Support {
    /// Honoured on every supported host.
    Supported,
    /// Requested from the windowing system, which may ignore it (for example
    /// a Wayland compositor that does not allow client placement).
    BestEffort,
    /// Not available; requesting it makes [`run`] fail with
    /// [`GfxErrorKind::Unsupported`].
    Unsupported,
}

/// The non-portable features of the current backend. See
/// `docs/gfx-backend.md` for why each one is here rather than assumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Capabilities {
    /// The backend compiled into this build.
    pub backend: BackendKind,
    /// [`WindowOptions::transparent`].
    pub transparent: Support,
    /// [`WindowOptions::click_through`].
    pub click_through: Support,
    /// [`WindowOptions::always_on_top`].
    pub always_on_top: Support,
    /// [`WindowOptions::decorations`] set to `false`.
    pub undecorated: Support,
    /// [`Placement::At`].
    pub absolute_placement: Support,
    /// [`Placement::Monitor`].
    pub monitor_placement: Support,
    /// [`Placement::Dock`]: an edge surface the desktop reserves space for.
    /// [`Support::Supported`] where the space really is reserved (a Wayland
    /// layer-shell exclusive zone, decided per session; a Windows AppBar);
    /// [`Support::BestEffort`] on macOS, where the dock is a status-level
    /// window and nothing is reserved because AppKit has nothing to reserve.
    /// [`run`] reports the exact reason when it is unavailable.
    pub dock: Support,
    /// [`Frame::show_panel`]: drop-down panels beside a dock. Available on
    /// layer-shell docks (Linux Wayland) only, and only to an app run with
    /// [`Placement::Dock`]; elsewhere `show_panel` returns
    /// [`GfxErrorKind::Unsupported`].
    pub panels: Support,
    /// [`WindowOptions::monitor`] with a [`Monitor::Named`] monitor for a
    /// dock. Available exactly where [`Capabilities::dock`] is.
    pub dock_monitor: Support,
    /// [`Frame::monitors`] and [`Frame::set_monitor`]: listing the monitors
    /// and moving a running dock between them. Available exactly where
    /// [`Capabilities::dock`] is; elsewhere `monitors` is empty and
    /// `set_monitor` returns [`GfxErrorKind::Unsupported`].
    pub dock_relocation: Support,
}

/// The capabilities of the backend compiled into this build.
#[must_use]
pub fn capabilities() -> Capabilities {
    backend::capabilities()
}

/// Why [`run`] failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum GfxErrorKind {
    /// No backend feature is enabled in this build.
    NoBackend,
    /// The window options request a capability the backend reports as
    /// [`Support::Unsupported`].
    Unsupported,
    /// The window options are invalid (for example a zero size).
    InvalidOptions,
    /// The windowing system or GPU could not be initialised.
    Backend,
    /// [`Frame::set_monitor`] named a monitor that is not connected.
    UnknownMonitor,
}

/// A failed [`run`].
#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct GfxError {
    kind: GfxErrorKind,
    message: String,
}

impl GfxError {
    pub(crate) fn new(kind: GfxErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// The failure class.
    #[must_use]
    pub fn kind(&self) -> GfxErrorKind {
        self.kind
    }
}

/// Open the window described by `options` and drive `app` until it closes.
///
/// Blocks the calling thread, which must be the process main thread on hosts
/// that require it (macOS). Options are validated against [`capabilities`]
/// before any window is created.
///
/// # Errors
///
/// [`GfxErrorKind::InvalidOptions`] or [`GfxErrorKind::Unsupported`] for a
/// request the backend cannot honour, [`GfxErrorKind::NoBackend`] without a
/// backend feature, and [`GfxErrorKind::Backend`] when the windowing system
/// or renderer fails.
pub fn run(options: WindowOptions, app: impl App + 'static) -> Result<(), GfxError> {
    let mut caps = capabilities();
    if matches!(options.placement, Placement::Dock { .. }) {
        // Whether a dock can be shown is decided per session; the backend
        // checks again and its `Unsupported` error says why.
        caps.dock = Support::Supported;
        caps.dock_monitor = Support::Supported;
        caps.dock_relocation = Support::Supported;
    }
    options.validate(&caps)?;
    backend::run(options, Box::new(app))
}

/// Pixels of a UI rendered by [`snapshot`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    width: u32,
    height: u32,
    rgba: std::sync::Arc<[u8]>,
}

impl Snapshot {
    #[cfg_attr(
        not(feature = "gfx-egui"),
        allow(dead_code, reason = "built by a backend")
    )]
    pub(crate) fn new(width: u32, height: u32, rgba: std::sync::Arc<[u8]>) -> Self {
        Self {
            width,
            height,
            rgba,
        }
    }

    /// Width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Row-major, top-down, 8-bit sRGB RGBA with straight alpha, in the same
    /// layout as [`Event::Screenshot`]: `width * height * 4` bytes.
    #[must_use]
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }
}

/// Render `app` without a window and return the pixels.
///
/// The UI is laid out as [`run`] would lay it out in a window of
/// `options.size` (one pixel per logical pixel) with `options.theme`, over a
/// few settling frames that carry no input events, then painted by the
/// backend's renderer into an offscreen image. No window is created and no
/// compositor is involved, so this works where [`Frame::request_screenshot`]
/// cannot: on a locked or display-less session, where a window receives no
/// frames (see `docs/gfx-backend.md`, "When frames happen"). Window-manager
/// options (placement, decorations, always-on-top, click-through) do not
/// apply; `Frame::close`, `request_repaint` and `request_screenshot` are
/// ignored.
///
/// # Errors
///
/// The same option checks as [`run`], [`GfxErrorKind::NoBackend`] without a
/// backend feature, and [`GfxErrorKind::Backend`] when no GPU adapter (or
/// software rasteriser) can be created.
pub fn snapshot(options: &WindowOptions, app: &mut impl App) -> Result<Snapshot, GfxError> {
    // No surface is created, so where a window would be placed is moot: a
    // dock renders on a host that cannot show one.
    let caps = Capabilities {
        dock: Support::Supported,
        dock_monitor: Support::Supported,
        ..capabilities()
    };
    options.validate(&caps)?;
    backend::snapshot(options, app)
}

/// Last system appearance reported by a live window backend. `None` before a
/// window reports it, on unsupported desktops, or without a backend. This is
/// an observation, not an OS probe; snapshots do not change it.
#[must_use]
pub fn system_theme() -> Option<ThemeMode> {
    #[cfg(feature = "gfx-egui")]
    {
        backend::system_theme()
    }
    #[cfg(not(feature = "gfx-egui"))]
    {
        None
    }
}
