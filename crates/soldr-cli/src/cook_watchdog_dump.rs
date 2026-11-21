//! Forensic dump + termination for a stalled `soldr cook` process tree
//! (soldr#3043 follow-up). Companion to `cook_watchdog.rs`; split out so the
//! watchdog's pure timing logic stays easy to read and unit test separately
//! from the process-inspection/termination side, which needs real PIDs.
//!
//! Native thread backtraces (`gdb` / `eu-stack`) are Linux-realistic tools
//! that are simply absent on other platforms -- the code checks `PATH` for
//! them rather than branching on OS, so the behavior degrades automatically
//! instead of needing a cfg.

use crate::core::SoldrPaths;
use crate::platform::process::metadata::{self, ProcessEntry};
use kernal_api::platform::process;
use std::collections::HashSet;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

const BACKTRACE_TOOL_TIMEOUT: Duration = Duration::from_secs(20);

/// One process captured into the dump.
struct ProcessSnapshot {
    pid: u32,
    parent_pid: Option<u32>,
    name: String,
    exe: Option<PathBuf>,
    cmd: Vec<String>,
    status: String,
    run_time_secs: u64,
}

/// Write a best-effort forensic dump of the current process's descendant
/// tree (the cook child: cargo, rustc wrappers, rustc, build scripts) plus
/// any discoverable soldr daemon/broker processes, under
/// `<cache>/logs/cook-stall-<timestamp>/`.
///
/// Every artifact inside is best-effort: a failure writing one file never
/// prevents the rest, because a partial dump remains far more useful than
/// none (mirrors `rss_ceiling::write_breach_dump`'s shape, soldr#3053).
pub(crate) fn write_stall_dump(paths: &SoldrPaths, phase: &str) -> std::io::Result<PathBuf> {
    let logs = paths.cache.join("logs");
    std::fs::create_dir_all(&logs)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(logs.join("cook-stall.lock"))?;
    // One writer across every wrapper process sharing this root. Never queue
    // diagnostic work behind another diagnostic.
    if kernal_api::fs_try_lock_exclusive(&lock).is_err() {
        return Ok(logs.join("cook-stall-suppressed"));
    }
    let table = metadata::snapshot();
    let this_pid = std::process::id();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let descendants: Vec<_> = descendants_of(&table, this_pid)
        .into_iter()
        .map(|entry| process_snapshot(entry, now))
        .collect();
    let services: Vec<_> = table
        .iter()
        .filter(|entry| entry.name.starts_with("soldr"))
        .map(|entry| process_snapshot(entry, now))
        .collect();
    let daemon_and_broker = soldr_service_processes(&services, this_pid);

    // Identify only this selected root's daemon; unrelated roots and transient
    // CLI commands cannot reset the dump budget. Failed status is conservatively
    // grouped by root so a wedged daemon still has bounded diagnostics.
    let generation =
        crate::daemon::client::status(&crate::daemon::client::default_sock_path(paths))
            .map(|s| format!("{}:{}:{}", paths.cache.display(), s.pid, s.generation))
            .unwrap_or_else(|_| format!("{}:unreachable", paths.cache.display()));
    let mut fingerprint: Vec<_> = descendants
        .iter()
        .chain(daemon_and_broker.iter())
        .map(|p| format!("{}:{}:{:?}:{}", p.pid, p.name, p.cmd, p.status))
        .collect();
    fingerprint.sort_unstable();
    let fingerprint = blake3::hash(fingerprint.join("\n").as_bytes())
        .to_hex()
        .to_string();
    if let Some(previous) = reusable_dump(&logs, &generation, &fingerprint)? {
        return Ok(previous);
    }
    let dir = logs.join(crate::cook_watchdog::stall_dump_dirname());
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("generation.txt"), generation)?;
    std::fs::write(dir.join("fingerprint.txt"), fingerprint)?;
    prune_dumps(&logs, 20)?;
    write_process_list(&dir.join("cook-descendants.txt"), &descendants)?;
    write_process_list(
        &dir.join("daemon-broker.txt"),
        daemon_and_broker.iter().copied(),
    )?;

    let backtrace_tool = detect_backtrace_tool();
    let mut backtrace_summary = String::new();
    for process in descendants.iter().chain(daemon_and_broker.iter().copied()) {
        if let Some(tool) = backtrace_tool {
            match capture_native_backtrace(tool, process.pid) {
                Some(text) => {
                    let file = dir.join(format!("backtrace-{}.txt", process.pid));
                    let _ = std::fs::write(&file, text);
                    backtrace_summary.push_str(&format!(
                        "pid={} ({}): backtrace written to {}\n",
                        process.pid,
                        process.name,
                        file.display()
                    ));
                }
                None => {
                    backtrace_summary.push_str(&format!(
                        "pid={} ({}): backtrace unavailable\n",
                        process.pid, process.name
                    ));
                }
            }
        }
    }
    if backtrace_summary.is_empty() {
        backtrace_summary =
            format!("no native backtrace tool (gdb / eu-stack) found on PATH; phase={phase}\n");
    }
    std::fs::write(dir.join("backtraces-summary.txt"), backtrace_summary)?;

    let summary = format!(
        "soldr cook stall dump\nphase={phase}\ncook_descendants={}\ndaemon_broker_processes={}\n",
        descendants.len(),
        daemon_and_broker.len(),
    );
    std::fs::write(dir.join("summary.txt"), summary)?;

    Ok(dir)
}

