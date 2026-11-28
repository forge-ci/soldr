//! Windows local IPC transport mechanics.

use std::io::{self, Read, Write};
#[cfg(feature = "ipc-async")]
use std::pin::Pin;
#[cfg(feature = "ipc-async")]
use std::task::{Context, Poll};

use interprocess::local_socket::prelude::*;
#[cfg(feature = "ipc-async")]
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions, PeerCreds, ToNsName};
use interprocess::os::windows::named_pipe::{pipe_mode, DuplexPipeStream};
use interprocess::{ConnectWaitMode, TryClone};
#[cfg(feature = "ipc-async")]
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint(String);

impl Endpoint {
    pub fn new(path: impl Into<String>) -> io::Result<Self> {
        let path = path.into();
        name(&path)?;
        Ok(Self(path))
    }

    pub fn display(&self) -> &str {
        &self.0
    }

    pub fn retire(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn ensure_owner_private_parent(&self) -> io::Result<()> {
        Ok(())
    }

    /// Whether this endpoint's target is present on the host.
    ///
    /// A named pipe has no filesystem entry to inspect. An instance exists
    /// only while the process that created it holds it open, and the kernel
    /// takes the name back when that process exits, so existence is answered
    /// by the same probe as staleness and `true` here also means a server is
    /// live. Nothing survives that server for [`Endpoint::retire`] to remove.
    pub fn target_exists(&self) -> io::Result<bool> {
        self.probe_server_instance()
    }

    pub fn ensure_parent_exists(&self) -> io::Result<()> {
        Ok(())
    }

    /// Whether the endpoint has no server behind it and may be taken over.
    ///
    /// Only a probe that answered "no server instance" reports stale. A probe
    /// that failed for any other reason leaves the question open, and an open
    /// question is not a licence to take a live endpoint over.
    pub fn is_stale(&self) -> bool {
        matches!(self.probe_server_instance(), Ok(false))
    }

    /// Classify one connect attempt against this pipe name.
    ///
    /// `CreateFileW` reports `ERROR_FILE_NOT_FOUND` when no server instance
    /// exists, which the transport surfaces as [`io::ErrorKind::NotFound`] --
    /// the same classification the filesystem-backed hosts key off.
    ///
    /// The attempt carries a zero connect timeout so that exactly one
    /// `CreateFileW` is issued, and it asks the named-pipe API for that
    /// directly: the local-socket layer this module otherwise uses hard-codes
    /// an unbounded wait and drops any wait mode handed to it. An unbounded
    /// wait would spin until a busy instance frees up, which the caller has no
    /// way to bring about -- only the server's next accept clears an instance
    /// -- so a liveness question against a server that is alive but wedged
    /// would never come back. With the attempt bounded, `ERROR_PIPE_BUSY`
    /// arrives as [`io::ErrorKind::TimedOut`] instead, and a busy instance is
    /// itself proof that a server holds the name.
    ///
    /// A server observes a successful probe as a connection that never sends
    /// a byte and clears it on its next accept, so a caller that only wants
    /// liveness should probe once rather than ask both predicates.
    fn probe_server_instance(&self) -> io::Result<bool> {
        let attempt = DuplexPipeStream::<pipe_mode::Bytes>::connect_by_path_with_wait_mode(
            pipe_path(&self.0),
            ConnectWaitMode::Timeout(std::time::Duration::ZERO),
        );
        match attempt {
            Ok(_stream) => Ok(true),
            Err(error) => match error.kind() {
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => Ok(false),
                io::ErrorKind::TimedOut => Ok(true),
                _ => Err(error),
            },
        }
    }

    /// Allocate a unique endpoint for a caller-owned test or probe.
    pub fn test(label: &str) -> io::Result<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self::new(format!(
            r"\\.\pipe\rp-ipc-{label}-{}-{nonce}",
            std::process::id()
        ))
    }
}

