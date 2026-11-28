//! The per-frame context.

use std::ops::RangeInclusive;

use super::backend::FrameImp;
use super::{
    DockLength, Event, GfxError, Monitor, MonitorInfo, PanelOptions, PointerButton, Theme,
};

/// One frame of UI. Widgets are laid out top to bottom unless nested in
/// [`Frame::horizontal`]. Each widget returns whether the user acted on it
/// this frame.
pub struct Frame<'a> {
    pub(crate) imp: FrameImp<'a>,
}

impl Frame<'_> {
    /// Input events received since the previous frame.
    #[must_use]
    pub fn events(&self) -> &[Event] {
        self.imp.events()
    }

    /// Show text.
    pub fn label(&mut self, text: &str) {
        self.imp.label(text);
    }

    /// Show heading-sized text.
    pub fn heading(&mut self, text: &str) {
        self.imp.heading(text);
    }

    /// A push button; `true` when clicked this frame.
    pub fn button(&mut self, text: &str) -> bool {
        self.imp.button(text)
    }

    /// A push button that tells which pointer button clicked it this frame:
    /// [`PointerButton::Primary`] for an ordinary click,
    /// [`PointerButton::Secondary`] for a context (right) click, `None`
    /// otherwise. [`Frame::button`] is this with only the primary click.
    pub fn button_click(&mut self, text: &str) -> Option<PointerButton> {
        self.imp.button_click(text)
    }

    /// A labelled on/off toggle bound to `value`; `true` when changed.
    pub fn toggle(&mut self, value: &mut bool, text: &str) -> bool {
        self.imp.toggle(value, text)
    }

    /// A single-line text field bound to `value`; `true` when edited.
    ///
    /// When no widget holds keyboard focus, the first text field drawn in a
    /// frame takes it, so a window whose UI is one field accepts typing
    /// immediately.
    pub fn text_field(&mut self, value: &mut String) -> bool {
        self.imp.text_field(value)
    }

    /// A masked append-only credential field with no copy, reveal or undo history.
    ///
    /// Takes focus when no widget holds it. Accepts text, paste and committed
    /// IME input; Backspace erases the last character. Enter inserts a newline
    /// only in multiline mode. The limit is 65536 UTF-8 bytes. Plaintext input
    /// events are scrubbed, including unfocused input; draw before other interactive
    /// widgets and inspect events afterwards. The caller owns and must erase `value`.
    /// Returns whether the value changed. No plaintext enters a text widget.
    pub fn secret_field(&mut self, value: &mut String, multiline: bool) -> bool {
        self.imp.secret_field(value, multiline)
    }

    /// Release the focused widget. The next text or secret field drawn takes focus.
    pub fn clear_keyboard_focus(&mut self) {
        self.imp.clear_keyboard_focus();
    }

    /// A multi-line, monospace text area bound to `value`, as wide as the
    /// space left and at least `rows` lines tall; `true` when edited. Enter
    /// inserts a newline (the app decides when to save), and it takes
    /// keyboard focus like [`Frame::text_field`] when nothing has it.
    pub fn text_area(&mut self, value: &mut String, rows: usize) -> bool {
        self.imp.text_area(value, rows)
    }

    /// A labelled slider bound to `value` within `range`; `true` when moved.
    pub fn slider(&mut self, value: &mut f64, range: RangeInclusive<f64>, text: &str) -> bool {
        self.imp.slider(value, range, text)
    }

    /// A horizontal rule.
    pub fn separator(&mut self) {
        self.imp.separator();
    }

    /// Empty space of `amount` logical pixels along the layout direction.
    pub fn space(&mut self, amount: f32) {
        self.imp.space(amount);
    }

    /// A vertically scrolling region `height` logical pixels tall and as wide
    /// as the layout. Content taller than the region is clipped and scrolls
    /// with the wheel, drag or scroll bar.
    pub fn scroll<R>(&mut self, height: f32, contents: impl FnOnce(&mut Frame<'_>) -> R) -> R {
        let mut contents = Some(contents);
        let mut result = None;
        self.imp.scroll(height, &mut |imp| {
            if let Some(contents) = contents.take() {
                result = Some(contents(&mut Frame { imp }));
            }
        });
        match result {
            Some(result) => result,
            None => unreachable!("the backend runs the scroll body exactly once"),
        }
    }

    /// Lay out a list: rows added by `contents` are stacked with half the
    /// theme spacing between them. Use [`Frame::separator`] between groups.
    pub fn list<R>(&mut self, contents: impl FnOnce(&mut Frame<'_>) -> R) -> R {
        let mut contents = Some(contents);
        let mut result = None;
        self.imp.list(&mut |imp| {
            if let Some(contents) = contents.take() {
                result = Some(contents(&mut Frame { imp }));
            }
        });
        match result {
            Some(result) => result,
            None => unreachable!("the backend runs the list body exactly once"),
        }
    }

    /// A full-width row showing `text`, highlighted with the theme accent
    /// when `selected`, with hover and press feedback; `true` when clicked.
    ///
    /// Inside [`Frame::scroll`], a row scrolls itself into view on the frame
    /// it becomes selected, so keyboard navigation keeps the selection
    /// visible without fighting the user's own scrolling.
    pub fn selectable_row(&mut self, text: &str, selected: bool) -> bool {
        self.imp.selectable_row(text, selected)
    }

    /// Lay out the widgets added by `contents` left to right.
    pub fn horizontal<R>(&mut self, contents: impl FnOnce(&mut Frame<'_>) -> R) -> R {
        let mut contents = Some(contents);
        let mut result = None;
        self.imp.horizontal(&mut |imp| {
            if let Some(contents) = contents.take() {
                result = Some(contents(&mut Frame { imp }));
            }
        });
        match result {
            Some(result) => result,
            None => unreachable!("the backend runs the horizontal body exactly once"),
        }
    }

    /// Show the drop-down panel described by `panel` after this frame, or
    /// keep it shown (and resize or move it to match). Call it on every
    /// frame of [`App::frame`](super::App::frame) the panel should stay up:
    /// a frame that does not call it closes the panel. Its contents come
    /// from [`App::panel`](super::App::panel).
    ///
    /// The panel takes keyboard focus when the desktop allows it. When it
    /// loses focus to another window (a click away), the backend closes it
    /// and delivers [`Event::PanelDismissed`] to the next
    /// [`App::frame`](super::App::frame); handle events before calling
    /// `show_panel` again. A panel receives named keys
    /// ([`Key::Escape`](super::Key::Escape), arrows, Enter, ...) and typed
    /// text through the desktop's keyboard layout, so a
    /// [`Frame::text_field`] in it is editable; a held key repeats at the
    /// desktop's keyboard repeat delay and rate. Calling this from [`App::panel`](super::App::panel) has no
    /// effect.
    ///
    /// # Errors
    ///
    /// [`GfxErrorKind::InvalidOptions`](super::GfxErrorKind::InvalidOptions)
    /// for a degenerate size or offset, and
    /// [`GfxErrorKind::Unsupported`](super::GfxErrorKind::Unsupported) when
    /// the app is not running as a dock (see
    /// [`Capabilities::panels`](super::Capabilities::panels)), including
    /// under [`snapshot`](super::snapshot).
    pub fn show_panel(&mut self, panel: &PanelOptions) -> Result<(), GfxError> {
        panel.validate()?;
        self.imp.show_panel(*panel)
    }

    /// Resize a running [`Placement::Dock`](super::Placement::Dock) along
    /// its edge after this frame (see [`DockLength`]); the desktop's
    /// reserved space stays exactly the dock's thickness. Unlike
    /// [`Frame::show_panel`] the length persists until changed again, so a
    /// dock sized to its content calls it whenever that content's width
    /// changes; calling it with the current length does nothing.
    ///
    /// # Errors
    ///
    /// [`GfxErrorKind::InvalidOptions`](super::GfxErrorKind::InvalidOptions)
    /// for a degenerate length, and
    /// [`GfxErrorKind::Unsupported`](super::GfxErrorKind::Unsupported) when
    /// the app is not running as a dock (including under
    /// [`snapshot`](super::snapshot)) or from [`App::panel`](super::App::panel).
    pub fn set_dock_length(&mut self, length: DockLength) -> Result<(), GfxError> {
        length.validate()?;
        self.imp.set_dock_length(length)
    }

    /// The monitors a running [`Placement::Dock`](super::Placement::Dock)
    /// can be moved to with [`Frame::set_monitor`], in the desktop's order,
    /// with [`MonitorInfo::current`] marking the one the dock is on. A
    /// per-frame value that follows hot-plugging. Empty where
    /// [`Capabilities::dock_relocation`](super::Capabilities::dock_relocation)
    /// is not available, from [`App::panel`](super::App::panel), and under
    /// [`snapshot`](super::snapshot). No entry is marked as the desktop's
    /// primary: Wayland has no such notion for clients.
    #[must_use]
    pub fn monitors(&self) -> &[MonitorInfo] {
        self.imp.monitors()
    }

    /// Move a running [`Placement::Dock`](super::Placement::Dock) to
    /// `monitor` after this frame: the desktop stops reserving the dock's
    /// thickness on the monitor it leaves and reserves it on the new one.
    /// An open panel is closed (with [`Event::PanelDismissed`]); the dock's
    /// length, alignment and theme carry over, and an
    /// [`Event::Monitor`](super::Event::Monitor) with `fallback: false`
    /// follows once the dock is shown there. [`Monitor::Primary`] hands the
    /// choice back to the desktop. Moving to the monitor the dock is on
    /// does nothing.
    ///
    /// # Errors
    ///
    /// [`GfxErrorKind::UnknownMonitor`](super::GfxErrorKind::UnknownMonitor)
    /// when no monitor in [`Frame::monitors`] has that name (nothing
    /// changes), and
    /// [`GfxErrorKind::Unsupported`](super::GfxErrorKind::Unsupported) when
    /// the app is not running as a dock (see
    /// [`Capabilities::dock_relocation`](super::Capabilities::dock_relocation)),
    /// including under [`snapshot`](super::snapshot) and from
    /// [`App::panel`](super::App::panel).
    pub fn set_monitor(&mut self, monitor: Monitor) -> Result<(), GfxError> {
        self.imp.set_monitor(monitor)
    }

    /// Replace the theme from the next frame on.
    pub fn set_theme(&mut self, theme: Theme) {
        self.imp.set_theme(theme);
    }

    /// Close the window after this frame (does not consult
    /// [`App::close_requested`](super::App::close_requested)).
    pub fn close(&mut self) {
        self.imp.close();
    }

    /// Capture the window's framebuffer after the next frame is painted. The
    /// pixels arrive as [`Event::Screenshot`] in a later frame's
    /// [`Frame::events`]; a repaint is scheduled for it. Delivery needs that
    /// later frame: a window the compositor is not painting (for example on
    /// a locked Wayland session) gets none. Use
    /// [`snapshot`](super::snapshot) for pixels without a window.
    pub fn request_screenshot(&mut self) {
        self.imp.request_screenshot();
    }

    /// Width in logical pixels that `text` occupies on one line when drawn
    /// at font `size` (logical pixels) with the active theme's body text
    /// style, the same font [`Frame::label`] uses. The empty string is `0`.
    /// A per-frame value: it follows theme and font changes.
    #[must_use]
    pub fn text_width(&self, text: &str, size: f32) -> f32 {
        self.imp.text_width(text, size)
    }

    /// Height in logical pixels of one line of text at font `size` with the
    /// active theme's body text style. A per-frame value.
    #[must_use]
    pub fn line_height(&self, size: f32) -> f32 {
        self.imp.line_height(size)
    }

    /// Width in logical pixels left for widgets at the current layout
    /// position: the real surface width minus the theme's panel margin at
    /// the top level, less inside nested layouts. A per-frame value; it
    /// tracks resizes and the compositor-assigned size of a dock.
    #[must_use]
    pub fn available_width(&self) -> f32 {
        self.imp.available_width()
    }

    /// Height in logical pixels left for widgets at the current layout
    /// position. A per-frame value, like [`Frame::available_width`].
    #[must_use]
    pub fn available_height(&self) -> f32 {
        self.imp.available_height()
    }

    /// Ask for another frame no later than `after` from now, without input.
    /// The backend wakes the app no later than this; a compositor that is
    /// not drawing the window may still withhold frames (see
    /// `docs/gfx-backend.md`, "When frames happen"). Input or other requests
    /// can bring the frame earlier. Call it every frame that needs a timer,
    /// for example once a second for a clock.
    pub fn request_repaint_after(&mut self, after: core::time::Duration) {
        self.imp.request_repaint_after(after);
    }

    /// Schedule another frame even without input (for animation). The frame
    /// runs once the windowing system lets the window draw; see
    /// `docs/gfx-backend.md`, "When frames happen".
    pub fn request_repaint(&mut self) {
        self.imp.request_repaint();
    }
}
