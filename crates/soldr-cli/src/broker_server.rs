//! Soldr's single stable broker listener (#2476).
//!
//! Admin, readiness, Hello negotiation, route progress, and SESSION bytes all
//! share one accepted connection. After negotiation the broker attempts the
//! optimized handoff path; the portable baseline keeps that same connection
//! and transparently proxies it to the selected daemon endpoint.

use crate::broker_retry::effective_retry_after_ms;
use interprocess::local_socket::traits::tokio::Listener as _;
use prost::Message;
use running_process::broker::protocol::{
    hello_reply, AdminRequest, AdminVerb, ErrorCode, Frame, FrameKind, Hello, HelloReply,
    PayloadEncoding, Refused, ADMIN_PAYLOAD_PROTOCOL, CONTROL_PAYLOAD_PROTOCOL, ENVELOPE_VERSION,
    MAX_FRAME_BYTES, PROTOCOL_VERSION,
};
use running_process::broker::server::{
    AdminSnapshot, BackendRegistry, BrokerInstanceKey, CombinedServiceDefinitionLoader,
    FdPressureGuard, HelloResponder, HelloRouter, PeerCredentialPolicy, ServeHandoffContext,
    SpawnCoordinator,
};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::broker_deadlines::BrokerDeadlines;

use crate::broker_debug::broker_debug_enabled;

pub(crate) const BROKER_INSTANCE_ID_ENV: &str = "SOLDR_INTERNAL_BROKER_INSTANCE_ID";

pub(crate) fn broker_image_instance_id() -> io::Result<String> {
    let executable = std::env::current_exe()?;
    // soldr#2521 B1: machine-scoped memo so isolated roots share hits.
    let cache = crate::daemon::image_hash::machine_scoped_cache_dir(
        &crate::daemon::service_definition::broker_owned_paths()
            .cache
            .join("broker-image-hash"),
    );
    let digest = broker_image_digest(&cache, &executable)?;
    Ok(format_broker_instance_id(
        env!("CARGO_PKG_VERSION"),
        &digest,
    ))
}

fn broker_image_digest(
    cache: &std::path::Path,
    executable: &std::path::Path,
) -> io::Result<String> {
    if let Some(build_id) = crate::platform::executable::image::current_build_id() {
        // A linker build ID identifies the running image and is already mapped
        // into this process. Hash the short note to retain the existing 64-hex
        // instance-id shape without reading a 100+ MiB no-opt executable.
        // Hosts without one (and images linked without one) fall back below.
        return Ok(zccache::hash::hash_bytes(&build_id).to_hex().to_string());
    }
    crate::daemon::image_hash::cached_blake3_hex(cache, executable)
}

fn broker_server_instance_id() -> io::Result<String> {
    if let Some(instance_id) = std::env::var(BROKER_INSTANCE_ID_ENV)
        .ok()
        .filter(|value| !value.is_empty())
    {
        return Ok(instance_id);
    }
    broker_image_instance_id()
}

fn format_broker_instance_id(version: &str, image_digest: &str) -> String {
    format!("soldr-{version}-{image_digest}")
}

/// Protobuf lane for route-start progress events (`RP` in ASCII).
pub(crate) const ROUTE_PROGRESS_PAYLOAD_PROTOCOL: u32 = 0x5250;
/// Soldr-private admin tunnel (`SC` in ASCII). The request selects an existing
/// verified daemon route; after a broker restart it may re-adopt a persisted
/// claim, but it never launches a missing backend.
pub(crate) const DAEMON_CONTROL_PAYLOAD_PROTOCOL: u32 = 0x5343;
const BROKER_LISTEN_BACKLOG: i32 = 1024;

#[derive(Clone, PartialEq, Message)]
pub(crate) struct RouteProgress {
    #[prost(string, tag = "1")]
    pub stage: String,
    #[prost(uint32, tag = "2")]
    pub attempt: u32,
    #[prost(uint64, tag = "3")]
    pub elapsed_ms: u64,
    #[prost(string, tag = "4")]
    pub latest_result: String,
    #[prost(uint64, tag = "5")]
    pub retry_after_ms: u64,
}