fn name(path: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    path.strip_prefix(r"\\.\pipe\")
        .unwrap_or(path)
        .to_ns_name::<GenericNamespaced>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

/// Spell this address the way the transport spells it to the kernel.
///
/// [`name`] hands a namespaced name to the local-socket layer, which prepends
/// the local pipe prefix on the way to `CreateFileW`. A probe that calls the
/// named-pipe API directly has to prepend it here instead, and has to strip
/// the same prefix first so that both spellings name one pipe.
fn pipe_path(path: &str) -> String {
    format!(
        r"\\.\pipe\{}",
        path.strip_prefix(r"\\.\pipe\").unwrap_or(path)
    )
}

pub fn select_endpoint_address(
    kernel_namespace: Option<String>,
    _filesystem: Option<std::path::PathBuf>,
) -> Option<String> {
    kernel_namespace
}

pub const fn nonblocking_zero_read_is_pending() -> bool {
    true
}

pub const fn endpoint_is_filesystem_backed() -> bool {
    false
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerIdentity {
    pub pid: u32,
    pub user_id: String,
}

pub trait PeerIdentitySource {
    fn ipc_peer_identity(&self) -> io::Result<PeerIdentity>;
}

fn peer_identity(creds: PeerCreds) -> PeerIdentity {
    let pid = creds.pid().unwrap_or(0);
    PeerIdentity {
        pid,
        user_id: if pid == 0 {
            String::new()
        } else {
            process_user_sid(pid).unwrap_or_default()
        },
    }
}

fn process_user_sid(pid: u32) -> io::Result<String> {
    let bytes = process_user_sid_bytes(pid)?;
    let mut out = String::with_capacity("windows-sid:".len() + bytes.len() * 2);
    out.push_str("windows-sid:");
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

fn process_user_sid_bytes(pid: u32) -> io::Result<Vec<u8>> {
    use windows_sys::Win32::Security::{
        GetLengthSid, GetTokenInformation, IsValidSid, TokenUser, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let process = OwnedHandle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid));
        if process.0.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(process.0, TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle(token);
        let mut required = 0;
        let _ = GetTokenInformation(
            token.0,
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut required,
        );
        if required == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0_u8; required as usize];
        let queried = GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        );
        if queried == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid = (*(buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid;
        if sid.is_null() || IsValidSid(sid) == 0 {
            return Err(io::Error::other("invalid Windows SID"));
        }
        let len = GetLengthSid(sid) as usize;
        if len == 0 || len > 1024 {
            return Err(io::Error::other("implausible Windows SID length"));
        }
        Ok(std::slice::from_raw_parts(sid.cast::<u8>(), len).to_vec())
    }
}

fn owner_only_security_descriptor(
) -> io::Result<interprocess::os::windows::security_descriptor::SecurityDescriptor> {
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;

    let sid = process_user_sid_bytes(std::process::id())?;
    let mut sid_string = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid.as_ptr().cast_mut().cast(), &mut sid_string) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let sid_text = unsafe {
        let mut length = 0;
        while *sid_string.add(length) != 0 {
            length += 1;
        }
        let text = String::from_utf16(std::slice::from_raw_parts(sid_string, length))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        LocalFree(sid_string.cast());
        text?
    };
    let sddl = widestring::U16CString::from_str(format!("D:P(A;;GA;;;{sid_text})"))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    SecurityDescriptor::deserialize(&sddl)
}

struct OwnedHandle(windows_sys::Win32::Foundation::HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }
}

pub fn current_user_id() -> io::Result<String> {
    process_user_sid(std::process::id())
}

pub struct Stream(pub(crate) interprocess::local_socket::Stream);

impl std::fmt::Debug for Stream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("IpcStream")
    }
}

impl Stream {
    #[deprecated(note = "can wait forever on a stuck peer; use `connect_within` (#433)")]
    pub fn connect(endpoint: &Endpoint) -> io::Result<Self> {
        interprocess::local_socket::Stream::connect(name(endpoint.display())?).map(Self)
    }

