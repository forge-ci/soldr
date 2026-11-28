//! Converge a route request on its generation's claimed daemon (soldr#3561).
//!
//! A route's claim slot is keyed by its broker `service_name`, which names the
//! canonical root, Soldr version, and daemon image digest. The SESSION
//! endpoint a launcher expects, however, is derived from the *executable path*
//! of the image it placed (`broker_identity::daemon_session_endpoint_from_executable`),
//! and one image digest can sit at more than one path: the placement directory
//! carries the placing broker's own version and build provenance
//! (`self_relocate::relocation_dir_name`), and the file name is the
//! registered source's. A daemon placed by one broker build survives a broker
//! restart, keeps this generation's root-owner lock, and publishes its claim
//! into the same slot.
//!
//! Comparing the claim's endpoint against the path-derived one therefore made
//! every later request for the generation wait out its whole acquisition
//! window on `daemon route claim endpoint mismatch`, while the generation's one
//! daemon was live and serving. Identity is the image digest, not a path: a
//! claim is adopted when its recorded BLAKE3 equals the route's image digest
//! and `BackendHandle::probe_with_service` then verifies, at the claim's own
//! endpoint, that the live PID runs exactly that executable (path, hash, boot
//! ID) and answers the nonce challenge. A claim for another image is never
//! adopted, so foreign generations are left untouched.

use super::backend_handle_adoption::read_broker_route_claim;
use crate::core::SoldrPaths;
use running_process::broker::backend_handle::{BackendHandle, BackendHandleError, DaemonProcess};
use running_process::broker::protocol::Endpoint;
use std::fmt;
use std::io;
use std::time::{Duration, Instant};

/// Why a route claim was not adopted.
#[derive(Debug)]
pub enum ClaimAdoptionError {
    /// The claim names a daemon of a different image: another generation's
    /// daemon, or a stale entry. Not corrupt, and never adopted or pruned here.
    ForeignImage {
        claimed_blake3: String,
        route_blake3: String,
    },
    /// The claim names this route's image but the exact probe failed: the
    /// daemon is gone, still starting, or the claim lies about it.
    Probe(BackendHandleError),
}

impl fmt::Display for ClaimAdoptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignImage {
                claimed_blake3,
                route_blake3,
            } => write!(
                f,
                "daemon route claim names a different image: claimed blake3={claimed_blake3}, route blake3={route_blake3}"
            ),
            Self::Probe(error) => write!(f, "daemon route claim failed its exact probe: {error}"),
        }
    }
}

/// True when `claim` records the daemon image whose BLAKE3 is
/// `route_image_blake3_hex` (the route's `soldr-image-blake3` label).
///
/// The recorded hash alone is only a statement; it becomes evidence once
/// [`adopt_route_generation_claim`]'s probe has checked the live executable
/// against it.
pub fn claim_matches_route_image(
    claim: &DaemonProcess,
    route_image_blake3_hex: &str,
) -> Result<(), ClaimAdoptionError> {
    let claimed_blake3 = kernal_api::hash::Blake3Digest::from_bytes(claim.exe_hash).to_hex();
    if claimed_blake3.eq_ignore_ascii_case(route_image_blake3_hex) {
        Ok(())
    } else {
        Err(ClaimAdoptionError::ForeignImage {
            claimed_blake3,
            route_blake3: route_image_blake3_hex.to_string(),
        })
    }
}

/// Adopt `claim` as the route's daemon when it is the route's image and the
/// live process proves it at the claim's own endpoint. The one decision every
/// launcher path shares: wherever the daemon's executable happens to live,
/// the generation converges on it.
pub fn adopt_route_generation_claim(
    service_name: &str,
    service_version: &str,
    claim: &DaemonProcess,
    route_image_blake3_hex: &str,
) -> Result<BackendHandle, ClaimAdoptionError> {
    claim_matches_route_image(claim, route_image_blake3_hex)?;
    BackendHandle::probe_with_service(
        service_name.to_string(),
        service_version.to_string(),
        &claim.ipc_endpoint,
        claim,
    )
    .map_err(ClaimAdoptionError::Probe)
}

/// Wait for the route generation's daemon to publish an authenticated claim.
///
/// `endpoint` is where the caller's own child was told to listen. The claim may
/// name another endpoint when the generation's daemon was placed at a different
/// executable path; that daemon is adopted once it authenticates as this
/// route's image. An actual child exit ends the wait unless the generation's
/// daemon is already serving, in which case the request converges on it.
#[allow(clippy::too_many_arguments)]
pub fn wait_for_broker_backend_handle_while(
    paths: &SoldrPaths,
    service_name: &str,
    service_version: &str,
    route_image_blake3_hex: &str,
    endpoint: &Endpoint,
    timeout: Duration,
    child_status: impl FnMut() -> io::Result<Option<i32>>,
    progress: impl FnMut(&str),
) -> io::Result<BackendHandle> {
    wait_for_route_claim_while(paths, endpoint, timeout, child_status, progress, |claim| {
        adopt_route_generation_claim(service_name, service_version, claim, route_image_blake3_hex)
    })
}

/// The polling loop behind [`wait_for_broker_backend_handle_while`], with the
/// adoption decision injected so it can be exercised without a live daemon.
pub(crate) fn wait_for_route_claim_while<T, E: fmt::Display>(
    paths: &SoldrPaths,
    endpoint: &Endpoint,
    timeout: Duration,
    mut child_status: impl FnMut() -> io::Result<Option<i32>>,
    mut progress: impl FnMut(&str),
    mut adopt: impl FnMut(&DaemonProcess) -> Result<T, E>,
) -> io::Result<T> {
    let deadline = Instant::now() + timeout;
    let mut next_progress = Instant::now() + Duration::from_secs(1);
    let mut last_error = "daemon has not published its protobuf route claim yet".to_string();
    loop {
        // Sampled before the claim is read: a child that exits because the
        // generation's daemon already serves must not hide that daemon.
        let exited = child_status()?;
        match read_broker_route_claim(paths) {
            Ok(Some(claim)) => match adopt(&claim) {
                Ok(handle) => {
                    if claim.ipc_endpoint != *endpoint {
                        tracing::info!(
                            claimed = %claim.ipc_endpoint.path,
                            expected = %endpoint.path,
                            pid = claim.pid,
                            "converged on the route generation's authenticated daemon \
                             at its claimed endpoint (soldr#3561)"
                        );
                    }
                    return Ok(handle);
                }
                Err(error) if claim.ipc_endpoint != *endpoint => {
                    last_error = format!(
                        "daemon route claim endpoint mismatch: claimed={}, expected={}; \
                         claimed daemon not adopted: {error}",
                        claim.ipc_endpoint.path, endpoint.path
                    );
                }
                Err(error) => last_error = error.to_string(),
            },
            Ok(None) => {}
            Err(error) => last_error = format!("daemon route claim is unreadable: {error}"),
        }
        if let Some(status) = exited {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!(
                    "broker-launched soldr-daemon exited before readiness ({status}): {last_error}"
                ),
            ));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "broker-launched soldr-daemon was not ready within {timeout:?}: {last_error}"
                ),
            ));
        }
        if Instant::now() >= next_progress {
            progress(&last_error);
            next_progress = Instant::now() + Duration::from_secs(1);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
#[path = "route_claim_convergence_tests.rs"]
mod tests;