/// Encoded in `Hello.peer_attestation_nonce`. The field was reserved for a
/// challenge-response payload, so carrying a protobuf attestation is additive
/// and leaves the frozen Hello field numbering intact.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct ClientHostAttestation {
    #[prost(string, tag = "1")]
    pub machine_id: String,
    #[prost(string, tag = "2")]
    pub boot_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct DaemonControlTunnelRequest {
    #[prost(string, tag = "1")]
    pub service_name: String,
    #[prost(string, tag = "2")]
    pub machine_id: String,
    #[prost(string, tag = "3")]
    pub boot_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct DaemonControlTunnelReply {
    #[prost(bool, tag = "1")]
    pub accepted: bool,
    #[prost(string, tag = "2")]
    pub error_detail: String,
    #[prost(bool, tag = "3")]
    pub not_running: bool,
}

pub(crate) fn client_host_attestation() -> Vec<u8> {
    let identity = running_process::broker::host_identity::current();
    ClientHostAttestation {
        machine_id: identity.machine_id,
        boot_id: identity.boot_id,
    }
    .encode_to_vec()
}

fn route_progress_heartbeat_interval(progress_silence: Duration) -> Duration {
    (progress_silence / 3)
        .max(Duration::from_millis(1))
        .min(Duration::from_secs(1))
}

struct BrokerState {
    instance_id: String,
    loader: CombinedServiceDefinitionLoader,
    // `Arc`-wrapped so the reaper's spawned task can hold its own clone.
    registry: Arc<Mutex<BackendRegistry>>,
    spawn_coordinator: Mutex<SpawnCoordinator>,
    launcher: crate::broker_launcher::SoldrBackendLauncher,
    started_at: Instant,
    connections_open: AtomicU64,
    fd_guard: FdPressureGuard,
    /// Which processes asked for each route, so a route whose requesters are
    /// all gone can be torn down (soldr#3054). Populated from kernel-supplied
    /// peer credentials, never from anything the caller declares.
    route_owners: Arc<Mutex<crate::broker_reaper::RouteOwnership>>,
}

impl BrokerState {
    /// No connection open and no route owned (soldr#3193).
    fn is_idle(&self) -> bool {
        self.connections_open.load(Ordering::Relaxed) == 0
            && self
                .route_owners
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len()
                == 0
    }

    fn route(
        &self,
        frame: Frame,
        peer: running_process::broker::server::PeerIdentity,
    ) -> HelloReply {
        let router = HelloRouter::with_lifecycle_monitor(&self.loader, &self.registry)
            .with_spawn_coordinator(&self.spawn_coordinator)
            .with_backend_launcher(&self.launcher);
        router.handle_frame(frame, peer)
    }

    fn snapshot(&self) -> AdminSnapshot {
        let registry = self
            .registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        AdminSnapshot::from_registry(
            &self.instance_id,
            self.started_at.elapsed(),
            true,
            self.connections_open.load(Ordering::Relaxed),
            &registry,
            &[],
        )
        .with_fd_pressure_demoted(self.fd_guard.is_demoted())
    }

    fn instance_for_route(
        &self,
        service_name: &str,
        service_version: &str,
        backend_pipe: &str,
    ) -> Option<BrokerInstanceKey> {
        let registry = self
            .registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = registry.iter().find_map(|(key, handle)| {
            (key.service_name == service_name
                && key.service_version == service_version
                && handle.daemon_process.ipc_endpoint.path == backend_pipe)
                .then(|| key.instance.clone())
        });
        instance
    }

    fn private_control_endpoint_for_service(&self, service_name: &str) -> Option<String> {
        let debug = broker_debug_enabled();
        let started = Instant::now();
        {
            let mut registry = self
                .registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            registry.prune_stale();
            let endpoint = registry.iter().find_map(|(key, handle)| {
                (key.service_name == service_name).then(|| {
                    private_control_endpoint_from_session(&handle.daemon_process.ipc_endpoint.path)
                })
            });
            if let Some(endpoint) = endpoint {
                if debug {
                    eprintln!(
                        "soldr broker: control route {service_name} found in registry endpoint={endpoint}"
                    );
                }
                return Some(endpoint);
            }
        }

        if debug {
            eprintln!("soldr broker: re-adopting control route {service_name}");
        }

        let adopted = match self
            .launcher
            .adopt_existing_control_route(&self.loader, service_name)
        {
            Ok(adopted) => adopted,
            Err(error) => {
                if broker_debug_enabled() {
                    eprintln!(
                        "soldr broker: control route {service_name} could not be re-adopted: {error}"
                    );
                }
                None
            }
        }?;
        let endpoint =
            private_control_endpoint_from_session(&adopted.1.daemon_process.ipc_endpoint.path);
        if debug {
            eprintln!(
                "soldr broker: control route {service_name} re-adopted endpoint={endpoint} elapsed={:?}",
                started.elapsed()
            );
        }
        self.registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(adopted.0, adopted.1);
        Some(endpoint)
    }
}

fn private_control_endpoint_from_session(session_endpoint: &str) -> String {
    crate::daemon::session_endpoint::private_control_endpoint_from_session(session_endpoint)
}

struct OpenConnection(Arc<BrokerState>);

impl Drop for OpenConnection {
    fn drop(&mut self) {
        self.0.connections_open.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) fn serve(endpoint: &crate::broker_identity::ResolvedBrokerEndpoint) -> io::Result<()> {
    let endpoint = endpoint.clone();
    std::thread::Builder::new()
        .name("soldr-broker-main".into())
        .spawn(move || serve_on_runtime_thread(&endpoint))
        .map_err(|error| {
            io::Error::other(format!("could not spawn broker runtime thread: {error}"))
        })?
        .join()
        .map_err(|_| io::Error::other("broker runtime thread panicked"))?
}

fn serve_on_runtime_thread(
    endpoint: &crate::broker_identity::ResolvedBrokerEndpoint,
) -> io::Result<()> {
    // soldr#2493 follow-up: every step between the caller's `binding stable
    // endpoint` line and the `stable endpoint bound at` line below is timed
    // and reported as it completes. A broker that stalls in cold start now
    // names the phase it stalled in instead of going silent.
    use crate::broker_bringup::phase;
    // Start the clock before the first phase, but open the durable log only
    // after the broker directory has been secured: the recorder's `open_append`
    // would otherwise create that directory itself, at the default umask.
    let bringup_started = Instant::now();

    endpoint
        .create_owner_only_directories()
        .map_err(io::Error::other)?;
    if let Some(diagnostic) = endpoint.fallback_diagnostic() {
        eprintln!("soldr broker: {diagnostic}");
    }
    let mut bringup = crate::broker_bringup::BringupRecorder::resuming(
        bringup_started,
        endpoint.executable_path.parent(),
    );
    bringup.phase(phase::SECURE_DIRECTORIES);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("soldr-broker")
        .build()?;
    bringup.phase(phase::TOKIO_RUNTIME);
    let peer_policy = PeerCredentialPolicy::current_user()
        .ok_or_else(|| io::Error::other("current-user broker peer policy is unavailable"))?;
    bringup.phase(phase::PEER_POLICY);
    // Split out of the state construction below: this is the only phase that
    // can block on another process (the image-hash lock), so it must be
    // attributable on its own rather than folded into `broker_state`.
    let instance_id = broker_server_instance_id()?;
    bringup.phase(phase::INSTANCE_ID);
    let state = Arc::new(BrokerState {
        instance_id,
        loader: CombinedServiceDefinitionLoader::new(
            running_process::broker::server::service_definition_dir(),
        ),
        registry: Arc::new(Mutex::new(BackendRegistry::new())),
        spawn_coordinator: Mutex::new(SpawnCoordinator::new()),
        launcher: crate::broker_launcher::SoldrBackendLauncher::new(),
        started_at: Instant::now(),
        connections_open: AtomicU64::new(0),
        fd_guard: FdPressureGuard::default(),
        route_owners: Arc::new(Mutex::new(crate::broker_reaper::RouteOwnership::new())),
    });
    bringup.phase(phase::BROKER_STATE);
    let runtime_context = runtime.enter();
    let listener = bind_listener(endpoint)?;
    drop(runtime_context);
    bringup.phase(phase::BIND_LISTENER);
    println!(
        "soldr broker: stable endpoint bound at {} (admin+hello+session)",
        endpoint.bind_endpoint
    );
    runtime.block_on(serve_loop(
        listener,
        state,
        Arc::new(peer_policy),
        endpoint.bind_endpoint.clone(),
        endpoint.executable_path.clone(),
    ))
}

async fn serve_loop(
    listener: interprocess::local_socket::tokio::Listener,
    state: Arc<BrokerState>,
    peer_policy: Arc<PeerCredentialPolicy>,
    endpoint: String,
    executable: std::path::PathBuf,
) -> io::Result<()> {
    // soldr#3158: a broadcast, latching signal — not a bare `Notify`.
    // Three loops park on this one signal (this accept loop, the route
    // reaper, the RSS watchdog), and `Notify::notify_one()` wakes exactly
    // one of them. The reaper registers first, so it consumed every
    // cooperative SHUTDOWN and the accept loop below never broke: each
    // `soldr broker stop` burned its full drain deadline and force-killed
    // a broker that had accepted the request and simply never heard it.
    let shutdown = Arc::new(crate::daemon::shutdown_signal::ShutdownSignal::default());
    let claim = crate::broker_endpoint_owner::EndpointClaim::shared(&endpoint);
    let reaper = tokio::spawn(crate::broker_reaper::run_route_reaper(
        Arc::clone(&state.route_owners),
        Arc::clone(&state.registry),
        (Arc::clone(&shutdown), Arc::clone(&claim)),
    ));
    // soldr#3075: a broker whose install directory was deleted (a disposable
    // HOME) can never be reached again, so it stands itself down.
    let home_watch = tokio::spawn(crate::broker_image_watch::run_home_watch(
        Arc::clone(&shutdown),
        crate::broker_image_watch::BROKER_HOME_CHECK_INTERVAL,
        move || crate::broker_image_watch::install_directory_present(&executable),
    ));
    // soldr#3193: a broker nobody uses for the idle window stands down.
    let idle_state = Arc::clone(&state);
    let idle_standdown =
        crate::broker_idle::spawn_standdown(&shutdown, move || idle_state.is_idle());
    // soldr#3057: the broker watches its own RSS against the same
    // SOLDR_DAEMON_RSS_CEILING_BYTES ceiling the daemon uses, rather than
    // cross-checking a specific daemon's breach.
    //
    // A host audit alongside the daemon-side leak found 180 `soldr-broker`
    // processes independently holding 3.61 GiB -- the broker is a
    // long-lived process in its own right and can balloon with no daemon
    // ever breaching at all, so watching only "did my daemon breach" would
    // miss the finding that motivated this in the first place. Reacting to
    // a specific daemon's breach instead (or in addition) would need the
    // broker to map a `BackendRegistry` route back to that route's
    // `SoldrPaths` and poll its `rss-ceiling-v1.json` -- solvable, but it
    // would lean on exactly the Child-handle/zombie-reaping plumbing the
    // companion investigation (soldr#3057's second finding) found broken:
    // the broker never retains a waitable handle on a daemon it spawned
    // past `launch()` returning, so there is no clean "this daemon just
    // exited because of a breach" signal to hang a reaction off yet. Self-
    // monitoring needs none of that: it reuses this exact, already-tested
    // sample/dump/die primitive unmodified, with no new cross-process
    // plumbing. This is a real gap -- a broker serving several healthy
    // routes will not exit just because one of their daemons breaches --
    // and is left as follow-up once the broker-population/zombie-reaping
    // design question (not in scope here) is settled.
    let rss_ceiling_bytes = crate::daemon::rss_ceiling::ceiling_bytes_from_env();
    crate::daemon::rss_ceiling::start_sampled_profiler_if_configured(rss_ceiling_bytes);
    let rss_watchdog = rss_ceiling_bytes.map(|ceiling_bytes| {
        let watchdog_paths = crate::daemon::service_definition::broker_owned_paths();
        let watchdog_shutdown = Arc::clone(&shutdown);
        tokio::spawn(crate::daemon::rss_ceiling::run_watchdog(
            watchdog_paths,
            watchdog_shutdown,
            ceiling_bytes,
            crate::daemon::rss_ceiling::ProcessRole::Broker,
        ))
    });
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            biased;
            () = shutdown.wait() => break,
            accepted = listener.accept() => {
                let stream = accepted?;
                let peer = match running_process::broker::server::connection::peer_identity_from_tokio_stream(&stream) {
                    Ok(peer) => peer,
                    Err(error) => {
                        eprintln!("soldr broker: rejected peer without credentials: {error}");
                        continue;
                    }
                };
                if !peer_policy.allows(&peer) {
                    eprintln!("soldr broker: rejected foreign peer pid={}", peer.pid);
                    continue;
                }
                state.connections_open.fetch_add(1, Ordering::Relaxed);
                let state = Arc::clone(&state);
                let shutdown = Arc::clone(&shutdown);
                connections.spawn(async move {
                    let _open = OpenConnection(Arc::clone(&state));
                    if let Err(error) = handle_connection(stream, peer, state, shutdown).await {
                        eprintln!("soldr broker: connection ended: {error}");
                    }
                });
            }
            Some(joined) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = joined {
                    eprintln!("soldr broker: connection task failed: {error}");
                }
            }
        }
    }

    // Stop admission, then let already-negotiated same-connection proxies
    // drain. A bounded stop command may still terminate this process if a
    // client never closes.
    drop(listener);
    // The reaper watches for shutdown too, but abort rather than await it: it
    // may be mid-sleep, and a retiring broker must not wait out a sweep.
    reaper.abort();
    home_watch.abort();
    // Same reasoning as the reaper: a retiring broker must not wait out an
    // RSS_SAMPLE_INTERVAL sleep to finish shutting down.
    for handle in [rss_watchdog, idle_standdown].into_iter().flatten() {
        handle.abort();
    }
    claim.retire(); // soldr#3815: never a successor's socket
    while let Some(joined) = connections.join_next().await {
        if let Err(error) = joined {
            eprintln!("soldr broker: draining connection task failed: {error}");
        }
    }
    Ok(())
}

