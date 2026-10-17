//! Thread dump on Nextest's SIGTERM: gdb when available, `/proc` otherwise.

use crate::memory::is_linux;
use crate::write_stderr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

const DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(12);

fn which(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

fn proc_thread_dump(pid: u32) {
    let task_root = PathBuf::from(format!("/proc/{pid}/task"));
    let Ok(entries) = std::fs::read_dir(&task_root) else {
        write_stderr(&format!(
            "nextest timeout: /proc thread state unavailable for pid {pid}\n"
        ));
        return;
    };
    let mut tasks: Vec<(u64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| Some((entry.file_name().to_str()?.parse().ok()?, entry.path())))
        .collect();
    tasks.sort();
    for (tid, task) in tasks {
        write_stderr(&format!("\n--- thread {tid} ---\n"));
        for name in ["comm", "wchan", "stack"] {
            let value = match std::fs::read(task.join(name)) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(error) => format!("<unavailable: {error}>\n"),
            };
            write_stderr(&format!("{name}:\n{value}"));
        }
        let Ok(status) = std::fs::read(Path::new(&task).join("status")) else {
            continue;
        };
        let selected: Vec<&str> = std::str::from_utf8(&status)
            .unwrap_or("")
            .lines()
            .filter(|line| {
                ["Name:", "State:", "Tgid:", "Pid:", "PPid:"]
                    .iter()
                    .any(|prefix| line.starts_with(prefix))
            })
            .collect();
        write_stderr(&format!("status:\n{}\n", selected.join("\n")));
    }
}

fn gdb_dump(debugger: &Path, pid: u32) -> Result<(), String> {
    let mut child = Command::new(debugger)
        .args([
            "--quiet",
            "--batch",
            "--nx",
            "-ex",
            "set pagination off",
            "-ex",
            "thread apply all backtrace full",
            "-p",
            &pid.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::io::stderr()))
        .stderr(Stdio::from(std::io::stderr()))
        .spawn()
        .map_err(|error| {
            format!("nextest timeout: gdb failed ({error}); using /proc fallback\n")
        })?;
    match child.wait_timeout(DIAGNOSTIC_TIMEOUT) {
        Ok(Some(status)) if status.success() => Ok(()),
        Ok(Some(status)) => Err(format!(
            "nextest timeout: gdb exited {}; using /proc fallback\n",
            status.code().unwrap_or(-1)
        )),
        Ok(None) => {
            let _ = child.kill();
            let _ = soldr_platform::wait::wait_for(&mut child, soldr_platform::wait::PROBE);
            Err(format!(
                "nextest timeout: gdb failed (timed out after {} seconds); using /proc fallback\n",
                DIAGNOSTIC_TIMEOUT.as_secs()
            ))
        }
        Err(error) => Err(format!(
            "nextest timeout: gdb failed ({error}); using /proc fallback\n"
        )),
    }
}

/// Dump userspace stacks when possible, then fall back to thread state.
pub fn dump_threads(pid: u32) {
    write_stderr(&format!(
        "\n=== nextest timeout: thread dump for pid {pid} ===\n"
    ));
    let debugger = std::env::var_os("SOLDR_NEXTEST_DISABLE_DEBUGGER")
        .filter(|value| !value.is_empty())
        .map_or_else(|| which("gdb"), |_| None);
    if let Some(debugger) = debugger.filter(|_| is_linux()) {
        match gdb_dump(&debugger, pid) {
            Ok(()) => {
                write_stderr("=== nextest timeout: debugger thread dump complete ===\n");
                return;
            }
            Err(message) => write_stderr(&message),
        }
    }
    if is_linux() {
        proc_thread_dump(pid);
    } else {
        write_stderr("nextest timeout: no platform thread dumper is available\n");
    }
    write_stderr("=== nextest timeout: thread dump complete ===\n");
}
