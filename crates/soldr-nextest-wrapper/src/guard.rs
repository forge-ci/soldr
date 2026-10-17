//! Memory-aware admission and memory-failure isolation for one test
//! (soldr#2885). The protocol (`paused` flag, `active/<pid>` slots,
//! `resume.lock` spacing, `infra/<identity>` records, exit status 75) is
//! shared with `soldr ci-test`'s controller
//! (`crates/soldr-cli/src/ci_test/test_pressure.rs`); renaming either side
//! breaks the gate.

use crate::memory::{
    format_bytes, is_linux, kill_tree, observe_memory, sample_interval, sample_tree, CgroupCeiling,
    CgroupOutcome, MemoryObservation, TreeSample,
};
use crate::tail::OutputTail;
use crate::write_stderr;
use sha2::{Digest, Sha256};
use soldr_platform::process::test_child;
use soldr_platform::sync::Mutex;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

pub const ADMISSION_DIR_ENV: &str = "SOLDR_NEXTEST_ADMISSION_DIR";
pub const MAX_WAIT_ENV: &str = "SOLDR_NEXTEST_ADMISSION_MAX_WAIT_SECS";
pub const CEILING_ENV: &str = "SOLDR_NEXTEST_TEST_MEMORY_CEILING_BYTES";
pub const SUMMARY_ENV: &str = "SOLDR_NEXTEST_ADMISSION_SUMMARY";
pub const CGROUP_ROOT_ENV: &str = "SOLDR_NEXTEST_CGROUP_ROOT";
/// Stripped from the test's own environment: a nested Nextest must not join
/// (or be counted by) the outer run's admission controller.
pub const CONTROL_ENVS: [&str; 5] = [
    ADMISSION_DIR_ENV,
    MAX_WAIT_ENV,
    CEILING_ENV,
    SUMMARY_ENV,
    CGROUP_ROOT_ENV,
];

const PAUSED_FLAG: &str = "paused";
const ACTIVE_DIR: &str = "active";
const INFRA_DIR: &str = "infra";
const RESUME_LOCK: &str = "resume.lock";

/// EX_TEMPFAIL: "a temporary failure ... the user is invited to retry".
pub const INFRA_EXIT_CODE: i32 = 75;
const DEFAULT_MAX_WAIT_SECS: f64 = 30.0;
const RESUME_SPACING: Duration = Duration::from_millis(500);
const GATE_POLL: Duration = Duration::from_millis(100);

const MEMORY_SIGNATURES: [&str; 5] = [
    "Cannot allocate memory",
    "MemoryError",
    "kind: OutOfMemory",
    "(os error 12)",
    "out of memory",
];