#[expect(clippy::cognitive_complexity, reason = "baseline, zackees/ci.yml#229")]
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
async fn handle_connection(
    mut stream: interprocess::local_socket::tokio::Stream,
    peer: running_process::broker::server::PeerIdentity,
    state: Arc<BrokerState>,
    shutdown: Arc<crate::daemon::shutdown_signal::ShutdownSignal>,
) -> io::Result<()> {
    let deadlines = BrokerDeadlines::from_env();
    let body = tokio::time::timeout(deadlines.first_response, read_frame_async(&mut stream))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "broker first frame deadline exceeded",
            )
        })??;
    let request = Frame::decode(body.as_slice())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

    if request.payload_protocol == DAEMON_CONTROL_PAYLOAD_PROTOCOL {
        return handle_daemon_control_tunnel(stream, request, state).await;
    }

    if request.payload_protocol == ADMIN_PAYLOAD_PROTOCOL {
        let admin = AdminRequest::decode(request.payload.as_slice())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let response =
            running_process::broker::server::admin::handle_admin_frame(request, &state.snapshot())
                .map_err(io::Error::other)?;
        write_frame_async(&mut stream, &response).await?;
        if AdminVerb::try_from(admin.verb) == Ok(AdminVerb::Shutdown) {
            request_shutdown(&shutdown);
        }
        return Ok(());
    }

    if request.payload_protocol != CONTROL_PAYLOAD_PROTOCOL {
        let reply = refused(
            ErrorCode::ErrorPeerRejected,
            "the first broker frame must be Hello or admin",
            0,
        );
        write_hello_reply(&mut stream, &request, &reply).await?;
        return Ok(());
    }
    if let Some(reply) = validate_client_host(&request) {
        write_hello_reply(&mut stream, &request, &reply).await?;
        return Ok(());
    }
    let hello = Hello::decode(request.payload.as_slice())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

    let route_started = Instant::now();
    let route_ceiling = tokio::time::Instant::now() + deadlines.route_ceiling;
    state.launcher.note_route_deadline(
        &hello.service_name,
        std::time::Instant::now() + deadlines.route_ceiling,
    );
    let mut observed_progress = state.launcher.subscribe_progress();
    let mut latest_route_result = "broker accepted the route request".to_string();
    let heartbeat_interval = route_progress_heartbeat_interval(deadlines.progress_silence);
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + heartbeat_interval,
        heartbeat_interval,
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    write_progress(
        &mut stream,
        &request,
        RouteProgress {
            stage: "route-request".into(),
            attempt: 1,
            elapsed_ms: 0,
            latest_result: latest_route_result.clone(),
            retry_after_ms: 0,
        },
    )
    .await?;
    // soldr#3054: record who asked, before the route is acquired rather than
    // after, so a caller that dies mid-acquisition still leaves the route
    // attributable and therefore reapable.
    crate::broker_reaper::record_route_request(&state.route_owners, &hello.service_name, peer.pid);

    let mut attempt = 0_u32;
    let reply = 'acquire: loop {
        attempt = attempt.saturating_add(1);
        let route_state = Arc::clone(&state);
        let route_frame = request.clone();
        let route_peer = peer.clone();
        let mut route =
            tokio::task::spawn_blocking(move || route_state.route(route_frame, route_peer));
        let reply = loop {
            tokio::select! {
                joined = &mut route => {
                    break joined.map_err(|error| io::Error::other(format!("route worker failed: {error}")))?;
                }
                progress = observed_progress.recv() => {
                    match progress {
                        Ok(progress) if progress.service_name == hello.service_name => {
                            latest_route_result = progress.latest_result.clone();
                            write_progress(&mut stream, &request, RouteProgress {
                                stage: progress.stage.into(),
                                attempt,
                                elapsed_ms: route_started.elapsed().as_millis() as u64,
                                latest_result: progress.latest_result,
                                retry_after_ms: 0,
                            }).await?;
                        }
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return Err(io::Error::other("broker launcher progress channel closed"));
                        }
                    }
                }
                _ = heartbeat.tick() => {
                    write_progress(&mut stream, &request, RouteProgress {
                        stage: "route-wait".into(),
                        attempt,
                        elapsed_ms: route_started.elapsed().as_millis() as u64,
                        latest_result: latest_route_result.clone(),
                        retry_after_ms: 0,
                    }).await?;
                }
                _ = tokio::time::sleep_until(route_ceiling) => {
                    route.abort();
                    break 'acquire refused(
                        ErrorCode::ErrorBackendSpawnFailed,
                        format!(
                            "route acquisition exceeded its hard ceiling after {} attempts; latest result: {}",
                            attempt, latest_route_result
                        ),
                        0,
                    );
                }
            }
        };
        let Some((latest_result, retry_after_ms)) = retryable_route_refusal(&reply) else {
            break reply;
        };
        latest_route_result = latest_result.clone();
        let delay = route_retry_delay(effective_retry_after_ms(&latest_result, retry_after_ms));
        write_progress(
            &mut stream,
            &request,
            RouteProgress {
                stage: "single-flight-wait".into(),
                attempt,
                elapsed_ms: route_started.elapsed().as_millis() as u64,
                latest_result,
                retry_after_ms: delay.as_millis() as u64,
            },
        )
        .await?;
        let retry_deadline = tokio::time::Instant::now() + delay;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(retry_deadline) => break,
                _ = heartbeat.tick() => {
                    write_progress(&mut stream, &request, RouteProgress {
                        stage: "single-flight-wait".into(),
                        attempt,
                        elapsed_ms: route_started.elapsed().as_millis() as u64,
                        latest_result: latest_route_result.clone(),
                        retry_after_ms: retry_deadline
                            .saturating_duration_since(tokio::time::Instant::now())
                            .as_millis() as u64,
                    }).await?;
                }
                _ = tokio::time::sleep_until(route_ceiling) => {
                    break 'acquire refused(
                        ErrorCode::ErrorBackendSpawnFailed,
                        format!(
                            "route acquisition exceeded its hard ceiling after {} attempts; latest result: {}",
                            attempt, latest_route_result
                        ),
                        0,
                    );
                }
            }
        }
    };
    write_hello_reply(&mut stream, &request, &reply).await?;
    let Some(hello_reply::Result::Negotiated(negotiated)) = reply.result.as_ref() else {
        return Ok(());
    };
    if negotiated.backend_pipe.is_empty() {
        return Err(io::Error::other("negotiated route has no daemon endpoint"));
    }

    if try_direct_handoff(&mut stream, &state, &hello.service_name, negotiated, &reply).await? {
        return Ok(());
    }

    write_handoff_fallback(&mut stream, &request, negotiated).await?;

    // Portable fallback: no reconnect. The exact accepted stream that carried
    // Hello now carries SessionStart and the complete compile exchange.
    running_process::broker::session_relay::relay_session(stream, &negotiated.backend_pipe).await
}

