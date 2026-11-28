//! UI-thread WebKitGTK viewport capture. Never returns native images or PNG
//! vectors through the guest boundary; the encoder writes into the shared hub.

use crate::operations::{NativeBlobEncoder, OpaqueToken, OperationHub};
use gio::prelude::CancellableExt;
use gtk::prelude::WidgetExt;
use std::sync::Arc;
use webkit2gtk::{SnapshotOptions, SnapshotRegion, WebViewExt};
use wry::WebViewExtUnix;

use crate::tauri::capture::{CaptureError, NativeCancellation};

fn check_pixels(width: i32, height: i32, scale: i32, maximum: u64) -> Result<(), CaptureError> {
    if width <= 0 || height <= 0 || scale <= 0 || maximum == 0 {
        return Err(CaptureError::InvalidDimensions);
    }
    let pixels = (width as u64)
        .checked_mul(height as u64)
        .and_then(|pixels| pixels.checked_mul(scale as u64))
        .and_then(|pixels| pixels.checked_mul(scale as u64))
        .ok_or(CaptureError::PixelLimit)?;
    if pixels > maximum {
        return Err(CaptureError::PixelLimit);
    }
    Ok(())
}

// The native operation service retains/cancels this handle on its UI thread.
// This adapter is not yet exposed as a generated Wasm operation.
pub(crate) fn capture(
    view: &wry::WebView,
    hub: Arc<OperationHub>,
    store: u64,
    operation: OpaqueToken,
    maximum_pixels: u64,
    maximum_encoded_bytes: usize,
    completed: impl FnOnce(Result<OpaqueToken, CaptureError>) + 'static,
) -> Result<NativeCancellation, CaptureError> {
    let native = view.webview();
    check_pixels(
        native.allocated_width(),
        native.allocated_height(),
        native.scale_factor(),
        maximum_pixels,
    )?;
    let encoder = NativeBlobEncoder::for_operation(hub, store, operation, maximum_encoded_bytes)
        .map_err(|error| match error {
            crate::operations::HubError::Closed | crate::operations::HubError::Invalid => CaptureError::Cancelled,
            _ => CaptureError::BlobLimit,
        })?;
    let cancellation = gio::Cancellable::new();
    let callback_cancellation = cancellation.clone();
    native.snapshot(
        SnapshotRegion::Visible,
        SnapshotOptions::NONE,
        Some(&cancellation),
        move |result| {
            let result = if callback_cancellation.is_cancelled() {
                Err(CaptureError::Cancelled)
            } else {
                result
                    .map_err(|_| CaptureError::NativeFailure)
                    .and_then(|surface| {
                        encode_surface_with(surface, encoder, maximum_pixels, &callback_cancellation,
                            |encoder| encoder.finish_for_operation(operation))
                    })
            };
            completed(result);
        },
    );
    Ok(NativeCancellation::new(move || cancellation.cancel()))
}

#[cfg(test)]
fn encode_surface(
    surface: cairo::Surface,
    encoder: NativeBlobEncoder,
    maximum_pixels: u64,
    cancellation: &gio::Cancellable,
) -> Result<OpaqueToken, CaptureError> {
    encode_surface_with(surface, encoder, maximum_pixels, cancellation, NativeBlobEncoder::finish)
}

