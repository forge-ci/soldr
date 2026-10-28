//! soldr#2361 Phase 2: the front door's "spawn the broker" allowlisted
//! exception. **Unconditional** (soldr#2388): every eligible top-level `soldr`
//! invocation that may need the broker spawns/confirms it, and the compile hot
//! path routes through it. Teardown commands never resurrect an absent broker.
//! There is no env-var opt-out — the broker-fronted daemon is the only
//! supported topology.
//!
//! Per the #2364 design, the front door is the sole broker-spawner, and the
//! broker is the sole daemon-spawner via `serve_launching_backends`. The
//! broker→daemon→SESSION compile path is proven end-to-end on the real-process
//! daemon and wrapper integration suites.
//!
//! The one invariant that must never regress: a `RUSTC_WRAPPER` re-entry
//! (`soldr /path/to/rustc ...`, cargo calling back into soldr once per
//! compile unit) must NEVER reach this code. That path is `run_main`'s
//! `wrapper::is_wrapper_invocation` branch, which returns before this
//! module's entry point is ever called -- see the call site in
//! `soldr_main.rs`. If a broker spawn attempt fired on every compile unit
//! instead of once at the top-level invocation, it would recreate the
//! spawn-storm this whole redesign exists to kill (soldr#2360: "154x root
//! ownership is busy"). [`front_door_broker_spawn_eligible`] is a pure,
//! directly-unit-testable predicate for exactly this reason -- the call site
//! placement is a second, structural line of defense, not the only one.
//!
//! Spawns via `running_process::spawn_daemon_with_stdio_and_env_policy`
//! (the single detach machinery for every Soldr-owned long-lived process)
//! rather than a bare `std::process::Command::spawn()`. A bare spawn on Windows
//! stays attached to the caller's job object / console, so a shell (or a
//! sandboxed tool harness) that waits for the whole descendant tree to exit
//! hangs on the long-lived broker even though the direct child (this `soldr`
//! invocation) has already returned -- caught by hand while smoke-testing
//! this against the real binary, not by any written test.

use running_process::{DaemonStdio, DaemonStdioSource, EnvironmentPolicy};
use std::time::{Duration, Instant};

/// How long the front door actively waits for a freshly-spawned broker's
/// control listener. Bounded so a wedged or slow-starting broker can never
/// turn an ordinary `soldr` invocation into a hang.
const SPAWN_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const RESURRECTION_WAIT_TIMEOUT: Duration = Duration::from_secs(12);
const EXISTING_BROKER_RETRY_TIMEOUT: Duration = Duration::from_secs(1);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Environment switch that stops the front door from auto-spawning a broker
/// (soldr#3193). Set to `0` by callers that only need soldr's read-only
/// surfaces -- a test suite running `soldr shims --json` under a throwaway
/// `HOME` per test does not want one broker per test left on the host.
pub(crate) const BROKER_AUTOSPAWN_ENV: &str = "SOLDR_BROKER_AUTOSPAWN";

/// Whether the front door may spawn a broker. The broker-fronted daemon is
/// the only supported topology (soldr#2388): there is no legacy "direct
/// client → daemon without a broker" mode to select, so this is not a
/// topology switch. It is the one opt-out, [`BROKER_AUTOSPAWN_ENV`]`=0`, for
/// callers that will never need a daemon and would otherwise leak a broker
/// per throwaway `HOME` (soldr#3193). A wrapper compile under such a caller
/// still fails loudly with "soldr broker is unreachable" rather than
/// silently running uncached.
pub(crate) fn broker_enabled() -> bool {
    broker_enabled_for(std::env::var_os(BROKER_AUTOSPAWN_ENV).as_deref())
}

pub(crate) fn broker_enabled_for(autospawn: Option<&std::ffi::OsStr>) -> bool {
    autospawn != Some(std::ffi::OsStr::new("0"))
}

/// Preserve Soldr's complete identity namespace plus the authoritative
/// endpoint resolver inputs across the detached front-door broker spawn.
/// `UserBaseline` intentionally removes process-local variables; without this
/// overlay the child can resolve a different home/runtime fallback than the
/// elected starter.
pub(crate) fn broker_spawn_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    filter_broker_spawn_env(std::env::vars_os())
}

/// The per-user identity variables that select a home, config, runtime, or
/// temp location. `UserBaseline` rebuilds these from the passwd entry (Unix)
/// or the user profile (Windows), i.e. from the *real* account, so a spawn
/// that must stay under the caller's home has to overlay them explicitly.
const HOME_IDENTITY_ENV_VARS: &[&str] = &[
    "HOME",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "XDG_CONFIG_HOME",
    "XDG_RUNTIME_DIR",
    "TMPDIR",
];

fn is_home_identity_env(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_uppercase();
    HOME_IDENTITY_ENV_VARS.contains(&name.as_str())
}

