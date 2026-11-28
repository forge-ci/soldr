//! `gfx` docks on Linux: a wlr-layer-shell surface drawn by egui-wgpu.
//!
//! eframe cannot host a dock. winit 0.30 creates every Wayland window through
//! `xdg_shell.create_window` (an `xdg_toplevel`), and a `wl_surface` that has
//! taken the xdg role can never take the layer-shell role, which is what
//! makes the compositor anchor a surface to an edge and reserve an exclusive
//! zone. So this module is a second surface path that never touches winit:
//! smithay-client-toolkit owns its own Wayland connection and layer surface,
//! wgpu renders into that surface through raw handles, and the frame itself
//! is the same `draw` / `FrameImp` the eframe path uses, so an `App` cannot
//! tell which path it runs on. X11 and non-layer-shell compositors (GNOME's
//! Mutter) report the capability as unsupported, with the reason in the
//! `run` error.
//!
//! A dock may also show one drop-down panel (`Frame::show_panel`): a second
//! layer surface on the same connection and wgpu device, with its own
//! swapchain, egui context and frame clock, anchored to an edge with a zero
//! exclusive zone so it opens beside the dock. It takes keyboard focus on
//! demand; losing it dismisses the panel.
//!
//! A dock shorter than its edge (`DockLength::Fixed`) keeps reserving its
//! thickness: the protocol only honours an exclusive zone for a surface
//! anchored to one edge, or to an edge and both edges perpendicular to it
//! (never to a corner). So a centred dock anchors to its edge alone, and a
//! start- or end-aligned one stays anchored across the edge and is shortened
//! with a margin computed from the output's logical size.

use std::ptr::NonNull;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::{
    registry_queue_init, GlobalList, GlobalListContents,
};
use smithay_client_toolkit::reexports::client::protocol::{
    wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_surface,
};
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, Proxy, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, registry_handlers,
};

use smithay_client_toolkit::reexports::client::EventQueue;

use crate::gfx::{
    App, DockAlign, DockLength, Edge, Event, GfxError, GfxErrorKind, Monitor, MonitorInfo, PanelOptions, Support, Theme,
    WindowOptions,
};
use super::gfx_keymap::Keymap;
use crate::gfx_egui::offscreen::{block_on, read_back};
use crate::gfx_egui::{apply_theme, clear_color, draw, input, screenshot_event, Shared};

/// The layer-shell global a dock needs.
const LAYER_SHELL: &str = "zwlr_layer_shell_v1";

/// Longest sleep between frames when the app asked for none: the loop still
/// wakes to notice a closed connection.
const IDLE: Duration = Duration::from_secs(1);

/// Key repeat until the compositor sends `wl_keyboard.repeat_info`: the
/// delay and rate (25 per second) most desktops ship with.
const REPEAT: Repeat = Repeat {
    delay: Duration::from_millis(600),
    interval: Some(Duration::from_millis(40)),
};

/// The desktop's keyboard repeat setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Repeat {
    /// How long a key is held before it starts repeating.
    delay: Duration,
    /// Time between repeats; `None` when repeat is off.
    interval: Option<Duration>,
}

impl Repeat {
    /// From `wl_keyboard.repeat_info`: `rate` keys per second (0 turns
    /// repeat off) after `delay` milliseconds.
    fn from_info(rate: i32, delay: i32) -> Self {
        let interval = u64::try_from(rate)
            .ok()
            .filter(|rate| *rate > 0)
            .map(|rate| Duration::from_micros(1_000_000 / rate));
        Self {
            delay: Duration::from_millis(u64::try_from(delay).unwrap_or(0)),
            interval,
        }
    }
}

/// Whether a held key with this evdev code repeats. Keys that act rather
/// than edit (Escape, Enter, Tab) and modifiers do not; text keys and
/// editing or cursor keys do.
fn repeats(code: u32, types_text: bool) -> bool {
    match named_key(code) {
        Some(egui::Key::Escape | egui::Key::Enter | egui::Key::Tab) => false,
        Some(_) => true,
        None => types_text,
    }
}

pub(crate) fn support() -> Support {
    if unavailable().is_none() {
        Support::Supported
    } else {
        Support::Unsupported
    }
}

/// Panels are extra layer surfaces of a dock, so they exist wherever it does.
pub(crate) fn panel_support() -> Support {
    support()
}

/// Why this session cannot show a dock, probed once per process.
fn unavailable() -> Option<&'static str> {
    static PROBE: OnceLock<Option<String>> = OnceLock::new();
    PROBE.get_or_init(probe).as_deref()
}

struct Probe;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Probe {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn probe() -> Option<String> {
    let Ok(conn) = Connection::connect_to_env() else {
        return Some("not a Wayland session (no reachable WAYLAND_DISPLAY)".to_owned());
    };
    let globals = match registry_queue_init::<Probe>(&conn) {
        Ok((globals, _queue)) => globals,
        Err(error) => return Some(format!("Wayland registry unavailable: {error}")),
    };
    if has_layer_shell(&globals) {
        None
    } else {
        Some(format!(
            "the compositor does not offer {LAYER_SHELL} (GNOME's Mutter does not; KWin and wlroots compositors do)"
        ))
    }
}

fn has_layer_shell(globals: &GlobalList) -> bool {
    globals.contents().with_list(|list| {
        list.iter()
            .any(|global| global.interface == LAYER_SHELL)
    })
}

fn backend_error(error: impl std::fmt::Display) -> GfxError {
    GfxError::new(GfxErrorKind::Backend, error.to_string())
}

pub(crate) fn run(
    options: &WindowOptions,
    edge: Edge,
    thickness: f32,
    app: Box<dyn App>,
) -> Result<(), GfxError> {
    if let Some(reason) = unavailable() {
        return Err(GfxError::new(
            GfxErrorKind::Unsupported,
            format!("Placement::Dock: {reason}"),
        ));
    }
    let conn = Connection::connect_to_env().map_err(backend_error)?;
    let (globals, queue) = registry_queue_init::<Dock>(&conn).map_err(backend_error)?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).map_err(backend_error)?;
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(|error| {
        GfxError::new(GfxErrorKind::Unsupported, format!("{LAYER_SHELL}: {error}"))
    })?;

