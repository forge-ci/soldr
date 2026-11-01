//! Cargo-doc front-door route deadline and direct-rustdoc integration tests.
//!
//! This module owns the doc-specific fake toolchain and route harness so the
//! general wrapper contract module can remain within the repository LOC ratchet.

#![allow(unused_imports)]

use crate::common;
use crate::common::*;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn fake_cargo_doc_hold_script(log_path: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             echo cargo-doc phase=%~1 hold-descendant=ready>>\"{}\"\n\
             ping -n 120 127.0.0.1 > nul\n",
            log_path.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             echo \"cargo-doc phase=$1 hold-descendant=ready\" >> \"{}\"\n\
             exec sleep 120\n",
            log_path.display()
        )
    }
}

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn fake_cargo_doc_script(
    log_path: &Path,
    source_path: &Path,
    rustdoc: &Path,
    hold_script: &Path,
) -> String {
    let output_dir = fake_rustc_output_dir(log_path);
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             set \"rustdoc=%RUSTDOC%\"\n\
             if not defined RUSTDOC set \"rustdoc={2}\"\n\
             if \"%~1\"==\"metadata\" (\n\
               echo {{}}\n\
               exit /b 0\n\
             )\n\
             echo cargo %* wrapper=%RUSTC_WRAPPER% rustc=%RUSTC% rustdoc=%rustdoc% env_rustdoc=%RUSTDOC% cache=%SOLDR_CACHE_ENABLED%>>\"{0}\"\n\
             if \"%~1\"==\"doc\" (\n\
               echo cargo-doc phase=route-ready>>\"{0}\"\n\
               echo cargo-doc phase=before-wrapper>>\"{0}\"\n\
               if /I \"%SOLDR_TEST_CARGO_DOC_HOLD_PHASE%\"==\"before-wrapper\" (\n\
                 start \"\" /b \"%ComSpec%\" /d /c call \"{4}\" before-wrapper\n\
                 ping -n 120 127.0.0.1 > nul\n\
               )\n\
               if defined RUSTC_WRAPPER (\n\
                 call \"%RUSTC_WRAPPER%\" \"%RUSTC%\" --crate-name doc_demo --emit dep-info,link \"{1}\" -o \"{3}\\doc_demo\" --out-dir \"{3}\"\n\
                 if errorlevel 1 exit /b 1\n\
               ) else (\n\
                 call \"%RUSTC%\" --crate-name doc_demo --emit dep-info,link \"{1}\" -o \"{3}\\doc_demo\" --out-dir \"{3}\"\n\
                 if errorlevel 1 exit /b 1\n\
               )\n\
               echo cargo-doc phase=after-wrapper>>\"{0}\"\n\
               echo cargo-doc phase=before-rustdoc>>\"{0}\"\n\
               if /I \"%SOLDR_TEST_CARGO_DOC_HOLD_PHASE%\"==\"before-rustdoc\" (\n\
                 start \"\" /b \"%ComSpec%\" /d /c call \"{4}\" before-rustdoc\n\
                 ping -n 120 127.0.0.1 > nul\n\
               )\n\
               call \"%rustdoc%\" \"{1}\"\n\
               if errorlevel 1 exit /b 1\n\
               echo cargo-doc phase=after-rustdoc>>\"{0}\"\n\
               exit /b\n\
             )\n\
             if \"%~1\"==\"test\" if \"%~2\"==\"--doc\" (\n\
               if defined RUSTC_WRAPPER (\n\
                 call \"%RUSTC_WRAPPER%\" \"%RUSTC%\" --crate-name doctest_demo --emit dep-info,link \"{1}\" -o \"{3}\\doctest_demo\" --out-dir \"{3}\"\n\
                 if errorlevel 1 exit /b 1\n\
               ) else (\n\
                 call \"%RUSTC%\" --crate-name doctest_demo --emit dep-info,link \"{1}\" -o \"{3}\\doctest_demo\" --out-dir \"{3}\"\n\
                 if errorlevel 1 exit /b 1\n\
               )\n\
               call \"%rustdoc%\" \"{1}\"\n\
               exit /b\n\
             )\n\
             echo unsupported fake cargo doc invocation %* 1>&2\n\
             exit /b 1\n",
            log_path.display(),
            source_path.display(),
            rustdoc.display(),
            output_dir.display(),
            hold_script.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             rustdoc=\"${{RUSTDOC:-{2}}}\"\n\
             if [ \"$1\" = \"metadata\" ]; then\n\
               echo '{{}}'\n\
               exit 0\n\
             fi\n\
             echo \"cargo $* wrapper=${{RUSTC_WRAPPER:-}} rustc=${{RUSTC:-}} rustdoc=$rustdoc env_rustdoc=${{RUSTDOC:-}} cache=${{SOLDR_CACHE_ENABLED:-}}\" >> \"{0}\"\n\
             doc_phase() {{\n\
               echo \"cargo-doc phase=$1\" >> \"{0}\"\n\
               if [ \"${{SOLDR_TEST_CARGO_DOC_HOLD_PHASE:-}}\" = \"$1\" ]; then\n\
                 \"{4}\" \"$1\" &\n\
                 wait \"$!\"\n\
               fi\n\
             }}\n\
             run_doc_compile() {{\n\
               crate_name=\"$1\"\n\
               doc_phase before-wrapper\n\
               if [ -n \"${{RUSTC_WRAPPER:-}}\" ]; then\n\
                 \"$RUSTC_WRAPPER\" \"$RUSTC\" --crate-name \"$crate_name\" --emit dep-info,link \"{1}\" -o \"{3}/$crate_name\" --out-dir \"{3}\" || exit $?\n\
               else\n\
                 \"$RUSTC\" --crate-name \"$crate_name\" --emit dep-info,link \"{1}\" -o \"{3}/$crate_name\" --out-dir \"{3}\" || exit $?\n\
               fi\n\
               doc_phase after-wrapper\n\
               doc_phase before-rustdoc\n\
               \"$rustdoc\" \"{1}\" || return $?\n\
               doc_phase after-rustdoc\n\
             }}\n\
             if [ \"$1\" = \"doc\" ]; then\n\
               doc_phase route-ready\n\
               run_doc_compile doc_demo\n\
               exit $?\n\
             fi\n\
             if [ \"$1\" = \"test\" ] && [ \"${{2:-}}\" = \"--doc\" ]; then\n\
               run_doc_compile doctest_demo\n\
               exit $?\n\
             fi\n\
             echo \"unsupported fake cargo doc invocation: $*\" >&2\n\
             exit 1\n",
            log_path.display(),
            source_path.display(),
            rustdoc.display(),
            output_dir.display(),
            hold_script.display()
        )
    }
}
fn install_fake_cargo_doc_toolchain(
    log_path: &Path,
    source_path: &Path,
) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
    let (rustup, cargo, rustc, _rustfmt) = install_fake_rustup_toolchain(log_path);
    let tool_dir = cargo
        .parent()
        .expect("fake cargo should live in a tool dir")
        .to_path_buf();
    let rustdoc = fake_script_path(&tool_dir, "rustdoc");
    let zccache = fake_script_path(&tool_dir, "zccache");
    let hold_script = fake_script_path(&tool_dir, "cargo-doc-hold");
    write_fake_script(&rustc, &fake_rustc_script(log_path));
    write_fake_script(&hold_script, &fake_cargo_doc_hold_script(log_path));
    write_fake_script(
        &cargo,
        &fake_cargo_doc_script(log_path, source_path, &rustdoc, &hold_script),
    );
    write_fake_script(&zccache, &fake_zccache_script(log_path));
    (rustup, cargo, rustc, rustdoc, zccache)
}