fn filter_broker_spawn_env(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    vars.into_iter()
        .filter(|(name, _)| {
            name.to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("SOLDR_")
                || is_home_identity_env(name)
        })
        .collect()
}

/// soldr#3516: the home-identity overlay the broker applies to every daemon
/// it launches.
///
/// The broker spawns daemons under `UserBaseline`, which rebuilds `HOME` (and
/// the rest of [`HOME_IDENTITY_ENV_VARS`]) from the real account. A broker
/// started under a test fixture's throwaway `HOME` therefore launched daemons
/// whose `HOME` was the developer's real one: they could resolve the real
/// broker endpoint, the real `~/dev` GC roots and the real default Soldr root,
/// and one such daemon outlived its test holding the real `~/.soldr` root
/// lock, wedging every host build. The broker's own identity is exactly the
/// one its starter chose ([`broker_spawn_env`] forwarded it), and it already
/// keys the broker's own paths (service definitions, staged route images), so
/// the daemons it launches inherit that same identity. `SOLDR_*` stays with
/// the route's service-definition labels, which are authoritative per route.
pub(crate) fn daemon_home_identity_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    filter_daemon_home_identity_env(std::env::vars_os())
}

pub(crate) fn filter_daemon_home_identity_env(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    vars.into_iter()
        .filter(|(name, value)| is_home_identity_env(name) && !value.is_empty())
        .collect()
}

/// Pure predicate: should this top-level invocation attempt to spawn the
/// broker as its allowlisted exception? `raw_args` is the full argv
/// (`raw_args[0]` is the program name), matching `run_main`'s shape.
///
/// Kept separate from the actual spawn so the wrapper-exclusion and
/// self-recursion-exclusion rules are unit-testable without spawning a
/// process.
pub(crate) fn front_door_broker_spawn_eligible(raw_args: &[String]) -> bool {
    broker_enabled() && front_door_command_shape(raw_args)
}

/// The argv half of [`front_door_broker_spawn_eligible`]: does this
/// invocation have the shape of a command the front door serves? Shared with
/// the soldr#3193 leak notice, which follows the same shape rules but must
/// still fire when [`BROKER_AUTOSPAWN_ENV`] has switched the spawn off.
pub(crate) fn front_door_command_shape(raw_args: &[String]) -> bool {
    let Some(first_positional) = first_command_positional(raw_args) else {
        return false;
    };
    // A flag-shaped first argument (`--version`, `--help`, `-V`, `--as ...`)
    // is not a command; it either prints-and-exits or gets peeled off before
    // dispatch. Booting broker infrastructure for it is never right, and one
    // shape is actively harmful: `global_upgrade::probe_version` runs
    // `<global soldr> --version` as a child of EVERY invocation in a
    // `prefer_newer_global` checkout, so an eligible `--version` made even
    // read-only probes (`soldr broker status` under an isolated test HOME)
    // stage a broker image into that HOME, spawn `broker serve`, and then
    // find "a broker" running -- the target-run broker-absent test failures
    // on Windows (#2521 D). An `--as`-pinned invocation loses nothing: the
    // pinned soldr the trampoline execs applies this predicate to its own
    // argv, where the real command is the first positional again.
    if first_positional.starts_with('-') {
        return false;
    }
    // A rustc-wrapper re-entry must never reach here -- see module doc.
    // Belt-and-suspenders: `run_main` already returns before calling this
    // module on that path, but the predicate stays correct standalone.
    if crate::wrapper::is_wrapper_invocation(first_positional) {
        return false;
    }
    // Don't spawn a broker to service a direct `soldr broker ...`
    // invocation -- that command IS the broker; spawning another one to
    // watch it start would just race its own singleton bind pointlessly.
    if matches!(first_positional.as_str(), "broker" | "replay-nextest") {
        return false;
    }
    true
}

/// The first argument that names a command, skipping the global pre-verb
/// flags clap accepts before it.
///
/// `soldr --debug cargo build` (cold cache root) used to be the failure:
/// `raw_args[1]` was `--debug`, the flag-shaped check below declared the
/// invocation broker-free, no broker ever started, and every cacheable
/// wrapper compile hard-failed with "soldr broker is unreachable". Any
/// leading global flag (`--trust-inherited-soldr-env`, `--allow-unpinned`,
/// …) had the same effect. Only KNOWN global flags are skipped: an
/// unrecognized flag (`--version`, `-V`, `--help`) keeps the deliberate
/// no-spawn behavior the flag-shaped rule exists for.
fn first_command_positional(raw_args: &[String]) -> Option<&String> {
    let mut iter = raw_args.iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--no-cache"
            | "--trust-inherited-soldr-env"
            | "--allow-unpinned"
            | "--timestamp-lines"
            | "--no-timestamp-lines"
            | "--no-cache-states"
            | "--debug" => continue,
            "--zccache" | "--jobs" => {
                let _ = iter.next();
                continue;
            }
            value if value.starts_with("--zccache=") || value.starts_with("--jobs=") => continue,
            _ => return Some(arg),
        }
    }
    None
}

