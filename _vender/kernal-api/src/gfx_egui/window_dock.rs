//! Docks drawn in an ordinary eframe window, for hosts where a dock is a
//! property of a top-level window rather than a separate surface type: a
//! Windows AppBar, or a macOS window raised to status-bar level.
//!
//! This module owns eframe and the geometry; the host's `gfx_dock` module
//! owns every native call behind [`DockHost`]. The frame is the same
//! `Adapter` / `draw` / `FrameImp` an ordinary window uses, so an `App`
//! cannot tell which path it runs on.
//!
//! Linux draws its docks as layer-shell surfaces instead, so nothing here is
//! reachable in a Linux build.
#![allow(
    dead_code,
    reason = "used by hosts whose dock is a top-level window (Windows, macOS), not by Linux"
)]

use crate::gfx::{
    App, DockAlign, DockLength, Edge, Event, GfxError, GfxErrorKind, Monitor, MonitorInfo,
    WindowOptions,
};

/// A rectangle in a y-down coordinate space (the host says which unit).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rect {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

impl Rect {
    pub(crate) fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Every coordinate multiplied by `factor` (logical to physical pixels
    /// at `factor` = DPI / 96, and back with its reciprocal).
    #[must_use]
    pub(crate) fn scaled(self, factor: f64) -> Self {
        Self::new(
            self.x * factor,
            self.y * factor,
            self.width * factor,
            self.height * factor,
        )
    }
}

/// What a dock looks like, independent of the monitor it is on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DockShape {
    pub(crate) edge: Edge,
    /// Logical pixels across the edge.
    pub(crate) thickness: f64,
    pub(crate) length: DockLength,
    pub(crate) align: DockAlign,
}

impl DockShape {
    /// The same shape with every length multiplied by `factor`.
    #[must_use]
    pub(crate) fn scaled(self, factor: f64) -> Self {
        Self {
            thickness: self.thickness * factor,
            length: match self.length {
                DockLength::Full => DockLength::Full,
                #[expect(clippy::cast_possible_truncation, reason = "pixel lengths fit an f32")]
                DockLength::Fixed(length) => DockLength::Fixed((f64::from(length) * factor) as f32),
            },
            ..self
        }
    }

    /// The dock's rectangle against `area`'s `edge`: `thickness` across the
    /// edge (at most the area), `length` along it (clamped to the edge),
    /// placed by `align`.
    pub(crate) fn rect(&self, area: Rect) -> Rect {
        let along_x = matches!(self.edge, Edge::Top | Edge::Bottom);
        let (span, depth) = if along_x {
            (area.width, area.height)
        } else {
            (area.height, area.width)
        };
        let thickness = self.thickness.clamp(0.0, depth.max(0.0));
        let length = match self.length {
            DockLength::Full => span,
            DockLength::Fixed(length) => f64::from(length).min(span),
        };
        let offset = match self.align {
            DockAlign::Start => 0.0,
            DockAlign::Center => (span - length) / 2.0,
            DockAlign::End => span - length,
        };
        match self.edge {
            Edge::Bottom => Rect::new(
                area.x + offset,
                area.y + area.height - thickness,
                length,
                thickness,
            ),
            Edge::Left => Rect::new(area.x, area.y + offset, thickness, length),
            Edge::Right => Rect::new(
                area.x + area.width - thickness,
                area.y + offset,
                thickness,
                length,
            ),
            Edge::Top => Rect::new(area.x + offset, area.y, length, thickness),
        }
    }
}

/// Which of `names` a dock asking for `monitor` goes to, and whether that
/// is a fallback to `primary` because the named monitor is not connected.
pub(crate) fn pick(names: &[String], primary: usize, monitor: &Monitor) -> (usize, bool) {
    match monitor {
        Monitor::Named(name) => names
            .iter()
            .position(|candidate| candidate == name)
            .map_or((primary, true), |index| (index, false)),
        Monitor::Primary => (primary, false),
    }
}

/// The native half of a window dock: the host's monitor list and the calls
/// that turn an ordinary window into a dock.
pub(crate) trait DockHost {
    /// The monitors' names, in the host's order.
    fn monitors(&mut self) -> Vec<String>;
    /// The index of the desktop's primary monitor in [`Self::monitors`].
    fn primary(&self) -> usize;
    /// Where the window should first appear on `monitor`, in the logical
    /// y-down units eframe positions windows in.
    fn initial(&self, monitor: usize, shape: &DockShape) -> Rect;
    /// Take over the created window.
    fn attach(&mut self, window: raw_window_handle::RawWindowHandle) -> Result<(), GfxError>;
    /// Put the attached window on `monitor` with `shape`.
    fn place(&mut self, monitor: usize, shape: &DockShape) -> Result<(), GfxError>;
    /// Give back whatever the dock reserved; the window is about to close.
    fn detach(&mut self);
}

/// A running window dock's state, held by the eframe adapter.
pub(crate) struct WindowDock {
    host: Box<dyn DockHost>,
    shape: DockShape,
    monitor: usize,
    names: Vec<String>,
    /// The `Event::Monitor` to deliver next frame.
    pending: Option<Event>,
}

impl WindowDock {
    /// The monitor list for this frame (refreshed: monitors come and go).
    pub(crate) fn monitors(&mut self) -> Vec<MonitorInfo> {
        self.names = self.host.monitors();
        self.monitor = self.monitor.min(self.names.len().saturating_sub(1));
        self.names
            .iter()
            .enumerate()
            .map(|(index, name)| MonitorInfo::new(name.clone(), index == self.monitor))
            .collect()
    }

    /// Hand the created window to the host and put it in place.
    pub(crate) fn attach(
        &mut self,
        window: raw_window_handle::RawWindowHandle,
    ) -> Result<(), GfxError> {
        self.host.attach(window)?;
        self.host.place(self.monitor, &self.shape)
    }

