//! Session-leader launch, owned process-group termination, generation-safe
//! priority, liveness-bound image paths, and standard-stream redirection.
//!
//! Children are this test binary re-executed with `--exact
//! process_host_control::child_probe`.

use kernal_api::platform::process::{
    capture_identity, configure_session_leader_command, detach_standard_streams, executable_path,
    force_terminate_process_group, redirect_standard_streams_to_log, set_priority,
    ProcessIdentityAction, ProcessIdentityCapture, ProcessLiveness,
};
use kernal_api::ProcessPriority;
use std::io::{Read as _, Write as _};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PROBE: &str = "KERNAL_HOST_CONTROL_PROBE";
const LOG: &str = "KERNAL_HOST_CONTROL_LOG";

#[test]
fn child_probe() {
    let Ok(mode) = std::env::var(PROBE) else {
        return;
    };
    match mode.as_str() {
        // Wait for stdin EOF (or a kill).
        "sleeper" => {
            say("ready\n");
            let _ = std::io::stdin().read_to_end(&mut Vec::new());
        }
        // Start a grandchild in this process's group, report its PID, wait.
        "group-root" => {
            let mut grandchild = probe("sleeper", false);
            wait_ready(&mut grandchild);
            say(&format!("grandchild={}\nready\n", grandchild.id()));
            let _ = std::io::stdin().read_to_end(&mut Vec::new());
        }
        "redirect" => {
            let log = std::env::var_os(LOG).expect("log path");
            assert!(redirect_standard_streams_to_log(std::path::Path::new(&log)));
            std::io::stdout().write_all(b"to-stdout\n").unwrap();
            std::io::stderr().write_all(b"to-stderr\n").unwrap();
        }
        "detach" => {
            detach_standard_streams();
            let _ = std::io::stdout().write_all(b"after-detach\n");
            let _ = std::io::stderr().write_all(b"after-detach\n");
        }
        other => panic!("unknown probe mode {other}"),
    }
    let _ = std::io::stdout().flush();
    std::process::exit(0);
}

fn say(text: &str) {
    let mut stdout = std::io::stdout();
    stdout.write_all(text.as_bytes()).expect("probe stdout");
    stdout.flush().expect("flush probe stdout");
}

fn command(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("test binary"));
    command
        .args([
            "--exact",
            "process_host_control::child_probe",
            "--nocapture",
        ])
        .env(PROBE, mode);
    command
}

fn probe(mode: &str, session_leader: bool) -> Child {
    let mut command = command(mode);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if session_leader {
        configure_session_leader_command(&mut command);
    }
    command.spawn().expect("spawn probe")
}

/// Read until the probe's `ready` line, returning everything before it.
fn wait_ready(child: &mut Child) -> String {
    let mut stdout = child.stdout.take().expect("probe stdout");
    let mut seen = Vec::new();
    let mut byte = [0_u8; 1];
    while !String::from_utf8_lossy(&seen).contains("ready\n") {
        assert_eq!(
            stdout.read(&mut byte).expect("read probe"),
            1,
            "probe ended early"
        );
        seen.push(byte[0]);
    }
    child.stdout = Some(stdout);
    String::from_utf8_lossy(&seen).into_owned()
}

