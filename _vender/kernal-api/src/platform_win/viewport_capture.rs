//! Private WebView2 viewport capture. COM objects never cross the facade.

use std::ffi::c_void;
use std::io::{Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, E_NOTIMPL, E_POINTER, RECT, S_OK};
use windows::Win32::System::Com::*;
use windows_core::{implement, HRESULT};
use wry::WebViewExtWindows as _;

use crate::operations::{HubError, NativeBlobEncoder, OpaqueToken, OperationHub};

use crate::tauri::capture::{CaptureError, NativeCancellation};

struct Encoding {
    encoder: Option<NativeBlobEncoder>,
    // Fixed-size format metadata, never a second encoded-image buffer.
    header: [u8; 24],
}

#[implement(IStream)]
struct PngStream(Arc<Mutex<Encoding>>);

#[allow(non_snake_case)]
impl ISequentialStream_Impl for PngStream_Impl {
    fn Read(&self, _: *mut c_void, _: u32, read: *mut u32) -> HRESULT {
        if !read.is_null() {
            unsafe {
                *read = 0;
            }
        }
        E_NOTIMPL
    }

    fn Write(&self, bytes: *const c_void, count: u32, written: *mut u32) -> HRESULT {
        if !written.is_null() {
            unsafe {
                *written = 0;
            }
        }
        if count > 0 && bytes.is_null() {
            return E_POINTER;
        }
        let Ok(mut state) = self.0.lock() else {
            return E_FAIL;
        };
        let Some(encoder) = state.encoder.as_mut() else {
            return E_FAIL;
        };
        let Ok(position) = encoder.stream_position() else {
            return E_FAIL;
        };
        // COM's caller guarantees a readable buffer for count bytes. Do not
        // construct a slice from a null pointer for an empty write.
        let bytes = if count == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(bytes.cast::<u8>(), count as usize) }
        };
        if encoder.write_all(bytes).is_err() {
            return E_FAIL;
        }
        if position < state.header.len() as u64 {
            let start = position as usize;
            let copy = bytes.len().min(state.header.len() - start);
            state.header[start..start + copy].copy_from_slice(&bytes[..copy]);
        }
        if !written.is_null() {
            unsafe {
                *written = count;
            }
        }
        S_OK
    }
}