fn encode_surface_with(
    surface: cairo::Surface,
    mut encoder: NativeBlobEncoder,
    maximum_pixels: u64,
    cancellation: &gio::Cancellable,
    publish: impl FnOnce(NativeBlobEncoder) -> Result<OpaqueToken, crate::operations::HubError>,
) -> Result<OpaqueToken, CaptureError> {
    if cancellation.is_cancelled() {
        return Err(CaptureError::Cancelled);
    }
    let image = cairo::ImageSurface::try_from(surface).map_err(|_| CaptureError::NativeFailure)?;
    check_pixels(image.width(), image.height(), 1, maximum_pixels)?;
    if image.write_to_png(&mut encoder).is_err() {
        return Err(match encoder.failure() {
            Some(crate::operations::HubError::Quota) => CaptureError::BlobLimit,
            Some(crate::operations::HubError::Closed | crate::operations::HubError::Invalid) => {
                CaptureError::Cancelled
            }
            _ => CaptureError::EncodingFailure,
        });
    }
    if cancellation.is_cancelled() {
        return Err(CaptureError::Cancelled);
    }
    publish(encoder).map_err(|error| match error {
        crate::operations::HubError::Closed | crate::operations::HubError::Invalid => CaptureError::Cancelled,
        _ => CaptureError::BlobLimit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires WebKitGTK 4.1 and an X11 display (run under xvfb-run)"]
    fn live_webkit_viewport_png_completes_capture_operation() {
        use gtk::prelude::GtkWindowExt as _;
        use std::cell::{Cell, RefCell};
        use std::rc::Rc;
        use std::time::{Duration, Instant};
        use wry::WebViewBuilderExtUnix as _;

        fn pump_until(mut ready: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !ready() {
                assert!(Instant::now() < deadline, "native browser callback timed out");
                while gtk::events_pending() {
                    gtk::main_iteration_do(false);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        gtk::init().unwrap();
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_default_size(320, 240);
        let loaded = Rc::new(Cell::new(false));
        let loaded_callback = Rc::clone(&loaded);
        let view = wry::WebViewBuilder::new()
            .with_html("<!doctype html><style>html,body{margin:0;width:100%;height:100%;background:#ff0000}div{position:absolute;left:50%;top:0;width:50%;height:100%;background:#0000ff}</style><div></div>")
            .with_on_page_load_handler(move |event, _| {
                if matches!(event, wry::PageLoadEvent::Finished) {
                    loaded_callback.set(true);
                }
            })
            .build_gtk(&window)
            .unwrap();
        window.show_all();
        pump_until(|| loaded.get());

        let hub = OperationHub::with_blob_limits(
            8, 4, crate::operations::BlobLimits::new(1024, 1024 * 1024, 1024 * 1024).unwrap(),
        ).unwrap();
        let (resource, open) = hub.begin_external_webview_open(1).unwrap();
        hub.finish_external_open(open, resource);
        hub.observe_terminal(1, open).unwrap().unwrap();
        let operation = hub.begin_external_webview_capture(1, resource).unwrap();
        let result = Rc::new(RefCell::new(None));
        let callback_result = Rc::clone(&result);
        let cancellation = capture(&view, Arc::clone(&hub), 1, operation, 4_000_000, 1024 * 1024,
            move |value| *callback_result.borrow_mut() = Some(value)).unwrap();
        pump_until(|| result.borrow().is_some());
        let blob = result.borrow_mut().take().unwrap().unwrap();
        let terminal = hub.observe_terminal(1, operation).unwrap().unwrap();
        assert_eq!(terminal.terminal, crate::operations::Terminal::Completed);
        assert_eq!(terminal.resource, Some(blob));
        let mut encoded = Vec::new(); // Test-only decoding, never the guest ABI.
        loop {
            let chunk = hub.blob_read(1, blob, 1024).unwrap();
            if chunk.is_empty() { break; }
            encoded.extend(chunk);
        }
        let mut image = cairo::ImageSurface::create_from_png(&mut std::io::Cursor::new(encoded)).unwrap();
        let native = view.webview();
        let width = native.allocated_width() * native.scale_factor();
        let height = native.allocated_height() * native.scale_factor();
        assert_eq!((image.width(), image.height()), (width, height));
        let stride = image.stride() as usize;
        let pixels = image.data().unwrap();
        for (x, expected) in [(width / 4, 0x00ff0000_u32), (width * 3 / 4, 0x000000ff_u32)] {
            let offset = height as usize / 2 * stride + x as usize * 4;
            let pixel = u32::from_ne_bytes(pixels[offset..offset + 4].try_into().unwrap());
            assert_eq!(pixel & 0x00ffffff, expected, "viewport region has incorrect color");
        }
        drop(pixels);
        drop(image);
        drop(cancellation);
        hub.close_resource(blob).unwrap();

        let cancelled_operation = hub.begin_external_webview_capture(1, resource).unwrap();
        let cancelled_result = Rc::new(RefCell::new(None));
        let callback_result = Rc::clone(&cancelled_result);
        let cancellation = capture(&view, Arc::clone(&hub), 1, cancelled_operation, 4_000_000, 1024 * 1024,
            move |value| *callback_result.borrow_mut() = Some(value)).unwrap();
        hub.finish_external_operation(cancelled_operation, crate::operations::Terminal::Cancelled);
        cancellation.cancel();
        pump_until(|| cancelled_result.borrow().is_some());
        assert_eq!(cancelled_result.borrow_mut().take().unwrap(), Err(CaptureError::Cancelled));
        let terminal = hub.observe_terminal(1, cancelled_operation).unwrap().unwrap();
        assert_eq!(terminal.terminal, crate::operations::Terminal::Cancelled);
        assert_eq!(terminal.resource, None);
        assert_eq!(hub.snapshot().live_blobs, 0);
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
        drop(cancellation);
        hub.close_all(crate::operations::Terminal::Closed);
        assert_eq!(hub.snapshot().live_resources, 0);
        assert_eq!(hub.snapshot().pending_operations, 0);
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
        drop(native);
        drop(view);
        window.close();
    }

    #[test]
    fn physical_pixel_limit_checks_scale_and_overflow() {
        assert_eq!(check_pixels(10, 20, 2, 800), Ok(()));
        assert_eq!(check_pixels(10, 20, 2, 799), Err(CaptureError::PixelLimit));
        assert_eq!(
            check_pixels(i32::MAX, i32::MAX, i32::MAX, u64::MAX),
            Err(CaptureError::PixelLimit)
        );
        assert_eq!(
            check_pixels(0, 20, 1, 800),
            Err(CaptureError::InvalidDimensions)
        );
    }

    #[test]
    fn cairo_png_is_decodable_and_uses_only_the_shared_blob_sink() {
        let hub = OperationHub::with_blob_limits(
            4,
            2,
            crate::operations::BlobLimits::new(256, 65_536, 65_536).unwrap(),
        )
        .unwrap();
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 8, 8).unwrap();
        let context = cairo::Context::new(&image).unwrap();
        context.set_source_rgb(1.0, 0.0, 0.0);
        context.paint().unwrap();
        drop(context);
        let encoder = NativeBlobEncoder::new(Arc::clone(&hub), 1, 65_536).unwrap();
        let blob = encode_surface(
            image.as_ref().clone(),
            encoder,
            64,
            &gio::Cancellable::new(),
        )
        .unwrap();
        let mut encoded = Vec::new();
        loop {
            let chunk = hub.blob_read(1, blob, 256).unwrap();
            if chunk.is_empty() {
                break;
            }
            encoded.extend(chunk);
        }
        let decoded =
            cairo::ImageSurface::create_from_png(&mut std::io::Cursor::new(encoded)).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (8, 8));
        hub.close_resource(blob).unwrap();
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
        assert_eq!(hub.snapshot().live_blobs, 0);
    }

    #[test]
    fn png_encoder_limit_is_typed_and_reclaims_partial_encoding() {
        let hub = OperationHub::with_blob_limits(
            4, 2, crate::operations::BlobLimits::new(4, 32, 32).unwrap(),
        ).unwrap();
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 8, 8).unwrap();
        // The PNG signature fits, but the complete encoding cannot. This
        // exercises Cairo's actual writer failure propagation, not a mock.
        let encoder = NativeBlobEncoder::new(Arc::clone(&hub), 1, 8).unwrap();
        assert_eq!(encode_surface(image.as_ref().clone(), encoder, 64, &gio::Cancellable::new()),
            Err(CaptureError::BlobLimit));
        let snapshot = hub.snapshot();
        assert!(snapshot.peak_buffered_blob_bytes <= 8);
        assert_eq!(snapshot.retained_transfer_capacity, 0);
        assert_eq!(snapshot.live_blobs, 0);
    }

    #[test]
    fn late_image_after_hub_teardown_cannot_publish_a_blob() {
        let hub = OperationHub::with_blob_limits(
            4, 2, crate::operations::BlobLimits::new(256, 65_536, 65_536).unwrap(),
        ).unwrap();
        let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 8, 8).unwrap();
        let encoder = NativeBlobEncoder::new(Arc::clone(&hub), 1, 65_536).unwrap();
        hub.close_all(crate::operations::Terminal::Cancelled);
        assert_eq!(encode_surface(image.as_ref().clone(), encoder, 64, &gio::Cancellable::new()),
            Err(CaptureError::Cancelled));
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
        assert_eq!(hub.snapshot().live_blobs, 0);
    }

    #[test]
    fn actual_surface_limit_and_cancel_reclaim_reserved_blob() {
        for cancelled in [false, true] {
            let hub = OperationHub::with_blob_limits(
                4,
                2,
                crate::operations::BlobLimits::new(256, 65_536, 65_536).unwrap(),
            )
            .unwrap();
            let image = cairo::ImageSurface::create(cairo::Format::ARgb32, 8, 8).unwrap();
            let encoder = NativeBlobEncoder::new(Arc::clone(&hub), 1, 65_536).unwrap();
            let cancellation = gio::Cancellable::new();
            if cancelled {
                cancellation.cancel();
            }
            assert_eq!(
                encode_surface(image.as_ref().clone(), encoder, 63, &cancellation),
                Err(if cancelled {
                    CaptureError::Cancelled
                } else {
                    CaptureError::PixelLimit
                })
            );
            assert_eq!(hub.snapshot().live_blobs, 0);
            assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
        }
    }
}