fn is_teardown_command(raw_args: &[String]) -> bool {
    let Some(command) = raw_args.get(1).and_then(|first| match first.as_str() {
        "cache" => Some("shutdown"),
        "daemon" => Some("stop"),
        _ => None,
    }) else {
        return false;
    };
    raw_args
        .iter()
        .skip(2)
        .filter(|arg| !arg.starts_with('-'))
        .any(|arg| arg == command)
}

/// Governs every stdout/stderr diagnostic `maybe_spawn_broker_front_door`
/// might emit -- not just the CI endpoint banner its name refers to. A caller
/// passing `--json` / `--shell-export` / `--github-env` intends to parse or
/// `eval` this process's output, so nothing unsolicited may reach either
/// stream: soldr#2554 found a caller that merges stdout+stderr to parse
/// `soldr env --json`, and an unrelated eprintln! (the soldr#2549 broker
/// image-mismatch warning) broke that parse.
pub(crate) fn ci_endpoint_diagnostics_eligible(raw_args: &[String]) -> bool {
    !raw_args.iter().any(|arg| {
        matches!(arg.as_str(), "--json" | "--github-env" | "--shell-export")
            || arg.starts_with("--github-env=")
    })
}

/// Best-effort: confirm an existing broker or spawn one detached, then wait
/// for an active connection to its control socket. Log text is deliberately
/// not synchronization: it can be stale in the append-only spawn log and is
/// printed before the control socket is usable. A broker Hello is deliberately
/// not used either: on a registry miss, Hello is launch-capable and a mere
/// readiness check must never resurrect the daemon.
///
/// Never fails a non-compiling caller merely because eager broker startup did
/// not finish. A later cacheable compile uses the mandatory broker route and
/// reports an attributed hard failure if that route is still unavailable.
pub(crate) fn maybe_spawn_broker_front_door(raw_args: &[String]) {
    if !front_door_broker_spawn_eligible(raw_args) {
        return;
    }
    // An idempotent teardown must observe an absent daemon as already stopped,
    // not spend the cold-start budget staging a broker solely to ask it the
    // same question. A live daemon is different: resurrect the broker so it
    // can re-adopt the backend and flush it gracefully before shutdown.
    if is_teardown_command(raw_args)
        && crate::core::SoldrPaths::new()
            .ok()
            .and_then(|paths| {
                soldr_daemon::daemon::lifecycle::claimed_daemon_occupies_route(&paths)
            })
            .is_none()
    {
        return;
    }
    let diagnostics_eligible = ci_endpoint_diagnostics_eligible(raw_args);
    if diagnostics_eligible {
        emit_ci_endpoint_diagnostics();
    }
    if let Err(error) = ensure_stable_broker_ready(diagnostics_eligible) {
        if diagnostics_eligible {
            eprintln!("soldr: broker resurrection did not complete: {error}");
        }
    }
}