    pub(crate) fn take_event(&mut self) -> Option<Event> {
        self.pending.take()
    }

    /// Apply a frame's `set_monitor` / `set_dock_length` requests. A failed
    /// native move leaves the dock where it was.
    pub(crate) fn update(&mut self, monitor: Option<Monitor>, length: Option<DockLength>) {
        let mut shape = self.shape;
        if let Some(length) = length {
            shape.length = length;
        }
        let (index, fallback) = match &monitor {
            Some(monitor) => pick(&self.names, self.host.primary(), monitor),
            None => (self.monitor, false),
        };
        if monitor.is_none() && shape == self.shape {
            return;
        }
        if self.host.place(index, &shape).is_ok() {
            self.shape = shape;
            if monitor.is_some() {
                self.monitor = index;
                self.pending = Some(monitor_event(&self.names, index, fallback));
            }
        }
    }

    pub(crate) fn detach(&mut self) {
        self.host.detach();
    }
}

fn monitor_event(names: &[String], index: usize, fallback: bool) -> Event {
    Event::Monitor {
        name: names.get(index).cloned().unwrap_or_default(),
        fallback,
    }
}

/// Run `app` as a dock in an eframe window driven by `host`.
pub(crate) fn run(
    options: &WindowOptions,
    edge: Edge,
    thickness: f32,
    mut host: Box<dyn DockHost>,
    app: Box<dyn App>,
) -> Result<(), GfxError> {
    let shape = DockShape {
        edge,
        thickness: f64::from(thickness),
        length: options.dock_length,
        align: options.dock_align,
    };
    let names = host.monitors();
    if names.is_empty() {
        return Err(GfxError::new(
            GfxErrorKind::Backend,
            "Placement::Dock: no monitor is connected",
        ));
    }
    let (monitor, fallback) = pick(&names, host.primary(), &options.monitor);
    let start = host.initial(monitor, &shape);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "screen coordinates fit an f32"
    )]
    let viewport = egui::ViewportBuilder::default()
        .with_title(options.title.clone())
        .with_position([start.x as f32, start.y as f32])
        .with_inner_size([start.width as f32, start.height as f32])
        .with_decorations(false)
        .with_resizable(false)
        .with_transparent(options.transparent)
        .with_mouse_passthrough(options.click_through)
        .with_window_level(egui::WindowLevel::AlwaysOnTop)
        .with_taskbar(false)
        .with_active(false);
    let native = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Wgpu,
        ..eframe::NativeOptions::default()
    };
    let dock = WindowDock {
        host,
        shape,
        monitor,
        pending: Some(monitor_event(&names, monitor, fallback)),
        names,
    };
    super::run_eframe_with(options, native, app, Some(dock))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(edge: Edge, length: DockLength, align: DockAlign) -> DockShape {
        DockShape {
            edge,
            thickness: 30.0,
            length,
            align,
        }
    }

    const AREA: Rect = Rect {
        x: 100.0,
        y: 50.0,
        width: 1000.0,
        height: 800.0,
    };

    #[test]
    fn a_full_dock_spans_its_edge() {
        let full = |edge| shape(edge, DockLength::Full, DockAlign::Center).rect(AREA);
        assert_eq!(full(Edge::Top), Rect::new(100.0, 50.0, 1000.0, 30.0));
        assert_eq!(full(Edge::Bottom), Rect::new(100.0, 820.0, 1000.0, 30.0));
        assert_eq!(full(Edge::Left), Rect::new(100.0, 50.0, 30.0, 800.0));
        assert_eq!(full(Edge::Right), Rect::new(1070.0, 50.0, 30.0, 800.0));
    }

    #[test]
    fn a_fixed_dock_is_aligned_along_its_edge_and_clamped() {
        let fixed = |edge, align| shape(edge, DockLength::Fixed(200.0), align).rect(AREA);
        assert_eq!(
            fixed(Edge::Top, DockAlign::Start),
            Rect::new(100.0, 50.0, 200.0, 30.0)
        );
        assert_eq!(
            fixed(Edge::Top, DockAlign::Center),
            Rect::new(500.0, 50.0, 200.0, 30.0)
        );
        assert_eq!(
            fixed(Edge::Top, DockAlign::End),
            Rect::new(900.0, 50.0, 200.0, 30.0)
        );
        assert_eq!(
            fixed(Edge::Right, DockAlign::End),
            Rect::new(1070.0, 650.0, 30.0, 200.0)
        );
        let long = shape(Edge::Left, DockLength::Fixed(5000.0), DockAlign::Center).rect(AREA);
        assert_eq!(
            long,
            Rect::new(100.0, 50.0, 30.0, 800.0),
            "clamped to the edge"
        );
    }

    #[test]
    fn scaling_turns_logical_into_physical_pixels() {
        let at_150 = shape(Edge::Top, DockLength::Fixed(200.0), DockAlign::Start).scaled(1.5);
        assert!((at_150.thickness - 45.0).abs() < 1e-9);
        assert_eq!(at_150.length, DockLength::Fixed(300.0));
        assert_eq!(
            Rect::new(10.0, 20.0, 30.0, 40.0).scaled(2.0),
            Rect::new(20.0, 40.0, 60.0, 80.0)
        );
    }

    #[test]
    fn a_missing_named_monitor_falls_back_to_the_primary() {
        let names = vec!["A".to_owned(), "B".to_owned()];
        assert_eq!(pick(&names, 1, &Monitor::Primary), (1, false));
        assert_eq!(pick(&names, 1, &Monitor::Named("A".into())), (0, false));
        assert_eq!(pick(&names, 1, &Monitor::Named("C".into())), (1, true));
    }
}
