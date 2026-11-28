//! Local endpoint, listener, connection, peer, handoff, and security primitives.
//!
//! Endpoint strings and protocol policy remain with callers. These opaque
//! values own the selected host transport so callers never name Unix sockets,
//! Windows named pipes, `interprocess` types, file descriptors, or handles.
//!
//! # Filesystem endpoints need an owner-private parent
//!
//! On Linux, `Listener::bind_owner_only` (and
//! `Endpoint::ensure_owner_private_parent`) check the socket's *parent
//! directory*:
//! it is created `0700` if missing, and an existing one must be a real
//! directory owned by the effective user with mode exactly `0700`. Otherwise
//! the bind fails with `PermissionDenied` and one of `IPC endpoint parent is
//! not owned by the current user` or `IPC endpoint parent is not
//! owner-private`. So `/tmp/app.sock` (sticky `1777`, owned by root) and
//! `~/.cache/app.sock` (usually `0755`) are refused; use
//! `$XDG_RUNTIME_DIR/app.sock` (already `0700`) or a dedicated subdirectory
//! such as `/tmp/app-$UID/app.sock` that this check creates `0700`.
//!
//! # Endpoint existence and staleness
//!
//! `Endpoint::target_exists` and `Endpoint::is_stale` ask two different
//! questions, and how far their answers can drift apart is a property of the
//! selected transport rather than of the caller's code. Every host answers
//! both for real; neither ever reports a fabricated value.
//!
//! Where [`endpoint_is_filesystem_backed`] reports `true`, the address names a
//! socket file that outlives the process which bound it. `target_exists` reads
//! the filesystem without disturbing a server, and the two predicates disagree
//! exactly when a dead server left its socket file behind: `Ok(true)` from
//! `target_exists` with `is_stale` reporting `true` is the leftover that
//! [`Endpoint::retire`] removes before a new server binds.
//!
//! Where it reports `false`, the address names a kernel object that cannot
//! outlive the process holding it. There is no leftover to find, `retire` has
//! nothing to remove, and both predicates are answered by one connect probe, so
//! existence and liveness always agree. That probe is observable -- the server
//! sees a connection that never sends a byte -- so a caller that only wants
//! liveness should probe once rather than ask both.
//!
//! Neither predicate takes an unclassifiable failure as a licence to reclaim an
//! endpoint: `target_exists` returns the error and `is_stale` reports `false`.

#[cfg(feature = "ipc")]
pub use crate::{
    ipc_current_user_id as current_user_id, IpcEndpoint as Endpoint,
    IpcInheritedListener as InheritedListener, IpcListener as Listener,
    IpcListenerNonblockingMode as ListenerNonblockingMode, IpcPeerIdentity as PeerIdentity,
    IpcPeerIdentitySource as PeerIdentitySource, IpcStream as Stream,
};

// Owner-only single-instance transport primitives.
//
// These are the lower-level building blocks beneath `Listener`/`Stream`: one
// pathname-socket listener or one named-pipe instance at a time, with the
// native endpoint string passed through verbatim. Endpoint spelling, stale
// retirement, parent-directory policy, pooling, retry, deadlines, and peer
// admission policy stay with the caller. Every host exports every name; the
// pair that is not the selected host's transport is uninhabited, and its
// constructors report `std::io::ErrorKind::Unsupported`. All polling byte I/O
// uses standard-library types only.
//
// * `LocalSocketListener::bind_owner_only(&Path)` binds a pathname socket and
//   sets the socket file to mode `0o600`. It never retires an existing path
//   and dropping it does not unlink the pathname. `accept(&self)` yields a
//   `LocalSocketStream` and the observed `SocketPeerCredentials`; a canceled
//   accept leaves the listener usable.
// * `LocalSocketStream::connect(&Path)` connects to a pathname socket.
// * `OwnerOnlyPipeInstance::create(endpoint, first)` creates one byte-mode
//   named-pipe instance whose protected DACL grants only the owner and SYSTEM
//   and which rejects remote clients. `first` requests first-instance
//   exclusivity. `connect(&self)` waits for one client; canceling that future
//   retains the instance.
// * `LocalPipeClient::open(endpoint)` performs one native open attempt and
//   preserves its error, including pipe-busy (231), so callers own retry.
//
// Construction requires an active I/O-enabled async runtime.
#[cfg(feature = "ipc")]
pub use crate::{
    IpcLocalPipeClient as LocalPipeClient, IpcLocalSocketListener as LocalSocketListener,
    IpcLocalSocketStream as LocalSocketStream, IpcOwnerOnlyPipeInstance as OwnerOnlyPipeInstance,
};

