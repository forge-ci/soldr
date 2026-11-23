//! Host-wide inventory of `soldr-broker` / `soldr-daemon` processes
//! (soldr#3193).
//!
//! A broker is one per `HOME`, spawned by the front door of any `soldr`
//! invocation, and it lives until its install directory disappears
//! (soldr#3184) or something stops it. Test suites -- soldr's own
//! integration tests and downstream pytest fixtures -- run `soldr` under a
//! fresh temporary `HOME` per test, so every fixture leaves one broker (and
//! often a daemon) behind. A host audit found 350 of them, none serving
//! anything, most from `HOME`s that still existed on disk so the image watch
//! never fired.
//!
//! This module answers "which soldr processes on this host serve a `HOME`
//! other than mine?" and exposes the answer three ways:
//!
//! * `soldr doctor` prints and serialises the inventory;
//! * a rate-limited stderr toast on interactive front-door invocations, so
//!   a human sees the pile-up without running doctor;
//! * `soldr broker purge` stops the leaked processes (see `broker_cmd.rs`).
//!
//! The process scan is one pass over the process table via `sysinfo`. The
//! classification is pure over [`ProcessRecord`]s so the rules are unit
//! testable, and [`PROCESS_LIST_FILE_ENV`] lets integration tests feed a
//! scripted table instead of the live one (the `doctor` standalone-zccache
//! scan uses the same seam shape).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Test seam: a JSON array of [`ProcessRecord`]s to use instead of the live
/// process table. Never set outside tests.
pub(crate) const PROCESS_LIST_FILE_ENV: &str = "SOLDR_TEST_BROKER_PROCESS_LIST_FILE";

/// `0` disables the leak toast; `always` emits it even when stderr is not a
/// terminal (tests). Unset means "interactive stderr only".
pub(crate) const TOAST_ENV: &str = "SOLDR_BROKER_LEAK_TOAST";

/// Minimum spacing between two toasts for one `HOME`, in seconds. The stamp
/// lives in the broker install directory of the invoking `HOME`.
pub(crate) const TOAST_INTERVAL_ENV: &str = "SOLDR_BROKER_LEAK_TOAST_INTERVAL_SECS";
const DEFAULT_TOAST_INTERVAL: Duration = Duration::from_secs(60 * 60);
const TOAST_STAMP_FILE: &str = "leak-toast.stamp";

/// One row of the process table, reduced to what classification needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcessRecord {
    pub(crate) pid: u32,
    pub(crate) exe: PathBuf,
    #[serde(default)]
    pub(crate) cmd: Vec<String>,
    /// `HOME` (Unix) or `USERPROFILE` (Windows) from the process environment,
    /// when readable. Unreadable for other users' processes; then the
    /// install-directory layout (`<home>/.soldr/broker/soldr-broker`)
    /// supplies it.
    #[serde(default)]
    pub(crate) home: Option<PathBuf>,
    #[serde(default)]
    pub(crate) start_time: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    Broker,
    Daemon,
}

impl Role {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Broker => "broker",
            Self::Daemon => "daemon",
        }
    }

    /// A process is a broker when it runs the staged `soldr-broker` image
    /// or when it is `soldr broker serve` in the foreground (a supported
    /// diagnostic surface, and what soldr's own broker tests spawn).
    fn of_record(record: &ProcessRecord) -> Option<Self> {
        Self::of_process(&record.exe, &record.cmd)
    }

    pub(crate) fn of_process(exe: &Path, cmd: &[String]) -> Option<Self> {
        Self::of_executable(exe).or_else(|| {
            let is_soldr = exe
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("soldr"));
            (is_soldr && is_foreground_broker(cmd)).then_some(Self::Broker)
        })
    }

    fn of_executable(exe: &Path) -> Option<Self> {
        let name = exe.file_name()?.to_str()?;
        // Linux reports an image whose file was unlinked (a fixture HOME
        // deleted under a still-running broker) as `<path> (deleted)`.
        let name = name.strip_suffix(" (deleted)").unwrap_or(name);
        let name = name.strip_suffix(".exe").unwrap_or(name);
        match name {
            "soldr-broker" => Some(Self::Broker),
            "soldr-daemon" => Some(Self::Daemon),
            _ => None,
        }
    }
}