/// `diagnostics_eligible` mirrors [`ci_endpoint_diagnostics_eligible`]: when a
/// caller asked for `--json` / `--shell-export` / `--github-env`, it is going
/// to parse or `eval` this process's output, very possibly with stdout and
/// stderr merged (the soldr#938 doc comment's own `eval "$(soldr env ...)"`
/// example does exactly that). An unconditional warning here would land
/// inside that machine-readable payload and break the parse -- reproduced by
/// soldr#2554: `soldr env --json` against a broker started by a different
/// Soldr image (the common CI shape once setup-soldr's pinned binary and a
/// freshly-built checkout binary share one broker endpoint) corrupted a
/// caller's `json.loads()` with the soldr#2549 mismatch warning.
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn ensure_stable_broker_ready(diagnostics_eligible: bool) -> Result<(), String> {
    // soldr#2571: the `startup_trace` marks below gate on their own env var,
    // NOT on `diagnostics_eligible` — see that module's doc for why folding
    // them into the suppression would silence the exact shape that wedges.
    let endpoint = crate::broker_identity::ResolvedBrokerEndpoint::resolve()
        .map_err(|error| error.to_string())?;
    crate::startup_trace::phase(crate::startup_trace::phase::BROKER_ENDPOINT_RESOLVE);
    endpoint
        .create_owner_only_directories()
        .map_err(|error| error.to_string())?;
    crate::startup_trace::phase(crate::startup_trace::phase::BROKER_OWNER_DIRECTORIES);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not create readiness runtime: {error}"))?;
    crate::startup_trace::phase(crate::startup_trace::phase::BROKER_PROBE_RUNTIME);
    // The hot path for an already-live broker: this dial is the whole of
    // `ensure_stable_broker_ready` when nothing needs resurrecting.
    let observed_instance = broker_instance_at(&runtime, &endpoint.bind_endpoint);
    crate::startup_trace::phase(crate::startup_trace::phase::BROKER_ADMIN_PROBE);
    if let Some(observed) = observed_instance {
        if known_bad_broker_retirement_candidate(&observed).is_none() {
            if diagnostics_eligible {
                warn_on_broker_image_mismatch(&observed);
            }
            return Ok(());
        }
    }

    let resurrection_deadline = Instant::now() + RESURRECTION_WAIT_TIMEOUT;
    let lease = loop {
        match crate::broker_lease::BrokerLease::acquire(&endpoint.lease_database_path) {
            Ok(lease) => break lease,
            Err(crate::broker_lease::BrokerLeaseError::Fenced) => {
                if stable_broker_is_ready(&runtime, &endpoint.bind_endpoint) {
                    return Ok(());
                }
                if Instant::now() >= resurrection_deadline {
                    return Err(format!(
                        "broker resurrection lease expired without readiness at {}",
                        endpoint.bind_endpoint
                    ));
                }
                // A dead or PID-reused owner is reclaimable immediately. A
                // live/SIGSTOP'd owner remains fenced until expiry.
                std::thread::sleep(crate::broker_lease::BrokerLease::contention_delay());
            }
            Err(error) => return Err(error.to_string()),
        }
    };
    crate::startup_trace::phase(crate::startup_trace::phase::BROKER_LEASE);

    #[cfg(debug_assertions)]
    if !test_pause_after_lease_acquired(&lease)? {
        lease.release();
        return Ok(());
    }

    let result = (|| {
        // A winner may have become ready while this process was entering the
        // immediate transaction. Never stage or spawn after that re-probe.
        let instance = broker_instance_at(&runtime, &endpoint.bind_endpoint).or_else(|| {
            admission_endpoint_exists(&endpoint.bind_endpoint)
                .then(|| wait_for_existing_broker(&runtime, &endpoint.bind_endpoint))
                .flatten()
        });
        if let Some(instance) = instance {
            // soldr#2549: the broker is a stable, long-lived singleton for its
            // user-home endpoint. A package-version or image-digest mismatch is
            // a loud diagnostic condition, never a lifecycle action — this path
            // must not stop, kill, replace, or stage over a live broker. The
            // running Soldr image still gets a closely-aligned daemon: the
            // route's service name is keyed on the daemon image hash, so the
            // stable broker launches (or adopts) a matching daemon generation
            // behind itself and the prior daemon drains under daemon lifecycle
            // policy. Operators recover deliberately with `soldr broker remove`.
            if let Some(candidate) = known_bad_broker_retirement_candidate(&instance) {
                lease.check_fence().map_err(|error| error.to_string())?;
                match crate::broker_cmd::retire_known_bad_broker(
                    &endpoint.bind_endpoint,
                    &instance,
                    &endpoint.executable_path,
                    &lease,
                ) {
                    Ok(
                        outcome @ crate::broker_cmd::KnownBadBrokerRetirementOutcome::Retired {
                            ..
                        },
                    )
                    | Ok(
                        outcome @ crate::broker_cmd::KnownBadBrokerRetirementOutcome::AlreadyExited,
                    ) => {
                        if diagnostics_eligible {
                            emit_known_bad_broker_retirement(&candidate, outcome);
                        }
                    }
                    Ok(crate::broker_cmd::KnownBadBrokerRetirementOutcome::IdentityChanged) => {
                        if diagnostics_eligible {
                            emit_known_bad_broker_retirement(
                                &candidate,
                                crate::broker_cmd::KnownBadBrokerRetirementOutcome::IdentityChanged,
                            );
                            if let Some(current) =
                                broker_instance_at(&runtime, &endpoint.bind_endpoint)
                            {
                                warn_on_broker_image_mismatch(&current);
                            }
                        }
                        return Ok(());
                    }
                    Err(error) => {
                        if diagnostics_eligible {
                            emit_known_bad_broker_retirement_failure(
                                &candidate,
                                &error.to_string(),
                            );
                        }
                        return Err(error.to_string());
                    }
                }
            } else {
                if diagnostics_eligible {
                    warn_on_broker_image_mismatch(&instance);
                }
                return Ok(());
            }
        }
        if admission_endpoint_accepts_connections(&endpoint.bind_endpoint) {
            // A listener that accepts connections but cannot answer admin
            // under a startup stampede is still a live owner. Preserve it;
            // the compile handshake will either succeed or report the
            // incompatible generation on a later bounded attempt. Spawning a
            // second candidate here only creates a guaranteed bind loser.
            return Ok(());
        }
        lease.check_fence().map_err(|error| error.to_string())?;
        // Publish the desired image identity into the shared hash cache while
        // this process is still the sole resurrection winner. Once the
        // listener appears, every contender can compare against this cache
        // hit instead of racing a cold executable scan while the lease ages.
        let broker_instance_id = crate::broker_server::broker_image_instance_id()
            .map_err(|error| format!("could not identify broker image: {error}"))?;
        crate::startup_trace::phase(crate::startup_trace::phase::BROKER_IMAGE_HASH);
        lease.renew().map_err(|error| error.to_string())?;
        stage_broker_image(&endpoint.executable_path, &lease).map_err(|error| {
            format!(
                "could not stage stable broker image {}: {error}",
                endpoint.executable_path.display()
            )
        })?;
        crate::startup_trace::phase(crate::startup_trace::phase::BROKER_STAGE_IMAGE);
        lease.renew().map_err(|error| error.to_string())?;
        lease.check_fence().map_err(|error| error.to_string())?;

        let log_file = open_append(
            &endpoint
                .executable_path
                .parent()
                .expect("broker executable has a parent")
                .join("broker-spawn.log"),
        )
        .ok_or_else(|| "could not open stable broker spawn log".to_string())?;
        let mut command = std::process::Command::new(&endpoint.executable_path);
        command.args(["broker", "serve"]);
        command.envs(broker_spawn_env());
        command.env(
            crate::broker_server::BROKER_INSTANCE_ID_ENV,
            broker_instance_id,
        );
        let stdio = daemon_stdio(&log_file);
        let child = running_process::spawn_daemon_with_stdio_and_env_policy(
            &mut command,
            stdio,
            EnvironmentPolicy::UserBaseline,
        )
        .map_err(|error| format!("could not spawn stable broker: {error}"))?;
        let ready =
            wait_for_stable_broker(&runtime, &endpoint.bind_endpoint, Some((&lease, child)));
        crate::startup_trace::phase(crate::startup_trace::phase::BROKER_SPAWN_WAIT);
        #[cfg(debug_assertions)]
        let ready = test_mark_replacement_ready_after_wait(ready);
        ready
    })();
    lease.release();
    result
}

