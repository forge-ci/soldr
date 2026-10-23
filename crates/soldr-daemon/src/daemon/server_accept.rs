// Control-endpoint accept loops, split from `server_runtime.rs` for the
// soldr#2493 1,000-line production-source ceiling. `include!`d into
// `server.rs`, so it shares that module's imports.

/// What a control connection needs before the daemon's [`State`] exists
/// (soldr#3169).
///
/// The control endpoint is claimed at the start of bringup, for fencing, but
/// `State` -- the compile service and state store -- is ready seconds later.
/// The accept loop used to start only once `State` did, so a connection in
/// that window sat unserviced in the listen backlog: a target touch waited out
/// its 200 ms receipt-ack bound and reported "delivery is unconfirmed" for a
/// touch that was delivered moments later. Serving from the claim onwards, with
/// each request parked on this cell until `State` is published, acknowledges
/// receipt at once while everything that needs state still waits for it.
#[derive(Clone)]
struct ControlContext {
    daemon_identity: running_process::broker::backend_handle::DaemonProcess,
    shutdown: Arc<crate::daemon::maintenance::ShutdownSignal>,
    state: tokio::sync::watch::Receiver<Option<Arc<State>>>,
    /// Permits for control requests being served; see
    /// [`MAX_CONTROL_REQUESTS_IN_FLIGHT`].
    in_flight: Arc<tokio::sync::Semaphore>,
}

/// soldr#3839: a wedged daemon held 542 control sockets, one parked task per
/// client that gave up. Past this many requests in flight the daemon answers
/// with a loud error instead of parking another. Control requests are short,
/// so a healthy daemon never comes near it; resident-capacity leases are
/// long-lived by design and are not counted.
const MAX_CONTROL_REQUESTS_IN_FLIGHT: usize = 256;

impl ControlContext {
    fn new(
        daemon_identity: running_process::broker::backend_handle::DaemonProcess,
        shutdown: Arc<crate::daemon::maintenance::ShutdownSignal>,
        state: tokio::sync::watch::Receiver<Option<Arc<State>>>,
    ) -> Self {
        Self {
            daemon_identity,
            shutdown,
            state,
            in_flight: Arc::new(tokio::sync::Semaphore::new(
                MAX_CONTROL_REQUESTS_IN_FLIGHT,
            )),
        }
    }

    /// The daemon's state once bringup publishes it, or `None` when bringup
    /// failed and the publisher is gone.
    async fn state(&mut self) -> Option<Arc<State>> {
        self.state
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|state| (*state).clone())
    }
}

/// The control accept task, aborted when dropped: a bringup that returns early
/// must not leave an accept loop holding the endpoint it claimed.
struct AcceptTask(tokio::task::JoinHandle<()>);

impl AcceptTask {
    fn abort(&self) {
        self.0.abort();
    }
}

impl Drop for AcceptTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Accept loop for the Unix control endpoint. `listener` is the claimed
/// filesystem socket from the platform listener leaf, which already
/// resolved each peer's identity and current-user admission during
/// accept.
async fn run_accept_loop_unix(
    listener: crate::platform::ipc::listener::BoxedControlListener,
    control: ControlContext,
) -> std::io::Result<()> {
    loop {
        let accepted = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(_) => continue,
        };
        if !accepted.peer.is_current_user {
            tracing::warn!(target: "soldr::daemon", "rejected foreign daemon-control peer");
            continue;
        }
        let peer = crate::daemon::ipc_peer::PeerIdentity::from_accepted_peer(&accepted.peer);
        let control = control.clone();
        tokio::spawn(async move {
            let _ = handle_connection(accepted.stream, control, peer).await;
        });
    }
}

/// Accept loop for the Windows control endpoint: a self-replenishing
/// named-pipe instance pool (see [`accept_windows_pipe_instance`]).
async fn run_accept_loop_windows(paths: SoldrPaths, control: ControlContext) -> std::io::Result<()> {
    // soldr#1808: identity failure is fatal here — this loop *is* the
    // endpoint. Propagating beats a fallback name no client would dial.
    let pipe_name = crate::daemon::session_endpoint::resolved_control_endpoint_path(&paths)?
        .to_string_lossy()
        .into_owned();
    let pool_size = windows_listener_pool_size();
    tracing::info!(
        pool_size,
        "soldr-daemon Windows named-pipe listener pool ready"
    );
    for index in 0..pool_size {
        spawn_windows_pipe_instance(pipe_name.clone(), control.clone(), index == 0);
    }
    // Park until shutdown rather than forever. The pool instances are
    // detached and self-replenishing, so aborting this task cannot stop
    // them — each instance observes the same signal and drops its own pipe
    // handle. Returning here is what lets the caller's `.await` complete.
    control.shutdown.wait().await;
    Ok(())
}

fn spawn_windows_pipe_instance(
    pipe_name: String,
    control: ControlContext,
    first_pipe_instance: bool,
) {
    // Keep this launcher synchronous. Calling `tokio::spawn` directly from
    // `accept_windows_pipe_instance` would make the async function's opaque
    // future recursively depend on itself, which Windows rejects because its
    // `Send` bound cannot be inferred.
    tokio::spawn(async move {
        if let Err(error) =
            accept_windows_pipe_instance(pipe_name, control, first_pipe_instance).await
        {
            tracing::debug!(%error, "Windows named-pipe listener exited");
        }
    });
}

async fn accept_windows_pipe_instance(
    pipe_name: String,
    control: ControlContext,
    first_pipe_instance: bool,
) -> std::io::Result<()> {
    // Never open a fresh instance once teardown has begun; that would
    // re-arm the endpoint the shutdown path is trying to retire.
    if control.shutdown.is_requested() {
        return Ok(());
    }
    let mut server = crate::platform::ipc::peer::create_owner_only_windows_pipe(
        &pipe_name,
        first_pipe_instance,
    )?;

    // Drop the instance as soon as shutdown starts. Otherwise a wrapper can
    // connect after the compile service has latched shut. Unix drops its
    // listener with the accept task; this gives Windows the same fallback.
    let connected = tokio::select! {
        result = crate::platform::ipc::peer::pipe_server_connect(&mut server) => result.is_ok(),
        _ = control.shutdown.wait() => return Ok(()),
    };

    if control.shutdown.is_requested() {
        return Ok(());
    }
    if connected {
        // Replenish before parsing the connected request, keeping the pool
        // admission capacity independent from compile execution throughput.
        spawn_windows_pipe_instance(pipe_name, control.clone(), false);
        let peer = PeerIdentity::from_windows_pipe_server(&mut server);
        let _ = handle_connection(server, control, peer).await;
    } else {
        spawn_windows_pipe_instance(pipe_name, control, false);
    }
    Ok(())
}
