//! soldr#2388 Step 3 — cold-start acceptance: the broker is unconditional, so a
//! front-door `soldr` invocation on a clean isolated root spawns **exactly one**
//! broker, and a second front-door invocation against the same endpoint does NOT
//! spawn a second (the front door is the sole broker-spawner and the broker
//! singleton-binds). This covers the broker half of the "one build → one
//! broker + one daemon" invariant #2364 calls for, exercised on the real
//! process harness (no Docker required).

use crate::common;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Read the broker-spawn log under the isolated home installation.
fn broker_spawn_log(home: &Path) -> String {
    std::fs::read_to_string(home.join(".soldr/broker/broker-spawn.log")).unwrap_or_default()
}

fn isolate_home(command: &mut Command, home: &Path) {
    command.env("HOME", home).env("USERPROFILE", home);
}

fn count_substr(text: &str, needle: &str) -> usize {
    text.lines().filter(|l| l.contains(needle)).count()
}

/// Kill the daemon this test launched for `root`, so nothing leaks past the
/// test. The broker is reaped separately through its stable control endpoint.
fn stop_daemons_in_root(root: &Path) {
    // soldr#3823: under the daemon's generation key (soldr#3374).
    let pid = crate::common::route_claim::route_claim_pid(root);
    if let Some(pid) = pid {
        soldr_platform::process::terminate::terminate_pid(pid);
    }
}

/// How long to wait for the first invocation's bind line before giving up.
///
/// Named so the failure can say what window was missed. "No bind line" and
/// "no bind line *within 30s*" send a reader to different places.
const FIRST_BIND_POLL: Duration = Duration::from_secs(30);

/// Render a drained stream, distinguishing "empty" from "not captured".
fn rendered(lines: &Arc<Mutex<Vec<String>>>) -> String {
    let guard = lines.lock().expect("drained stream mutex");
    if guard.is_empty() {
        "(nothing)".to_string()
    } else {
        guard.join("\n")
    }
}

/// The first-bind failure, written so it says *which* story happened.
///
/// soldr#2624: this assertion used to read "no stable bind line in the spawn
/// log" and then print a spawn log containing one. That is not the product
/// contradicting itself -- the assertion is on a snapshot taken before the
/// second invocation, and the log printed beside it was re-read after. So the
/// message rendered a timing fact as an absence, directly above its own
/// counter-evidence, and sent the reader looking for a broker that failed to
/// start.
///
/// Two stories fit `binds_after_first == 0`, and they need different fixes:
///
/// 1. the broker never came up at all, or
/// 2. it came up during the **second** invocation -- so the first front-door
///    call returned without spawning it, which is the invariant this test
///    exists to protect.
///
/// `total_binds` separates them, so it is stated outright rather than left for
/// the reader to infer from two pasted logs. The first invocation's own output
/// and exit status are included for the same reason: they were being drained
/// and discarded, and they are what says *why* it returned early.
fn first_bind_failure(
    first_status: &str,
    first_out: &str,
    first_err: &str,
    log_after_first: &str,
    total_binds: usize,
    final_log: &str,
) -> String {
    let poll_secs = FIRST_BIND_POLL.as_secs();
    let verdict = if total_binds == 0 {
        "the broker never bound at all".to_string()
    } else {
        format!(
            "{total_binds} bind line(s) exist by the end of the test, so the broker \
             came up during the SECOND invocation -- the first front door returned \
             without spawning it"
        )
    };
    format!(
        concat!(
            "the first front-door invocation on a clean root must bring up a broker\n",
            "  no `stable endpoint bound at` line appeared within {poll_secs}s of it returning\n",
            "  verdict: {verdict}\n",
            "  first invocation exit: {first_status}\n",
            "  first invocation stdout:\n{first_out}\n",
            "  first invocation stderr:\n{first_err}\n",
            "--- spawn log as it stood when the poll gave up (what was asserted on) ---\n",
            "{log_after_first}\n",
            "--- spawn log at the end of the test (after the second invocation) ---\n",
            "{final_log}",
        ),
        poll_secs = poll_secs,
        verdict = verdict,
        first_status = first_status,
        first_out = first_out,
        first_err = first_err,
        log_after_first = log_after_first,
        final_log = final_log,
    )
}

/// Drain a child stream into a shared buffer so its pipe never blocks.
fn drain<R: std::io::Read + Send + 'static>(reader: R) -> Arc<Mutex<Vec<String>>> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            sink.lock().unwrap().push(line);
        }
    });
    lines
}