/// Latch the stop request and wake every loop parked on it (soldr#3158).
///
/// Both hazards are real here and pull in opposite directions, which is why
/// this goes through [`ShutdownSignal`] rather than a bare `Notify`: the
/// broker has several waiters, so `notify_one()` reaches only one of them,
/// while a handler that wins the scheduling race can signal before a loop
/// first polls, which `notify_waiters()` alone would drop. The latched flag
/// covers the second, broadcasting covers the first.
///
/// [`ShutdownSignal`]: crate::daemon::shutdown_signal::ShutdownSignal
fn request_shutdown(shutdown: &crate::daemon::shutdown_signal::ShutdownSignal) {
    shutdown.request();
}

async fn handle_daemon_control_tunnel(
    mut stream: interprocess::local_socket::tokio::Stream,
    request_frame: Frame,
    state: Arc<BrokerState>,
) -> io::Result<()> {
    let request = DaemonControlTunnelRequest::decode(request_frame.payload.as_slice())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let local = running_process::broker::host_identity::current();
    let requested_service = request.service_name.clone();
    let existing_endpoint =
        if request.machine_id == local.machine_id && request.boot_id == local.boot_id {
            tokio::task::spawn_blocking(move || {
                state.private_control_endpoint_for_service(&requested_service)
            })
            .await
            .map_err(|error| io::Error::other(format!("control adoption worker failed: {error}")))?
        } else {
            None
        };
    let (reply, endpoint) = if request.machine_id != local.machine_id
        || request.boot_id != local.boot_id
    {
        (
            DaemonControlTunnelReply {
                accepted: false,
                error_detail: "shared Soldr home points at a broker from another machine or boot; use a machine-local home".into(),
                not_running: false,
            },
            None,
        )
    } else if let Some(endpoint) = existing_endpoint {
        (
            DaemonControlTunnelReply {
                accepted: true,
                error_detail: String::new(),
                not_running: false,
            },
            Some(endpoint),
        )
    } else {
        (
            DaemonControlTunnelReply {
                accepted: false,
                error_detail: format!(
                    "daemon route {} is not running in the broker registry",
                    request.service_name
                ),
                not_running: true,
            },
            None,
        )
    };
    write_frame_async(
        &mut stream,
        &Frame {
            envelope_version: PROTOCOL_VERSION,
            kind: FrameKind::Response as i32,
            payload_protocol: DAEMON_CONTROL_PAYLOAD_PROTOCOL,
            payload: reply.encode_to_vec(),
            request_id: request_frame.request_id,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: request_frame.traceparent,
            tracestate: request_frame.tracestate,
        },
    )
    .await?;
    let Some(endpoint) = endpoint else {
        return Ok(());
    };
    running_process::broker::session_relay::relay_session(stream, &endpoint).await
}