#[cfg(debug_assertions)]
fn test_pause_after_lease_acquired(
    lease: &crate::broker_lease::BrokerLease,
) -> Result<bool, String> {
    let Some(milliseconds) = std::env::var("SOLDR_TEST_BROKER_LEASE_PAUSE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
    else {
        return Ok(true);
    };
    if let Some(path) = std::env::var_os("SOLDR_TEST_BROKER_LEASE_READY_FILE") {
        let _ = std::fs::write(path, b"acquired\n");
    }
    // Sleep in short slices so a SIGSTOP'd test owner notices wall-clock
    // lease expiry promptly after SIGCONT. One long nanosleep may be restarted
    // with its original remainder by the platform, delaying the fence check
    // for the full injected pause after the process resumes.
    let deadline = Instant::now() + Duration::from_millis(milliseconds);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    match lease.check_fence() {
        Ok(()) => Ok(true),
        Err(crate::broker_lease::BrokerLeaseError::Fenced) => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

fn wait_for_stable_broker(
    runtime: &tokio::runtime::Runtime,
    endpoint: &str,
    mut owned: Option<(
        &crate::broker_lease::BrokerLease,
        running_process::DaemonChild,
    )>,
) -> Result<(), String> {
    let deadline = Instant::now() + SPAWN_WAIT_TIMEOUT;
    let mut next_renew = Instant::now() + Duration::from_secs(1);
    loop {
        if stable_broker_is_ready(runtime, endpoint) {
            return Ok(());
        }
        if let Some((lease, child)) = owned.as_mut() {
            if child
                .try_wait()
                .map_err(|error| format!("could not inspect broker child: {error}"))?
                .is_some()
            {
                return Err("stable broker exited before readiness".into());
            }
            if Instant::now() >= next_renew {
                lease.renew().map_err(|error| error.to_string())?;
                next_renew = Instant::now() + Duration::from_secs(1);
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "stable broker did not answer readiness at {endpoint} within {}ms",
                SPAWN_WAIT_TIMEOUT.as_millis()
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Debug-fixture synchronization for the known-bad retirement race. The marker
/// is deliberately emitted only after the spawned replacement has answered
/// STATUS through [`wait_for_stable_broker`]; lease acquisition, image staging,
/// and a `start`/spawn call are all too early to prove a replacement owns the
/// route. Production builds do not contain this seam.
#[cfg(debug_assertions)]
fn test_mark_replacement_ready_after_wait(ready: Result<(), String>) -> Result<(), String> {
    let path = std::env::var_os("SOLDR_TEST_KNOWN_BAD_REPLACEMENT_READY_FILE")
        .map(std::path::PathBuf::from);
    write_test_replacement_ready_marker(ready, path.as_deref())
}

#[cfg(debug_assertions)]
fn write_test_replacement_ready_marker(
    ready: Result<(), String>,
    path: Option<&std::path::Path>,
) -> Result<(), String> {
    ready?;
    if let Some(path) = path {
        std::fs::write(path, b"ready\n").map_err(|error| {
            format!(
                "could not write known-bad replacement readiness marker {}: {error}",
                path.display()
            )
        })?;
    }
    Ok(())
}

/// soldr#2549: readiness is liveness. Any broker that answers admin STATUS at
/// the stable endpoint owns it, whatever image it was linked from. Folding
/// image compatibility into this predicate is what used to make an ordinary
/// front door retire and re-stage a perfectly healthy broker.
fn stable_broker_is_ready(runtime: &tokio::runtime::Runtime, endpoint: &str) -> bool {
    broker_instance_at(runtime, endpoint).is_some()
}

/// The operator-facing recovery command for a broker image/version mismatch.
/// Named in the front door's warning and asserted by tests so the two cannot
/// drift apart.
pub(crate) const BROKER_REMOVE_COMMAND: &str = "soldr broker remove";

/// Render the default mismatch diagnostic. soldr#2549 keeps every ordinary
/// identity mismatch loud, actionable, and free of lifecycle action; soldr#2920
/// is the separately classified, evidence-bound `=0.9.0` exception.
pub(crate) fn broker_image_mismatch_warning(observed: &str, expected: &str) -> String {
    format!(
        "soldr: warning: the running broker was started from a different Soldr image\n\
         soldr:   running broker: {observed}\n\
         soldr:   this soldr:     {expected}\n\
         soldr: the broker is a stable singleton and is never replaced automatically; \
         work continues through it and a matching daemon generation is launched behind it.\n\
         soldr: to retire it deliberately, run: {BROKER_REMOVE_COMMAND}"
    )
}

fn warn_on_broker_image_mismatch(observed: &str) {
    // A local identity that cannot be computed is not evidence of a mismatch.
    // Stay quiet rather than warning about a comparison that never happened.
    let Ok(expected) = crate::broker_server::broker_image_instance_id() else {
        return;
    };
    if observed == expected {
        return;
    }
    eprintln!("{}", broker_image_mismatch_warning(observed, &expected));
}

fn known_bad_broker_retirement_candidate(
    observed: &str,
) -> Option<crate::broker_policy::KnownBadBrokerRetirement> {
    match crate::broker_policy::classify_broker_instance(observed, env!("CARGO_PKG_VERSION")) {
        crate::broker_policy::BrokerInstanceDecision::RetireKnownBadOlder(candidate) => {
            Some(candidate)
        }
        crate::broker_policy::BrokerInstanceDecision::WarnOnly => None,
    }
}

fn emit_known_bad_broker_retirement(
    candidate: &crate::broker_policy::KnownBadBrokerRetirement,
    outcome: crate::broker_cmd::KnownBadBrokerRetirementOutcome,
) {
    eprintln!(
        "soldr: broker retirement {}",
        known_bad_broker_retirement_record(candidate, outcome)
    );
}

fn known_bad_broker_retirement_record(
    candidate: &crate::broker_policy::KnownBadBrokerRetirement,
    outcome: crate::broker_cmd::KnownBadBrokerRetirementOutcome,
) -> serde_json::Value {
    let staged_image_removed = matches!(
        outcome,
        crate::broker_cmd::KnownBadBrokerRetirementOutcome::Retired {
            staged_image_removed: true,
            ..
        }
    );
    serde_json::json!({
        "schema_version": 1,
        "event": "known_bad_broker_retirement",
        "old_version": candidate.observed_version.to_string(),
        "new_version": candidate.client_version.to_string(),
        "matched_range": candidate.matched_range,
        "shutdown_path": outcome.shutdown_path(),
        "outcome": outcome.outcome(),
        "staged_image_removed": staged_image_removed,
    })
}

fn emit_known_bad_broker_retirement_failure(
    candidate: &crate::broker_policy::KnownBadBrokerRetirement,
    error: &str,
) {
    eprintln!(
        "soldr: broker retirement {}",
        serde_json::json!({
            "schema_version": 1,
            "event": "known_bad_broker_retirement",
            "old_version": candidate.observed_version.to_string(),
            "new_version": candidate.client_version.to_string(),
            "matched_range": candidate.matched_range,
            "shutdown_path": "failed",
            "outcome": "failed",
            "error": error,
        })
    );
}

fn wait_for_existing_broker(runtime: &tokio::runtime::Runtime, endpoint: &str) -> Option<String> {
    let deadline = Instant::now() + EXISTING_BROKER_RETRY_TIMEOUT;
    loop {
        if let Some(instance) = broker_instance_at(runtime, endpoint) {
            return Some(instance);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn admission_endpoint_exists(endpoint: &str) -> bool {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        // Named-pipe endpoints are names, not paths; nothing to stat.
        return false;
    }
    std::path::Path::new(endpoint).exists()
}

fn admission_endpoint_accepts_connections(endpoint: &str) -> bool {
    crate::platform::ipc::connect::probe_accepts_connections(endpoint)
}

fn broker_instance_at(runtime: &tokio::runtime::Runtime, endpoint: &str) -> Option<String> {
    broker_snapshot_at(runtime, endpoint).map(|(instance, _pid)| instance)
}

fn broker_snapshot_at(runtime: &tokio::runtime::Runtime, endpoint: &str) -> Option<(String, u32)> {
    crate::platform::wait::block_on_for(
        runtime,
        async {
            use prost::Message as _;
            use running_process::broker::protocol::{
                validate_frame_envelope, AdminReply, AdminRequest, AdminVerb, Frame, FrameKind,
                ADMIN_PAYLOAD_PROTOCOL,
            };
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

            let probe = async {
                let name = crate::session_transport::local_session_name(endpoint)?;
                let mut stream = crate::platform::ipc::connect::connect_local_socket(name).await?;
                let request = AdminRequest {
                    verb: AdminVerb::Status as i32,
                    json: true,
                    ..Default::default()
                };
                let request = Frame::request(ADMIN_PAYLOAD_PROTOCOL, request.encode_to_vec())
                    .with_request_id(1);
                let bytes = running_process::broker::protocol::encode_framed(&request)
                    .map_err(std::io::Error::other)?;
                stream.write_all(&bytes).await?;
                stream.flush().await?;
                let mut header = [0_u8; 5];
                stream.read_exact(&mut header).await?;
                if header[0] != running_process::broker::protocol::ENVELOPE_VERSION {
                    return Err(std::io::Error::other("wrong broker framing version"));
                }
                let len = u32::from_le_bytes(header[1..].try_into().expect("four bytes")) as usize;
                if len > running_process::broker::protocol::MAX_FRAME_BYTES {
                    return Err(std::io::Error::other("oversized readiness reply"));
                }
                let mut body = vec![0_u8; len];
                stream.read_exact(&mut body).await?;
                let response = Frame::decode(body.as_slice()).map_err(std::io::Error::other)?;
                validate_frame_envelope(&response, FrameKind::Response, ADMIN_PAYLOAD_PROTOCOL)
                    .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
                if response.request_id != request.request_id {
                    return Err(std::io::Error::other("readiness request id mismatch"));
                }
                let reply = AdminReply::decode(response.payload.as_slice())
                    .map_err(std::io::Error::other)?;
                if reply.exit_code != 0 {
                    return Err(std::io::Error::other(format!(
                        "broker readiness returned exit code {}",
                        reply.exit_code
                    )));
                }
                let snapshot: serde_json::Value =
                    serde_json::from_str(&reply.body).map_err(std::io::Error::other)?;
                let instance = snapshot
                    .get("broker_instance")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        std::io::Error::other("readiness reply omitted broker_instance")
                    })?;
                let pid = snapshot
                    .get("broker_pid")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|pid| u32::try_from(pid).ok())
                    .ok_or_else(|| std::io::Error::other("readiness reply omitted broker_pid"))?;
                Ok((instance, pid))
            };
            match tokio::time::timeout(
                crate::broker_deadlines::BrokerDeadlines::from_env().first_response,
                probe,
            )
            .await
            {
                Ok(Ok(instance)) => Some(instance),
                Ok(Err(_)) | Err(_) => None,
            }
        },
        crate::platform::wait::PROBE,
    )
    .ok()
    .flatten()
}

/// Unlink a stopped broker's admission endpoint. soldr#2549 leaves exactly one
/// caller: the deliberate `soldr broker remove` operation. A broker that exits
/// through its own cooperative drain retires the endpoint itself
/// (`broker_server::serve_loop`); a force-killed one cannot, so removal has to.
pub(crate) fn retire_admission_endpoint(endpoint: &str) -> Result<(), String> {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        // Named-pipe listeners have no filesystem entry to unlink. Once the
        // owner exits, the stable pipe name is free for the next broker.
        return Ok(());
    }
    match std::fs::remove_file(endpoint) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not retire the broker admission endpoint at {endpoint}: {error}"
        )),
    }
}

fn stage_broker_image(
    target: &std::path::Path,
    lease: &crate::broker_lease::BrokerLease,
) -> Result<(), String> {
    use std::io::{Read as _, Write as _};

    let source = std::env::current_exe().map_err(|error| error.to_string())?;
    if source == target {
        lease.renew().map_err(|error| error.to_string())?;
        lease.check_fence().map_err(|error| error.to_string())?;
        return Ok(());
    }
    let mut next_renew = Instant::now() + Duration::from_secs(1);
    let mut checkpoint = || -> Result<(), String> {
        if Instant::now() >= next_renew {
            lease.renew().map_err(|error| error.to_string())?;
            next_renew = Instant::now() + Duration::from_secs(1);
        }
        Ok(())
    };
    if target.exists() && same_file_contents(&source, target, &mut checkpoint)? {
        lease.renew().map_err(|error| error.to_string())?;
        lease.check_fence().map_err(|error| error.to_string())?;
        return Ok(());
    }
    crate::shim_materialize::require_relocatable(&source, "the stable broker image")?;
    let parent = target
        .parent()
        .ok_or_else(|| "stable broker path has no parent".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let suffix =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            ".exe"
        } else {
            ""
        };
    let temporary = parent.join(format!(
        ".soldr-broker.stage-{}-{:016x}{suffix}",
        std::process::id(),
        stage_nonce()
    ));
    struct RemoveStagingFile(std::path::PathBuf);
    impl Drop for RemoveStagingFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _remove_staging_file = RemoveStagingFile(temporary.clone());
    let mut input = std::fs::File::open(&source).map_err(|error| error.to_string())?;
    let mut output = std::fs::File::create(&temporary).map_err(|error| error.to_string())?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = input.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|error| error.to_string())?;
        checkpoint()?;
    }
    output.flush().map_err(|error| error.to_string())?;
    crate::platform::fs::permissions::make_private(&temporary)
        .map_err(|error| error.to_string())?;
    output.sync_all().map_err(|error| error.to_string())?;
    drop(output);
    drop(input);
    #[cfg(debug_assertions)]
    staging_test::pause_after_sync();
    // A blocking flush or sync can outlive the lease. Renew atomically before
    // publishing: this succeeds only if our generation still owns the row.
    lease.renew().map_err(|error| error.to_string())?;
    lease.check_fence().map_err(|error| error.to_string())?;
    replace_staged_image(&temporary, target).map_err(|error| error.to_string())?;
    #[cfg(debug_assertions)]
    staging_test::note_publication();
    Ok(())
}