    let chooser = Chooser::new(&conn)?;
    let names = chooser.names();
    let (output, fallback) = match choose(&names, &options.monitor) {
        Choice::Primary => (None, false),
        Choice::Fallback => (None, true),
        Choice::Output(index) => (chooser.output(index), false),
    };

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface.clone(),
        Layer::Top,
        Some(options.title.clone()),
        output.as_ref(),
    );
    let shape = Shape {
        edge,
        depth: logical_px(thickness),
        length: options.dock_length,
        align: options.dock_align,
        edge_len: output
            .as_ref()
            .and_then(|output| chooser.state.outputs.info(output))
            .and_then(|info| edge_length(edge, info.logical_size)),
    };
    shape.apply(&layer);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    // The first commit carries no buffer: the compositor answers with the
    // configure that gives the edge length.
    layer.commit();

    let (gpu, target) = Gpu::new(&conn, &surface, options.transparent)?;
    let main = View::new(target, layer, surface, options.theme);
    if fallback {
        // Said once even if the compositor never reports an output.
        main_fallback_note(&options.monitor, &names);
    }

    let mut dock = Dock {
        main,
        shape,
        panel: None,
        gpu,
        output,
        fallback,
        monitor: None,
        chooser,
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        seats: SeatState::new(&globals, &qh),
        compositor,
        layer_shell,
        closed: false,
        pointer: None,
        keyboard: None,
        keymap: None,
        app,
        title: options.title.clone(),
        theme: options.theme,
        transparent: options.transparent,
        repeat: REPEAT,
        held: None,
        start: Instant::now(),
        qh: qh.clone(),
        _conn: conn.clone(),
    };

    let mut event_loop: EventLoop<'_, Dock> = EventLoop::try_new().map_err(backend_error)?;
    WaylandSource::new(conn, queue)
        .insert(event_loop.handle())
        .map_err(|error| backend_error(error.error))?;
    while !dock.closed {
        let next = [
            dock.main.next_frame,
            dock.panel.as_ref().and_then(|panel| panel.view.next_frame),
            dock.held.map(|(_, at)| at),
        ]
        .into_iter()
        .flatten()
        .min();
        let timeout =
            next.map_or(IDLE, |at| at.saturating_duration_since(Instant::now()).min(IDLE));
        event_loop
            .dispatch(Some(timeout), &mut dock)
            .map_err(backend_error)?;
        dock.repeat_held();
        dock.maybe_draw()?;
    }
    Ok(())
}

/// Where the dock goes, given the connected outputs' names.
#[derive(Debug, PartialEq, Eq)]
enum Choice {
    /// The compositor's choice, as asked.
    Primary,
    /// The compositor's choice, because the named output is not connected.
    Fallback,
    /// The output at this index of the names.
    Output(usize),
}

fn choose(names: &[Option<String>], wanted: &Monitor) -> Choice {
    match wanted {
        Monitor::Named(name) => names
            .iter()
            .position(|candidate| candidate.as_deref() == Some(name.as_str()))
            .map_or(Choice::Fallback, Choice::Output),
        _ => Choice::Primary,
    }
}

/// The fallback is also reported as `Event::Monitor { fallback: true }`;
/// this line explains it in the process log with what was there instead.
fn main_fallback_note(wanted: &Monitor, names: &[Option<String>]) {
    let present: Vec<&str> = names.iter().flatten().map(String::as_str).collect();
    eprintln!(
        "gfx dock: {wanted:?} is not connected (outputs: {present:?}); using the desktop's primary output"
    );
}

/// The outputs as they are before the dock exists, read on a private queue
/// of the dock's connection so a layer surface can be created on one. Kept
/// alive with the dock: dropping it would release the output proxies.
struct Chooser {
    state: Outputs,
    _queue: EventQueue<Outputs>,
}

struct Outputs {
    registry: RegistryState,
    outputs: OutputState,
}

impl Chooser {
    fn new(conn: &Connection) -> Result<Self, GfxError> {
        let (globals, mut queue) = registry_queue_init::<Outputs>(conn).map_err(backend_error)?;
        let mut state = Outputs {
            registry: RegistryState::new(&globals),
            outputs: OutputState::new(&globals, &queue.handle()),
        };
        // The first round trip binds the outputs; the second delivers their
        // names (wl_output v4 or xdg-output).
        for _ in 0..2 {
            queue.roundtrip(&mut state).map_err(backend_error)?;
        }
        Ok(Self {
            state,
            _queue: queue,
        })
    }

    fn names(&self) -> Vec<Option<String>> {
        self.state
            .outputs
            .outputs()
            .map(|output| self.state.outputs.info(&output).and_then(|info| info.name))
            .collect()
    }

    fn output(&self, index: usize) -> Option<wl_output::WlOutput> {
        self.state.outputs.outputs().nth(index)
    }
}

impl OutputHandler for Outputs {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
    }
}

impl ProvidesRegistryState for Outputs {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

delegate_output!(Outputs);
delegate_registry!(Outputs);

/// Logical pixels to a whole, non-zero protocol size.
fn logical_px(value: f32) -> u32 {
    // Validated finite and positive by `WindowOptions::validate`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "clamped")]
    let px = value.round().clamp(1.0, 16384.0) as u32;
    px
}

/// Logical pixels to a protocol margin (non-negative, may be zero).
fn margin_px(value: f32) -> i32 {
    // Validated finite and non-negative by `PanelOptions::validate`.
    #[allow(clippy::cast_possible_truncation, reason = "clamped")]
    let px = value.round().clamp(0.0, 16384.0) as i32;
    px
}

/// The along-edge length of an output of `logical_size` (width, height).
fn edge_length(edge: Edge, logical_size: Option<(i32, i32)>) -> Option<u32> {
    let (width, height) = logical_size?;
    let length = match edge {
        Edge::Top | Edge::Bottom => width,
        Edge::Left | Edge::Right => height,
    };
    u32::try_from(length).ok().filter(|length| *length > 0)
}

/// What the main layer surface asks of the compositor.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Shape {
    edge: Edge,
    /// Thickness in logical pixels; always the exclusive zone.
    depth: u32,
    length: DockLength,
    align: DockAlign,
    /// The output's along-edge length, once known.
    edge_len: Option<u32>,
}

/// Anchor, size and margins (top, right, bottom, left) for a layer surface.
type Geometry = (Anchor, (u32, u32), (i32, i32, i32, i32));

