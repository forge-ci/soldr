//! Linux local IPC transport mechanics.

use std::io::{self, Read, Write};
#[cfg(feature = "ipc-async")]
use std::pin::Pin;
#[cfg(feature = "ipc-async")]
use std::task::{Context, Poll};

use interprocess::local_socket::prelude::*;
#[cfg(feature = "ipc-async")]
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, PeerCreds, ToFsName};
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
        match std::fs::remove_file(&self.0) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn ensure_owner_private_parent(&self) -> io::Result<()> {
        prepare_owner_private_parent(&self.0)
    }

    /// Whether this endpoint's target is present on the host.
    ///
    /// The address names a socket file, which outlives the process that bound
    /// it. `true` therefore means the name is taken, not that a server is
    /// listening: [`Endpoint::is_stale`] answers that separately, and a target
    /// that still exists behind a stale endpoint is the leftover that
    /// [`Endpoint::retire`] removes.
    pub fn target_exists(&self) -> io::Result<bool> {
        match std::fs::symlink_metadata(&self.0) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub fn ensure_parent_exists(&self) -> io::Result<()> {
        if let Some(parent) = std::path::Path::new(&self.0).parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    /// Whether the endpoint has no server behind it and may be taken over.
    ///
    /// A connect classified by error kind: a refused or absent target means
    /// nothing is listening. Any other failure leaves the question open, and
    /// an open question is not a licence to take a live endpoint over. A
    /// server observes the probe as a connection that never sends a byte.
    pub fn is_stale(&self) -> bool {
        use interprocess::local_socket::traits::Stream as _;

        let Ok(endpoint_name) = name(&self.0) else {
            return false;
        };
        match interprocess::local_socket::Stream::connect(endpoint_name) {
            Ok(_stream) => false,
            Err(error) => matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ),
        }
    }

    /// Allocate a unique endpoint for a caller-owned test or probe.
    pub fn test(label: &str) -> io::Result<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let digest = blake3::hash(format!("{label}-{}-{nonce}", std::process::id()).as_bytes());
        Self::new(format!("/tmp/rp-ipc-{}.sock", &digest.to_hex()[..16]))
    }
}

fn name(path: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    path.to_fs_name::<GenericFilePath>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

pub fn select_endpoint_address(
    _kernel_namespace: Option<String>,
    filesystem: Option<std::path::PathBuf>,
) -> Option<String> {
    filesystem.map(|path| path.to_string_lossy().into_owned())
}

pub const fn nonblocking_zero_read_is_pending() -> bool {
    false
}

pub const fn endpoint_is_filesystem_backed() -> bool {
    true
}

fn prepare_owner_private_parent(path: &str) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};

    let parent = std::path::Path::new(path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "IPC endpoint has no parent"))?;
    let mut builder = std::fs::DirBuilder::new();
    match builder.mode(0o700).create(parent) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }

    let metadata = std::fs::symlink_metadata(parent)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC endpoint parent is not a real directory",
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC endpoint parent is not owned by the current user",
        ));
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC endpoint parent is not owner-private",
        ));
    }
    Ok(())
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
    PeerIdentity {
        pid: creds
            .pid()
            .and_then(|pid| u32::try_from(pid).ok())
            .unwrap_or(0),
        user_id: creds.euid().map(|uid| uid.to_string()).unwrap_or_default(),
    }
}