fn positive_number(raw: Option<&str>) -> Option<f64> {
    let value: f64 = raw.unwrap_or("").trim().parse().ok()?;
    (value > 0.0 && value.is_finite()).then_some(value)
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// `{:g}`-style seconds: `30` rather than `30.0`.
fn seconds(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

pub struct GuardConfig {
    pub admission_dir: Option<PathBuf>,
    pub max_wait_secs: f64,
    pub ceiling_bytes: Option<u64>,
    pub summary: String,
    pub cgroup_root: Option<PathBuf>,
}

impl GuardConfig {
    pub fn from_env() -> Self {
        let directory = env(ADMISSION_DIR_ENV).unwrap_or_default();
        let cgroup = env(CGROUP_ROOT_ENV).unwrap_or_default();
        Self {
            admission_dir: (!directory.trim().is_empty()).then(|| PathBuf::from(directory.trim())),
            max_wait_secs: positive_number(env(MAX_WAIT_ENV).as_deref())
                .unwrap_or(DEFAULT_MAX_WAIT_SECS),
            ceiling_bytes: positive_number(env(CEILING_ENV).as_deref())
                .map(|value| value as u64)
                .filter(|value| *value > 0),
            summary: env(SUMMARY_ENV).unwrap_or_default().trim().to_owned(),
            cgroup_root: (!cgroup.trim().is_empty() && is_linux())
                .then(|| PathBuf::from(cgroup.trim())),
        }
    }

    pub fn enabled(&self) -> bool {
        self.admission_dir.is_some() || self.ceiling_bytes.is_some()
    }
}

/// Nextest's own `<binary-id> <test-name>`, or the argv it implies.
pub fn identity_of(command: &[String]) -> String {
    let binary = env("NEXTEST_BINARY_ID").unwrap_or_default();
    let name = env("NEXTEST_TEST_NAME").unwrap_or_default();
    if !binary.trim().is_empty() && !name.trim().is_empty() {
        return format!("{} {}", binary.trim(), name.trim());
    }
    let program = command
        .first()
        .map(|program| {
            Path::new(program).file_name().map_or_else(
                || program.clone(),
                |name| name.to_string_lossy().into_owned(),
            )
        })
        .unwrap_or_else(|| "<unknown>".into());
    let positional = command
        .iter()
        .skip(1)
        .find(|arg| !arg.starts_with('-'))
        .map_or("", String::as_str);
    format!("{program} {positional}").trim().to_owned()
}

/// A file name that round-trips the identity through percent-encoding.
pub fn infra_record_name(identity: &str) -> String {
    let encoded: String = identity
        .bytes()
        .map(|byte| {
            if byte == b'%' || byte == b'/' || !(0x20..0x7F).contains(&byte) {
                format!("%{byte:02X}")
            } else {
                char::from(byte).to_string()
            }
        })
        .collect();
    if encoded.len() <= 200 {
        return encoded;
    }
    let digest = Sha256::digest(identity.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}~{}", &encoded[..160], &hex[..16])
}

fn running_tests(admission_dir: &Path, exclude_pid: u32) -> usize {
    let Ok(entries) = std::fs::read_dir(admission_dir.join(ACTIVE_DIR)) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| *pid != exclude_pid && test_child::pid_alive(*pid))
        .count()
}

/// Release paused waiters one at a time, `RESUME_SPACING` apart.
fn space_resume(admission_dir: &Path) {
    let lock_path = admission_dir.join(RESUME_LOCK);
    let (file, first) = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(file) => (file, true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)
            {
                Ok(file) => (file, false),
                Err(_) => return,
            }
        }
        Err(_) => return,
    };
    if soldr_platform::wait::lock_exclusive_for(&file, soldr_platform::wait::LONG_LOCK).is_err() {
        return;
    }
    let since = file
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok());
    if let Some(since) = since.filter(|since| !first && *since < RESUME_SPACING) {
        std::thread::sleep(RESUME_SPACING - since);
    }
    let _ = file.set_modified(SystemTime::now());
}

/// Hold a not-yet-started test while ci-test reports memory pressure.
fn await_admission(config: &GuardConfig, own_pid: u32) {
    let Some(directory) = &config.admission_dir else {
        return;
    };
    let paused = directory.join(PAUSED_FLAG);
    let blocked = || paused.exists() && running_tests(directory, own_pid) > 0;
    let started = Instant::now();
    let mut waited = false;
    loop {
        if !blocked() {
            if !waited {
                return;
            }
            // Queue behind earlier waiters, then re-check: pressure may have
            // returned while this test was queued.
            space_resume(directory);
            if !blocked() {
                break;
            }
        }
        if started.elapsed().as_secs_f64() >= config.max_wait_secs {
            write_stderr(&format!(
                "nextest memory: admitted after the {}s pressure wait bound; memory available now: {}\n",
                seconds(config.max_wait_secs),
                observe_memory().describe()
            ));
            return;
        }
        waited = true;
        std::thread::sleep(GATE_POLL);
    }
    write_stderr(&format!(
        "nextest memory: admission paused by memory pressure for {:.1}s\n",
        started.elapsed().as_secs_f64()
    ));
}

/// `<dir>/active/<wrapper pid>` for exactly the life of the test.
struct ActiveSlot(Option<PathBuf>);