impl Shape {
    fn geometry(&self) -> Geometry {
        let (edge_anchor, across) = match self.edge {
            Edge::Top => (Anchor::TOP, Anchor::LEFT | Anchor::RIGHT),
            Edge::Bottom => (Anchor::BOTTOM, Anchor::LEFT | Anchor::RIGHT),
            Edge::Left => (Anchor::LEFT, Anchor::TOP | Anchor::BOTTOM),
            Edge::Right => (Anchor::RIGHT, Anchor::TOP | Anchor::BOTTOM),
        };
        let horizontal = matches!(self.edge, Edge::Top | Edge::Bottom);
        // (along, depth) to the protocol's (width, height).
        let size = |along: u32| {
            if horizontal {
                (along, self.depth)
            } else {
                (self.depth, along)
            }
        };
        let full = (edge_anchor | across, size(0), (0, 0, 0, 0));
        let DockLength::Fixed(length) = self.length else {
            return full;
        };
        // Never longer than the edge: a compositor may not clamp it itself.
        let length = self
            .edge_len
            .map_or(logical_px(length), |edge_len| logical_px(length).min(edge_len));
        let shrink = |edge_len: u32| i32::try_from(edge_len.saturating_sub(length)).unwrap_or(0);
        // The margin on the far side shortens a surface stretched across the
        // edge: (top, right, bottom, left).
        let margins = |far: i32| match (horizontal, self.align) {
            (true, DockAlign::End) => (0, 0, 0, far),
            (true, _) => (0, far, 0, 0),
            (false, DockAlign::End) => (far, 0, 0, 0),
            (false, _) => (0, 0, far, 0),
        };
        match (self.align, self.edge_len) {
            // Anchored to its edge alone, the compositor centres it.
            (DockAlign::Center, _) => (edge_anchor, size(length), (0, 0, 0, 0)),
            (_, Some(edge_len)) => (edge_anchor | across, size(0), margins(shrink(edge_len))),
            // Until the output's size is known, the whole edge.
            (_, None) => full,
        }
    }

    /// Where the dock begins along its edge, in logical pixels: what a
    /// panel's offset (measured from the dock's start) is added to.
    fn start(&self) -> f32 {
        let DockLength::Fixed(length) = self.length else {
            return 0.0;
        };
        let Some(edge_len) = self.edge_len else {
            return 0.0;
        };
        #[allow(clippy::cast_precision_loss, reason = "output sizes are far below 2^24")]
        let free = edge_len.saturating_sub(logical_px(length)) as f32;
        match self.align {
            DockAlign::Start => 0.0,
            // The compositor centres on whole pixels.
            DockAlign::Center => (free / 2.0).floor(),
            DockAlign::End => free,
        }
    }

    fn apply(&self, layer: &LayerSurface) {
        let (anchor, (width, height), (top, right, bottom, left)) = self.geometry();
        layer.set_anchor(anchor);
        layer.set_size(width, height);
        layer.set_margin(top, right, bottom, left);
        layer.set_exclusive_zone(i32::try_from(self.depth).unwrap_or(i32::MAX));
    }
}

/// Anchor and margins (top, right, bottom, left) that hang a panel from
/// `edge` at `offset` along it.
fn panel_anchor(edge: Edge, offset: f32) -> (Anchor, (i32, i32, i32, i32)) {
    let along = margin_px(offset);
    match edge {
        Edge::Top => (Anchor::TOP | Anchor::LEFT, (0, 0, 0, along)),
        Edge::Bottom => (Anchor::BOTTOM | Anchor::LEFT, (0, 0, 0, along)),
        Edge::Left => (Anchor::LEFT | Anchor::TOP, (along, 0, 0, 0)),
        Edge::Right => (Anchor::RIGHT | Anchor::TOP, (along, 0, 0, 0)),
    }
}

/// The adapter and device every surface of the dock renders with.
struct Gpu {
    state: egui_wgpu::RenderState,
}

/// One surface's swapchain and egui renderer.
struct Target {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    renderer: egui_wgpu::Renderer,
}

impl Gpu {
    /// Pick an adapter that can present to `surface`, and its target.
    fn new(
        conn: &Connection,
        surface: &wl_surface::WlSurface,
        transparent: bool,
    ) -> Result<(Self, Target), GfxError> {
        let config = egui_wgpu::WgpuConfiguration::default();
        let instance = block_on(config.wgpu_setup.new_instance());
        let target = wgpu_surface(&instance, conn, surface)?;
        let state = block_on(egui_wgpu::RenderState::create(
            &config,
            &instance,
            Some(&target),
            egui_wgpu::RendererOptions::default(),
        ))
        .map_err(backend_error)?;
        let gpu = Self { state };
        let target = gpu.target(target, transparent)?;
        Ok((gpu, target))
    }

    /// A target for another surface of this connection, on the same device.
    fn open(
        &self,
        conn: &Connection,
        surface: &wl_surface::WlSurface,
        transparent: bool,
    ) -> Result<Target, GfxError> {
        let target = wgpu_surface(&self.state.instance, conn, surface)?;
        self.target(target, transparent)
    }

    fn target(
        &self,
        target: wgpu::Surface<'static>,
        transparent: bool,
    ) -> Result<Target, GfxError> {
        let state = &self.state;
        let caps = target.get_capabilities(&state.adapter);
        let alpha_mode = if transparent
            && caps
                .alpha_modes
                .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
        {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else {
            caps.alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto)
        };
        let format = if caps.formats.contains(&state.target_format) {
            state.target_format
        } else {
            caps.formats
                .first()
                .copied()
                .ok_or_else(|| backend_error("the GPU cannot present to this surface"))?
        };
        let mut usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        if caps.usages.contains(wgpu::TextureUsages::COPY_SRC) {
            // Lets `Frame::request_screenshot` read the presented frame.
            usage |= wgpu::TextureUsages::COPY_SRC;
        }
        let config = wgpu::SurfaceConfiguration {
            usage,
            format,
            alpha_mode,
            view_formats: vec![format],
            ..target
                .get_default_config(&state.adapter, 1, 1)
                .ok_or_else(|| backend_error("the GPU cannot present to this surface"))?
        };
        let renderer = egui_wgpu::Renderer::new(
            &state.device,
            format,
            egui_wgpu::RendererOptions::default(),
        );
        Ok(Target {
            surface: target,
            config,
            renderer,
        })
    }
}

fn wgpu_surface(
    instance: &wgpu::Instance,
    conn: &Connection,
    surface: &wl_surface::WlSurface,
) -> Result<wgpu::Surface<'static>, GfxError> {
    let display = NonNull::new(conn.backend().display_ptr().cast::<std::ffi::c_void>())
        .ok_or_else(|| backend_error("null wl_display"))?;
    let window = NonNull::new(surface.id().as_ptr().cast::<std::ffi::c_void>())
        .ok_or_else(|| backend_error("null wl_surface"))?;
    // SAFETY: both handles come from `conn` and `surface`. The `Target`
    // holding this surface sits in a `View` beside the layer surface that
    // owns `surface`, declared (and so dropped) before it, and every `View`
    // is dropped before `Dock::_conn`.
    unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
                display,
            ))),
            raw_window_handle: RawWindowHandle::Wayland(WaylandWindowHandle::new(window)),
        })
    }
    .map_err(backend_error)
}

