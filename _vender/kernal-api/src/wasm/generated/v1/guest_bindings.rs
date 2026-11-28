// Generated Core Wasm guest bindings for `kernal-api:v1`.
// Public APIs use semantic scalar and resource types; raw ABI values stay private.

use std::sync::OnceLock;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AbiError {
    InvalidBoolean(i32),
    OutOfRange { ty: &'static str, value: i32 },
    ResourceClosed { resource: &'static str },
    ResourceReleaseRejected { resource: &'static str, status: i32 },
            StreamChunkTooLarge { requested: usize, maximum: usize },
            StreamRejected,
            StreamInvalidCount { transferred: i32, requested: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExportInstallError {
    AlreadyInstalled,
}

pub mod resources {
    #[derive(Debug, Eq, Hash, PartialEq)]
    pub struct ArchiveEntry(::core::option::Option<u64>);

    impl ArchiveEntry {
        pub(crate) fn from_abi(raw: u64) -> Self { Self(::core::option::Option::Some(raw)) }
        pub(crate) fn decode_i64(raw: i64) -> ::core::result::Result<Self, super::AbiError> { ::core::result::Result::Ok(Self::from_abi(raw as u64)) }
        pub(crate) fn encode_i64(&self) -> ::core::result::Result<i64, super::AbiError> { self.0.map(|raw| raw as i64).ok_or(super::AbiError::ResourceClosed { resource: "ArchiveEntry" }) }
        pub fn close(mut self) -> ::core::result::Result<(), super::AbiError> { self.release() }
        fn release(&mut self) -> ::core::result::Result<(), super::AbiError> { let raw = self.0.take().ok_or(super::AbiError::ResourceClosed { resource: "ArchiveEntry" })?; let status = unsafe { super::raw_imports::__kernal_api_v1_import_resource_release_archive_entry(raw as i64) }; if status == 0 { ::core::result::Result::Ok(()) } else { ::core::result::Result::Err(super::AbiError::ResourceReleaseRejected { resource: "ArchiveEntry", status }) } }
    }

    impl ::core::ops::Drop for ArchiveEntry { fn drop(&mut self) { if self.0.is_some() { let _ = self.release(); } } }

    #[derive(Debug, Eq, Hash, PartialEq)]
    pub struct AuthenticatedArchive(::core::option::Option<u64>);

    impl AuthenticatedArchive {
        pub(crate) fn from_abi(raw: u64) -> Self { Self(::core::option::Option::Some(raw)) }
        pub(crate) fn decode_i64(raw: i64) -> ::core::result::Result<Self, super::AbiError> { ::core::result::Result::Ok(Self::from_abi(raw as u64)) }
        pub(crate) fn encode_i64(&self) -> ::core::result::Result<i64, super::AbiError> { self.0.map(|raw| raw as i64).ok_or(super::AbiError::ResourceClosed { resource: "AuthenticatedArchive" }) }
        pub fn close(mut self) -> ::core::result::Result<(), super::AbiError> { self.release() }
        fn release(&mut self) -> ::core::result::Result<(), super::AbiError> { let raw = self.0.take().ok_or(super::AbiError::ResourceClosed { resource: "AuthenticatedArchive" })?; let status = unsafe { super::raw_imports::__kernal_api_v1_import_resource_release_authenticated_archive(raw as i64) }; if status == 0 { ::core::result::Result::Ok(()) } else { ::core::result::Result::Err(super::AbiError::ResourceReleaseRejected { resource: "AuthenticatedArchive", status }) } }
    }

    impl ::core::ops::Drop for AuthenticatedArchive { fn drop(&mut self) { if self.0.is_some() { let _ = self.release(); } } }

    #[derive(Debug, Eq, Hash, PartialEq)]
    pub struct Blob(::core::option::Option<u64>);

    impl Blob {
        pub(crate) fn from_abi(raw: u64) -> Self { Self(::core::option::Option::Some(raw)) }
        pub(crate) fn decode_i64(raw: i64) -> ::core::result::Result<Self, super::AbiError> { ::core::result::Result::Ok(Self::from_abi(raw as u64)) }
        pub(crate) fn encode_i64(&self) -> ::core::result::Result<i64, super::AbiError> { self.0.map(|raw| raw as i64).ok_or(super::AbiError::ResourceClosed { resource: "Blob" }) }
        pub fn read(&mut self, destination: &mut [u8]) -> ::core::result::Result<usize, super::AbiError> { let stream = self.encode_i64()?; super::check_stream_chunk(destination.len())?; let transferred = unsafe { super::raw_imports::__kernal_api_v1_import_stream_read(stream, destination.as_mut_ptr() as usize as i32, destination.len() as i32) }; super::stream_count_from_i32(transferred, destination.len()) }
        pub fn write(&mut self, source: &[u8]) -> ::core::result::Result<usize, super::AbiError> { let stream = self.encode_i64()?; super::check_stream_chunk(source.len())?; let transferred = unsafe { super::raw_imports::__kernal_api_v1_import_stream_write(stream, source.as_ptr() as usize as i32, source.len() as i32) }; super::stream_count_from_i32(transferred, source.len()) }
        pub fn close(mut self) -> ::core::result::Result<(), super::AbiError> { self.release() }
        fn release(&mut self) -> ::core::result::Result<(), super::AbiError> { let raw = self.0.take().ok_or(super::AbiError::ResourceClosed { resource: "Blob" })?; let status = unsafe { super::raw_imports::__kernal_api_v1_import_stream_close(raw as i64) }; if status == 0 { ::core::result::Result::Ok(()) } else { ::core::result::Result::Err(super::AbiError::ResourceReleaseRejected { resource: "Blob", status }) } }
    }

    impl ::core::ops::Drop for Blob { fn drop(&mut self) { if self.0.is_some() { let _ = self.release(); } } }

    #[derive(Debug, Eq, Hash, PartialEq)]
    pub struct EncryptedArchive(::core::option::Option<u64>);

    impl EncryptedArchive {
        pub(crate) fn from_abi(raw: u64) -> Self { Self(::core::option::Option::Some(raw)) }
        pub(crate) fn decode_i64(raw: i64) -> ::core::result::Result<Self, super::AbiError> { ::core::result::Result::Ok(Self::from_abi(raw as u64)) }
        pub(crate) fn encode_i64(&self) -> ::core::result::Result<i64, super::AbiError> { self.0.map(|raw| raw as i64).ok_or(super::AbiError::ResourceClosed { resource: "EncryptedArchive" }) }
        pub fn close(mut self) -> ::core::result::Result<(), super::AbiError> { self.release() }
        fn release(&mut self) -> ::core::result::Result<(), super::AbiError> { let raw = self.0.take().ok_or(super::AbiError::ResourceClosed { resource: "EncryptedArchive" })?; let status = unsafe { super::raw_imports::__kernal_api_v1_import_resource_release_encrypted_archive(raw as i64) }; if status == 0 { ::core::result::Result::Ok(()) } else { ::core::result::Result::Err(super::AbiError::ResourceReleaseRejected { resource: "EncryptedArchive", status }) } }
    }

    impl ::core::ops::Drop for EncryptedArchive { fn drop(&mut self) { if self.0.is_some() { let _ = self.release(); } } }

 }

fn bool_from_i32(value: i32) -> Result<bool, AbiError> { match value { 0 => Ok(false), 1 => Ok(true), value => Err(AbiError::InvalidBoolean(value)) } }
fn i8_from_i32(value: i32) -> Result<i8, AbiError> { value.try_into().map_err(|_| AbiError::OutOfRange { ty: "i8", value }) }
fn i16_from_i32(value: i32) -> Result<i16, AbiError> { value.try_into().map_err(|_| AbiError::OutOfRange { ty: "i16", value }) }
fn u8_from_i32(value: i32) -> Result<u8, AbiError> { value.try_into().map_err(|_| AbiError::OutOfRange { ty: "u8", value }) }
fn u16_from_i32(value: i32) -> Result<u16, AbiError> { value.try_into().map_err(|_| AbiError::OutOfRange { ty: "u16", value }) }

fn i32_from_i32(value: i32) -> Result<i32, AbiError> { Ok(value) }
fn u32_from_i32(value: i32) -> Result<u32, AbiError> { Ok(value as u32) }
fn i64_from_i64(value: i64) -> Result<i64, AbiError> { Ok(value) }
fn u64_from_i64(value: i64) -> Result<u64, AbiError> { Ok(value as u64) }
fn f32_from_f32(value: f32) -> Result<f32, AbiError> { Ok(value) }
fn f64_from_f64(value: f64) -> Result<f64, AbiError> { Ok(value) }
fn bool_to_i32(value: bool) -> i32 { i32::from(value) }
fn i8_to_i32(value: i8) -> i32 { value as i32 }
fn i16_to_i32(value: i16) -> i32 { value as i32 }
fn i32_to_i32(value: i32) -> i32 { value }
fn u8_to_i32(value: u8) -> i32 { value as i32 }
fn u16_to_i32(value: u16) -> i32 { value as i32 }
fn u32_to_i32(value: u32) -> i32 { value as i32 }
fn i64_to_i64(value: i64) -> i64 { value }
fn u64_to_i64(value: u64) -> i64 { value as i64 }
fn f32_to_f32(value: f32) -> f32 { value }
fn f64_to_f64(value: f64) -> f64 { value }

const MAX_STREAM_CHUNK_BYTES: usize = 65536usize;
fn check_stream_chunk(requested: usize) -> Result<(), AbiError> { if requested <= MAX_STREAM_CHUNK_BYTES { Ok(()) } else { Err(AbiError::StreamChunkTooLarge { requested, maximum: MAX_STREAM_CHUNK_BYTES }) } }
fn stream_count_from_i32(transferred: i32, requested: usize) -> Result<usize, AbiError> { if transferred == -1 { return Err(AbiError::StreamRejected); } let transferred = usize::try_from(transferred).map_err(|_| AbiError::StreamInvalidCount { transferred, requested })?; if transferred <= requested { Ok(transferred) } else { Err(AbiError::StreamInvalidCount { transferred: transferred as i32, requested }) } }

mod raw_imports {
    #[link(wasm_import_module = "kernal-api:v1")]
    extern "C" {
        #[link_name = "kernel_yield"]
        pub(super) fn __kernal_api_v1_import_kernel_yield() -> ();
        #[link_name = "operation_cancel"]
        pub(super) fn __kernal_api_v1_import_operation_cancel(operation: i64) -> i32;
        #[link_name = "operation_poll"]
        pub(super) fn __kernal_api_v1_import_operation_poll(operation: i64) -> i64;
        #[link_name = "operation_submit"]
        pub(super) fn __kernal_api_v1_import_operation_submit(kind: i32, arg0: i64, arg1: i64) -> i64;
        #[link_name = "operation_yield"]
        pub(super) fn __kernal_api_v1_import_operation_yield(operation: i64) -> i32;

        #[link_name = "resource_release_archive_entry"]
        pub(super) fn __kernal_api_v1_import_resource_release_archive_entry(resource: i64) -> i32;
        #[link_name = "resource_release_authenticated_archive"]
        pub(super) fn __kernal_api_v1_import_resource_release_authenticated_archive(resource: i64) -> i32;
        #[link_name = "resource_release_encrypted_archive"]
        pub(super) fn __kernal_api_v1_import_resource_release_encrypted_archive(resource: i64) -> i32;

        #[link_name = "stream_read"]
        pub(super) fn __kernal_api_v1_import_stream_read(stream: i64, destination: i32, destination_len: i32) -> i32;
        #[link_name = "stream_write"]
        pub(super) fn __kernal_api_v1_import_stream_write(stream: i64, source: i32, source_len: i32) -> i32;
        #[link_name = "stream_close"]
        pub(super) fn __kernal_api_v1_import_stream_close(stream: i64) -> i32;

    }
}


pub mod imports {
    use super::{raw_imports, AbiError};

    pub fn kernel_yield() -> Result<(), AbiError> {
        unsafe { raw_imports::__kernal_api_v1_import_kernel_yield() };
        Ok(())
    }

    pub fn operation_cancel(operation: u64) -> Result<i32, AbiError> {
        let raw = unsafe { raw_imports::__kernal_api_v1_import_operation_cancel(super::u64_to_i64(operation)) };
        super::i32_from_i32(raw)
    }

    pub fn operation_poll(operation: u64) -> Result<u64, AbiError> {
        let raw = unsafe { raw_imports::__kernal_api_v1_import_operation_poll(super::u64_to_i64(operation)) };
        super::u64_from_i64(raw)
    }

    pub fn operation_submit(kind: u32, arg0: u64, arg1: u64) -> Result<u64, AbiError> {
        let raw = unsafe { raw_imports::__kernal_api_v1_import_operation_submit(super::u32_to_i32(kind), super::u64_to_i64(arg0), super::u64_to_i64(arg1)) };
        super::u64_from_i64(raw)
    }

    pub fn operation_yield(operation: u64) -> Result<i32, AbiError> {
        let raw = unsafe { raw_imports::__kernal_api_v1_import_operation_yield(super::u64_to_i64(operation)) };
        super::i32_from_i32(raw)
    }
}

pub struct KernalApiV1Exports {

}

static EXPORTS: OnceLock<KernalApiV1Exports> = OnceLock::new();

pub fn install_exports(exports: KernalApiV1Exports) -> Result<(), ExportInstallError> { EXPORTS.set(exports).map_err(|_| ExportInstallError::AlreadyInstalled) }

fn installed_exports() -> &'static KernalApiV1Exports { EXPORTS.get().expect("install KernalApiV1Exports before invoking guest exports") }
fn require_abi<T>(value: Result<T, AbiError>) -> T { value.expect("host passed an invalid scalar Core Wasm ABI value") }




#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationError { Rejected, Cancelled, Closed, Failed, TimedOut }
pub struct OperationFuture { operation: u64 }
impl OperationFuture {
    fn submit(kind: u32, arg0: u64, arg1: u64) -> Result<Self, OperationError> {
        let operation = imports::operation_submit(kind, arg0, arg1).map_err(|_| OperationError::Failed)?;
        if operation == 0 { Err(OperationError::Rejected) } else { Ok(Self { operation }) }
    }
    pub fn poll(&self) -> Result<Option<u64>, OperationError> {
        let packed = imports::operation_poll(self.operation).map_err(|_| OperationError::Failed)?;
        match packed as u8 { 0 => Ok(None), 1 => Ok(Some(packed >> 8)), 2 => Err(OperationError::Cancelled), 3 => Err(OperationError::TimedOut), 6 => Err(OperationError::Closed), 7 => Err(OperationError::Rejected), _ => Err(OperationError::Failed) }
    }
    pub fn yield_now(&self) -> Result<(), OperationError> { if imports::operation_yield(self.operation).map_err(|_| OperationError::Failed)? == 1 { Ok(()) } else { Err(OperationError::Failed) } }
    pub fn cancel(&self) { let _ = imports::operation_cancel(self.operation); }
    /// Discard a blob transfer and its result without allocating another slot.
    /// Non-transfer operation tokens are rejected by the host.
    pub fn abandon_transfer(&self) { let _ = imports::operation_submit(18, self.operation, 0); }
    /// The async host import parks this Wasm stack until the operation wakes.
    /// No native thread blocks and no guest-side scheduler is constructed.
    pub async fn wait(self) -> Result<u64, OperationError> {
        loop {
            if let Some(payload) = self.poll()? { return Ok(payload); }
            self.yield_now()?;
        }
    }
}

/// Drive a command composed exclusively of generated kernel futures.
/// Suspension happens inside the generated async host import, not through a
/// second guest executor. A foreign future returning Pending is unsupported.
pub fn run<T>(future: impl std::future::Future<Output = Result<T, OperationError>>) -> Result<T, OperationError> {
    let mut future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut context) {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => Err(OperationError::Failed),
    }
}

/// Kernel-owned incremental hash authority, never a guest hashing backend.
pub struct Blake3Hasher { token: u64 }
struct HashOperation { inner: OperationFuture }
impl HashOperation {
    async fn wait(&self) -> Result<u64, OperationError> {
        loop {
            if let Some(payload) = self.inner.poll()? { return Ok(payload); }
            self.inner.yield_now()?;
        }
    }
}
impl Drop for HashOperation {
    fn drop(&mut self) { let _ = imports::operation_submit(34, self.inner.operation, 0); }
}
impl Blake3Hasher {
    pub async fn new() -> Result<Self, OperationError> {
        let operation = HashOperation { inner: OperationFuture::submit(30, 0, 0)? };
        let token = operation.wait().await?;
        if token == 0 { return Err(OperationError::Failed); }
        Ok(Self { token })
    }
    /// One update is at most 64 KiB. Dropping an uncollected update revokes
    /// this hasher: committed bytes cannot safely be replayed after cancellation.
    pub async fn update(&mut self, bytes: &[u8]) -> Result<(), OperationError> {
        if bytes.len() > 65536 { return Err(OperationError::Rejected); }
        let pointer = u32::try_from(bytes.as_ptr() as usize).map_err(|_| OperationError::Rejected)?;
        let packed = ((bytes.len() as u64) << 32) | u64::from(pointer);
        let operation = HashOperation { inner: OperationFuture::submit(31, self.token, packed)? };
        operation.wait().await?;
        Ok(())
    }
    pub async fn finalize(mut self) -> Result<[u8; 32], OperationError> {
        let mut digest = [0; 32];
        let pointer = u32::try_from(digest.as_mut_ptr() as usize).map_err(|_| OperationError::Rejected)?;
        let status = imports::operation_submit(32, self.token, (32_u64 << 32) | u64::from(pointer)).map_err(|_| OperationError::Failed)?;
        if status != 1 { return Err(OperationError::Failed); }
        self.token = 0;
        Ok(digest)
    }
}
impl Drop for Blake3Hasher {
    fn drop(&mut self) {
        if self.token != 0 { let _ = imports::operation_submit(33, self.token, 0); }
    }
}

/// One pre-authorized native URL, never a guest URL string or network grant.
pub struct WebviewUrl { token: u64 }
/// Opaque native viewport authority owned by this logical sketch.
pub struct Webview { token: u64 }
impl WebviewUrl {
    pub fn granted() -> Result<Option<Self>, OperationError> {
        let token = imports::operation_submit(13, 0, 0).map_err(|_| OperationError::Failed)?;
        Ok(if token == 0 { None } else { Some(Self { token }) })
    }
    pub async fn open(&self) -> Result<Webview, OperationError> {
        let token = OperationFuture::submit(14, self.token, 0)?.wait().await?;
        if token == 0 { return Err(OperationError::Failed); }
        Ok(Webview { token })
    }
}
impl Webview {
    pub async fn wait_until_loaded(&self) -> Result<(), OperationError> {
        OperationFuture::submit(15, self.token, 0)?.wait().await?;
        Ok(())
    }
    pub async fn capture_visible_png(&self) -> Result<BlobHandle, OperationError> {
        let token = OperationFuture::submit(16, self.token, 0)?.wait().await?;
        if token == 0 { return Err(OperationError::Failed); }
        Ok(BlobHandle::from_create_payload(token))
    }
    pub async fn close(self) -> Result<(), OperationError> {
        OperationFuture::submit(17, self.token, 0)?.wait().await?;
        Ok(())
    }
}
#[derive(Clone, Copy)]
pub struct SyntheticResource { token: u64 }
impl SyntheticResource {
    pub fn create(shareable: bool) -> Result<OperationFuture, OperationError> { OperationFuture::submit(2, u64::from(shareable), 1) }
    pub fn from_create_payload(token: u64) -> Self { Self { token } }
    pub fn use_(&self) -> Result<OperationFuture, OperationError> { OperationFuture::submit(3, self.token, 1) }
    pub fn close(&self) -> Result<OperationFuture, OperationError> { OperationFuture::submit(4, self.token, 0) }
}
pub fn synthetic_yield() -> Result<OperationFuture, OperationError> { OperationFuture::submit(1, 0, 0) }

/// Sleep on the embedding kernel's monotonic timer. The guest imports no
/// clock and creates no runtime; yield parks this Wasm execution on the host.
pub fn clock_sleep(milliseconds: u32) -> Result<OperationFuture, OperationError> {
    OperationFuture::submit(12, u64::from(milliseconds), 0)
}

/// Opaque host-owned bulk stream. No buffer or native path is carried here.
/// Generated bounded stream controls delegate ownership and close to
/// OperationHub's canonical Store-scoped registry.
pub struct BlobHandle { resource: resources::Blob }
/// Optional host-owned encrypted input. No source path or key crosses the ABI.
pub struct EncryptedArchive { resource: resources::EncryptedArchive }
impl EncryptedArchive {
    pub fn granted() -> Result<Option<Self>, OperationError> {
        let token = imports::operation_submit(20, 0, 0).map_err(|_| OperationError::Failed)?;
        Ok(if token == 0 { None } else { Some(Self { resource: resources::EncryptedArchive::from_abi(token) }) })
    }
    pub fn read_header(&self, destination: &mut [u8]) -> Result<usize, OperationError> {
        let length = u32::try_from(destination.len()).map_err(|_| OperationError::Rejected)?;
        let pointer = u32::try_from(destination.as_mut_ptr() as usize).map_err(|_| OperationError::Rejected)?;
        let result = imports::operation_submit(21, self.token()?, (u64::from(length) << 32) | u64::from(pointer)).map_err(|_| OperationError::Failed)?;
        let copied = (result >> 8) as usize;
        if result as u8 != 1 || copied > destination.len() || copied > 16 * 1024 + 12 { return Err(OperationError::Rejected); }
        Ok(copied)
    }
    pub fn abandon(self) { let _ = self.resource.close(); }
    pub fn authenticate(&self, nonce: &[u8; 12]) -> Result<ArchiveAuthentication, OperationError> {
        let pointer = u32::try_from(nonce.as_ptr() as usize).map_err(|_| OperationError::Rejected)?;
        Ok(ArchiveAuthentication { inner: OperationFuture::submit(23, self.token()?, u64::from(pointer))? })
    }
    fn token(&self) -> Result<u64, OperationError> { self.resource.encode_i64().map(|raw| raw as u64).map_err(|_| OperationError::Closed) }
}
pub struct ArchiveAuthentication { inner: OperationFuture }
impl ArchiveAuthentication {
    pub async fn wait(self) -> Result<AuthenticatedArchive, OperationError> {
        loop {
            if let Some(token) = self.inner.poll()? {
                if token == 0 { return Err(OperationError::Failed); }
                return Ok(AuthenticatedArchive { resource: resources::AuthenticatedArchive::from_abi(token) });
            }
            self.inner.yield_now()?;
        }
    }
}
impl Drop for ArchiveAuthentication {
    fn drop(&mut self) { let _ = imports::operation_submit(24, self.inner.operation, 0); }
}
pub struct AuthenticatedArchive { resource: resources::AuthenticatedArchive }
impl AuthenticatedArchive {
    pub fn next_entry(&self) -> Result<ArchiveNextEntry, OperationError> {
        Ok(ArchiveNextEntry { inner: OperationFuture::submit(26, self.token()?, 0)? })
    }
    pub fn close(self) -> Result<(), OperationError> { self.resource.close().map_err(|_| OperationError::Rejected) }
    pub fn abandon(self) { let _ = self.resource.close(); }
    fn token(&self) -> Result<u64, OperationError> { self.resource.encode_i64().map(|raw| raw as u64).map_err(|_| OperationError::Closed) }
}
pub struct ArchiveNextEntry { inner: OperationFuture }
impl ArchiveNextEntry {
    pub async fn wait(self) -> Result<Option<ArchiveEntry>, OperationError> {
        loop {
            if let Some(token) = self.inner.poll()? {
                return Ok(if token == 0 { None } else { Some(ArchiveEntry { resource: resources::ArchiveEntry::from_abi(token) }) });
            }
            self.inner.yield_now()?;
        }
    }
}
impl Drop for ArchiveNextEntry {
    fn drop(&mut self) { let _ = imports::operation_submit(24, self.inner.operation, 0); }
}
pub struct ArchiveEntry { resource: resources::ArchiveEntry }
impl ArchiveEntry {
    pub fn open(&self) -> Result<ArchiveEntryOpen, OperationError> {
        Ok(ArchiveEntryOpen { inner: OperationFuture::submit(29, self.token()?, 0)? })
    }
    pub fn metadata(&self, destination: &mut [u8]) -> Result<usize, OperationError> {
        let length = u32::try_from(destination.len()).map_err(|_| OperationError::Rejected)?;
        let pointer = u32::try_from(destination.as_mut_ptr() as usize).map_err(|_| OperationError::Rejected)?;
        let result = imports::operation_submit(27, self.token()?, (u64::from(length) << 32) | u64::from(pointer)).map_err(|_| OperationError::Failed)?;
        let count = (result >> 8) as usize;
        if result as u8 != 1 || count > destination.len() || !(12..=4108).contains(&count) {
            return Err(OperationError::Rejected);
        }
        Ok(count)
    }
}
impl ArchiveEntry { fn token(&self) -> Result<u64, OperationError> { self.resource.encode_i64().map(|raw| raw as u64).map_err(|_| OperationError::Closed) } }
pub struct ArchiveEntryOpen { inner: OperationFuture }
impl ArchiveEntryOpen {
    pub async fn wait(self) -> Result<BlobHandle, OperationError> {
        loop {
            if let Some(token) = self.inner.poll()? {
                if token == 0 { return Err(OperationError::Failed); }
                return Ok(BlobHandle::from_create_payload(token));
            }
            self.inner.yield_now()?;
        }
    }
}
impl Drop for ArchiveEntryOpen {
    fn drop(&mut self) { let _ = imports::operation_submit(24, self.inner.operation, 0); }
}
/// One exact destination authorized by the embedding host; never a guest path.
pub struct OutputFile { token: u64 }
impl OutputFile {
    /// Discover the optional initial grant for the root Store.
    pub fn granted() -> Result<Option<Self>, OperationError> {
        let token = imports::operation_submit(11, 0, 0).map_err(|_| OperationError::Failed)?;
        Ok(if token == 0 { None } else { Some(Self { token }) })
    }
    /// Wrap the scoped token supplied by the host. Forging this value grants
    /// no authority: every commit checks the host's resource registry.
    pub fn from_granted_token(token: u64) -> Self { Self { token } }
    pub fn write_blob(&self, blob: &BlobHandle) -> Result<OperationFuture, OperationError> {
        OperationFuture::submit(10, blob.token()?, self.token)
    }
}
pub struct BlobReadFuture { operation: u64 }
impl BlobReadFuture {
    /// Poll and collect into a caller-owned bounded destination. No pointer
    /// is retained after this call, including when the result is pending.
    pub fn poll_into(&self, destination: &mut [u8]) -> Result<Option<usize>, OperationError> {
        let length = u32::try_from(destination.len()).map_err(|_| OperationError::Rejected)?;
        let pointer = u32::try_from(destination.as_mut_ptr() as usize).map_err(|_| OperationError::Rejected)?;
        let packed = imports::operation_submit(8, self.operation, (u64::from(length) << 32) | u64::from(pointer)).map_err(|_| OperationError::Failed)?;
        match packed as u8 {
            0 => Ok(None),
            1 => Ok(Some((packed >> 8) as usize)),
            2 => Err(OperationError::Cancelled),
            6 => Err(OperationError::Closed),
            _ => Err(OperationError::Failed),
        }
    }
    pub fn yield_now(&self) -> Result<(), OperationError> { OperationFuture { operation: self.operation }.yield_now() }
    pub fn cancel(&self) { OperationFuture { operation: self.operation }.cancel(); }
    pub fn abandon_transfer(&self) { OperationFuture { operation: self.operation }.abandon_transfer(); }
}
impl BlobHandle {
    fn token(&self) -> Result<u64, OperationError> {
        self.resource.encode_i64().map(|raw| raw as u64).map_err(|_| OperationError::Closed)
    }
    /// Revoke this resource synchronously without allocating an operation.
    pub fn abandon(self) { let _ = self.resource.close(); }
    /// Transfer one bounded chunk synchronously through caller memory.
    pub fn read(&mut self, destination: &mut [u8]) -> Result<usize, OperationError> {
        self.resource.read(destination).map_err(|_| OperationError::Rejected)
    }
    /// Transfer one bounded chunk synchronously through caller memory.
    pub fn write(&mut self, source: &[u8]) -> Result<usize, OperationError> {
        self.resource.write(source).map_err(|_| OperationError::Rejected)
    }
    pub fn create() -> Result<OperationFuture, OperationError> { OperationFuture::submit(5, 0, 0) }
    pub fn from_create_payload(token: u64) -> Self { Self { resource: resources::Blob::from_abi(token) } }
    pub fn read_chunk(&self, maximum_bytes: u32) -> Result<BlobReadFuture, OperationError> {
        let operation = OperationFuture::submit(7, self.token()?, u64::from(maximum_bytes))?;
        Ok(BlobReadFuture { operation: operation.operation })
    }
    /// Publish EOF. Call only after preceding writes have completed; pending
    /// writes are rejected when the producer seals its stream.
    pub fn seal(&self) -> Result<OperationFuture, OperationError> { OperationFuture::submit(9, self.token()?, 0) }
    pub fn close(self) -> Result<(), OperationError> { self.resource.close().map_err(|_| OperationError::Closed) }
    /// The host copies this bounded slice before returning the future.
    /// No guest pointer or borrow is retained while waiting for capacity.
    pub fn write_chunk(&self, bytes: &[u8]) -> Result<OperationFuture, OperationError> {
        let length = u32::try_from(bytes.len()).map_err(|_| OperationError::Rejected)?;
        let pointer = u32::try_from(bytes.as_ptr() as usize).map_err(|_| OperationError::Rejected)?;
        OperationFuture::submit(6, self.token()?, (u64::from(length) << 32) | u64::from(pointer))
    }
}

/// Exact command authority selected by the host, never a guest SpawnSpec.
pub struct CompilerGrant { token: u64 }
pub struct CompilerProcess { token: u64 }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompilerOutputEvent {
    Stdout(usize), Stderr(usize), StdoutEof, StderrEof,
    StdoutAbandoned, StderrAbandoned, StdoutError, StderrError, Exhausted,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompilerExit { pub code: Option<i32>, pub success: bool }
struct CompilerOperation { inner: OperationFuture, abandon: u32 }
impl Drop for CompilerOperation {
    fn drop(&mut self) { let _ = imports::operation_submit(self.abandon, self.inner.operation, 0); }
}
impl CompilerOperation {
    fn submit(kind: u32, token: u64, abandon: u32) -> Result<Self, OperationError> {
        Ok(Self { inner: OperationFuture::submit(kind, token, 0)?, abandon })
    }
    async fn wait(&self) -> Result<u64, OperationError> {
        loop {
            if let Some(payload) = self.inner.poll()? { return Ok(payload); }
            self.inner.yield_now()?;
        }
    }
}
fn compiler_terminal(packed: u64) -> Result<Option<u64>, OperationError> {
    match packed as u8 {
        0 => Ok(None), 1 => Ok(Some(packed >> 8)),
        2 => Err(OperationError::Cancelled), 3 => Err(OperationError::TimedOut),
        6 => Err(OperationError::Closed), 7 => Err(OperationError::Rejected),
        _ => Err(OperationError::Failed),
    }
}
impl CompilerGrant {
    pub fn granted() -> Result<Option<Self>, OperationError> {
        let token = imports::operation_submit(35, 0, 0).map_err(|_| OperationError::Failed)?;
        Ok(if token == 0 { None } else { Some(Self { token }) })
    }
    pub async fn spawn(self) -> Result<CompilerProcess, OperationError> {
        let operation = CompilerOperation::submit(36, self.token, 42)?;
        let token = operation.wait().await?;
        if token == 0 { return Err(OperationError::Failed); }
        Ok(CompilerProcess { token })
    }
    /// Ask this exact host grant whether a guest-derived cache identity is
    /// present. The guest receives neither paths nor cache contents.
    pub fn cache_status(&self, key: &[u8; 32]) -> Result<bool, OperationError> {
        let pointer = u32::try_from(key.as_ptr() as usize).map_err(|_| OperationError::Rejected)?;
        match imports::operation_submit(48, self.token, u64::from(pointer)).map_err(|_| OperationError::Failed)? {
            1 => Ok(true),
            2 => Ok(false),
            _ => Err(OperationError::Rejected),
        }
    }
}
impl Drop for CompilerGrant {
    fn drop(&mut self) { let _ = imports::operation_submit(41, self.token, 0); }
}
impl CompilerProcess {
    pub async fn read_output(&mut self, destination: &mut [u8]) -> Result<CompilerOutputEvent, OperationError> {
        if destination.len() < 65536 { return Err(OperationError::Rejected); }
        let pointer = u32::try_from(destination.as_mut_ptr() as usize).map_err(|_| OperationError::Rejected)?;
        let operation = CompilerOperation::submit(37, self.token, 39)?;
        loop {
            let packed = imports::operation_submit(38, operation.inner.operation, (65536_u64 << 32) | u64::from(pointer)).map_err(|_| OperationError::Failed)?;
            if let Some(payload) = compiler_terminal(packed)? {
                let count = (payload >> 8) as usize;
                if count > 65536 { return Err(OperationError::Failed); }
                return Ok(match payload as u8 {
                    1 => CompilerOutputEvent::Stdout(count), 2 => CompilerOutputEvent::Stderr(count),
                    3 if count == 0 => CompilerOutputEvent::StdoutEof,
                    4 if count == 0 => CompilerOutputEvent::StderrEof,
                    5 if count == 0 => CompilerOutputEvent::StdoutAbandoned,
                    6 if count == 0 => CompilerOutputEvent::StderrAbandoned,
                    7 if count == 0 => CompilerOutputEvent::StdoutError,
                    8 if count == 0 => CompilerOutputEvent::StderrError,
                    9 if count == 0 => CompilerOutputEvent::Exhausted,
                    _ => return Err(OperationError::Failed),
                });
            }
            operation.inner.yield_now()?;
        }
    }
    pub async fn wait(&self) -> Result<CompilerExit, OperationError> {
        let operation = CompilerOperation::submit(43, self.token, 47)?;
        loop {
            let packed = imports::operation_submit(44, operation.inner.operation, 0).map_err(|_| OperationError::Failed)?;
            if let Some(payload) = compiler_terminal(packed)? {
                return Ok(CompilerExit { code: if payload & (1_u64 << 32) != 0 { Some(payload as u32 as i32) } else { None }, success: payload & (1_u64 << 33) != 0 });
            }
            operation.inner.yield_now()?;
        }
    }
    pub async fn close(mut self) -> Result<(), OperationError> {
        let operation = CompilerOperation::submit(45, self.token, 46)?;
        self.token = 0;
        operation.wait().await?;
        Ok(())
    }
}
impl Drop for CompilerProcess {
    fn drop(&mut self) { if self.token != 0 { let _ = imports::operation_submit(40, self.token, 0); } }
}
