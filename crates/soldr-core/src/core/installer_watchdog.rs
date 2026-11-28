//! Progress-based supervision for long-running installer subprocesses.
//!
//! Installer downloads and source builds can legitimately take much longer
//! than a fixed wall-clock deadline.  This module therefore fails only when a
//! child has stopped making observable progress, while retaining a large,
//! configurable safety ceiling for genuinely runaway commands.

use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::SoldrError;

/// Environment variable for the silent-work heartbeat cadence (soldr#2359).
/// `0` disables heartbeat lines entirely.
pub const INSTALLER_HEARTBEAT_ENV_VAR: &str = "SOLDR_INSTALLER_HEARTBEAT_SECS";
/// Default heartbeat cadence while a supervised child produces no output.
pub const DEFAULT_INSTALLER_HEARTBEAT_SECS: u64 = 10;
/// Environment variable for the maximum time an installer may be quiet.
pub const INSTALLER_STALL_TIMEOUT_ENV_VAR: &str = "SOLDR_INSTALLER_STALL_TIMEOUT_SECS";
/// Environment variable for the maximum total installer runtime.
pub const INSTALLER_SAFETY_TIMEOUT_ENV_VAR: &str = "SOLDR_INSTALLER_SAFETY_TIMEOUT_SECS";
/// Default quiet-period watchdog. Output and CPU activity reset this timer.
pub const DEFAULT_INSTALLER_STALL_TIMEOUT_SECS: u64 = 15 * 60;
/// Deliberately high runaway-process backstop. It is not a normal installer
/// deadline; active work may continue up to this ceiling.
pub const DEFAULT_INSTALLER_SAFETY_TIMEOUT_SECS: u64 = 24 * 60 * 60;
const KILLED_INSTALLER_REAP_TIMEOUT_SECS: u64 = 5;
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Effective bounds for one installer subprocess.
#[derive(Debug, Clone)]
pub struct InstallerWatchdogConfig {
    /// Maximum duration with no output or observed CPU activity.
    pub stall_timeout: Duration,
    /// Absolute, opt-in-adjustable upper bound for a runaway process.
    pub safety_timeout: Duration,
    /// Cadence for "still working" lines while the child emits no output
    /// (soldr#2359). `None` disables the heartbeat.
    pub heartbeat_interval: Option<Duration>,
    safety_timeout_env_var: &'static str,
    poll_interval: Duration,
}

impl InstallerWatchdogConfig {
    /// Resolve the common watchdog configuration.
    ///
    /// `explicit_safety_timeout_env_var` is a legacy, operation-specific
    /// setting. When present and valid, it remains an explicit hard ceiling so
    /// existing automation keeps its requested bound. Without it, the shared
    /// 24-hour ceiling is used.
    pub fn from_env(explicit_safety_timeout_env_var: &'static str) -> Self {
        let (safety_timeout, safety_timeout_env_var) =
            match positive_env_duration(explicit_safety_timeout_env_var) {
                Some(timeout) => (timeout, explicit_safety_timeout_env_var),
                None => (installer_safety_timeout(), INSTALLER_SAFETY_TIMEOUT_ENV_VAR),
            };
        Self {
            stall_timeout: installer_stall_timeout(),
            safety_timeout,
            heartbeat_interval: installer_heartbeat_interval(),
            safety_timeout_env_var,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }

    #[cfg(test)]
    fn for_test(stall_timeout: Duration, safety_timeout: Duration) -> Self {
        Self {
            stall_timeout,
            safety_timeout,
            heartbeat_interval: None,
            safety_timeout_env_var: INSTALLER_SAFETY_TIMEOUT_ENV_VAR,
            poll_interval: Duration::from_millis(10),
        }
    }
}

/// Resolve the shared no-progress deadline.
pub fn installer_stall_timeout() -> Duration {
    positive_env_duration(INSTALLER_STALL_TIMEOUT_ENV_VAR)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_INSTALLER_STALL_TIMEOUT_SECS))
}

/// Resolve the shared maximum-runtime safety ceiling.
pub fn installer_safety_timeout() -> Duration {
    positive_env_duration(INSTALLER_SAFETY_TIMEOUT_ENV_VAR)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_INSTALLER_SAFETY_TIMEOUT_SECS))
}