fn same_file_contents(
    left: &std::path::Path,
    right: &std::path::Path,
    checkpoint: &mut impl FnMut() -> Result<(), String>,
) -> Result<bool, String> {
    use std::io::Read as _;

    let left_meta = std::fs::metadata(left).map_err(|error| error.to_string())?;
    let right_meta = std::fs::metadata(right).map_err(|error| error.to_string())?;
    if left_meta.len() != right_meta.len() {
        return Ok(false);
    }
    let hash = |path: &std::path::Path,
                checkpoint: &mut dyn FnMut() -> Result<(), String>|
     -> Result<zccache::hash::ContentHash, String> {
        let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
        let mut hasher = zccache::hash::StreamHasher::new();
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            checkpoint()?;
        }
        Ok(hasher.finalize())
    };
    Ok(hash(left, checkpoint)? == hash(right, checkpoint)?)
}

fn stage_nonce() -> u64 {
    let mut bytes = [0_u8; 8];
    let _ = getrandom::fill(&mut bytes);
    u64::from_le_bytes(bytes)
}

fn replace_staged_image(source: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    crate::platform::fs::replace::atomic_replace(source, target)
}

#[derive(Debug, PartialEq, Eq)]
struct BrokerEndpointDiagnostics {
    executable: std::path::PathBuf,
    logical: String,
    bind: String,
    log: std::path::PathBuf,
}