pub fn current_user_id() -> io::Result<String> {
    Ok(unsafe { libc::geteuid() }.to_string())
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

    /// Connect, waiting at most `timeout` for the listener to take the
    /// connection; returns [`io::ErrorKind::TimedOut`] on expiry.
    pub fn connect_within(endpoint: &Endpoint, timeout: std::time::Duration) -> io::Result<Self> {
        interprocess::local_socket::ConnectOptions::new()
            .name(name(endpoint.display())?)
            .wait_mode(ConnectWaitMode::Timeout(timeout))
            .connect_sync()
            .map(Self)
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
    /// A peer that stops reading fills the transport buffer and would
    /// otherwise hold the writer forever. An expired send fails with
    /// [`io::ErrorKind::WouldBlock`] or [`io::ErrorKind::TimedOut`].
    pub fn set_send_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        interprocess::local_socket::traits::Stream::set_send_timeout(&self.0, timeout)
    }

    /// Consume the stream and hand back the descriptor that backs it.
    ///
    /// The transfer of ownership is the point: a caller that wants to run its
    /// own protocol over an already-negotiated connection needs the
    /// descriptor itself, and taking it here is what keeps that caller from
    /// naming the implementation type to get at it.
    pub fn into_owned_fd(self) -> std::os::fd::OwnedFd {
        match self.0 {
            interprocess::local_socket::Stream::UdSocket(uds) => std::os::fd::OwnedFd::from(uds),
        }
    }

    pub fn peer_identity(&self) -> io::Result<PeerIdentity> {
        self.0.peer_creds().map(peer_identity)
    }

    /// Pass this accepted connection to a backend over its control stream.
    pub fn transfer_to_backend(
        &self,
        backend_control: &Self,
        _backend_endpoint: &Endpoint,
        _backend_pid: u32,
        sideband_payload: &[u8],
    ) -> Result<crate::platform::ipc::HandoffAttachment, crate::platform::ipc::HandoffTransferError>
    {
        use std::os::fd::{AsFd as _, AsRawFd as _};

        let control_fd = match &backend_control.0 {
            interprocess::local_socket::Stream::UdSocket(stream) => stream.as_fd().as_raw_fd(),
        };
        let connection_fd = match &self.0 {
            interprocess::local_socket::Stream::UdSocket(stream) => stream.as_fd().as_raw_fd(),
        };
        send_connection_with_payload(control_fd, connection_fd, sideband_payload)?;
        Ok(crate::platform::ipc::HandoffAttachment::new(0, true))
    }
}

fn send_connection_with_payload(
    control_fd: std::os::fd::RawFd,
    connection_fd: std::os::fd::RawFd,
    sideband_payload: &[u8],
) -> Result<(), crate::platform::ipc::HandoffTransferError> {
    use crate::platform::ipc::HandoffTransferError;

    legacy_send_fd_over(control_fd, connection_fd, sideband_payload).map_err(|error| {
        let may_have_reached_backend = error
            .partial_counts()
            .is_some_and(|(transferred, _)| transferred > 0);
        HandoffTransferError::new(
            error.kind(),
            may_have_reached_backend,
            error
                .detail()
                .unwrap_or("SCM_RIGHTS connection transfer failed"),
        )
    })
}

pub fn legacy_send_fd_to(
    socket: &std::path::Path,
    sent_fd: i32,
    payload: &[u8],
) -> Result<(), crate::LegacyHandoffError> {
    use std::os::fd::AsRawFd as _;

    let stream = std::os::unix::net::UnixStream::connect(socket)
        .map_err(legacy_connect_error)?;
    stream.set_nonblocking(true).map_err(legacy_connect_error)?;
    legacy_send_fd_over(stream.as_raw_fd(), sent_fd, payload)
}

pub fn legacy_send_fd_over(
    socket_fd: i32,
    sent_fd: i32,
    payload: &[u8],
) -> Result<(), crate::LegacyHandoffError> {
    use crate::platform::ipc::HandoffTransferErrorKind;
    use crate::LegacyHandoffError;

    if payload.is_empty() {
        return Err(LegacyHandoffError::with_detail(
            HandoffTransferErrorKind::Failed,
            None,
            "connection transfer requires a non-empty sideband payload",
        ));
    }
    let mut payload = payload.to_vec();
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    // SAFETY: CMSG_SPACE only computes aligned storage for the supplied size.
    let control_len = unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) } as usize;
    let control_slots = control_len.div_ceil(std::mem::size_of::<libc::cmsghdr>());
    // `Vec<cmsghdr>` guarantees the alignment required by CMSG_FIRSTHDR;
    // msg_controllen retains the exact byte length returned by CMSG_SPACE.
    let mut control = (0..control_slots)
        .map(|_| unsafe { std::mem::zeroed::<libc::cmsghdr>() })
        .collect::<Vec<_>>();
    // SAFETY: an all-zero msghdr is the documented empty initialization; the
    // live iovec and control-buffer pointers are installed immediately below.
    let mut message = unsafe { std::mem::zeroed::<libc::msghdr>() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control_len as _;

    // SAFETY: `message` points at live, correctly sized iovec/control storage;
    // the one SCM_RIGHTS payload is a libc::c_int file descriptor.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null() {
            return Err(LegacyHandoffError::with_detail(
                HandoffTransferErrorKind::Failed,
                None,
                "could not construct SCM_RIGHTS control message",
            ));
        }
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as _) as _;
        *libc::CMSG_DATA(header).cast::<libc::c_int>() = sent_fd;
    }

    let flags = libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL;
    // SAFETY: both descriptors are borrowed from live opaque streams for the
    // duration of this call and every msghdr pointer references live storage.
    let sent = unsafe { libc::sendmsg(socket_fd, &message, flags) };
    if sent < 0 {
        let error = io::Error::last_os_error();
        let raw = error.raw_os_error();
        let kind = legacy_send_error_kind(&error);
        return Err(LegacyHandoffError::with_detail(
            kind,
            raw,
            format!("SCM_RIGHTS connection transfer failed: {error}"),
        ));
    }
    if sent as usize != payload.len() {
        return Err(LegacyHandoffError::partial(sent as usize, payload.len()));
    }
    Ok(())
}