/// Terminate every descendant of this process (the cook subprocess tree).
/// Best-effort: a process that already exited or refuses signals is simply
/// skipped, because the caller is already returning a hard error either way.
pub(crate) fn terminate_cook_descendants() {
    let table = metadata::snapshot();
    let current = std::process::id();
    // Capture each descendant before signaling anything, while the observed
    // tree still exists. Never signal the watchdog itself.
    let identities: Vec<_> = descendants_of(&table, current)
        .into_iter()
        .filter_map(|entry| match process::capture_identity(entry.pid) {
            process::ProcessIdentityCapture::Found(identity) => Some(identity),
            _ => None,
        })
        .collect();
    for identity in identities.into_iter().rev() {
        if matches!(
            process::signal_terminate(identity),
            Err(process::ProcessIdentityActionError::Unavailable(
                process::ProcessIdentityUnavailable::Unsupported
            ))
        ) {
            let _ = process::force_kill(identity);
        }
    }
}

fn process_snapshot(process: &ProcessEntry, now: u64) -> ProcessSnapshot {
    let pid = process.pid;
    let (cmd, _) = metadata::arguments_and_environment(pid);
    ProcessSnapshot {
        pid,
        parent_pid: process.parent_pid,
        name: process.name.clone(),
        exe: metadata::executable_path(pid),
        cmd,
        status: if process.running {
            "running"
        } else {
            "not running"
        }
        .into(),
        run_time_secs: metadata::start_time_unix(pid).map_or(0, |start| now.saturating_sub(start)),
    }
}

fn descendants_of(table: &[ProcessEntry], root_pid: u32) -> Vec<&ProcessEntry> {
    let mut result = Vec::new();
    let mut frontier = vec![root_pid];
    let mut visited = HashSet::from([root_pid]);
    while let Some(pid) = frontier.pop() {
        for candidate in table {
            if candidate.parent_pid == Some(pid) && visited.insert(candidate.pid) {
                result.push(candidate);
                frontier.push(candidate.pid);
            }
        }
    }
    result
}

/// Discover Soldr services for the forensic dump. Always exclude the
/// watchdog process because attaching would suspend its own timeout.
fn soldr_service_processes(table: &[ProcessSnapshot], current_pid: u32) -> Vec<&ProcessSnapshot> {
    table
        .iter()
        .filter(|process| {
            process.pid != current_pid
                && process.exe.as_deref().is_some_and(|exe| {
                    crate::broker_inventory::Role::of_process(exe, &process.cmd).is_some()
                })
        })
        .collect()
}

fn write_process_list<'a>(
    path: &std::path::Path,
    processes: impl IntoIterator<Item = &'a ProcessSnapshot>,
) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    let mut processes = processes.into_iter().peekable();
    if processes.peek().is_none() {
        writeln!(file, "(none found)")?;
        return Ok(());
    }
    for process in processes {
        writeln!(
            file,
            "pid={} ppid={:?} name={} status={} run_time_secs={} cmd={:?}",
            process.pid,
            process.parent_pid,
            process.name,
            process.status,
            process.run_time_secs,
            process.cmd,
        )?;
    }
    Ok(())
}

/// A native thread-backtrace tool discovered on `PATH`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BacktraceTool {
    Gdb,
    EuStack,
}

fn detect_backtrace_tool() -> Option<BacktraceTool> {
    if which_on_path("gdb") {
        Some(BacktraceTool::Gdb)
    } else if which_on_path("eu-stack") {
        Some(BacktraceTool::EuStack)
    } else {
        None
    }
}

fn which_on_path(program: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| {
        let candidate = dir.join(program);
        candidate.is_file()
    })
}

