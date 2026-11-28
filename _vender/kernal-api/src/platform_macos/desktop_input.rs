//! Quartz event injection (`CGEventPost` at the HID tap). Without Accessibility permission the
//! system drops the events silently, so success means "posted", not "delivered".
use crate::desktop_input::{InputError, MouseButton};
use std::ffi::c_void;

type CGEventRef = *mut c_void;
type CGEventSourceRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventCreateKeyboardEvent(source: CGEventSourceRef, key: u16, down: bool) -> CGEventRef;
    fn CGEventKeyboardSetUnicodeString(event: CGEventRef, length: usize, string: *const u16);
    fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        kind: u32,
        position: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventPost(tap: u32, event: CGEventRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: *const c_void);
}

const HID_EVENT_TAP: u32 = 0;
const MOUSE_MOVED: u32 = 5;
const LEFT_DOWN: u32 = 1;
const LEFT_UP: u32 = 2;
const RIGHT_DOWN: u32 = 3;
const RIGHT_UP: u32 = 4;
const OTHER_DOWN: u32 = 25;
const OTHER_UP: u32 = 26;
/// Quartz accepts at most 20 UTF-16 units per keyboard event.
const UNITS_PER_EVENT: usize = 20;

/// Post and release one created event. A null event means Quartz refused to build it.
fn post(event: CGEventRef) -> Result<(), InputError> {
    if event.is_null() {
        return Err(InputError::Unavailable);
    }
    // SAFETY: `event` is a non-null, owned CGEvent released exactly once here.
    unsafe {
        CGEventPost(HID_EVENT_TAP, event);
        CFRelease(event);
    }
    Ok(())
}

fn pointer_location() -> Result<CGPoint, InputError> {
    // SAFETY: a null source asks Quartz for the current state; the event is released here.
    unsafe {
        let event = CGEventCreate(std::ptr::null_mut());
        if event.is_null() {
            return Err(InputError::Unavailable);
        }
        let point = CGEventGetLocation(event);
        CFRelease(event);
        Ok(point)
    }
}

pub(crate) async fn type_text(text: &str) -> Result<(), InputError> {
    let units: Vec<u16> = text.encode_utf16().collect();
    for chunk in units.chunks(UNITS_PER_EVENT) {
        for down in [true, false] {
            // SAFETY: `chunk` is a live UTF-16 buffer of `chunk.len()` units.
            let event = unsafe {
                let event = CGEventCreateKeyboardEvent(std::ptr::null_mut(), 0, down);
                if !event.is_null() {
                    CGEventKeyboardSetUnicodeString(event, chunk.len(), chunk.as_ptr());
                }
                event
            };
            post(event)?;
        }
    }
    Ok(())
}

pub(crate) async fn pointer_move_by(dx: i32, dy: i32) -> Result<(), InputError> {
    let from = pointer_location()?;
    let to = CGPoint {
        x: from.x + f64::from(dx),
        y: from.y + f64::from(dy),
    };
    // SAFETY: plain value arguments; the returned event is checked and released by `post`.
    post(unsafe { CGEventCreateMouseEvent(std::ptr::null_mut(), MOUSE_MOVED, to, 0) })
}

pub(crate) async fn click(button: MouseButton) -> Result<(), InputError> {
    let at = pointer_location()?;
    let (down, up, index) = match button {
        MouseButton::Left => (LEFT_DOWN, LEFT_UP, 0),
        MouseButton::Right => (RIGHT_DOWN, RIGHT_UP, 1),
        MouseButton::Middle => (OTHER_DOWN, OTHER_UP, 2),
    };
    for kind in [down, up] {
        // SAFETY: plain value arguments; the returned event is checked and released by `post`.
        post(unsafe { CGEventCreateMouseEvent(std::ptr::null_mut(), kind, at, index) })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Moves the real pointer of whatever desktop runs the suite, under every
    // other test in parallel; run it deliberately on an interactive session.
    #[tokio::test]
    #[ignore = "injects real input into the host desktop"]
    async fn pointer_round_trip_posts() {
        pointer_move_by(5, 5).await.expect("move");
        pointer_move_by(-5, -5).await.expect("move back");
    }
}
