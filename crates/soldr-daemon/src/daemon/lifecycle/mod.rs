//! Route-claim based lifecycle for soldr-daemon: detect a live daemon,
//! spawn one detached when none is found, and append structured JSONL
//! lifecycle events.
//!
//! A route-local protobuf claim records the daemon process and its private
//! endpoint. Readers verify the live process before acting on that claim.

mod displacement_policy;
mod journal_hygiene;
mod legacy_endpoint;
mod readiness;
mod root_ownership;
mod shutdown_wait;
mod spawn_env;
pub(crate) use displacement_policy::{displacement_drain_timeout, ephemeral_displacement_blocked};
pub use displacement_policy::{
    ALLOW_EPHEMERAL_DISPLACE_ENV_VAR, DISPLACEMENT_DRAIN_TIMEOUT_ENV_VAR,
};
pub use journal_hygiene::{detect_unclean_shutdown, rotate_lifecycle_journal};
#[cfg(test)]
pub(crate) use readiness::status_with_retiring_retry;
pub use readiness::{
    status_after_negotiated_route, status_after_route_ready, NegotiatedRouteError,
    START_STATUS_READY_TIMEOUT, STATUS_RETIRING_RETRY_TIMEOUT,
};
pub(crate) use root_ownership::root_owner_lock_path;
pub use root_ownership::{RootAcquireOutcome, RootOwnershipGuard};
#[cfg(test)]
pub(crate) use shutdown_wait::latest_shutdown_phase;
pub use shutdown_wait::{wait_for_shutdown_responder, GRACEFUL_SHUTDOWN_WAIT_TIMEOUT};
pub(crate) use spawn_env::*;

use crate::cache_lib::{daemon_lifecycle_log_path, soldr_daemon_dir};
use crate::core::SoldrPaths;
use crate::platform::process::inspect::find_file_holders;
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Private parent-to-wrapper handoff for the canonical daemon executable.
///
/// Cargo invokes Soldr through compiler-named hardlinks such as `rustc`.
/// Spawning a long-lived daemon from that `current_exe()` gives the process a
/// `rustc` executable identity, which the PID-recycling safety gate must reject.
/// The Cargo front door therefore materializes a `soldr-daemon` alias and
/// passes its absolute path through this variable.
pub const SOLDR_DAEMON_EXE_ENV_VAR: &str = "SOLDR_INTERNAL_DAEMON_EXE";

#[derive(Debug)]
pub enum LifecycleError {
    Io(std::io::Error),
    Spawn(std::io::Error),
}

impl From<std::io::Error> for LifecycleError {
    fn from(e: std::io::Error) -> Self {
        LifecycleError::Io(e)
    }
}

/// Consecutive observations of a missing image before the daemon gives up.
///
/// Hysteresis is the whole safety argument. A daemon that exits the first time
/// `current_exe()` fails to resolve -- a momentarily unavailable network path,
/// a slow removable volume, an antivirus lock -- is strictly worse than the
/// orphan it prevents: it would take down healthy daemons for transient
/// reasons. Three consecutive maintenance ticks is minutes of sustained
/// absence, which no transient survives and a deleted image never recovers
/// from.
pub const DAEMON_IMAGE_MISSING_STRIKES: u32 = 3;

/// Does this process's own executable still exist on disk?
///
/// `None` when the path cannot be determined at all, which is not evidence of
/// deletion and must not be counted as a strike.
pub fn daemon_image_present() -> Option<bool> {
    std::env::current_exe().ok().map(|exe| exe.exists())
}

/// Tracks sustained absence of the daemon's own image (soldr#1987).
///
/// A daemon spawned from a temp directory that is later deleted can keep the
/// root-ownership lock indefinitely. The broker-owned placement model prevents
/// new daemons from using disposable images; this detector remains to retire
/// orphans created by older Soldr versions.
///
/// A daemon whose own executable is gone can never again be a legitimate
/// owner: it cannot be upgraded, restarted in place, or verified by the
/// PID-recycling identity gate. Standing down is the only correct response.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MissingImageDetector {
    consecutive: u32,
}

impl MissingImageDetector {
    /// Feed one observation. Returns true exactly once, on the strike that
    /// confirms sustained absence.
    ///
    /// `None` (path unknown) resets rather than accumulates: not knowing where
    /// our image is says nothing about whether it exists.
    pub fn observe(&mut self, present: Option<bool>) -> bool {
        match present {
            Some(false) => {
                self.consecutive = self.consecutive.saturating_add(1);
                self.consecutive == DAEMON_IMAGE_MISSING_STRIKES
            }
            // A single successful sighting clears the count: the condition
            // this detects is permanent, so any recovery proves it was not it.
            Some(true) | None => {
                self.consecutive = 0;
                false
            }
        }
    }

    pub fn strikes(&self) -> u32 {
        self.consecutive
    }
}