// Inspect the command prefix without building the full Clap command tree in a
// watchdog thread. These are the value-taking options on the top-level Cli.
fn is_foreground_broker(cmd: &[String]) -> bool {
    let mut args = cmd.iter().skip(1);
    let mut words = std::iter::from_fn(|| loop {
        let arg = args.next()?.as_str();
        match arg {
            "--" => return None,
            "--jobs" | "--zccache" | "--zccache-mode" => {
                args.next()?;
            }
            flag if flag.starts_with('-') => {}
            word => return Some(word),
        }
    });
    words.next() == Some("broker") && words.next() == Some("serve")
}

/// A soldr process serving some `HOME` other than the invoking one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LeakedProcess {
    pub(crate) pid: u32,
    pub(crate) role: Role,
    pub(crate) executable: String,
    /// The `HOME` the process serves, when it could be determined.
    pub(crate) home: Option<String>,
    /// Whether that `HOME` still exists on disk. A missing `HOME` is the
    /// clearest possible leak: nothing can ever reach the process again.
    pub(crate) home_present: bool,
    pub(crate) start_time_unix: u64,
}

/// The inventory `doctor` prints and `purge` acts on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Inventory {
    /// The invoking `HOME`, which every rule below is relative to.
    pub(crate) own_home: String,
    /// soldr processes serving the invoking `HOME`. Never leaked, never purged.
    pub(crate) own_processes: usize,
    pub(crate) leaked: Vec<LeakedProcess>,
}

impl Inventory {
    pub(crate) fn leaked_brokers(&self) -> usize {
        self.leaked
            .iter()
            .filter(|p| p.role == Role::Broker)
            .count()
    }

    pub(crate) fn leaked_daemons(&self) -> usize {
        self.leaked
            .iter()
            .filter(|p| p.role == Role::Daemon)
            .count()
    }

    pub(crate) fn leaked_with_missing_home(&self) -> usize {
        self.leaked.iter().filter(|p| !p.home_present).count()
    }
}

/// The invoking process's `HOME`, as the broker identity resolver sees it.
pub(crate) fn own_home() -> Option<PathBuf> {
    crate::platform::host::dirs::home()
}

/// A broker's install directory is `<home>/.soldr/broker/`; walking up from
/// the executable recovers `<home>` without reading the process environment.
fn home_from_install_layout(exe: &Path) -> Option<PathBuf> {
    // `Role::of_executable` already accepted the (possibly ` (deleted)`)
    // file name; only the directories above it matter here.
    let broker_dir = exe.parent()?;
    let soldr_dir = broker_dir.parent()?;
    (broker_dir.file_name()? == "broker" && soldr_dir.file_name()? == ".soldr")
        .then(|| soldr_dir.parent().map(Path::to_path_buf))?
}

fn same_home(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Pure classification: which records are soldr processes, and which of
/// those serve a `HOME` other than `own_home`.
pub(crate) fn classify(records: &[ProcessRecord], own_home: &Path) -> Inventory {
    let mut own_processes = 0;
    let mut leaked = Vec::new();
    for record in records {
        let Some(role) = Role::of_record(record) else {
            continue;
        };
        let home = record
            .home
            .clone()
            .or_else(|| home_from_install_layout(&record.exe));
        if home
            .as_deref()
            .is_some_and(|home| same_home(home, own_home))
        {
            own_processes += 1;
            continue;
        }
        leaked.push(LeakedProcess {
            pid: record.pid,
            role,
            executable: record.exe.display().to_string(),
            home_present: home.as_deref().is_some_and(Path::is_dir),
            home: home.map(|home| home.display().to_string()),
            start_time_unix: record.start_time,
        });
    }
    leaked.sort_by_key(|p| (p.role == Role::Daemon, p.start_time_unix, p.pid));
    Inventory {
        own_home: own_home.display().to_string(),
        own_processes,
        leaked,
    }
}

/// `USERPROFILE` is the Windows spelling and is never set on Unix; `HOME`
/// is set on both (soldr's own fixtures set the pair to the same path), so
/// checking both keys needs no host selection.
fn env_home_of(environ: &[String]) -> Option<PathBuf> {
    ["USERPROFILE=", "HOME="].iter().find_map(|key| {
        environ
            .iter()
            .find_map(|entry| entry.strip_prefix(key))
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    })
}

fn live_process_table() -> Vec<ProcessRecord> {
    use sysinfo::{ProcessRefreshKind, System, UpdateKind};

    // Two phases. A full refresh with exe/cmd/environ for every task on the
    // host (Linux threads included: 20k on a busy build box) took 5 s; the
    // cheap enumeration reads one `stat` per task and the detail refresh
    // runs only for the handful whose image name starts with `soldr`.
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessRefreshKind::new());
    let candidates: Vec<sysinfo::Pid> = system
        .processes()
        .values()
        .filter(|process| process.thread_kind().is_none())
        .filter(|process| process.name().starts_with("soldr"))
        .map(sysinfo::Process::pid)
        .collect();
    let detail = ProcessRefreshKind::new()
        .with_exe(UpdateKind::Always)
        .with_cmd(UpdateKind::Always)
        .with_environ(UpdateKind::Always);
    candidates
        .into_iter()
        .filter_map(|pid| {
            system.refresh_process_specifics(pid, detail);
            let process = system.process(pid)?;
            let record = ProcessRecord {
                pid: pid.as_u32(),
                exe: process.exe()?.to_path_buf(),
                cmd: process.cmd().to_vec(),
                home: env_home_of(process.environ()),
                start_time: process.start_time(),
            };
            Role::of_record(&record).map(|_| record)
        })
        .collect()
}

