//! Tests for Windows process-tree termination.
//!
//! A sibling file rather than an inline `#[cfg(test)]` block: these tests
//! spawn real `cmd`/`ping` trees, and `.github/scripts/spawn_path_guard.py`
//! counts `.spawn()` per FILE without stripping cfg(test). Inline, the test
//! spawns would have to be allowlisted as if they were sanctioned production
//! spawn sites -- exactly the misrepresentation the guard exists to prevent.
//! The guard already skips `*_tests.rs` siblings for this reason.

use super::*;

/// Liveness by exit code, not handle existence: a terminated
/// process stays openable while any handle survives, so a
/// handle-existence check would report a successful kill as failed.
fn is_alive(pid: u32) -> bool {
    crate::process::inspect::is_alive(pid)
}

/// Wine can publish a snapshot entry before initializing the process's
/// creation FILETIME (observed as 0x5555555555555555). Fixture readiness
/// requires a valid, stable creation time, not merely an enumerated PID.
fn descendants_have_initialized_times(pids: &[u32]) -> bool {
    let mut now = windows_sys::Win32::Foundation::FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    // SAFETY: `now` is a writable FILETIME, and the API retains no pointer.
    unsafe { windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime(&mut now) };
    let now = filetime_value(now);
    !pids.is_empty()
        && pids.iter().all(|pid| {
            TrackedDescendant::open(*pid).is_ok_and(|observed| {
                observed.created != 0
                    && observed.created <= now
                    && observed.times().is_ok_and(|times| times.exited == 0)
            })
        })
}

/// Fixture creation times: a lower PID was created earlier, so every edge in
/// the shape tests below is a genuine parent -> child edge.
fn in_pid_order(pid: u32) -> Option<u64> {
    Some(u64::from(pid))
}

#[test]
fn an_orphan_naming_a_recycled_parent_pid_is_not_collected() {
    // 900 was created long ago by a parent that has since exited.
    // That PID (300) was then recycled for a new child of the root. The stale
    // edge 900 -> 300 must not make 900 (or its child 950) a descendant.
    let edges = [(300, 100), (900, 300), (950, 900), (400, 300)];
    let created = |pid| match pid {
        100 => Some(10),
        300 => Some(20),
        400 => Some(30),
        900 => Some(1),
        950 => Some(2),
        _ => None,
    };
    assert_eq!(collect_descendants(&edges, 100, created), vec![300, 400]);
}

#[test]
fn an_edge_without_creation_times_is_not_trusted() {
    let edges = [(200, 100), (300, 100)];
    let created = |pid| (pid != 300).then_some(u64::from(pid));
    assert_eq!(collect_descendants(&edges, 100, created), vec![200]);
}

#[test]
fn descendants_are_ordered_parents_before_children() {
    // 100 -> 200 -> 300, plus an unrelated 400.
    let edges = [(200, 100), (300, 200), (400, 1)];
    assert_eq!(collect_descendants(&edges, 100, in_pid_order), vec![200, 300]);
}

#[test]
fn unrelated_processes_are_never_collected() {
    let edges = [(200, 100), (400, 1), (500, 400)];
    assert_eq!(collect_descendants(&edges, 100, in_pid_order), vec![200]);
}

#[test]
fn a_leaf_root_has_no_descendants() {
    let edges = [(200, 100), (300, 200)];
    assert!(collect_descendants(&edges, 300, in_pid_order).is_empty());
}

#[test]
fn a_self_parenting_process_does_not_loop() {
    // Recycled pids can produce a self-edge; the walk must still finish.
    let edges = [(100, 100), (200, 100)];
    assert_eq!(collect_descendants(&edges, 100, in_pid_order), vec![200]);
}

#[test]
fn a_cycle_back_to_the_root_does_not_loop() {
    // 100 -> 200 -> 300 -> 100. The root is pre-seeded as seen, so the edge
    // closing the cycle is ignored rather than revisited forever.
    let edges = [(200, 100), (300, 200), (100, 300)];
    assert_eq!(collect_descendants(&edges, 100, in_pid_order), vec![200, 300]);
}