/// Remove a caller-owned socket pathname, refusing non-sockets and symlinks.
///
/// A missing path succeeds. On a host whose local IPC transport leaves no
/// filesystem endpoint behind (named pipes), this is a no-op. It does not
/// probe liveness: the caller decides retirement is appropriate, and the
/// parent directory must be protected against concurrent path replacement
/// because inspection and unlink are separate operations.
#[cfg(feature = "ipc")]
pub fn retire_socket_endpoint(path: &std::path::Path) -> std::io::Result<()> {
    crate::ipc_retire_socket_endpoint(path)
}

/// Credentials observed on a connection accepted by
/// [`LocalSocketListener::accept`].
///
/// An unavailable PID does not imply unavailable credentials: some hosts
/// report only the peer's user.
#[cfg(feature = "ipc")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SocketPeerCredentials {
    pid: Option<u32>,
    current_user: bool,
    available: bool,
}

#[cfg(feature = "ipc")]
impl SocketPeerCredentials {
    // Only a pathname-socket host observes credentials; the other host never
    // constructs this value.
    #[allow(dead_code, reason = "constructed only by pathname-socket hosts")]
    pub(crate) fn observed(pid: Option<u32>, current_user: bool) -> Self {
        Self {
            pid,
            current_user,
            available: true,
        }
    }

    #[allow(dead_code, reason = "constructed only by pathname-socket hosts")]
    pub(crate) fn unavailable() -> Self {
        Self {
            pid: None,
            current_user: false,
            available: false,
        }
    }

    /// The peer process ID, when the host reports one.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Whether the peer runs as this process's effective user.
    pub fn is_current_user(&self) -> bool {
        self.current_user
    }

    /// Whether the host reported peer credentials at all.
    pub fn credentials_available(&self) -> bool {
        self.available
    }
}

/// Opaque platform attachment created while transferring an accepted IPC
/// connection to a backend process.
///
/// On Windows this owns the handle-table value that must be carried by the
/// caller's existing protocol. On Unix the descriptor travels out-of-band via
/// `SCM_RIGHTS`. Native handle and descriptor values never cross the facade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandoffAttachment {
    protocol_value: u64,
    backend_may_adopt_before_offer: bool,
}

/// Host-neutral candidates for one endpoint address.
///
/// Product naming policy may derive both a kernel-namespace name and a
/// filesystem path. The selected transport chooses the applicable standard
/// library value without exposing that host choice to the caller.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg(feature = "ipc")]
pub struct EndpointAddressCandidates {
    kernel_namespace: Option<String>,
    filesystem: Option<std::path::PathBuf>,
}

#[cfg(feature = "ipc")]
impl EndpointAddressCandidates {
    pub fn new(kernel_namespace: Option<String>, filesystem: Option<std::path::PathBuf>) -> Self {
        Self {
            kernel_namespace,
            filesystem,
        }
    }

    /// Select the address used by the active local IPC transport.
    pub fn select(self) -> Option<String> {
        crate::ipc_select_endpoint_address(self.kernel_namespace, self.filesystem)
    }
}

impl HandoffAttachment {
    #[cfg(feature = "ipc")]
    pub(crate) fn new(protocol_value: u64, backend_may_adopt_before_offer: bool) -> Self {
        Self {
            protocol_value,
            backend_may_adopt_before_offer,
        }
    }

    /// Append this attachment's opaque value as an unsigned protobuf varint.
    ///
    /// The caller owns the wire envelope while this facade retains ownership
    /// of the native value and its representation.
    pub fn append_unsigned_varint(self, output: &mut Vec<u8>) {
        let mut value = self.protocol_value;
        while value >= 0x80 {
            output.push((value as u8 & 0x7f) | 0x80);
            value >>= 7;
        }
        output.push(value as u8);
    }

    /// Whether the backend may adopt the connection before its offer arrives.
    ///
    /// Unix transfers the descriptor and token together in the sideband
    /// message, while Windows requires the later offer to identify the
    /// duplicated handle. Callers use this transport fact to make their own
    /// proxy-fallback ownership decision without selecting a host.
    pub fn backend_may_adopt_before_offer(self) -> bool {
        self.backend_may_adopt_before_offer
    }
}

/// Result of enforcing owner-private permissions on a local IPC directory.
#[cfg(feature = "ipc")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerPrivateDirectoryOutcome {
    /// The existing directory already had the complete host policy.
    AlreadyPrivate,
    /// Permissions were applied or repaired.
    Hardened,
}

