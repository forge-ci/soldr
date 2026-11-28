//! Private WKWebView snapshot and callback-backed ImageIO PNG encoding.

use std::cell::RefCell;
use std::ffi::c_void;
use std::io::Write as _;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use objc2_app_kit::NSImage;
use objc2_core_foundation::CFString;
use objc2_core_graphics::{CGDataConsumer, CGDataConsumerCallbacks, CGImage};
use objc2_foundation::NSError;
use objc2_image_io::CGImageDestination;
use wry::WebViewExtMacOS as _;

use crate::operations::{HubError, NativeBlobEncoder, OpaqueToken, OperationHub};

use crate::tauri::capture::{CaptureError, NativeCancellation};

type Encoding = Arc<Mutex<Option<NativeBlobEncoder>>>;

unsafe extern "C-unwind" fn put_bytes(
    info: *mut c_void,
    bytes: NonNull<c_void>,
    count: usize,
) -> usize {
    // The consumer owns a Box<Encoding> until release_info. Core Graphics
    // guarantees the input buffer is readable for count bytes during this call.
    let state = unsafe { &*info.cast::<Encoding>() };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Ok(mut state) = state.lock() else {
            return 0;
        };
        let Some(encoder) = state.as_mut() else {
            return 0;
        };
        let bytes = unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), count) };
        if encoder.write_all(bytes).is_ok() {
            count
        } else {
            0
        }
    }))
    .unwrap_or(0)
}

unsafe extern "C-unwind" fn release_info(info: *mut c_void) {
    // Exactly one retained Arc was transferred to the successful consumer.
    drop(unsafe { Box::from_raw(info.cast::<Encoding>()) });
}

fn map_hub(error: HubError) -> CaptureError {
    match error {
        HubError::Closed | HubError::Invalid => CaptureError::Cancelled,
        _ => CaptureError::BlobLimit,
    }
}

fn check_pixels(width: u64, height: u64, maximum: u64) -> Result<(), CaptureError> {
    if width == 0
        || height == 0
        || width
            .checked_mul(height)
            .is_none_or(|pixels| pixels > maximum)
    {
        Err(CaptureError::PixelLimit)
    } else {
        Ok(())
    }
}

fn check_scaled_pixels(
    width: f64,
    height: f64,
    scale: f64,
    maximum: u64,
) -> Result<(), CaptureError> {
    if !width.is_finite()
        || !height.is_finite()
        || !scale.is_finite()
        || width <= 0.0
        || height <= 0.0
        || scale <= 0.0
    {
        return Err(CaptureError::PixelLimit);
    }
    let width = (width * scale).ceil();
    let height = (height * scale).ceil();
    if width >= u64::MAX as f64 || height >= u64::MAX as f64 {
        return Err(CaptureError::PixelLimit);
    }
    check_pixels(width as u64, height as u64, maximum)
}

fn encode_image(
    image: &NSImage,
    encoder: NativeBlobEncoder,
    operation: OpaqueToken,
    maximum_pixels: u64,
) -> Result<OpaqueToken, CaptureError> {
    // Snapshot images are borrowed only for the native completion's lifetime.
    // The returned CGImage is retained while ImageIO synchronously encodes it.
    let image =
        unsafe { image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }
            .ok_or(CaptureError::NativeFailure)?;
    check_pixels(
        CGImage::width(Some(&image)) as u64,
        CGImage::height(Some(&image)) as u64,
        maximum_pixels,
    )?;
    let state: Encoding = Arc::new(Mutex::new(Some(encoder)));
    let mut owned_info = Box::new(Arc::clone(&state));
    let callbacks = CGDataConsumerCallbacks {
        putBytes: Some(put_bytes),
        releaseConsumer: Some(release_info),
    };
    let consumer =
        unsafe { CGDataConsumer::new((&mut *owned_info as *mut Encoding).cast(), &callbacks) }
            .ok_or(CaptureError::EncodingFailure)?;
    // A successful consumer owns this pointer until its release callback.
    let _ = Box::into_raw(owned_info);
    let format = CFString::from_str("public.png");
    let destination =
        unsafe { CGImageDestination::with_data_consumer(&consumer, &format, 1, None) }
            .ok_or(CaptureError::EncodingFailure)?;
    unsafe {
        destination.add_image(&image, None);
    }
    let success = unsafe { destination.finalize() };
    drop(destination);
    drop(consumer);
    let encoder = state
        .lock()
        .map_err(|_| CaptureError::EncodingFailure)?
        .take()
        .ok_or(CaptureError::Cancelled)?;
    if let Some(error) = encoder.failure() {
        return Err(map_hub(error));
    }
    if !success {
        return Err(CaptureError::EncodingFailure);
    }
    encoder.finish_for_operation(operation).map_err(map_hub)
}