impl Target {
    fn resize(&mut self, gpu: &Gpu, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&gpu.state.device, &self.config);
    }

    /// Paint `jobs` into `output`, present it, and read it back first when
    /// `capture` is set and the surface allows copies.
    #[allow(clippy::too_many_arguments, reason = "one paint's inputs")]
    fn paint(
        &mut self,
        gpu: &Gpu,
        output: wgpu::SurfaceTexture,
        jobs: &[egui::ClippedPrimitive],
        textures: &mut egui::TexturesDelta,
        screen: &egui_wgpu::ScreenDescriptor,
        clear: [f32; 4],
        capture: bool,
    ) -> Result<Option<egui::ColorImage>, GfxError> {
        let state = &gpu.state;
        let renderer = &mut self.renderer;
        for (id, deltas) in &textures.set {
            for delta in deltas {
                renderer.update_texture(&state.device, &state.queue, *id, delta);
            }
        }
        let mut encoder = state
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let user_buffers =
            renderer.update_buffers(&state.device, &state.queue, &mut encoder, jobs, screen);
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("gfx dock"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(clear[0]),
                            g: f64::from(clear[1]),
                            b: f64::from(clear[2]),
                            a: f64::from(clear[3]),
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..wgpu::RenderPassDescriptor::default()
            });
            renderer.render(&mut pass.forget_lifetime(), jobs, screen);
        }
        for id in &textures.free {
            renderer.free_texture(id);
        }
        textures.clear();

        let readable = self.config.usage.contains(wgpu::TextureUsages::COPY_SRC);
        let captured = if capture && readable {
            let image = read_back(
                &state.device,
                &state.queue,
                &output.texture,
                encoder,
                user_buffers,
            )?;
            Some(to_rgba(image, self.config.format))
        } else {
            state
                .queue
                .submit(user_buffers.into_iter().chain([encoder.finish()]));
            None
        };
        state.queue.present(output);
        Ok(captured)
    }
}

/// One layer surface: its swapchain, egui context and frame clock. Field
/// order is drop order: `target` holds a raw pointer to `surface`.
struct View {
    target: Target,
    layer: LayerSurface,
    surface: wl_surface::WlSurface,
    ctx: egui::Context,
    /// The theme `ctx` was styled with; its background alpha is the clear.
    theme: Theme,
    /// Logical size from the last configure.
    size: (u32, u32),
    /// Integer buffer scale of the output the surface is on.
    scale: i32,
    configured: bool,
    /// A `wl_surface.frame` callback is outstanding: drawing now would only
    /// queue behind it.
    frame_pending: bool,
    resized: bool,
    /// Holds keyboard focus (only a panel ever does).
    focused: bool,
    egui_events: Vec<egui::Event>,
    events: Vec<Event>,
    /// When the next frame is due; `None` until input or the app asks.
    next_frame: Option<Instant>,
}

impl View {
    fn new(target: Target, layer: LayerSurface, surface: wl_surface::WlSurface, theme: Theme) -> Self {
        let ctx = egui::Context::default();
        apply_theme(&ctx, theme);
        Self {
            target,
            layer,
            surface,
            ctx,
            theme,
            size: (0, 0),
            scale: 1,
            configured: false,
            frame_pending: false,
            resized: false,
            focused: false,
            egui_events: Vec::new(),
            events: Vec::new(),
            next_frame: Some(Instant::now()),
        }
    }

    fn due(&self) -> bool {
        self.configured
            && !self.frame_pending
            && self.next_frame.is_some_and(|at| at <= Instant::now())
    }

    /// Schedule a frame `delay` from now, keeping an earlier one.
    fn request_frame(&mut self, delay: Duration) {
        let Some(at) = Instant::now().checked_add(delay) else {
            return;
        };
        self.next_frame = Some(self.next_frame.map_or(at, |next| next.min(at)));
    }

    /// Run one frame of `body` and present it. `shared` carries the app's
    /// requests back; `None` when the surface was not ready to draw.
    #[allow(clippy::cast_precision_loss, reason = "surface sizes are far below 2^24")]
    fn draw(
        &mut self,
        gpu: &Gpu,
        qh: &QueueHandle<Dock>,
        start: Instant,
        transparent: bool,
        mut shared: Shared,
        body: &mut dyn FnMut(&mut crate::gfx::Frame<'_>),
    ) -> Result<Option<Shared>, GfxError> {
        let (width, height) = self.size;
        let scale = u32::try_from(self.scale.max(1)).unwrap_or(1);
        let (px_w, px_h) = (width * scale, height * scale);
        if self.resized {
            self.target.resize(gpu, px_w, px_h);
            self.surface.set_buffer_scale(self.scale.max(1));
            self.resized = false;
        }
        let output = match self.target.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.resized = true;
                return Ok(None);
            }
            other => return Err(backend_error(format!("surface texture: {other:?}"))),
        };

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width as f32, height as f32),
            )),
            time: Some(start.elapsed().as_secs_f64()),
            focused: self.focused,
            events: std::mem::take(&mut self.egui_events),
            ..egui::RawInput::default()
        };
        self.ctx.set_pixels_per_point(self.scale.max(1) as f32);
        shared.events.append(&mut self.events);
        let full = self.ctx.run_ui(input, |ui| {
            let mut translated = Vec::new();
            ui.ctx()
                .input(|input| input::translate(&input.events, &mut translated));
            shared.events.extend(translated);
            draw(ui, transparent, &mut shared, body);
        });

        let egui::FullOutput {
            shapes,
            pixels_per_point,
            mut textures_delta,
            viewport_output,
            ..
        } = full;
        let jobs = self.ctx.tessellate(shapes, pixels_per_point);
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [px_w, px_h],
            pixels_per_point,
        };
        let clear = clear_color(&self.ctx, self.theme, transparent);
        // Ask for the next frame callback before the commit that present does.
        self.surface.frame(qh, self.surface.clone());
        self.frame_pending = true;
        let captured = self.target.paint(
            gpu,
            output,
            &jobs,
            &mut textures_delta,
            &screen,
            clear,
            shared.screenshot,
        )?;
        let delay = viewport_output
            .get(&egui::ViewportId::ROOT)
            .map_or(Duration::MAX, |viewport| viewport.repaint_delay);
        self.next_frame = None;
        self.request_frame(delay);
        if let Some(image) = captured {
            // Delivered as an event next frame, as on the eframe path.
            self.events.push(screenshot_event(&image));
            self.request_frame(Duration::ZERO);
        }
        Ok(Some(shared))
    }
}