fn scripted_process_table(path: &Path) -> Vec<ProcessRecord> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub(crate) fn process_table() -> Vec<ProcessRecord> {
    match std::env::var_os(PROCESS_LIST_FILE_ENV) {
        Some(path) => scripted_process_table(Path::new(&path)),
        None => live_process_table(),
    }
}

/// Take the inventory for the invoking `HOME`. `None` when no `HOME` is set,
/// in which case there is no "own" broker to distinguish from the rest.
pub(crate) fn scan() -> Option<Inventory> {
    let own = own_home()?;
    Some(classify(&process_table(), &own))
}

/// Is `pid` still the soldr process the inventory saw? Guards every signal
/// `purge` sends against PID reuse between scan and kill.
pub(crate) fn still_soldr_process(pid: u32, expected_role: Role) -> bool {
    if let Some(path) = std::env::var_os(PROCESS_LIST_FILE_ENV) {
        // The scripted table stands in for the exe check only; liveness is
        // still real, so a purge test can watch its fixture process go away.
        return scripted_process_table(Path::new(&path))
            .iter()
            .any(|r| r.pid == pid && Role::of_record(r) == Some(expected_role))
            && crate::platform::process::inspect::is_alive(pid);
    }
    use sysinfo::{Pid, ProcessRefreshKind, System, UpdateKind};
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_process_specifics(
        pid,
        ProcessRefreshKind::new()
            .with_exe(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );
    system.process(pid).and_then(|process| {
        Role::of_record(&ProcessRecord {
            pid: pid.as_u32(),
            exe: process.exe()?.to_path_buf(),
            cmd: process.cmd().to_vec(),
            home: None,
            start_time: 0,
        })
    }) == Some(expected_role)
}

// ---------------------------------------------------------------------------
// Toast
// ---------------------------------------------------------------------------

/// The remedy the toast and doctor name. Bound to the real verb by a test in
/// `broker_cmd.rs`.
pub(crate) const BROKER_PURGE_COMMAND: &str = "soldr broker purge";

fn toast_interval() -> Duration {
    std::env::var(TOAST_INTERVAL_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .map_or(DEFAULT_TOAST_INTERVAL, Duration::from_secs)
}

/// Whether a toast should even be considered for this invocation: an
/// interactive stderr (or the `always` override), not silenced, and not a
/// machine-parsed output mode.
fn toast_wanted(raw_args: &[String]) -> bool {
    use std::io::IsTerminal;
    match std::env::var(TOAST_ENV).as_deref() {
        Ok("0") => return false,
        Ok("always") => {}
        _ if !std::io::stderr().is_terminal() => return false,
        _ => {}
    }
    crate::broker_spawn::front_door_command_shape(raw_args)
        && crate::broker_spawn::ci_endpoint_diagnostics_eligible(raw_args)
}

/// The stamp that rate-limits the toast for one `HOME`. Touched only when a
/// toast was emitted, so a clean host re-scans no more often than the
/// interval either.
fn toast_stamp_path(own_home: &Path) -> PathBuf {
    own_home
        .join(".soldr")
        .join("broker")
        .join(TOAST_STAMP_FILE)
}

fn stamp_is_fresh(stamp: &Path, interval: Duration) -> bool {
    std::fs::metadata(stamp)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < interval)
}

fn touch_stamp(stamp: &Path) {
    if let Some(parent) = stamp.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(stamp, b"");
}

pub(crate) fn toast_text(inventory: &Inventory) -> String {
    format!(
        "soldr: {brokers} leaked soldr-broker and {daemons} leaked soldr-daemon process(es) \
         are running for other HOMEs ({gone} whose HOME no longer exists). They serve nothing; \
         run `{purge}` to stop them, `soldr doctor` to list them, or set {env}=0 to silence \
         this notice (soldr#3193).",
        brokers = inventory.leaked_brokers(),
        daemons = inventory.leaked_daemons(),
        gone = inventory.leaked_with_missing_home(),
        purge = BROKER_PURGE_COMMAND,
        env = TOAST_ENV,
    )
}

/// Front-door hook: warn once per interval when soldr processes for other
/// `HOME`s are piling up on this host. Runs after the broker spawn so it
/// never delays the path that makes the invocation work, and the stamp check
/// comes before the process scan so a quiet interval costs one `stat`.
pub(crate) fn maybe_toast(raw_args: &[String]) {
    if !toast_wanted(raw_args) {
        return;
    }
    let Some(own) = own_home() else {
        return;
    };
    let stamp = toast_stamp_path(&own);
    if stamp_is_fresh(&stamp, toast_interval()) {
        return;
    }
    let inventory = classify(&process_table(), &own);
    if inventory.leaked.is_empty() {
        return;
    }
    touch_stamp(&stamp);
    eprintln!("{}", toast_text(&inventory));
}

// ---------------------------------------------------------------------------
// Doctor
// ---------------------------------------------------------------------------

/// How many leaked rows `doctor` lists. The counts are always exact; the
/// rows are a sample. A host with hundreds of fixture brokers must not turn
/// the doctor report into a 100 KB document (that also overflows the 64 KB
/// pipe of any caller that waits before reading, which is how this surfaced),
/// and `soldr broker purge --dry-run` lists every row for whoever wants them.
pub(crate) const DOCTOR_LISTED_ROWS: usize = 20;

/// The `doctor` rendering of an [`Inventory`]: exact totals, a bounded
/// sample of rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DoctorInventory {
    pub(crate) own_home: String,
    pub(crate) own_processes: usize,
    pub(crate) leaked_brokers: usize,
    pub(crate) leaked_daemons: usize,
    pub(crate) leaked_with_missing_home: usize,
    /// The first [`DOCTOR_LISTED_ROWS`] leaked processes.
    pub(crate) leaked: Vec<LeakedProcess>,
    /// How many leaked processes are not in `leaked`.
    pub(crate) leaked_omitted: usize,
}

