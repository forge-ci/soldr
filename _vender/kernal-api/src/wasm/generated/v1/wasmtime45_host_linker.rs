// Generated private Wasmtime 45 glue for Core Wasm ABI `kernal-api:v1`.
// Host trait and invocation helpers use semantic scalar and resource types.

pub(crate) mod resources {
    #[repr(transparent)]
    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    pub(crate) struct ArchiveEntry(pub(crate) u64);

    impl ArchiveEntry {
        pub(crate) fn decode_i64(raw: i64) -> ::wasmtime::Result<Self> { ::std::result::Result::Ok(Self(raw as u64)) }
        pub(crate) fn encode_i64(self) -> i64 { self.0 as i64 }
    }

    #[repr(transparent)]
    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    pub(crate) struct AuthenticatedArchive(pub(crate) u64);

    impl AuthenticatedArchive {
        pub(crate) fn decode_i64(raw: i64) -> ::wasmtime::Result<Self> { ::std::result::Result::Ok(Self(raw as u64)) }
        pub(crate) fn encode_i64(self) -> i64 { self.0 as i64 }
    }

    #[repr(transparent)]
    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    pub(crate) struct Blob(pub(crate) u64);

    impl Blob {
        pub(crate) fn decode_i64(raw: i64) -> ::wasmtime::Result<Self> { ::std::result::Result::Ok(Self(raw as u64)) }
        pub(crate) fn encode_i64(self) -> i64 { self.0 as i64 }
    }

    #[repr(transparent)]
    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    pub(crate) struct EncryptedArchive(pub(crate) u64);

    impl EncryptedArchive {
        pub(crate) fn decode_i64(raw: i64) -> ::wasmtime::Result<Self> { ::std::result::Result::Ok(Self(raw as u64)) }
        pub(crate) fn encode_i64(self) -> i64 { self.0 as i64 }
    }

 }

fn i32_from_i32(value: i32) -> wasmtime::Result<i32> {
    Ok(value)
}
fn i32_to_i32(value: i32) -> i32 {
    value
}
fn u32_from_i32(value: i32) -> wasmtime::Result<u32> {
    Ok(value as u32)
}
fn u32_to_i32(value: u32) -> i32 {
    value as i32
}
fn u64_from_i64(value: i64) -> wasmtime::Result<u64> {
    Ok(value as u64)
}
fn u64_to_i64(value: u64) -> i64 {
    value as i64
}
/// A stream transfer either made bounded progress or was rejected without trapping the guest.
pub(crate) enum StreamTransfer { Transferred(usize), Rejected }

enum CallerMemory {
    Ordinary(wasmtime::Memory),
    Shared(wasmtime::SharedMemory),
}
impl CallerMemory {
    fn data_size<T>(&self, caller: &wasmtime::Caller<'_, T>) -> usize {
        match self { Self::Ordinary(memory) => memory.data_size(caller), Self::Shared(memory) => memory.data_size() }
    }
    fn read<T>(&self, caller: &wasmtime::Caller<'_, T>, offset: usize, destination: &mut [u8]) -> wasmtime::Result<()> {
        match self {
            Self::Ordinary(memory) => Ok(memory.read(caller, offset, destination)?),
            Self::Shared(memory) => {
                let end = offset.checked_add(destination.len()).ok_or_else(|| wasmtime::Error::msg("stream guest-memory range overflow"))?;
                let cells = memory.data().get(offset..end).ok_or_else(|| wasmtime::Error::msg("stream guest-memory range is out of bounds"))?;
                for (destination, cell) in destination.iter_mut().zip(cells) {
                    // SAFETY: SharedMemory pins its backing allocation; atomic access avoids racing guest threads.
                    *destination = unsafe { ::std::sync::atomic::AtomicU8::from_ptr(cell.get()) }.load(::std::sync::atomic::Ordering::Relaxed);
                }
                Ok(())
            }
        }
    }
    fn write<T>(&self, caller: &mut wasmtime::Caller<'_, T>, offset: usize, source: &[u8]) -> wasmtime::Result<()> {
        match self {
            Self::Ordinary(memory) => Ok(memory.write(caller, offset, source)?),
            Self::Shared(memory) => {
                let end = offset.checked_add(source.len()).ok_or_else(|| wasmtime::Error::msg("stream guest-memory range overflow"))?;
                let cells = memory.data().get(offset..end).ok_or_else(|| wasmtime::Error::msg("stream guest-memory range is out of bounds"))?;
                for (source, cell) in source.iter().zip(cells) {
                    // SAFETY: SharedMemory pins its backing allocation; atomic access avoids racing guest threads.
                    unsafe { ::std::sync::atomic::AtomicU8::from_ptr(cell.get()) }.store(*source, ::std::sync::atomic::Ordering::Relaxed);
                }
                Ok(())
            }
        }
    }
}
fn bounded_stream_len(value: i32) -> wasmtime::Result<usize> {
    let value = usize::try_from(value).map_err(|_| wasmtime::Error::msg("negative stream chunk length"))?;
    if value <= 65536usize { Ok(value) } else { Err(wasmtime::Error::msg(format!("stream chunk exceeds 65536 bytes: {value}"))) }
}
fn guest_memory_offset(value: i32) -> usize { value as u32 as usize }
fn caller_memory<T>(caller: &mut wasmtime::Caller<'_, T>) -> wasmtime::Result<CallerMemory> {
    match caller.get_export("memory") {
        Some(wasmtime::Extern::Memory(memory)) => Ok(CallerMemory::Ordinary(memory)),
        Some(wasmtime::Extern::SharedMemory(memory)) => Ok(CallerMemory::Shared(memory)),
        _ => Err(wasmtime::Error::msg("stream controls require exported guest memory named `memory`")),
    }
}
fn checked_guest_memory_range<T>(memory: &CallerMemory, caller: &wasmtime::Caller<'_, T>, offset: usize, length: usize) -> wasmtime::Result<()> {
    let end = offset.checked_add(length).ok_or_else(|| wasmtime::Error::msg("stream guest-memory range overflow"))?;
    if end <= memory.data_size(caller) { Ok(()) } else { Err(wasmtime::Error::msg("stream guest-memory range is out of bounds")) }
}
fn checked_stream_count(transferred: usize, requested: usize) -> wasmtime::Result<()> {
    if transferred <= requested && transferred <= 65536usize { Ok(()) } else { Err(wasmtime::Error::msg(format!("host returned invalid stream count {transferred} for requested {requested}"))) }
}