/// An open drop-down panel and the options it was opened with.
struct Panel {
    view: View,
    options: PanelOptions,
    /// The dock start its offset was placed against.
    start: f32,
}

/// Which surface of the dock an event belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    Main,
    Panel,
}

/// Field order is drop order: the views hold raw pointers into their
/// surfaces and the connection, so they go first and `_conn` last.
struct Dock {
    main: View,
    /// The main surface's anchor, size and reserved space.
    shape: Shape,
    panel: Option<Panel>,
    gpu: Gpu,
    /// The output asked for, which panels open on too; `None` is the
    /// compositor's choice.
    output: Option<wl_output::WlOutput>,
    /// A named output was asked for and is not connected.
    fallback: bool,
    /// The output last reported as `Event::Monitor`.
    monitor: Option<String>,
    chooser: Chooser,
    registry: RegistryState,
    outputs: OutputState,
    seats: SeatState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    closed: bool,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// The compositor's keymap, for panels' text input.
    keymap: Option<Keymap>,
    app: Box<dyn App>,
    title: String,
    theme: Theme,
    transparent: bool,
    /// The desktop's key repeat setting.
    repeat: Repeat,
    /// The key held in the panel and when it next repeats.
    held: Option<(u32, Instant)>,
    start: Instant,
    qh: QueueHandle<Self>,
    _conn: Connection,
}

impl Dock {
    fn view(&mut self, surface: &wl_surface::WlSurface) -> Option<(Which, &mut View)> {
        if *surface == self.main.surface {
            return Some((Which::Main, &mut self.main));
        }
        match &mut self.panel {
            Some(panel) if *surface == panel.view.surface => Some((Which::Panel, &mut panel.view)),
            _ => None,
        }
    }

    fn maybe_draw(&mut self) -> Result<(), GfxError> {
        if self.main.due() {
            let mut shared = Shared::new(Vec::new());
            shared.panels = true;
            shared.dock = true;
            shared.monitors = self.monitor_list();
            let app = &mut *self.app;
            let drawn = self.main.draw(
                &self.gpu,
                &self.qh,
                self.start,
                self.transparent,
                shared,
                &mut |frame| app.frame(frame),
            )?;
            if let Some(shared) = drawn {
                if let Some(theme) = shared.theme {
                    self.set_theme(theme);
                }
                if shared.close {
                    self.closed = true;
                }
                if let Some(length) = shared.dock_length {
                    self.reshape(Shape {
                        length,
                        ..self.shape
                    });
                }
                if let Some(monitor) = shared.monitor {
                    if self.relocate(&monitor)? {
                        // The panel belonged to the old output.
                        return Ok(());
                    }
                }
                self.sync_panel(shared.panel)?;
            }
        }
        let ready = self.panel.as_ref().is_some_and(|panel| panel.view.due());
        if ready {
            let Some(panel) = self.panel.as_mut() else {
                return Ok(());
            };
            let had_input = !panel.view.egui_events.is_empty();
            let app = &mut *self.app;
            let drawn = panel.view.draw(
                &self.gpu,
                &self.qh,
                self.start,
                self.transparent,
                Shared::new(Vec::new()),
                &mut |frame| app.panel(frame),
            )?;
            if let Some(shared) = drawn {
                if let Some(theme) = shared.theme {
                    self.set_theme(theme);
                }
                if shared.close {
                    self.dismiss_panel();
                } else if had_input {
                    // The app may have decided to close the panel (Esc, a
                    // row click): give its dock frame a chance to say so.
                    self.main.request_frame(Duration::ZERO);
                }
            }
        }
        Ok(())
    }

    /// Open, move or close the panel to match what the dock frame asked.
    fn sync_panel(&mut self, wanted: Option<PanelOptions>) -> Result<(), GfxError> {
        let Some(options) = wanted else {
            self.panel = None;
            return Ok(());
        };
        let start = self.shape.start();
        if let Some(panel) = &mut self.panel {
            if panel.options != options || panel.start != start {
                configure_panel(&panel.view.layer, &options, start);
                panel.view.layer.commit();
                panel.options = options;
                panel.start = start;
            }
            return Ok(());
        }
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface.clone(),
            Layer::Top,
            Some(format!("{} panel", self.title)),
            self.output.as_ref(),
        );
        configure_panel(&layer, &options, start);
        // Focus when the user clicks it or it maps; losing it closes it.
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
        layer.commit();
        let target = self.gpu.open(&self._conn, &surface, self.transparent)?;
        let mut view = View::new(target, layer, surface, self.theme.for_panel());
        view.scale = self.main.scale;
        self.panel = Some(Panel {
            view,
            options,
            start,
        });
        Ok(())
    }

    /// The outputs the dock can move to, in the compositor's order.
    fn monitor_list(&self) -> Vec<MonitorInfo> {
        self.outputs
            .outputs()
            .filter_map(|output| self.outputs.info(&output))
            .map(|info| {
                let name = info.name.unwrap_or_default();
                let current = self.monitor.as_ref() == Some(&name);
                MonitorInfo::new(name, current)
            })
            .collect()
    }

    /// Move the dock to `monitor`. A layer surface's output is fixed when it
    /// is created, so the main surface is destroyed (which releases its
    /// exclusive zone on the old output) and created again on the new one,
    /// keeping its egui context and shape. `Ok(true)` when it moved.
    fn relocate(&mut self, monitor: &Monitor) -> Result<bool, GfxError> {
        let (output, info) = match monitor {
            Monitor::Named(name) => {
                if !self.fallback && self.monitor.as_ref() == Some(name) {
                    return Ok(false);
                }
                let found = self.outputs.outputs().find_map(|output| {
                    let info = self.outputs.info(&output)?;
                    (info.name.as_ref() == Some(name)).then_some((output, info))
                });
                let Some((output, info)) = found else {
                    // Unplugged between the frame's check and now.
                    return Err(GfxError::new(
                        GfxErrorKind::UnknownMonitor,
                        format!("set_monitor: {name:?} is no longer connected"),
                    ));
                };
                if !self.fallback && self.output.as_ref() == Some(&output) {
                    // Already there, or on its way (before the enter arrives).
                    return Ok(false);
                }
                (Some(output), Some(info))
            }
            _ => {
                if self.output.is_none() && !self.fallback {
                    return Ok(false);
                }
                (None, None)
            }
        };
        if self.panel.is_some() {
            self.held = None;
            self.dismiss_panel();
        }
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface.clone(),
            Layer::Top,
            Some(self.title.clone()),
            output.as_ref(),
        );
        self.shape.edge_len = info.and_then(|info| edge_length(self.shape.edge, info.logical_size));
        self.shape.apply(&layer);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        let mut target = self.gpu.open(&self._conn, &surface, self.transparent)?;
        if target.config.format == self.main.target.config.format {
            // The renderer holds the textures egui already uploaded (the
            // font atlas); egui sends only deltas from here on.
            std::mem::swap(&mut target.renderer, &mut self.main.target.renderer);
        } else {
            // A fresh context uploads every texture again.
            self.main.ctx = egui::Context::default();
            apply_theme(&self.main.ctx, self.main.theme);
        }
        // Field by field, in drop order: the old swapchain goes while its
        // surface is still alive, then the old layer surface (destroying it
        // and its wl_surface), so the compositor releases the old zone.
        self.main.target = target;
        self.main.layer = layer;
        self.main.surface = surface;
        self.main.size = (0, 0);
        self.main.configured = false;
        self.main.frame_pending = false;
        self.main.resized = true;
        self.main.request_frame(Duration::ZERO);
        self.output = output;
        self.fallback = false;
        // The enter on the new output reports `Event::Monitor` again.
        self.monitor = None;
        Ok(true)
    }

    /// Repeat the key held in the panel, at the desktop's delay and rate.
    fn repeat_held(&mut self) {
        let Some((code, at)) = self.held else {
            return;
        };
        let now = Instant::now();
        if at > now {
            return;
        }
        let (Some(interval), Some(panel)) = (
            self.repeat.interval,
            self.panel.as_mut().filter(|panel| panel.view.focused),
        ) else {
            self.held = None;
            return;
        };
        push_key(&mut panel.view, self.keymap.as_ref(), code, true, true);
        // A stalled loop does not burst the missed repeats.
        let next = at + interval;
        self.held = Some((code, if next > now { next } else { now + interval }));
    }

    /// Ask the compositor for a new main-surface shape, if it changed. The
    /// configure that answers resizes the surface and schedules a frame.
    fn reshape(&mut self, shape: Shape) {
        if shape != self.shape {
            let before = self.shape.geometry();
            self.shape = shape;
            if shape.geometry() != before {
                shape.apply(&self.main.layer);
                self.main.layer.commit();
            }
        }
    }

    /// Close the panel on the backend's own initiative and tell the app.
    fn dismiss_panel(&mut self) {
        if self.panel.take().is_some() {
            self.main.events.push(Event::PanelDismissed);
            self.main.request_frame(Duration::ZERO);
        }
    }

    fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        apply_theme(&self.main.ctx, theme);
        self.main.theme = theme;
        self.main.request_frame(Duration::ZERO);
        if let Some(panel) = &mut self.panel {
            apply_theme(&panel.view.ctx, theme.for_panel());
            panel.view.theme = theme.for_panel();
            panel.view.request_frame(Duration::ZERO);
        }
    }
}

