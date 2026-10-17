//! The Nextest run-wrapper (soldr#3453, soldr#3454).
//!
//! `.config/nextest.toml` runs every Unix test through
//! `.github/scripts/nextest_wrapper.sh`, which execs this binary -- the only
//! implementation of the wrapper contract since soldr#3454 retired the
//! Python one (whose interpreter start cost ~48 ms before each of ~3,500
//! tests). For each test:
//!
//! * the test gets a private `TMPDIR`, removed afterwards (soldr#3079;
//!   Linux only), and any process still carrying it -- a daemon the test
//!   auto-started -- is terminated first (soldr#3739);
//! * it refuses toolchain downloads and names its own binary for the
//!   target-dir tripwire (soldr#3195, soldr#3203);
//! * it runs in its own session, dies with the wrapper, and may be traced;
//! * Nextest's SIGTERM dumps its threads, terminates its process group, and
//!   drains both pipes before the grace period forces an exit;
//! * soldr#2885's memory admission and per-test ceiling apply (`guard`).

mod dump;
mod guard;
mod memory;
mod reap;
mod tail;

use guard::{ProcessGuard, CONTROL_ENVS};
use soldr_platform::process::test_child::{self, TestSignal};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};
use tail::OutputTail;

const CHILD_EXIT_GRACE_SECS: f64 = 8.0;
const CHILD_EXIT_GRACE_ENV: &str = "SOLDR_NEXTEST_CHILD_EXIT_GRACE_SECS";
/// Truthy keeps a test's private TMPDIR for inspection.
const KEEP_TMPDIR_ENV: &str = "SOLDR_NEXTEST_KEEP_TMPDIR";
const FORBID_TARGET_CONTAINING_ENV: &str = "SOLDR_TEST_FORBID_TARGET_CONTAINING";
/// An explicit path to this binary for `nextest_wrapper.sh`; never inherited
/// by the test, so a nested Nextest inside a test resolves its own wrapper.
const NATIVE_WRAPPER_ENV: &str = "SOLDR_NEXTEST_NATIVE_WRAPPER";
const POLL: Duration = Duration::from_millis(20);
const DRAIN_SLICE: Duration = Duration::from_millis(100);

pub(crate) fn write_stderr(message: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(message.as_bytes());
    let _ = stderr.flush();
}

fn child_exit_grace() -> Duration {
    std::env::var(CHILD_EXIT_GRACE_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|value| *value > 0.0 && value.is_finite())
        .map_or(
            Duration::from_secs_f64(CHILD_EXIT_GRACE_SECS),
            Duration::from_secs_f64,
        )
}

