//! soldr#3197: a test must never spawn a child with a piped stdout/stderr it
//! does not drain.
//!
//! A pipe holds ~64 KB. A child that writes past that blocks in `write(2)`
//! until somebody reads, so the fixture shape
//!
//! ```text
//! let mut child = cmd.stdout(Stdio::piped()).spawn()?;
//! try_wait_for(&mut child, budget)?;             // nobody is reading yet
//! let output = child.wait_with_output()?;  // too late
//! ```
//!
//! deadlocks once the output grows, and reports the deadlock as a timeout.
//! `run_soldr_with_timeout` did exactly that when `soldr doctor --json` briefly
//! carried a ~100 KB process inventory. Nothing about the shape looks wrong
//! until the output crosses a size no fixture author thinks about, and ten more
//! fixtures had it -- mostly long-lived brokers and daemons whose piped stderr
//! nobody ever read.
//!
//! The rule, per function as rustfmt lays functions out: a function that
//! spawns a child and pipes its stdout or stderr must drain that stream itself
//! -- through `common::tracked_child::spawn_tracked`, by taking the pipe
//! (`.stdout.take()` / `.stderr.take()`), or with `wait_with_output()` and no
//! wait on the child before it. `Command::output()` never spawns a handle and
//! always drains, so it is not affected.
//!
//! It is a source heuristic, not a proof: a pipe taken and then abandoned still
//! passes. The two behavioural tests at the bottom pin the failure itself --
//! one proves the old shape really deadlocks on a large writer, the other that
//! `spawn_tracked` collects the same output.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::common;

/// Test sources the lint skips, relative to `crates/`, with the reason.
const EXEMPT: &[(&str, &str)] = &[
    (
        "soldr-cli/tests/common/tracked_child.rs",
        "the draining implementation; its docs quote the trap",
    ),
    (
        "soldr-cli/tests/guards/piped_child_drain_lint.rs",
        "this lint; its fixtures and negative control spell the trap on purpose",
    ),
];

/// Calls that wait on a child. Any of them before `wait_with_output` means the
/// child ran with nobody reading its pipes.
const WAITS: &[&str] = &[
    ".try_wait()",
    ".wait_timeout(",
    ".wait()",
    "try_wait_for(",
    "wait::wait_for(",
];

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn fn_name(trimmed: &str) -> Option<String> {
    let mut rest = trimmed;
    for prefix in ["pub(crate) ", "pub(super) ", "pub ", "async ", "unsafe "] {
        rest = rest.strip_prefix(prefix).unwrap_or(rest);
    }
    let name: String = rest
        .strip_prefix("fn ")?
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// `(line, name, body)` for every `fn`, the body running to the closing brace
/// at the `fn`'s own indentation. Comment lines are dropped and whitespace
/// removed, so a builder chain rustfmt wrapped across lines matches as one.
fn functions(source: &str) -> Vec<(usize, String, String)> {
    let lines: Vec<&str> = source.lines().collect();
    let mut out = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let Some(name) = fn_name(trimmed) else {
            continue;
        };
        let closing = format!("{}}}", &line[..line.len() - trimmed.len()]);
        let end = if trimmed.contains('{') && trimmed.ends_with('}') {
            index
        } else {
            (index + 1..lines.len())
                .find(|&i| lines[i] == closing)
                .unwrap_or(lines.len() - 1)
        };
        let body: String = lines[index..=end]
            .iter()
            .filter(|line| !line.trim_start().starts_with("//"))
            .flat_map(|line| line.chars().filter(|c| !c.is_whitespace()))
            .collect();
        out.push((index + 1, name, body));
    }
    out
}

fn violations_in(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (line, name, body) in functions(source) {
        let spawns = body.contains(".spawn()") || body.contains("spawn_staged(");
        if !spawns || body.contains("spawn_tracked(") {
            continue;
        }
        let waits_first = WAITS.iter().any(|wait| body.contains(wait));
        for stream in ["stdout", "stderr"] {
            let piped = body.contains(&format!(".{stream}(Stdio::piped())"))
                || body.contains(&format!(".{stream}(std::process::Stdio::piped())"));
            let drained = body.contains(&format!(".{stream}.take()"))
                || (body.contains(".wait_with_output()") && !waits_first);
            if piped && !drained {
                let wait_note = if waits_first {
                    " and waits on the child before reading"
                } else {
                    ""
                };
                found.push(format!(
                    "{line}: fn {name} pipes {stream} without draining it{wait_note}"
                ));
            }
        }
    }
    found
}