fn configure_panel(layer: &LayerSurface, options: &PanelOptions, dock_start: f32) {
    let (anchor, (top, right, bottom, left)) =
        panel_anchor(options.edge, dock_start + options.offset);
    layer.set_anchor(anchor);
    layer.set_margin(top, right, bottom, left);
    layer.set_size(logical_px(options.size.0), logical_px(options.size.1));
    // Zero: kept clear of the dock's exclusive zone, so it opens below it.
    layer.set_exclusive_zone(0);
}

/// The read-back bytes are in surface order; `screenshot_event` wants RGBA.
fn to_rgba(image: egui::ColorImage, format: wgpu::TextureFormat) -> egui::ColorImage {
    if !matches!(
        format,
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    ) {
        return image;
    }
    let pixels = image
        .pixels
        .iter()
        .map(|px| egui::Color32::from_rgba_premultiplied(px.b(), px.g(), px.r(), px.a()))
        .collect();
    egui::ColorImage::new(image.size, pixels)
}

impl CompositorHandler for Dock {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        factor: i32,
    ) {
        if let Some((_, view)) = self.view(surface) {
            if factor != view.scale {
                view.scale = factor;
                view.resized = true;
                view.request_frame(Duration::ZERO);
            }
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if let Some((_, view)) = self.view(surface) {
            view.frame_pending = false;
        }
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        if *surface != self.main.surface {
            return;
        }
        // The compositor sends one enter per binding of the output, and the
        // chooser bound them too: name it from either, report it once.
        let info = self
            .outputs
            .info(output)
            .or_else(|| self.chooser.state.outputs.info(output));
        let edge_len = info
            .as_ref()
            .and_then(|info| edge_length(self.shape.edge, info.logical_size));
        if edge_len.is_some() {
            self.reshape(Shape {
                edge_len,
                ..self.shape
            });
        }
        let name = info.and_then(|info| info.name).unwrap_or_default();
        if self.monitor.as_ref() == Some(&name) {
            return;
        }
        self.monitor = Some(name.clone());
        self.main.events.push(Event::Monitor {
            name,
            fallback: self.fallback,
        });
        self.main.request_frame(Duration::ZERO);
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for Dock {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if layer.wl_surface() == &self.main.surface {
            self.closed = true;
        } else {
            self.dismiss_panel();
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let Some((_, view)) = self.view(layer.wl_surface()) else {
            return;
        };
        let (width, height) = configure.new_size;
        let size = (width.max(1), height.max(1));
        if size != view.size {
            view.size = size;
            view.resized = true;
        }
        view.configured = true;
        // SCTK acks the configure; the first buffer must follow it.
        view.frame_pending = false;
        view.request_frame(Duration::ZERO);
    }
}

impl OutputHandler for Dock {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
    }
}

impl SeatHandler for Dock {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seats.get_pointer(qh, &seat).ok();
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            // Raw wl_keyboard: named keys are mapped from evdev codes
            // directly; text goes through the compositor's keymap
            // (`gfx_keymap`), loaded at run time.
            self.keyboard = Some(seat.get_keyboard(qh, ()));
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
        if capability == Capability::Keyboard {
            if let Some(keyboard) = self.keyboard.take() {
                keyboard.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

/// Linux evdev button codes.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

impl PointerHandler for Dock {
    #[allow(clippy::cast_possible_truncation, reason = "surface coordinates fit f32")]
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let Some((_, view)) = self.view(&event.surface) else {
                continue;
            };
            let pos = egui::pos2(event.position.0 as f32, event.position.1 as f32);
            let button = |code| match code {
                BTN_LEFT => Some(egui::PointerButton::Primary),
                BTN_RIGHT => Some(egui::PointerButton::Secondary),
                BTN_MIDDLE => Some(egui::PointerButton::Middle),
                _ => None,
            };
            let translated = match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    Some(egui::Event::PointerMoved(pos))
                }
                PointerEventKind::Leave { .. } => Some(egui::Event::PointerGone),
                PointerEventKind::Press { button: code, .. }
                | PointerEventKind::Release { button: code, .. } => {
                    button(code).map(|button| egui::Event::PointerButton {
                        pos,
                        button,
                        pressed: matches!(event.kind, PointerEventKind::Press { .. }),
                        modifiers: egui::Modifiers::NONE,
                    })
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } => Some(egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(-horizontal.absolute as f32, -vertical.absolute as f32),
                    modifiers: egui::Modifiers::NONE,
                    phase: egui::TouchPhase::Move,
                    source: egui::MouseWheelSource::Unknown,
                }),
            };
            if let Some(event) = translated {
                view.egui_events.push(event);
                view.request_frame(Duration::ZERO);
            }
        }
    }
}

