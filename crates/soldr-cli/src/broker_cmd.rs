//! Dispatch for `soldr broker <verb>` (soldr#2361 Phase 2). The front door
//! spawns `soldr broker serve` as its sole allowlisted spawn exception
//! (unconditional — soldr#2388, see `broker_spawn.rs`); manual invocation
//! remains supported and safe.
//!
//! Split out of `soldr_main.rs` on introduction rather than grown inline
//! there: that file is already over the repo's 1,500-line ratchet
//! (`.github/scripts/loc_ratchet.py`), which only allows a file already
//! over the ceiling to shrink, never grow. New dispatch surfaces belong in
//! their own module from the start, matching `daemon_entry.rs`,
//! `cache.rs`, etc.

use std::time::{Duration, Instant};

use crate::core::SoldrError;
use crate::exit_guard::{self, guarded_exit};

/// Verbs for `soldr broker` (soldr#2361 Phase 2). Declared here rather than
/// in `cli_args.rs` (already over the repo's loc_ratchet ceiling) even
/// though `Commands::Broker` still lives there -- clap's derive works fine
/// across the module boundary as long as this type is in scope.
///
/// This is a new dispatch surface, not an argv[0] shim identity like
/// `soldr-daemon` (see `multicall.rs`'s `ShimIdentity`) -- nothing external
/// ever needs to find a broker process by a conventional binary name, only
/// soldr's own front door spawns it, so a plain subcommand is simpler than
/// installing a hardlinked shim (soldr#2364 design comment).
#[derive(clap::Subcommand)]
pub(crate) enum BrokerSubcommand {
    /// Bind the v2 broker socket and serve Hello connections, launching
    /// soldr-daemon on a verified registry miss. Blocks until the process
    /// is killed or (future work) an idle/displacement policy exits it.
    ///
    /// Spawned unconditionally by the front door (soldr#2388, see
    /// `broker_spawn.rs`); running it manually is also safe -- it enforces
    /// the same per-user-session singleton property
    /// `running-process-broker-v2` does, refusing to start a second
    /// instance rather than racing one.
    Serve,
    /// Query the running broker over its control socket and print an admin
    /// status snapshot (owned routes, uptime, versions). Prints a clean
    /// "not running" line and exits 0 when no broker is bound (soldr#2442
    /// slice 2). Read-only: it never starts a broker.
    Status {
        /// Emit the broker's JSON status payload instead of its text render.
        #[arg(long)]
        json: bool,
    },
    /// List the broker's verified in-memory daemon routes. This is read-only
    /// and never resurrects either broker or daemon.
    Routes {
        /// Emit the stable schema_version=1 JSON representation.
        #[arg(long)]
        json: bool,
    },
    /// Stop the running broker without stopping its daemon routes. A replacement
    /// broker re-adopts those daemons from their verified claims. With no broker
    /// bound this prints a "not running" line and exits 0.
    Stop,
    /// Deliberately retire the broker installed for this Soldr home (soldr#2549).
    ///
    /// The broker is a stable, long-lived singleton: Soldr never stops, kills,
    /// replaces, or stages over a live broker on its own, not even when the
    /// running image's version or digest differs. This is the explicit,
    /// operator-driven recovery the front door's mismatch warning names. It
    /// stops the verified broker process, unlinks its admission endpoint, and
    /// deletes the staged broker image so the next invocation installs a
    /// matching one. Daemon routes are retained and re-adopted from their
    /// verified claims. With no broker bound it prints a "not running" line and
    /// exits 0.
    Remove,
    /// Stop every soldr-broker / soldr-daemon process on this host that serves
    /// a HOME other than the current one (soldr#3193). Test suites run soldr
    /// under a throwaway HOME per test and leave one broker each behind; this
    /// is the operator-driven sweep `soldr doctor` and the front-door notice
    /// point at. The broker for the current HOME is never touched. Each
    /// process gets a terminate request first and is force-killed only if it
    /// is still alive after the drain deadline.
    Purge {
        /// List what would be stopped without signalling anything.
        #[arg(long)]
        dry_run: bool,
        /// Emit the stable schema_version=1 JSON report instead of text.
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn run_broker_command(command: BrokerSubcommand) -> Result<(), SoldrError> {
    match command {
        BrokerSubcommand::Serve => run_broker_serve(),
        BrokerSubcommand::Status { json } => run_broker_status(json),
        BrokerSubcommand::Routes { json } => run_broker_routes(json),
        BrokerSubcommand::Stop => run_broker_stop(),
        BrokerSubcommand::Remove => run_broker_remove(),
        BrokerSubcommand::Purge { dry_run, json } => {
            crate::broker_purge::run_broker_purge(dry_run, json)
        }
    }
}

fn run_broker_routes(json: bool) -> Result<(), SoldrError> {
    use running_process::broker::client::{send_admin_request, BrokerClientError};
    use running_process::broker::protocol::{AdminRequest, AdminVerb};

    let socket_path = broker_control_socket_path()?;
    let reply = match send_admin_request(
        &socket_path,
        AdminRequest {
            verb: AdminVerb::Status as i32,
            json: true,
            ..Default::default()
        },
    ) {
        Ok(reply) => reply,
        Err(BrokerClientError::BrokerConnect(_)) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"schema_version": 1, "running": false, "routes": []})
                );
            } else {
                println!("soldr broker: not running (no routes)");
            }
            return Ok(());
        }
        Err(error) => {
            return Err(SoldrError::Other(format!(
                "soldr broker: routes query failed: {error}"
            )))
        }
    };
    let snapshot: serde_json::Value = serde_json::from_str(&reply.body).map_err(|error| {
        SoldrError::Other(format!("soldr broker: invalid routes snapshot: {error}"))
    })?;
    let routes = snapshot
        .get("backends")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
    if json {
        println!(
            "{}",
            serde_json::json!({
                "schema_version": 1,
                "running": true,
                "endpoint": socket_path,
                "routes": routes,
            })
        );
    } else if let Some(routes) = routes.as_array() {
        if routes.is_empty() {
            println!("soldr broker: no routes");
        }
        for route in routes {
            println!(
                "{} {} pid={} state={}",
                route
                    .get("service_name")
                    .and_then(|value| value.as_str())
                    .unwrap_or("?"),
                route
                    .get("service_version")
                    .and_then(|value| value.as_str())
                    .unwrap_or("?"),
                route
                    .get("pid")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0),
                route
                    .get("state")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown"),
            );
        }
    }
    Ok(())
}