/// Create a directory and enforce the selected host's owner-private policy.
#[cfg(feature = "ipc")]
pub fn ensure_owner_private_directory(
    path: &std::path::Path,
) -> std::io::Result<OwnerPrivateDirectoryOutcome> {
    crate::ipc_ensure_owner_private_directory(path)
}

/// Return whether a directory has the selected host's owner-private policy.
#[cfg(feature = "ipc")]
pub fn owner_private_directory(path: &std::path::Path) -> std::io::Result<bool> {
    crate::ipc_owner_private_directory(path)
}

/// Whether an empty nonblocking read means "not ready yet" for this host's
/// local IPC transport rather than end-of-stream.
#[cfg(feature = "ipc")]
pub fn nonblocking_zero_read_is_pending() -> bool {
    crate::ipc_nonblocking_zero_read_is_pending()
}

/// Whether the selected local IPC transport uses filesystem endpoint names.
#[cfg(feature = "ipc")]
pub fn endpoint_is_filesystem_backed() -> bool {
    crate::ipc_endpoint_is_filesystem_backed()
}

/// Host-neutral classification of a failed connection-transfer primitive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffTransferErrorKind {
    Unsupported,
    PermissionDenied,
    BackendUnavailable,
    WouldBlock,
    Failed,
}

/// Failure from the platform-owned connection-transfer primitive.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{detail}")]
pub struct HandoffTransferError {
    kind: HandoffTransferErrorKind,
    may_have_reached_backend: bool,
    detail: String,
}

impl HandoffTransferError {
    #[cfg(feature = "ipc")]
    pub(crate) fn new(
        kind: HandoffTransferErrorKind,
        may_have_reached_backend: bool,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            may_have_reached_backend,
            detail: detail.into(),
        }
    }

    /// Return the policy-neutral failure category.
    pub fn kind(&self) -> HandoffTransferErrorKind {
        self.kind
    }

    /// Whether the backend may already own a duplicated connection.
    pub fn may_have_reached_backend(&self) -> bool {
        self.may_have_reached_backend
    }
}

/// Resolve a broker endpoint name using selected-host path and pipe rules.
#[cfg(feature = "ipc")]
pub fn broker_endpoint_name(bare_name: &str, path_scoped: bool) -> std::io::Result<String> {
    crate::IpcBrokerEndpointName(bare_name, path_scoped)
}

/// The selected host's limit on a local IPC endpoint name.
///
/// Unix transports are bounded by the `sun_path` field of `sockaddr_un`;
/// Windows named pipes are bounded by `MAX_PATH` unless the long-path
/// prefix is in use. Callers use this to report a budget without naming
/// which host they are on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointNameLimit {
    /// Largest endpoint name this host accepts, in bytes.
    pub max_bytes: usize,
    /// Operator-facing name of the limit, e.g. `"macOS sun_path"`.
    pub label: &'static str,
}

/// Report the selected host's endpoint-name budget.
#[cfg(feature = "ipc")]
pub fn endpoint_name_limit() -> EndpointNameLimit {
    crate::ipc_endpoint_name_limit()
}

/// A derived endpoint name that does not fit this host's budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointNameTooLong {
    /// Length of the name that was derived.
    pub len: usize,
    /// Largest length that would have been accepted.
    pub max: usize,
    /// Operator-facing name of the limit that rejected it.
    pub limit_label: &'static str,
}

/// Last-resort per-user root when the host's runtime/temp variable is unset.
///
/// Deliberately not `/tmp`: a per-user directory keeps two accounts on one
/// host from colliding without naming a uid. Only reached when the platform's
/// runtime variable is missing -- cron and sessionless ssh being the realistic
/// cases.
#[cfg(feature = "ipc")]
pub(crate) fn per_user_runtime_fallback() -> std::path::PathBuf {
    dirs::cache_dir()
        .or_else(dirs::data_local_dir)
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("running-process")
        .join("broker-v2")
}

/// Canonical byte spelling of `path` for endpoint-scope identity.
///
/// Two callers naming the same installed file must hash to the same scope, so
/// the selected host decides which spelling differences are meaningless. On a
/// case-insensitive host that means folding case and separator style; on a
/// host whose paths are opaque byte strings it means the bytes as they are.
/// Callers own the hash itself, its domain separator, and its encoding.
#[cfg(feature = "ipc")]
pub fn endpoint_scope_bytes(path: &std::path::Path) -> Vec<u8> {
    crate::ipc_endpoint_scope_bytes(path)
}