    /// Connect, waiting at most `timeout` for a free pipe instance; returns
    /// [`io::ErrorKind::TimedOut`] on expiry.
    ///
    /// The local-socket layer ignores connect wait modes on Windows, so this
    /// dials the named pipe with the bound and wraps that connected stream.
    ///
    /// The stream is wrapped as is, never round-tripped through an
    /// `OwnedHandle`: interprocess's `TryFrom<OwnedHandle>` for a pipe stream
    /// calls `ReOpenFile`, which on a client pipe handle opens a *second*
    /// connection to the next free server instance and closes the dialed one
    /// (kotauskas/interprocess#100). The server then accepts a connection that
    /// is already closed while this client talks to an instance nobody
    /// accepted. Whether a free instance exists at that moment depends on the
    /// server's `accept`, so the swap happened only some of the time.
    pub fn connect_within(endpoint: &Endpoint, timeout: std::time::Duration) -> io::Result<Self> {
        let pipe = DuplexPipeStream::<pipe_mode::Bytes>::connect_by_path_with_wait_mode(
            pipe_path(endpoint.display()),
            ConnectWaitMode::Timeout(timeout),
        )?;
        let stream = interprocess::os::windows::named_pipe::local_socket::Stream::from(pipe);
        Ok(Self(stream.into()))
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        interprocess::local_socket::traits::Stream::set_nonblocking(&self.0, nonblocking)
    }

    /// Bound how long a receive may block.
    ///
    /// A peer that accepts and then stalls would otherwise hold the calling
    /// thread forever. The send side is not bounded here: the selected
    /// transport exposes only a receive timeout.
    pub fn set_recv_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        interprocess::local_socket::traits::Stream::set_recv_timeout(&self.0, timeout)
    }

    /// Bound how long a send may block (`None` waits indefinitely).
    ///
    /// Named pipes support no I/O timeouts, so this returns
    /// [`io::ErrorKind::Unsupported`] on this host, as
    /// [`Self::set_recv_timeout`] does.
    pub fn set_send_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        interprocess::local_socket::traits::Stream::set_send_timeout(&self.0, timeout)
    }

    pub fn peer_identity(&self) -> io::Result<PeerIdentity> {
        self.0.peer_creds().map(peer_identity)
    }

    /// Duplicate this accepted named-pipe connection into a backend process.
    pub fn transfer_to_backend(
        &self,
        _backend_control: &Self,
        _backend_endpoint: &Endpoint,
        backend_pid: u32,
        _sideband_payload: &[u8],
    ) -> Result<crate::platform::ipc::HandoffAttachment, crate::platform::ipc::HandoffTransferError>
    {
        use crate::platform::ipc::{HandoffAttachment, HandoffTransferError};
        use std::os::windows::io::{AsHandle as _, AsRawHandle as _};
        use windows_sys::Win32::Foundation::HANDLE;

        let source = match &self.0 {
            interprocess::local_socket::Stream::NamedPipe(stream) => {
                stream.as_handle().as_raw_handle() as HANDLE
            }
        };
        let duplicated = legacy_duplicate_handle(source as usize, backend_pid).map_err(|error| {
            HandoffTransferError::new(
                error.kind(),
                false,
                error
                    .detail()
                    .unwrap_or("connection handle duplication failed"),
            )
        })?;
        Ok(HandoffAttachment::new(duplicated as u64, false))
    }
}