/// Resolve the heartbeat cadence. Unset or invalid values keep the default;
/// an explicit `0` disables the heartbeat (soldr#2359's escape hatch).
fn installer_heartbeat_interval() -> Option<Duration> {
    match std::env::var(INSTALLER_HEARTBEAT_ENV_VAR) {
        Ok(value) => match value.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(seconds) => Some(Duration::from_secs(seconds)),
            Err(_) => Some(Duration::from_secs(DEFAULT_INSTALLER_HEARTBEAT_SECS)),
        },
        Err(_) => Some(Duration::from_secs(DEFAULT_INSTALLER_HEARTBEAT_SECS)),
    }
}

/// A heartbeat is due when the child has been output-silent for at least one
/// interval AND at least one interval has passed since the previous emission.
/// CPU-probe progress deliberately does not suppress it: a busy-but-silent
/// compile is exactly the state a caller mistakes for a hang (soldr#2359).
fn heartbeat_due(since_output: Duration, since_emit: Duration, interval: Duration) -> bool {
    since_output >= interval && since_emit >= interval
}

fn heartbeat_line(context: &str, elapsed: Duration, since_output: Duration) -> String {
    format!(
        concat!(
            "soldr: {} is still working ({}s elapsed, no output for {}s); ",
            "this can take several minutes on first-time setup - ",
            "do not kill this process"
        ),
        context,
        elapsed.as_secs(),
        since_output.as_secs(),
    )
}

fn positive_env_duration(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
}

/// Where an installer child's stdout goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildStdoutRoute {
    /// Fully-quiet machine-readable mode: nothing is written anywhere.
    Discard,
    /// This process's stdout is a payload, but its stderr is a log
    /// (soldr#2892).
    Stderr,
    /// Ordinary interactive/CI run.
    Stdout,
}

/// Decide the route from the two process-scoped markers.
///
/// Suppression wins. A caller that asked for silence must not start receiving
/// relocated child output merely because it also declared its stdout a
/// payload -- `soldr env --json` sets only the first marker today, but the
/// two are independent flags and the precedence has to be stated rather than
/// implied by the order of an if-chain.
pub(crate) fn child_stdout_route(suppress: bool, stdout_is_payload: bool) -> ChildStdoutRoute {
    match (suppress, stdout_is_payload) {
        (true, _) => ChildStdoutRoute::Discard,
        (false, true) => ChildStdoutRoute::Stderr,
        (false, false) => ChildStdoutRoute::Stdout,
    }
}

/// Spawn and supervise a long-running installer while preserving its stdout
/// and stderr exactly as live terminal output.
pub fn run_installer_command(
    command: &mut Command,
    context: &str,
    phase: &str,
    config: InstallerWatchdogConfig,
) -> Result<ExitStatus, SoldrError> {
    match run_installer_command_inner(
        command,
        context,
        phase,
        config,
        InstallerCommandMode::Forward,
    )? {
        InstallerCommandResult::Forward(status) => Ok(status),
        InstallerCommandResult::Captured(_) => unreachable!("forwarding mode must not capture"),
    }
}

/// Run an installer-shaped command under the long-progress watchdog and
/// capture both streams for a caller which needs a machine-readable result.
///
/// Normal installer commands forward progress to the terminal, whereas a
/// manager lookup needs its stdout to be its resolved path. Both modes use the
/// same spawn/watchdog path, so a lookup waiting behind the manager lock gets
/// heartbeats and the installer safety ceiling instead of the generic silence
/// timeout.
pub fn run_installer_command_output(
    command: &mut Command,
    context: &str,
    phase: &str,
    config: InstallerWatchdogConfig,
) -> Result<std::process::Output, SoldrError> {
    match run_installer_command_inner(
        command,
        context,
        phase,
        config,
        InstallerCommandMode::Capture,
    )? {
        InstallerCommandResult::Captured(output) => Ok(output),
        InstallerCommandResult::Forward(_) => unreachable!("capture mode must not forward"),
    }
}

#[derive(Clone, Copy)]
enum InstallerCommandMode {
    Forward,
    Capture,
}

enum InstallerCommandResult {
    Forward(ExitStatus),
    Captured(std::process::Output),
}