fn write_rustdoc_source(cache_root: &Path) -> PathBuf {
    let src_dir = cache_root.join("src");
    fs::create_dir_all(&src_dir).expect("failed to create rustdoc source dir");
    let source_path = src_dir.join("lib.rs");
    fs::write(
        &source_path,
        "/// Adds two numbers.\npub fn add(left: usize, right: usize) -> usize { left + right }\n",
    )
    .expect("failed to write rustdoc source");
    source_path
}
// The old `.output()` call delegated every route's liveness to Nextest's
// generic 120-second deadline. The fake Cargo route-ready line is the stable
// hand-off from front-door startup to fixture work; each later fixture state
// has its own budget, while the injected-hold budget begins only after its
// child marker proves the descendant exists.
// Before fake Cargo can accept `doc`, the real front door may contend with
// parallel test routes while it resolves toolchain/bootstrap state. This is
// deliberately the only cold-start allowance; every post-ready route stage
// below remains independently bounded. The route as a whole also has an
// absolute monotonic execution deadline. It leaves one reap budget and one
// shared pipe-drain budget before Nextest's 120-second ceiling.
const CARGO_DOC_ROUTE_STARTUP_BUDGET: Duration = Duration::from_secs(90);
const CARGO_DOC_ROUTE_READY_BUDGET: Duration = Duration::from_secs(15);
// The wrapper phase can cold-start a nested Soldr/daemon generation before
// fake Cargo can observe `after-wrapper`; it is intentionally distinct from
// both the front-door startup and the injected descendant's 2-second hold.
const CARGO_DOC_ROUTE_WRAPPER_BUDGET: Duration = Duration::from_secs(60);
const CARGO_DOC_ROUTE_RUSTDOC_BUDGET: Duration = Duration::from_secs(15);
const CARGO_DOC_ROUTE_HOLD_PHASE_BUDGET: Duration = Duration::from_secs(2);
const CARGO_DOC_ROUTE_REAP_BUDGET: Duration = Duration::from_secs(5);
const CARGO_DOC_ROUTE_EXECUTION_BUDGET: Duration = Duration::from_secs(105);
const CARGO_DOC_ROUTE_POLL: Duration = Duration::from_millis(25);