fn legacy_send_error_kind(error: &io::Error) -> crate::platform::ipc::HandoffTransferErrorKind {
    use crate::platform::ipc::HandoffTransferErrorKind;

    if error.kind() == io::ErrorKind::PermissionDenied {
        HandoffTransferErrorKind::PermissionDenied
    } else if error.kind() == io::ErrorKind::WouldBlock
        || error.raw_os_error() == Some(libc::ENOBUFS)
    {
        HandoffTransferErrorKind::WouldBlock
    } else if matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::NotConnected
    ) {
        HandoffTransferErrorKind::BackendUnavailable
    } else {
        HandoffTransferErrorKind::Failed
    }
}

fn legacy_connect_error(error: io::Error) -> crate::LegacyHandoffError {
    use crate::platform::ipc::HandoffTransferErrorKind;
    use crate::LegacyHandoffError;

    let kind = match error.kind() {
        io::ErrorKind::PermissionDenied => HandoffTransferErrorKind::PermissionDenied,
        io::ErrorKind::WouldBlock => HandoffTransferErrorKind::WouldBlock,
        _ => HandoffTransferErrorKind::BackendUnavailable,
    };
    LegacyHandoffError::new(kind, error.raw_os_error())
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
        use interprocess::os::unix::local_socket::ListenerOptionsExt as _;

        prepare_owner_private_parent(endpoint.display())?;
        ListenerOptions::new()
            .name(name(endpoint.display())?)
            .mode(0o600)
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

/// A Unix-domain listener deliberately inherited by a child process.
///
/// The descriptor and its close-on-exec state never leave this platform
/// implementation. Callers provide their product-owned environment key and
/// command, then receive/return only opaque IPC values.
pub struct InheritedListener {
    listener: interprocess::os::unix::uds_local_socket::Listener,
}

impl InheritedListener {
    pub fn supported() -> bool {
        true
    }

    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        use interprocess::os::unix::uds_local_socket::Listener as UdsListener;

        ListenerOptions::new()
            .name(name(endpoint.display())?)
            .create_sync_as::<UdsListener>()
            .map(|listener| Self { listener })
    }

    pub fn prepare(&self, command: &mut std::process::Command, env_key: &str) -> io::Result<()> {
        use std::os::fd::{AsFd as _, AsRawFd as _};

        let fd = self.listener.as_fd();
        clear_cloexec(&fd)?;
        command.env(env_key, fd.as_raw_fd().to_string());
        Ok(())
    }

    pub fn disown_endpoint(&mut self) {
        use interprocess::local_socket::traits::Listener as _;

        self.listener.do_not_reclaim_name_on_drop();
    }

    pub fn recover_from_env(env_key: &str) -> io::Result<Option<Listener>> {
        let Some(raw) = std::env::var_os(env_key) else {
            return Ok(None);
        };
        let raw = raw.to_string_lossy();
        let fd = parse_descriptor(env_key, &raw)?;
        if !is_listening_socket(fd)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{env_key}={fd} does not name a listening socket"),
            ));
        }
        use interprocess::os::unix::uds_local_socket::Listener as UdsListener;
        use std::os::fd::{FromRawFd as _, OwnedFd};
        // SAFETY: the descriptor was validated as a live stream listener and
        // is inherited into this fresh process descriptor table.
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Some(Listener(UdsListener::from(owned).into())))
    }
}

