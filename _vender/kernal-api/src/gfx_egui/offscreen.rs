//! Windowless rendering for [`crate::gfx::snapshot`]: egui laid out over a
//! fixed screen rect and painted by egui-wgpu into an offscreen texture on a
//! surface-less device, so no compositor or frame callback is involved.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use super::{apply_theme, draw, screenshot_event, Shared};
use crate::gfx::{App, Event, GfxError, GfxErrorKind, Snapshot, WindowOptions};

/// Frames laid out before painting: enough for egui's first-frame sizing
/// pass, text-field focus and scroll-to-selection to settle.
const SETTLE_FRAMES: usize = 3;

/// Offscreen format. Non-sRGB, matching egui-wgpu's gamma-space output.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

pub(crate) fn snapshot(options: &WindowOptions, app: &mut dyn App) -> Result<Snapshot, GfxError> {
    let (width, height) = pixel_size(options.size);
    let ctx = egui::Context::default();
    let mut theme = options.theme;
    apply_theme(&ctx, theme);

    let mut textures = Vec::new();
    let mut output = None;
    for _ in 0..SETTLE_FRAMES {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(options.size.0, options.size.1),
            )),
            ..egui::RawInput::default()
        };
        let mut shared = Shared::new(Vec::new());
        let mut full = ctx.run_ui(input, |ui| {
            draw(ui, options.transparent, &mut shared, &mut |frame| {
                app.frame(frame)
            });
        });
        for (id, deltas) in &full.textures_delta.set {
            textures.extend(deltas.iter().map(|delta| (*id, delta.clone())));
        }
        // Applied in order to the renderer below; frees are moot for a
        // renderer that lives for one paint.
        full.textures_delta.clear();
        output = Some(full);
        if let Some(next) = shared.theme {
            theme = next;
            apply_theme(&ctx, theme);
        }
    }
    let Some(output) = output else {
        unreachable!("SETTLE_FRAMES is non-zero");
    };
    let jobs = ctx.tessellate(output.shapes, output.pixels_per_point);
    let clear = super::clear_color(&ctx, theme, options.transparent);
    let image = paint(width, height, &textures, &jobs, clear)?;
    match screenshot_event(&image) {
        Event::Screenshot {
            width,
            height,
            rgba,
        } => Ok(Snapshot::new(width, height, rgba)),
        _ => unreachable!("screenshot_event builds Event::Screenshot"),
    }
}

/// Logical size to whole pixels at one pixel per point, at least 1x1.
fn pixel_size((width, height): (f32, f32)) -> (u32, u32) {
    let clamp = |value: f32| {
        // Validated finite and positive by `WindowOptions::validate`.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped"
        )]
        let pixels = value.round().clamp(1.0, 16384.0) as u32;
        pixels
    };
    (clamp(width), clamp(height))
}

fn backend_error(error: impl std::fmt::Display) -> GfxError {
    GfxError::new(GfxErrorKind::Backend, error.to_string())
}

fn paint(
    width: u32,
    height: u32,
    textures: &[(egui::TextureId, egui::epaint::ImageDelta)],
    jobs: &[egui::ClippedPrimitive],
    clear: [f32; 4],
) -> Result<egui::ColorImage, GfxError> {
    let config = egui_wgpu::WgpuConfiguration::default();
    let instance = block_on(config.wgpu_setup.new_instance());
    let state = block_on(egui_wgpu::RenderState::create(
        &config,
        &instance,
        None,
        egui_wgpu::RendererOptions::default(),
    ))
    .map_err(backend_error)?;
    let (device, queue) = (&state.device, &state.queue);
    let mut renderer =
        egui_wgpu::Renderer::new(device, FORMAT, egui_wgpu::RendererOptions::default());
    for (id, delta) in textures {
        renderer.update_texture(device, queue, *id, delta);
    }

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("gfx snapshot"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let screen = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [width, height],
        pixels_per_point: 1.0,
    };
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    let user_buffers = renderer.update_buffers(device, queue, &mut encoder, jobs, &screen);
    {
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("gfx snapshot"),
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
        renderer.render(&mut pass.forget_lifetime(), jobs, &screen);
    }

    read_back(device, queue, &target, encoder, user_buffers)
}

