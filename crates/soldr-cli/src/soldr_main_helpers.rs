/// How long `soldr daemon start` waits for the broker to publish a daemon
/// route before giving up.
///
/// Named rather than inline so the one test that pins it has something to
/// name, and so the reasoning below lives with the number instead of with the
/// call.
pub(crate) const DAEMON_START_ROUTE_BUDGET: std::time::Duration =
    std::time::Duration::from_secs(180);

/// Stamp the newest `.whl` in `out_dir` (when the build succeeded and an
/// `out_dir` was resolved) that this build just wrote — mtime at or after
/// `build_started` — with a `Generator:` naming soldr (soldr#3433).
/// Best-effort: any problem is a warning, never a build failure, because a
/// missed stamp is a provenance gap, not a broken wheel.
fn maybe_stamp_wheel(code: i32, out_dir: Option<&std::path::Path>, build_started: std::time::SystemTime) {
    let (0, Some(out_dir)) = (code, out_dir) else {
        return;
    };
    let Some(wheel) = newest_built_wheel(out_dir, build_started) else {
        return;
    };
    match crate::wheel_stamp::stamp_wheel_generator(&wheel, env!("CARGO_PKG_VERSION")) {
        Ok(true) => {
            eprintln!(
                "soldr: stamped {} with Generator: soldr {}",
                wheel.display(),
                env!("CARGO_PKG_VERSION")
            );
        }
        Ok(false) => {} // already stamped, nothing to say
        Err(err) => eprintln!(
            "soldr warning: could not stamp {} with soldr's Generator provenance: {err}",
            wheel.display()
        ),
    }
}

/// zackees/soldr#3468: after a successful `soldr wheel` maturin build, stage
/// `[tool.soldr.pep517] bundle-bins` into the wheel it wrote. Unlike the
/// stamp, a failure here fails the build: a wheel missing a declared CLI is
/// a broken artifact. Returns the exit code to use.
fn maybe_stage_bundle_bins(
    code: i32,
    out_dir: Option<&std::path::Path>,
    build_started: std::time::SystemTime,
) -> i32 {
    let (0, Some(out_dir)) = (code, out_dir) else {
        return code;
    };
    let Some(wheel) = newest_built_wheel(out_dir, build_started) else {
        return code;
    };
    match crate::wheel_bundle::stage_requested(&wheel) {
        Ok(()) => code,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

/// The newest `.whl` in `out_dir` written at or after `build_started`.
fn newest_built_wheel(
    out_dir: &std::path::Path,
    build_started: std::time::SystemTime,
) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(out_dir).ok()?;
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "whl"))
        .filter(|entry| {
            entry
                .metadata()
                .and_then(|meta| meta.modified())
                .is_ok_and(|modified| modified >= build_started)
        })
        .max_by_key(|entry| {
            entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        })
        .map(|entry| entry.path())
}

fn acquire_maturin_build_lease(
    paths: &SoldrPaths,
    args: &[String],
) -> Result<Option<crate::cache_lib::build_active::BuildActivityLease>, SoldrError> {
    if !crate::pyo3_detect::maturin_args_are_build(args) {
        return Ok(None);
    }
    crate::cache_lib::build_active::BuildActivityLease::acquire(
        paths,
        crate::cargo_front_door::generate_build_session_id(),
    )
    .map(Some)
    .map_err(|error| {
        SoldrError::Other(format!(
            "failed to acquire maturin build activity lease: {error}"
        ))
    })
}

fn run_build_lease_helper() -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let _lease = crate::cache_lib::build_active::BuildActivityLease::acquire(
        &paths,
        crate::cargo_front_door::generate_build_session_id(),
    )
    .map_err(|error| SoldrError::Other(format!("failed to hold build lease: {error}")))?;
    println!("ready");
    std::io::stdout().flush()?;
    std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink())?;
    Ok(())
}