enum InstallerPipeReaders {
    Forward {
        stdout: Option<mpsc::Receiver<std::io::Result<()>>>,
        stderr: Option<mpsc::Receiver<std::io::Result<()>>>,
    },
    Capture {
        stdout: Option<mpsc::Receiver<std::io::Result<Vec<u8>>>>,
        stderr: Option<mpsc::Receiver<std::io::Result<Vec<u8>>>>,
    },
}

fn run_installer_command_inner(
    command: &mut Command,
    context: &str,
    phase: &str,
    config: InstallerWatchdogConfig,
    mode: InstallerCommandMode,
) -> Result<InstallerCommandResult, SoldrError> {
    configure_installer_process_tree(command);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = {
        // soldr#3098: spawns share, staged writes exclude.
        let _spawn = super::spawn_exclusion::spawn_shared();
        command
            .spawn()
            .map_err(|err| SoldrError::Other(format!("failed to invoke {context}: {err}")))?
    };
    let (progress_tx, progress_rx) = mpsc::channel();
    let readers = match mode {
        InstallerCommandMode::Forward => installer_forwarding_readers(&mut child, &progress_tx),
        InstallerCommandMode::Capture => InstallerPipeReaders::Capture {
            stdout: child
                .stdout
                .take()
                .map(|pipe| capture_pipe_async(pipe, progress_tx.clone())),
            stderr: child
                .stderr
                .take()
                .map(|pipe| capture_pipe_async(pipe, progress_tx.clone())),
        },
    };
    drop(progress_tx);

    let cpu_probe = ProcessTreeCpuProbe::new(child.id());
    // A killed installer can leave a descendant holding a copied pipe
    // descriptor. Propagating the watchdog error drops `readers`, detaches the
    // forwarding/capture threads, and avoids turning that useful diagnosis
    // into an unbounded join on the descendant.
    let status =
        wait_for_child_with_watchdog(&mut child, context, phase, &config, &progress_rx, cpu_probe)?;

    match readers {
        InstallerPipeReaders::Forward { stdout, stderr } => {
            wait_for_pipe_drain(stdout, context, "stdout")?;
            wait_for_pipe_drain(stderr, context, "stderr")?;
            Ok(InstallerCommandResult::Forward(status))
        }
        InstallerPipeReaders::Capture { stdout, stderr } => {
            let stdout = wait_for_capture(stdout, context, "stdout")?;
            let stderr = wait_for_capture(stderr, context, "stderr")?;
            Ok(InstallerCommandResult::Captured(std::process::Output {
                status,
                stdout,
                stderr,
            }))
        }
    }
}

fn installer_forwarding_readers(
    child: &mut Child,
    progress_tx: &mpsc::Sender<()>,
) -> InstallerPipeReaders {
    // soldr#2304 x soldr#2554: machine-readable verbs (env --json) set the
    // internal quiet marker; child installer output must not corrupt their
    // parseable payload, so both streams tee to a sink instead of the
    // terminal. The progress channel still sees every byte, so the stall
    // watchdog is unaffected.
    let suppress = super::quiet::diagnostics_suppressed();
    // soldr#2892: a verb whose stdout is a payload must not let a child write
    // to it. `soldr toolchain ensure --json` did, and rustup's own stdout
    // landed in front of the JSON:
    //
    //     (blank)
    //       1.95.0-x86_64-apple-darwin unchanged - rustc 1.95.0 (...)
    //     (blank)
    //     { "schema_version": 1, ...
    //
    // which `json.load` rejects with `Extra data: line 2 column 7`.
    //
    // Relocated to stderr rather than discarded: unlike the fully-quiet mode
    // above, this caller reads stderr, and rustup's progress is what makes a
    // multi-minute first-time install legible.
    let route = child_stdout_route(suppress, super::quiet::stdout_carries_payload());
    let stdout_reader = child.stdout.take().map(|pipe| match route {
        ChildStdoutRoute::Discard => tee_pipe_async(pipe, std::io::sink(), progress_tx.clone()),
        ChildStdoutRoute::Stderr => tee_pipe_async(pipe, std::io::stderr(), progress_tx.clone()),
        ChildStdoutRoute::Stdout => tee_pipe_async(pipe, std::io::stdout(), progress_tx.clone()),
    });
    let stderr_reader = child.stderr.take().map(|pipe| {
        if suppress {
            tee_pipe_async(pipe, std::io::sink(), progress_tx.clone())
        } else {
            tee_pipe_async(pipe, std::io::stderr(), progress_tx.clone())
        }
    });
    InstallerPipeReaders::Forward {
        stdout: stdout_reader,
        stderr: stderr_reader,
    }
}

