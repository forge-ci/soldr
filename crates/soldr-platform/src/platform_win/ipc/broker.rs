#![allow(missing_docs)]

use std::io;
use std::os::windows::io::{AsHandle as _, AsRawHandle as _, FromRawHandle as _};
use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

pub fn bind_listener(endpoint: &str, _backlog: i32) -> io::Result<interprocess::local_socket::tokio::Listener> {
    use interprocess::local_socket::ListenerOptions;
    use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;

    let name = running_process::broker::server::singleton_bind::wrap_socket_name(endpoint)
        .map_err(io::Error::other)?;
    let sddl = widestring::U16CString::from_str("D:P(A;;GA;;;OW)").map_err(io::Error::other)?;
    let descriptor = SecurityDescriptor::deserialize(&sddl).map_err(io::Error::other)?;
    ListenerOptions::new()
        .name(name)
        .security_descriptor(descriptor)
        .create_tokio()
}

pub fn duplicate_stream(
    stream: &interprocess::local_socket::tokio::Stream,
) -> io::Result<interprocess::local_socket::Stream> {
    let interprocess::local_socket::tokio::Stream::NamedPipe(stream) = stream;
    let process = unsafe { GetCurrentProcess() };
    let mut duplicated: HANDLE = std::ptr::null_mut();
    let result = unsafe {
        DuplicateHandle(
            process,
            stream.as_handle().as_raw_handle() as HANDLE,
            process,
            &mut duplicated,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(duplicated.cast()) };
    interprocess::os::windows::named_pipe::local_socket::Stream::try_from(owned)
        .map(Into::into)
        .map_err(|error| error.to_io_error())
}

pub fn retire_endpoint(_endpoint: &str) {}

/// Named pipes have no filesystem entry a successor could rebind, so the
/// endpoint identity is constant and ownership never changes underneath us.
pub fn endpoint_identity(_endpoint: &str) -> Option<(u64, u64, i128)> {
    Some((0, 0, 0))
}

/// Named-pipe listeners have no filesystem entry to retire.
pub fn retire_endpoint_if_identity(_endpoint: &str, _identity: Option<(u64, u64, i128)>) {}

pub fn seed_stale_endpoint(_endpoint: &std::path::Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Windows named pipes do not leave stale filesystem endpoints"))
}
