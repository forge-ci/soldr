//! Per-process CPU and resident-memory readings, including a whole tree.
//!
//! Children are this test binary re-executed with `--exact
//! process_usage::child_probe`, so the suite needs no shell or interpreter.

use kernal_api::platform::process::{
    cpu_ticks_for_pid, peak_rss_bytes_for_pid, tree_rss_bytes_for_pid, MAX_TREE_RSS_PROCESSES,
    PEAK_RSS_READABLE_AFTER_EXIT,
};
use std::io::{Read as _, Write as _};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PROBE: &str = "KERNAL_PROCESS_USAGE_PROBE";
const HEAVY_BYTES: usize = 96 * 1024 * 1024;

// The walk bound is generous enough for a real compiler tree.
const _: () = assert!(MAX_TREE_RSS_PROCESSES >= 1024);

/// `heavy` touches [`HEAVY_BYTES`] and waits for stdin EOF; `root` starts a
/// `heavy` grandchild, says `ready`, and waits for stdin EOF itself.
#[test]
fn child_probe() {
    let Ok(mode) = std::env::var(PROBE) else {
        return;
    };
    match mode.as_str() {
        "heavy" => {
            let mut block = vec![0_u8; HEAVY_BYTES];
            for page in block.chunks_mut(4096) {
                page[0] = 1;
            }
            std::hint::black_box(&block);
            write_raw(b"ready\n");
            let _ = std::io::stdin().read_to_end(&mut Vec::new());
        }
        "root" => {
            let mut heavy = probe("heavy");
            wait_ready(&mut heavy);
            write_raw(b"ready\n");
            let _ = std::io::stdin().read_to_end(&mut Vec::new());
            drop(heavy.stdin.take());
            let _ = heavy.wait();
        }
        other => panic!("unknown probe mode {other}"),
    }
    std::process::exit(0);
}

fn write_raw(bytes: &[u8]) {
    let mut stdout = std::io::stdout();
    stdout.write_all(bytes).expect("probe stdout");
    stdout.flush().expect("flush probe stdout");
}

fn probe(mode: &str) -> Child {
    Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "process_usage::child_probe", "--nocapture"])
        .env(PROBE, mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn probe")
}

/// Read until the probe's `ready` line, skipping the harness banner.
fn wait_ready(child: &mut Child) {
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
}

#[test]
fn this_process_has_cpu_time_and_a_positive_peak() {
    let pid = std::process::id();
    let before = cpu_ticks_for_pid(pid).expect("own CPU ticks");
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut spin = 0_u64;
    while Instant::now() < deadline {
        spin = std::hint::black_box(spin.wrapping_add(1));
    }
    let after = cpu_ticks_for_pid(pid).expect("own CPU ticks");
    assert!(
        after >= before,
        "CPU ticks are monotonic: {before} -> {after}"
    );
    assert!(peak_rss_bytes_for_pid(pid).expect("own peak") > 0);
    let tree = tree_rss_bytes_for_pid(pid).expect("own tree");
    assert!(tree > 0);
}

#[test]
fn an_absent_pid_reads_as_none() {
    let absent = i32::MAX as u32;
    assert_eq!(cpu_ticks_for_pid(absent), None);
    assert_eq!(peak_rss_bytes_for_pid(absent), None);
    assert_eq!(tree_rss_bytes_for_pid(absent), None);
}

/// A memory-heavy grandchild is invisible to the root's own peak but counted
/// by the tree reading -- the linker-under-`cc`-under-`rustc` case.
#[test]
fn tree_reading_counts_a_memory_heavy_grandchild() {
    let mut root = probe("root");
    wait_ready(&mut root);
    let pid = root.id();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut tree = 0;
    while Instant::now() < deadline {
        tree = tree_rss_bytes_for_pid(pid).unwrap_or(0);
        if tree >= (HEAVY_BYTES as u64) / 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let own_peak = peak_rss_bytes_for_pid(pid).expect("root peak");
    drop(root.stdin.take());
    let _ = root.wait();
    assert!(
        tree >= (HEAVY_BYTES as u64) / 2,
        "tree reading includes the grandchild, got {tree}"
    );
    assert!(
        own_peak < (HEAVY_BYTES as u64) / 2,
        "the root itself stayed small, got {own_peak}"
    );
}

/// The after-exit contract: Windows still answers through the retained child
/// handle; Unix hosts do not claim to.
#[test]
fn peak_after_exit_follows_the_declared_contract() {
    let mut child = probe("heavy");
    wait_ready(&mut child);
    let pid = child.id();
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().expect("try_wait").is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let after = peak_rss_bytes_for_pid(pid);
    if PEAK_RSS_READABLE_AFTER_EXIT {
        assert!(
            after.expect("peak readable after exit") >= (HEAVY_BYTES as u64) / 2,
            "final peak retained"
        );
    }
    drop(child);
}