struct CargoDocRouteOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    phase_markers: Vec<String>,
}

struct CargoDocRouteTimeout {
    phase: CargoDocRoutePhase,
    phase_markers: Vec<String>,
    phase_elapsed: Duration,
    total_elapsed: Duration,
    diagnostics: String,
}

enum CargoDocRouteResult {
    Completed(CargoDocRouteOutput),
    TimedOut(CargoDocRouteTimeout),
}

struct CargoDocRoute<'a> {
    label: &'a str,
    args: &'a [&'a str],
    cache_root: &'a Path,
    log_path: &'a Path,
    cargo: &'a Path,
    rustc: &'a Path,
    rustup: &'a Path,
    hold_phase: Option<&'a str>,
}

struct CargoDocRouteProcess<'a> {
    label: &'a str,
    pid: u32,
    log_path: &'a Path,
}

struct CargoDocRouteTimeoutContext<'a> {
    process: CargoDocRouteProcess<'a>,
    started: Instant,
    phase_started: Instant,
    phase: CargoDocRoutePhase,
    stdout: &'a Receiver<std::io::Result<Vec<u8>>>,
    stderr: &'a Receiver<std::io::Result<Vec<u8>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CargoDocRoutePhase {
    Startup,
    RouteReady,
    Wrapper,
    Rustdoc,
    InjectedHoldReady,
}

impl CargoDocRoutePhase {
    fn budget(self) -> Duration {
        match self {
            Self::Startup => CARGO_DOC_ROUTE_STARTUP_BUDGET,
            Self::RouteReady => CARGO_DOC_ROUTE_READY_BUDGET,
            Self::Wrapper => CARGO_DOC_ROUTE_WRAPPER_BUDGET,
            Self::Rustdoc => CARGO_DOC_ROUTE_RUSTDOC_BUDGET,
            Self::InjectedHoldReady => CARGO_DOC_ROUTE_HOLD_PHASE_BUDGET,
        }
    }
}

fn spawn_route_pipe_drain<R>(mut pipe: R) -> Receiver<std::io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = sender.send(pipe.read_to_end(&mut bytes).map(|_| bytes));
    });
    receiver
}

fn receive_route_pipe(
    receiver: &Receiver<std::io::Result<Vec<u8>>>,
    deadline: Instant,
    label: &str,
    pid: u32,
    pipe: &str,
) -> Result<Vec<u8>, String> {
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|error| {
            format!(
                "route {label} child pid {pid} left its {pipe} pipe open after tree cleanup: {error}"
            )
        })?
        .map_err(|error| format!("route {label} child pid {pid} failed to drain {pipe}: {error}"))
}

fn read_route_log(log_path: &Path) -> String {
    fs::read_to_string(log_path).unwrap_or_default()
}

fn cargo_doc_phase_markers(log: &str) -> Vec<String> {
    log.lines()
        .filter_map(|line| line.strip_prefix("cargo-doc phase=").map(str::to_string))
        .collect()
}

fn cargo_doc_route_phase(
    markers: &[String],
    hold_phase: Option<&str>,
) -> Result<CargoDocRoutePhase, String> {
    const SEQUENCE: [&str; 5] = [
        "route-ready",
        "before-wrapper",
        "after-wrapper",
        "before-rustdoc",
        "after-rustdoc",
    ];

    let mut next = 0;
    let mut held = false;
    for marker in markers {
        if held {
            return Err(format!(
                "fixture advanced after the hold descendant marker: {markers:?}"
            ));
        }
        if SEQUENCE
            .get(next)
            .is_some_and(|expected| marker == expected)
        {
            next += 1;
            continue;
        }
        if let Some(hold_phase) = hold_phase {
            let hold_prefix = format!("{hold_phase} hold-descendant=");
            let hold_marker = format!("{hold_phase} hold-descendant=ready");
            if marker.starts_with(&hold_prefix) && next > 0 && SEQUENCE[next - 1] == hold_phase {
                if marker == &hold_marker {
                    held = true;
                }
                // Fixture observations such as `spawned` only say the parent
                // intends to launch a child. They cannot start the short hold
                // budget: Windows `start` returns before that child exists.
                continue;
            }
        }
        let expected = SEQUENCE.get(next).copied().unwrap_or("route completion");
        return Err(format!(
            "fixture phase sequence expected {expected:?}, got {marker:?}: {markers:?}"
        ));
    }

    if held {
        Ok(CargoDocRoutePhase::InjectedHoldReady)
    } else {
        Ok(match next {
            0 => CargoDocRoutePhase::Startup,
            1 => CargoDocRoutePhase::RouteReady,
            2 | 3 => CargoDocRoutePhase::Wrapper,
            4 | 5 => CargoDocRoutePhase::Rustdoc,
            _ => unreachable!("fixed phase sequence bounds the cursor"),
        })
    }
}