/// Bounded deadline for `broker stop` to observe graceful exits before it
/// force-kills stragglers (soldr#2442). A single constant so implementation,
/// diagnostics, and tests agree on one number; env-overridable for tests,
/// mirroring the other bounded broker deadlines. The same value travels in the
/// cooperative `SHUTDOWN` request as `drain_deadline_ms`.
fn broker_stop_deadline() -> std::time::Duration {
    const DEFAULT_MS: u64 = 10_000;
    let ms = std::env::var("SOLDR_BROKER_DRAIN_DEADLINE_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MS)
        .max(1);
    std::time::Duration::from_millis(ms)
}

/// Bind the one stable v2 broker socket and serve Hello connections,
/// launching soldr-daemon on a verified registry miss.  The requesting soldr
/// front door registers the exact source image and route before Cargo starts;
/// the broker never guesses a sibling image from its own executable.
///
/// Soldr's accept loop handles admin, Hello/progress, and SESSION traffic on
/// this endpoint while reusing running-process's router, registry,
/// single-flight coordinator, peer validation, and launcher contracts.
fn run_broker_serve() -> Result<(), SoldrError> {
    // Every daemon this broker launches inherits its mask (soldr#3995).
    if let Some(reset) = crate::platform::process::affinity::widen_cpu_affinity() {
        if reset.after != reset.before {
            println!(
                "soldr broker: widened inherited CPU affinity from {} to {} CPU(s) (soldr#3995)",
                reset.before, reset.after
            );
        }
    }
    let endpoint = crate::broker_identity::ResolvedBrokerEndpoint::resolve()
        .map_err(|error| SoldrError::Other(format!("soldr broker: {error}")))?;
    println!(
        "soldr broker: binding stable endpoint {} (logical={})",
        endpoint.bind_endpoint, endpoint.logical_socket_path
    );
    match crate::broker_server::serve(&endpoint) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::AddrInUse
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            eprintln!(
                "soldr broker: another broker is already bound at {}; refusing a second owner",
                endpoint.bind_endpoint
            );
            exit_guard::mark_spoke();
            guarded_exit(75)
        }
        Err(error) => Err(SoldrError::Other(format!(
            "soldr broker: serve failed: {error}"
        ))),
    }
}

