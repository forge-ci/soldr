//! The X11 `_NET_WM_ICON` backend (#577).
//!
//! # Why this is a real backend and OSC 1 is not
//!
//! `_NET_WM_ICON` carries actual pixels. The window manager scales them for
//! the taskbar, the Alt-Tab switcher, and the title bar, so an icon set this
//! way is the icon the user sees. OSC 1 carries a *name* that most emulators
//! ignore — it is the fallback for hosts with no property to write.
//!
//! # Finding the window
//!
//! X11 has no "the window my process is drawing in" call: a terminal emulator
//! owns its window, and this process is a child holding a pty, not an X
//! client with a window of its own.
//!
//! The `WINDOWID` environment variable is the long-standing convention for
//! bridging that gap — xterm, urxvt, and most emulators that inherit its
//! behaviour export it to the shell they spawn. When it is absent there is no
//! honest way to guess which window belongs to this process, and guessing
//! wrong means writing an icon onto someone else's window. So an absent
//! `WINDOWID` is reported as unsupported rather than searched for heuristically.
//!
//! # The property format
//!
//! `_NET_WM_ICON` is `CARDINAL[]`: width, height, then `width * height`
//! pixels, row-major, each `0xAARRGGBB`. The spec permits several images
//! concatenated so the WM can pick a size; one is written here, and the WM
//! scales it.

use std::path::Path;

use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode, Window};
// `change_property32` is a convenience wrapper, not part of the generated
// xproto surface, so it needs its own trait in scope.
use x11rb::wrapper::ConnectionExt as _;

use crate::platform::window_icon::{
    IconDegradedReason, IconError, IconScope, IconSource, IconSupport, IconUnsupportedReason,
};

#[cfg(test)]
#[path = "window_icon/tests_support.rs"]
mod tests_support;

/// Decoded pixels, ready for the property.
#[derive(Debug)]
pub(super) struct Rgba {
    pub width: u32,
    pub height: u32,
    /// Row-major RGBA, four bytes per pixel.
    pub pixels: Vec<u8>,
}

/// Largest icon accepted, per side.
///
/// `_NET_WM_ICON` is sent over the X connection as one property, and a
/// 4096×4096 icon would be 64 MiB on a socket shared with every other request
/// this client makes. Window managers scale down from something far smaller.
const MAX_SIDE: u32 = 512;

/// The window this process should set an icon on, if it can be known.
pub(super) fn window_id() -> Option<Window> {
    parse_window_id(&std::env::var("WINDOWID").ok()?)
}

/// Parse a `WINDOWID` value.
///
/// Split from the env read so it can be tested without mutating
/// process-global state — a test that sets `WINDOWID` is visible to every
/// other test running in the same process, which is how a parallel harness
/// turns one test's fixture into another test's flake.
fn parse_window_id(raw: &str) -> Option<Window> {
    // Emulators export it as decimal; accept hex too because some tools
    // re-export it in the form `xwininfo` prints.
    let trimmed = raw.trim();
    let parsed = trimmed
        .strip_prefix("0x")
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .or_else(|| trimmed.parse::<u32>().ok())?;
    // Zero is the X "no window" sentinel, and a property write against it
    // would be silently accepted by some servers.
    (parsed != 0).then_some(parsed)
}

/// Whether this host can take a real icon.
pub fn icon_support(scope: IconScope) -> IconSupport {
    // A child's window cannot be identified. `WINDOWID` names *this*
    // process's terminal, and mapping a pid to an X window would mean the
    // same heuristic guessing that finding our own window deliberately
    // avoids — with the added cost that guessing wrong writes an icon onto
    // an unrelated application.
    if matches!(scope, IconScope::Child { .. }) {
        return IconSupport::Unsupported(IconUnsupportedReason::LinuxChildScope);
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return IconSupport::Unsupported(IconUnsupportedReason::Wayland);
    }
    if std::env::var_os("DISPLAY").is_none() {
        return IconSupport::Unsupported(IconUnsupportedReason::LinuxNoDisplay);
    }
    if window_id().is_none() {
        // Degraded rather than unsupported: OSC 1 still reaches the terminal
        // even when we cannot identify its window.
        return IconSupport::Degraded(IconDegradedReason::LinuxNameOnly);
    }
    IconSupport::Available
}