// Service dispatch must call this on the AppKit thread. Revoking the hub
// operation prevents publication; WKWebView supplies no snapshot cancel handle.
pub(crate) fn capture(
    view: &wry::WebView,
    hub: Arc<OperationHub>,
    store: u64,
    operation: OpaqueToken,
    maximum_pixels: u64,
    maximum_bytes: usize,
    completed: impl FnOnce(Result<OpaqueToken, CaptureError>) + 'static,
) -> Result<NativeCancellation, CaptureError> {
    let native = view.webview();
    let bounds = native.bounds();
    let window = native.window().ok_or(CaptureError::NativeFailure)?;
    check_scaled_pixels(
        bounds.size.width,
        bounds.size.height,
        window.backingScaleFactor(),
        maximum_pixels,
    )?;
    let encoder =
        NativeBlobEncoder::for_operation(hub, store, operation, maximum_bytes).map_err(map_hub)?;
    let completion = RefCell::new(Some((encoder, completed)));
    let callback = block2::RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
        let Some((encoder, completed)) = completion.borrow_mut().take() else {
            return;
        };
        let result = if !error.is_null() || image.is_null() {
            Err(CaptureError::NativeFailure)
        } else {
            // WebKit retains this image throughout the completion invocation.
            encode_image(unsafe { &*image }, encoder, operation, maximum_pixels)
        };
        completed(result);
    });
    // A nil configuration captures the visible bounds at native scale.
    unsafe {
        native.takeSnapshotWithConfiguration_completionHandler(None, &callback);
    }
    Ok(NativeCancellation::new(|| {}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn consumer_callback_bounds_writes_and_releases_owned_state() {
        let hub = OperationHub::with_blob_limits(
            4,
            3,
            crate::operations::BlobLimits::new(4, 8, 8).unwrap(),
        )
        .unwrap();
        let state: Encoding = Arc::new(Mutex::new(Some(
            NativeBlobEncoder::new(Arc::clone(&hub), 7, 8).unwrap(),
        )));
        let info = Box::into_raw(Box::new(Arc::clone(&state))).cast::<c_void>();
        let bytes = b"bounded";
        assert_eq!(
            unsafe {
                put_bytes(
                    info,
                    NonNull::new(bytes.as_ptr().cast_mut().cast()).unwrap(),
                    bytes.len(),
                )
            },
            bytes.len()
        );
        assert_eq!(hub.snapshot().buffered_blob_bytes, 7);
        assert_eq!(
            unsafe {
                put_bytes(
                    info,
                    NonNull::new(bytes.as_ptr().cast_mut().cast()).unwrap(),
                    bytes.len(),
                )
            },
            0
        );
        assert_eq!(
            state.lock().unwrap().as_ref().unwrap().failure(),
            Some(HubError::Quota)
        );
        unsafe {
            release_info(info);
        }
        assert_eq!(Arc::strong_count(&state), 1);
        drop(state.lock().unwrap().take());
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
        assert_eq!(hub.snapshot().live_blobs, 0);
    }

    #[test]
    fn scale_and_pixel_admission_reject_nonfinite_and_overflow() {
        assert_eq!(check_scaled_pixels(10.0, 20.0, 2.0, 800), Ok(()));
        assert_eq!(
            check_scaled_pixels(10.0, 20.0, 2.0, 799),
            Err(CaptureError::PixelLimit)
        );
        for invalid in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
            assert_eq!(
                check_scaled_pixels(10.0, 20.0, invalid, u64::MAX),
                Err(CaptureError::PixelLimit)
            );
        }
        assert_eq!(
            check_scaled_pixels(f64::MAX, 20.0, 2.0, u64::MAX),
            Err(CaptureError::PixelLimit)
        );
    }
}
