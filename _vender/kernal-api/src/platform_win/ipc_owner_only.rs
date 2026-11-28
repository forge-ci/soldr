//! Owner-only local IPC instances for the Windows named-pipe host.
//!
//! One pipe instance or one client per value: endpoint spelling, pooling,
//! retry, and deadline policy stay with callers. The pathname-socket types
//! exist here only so every host exports the same names; they cannot be
//! constructed on this transport, and retiring a socket pathname is a no-op
//! because named pipes leave no filesystem endpoint behind.

use std::convert::Infallible;
use std::ffi::c_void;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

use crate::platform::ipc::SocketPeerCredentials;

/// Protected DACL granting generic-all to the object owner and SYSTEM only.
const OWNER_ONLY_SDDL: &str = "D:P(A;;GA;;;OW)(A;;GA;;;SY)";

/// Named pipes have no persistent filesystem endpoint to unlink.
pub fn retire_socket_endpoint(path: &Path) -> io::Result<()> {
    let _ = path;
    Ok(())
}

fn reject_nul(endpoint: &str) -> io::Result<()> {
    if endpoint.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pipe endpoint contains NUL",
        ));
    }
    Ok(())
}

/// A connected byte-mode named-pipe client.
///
/// Opening performs one native attempt and preserves its error, including
/// pipe-busy (231); retry and deadline policy stay with the caller. Opening
/// requires an active I/O-enabled async runtime and does not authenticate the
/// server's owner.
pub struct LocalPipeClient {
    inner: NamedPipeClient,
}

impl LocalPipeClient {
    pub fn open(endpoint: &str) -> io::Result<Self> {
        reject_nul(endpoint)?;
        ClientOptions::new()
            .open(endpoint)
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

/// A `LocalAlloc` allocation returned by a security-descriptor API.
struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: every value held here came from an API that transfers
        // `LocalFree` ownership to the caller.
        unsafe { LocalFree(self.0) };
    }
}

fn owner_only_descriptor() -> io::Result<LocalAllocation> {
    let sddl: Vec<u16> = OWNER_ONLY_SDDL
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated and `descriptor` is a live out pointer.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }
    if descriptor.is_null() {
        return Err(io::Error::other(
            "Windows returned a null security descriptor",
        ));
    }
    Ok(LocalAllocation(descriptor))
}

/// One local byte-mode named-pipe server instance with a protected DACL
/// granting access to its owner and SYSTEM; remote clients are rejected.
///
/// No pool, retry, timeout, or background task is created. Creation requires
/// an active I/O-enabled async runtime. Dropping the instance closes its
/// handle, including a pending connection.
pub struct OwnerOnlyPipeInstance {
    inner: NamedPipeServer,
}

impl OwnerOnlyPipeInstance {
    /// Create one instance. `first` requests first-instance protection, so an
    /// existing pipe of the same name makes creation fail. Native errors are
    /// preserved for the caller's retry policy.
    pub fn create(endpoint: &str, first: bool) -> io::Result<Self> {
        reject_nul(endpoint)?;
        let descriptor = owner_only_descriptor()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true);
        // SAFETY: `attributes` and the descriptor it points to stay alive
        // through creation; Windows copies the descriptor, and the returned
        // handle is not inheritable.
        let inner = unsafe {
            options.create_with_security_attributes_raw(
                endpoint,
                std::ptr::from_ref(&attributes).cast_mut().cast(),
            )
        }?;
        Ok(Self { inner })
    }

    /// Wait for one client on this instance. Canceling the future retains the
    /// instance; drop the instance to abandon it.
    pub async fn connect(&self) -> io::Result<()> {
        self.inner.connect().await
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

fn sockets_unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "owner-only pathname sockets are not this host's local IPC transport",
    )
}

/// Uninhabited on this host: pathname sockets are not the local IPC transport.
pub struct LocalSocketListener {
    never: Infallible,
}

impl LocalSocketListener {
    pub fn bind_owner_only(path: &Path) -> io::Result<Self> {
        let _ = path;
        Err(sockets_unsupported())
    }

    pub async fn accept(&self) -> io::Result<(LocalSocketStream, SocketPeerCredentials)> {
        match self.never {}
    }
}

/// Uninhabited on this host: pathname sockets are not the local IPC transport.
pub struct LocalSocketStream {
    never: Infallible,
}

impl LocalSocketStream {
    pub async fn connect(path: &Path) -> io::Result<Self> {
        let _ = path;
        Err(sockets_unsupported())
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

#[cfg(test)]
mod tests {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

    use super::*;

    fn bound_dacl(pipe: &OwnerOnlyPipeInstance) -> String {
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: the pipe handle is live and every output pointer is valid.
        let status = unsafe {
            GetSecurityInfo(
                pipe.inner.as_raw_handle(),
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(status, 0, "GetSecurityInfo failed");
        assert!(!descriptor.is_null());
        let descriptor = LocalAllocation(descriptor);
        let mut text = std::ptr::null_mut();
        let mut length = 0;
        // SAFETY: the descriptor stays live through conversion, and the
        // outputs point to writable storage.
        let converted = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.0,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                &mut length,
            )
        };
        assert_ne!(converted, 0, "DACL conversion failed");
        assert!(!text.is_null());
        let text_owner = LocalAllocation(text.cast());
        // SAFETY: Windows returned `length` UTF-16 units in this allocation.
        let result =
            unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(text, length as usize)) };
        drop(text_owner);
        result.trim_end_matches('\0').to_owned()
    }

    #[tokio::test]
    async fn bound_pipe_has_protected_owner_and_system_dacl() {
        let endpoint = format!(r"\\.\pipe\kernal-api-owner-only-dacl-{}", std::process::id());
        let pipe = OwnerOnlyPipeInstance::create(&endpoint, true).unwrap();
        let sddl = bound_dacl(&pipe);
        assert!(sddl.starts_with("D:P"), "DACL must be protected: {sddl}");
        assert_eq!(sddl.matches('(').count(), 2, "unexpected trustees: {sddl}");
        assert!(sddl.contains(";;;SY)"), "SYSTEM grant missing: {sddl}");
        assert!(!sddl.contains(";;;WD)"), "Everyone grant: {sddl}");
        assert!(!sddl.contains(";;;AN)"), "Anonymous grant: {sddl}");
        assert!(!sddl.contains(";;;BA)"), "Administrators grant: {sddl}");
    }
}