/// The named keys a panel receives, by Linux evdev key code.
fn named_key(code: u32) -> Option<egui::Key> {
    Some(match code {
        1 => egui::Key::Escape,
        14 => egui::Key::Backspace,
        15 => egui::Key::Tab,
        28 | 96 => egui::Key::Enter,
        57 => egui::Key::Space,
        102 => egui::Key::Home,
        103 => egui::Key::ArrowUp,
        104 => egui::Key::PageUp,
        105 => egui::Key::ArrowLeft,
        106 => egui::Key::ArrowRight,
        107 => egui::Key::End,
        108 => egui::Key::ArrowDown,
        109 => egui::Key::PageDown,
        111 => egui::Key::Delete,
        _ => return None,
    })
}

/// Queue one key press, release or repeat (and the text it types) for
/// `view`. Text is typed on press, after the key event, as winit orders them.
fn push_key(view: &mut View, keymap: Option<&Keymap>, code: u32, pressed: bool, repeat: bool) {
    let mods = keymap.map(Keymap::mods).unwrap_or_default();
    let modifiers = egui::Modifiers {
        alt: mods.alt,
        ctrl: mods.ctrl,
        shift: mods.shift,
        mac_cmd: false,
        command: mods.ctrl,
    };
    if let Some(key) = named_key(code) {
        view.egui_events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat,
            modifiers,
        });
    }
    if let Some(text) = keymap.filter(|_| pressed).and_then(|keymap| keymap.text(code)) {
        view.egui_events.push(egui::Event::Text(text));
    }
    view.request_frame(Duration::ZERO);
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Dock {
    fn event(
        dock: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { surface, .. } => {
                if let Some((_, view)) = dock.view(&surface) {
                    view.focused = true;
                    view.egui_events.push(egui::Event::WindowFocused(true));
                    view.request_frame(Duration::ZERO);
                }
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                dock.held = None;
                if let Some((which, view)) = dock.view(&surface) {
                    view.focused = false;
                    if which == Which::Panel {
                        // Focus went to another window: the user clicked away.
                        dock.dismiss_panel();
                    }
                }
            }
            wl_keyboard::Event::Keymap { format, fd, size } => {
                // Anything but an XKB text keymap leaves named keys only.
                dock.keymap = matches!(
                    format,
                    smithay_client_toolkit::reexports::client::WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)
                )
                .then(|| Keymap::from_fd(fd, size))
                .flatten();
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(keymap) = &mut dock.keymap {
                    keymap.set_modifiers(mods_depressed, mods_latched, mods_locked, group);
                }
            }
            wl_keyboard::Event::Key { key: code, state, .. } => {
                let Some(panel) = dock.panel.as_mut().filter(|panel| panel.view.focused) else {
                    dock.held = None;
                    return;
                };
                let pressed = matches!(
                    state,
                    smithay_client_toolkit::reexports::client::WEnum::Value(wl_keyboard::KeyState::Pressed)
                );
                push_key(&mut panel.view, dock.keymap.as_ref(), code, pressed, false);
                if pressed {
                    let types_text = dock.keymap.as_ref().and_then(|keymap| keymap.text(code)).is_some();
                    // A new press takes over the repeat, as on every desktop.
                    dock.held = (dock.repeat.interval.is_some() && repeats(code, types_text))
                        .then(|| (code, Instant::now() + dock.repeat.delay));
                } else if dock.held.is_some_and(|(held, _)| held == code) {
                    dock.held = None;
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                dock.repeat = Repeat::from_info(rate, delay);
                eprintln!("gfx dock: key repeat from the desktop: {rate}/s after {delay} ms");
                if dock.repeat.interval.is_none() {
                    dock.held = None;
                }
            }
            _ => {}
        }
    }
}

impl ProvidesRegistryState for Dock {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(Dock);
delegate_output!(Dock);
delegate_seat!(Dock);
delegate_pointer!(Dock);
delegate_layer!(Dock);
delegate_registry!(Dock);

#[cfg(test)]
mod tests {
    use super::{
        choose, logical_px, named_key, panel_anchor, repeats, to_rgba, Anchor, Choice, DockAlign, DockLength,
        Repeat, Shape,
    };
    use std::time::Duration;
    use crate::gfx::{Edge, Monitor};

    #[test]
    fn a_named_output_is_found_or_falls_back_to_primary() {
        let names = [Some("Virtual-1".to_owned()), None, Some("Virtual-2".to_owned())];
        assert_eq!(choose(&names, &Monitor::Primary), Choice::Primary);
        assert_eq!(choose(&names, &Monitor::Named("Virtual-2".into())), Choice::Output(2));
        assert_eq!(choose(&names, &Monitor::Named("HDMI-A-1".into())), Choice::Fallback);
        assert_eq!(choose(&[], &Monitor::Named("Virtual-1".into())), Choice::Fallback);
    }