pub fn legacy_duplicate_handle(
    source_handle: usize,
    backend_pid: u32,
) -> Result<usize, crate::LegacyHandoffError> {
    use crate::platform::ipc::HandoffTransferErrorKind;
    use crate::LegacyHandoffError;
    use windows_sys::Win32::Foundation::{
        CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, ERROR_ACCESS_DENIED, HANDLE,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE,
    };

    // SAFETY: OpenProcess receives a numeric PID and requests only the
    // duplication right; the returned handle is closed below.
    let backend = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, backend_pid) };
    if backend.is_null() {
        let error = io::Error::last_os_error();
        let kind = if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
            HandoffTransferErrorKind::PermissionDenied
        } else {
            HandoffTransferErrorKind::BackendUnavailable
        };
        return Err(LegacyHandoffError::with_detail(
            kind,
            error.raw_os_error(),
            format!(
                "cannot open backend process {backend_pid} for connection transfer: {error}"
            ),
        ));
    }

    let mut duplicated: HANDLE = std::ptr::null_mut();
    // SAFETY: source and backend are live handles, `duplicated` is a valid
    // writable out-parameter, and the API duplicates with the same access.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            source_handle as HANDLE,
            backend,
            &mut duplicated,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    let error = io::Error::last_os_error();
    // SAFETY: `backend` was successfully opened above and is owned here.
    unsafe { CloseHandle(backend) };
    if ok == 0 || duplicated.is_null() || duplicated == INVALID_HANDLE_VALUE {
        let kind = if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
            HandoffTransferErrorKind::PermissionDenied
        } else {
            HandoffTransferErrorKind::Failed
        };
        return Err(LegacyHandoffError::with_detail(
            kind,
            error.raw_os_error(),
            format!(
                "failed to duplicate connection into backend process {backend_pid}: {error}"
            ),
        ));
    }

    Ok(duplicated as usize)
}

impl PeerIdentitySource for Stream {
    fn ipc_peer_identity(&self) -> io::Result<PeerIdentity> {
        self.peer_identity()
    }
}

impl PeerIdentitySource for interprocess::local_socket::Stream {
    fn ipc_peer_identity(&self) -> io::Result<PeerIdentity> {
        self.peer_creds().map(peer_identity)
    }
}

impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Write for Stream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ListenerNonblockingMode {
    #[default]
    Neither,
    Accept,
    Stream,
    Both,
}

impl From<ListenerNonblockingMode> for interprocess::local_socket::ListenerNonblockingMode {
    fn from(value: ListenerNonblockingMode) -> Self {
        match value {
            ListenerNonblockingMode::Neither => Self::Neither,
            ListenerNonblockingMode::Accept => Self::Accept,
            ListenerNonblockingMode::Stream => Self::Stream,
            ListenerNonblockingMode::Both => Self::Both,
        }
    }
}

pub struct Listener(interprocess::local_socket::Listener);

impl std::fmt::Debug for Listener {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("IpcListener")
    }
}

impl Listener {
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        Self::bind_with_options(endpoint, true, ListenerNonblockingMode::Neither)
    }

    pub fn bind_owner_only(endpoint: &Endpoint) -> io::Result<Self> {
        use interprocess::os::windows::local_socket::ListenerOptionsExt as _;

        ListenerOptions::new()
            .name(name(endpoint.display())?)
            .security_descriptor(owner_only_security_descriptor()?)
            .create_sync()
            .map(Self)
    }

    pub fn bind_with_options(
        endpoint: &Endpoint,
        reclaim_name: bool,
        nonblocking: ListenerNonblockingMode,
    ) -> io::Result<Self> {
        ListenerOptions::new()
            .name(name(endpoint.display())?)
            .reclaim_name(reclaim_name)
            .nonblocking(nonblocking.into())
            .create_sync()
            .map(Self)
    }

    pub fn accept(&self) -> io::Result<Stream> {
        self.0.accept().map(Stream)
    }

    pub fn set_nonblocking(&self, mode: ListenerNonblockingMode) -> io::Result<()> {
        self.0.set_nonblocking(mode.into())
    }

    pub fn do_not_reclaim_name_on_drop(&mut self) {
        self.0.do_not_reclaim_name_on_drop();
    }
}

/// Windows named-pipe listeners cannot be handed off as a bound listener:
/// one accepted pipe instance becomes the connection. The opaque type keeps
/// that capability gap at the platform boundary.
pub struct InheritedListener;

impl InheritedListener {
    pub fn supported() -> bool {
        false
    }

    pub fn bind(_endpoint: &Endpoint) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a Windows named-pipe listener cannot be inherited by a child",
        ))
    }

    pub fn prepare(&self, _command: &mut std::process::Command, _env_key: &str) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a Windows named-pipe listener cannot be inherited by a child",
        ))
    }

    pub fn disown_endpoint(&mut self) {}

    pub fn recover_from_env(_env_key: &str) -> io::Result<Option<Listener>> {
        Ok(None)
    }
}