fn diagnostic_tail(text: &str) -> String {
    let mut lines: Vec<_> = text.lines().rev().take(12).collect();
    lines.reverse();
    lines.join("\n")
}

fn route_diagnostics(
    context: &CargoDocRouteTimeoutContext<'_>,
    phase_markers: &[String],
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    let CargoDocRouteTimeoutContext {
        process:
            CargoDocRouteProcess {
                label,
                pid,
                log_path,
            },
        started,
        phase_started,
        phase,
        ..
    } = context;
    let log = read_route_log(log_path);
    format!(
        "route={label} child_pid={pid} elapsed={:?} phase_elapsed={:?} expired_phase={phase:?} phase_markers={phase_markers:?}\nstdout_tail:\n{}\nstderr_tail:\n{}\nfixture_log_tail:\n{}",
        started.elapsed(),
        phase_started.elapsed(),
        diagnostic_tail(&String::from_utf8_lossy(stdout)),
        diagnostic_tail(&String::from_utf8_lossy(stderr)),
        diagnostic_tail(&log),
    )
}

fn reap_route_child(
    child: &mut std::process::Child,
    label: &str,
    pid: u32,
) -> Result<std::process::ExitStatus, String> {
    let deadline = Instant::now() + CARGO_DOC_ROUTE_REAP_BUDGET;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(CARGO_DOC_ROUTE_POLL),
            Ok(None) => {
                return Err(format!(
                    "route {label} child pid {pid} did not reap within {CARGO_DOC_ROUTE_REAP_BUDGET:?} after tree termination"
                ));
            }
            Err(error) => {
                return Err(format!(
                    "route {label} child pid {pid} could not be reaped: {error}"
                ));
            }
        }
    }
}