impl ActiveSlot {
    fn new(admission_dir: Option<&Path>, pid: u32) -> Self {
        let path = admission_dir.map(|dir| dir.join(ACTIVE_DIR).join(pid.to_string()));
        Self(path.filter(|path| {
            path.parent()
                .is_some_and(|parent| std::fs::create_dir_all(parent).is_ok())
                && std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .is_ok()
        }))
    }

    fn release(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Default)]
struct MonitorState {
    peak: TreeSample,
    exceeded: Option<TreeSample>,
}

/// Sample the test's tree; kill it once it crosses the sampled ceiling.
struct TreeMonitor {
    state: Arc<Mutex<MonitorState>>,
    stop: mpsc::Sender<()>,
    handle: JoinHandle<()>,
}

impl TreeMonitor {
    fn start(root: u32, ceiling_bytes: Option<u64>) -> Self {
        let state = Arc::new(Mutex::new(MonitorState::default()));
        let (stop, stopped) = mpsc::channel();
        let shared = Arc::clone(&state);
        let interval = sample_interval();
        let handle = std::thread::spawn(move || loop {
            let Some(sample) = sample_tree(root) else {
                return;
            };
            {
                let mut state = shared.lock();
                if sample.rss_bytes > state.peak.rss_bytes {
                    state.peak = sample.clone();
                }
                if ceiling_bytes.is_some_and(|ceiling| sample.rss_bytes > ceiling) {
                    state.exceeded = Some(sample);
                    drop(state);
                    kill_tree(root);
                    return;
                }
            }
            match stopped.recv_timeout(interval) {
                Err(RecvTimeoutError::Timeout) => {}
                _ => return,
            }
        });
        Self {
            state,
            stop,
            handle,
        }
    }

    fn stop(self) -> MonitorState {
        let _ = self.stop.send(());
        let _ = soldr_platform::wait::join_for(self.handle, soldr_platform::wait::HELPER_JOIN);
        std::mem::take(&mut *self.state.lock())
    }
}

pub struct ProcessGuard {
    config: GuardConfig,
    identity: String,
    own_pid: u32,
    at_admission: Option<MemoryObservation>,
    slot: Option<ActiveSlot>,
    monitor: Option<TreeMonitor>,
    cgroup: Option<CgroupCeiling>,
    pub stdout_tail: OutputTail,
    pub stderr_tail: OutputTail,
}

impl ProcessGuard {
    pub fn new(command: &[String]) -> Self {
        Self {
            config: GuardConfig::from_env(),
            identity: identity_of(command),
            own_pid: std::process::id(),
            at_admission: None,
            slot: None,
            monitor: None,
            cgroup: None,
            stdout_tail: OutputTail::default(),
            stderr_tail: OutputTail::default(),
        }
    }

    pub fn before_spawn(&mut self) {
        if !self.config.enabled() {
            return;
        }
        await_admission(&self.config, self.own_pid);
        self.at_admission = Some(observe_memory());
        self.slot = Some(ActiveSlot::new(
            self.config.admission_dir.as_deref(),
            self.own_pid,
        ));
        if let (Some(root), Some(ceiling)) = (&self.config.cgroup_root, self.config.ceiling_bytes) {
            self.cgroup = CgroupCeiling::create(root, ceiling, self.own_pid);
        }
    }

    /// The `cgroup.procs` file the child joins before exec, if any.
    pub fn cgroup_procs(&self) -> Option<PathBuf> {
        self.cgroup.as_ref().map(CgroupCeiling::procs_path)
    }

    pub fn after_spawn(&mut self, pid: u32) {
        if !self.config.enabled() {
            return;
        }
        let mut sampled_ceiling = self.config.ceiling_bytes;
        if let Some(cgroup) = &mut self.cgroup {
            if cgroup.confirm(pid) {
                sampled_ceiling = None; // the kernel enforces memory.max
            } else {
                write_stderr(
                    "nextest memory: could not join the per-test cgroup; falling back to the sampled ceiling\n",
                );
            }
        }
        self.monitor = Some(TreeMonitor::start(pid, sampled_ceiling));
    }