/// Spawn `gdb -batch -p <pid> -ex "thread apply all bt"` (or `eu-stack -p
/// <pid>`) through `running_process`, bounded by `BACKTRACE_TOOL_TIMEOUT`.
/// Never a raw `std::process::Command` spawn (Dylint `ban_raw_process_creation`).
fn capture_native_backtrace(tool: BacktraceTool, pid: u32) -> Option<String> {
    use running_process::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};

    let argv = match tool {
        BacktraceTool::Gdb => vec![
            "gdb".to_string(),
            "-batch".to_string(),
            "-p".to_string(),
            pid.to_string(),
            "-ex".to_string(),
            "thread apply all bt".to_string(),
        ],
        BacktraceTool::EuStack => vec!["eu-stack".to_string(), "-p".to_string(), pid.to_string()],
    };

    let config = ProcessConfig {
        command: CommandSpec::Argv(argv),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Pipe,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    };
    let process = NativeProcess::new(config);
    process.start().ok()?;
    let _ = process.wait(Some(BACKTRACE_TOOL_TIMEOUT));
    let mut out = String::new();
    for chunk in process.captured_stdout() {
        out.push_str(&String::from_utf8_lossy(&chunk));
    }
    // Stderr is preserved in the dump alongside stdout rather than
    // discarded: a failing `gdb`/`eu-stack` invocation explains itself on
    // stderr (e.g. "ptrace: Operation not permitted"), which is exactly the
    // detail a forensic dump needs.
    for chunk in process.captured_stderr() {
        out.push_str("--- stderr ---\n");
        out.push_str(&String::from_utf8_lossy(&chunk));
    }
    let _ = process.close();
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_backtraces_exclude_the_watchdog_process() {
        let snapshot = |pid, name: &str, arg: &str| ProcessSnapshot {
            pid,
            parent_pid: None,
            name: name.into(),
            exe: Some(PathBuf::from(name)),
            cmd: vec![arg.into()],
            status: String::new(),
            run_time_secs: 0,
        };
        let table = [
            snapshot(10, "soldr-daemon", "/bin/soldr-daemon"),
            snapshot(11, "soldr-daemon", "/bin/soldr-daemon"),
            snapshot(12, "soldr-dylint", "--crate-name=zccache_daemon"),
            snapshot(13, "soldr", "--target-dir=daemon-build"),
        ];

        let services = soldr_service_processes(&table, 10);
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].pid, 11);
    }
}

fn dump_dirs(logs: &std::path::Path) -> std::io::Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(logs)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && entry
                .file_name()
                .to_string_lossy()
                .starts_with("cook-stall-")
        {
            dirs.push(entry.path());
        }
    }
    dirs.sort_by_key(|p| std::cmp::Reverse(std::fs::metadata(p).and_then(|m| m.modified()).ok()));
    Ok(dirs)
}
fn reusable_dump(
    logs: &std::path::Path,
    generation: &str,
    fingerprint: &str,
) -> std::io::Result<Option<PathBuf>> {
    for dir in dump_dirs(logs)? {
        let same_generation = std::fs::read_to_string(dir.join("generation.txt"))
            .ok()
            .as_deref()
            == Some(generation);
        let same_dump = std::fs::read_to_string(dir.join("fingerprint.txt"))
            .ok()
            .as_deref()
            == Some(fingerprint);
        let recent = std::fs::metadata(&dir)?
            .modified()?
            .elapsed()
            .unwrap_or_default()
            < Duration::from_secs(600);
        if same_generation && (recent || same_dump) {
            return Ok(Some(dir));
        }
    }
    Ok(None)
}
fn prune_dumps(logs: &std::path::Path, keep: usize) -> std::io::Result<()> {
    for dir in dump_dirs(logs)?.into_iter().skip(keep) {
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}
#[cfg(test)]
mod retention_tests {
    use super::*;
    #[test]
    fn dump_rate_limit_dedup_and_pruning() {
        let tmp = tempfile::tempdir().unwrap();
        for n in 0..25 {
            let dir = tmp.path().join(format!("cook-stall-{n:03}"));
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("generation.txt"), "generation").unwrap();
            std::fs::write(dir.join("fingerprint.txt"), "same").unwrap();
            let age = std::time::SystemTime::now() - Duration::from_secs(900 - n);
            filetime::set_file_mtime(&dir, filetime::FileTime::from_system_time(age)).unwrap();
        }
        assert!(reusable_dump(tmp.path(), "generation", "changed")
            .unwrap()
            .is_none());
        assert!(reusable_dump(tmp.path(), "generation", "same")
            .unwrap()
            .is_some());
        let newest = tmp.path().join("cook-stall-024");
        filetime::set_file_mtime(&newest, filetime::FileTime::now()).unwrap();
        assert!(reusable_dump(tmp.path(), "generation", "changed")
            .unwrap()
            .is_some());
        assert!(reusable_dump(tmp.path(), "new-generation", "same")
            .unwrap()
            .is_none());
        prune_dumps(tmp.path(), 20).unwrap();
        assert_eq!(dump_dirs(tmp.path()).unwrap().len(), 20);
        for n in 0..5 {
            assert!(!tmp.path().join(format!("cook-stall-{n:03}")).exists());
        }
        for n in 5..25 {
            assert!(tmp.path().join(format!("cook-stall-{n:03}")).exists());
        }
    }
}