fn gone_within(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match ProcessLiveness::open(pid) {
            Err(_) => return true,
            Ok(handle) if !handle.is_alive() => return true,
            Ok(_) if Instant::now() >= deadline => return false,
            Ok(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// The group led by an owned session-leader child dies as one, grandchild
/// included; Windows reports the capability as unsupported.
#[test]
fn owned_group_termination_reaches_the_grandchild() {
    let mut root = probe("group-root", true);
    let banner = wait_ready(&mut root);
    let grandchild: u32 = banner
        .lines()
        .find_map(|line| line.strip_prefix("grandchild="))
        .expect("grandchild pid")
        .parse()
        .expect("numeric pid");

    let result = force_terminate_process_group(&root);
    if cfg!(windows) {
        assert_eq!(
            result.expect_err("no group kill on Windows").kind(),
            std::io::ErrorKind::Unsupported
        );
        let _ = root.kill();
        let _ = root.wait();
        return;
    }
    result.expect("group kill");
    // Keep the root's stdin open: `Child::wait` closes it first, and a root
    // that read that EOF before the signal landed would exit 0 on its own,
    // passing or failing for the wrong reason (#283).
    let stdin = root.stdin.take();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = root.try_wait().expect("poll root") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the group kill never reached the root"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(stdin);
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "root was killed: {status:?}"
        );
    }
    assert!(!status.success(), "root was killed");
    assert!(
        gone_within(grandchild, Duration::from_secs(5)),
        "grandchild {grandchild} died with its group"
    );
}

/// Once the owner has reaped its child, the group id is no longer provably
/// its own, so nothing is signalled.
#[cfg(unix)]
#[test]
fn a_reaped_child_group_is_refused() {
    let mut child = probe("sleeper", true);
    wait_ready(&mut child);
    drop(child.stdin.take());
    child.wait().expect("reap");
    let error = force_terminate_process_group(&child).expect_err("reaped child");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

/// `setsid` makes the child lead a group whose id is its own PID.
#[cfg(unix)]
#[test]
fn session_leader_leads_its_own_group() {
    let mut child = probe("sleeper", true);
    wait_ready(&mut child);
    // SAFETY: scalar query of an unreaped child.
    let group = unsafe { libc::getpgid(child.id() as libc::pid_t) };
    let session = unsafe { libc::getsid(child.id() as libc::pid_t) };
    drop(child.stdin.take());
    let _ = child.wait();
    assert_eq!(group, child.id() as libc::pid_t);
    assert_eq!(session, child.id() as libc::pid_t);
}

#[test]
fn priority_is_applied_to_the_exact_generation() {
    let mut child = probe("sleeper", false);
    wait_ready(&mut child);
    let ProcessIdentityCapture::Found(identity) = capture_identity(child.id()) else {
        panic!("a live child has an identity");
    };
    assert_eq!(
        set_priority(identity, ProcessPriority::Normal).expect("normal is a no-op"),
        ProcessIdentityAction::Performed
    );
    assert_eq!(
        // Idle, the lowest band, is permitted however niced this runner is.
        set_priority(identity, ProcessPriority::Idle).expect("lower priority"),
        ProcessIdentityAction::Performed
    );
    #[cfg(unix)]
    {
        // SAFETY: scalar query of an unreaped child.
        let nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, child.id() as libc::id_t) };
        assert_eq!(nice, 19, "Idle is nice 19 on Unix");
    }
    drop(child.stdin.take());
    child.wait().expect("reap");
    match set_priority(identity, ProcessPriority::Low) {
        Ok(ProcessIdentityAction::AlreadyExited)
        | Err(kernal_api::platform::process::ProcessIdentityActionError::StaleIdentity) => {}
        other => panic!("an exited generation is never re-prioritised: {other:?}"),
    }
}

#[test]
fn image_path_is_read_through_a_live_reference() {
    let me = ProcessLiveness::open(std::process::id()).expect("self");
    assert!(kernal_api::platform::process::same_executable_path(
        &executable_path(&me).expect("own image"),
        &std::env::current_exe().expect("current_exe"),
    ));

    let mut child = probe("sleeper", false);
    wait_ready(&mut child);
    let handle = ProcessLiveness::open(child.id()).expect("child");
    assert!(executable_path(&handle).is_ok());
    drop(child.stdin.take());
    child.wait().expect("reap");
    assert_eq!(
        executable_path(&handle).expect_err("exited").kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn bare_pid_liveness_probe_tracks_a_child_through_exit() {
    assert!(kernal_api::platform::process::is_alive(std::process::id()));
    assert!(!kernal_api::platform::process::is_alive(u32::MAX));

    let mut child = probe("sleeper", false);
    wait_ready(&mut child);
    let pid = child.id();
    assert!(kernal_api::platform::process::is_alive(pid));
    drop(child.stdin.take());
    child.wait().expect("reap");
    assert!(!kernal_api::platform::process::is_alive(pid));
}

#[test]
fn standard_streams_redirect_to_an_append_log() {
    let directory = tempfile::tempdir().expect("tempdir");
    let log = directory.path().join("daemon.log");
    std::fs::write(&log, b"existing\n").expect("seed log");
    let output = command("redirect")
        .env(LOG, &log)
        .stdin(Stdio::piped())
        .output()
        .expect("run redirect probe");
    assert!(output.status.success(), "probe exit {:?}", output.status);
    let text = std::fs::read_to_string(&log).expect("read log");
    assert!(
        text.starts_with("existing\n"),
        "appended, not truncated: {text:?}"
    );
    assert!(
        text.contains("to-stdout\n"),
        "stdout reached the log: {text:?}"
    );
    assert!(
        text.contains("to-stderr\n"),
        "stderr reached the log: {text:?}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("to-stdout"));
}

/// An unopenable log is reported as `false`. Run in a child because stdin is
/// detached before the log is opened, which is process-global.
#[test]
fn unopenable_log_is_reported_without_redirecting() {
    let directory = tempfile::tempdir().expect("tempdir");
    let missing = directory.path().join("no-such-dir").join("x.log");
    let output = command("redirect")
        .env(LOG, &missing)
        .stdin(Stdio::piped())
        .output()
        .expect("run redirect probe");
    assert!(
        !output.status.success(),
        "the probe's `redirect` assertion fails"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("child_probe"),
        "stdout still reaches the parent after a failed redirect"
    );
}

#[test]
fn detached_streams_write_nowhere() {
    let output = command("detach")
        .stdin(Stdio::piped())
        .output()
        .expect("run detach probe");
    assert!(output.status.success(), "probe exit {:?}", output.status);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("after-detach"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("after-detach"));
}