fn timeout_cargo_doc_route(
    child: &mut std::process::Child,
    context: CargoDocRouteTimeoutContext<'_>,
) -> Result<CargoDocRouteResult, String> {
    use soldr_platform::process::terminate::{terminate_tree, TreeKill};

    let label = context.process.label;
    let pid = context.process.pid;
    let log_path = context.process.log_path;
    let phase_started = context.phase_started;
    let phase = context.phase;
    let stdout = context.stdout;
    let stderr = context.stderr;

    let tree_kill = terminate_tree(child).map_err(|error| {
        format!("route {label} child pid {pid} tree termination failed: {error}")
    })?;
    let status = reap_route_child(child, label, pid)?;
    let drain_deadline = Instant::now() + CARGO_DOC_ROUTE_REAP_BUDGET;
    let stdout = receive_route_pipe(stdout, drain_deadline, label, pid, "stdout")?;
    let stderr = receive_route_pipe(stderr, drain_deadline, label, pid, "stderr")?;
    let phase_markers = cargo_doc_phase_markers(&read_route_log(log_path));
    let diagnostics = route_diagnostics(&context, &phase_markers, &stdout, &stderr);
    if tree_kill != TreeKill::TreeKilled {
        return Err(format!(
            "route {label} timed out in {phase:?} but tree cleanup was not verified ({tree_kill:?}); child status {status:?}\n{diagnostics}"
        ));
    }
    Ok(CargoDocRouteResult::TimedOut(CargoDocRouteTimeout {
        phase,
        phase_markers,
        phase_elapsed: phase_started.elapsed(),
        total_elapsed: context.started.elapsed(),
        diagnostics,
    }))
}

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn run_cargo_doc_route(route: CargoDocRoute<'_>) -> Result<CargoDocRouteResult, String> {
    let CargoDocRoute {
        label,
        args,
        cache_root,
        log_path,
        cargo,
        rustc,
        rustup,
        hold_phase,
    } = route;
    let mut command = isolated_soldr_command();
    command
        .args(args)
        .current_dir(cache_root)
        .env("SOLDR_CACHE_DIR", cache_root)
        .env("SOLDR_TEST_CARGO_BIN", cargo)
        .env("SOLDR_TEST_RUSTC_BIN", rustc)
        .env("SOLDR_TEST_RUSTUP_BIN", rustup)
        .env("PATH", isolated_test_path())
        .env_remove("RUSTDOC")
        .env_remove("CARGO_HOME")
        .env_remove("RUSTUP_HOME")
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("ZCCACHE_CACHE_DIR")
        .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
        .env_remove("ZCCACHE_DISABLE")
        // The child root owns the process group below. The one-hop marker
        // makes the nested front door keep its Cargo descendant in that
        // group, so the merged tree terminator can prove pipe EOF.
        .env("SOLDR_INTERNAL_INHERIT_PROCESS_GROUP", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(phase) = hold_phase {
        command.env("SOLDR_TEST_CARGO_DOC_HOLD_PHASE", phase);
    } else {
        command.env_remove("SOLDR_TEST_CARGO_DOC_HOLD_PHASE");
    }
    // Match the product timeout boundary: the merged platform helper kills
    // this root's complete process group on Unix and snapshots its tree on
    // Windows, including fake Cargo's intentional hold descendant.
    soldr_platform::process::command::configure_process_group(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn cargo-doc route {label}: {error}"))?;
    let pid = child.id();
    let stdout = spawn_route_pipe_drain(child.stdout.take().expect("piped route stdout"));
    let stderr = spawn_route_pipe_drain(child.stderr.take().expect("piped route stderr"));
    let started = Instant::now();
    let mut phase = CargoDocRoutePhase::Startup;
    let mut phase_started = started;
    let mut phase_deadline = phase_started + phase.budget();
    let route_deadline = started + CARGO_DOC_ROUTE_EXECUTION_BUDGET;
    loop {
        let markers = cargo_doc_phase_markers(&read_route_log(log_path));
        let observed_phase = match cargo_doc_route_phase(&markers, hold_phase) {
            Ok(phase) => phase,
            Err(error) => {
                let timed_out = timeout_cargo_doc_route(
                    &mut child,
                    CargoDocRouteTimeoutContext {
                        process: CargoDocRouteProcess {
                            label,
                            pid,
                            log_path,
                        },
                        started,
                        phase_started,
                        phase,
                        stdout: &stdout,
                        stderr: &stderr,
                    },
                )?;
                let CargoDocRouteResult::TimedOut(timeout) = timed_out else {
                    unreachable!("timeout cleanup always returns TimedOut");
                };
                return Err(format!(
                    "route {label} child pid {pid} had an invalid fixture phase transition: {error}\n{}",
                    timeout.diagnostics
                ));
            }
        };
        if observed_phase != phase {
            phase = observed_phase;
            phase_started = Instant::now();
            phase_deadline = phase_started + phase.budget();
        }
        if Instant::now() >= route_deadline || Instant::now() >= phase_deadline {
            return timeout_cargo_doc_route(
                &mut child,
                CargoDocRouteTimeoutContext {
                    process: CargoDocRouteProcess {
                        label,
                        pid,
                        log_path,
                    },
                    started,
                    phase_started,
                    phase,
                    stdout: &stdout,
                    stderr: &stderr,
                },
            );
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                let drain_deadline = Instant::now() + CARGO_DOC_ROUTE_REAP_BUDGET;
                let stdout = receive_route_pipe(&stdout, drain_deadline, label, pid, "stdout")?;
                let stderr = receive_route_pipe(&stderr, drain_deadline, label, pid, "stderr")?;
                let phase_markers = cargo_doc_phase_markers(&read_route_log(log_path));
                return Ok(CargoDocRouteResult::Completed(CargoDocRouteOutput {
                    status,
                    stdout,
                    stderr,
                    phase_markers,
                }));
            }
            Ok(None) => std::thread::sleep(CARGO_DOC_ROUTE_POLL),
            Err(error) => {
                return Err(format!(
                    "route {label} child pid {pid} poll failed: {error}"
                ))
            }
        }
    }
}

fn assert_cargo_doc_phase_order(label: &str, markers: &[String], diagnostics: &str) {
    let expected = [
        "before-wrapper",
        "after-wrapper",
        "before-rustdoc",
        "after-rustdoc",
    ];
    let mut cursor = 0;
    for phase in expected {
        let Some(position) = markers[cursor..].iter().position(|marker| marker == phase) else {
            panic!("route {label} did not reach cargo-doc phase {phase}; markers={markers:?}\n{diagnostics}");
        };
        cursor += position + 1;
    }
}

#[test]
fn cargo_doc_route_phase_rejects_wrapper_without_route_ready() {
    let markers = vec!["before-wrapper".to_string()];
    let error = cargo_doc_route_phase(&markers, None)
        .expect_err("wrapper work without route-ready must remain a startup failure");
    assert!(
        error.contains("route-ready"),
        "missing ready diagnostic should name the phase boundary: {error}"
    );
}

#[test]
fn cargo_doc_route_phase_tracks_wrapper_then_rustdoc() {
    let ready = vec!["route-ready".to_string()];
    assert_eq!(
        cargo_doc_route_phase(&ready, None).expect("route-ready state"),
        CargoDocRoutePhase::RouteReady
    );
    let wrapper = vec!["route-ready".to_string(), "before-wrapper".to_string()];
    assert_eq!(
        cargo_doc_route_phase(&wrapper, None).expect("wrapper state"),
        CargoDocRoutePhase::Wrapper
    );
    let rustdoc = vec![
        "route-ready".to_string(),
        "before-wrapper".to_string(),
        "after-wrapper".to_string(),
        "before-rustdoc".to_string(),
    ];
    assert_eq!(
        cargo_doc_route_phase(&rustdoc, None).expect("rustdoc state"),
        CargoDocRoutePhase::Rustdoc
    );
}

#[test]
fn cargo_doc_route_phase_rejects_skipped_marker() {
    let markers = vec!["route-ready".to_string(), "after-wrapper".to_string()];
    let error = cargo_doc_route_phase(&markers, None)
        .expect_err("wrapper completion cannot skip its before-wrapper marker");
    assert!(
        error.contains("before-wrapper"),
        "skipped-marker diagnostic should name the missing transition: {error}"
    );
}

#[test]
fn cargo_doc_route_phase_rejects_regression() {
    let markers = vec![
        "route-ready".to_string(),
        "before-wrapper".to_string(),
        "after-wrapper".to_string(),
        "before-wrapper".to_string(),
    ];
    let error = cargo_doc_route_phase(&markers, None)
        .expect_err("a repeated wrapper marker must not move the route backwards");
    assert!(
        error.contains("before-rustdoc"),
        "regression diagnostic should name the next monotonic transition: {error}"
    );
}

#[test]
fn cargo_doc_route_hold_budget_waits_for_matching_descendant_marker() {
    let before_marker = vec!["route-ready".to_string(), "before-wrapper".to_string()];
    assert_eq!(
        cargo_doc_route_phase(&before_marker, Some("before-wrapper"))
            .expect("wrapper state before descendant"),
        CargoDocRoutePhase::Wrapper
    );
    let pre_spawn = vec![
        "route-ready".to_string(),
        "before-wrapper".to_string(),
        "before-wrapper hold-descendant=spawned".to_string(),
    ];
    assert_eq!(
        cargo_doc_route_phase(&pre_spawn, Some("before-wrapper"))
            .expect("pre-spawn marker remains wrapper state"),
        CargoDocRoutePhase::Wrapper,
        "a parent-side marker must not start the two-second descendant budget"
    );
    let ready = vec![
        "route-ready".to_string(),
        "before-wrapper".to_string(),
        "before-wrapper hold-descendant=ready".to_string(),
    ];
    let phase = cargo_doc_route_phase(&ready, Some("before-wrapper"))
        .expect("matching descendant marker starts hold phase");
    assert_eq!(phase, CargoDocRoutePhase::InjectedHoldReady);
    assert_eq!(phase.budget(), CARGO_DOC_ROUTE_HOLD_PHASE_BUDGET);
}

#[test]
fn cargo_doc_route_execution_budget_leaves_cleanup_headroom() {
    assert!(
        CARGO_DOC_ROUTE_EXECUTION_BUDGET
            + CARGO_DOC_ROUTE_REAP_BUDGET
            + CARGO_DOC_ROUTE_REAP_BUDGET
            < Duration::from_secs(120),
        "absolute execution deadline must leave root-reap and shared pipe-drain headroom below Nextest's 120-second ceiling"
    );
}

fn expected_link_shim_path(dir: &Path, tool: &str) -> PathBuf {
    dir.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX))
}
#[test]
fn rustdoc_driver_is_intentionally_direct_without_zccache() {
    let cache_root = unique_temp_dir("rustdoc-direct-no-zccache");
    let log_path = cache_root.join("tool.log");
    let source_path = write_rustdoc_source(&cache_root);
    let (rustup, _, _, _) = install_fake_rustup_toolchain(&log_path);
    let zccache_dir = unique_temp_dir("rustdoc-direct-zccache-bin");
    let zccache = fake_script_path(&zccache_dir, "zccache");
    write_fake_script(&zccache, &fake_zccache_script(&log_path));

    let output = isolated_soldr_command()
        .arg("rustdoc")
        .arg(&source_path)
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("PATH", isolated_test_path())
        .env_remove("CARGO_HOME")
        .env_remove("RUSTUP_HOME")
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("ZCCACHE_CACHE_DIR")
        .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("failed to run soldr rustdoc with fake tools");

    assert!(
        output.status.success(),
        "rustdoc direct passthrough failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.lines().any(|line| line.starts_with("rustdoc ")),
        "rustdoc should run directly: {log}"
    );
    assert!(
        path_display_variants(&source_path)
            .iter()
            .any(|path| log.contains(path)),
        "rustdoc should receive the source file: {log}"
    );
    assert!(
        !log.contains("zccache wrapper"),
        "direct rustdoc should not route through zccache: {log}"
    );
}