fn parse_descriptor(env_key: &str, raw: &str) -> io::Result<i32> {
    let fd: i32 = raw.trim().parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{env_key}={raw:?} is not a descriptor number"),
        )
    })?;
    if fd < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{env_key}={fd} is not a valid descriptor"),
        ));
    }
    Ok(fd)
}

fn clear_cloexec(fd: &std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;

    let raw = fd.as_raw_fd();
    // SAFETY: `raw` is borrowed from a live listener for both operations.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above; only FD_CLOEXEC is cleared from the returned flags.
    if unsafe { libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn socket_option(fd: i32, option: libc::c_int) -> io::Result<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `value` and `len` are correctly sized writable locals.
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            std::ptr::addr_of_mut!(value).cast(),
            std::ptr::addr_of_mut!(len),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(value)
}

fn is_listening_socket(fd: i32) -> io::Result<bool> {
    if socket_option(fd, libc::SO_TYPE)? != libc::SOCK_STREAM {
        return Ok(false);
    }
    match socket_option(fd, libc::SO_ACCEPTCONN) {
        Ok(listening) => Ok(listening != 0),
        Err(error) if error.raw_os_error() == Some(libc::ENOPROTOOPT) => Ok(true),
        Err(error) => Err(error),
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
        use interprocess::os::unix::local_socket::ListenerOptionsExt as _;

        prepare_owner_private_parent(endpoint.display())?;
        ListenerOptions::new()
            .name(name(endpoint.display())?)
            .mode(0o600)
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

#[cfg(test)]
mod legacy_handoff_tests {
    use std::io::Write as _;
    use std::os::fd::{AsRawFd as _, RawFd};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{legacy_send_error_kind, legacy_send_fd_over, legacy_send_fd_to};
    use crate::platform::ipc::HandoffTransferErrorKind;

    #[test]
    fn sendmsg_flags_always_include_per_call_nonblocking() {
        assert_ne!(libc::MSG_DONTWAIT, 0);
        assert_ne!(libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL, 0);
    }

    #[test]
    fn send_scm_rights_to_backend_socket_transfers_fd_and_token() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("handoff.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let receiver = std::thread::spawn(move || receive(listener.accept().unwrap().0));
        let file = std::fs::File::open("/dev/null").unwrap();
        let payload = [0x41; 16];
        legacy_send_fd_to(&socket, file.as_raw_fd(), &payload).unwrap();
        let (received_fd, received_payload) = receiver.join().unwrap();
        assert_eq!(received_payload, payload);
        assert_ne!(received_fd, file.as_raw_fd());
        // SAFETY: recvmsg returned a newly owned descriptor.
        unsafe { libc::close(received_fd) };
    }

    #[test]
    fn missing_backend_socket_maps_to_fallback_safe_error() {
        let directory = tempfile::tempdir().unwrap();
        let error = legacy_send_fd_to(&directory.path().join("missing.sock"), -1, &[0x44; 16])
            .unwrap_err();
        assert_eq!(error.kind(), HandoffTransferErrorKind::BackendUnavailable);
    }

    #[test]
    fn ancillary_queue_enobufs_maps_to_silent_would_block() {
        let error = std::io::Error::from_raw_os_error(libc::ENOBUFS);
        assert_eq!(
            legacy_send_error_kind(&error),
            HandoffTransferErrorKind::WouldBlock
        );
    }

    #[test]
    fn send_scm_rights_over_connected_socket_transfers_fd_and_token() {
        let (sender, receiver) = std::os::unix::net::UnixStream::pair().unwrap();
        let file = std::fs::File::open("/dev/null").unwrap();
        let payload = [0x43; 16];
        legacy_send_fd_over(sender.as_raw_fd(), file.as_raw_fd(), &payload).unwrap();
        let (received_fd, received_payload) = receive(receiver);
        assert_eq!(received_payload, payload);
        assert_ne!(received_fd, file.as_raw_fd());
        // SAFETY: recvmsg returned a newly owned descriptor.
        unsafe { libc::close(received_fd) };
    }

    #[test]
    fn saturated_blocking_handoff_socket_returns_silent_would_block_promptly() {
        let (mut sender, receiver) = std::os::unix::net::UnixStream::pair().unwrap();
        sender.set_nonblocking(true).unwrap();
        let fill = [0_u8; 64 * 1024];
        loop {
            match sender.write(&fill) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("fill handoff socket: {error}"),
            }
        }
        sender.set_nonblocking(false).unwrap();
        let file = std::fs::File::open("/dev/null").unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let send_thread = std::thread::spawn(move || {
            done_tx
                .send(legacy_send_fd_over(
                    sender.as_raw_fd(),
                    file.as_raw_fd(),
                    &[0x44; 16],
                ))
                .unwrap();
        });
        let result = done_rx.recv_timeout(Duration::from_millis(500));
        drop(receiver);
        send_thread.join().unwrap();
        let error = result.expect("send must remain nonblocking").unwrap_err();
        assert_eq!(error.kind(), HandoffTransferErrorKind::WouldBlock);
    }

    fn receive(stream: std::os::unix::net::UnixStream) -> (RawFd, [u8; 16]) {
        let mut payload = [0_u8; 16];
        let mut iov = libc::iovec {
            iov_base: payload.as_mut_ptr().cast(),
            iov_len: payload.len(),
        };
        // SAFETY: CMSG_SPACE only computes aligned ancillary storage size.
        let control_len = unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) };
        let slots = (control_len as usize).div_ceil(std::mem::size_of::<libc::cmsghdr>());
        let mut control = (0..slots)
            .map(|_| unsafe { std::mem::zeroed::<libc::cmsghdr>() })
            .collect::<Vec<_>>();
        let mut message = unsafe { std::mem::zeroed::<libc::msghdr>() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control_len as _;
        // SAFETY: every msghdr pointer references live, correctly sized storage.
        let received = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, 0) };
        assert_eq!(received as usize, payload.len());
        // SAFETY: recvmsg initialized the asserted SCM_RIGHTS header and payload.
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        assert!(!header.is_null());
        let received_fd = unsafe { *libc::CMSG_DATA(header).cast::<libc::c_int>() };
        (received_fd, payload)
    }
}