/// Run one async IPC operation against a timeout, reporting expiry as
/// [`io::ErrorKind::TimedOut`].
#[cfg(feature = "ipc-async")]
async fn within<T>(
    timeout: std::time::Duration,
    operation: impl std::future::Future<Output = io::Result<T>>,
) -> io::Result<T> {
    crate::async_engine::timeout(timeout, operation)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "IPC operation did not finish within its timeout"))?
}

#[cfg(feature = "ipc-async")]
pub struct AsyncStream(pub(crate) interprocess::local_socket::tokio::Stream);

#[cfg(feature = "ipc-async")]
impl AsyncStream {
    #[deprecated(note = "can wait forever on a stuck peer; use `connect_within` (#433)")]
    pub async fn connect(endpoint: &Endpoint) -> io::Result<Self> {
        interprocess::local_socket::tokio::Stream::connect(name(endpoint.display())?)
            .await
            .map(Self)
    }

    /// [`Self::connect`], failing with [`io::ErrorKind::TimedOut`] if the
    /// connection is not established within `timeout`.
    pub async fn connect_within(endpoint: &Endpoint, timeout: std::time::Duration) -> io::Result<Self> {
        // Bounds the deprecated wait; inline it when that is removed (#433).
        #[allow(deprecated)]
        let wait = Self::connect(endpoint);
        within(timeout, wait).await
    }

    /// [`Self::read`], failing with [`io::ErrorKind::TimedOut`] if no bytes
    /// arrive within `timeout`.
    pub async fn read_within(&mut self, buffer: &mut [u8], timeout: std::time::Duration) -> io::Result<usize> {
        // Bounds the deprecated wait; inline it when that is removed (#433).
        #[allow(deprecated)]
        let wait = self.read(buffer);
        within(timeout, wait).await
    }

    /// [`Self::read_exact`] within `timeout`. On expiry the bytes already read
    /// into `buffer` are consumed from the stream and the error is
    /// [`io::ErrorKind::TimedOut`].
    pub async fn read_exact_within(&mut self, buffer: &mut [u8], timeout: std::time::Duration) -> io::Result<usize> {
        // Bounds the deprecated wait; inline it when that is removed (#433).
        #[allow(deprecated)]
        let wait = self.read_exact(buffer);
        within(timeout, wait).await
    }

    /// [`Self::write_all`] within `timeout`. On expiry part of `bytes` may
    /// already have been written and the error is [`io::ErrorKind::TimedOut`].
    pub async fn write_all_within(&mut self, bytes: &[u8], timeout: std::time::Duration) -> io::Result<()> {
        // Bounds the deprecated wait; inline it when that is removed (#433).
        #[allow(deprecated)]
        let wait = self.write_all(bytes);
        within(timeout, wait).await
    }

    /// [`Self::flush`] within `timeout`; expiry is [`io::ErrorKind::TimedOut`].
    pub async fn flush_within(&mut self, timeout: std::time::Duration) -> io::Result<()> {
        // Bounds the deprecated wait; inline it when that is removed (#433).
        #[allow(deprecated)]
        let wait = self.flush();
        within(timeout, wait).await
    }

    /// [`Self::shutdown`] within `timeout`; expiry is [`io::ErrorKind::TimedOut`].
    pub async fn shutdown_within(&mut self, timeout: std::time::Duration) -> io::Result<()> {
        // Bounds the deprecated wait; inline it when that is removed (#433).
        #[allow(deprecated)]
        let wait = self.shutdown();
        within(timeout, wait).await
    }

    pub fn peer_identity(&self) -> io::Result<PeerIdentity> {
        self.0.peer_creds().map(peer_identity)
    }

    /// Read into `buffer`, returning the number of bytes read.
    ///
    /// This is an inherent method rather than a re-exported trait method, so
    /// a caller never needs an extension-trait import to reach it.
    #[deprecated(note = "can wait forever on a stuck peer; use `read_within` (#433)")]
    pub async fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        use tokio::io::AsyncReadExt as _;

