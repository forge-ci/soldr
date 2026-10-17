//! Soldr-owned broker launcher.
//!
//! `running_process::CommandBackendLauncher` intentionally starts children
//! from a user-baseline environment. Soldr's root and identity live in the
//! process-local `SOLDR_*` namespace, so the broker must apply that namespace
//! explicitly when it creates `soldr-daemon`.

use crate::platform::sync::Mutex;
use running_process::broker::backend_handle::BackendHandle;
use running_process::broker::protocol::Endpoint;
use running_process::broker::server::{
    BackendKey, BackendLaunchError, BackendLaunchRequest, BackendLauncher, BrokerInstanceKey,
    CombinedServiceDefinitionLoader, TraceContext, BACKEND_ENV_ENDPOINT_NAMESPACE,
    BACKEND_ENV_ENDPOINT_PATH, BACKEND_ENV_INSTANCE, BACKEND_ENV_SERVICE_NAME,
    BACKEND_ENV_SERVICE_VERSION, BACKEND_ENV_TRACEPARENT, BACKEND_ENV_TRACESTATE,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

// A cold embedded-zccache startup can exceed the old 25-second budget on
// emulated macOS x86 runners. Keep this bounded, while leaving enough room for
// the daemon to publish its authenticated route claim.
const DAEMON_ROUTE_READINESS_TIMEOUT: Duration = Duration::from_secs(45);

pub(crate) struct SoldrBackendLauncher {
    placed_images: Mutex<HashMap<String, PlacedImage>>,
    route_deadlines: Mutex<HashMap<String, std::time::Instant>>,
    progress: tokio::sync::broadcast::Sender<LauncherProgress>,
}

#[derive(Clone, Debug)]
pub(crate) struct LauncherProgress {
    pub(crate) service_name: String,
    pub(crate) stage: &'static str,
    pub(crate) latest_result: String,
}

struct PlacedImage {
    path: PathBuf,
    source_fingerprint: FileFingerprint,
    placed_fingerprint: FileFingerprint,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileFingerprint {
    len: u64,
    modified_nanos: u128,
}

impl SoldrBackendLauncher {
    pub(crate) fn new() -> Self {
        let (progress, _) = tokio::sync::broadcast::channel(256);
        Self {
            placed_images: Mutex::new(HashMap::new()),
            route_deadlines: Mutex::new(HashMap::new()),
            progress,
        }
    }

    pub(crate) fn note_route_deadline(&self, service_name: &str, deadline: std::time::Instant) {
        let mut deadlines = self.route_deadlines.lock();
        deadlines
            .entry(service_name.to_string())
            .and_modify(|current| *current = (*current).max(deadline))
            .or_insert(deadline);
    }

    fn ensure_route_deadline(
        &self,
        request: &BackendLaunchRequest<'_>,
    ) -> Result<(), BackendLaunchError> {
        let deadline = self
            .route_deadlines
            .lock()
            .get(&request.key.service_name)
            .copied();
        if deadline.is_some_and(|value| std::time::Instant::now() >= value) {
            return Err(BackendLaunchError::Launcher(format!(
                "route {} expired before daemon launch completed",
                request.key.service_name
            )));
        }
        Ok(())
    }

    pub(crate) fn subscribe_progress(&self) -> tokio::sync::broadcast::Receiver<LauncherProgress> {
        self.progress.subscribe()
    }

    /// Rehydrate a requested control route without launching a missing daemon.
    /// This is the admin-path counterpart to the Hello launcher's claim-first
    /// behavior: a broker restart can answer status/stop for a surviving daemon,
    /// while a genuinely absent route stays absent.
    pub(crate) fn adopt_existing_control_route(
        &self,
        loader: &CombinedServiceDefinitionLoader,
        service_name: &str,
    ) -> Result<Option<(BrokerInstanceKey, BackendHandle)>, String> {
        crate::daemon::backend_handle_adoption::with_generation_key(service_name, || {
            self.adopt_existing_control_route_keyed(loader, service_name)
        })
    }

    fn adopt_existing_control_route_keyed(
        &self,
        loader: &CombinedServiceDefinitionLoader,
        service_name: &str,
    ) -> Result<Option<(BrokerInstanceKey, BackendHandle)>, String> {
        let definition = loader
            .lookup_or_reload(service_name)
            .map_err(|error| error.to_string())?;
        let instance = BrokerInstanceKey::from_service_definition(&definition)
            .map_err(|error| error.to_string())?;
        let key = BackendKey::new(
            instance.clone(),
            service_name,
            crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_VERSION,
            "control-re-adoption",
        );
        let trace = TraceContext::default();
        let request = BackendLaunchRequest {
            key: &key,
            service_definition: &definition,
            trace_context: &trace,
            session_token: None,
        };
        let paths = routed_paths(&request).map_err(|error| error.to_string())?;
        // The persisted claim already carries the executable path and digest
        // that BackendHandle verifies. Re-adoption must fit inside a normal
        // control request's timeout, so do not re-hash and re-place a large
        // source image after every broker restart. Constrain the claimed path
        // to this route's broker-owned runtime before probing it.
        let claim = match crate::daemon::backend_handle_adoption::read_broker_route_claim(&paths)
            .map_err(|error| error.to_string())?
        {
            Some(claim) => claim,
            None => return Ok(None),
        };
        let claimed_binary = std::fs::canonicalize(&claim.exe_path).ok();
        let route_runtime = crate::self_relocate::daemon_runtime_root(&route_image_paths(&request));
        let route_runtime = std::fs::canonicalize(&route_runtime).unwrap_or(route_runtime);
        if claimed_binary
            .as_deref()
            .is_none_or(|path| !path.starts_with(&route_runtime))
        {
            // The claim is root-scoped, not route-scoped. A different daemon
            // image may legitimately own this root while its replacement is
            // being registered. Preserve that foreign claim so the lifecycle
            // preflight can identify and retire the incumbent.
            return Ok(None);
        }
        Ok(self
            .adopt_route_claim(&request, &paths, false)
            .map(|handle| (instance, handle)))
    }

    fn report_progress(
        &self,
        request: &BackendLaunchRequest<'_>,
        stage: &'static str,
        latest_result: impl Into<String>,
    ) {
        let _ = self.progress.send(LauncherProgress {
            service_name: request.key.service_name.clone(),
            stage,
            latest_result: latest_result.into(),
        });
    }
}

impl BackendLauncher for SoldrBackendLauncher {
    fn launch(
        &self,
        request: &BackendLaunchRequest<'_>,
    ) -> Result<BackendHandle, BackendLaunchError> {
        // soldr#3374: one broker serves every route, so claim state is keyed
        // by the route being launched, never by the broker's own environment.
        crate::daemon::backend_handle_adoption::with_generation_key(
            &request.key.service_name,
            || self.launch_keyed(request),
        )
    }
}

impl SoldrBackendLauncher {
    #[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
    fn launch_keyed(
        &self,
        request: &BackendLaunchRequest<'_>,
    ) -> Result<BackendHandle, BackendLaunchError> {
        let started = std::time::Instant::now();
        let debug = crate::broker_debug::broker_debug_enabled();
        if debug {
            eprintln!(
                "soldr broker: launch begin route={}",
                request.key.service_name
            );
        }
        self.report_progress(
            request,
            "route-request",
            "broker accepted the route request",
        );
        self.ensure_route_deadline(request)?;
        let paths = routed_paths(request)?;
        let source_binary = canonical_backend_binary(request)?;
        self.report_progress(
            request,
            "image-source-resolved",
            format!("resolved daemon image {}", source_binary.display()),
        );
        // The broker is the sole owner of daemon placement.  Resolve the
        // request's allow-listed source image into this route's stable runtime
        // tree before spawning so the child PID registered below is the
        // long-lived daemon, never a short-lived self-relocation trampoline.
        // `ensure_daemon_relocated` serializes concurrent copies under the
        // route root; running-process serializes launch for the backend key.
        let binary_path = self.place_backend_image(request, &source_binary)?;
        self.ensure_route_deadline(request)?;
        self.report_progress(
            request,
            "image-verified",
            format!("verified daemon image {}", binary_path.display()),
        );
        if let Some(handle) = self.adopt_route_claim(request, &paths, true) {
            self.report_progress(
                request,
                "claim-adopted",
                format!("verified live daemon pid={}", handle.daemon_process.pid),
            );
            if debug {
                eprintln!(
                    "soldr broker: route re-adopted route={} pid={} elapsed={:?}",
                    request.key.service_name,
                    handle.daemon_process.pid,
                    started.elapsed()
                );
            }
            return Ok(handle);
        }
        let endpoint =
            crate::broker_identity::daemon_session_endpoint_from_executable(&binary_path)
                .map_err(|error| BackendLaunchError::Launcher(error.to_string()))?;
        if debug {
            eprintln!(
                "soldr broker: image ready route={} elapsed={:?}",
                request.key.service_name,
                started.elapsed()
            );
        }
        let mut command = std::process::Command::new(&binary_path);
        // soldr#3516: keep the daemon under the broker's own HOME. Applied
        // first so the route's `SOLDR_*` labels and root below still win.
        command.envs(crate::broker_spawn::daemon_home_identity_env());
        let daemon_env = crate::daemon::service_definition::daemon_env_from_labels(
            &request.service_definition.labels,
        )
        .map_err(|err| BackendLaunchError::Launcher(err.to_string()))?;
        command.envs(daemon_env);
        command.env(crate::core::SOLDR_CACHE_DIR_ENV_VAR, &paths.root);
        configure_backend_command(&mut command, request, &endpoint);
        let daemon_log_path = paths.root.join("daemon-spawn.log");
        let log = crate::broker_spawn::open_append(&daemon_log_path).ok_or_else(|| {
            BackendLaunchError::Launcher(format!(
                "could not open daemon log under {}",
                paths.root.display()
            ))
        })?;
        // spawn_blocking cannot cancel a running OS thread. Re-check the
        // original connection's hard deadline at the last possible point so
        // a timed-out route worker cannot materialize a daemon afterward.
        self.ensure_route_deadline(request)?;
        let mut child = running_process::spawn_daemon_with_stdio_and_env_policy(
            &mut command,
            crate::broker_spawn::daemon_stdio(&log),
            running_process::EnvironmentPolicy::UserBaseline,
        )
        .map_err(BackendLaunchError::Spawn)?;
        self.report_progress(
            request,
            "child-spawned",
            format!("spawned daemon child pid={}", child.id()),
        );
        if debug {
            eprintln!(
                "soldr broker: daemon spawned route={} elapsed={:?}",
                request.key.service_name,
                started.elapsed()
            );
        }

        self.report_progress(
            request,
            "readiness-probe",
            "daemon child is alive; waiting for exact endpoint and nonce verification",
        );

        match crate::daemon::backend_handle_adoption::wait_for_broker_backend_handle_while(
            &paths,
            &request.key.service_name,
            &request.key.service_version,
            route_image_hash(request)?,
            &endpoint,
            DAEMON_ROUTE_READINESS_TIMEOUT,
            || child.try_wait(),
            |latest_result| {
                self.report_progress(request, "readiness-probe", latest_result.to_string());
            },
        ) {
            Ok(handle) => {
                // spawn_blocking cannot cancel a running OS thread, so a
                // timed-out route worker re-checks the connection deadline
                // here too: a daemon that became ready after the caller gave
                // up must not be published as this route's backend.
                if let Err(error) = self.ensure_route_deadline(request) {
                    let _ = child.kill();
                    reap_on_exit(child);
                    return Err(error);
                }
                // soldr#3561: the request converged on the generation's
                // already-serving daemon, which this child cannot displace
                // (one daemon owns a root); retire the redundant child.
                if handle.daemon_process.pid != child.id() {
                    let _ = child.kill();
                }
                reap_on_exit(child);
                if debug {
                    eprintln!(
                        "soldr broker: route ready route={} elapsed={:?}",
                        request.key.service_name,
                        started.elapsed()
                    );
                }
                Ok(handle)
            }
            Err(err) => {
                let _ = child.kill();
                reap_on_exit(child);
                drop(log);
                let err = daemon_launch_failure(&err, &daemon_log_path);
                if debug {
                    eprintln!(
                        "soldr broker: route failed route={} elapsed={:?}: {err}",
                        request.key.service_name,
                        started.elapsed()
                    );
                }
                Err(BackendLaunchError::Launcher(err))
            }
        }
    }
}

/// Wait on a spawned daemon from a background thread so its exit is reaped.
///
/// The daemon detaches with `setsid`, not a double fork, so this broker stays
/// its kernel parent. Dropping the handle without waiting left every exited
/// daemon a zombie under a long-lived broker until the broker itself died:
/// 48 on one host, 100+ on another (soldr#3057, soldr#3075). The reaper thread
/// is per daemon and parks in `wait`, so it costs nothing while the daemon runs.
/// The handle is returned only so a test can join it.
fn reap_on_exit(mut child: running_process::DaemonChild) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("soldr-daemon-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .ok()
}

fn daemon_launch_failure(error: &std::io::Error, log_path: &std::path::Path) -> String {
    let mut message = error.to_string();
    let Ok(log) = std::fs::read_to_string(log_path) else {
        return message;
    };
    let excerpt = log
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" | ");
    if !excerpt.is_empty() {
        message.push_str("; daemon startup log: ");
        message.push_str(&excerpt);
    }
    message
}

impl SoldrBackendLauncher {
    fn adopt_route_claim(
        &self,
        request: &BackendLaunchRequest<'_>,
        paths: &crate::core::SoldrPaths,
        prune_invalid: bool,
    ) -> Option<BackendHandle> {
        crate::daemon::backend_handle_adoption::with_generation_key(
            &request.key.service_name,
            || self.adopt_route_claim_keyed(request, paths, prune_invalid),
        )
    }

    fn adopt_route_claim_keyed(
        &self,
        request: &BackendLaunchRequest<'_>,
        paths: &crate::core::SoldrPaths,
        prune_invalid: bool,
    ) -> Option<BackendHandle> {
        use crate::daemon::backend_handle_adoption::ClaimAdoptionError;

        // soldr#3685: corrupt claims are pruned by compare-and-delete; an IO
        // error is inconclusive and keeps the claim.
        let (claim, claim_bytes) =
            crate::daemon::backend_handle_adoption::read_route_claim_pruning_corrupt(
                paths,
                prune_invalid,
            )?;
        let route_image = route_image_hash(request).ok()?;
        // soldr#3561: identity is the image digest, proven by the exact probe
        // at the claim's own endpoint (PID, executable path and hash, boot ID,
        // nonce) -- never the executable path the caller would have placed,
        // which differs between placements of one image.
        match crate::daemon::backend_handle_adoption::adopt_route_generation_claim(
            &request.key.service_name,
            &request.key.service_version,
            &claim,
            route_image,
        ) {
            Ok(handle) => Some(handle),
            Err(error @ ClaimAdoptionError::ForeignImage { .. }) => {
                if crate::broker_debug::broker_debug_enabled() {
                    eprintln!(
                        "soldr broker: route claim identity mismatch claimed_binary={} {error}",
                        claim.exe_path.display()
                    );
                }
                // A root-scoped claim for another image route is not corrupt.
                // Deleting it here prevents preflight from naming the process
                // that owns the root lock during an image transition.
                None
            }
            Err(error @ ClaimAdoptionError::Probe(_)) => {
                prune_route_claim_after_probe_failure(paths, &claim_bytes, &error, prune_invalid);
                None
            }
        }
    }

    #[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
    fn place_backend_image(
        &self,
        request: &BackendLaunchRequest<'_>,
        source_binary: &std::path::Path,
    ) -> Result<PathBuf, BackendLaunchError> {
        let image_hash = route_image_hash(request)?;
        let cache_key = format!(
            "{}\0{}\0{image_hash}",
            request.key.service_name,
            source_binary.display()
        );
        let lock_started = std::time::Instant::now();
        let mut next_lock_progress = Duration::from_secs(1);
        let mut placed = loop {
            match self.placed_images.try_lock() {
                Some(placed) => break placed,
                None => {
                    let elapsed = lock_started.elapsed();
                    if elapsed >= next_lock_progress {
                        self.report_progress(
                            request,
                            "image-placement-wait",
                            format!(
                                "waiting for concurrent daemon image verification ({}ms)",
                                elapsed.as_millis()
                            ),
                        );
                        next_lock_progress += Duration::from_secs(1);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        };
        let source_fingerprint = file_fingerprint(source_binary).map_err(|err| {
            BackendLaunchError::Launcher(format!(
                "could not inspect registered soldr-daemon image {}: {err}",
                source_binary.display()
            ))
        })?;
        if let Some(image) = placed.get(&cache_key) {
            let placed_unchanged = file_fingerprint(&image.path)
                .is_ok_and(|fingerprint| fingerprint == image.placed_fingerprint);
            if image.source_fingerprint == source_fingerprint && placed_unchanged {
                return Ok(image.path.clone());
            }
            placed.remove(&cache_key);
        }
        // soldr#2442 / 0.9.0: blake3 (via zccache's shared hasher) with a
        // (path,size,mtime) cache instead of a whole-file SHA-256 read, so a
        // warm image is not re-read on every launch. Matches the registration
        // side's algorithm so the label comparison below still holds.
        self.report_progress(
            request,
            "image-hash",
            format!(
                "verifying daemon image hash for {}",
                source_binary.display()
            ),
        );
        let mut last_hash_progress = std::time::Instant::now();
        let source_hash = crate::daemon::image_hash::cached_blake3_hex_with_progress(
            // soldr#2521 B1: machine-scoped memo so isolated roots share hits.
            &crate::daemon::image_hash::machine_scoped_cache_dir(
                &crate::daemon::service_definition::broker_owned_paths()
                    .cache
                    .join("image-hash"),
            ),
            source_binary,
            |completed, total| {
                if last_hash_progress.elapsed() >= Duration::from_millis(500) || completed >= total
                {
                    self.report_progress(
                        request,
                        "image-hash",
                        format!("daemon image hash: {completed}/{total} bytes"),
                    );
                    last_hash_progress = std::time::Instant::now();
                }
            },
        )
        .map_err(|err| {
            BackendLaunchError::Launcher(format!(
                "could not hash registered soldr-daemon image {}: {err}",
                source_binary.display()
            ))
        })?;
        if !source_hash.eq_ignore_ascii_case(image_hash) {
            return Err(BackendLaunchError::Launcher(format!(
                "registered soldr-daemon image changed before broker launch: expected {image_hash}, got {source_hash} for {}",
                source_binary.display()
            )));
        }
        if file_fingerprint(source_binary).ok() != Some(source_fingerprint) {
            return Err(BackendLaunchError::Launcher(format!(
                "registered soldr-daemon image changed while it was being verified: {}",
                source_binary.display()
            )));
        }
        self.report_progress(
            request,
            "image-hash-verified",
            format!("verified daemon image hash for {}", source_binary.display()),
        );
        // Every route gets its own real executable path. The SESSION,
        // control, and handoff endpoint names are derived from that path, so
        // distinct soldr roots cannot collide even when they use the same
        // installed source image.
        let image_paths = route_image_paths(request);
        self.report_progress(
            request,
            "image-placement",
            "placing the verified daemon image in the broker-owned runtime",
        );
        let mut last_placement_progress = std::time::Instant::now();
        let path = crate::self_relocate::ensure_daemon_relocated_for_route_with_progress(
            &image_paths,
            source_binary,
            |operation, completed, total| {
                if last_placement_progress.elapsed() >= Duration::from_millis(500)
                    || completed >= total
                {
                    self.report_progress(
                        request,
                        "image-placement",
                        format!("daemon image {operation}: {completed}/{total} bytes"),
                    );
                    last_placement_progress = std::time::Instant::now();
                }
            },
        )
        .map_err(|err| {
            BackendLaunchError::Launcher(format!(
                "could not place soldr-daemon image {} for route {}: {err}",
                source_binary.display(),
                request.key.service_name
            ))
        })?;
        let placed_fingerprint = file_fingerprint(&path).map_err(|err| {
            BackendLaunchError::Launcher(format!(
                "could not inspect broker-owned soldr-daemon image {}: {err}",
                path.display()
            ))
        })?;
        // soldr#2442 / 0.9.0: the source was already hash-verified against the
        // label above, and a same-filesystem copy is byte-faithful, so a full
        // re-hash of the placed copy (a third whole-file read of a large binary
        // on the cold path) is redundant. Guard the copy with its (size, mtime)
        // fingerprint, which catches a truncated or replaced file.
        if file_fingerprint(&path).ok() != Some(placed_fingerprint) {
            return Err(BackendLaunchError::Launcher(format!(
                "broker-owned soldr-daemon image changed while it was being verified: {}",
                path.display()
            )));
        }
        placed.insert(
            cache_key,
            PlacedImage {
                path: path.clone(),
                source_fingerprint,
                placed_fingerprint,
            },
        );
        Ok(path)
    }
}

/// The directory every broker route's staged daemon image lives under.
pub(crate) fn routes_root() -> PathBuf {
    crate::daemon::service_definition::broker_owned_paths()
        .root
        .join("routes")
}

/// The route's daemon image digest (`soldr-image-blake3` label): the identity a
/// placed image is verified against and a route claim is adopted by.
fn route_image_hash<'a>(
    request: &'a BackendLaunchRequest<'_>,
) -> Result<&'a str, BackendLaunchError> {
    request
        .service_definition
        .labels
        .get(crate::daemon::backend_handle_adoption::SOLDR_DAEMON_IMAGE_HASH_LABEL)
        .map(String::as_str)
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| {
            BackendLaunchError::Launcher(
                "soldr daemon service definition is missing a valid BLAKE3 image hash".to_string(),
            )
        })
}

fn route_image_paths(request: &BackendLaunchRequest<'_>) -> crate::core::SoldrPaths {
    crate::core::SoldrPaths::with_root(routes_root().join(&request.key.service_name))
}

fn file_fingerprint(path: &std::path::Path) -> std::io::Result<FileFingerprint> {
    use std::time::UNIX_EPOCH;

    let metadata = std::fs::metadata(path)?;
    let modified_nanos = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_nanos();
    Ok(FileFingerprint {
        len: metadata.len(),
        modified_nanos,
    })
}

fn routed_paths(
    request: &BackendLaunchRequest<'_>,
) -> Result<crate::core::SoldrPaths, BackendLaunchError> {
    let root = request
        .service_definition
        .labels
        .get(crate::daemon::service_definition::SOLDR_ROOT_SERVICE_LABEL)
        .filter(|root| !root.is_empty())
        .ok_or_else(|| {
            BackendLaunchError::Launcher(
                "soldr daemon service definition is missing its soldr-root route".to_string(),
            )
        })?;
    Ok(crate::core::SoldrPaths::with_root(PathBuf::from(root)))
}

fn canonical_backend_binary(
    request: &BackendLaunchRequest<'_>,
) -> Result<PathBuf, BackendLaunchError> {
    let definition = request.service_definition;
    if definition.binary_path.is_empty() {
        return Err(BackendLaunchError::EmptyBinaryPath);
    }
    if definition.per_version_binary_dir.is_empty() {
        return Err(BackendLaunchError::EmptyPerVersionBinaryDir);
    }
    let path = PathBuf::from(&definition.binary_path);
    let binary = std::fs::canonicalize(&path)
        .map_err(|source| BackendLaunchError::CanonicalizeBinary { path, source })?;
    let path = PathBuf::from(&definition.per_version_binary_dir);
    let root = std::fs::canonicalize(&path)
        .map_err(|source| BackendLaunchError::CanonicalizeBinaryRoot { path, source })?;
    if !binary.starts_with(&root) {
        return Err(BackendLaunchError::BinaryOutsideAllowRoot { binary, root });
    }
    Ok(binary)
}

fn configure_backend_command(
    command: &mut std::process::Command,
    request: &BackendLaunchRequest<'_>,
    endpoint: &Endpoint,
) {
    command
        .env(BACKEND_ENV_SERVICE_NAME, &request.key.service_name)
        .env(BACKEND_ENV_SERVICE_VERSION, &request.key.service_version)
        .env(BACKEND_ENV_ENDPOINT_PATH, &endpoint.path)
        .env(BACKEND_ENV_ENDPOINT_NAMESPACE, &endpoint.namespace_id)
        .env(BACKEND_ENV_INSTANCE, request.key.instance.id());
    command.env(
        crate::daemon::session_endpoint::SOLDR_SESSION_ENDPOINT_PATH_ENV,
        &endpoint.path,
    );
    command.env(
        crate::daemon::session_endpoint::SOLDR_CONTROL_ENDPOINT_PATH_ENV,
        crate::daemon::session_endpoint::private_control_endpoint_from_session(&endpoint.path),
    );
    if !request.trace_context.traceparent.is_empty() {
        command.env(BACKEND_ENV_TRACEPARENT, &request.trace_context.traceparent);
    }
    if !request.trace_context.tracestate.is_empty() {
        command.env(BACKEND_ENV_TRACESTATE, &request.trace_context.tracestate);
    }
}

/// soldr#3684: the broker prunes a route claim only when the exact probe gave
/// a definitive "no". A probe that never got an answer (timeout, transient IO)
/// says nothing about a loaded daemon, so its claim must survive -- the same
/// rule `probe_error_is_transient` (soldr#1893) applies on the daemon side.
/// Returns whether the claim was pruned.
fn prune_route_claim_after_probe_failure(
    paths: &crate::core::SoldrPaths,
    claim_bytes: &[u8],
    error: &crate::daemon::backend_handle_adoption::ClaimAdoptionError,
    prune_invalid: bool,
) -> bool {
    use crate::daemon::backend_handle_adoption::{probe_error_is_transient, ClaimAdoptionError};
    let ClaimAdoptionError::Probe(probe) = error else {
        return false;
    };
    let debug = crate::broker_debug::broker_debug_enabled();
    if probe_error_is_transient(probe) {
        if debug {
            eprintln!("soldr broker: daemon route claim probe inconclusive, claim kept: {error}");
        }
        return false;
    }
    if debug {
        eprintln!("soldr broker: daemon route claim failed exact probe and was pruned: {error}");
    }
    if prune_invalid {
        return crate::daemon::backend_handle_adoption::prune_broker_route_claim_if_unchanged(
            paths,
            claim_bytes,
        )
        .unwrap_or(false);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use running_process::broker::protocol::ServiceDefinition;
    use running_process::broker::protocol_v2::ServiceDefinitionBuilder;
    use running_process::broker::server::service_definition_v2_to_v1;
    use running_process::broker::server::{BackendKey, BrokerInstanceKey, TraceContext};

    fn request_parts(
        root: &std::path::Path,
        binary: &std::path::Path,
        image_hash: &str,
    ) -> (ServiceDefinition, BackendKey, TraceContext) {
        let mut definition = service_definition_v2_to_v1(
            ServiceDefinitionBuilder::shared_broker(
                "soldr-daemon-test",
                binary.display().to_string(),
            )
            .per_version_binary_dir(
                binary
                    .parent()
                    .expect("binary parent")
                    .display()
                    .to_string(),
            )
            .min_version("1.0.0")
            .version_allow_list(["1.0.0"])
            .build(),
        );
        definition.labels.insert(
            crate::daemon::service_definition::SOLDR_ROOT_SERVICE_LABEL.into(),
            root.display().to_string(),
        );
        definition.labels.insert(
            crate::daemon::backend_handle_adoption::SOLDR_DAEMON_IMAGE_HASH_LABEL.into(),
            image_hash.into(),
        );
        let key = BackendKey::new(BrokerInstanceKey::Shared, "soldr-daemon-test", "1.0.0", "");
        (definition, key, TraceContext::default())
    }

    // soldr#3684: a probe timeout is inconclusive and must not prune a live
    // daemon's route claim; a definitive failure still prunes it.
    fn claim_probe_error(
        inner: running_process::broker::backend_lifecycle::probe::EndpointProbeError,
    ) -> crate::daemon::backend_handle_adoption::ClaimAdoptionError {
        crate::daemon::backend_handle_adoption::ClaimAdoptionError::Probe(
            running_process::broker::backend_handle::BackendHandleError::Probe(
                running_process::broker::backend_lifecycle::probe::ProbeError::EndpointResponse(
                    inner,
                ),
            ),
        )
    }

    fn seeded_claim(temp: &tempfile::TempDir) -> (crate::core::SoldrPaths, PathBuf) {
        let paths = crate::core::SoldrPaths::with_root(temp.path().join("root"));
        let claim = crate::daemon::backend_handle_adoption::broker_route_claim_path(&paths);
        std::fs::create_dir_all(claim.parent().expect("claim parent")).expect("claim dir");
        std::fs::write(&claim, b"claim").expect("write claim");
        (paths, claim)
    }

    #[test]
    fn probe_timeout_keeps_route_claim() {
        use running_process::broker::backend_lifecycle::probe::EndpointProbeError;
        let temp = tempfile::tempdir().expect("tempdir");
        let (paths, claim) = seeded_claim(&temp);
        let pruned = prune_route_claim_after_probe_failure(
            &paths,
            b"claim",
            &claim_probe_error(EndpointProbeError::Timeout),
            true,
        );
        assert!(!pruned);
        assert!(claim.exists(), "a probe timeout must not prune the claim");
    }

    #[test]
    fn definitive_probe_failure_prunes_route_claim() {
        use running_process::broker::backend_lifecycle::probe::EndpointProbeError;
        let temp = tempfile::tempdir().expect("tempdir");
        let (paths, claim) = seeded_claim(&temp);
        let refused = EndpointProbeError::Connect(std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused,
        ));
        assert!(prune_route_claim_after_probe_failure(
            &paths,
            b"claim",
            &claim_probe_error(refused),
            true
        ));
        assert!(!claim.exists(), "a definitive failure must prune the claim");
    }

    #[test]
    fn changed_registered_image_is_rejected_before_spawn() {
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("soldr-daemon");
        std::fs::write(&binary, b"changed image").expect("write image");
        let root = temp.path().join("root");
        let (definition, key, trace_context) = request_parts(&root, &binary, &"0".repeat(64));
        let request = BackendLaunchRequest {
            key: &key,
            service_definition: &definition,
            trace_context: &trace_context,
            session_token: None,
        };

        let err = match SoldrBackendLauncher::new().launch(&request) {
            Ok(_) => panic!("stale image registration must fail"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("changed before broker launch"),
            "{err}"
        );
    }

    #[test]
    fn daemon_launch_failure_surfaces_the_startup_log() {
        let temp = tempfile::tempdir().expect("tempdir");
        let log = temp.path().join("daemon-spawn.log");
        std::fs::write(
            &log,
            "old launch\n\nsoldr-daemon failed: Io(Kind(Unsupported))\n",
        )
        .expect("write daemon log");
        let error = std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "broker-launched soldr-daemon exited before readiness (1)",
        );

        let message = daemon_launch_failure(&error, &log);

        assert!(message.contains("exited before readiness (1)"), "{message}");
        assert!(
            message.contains("soldr-daemon failed: Io(Kind(Unsupported))"),
            "{message}"
        );
    }

    #[test]
    fn foreign_live_route_claim_survives_replacement_preflight() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("root");
        let paths = crate::core::SoldrPaths::with_root(root.clone());
        let endpoint = running_process::broker::protocol::Endpoint::unix_socket(
            "incumbent-test",
            temp.path().join("incumbent.sock").display().to_string(),
        )
        .or_else(|_| {
            running_process::broker::protocol::Endpoint::windows_pipe(
                "incumbent-test",
                "soldr-incumbent-test",
            )
        })
        .expect("incumbent endpoint");
        let incumbent =
            running_process::broker::backend_handle::DaemonProcess::current_process(endpoint, None)
                .expect("incumbent claim");
        crate::daemon::backend_handle_adoption::publish_broker_route_claim(&paths, &incumbent)
            .expect("publish incumbent claim");

        let replacement = temp.path().join("replacement").join("soldr-daemon");
        std::fs::create_dir_all(replacement.parent().expect("replacement parent"))
            .expect("replacement dir");
        std::fs::write(&replacement, b"replacement image").expect("replacement image");
        let (definition, key, trace_context) = request_parts(&root, &replacement, &"0".repeat(64));
        let request = BackendLaunchRequest {
            key: &key,
            service_definition: &definition,
            trace_context: &trace_context,
            session_token: None,
        };

        assert!(
            SoldrBackendLauncher::new()
                .adopt_route_claim(&request, &paths, true)
                .is_none(),
            "a foreign image claim must not be adopted"
        );
        assert!(
            crate::daemon::backend_handle_adoption::broker_route_claim_path(&paths).exists(),
            "replacement preflight still needs the incumbent PID/image claim"
        );
    }

    /// soldr#3057: an exited daemon child must be reaped, not left a zombie
    /// under the broker that spawned it.
    #[test]
    fn an_exited_daemon_child_is_reaped_rather_than_left_a_zombie() {
        let temp = tempfile::tempdir().expect("temp dir");
        let log = crate::broker_spawn::open_append(&temp.path().join("daemon-spawn.log"))
            .expect("daemon log");
        // The test binary itself is a portable short-lived child: `--list`
        // prints the test names and exits.
        let mut command = std::process::Command::new(std::env::current_exe().expect("test exe"));
        command.arg("--list");
        let child = running_process::spawn_daemon_with_stdio_and_env_policy(
            &mut command,
            crate::broker_spawn::daemon_stdio(&log),
            running_process::EnvironmentPolicy::UserBaseline,
        )
        .expect("spawn short-lived daemon child");
        let pid = child.id();
        let reaper = reap_on_exit(child).expect("reaper thread");

        // A zombie is not alive, so this loop ends once the child has exited
        // whether or not anything reaped it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while crate::platform::process::inspect::is_alive(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "child {pid} did not exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        reaper.join().expect("reaper thread joined");

        assert!(
            !crate::platform::process::inspect::is_zombie(pid),
            "daemon child {pid} exited but was never reaped"
        );
    }
}