/// Copy `target` out after the recorded `encoder` work and wait for it.
pub(crate) fn read_back(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    target: &wgpu::Texture,
    mut encoder: wgpu::CommandEncoder,
    user_buffers: Vec<wgpu::CommandBuffer>,
) -> Result<egui::ColorImage, GfxError> {
    let (width, height) = (target.width(), target.height());
    let unpadded = width * 4;
    let padded =
        unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("gfx snapshot readback"),
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        target.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        target.size(),
    );
    queue.submit(user_buffers.into_iter().chain([encoder.finish()]));

    let slice = buffer.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(backend_error)?;
    receiver
        .recv()
        .map_err(backend_error)?
        .map_err(backend_error)?;
    let mapped = slice.get_mapped_range().map_err(backend_error)?;
    let mut pixels = Vec::with_capacity(width as usize * height as usize);
    for row in mapped.chunks(padded as usize) {
        for px in row[..unpadded as usize].as_chunks::<4>().0 {
            pixels.push(egui::Color32::from_rgba_premultiplied(
                px[0], px[1], px[2], px[3],
            ));
        }
    }
    drop(mapped);
    buffer.unmap();
    Ok(egui::ColorImage::new(
        [width as usize, height as usize],
        pixels,
    ))
}

/// Drive a future to completion on this thread. wgpu's native adapter and
/// device requests resolve without an executor; this only parks between
/// polls.
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::pixel_size;
    use crate::gfx::{App, Frame, GfxErrorKind, WindowOptions};

    struct Ui(u32);
    impl App for Ui {
        fn frame(&mut self, frame: &mut Frame<'_>) {
            self.0 += 1;
            frame.heading("snapshot");
            frame.button("a button");
        }
    }

    #[test]
    fn snapshot_paints_the_ui_without_a_window() {
        let mut app = Ui(0);
        let options = WindowOptions::new("t").size(200.0, 100.0);
        let shot = match crate::gfx::snapshot(&options, &mut app) {
            Ok(shot) => shot,
            // A host with no GPU and no software rasteriser: nothing to prove.
            Err(error) if error.kind() == GfxErrorKind::Backend => {
                eprintln!("skipped: {error}");
                return;
            }
            Err(error) => panic!("{error}"),
        };
        assert_eq!(app.0, 3, "settling frames");
        assert_eq!((shot.width(), shot.height()), (200, 100));
        assert_eq!(shot.rgba().len(), 200 * 100 * 4);
        let first = &shot.rgba()[..4];
        assert_eq!(first[3], 255, "opaque panel");
        assert!(
            shot.rgba().as_chunks::<4>().0.iter().any(|px| px != first),
            "text was drawn over the panel"
        );
    }

    #[test]
    fn utility_panel_fixture_renders_both_palettes() {
        for mode in [crate::gfx::ThemeMode::Dark, crate::gfx::ThemeMode::Light] {
            let options = WindowOptions::new("widgets")
                .size(360.0, 340.0)
                .theme(crate::gfx::Theme::with_mode(mode));
            let shot = match crate::gfx::snapshot(&options, &mut super::widget_fixture::StatusPanel)
            {
                Ok(shot) => shot,
                Err(error) if error.kind() == GfxErrorKind::Backend => {
                    eprintln!("skipped: fixture needs a GPU or software rasteriser: {error}");
                    return;
                }
                Err(error) => panic!("{error}"),
            };
            assert_eq!((shot.width(), shot.height()), (360, 340));
            let bg = &shot.rgba()[..4];
            let painted = shot
                .rgba()
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|px| *px != bg)
                .count();
            assert!(painted > 2000, "fixture contains text, bar and sparkline");
            // No pixels should reach the last four rows: all content fits.
            assert!(shot.rgba()[(360 * 336 * 4)..]
                .as_chunks::<4>()
                .0
                .iter()
                .all(|px| px == bg));
        }
    }

    #[test]
    fn pixel_size_rounds_and_clamps() {
        assert_eq!(pixel_size((640.4, 479.6)), (640, 480));
        assert_eq!(pixel_size((0.2, 1.0e9)), (1, 16384));
    }
}

#[cfg(test)]
use crate::gfx;

#[cfg(test)]
#[path = "../../tests/support/gfx_widgets.rs"]
mod widget_fixture;