    #[test]
    fn a_short_dock_reserves_its_thickness_and_never_anchors_to_a_corner() {
        let shape = |edge, length, align, edge_len| Shape {
            edge,
            depth: 36,
            length,
            align,
            edge_len,
        };
        let full = shape(Edge::Top, DockLength::Full, DockAlign::Center, None).geometry();
        assert_eq!(full, (Anchor::TOP | Anchor::LEFT | Anchor::RIGHT, (0, 36), (0, 0, 0, 0)));
        let fit = DockLength::Fixed(900.4);
        let centred = shape(Edge::Top, fit, DockAlign::Center, Some(1920)).geometry();
        assert_eq!(centred, (Anchor::TOP, (900, 36), (0, 0, 0, 0)));
        let start = shape(Edge::Top, fit, DockAlign::Start, Some(1920)).geometry();
        assert_eq!(start, (Anchor::TOP | Anchor::LEFT | Anchor::RIGHT, (0, 36), (0, 1020, 0, 0)));
        let end = shape(Edge::Bottom, fit, DockAlign::End, Some(1920)).geometry();
        assert_eq!(end, (Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT, (0, 36), (0, 0, 0, 1020)));
        let side = shape(Edge::Left, fit, DockAlign::Start, Some(1080)).geometry();
        assert_eq!(side, (Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM, (36, 0), (0, 0, 180, 0)));
        let start = |align, edge_len| shape(Edge::Top, fit, align, edge_len).start();
        assert!((start(DockAlign::Center, Some(1920)) - 510.0).abs() < f32::EPSILON);
        assert!((start(DockAlign::End, Some(1920)) - 1020.0).abs() < f32::EPSILON);
        assert!(start(DockAlign::Start, Some(1920)).abs() < f32::EPSILON);
        assert!(start(DockAlign::Center, None).abs() < f32::EPSILON);
        let unknown = shape(Edge::Top, fit, DockAlign::End, None).geometry();
        assert_eq!(unknown, full, "the whole edge until the output's size is known");
        let wider = shape(Edge::Top, DockLength::Fixed(4000.0), DockAlign::Start, Some(1920));
        assert_eq!(wider.geometry().2, (0, 0, 0, 0), "never a negative margin");
        let wider = shape(Edge::Top, DockLength::Fixed(1286.0), DockAlign::Center, Some(1280));
        assert_eq!(wider.geometry().1, (1280, 36), "clamped to the edge");
        assert!(wider.start().abs() < f32::EPSILON);
        assert_eq!(super::edge_length(Edge::Right, Some((1920, 1080))), Some(1080));
        assert_eq!(super::edge_length(Edge::Top, Some((0, 1080))), None);
    }

    #[test]
    fn thickness_rounds_to_a_whole_nonzero_size() {
        assert_eq!(logical_px(36.4), 36);
        assert_eq!(logical_px(0.2), 1);
    }

    #[test]
    fn a_panel_hangs_from_its_edge_at_its_offset() {
        let (anchor, margins) = panel_anchor(Edge::Top, 412.6);
        assert_eq!(anchor, Anchor::TOP | Anchor::LEFT);
        assert_eq!(margins, (0, 0, 0, 413));
        let (anchor, margins) = panel_anchor(Edge::Right, 36.0);
        assert_eq!(anchor, Anchor::RIGHT | Anchor::TOP);
        assert_eq!(margins, (36, 0, 0, 0));
    }

    #[test]
    fn named_keys_map_from_evdev_codes() {
        assert_eq!(named_key(1), Some(egui::Key::Escape));
        assert_eq!(named_key(108), Some(egui::Key::ArrowDown));
        assert_eq!(named_key(30), None, "letters are text, from the keymap");
    }

    #[test]
    fn repeat_info_becomes_a_delay_and_interval() {
        let repeat = Repeat::from_info(25, 600);
        assert_eq!(repeat.delay, Duration::from_millis(600));
        assert_eq!(repeat.interval, Some(Duration::from_millis(40)));
        assert_eq!(Repeat::from_info(0, 600).interval, None, "rate 0 turns repeat off");
        assert_eq!(Repeat::from_info(-3, -1).delay, Duration::ZERO);
    }

    #[test]
    fn editing_keys_repeat_and_acting_keys_do_not() {
        assert!(repeats(30, true), "a letter");
        assert!(repeats(14, false), "Backspace");
        assert!(repeats(105, false), "ArrowLeft");
        assert!(!repeats(28, false), "Enter submits once");
        assert!(!repeats(1, false), "Escape");
        assert!(!repeats(42, false), "Shift types nothing");
    }

    #[test]
    fn bgra_surfaces_read_back_as_rgba() {
        let bgra = egui::ColorImage::new(
            [1, 1],
            vec![egui::Color32::from_rgba_premultiplied(30, 20, 10, 255)],
        );
        let rgba = to_rgba(bgra.clone(), wgpu::TextureFormat::Bgra8Unorm);
        assert_eq!(rgba.pixels[0].to_array(), [10, 20, 30, 255]);
        let same = to_rgba(bgra, wgpu::TextureFormat::Rgba8Unorm);
        assert_eq!(same.pixels[0].to_array(), [30, 20, 10, 255]);
    }

    /// A real eframe window (not a dock) on X11, kept with the Linux host
    /// because it names winit's X11 extension: `request_repaint_after(1s)` alone keeps frames
    /// coming. Runs only on a bare X server (Xvfb) so a test never opens a
    /// window on a developer's Wayland desktop:
    /// `env -u WAYLAND_DISPLAY DISPLAY=:99 cargo test ... gfx_xvfb`.
    #[test]
    fn gfx_xvfb_request_repaint_after_wakes_the_app() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;

        use crate::gfx::{App, WindowOptions};
        use std::time::{Duration, Instant};
        use winit::platform::x11::EventLoopBuilderExtX11;

        if std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_none()
        {
            eprintln!("skipped: needs DISPLAY on a bare X server and no WAYLAND_DISPLAY");
            return;
        }
        struct Ticker {
            frames: Arc<AtomicU32>,
            start: Instant,
        }
        impl App for Ticker {
            fn frame(&mut self, frame: &mut crate::gfx::Frame<'_>) {
                // Only frames past startup can come from the timer.
                if self.start.elapsed() >= Duration::from_millis(300) {
                    self.frames.fetch_add(1, Ordering::SeqCst);
                }
                frame.label("tick");
                if self.start.elapsed() >= Duration::from_millis(2500) {
                    frame.close();
                } else {
                    frame.request_repaint_after(Duration::from_secs(1));
                }
            }
        }
        let frames = Arc::new(AtomicU32::new(0));
        let options = WindowOptions::new("repaint-after").size(160.0, 60.0);
        let mut native = crate::gfx_egui::native_options(&options);
        native.event_loop_builder = Some(Box::new(|builder| {
            builder.with_x11().with_any_thread(true);
        }));
        let app = Ticker {
            frames: Arc::clone(&frames),
            start: Instant::now(),
        };
        let watchdog = Instant::now();
        let result = crate::gfx_egui::run_eframe_with(&options, native, Box::new(app), None);
        let count = frames.load(Ordering::SeqCst);
        assert!(result.is_ok(), "{result:?}");
        // At least two timer wakes in ~2.5 s; bounded, not exact.
        assert!(count >= 2, "only {count} frames after startup");
        assert!(watchdog.elapsed() < Duration::from_secs(10), "{:?}", watchdog.elapsed());
    }
}