fn retryable_route_refusal(reply: &HelloReply) -> Option<(String, u64)> {
    let Some(hello_reply::Result::Refused(refused)) = reply.result.as_ref() else {
        return None;
    };
    (ErrorCode::try_from(refused.code) == Ok(ErrorCode::ErrorRateLimited)
        && refused.retry_after_ms > 0)
        .then(|| (refused.reason.clone(), refused.retry_after_ms))
}

fn route_retry_delay(retry_after_ms: u64) -> Duration {
    let ceiling = retry_after_ms.clamp(5, 1_000);
    let mut random = [0_u8; 8];
    let value = if getrandom::fill(&mut random).is_ok() {
        u64::from_le_bytes(random)
    } else {
        0
    };
    Duration::from_millis(5 + value % (ceiling - 4))
}

async fn write_handoff_fallback(
    stream: &mut interprocess::local_socket::tokio::Stream,
    request: &Frame,
    negotiated: &running_process::broker::protocol::Negotiated,
) -> io::Result<()> {
    use running_process::broker::capabilities::CAP_HANDLE_PASSING;

    if negotiated.server_capabilities & CAP_HANDLE_PASSING == 0
        || negotiated.handle_passed_token.is_empty()
    {
        return Ok(());
    }
    let ack = running_process::broker::protocol::HandoffAck {
        token: negotiated.handle_passed_token.clone(),
        accepted: false,
        error_detail: "broker retained the accepted connection for proxy fallback".into(),
        correlation_id: negotiated.connection_id,
    };
    write_frame_async(
        stream,
        &Frame {
            envelope_version: PROTOCOL_VERSION,
            kind: FrameKind::Event as i32,
            payload_protocol: running_process::broker::protocol::HANDOFF_PAYLOAD_PROTOCOL,
            payload: ack.encode_to_vec(),
            request_id: request.request_id,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: request.traceparent.clone(),
            tracestate: request.tracestate.clone(),
        },
    )
    .await
}