/// The selected host's directory for broker-v2 runtime files and sockets.
///
/// Every host lands inside a location the OS already scopes to a single user,
/// so two accounts stay apart without a uid spelled into the path. The leaf is
/// chosen per host rather than shared: on macOS the broker's sockets live under
/// this root, and `sun_path` leaves no budget for a long one.
///
/// The directory is not created here. A caller that writes into it creates it
/// owner-only at that point; a caller that only reads treats an absent
/// directory as "nothing published", which is a normal state.
#[cfg(feature = "ipc")]
pub fn broker_v2_runtime_dir() -> std::path::PathBuf {
    crate::ipc_broker_v2_runtime_dir()
}

/// Derive the v1 broker endpoint address for `bare_name`.
///
/// The selected host owns directory placement, the leaf spelling, and the
/// length check. Callers own which bare name to ask for. The returned string
/// is the address the caller passes back to [`Endpoint::new`]; it is a
/// filesystem path where [`endpoint_is_filesystem_backed`] reports `true` and
/// a kernel-namespace name otherwise.
#[cfg(feature = "ipc")]
pub fn broker_v1_endpoint_path(bare_name: &str) -> Result<String, EndpointNameTooLong> {
    crate::ipc_broker_v1_endpoint_path(bare_name)
}

#[cfg(feature = "ipc-async")]
pub use crate::{
    IpcAsyncListener as AsyncListener, IpcAsyncReadHalf as AsyncReadHalf,
    IpcAsyncStream as AsyncStream, IpcAsyncWriteHalf as AsyncWriteHalf,
    IpcIntoAsyncListener as IntoAsyncListener, IpcIntoAsyncStream as IntoAsyncStream,
};

// These round-trips are the contract tests of the unbounded IPC forms
// deprecated by #433; they keep exercising them until the forms are removed.
#[cfg(all(test, feature = "ipc"))]
#[allow(deprecated)]
mod tests {
    use std::io::{Read, Write};

    use super::{
        current_user_id, ensure_owner_private_directory, owner_private_directory, Endpoint,
        HandoffAttachment, Listener, Stream,
    };