/// Relay a captured child's output to our own stdout/stderr.
///
/// soldr#1878: the flushes are load-bearing. Rust block-buffers stdout when
/// it is not a terminal, and under the PEP 517 backend it is a pipe, so
/// `write_all` alone only fills the buffer. The maturin lane ends in
/// `std::process::exit`, which does not run destructors and therefore never
/// flushes -- so a failing build's captured output was written and then
/// silently discarded, leaving the PEP 517 log with soldr's own unbuffered
/// stderr lines and no maturin output at all.
fn emit_child_output(output: &std::process::Output) {
    let _ = relay_child_output(
        &output.stdout,
        &output.stderr,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
}

/// Write a captured child's streams out and **flush both**.
///
/// Split from [`emit_child_output`] so the flush is testable: the bug in
/// soldr#1878 was an absent flush, which no assertion on the written bytes
/// can detect.
fn relay_child_output<O: Write, E: Write>(
    child_stdout: &[u8],
    child_stderr: &[u8],
    out: &mut O,
    err: &mut E,
) -> std::io::Result<()> {
    out.write_all(child_stdout)?;
    out.flush()?;
    err.write_all(child_stderr)?;
    err.flush()?;
    Ok(())
}

/// soldr#1264 follow-on: maturin provisioning ladder. `auto` (default)
/// tries the prebuilt GitHub-Releases binary and falls back to the
/// manual uv-provisioned isolated env; `binary` / `uv` force one rung.
/// See `fetch::uv_env` for the env-var contract.
async fn fetch_maturin_with_provisioner(
    version: &VersionSpec,
) -> Result<crate::fetch::FetchResult, SoldrError> {
    use crate::fetch::uv_env::MaturinProvisioner;

    let pinned = match version {
        VersionSpec::Exact(v) => v.clone(),
        VersionSpec::Latest => crate::fetch::MANAGED_MATURIN_VERSION.to_string(),
    };

    match MaturinProvisioner::from_env() {
        MaturinProvisioner::Binary => crate::fetch::fetch_tool("maturin", version).await,
        MaturinProvisioner::Uv => provisioned_maturin_fetch_result(&pinned).await,
        MaturinProvisioner::Auto => match crate::fetch::fetch_tool("maturin", version).await {
            Ok(result) => Ok(result),
            Err(err) => {
                eprintln!("soldr: prebuilt maturin fetch failed: {err}");
                eprintln!("soldr: falling back to the uv-provisioned maturin env...");
                provisioned_maturin_fetch_result(&pinned).await
            }
        },
    }
}

async fn provisioned_maturin_fetch_result(
    version: &str,
) -> Result<crate::fetch::FetchResult, SoldrError> {
    let paths = SoldrPaths::new()?;
    let cached = crate::fetch::uv_env::env_is_complete(
        &crate::fetch::uv_env::env_dir_for(
            &paths,
            crate::fetch::uv_env::MATURIN_PYPI_PACKAGE,
            version,
        ),
        "maturin",
    );
    let binary_path = crate::fetch::uv_env::provision_maturin_via_uv(&paths, version).await?;
    Ok(crate::fetch::FetchResult {
        binary_path,
        version: version.to_string(),
        cached,
    })
}

const MATURIN_USE_XWIN_ENV_VAR: &str = "MATURIN_USE_XWIN";

fn maturin_xwin_policy(target: &str, explicit_maturin: Option<&str>) -> Option<&'static str> {
    if explicit_maturin.is_some() || !target.to_ascii_lowercase().ends_with("-pc-windows-msvc") {
        return None;
    }
    Some("0")
}