#[allow(non_snake_case)]
impl IStream_Impl for PngStream_Impl {
    fn Seek(
        &self,
        offset: i64,
        origin: STREAM_SEEK,
        position: *mut u64,
    ) -> windows_core::Result<()> {
        let from = match origin {
            STREAM_SEEK_SET if offset >= 0 => SeekFrom::Start(offset as u64),
            STREAM_SEEK_CUR => SeekFrom::Current(offset),
            STREAM_SEEK_END => SeekFrom::End(offset),
            _ => return Err(E_INVALIDARG.into()),
        };
        let mut state = self
            .0
            .lock()
            .map_err(|_| windows_core::Error::from(E_FAIL))?;
        let value = state
            .encoder
            .as_mut()
            .ok_or_else(|| windows_core::Error::from(E_FAIL))?
            .seek(from)
            .map_err(|_| windows_core::Error::from(E_INVALIDARG))?;
        if !position.is_null() {
            unsafe {
                *position = value;
            }
        }
        Ok(())
    }
    fn SetSize(&self, _: u64) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn CopyTo(
        &self,
        _: windows_core::Ref<IStream>,
        _: u64,
        _: *mut u64,
        _: *mut u64,
    ) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Commit(&self, _: &STGC) -> windows_core::Result<()> {
        Ok(())
    }
    fn Revert(&self) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Stat(&self, output: *mut STATSTG, _: &STATFLAG) -> windows_core::Result<()> {
        if output.is_null() {
            return Err(E_POINTER.into());
        }
        let mut state = self
            .0
            .lock()
            .map_err(|_| windows_core::Error::from(E_FAIL))?;
        let encoder = state
            .encoder
            .as_mut()
            .ok_or_else(|| windows_core::Error::from(E_FAIL))?;
        let position = encoder
            .stream_position()
            .map_err(|_| windows_core::Error::from(E_FAIL))?;
        let size = encoder
            .seek(SeekFrom::End(0))
            .map_err(|_| windows_core::Error::from(E_FAIL))?;
        encoder
            .seek(SeekFrom::Start(position))
            .map_err(|_| windows_core::Error::from(E_FAIL))?;
        // The COM caller supplies writable STATSTG storage. No allocated name
        // is returned, so there is no cross-allocator ownership transfer.
        unsafe {
            *output = STATSTG {
                r#type: STGTY_STREAM.0 as u32,
                cbSize: size,
                grfMode: STGM_WRITE,
                ..Default::default()
            };
        }
        Ok(())
    }
    fn Clone(&self) -> windows_core::Result<IStream> {
        Err(E_NOTIMPL.into())
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

fn check_png(header: &[u8; 24], maximum: u64) -> Result<(), CaptureError> {
    if &header[..8] != b"\x89PNG\r\n\x1a\n" || &header[8..16] != b"\0\0\0\rIHDR" {
        return Err(CaptureError::InvalidPng);
    }
    check_pixels(
        u32::from_be_bytes(header[16..20].try_into().unwrap()) as u64,
        u32::from_be_bytes(header[20..24].try_into().unwrap()) as u64,
        maximum,
    )
}

fn map_hub(error: HubError) -> CaptureError {
    match error {
        HubError::Closed | HubError::Invalid => CaptureError::Cancelled,
        _ => CaptureError::BlobLimit,
    }
}

// Service cancellation revokes the hub operation;
// WebView2 has no CapturePreview cancellation method. Late writes are rejected
// by the revoked reservation and the eventual callback releases COM storage.
pub(crate) fn capture(
    view: &wry::WebView,
    hub: Arc<OperationHub>,
    store: u64,
    operation: OpaqueToken,
    maximum_pixels: u64,
    maximum_bytes: usize,
    completed: impl FnOnce(Result<OpaqueToken, CaptureError>) + 'static,
) -> Result<NativeCancellation, CaptureError> {
    let mut bounds = RECT::default();
    unsafe { view.controller().Bounds(&mut bounds) }.map_err(|_| CaptureError::NativeFailure)?;
    let width = i64::from(bounds.right) - i64::from(bounds.left);
    let height = i64::from(bounds.bottom) - i64::from(bounds.top);
    if width <= 0 || height <= 0 {
        return Err(CaptureError::PixelLimit);
    }
    check_pixels(width as u64, height as u64, maximum_pixels)?;
    let encoder =
        NativeBlobEncoder::for_operation(hub, store, operation, maximum_bytes).map_err(map_hub)?;
    let state = Arc::new(Mutex::new(Encoding {
        encoder: Some(encoder),
        header: [0; 24],
    }));
    let stream: IStream = PngStream(Arc::clone(&state)).into();
    let callback = webview2_com::CapturePreviewCompletedHandler::create(Box::new(move |status| {
        let result = (|| {
            let mut state = state.lock().map_err(|_| CaptureError::NativeFailure)?;
            let encoder = state.encoder.take().ok_or(CaptureError::Cancelled)?;
            if let Some(error) = encoder.failure() {
                return Err(map_hub(error));
            }
            status.map_err(|_| CaptureError::NativeFailure)?;
            check_png(&state.header, maximum_pixels)?;
            encoder.finish_for_operation(operation).map_err(map_hub)
        })();
        completed(result);
        Ok(())
    }));
    unsafe { view.webview().CapturePreview(
        webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
        &stream, &callback,
    ) }.map_err(|_| CaptureError::NativeFailure)?;
    Ok(NativeCancellation::new(|| {}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn com_stream_rewrites_and_rejects_quota_and_revocation() {
        let hub = OperationHub::with_blob_limits(
            4,
            3,
            crate::operations::BlobLimits::new(4, 8, 8).unwrap(),
        )
        .unwrap();
        let encoder = NativeBlobEncoder::new(Arc::clone(&hub), 7, 8).unwrap();
        let state = Arc::new(Mutex::new(Encoding {
            encoder: Some(encoder),
            header: [0; 24],
        }));
        let stream: IStream = PngStream(Arc::clone(&state)).into();
        let mut written = 99;
        unsafe {
            stream
                .Write(b"abcd".as_ptr().cast(), 4, Some(&mut written))
                .ok()
                .unwrap();
            assert_eq!(written, 4);
            stream.Seek(0, STREAM_SEEK_SET, None).unwrap();
            stream.Write(b"AB".as_ptr().cast(), 2, None).ok().unwrap();
        }
        let blob = state
            .lock()
            .unwrap()
            .encoder
            .take()
            .unwrap()
            .finish()
            .unwrap();
        assert_eq!(hub.blob_read(7, blob, 4).unwrap(), b"ABcd");
        hub.close_resource(blob).unwrap();
        // COM may retain the stream after completion, but it has no encoder.
        assert!(unsafe { stream.Write(b"x".as_ptr().cast(), 1, Some(&mut written)) }.is_err());
        assert_eq!(written, 0);
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);

        let encoder = NativeBlobEncoder::new(Arc::clone(&hub), 7, 8).unwrap();
        let state = Arc::new(Mutex::new(Encoding {
            encoder: Some(encoder),
            header: [0; 24],
        }));
        let stream: IStream = PngStream(Arc::clone(&state)).into();
        assert!(
            unsafe { stream.Write(b"too-large".as_ptr().cast(), 9, Some(&mut written)) }.is_err()
        );
        assert_eq!(written, 0);
        assert_eq!(hub.snapshot().buffered_blob_bytes, 0);
        drop(state.lock().unwrap().encoder.take());

        let (view, open) = hub.begin_external_webview_open(7).unwrap();
        hub.finish_external_open(open, view);
        hub.observe_terminal(7, open).unwrap().unwrap();
        let operation = hub.begin_external_webview_capture(7, view).unwrap();
        let encoder = NativeBlobEncoder::for_operation(Arc::clone(&hub), 7, operation, 8).unwrap();
        let state = Arc::new(Mutex::new(Encoding {
            encoder: Some(encoder),
            header: [0; 24],
        }));
        let stream: IStream = PngStream(Arc::clone(&state)).into();
        unsafe { stream.Write(b"part".as_ptr().cast(), 4, None) }
            .ok()
            .unwrap();
        hub.close_resource(view).unwrap();
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
        assert!(unsafe { stream.Write(b"late".as_ptr().cast(), 4, Some(&mut written)) }.is_err());
        assert_eq!(written, 0);
        assert!(
            state.lock().unwrap().encoder.is_some(),
            "native owner remains alive during revocation"
        );
        hub.close_all(crate::operations::Terminal::Closed);
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
    }

    #[test]
    fn png_header_and_pixel_budget_are_checked() {
        let mut header = [0; 24];
        assert_eq!(check_png(&header, 64), Err(CaptureError::InvalidPng));
        header[..16].copy_from_slice(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
        header[16..20].copy_from_slice(&8_u32.to_be_bytes());
        header[20..24].copy_from_slice(&8_u32.to_be_bytes());
        assert_eq!(check_png(&header, 64), Ok(()));
        assert_eq!(check_png(&header, 63), Err(CaptureError::PixelLimit));
        assert_eq!(
            check_pixels(u64::MAX, 2, u64::MAX),
            Err(CaptureError::PixelLimit)
        );
    }
}
