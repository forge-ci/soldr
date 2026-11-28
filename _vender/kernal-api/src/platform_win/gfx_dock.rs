//! `gfx` docks on Windows: a borderless, topmost eframe window registered as
//! a shell AppBar (`SHAppBarMessage`), so the shell reserves its band of the
//! work area and maximised windows stop at it.
//!
//! The AppBar protocol works in physical pixels: the band is the monitor's
//! rectangle (from `GetMonitorInfoW`) cut to the dock's thickness scaled by
//! the monitor's effective DPI, negotiated with `ABM_QUERYPOS` and committed
//! with `ABM_SETPOS`. The whole edge is reserved even for a shorter
//! `DockLength::Fixed` dock, because an AppBar reserves a band, not a
//! segment; the window itself is shortened and aligned inside the band.
//! `ABM_REMOVE` gives the band back when the window closes.
//!
//! Not handled: the shell's `ABN_POSCHANGED` / `ABN_FULLSCREENAPP`
//! notifications (the window has no callback message), so a taskbar moved
//! while the dock runs is only accounted for at the next `set_monitor` or
//! `set_dock_length`.

use std::num::NonZeroIsize;

use raw_window_handle::RawWindowHandle;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Shell::{
    SHAppBarMessage, ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE,
    ABM_SETPOS, APPBARDATA,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SetWindowPos, HWND_TOPMOST, MONITORINFOF_PRIMARY, SWP_NOACTIVATE, WM_APP,
};

use crate::gfx::{App, Edge, GfxError, GfxErrorKind, Support, WindowOptions};
use crate::gfx_egui::window_dock::{self, DockHost, DockShape, Rect};

/// The DPI Windows calls 100% scale.
const BASE_DPI: f64 = 96.0;

pub(crate) fn support() -> Support {
    Support::Supported
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
    window_dock::run(options, edge, thickness, Box::new(AppBar::default()), app)
}

/// One monitor as the AppBar protocol sees it.
#[derive(Clone, Debug)]
struct Display {
    name: String,
    /// The whole monitor, in physical pixels.
    rect: Rect,
    /// Physical pixels per logical pixel (effective DPI / 96).
    scale: f64,
    primary: bool,
}

#[derive(Default)]
struct AppBar {
    displays: Vec<Display>,
    window: Option<HWND>,
    registered: bool,
}

impl AppBar {
    fn data(&self, window: HWND) -> APPBARDATA {
        APPBARDATA {
            cbSize: u32::try_from(std::mem::size_of::<APPBARDATA>()).unwrap_or(u32::MAX),
            hWnd: window,
            uCallbackMessage: WM_APP + 0x0D0C,
            ..APPBARDATA::default()
        }
    }
}

impl DockHost for AppBar {
    fn monitors(&mut self) -> Vec<String> {
        self.displays = displays();
        self.displays.iter().map(|display| display.name.clone()).collect()
    }

    fn primary(&self) -> usize {
        self.displays.iter().position(|display| display.primary).unwrap_or(0)
    }

    fn initial(&self, monitor: usize, shape: &DockShape) -> Rect {
        self.displays.get(monitor).map_or(Rect::new(0.0, 0.0, 800.0, shape.thickness), |display| {
            shape.rect(display.rect.scaled(1.0 / display.scale))
        })
    }

    fn attach(&mut self, window: RawWindowHandle) -> Result<(), GfxError> {
        let RawWindowHandle::Win32(handle) = window else {
            return Err(GfxError::new(GfxErrorKind::Backend, "Placement::Dock: not a Win32 window"));
        };
        let window = hwnd(handle.hwnd);
        let mut data = self.data(window);
        // SAFETY: `data` is a valid, sized APPBARDATA for this live window.
        if unsafe { SHAppBarMessage(ABM_NEW, &raw mut data) } == 0 {
            return Err(GfxError::new(
                GfxErrorKind::Backend,
                "Placement::Dock: the shell refused to register the AppBar (ABM_NEW)",
            ));
        }
        self.window = Some(window);
        self.registered = true;
        Ok(())
    }

    fn place(&mut self, monitor: usize, shape: &DockShape) -> Result<(), GfxError> {
        let (Some(window), Some(display)) = (self.window, self.displays.get(monitor)) else {
            return Err(GfxError::new(GfxErrorKind::Backend, "Placement::Dock: no window or monitor"));
        };
        let physical = shape.scaled(display.scale);
        let thickness = physical.thickness.round();
        let mut data = self.data(window);
        data.uEdge = abe(shape.edge);
        data.rc = to_rect(band(display.rect, shape.edge, thickness));
        // SAFETY: `data` is a valid APPBARDATA for the registered window; the
        // shell adjusts `rc` in place.
        unsafe { SHAppBarMessage(ABM_QUERYPOS, &raw mut data) };
        // QUERYPOS moves the edge the shell must keep (a taskbar on the same
        // edge); restore the thickness from that edge.
        data.rc = to_rect(band(from_rect(data.rc), shape.edge, thickness));
        // SAFETY: as above; SETPOS commits the reservation.
        unsafe { SHAppBarMessage(ABM_SETPOS, &raw mut data) };
        let reserved = from_rect(data.rc);
        let window_rect = DockShape { thickness, ..physical }.rect(reserved);
        let [x, y, width, height] = pixels(window_rect);
        // SAFETY: `window` is the live window this dock owns.
        unsafe { SetWindowPos(window, Some(HWND_TOPMOST), x, y, width, height, SWP_NOACTIVATE) }
            .map_err(|error| GfxError::new(GfxErrorKind::Backend, format!("SetWindowPos: {error}")))
    }