fn report_and_exit(error: SoldrError) -> i32 {
    eprintln!("soldr: {error}");
    exit_guard::mark_spoke(); // soldr#2024: this IS the explanation.
    1
}

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub(crate) async fn run_daemon_command(command: DaemonSubcommand) -> Result<(), SoldrError> {
    use crate::daemon::client;
    use core::SoldrPaths;

    let paths = SoldrPaths::new()?;
    let sock = client::default_sock_path(&paths);

    match command {
        DaemonSubcommand::Start {
            foreground,
            idle_timeout,
        } => {
            if foreground || idle_timeout != 0 {
                return Err(SoldrError::Other(
                    "`soldr daemon start --foreground/--idle-timeout` is incompatible with the broker-owned daemon model; run `soldr daemon start` and let the singleton broker own placement and lifetime"
                        .to_string(),
                ));
            }
            let daemon = crate::binaries::soldr_daemon_binary()?;
            let installed = register_daemon_service(&daemon)?;
            crate::daemon::lifecycle::preflight_displace_stale_daemon(&paths);
            // Normal SESSION route acquisition remains bounded by
            // `BrokerDeadlines::route_ceiling`: 120s by default, configurable
            // with `SOLDR_ROUTE_ACQUIRE_CEILING_MS`. This explicit `daemon
            // start` command has a separate enclosing caller safeguard because
            // a cold root legitimately covers image staging (a multi-hundred-MB
            // copy), spawn, and the broker launcher's own 45s readiness window.
            // On slow hosts (emulated ARM target-run lanes) the whole chain
            // measures ~32s — real bounded work, not a hang. Formerly, a 30s
            // caller budget fired first with a generic timeout, masking the
            // launcher's more precise attribution. This is a deliberate
            // lifecycle command, not a compile hot path, so the wider safeguard
            // costs nothing when healthy and still fails fast enough when not.
            //
            // soldr#2883: 60s was still too tight for windows-gnu, which is
            // the slowest lane by a wide margin — rustc has no debug-info
            // sidecar for that target (`--print split-debuginfo` → `off`), so
            // its images are ~124 MiB against msvc's ~56 MiB and every process
            // start pays for it. Measured on the target-run lane: the same
            // chain that msvc completes in 20.6s took 64s once (passing at
            // 65.2s overall) and 60.2s+ on the runs that failed, always with
            // `command_dispatch` holding the whole span and
            // `broker_stage_image`/`broker_spawn_wait` each finishing in about
            // a second. That is the budget clipping real, bounded work — the
            // same shape that raised the former explicit-start caller safeguard
            // from 30s, one lane slower.
            //
            // Deliberately not sized to the measurement plus epsilon: 65s
            // observed against a 60s bound is the failure, so a 70s bound
            // would just move the cliff. 180s is chosen to be uninteresting on
            // a healthy host and still bounded, and the launcher keeps its own
            // more precise attribution for anything genuinely wedged.
            await_daemon_route(
                &paths,
                &sock,
                &installed.definition.service_name,
                DAEMON_START_ROUTE_BUDGET,
            )?;
            println!("soldr-daemon: broker route ready");
            Ok(())
        }
        DaemonSubcommand::Stop => {
            match client::shutdown(&sock) {
                Ok(responder) => {
                    let outcome = crate::daemon::lifecycle::wait_for_shutdown_responder(
                        &paths,
                        &sock,
                        responder,
                        crate::daemon::lifecycle::GRACEFUL_SHUTDOWN_WAIT_TIMEOUT,
                    );
                    if outcome.is_complete() {
                        println!("soldr-daemon: stopped");
                        Ok(())
                    } else {
                        Err(SoldrError::Other(format!(
                            "daemon generation {} (pid {}) acknowledged shutdown but is still \
                             completing its graceful flush after {}s; it was not force-killed",
                            responder.generation,
                            responder.pid,
                            crate::daemon::lifecycle::GRACEFUL_SHUTDOWN_WAIT_TIMEOUT.as_secs(),
                        )))
                    }
                }
                Err(client::ClientError::NotRunning) => {
                    println!("soldr-daemon: not running");
                    Ok(())
                }
                Err(e) => {
                    // soldr#1495: current and compatibility wire shutdown both
                    // failed. Let the lifecycle layer retry compatibility IPC
                    // before considering a signal-safe, verified-PID fallback.
                    use crate::daemon::lifecycle::{
                        claimed_daemon_occupies_route, displace_stale_daemon, LifecycleSource,
                    };
                    if claimed_daemon_occupies_route(&paths).is_some()
                        && displace_stale_daemon(&paths, Some(LifecycleSource::Cli))
                    {
                        println!("soldr-daemon: stopped through compatibility displacement");
                        Ok(())
                    } else {
                        Err(SoldrError::Other(format!("daemon stop failed: {e:?}")))
                    }
                }
            }
        }
        DaemonSubcommand::Status { json } => {
            match crate::daemon::lifecycle::status_after_route_ready(
                &paths,
                &sock,
                crate::daemon::lifecycle::STATUS_RETIRING_RETRY_TIMEOUT,
            ) {
                Ok(info) => {
                    crate::daemon_status_render::render(&info, &paths, json);
                    Ok(())
                }
                Err(client::ClientError::NotRunning) => {
                    if json {
                        println!("{}", serde_json::json!({"running": false}));
                    } else {
                        println!("soldr-daemon: not running");
                    }
                    Ok(())
                }
                Err(e) => Err(SoldrError::Other(format!("daemon status failed: {e:?}"))),
            }
        }
        DaemonSubcommand::InstallServiceDef {
            daemon_binary,
            json,
        } => {
            let installed = match daemon_binary {
                Some(path) => crate::daemon::service_definition::install_service_definition(&path),
                None => crate::daemon::service_definition::install_default_service_definition(),
            }
            .map_err(|e| SoldrError::Other(format!("failed to install servicedef: {e}")))?;
            if json {
                let payload = serde_json::json!({
                    "path": installed.path,
                    "service_name": installed.definition.service_name,
                    "binary_path": installed.definition.binary_path,
                    "per_version_binary_dir": installed.definition.per_version_binary_dir,
                    "min_version": installed.definition.min_version,
                    "version_allow_list": installed.definition.version_allow_list,
                    "isolation": "SHARED_BROKER",
                    "deferred": crate::daemon::service_definition::SOLDR_DAEMON_SERVICE_DEF_DEFERRED,
                });
                println!("{}", serde_json::to_string(&payload).unwrap_or_default());
            } else {
                println!(
                    "soldr-daemon servicedef installed at {}",
                    installed.path.display()
                );
            }
            Ok(())
        }
        DaemonSubcommand::Builds { command } => match command {
            DaemonBuildsSubcommand::List {
                limit,
                since_ms,
                json,
            } => crate::daemon_status_render::render_builds(
                client::list_builds(&sock, limit, since_ms),
                json,
            ),
            DaemonBuildsSubcommand::Slow {
                threshold_ms,
                limit,
                json,
            } => crate::daemon_status_render::render_builds(
                client::list_slow_builds(&sock, threshold_ms, limit),
                json,
            ),
        },
    }
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;

/// Register the soldr-daemon image with the broker and export its service
/// name for this process's broker connections.
fn register_daemon_service(
    daemon: &std::path::Path,
) -> Result<crate::daemon::service_definition::InstalledServiceDefinition, SoldrError> {
    let installed = crate::daemon::service_definition::install_service_definition(daemon)
        .map_err(|err| {
            SoldrError::Other(format!(
                "failed to register soldr-daemon image {} with the broker: {err}",
                daemon.display()
            ))
        })?;
    std::env::set_var(
        crate::daemon::backend_handle_adoption::SOLDR_BROKER_SERVICE_ENV_VAR,
        &installed.definition.service_name,
    );
    Ok(installed)
}

/// Ask the broker to launch (or adopt) the daemon route for `service_name`
/// and wait until that daemon answers status.
fn await_daemon_route(
    paths: &crate::core::SoldrPaths,
    sock: &std::path::Path,
    service_name: &str,
    budget: std::time::Duration,
) -> Result<(), SoldrError> {
    let ready_route =
        crate::session_transport::ensure_broker_route(service_name, budget).map_err(|err| {
            SoldrError::Other(format!(
                "broker could not start soldr-daemon route {service_name}: {err}"
            ))
        })?;
    crate::daemon::lifecycle::status_after_negotiated_route(
        paths,
        sock,
        &ready_route.backend_pipe,
        &ready_route.daemon_version,
        crate::daemon::lifecycle::START_STATUS_READY_TIMEOUT,
    )
    .map_err(|err| {
        SoldrError::Other(format!(
            "broker route was published but soldr-daemon did not become status-ready: {err}"
        ))
    })?;
    Ok(())
}

/// Bring up the broker-owned daemon route without displacing any running
/// daemon (soldr#3855). Used by callers that found no daemon and need one
/// before asking it for admission, such as `soldr rustfmt`.
pub(crate) fn start_daemon_route(budget: std::time::Duration) -> Result<(), SoldrError> {
    let paths = crate::core::SoldrPaths::new()?;
    let sock = crate::daemon::client::default_sock_path(&paths);
    let daemon = crate::binaries::soldr_daemon_binary()?;
    let installed = register_daemon_service(&daemon)?;
    await_daemon_route(&paths, &sock, &installed.definition.service_name, budget)
}