/// Write `source` onto this process's terminal window.
pub fn set_icon(scope: IconScope, source: &IconSource) -> Result<(), IconError> {
    if let IconSupport::Unsupported(reason) = icon_support(scope) {
        return Err(IconError::Unsupported(reason));
    }
    let window = window_id().ok_or(IconError::Unsupported(
        IconUnsupportedReason::TargetDisappeared,
    ))?;
    let image = decode(source)?;
    write_property(window, &image)
}

/// Decode an icon source to RGBA.
fn decode(source: &IconSource) -> Result<Rgba, IconError> {
    match source {
        IconSource::Path(path) => {
            let bytes = std::fs::read(path).map_err(|source| IconError::Load {
                path: path.clone(),
                source,
            })?;
            decode_bytes(&bytes, Some(path))
        }
        IconSource::Bytes(bytes) => decode_bytes(bytes, None),
        // A stock name is a theme lookup, not an image. Resolving it would
        // mean reading the user's icon theme, which is a different feature;
        // the OSC 1 fallback already carries the name.
        IconSource::Stock(_) => Err(IconError::Unsupported(
            IconUnsupportedReason::StockNeedsPixels,
        )),
    }
}

/// PNG magic, per the spec's fixed 8-byte signature.
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

fn decode_bytes(bytes: &[u8], path: Option<&Path>) -> Result<Rgba, IconError> {
    if bytes.len() >= PNG_MAGIC.len() && bytes[..PNG_MAGIC.len()] == PNG_MAGIC {
        return decode_png(bytes);
    }
    // An `.ico` embeds either a PNG or a bottom-up DIB. The PNG case is
    // handled by unwrapping to the embedded image; a DIB would need a second
    // decoder, and saying so is more useful than a generic parse failure that
    // sends the caller looking for a corrupt file.
    if let Ok(span) = crate::platform::window_icon::ico::best_image(bytes) {
        let inner = &bytes[span.offset..span.offset + span.len];
        if inner.len() >= PNG_MAGIC.len() && inner[..PNG_MAGIC.len()] == PNG_MAGIC {
            return decode_png(inner);
        }
        return Err(IconError::Unsupported(
            IconUnsupportedReason::UnknownImageFormat,
        ));
    }
    let _ = path;
    Err(IconError::Unsupported(
        IconUnsupportedReason::UnknownImageFormat,
    ))
}

fn decode_png(bytes: &[u8]) -> Result<Rgba, IconError> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder
        .read_info()
        .map_err(|e| IconError::Apply(std::io::Error::other(e.to_string())))?;

    // `read_info` already refused a size with no representation, with this error.
    let size = reader
        .output_buffer_size()
        .ok_or(png::DecodingError::LimitsExceeded)
        .map_err(|e| IconError::Apply(std::io::Error::other(e.to_string())))?;
    let mut buffer = vec![0; size];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|e| IconError::Apply(std::io::Error::other(e.to_string())))?;

    if info.width > MAX_SIDE || info.height > MAX_SIDE {
        return Err(IconError::Unsupported(
            IconUnsupportedReason::OversizedIcon,
        ));
    }

    let pixels = match info.color_type {
        png::ColorType::Rgba => buffer[..info.buffer_size()].to_vec(),
        // Opaque source: synthesize full alpha rather than refusing, because a
        // photo-style PNG with no alpha channel is a perfectly ordinary icon.
        png::ColorType::Rgb => buffer[..info.buffer_size()]
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 0xff])
            .collect(),
        other => {
            let _ = other;
            return Err(IconError::Unsupported(
                IconUnsupportedReason::UnsupportedPngColorType,
            ));
        }
    };

    Ok(Rgba {
        width: info.width,
        height: info.height,
        pixels,
    })
}

/// Pack RGBA bytes into the `0xAARRGGBB` cardinals the property wants.
pub(super) fn to_cardinals(image: &Rgba) -> Vec<u32> {
    let mut data = Vec::with_capacity(2 + (image.width * image.height) as usize);
    data.push(image.width);
    data.push(image.height);
    for pixel in image.pixels.as_chunks::<4>().0 {
        data.push(
            (u32::from(pixel[3]) << 24)
                | (u32::from(pixel[0]) << 16)
                | (u32::from(pixel[1]) << 8)
                | u32::from(pixel[2]),
        );
    }
    data
}

