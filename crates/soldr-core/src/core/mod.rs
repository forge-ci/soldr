//! Shared foundational types for soldr.
//!
//! The `core` module owns target-triple resolution, `~/.soldr/`
//! layout (`SoldrPaths` / `SoldrConfig`), `SoldrError`, and the
//! runtime toolchain-discovery helpers. It is split across
//! sub-modules but the external API (`crate::core::*` /
//! `soldr_cli::core::*`) is preserved via re-exports below — every
//! identifier consumers reach for must show up in this `mod.rs`.

use std::ffi::OsStr;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;
use wait_timeout::ChildExt;

/// soldr#941/#2336 — single-source-of-truth for the canonical 9-target list.
/// Mirror of `[workspace.metadata.soldr].targets` in the root
/// `Cargo.toml`; a parity test enforces byte-equality.
pub mod canonical_targets;
pub mod cargo_target_dir;
pub mod cpu_topology;
pub mod env_flag;
pub mod git;
pub mod installer_watchdog;
/// soldr#1761 — soldr-owned compile concurrency limit, resolved once
/// and shared by the admission queue and the compile semaphore.
pub mod jobs;
/// soldr#3407 — zccache cache-hit delivery mode (`ZCCACHE_MODE`), resolved
/// once and injected on the cargo child.
pub mod materialization_mode;
mod paths;
pub mod quiet;
/// soldr#3098 -- staged-write / child-spawn exclusion (re-export of the
/// platform leaf's lock).
pub mod spawn_exclusion;
mod target_triple;
mod temp;
pub mod tool_output;
pub mod toolchain_install_tripwire;
mod toolchain_manifest;
mod toolchain_resolve;
pub mod wire;

pub use canonical_targets::{canonical_targets, is_canonical, CANONICAL_TARGETS};
pub use env_flag::{flag, flag_value, foreign_flag, foreign_flag_value, is_off_value};
pub use installer_watchdog::{
    installer_safety_timeout, installer_stall_timeout, run_installer_command,
    run_installer_command_output, InstallerWatchdogConfig, DEFAULT_INSTALLER_SAFETY_TIMEOUT_SECS,
    DEFAULT_INSTALLER_STALL_TIMEOUT_SECS, INSTALLER_SAFETY_TIMEOUT_ENV_VAR,
    INSTALLER_STALL_TIMEOUT_ENV_VAR,
};
pub use paths::{
    resolve_cargo_home, resolve_rustup_home, AutoGcConfig, CookConfig, GcConfig, InstallConfig,
    PinsConfig, SoldrConfig, SoldrConfigLoadError, SoldrPaths, MANAGED_SHIM_VERSION,
    SMALL_TOOL_LOG_FILE, SOLDR_CACHE_DIR_ENV_VAR,
};
pub use target_triple::{Arch, Env, Os, TargetTriple};
pub use temp::{
    ensure_temp_root, ensure_temp_root_for, replace_file_with_dir, temp_root, temp_root_for,
    SOLDR_TMPDIR_ENV_VAR,
};
pub use toolchain_install_tripwire::{
    forbid_toolchain_install_tripwire, FORBID_TOOLCHAIN_INSTALL_ENV_VAR, TEST_RUSTUP_BIN_ENV_VAR,
};
pub use toolchain_manifest::{
    read_rust_toolchain_manifest, read_rust_toolchain_manifest_from_ancestors, PluginSpec,
    RustToolchainManifest, SoldrCookManifest, SoldrManifestSection,
};
pub use toolchain_resolve::{
    apply_implicit_toolchain_homes, find_rust_toolchain_manifest, probe_toolchain_binary,
    suppress_windows_console_window,
};

