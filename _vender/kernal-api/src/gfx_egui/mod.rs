//! Private egui backend for [`crate::gfx`]. Every egui, eframe, wgpu and
//! winit name stays inside this module tree.

pub(crate) mod input;
pub(crate) mod offscreen;
mod secret;
mod widgets;
pub(crate) mod window_dock;

pub(crate) use offscreen::snapshot;
pub(crate) use widgets::system_theme;

use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex, PoisonError};

use crate::gfx::{
    App, BackendKind, Capabilities, Color, DockLength, Event, GfxError, GfxErrorKind, Monitor,
    MonitorInfo, PanelOptions, Placement, PointerButton, Support, Theme, ThemeMode, WindowOptions,
};

pub(crate) fn capabilities() -> Capabilities {
    Capabilities {
        backend: BackendKind::Egui,
        // Each depends on the compositor / window manager; winit forwards
        // the request and a host may ignore it (see docs/gfx-backend.md).
        transparent: Support::BestEffort,
        click_through: Support::BestEffort,
        always_on_top: Support::BestEffort,
        undecorated: Support::Supported,
        absolute_placement: Support::BestEffort,
        // eframe creates the window before monitors can be enumerated.
        monitor_placement: Support::Unsupported,
        // Per host: a layer-shell surface on Linux Wayland (probed once per
        // process), an AppBar on Windows, a status-level window on macOS
        // (best effort: nothing is reserved); see docs/gfx-backend.md, "Docks".
        dock: crate::native_gfx_dock::support(),
        // Panels are extra layer surfaces of a running layer-shell dock;
        // a window dock (Windows, macOS) has none.
        panels: crate::native_gfx_dock::panel_support(),
        // A dock's output is chosen when its layer surface is created.
        dock_monitor: crate::native_gfx_dock::support(),
        // A running dock is re-created on the new output.
        dock_relocation: crate::native_gfx_dock::support(),
    }
}

pub(crate) fn run(options: WindowOptions, app: Box<dyn App>) -> Result<(), GfxError> {
    if let Placement::Dock { edge, thickness } = options.placement {
        // winit creates every Wayland window as an xdg_toplevel, a role a
        // surface can never trade for the layer-shell role a dock needs, so
        // a dock is a separate surface path that does not go through eframe.
        return crate::native_gfx_dock::run(&options, edge, thickness, app);
    }
    let native = native_options(&options);
    run_eframe_with(&options, native, app, None)
}

/// Drive `app` in an eframe window; `dock` turns the window into a dock
/// through its host once the window exists.
pub(crate) fn run_eframe_with(
    options: &WindowOptions,
    native: eframe::NativeOptions,
    app: Box<dyn App>,
    dock: Option<window_dock::WindowDock>,
) -> Result<(), GfxError> {
    let title = options.title.clone();
    let theme = options.theme;
    let transparent = options.transparent;
    eframe::run_native(
        &title,
        native,
        Box::new(move |cc| {
            widgets::observe_system_theme(&cc.egui_ctx);
            apply_theme(&cc.egui_ctx, theme);
            let mut dock = dock;
            if let Some(dock) = dock.as_mut() {
                use raw_window_handle::HasWindowHandle as _;
                let handle = cc
                    .window_handle()
                    .map_err(|error| GfxError::new(GfxErrorKind::Backend, error.to_string()))?;
                dock.attach(handle.as_raw())?;
            }
            Ok(Box::new(Adapter {
                dock,
                app,
                theme,
                pending_theme: None,
                transparent,
                events: Vec::new(),
                screenshots: Arc::default(),
            }))
        }),
    )
    .map_err(|error| GfxError::new(GfxErrorKind::Backend, error.to_string()))
}

pub(crate) fn native_options(options: &WindowOptions) -> eframe::NativeOptions {
    let (width, height) = options.size;
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(options.title.clone())
        .with_inner_size([width, height])
        .with_resizable(options.resizable)
        .with_decorations(options.decorations)
        .with_transparent(options.transparent)
        .with_mouse_passthrough(options.click_through);
    if options.always_on_top {
        viewport = viewport.with_window_level(egui::WindowLevel::AlwaysOnTop);
    }
    let mut centered = false;
    match options.placement {
        Placement::At { x, y } => viewport = viewport.with_position([x, y]),
        Placement::Centered => centered = true,
        // Rejected by `WindowOptions::validate` before reaching here.
        Placement::Default | Placement::Monitor { .. } | Placement::Dock { .. } => {}
    }
    eframe::NativeOptions {
        viewport,
        centered,
        renderer: eframe::Renderer::Wgpu,
        ..eframe::NativeOptions::default()
    }
}