        self.0.read(buffer).await
    }

    /// Read until `buffer` is completely filled.
    ///
    /// On success the return value equals `buffer.len()`; an end-of-stream
    /// before `buffer` fills yields [`io::ErrorKind::UnexpectedEof`].
    #[deprecated(note = "can wait forever on a stuck peer; use `read_exact_within` (#433)")]
    pub async fn read_exact(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        use tokio::io::AsyncReadExt as _;

        self.0.read_exact(buffer).await
    }

    /// Write the entirety of `bytes`, retrying partial writes internally.
    #[deprecated(note = "can wait forever on a stuck peer; use `write_all_within` (#433)")]
    pub async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        use tokio::io::AsyncWriteExt as _;

        self.0.write_all(bytes).await
    }

    /// Flush any buffered writes to the underlying transport.
    #[deprecated(note = "can wait forever on a stuck peer; use `flush_within` (#433)")]
    pub async fn flush(&mut self) -> io::Result<()> {
        use tokio::io::AsyncWriteExt as _;

        self.0.flush().await
    }

    /// Shut down the write half of this stream, signaling end-of-stream to
    /// the peer's reader.
    #[deprecated(note = "can wait forever on a stuck peer; use `shutdown_within` (#433)")]
    pub async fn shutdown(&mut self) -> io::Result<()> {
        use tokio::io::AsyncWriteExt as _;

        self.0.shutdown().await
    }

    /// Split this stream into an owned, facade-owned read half and write
    /// half that may be driven concurrently or moved to separate tasks.
    ///
    /// Both halves keep their backing connection behind a private field, so
    /// a caller that stores them never names the backend transport.
    pub fn into_split(self) -> (AsyncReadHalf, AsyncWriteHalf) {
        let (reader, writer) = tokio::io::split(self.0);
        (AsyncReadHalf(reader), AsyncWriteHalf(writer))
    }
}

/// The facade-owned read half produced by [`AsyncStream::into_split`].
#[cfg(feature = "ipc-async")]
pub struct AsyncReadHalf(tokio::io::ReadHalf<interprocess::local_socket::tokio::Stream>);

#[cfg(feature = "ipc-async")]
impl AsyncReadHalf {
    /// [`Self::read`] within `timeout`; expiry is [`io::ErrorKind::TimedOut`].
    pub async fn read_within(&mut self, buffer: &mut [u8], timeout: std::time::Duration) -> io::Result<usize> {
        // Bounds the deprecated wait; inline it when that is removed (#433).
        #[allow(deprecated)]
        let wait = self.read(buffer);
        within(timeout, wait).await
    }

    /// [`Self::read_exact`] within `timeout`; expiry is [`io::ErrorKind::TimedOut`].
    /// Bytes already read into `buffer` before expiry are consumed.
    pub async fn read_exact_within(&mut self, buffer: &mut [u8], timeout: std::time::Duration) -> io::Result<usize> {
        // Bounds the deprecated wait; inline it when that is removed (#433).
        #[allow(deprecated)]
        let wait = self.read_exact(buffer);
        within(timeout, wait).await
    }

    /// Read into `buffer`, returning the number of bytes read.
    #[deprecated(note = "can wait forever on a stuck peer; use `read_within` (#433)")]
    pub async fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        use tokio::io::AsyncReadExt as _;

        self.0.read(buffer).await
    }

    /// Read until `buffer` is completely filled.
    #[deprecated(note = "can wait forever on a stuck peer; use `read_exact_within` (#433)")]
    pub async fn read_exact(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        use tokio::io::AsyncReadExt as _;

        self.0.read_exact(buffer).await
    }
}

/// The facade-owned write half produced by [`AsyncStream::into_split`].
#[cfg(feature = "ipc-async")]
pub struct AsyncWriteHalf(tokio::io::WriteHalf<interprocess::local_socket::tokio::Stream>);