    /// Classify the exited test; return the status the wrapper reports. A
    /// test Nextest itself terminated (its timeout) keeps Nextest's verdict.
    pub fn finish(&mut self, returncode: i32, terminated: bool) -> i32 {
        let monitor = self.monitor.take().map(TreeMonitor::stop);
        let outcome = self.cgroup.as_ref().map(CgroupCeiling::outcome);
        let cause = if terminated {
            None
        } else {
            self.classify(returncode, monitor.as_ref(), outcome.as_ref())
        };
        let status = match cause {
            None => returncode,
            Some(cause) => {
                self.report(&cause, Some(returncode), outcome.as_ref(), monitor.as_ref());
                INFRA_EXIT_CODE
            }
        };
        if let Some(cgroup) = self.cgroup.take() {
            cgroup.release();
        }
        if let Some(slot) = &mut self.slot {
            slot.release();
        }
        status
    }

    pub fn spawn_failed(&mut self, error: &std::io::Error) -> i32 {
        if let Some(slot) = &mut self.slot {
            slot.release();
        }
        if let Some(cgroup) = self.cgroup.take() {
            cgroup.release();
        }
        let hint = match error.raw_os_error() {
            Some(12) => "ENOMEM: memory exhaustion",
            Some(11) => "EAGAIN: PID or memory pressure",
            _ => "operating-system resource failure",
        };
        self.report(
            &format!("could not start the test process: {error} ({hint})"),
            None,
            None,
            None,
        );
        INFRA_EXIT_CODE
    }

    fn classify(
        &self,
        returncode: i32,
        monitor: Option<&MonitorState>,
        outcome: Option<&CgroupOutcome>,
    ) -> Option<String> {
        if let Some(exceeded) = monitor.and_then(|monitor| monitor.exceeded.as_ref()) {
            return Some(format!(
                "process tree exceeded the per-test memory ceiling {} (sampled RSS {}); terminated its process tree",
                format_bytes(self.config.ceiling_bytes),
                format_bytes(Some(exceeded.rss_bytes))
            ));
        }
        if let (Some(outcome), Some(cgroup)) = (outcome, &self.cgroup) {
            if outcome.oom_kills > 0 {
                return Some(format!(
                    "the kernel OOM-killed the test inside its per-test cgroup (memory.max={}, {})",
                    format_bytes(self.config.ceiling_bytes),
                    cgroup.path.display()
                ));
            }
        }
        if returncode != 0 && self.config.admission_dir.is_some() {
            let signature = memory_signature(&self.stderr_tail.bytes())
                .or_else(|| memory_signature(&self.stdout_tail.bytes()));
            if let Some(signature) = signature {
                return Some(format!(
                    "memory-exhaustion signature '{signature}' in the test's output"
                ));
            }
        }
        None
    }