/// Explain who owns the soldr root when acquisition fails.
///
/// soldr#1987: older versions could spawn a daemon from a disposable build
/// directory and leave it holding the root after its image vanished. Mandatory
/// broker routing now reports that ownership conflict as a hard failure, while
/// the detector below supplies the recorded PID and image-presence evidence.
///
/// The orphan is also unreachable by `soldr daemon stop`, which probes the
/// pipe while the orphan holds only the filesystem lock. So the message has to
/// carry enough to act on: the PID, and whether its image still exists --
/// because a holder whose executable is gone cannot be a legitimate owner.
///
/// Deliberately hedged. The route claim records the daemon that last wrote it,
/// which is *probably* the lock holder but is not proven to be, so the wording
/// says "recorded" rather than asserting identity. Naming a plausible suspect
/// beats today's silence; claiming certainty we do not have would be worse.
pub fn describe_root_ownership_conflict(paths: &SoldrPaths) -> String {
    let root = paths.root.display();
    let Some((pid, exe)) = read_recorded_daemon_identity(paths) else {
        // A sibling generation may be live, but it cannot own this route's lock.
        return crate::daemon::other_generations::describe_unrecorded_owner(
            paths,
            &crate::daemon::other_generations::recorded_generation_owners(paths),
            pid_is_alive,
        );
    };
    let alive = pid_is_alive(pid);
    let image_exists = exe.exists();
    match (alive, image_exists) {
        (true, false) => format!(
            "soldr root ownership is busy: {root}
             soldr: the recorded daemon is PID {pid}, whose image no longer exists ({}).
             soldr: an orphan like this holds the lock indefinitely and cannot be reached by              `soldr daemon stop`, which probes the pipe rather than the lock (soldr#1987).
             soldr: terminate PID {pid} to recover.",
            exe.display()
        ),
        (true, true) => format!(
            "soldr root ownership is busy: {root} (held by PID {pid}, image {})",
            exe.display()
        ),
        (false, _) => {
            // The stale claim does not identify the process holding this
            // route's private lock. Never suggest killing sibling daemons.
            let lock = root_ownership::root_owner_lock_path(paths);
            let holders = find_file_holders(&lock);
            if holders.is_empty() {
                format!(
                    "soldr root ownership is busy: {root} -- recorded owner PID {pid} is dead, but this route's lock is still held (soldr#2316).\n\
                     soldr: no live process currently holds {}.\n\
                     soldr: the lock may be held by a process we cannot inspect (permission denied) or the holder exited without releasing it.\n\
                     soldr: run `soldr status` and `soldr logs paths` to inspect this route. Use SOLDR_CACHE_DIR=<scratch dir> to build with a separate root.",
                    lock.display()
                )
            } else {
                let mut details = String::new();
                for (i, holder) in holders.iter().enumerate() {
                    let is_soldr_daemon = holder.exe.file_stem()
                        .and_then(|s| s.to_str())
                        .is_some_and(|stem| stem.eq_ignore_ascii_case("soldr-daemon") || stem.eq_ignore_ascii_case("soldr"));
                    let daemon_note = if is_soldr_daemon { " (soldr-daemon)" } else { "" };
                    details.push_str(&format!(
                        "  {}: PID {} image {}{}\n",
                        i + 1,
                        holder.pid,
                        holder.exe.display(),
                        daemon_note
                    ));
                }
                format!(
                    "soldr root ownership is busy: {root} -- recorded owner PID {pid} is dead, but this route's lock is still held (soldr#2316).\n\
                     soldr: the following live process(es) hold the lock:\n{}\
                     soldr: terminate only the process(es) holding this lock. Other daemon routes can coexist and must keep serving.\n\
                     soldr: run `soldr status` and `soldr logs paths` to inspect this route.",
                    details
                )
            }
        }
    }
}

/// Read `(pid, exe_path)` from the route claim. This does not verify liveness;
/// [`is_live`] performs the exact process and endpoint probe.
///
/// The identity-only reader retains enough information to identify a live
/// daemon from an older route-claim format. It must never authenticate a broker
/// route; every caller here verifies the live executable before using the PID.
pub fn read_route_claim_identity(paths: &SoldrPaths) -> Option<(u32, PathBuf)> {
    crate::daemon::backend_handle_adoption::read_broker_route_claim_owner_identity(paths)
        .ok()
        .flatten()
}

/// Read the daemon identity recorded by a release predating route claims.
///
/// Soldr 0.8.29 and earlier wrote two lines (`pid`, then executable path) to
/// `daemon.pid`. Keep this reader private and read-only: new daemons publish a
/// route claim, but an upgraded broker still needs enough verified identity to
/// displace the old process that owns the version-independent root lock.
fn read_legacy_daemon_pid_identity(paths: &SoldrPaths) -> Option<(u32, PathBuf)> {
    let raw = fs::read_to_string(soldr_daemon_dir(paths).join("daemon.pid")).ok()?;
    let mut lines = raw.lines();
    let pid = lines.next()?.trim().parse().ok()?;
    let exe = lines.next()?.trim();
    if exe.is_empty() {
        return None;
    }
    Some((pid, PathBuf::from(exe)))
}

fn legacy_daemon_identity_is_verified(identity: &(u32, PathBuf)) -> bool {
    let (pid, exe) = identity;
    *pid != std::process::id()
        && legacy_executable_stem_is_supported(exe)
        && pid_is_alive(*pid)
        && pid_exe_path_matches(*pid, exe)
}

fn legacy_executable_stem_is_supported(exe: &Path) -> bool {
    exe.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| {
            ["soldr-daemon", "soldr", "rustc"]
                .iter()
                .any(|expected| stem.eq_ignore_ascii_case(expected))
        })
}