fn write_property(window: Window, image: &Rgba) -> Result<(), IconError> {
    let (connection, _screen) =
        x11rb::connect(None).map_err(|e| IconError::Apply(std::io::Error::other(e.to_string())))?;

    let cookie = connection
        .intern_atom(false, b"_NET_WM_ICON")
        .map_err(|e| IconError::Apply(std::io::Error::other(e.to_string())))?;
    let atom = cookie
        .reply()
        .map_err(|e| IconError::Apply(std::io::Error::other(e.to_string())))?
        .atom;

    let data = to_cardinals(image);
    connection
        .change_property32(PropMode::REPLACE, window, atom, AtomEnum::CARDINAL, &data)
        .map_err(|e: x11rb::errors::ConnectionError| {
            IconError::Apply(std::io::Error::other(e.to_string()))
        })?
        .check()
        .map_err(|e: x11rb::errors::ReplyError| {
            IconError::Apply(std::io::Error::other(e.to_string()))
        })?;

    // `check()` above round-trips, which both flushes the request and
    // surfaces an X error the server would otherwise report asynchronously —
    // without it a bad window id fails silently and the call reports success
    // having done nothing.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cardinals_lead_with_dimensions_then_argb() {
        let image = Rgba {
            width: 2,
            height: 1,
            // Opaque red, then half-transparent blue.
            pixels: vec![0xff, 0x00, 0x00, 0xff, 0x00, 0x00, 0xff, 0x80],
        };
        assert_eq!(to_cardinals(&image), vec![2, 1, 0xffff_0000, 0x8000_00ff]);
    }

    #[test]
    fn a_windowid_is_read_as_decimal_or_hex() {
        // Emulators export decimal; tools that echo `xwininfo` export hex.
        // Reading only one form would silently skip the backend.
        assert_eq!(parse_window_id("12345"), Some(12345));
        assert_eq!(parse_window_id("0x3039"), Some(0x3039));
        assert_eq!(parse_window_id("  42  "), Some(42));
        assert_eq!(parse_window_id("not a window"), None);
    }

    #[test]
    fn a_zero_windowid_is_rejected() {
        // Zero is X's "no window" sentinel, and some servers accept a
        // property write against it without error — a silent no-op.
        assert_eq!(parse_window_id("0"), None);
        assert_eq!(parse_window_id("0x0"), None);
    }

    #[test]
    fn a_child_scope_is_refused_rather_than_silently_hitting_our_own_window() {
        // The bug this prevents: without the scope check, targeting a child
        // writes the icon onto *this* process's terminal and reports success.
        let support = icon_support(IconScope::Child { pid: 4242 });
        match support {
            IconSupport::Unsupported(IconUnsupportedReason::LinuxChildScope) => {}
            other => panic!("a child's window is not identifiable on X11, got {other:?}"),
        }
    }

    #[test]
    fn an_rgb_png_gains_full_alpha_rather_than_being_refused() {
        let png = super::tests_support::rgb_png(1, 1, [0x10, 0x20, 0x30]);
        let image = decode_png(&png).expect("an RGB PNG is an ordinary icon");
        assert_eq!(image.pixels, vec![0x10, 0x20, 0x30, 0xff]);
    }

    #[test]
    fn a_non_image_is_refused_with_a_reason_naming_the_accepted_formats() {
        let error = decode_bytes(b"not an image at all", None)
            .expect_err("arbitrary bytes are not an icon");
        match error {
            IconError::Unsupported(IconUnsupportedReason::UnknownImageFormat) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn an_oversized_png_is_refused_before_it_reaches_the_socket() {
        let png = super::tests_support::rgb_png(MAX_SIDE + 1, 1, [0, 0, 0]);
        let error = decode_png(&png).expect_err("an oversized icon must be refused");
        assert!(matches!(
            error,
            IconError::Unsupported(IconUnsupportedReason::OversizedIcon)
        ));
    }

    #[test]
    fn a_stock_icon_is_refused_because_x11_needs_pixels() {
        let error = decode(&IconSource::Stock(crate::platform::window_icon::StockIcon::Shield))
            .expect_err("a theme name is not an image");
        match error {
            IconError::Unsupported(IconUnsupportedReason::StockNeedsPixels) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }
}