    #[test]
    fn ensure_private_dir_passes_private_check() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("private");
        ensure_owner_private_directory(&path).expect("harden directory");
        assert!(owner_private_directory(&path).expect("inspect directory"));
    }

    #[test]
    fn handoff_attachment_can_be_encoded_without_exposing_its_value() {
        let mut encoded = Vec::new();
        HandoffAttachment::new(300, false).append_unsigned_varint(&mut encoded);
        assert_eq!(encoded, [0xac, 0x02]);
    }

    #[test]
    fn handoff_attachment_reports_pre_offer_adoption_semantics() {
        assert!(HandoffAttachment::new(0, true).backend_may_adopt_before_offer());
        assert!(!HandoffAttachment::new(0, false).backend_may_adopt_before_offer());
    }

    #[test]
    fn endpoint_lifecycle_mechanics_are_facade_owned() {
        let endpoint = Endpoint::test("lifecycle").expect("test endpoint");
        endpoint.retire().expect("retire absent endpoint");

        let listener = Listener::bind(&endpoint).expect("bind endpoint");

        drop(listener);
        endpoint.retire().expect("retire endpoint");
    }

    // The tests that follow pin the exact connect-and-drop liveness probe that
    // zccache's per-platform `probe_native` performs directly against
    // `interprocess` today (three call sites, one per OS, identical body):
    // resolve the endpoint name, attempt a blocking connect, and classify the
    // result. `Stream::connect` and `Endpoint::is_stale` already give callers
    // both forms of that probe without naming the backend transport crate.
    #[test]
    fn missing_endpoint_probe_reports_a_stable_not_found_or_refused_error() {
        let endpoint = Endpoint::test("probe-missing").expect("test endpoint");

        let error = Stream::connect(&endpoint).expect_err("missing endpoint must fail to connect");

        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        ));
    }

    // `Endpoint::is_stale` performs the same connect-and-classify probe as
    // `missing_endpoint_probe_reports_a_stable_not_found_or_refused_error`,
    // collapsed to a bool. Every transport implements it, so this runs on
    // every host.
    #[test]
    fn missing_endpoint_is_reported_stale() {
        let endpoint = Endpoint::test("probe-missing-stale").expect("test endpoint");

        assert!(endpoint.is_stale());
        assert!(!endpoint.target_exists().expect("probe a missing endpoint"));
    }

    // A bound listener answers a probe connect with no accept required: a Unix
    // socket completes it from the kernel backlog, and a named pipe from the
    // instance the listener created while binding.
    //
    // The second probe covers the busy named-pipe instance the first one
    // leaves behind, which only the server's next accept would clear. Busy
    // still means a server holds the name, so existence must come back `true`
    // without waiting for an accept that never comes. A regression there hangs
    // rather than fails, so the answer is collected with a deadline instead of
    // by blocking the test thread.
    #[test]
    fn bound_endpoint_is_not_reported_stale() {
        let endpoint = Endpoint::test("probe-live-stale").expect("test endpoint");
        let listener = Listener::bind(&endpoint).expect("bind");

        assert!(!endpoint.is_stale());

        let probed = endpoint.clone();
        let (answer_tx, answer_rx) = std::sync::mpsc::channel();
        let prober = std::thread::spawn(move || {
            let _ = answer_tx.send(probed.target_exists().map_err(|error| error.to_string()));
        });
        let exists = answer_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the second probe answered without waiting for an accept")
            .expect("probe a bound endpoint");
        assert!(exists);
        prober.join().expect("prober thread");

        drop(listener);
    }

    // The two transports diverge once the server is gone, and each half of
    // that divergence is pinned on the host it applies to. A socket file is
    // still there to find and retire; a named pipe is not, because the name
    // died with the process that held it.
    #[cfg(unix)]
    #[test]
    fn a_departed_server_leaves_a_stale_socket_file_to_retire() {
        let endpoint = Endpoint::test("probe-leftover").expect("test endpoint");
        let mut listener = Listener::bind(&endpoint).expect("bind");
        // Binding reclaims the name on drop by default, which is the very
        // cleanup a crashed server never gets to perform.
        listener.do_not_reclaim_name_on_drop();
        drop(listener);

        assert!(endpoint
            .target_exists()
            .expect("inspect the leftover target"));
        assert!(endpoint.is_stale());

        endpoint.retire().expect("retire the leftover");
        assert!(!endpoint
            .target_exists()
            .expect("inspect the retired target"));
    }

    #[cfg(windows)]
    #[test]
    fn a_departed_server_leaves_no_named_pipe_behind() {
        let endpoint = Endpoint::test("probe-leftover").expect("test endpoint");
        let listener = Listener::bind(&endpoint).expect("bind");
        assert!(endpoint.target_exists().expect("inspect the live target"));

        drop(listener);

        assert!(!endpoint
            .target_exists()
            .expect("inspect the vanished target"));
        assert!(endpoint.is_stale());
        // Nothing was left to clean up, so retiring is a no-op that must not
        // report a failure.
        endpoint.retire().expect("retire a vanished endpoint");
    }

    #[test]
    fn live_endpoint_probe_succeeds_without_disturbing_a_later_accept() {
        let endpoint = Endpoint::test("probe-live").expect("test endpoint");
        let listener = Listener::bind(&endpoint).expect("bind");
        let (probe_accepted_tx, probe_accepted_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            // One accept absorbs the probe connection; the second serves the
            // ordinary connection made after the probe returns.
            listener.accept().expect("accept probe connection");
            probe_accepted_tx.send(()).expect("report probe accepted");
            listener.accept().expect("accept later connection");
        });

        // The probe itself: connect, observe success, and let it go without
        // ever exchanging a byte, exactly like zccache's `probe_native`.
        let probe = Stream::connect(&endpoint).expect("probe connect");

        // Wait for the server to absorb the probe before dropping it and
        // dialing again. The probe is held until the accept lands because the
        // Windows named-pipe listener treats a client that disconnects before
        // `ConnectNamedPipe` runs as an empty connection: it clears that
        // instance and keeps blocking, so the accept would never report.
        // Waiting also guarantees the next server instance exists, since the
        // listener only creates it inside `accept()`.
        probe_accepted_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("server accepted the probe connection");
        drop(probe);

        // The endpoint keeps serving ordinary connections after the probe.
        let _client = Stream::connect(&endpoint).expect("connect after probe");
        server.join().expect("server thread");
    }

    #[test]
    fn sync_bind_accept_connect_and_peer_identity_round_trip() {
        let endpoint = Endpoint::test("sync-roundtrip").expect("test endpoint");
        let listener = Listener::bind(&endpoint).expect("bind");
        let expected_user = current_user_id().expect("current user identity");
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().expect("accept");
            let peer = stream.peer_identity().expect("peer identity");
            assert_eq!(peer.user_id, expected_user);
            let mut request = [0_u8; 4];
            stream.read_exact(&mut request).expect("read request");
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").expect("write response");
        });

        let mut client = Stream::connect(&endpoint).expect("connect");
        client.write_all(b"ping").expect("write request");
        let mut response = [0_u8; 4];
        client.read_exact(&mut response).expect("read response");
        assert_eq!(&response, b"pong");
        server.join().expect("server thread");
    }

    #[cfg(feature = "ipc-async")]
    #[tokio::test]
    async fn async_bind_accept_connect_and_peer_identity_round_trip() {
        use super::{AsyncListener, AsyncStream};

        let endpoint = Endpoint::test("async-roundtrip").expect("test endpoint");
        let listener = AsyncListener::bind(&endpoint).expect("bind");
        let expected_user = current_user_id().expect("current user identity");
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept");
            let peer = stream.peer_identity().expect("peer identity");
            assert_eq!(peer.user_id, expected_user);
            let mut request = [0_u8; 4];
            stream.read_exact(&mut request).await.expect("read request");
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").await.expect("write response");
        });

        let mut client = AsyncStream::connect(&endpoint).await.expect("connect");
        client.write_all(b"ping").await.expect("write request");
        let mut response = [0_u8; 4];
        client
            .read_exact(&mut response)
            .await
            .expect("read response");
        assert_eq!(&response, b"pong");
        server.await.expect("server task");
    }

    // No `use tokio::io::{AsyncReadExt, AsyncWriteExt}` here: `AsyncStream`'s
    // `read`/`read_exact`/`write_all`/`flush`/`shutdown` are inherent methods,
    // so this test exercises the whole method set with no extension-trait
    // import in scope, which is exactly what an external client now gets.
    #[cfg(feature = "ipc-async")]
    #[tokio::test]
    async fn async_inherent_methods_round_trip_with_no_extension_trait_import() {
        use super::{AsyncListener, AsyncStream};

        let endpoint = Endpoint::test("async-inherent-roundtrip").expect("test endpoint");
        let listener = AsyncListener::bind(&endpoint).expect("bind");
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept");
            let mut request = [0_u8; 4];
            stream.read_exact(&mut request).await.expect("read request");
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").await.expect("write response");
            stream.flush().await.expect("flush response");
            stream.shutdown().await.expect("shutdown write half");
        });

        let mut client = AsyncStream::connect(&endpoint).await.expect("connect");
        client.write_all(b"ping").await.expect("write request");
        client.flush().await.expect("flush request");
        // `read` may return a short read even over a live local connection,
        // so fill the buffer in a loop rather than asserting one call
        // returns all four bytes.
        let mut response = [0_u8; 4];
        let mut filled = 0;
        while filled < response.len() {
            let read = client
                .read(&mut response[filled..])
                .await
                .expect("read response");
            assert_ne!(read, 0, "peer closed before the full response arrived");
            filled += read;
        }
        assert_eq!(&response, b"pong");
        server.await.expect("server task");
    }

    // Proves `into_split` frees a caller from ever storing tokio's
    // `ReadHalf`/`WriteHalf` itself (the leak zccache's `transport::mod`
    // currently has): both halves here are facade-owned types with private
    // fields, driven independently, still with no tokio import in scope.
    #[cfg(feature = "ipc-async")]
    #[tokio::test]
    async fn async_into_split_round_trips_across_owned_halves() {
        use super::{AsyncListener, AsyncStream};

        let endpoint = Endpoint::test("async-split-roundtrip").expect("test endpoint");
        let listener = AsyncListener::bind(&endpoint).expect("bind");
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.expect("accept");
            let (mut read_half, mut write_half) = stream.into_split();
            let mut request = [0_u8; 4];
            read_half
                .read_exact(&mut request)
                .await
                .expect("read request");
            assert_eq!(&request, b"ping");
            write_half.write_all(b"pong").await.expect("write response");
            write_half.flush().await.expect("flush response");
            write_half.shutdown().await.expect("shutdown write half");
        });

        let client = AsyncStream::connect(&endpoint).await.expect("connect");
        let (mut client_read, mut client_write) = client.into_split();
        client_write
            .write_all(b"ping")
            .await
            .expect("write request");
        client_write.flush().await.expect("flush request");
        let mut response = [0_u8; 4];
        client_read
            .read_exact(&mut response)
            .await
            .expect("read response");
        assert_eq!(&response, b"pong");
        server.await.expect("server task");
    }
}