fn should_use_legacy_endpoint(route_is_verified: bool, legacy_is_verified: bool) -> bool {
    !route_is_verified && legacy_is_verified
}

fn select_recorded_daemon_identity(
    route: Option<(u32, PathBuf)>,
    route_is_verified: bool,
    legacy: Option<(u32, PathBuf)>,
    legacy_is_verified: bool,
) -> Option<(u32, PathBuf)> {
    if route_is_verified {
        return route;
    }
    if legacy_is_verified {
        return legacy;
    }
    route.or(legacy)
}

/// Prefer a live, verified route claim, then a live legacy identity. Dead
/// records remain useful for ownership diagnostics, but a dead route claim
/// must not mask an older daemon that is still holding the root lock.
fn read_recorded_daemon_identity(paths: &SoldrPaths) -> Option<(u32, PathBuf)> {
    let route = read_route_claim_identity(paths);
    let legacy = read_legacy_daemon_pid_identity(paths);
    let route_is_verified = route
        .as_ref()
        .is_some_and(|(pid, _)| pid_is_soldr_daemon(*pid));
    let legacy_is_verified = legacy
        .as_ref()
        .is_some_and(legacy_daemon_identity_is_verified);
    select_recorded_daemon_identity(route, route_is_verified, legacy, legacy_is_verified)
}

/// Verify the daemon recorded in the route claim is still alive AND its
/// running exe stem looks like a soldr-daemon. Returns the PID on
/// success, None on any mismatch / missing file.
pub fn is_live(paths: &SoldrPaths) -> Option<u32> {
    direct_backend_handle_probe(paths)
}

/// Probe the route-local claim's recorded daemon with the
/// `running-process` `BackendHandle` nonce challenge.
pub(crate) fn direct_backend_handle_probe(paths: &SoldrPaths) -> Option<u32> {
    crate::daemon::backend_handle_adoption::probe_soldr_daemon(paths).map(|handle| handle.pid())
}

pub(crate) fn claimed_process_live(paths: &SoldrPaths) -> Option<u32> {
    let (pid, _exe_path) = read_route_claim_identity(paths)?;
    pid_is_soldr_daemon(pid).then_some(pid)
}

/// Env escape hatch (soldr#1495): set `SOLDR_DAEMON_DISPLACE=off` (or
/// `0`/`false`/`no`) to disable displacement of a stale-version daemon
/// and revert to the pre-#1495 "first daemon wins" behavior.
pub(crate) const SOLDR_DAEMON_DISPLACE_ENV: &str = "SOLDR_DAEMON_DISPLACE";

/// Normal upper bound for an acknowledged graceful shutdown.
pub(crate) fn displacement_enabled() -> bool {
    match std::env::var(SOLDR_DAEMON_DISPLACE_ENV) {
        Ok(v) => {
            let v = v.trim();
            !(v.eq_ignore_ascii_case("off")
                || v == "0"
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("no"))
        }
        Err(_) => true,
    }
}

/// True when the running daemon's published claim matches this build's
/// identity. The stable broker route already partitions daemon images by
/// package version and executable digest.
///
/// A missing claim (a pre-#1495 daemon that never wrote a manifest) is
/// treated as a mismatch — version-unknown is stale — so a newer client
/// always converges to a daemon it can name. A daemon claiming only a bare
/// package version, i.e. one built before this change, is stale for the
/// same reason, and is displaced once on first contact.
pub(crate) fn current_version_claim_matches(paths: &SoldrPaths) -> bool {
    let service =
        std::env::var_os(crate::daemon::backend_handle_adoption::SOLDR_BROKER_SERVICE_ENV_VAR);
    current_version_claim_matches_service(paths, service.as_deref())
}

