//! Validated window presentation for the external webview facade.
//!
//! Everything here is pure data: builders record a request and validate it
//! before any native effect. The native wiring lives in `native_window_builder`
//! (the parent module) and the concrete platform trees.

/// Invalid presentation options, rejected before allocating native resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WindowOptionsError {
    #[error("webview title exceeds 1024 UTF-8 bytes or contains a control character")]
    InvalidTitle,
    #[error("webview logical width and height must each be between 1 and 16384")]
    InvalidSize,
    #[error(
        "webview app id must be 1 to 128 bytes of dot-separated [A-Za-z0-9_-] elements, \
         at least two, none empty or starting with a digit"
    )]
    InvalidAppId,
    #[error("webview logical position must have each coordinate between -32768 and 32767")]
    InvalidPosition,
}

/// Longest accepted application id: the Windows AppUserModelID bound (128
/// characters), which is tighter than GLib's 255 bytes. One rule on every
/// host, so an id valid on one desktop is valid on all of them.
pub(crate) const MAX_APP_ID_BYTES: usize = 128;

/// Each logical dimension is 1 through 16384.
const SIZE_BOUND: std::ops::RangeInclusive<u32> = 1..=16384;

/// Each logical coordinate fits the X11 protocol's signed 16-bit range, which
/// also covers any realistic multi-monitor desktop on the other hosts.
const POSITION_BOUND: std::ops::RangeInclusive<i32> = -32768..=32767;

/// Validate a logical client-area size before any native effect.
pub(crate) fn validate_size(width: u32, height: u32) -> Result<(), WindowOptionsError> {
    if SIZE_BOUND.contains(&width) && SIZE_BOUND.contains(&height) {
        Ok(())
    } else {
        Err(WindowOptionsError::InvalidSize)
    }
}

/// Validate a logical outer-window position before any native effect.
pub(crate) fn validate_position(x: i32, y: i32) -> Result<(), WindowOptionsError> {
    if POSITION_BOUND.contains(&x) && POSITION_BOUND.contains(&y) {
        Ok(())
    } else {
        Err(WindowOptionsError::InvalidPosition)
    }
}

/// Validate a reverse-DNS application id before any native effect.
///
/// The accepted alphabet is `[A-Za-z0-9._-]`, 1 to 128 bytes. Because GTK
/// refuses (and tao would then panic on) an id that is not a valid GLib
/// application id, the id must also have at least two dot-separated elements,
/// none empty and none starting with a digit. The rule is the same on every
/// host so a caller's id cannot be valid on one desktop and not another.
pub(crate) fn validate_app_id(app_id: &str) -> Result<(), WindowOptionsError> {
    let valid = (1..=MAX_APP_ID_BYTES).contains(&app_id.len())
        && app_id.contains('.')
        && app_id.split('.').all(|element| {
            element
                .bytes()
                .next()
                .is_some_and(|first| !first.is_ascii_digit())
                && element
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        });
    if valid {
        Ok(())
    } else {
        Err(WindowOptionsError::InvalidAppId)
    }
}

/// What the host does with one best-effort window request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BestEffort {
    /// The request reaches the native window system. A window manager may
    /// still adjust it (an X11 window manager can refuse keep-above).
    Requested,
    /// This display leaves the decision to the compositor (Wayland
    /// xdg-shell), so the request is not sent. Use a compositor window rule
    /// keyed on the host app id instead.
    Unsupported,
}

/// Which best-effort window controls this host's display honours, decided
/// once when the host starts. See `docs/tauri-external-content-isolation.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebviewWindowSupport {
    /// [`WebviewWindowOptions::initial_position`] and
    /// [`crate::webview::WebviewHandle::set_position`].
    pub position: BestEffort,
    /// [`WebviewWindowOptions::always_on_top`].
    pub keep_above: BestEffort,
    /// [`WebviewWindowOptions::skip_taskbar`].
    pub skip_taskbar: BestEffort,
}

impl WebviewWindowSupport {
    /// Support on a host whose compositor alone places and stacks windows
    /// (`true`, Wayland) or that lets the client ask (`false`).
    pub(crate) const fn for_host(compositor_places_windows: bool) -> Self {
        let outcome = if compositor_places_windows {
            BestEffort::Unsupported
        } else {
            BestEffort::Requested
        };
        Self {
            position: outcome,
            keep_above: outcome,
            skip_taskbar: outcome,
        }
    }
}