async fn try_direct_handoff(
    stream: &mut interprocess::local_socket::tokio::Stream,
    state: &Arc<BrokerState>,
    service_name: &str,
    negotiated: &running_process::broker::protocol::Negotiated,
    reply: &HelloReply,
) -> io::Result<bool> {
    if !direct_handoff_eligible(
        negotiated.server_capabilities,
        &negotiated.handle_passed_token,
    ) {
        return Ok(false);
    }

    #[cfg(debug_assertions)]
    if crate::core::flag(crate::broker_debug::TEST_DISABLE_HANDOFF_ENV_VAR) {
        return Ok(false);
    }
    let Some(instance) = state.instance_for_route(
        service_name,
        &negotiated.daemon_version,
        &negotiated.backend_pipe,
    ) else {
        return Ok(false);
    };
    let Ok(mut handoff_client) = duplicate_handoff_stream(stream) else {
        return Ok(false);
    };
    let state = Arc::clone(state);
    let service_name = service_name.to_string();
    let service_version = negotiated.daemon_version.clone();
    let handoff_endpoint =
        crate::daemon::session_endpoint::handoff_endpoint_path(&negotiated.backend_pipe);
    let reply = reply.clone();
    let transferred = tokio::task::spawn_blocking(move || {
        let context = ServeHandoffContext {
            handoff_endpoint: &handoff_endpoint,
            service_name: &service_name,
            service_version: &service_version,
            instance: &instance,
            registry: &state.registry,
        };
        running_process::broker::server::try_transfer_negotiated_handoff(
            &context,
            &mut handoff_client,
            &reply,
        )
    })
    .await
    .map_err(|error| io::Error::other(format!("broker handoff worker failed: {error}")))?;
    if !transferred {
        return Ok(false);
    }
    let ack = running_process::broker::protocol::HandoffAck {
        token: negotiated.handle_passed_token.clone(),
        accepted: true,
        error_detail: String::new(),
        correlation_id: negotiated.connection_id,
    };
    write_handoff_ready_async(stream, &ack).await.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("backend accepted broker handoff but the ready event could not be delivered: {error}"),
        )
    })?;
    Ok(true)
}