impl DoctorInventory {
    pub(crate) fn from_inventory(inventory: &Inventory) -> Self {
        let listed = inventory.leaked.len().min(DOCTOR_LISTED_ROWS);
        Self {
            own_home: inventory.own_home.clone(),
            own_processes: inventory.own_processes,
            leaked_brokers: inventory.leaked_brokers(),
            leaked_daemons: inventory.leaked_daemons(),
            leaked_with_missing_home: inventory.leaked_with_missing_home(),
            leaked: inventory.leaked[..listed].to_vec(),
            leaked_omitted: inventory.leaked.len() - listed,
        }
    }
}

pub(crate) fn scan_for_doctor() -> Option<DoctorInventory> {
    scan().map(|inventory| DoctorInventory::from_inventory(&inventory))
}

pub(crate) fn print_doctor_human(inventory: Option<&DoctorInventory>) {
    println!();
    println!("soldr processes for other HOMEs:");
    let Some(inventory) = inventory else {
        println!("  (HOME is not set; nothing to compare against)");
        return;
    };
    println!(
        "  own HOME:          {} ({} process(es) serving it)",
        inventory.own_home, inventory.own_processes
    );
    if inventory.leaked_brokers + inventory.leaked_daemons == 0 {
        println!("  leaked:            none detected");
        return;
    }
    println!(
        "  leaked:            {} broker(s), {} daemon(s); {} with a HOME that no longer exists",
        inventory.leaked_brokers, inventory.leaked_daemons, inventory.leaked_with_missing_home
    );
    println!("  remedy:            {BROKER_PURGE_COMMAND} (soldr#3193)");
    for process in &inventory.leaked {
        println!(
            "  {:<7} pid {:<8} HOME {}{}",
            process.role.as_str(),
            process.pid,
            process.home.as_deref().unwrap_or("(unknown)"),
            if process.home_present {
                ""
            } else {
                "  [missing]"
            }
        );
    }
    if inventory.leaked_omitted > 0 {
        println!(
            "  ... and {} more; `{BROKER_PURGE_COMMAND} --dry-run` lists every one",
            inventory.leaked_omitted
        );
    }
}

#[cfg(test)]
#[path = "broker_inventory_tests.rs"]
mod tests;