pub const CARGO_HOME_ENV_VAR: &str = "CARGO_HOME";
pub const RUSTUP_HOME_ENV_VAR: &str = "RUSTUP_HOME";
pub(crate) const RUSTUP_TOOLCHAIN_ENV_VAR: &str = "RUSTUP_TOOLCHAIN";
pub const COMMAND_OUTPUT_TIMEOUT_ENV_VAR: &str = "SOLDR_COMMAND_OUTPUT_TIMEOUT_SECS";
/// Compiler-unit report path propagated from `soldr ci-test` stages to the
/// daemon through each wrapper request.
pub const CI_TEST_REPORT_PATH_ENV_VAR: &str = "SOLDR_CI_TEST_REPORT_PATH";
/// Name of the `soldr ci-test` stage that originated a compiler request.
pub const CI_TEST_STAGE_ENV_VAR: &str = "SOLDR_CI_TEST_STAGE";
pub const DEFAULT_COMMAND_OUTPUT_TIMEOUT_SECS: u64 = 60;
const KILLED_COMMAND_OUTPUT_REAP_TIMEOUT_SECS: u64 = 5;
/// How often [`command_output_with_timeout`] re-checks a still-running child.
/// Only bounds how late a genuinely silent command is noticed; it does not
/// bound how long a *progressing* command may run.
const COMMAND_OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum SoldrError {
    #[error("unsupported platform: {0}")]
    UnsupportedPlatform(String),
    #[error("no home directory found")]
    NoHomeDir,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("network error: {0}")]
    Network(String),
    #[error("tool not found: {0}")]
    ToolNotFound(String),
    #[error("archive error: {0}")]
    Archive(String),
    #[error("{0}")]
    Other(String),
}

// ---------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ---------------------------------------------------------------------------
// Home-dir resolution + small env helpers
// ---------------------------------------------------------------------------

/// Public accessor for the soldr home directory used by the `gc`
/// allowlist default (`~/dev`). Returns `Err` when no home dir can be
/// resolved.
pub fn user_home_dir() -> Result<PathBuf, SoldrError> {
    home_dir()
}

/// Expand `~` and `~/...` strings to absolute paths under the user's
/// home directory. Other inputs pass through unchanged.
pub fn expand_user_home(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~") {
        if let Ok(home) = home_dir() {
            let trimmed = rest.trim_start_matches(['/', '\\']);
            if trimmed.is_empty() {
                return home;
            }
            return home.join(trimmed);
        }
    }
    PathBuf::from(input)
}

pub(crate) fn soldr_root_from_env_var(
    value: Option<&OsStr>,
) -> Option<Result<PathBuf, SoldrError>> {
    non_empty_env_path(value).map(Ok)
}

pub(crate) fn non_empty_env_path(value: Option<&OsStr>) -> Option<PathBuf> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

pub fn home_dir() -> Result<PathBuf, SoldrError> {
    crate::platform::host::dirs::home().ok_or(SoldrError::NoHomeDir)
}

/// Run `command` to completion, failing only when it goes **silent** for the
/// budget — not when it merely takes a long time.
///
/// soldr#2974: this used to be `child.wait_timeout(command_output_timeout())`,
/// a hard wall-clock deadline, despite the name and the operator-facing
/// `SOLDR_COMMAND_OUTPUT_TIMEOUT_SECS` both promising an *output* timeout. Any
/// child that legitimately ran longer than 60s was killed mid-work. That is
/// how `soldr dylint` destroyed its own nightly install: the toolchain fetch
/// was still downloading when the deadline fired, and the partial tree it left
/// behind has no rustup manifest, so `rustup toolchain list` reports it as
/// installed while `rustup component add` refuses to repair it.
///
/// The budget now measures time since the last byte of output. A command that
/// keeps streaming runs as long as it needs; only genuine silence expires it.
/// `installer_watchdog::run_installer_command` already worked this way (10s
/// heartbeats under a 24h ceiling), which is why `rustup component add`
/// survived 70s of silent downloading in the same run that killed a sibling
/// call — two implementations of one idea, disagreeing.
pub fn command_output_with_timeout(
    command: &mut Command,
    context: &str,
) -> Result<Output, SoldrError> {
    command_output_with_timeout_inner(
        command,
        context,
        command_output_timeout(),
        CommandOutputTimeout::Inactivity,
        true,
    )
}