fn broker_endpoint_diagnostics() -> Result<BrokerEndpointDiagnostics, String> {
    let endpoint = crate::broker_identity::ResolvedBrokerEndpoint::resolve()
        .map_err(|error| error.to_string())?;
    let log = endpoint
        .executable_path
        .parent()
        .ok_or_else(|| "broker executable has no parent".to_string())?
        .join("broker-spawn.log");
    Ok(BrokerEndpointDiagnostics {
        executable: endpoint.executable_path,
        logical: endpoint.logical_socket_path,
        bind: endpoint.bind_endpoint,
        log,
    })
}

fn render_ci_endpoint_diagnostics(
    ci_label: &str,
    diagnostics: &BrokerEndpointDiagnostics,
) -> String {
    format!(
        "soldr broker endpoint: ci={ci_label} executable={} logical={} bind={} log={}",
        diagnostics.executable.display(),
        diagnostics.logical,
        diagnostics.bind,
        diagnostics.log.display()
    )
}

fn emit_ci_endpoint_diagnostics() {
    let Some(ci_label) = crate::optimize_detect::detect_ci() else {
        return;
    };
    match broker_endpoint_diagnostics() {
        Ok(diagnostics) => eprintln!("{}", render_ci_endpoint_diagnostics(ci_label, &diagnostics)),
        Err(error) => eprintln!("soldr broker endpoint: ci={ci_label} unresolved: {error}"),
    }
}

pub(crate) fn open_append(path: &std::path::Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

pub(crate) fn daemon_stdio(log: &std::fs::File) -> DaemonStdio<'_> {
    crate::platform::process::spawn::daemon_stdio(Some(log))
}

#[cfg(test)]
#[path = "broker_spawn_tests.rs"]
mod tests;

#[cfg(debug_assertions)]
#[path = "broker_staging_test.rs"]
mod staging_test;