#[cfg(all(test, feature = "ipc-async"))]
mod security_tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::{Endpoint, IntoAsyncListener, IntoAsyncStream, Listener};

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
    fn sync_owner_only_security_sets_socket_mode_0600() {
        let directory = tempfile::tempdir().expect("private tempdir");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private tempdir permissions");
        let endpoint = Endpoint::new(
            directory
                .path()
                .join("sync-owner-only.sock")
                .to_string_lossy()
                .into_owned(),
        )
        .expect("test endpoint");
        let listener = Listener::bind_owner_only(&endpoint).expect("bind endpoint");
        let mode = std::fs::metadata(endpoint.display())
            .expect("endpoint metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        drop(listener);
        endpoint.retire().expect("retire endpoint");
    }

    #[tokio::test]
    async fn owner_only_security_sets_socket_mode_0600() {
        let directory = tempfile::tempdir().expect("private tempdir");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private tempdir permissions");
        let endpoint = Endpoint::new(
            directory
                .path()
                .join("owner-only.sock")
                .to_string_lossy()
                .into_owned(),
        )
        .expect("test endpoint");
        endpoint.retire().expect("retire absent endpoint");
        let listener = super::AsyncListener::bind_owner_only(&endpoint).expect("bind endpoint");
        let mode = std::fs::metadata(endpoint.display())
            .expect("endpoint metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        drop(listener);
        endpoint.retire().expect("retire endpoint");
    }

    #[test]
    fn owner_only_security_rejects_without_mutating_a_shared_parent() {
        let directory = tempfile::tempdir().expect("private tempdir");
        let shared = directory.path().join("shared");
        std::fs::create_dir(&shared).expect("shared directory");
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755))
            .expect("shared permissions");
        let endpoint = Endpoint::new(
            shared
                .join("owner-only.sock")
                .to_string_lossy()
                .into_owned(),
        )
        .expect("test endpoint");

        let error = match super::AsyncListener::bind_owner_only(&endpoint) {
            Ok(_) => panic!("shared parent must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        let mode = std::fs::metadata(&shared)
            .expect("shared metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
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