/// Resolve the one stable endpoint shared by admin and compile sessions.
fn broker_control_socket_path() -> Result<String, SoldrError> {
    crate::broker_identity::ResolvedBrokerEndpoint::resolve()
        .map(|endpoint| endpoint.bind_endpoint)
        .map_err(|error| SoldrError::Other(format!("soldr broker: {error}")))
}

/// `soldr broker status`: send one admin STATUS request to the running broker
/// and print its reply (soldr#2442 slice 2). A missing broker is not an error
/// — it prints a "not running" line and exits 0, mirroring
/// `soldr daemon stop`'s not-running convention — so scripts can probe without
/// starting anything. Read-only: this never binds or launches a broker.
fn run_broker_status(json: bool) -> Result<(), SoldrError> {
    use running_process::broker::client::{send_admin_request, BrokerClientError};
    use running_process::broker::protocol::{AdminRequest, AdminVerb};

    let socket_path = broker_control_socket_path()?;
    let request = AdminRequest {
        verb: AdminVerb::Status as i32,
        json,
        ..Default::default()
    };
    match send_admin_request(&socket_path, request) {
        Ok(reply) => {
            print!("{}", reply.body);
            if !reply.body.ends_with('\n') {
                println!();
            }
            Ok(())
        }
        // No broker bound at this socket: report cleanly and succeed, rather
        // than surfacing a transport error, so `broker status` is a safe probe.
        Err(BrokerClientError::BrokerConnect(_)) => {
            println!("soldr broker: not running (no broker bound at {socket_path})");
            Ok(())
        }
        Err(err) => Err(SoldrError::Other(format!(
            "soldr broker: status query failed: {err}"
        ))),
    }
}