    fn report(
        &self,
        cause: &str,
        returncode: Option<i32>,
        outcome: Option<&CgroupOutcome>,
        monitor: Option<&MonitorState>,
    ) {
        let at_failure = observe_memory();
        let peak = monitor.map(|monitor| &monitor.peak);
        let peak_text = if let Some(bytes) = outcome.and_then(|outcome| outcome.peak_bytes) {
            format!("{} (cgroup memory.peak)", format_bytes(Some(bytes)))
        } else if let Some(peak) = peak.filter(|peak| peak.process_count > 0) {
            format!(
                "{} across {} process(es)",
                format_bytes(Some(peak.rss_bytes)),
                peak.process_count
            )
        } else {
            "not observable".into()
        };
        let ceiling = if self.cgroup.as_ref().is_some_and(|cgroup| cgroup.joined) {
            format!(
                "{} (cgroup v2 memory.max)",
                format_bytes(self.config.ceiling_bytes)
            )
        } else if self.config.ceiling_bytes.is_some() {
            format!(
                "{} (sampled process-tree RSS)",
                format_bytes(self.config.ceiling_bytes)
            )
        } else {
            "none".into()
        };
        let tree = peak.map_or(0, |peak| peak.process_count);
        let pids = match at_failure.pids_current {
            Some(current) => format!(
                "cgroup pids.current={current} pids.max={}",
                at_failure.pids_limit.as_deref().unwrap_or("None")
            ),
            None => "cgroup pids unavailable".into(),
        };
        let summary = if self.config.summary.is_empty() {
            "no soldr ci-test admission summary"
        } else {
            &self.config.summary
        };
        let lines = [
            String::new(),
            "=== nextest memory: infrastructure failure, not a test assertion ===".into(),
            format!("test: {}", self.identity),
            format!("cause: {cause}"),
            format!("admission: {summary}"),
            format!("per-test memory ceiling: {ceiling}"),
            format!("process-tree peak RSS: {peak_text}"),
            format!(
                "memory available at admission: {}",
                self.at_admission
                    .as_ref()
                    .map_or_else(|| "not measured".into(), MemoryObservation::describe)
            ),
            format!("memory available at failure: {}", at_failure.describe()),
            format!("pid pressure: {tree} process(es) in the test tree; {pids}"),
            format!("original exit status: {}", describe_status(returncode)),
            "=== nextest memory: only this test's process tree was affected ===".into(),
            String::new(),
        ];
        write_stderr(&lines.join("\n"));
        if let Some(dir) = &self.config.admission_dir {
            let record = dir.join(INFRA_DIR).join(infra_record_name(&self.identity));
            if let Some(parent) = record.parent() {
                if std::fs::create_dir_all(parent).is_ok() {
                    let _ = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(record);
                }
            }
        }
    }
}

pub fn memory_signature(text: &[u8]) -> Option<String> {
    let contains = |needle: &[u8]| text.windows(needle.len()).any(|window| window == needle);
    for signature in MEMORY_SIGNATURES {
        if contains(signature.as_bytes()) {
            return Some(signature.into());
        }
    }
    (contains(b"memory allocation of ") && contains(b" failed"))
        .then(|| "memory allocation of <n> bytes failed".into())
}

const SIGNAL_NAMES: [&str; 31] = [
    "SIGHUP",
    "SIGINT",
    "SIGQUIT",
    "SIGILL",
    "SIGTRAP",
    "SIGABRT",
    "SIGBUS",
    "SIGFPE",
    "SIGKILL",
    "SIGUSR1",
    "SIGSEGV",
    "SIGUSR2",
    "SIGPIPE",
    "SIGALRM",
    "SIGTERM",
    "SIGSTKFLT",
    "SIGCHLD",
    "SIGCONT",
    "SIGSTOP",
    "SIGTSTP",
    "SIGTTIN",
    "SIGTTOU",
    "SIGURG",
    "SIGXCPU",
    "SIGXFSZ",
    "SIGVTALRM",
    "SIGPROF",
    "SIGWINCH",
    "SIGIO",
    "SIGPWR",
    "SIGSYS",
];

/// Signal numbers POSIX hosts (Linux and macOS alike) share, so their
/// `SIGNAL_NAMES` entry is right off Linux too.
const PORTABLE_SIGNALS: [usize; 12] = [1, 2, 3, 4, 5, 6, 8, 9, 11, 13, 14, 15];

/// `returncode` follows Python's convention: negative is `-signal`.
fn describe_status(returncode: Option<i32>) -> String {
    match returncode {
        None => "not started".into(),
        Some(code) if code < 0 => {
            let signal = code.unsigned_abs() as usize;
            match SIGNAL_NAMES.get(signal.wrapping_sub(1)) {
                Some(name) if is_linux() || PORTABLE_SIGNALS.contains(&signal) => {
                    format!("killed by {name}")
                }
                _ => format!("killed by signal {signal}"),
            }
        }
        Some(code) => code.to_string(),
    }
}

#[cfg(test)]
#[path = "guard_tests.rs"]
mod tests;