fn assert_normal_cargo_doc_route(label: &str, args: &[&str]) {
    let cache_root = unique_temp_dir(&format!("cargo-doc-rustdoc-policy-{label}"));
    let log_path = cache_root.join("tool.log");
    let source_path = write_rustdoc_source(&cache_root);
    let (rustup, cargo, rustc, _rustdoc, _zccache) =
        install_fake_cargo_doc_toolchain(&log_path, &source_path);

    let result = run_cargo_doc_route(CargoDocRoute {
        label,
        args,
        cache_root: &cache_root,
        log_path: &log_path,
        cargo: &cargo,
        rustc: &rustc,
        rustup: &rustup,
        hold_phase: None,
    })
    .unwrap_or_else(|error| panic!("failed to run soldr doc route {label}: {error}"));
    let CargoDocRouteResult::Completed(output) = result else {
        panic!("normal cargo-doc route {label} unexpectedly timed out");
    };

    assert!(
        output.status.success(),
        "cargo doc route {label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    // An absent private marker is enabled by contract; only an explicit `0`
    // disables caching. The wrapped fake-rustc assertion below proves the
    // behavioral path instead of overfitting Windows self-relocation.
    let cargo_doc = log
        .lines()
        .find(|line| line.starts_with("cargo doc wrapper="))
        .unwrap_or_else(|| panic!("cargo doc invocation missing from log: {log}"));
    assert!(
        !cargo_doc.contains("wrapper= ") && !cargo_doc.contains("cache=0"),
        "cargo doc should run with cache enabled and a wrapper: {cargo_doc}"
    );
    assert!(
        log.lines().any(|line| line.starts_with("rustc ")),
        "cargo doc route {label} should reach the wrapped rustc path: {log}"
    );
    assert!(
        log.lines().any(|line| line.starts_with("rustdoc ")),
        "cargo doc route {label} should invoke rustdoc directly: {log}"
    );
    let diagnostics = format!(
        "stdout:\n{}\nstderr:\n{}\nlog:\n{log}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_cargo_doc_phase_order(label, &output.phase_markers, &diagnostics);
}

#[test]
fn cargo_doc_keeps_rustc_wrapped_but_rustdoc_direct() {
    assert_normal_cargo_doc_route("cargo-doc", &["cargo", "doc"]);
}

#[test]
fn bare_doc_keeps_rustc_wrapped_but_rustdoc_direct() {
    assert_normal_cargo_doc_route("bare-doc", &["doc"]);
}

/// soldr#2944: a held fake-Cargo phase must fail under the harness-owned
/// deadline, not Nextest's generic 120-second ceiling. `run_cargo_doc_route`
/// returns this expected timeout only after merged `terminate_tree` verifies
/// the process tree, reaps the root, and both inherited pipes reach EOF; the
/// background sleep/ping is therefore a concrete no-surviving-descendant
/// proof rather than merely a short wall-clock assertion.
fn assert_cargo_doc_held_phase(label: &str, args: &[&str], hold_phase: &str) {
    let cache_root = unique_temp_dir(&format!("cargo-doc-held-phase-{label}"));
    let log_path = cache_root.join("tool.log");
    let source_path = write_rustdoc_source(&cache_root);
    let (rustup, cargo, rustc, _rustdoc, _zccache) =
        install_fake_cargo_doc_toolchain(&log_path, &source_path);

    let result = run_cargo_doc_route(CargoDocRoute {
        label,
        args,
        cache_root: &cache_root,
        log_path: &log_path,
        cargo: &cargo,
        rustc: &rustc,
        rustup: &rustup,
        hold_phase: Some(hold_phase),
    })
    .unwrap_or_else(|error| panic!("held cargo-doc route {label} was not cleaned up: {error}"));
    let CargoDocRouteResult::TimedOut(timeout) = result else {
        panic!("held cargo-doc route {label} unexpectedly completed");
    };

    assert_eq!(
        timeout.phase,
        CargoDocRoutePhase::InjectedHoldReady,
        "route {label} should start its short deadline only after the held child marker\n{}",
        timeout.diagnostics
    );
    assert!(
        timeout
            .phase_markers
            .iter()
            .any(|marker| marker.starts_with(hold_phase)),
        "route {label} timed out before its intentional {hold_phase} hold\n{}",
        timeout.diagnostics
    );
    assert!(
        timeout.diagnostics.contains("hold-descendant="),
        "route {label} did not record the held descendant\n{}",
        timeout.diagnostics
    );
    assert!(
        timeout.phase_elapsed
            < CARGO_DOC_ROUTE_HOLD_PHASE_BUDGET
                + CARGO_DOC_ROUTE_REAP_BUDGET
                + Duration::from_secs(5),
        "route {label} exceeded its hold-marker + cleanup budget ({:?})\n{}",
        timeout.phase_elapsed,
        timeout.diagnostics
    );
    assert!(
        timeout.total_elapsed
            < CARGO_DOC_ROUTE_EXECUTION_BUDGET
                + CARGO_DOC_ROUTE_REAP_BUDGET
                + CARGO_DOC_ROUTE_REAP_BUDGET,
        "route {label} exceeded its absolute execution + cleanup budget ({:?})\n{}",
        timeout.total_elapsed,
        timeout.diagnostics
    );
}

#[test]
fn cargo_doc_route_deadline_kills_wrapper_hold_tree() {
    assert_cargo_doc_held_phase("cargo-doc", &["cargo", "doc"], "before-wrapper");
}

#[test]
fn bare_doc_route_deadline_kills_rustdoc_hold_tree() {
    assert_cargo_doc_held_phase("bare-doc", &["doc"], "before-rustdoc");
}

#[test]
fn cargo_doc_tests_keep_rustc_wrapped_but_rustdoc_direct() {
    let cache_root = unique_temp_dir("cargo-doctest-rustdoc-policy");
    let log_path = cache_root.join("tool.log");
    let source_path = write_rustdoc_source(&cache_root);
    let (rustup, cargo, rustc, _rustdoc, _zccache) =
        install_fake_cargo_doc_toolchain(&log_path, &source_path);

    let output = isolated_soldr_command()
        .args(["cargo", "test", "--doc"])
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("PATH", isolated_test_path())
        .env_remove("RUSTDOC")
        .env_remove("CARGO_HOME")
        .env_remove("RUSTUP_HOME")
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("ZCCACHE_CACHE_DIR")
        .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("failed to run soldr cargo test --doc with fake tools");

    assert!(
        output.status.success(),
        "cargo doctest route failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    // See the cargo-doc case above: absence is enabled, `0` is disabled.
    let cargo_doc_test = log
        .lines()
        .find(|line| line.starts_with("cargo test --doc wrapper="))
        .unwrap_or_else(|| panic!("cargo doctest invocation missing from log: {log}"));
    assert!(
        !cargo_doc_test.contains("wrapper= ") && !cargo_doc_test.contains("cache=0"),
        "cargo doctest should run with cache enabled and a wrapper: {cargo_doc_test}"
    );
}

#[test]
fn rustdoc_path_shim_reenters_direct_passthrough_without_zccache() {
    let cache_root = unique_temp_dir("rustdoc-link-shim-no-zccache");
    let log_path = cache_root.join("tool.log");
    let shim_dir = cache_root.join("shims");
    let source_path = write_rustdoc_source(&cache_root);
    let (rustup, _, _, _) = install_fake_rustup_toolchain(&log_path);
    let zccache_dir = unique_temp_dir("rustdoc-link-shim-zccache-bin");
    let zccache = fake_script_path(&zccache_dir, "zccache");
    write_fake_script(&zccache, &fake_zccache_script(&log_path));

    let link_output = isolated_soldr_command()
        .args([
            "toolchain",
            "link",
            "--shim-dir",
            &shim_dir.display().to_string(),
        ])
        .current_dir(&cache_root)
        .output()
        .expect("failed to run soldr toolchain link");

    assert!(
        link_output.status.success(),
        "toolchain link for rustdoc shim failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&link_output.stdout),
        String::from_utf8_lossy(&link_output.stderr)
    );

    let rustdoc_shim = expected_link_shim_path(&shim_dir, "rustdoc");
    let mut command = Command::new(&rustdoc_shim);
    scrub_outer_soldr_env(&mut command);
    let output = command
        .arg(&source_path)
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("PATH", isolated_test_path())
        .env_remove("CARGO_HOME")
        .env_remove("RUSTUP_HOME")
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("ZCCACHE_CACHE_DIR")
        .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("failed to run rustdoc PATH shim");

    assert!(
        output.status.success(),
        "rustdoc PATH shim failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.lines().any(|line| line.starts_with("rustdoc ")),
        "rustdoc shim should re-enter direct rustdoc passthrough: {log}"
    );
    assert!(
        !log.contains("zccache wrapper"),
        "rustdoc shim should not route through zccache: {log}"
    );
}