fn current_version_claim_matches_service(
    paths: &SoldrPaths,
    service: Option<&std::ffi::OsStr>,
) -> bool {
    let Some(claim) = crate::daemon::backend_handle_adoption::read_broker_route_claim(paths)
        .ok()
        .flatten()
    else {
        return false;
    };
    let Some(service) = service else {
        #[cfg(debug_assertions)]
        if std::env::var_os(crate::daemon::client::TEST_DIRECT_CONTROL_ENV).is_some() {
            let current = std::env::current_exe()
                .ok()
                .and_then(|path| std::fs::canonicalize(path).ok());
            return current.is_some_and(|path| {
                std::fs::canonicalize(claim.exe_path).is_ok_and(|claim| claim == path)
            });
        }
        // No route identity means there is no evidence that this claim belongs
        // to the current daemon image. Accepting any claim here let an old
        // image suppress displacement before the front door registered its
        // current route.
        return false;
    };
    let expected_route = crate::daemon::service_definition::broker_owned_paths()
        .root
        .join("routes")
        .join(service);
    let expected_route = std::fs::canonicalize(&expected_route).unwrap_or(expected_route);
    std::fs::canonicalize(claim.exe_path).is_ok_and(|path| path.starts_with(expected_route))
}

/// Version-aware liveness: the daemon is live (passes the existing PID +
/// nonce/IPC probe, which already rejects a different `PROTOCOL_VERSION`)
/// **and** its package-version claim matches this build. Returns the PID
/// only when both hold. Used by the spawn path and the managed-build
/// preflight to decide whether the running daemon is the one this client
/// wants, or a stale version to displace.
pub fn is_live_current_version(paths: &SoldrPaths) -> Option<u32> {
    let pid = is_live(paths)?;
    current_version_claim_matches(paths).then_some(pid)
}

fn direct_status_current_version_for_service(
    paths: &SoldrPaths,
    service: Option<&std::ffi::OsStr>,
) -> Option<u32> {
    let pid = claimed_process_live(paths)?;
    let sock = crate::daemon::client::default_sock_path(paths);
    let status_pid = crate::daemon::client::status(&sock).ok()?.pid;
    preflight_identity_matches(
        Some(pid),
        Some(status_pid),
        current_version_claim_matches_service(paths, service),
    )
    .then_some(pid)
}

fn preflight_identity_matches(
    recorded_live_pid: Option<u32>,
    status_pid: Option<u32>,
    current_version_claim: bool,
) -> bool {
    current_version_claim && recorded_live_pid.is_some() && recorded_live_pid == status_pid
}

/// Version-blind occupancy check: is the singleton endpoint held by a
/// live soldr daemon process at all? Accepts both the sibling
/// `soldr-daemon` binary and the via-self `soldr` form (CI / slim
/// deployments spawn `soldr daemon start`). Returns its PID. This is the
/// signal that a stale daemon must be *displaced* before a new one can
/// bind — distinct from `is_live` (which additionally requires the IPC
/// probe to succeed, and so misses a protocol-mismatched daemon that is
/// nonetheless holding the socket).
pub fn claimed_daemon_occupies_route(paths: &SoldrPaths) -> Option<u32> {
    if let Some((pid, _exe)) = read_route_claim_identity(paths) {
        if pid_is_soldr_daemon(pid) {
            return Some(pid);
        }
    }
    read_legacy_daemon_pid_identity(paths)
        .filter(legacy_daemon_identity_is_verified)
        .map(|(pid, _)| pid)
}

/// Only a route claim may authorize the protocol-mismatch signal fallback.
/// A legacy PID file has no nonce or boot identity, so it is discovery-only:
/// it may lead us to the old endpoint for graceful shutdown, never to signal a
/// PID that the operating system could have recycled.
fn force_terminable_claimed_daemon(paths: &SoldrPaths) -> Option<u32> {
    let (pid, _exe) = read_route_claim_identity(paths)?;
    pid_is_soldr_daemon(pid).then_some(pid)
}

/// PID-recycling-safe identity gate for a signalled kill: the PID must be
/// alive and its running image must look like one of soldr's own daemon
/// process names. A recycled PID running an unrelated program fails the
/// stem check, so we never signal a stranger.
fn pid_is_soldr_daemon(pid: u32) -> bool {
    pid_is_alive(pid)
        && (pid_exe_stem_matches(
            pid,
            crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_NAME,
        ) || pid_exe_stem_matches(pid, "soldr"))
}

/// Result of waiting for one acknowledged daemon generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownWaitOutcome {
    /// The acknowledged PID is no longer alive.
    Exited,
    /// The endpoint now reports a different daemon generation.
    Replaced,
    /// The acknowledged generation could not be proven gone in time.
    TimedOut,
}

impl ShutdownWaitOutcome {
    pub fn is_complete(self) -> bool {
        matches!(self, Self::Exited | Self::Replaced)
    }
}