/// Spawn `cmd -> ping` and return `(child, grandchild pids)` once the
/// grandchild exists. Killing before `ping` spawns would prove nothing.
///
/// `zombie_processes` cannot follow a `Child` handed back to a caller, so
/// it reads the success path as a leak. It is not: ownership transfers,
/// and both callers reap (one via `child.kill()` + `wait()`, the other via
/// `terminate_tree` + `wait()`). The timeout path reaps here before it
/// panics, so no path leaves the pair running.
#[allow(clippy::zombie_processes)]
fn spawn_cmd_with_ping_grandchild() -> (std::process::Child, Vec<u32>) {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new("cmd")
        .args(["/C", "ping -n 30 127.0.0.1 > nul"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cmd wrapper");
    let root = child.id();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let found = descendants_of(root).expect("snapshot");
        if descendants_have_initialized_times(&found) {
            return (child, found);
        }
        if Instant::now() >= deadline {
            // Reap before unwinding: a panic here would otherwise leave a
            // 30s `cmd`/`ping` pair behind for every run of the suite.
            let _ = child.kill();
            let _ = child.wait();
            panic!("ping grandchild never appeared under cmd pid {root}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The cargo diagnostic-capture shape: `cmd` launches `ping`, which inherits
/// an open stderr pipe.  A root-only kill leaves the reader blocked until the
/// ping's natural timeout, so EOF is a concrete proof that the descendant was
/// actually terminated rather than merely becoming unreachable in a snapshot.
#[allow(clippy::zombie_processes)]
fn spawn_cmd_with_ping_grandchild_and_stderr_pipe(
) -> (
    std::process::Child,
    Vec<u32>,
    std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
) {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new("cmd")
        .args(["/C", "ping -n 30 127.0.0.1 > nul"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cmd wrapper with stderr pipe");
    let root = child.id();

    let deadline = Instant::now() + Duration::from_secs(10);
    let descendants = loop {
        let found = descendants_of(root).expect("snapshot");
        if descendants_have_initialized_times(&found) {
            break found;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("ping grandchild never appeared under cmd pid {root}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let mut stderr = child.stderr.take().expect("piped stderr");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = sender.send(stderr.read_to_end(&mut bytes).map(|_| bytes));
    });
    (child, descendants, receiver)
}

/// Keep the test fixture from leaking its intentionally long-lived `ping` on
/// an assertion failure. Production returns the weaker result in that case;
/// the test still must leave the runner clean before it reports the failure.
fn cleanup_piped_cmd_tree(
    child: &mut std::process::Child,
    descendants: &[u32],
    stderr_closed: &std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
) {
    let _ = child.kill();
    let _ = child.wait();
    for pid in descendants {
        let _ = signal_pid(*pid, true);
    }
    let _ = stderr_closed.recv_timeout(std::time::Duration::from_secs(2));
}

/// Pins the negative result that shaped this module.
///
/// The obvious explanation for soldr#2605 is that a grandchild becomes
/// unreachable once its parent dies, so a kill-time tree walk cannot find
/// it. That is false on Windows, and this test is here so nobody (including
/// a future reader of the commit that introduced it) re-derives the wrong
/// mechanism: a process entry keeps its `th32ParentProcessID` after the
/// parent exits, so the walk still resolves.
///
/// Which is why `terminate_tree` does not rest on enumeration order alone
/// -- it verifies. If this test ever starts failing, the walk has become
/// order-sensitive after all and the snapshot-first ordering becomes
/// load-bearing rather than merely tidy.
///
/// **Scope, narrowed by soldr#2806 follow-up.** What this pins is that a dead
/// *root* still exposes its children: the fixture is `cmd -> ping`, so the
/// grandchild's parent is the root itself, and edges are matched by parent pid
/// rather than walked through parent nodes. It does not extend to a dead
/// *intermediate* one level further down, which really is unreachable --
/// see `the_walk_cannot_bridge_a_dead_intermediate`.
#[test]
fn a_dead_root_still_exposes_its_grandchildren() {
    use std::time::{Duration, Instant};

    let (mut child, grandchildren) = spawn_cmd_with_ping_grandchild();
    let root = child.id();

    // Kill ONLY the root, the way the old fallback path did.
    child.kill().expect("kill root");
    let _ = child.wait();

    let survivors: Vec<u32> = grandchildren
        .iter()
        .copied()
        .filter(|pid| is_alive(*pid))
        .collect();
    assert!(
        !survivors.is_empty(),
        "fixture invalid: the grandchild died with the root, so there is              nothing for a post-kill walk to find"
    );

    let after = descendants_of(root).expect("snapshot");
    assert!(
        after.iter().any(|pid| survivors.contains(pid)),
        "a walk rooted at the dead pid {root} lost the still-live              grandchildren {survivors:?} (walk returned {after:?}). If this              fails, kill-time enumeration IS unsound on this host and the              snapshot-first ordering is what saves us."
    );

    // Clean up the orphan the fixture deliberately created.
    for pid in survivors {
        let _ = signal_pid(pid, true);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while grandchildren.iter().any(|pid| is_alive(*pid)) {
        assert!(Instant::now() < deadline, "orphaned ping outlived cleanup");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_real_grandchild_tree_is_killed_whole() {
    // The soldr#2605 shape: soldr -> cmd.exe -> ping.exe. Assert the
    // grandchild is gone, not merely the root -- a kill that reaches only the
    // direct child is precisely the outcome that left `ping` holding the
    // inherited stderr pipe for its full sleep.
    use std::time::Instant;

    let (mut child, grandchildren) = spawn_cmd_with_ping_grandchild();
    let grandchild = *grandchildren.first().expect("ping grandchild");
    // Open this query-only handle before termination. It remains bound to the
    // original ping process even after the pid becomes reusable, so its exit
    // FILETIME is the same proof production relies on.
    let observed = TrackedDescendant::open(grandchild).expect("open query handle");
    let started = Instant::now();

    assert_eq!(
        terminate_tree(&mut child).expect("terminate tree"),
        TreeKill::TreeKilled
    );
    let _ = child.wait();
    assert!(
        started.elapsed() <= VERIFY_BUDGET,
        "tree verification exceeded its fixed two-second budget"
    );
    assert_ne!(
        observed.times().expect("query retained handle").exited,
        0,
        "the pre-opened grandchild handle must report its exit FILETIME"
    );

    // Do not reopen `grandchild` by PID here: after the retained original
    // handle has proved its exit, that PID may already name an unrelated
    // process. The retained handle plus exit FILETIME is the identity-correct
    // completion proof; the pure PID-reuse regression below covers that rule.
}

#[test]
fn a_real_grandchild_with_inherited_stderr_pipe_is_killed_whole() {
    use std::time::Instant;

    let (mut child, grandchildren, stderr_closed) =
        spawn_cmd_with_ping_grandchild_and_stderr_pipe();
    let started = Instant::now();

    match terminate_tree(&mut child) {
        Ok(TreeKill::TreeKilled) => {}
        Ok(other) => {
            cleanup_piped_cmd_tree(&mut child, &grandchildren, &stderr_closed);
            panic!("terminate tree returned {other:?}");
        }
        Err(error) => {
            cleanup_piped_cmd_tree(&mut child, &grandchildren, &stderr_closed);
            panic!("terminate tree failed: {error}");
        }
    }
    let _ = child.wait();
    if started.elapsed() > VERIFY_BUDGET {
        cleanup_piped_cmd_tree(&mut child, &grandchildren, &stderr_closed);
        panic!("tree verification exceeded its fixed two-second budget");
    }
    match stderr_closed.recv_timeout(VERIFY_BUDGET) {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            cleanup_piped_cmd_tree(&mut child, &grandchildren, &stderr_closed);
            panic!("read inherited stderr pipe: {error}");
        }
        Err(error) => {
            cleanup_piped_cmd_tree(&mut child, &grandchildren, &stderr_closed);
            panic!("the inherited stderr pipe did not close after tree termination: {error}");
        }
    }
    if grandchildren.iter().any(|pid| is_alive(*pid)) {
        cleanup_piped_cmd_tree(&mut child, &grandchildren, &stderr_closed);
        panic!("a ping grandchild still holds the inherited stderr pipe");
    }
}

#[test]
fn query_only_and_terminate_only_accesses_avoid_synchronize() {
    assert_eq!(PROCESS_QUERY_LIMITED_INFORMATION, 0x1000);
    assert_eq!(PROCESS_TERMINATE, 0x0001);
    assert_eq!(
        PROCESS_QUERY_LIMITED_INFORMATION & SYNCHRONIZE,
        0,
        "the retained identity handle must work under a token that denies SYNCHRONIZE"
    );
    assert_eq!(
        PROCESS_TERMINATE & SYNCHRONIZE,
        0,
        "the termination request must not require a waitable handle"
    );
}

#[test]
fn query_only_handle_and_successful_terminate_can_prove_completion() {
    let before = ProcessTimes {
        created: 10,
        exited: 0,
    };
    let after = ProcessTimes {
        created: 10,
        exited: 99,
    };
    assert!(is_same_process(10, Some(before.created)));
    assert!(completion_proven(10, Some(after), terminate_request_succeeded(true)));
}

#[test]
fn a_false_terminate_bool_prevents_tree_killed() {
    assert!(!completion_proven(
        10,
        Some(ProcessTimes {
            created: 10,
            exited: 99,
        }),
        terminate_request_succeeded(false)
    ));
}

#[test]
fn query_or_identity_failure_prevents_tree_killed() {
    assert!(!completion_proven(10, None, true));
    assert!(!completion_proven(
        10,
        Some(ProcessTimes {
            created: 11,
            exited: 99,
        }),
        true
    ));
}

#[test]
fn an_unqueryable_fresh_descendant_is_still_terminated_best_effort() {
    let mut requested = Vec::new();
    terminate_pids_leaves_first(&[200, 300], |pid| requested.push(pid));
    assert_eq!(requested, vec![300, 200]);
}

#[test]
fn an_unqueryable_sibling_does_not_skip_tracked_termination() {
    let mut verified = false;
    let mut requested = 0;
    request_tracked_after_root(&mut verified, || {
        requested += 1;
        true
    });
    assert_eq!(requested, 1, "tracked descendants must still be terminated");
    assert!(
        !verified,
        "the unqueryable sibling still prevents a verified tree-killed claim"
    );
}

#[test]
fn pid_reuse_is_a_new_identity_even_with_the_same_pid() {
    let original = ProcessTimes {
        created: 10,
        exited: 50,
    };
    let recycled = ProcessTimes {
        created: 99,
        exited: 0,
    };
    assert!(!completion_proven(original.created, Some(recycled), true));
}

/// leaves-first termination kills `ping`, and its waiting
/// `cmd.exe` then exits by itself before its own TerminateProcess, which fails
/// with ERROR_ACCESS_DENIED. A proven exit must not be reported as a failure.
#[test]
fn a_descendant_that_exits_before_terminate_still_completes() {
    let exited = ProcessTimes {
        created: 10,
        exited: 99,
    };
    assert!(exited_despite_failed_terminate(10, Some(exited)));
    let running = ProcessTimes {
        created: 10,
        exited: 0,
    };
    assert!(!exited_despite_failed_terminate(10, Some(running)));
    assert!(!exited_despite_failed_terminate(10, None));
    assert!(!exited_despite_failed_terminate(11, Some(exited)));
}

/// a snapshot entry that exited before OpenProcess is gone, not
/// unverifiable; any other open failure still weakens the claim.
#[test]
fn a_vanished_snapshot_entry_is_gone_not_untracked() {
    assert!(open_failure_proves_gone(&io::Error::from_raw_os_error(87)));
    assert!(!open_failure_proves_gone(&io::Error::from_raw_os_error(5)));
    assert!(!open_failure_proves_gone(&io::Error::other("query failed")));
}