/// Validated initial window presentation, independent of page permissions.
///
/// Dimensions are logical client-area pixels, not physical screen pixels or
/// a guarantee of the page's CSS viewport. Desktop window managers may constrain
/// the requested size. This supplies no script execution or native IPC authority.
///
/// The widget builders ([`Self::decorations`], [`Self::transparent`],
/// [`Self::always_on_top`], [`Self::skip_taskbar`], [`Self::initial_position`])
/// only record a request; nothing native happens until an open. Keep-above,
/// taskbar exclusion and position are best effort:
/// [`crate::webview::ExternalWebviewClient::window_support`] reports which
/// the display honours. See `docs/tauri-external-content-isolation.md` for
/// the per-host table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebviewWindowOptions {
    pub(crate) title: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) decorations: bool,
    pub(crate) transparent: bool,
    pub(crate) always_on_top: bool,
    pub(crate) skip_taskbar: bool,
    pub(crate) position: Option<(i32, i32)>,
}

impl WebviewWindowOptions {
    /// Validate before copying: title is at most 1024 UTF-8 bytes, with no
    /// Unicode control characters; each logical dimension is 1 through 16384.
    /// An empty title is allowed. These are input bounds, not GPU-memory quotas.
    pub fn new(title: &str, width: u32, height: u32) -> Result<Self, WindowOptionsError> {
        if title.len() > 1024 || title.chars().any(char::is_control) {
            return Err(WindowOptionsError::InvalidTitle);
        }
        validate_size(width, height)?;
        Ok(Self {
            title: title.to_owned(),
            width,
            height,
            decorations: true,
            transparent: false,
            always_on_top: false,
            skip_taskbar: false,
            position: None,
        })
    }

    /// Request native title bar and borders (`true`, the default) or an
    /// undecorated window (`false`).
    #[must_use]
    pub const fn decorations(mut self, decorations: bool) -> Self {
        self.decorations = decorations;
        self
    }

    /// Request a transparent window and page background, so only what the
    /// page paints is visible. Default `false`. On macOS only the page
    /// background becomes transparent; the window keeps its opaque backing.
    #[must_use]
    pub const fn transparent(mut self, transparent: bool) -> Self {
        self.transparent = transparent;
        self
    }

    /// Best effort: ask to keep the window above ordinary windows. Default
    /// `false`. Windows uses `HWND_TOPMOST` and macOS the floating window
    /// level. Wayland compositors decide this themselves
    /// ([`WebviewWindowSupport::keep_above`] is then `Unsupported`); use a
    /// compositor window rule keyed on the host app id.
    #[must_use]
    pub const fn always_on_top(mut self, always_on_top: bool) -> Self {
        self.always_on_top = always_on_top;
        self
    }

    /// Best effort: ask to keep the window out of the taskbar. Default
    /// `false`. Windows makes it a tool window (`WS_EX_TOOLWINDOW`, also kept
    /// out of Alt+Tab); X11 sets the skip-taskbar hint. On macOS the request
    /// switches the whole application to the accessory activation policy (no
    /// Dock icon, no menu bar), for every window, until the process exits.
    /// Wayland compositors ignore it.
    #[must_use]
    pub const fn skip_taskbar(mut self, skip_taskbar: bool) -> Self {
        self.skip_taskbar = skip_taskbar;
        self
    }

    /// Best effort: ask for the window's outer top-left corner at logical
    /// `(x, y)` on the virtual desktop. Each coordinate must be within
    /// -32768 through 32767. Without it the window manager chooses. Wayland
    /// compositors place windows themselves
    /// ([`WebviewWindowSupport::position`] is then `Unsupported`).
    pub fn initial_position(mut self, x: i32, y: i32) -> Result<Self, WindowOptionsError> {
        validate_position(x, y)?;
        self.position = Some((x, y));
        Ok(self)
    }

    /// Whether native decorations were requested.
    pub const fn has_decorations(&self) -> bool {
        self.decorations
    }

    /// Whether a transparent window and page background were requested.
    pub const fn is_transparent(&self) -> bool {
        self.transparent
    }

    /// Whether keep-above was requested.
    pub const fn is_always_on_top(&self) -> bool {
        self.always_on_top
    }

    /// Whether taskbar exclusion was requested.
    pub const fn skips_taskbar(&self) -> bool {
        self.skip_taskbar
    }

    /// Requested initial outer position in logical pixels, if any.
    pub const fn logical_position(&self) -> Option<(i32, i32)> {
        self.position
    }

    /// Requested initial native-window title.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Requested initial client-area width and height in logical pixels.
    pub const fn logical_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}