fn classify_shutdown_observation(
    responder: crate::daemon::protocol::ShutdownAck,
    responder_pid_alive: bool,
    endpoint_identity: Option<(u32, u64)>,
) -> Option<ShutdownWaitOutcome> {
    if !responder_pid_alive {
        return Some(ShutdownWaitOutcome::Exited);
    }
    endpoint_identity
        .filter(|identity| *identity != (responder.pid, responder.generation))
        .map(|_| ShutdownWaitOutcome::Replaced)
}

/// Displace the stale daemon currently holding the endpoint so a
/// current-version daemon can take over (soldr#1495). Graceful IPC is always
/// attempted first, including for historical daemons whose executable was
/// accidentally named `rustc`. A verified-PID signal is permitted only when
/// no shutdown acknowledgement was received. An acknowledged daemon owns
/// its graceful flush for the whole D7 drain budget
/// (`SOLDR_DISPLACEMENT_DRAIN_TIMEOUT_SECS`, default 120 s); only on
/// budget expiry — recorded as `drain-timeout` — does the kill fallback
/// engage (soldr#2436 phase 3).
/// `source` attributes the request, so two `displace-kill-fallback`
/// records from different entry points stay distinguishable (soldr#1808).
pub fn displace_stale_daemon(paths: &SoldrPaths, source: Option<LifecycleSource>) -> bool {
    let verified_pid = force_terminable_claimed_daemon(paths);
    let recorded_pid = read_recorded_daemon_identity(paths).map(|(pid, _)| pid);
    let recorded_live_pid = recorded_pid.filter(|pid| pid_is_alive(*pid));
    append_lifecycle_event_with(
        paths,
        "displace-stale-requested",
        LifecycleDetails::requested(LifecycleReason::StaleVersion).from_current_process(source),
    );

    let route_is_verified =
        read_route_claim_identity(paths).is_some_and(|(pid, _)| pid_is_soldr_daemon(pid));
    let legacy_is_verified = read_legacy_daemon_pid_identity(paths)
        .as_ref()
        .is_some_and(legacy_daemon_identity_is_verified);
    let sock = if should_use_legacy_endpoint(route_is_verified, legacy_is_verified) {
        legacy_endpoint::resolve(paths)
            .unwrap_or_else(|_| crate::daemon::client::default_sock_path(paths))
    } else {
        crate::daemon::client::default_sock_path(paths)
    };
    match crate::daemon::client::shutdown(&sock) {
        Ok(responder) => {
            // soldr#2436 phase 3 (D7): an acknowledged shutdown gets the
            // whole drain budget, not a 5 s proof-wait — a real drain
            // (depgraph snapshot, event flush) can take minutes, and
            // racing it was the #1814 two-daemons window that loses
            // in-memory compile contexts. On expiry, record the timeout
            // and fall through to the existing kill fallback below.
            if wait_for_shutdown_responder(paths, &sock, responder, displacement_drain_timeout())
                .is_complete()
            {
                return true;
            }
            append_lifecycle_event_with(
                paths,
                "drain-timeout",
                LifecycleDetails::requested(LifecycleReason::StaleVersion)
                    .from_current_process(source),
            );
        }
        Err(crate::daemon::client::ClientError::NotRunning) if recorded_live_pid.is_none() => {
            append_lifecycle_event_with(
                paths,
                "previous-daemon-vanished-without-ack",
                LifecycleDetails::vanished_without_ack(recorded_pid, LifecycleReason::StaleVersion)
                    .from_current_process(source),
            );
            return true;
        }
        Err(_) => {}
    }

    // Pre-route-claim daemons can receive this shutdown request but cannot
    // encode the current responder acknowledgement. Give such a daemon a
    // brief chance to honor the request before considering the route-claim
    // signal fallback. The compatibility PID record itself never grants
    // signal authority.
    if let Some(pid) = recorded_live_pid {
        if wait_for_pid_exit(pid, Duration::from_millis(500)) {
            append_lifecycle_event_with(
                paths,
                "previous-daemon-vanished-without-ack",
                LifecycleDetails::vanished_without_ack(
                    Some(pid),
                    LifecycleReason::ProtocolMismatch,
                )
                .from_current_process(source),
            );
            return true;
        }
    }

    // No acknowledgement: a protocol-mismatched daemon can only be stopped
    // through a currently verified Soldr process identity.
    let Some(pid) = verified_pid else {
        return false;
    };
    if !pid_is_soldr_daemon(pid) {
        if !pid_is_alive(pid) {
            append_lifecycle_event_with(
                paths,
                "previous-daemon-vanished-without-ack",
                LifecycleDetails::vanished_without_ack(
                    Some(pid),
                    LifecycleReason::ProtocolMismatch,
                )
                .from_current_process(source),
            );
            return true;
        }
        return false;
    }

    // Reached only after the shutdown request produced no acknowledgement,
    // which is the protocol-mismatch case the block above describes.
    terminate_pid(pid, None);
    let exited = wait_for_pid_exit(pid, Duration::from_secs(5));
    append_lifecycle_event_with(
        paths,
        "displace-kill-fallback",
        LifecycleDetails::forced(pid, LifecycleReason::ProtocolMismatch)
            .with_outcome(if exited {
                LifecycleOutcome::Forced
            } else {
                LifecycleOutcome::Failed
            })
            .from_current_process(source),
    );
    exited
}

