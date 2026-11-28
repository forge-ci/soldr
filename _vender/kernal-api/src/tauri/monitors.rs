//! Read-only display geometry for placing a window: each monitor's bounds and
//! work area in logical pixels, answered on the native event loop.
//!
//! The toolkit already measures the work area per host (Windows `rcWork`,
//! macOS `visibleFrame`, GDK's work area on X11), so this module only
//! converts its physical rectangles. No backend type crosses the facade.

use tauri_runtime::monitor::Monitor;
use tauri_runtime::RuntimeHandle as _;

use super::{BestEffort, ExternalWebviewClient, WebviewError};
use crate::async_engine;

/// A rectangle in logical pixels on the virtual desktop; `(x, y)` is its
/// top-left corner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WebviewLogicalRect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub width: f64,
    /// Height.
    pub height: f64,
}

impl WebviewLogicalRect {
    /// Whether `other` lies entirely inside this rectangle.
    pub fn contains(&self, other: &Self) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.x + other.width <= self.x + self.width
            && other.y + other.height <= self.y + self.height
    }
}

/// One monitor as the native toolkit reports it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WebviewMonitor {
    /// The whole monitor.
    pub bounds: WebviewLogicalRect,
    /// The usable area, excluding taskbar, Dock, menu bar and panels.
    pub work_area: WebviewLogicalRect,
    /// Physical pixels per logical pixel on this monitor.
    pub scale_factor: f64,
    /// Whether this is the primary monitor. Exactly one monitor in a
    /// non-empty answer is primary.
    pub primary: bool,
}

fn logical(x: i32, y: i32, width: u32, height: u32, scale: f64) -> WebviewLogicalRect {
    WebviewLogicalRect {
        x: f64::from(x) / scale,
        y: f64::from(y) / scale,
        width: f64::from(width) / scale,
        height: f64::from(height) / scale,
    }
}

/// Converts toolkit monitors to logical rectangles. The toolkit's primary
/// monitor is matched by physical origin and size; when the toolkit names
/// none, the first reported monitor is primary. A work area the toolkit
/// leaves empty or outside its monitor is replaced by the bounds.
pub(crate) fn convert(
    monitors: &[Monitor],
    primary: Option<&Monitor>,
) -> Result<Vec<WebviewMonitor>, WebviewError> {
    let primary_index = primary
        .and_then(|primary| {
            monitors.iter().position(|monitor| {
                monitor.position == primary.position && monitor.size == primary.size
            })
        })
        .unwrap_or(0);
    monitors
        .iter()
        .enumerate()
        .map(|(index, monitor)| {
            let scale = monitor.scale_factor;
            if !(scale.is_finite() && scale > 0.0) {
                return Err(WebviewError::HostFailure(format!(
                    "invalid monitor scale factor {scale}"
                )));
            }
            let bounds = logical(
                monitor.position.x,
                monitor.position.y,
                monitor.size.width,
                monitor.size.height,
                scale,
            );
            let area = &monitor.work_area;
            let work_area = logical(
                area.position.x,
                area.position.y,
                area.size.width,
                area.size.height,
                scale,
            );
            let work_area =
                if work_area.width > 0.0 && work_area.height > 0.0 && bounds.contains(&work_area) {
                    work_area
                } else {
                    bounds
                };
            Ok(WebviewMonitor {
                bounds,
                work_area,
                scale_factor: scale,
                primary: index == primary_index,
            })
        })
        .collect()
}

impl ExternalWebviewClient {
    /// Best effort: the monitors a window can be placed on, so a widget can
    /// position itself, for example at its work area's bottom-right corner.
    ///
    /// Returns `Ok(None)` without any native effect where the compositor
    /// places windows (Wayland; see [`Self::window_support`]): there are no
    /// global coordinates to place against. Otherwise the answer comes from
    /// the native event loop.
    pub async fn monitors(&self) -> Result<Option<Vec<WebviewMonitor>>, WebviewError> {
        if self.service.backend.support.position == BestEffort::Unsupported {
            return Ok(None);
        }
        let (sender, receiver) = async_engine::oneshot_channel();
        let wry = self.service.backend.wry.clone();
        let ui_wry = wry.clone();
        wry.run_on_main_thread(move || {
            // The toolkit's monitor getters read event-loop state; they are
            // only sound on its own thread.
            let host_failure = |error: tauri_runtime::Error| {
                WebviewError::HostFailure(format!("monitor query failed: {error}"))
            };
            let result = ui_wry
                .available_monitors()
                .map_err(host_failure)
                .and_then(|monitors| {
                    let primary = ui_wry.primary_monitor().map_err(host_failure)?;
                    convert(&monitors, primary.as_ref())
                });
            let _ = sender.send(result);
        })
        .map_err(|error| WebviewError::HostFailure(error.to_string()))?;
        let monitors = receiver.await.map_err(|_| {
            WebviewError::HostFailure("event loop stopped during monitor query".into())
        })??;
        if monitors.is_empty() {
            return Err(WebviewError::HostFailure(
                "the toolkit reported no monitors".into(),
            ));
        }
        Ok(Some(monitors))
    }
}

#[cfg(test)]
mod tests {
    use tauri_runtime::dpi::{PhysicalPosition, PhysicalRect, PhysicalSize};

    use super::*;

    fn monitor(x: i32, width: u32, work_height: u32, scale: f64) -> Monitor {
        Monitor {
            name: None,
            size: PhysicalSize::new(width, 1200),
            position: PhysicalPosition::new(x, 0),
            work_area: PhysicalRect {
                position: PhysicalPosition::new(x, 0),
                size: PhysicalSize::new(width, work_height),
            },
            scale_factor: scale,
        }
    }

    #[test]
    fn converts_physical_geometry_and_marks_the_toolkit_primary() {
        let left = monitor(0, 1920, 1140, 1.0);
        let right = monitor(1920, 2400, 1100, 2.0);
        let converted = convert(&[left, right.clone()], Some(&right)).unwrap();
        assert_eq!(
            converted.iter().filter(|monitor| monitor.primary).count(),
            1
        );
        assert!(converted[1].primary);
        assert_eq!(
            converted[1].bounds,
            WebviewLogicalRect {
                x: 960.0,
                y: 0.0,
                width: 1200.0,
                height: 600.0
            }
        );
        assert_eq!(converted[1].work_area.height, 550.0);
        assert!(converted
            .iter()
            .all(|monitor| monitor.bounds.contains(&monitor.work_area)));
    }

    #[test]
    fn without_a_toolkit_primary_the_first_monitor_is_primary() {
        let converted = convert(
            &[monitor(0, 800, 600, 1.0), monitor(800, 800, 600, 1.0)],
            None,
        )
        .unwrap();
        assert_eq!(
            converted
                .iter()
                .map(|monitor| monitor.primary)
                .collect::<Vec<_>>(),
            [true, false]
        );
    }

    #[test]
    fn an_empty_or_escaping_work_area_falls_back_to_the_bounds() {
        let mut escaping = monitor(0, 800, 600, 1.0);
        escaping.work_area.size = PhysicalSize::new(900, 600);
        let empty = monitor(800, 800, 0, 1.0);
        let converted = convert(&[escaping, empty], None).unwrap();
        assert!(converted
            .iter()
            .all(|monitor| monitor.work_area == monitor.bounds));
    }

    #[test]
    fn rejects_a_nonpositive_scale() {
        assert!(matches!(
            convert(&[monitor(0, 800, 600, 0.0)], None),
            Err(WebviewError::HostFailure(_))
        ));
    }
}