fn tee_pipe_async<R, W>(
    mut pipe: R,
    mut terminal: W,
    progress: mpsc::Sender<()>,
) -> mpsc::Receiver<std::io::Result<()>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let (done_tx, done_rx) = mpsc::channel();
    let _ = thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        let result = (|| loop {
            let read = pipe.read(&mut buffer)?;
            if read == 0 {
                return Ok(());
            }
            terminal.write_all(&buffer[..read])?;
            terminal.flush()?;
            // The receiver intentionally does not care which pipe moved: any
            // byte from the child is progress.
            let _ = progress.send(());
        })();
        let _ = done_tx.send(result);
    });
    done_rx
}

fn capture_pipe_async<R>(
    mut pipe: R,
    progress: mpsc::Sender<()>,
) -> mpsc::Receiver<std::io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    let (done_tx, done_rx) = mpsc::channel();
    let _ = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 8192];
        let result = (|| loop {
            let read = pipe.read(&mut buffer)?;
            if read == 0 {
                return Ok(bytes);
            }
            bytes.extend_from_slice(&buffer[..read]);
            let _ = progress.send(());
        })();
        let _ = done_tx.send(result);
    });
    done_rx
}

fn wait_for_pipe_drain(
    reader: Option<mpsc::Receiver<std::io::Result<()>>>,
    context: &str,
    pipe_name: &str,
) -> Result<(), SoldrError> {
    match reader {
        Some(receiver) => receiver
            .recv_timeout(PIPE_DRAIN_TIMEOUT)
            .map_err(|_| {
                SoldrError::Other(format!(
                    "{context}: {pipe_name} remained open for {} seconds after the installer exited; a descendant may still be running",
                    PIPE_DRAIN_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|err| {
                SoldrError::Other(format!(
                    "failed to forward {pipe_name} from {context}: {err}"
                ))
            }),
        None => Ok(()),
    }
}

fn wait_for_capture(
    reader: Option<mpsc::Receiver<std::io::Result<Vec<u8>>>>,
    context: &str,
    pipe_name: &str,
) -> Result<Vec<u8>, SoldrError> {
    match reader {
        Some(receiver) => receiver
            .recv_timeout(PIPE_DRAIN_TIMEOUT)
            .map_err(|_| {
                SoldrError::Other(format!(
                    "{context}: {pipe_name} remained open for {} seconds after the installer exited; a descendant may still be running",
                    PIPE_DRAIN_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|err| {
                SoldrError::Other(format!(
                    "failed to capture {pipe_name} from {context}: {err}"
                ))
            }),
        None => Ok(Vec::new()),
    }
}

trait ProgressProbe {
    fn made_progress(&mut self) -> bool;
}

struct ProcessTreeCpuProbe {
    root_pid: u32,
    previous_ticks: Option<u64>,
}

impl ProcessTreeCpuProbe {
    fn new(root_pid: u32) -> Self {
        Self {
            root_pid,
            previous_ticks: crate::platform::process::cpu_ticks::process_tree_cpu_ticks(root_pid),
        }
    }
}

impl ProgressProbe for ProcessTreeCpuProbe {
    fn made_progress(&mut self) -> bool {
        let Some(current_ticks) =
            crate::platform::process::cpu_ticks::process_tree_cpu_ticks(self.root_pid)
        else {
            return false;
        };
        let progressed = self
            .previous_ticks
            .is_some_and(|previous_ticks| current_ticks > previous_ticks);
        self.previous_ticks = Some(current_ticks);
        progressed
    }
}

fn wait_for_child_with_watchdog<P: ProgressProbe>(
    child: &mut Child,
    context: &str,
    phase: &str,
    config: &InstallerWatchdogConfig,
    progress_rx: &mpsc::Receiver<()>,
    mut probe: P,
) -> Result<ExitStatus, SoldrError> {
    let started = Instant::now();
    let mut last_progress = started;
    // Output-only timeline for the soldr#2359 heartbeat: CPU activity resets
    // the stall watchdog but not this, because a busy-but-silent child is
    // what looks like a hang from the caller's side.
    let mut last_output = started;
    let mut last_heartbeat = started;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|err| SoldrError::Other(format!("wait on {context} failed: {err}")))?
        {
            return Ok(status);
        }

        while progress_rx.try_recv().is_ok() {
            last_progress = Instant::now();
            last_output = last_progress;
        }
        if probe.made_progress() {
            last_progress = Instant::now();
        }

        let now = Instant::now();
        // Quiet machine-readable mode also silences the heartbeat — with
        // the tee routed to a sink, output-silence is guaranteed and the
        // heartbeat would corrupt the parseable payload it protects.
        if let (Some(interval), false) = (
            config.heartbeat_interval,
            super::quiet::diagnostics_suppressed(),
        ) {
            if heartbeat_due(
                now.duration_since(last_output),
                now.duration_since(last_heartbeat),
                interval,
            ) {
                eprintln!(
                    "{}",
                    heartbeat_line(
                        context,
                        now.duration_since(started),
                        now.duration_since(last_output)
                    )
                );
                last_heartbeat = now;
            }
        }
        if now.duration_since(started) >= config.safety_timeout {
            return Err(kill_for_watchdog(
                child,
                context,
                phase,
                "safety-ceiling",
                now.duration_since(started),
                now.duration_since(last_progress),
                config,
            ));
        }
        if now.duration_since(last_progress) >= config.stall_timeout {
            return Err(kill_for_watchdog(
                child,
                context,
                phase,
                "stall",
                now.duration_since(started),
                now.duration_since(last_progress),
                config,
            ));
        }
        // Wake on child exit instead of rounding short toolchain probes up
        // to the watchdog's progress sampling interval.
        match crate::platform::wait::try_wait_for(child, config.poll_interval) {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            // Keep supervising after a wait error.
            Err(_) => thread::sleep(config.poll_interval),
        }
    }
}

fn kill_for_watchdog(
    child: &mut Child,
    context: &str,
    phase: &str,
    category: &str,
    elapsed: Duration,
    since_progress: Duration,
    config: &InstallerWatchdogConfig,
) -> SoldrError {
    let kill_result = kill_installer_process_tree(child);
    let reap_result = crate::platform::wait::try_wait_for(
        child,
        Duration::from_secs(KILLED_INSTALLER_REAP_TIMEOUT_SECS),
    );
    let mut message = format!(
        "{context}: installer watchdog category={category} phase={phase} total_elapsed={}s since_progress={}s; stall_timeout={}s ({INSTALLER_STALL_TIMEOUT_ENV_VAR}); safety_ceiling={}s ({})",
        elapsed.as_secs(),
        since_progress.as_secs(),
        config.stall_timeout.as_secs(),
        config.safety_timeout.as_secs(),
        config.safety_timeout_env_var,
    );
    match kill_result {
        Ok(detail) => message.push_str(&format!("; {detail}")),
        Err(err) => message.push_str(&format!("; kill failed: {err}")),
    }
    match reap_result {
        Ok(Some(_)) => {}
        Ok(None) => message.push_str(&format!(
            "; process did not exit within {KILLED_INSTALLER_REAP_TIMEOUT_SECS} seconds after kill"
        )),
        Err(err) => message.push_str(&format!("; reap after kill failed: {err}")),
    }
    SoldrError::Other(message)
}

fn kill_installer_process_tree(child: &mut Child) -> std::io::Result<&'static str> {
    use crate::platform::process::terminate::TreeKill;
    match crate::platform::process::terminate::terminate_tree(child)? {
        TreeKill::TreeKilled => Ok("killed installer process tree"),
        TreeKill::ProcessKilled => Ok("killed installer process"),
    }
}

fn configure_installer_process_tree(command: &mut Command) {
    crate::platform::process::command::configure_process_group(command);
}

#[cfg(test)]
#[path = "installer_watchdog_tests.rs"]
mod tests;