/// One-shot preflight for the managed-build front door (soldr#1495).
/// Mode 1 — a same-`PROTOCOL_VERSION` older release serving our compiles
/// — never fails the hot path. Run this once at `soldr cargo` startup: if a
/// stale-version daemon holds the endpoint, displace it here so the broker can
/// launch the current route image on the first compile request. A no-op
/// when displacement is disabled or the running daemon is already current.
pub fn preflight_displace_stale_daemon(paths: &SoldrPaths) {
    let service =
        std::env::var_os(crate::daemon::backend_handle_adoption::SOLDR_BROKER_SERVICE_ENV_VAR);
    preflight_displace_stale_daemon_for_service(paths, service.as_deref());
}

pub fn preflight_displace_stale_daemon_for_service(
    paths: &SoldrPaths,
    service: Option<&std::ffi::OsStr>,
) {
    if !displacement_enabled() {
        return;
    }
    // Regression for #1832: use Soldr's bounded direct status handshake here.
    // The general `is_live_current_version` route probes both the optional
    // broker and a full executable identity hash; together those added tens
    // of seconds to every warm cargo invocation. Requiring the status PID to
    // match the live claimed process prevents PID reuse from accepting an
    // unrelated same-stem process, while the normal status wire exchange
    // still proves that the current-protocol daemon owns the endpoint.
    if direct_status_current_version_for_service(paths, service).is_some() {
        return;
    }
    // Issue #1865: a `None` above is ambiguous — it means either "this daemon
    // is a stale version" or "the status probe did not finish inside
    // REPLY_TIMEOUT". Treating both as staleness displaced healthy daemons
    // that were merely too busy to answer a 2 s ping, which is how a build
    // could lose its warm daemon mid-run (and, per #1814, briefly end up with
    // two daemons contending for state.sqlite3).
    //
    // Require positive evidence instead. A process that is alive, looks like
    // one of our daemons, and publishes *this exact* version claim cannot be
    // the stale-version daemon this preflight exists to displace, however
    // unresponsive it currently is. Exact claimed-process + route matching
    // costs no IPC, so this stays latency-neutral for the warm path #1832 tuned.
    //
    // Trade-off: a genuinely wedged current-version daemon is no longer
    // displaced *here*. That is outside preflight's remit (its job is version
    // skew) and is covered by the wedge/recovery ladder — and killing a
    // busy-but-healthy daemon is strictly worse than leaving it alone.
    let claim_proves_current = claimed_process_live(paths)
        .filter(|_| current_version_claim_matches_service(paths, service));
    let recorded_process_is_alive = claimed_daemon_occupies_route(paths).is_some();
    let endpoint_artifact_exists =
        crate::daemon::backend_handle_adoption::broker_route_claim_path(paths).exists();
    if preflight_should_displace(
        claim_proves_current.is_some(),
        claimed_daemon_occupies_route(paths).is_some(),
        recorded_process_is_alive,
        endpoint_artifact_exists,
    ) {
        if ephemeral_displacement_blocked() {
            eprintln!(
                "soldr: not displacing the running daemon: this soldr runs from a \
                 disposable build directory (pip/uv build env), whose image will \
                 vanish (soldr#2436 D8). Set {ALLOW_EPHEMERAL_DISPLACE_ENV_VAR}=1 \
                 to override."
            );
            return;
        }
        displace_stale_daemon(paths, Some(LifecycleSource::Preflight));
    } else if let Some(pid) = claim_proves_current {
        tracing::warn!(
            event = "preflight_displacement_declined",
            pid,
            "status probe did not answer, but the daemon is alive and claims the \
             current version — not displacing it (issue #1865)"
        );
    }
}

/// Pure policy for [`preflight_displace_stale_daemon`], so the
/// probe-failed-vs-actually-stale matrix is unit-testable without real
/// daemons or real timeouts (issue #1865).
///
/// `claim_proves_current` is the evidence that closes the #1865 hole: the
/// caller has already failed to get a status answer, and this says whether the
/// claimed process is nonetheless alive, one of ours, and publishing this
/// exact version. When it is, no amount of endpoint occupancy justifies a
/// displacement — the daemon is current, just unresponsive right now.
fn preflight_should_displace(
    claim_proves_current: bool,
    stale_daemon_occupies: bool,
    recorded_process_is_alive: bool,
    endpoint_artifact_exists: bool,
) -> bool {
    if claim_proves_current {
        return false;
    }
    stale_daemon_occupies || recorded_process_is_alive || endpoint_artifact_exists
}

fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !pid_is_alive(pid) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50).min(remaining));
    }
    !pid_is_alive(pid)
}

/// Wait until a daemon PID is no longer alive. A zero timeout is an
/// instantaneous observation. Callers capture the PID before sending the
/// shutdown request so removal of the route claim cannot be mistaken for process
/// exit.
pub fn wait_for_daemon_exit(pid: u32, timeout: Duration) -> bool {
    wait_for_pid_exit(pid, timeout)
}

fn terminate_pid(pid: u32, deadline: Option<Instant>) {
    // SIGTERM (Windows: the daemon's named terminate event, soldr#3096,
    // falling back to TerminateProcess when the target has none), wait a
    // short grace, then escalate to SIGKILL. The deadline
    // bookkeeping is lifecycle policy; the platform crate owns the
    // signaling.
    let decision = crate::daemon::kill_decisions::record(
        "terminate-pid",
        Some(pid),
        "lifecycle terminate: SIGTERM then SIGKILL after grace".to_string(),
    );
    let _ = crate::platform::process::terminate::signal_pid(pid, false);
    let grace = deadline
        .map(|deadline| deadline.saturating_duration_since(Instant::now()))
        .unwrap_or(Duration::from_secs(3))
        .min(Duration::from_secs(3));
    if grace.is_zero() || wait_for_pid_exit(pid, grace) {
        decision.resolve("exited-within-grace");
        return;
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        decision.resolve("deadline-expired");
        return;
    }
    decision.mark_escalated();
    let _ = crate::platform::process::terminate::signal_pid(pid, true);
    decision.resolve("sigkill-sent");
}

/// Why a lifecycle transition happened.
///
/// soldr#1808 asks for typed details rather than event names assembled by
/// concatenating free-form values: `event` stays a stable identifier readers
/// can match on, and the circumstances travel in their own fields.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum LifecycleReason {
    /// An operator or wrapper asked the daemon to stop.
    ExplicitStop,
    /// The running daemon does not claim the current package version.
    StaleVersion,
    /// The daemon could not be reached over IPC at a compatible version, so
    /// no graceful shutdown was possible.
    ProtocolMismatch,
    /// A spawn deadline expired while a stale daemon still held the endpoint.
    StartupDeadline,
    /// The daemon process panicked (soldr#2436 phase 2: every restart must
    /// be attributable; a panic previously left no exit record at all).
    Panic,
}

/// Which entry point asked for a lifecycle transition.
///
/// soldr#1808 wants displacement attributable. Two records that both say
/// `displace-kill-fallback` are indistinguishable without this: one may be a
/// build's own preflight clearing a stale-version daemon, the other an
/// operator running `soldr daemon stop`. They have different causes and
/// different remedies.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum LifecycleSource {
    /// A user-invoked command.
    Cli,
    /// The per-build preflight that clears a stale-version daemon.
    Preflight,
    /// The OS-observed peer on an accepted IPC connection.
    IpcPeer,
    /// The transport did not expose a trustworthy peer identity.
    Unknown,
}

/// How the transition ended.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum LifecycleOutcome {
    /// Asked for; the result was not yet known when the record was written.
    Requested,
    /// The daemon acknowledged and owns its graceful flush to completion.
    Acknowledged,
    /// Terminated by signal after no acknowledgement arrived.
    Forced,
    /// The process was already gone before the transition completed.
    VanishedWithoutAck,
    /// Attempted, and did not succeed.
    Failed,
}

/// Optional attribution attached to a lifecycle record.
///
/// Every field is skipped when absent, so a record carrying no details
/// serializes byte-identically to the pre-soldr#1808 three-field shape --
/// including for the substring-matching reader in
/// `crates/soldr-cli/tests/daemon/cli_daemon_lifecycle.rs`, which looks
/// for `"event":"spawn"`.
#[derive(Serialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct LifecycleDetails {
    /// OS-observed process that requested the transition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requester_pid: Option<u32>,
    /// Best-effort executable path for `requester_pid`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requester_exe: Option<String>,
    /// The process acted upon, when it differs from the recording process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_pid: Option<u32>,
    /// Target daemon generation, when an IPC response made it knowable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_generation: Option<u64>,
    /// Target daemon route endpoint, when the recording daemon owns it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_endpoint: Option<String>,
    /// Which entry point asked for this transition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requester_source: Option<LifecycleSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<LifecycleReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<LifecycleOutcome>,
    /// Soldr package version of the recording daemon (soldr#2436 D2:
    /// restart forensics need the version without correlating PIDs
    /// against install logs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soldr_version: Option<String>,
    /// Embedded zccache library version of the recording daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zccache_version: Option<String>,
}