#[cfg(feature = "ipc-async")]
impl AsyncWriteHalf {
    /// [`Self::write_all`] within `timeout`; expiry is [`io::ErrorKind::TimedOut`].
    /// Part of `bytes` may already be written before expiry.
    pub async fn write_all_within(&mut self, bytes: &[u8], timeout: std::time::Duration) -> io::Result<()> {
        // Bounds the deprecated wait; inline it when that is removed (#433).
        #[allow(deprecated)]
        let wait = self.write_all(bytes);
        within(timeout, wait).await
    }

    /// [`Self::flush`] within `timeout`; expiry is [`io::ErrorKind::TimedOut`].
    pub async fn flush_within(&mut self, timeout: std::time::Duration) -> io::Result<()> {
        // Bounds the deprecated wait; inline it when that is removed (#433).
        #[allow(deprecated)]
        let wait = self.flush();
        within(timeout, wait).await
    }

    /// [`Self::shutdown`] within `timeout`; expiry is [`io::ErrorKind::TimedOut`].
    pub async fn shutdown_within(&mut self, timeout: std::time::Duration) -> io::Result<()> {
        // Bounds the deprecated wait; inline it when that is removed (#433).
        #[allow(deprecated)]
        let wait = self.shutdown();
        within(timeout, wait).await
    }

    /// Write the entirety of `bytes`, retrying partial writes internally.
    #[deprecated(note = "can wait forever on a stuck peer; use `write_all_within` (#433)")]
    pub async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        use tokio::io::AsyncWriteExt as _;

        self.0.write_all(bytes).await
    }

    /// Flush any buffered writes to the underlying transport.
    #[deprecated(note = "can wait forever on a stuck peer; use `flush_within` (#433)")]
    pub async fn flush(&mut self) -> io::Result<()> {
        use tokio::io::AsyncWriteExt as _;

        self.0.flush().await
    }

    /// Shut down this write half, signaling end-of-stream to the peer's
    /// reader.
    #[deprecated(note = "can wait forever on a stuck peer; use `shutdown_within` (#433)")]
    pub async fn shutdown(&mut self) -> io::Result<()> {
        use tokio::io::AsyncWriteExt as _;

        self.0.shutdown().await
    }
}

#[cfg(feature = "ipc-async")]
impl PeerIdentitySource for AsyncStream {
    fn ipc_peer_identity(&self) -> io::Result<PeerIdentity> {
        self.peer_identity()
    }
}

#[cfg(feature = "ipc-async")]
impl PeerIdentitySource for interprocess::local_socket::tokio::Stream {
    fn ipc_peer_identity(&self) -> io::Result<PeerIdentity> {
        self.peer_creds().map(peer_identity)
    }
}

#[cfg(feature = "ipc-async")]
impl AsyncRead for AsyncStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(context, buffer)
    }
}

#[cfg(feature = "ipc-async")]
impl AsyncWrite for AsyncStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(context)
    }
}

#[cfg(feature = "ipc-async")]
pub trait IntoAsyncStream {
    fn into_async_stream(self) -> AsyncStream;
}

#[cfg(feature = "ipc-async")]
impl IntoAsyncStream for AsyncStream {
    fn into_async_stream(self) -> AsyncStream {
        self
    }
}

#[cfg(feature = "ipc-async")]
impl IntoAsyncStream for interprocess::local_socket::tokio::Stream {
    fn into_async_stream(self) -> AsyncStream {
        AsyncStream(self)
    }
}

#[cfg(feature = "ipc-async")]
pub struct AsyncListener(interprocess::local_socket::tokio::Listener);

#[cfg(feature = "ipc-async")]
impl AsyncListener {
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        ListenerOptions::new()
            .name(name(endpoint.display())?)
            .create_tokio()
            .map(Self)
    }

    pub fn bind_owner_only(endpoint: &Endpoint) -> io::Result<Self> {
        use interprocess::os::windows::local_socket::ListenerOptionsExt as _;

        ListenerOptions::new()
            .name(name(endpoint.display())?)
            .security_descriptor(owner_only_security_descriptor()?)
            .create_tokio()
            .map(Self)
    }

    pub async fn accept(&self) -> io::Result<AsyncStream> {
        self.0.accept().await.map(AsyncStream)
    }

    pub fn do_not_reclaim_name_on_drop(&mut self) {
        self.0.do_not_reclaim_name_on_drop();
    }
}