pub(crate) trait KernalApiV1Imports {
    fn kernel_yield(&mut self) -> wasmtime::Result<()>;
    fn operation_cancel(&mut self, operation: u64) -> wasmtime::Result<i32>;
    fn operation_poll(&mut self, operation: u64) -> wasmtime::Result<u64>;
    fn operation_submit(&mut self, kind: u32, arg0: u64, arg1: u64) -> wasmtime::Result<u64>;
    fn operation_yield(&mut self, operation: u64) -> wasmtime::Result<std::sync::Arc<crate::async_engine::Notify>>;
    /// Atomically revoke this guest-owned handle through the host's canonical scope/generation registry.
    fn resource_release_archive_entry(&mut self, resource: resources::ArchiveEntry) -> wasmtime::Result<i32>;
    /// Atomically revoke this guest-owned handle through the host's canonical scope/generation registry.
    fn resource_release_authenticated_archive(&mut self, resource: resources::AuthenticatedArchive) -> wasmtime::Result<i32>;
    /// Atomically revoke this guest-owned handle through the host's canonical scope/generation registry.
    fn resource_release_encrypted_archive(&mut self, resource: resources::EncryptedArchive) -> wasmtime::Result<i32>;
    /// Transfer one bounded caller-memory chunk. Return `StreamTransfer::Rejected` for a guest-visible rejection.
    fn stream_read(&mut self, stream: u64, destination: &mut [u8]) -> wasmtime::Result<StreamTransfer>;
    /// Transfer one bounded caller-memory chunk. Return `StreamTransfer::Rejected` for a guest-visible rejection.
    fn stream_write(&mut self, stream: u64, source: &[u8]) -> wasmtime::Result<StreamTransfer>;
    /// Atomically revoke a stream handle through the host's canonical scope/generation registry.
    fn stream_close(&mut self, stream: u64) -> wasmtime::Result<i32>;
}