impl LifecycleDetails {
    /// A forced termination of `target_pid`, for `reason`.
    pub fn forced(target_pid: u32, reason: LifecycleReason) -> Self {
        Self {
            target_pid: Some(target_pid),
            reason: Some(reason),
            outcome: Some(LifecycleOutcome::Forced),
            ..Self::default()
        }
    }

    /// Version + executable identity of the recording daemon, for the
    /// `spawn` and panic exit records (soldr#2436 phase 2).
    pub fn recording_daemon_identity() -> Self {
        Self {
            requester_pid: Some(std::process::id()),
            requester_exe: std::env::current_exe()
                .ok()
                .map(|path| path.to_string_lossy().into_owned()),
            soldr_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            zccache_version: Some(zccache::core::VERSION.to_string()),
            ..Self::default()
        }
    }

    /// Attribute this record to the entry point that asked for it.
    pub fn from_source(mut self, source: Option<LifecycleSource>) -> Self {
        self.requester_source = source;
        self
    }

    /// Attribute this record to an OS-observed IPC peer.
    pub(crate) fn with_peer(mut self, peer: crate::daemon::ipc_peer::PeerIdentity) -> Self {
        self.requester_pid = peer.pid;
        self.requester_exe = peer.exe;
        self.requester_source = Some(peer.source);
        self
    }

    /// Attribute a local displacement to the process performing it.
    pub fn from_current_process(mut self, source: Option<LifecycleSource>) -> Self {
        self.requester_pid = Some(std::process::id());
        self.requester_exe = std::env::current_exe()
            .ok()
            .map(|path| path.to_string_lossy().into_owned());
        self.requester_source = Some(source.unwrap_or(LifecycleSource::Unknown));
        self
    }

    /// Name the daemon generation receiving a graceful IPC request.
    pub(crate) fn for_target_generation(mut self, pid: u32, generation: u64) -> Self {
        self.target_pid = Some(pid);
        self.target_generation = Some(generation);
        self
    }

    pub(crate) fn for_target_route(
        mut self,
        pid: u32,
        generation: u64,
        endpoint: impl Into<String>,
    ) -> Self {
        self.target_pid = Some(pid);
        self.target_generation = Some(generation);
        self.target_endpoint = Some(endpoint.into());
        self
    }

    /// Override the result after observing what actually happened.
    pub fn with_outcome(mut self, outcome: LifecycleOutcome) -> Self {
        self.outcome = Some(outcome);
        self
    }

    /// A previous daemon disappeared before acknowledging shutdown.
    pub fn vanished_without_ack(target_pid: Option<u32>, reason: LifecycleReason) -> Self {
        Self {
            target_pid,
            reason: Some(reason),
            outcome: Some(LifecycleOutcome::VanishedWithoutAck),
            ..Self::default()
        }
    }

    /// A transition that has been asked for but not yet resolved.
    pub fn requested(reason: LifecycleReason) -> Self {
        Self {
            reason: Some(reason),
            outcome: Some(LifecycleOutcome::Requested),
            ..Self::default()
        }
    }
}

#[derive(Serialize)]
struct LifecycleEvent<'a> {
    ts_ms: i64,
    pid: u32,
    event: &'a str,
    #[serde(flatten)]
    details: LifecycleDetails,
}

/// Record a lifecycle event carrying no attribution.
pub fn append_lifecycle_event(paths: &SoldrPaths, event: &str) {
    append_lifecycle_event_with(paths, event, LifecycleDetails::default());
}

/// Record a lifecycle event with typed attribution (soldr#1808).
pub fn append_lifecycle_event_with(paths: &SoldrPaths, event: &str, details: LifecycleDetails) {
    let pid = std::process::id();
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let line = match serde_json::to_string(&LifecycleEvent {
        ts_ms,
        pid,
        event,
        details,
    }) {
        Ok(s) => s,
        Err(_) => return,
    };
    let path = daemon_lifecycle_log_path(paths);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    journal_hygiene::with_lifecycle_journal_lock(&path, || {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{line}");
        }
    });
}

pub(crate) fn pid_is_alive(pid: u32) -> bool {
    // Zombie handling lives in the platform crate: Unix retains an exited
    // child in the process table until its parent reaps it, and a zombie
    // can never serve IPC again, so it is reported as dead.
    crate::platform::process::inspect::is_alive(pid)
}

pub(crate) fn pid_exe_stem_matches(pid: u32, expected_stem: &str) -> bool {
    crate::platform::process::inspect::executable_stem_matches(pid, expected_stem)
}

fn pid_exe_path_matches(pid: u32, expected_path: &Path) -> bool {
    crate::platform::process::inspect::executable_path_matches(pid, expected_path)
}

#[cfg(test)]
mod tests;