#[test]
fn tests_never_spawn_a_child_with_an_undrained_pipe() {
    let crates = common::crate_root()
        .parent()
        .expect("soldr-cli crate root lies under workspace crates/")
        .to_path_buf();
    for (path, _) in EXEMPT {
        assert!(
            crates.join(path).is_file(),
            "stale exemption: crates/{path} no longer exists"
        );
    }
    let mut files = Vec::new();
    for crate_dir in fs::read_dir(&crates)
        .expect("read workspace crates directory")
        .flatten()
    {
        collect_rs_files(&crate_dir.path().join("tests"), &mut files);
    }
    assert!(
        files.len() > 50,
        "lint found only {} test sources",
        files.len()
    );

    let mut offenders = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(&crates)
            .expect("test source lies under crates/")
            .to_string_lossy()
            .replace('\\', "/");
        if EXEMPT.iter().any(|(path, _)| *path == relative) {
            continue;
        }
        let Ok(source) = fs::read_to_string(&file) else {
            continue;
        };
        offenders.extend(
            violations_in(&source)
                .into_iter()
                .map(|violation| format!("crates/{relative}:{violation}")),
        );
    }
    assert!(
        offenders.is_empty(),
        "soldr#3197: these fixtures pipe a child's output without draining it. A child \
         that writes past the ~64 KB pipe buffer blocks until it is read, so waiting \
         before reading deadlocks and reports a timeout. Spawn through \
         `common::tracked_child::spawn_tracked` instead:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_rule_flags_undrained_pipes_and_accepts_every_draining_shape() {
    let cases: &[(&str, usize, &str)] = &[
        (
            r#"fn trap() {
    let mut child = Command::new("x")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    try_wait_for(&mut child, budget).unwrap();
    child.wait_with_output().unwrap()
}
"#,
            2,
            "try_wait_for before wait_with_output",
        ),
        (
            r#"fn never_read() -> Child {
    common::isolated_soldr_command()
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn")
}
"#,
            1,
            "a piped stream returned to a caller nobody checks",
        ),
        (
            r#"fn tracked() {
    cmd.stdout(Stdio::piped());
    let child = common::tracked_child::spawn_tracked(&mut cmd).unwrap();
}
"#,
            0,
            "spawn_tracked drains",
        ),
        (
            r#"fn collected() {
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"x").unwrap();
    child.wait_with_output().unwrap()
}
"#,
            0,
            "wait_with_output with no wait before it drains while waiting",
        ),
        (
            r#"fn taken() {
    let mut child = cmd.stdout(Stdio::piped()).spawn().unwrap();
    let reader = child.stdout.take().unwrap();
}
"#,
            0,
            "a taken pipe is the caller's to drain",
        ),
        (
            r#"fn output_only() {
    let out = cmd.stdout(Stdio::piped()).output().unwrap();
}
"#,
            0,
            "output() is not a spawned handle",
        ),
    ];
    for (source, expected, why) in cases {
        let found = violations_in(source);
        assert_eq!(found.len(), *expected, "{why}: {found:?}");
    }
}

/// Past any pipe buffer a host could plausibly configure: Linux defaults to
/// 64 KB and caps an unprivileged pipe at 1 MiB.
const WRITER_LINES: usize = 16_000;
const WRITER_LINE: &str =
    "soldr-3197 pipe buffer overflow fixture line padded out to eighty bytes of text";

fn large_writer(dir: &Path) -> PathBuf {
    let script = common::fake_script_path(dir, "large-writer");
    let body = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!("@echo off\nfor /L %%i in (1,1,{WRITER_LINES}) do echo {WRITER_LINE}\n")
    } else {
        format!(
            "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt {WRITER_LINES} ]; do\n  echo '{WRITER_LINE}'\n  i=$((i + 1))\ndone\n"
        )
    };
    common::write_fake_script(&script, &body);
    script
}

/// The negative control that makes the next test meaningful: the same writer,
/// spawned the old way, cannot finish while nobody reads. Without it, a writer
/// that happened to fit in the buffer would pass both tests.
#[test]
fn waiting_before_reading_deadlocks_a_large_writer() {
    let dir = common::unique_temp_dir("pipe-drain-trap");
    let mut command = Command::new(large_writer(&dir));
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = common::spawn_staged(&mut command).expect("spawn large writer");
    let started = Instant::now();
    let settled = soldr_platform::wait::try_wait_for(&mut child, Duration::from_secs(3))
        .expect("wait on large writer");
    // Closing the read end ends the writer with EPIPE/SIGPIPE; nothing is left.
    drop(child.stdout.take());
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        settled.is_none(),
        "the writer exited after {:?} with nobody reading its stdout, so it no longer \
         overflows the pipe buffer and the drain test below proves nothing",
        started.elapsed()
    );
}

#[test]
fn spawn_tracked_collects_output_far_past_the_pipe_buffer() {
    let dir = common::unique_temp_dir("pipe-drain-tracked");
    let mut command = Command::new(large_writer(&dir));
    let output = common::tracked_child::spawn_tracked(&mut command)
        .expect("spawn large writer")
        .wait_bounded(Duration::from_secs(60));
    assert!(
        !output.timed_out && output.pipes_closed,
        "a draining child must finish a large write: {}",
        output.disposition()
    );
    assert!(
        output.stdout.len() >= WRITER_LINES * WRITER_LINE.len(),
        "collected {} bytes, expected at least {}",
        output.stdout.len(),
        WRITER_LINES * WRITER_LINE.len()
    );
    assert!(output.into_output().status.success());
}