pub(crate) fn color32(color: Color) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(color.r, color.g, color.b, color.a)
}

pub(crate) fn apply_theme(ctx: &egui::Context, theme: Theme) {
    let mode = match theme.mode {
        ThemeMode::FollowSystem => match ctx.system_theme() {
            Some(egui::Theme::Light) => ThemeMode::Light,
            _ => ThemeMode::Dark,
        },
        mode => mode,
    };
    // Keep the native window subscribed to system changes. A concrete theme
    // here would pin its appearance and suppress notifications on some hosts.
    ctx.set_theme(match theme.mode {
        ThemeMode::FollowSystem => egui::ThemePreference::System,
        ThemeMode::Light => egui::ThemePreference::Light,
        ThemeMode::Dark => egui::ThemePreference::Dark,
    });
    ctx.data_mut(|data| data.insert_temp(egui::Id::unique("kernal-theme"), theme));
    let mut visuals = match mode {
        ThemeMode::Light => egui::Visuals::light(),
        ThemeMode::Dark | ThemeMode::FollowSystem => egui::Visuals::dark(),
    };
    let accent = color32(theme.accent);
    visuals.selection.bg_fill = accent;
    visuals.widgets.active.bg_fill = accent;
    visuals.hyperlink_color = accent;
    if let Some(background) = theme.background {
        visuals.panel_fill = color32(background);
        visuals.window_fill = color32(background);
    }
    if let Some(foreground) = theme.foreground {
        visuals.override_text_color = Some(color32(foreground));
    }
    let radius = egui::CornerRadius::same(corner(theme.corner_radius));
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = radius;
    }
    ctx.set_visuals(visuals);
    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(theme.spacing, theme.spacing);
        for (text_style, font) in &mut style.text_styles {
            font.size = match text_style {
                egui::TextStyle::Heading => theme.text_size * 1.5,
                egui::TextStyle::Small => theme.text_size * 0.75,
                _ => theme.text_size,
            };
        }
    });
}

/// Vertical gap between list rows: half the theme spacing.
fn list_spacing(spacing: f32) -> f32 {
    (spacing / 2.0).max(0.0)
}

/// Clamp a token radius into egui's `u8` corner radius.
fn corner(radius: f32) -> u8 {
    if radius.is_nan() || radius <= 0.0 {
        0
    } else if radius >= f32::from(u8::MAX) {
        u8::MAX
    } else {
        // In range and non-negative after the checks above.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "range checked"
        )]
        let value = radius.round() as u8;
        value
    }
}

struct Adapter {
    app: Box<dyn App>,
    /// Set when this window is a dock (see `window_dock`).
    dock: Option<window_dock::WindowDock>,
    theme: Theme,
    pending_theme: Option<Theme>,
    transparent: bool,
    events: Vec<Event>,
    /// Captures completed by the renderer, delivered as events next frame.
    screenshots: Arc<Mutex<Vec<Event>>>,
}