/// This test's private TMPDIR, or `None` to leave TMPDIR alone.
///
/// soldr#3079: about 420 integration-test call sites create a uniquely named
/// directory under TMPDIR and never remove it; a private TMPDIR removed after
/// the test reclaims all of them at the source. Linux only: macOS's TMPDIR is
/// already long and its `sun_path` limit is 104 bytes, so extra depth risks
/// the Unix-socket endpoints tests bind under TMPDIR. The name is kept short
/// (`snt<pid hex>`) for the same reason.
fn private_tmpdir() -> Option<PathBuf> {
    if !memory::is_linux() {
        return None;
    }
    let base = std::env::var_os("TMPDIR")
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    let stem = format!("snt{:x}", std::process::id());
    for attempt in 0..16 {
        let path = base.join(if attempt == 0 {
            stem.clone()
        } else {
            format!("{stem}-{attempt}")
        });
        match test_child::create_private_dir(&path) {
            Ok(()) => return Some(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

fn remove_private_tmpdir(path: Option<&PathBuf>) {
    if let Some(path) = path {
        // soldr#3739: stop daemons the test auto-started before deleting the
        // directory their state lives in.
        reap::reap_tmpdir_holders(path.as_os_str());
        if !soldr_core::core::flag(KEEP_TMPDIR_ENV) {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn set_default(command: &mut Command, name: &str, value: impl AsRef<std::ffi::OsStr>) {
    if std::env::var_os(name).is_none() {
        command.env(name, value);
    }
}

fn pump(mut source: impl Read, mut sink: impl Write, tail: OutputTail, done: mpsc::Sender<()>) {
    let mut buffer = vec![0; 64 * 1024];
    while let Ok(read) = source.read(&mut buffer) {
        if read == 0 {
            break;
        }
        let _ = sink.write_all(&buffer[..read]);
        let _ = sink.flush();
        tail.feed(&buffer[..read]);
    }
    let _ = done.send(());
}

/// The first SIGTERM/SIGINT: dump threads (SIGTERM only), then forward the
/// signal to the test's process group.
struct Termination {
    started: Option<Instant>,
}

impl Termination {
    fn poll(&mut self, pid: u32) {
        if self.started.is_some() {
            return;
        }
        let Some(signal) = test_child::received_termination() else {
            return;
        };
        if signal == TestSignal::Terminate {
            dump::dump_threads(pid);
        }
        self.started = Some(Instant::now());
        test_child::signal_group(pid, signal);
    }

    fn remaining(&self, grace: Duration) -> Option<Duration> {
        self.started
            .map(|started| grace.saturating_sub(started.elapsed()))
    }
}

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn run(argv: &[std::ffi::OsString]) -> i32 {
    let command: Vec<String> = argv
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let Some((program, args)) = argv.split_first() else {
        write_stderr("nextest timeout wrapper: missing test command\n");
        return 2;
    };
    let parent_pid = std::process::id();
    let grace = child_exit_grace();
    test_child::install_termination_flag();
    let mut guard = ProcessGuard::new(&command);
    let private = private_tmpdir();

    let mut child_command = Command::new(program);
    child_command.args(args);
    for name in CONTROL_ENVS.iter().chain([&NATIVE_WRAPPER_ENV]) {
        child_command.env_remove(name);
    }
    if let Some(path) = &private {
        child_command.env("TMPDIR", path);
    }
    set_default(
        &mut child_command,
        soldr_core::core::FORBID_TOOLCHAIN_INSTALL_ENV_VAR,
        "1",
    );
    set_default(&mut child_command, "RUSTUP_AUTO_INSTALL", "0");
    let absolute = std::path::absolute(program).unwrap_or_else(|_| PathBuf::from(program));
    set_default(&mut child_command, FORBID_TARGET_CONTAINING_ENV, absolute);

    // soldr#2885: wait here while ci-test reports memory pressure, and hold
    // an active-test slot from now until the test exits.
    guard.before_spawn();
    test_child::configure_test_child(
        &mut child_command,
        parent_pid,
        guard.cgroup_procs().as_deref(),
    );
    child_command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match child_command.spawn() {
        Ok(child) => child,
        Err(error) => {
            remove_private_tmpdir(private.as_ref());
            return guard.spawn_failed(&error);
        }
    };
    let pid = child.id();
    guard.after_spawn(pid);

    let (done_tx, done_rx) = mpsc::channel();
    let mut pumps = 0;
    if let Some(stdout) = child.stdout.take() {
        let (tail, done) = (guard.stdout_tail.clone(), done_tx.clone());
        std::thread::spawn(move || pump(stdout, std::io::stdout(), tail, done));
        pumps += 1;
    }
    if let Some(stderr) = child.stderr.take() {
        let (tail, done) = (guard.stderr_tail.clone(), done_tx.clone());
        std::thread::spawn(move || pump(stderr, std::io::stderr(), tail, done));
        pumps += 1;
    }
    drop(done_tx);
    let (status_tx, status_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let code = soldr_platform::wait::wait_for(&mut child, soldr_platform::wait::LONG_CHILD)
            .map_or(1, |status| test_child::returncode(&status));
        let _ = status_tx.send(code);
    });

    let mut termination = Termination { started: None };
    let mut forced = false;
    let mut returncode = None;
    loop {
        termination.poll(pid);
        let slice = match termination.remaining(grace) {
            Some(remaining) if remaining.is_zero() => {
                write_stderr("nextest timeout: child ignored termination; forcing exit\n");
                test_child::signal_group(pid, TestSignal::Kill);
                forced = true;
                break;
            }
            Some(remaining) => remaining.min(POLL),
            None => POLL,
        };
        match status_rx.recv_timeout(slice) {
            Ok(code) => {
                returncode = Some(code);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let returncode = returncode.unwrap_or_else(|| {
        soldr_platform::wait::recv_for(&status_rx, soldr_platform::wait::HELPER_JOIN).unwrap_or(1)
    });

    while pumps > 0 {
        termination.poll(pid);
        let slice = match termination.remaining(grace) {
            Some(remaining) if remaining.is_zero() => {
                if !forced {
                    write_stderr(
                        "nextest timeout: descendants retained output pipes; forcing exit\n",
                    );
                    test_child::signal_group(pid, TestSignal::Kill);
                }
                break;
            }
            Some(remaining) => remaining.min(DRAIN_SLICE),
            None => DRAIN_SLICE,
        };
        match done_rx.recv_timeout(slice) {
            Ok(()) => pumps -= 1,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if termination.started.is_some() {
        let deadline = Instant::now() + Duration::from_secs(2);
        while pumps > 0 {
            match done_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(()) => pumps -= 1,
                Err(_) => break,
            }
        }
        write_stderr(if pumps > 0 {
            "=== nextest timeout: output drain incomplete after SIGKILL ===\n"
        } else {
            "=== nextest timeout: stdout/stderr drained ===\n"
        });
    }
    remove_private_tmpdir(private.as_ref());
    guard.finish(returncode, termination.started.is_some())
}

fn main() {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    std::process::exit(run(&argv));
}