fn direct_handoff_eligible(server_capabilities: u64, token: &[u8]) -> bool {
    use running_process::broker::capabilities::CAP_HANDLE_PASSING;

    // A duplicated Windows named-pipe handle can be converted and ACKed by
    // the daemon, yet the adopted SESSION receives ERROR_BROKEN_PIPE as soon
    // as the broker drops its original accepted handle. Keep Windows on the
    // same-connection relay, which preserves the protocol and ownership
    // contract without exposing a client-visible false-positive handoff.
    crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows
        && server_capabilities & CAP_HANDLE_PASSING != 0
        && !token.is_empty()
}

async fn write_handoff_ready_async(
    stream: &mut interprocess::local_socket::tokio::Stream,
    ack: &running_process::broker::protocol::HandoffAck,
) -> io::Result<()> {
    let frame = running_process::broker::server::handoff_ready_frame(ack);
    write_frame_async(stream, &frame).await
}

fn duplicate_handoff_stream(
    stream: &interprocess::local_socket::tokio::Stream,
) -> io::Result<interprocess::local_socket::Stream> {
    crate::platform::ipc::broker::duplicate_stream(stream)
}

fn validate_client_host(request: &Frame) -> Option<HelloReply> {
    let hello = match Hello::decode(request.payload.as_slice()) {
        Ok(hello) => hello,
        Err(_) => {
            return Some(refused(
                ErrorCode::ErrorPeerRejected,
                "malformed broker Hello",
                0,
            ));
        }
    };
    if hello.peer_attestation_nonce.is_empty() {
        return Some(refused(
            ErrorCode::ErrorPeerRejected,
            "broker Hello omitted machine/boot attestation for the shared home",
            0,
        ));
    }
    let client = match ClientHostAttestation::decode(hello.peer_attestation_nonce.as_slice()) {
        Ok(attestation) => attestation,
        Err(_) => {
            return Some(refused(
                ErrorCode::ErrorPeerRejected,
                "soldr Hello carried a malformed machine/boot attestation",
                0,
            ));
        }
    };
    let local = running_process::broker::host_identity::current();
    if client.machine_id != local.machine_id || client.boot_id != local.boot_id {
        let mut reply = refused(
            ErrorCode::ErrorPeerRejected,
            "shared Soldr home points at a broker from another machine or boot; use a machine-local home",
            0,
        );
        if let Some(hello_reply::Result::Refused(ref mut detail)) = reply.result {
            detail
                .details
                .insert("client_machine_id".into(), client.machine_id);
            detail
                .details
                .insert("broker_machine_id".into(), local.machine_id);
            detail
                .details
                .insert("client_boot_id".into(), client.boot_id);
            detail
                .details
                .insert("broker_boot_id".into(), local.boot_id);
        }
        return Some(reply);
    }
    None
}

fn refused(code: ErrorCode, reason: impl Into<String>, retry_after_ms: u64) -> HelloReply {
    HelloReply {
        result: Some(hello_reply::Result::Refused(Refused {
            reason: reason.into(),
            daemon_min_protocol: PROTOCOL_VERSION,
            daemon_max_protocol: PROTOCOL_VERSION,
            code: code as i32,
            details: Default::default(),
            retry_after_ms,
        })),
    }
}

include!("broker_server_io.rs");
#[cfg(test)]
#[path = "broker_server_tests.rs"]
mod tests;
