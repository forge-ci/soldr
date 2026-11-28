//! `gfx` docks on macOS, best effort: a borderless eframe window raised to
//! status-bar window level along an edge of a screen's visible frame.
//!
//! AppKit has no exclusive zone. Nothing reserves the dock's band: other
//! windows can be placed or maximised beneath it, and `NSScreen.visibleFrame`
//! (which excludes only the menu bar and the system Dock) does not shrink.
//! So [`support`] reports [`Support::BestEffort`]: the dock appears and draws
//! where asked, at `NSStatusWindowLevel`, on every Space, outside the window
//! cycle; it does not keep other windows out of its way.
//!
//! The window is winit's `NSWindow`, not an `NSPanel`, so AppKit may still
//! activate the application when it is clicked; eframe asks for it not to be
//! focused when it opens.

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSScreen, NSStatusWindowLevel, NSView, NSWindow, NSWindowCollectionBehavior};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use raw_window_handle::RawWindowHandle;

use crate::gfx::{App, Edge, GfxError, GfxErrorKind, Support, WindowOptions};
use crate::gfx_egui::window_dock::{self, DockHost, DockShape, Rect};

pub(crate) fn support() -> Support {
    Support::BestEffort
}

/// A window dock has no panels (they are layer-shell surfaces on Linux).
pub(crate) fn panel_support() -> Support {
    Support::Unsupported
}

pub(crate) fn run(
    options: &WindowOptions,
    edge: Edge,
    thickness: f32,
    app: Box<dyn App>,
) -> Result<(), GfxError> {
    let main_thread = MainThreadMarker::new().ok_or_else(|| {
        GfxError::new(GfxErrorKind::Backend, "Placement::Dock must run on the main thread on macOS")
    })?;
    let host = StatusWindow {
        main_thread,
        screens: Vec::new(),
        window: None,
    };
    window_dock::run(options, edge, thickness, Box::new(host), app)
}

/// One screen's usable area in y-down points from the primary screen's top.
struct Screen {
    name: String,
    visible: Rect,
}

struct StatusWindow {
    main_thread: MainThreadMarker,
    screens: Vec<Screen>,
    window: Option<Retained<NSWindow>>,
}

/// The height of the primary screen (the one with the menu bar), whose
/// bottom-left corner is the origin of AppKit's y-up screen space.
fn primary_height(main_thread: MainThreadMarker) -> f64 {
    NSScreen::screens(main_thread)
        .firstObject()
        .map_or(0.0, |screen| screen.frame().size.height)
}

/// AppKit's y-up rectangle as a y-down one (and back: the flip is its own
/// inverse) against a primary screen `primary_height` points tall.
fn flip(rect: Rect, primary_height: f64) -> Rect {
    Rect::new(rect.x, primary_height - (rect.y + rect.height), rect.width, rect.height)
}

impl DockHost for StatusWindow {
    fn monitors(&mut self) -> Vec<String> {
        let height = primary_height(self.main_thread);
        self.screens = NSScreen::screens(self.main_thread)
            .iter()
            .map(|screen| {
                let frame = screen.visibleFrame();
                Screen {
                    name: screen.localizedName().to_string(),
                    visible: flip(
                        Rect::new(frame.origin.x, frame.origin.y, frame.size.width, frame.size.height),
                        height,
                    ),
                }
            })
            .collect();
        self.screens.iter().map(|screen| screen.name.clone()).collect()
    }

    fn primary(&self) -> usize {
        // `NSScreen.screens` lists the menu-bar screen first.
        0
    }

    fn initial(&self, monitor: usize, shape: &DockShape) -> Rect {
        self.screens
            .get(monitor)
            .map_or(Rect::new(0.0, 0.0, 800.0, shape.thickness), |screen| shape.rect(screen.visible))
    }

    fn attach(&mut self, window: RawWindowHandle) -> Result<(), GfxError> {
        let RawWindowHandle::AppKit(handle) = window else {
            return Err(GfxError::new(GfxErrorKind::Backend, "Placement::Dock: not an AppKit window"));
        };
        // SAFETY: the AppKit handle's `ns_view` is the live view of the window
        // eframe just created, and this runs on the main thread that owns it.
        let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
        let window = view
            .window()
            .ok_or_else(|| GfxError::new(GfxErrorKind::Backend, "Placement::Dock: the view has no window"))?;
        window.setLevel(NSStatusWindowLevel);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        window.setHidesOnDeactivate(false);
        self.window = Some(window);
        Ok(())
    }

    fn place(&mut self, monitor: usize, shape: &DockShape) -> Result<(), GfxError> {
        let (Some(window), Some(screen)) = (&self.window, self.screens.get(monitor)) else {
            return Err(GfxError::new(GfxErrorKind::Backend, "Placement::Dock: no window or screen"));
        };
        let rect = flip(shape.rect(screen.visible), primary_height(self.main_thread));
        window.setFrame_display(
            NSRect::new(NSPoint::new(rect.x, rect.y), NSSize::new(rect.width, rect.height)),
            true,
        );
        Ok(())
    }

    fn detach(&mut self) {
        // Nothing was reserved.
        self.window = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flipping_converts_between_appkit_and_y_down_space() {
        // A 1440x900 primary with a 25pt menu bar: its visible frame in
        // AppKit space starts at y = 0 and is 875 tall.
        let visible = flip(Rect::new(0.0, 0.0, 1440.0, 875.0), 900.0);
        assert_eq!(visible, Rect::new(0.0, 25.0, 1440.0, 875.0));
        assert_eq!(flip(visible, 900.0), Rect::new(0.0, 0.0, 1440.0, 875.0));
        // A screen above the primary has a negative y-down origin.
        assert_eq!(flip(Rect::new(0.0, 900.0, 1920.0, 1080.0), 900.0).y, -1080.0);
    }
}
