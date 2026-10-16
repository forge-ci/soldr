#![allow(missing_docs)]

use std::io;
use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _};

pub fn bind_listener(
    endpoint: &str,
    _backlog: i32,
) -> io::Result<interprocess::local_socket::tokio::Listener> {
    running_process::broker::server::singleton_bind::bind_singleton_with(endpoint, || {
        create_listener(endpoint)
    })
    .map_err(map_bind_singleton_error)
}

fn create_listener(endpoint: &str) -> io::Result<interprocess::local_socket::tokio::Listener> {
    use interprocess::local_socket::ListenerOptions;
    use std::os::unix::fs::PermissionsExt as _;

    let name = running_process::broker::server::singleton_bind::wrap_socket_name(endpoint)
        .map_err(io::Error::other)?;
    let listener = ListenerOptions::new()
        .name(name)
        .create_tokio_as::<interprocess::os::unix::uds_local_socket::tokio::Listener>()?;
    std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener.into())
}

fn map_bind_singleton_error(
    error: running_process::broker::server::singleton_bind::BindSingletonError,
) -> io::Error {
    use running_process::broker::server::singleton_bind::BindSingletonError;

    match error {
        BindSingletonError::InvalidName(message) => {
            io::Error::new(io::ErrorKind::InvalidInput, message)
        }
        BindSingletonError::AlreadyBound(error) | BindSingletonError::Other(error) => error,
    }
}

pub fn duplicate_stream(
    stream: &interprocess::local_socket::tokio::Stream,
) -> io::Result<interprocess::local_socket::Stream> {
    let interprocess::local_socket::tokio::Stream::UdSocket(stream) = stream;
    let duplicated = unsafe { libc::fcntl(stream.as_fd().as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicated < 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(duplicated) };
    Ok(interprocess::os::unix::uds_local_socket::Stream::from(owned).into())
}

pub fn retire_endpoint(endpoint: &str) {
    let _ = std::fs::remove_file(endpoint);
}

/// Device, inode and change time of the socket file at `endpoint`, or `None`
/// when absent. soldr#3815: compared before unlinking so a broker only retires
/// its own file. The change time is part of it because a rebind can reuse the
/// freed inode number.
pub fn endpoint_identity(endpoint: &str) -> Option<(u64, u64, i128)> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(endpoint).ok().map(|meta| {
        let ctime = i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec());
        (meta.dev(), meta.ino(), ctime)
    })
}

/// Unlink `endpoint` only while it is still the file identified by `identity`.
/// A successor broker that rebound the path keeps its socket.
pub fn retire_endpoint_if_identity(endpoint: &str, identity: Option<(u64, u64, i128)>) {
    if identity.is_some() && endpoint_identity(endpoint) == identity {
        let _ = std::fs::remove_file(endpoint);
    }
}

pub fn seed_stale_endpoint(endpoint: &std::path::Path) -> io::Result<()> {
    drop(std::os::unix::net::UnixListener::bind(endpoint)?);
    Ok(())
}