    fn detach(&mut self) {
        if let (Some(window), true) = (self.window, self.registered) {
            let mut data = self.data(window);
            // SAFETY: removes the AppBar this dock registered.
            unsafe { SHAppBarMessage(ABM_REMOVE, &raw mut data) };
            self.registered = false;
        }
    }
}

impl Drop for AppBar {
    fn drop(&mut self) {
        // A dock torn down without eframe's exit hook still frees its band.
        self.detach();
    }
}

fn hwnd(handle: NonZeroIsize) -> HWND {
    HWND(handle.get() as *mut core::ffi::c_void)
}

fn abe(edge: Edge) -> u32 {
    match edge {
        Edge::Top => ABE_TOP,
        Edge::Bottom => ABE_BOTTOM,
        Edge::Left => ABE_LEFT,
        Edge::Right => ABE_RIGHT,
    }
}

/// The full-length band `thickness` physical pixels deep along `edge` of `area`.
fn band(area: Rect, edge: Edge, thickness: f64) -> Rect {
    DockShape {
        edge,
        thickness,
        length: crate::gfx::DockLength::Full,
        align: crate::gfx::DockAlign::Start,
    }
    .rect(area)
}

/// Whole physical pixels `[x, y, width, height]`.
#[expect(clippy::cast_possible_truncation, reason = "screen coordinates fit an i32")]
fn pixels(rect: Rect) -> [i32; 4] {
    [rect.x.round() as i32, rect.y.round() as i32, rect.width.round() as i32, rect.height.round() as i32]
}

fn to_rect(rect: Rect) -> RECT {
    let [x, y, width, height] = pixels(rect);
    RECT { left: x, top: y, right: x + width, bottom: y + height }
}

fn from_rect(rect: RECT) -> Rect {
    Rect::new(
        f64::from(rect.left),
        f64::from(rect.top),
        f64::from(rect.right - rect.left),
        f64::from(rect.bottom - rect.top),
    )
}

/// Every monitor, primary flagged, in the order the system enumerates them.
fn displays() -> Vec<Display> {
    unsafe extern "system" fn collect(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `&mut Vec<HMONITOR>` passed below, alive for
        // the duration of `EnumDisplayMonitors`.
        let monitors = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
        monitors.push(monitor);
        BOOL::from(true)
    }
    let mut monitors: Vec<HMONITOR> = Vec::new();
    // SAFETY: the callback only pushes into `monitors`, which outlives the call.
    let _ = unsafe {
        EnumDisplayMonitors(None, None, Some(collect), LPARAM(&raw mut monitors as isize))
    };
    monitors.into_iter().filter_map(display).collect()
}

fn display(monitor: HMONITOR) -> Option<Display> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = u32::try_from(std::mem::size_of::<MONITORINFOEXW>()).ok()?;
    // SAFETY: `info` is a sized MONITORINFOEXW, which GetMonitorInfoW accepts
    // through its MONITORINFO header.
    if !unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast::<MONITORINFO>()) }.as_bool() {
        return None;
    }
    let (mut dpi_x, mut dpi_y) = (0, 0);
    // SAFETY: plain out-parameters.
    let dpi = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &raw mut dpi_x, &raw mut dpi_y) }
        .map_or(BASE_DPI, |()| f64::from(dpi_x));
    let end = info.szDevice.iter().position(|&unit| unit == 0).unwrap_or(info.szDevice.len());
    Some(Display {
        name: String::from_utf16_lossy(&info.szDevice[..end]),
        rect: from_rect(info.monitorInfo.rcMonitor),
        scale: scale_for(dpi),
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    })
}

/// Physical pixels per logical pixel at `dpi` (100% when the DPI is unknown).
fn scale_for(dpi: f64) -> f64 {
    if dpi > 0.0 { dpi / BASE_DPI } else { 1.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_band_is_a_full_edge_in_physical_pixels() {
        // A 150% monitor to the right of the primary.
        let monitor = Rect::new(1920.0, 0.0, 2560.0, 1440.0);
        let thickness = 30.0 * scale_for(144.0);
        assert_eq!(to_rect(band(monitor, Edge::Top, thickness)), RECT { left: 1920, top: 0, right: 4480, bottom: 45 });
        assert_eq!(to_rect(band(monitor, Edge::Right, thickness)), RECT { left: 4435, top: 0, right: 4480, bottom: 1440 });
        assert_eq!(scale_for(0.0), 1.0);
    }

    #[test]
    fn rect_round_trips() {
        let rect = RECT { left: -1280, top: 10, right: 0, bottom: 1034 };
        assert_eq!(to_rect(from_rect(rect)), rect);
    }
}