/// Run a command through soldr's sanctioned output-capture containment with a
/// caller-selected **wall-clock** deadline. This is for small host probes whose
/// timeout is a protocol property rather than user-configurable build policy.
/// Unlike [`command_output_with_timeout`], output does not extend this bound.
pub fn command_output_with_timeout_duration(
    command: &mut Command,
    context: &str,
    timeout: Duration,
) -> Result<Output, SoldrError> {
    command_output_with_timeout_inner(
        command,
        context,
        timeout,
        CommandOutputTimeout::WallClock,
        false,
    )
}

#[derive(Clone, Copy)]
enum CommandOutputTimeout {
    /// The user-configurable command setting measures a quiet interval.
    Inactivity,
    /// Bounded host probes must finish within their caller-provided deadline,
    /// even if a wedged child keeps writing progress-like output.
    WallClock,
}

fn command_output_with_timeout_inner(
    command: &mut Command,
    context: &str,
    timeout: Duration,
    timeout_kind: CommandOutputTimeout,
    suggest_timeout_override: bool,
) -> Result<Output, SoldrError> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = {
        // soldr#3098: spawns share, staged writes exclude.
        let _spawn = spawn_exclusion::spawn_shared();
        command
            .spawn()
            .map_err(|err| SoldrError::Other(format!("failed to invoke {context}: {err}")))?
    };
    let started = Instant::now();
    // Milliseconds since `started` at which either pipe last produced bytes.
    // Shared with both reader threads; `Relaxed` is sufficient because the
    // value is only ever compared against a clock, never used to order other
    // memory.
    let last_output = Arc::new(AtomicU64::new(0));
    let stdout_reader = child
        .stdout
        .take()
        .map(|pipe| read_pipe_async(pipe, Arc::clone(&last_output), started));
    let stderr_reader = child
        .stderr
        .take()
        .map(|pipe| read_pipe_async(pipe, Arc::clone(&last_output), started));
    let status = loop {
        match child
            .wait_timeout(COMMAND_OUTPUT_POLL_INTERVAL)
            .map_err(|err| SoldrError::Other(format!("wait on {context} failed: {err}")))?
        {
            Some(status) => break status,
            None => {
                let elapsed = started.elapsed();
                let timed_out = match timeout_kind {
                    CommandOutputTimeout::Inactivity => {
                        let silent_for = elapsed.saturating_sub(Duration::from_millis(
                            last_output.load(Ordering::Relaxed),
                        ));
                        silent_for >= timeout
                    }
                    CommandOutputTimeout::WallClock => elapsed >= timeout,
                };
                if !timed_out {
                    continue;
                }
                let kill_result = child.kill();
                let reap_result = child
                    .wait_timeout(Duration::from_secs(KILLED_COMMAND_OUTPUT_REAP_TIMEOUT_SECS));
                let timeout_secs = timeout.as_secs();
                let elapsed_secs = elapsed.as_secs();
                let mut message = match timeout_kind {
                    CommandOutputTimeout::Inactivity => format!(
                        "{context} produced no output for {timeout_secs} seconds \
                         ({elapsed_secs}s elapsed)"
                    ),
                    CommandOutputTimeout::WallClock => {
                        format!("{context} timed out after {timeout_secs} seconds")
                    }
                };
                if suggest_timeout_override {
                    message.push_str(&format!(
                        " (set {COMMAND_OUTPUT_TIMEOUT_ENV_VAR} to override)"
                    ));
                }
                match kill_result {
                    Ok(()) => message.push_str("; killed child process"),
                    Err(err) => message.push_str(&format!("; kill failed: {err}")),
                }
                match reap_result {
                    Ok(Some(_)) => message.push_str("; reaped child process"),
                    Ok(None) => message.push_str(&format!(
                        "; process did not exit within {KILLED_COMMAND_OUTPUT_REAP_TIMEOUT_SECS} seconds after kill"
                    )),
                    Err(err) => message.push_str(&format!("; reap after kill failed: {err}")),
                }
                return Err(SoldrError::Other(message));
            }
        }
    };

    let stdout = join_pipe_reader(stdout_reader, context, "stdout")?;
    let stderr = join_pipe_reader(stderr_reader, context, "stderr")?;

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Drain `pipe`, stamping `last_output` on every chunk.
///
/// Chunked rather than `read_to_end` because the stamp is the whole point:
/// `read_to_end` only returns at EOF, so it can report progress exactly once,
/// when the command is already over.
fn read_pipe_async<R>(
    mut pipe: R,
    last_output: Arc<AtomicU64>,
    started: Instant,
) -> JoinHandle<std::io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => return Ok(bytes),
                Ok(read) => {
                    bytes.extend_from_slice(&buffer[..read]);
                    let elapsed_ms =
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    // Monotonic: two readers race, and a stale stamp from the
                    // slower one must never move the deadline backwards.
                    last_output.fetch_max(elapsed_ms, Ordering::Relaxed);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    })
}

