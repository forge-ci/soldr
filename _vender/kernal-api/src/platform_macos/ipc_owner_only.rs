//! Owner-only local IPC instances for the macOS pathname-socket host.
//!
//! One listener, one connection, one pathname: endpoint spelling, stale
//! retirement, parent-directory policy, pooling, and retry stay with callers.
//! The Windows named-pipe instance types exist here only so every host exports
//! the same names; they cannot be constructed on this transport.

use std::convert::Infallible;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::platform::ipc::SocketPeerCredentials;

/// Remove a stale socket pathname, refusing anything that is not a socket.
/// A missing path succeeds.
pub fn retire_socket_endpoint(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(path),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPC endpoint is not a socket",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// A pathname socket listener whose socket file is mode `0o600`.
///
/// Binding never retires an existing path, and dropping the listener does not
/// unlink its pathname. Binding and then restricting the mode is not atomic,
/// so the caller's parent directory must already exclude other users.
/// Requires an active I/O-enabled async runtime.
pub struct LocalSocketListener {
    inner: tokio::net::UnixListener,
}

impl LocalSocketListener {
    pub fn bind_owner_only(path: &Path) -> io::Result<Self> {
        let inner = tokio::net::UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self { inner })
    }

    /// Accept one connection and report the peer credentials the host
    /// observed. Canceling this future leaves the listener usable.
    pub async fn accept(&self) -> io::Result<(LocalSocketStream, SocketPeerCredentials)> {
        let (inner, _) = self.inner.accept().await?;
        let peer = match inner.peer_cred() {
            Ok(credentials) => SocketPeerCredentials::observed(
                credentials.pid().and_then(|pid| u32::try_from(pid).ok()),
                // SAFETY: geteuid has no preconditions and does not mutate state.
                credentials.uid() == unsafe { libc::geteuid() },
            ),
            Err(_) => SocketPeerCredentials::unavailable(),
        };
        Ok((LocalSocketStream { inner }, peer))
    }
}

/// A connected pathname socket with standard-library-typed polling byte I/O.
pub struct LocalSocketStream {
    inner: tokio::net::UnixStream,
}

impl LocalSocketStream {
    pub async fn connect(path: &Path) -> io::Result<Self> {
        tokio::net::UnixStream::connect(path)
            .await
            .map(|inner| Self { inner })
    }

    pub fn poll_read(&mut self, cx: &mut Context<'_>, bytes: &mut [u8]) -> Poll<io::Result<usize>> {
        let mut buffer = ReadBuf::new(bytes);
        match Pin::new(&mut self.inner).poll_read(cx, &mut buffer) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(buffer.filled().len())),
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, bytes)
    }

    pub fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    pub fn poll_shutdown(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

fn named_pipes_unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "owner-only named-pipe instances are not this host's local IPC transport",
    )
}

/// Uninhabited on this host: named pipes are not the local IPC transport.
pub struct LocalPipeClient {
    never: Infallible,
}

impl LocalPipeClient {
    pub fn open(endpoint: &str) -> io::Result<Self> {
        let _ = endpoint;
        Err(named_pipes_unsupported())
    }

    pub fn poll_read(&mut self, cx: &mut Context<'_>, bytes: &mut [u8]) -> Poll<io::Result<usize>> {
        let _ = (cx, bytes);
        match self.never {}
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        let _ = (cx, bytes);
        match self.never {}
    }

    pub fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let _ = cx;
        match self.never {}
    }

    pub fn poll_shutdown(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let _ = cx;
        match self.never {}
    }
}

/// Uninhabited on this host: named pipes are not the local IPC transport.
pub struct OwnerOnlyPipeInstance {
    never: Infallible,
}

impl OwnerOnlyPipeInstance {
    pub fn create(endpoint: &str, first: bool) -> io::Result<Self> {
        let _ = (endpoint, first);
        Err(named_pipes_unsupported())
    }

    pub async fn connect(&self) -> io::Result<()> {
        match self.never {}
    }

    pub fn poll_read(&mut self, cx: &mut Context<'_>, bytes: &mut [u8]) -> Poll<io::Result<usize>> {
        let _ = (cx, bytes);
        match self.never {}
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        let _ = (cx, bytes);
        match self.never {}
    }

    pub fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let _ = cx;
        match self.never {}
    }

    pub fn poll_shutdown(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let _ = cx;
        match self.never {}
    }
}