#[expect(
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
pub(crate) fn link_kernal_api_v1<T>(linker: &mut wasmtime::Linker<T>) -> wasmtime::Result<()>
where
    T: KernalApiV1Imports + Send + 'static,
{
    linker.func_wrap(
        "kernal-api:v1",
        "kernel_yield",
        |mut caller: wasmtime::Caller<'_, T>| -> wasmtime::Result<()> {
            caller.data_mut().kernel_yield()?;
            Ok(())
        },
    )?;
    linker.func_wrap(
        "kernal-api:v1",
        "operation_cancel",
        |mut caller: wasmtime::Caller<'_, T>,
         operation: i64|
         -> wasmtime::Result<i32> {
            let operation = u64_from_i64(operation)?;
            Ok(i32_to_i32(caller.data_mut().operation_cancel(operation)?))
        },
    )?;
    linker.func_wrap(
        "kernal-api:v1",
        "operation_poll",
        |mut caller: wasmtime::Caller<'_, T>,
         operation: i64|
         -> wasmtime::Result<i64> {
            let operation = u64_from_i64(operation)?;
            Ok(u64_to_i64(caller.data_mut().operation_poll(operation)?))
        },
    )?;
    linker.func_wrap(
        "kernal-api:v1",
        "operation_submit",
        |mut caller: wasmtime::Caller<'_, T>,
         kind: i32,
         arg0: i64,
         arg1: i64|
         -> wasmtime::Result<i64> {
            let kind = u32_from_i32(kind)?;
            let arg0 = u64_from_i64(arg0)?;
            let arg1 = u64_from_i64(arg1)?;
            Ok(u64_to_i64(caller.data_mut().operation_submit(kind, arg0, arg1)?))
        },
    )?;
    linker.func_wrap_async(
        "kernal-api:v1",
        "operation_yield",
        |mut caller: wasmtime::Caller<'_, T>, (operation,): (i64,)| {
            let waiter = caller.data_mut().operation_yield(operation as u64);
            Box::new(async move {
                match waiter { Ok(waiter) => { waiter.notified().await; 1_i32 }, Err(_) => -1_i32 }
            })
        },
    )?;
    linker.func_wrap(
        "kernal-api:v1",
        "resource_release_archive_entry",
        |mut caller: wasmtime::Caller<'_, T>, resource: i64| -> wasmtime::Result<i32> {
            caller.data_mut().resource_release_archive_entry(resources::ArchiveEntry::decode_i64(resource)?)
        },
    )?;
    linker.func_wrap(
        "kernal-api:v1",
        "resource_release_authenticated_archive",
        |mut caller: wasmtime::Caller<'_, T>, resource: i64| -> wasmtime::Result<i32> {
            caller.data_mut().resource_release_authenticated_archive(resources::AuthenticatedArchive::decode_i64(resource)?)
        },
    )?;
    linker.func_wrap(
        "kernal-api:v1",
        "resource_release_encrypted_archive",
        |mut caller: wasmtime::Caller<'_, T>, resource: i64| -> wasmtime::Result<i32> {
            caller.data_mut().resource_release_encrypted_archive(resources::EncryptedArchive::decode_i64(resource)?)
        },
    )?;
    linker.func_wrap(
        "kernal-api:v1",
        "stream_read",
        |mut caller: wasmtime::Caller<'_, T>, stream: i64, destination: i32, destination_len: i32| -> wasmtime::Result<i32> {
            let destination_len = bounded_stream_len(destination_len)?;
            let destination = guest_memory_offset(destination);
            let memory = caller_memory(&mut caller)?;
            checked_guest_memory_range(&memory, &caller, destination, destination_len)?;
            let mut chunk = ::std::vec![0; destination_len];
            let transferred = match caller.data_mut().stream_read(stream as u64, &mut chunk)? { StreamTransfer::Rejected => return Ok(-1), StreamTransfer::Transferred(transferred) => transferred };
            checked_stream_count(transferred, destination_len)?;
            memory.write(&mut caller, destination, &chunk[..transferred])?;
            Ok(transferred as i32)
        },
    )?;
    linker.func_wrap(
        "kernal-api:v1",
        "stream_write",
        |mut caller: wasmtime::Caller<'_, T>, stream: i64, source: i32, source_len: i32| -> wasmtime::Result<i32> {
            let source_len = bounded_stream_len(source_len)?;
            let source = guest_memory_offset(source);
            let memory = caller_memory(&mut caller)?;
            checked_guest_memory_range(&memory, &caller, source, source_len)?;
            let mut chunk = ::std::vec![0; source_len];
            memory.read(&caller, source, &mut chunk)?;
            let transferred = match caller.data_mut().stream_write(stream as u64, &chunk)? { StreamTransfer::Rejected => return Ok(-1), StreamTransfer::Transferred(transferred) => transferred };
            checked_stream_count(transferred, source_len)?;
            Ok(transferred as i32)
        },
    )?;
    linker.func_wrap(
        "kernal-api:v1",
        "stream_close",
        |mut caller: wasmtime::Caller<'_, T>, stream: i64| -> wasmtime::Result<i32> {
            caller.data_mut().stream_close(stream as u64)
        },
    )?;
    Ok(())
}