fn join_pipe_reader(
    reader: Option<JoinHandle<std::io::Result<Vec<u8>>>>,
    context: &str,
    pipe_name: &str,
) -> Result<Vec<u8>, SoldrError> {
    match reader {
        Some(handle) => handle
            .join()
            .map_err(|_| SoldrError::Other(format!("{pipe_name} reader panicked for {context}")))?
            .map_err(|err| {
                SoldrError::Other(format!("failed to read {pipe_name} from {context}: {err}"))
            }),
        None => Ok(Vec::new()),
    }
}

pub fn command_output_timeout() -> Duration {
    std::env::var(COMMAND_OUTPUT_TIMEOUT_ENV_VAR)
        .ok()
        .and_then(|value| command_output_timeout_from_str(&value))
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_COMMAND_OUTPUT_TIMEOUT_SECS))
}

fn command_output_timeout_from_str(value: &str) -> Option<Duration> {
    value
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn command_output_timeout_parses_positive_values_only() {
        assert_eq!(
            command_output_timeout_from_str("7"),
            Some(Duration::from_secs(7))
        );
        assert_eq!(command_output_timeout_from_str("0"), None);
        assert_eq!(command_output_timeout_from_str("not-a-number"), None);
    }

    #[test]
    fn hard_wall_clock_timeout_drains_pipe_filling_child_then_kills_and_reaps_it() {
        if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Linux {
            return;
        }

        let script = "dd if=/dev/zero bs=1024 count=256 2>/dev/null; while :; do :; done";
        let started = std::time::Instant::now();
        let error = command_output_with_timeout_duration(
            Command::new("sh").args(["-c", script]),
            "pipe-filling test child",
            Duration::from_secs(1),
        )
        .expect_err("pipe-filling child must time out after its output drains");

        assert!(
            started.elapsed() < Duration::from_secs(3),
            "pipe filling must not prevent the bounded timeout"
        );
        let message = error.to_string();
        assert!(
            message.contains("timed out after 1 seconds"),
            "the hard probe timeout must remain wall-clock based: {message}"
        );
        assert!(message.contains("killed child process"), "{message}");
        assert!(message.contains("reaped child process"), "{message}");
    }

    /// The caller-selected probe deadline is deliberately different from the
    /// generic output-inactivity budget: a wedged host probe that keeps
    /// printing must still be contained within the declared wall-clock bound.
    #[test]
    fn explicit_duration_timeout_stops_a_chatty_child_and_reaps_it() {
        let mut command = chatty_command(30, 1);
        let started = Instant::now();
        let error = command_output_with_timeout_duration(
            &mut command,
            "chatty bounded probe",
            Duration::from_secs(1),
        )
        .expect_err("a bounded probe must not let continuing output extend its deadline");

        assert!(
            started.elapsed() < Duration::from_secs(3),
            "a chatty child must still be stopped at the explicit deadline"
        );
        let message = error.to_string();
        assert!(message.contains("timed out after 1 seconds"), "{message}");
        assert!(message.contains("killed child process"), "{message}");
        assert!(message.contains("reaped child process"), "{message}");
    }

    /// Host dispatch goes through `platform::host::facts`, never `cfg!`:
    /// `.github/scripts/platform_cfg_boundary_ratchet.py` and the
    /// `ban_platform_cfg_outside_boundary` lint both reject a raw host `cfg`
    /// here, and a test is not exempt from the boundary its production code
    /// keeps.
    fn host_is_windows() -> bool {
        crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows
    }

    /// Sleep `seconds` in a `cmd` one-liner without `timeout.exe`.
    ///
    /// The Windows target-run lanes put an MSYS `timeout` ahead of the Windows
    /// one on PATH, so `timeout /T 30 /NOBREAK` there fails instantly with
    /// `timeout: invalid time interval '/T'` and the child exits rather than
    /// sleeping. `ping -n` is the sleep the repo's own fake-cargo fixtures
    /// already use for this reason. `-n` counts pings, not gaps, so a
    /// `seconds`-long wait needs `seconds + 1`.
    fn windows_sleep(seconds: u64) -> String {
        format!("ping -n {} 127.0.0.1 >nul", seconds + 1)
    }

    /// A child that makes immediate, regular progress on both captured
    /// streams. The generic output watchdog must treat either stream as
    /// progress; a true wall-clock probe uses a separate test above.
    fn chatty_command(chunks: usize, gap_secs: u64) -> Command {
        if host_is_windows() {
            let mut command = Command::new("powershell.exe");
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "1..{chunks} | ForEach-Object {{ Write-Output stdout-progress; [Console]::Error.WriteLine('stderr-progress'); Start-Sleep -Seconds {gap_secs} }}; Write-Output done"
                ),
            ]);
            command
        } else {
            let mut command = Command::new("sh");
            command.arg("-c").arg(format!(
                "i=0; while [ $i -lt {chunks} ]; do echo stdout-progress; echo stderr-progress >&2; sleep {gap_secs}; i=$((i+1)); done; echo done"
            ));
            command
        }
    }

    fn generic_chatty_inactivity_budget() -> Duration {
        if host_is_windows() {
            // Windows target-run startup and pipe scheduling are routinely
            // slower than Linux. The child prints immediately and repeatedly,
            // then exceeds this budget, so this remains a real inactivity
            // regression rather than a tiny timing assertion.
            Duration::from_secs(10)
        } else {
            Duration::from_secs(2)
        }
    }

    /// soldr#2974: the budget is silence, not runtime. This command runs
    /// materially longer than the selected inactivity budget and must finish,
    /// because it never stops talking on either stream. Under the previous
    /// `wait_timeout(budget)` it was killed at that budget.
    #[test]
    fn a_command_that_keeps_producing_output_outlives_the_budget() {
        let budget = generic_chatty_inactivity_budget();
        let budget_secs = budget.as_secs().to_string();
        let _guard = EnvGuard::set(COMMAND_OUTPUT_TIMEOUT_ENV_VAR, &budget_secs);
        let chunks = usize::try_from(budget.as_secs() + 2).expect("short test budget");
        let mut command = chatty_command(chunks, 1);
        let started = Instant::now();
        let output = command_output_with_timeout(&mut command, "chatty probe")
            .expect("a continuously-progressing command must not be killed");
        assert!(output.status.success(), "{output:?}");
        assert!(
            started.elapsed() > budget,
            "the child must outlast the inactivity budget while both streams progress"
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("done"), "child was cut short: {text:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("stderr-progress"),
            "stderr progress must also reset the generic watchdog"
        );
    }

    /// The other half: silence still expires, and the message says so.
    #[test]
    fn a_silent_command_still_expires_and_names_the_silence() {
        let _guard = EnvGuard::set(COMMAND_OUTPUT_TIMEOUT_ENV_VAR, "1");
        let mut command = if host_is_windows() {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(windows_sleep(30));
            c
        } else {
            let mut c = Command::new("sh");
            c.arg("-c").arg("sleep 30");
            c
        };
        let error = command_output_with_timeout(&mut command, "silent probe")
            .expect_err("a command that produces nothing must still expire");
        let message = error.to_string();
        assert!(
            message.contains("produced no output for 1 seconds"),
            "the message must name silence, not runtime: {message}"
        );
        assert!(message.contains("killed child process"), "{message}");
    }

    /// Scoped env mutation for the two tests above. They are the only users,
    /// and both set the same variable, so one barrier covers it.
    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let lock = LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self {
                key,
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