#[cfg(feature = "ipc-async")]
pub trait IntoAsyncListener {
    fn into_async_listener(self) -> AsyncListener;
}

#[cfg(feature = "ipc-async")]
impl IntoAsyncListener for AsyncListener {
    fn into_async_listener(self) -> AsyncListener {
        self
    }
}

#[cfg(feature = "ipc-async")]
impl IntoAsyncListener for interprocess::local_socket::tokio::Listener {
    fn into_async_listener(self) -> AsyncListener {
        AsyncListener(self)
    }
}

#[cfg(test)]
mod legacy_handoff_tests {
    use super::legacy_duplicate_handle;
    use crate::platform::ipc::HandoffTransferErrorKind;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    #[test]
    fn duplicate_handle_into_current_process_returns_backend_owned_handle() {
        // SAFETY: GetCurrentProcess returns the documented non-owning pseudo-handle.
        let source = unsafe { GetCurrentProcess() } as usize;
        let duplicated = legacy_duplicate_handle(source, std::process::id()).unwrap();
        assert_ne!(duplicated, 0);
        assert_ne!(duplicated, INVALID_HANDLE_VALUE as usize);
        // SAFETY: DuplicateHandle returned an owned handle in this process.
        unsafe { CloseHandle(duplicated as HANDLE) };
    }

    #[test]
    fn missing_backend_pid_maps_to_fallback_safe_error() {
        // SAFETY: GetCurrentProcess returns the documented non-owning pseudo-handle.
        let source = unsafe { GetCurrentProcess() } as usize;
        let error = legacy_duplicate_handle(source, u32::MAX).unwrap_err();
        assert!(matches!(
            error.kind(),
            HandoffTransferErrorKind::BackendUnavailable
                | HandoffTransferErrorKind::PermissionDenied
        ));
    }
}

// Exercises the unbounded IPC forms deprecated by #433 until they are removed.
#[cfg(all(test, feature = "ipc-async"))]
#[allow(deprecated)]
mod security_tests {
    use std::io::{Read as _, Write as _};

    use super::{
        AsyncListener, AsyncStream, Endpoint, IntoAsyncListener, IntoAsyncStream, Listener, Stream,
    };

    #[test]
    fn legacy_async_listener_keeps_its_conversion_contract() {
        fn accepts<T: IntoAsyncListener>() {}
        accepts::<interprocess::local_socket::tokio::Listener>();
    }

    #[test]
    fn legacy_async_stream_keeps_its_conversion_contract() {
        fn accepts<T: IntoAsyncStream>() {}
        accepts::<interprocess::local_socket::tokio::Stream>();
    }

    #[test]
    fn sync_owner_only_security_allows_the_current_user() {
        let endpoint = Endpoint::test("sync-owner-only").expect("test endpoint");
        let listener = Listener::bind_owner_only(&endpoint).expect("bind endpoint");
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().expect("accept current user");
            stream.write_all(b"ok").expect("write response");
        });
        let mut client = Stream::connect(&endpoint).expect("current user can connect");
        let mut response = [0_u8; 2];
        client.read_exact(&mut response).expect("read response");
        assert_eq!(&response, b"ok");
        server.join().expect("server thread");
    }

    #[tokio::test]
    async fn owner_only_security_allows_the_current_user() {
        let endpoint = Endpoint::test("owner-only").expect("test endpoint");
        let listener = AsyncListener::bind_owner_only(&endpoint).expect("bind endpoint");
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept current user");
            stream.write_all(b"ok").await.expect("write response");
        });
        let mut client = AsyncStream::connect(&endpoint)
            .await
            .expect("current user can connect");
        let mut response = [0_u8; 2];
        client
            .read_exact(&mut response)
            .await
            .expect("read response");
        assert_eq!(&response, b"ok");
        server.await.expect("server task");
    }
}