impl eframe::App for Adapter {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        widgets::observe_system_theme(&ctx);
        if let Some(theme) = self.pending_theme.take() {
            self.theme = theme;
            apply_theme(&ctx, theme);
        }
        self.events.clear();
        ctx.input(|input| input::translate(&input.events, &mut self.events));
        self.events.append(
            &mut self
                .screenshots
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        if ctx.input(|input| input.viewport().close_requested()) && !self.app.close_requested() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if let Some(event) = self
            .dock
            .as_mut()
            .and_then(window_dock::WindowDock::take_event)
        {
            self.events.insert(0, event);
        }
        let mut shared = Shared::new(std::mem::take(&mut self.events));
        if let Some(dock) = self.dock.as_mut() {
            shared.dock = true;
            shared.monitors = dock.monitors();
        }
        let app = &mut *self.app;
        draw(ui, self.transparent, &mut shared, &mut |frame| {
            app.frame(frame)
        });
        if let Some(dock) = self.dock.as_mut() {
            let moved = shared.monitor.is_some();
            dock.update(shared.monitor.take(), shared.dock_length.take());
            if moved {
                ctx.request_repaint();
            }
        }
        self.events = shared.events;
        self.pending_theme = shared.theme;
        if self.pending_theme.is_some() {
            ctx.request_repaint();
        }
        if shared.screenshot {
            let sink = Arc::clone(&self.screenshots);
            let waker = ctx.clone();
            ctx.request_screenshot(move |image| {
                sink.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(screenshot_event(&image));
                waker.request_repaint();
            });
            ctx.request_repaint();
        }
        if shared.close {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn on_exit(&mut self) {
        if let Some(dock) = self.dock.as_mut() {
            dock.detach();
        }
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        if self.transparent {
            self.theme.background.map_or([0.0; 4], |background| {
                color32(background).to_normalized_gamma_f32()
            })
        } else {
            visuals.panel_fill.to_normalized_gamma_f32()
        }
    }
}

/// The colour a surface is cleared to, premultiplied, in the components
/// egui paints with. Opaque windows clear to the panel fill (alpha is
/// ignored by an opaque swapchain). Transparent ones clear to the theme's
/// background including its alpha, or fully clear without one; the central
/// panel draws no fill of its own then (see `draw`), so the alpha is not
/// applied twice.
pub(crate) fn clear_color(ctx: &egui::Context, theme: Theme, transparent: bool) -> [f32; 4] {
    if !transparent {
        return ctx
            .global_style()
            .visuals
            .panel_fill
            .to_normalized_gamma_f32();
    }
    theme.background.map_or([0.0; 4], |background| {
        color32(background).to_normalized_gamma_f32()
    })
}

/// Lay out one frame of `body` (an app's `frame` or `panel`) in a central panel.
pub(crate) fn draw(
    ui: &mut egui::Ui,
    transparent: bool,
    shared: &mut Shared,
    body: &mut dyn FnMut(&mut crate::gfx::Frame<'_>),
) {
    widgets::refresh_system_theme(ui);
    let panel = egui::CentralPanel::default();
    let panel = if transparent {
        panel.frame(egui::Frame::NONE)
    } else {
        panel
    };
    panel.show(ui, |ui| {
        let mut frame = crate::gfx::Frame {
            imp: FrameImp { ui, shared },
        };
        body(&mut frame);
    });
}

/// What one frame of app code asked of the backend.
pub(crate) struct Shared {
    pub(crate) events: Vec<Event>,
    pub(crate) theme: Option<Theme>,
    pub(crate) close: bool,
    pub(crate) screenshot: bool,
    /// Whether this surface may open a panel (a layer-shell dock's frame).
    pub(crate) panels: bool,
    /// Whether this surface is a dock (may change its length or monitor).
    pub(crate) dock: bool,
    /// The panel asked for this frame, if any.
    pub(crate) panel: Option<PanelOptions>,
    /// A new dock length asked for this frame, if any.
    pub(crate) dock_length: Option<DockLength>,
    /// The outputs a dock can move to (empty off a dock's own frame).
    pub(crate) monitors: Vec<MonitorInfo>,
    /// A move asked for this frame, if any.
    pub(crate) monitor: Option<Monitor>,
}

impl Shared {
    pub(crate) fn new(events: Vec<Event>) -> Self {
        Self {
            events,
            theme: None,
            close: false,
            screenshot: false,
            panels: false,
            dock: false,
            panel: None,
            dock_length: None,
            monitors: Vec::new(),
            monitor: None,
        }
    }
}

/// A captured frame as a neutral event: straight-alpha sRGB bytes.
pub(crate) fn screenshot_event(image: &egui::ColorImage) -> Event {
    let [width, height] = image.size;
    let rgba: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(egui::Color32::to_srgba_unmultiplied)
        .collect();
    Event::Screenshot {
        width: u32::try_from(width).unwrap_or(u32::MAX),
        height: u32::try_from(height).unwrap_or(u32::MAX),
        rgba: rgba.into(),
    }
}

pub(crate) struct FrameImp<'a> {
    ui: &'a mut egui::Ui,
    shared: &'a mut Shared,
}

impl FrameImp<'_> {
    pub(crate) fn events(&self) -> &[Event] {
        &self.shared.events
    }

    pub(crate) fn label(&mut self, text: &str) {
        self.ui.label(text);
    }

    pub(crate) fn heading(&mut self, text: &str) {
        self.ui.heading(text);
    }

    pub(crate) fn button(&mut self, text: &str) -> bool {
        self.ui.button(text).clicked()
    }

    pub(crate) fn button_click(&mut self, text: &str) -> Option<PointerButton> {
        let response = self.ui.button(text);
        if response.clicked() {
            Some(PointerButton::Primary)
        } else if response.secondary_clicked() {
            Some(PointerButton::Secondary)
        } else if response.middle_clicked() {
            Some(PointerButton::Middle)
        } else {
            None
        }
    }

    pub(crate) fn toggle(&mut self, value: &mut bool, text: &str) -> bool {
        self.ui.checkbox(value, text).changed()
    }

    pub(crate) fn text_field(&mut self, value: &mut String) -> bool {
        let response = self.ui.text_edit_singleline(value);
        if self.ui.is_enabled() && self.ui.ctx().memory(|memory| memory.focused().is_none()) {
            response.request_focus();
        }
        response.changed()
    }

    pub(crate) fn clear_keyboard_focus(&mut self) {
        self.ui.ctx().memory_mut(|memory| {
            if let Some(id) = memory.focused() {
                memory.surrender_focus(id);
            }
        });
    }

    pub(crate) fn secret_field(&mut self, value: &mut String, multiline: bool) -> bool {
        secret::field(self.ui, &mut self.shared.events, value, multiline)
    }

    pub(crate) fn text_area(&mut self, value: &mut String, rows: usize) -> bool {
        let edit = egui::TextEdit::multiline(value)
            .code_editor()
            .desired_rows(rows.max(1))
            .desired_width(f32::INFINITY);
        let response = self.ui.add(edit);
        if self.ui.is_enabled() && self.ui.ctx().memory(|memory| memory.focused().is_none()) {
            response.request_focus();
        }
        response.changed()
    }

    pub(crate) fn slider(
        &mut self,
        value: &mut f64,
        range: RangeInclusive<f64>,
        text: &str,
    ) -> bool {
        self.ui
            .add(egui::Slider::new(value, range).text(text))
            .changed()
    }

    pub(crate) fn separator(&mut self) {
        self.ui.separator();
    }

    pub(crate) fn space(&mut self, amount: f32) {
        self.ui.add_space(amount);
    }

    pub(crate) fn horizontal(&mut self, body: &mut dyn FnMut(FrameImp<'_>)) {
        let shared = &mut *self.shared;
        self.ui.horizontal(|ui| body(FrameImp { ui, shared }));
    }

    pub(crate) fn scroll(&mut self, height: f32, body: &mut dyn FnMut(FrameImp<'_>)) {
        let shared = &mut *self.shared;
        egui::ScrollArea::vertical()
            .max_height(height.max(0.0))
            .min_scrolled_height(height.max(0.0))
            .auto_shrink([false, false])
            .show(self.ui, |ui| body(FrameImp { ui, shared }));
    }

    pub(crate) fn list(&mut self, body: &mut dyn FnMut(FrameImp<'_>)) {
        let shared = &mut *self.shared;
        self.ui.vertical(|ui| {
            let spacing = ui.spacing().item_spacing.y;
            ui.spacing_mut().item_spacing.y = list_spacing(spacing);
            body(FrameImp { ui, shared });
        });
    }

    pub(crate) fn selectable_row(&mut self, text: &str, selected: bool) -> bool {
        let width = self.ui.available_width();
        let row = egui::Button::selectable(selected, text)
            .min_size(egui::vec2(width, 0.0))
            .frame_when_inactive(selected);
        let response = self.ui.add(row);
        // Scroll into view only on the transition to selected, so a wheel
        // scroll away from a still-selected row is not undone next frame.
        let was_selected = self.ui.ctx().data_mut(|data| {
            let was = data.get_temp::<bool>(response.id).unwrap_or(false);
            data.insert_temp(response.id, selected);
            was
        });
        if selected && !was_selected {
            response.scroll_to_me(None);
        }
        response.clicked()
    }

    pub(crate) fn set_theme(&mut self, theme: Theme) {
        self.shared.theme = Some(theme);
    }

    pub(crate) fn close(&mut self) {
        self.shared.close = true;
    }

    pub(crate) fn request_repaint(&mut self) {
        self.ui.ctx().request_repaint();
    }

    pub(crate) fn request_screenshot(&mut self) {
        self.shared.screenshot = true;
    }

    pub(crate) fn request_repaint_after(&mut self, after: std::time::Duration) {
        self.ui.ctx().request_repaint_after(after);
    }

    /// The body font family at `size`, as `label` draws it.
    fn font(&self, size: f32) -> egui::FontId {
        let family = self
            .ui
            .style()
            .text_styles
            .get(&egui::TextStyle::Body)
            .map_or(egui::FontFamily::Proportional, |font| font.family.clone());
        egui::FontId::new(size.max(0.0), family)
    }

    pub(crate) fn text_width(&self, text: &str, size: f32) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let font = self.font(size);
        let galley = self.ui.fonts_mut(|fonts| {
            fonts.layout_no_wrap(text.to_owned(), font, egui::Color32::PLACEHOLDER)
        });
        galley.size().x
    }

    pub(crate) fn line_height(&self, size: f32) -> f32 {
        let font = self.font(size);
        self.ui.fonts_mut(|fonts| fonts.row_height(&font))
    }

    pub(crate) fn available_width(&self) -> f32 {
        self.ui.available_width()
    }

    pub(crate) fn available_height(&self) -> f32 {
        self.ui.available_height()
    }

    pub(crate) fn show_panel(&mut self, panel: PanelOptions) -> Result<(), GfxError> {
        if !self.shared.panels {
            return Err(GfxError::new(
                GfxErrorKind::Unsupported,
                "show_panel needs an app run with Placement::Dock on a host whose docks have panels (Capabilities::panels)",
            ));
        }
        self.shared.panel = Some(panel);
        Ok(())
    }

    pub(crate) fn set_dock_length(&mut self, length: DockLength) -> Result<(), GfxError> {
        if !self.shared.dock {
            return Err(GfxError::new(
                GfxErrorKind::Unsupported,
                "set_dock_length needs an app run with Placement::Dock on a host that supports docks",
            ));
        }
        self.shared.dock_length = Some(length);
        Ok(())
    }

    pub(crate) fn monitors(&self) -> &[MonitorInfo] {
        &self.shared.monitors
    }

    pub(crate) fn set_monitor(&mut self, monitor: Monitor) -> Result<(), GfxError> {
        if !self.shared.dock {
            return Err(GfxError::new(
                GfxErrorKind::Unsupported,
                "set_monitor needs an app run with Placement::Dock on a host that supports docks",
            ));
        }
        if let Monitor::Named(name) = &monitor {
            if !self.shared.monitors.iter().any(|info| &info.name == name) {
                let present: Vec<&str> = self
                    .shared
                    .monitors
                    .iter()
                    .map(|info| info.name.as_str())
                    .collect();
                return Err(GfxError::new(
                    GfxErrorKind::UnknownMonitor,
                    format!("set_monitor: {name:?} is not connected (monitors: {present:?})"),
                ));
            }
        }
        self.shared.monitor = Some(monitor);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transparent_surface_clears_to_the_theme_background_alpha() {
        let ctx = egui::Context::default();
        let theme = Theme::default().with_background(Color::rgba(200, 100, 0, 179));
        apply_theme(&ctx, theme);
        let [r, g, b, a] = clear_color(&ctx, theme, true);
        // Premultiplied: each channel scaled by alpha 179/255.
        let alpha = 179.0 / 255.0;
        assert!((a - alpha).abs() < 1e-3, "{a}");
        assert!((r - 200.0 / 255.0 * alpha).abs() < 2e-3, "{r}");
        assert!((g - 100.0 / 255.0 * alpha).abs() < 2e-3, "{g}");
        assert!(b.abs() < 1e-3);
        assert_eq!(
            clear_color(&ctx, Theme::default(), true),
            [0.0; 4],
            "no background: clear"
        );
        let [.., opaque] = clear_color(&ctx, theme, false);
        assert!(
            opaque < 1.0,
            "opaque swapchains ignore it, but it is the panel fill"
        );
    }

    /// Run one headless frame (no window, no GPU) over `events`.
    pub(super) fn frame(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        mut body: impl FnMut(&mut crate::gfx::Frame<'_>),
    ) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 300.0),
            )),
            events,
            ..egui::RawInput::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            let mut events = Vec::new();
            ctx.input(|input| input::translate(&input.events, &mut events));
            let mut shared = Shared::new(events);
            let mut frame = crate::gfx::Frame {
                imp: FrameImp {
                    ui,
                    shared: &mut shared,
                },
            };
            body(&mut frame);
        });
        // Headless: there is no renderer to upload font textures to.
        output.textures_delta.clear();
    }

    pub(super) fn click(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn screenshot_is_straight_alpha_rgba() {
        let image = egui::ColorImage::new(
            [2, 1],
            vec![
                egui::Color32::from_rgba_unmultiplied(255, 0, 0, 255),
                egui::Color32::from_rgba_unmultiplied(0, 0, 255, 128),
            ],
        );
        let Event::Screenshot {
            width,
            height,
            rgba,
        } = screenshot_event(&image)
        else {
            panic!("not a screenshot event");
        };
        assert_eq!((width, height), (2, 1));
        assert_eq!(rgba.len(), 8);
        assert_eq!(&rgba[..4], &[255, 0, 0, 255]);
        assert_eq!(rgba[7], 128);
        assert!(rgba[6] > 240, "blue un-premultiplied: {rgba:?}");
    }

    #[test]
    fn show_panel_is_unsupported_off_a_dock_and_recorded_on_one() {
        let ctx = egui::Context::default();
        let panel = crate::gfx::PanelOptions::new(200.0, 100.0).offset(10.0);
        let mut kind = None;
        frame(&ctx, Vec::new(), |frame| {
            kind = frame.show_panel(&panel).err().map(|err| err.kind());
        });
        assert_eq!(kind, Some(GfxErrorKind::Unsupported));

        let mut asked = None;
        let input = egui::RawInput::default();
        let mut output = ctx.run_ui(input, |ui| {
            let mut shared = Shared::new(Vec::new());
            shared.panels = true;
            shared.dock = true;
            let mut frame = crate::gfx::Frame {
                imp: FrameImp {
                    ui,
                    shared: &mut shared,
                },
            };
            assert!(frame.show_panel(&panel).is_ok());
            let bad = crate::gfx::PanelOptions::new(0.0, 1.0);
            assert_eq!(
                frame.show_panel(&bad).err().map(|err| err.kind()),
                Some(GfxErrorKind::InvalidOptions)
            );
            asked = shared.panel;
        });
        output.textures_delta.clear();
        assert_eq!(asked, Some(panel));
    }

    #[test]
    fn set_dock_length_is_unsupported_off_a_dock_and_recorded_on_one() {
        use crate::gfx::DockLength;
        let ctx = egui::Context::default();
        let mut kind = None;
        frame(&ctx, Vec::new(), |frame| {
            kind = frame
                .set_dock_length(DockLength::Fixed(900.0))
                .err()
                .map(|err| err.kind());
        });
        assert_eq!(kind, Some(GfxErrorKind::Unsupported));

        let mut asked = None;
        ctx.input(|input| {
            assert!(input.events.iter().chain(&input.raw.events).all(|e| {
                !matches!(e, egui::Event::Text(t) | egui::Event::Paste(t) if !t.is_empty())
            }));
        });
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut shared = Shared::new(Vec::new());
            shared.panels = true;
            shared.dock = true;
            let mut frame = crate::gfx::Frame {
                imp: FrameImp {
                    ui,
                    shared: &mut shared,
                },
            };
            assert!(frame.set_dock_length(DockLength::Fixed(900.0)).is_ok());
            assert_eq!(
                frame
                    .set_dock_length(DockLength::Fixed(0.0))
                    .err()
                    .map(|err| err.kind()),
                Some(GfxErrorKind::InvalidOptions)
            );
            asked = shared.dock_length;
        });
        output.textures_delta.clear();
        assert_eq!(asked, Some(DockLength::Fixed(900.0)));
    }

    #[test]
    fn button_click_tells_primary_from_secondary() {
        let ctx = egui::Context::default();
        let at = egui::pos2(12.0, 8.0);
        let press = |button, pressed| egui::Event::PointerButton {
            pos: at,
            button,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        let mut clicks = Vec::new();
        for button in [egui::PointerButton::Secondary, egui::PointerButton::Primary] {
            frame(&ctx, vec![egui::Event::PointerMoved(at)], |frame| {
                clicks.push(frame.button_click("Menu"));
            });
            frame(&ctx, vec![press(button, true)], |frame| {
                clicks.push(frame.button_click("Menu"));
            });
            frame(&ctx, vec![press(button, false)], |frame| {
                clicks.push(frame.button_click("Menu"));
            });
        }
        assert_eq!(
            clicks,
            [
                None,
                None,
                Some(PointerButton::Secondary),
                None,
                None,
                Some(PointerButton::Primary)
            ]
        );
    }

    #[test]
    fn list_spacing_is_half_the_theme_spacing() {
        assert!((list_spacing(8.0) - 4.0).abs() < f32::EPSILON);
        assert!(list_spacing(-2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn scroll_clips_tall_content_to_the_region() {
        let ctx = egui::Context::default();
        let mut outer = egui::Rect::NOTHING;
        let mut inner_bottom = 0.0;
        for _ in 0..2 {
            frame(&ctx, Vec::new(), |f| {
                let top = f.imp.ui.cursor().top();
                f.scroll(100.0, |f| {
                    for i in 0..50 {
                        f.selectable_row(&format!("row {i}"), false);
                    }
                    inner_bottom = f.imp.ui.min_rect().bottom();
                });
                outer = egui::Rect::from_x_y_ranges(0.0..=1.0, top..=f.imp.ui.cursor().top());
            });
        }
        assert!(
            inner_bottom - outer.top() > 500.0,
            "content is taller than the region"
        );
        let used = outer.height();
        assert!(
            (100.0..120.0).contains(&used),
            "region takes ~100 px, took {used}"
        );
    }

    #[test]
    fn selectable_row_reports_a_click() {
        let ctx = egui::Context::default();
        let rect = std::cell::Cell::new(egui::Rect::NOTHING);
        let mut clicked = Vec::new();
        let mut draw = |events| {
            frame(&ctx, events, |f| {
                let hit = f.list(|f| {
                    let first = f.selectable_row("first", true);
                    rect.set(f.imp.ui.min_rect());
                    let second = f.selectable_row("second", false);
                    (first, second)
                });
                clicked.push(hit);
            });
        };
        draw(Vec::new());
        let first = rect.get();
        let target = egui::pos2(first.center().x, first.top() + 4.0);
        draw(vec![egui::Event::PointerMoved(target)]);
        draw(vec![click(target, true)]);
        draw(vec![click(target, false)]);
        assert!(
            clicked.iter().any(|&(first, _)| first),
            "first row clicked: {clicked:?}"
        );
        assert!(
            clicked.iter().all(|&(_, second)| !second),
            "second row untouched: {clicked:?}"
        );
    }

    #[test]
    fn secret_field_reserves_without_abandoning_a_prefilled_buffer() {
        let ctx = egui::Context::default();
        let mut value = String::from("prefilled");
        frame(&ctx, Vec::new(), |f| {
            f.secret_field(&mut value, false);
        });
        assert!(value.capacity() >= 65_536);
        assert_eq!(value, "prefilled");
    }

    #[test]
    fn secret_field_masks_and_consumes_text_without_undo() {
        let ctx = egui::Context::default();
        let mut value = String::with_capacity(65536);
        frame(&ctx, Vec::new(), |f| {
            f.secret_field(&mut value, false);
        });
        frame(&ctx, vec![egui::Event::Text("pri".into())], |f| {
            assert!(f.secret_field(&mut value, false));
            assert!(f.events().iter().all(|e| !matches!(e, Event::Text(_))));
        });
        frame(&ctx, vec![egui::Event::Paste("vate".into())], |f| {
            assert!(f.secret_field(&mut value, false));
        });
        assert_eq!(value, "private");
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut shared = Shared::new(Vec::new());
            let mut f = crate::gfx::Frame {
                imp: FrameImp {
                    ui,
                    shared: &mut shared,
                },
            };
            f.secret_field(&mut value, false);
        });
        assert!(!format!("{:?}", output.shapes).contains("private"));
        output.textures_delta.clear();

        frame(
            &ctx,
            vec![egui::Event::Key {
                key: egui::Key::Z,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            }],
            |f| {
                f.secret_field(&mut value, false);
            },
        );
        assert_eq!(value, "private");
    }

    #[test]
    fn secret_field_filters_bounds_erases_and_recovers_focus() {
        let ctx = egui::Context::default();
        let mut value = String::with_capacity(65536);
        frame(&ctx, Vec::new(), |f| {
            f.secret_field(&mut value, false);
        });
        frame(&ctx, vec![egui::Event::Text("é\n\tX".into())], |f| {
            f.secret_field(&mut value, false);
        });
        assert_eq!(value, "éX");
        let key = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        frame(&ctx, vec![key(egui::Key::Backspace)], |f| {
            f.secret_field(&mut value, false);
        });
        assert_eq!(value, "é");
        frame(&ctx, vec![key(egui::Key::Enter)], |f| {
            f.secret_field(&mut value, true);
        });
        assert_eq!(value, "é\n");
        frame(&ctx, vec![egui::Event::Paste("x".repeat(65536))], |f| {
            f.secret_field(&mut value, true);
        });
        assert_eq!(value, "é\n");
        value.clear(); // Simulate a rejected submission; production callers zeroize.
        ctx.memory_mut(|m| m.request_focus(egui::Id::unique("store")));
        frame(&ctx, Vec::new(), |f| {
            f.clear_keyboard_focus();
            f.secret_field(&mut value, false);
        });
        frame(&ctx, vec![egui::Event::Text("retry".into())], |f| {
            f.secret_field(&mut value, false);
        });
        assert_eq!(value, "retry");
    }

    #[test]
    fn text_field_takes_focus_when_nothing_has_it() {
        let ctx = egui::Context::default();
        let mut query = String::new();
        frame(&ctx, Vec::new(), |f| {
            f.text_field(&mut query);
        });
        frame(&ctx, vec![egui::Event::Text("fi".into())], |f| {
            f.text_field(&mut query);
        });
        assert_eq!(query, "fi");
    }

    #[test]
    fn text_area_takes_focus_and_enter_inserts_a_newline() {
        let ctx = egui::Context::default();
        let mut text = String::from("- [ ] a");
        frame(&ctx, Vec::new(), |f| {
            f.text_area(&mut text, 4);
        });
        let enter = egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let mut edited = false;
        frame(&ctx, vec![enter, egui::Event::Text("b".into())], |f| {
            edited = f.text_area(&mut text, 4);
        });
        assert!(edited);
        assert!(text.contains('\n') && text.contains('b'), "{text:?}");
    }

    /// Lay `body` out exactly as `run` does, in a central panel.
    fn panel_frame(
        ctx: &egui::Context,
        size: (f32, f32),
        mut body: impl FnMut(&mut crate::gfx::Frame<'_>),
    ) {
        struct Body<F>(F);
        impl<F: FnMut(&mut crate::gfx::Frame<'_>)> App for Body<F> {
            fn frame(&mut self, frame: &mut crate::gfx::Frame<'_>) {
                (self.0)(frame);
            }
        }
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size.0, size.1),
            )),
            ..egui::RawInput::default()
        };
        let mut app = Body(&mut body);
        let mut output = ctx.run_ui(input, |ui| {
            draw(ui, false, &mut Shared::new(Vec::new()), &mut |frame| {
                app.frame(frame)
            });
        });
        output.textures_delta.clear();
    }

    #[test]
    fn gfx_text_width_measures_with_the_label_font() {
        let ctx = egui::Context::default();
        let mut widths = None;
        frame(&ctx, Vec::new(), |f| {
            let short = f.text_width("12:00", 14.0);
            let long = f.text_width("12:00:00 UTC", 14.0);
            let again = f.text_width("12:00:00 UTC", 14.0);
            let empty = f.text_width("", 14.0);
            let bigger = f.text_width("12:00:00 UTC", 28.0);
            let line = f.line_height(14.0);
            widths = Some((short, long, again, empty, bigger, line));
        });
        let Some((short, long, again, empty, bigger, line)) = widths else {
            panic!("frame body did not run");
        };
        assert!(short > 0.0, "{short}");
        assert!(long > short, "{long} <= {short}");
        assert!((long - again).abs() < f32::EPSILON, "{long} != {again}");
        assert!(empty.abs() < f32::EPSILON, "{empty}");
        assert!(bigger > long * 1.5, "{bigger} vs {long}");
        assert!(line > 10.0 && line < 30.0, "{line}");
    }

    #[test]
    fn gfx_text_width_matches_the_drawn_label() {
        let ctx = egui::Context::default();
        let size = ctx.global_style().text_styles[&egui::TextStyle::Body].size;
        let mut measured = 0.0;
        let mut drawn = 0.0;
        frame(&ctx, Vec::new(), |f| {
            measured = f.text_width("Mon 2 Nov 12:00", size);
            drawn = f.imp.ui.label("Mon 2 Nov 12:00").rect.width();
        });
        assert!((measured - drawn).abs() < 0.5, "{measured} vs {drawn}");
    }

    #[test]
    fn gfx_available_width_matches_the_window_size() {
        let options = WindowOptions::new("t").size(640.0, 36.0);
        let ctx = egui::Context::default();
        let mut available = (0.0, 0.0);
        panel_frame(&ctx, options.size, |f| {
            available = (f.available_width(), f.available_height());
        });
        let (width, height) = available;
        assert!(width > 0.0 && height > 0.0, "{available:?}");
        // The central panel's margin is all that separates them.
        assert!((options.size.0 - width).abs() <= 24.0, "{width}");
        assert!((options.size.1 - height).abs() <= 24.0, "{height}");
    }

    #[test]
    fn gfx_request_repaint_after_schedules_a_frame() {
        let ctx = egui::Context::default();
        let input = egui::RawInput::default();
        let mut output = ctx.run_ui(input, |ui| {
            let mut shared = Shared::new(Vec::new());
            let mut frame = crate::gfx::Frame {
                imp: FrameImp {
                    ui,
                    shared: &mut shared,
                },
            };
            frame.request_repaint_after(std::time::Duration::from_secs(1));
        });
        output.textures_delta.clear();
        let delay = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
        assert!(delay <= std::time::Duration::from_secs(1), "{delay:?}");
    }
}