/// soldr#2624: the message must name which story happened, not just fail.
///
/// This is a pure-string test beside a process test on purpose. The process
/// test takes tens of seconds and only fails on a contended Windows runner --
/// exactly the conditions under which nobody can iterate on the wording. The
/// wording is the part that has been wrong, so it is checked where it is cheap.
#[test]
fn the_first_bind_failure_says_the_broker_never_bound() {
    let message = first_bind_failure("exit code: 0", "(nothing)", "(nothing)", "", 0, "");
    assert!(
        message.contains("the broker never bound at all"),
        "{message}"
    );
    assert!(
        !message.contains("SECOND invocation"),
        "must not blame the second invocation when nothing ever bound\n{message}"
    );
}

#[test]
fn the_first_bind_failure_blames_the_second_invocation_when_a_bind_exists() {
    let final_log = "soldr broker: stable endpoint bound at \\\\.\\pipe\\x";
    let message = first_bind_failure("exit code: 0", "(nothing)", "(nothing)", "", 1, final_log);
    assert!(
        message.contains("came up during the SECOND invocation"),
        "a bind that exists only at the end is the invariant this test guards\n{message}"
    );
    assert!(!message.contains("never bound at all"), "{message}");
}

#[test]
fn the_first_bind_failure_names_the_window_and_both_snapshots() {
    let message = first_bind_failure(
        "exit code: 1",
        "out-marker",
        "err-marker",
        "early-snapshot",
        1,
        "late-snapshot",
    );
    // The window, so "no bind line" reads as the timing statement it is.
    assert!(message.contains("within 30s"), "{message}");
    // Both snapshots, labelled -- printing only the late one is the bug.
    assert!(message.contains("early-snapshot"), "{message}");
    assert!(message.contains("late-snapshot"), "{message}");
    assert!(message.contains("what was asserted on"), "{message}");
    // The first invocation's own account of itself, previously discarded.
    assert!(message.contains("exit code: 1"), "{message}");
    assert!(message.contains("out-marker"), "{message}");
    assert!(message.contains("err-marker"), "{message}");
}

#[test]
fn front_door_cold_start_spawns_exactly_one_broker() {
    let root = common::unique_temp_dir("coldstart-root");
    let home = common::unique_temp_dir("coldstart-home");
    // First front-door invocation on a clean root: it must spawn the broker.
    let mut first = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut first);
    isolate_home(&mut first, &home);
    let mut first_child = first
        .arg("status")
        .env("SOLDR_CACHE_DIR", &root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn first front-door soldr status");
    let o1 = drain(first_child.stdout.take().expect("stdout"));
    let e1 = drain(first_child.stderr.take().expect("stderr"));
    let first_status = first_child.wait().expect("wait first status");

    // The front door waits for the broker to report a bind (or an
    // already-bound refusal) before returning; give it a moment to flush.
    let deadline = Instant::now() + FIRST_BIND_POLL;
    while count_substr(&broker_spawn_log(&home), "stable endpoint bound at") == 0
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    // Read once and keep it. Re-reading at assertion time is what made the
    // old failure print a snapshot it had not asserted on (soldr#2624).
    let log_after_first = broker_spawn_log(&home);
    let binds_after_first = count_substr(&log_after_first, "stable endpoint bound at");

    // Second front-door invocation against the same endpoint. It may reuse
    // the live singleton without spawning a duplicate candidate.
    let mut second = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut second);
    isolate_home(&mut second, &home);
    let mut second_child = second
        .arg("status")
        .env("SOLDR_CACHE_DIR", &root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn second front-door soldr status");
    let _o2 = drain(second_child.stdout.take().expect("stdout"));
    let _e2 = drain(second_child.stderr.take().expect("stderr"));
    let second_status = second_child.wait().expect("wait second status");
    std::thread::sleep(Duration::from_millis(500));
    let log = broker_spawn_log(&home);
    let total_binds = count_substr(&log, "stable endpoint bound at");

    // Cleanup before asserting so a failure never leaks processes.
    let mut stop = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut stop);
    isolate_home(&mut stop, &home);
    let _ = stop.args(["broker", "stop"]).output();
    stop_daemons_in_root(&root);

    assert!(
        binds_after_first >= 1,
        "{}",
        first_bind_failure(
            &format!("{first_status}"),
            &rendered(&o1),
            &rendered(&e1),
            &log_after_first,
            total_binds,
            &log,
        )
    );
    assert!(
        second_status.success(),
        "the second front-door command must reuse the live broker\n{log}"
    );
    assert_eq!(
        total_binds, 1,
        "exactly one broker may bind the stable endpoint; the front door may reuse \
             it without spawning a duplicate candidate\n{log}"
    );
}