/// Stop only the broker. Daemon lifetime is intentionally independent: a new
/// broker re-adopts each live daemon through its deterministic protobuf claim.
fn run_broker_stop() -> Result<(), SoldrError> {
    let socket_path = broker_control_socket_path()?;
    let VerifiedBrokerStop::Stopped { broker_pid, .. } =
        stop_verified_broker(&socket_path, "stop", None, None, true)?
    else {
        println!("soldr broker: not running (nothing to stop)");
        return Ok(());
    };
    println!("soldr broker: stopped (broker pid {broker_pid}; daemon routes retained)");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrokerShutdownPath {
    Cooperative,
    Terminated,
    Forced,
}

impl BrokerShutdownPath {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Cooperative => "cooperative",
            Self::Terminated => "terminated",
            Self::Forced => "forced",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerifiedBrokerStop {
    NotRunning,
    IdentityChanged,
    Stopped {
        broker_pid: u32,
        shutdown_path: BrokerShutdownPath,
    },
}

/// The complete, PID-verified broker teardown shared by explicit removal and
/// the soldr#2920 known-bad exception. An expected instance re-probes STATUS
/// immediately before signaling, so a winner can never retire a broker that
/// replaced the one whose compatibility it classified.
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn stop_verified_broker(
    socket_path: &str,
    operation: &str,
    expected_instance: Option<&str>,
    retirement_lease: Option<&crate::broker_lease::BrokerLease>,
    emit_diagnostics: bool,
) -> Result<VerifiedBrokerStop, SoldrError> {
    use running_process::broker::backend_lifecycle::verify_pid::{
        force_kill_pid, signal_terminate,
    };

    let Some(snapshot) = broker_status_snapshot(socket_path, operation)? else {
        return Ok(VerifiedBrokerStop::NotRunning);
    };
    if let Some(expected_instance) = expected_instance {
        if snapshot
            .get("broker_instance")
            .and_then(serde_json::Value::as_str)
            != Some(expected_instance)
        {
            return Ok(VerifiedBrokerStop::IdentityChanged);
        }
    }
    let broker_pid = snapshot
        .get("broker_pid")
        .and_then(serde_json::Value::as_u64)
        .filter(|pid| *pid != 0)
        .map(|pid| pid as u32);

    let Some(broker_pid) = broker_pid else {
        return Err(SoldrError::Other(format!(
            "soldr broker: the stable broker answered status but reported no pid; refusing to \
             {operation} it by any less-verified means"
        )));
    };
    let broker_start_token = verified_broker_generation(broker_pid).ok_or_else(|| {
        SoldrError::Other(format!(
            "soldr broker: pid {broker_pid} no longer names the responding stable broker; refusing to signal it"
        ))
    })?;

    // soldr#2442 Option B: prefer a cooperative drain. Ask the broker to shut
    // itself down gracefully — it stops admission, drains in-flight sessions as
    // the accept loop's worker scope joins, and exits. Brokers that predate the
    // SHUTDOWN verb answer exit_code 2 (or the request errors), and we fall back
    // to terminating the broker by its verified PID.
    maintain_retirement_lease(retirement_lease)?;
    let cooperative = try_cooperative_shutdown(socket_path);
    if cooperative && emit_diagnostics {
        eprintln!(
            "soldr broker: requested cooperative shutdown (drain deadline {}ms)",
            broker_stop_deadline().as_millis()
        );
    }

    // Stop only the verified broker PID. Daemons intentionally outlive the
    // broker and are re-adopted from their deterministic protobuf claims.
    let mut shutdown_path = BrokerShutdownPath::Cooperative;
    let mut terminate: Vec<(&str, u32)> = Vec::new();
    if !cooperative {
        shutdown_path = BrokerShutdownPath::Terminated;
        terminate.push(("broker", broker_pid));
    }
    for (kind, pid) in &terminate {
        maintain_retirement_lease(retirement_lease)?;
        if verified_broker_generation(*pid) != Some(broker_start_token) {
            continue;
        }
        if let Err(err) = signal_terminate(*pid) {
            if !emit_diagnostics {
                continue;
            }
            eprintln!("soldr broker: could not signal {kind} pid {pid} to terminate: {err}");
        }
    }

    // Watch the broker too, so a cooperative drain that overruns the deadline is
    // force-killed rather than left running.
    let watch: Vec<(&str, u32, u64)> = vec![("broker", broker_pid, broker_start_token)];
    let deadline = std::time::Instant::now() + broker_stop_deadline();
    loop {
        maintain_retirement_lease(retirement_lease)?;
        let alive: Vec<(&str, u32, u64)> = watch
            .iter()
            .copied()
            .filter(|(_, pid, token)| verified_broker_generation(*pid) == Some(*token))
            .collect();
        if alive.is_empty() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            shutdown_path = BrokerShutdownPath::Forced;
            for (kind, pid, token) in &alive {
                maintain_retirement_lease(retirement_lease)?;
                if emit_diagnostics {
                    eprintln!(
                        "soldr broker: {kind} pid {pid} did not exit within the stop deadline; \
                         force-killing"
                    );
                }
                if verified_broker_generation(*pid) == Some(*token) {
                    force_kill_pid(*pid).map_err(|error| {
                        SoldrError::Other(format!(
                            "soldr broker: could not force-kill verified {kind} pid {pid}: {error}"
                        ))
                    })?;
                }
            }
            let reap_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while std::time::Instant::now() < reap_deadline {
                maintain_retirement_lease(retirement_lease)?;
                if watch
                    .iter()
                    .all(|(_, pid, token)| verified_broker_generation(*pid) != Some(*token))
                {
                    return Ok(VerifiedBrokerStop::Stopped {
                        broker_pid,
                        shutdown_path,
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            return Err(SoldrError::Other(format!(
                "soldr broker: verified broker pid {broker_pid} survived forced {operation}; refusing endpoint/image cleanup"
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    Ok(VerifiedBrokerStop::Stopped {
        broker_pid,
        shutdown_path,
    })
}

/// One admin STATUS round trip, parsed. `Ok(None)` means nothing is bound —
/// every broker verb treats that as a clean, non-error outcome so scripts can
/// probe without starting anything.
fn broker_status_snapshot(
    socket_path: &str,
    operation: &str,
) -> Result<Option<serde_json::Value>, SoldrError> {
    use running_process::broker::client::{send_admin_request, BrokerClientError};
    use running_process::broker::protocol::{AdminRequest, AdminVerb};

    let reply = match send_admin_request(
        socket_path,
        AdminRequest {
            verb: AdminVerb::Status as i32,
            json: true,
            ..Default::default()
        },
    ) {
        Ok(reply) => reply,
        Err(BrokerClientError::BrokerConnect(_)) => return Ok(None),
        Err(err) => {
            return Err(SoldrError::Other(format!(
                "soldr broker: could not query broker before {operation}: {err}"
            )))
        }
    };
    serde_json::from_str(&reply.body).map(Some).map_err(|err| {
        SoldrError::Other(format!(
            "soldr broker: could not parse broker status snapshot before {operation}: {err}"
        ))
    })
}

/// `soldr broker remove` (soldr#2549): the explicit operator recovery for a
/// broker whose image or package version no longer matches the running Soldr.
///
/// Soldr never takes this action on its own — the front door only warns and
/// keeps using the live broker. Removal stops the PID-verified broker, unlinks
/// its admission endpoint, and deletes the staged broker image so the next
/// invocation stages a matching one. Daemon routes survive: they are keyed on
/// the daemon image and re-adopted from their verified claims by the next
/// broker.
fn run_broker_remove() -> Result<(), SoldrError> {
    let endpoint = crate::broker_identity::ResolvedBrokerEndpoint::resolve()
        .map_err(|error| SoldrError::Other(format!("soldr broker: {error}")))?;
    let socket_path = endpoint.bind_endpoint.clone();

    // Diagnostics before the destructive step, so an operator who ran this by
    // mistake can see exactly which generation was retired and why.
    match broker_status_snapshot(&socket_path, "remove")? {
        Some(snapshot) => {
            let observed = snapshot
                .get("broker_instance")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<unreported>");
            println!("soldr broker: removing the broker bound at {socket_path}");
            println!("soldr broker:   running broker: {observed}");
            match crate::broker_server::broker_image_instance_id() {
                Ok(expected) if expected == observed => println!(
                    "soldr broker:   this soldr:     {expected} (matches; removal is not required)"
                ),
                Ok(expected) => println!("soldr broker:   this soldr:     {expected} (mismatch)"),
                Err(error) => {
                    println!("soldr broker:   this soldr:     <unavailable: {error}>")
                }
            }
        }
        None => {
            println!("soldr broker: not running (nothing to remove)");
            return Ok(());
        }
    }

    let VerifiedBrokerStop::Stopped { broker_pid, .. } =
        stop_verified_broker(&socket_path, "remove", None, None, true)?
    else {
        // The broker exited between the diagnostic probe and the stop request.
        println!("soldr broker: not running (nothing to remove)");
        return Ok(());
    };
    crate::broker_spawn::retire_admission_endpoint(&socket_path).map_err(SoldrError::Other)?;

    // Delete the staged image last: `verified_broker_generation` resolves the
    // installed path on every poll above, so removing it earlier would break
    // the PID verification that makes the stop safe.
    let image = &endpoint.executable_path;
    match remove_staged_broker_image(image) {
        Ok(true) => println!(
            "soldr broker: removed staged broker image {}",
            image.display()
        ),
        Ok(false) => {}
        Err(error) => {
            return Err(SoldrError::Other(format!(
                "soldr broker: stopped broker pid {broker_pid} but could not remove its staged \
                 image {}: {error}",
                image.display()
            )))
        }
    }
    println!("soldr broker: removed (broker pid {broker_pid}; daemon routes retained)");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KnownBadBrokerRetirementOutcome {
    Retired {
        broker_pid: u32,
        shutdown_path: BrokerShutdownPath,
        staged_image_removed: bool,
    },
    AlreadyExited,
    IdentityChanged,
}

impl KnownBadBrokerRetirementOutcome {
    pub(crate) const fn outcome(self) -> &'static str {
        match self {
            Self::Retired { .. } => "retired",
            Self::AlreadyExited => "already_exited",
            Self::IdentityChanged => "identity_changed",
        }
    }

    pub(crate) const fn shutdown_path(self) -> &'static str {
        match self {
            Self::Retired { shutdown_path, .. } => shutdown_path.as_str(),
            Self::AlreadyExited | Self::IdentityChanged => "not_signaled",
        }
    }
}

/// Retire exactly the verified generation selected by soldr#2920 policy.
///
/// The second STATUS probe is bound to `expected_instance`; an endpoint race
/// becomes `IdentityChanged`, never a signal or file deletion. Any stop or
/// cleanup error returns before staging a replacement, so one invocation never
/// spins a retirement loop.
pub(crate) fn retire_known_bad_broker(
    socket_path: &str,
    expected_instance: &str,
    image: &std::path::Path,
    lease: &crate::broker_lease::BrokerLease,
) -> Result<KnownBadBrokerRetirementOutcome, SoldrError> {
    finalize_known_bad_broker_retirement(
        stop_verified_broker(
            socket_path,
            "known-bad broker retirement",
            Some(expected_instance),
            Some(lease),
            false,
        ),
        |broker_pid, _shutdown_path| {
            #[cfg(debug_assertions)]
            test_pause_after_known_bad_stop();
            maintain_retirement_lease(Some(lease))?;
            crate::broker_spawn::retire_admission_endpoint(socket_path)
                .map_err(SoldrError::Other)?;
            maintain_retirement_lease(Some(lease))?;
            remove_staged_broker_image(image).map_err(|error| {
                SoldrError::Other(format!(
                    "soldr broker: stopped known-bad broker pid {broker_pid} but could not remove \
                     staged image {}: {error}",
                    image.display()
                ))
            })
        },
    )
}

/// Renew and fence the resurrection lease across the entire known-bad
/// retirement transaction. Manual `broker stop`/`remove` deliberately pass
/// no lease; only the automatic exception has authority to replace resources.
fn maintain_retirement_lease(
    lease: Option<&crate::broker_lease::BrokerLease>,
) -> Result<(), SoldrError> {
    let Some(lease) = lease else {
        return Ok(());
    };
    lease
        .renew()
        .map_err(|error| SoldrError::Other(error.to_string()))?;
    lease
        .check_fence()
        .map_err(|error| SoldrError::Other(error.to_string()))
}

/// Test-only race seam: the lease must still fence cleanup if a stopped
/// known-bad broker leaves its owner paused long enough for another front door
/// to take the lease and stage a replacement. Production retirement never
/// reads these variables.
#[cfg(debug_assertions)]
fn test_pause_after_known_bad_stop() {
    let Some(milliseconds) = std::env::var("SOLDR_TEST_KNOWN_BAD_STOP_PAUSE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
    else {
        return;
    };
    if let Some(path) = std::env::var_os("SOLDR_TEST_KNOWN_BAD_STOP_READY_FILE") {
        let _ = std::fs::write(path, b"stopped\n");
    }
    let continue_file =
        std::env::var_os("SOLDR_TEST_KNOWN_BAD_STOP_CONTINUE_FILE").map(std::path::PathBuf::from);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(milliseconds);
    while std::time::Instant::now() < deadline {
        if continue_file.as_ref().is_none_or(|path| path.exists()) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Convert a verified stop into retirement cleanup. Keeping cleanup behind this
/// small boundary lets the failure and identity-change cases prove that they
/// cannot unlink the admission endpoint or delete a staged image.
fn finalize_known_bad_broker_retirement<F>(
    verified_stop: Result<VerifiedBrokerStop, SoldrError>,
    cleanup: F,
) -> Result<KnownBadBrokerRetirementOutcome, SoldrError>
where
    F: FnOnce(u32, BrokerShutdownPath) -> Result<bool, SoldrError>,
{
    match verified_stop? {
        VerifiedBrokerStop::NotRunning => Ok(KnownBadBrokerRetirementOutcome::AlreadyExited),
        VerifiedBrokerStop::IdentityChanged => Ok(KnownBadBrokerRetirementOutcome::IdentityChanged),
        VerifiedBrokerStop::Stopped {
            broker_pid,
            shutdown_path,
        } => {
            let staged_image_removed = cleanup(broker_pid, shutdown_path)?;
            Ok(KnownBadBrokerRetirementOutcome::Retired {
                broker_pid,
                shutdown_path,
                staged_image_removed,
            })
        }
    }
}

/// Delete the staged broker image, reporting whether one was there. Windows can
/// still hold a sharing lock on the executable for a short window after the
/// process it backed has exited, so a first `PermissionDenied` is retried
/// briefly rather than reported as a failed removal.
fn remove_staged_broker_image(image: &std::path::Path) -> std::io::Result<bool> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match std::fs::remove_file(image) {
            Ok(()) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) if std::time::Instant::now() >= deadline => return Err(error),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}

#[cfg(test)]
mod retirement_tests {
    use super::{
        finalize_known_bad_broker_retirement, BrokerShutdownPath, KnownBadBrokerRetirementOutcome,
        VerifiedBrokerStop,
    };
    use crate::core::SoldrError;

    #[test]
    fn retirement_reprobe_identity_change_never_enters_cleanup() {
        let mut cleanup_called = false;
        let outcome = finalize_known_bad_broker_retirement(
            Ok(VerifiedBrokerStop::IdentityChanged),
            |_, _| {
                cleanup_called = true;
                Ok(true)
            },
        )
        .expect("identity change is a safe non-error outcome");
        assert_eq!(outcome, KnownBadBrokerRetirementOutcome::IdentityChanged);
        assert!(
            !cleanup_called,
            "a post-lease STATUS identity change must retain the socket and staged image"
        );
    }

    #[test]
    fn retirement_stop_or_force_failure_never_enters_cleanup() {
        let mut cleanup_called = false;
        let error = finalize_known_bad_broker_retirement(
            Err(SoldrError::Other(
                "verified broker survived forced retirement".into(),
            )),
            |_, _| {
                cleanup_called = true;
                Ok(true)
            },
        )
        .expect_err("failed stop must block endpoint/image cleanup");
        assert!(error.to_string().contains("survived forced retirement"));
        assert!(
            !cleanup_called,
            "a termination failure must preserve the socket and staged image for operator recovery"
        );
    }

    #[test]
    fn retirement_cleans_up_only_after_a_verified_stop() {
        let mut cleanup = Vec::new();
        let outcome = finalize_known_bad_broker_retirement(
            Ok(VerifiedBrokerStop::Stopped {
                broker_pid: 42,
                shutdown_path: BrokerShutdownPath::Forced,
            }),
            |pid, path| {
                cleanup.push((pid, path));
                Ok(true)
            },
        )
        .expect("verified stop may clean up");
        assert_eq!(cleanup, vec![(42, BrokerShutdownPath::Forced)]);
        assert_eq!(
            outcome,
            KnownBadBrokerRetirementOutcome::Retired {
                broker_pid: 42,
                shutdown_path: BrokerShutdownPath::Forced,
                staged_image_removed: true,
            }
        );
    }
}

/// Return the start token only when `pid` still names the exact installed
/// stable-broker executable. Both fields are required before every signal so
/// PID reuse can only make stop conclude that the original generation exited.
pub(crate) fn verified_broker_generation(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, ProcessRefreshKind, System};

    let installed = crate::broker_identity::ResolvedBrokerEndpoint::resolve()
        .ok()
        .and_then(|endpoint| std::fs::canonicalize(endpoint.executable_path).ok());
    // `soldr broker serve` is also a supported foreground diagnostic surface.
    // In that mode the responder is this exact soldr image rather than the
    // staged broker copy, so admit the caller image as a second exact path.
    let caller = std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok());
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_process_specifics(pid, ProcessRefreshKind::everything());
    let process = system.process(pid)?;
    let actual = std::fs::canonicalize(process.exe()?).ok()?;
    (installed.as_ref() == Some(&actual) || caller.as_ref() == Some(&actual))
        .then(|| process.start_time())
}

/// Ask the broker to shut down cooperatively via the running-process
/// `SHUTDOWN` admin verb (soldr#2442 Option B), carrying the drain deadline.
/// Returns true if the broker acknowledged (exit_code 0); false if it does not
/// support the verb (exit_code 2) or the request failed — the caller then falls
/// back to verified-PID termination.
fn try_cooperative_shutdown(socket_path: &str) -> bool {
    use running_process::broker::client::send_admin_request;
    use running_process::broker::protocol::{AdminRequest, AdminVerb};

    let request = AdminRequest {
        verb: AdminVerb::Shutdown as i32,
        drain_deadline_ms: broker_stop_deadline().as_millis() as u64,
        ..Default::default()
    };
    matches!(
        send_admin_request(socket_path, request),
        Ok(reply) if reply.exit_code == 0
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn broker_commands_have_no_endpoint_namespace_argument() {
        let command = crate::cli_args::Cli::command();
        let broker = command
            .find_subcommand("broker")
            .expect("broker subcommand registered");
        for verb in ["serve", "status", "stop", "remove", "purge"] {
            let command = broker
                .find_subcommand(verb)
                .unwrap_or_else(|| panic!("{verb} subcommand registered"));
            assert!(
                command.get_arguments().all(|arg| arg.get_id() != "program"),
                "{verb} must use the one stable endpoint"
            );
        }
    }

    /// soldr#3193: same binding for the leak notice's remedy.
    #[test]
    fn the_command_named_by_the_leak_notice_is_a_real_verb() {
        let purge_verb = crate::broker_inventory::BROKER_PURGE_COMMAND
            .strip_prefix("soldr broker ")
            .expect("the remedy is a `soldr broker` verb");
        assert!(crate::cli_args::Cli::command()
            .find_subcommand("broker")
            .expect("broker subcommand registered")
            .find_subcommand(purge_verb)
            .is_some());
    }

    /// soldr#2549: the mismatch warning is only actionable if the command it
    /// names actually exists. Bind the two together so neither can drift.
    #[test]
    fn the_command_named_by_the_mismatch_warning_is_a_real_verb() {
        let remove_verb = crate::broker_spawn::BROKER_REMOVE_COMMAND
            .strip_prefix("soldr broker ")
            .expect("the remedy is a `soldr broker` verb");
        assert!(crate::cli_args::Cli::command()
            .find_subcommand("broker")
            .expect("broker subcommand registered")
            .find_subcommand(remove_verb)
            .is_some());
    }
}
