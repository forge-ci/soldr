//! Forensic dump + termination for a stalled `soldr cook` process tree
//! (soldr#3043 follow-up). Companion to `cook_watchdog.rs`; split out so the
//! watchdog's pure timing logic stays easy to read and unit test separately
//! from the process-inspection/termination side, which needs real PIDs.
//!
//! Process discovery uses `sysinfo` (already a `soldr-cli` dependency, see
//! `broker_inventory.rs` for the established pattern) rather than hand-rolled
//! `/proc` parsing, so the same code paths work on Linux, macOS and Windows
//! without any `#[cfg(target_os = ...)]` outside the platform boundary.
//! Native thread backtraces (`gdb` / `eu-stack`) are Linux-realistic tools
//! that are simply absent on other platforms -- the code checks `PATH` for
//! them rather than branching on OS, so the behavior degrades automatically
//! instead of needing a cfg.

use crate::core::SoldrPaths;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

const BACKTRACE_TOOL_TIMEOUT: Duration = Duration::from_secs(20);

/// One process captured into the dump.
struct ProcessSnapshot {
    pid: u32,
    parent_pid: Option<u32>,
    name: String,
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
    let dir = paths
        .cache
        .join("logs")
        .join(crate::cook_watchdog::stall_dump_dirname());
    std::fs::create_dir_all(&dir)?;

    let table = live_process_snapshot();
    let this_pid = std::process::id();
    let descendants = descendants_of(&table, this_pid);
    let daemon_and_broker = soldr_service_processes(&table, this_pid);

    write_process_list(&dir.join("cook-descendants.txt"), &descendants)?;
    write_process_list(&dir.join("daemon-broker.txt"), &daemon_and_broker)?;

    let backtrace_tool = detect_backtrace_tool();
    let mut backtrace_summary = String::new();
    for process in descendants.iter().chain(daemon_and_broker.iter()) {
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
    let table = live_process_snapshot();
    let descendants = descendants_of(&table, std::process::id());
    for process in &descendants {
        terminate_pid_best_effort(process.pid);
    }
}

fn live_process_snapshot() -> Vec<ProcessSnapshot> {
    use sysinfo::{ProcessRefreshKind, System, UpdateKind};

    let mut system = System::new();
    let detail = ProcessRefreshKind::new()
        .with_cmd(UpdateKind::Always)
        .with_exe(UpdateKind::Always);
    system.refresh_processes_specifics(detail);
    system
        .processes()
        .values()
        .filter(|process| process.thread_kind().is_none())
        .map(|process| ProcessSnapshot {
            pid: process.pid().as_u32(),
            parent_pid: process.parent().map(sysinfo::Pid::as_u32),
            name: process.name().to_string(),
            cmd: process.cmd().to_vec(),
            status: format!("{:?}", process.status()),
            run_time_secs: process.run_time(),
        })
        .collect()
}

fn descendants_of(table: &[ProcessSnapshot], root_pid: u32) -> Vec<&ProcessSnapshot> {
    let mut result = Vec::new();
    let mut frontier = vec![root_pid];
    while let Some(pid) = frontier.pop() {
        for candidate in table {
            if candidate.parent_pid == Some(pid) && candidate.pid != root_pid {
                result.push(candidate);
                frontier.push(candidate.pid);
            }
        }
    }
    result
}

/// Best-effort discovery of soldr daemon/broker processes on the host, for
/// forensic context alongside the cook child tree. Mirrors the
/// `name().starts_with("soldr")` filter already used by
/// `broker_inventory::live_process_table`.
fn soldr_service_processes(table: &[ProcessSnapshot], current_pid: u32) -> Vec<&ProcessSnapshot> {
    table
        .iter()
        .filter(|process| {
            // A debugger attaching to this watchdog's own process would
            // suspend the thread that drains the debugger's output and
            // enforces its timeout.
            process.pid != current_pid
                && process.name.starts_with("soldr")
                && process
                    .cmd
                    .iter()
                    .any(|arg| arg.contains("daemon") || arg.contains("broker"))
        })
        .collect()
}

fn write_process_list(
    path: &std::path::Path,
    processes: &[&ProcessSnapshot],
) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    if processes.is_empty() {
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

fn terminate_pid_best_effort(pid: u32) {
    use sysinfo::{Pid, ProcessRefreshKind, Signal, System};

    let mut system = System::new();
    system.refresh_processes_specifics(ProcessRefreshKind::new());
    if let Some(process) = system.process(Pid::from_u32(pid)) {
        let _ = process.kill_with(Signal::Term);
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
            cmd: vec![arg.into()],
            status: String::new(),
            run_time_secs: 0,
        };
        let table = [
            snapshot(10, "soldr-dylint", "--crate-name=zccache_daemon"),
            snapshot(11, "soldr-daemon", "/bin/soldr-daemon"),
            snapshot(12, "rustc", "--crate-name=zccache_daemon"),
        ];

        let services = soldr_service_processes(&table, 10);
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].pid, 11);
    }
}
