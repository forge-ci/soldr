//! Public contract for reuse-safe exit observation and owner-bound lifetime.
//!
//! Every assertion here is about a guarantee rather than a mechanism, because
//! the mechanism differs per host and the guarantee is what a consumer writes
//! its cleanup against. The one place a host is named is the enforcement
//! report, which exists precisely so a caller can tell the hosts apart.
//!
//! The release checks are the exception that proves it: what they assert is
//! that a watch nobody can act on any more is gone, and the only honest way
//! to ask that is to read the host's own descriptor table, so they are
//! written against the host whose watch is a descriptor.

use std::time::Duration;

use kernal_api::async_engine::{timeout, Runtime, RuntimeBuilder};
use kernal_api::platform::process::{
    lifetime_enforcement_for, LifetimeEnforcement, LifetimeOwner, ProcessExitObservation,
    ProcessExitWatch, ProcessId, ProcessInspectErrorKind,
};
use kernal_api::{shell_spec, SpawnSpec, StreamMode};
#[cfg(target_os = "linux")]
use kernal_api::{ProcessPostExitDrain, ProcessSessionOptions};

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("facade runtime")
}

/// A process that ends the moment it starts, on whichever host runs this.
fn command_that_exits_now() -> std::process::Command {
    let mut command = shell_command();
    command.arg("exit 0");
    command
}

/// A process that lives long enough to be watched, then exits with 7.
fn command_that_exits_with_seven() -> std::process::Command {
    let mut command = shell_command();
    #[cfg(windows)]
    command.arg("ping 127.0.0.1 -n 2 > nul & exit 7");
    #[cfg(not(windows))]
    command.arg("sleep 0.2; exit 7");
    command
}

/// A process that outlives the test unless something ends it.
fn command_that_runs_long() -> std::process::Command {
    let mut command = shell_command();
    command.arg(LONG_RUNNING);
    command
}

fn long_running_spec() -> SpawnSpec {
    shell_spec(LONG_RUNNING)
}

#[cfg(windows)]
const LONG_RUNNING: &str = "ping 127.0.0.1 -n 31 > nul";
#[cfg(not(windows))]
const LONG_RUNNING: &str = "sleep 30";

fn shell_command() -> std::process::Command {
    #[cfg(windows)]
    {
        let mut command = std::process::Command::new("cmd.exe");
        command.arg("/C");
        command
    }
    #[cfg(not(windows))]
    {
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c");
        command
    }
}

/// The trap, closed in the type rather than tested for at each call site.
///
/// `u32::MAX` is the value that reached `kill(2)` as `-1` in the consumer this
/// crate is taking the capability over from, where it would have signalled
/// every process the caller was permitted to signal.
#[test]
fn a_process_id_holds_only_values_that_name_one_process() {
    assert!(
        ProcessId::new(0).is_err(),
        "0 is a process group, not a pid"
    );
    assert!(ProcessId::new(1).is_ok());
    assert!(ProcessId::new(i32::MAX as u32).is_ok());
    assert!(
        ProcessId::new(i32::MAX as u32 + 1).is_err(),
        "the first value whose signed reading is negative"
    );
    assert!(
        ProcessId::new(u32::MAX).is_err(),
        "u32::MAX reads as -1: every process this caller may signal"
    );

    let error = ProcessId::new(u32::MAX).expect_err("out of range");
    assert_eq!(error.kind, ProcessInspectErrorKind::InvalidPid);

    assert_eq!(ProcessId::current().get(), std::process::id());
    assert_eq!(
        ProcessId::try_from(std::process::id()).expect("own pid is in range"),
        ProcessId::current()
    );
}

/// Acquisition is the reuse safety: a watch either names the process running
/// at that moment or does not exist, so there is nothing to silently retarget
/// when the number is handed to somebody else.
///
/// The child handle is deliberately still held here. On Windows that keeps
/// the process *object* alive after the process itself is finished, and
/// `OpenProcess` goes on succeeding against it; the hosts have to agree
/// anyway, because a caller reading a PID out of a state row has no idea who
/// else is holding a handle to it.
#[test]
fn a_watch_cannot_be_acquired_for_a_process_that_has_gone() {
    let mut child = command_that_exits_now()
        .spawn()
        .expect("spawn a process to lose");
    let pid = ProcessId::new(child.id()).expect("a spawned pid is in range");
    child.wait().expect("reap");

    let error = ProcessExitWatch::open(pid).expect_err("a finished process has no exit left");
    assert_eq!(error.kind, ProcessInspectErrorKind::NotFound);
}

/// The exit arrives because the kernel pushed it, and the status comes with
/// it on a host that reports one for a process this one parented.
#[test]
fn a_watch_reports_an_exit_without_reaping_it() {
    let mut child = command_that_exits_with_seven()
        .spawn()
        .expect("spawn the observed process");
    let watch = ProcessExitWatch::open(ProcessId::new(child.id()).expect("pid in range"))
        .expect("open a watch on a live process");

    let observation = runtime()
        .run_within(
            async {
                timeout(Duration::from_secs(10), watch.exited())
                    .await
                    .expect("the exit must arrive without polling")
                    .expect("observing an exit must not fail")
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");

    match observation {
        ProcessExitObservation::Reported(exit) => {
            assert_eq!(exit.exit_code(), Some(7));
            assert_eq!(exit.signal(), None);
        }
        ProcessExitObservation::Unreported => {
            // Every supported host reports a status for a process this one
            // parented, except where macOS withholds `NOTE_EXITSTATUS`. The
            // exit itself was still observed, which is the guarantee; the
            // status is the bonus, and only this host may skip it.
            #[cfg(not(target_os = "macos"))]
            panic!("this host reports the status of a child it parented");
        }
    }

    // The watch peeked; it did not reap. The real parent still can.
    let status = child.wait().expect("the observer must not have reaped");
    assert_eq!(status.code(), Some(7));
}

/// A watch that outlives its target keeps answering rather than hanging.
#[test]
fn a_watch_kept_past_the_exit_still_returns_at_once() {
    let mut child = command_that_exits_now()
        .spawn()
        .expect("spawn the observed process");
    let watch = ProcessExitWatch::open(ProcessId::new(child.id()).expect("pid in range"))
        .expect("open a watch on a live process");
    assert_eq!(watch.pid().get(), child.id());

    runtime()
        .run_within(
            async {
                timeout(Duration::from_secs(10), watch.exited())
                    .await
                    .expect("the first observation arrives")
                    .expect("observing an exit must not fail");
                child.wait().expect("reap");
                timeout(Duration::from_secs(10), watch.exited())
                    .await
                    .expect("a second observation must not wait for an exit already seen")
                    .expect("observing an exit must not fail");
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

/// Which enforcement each owner gets is a host fact the caller can read
/// before it has a child to clean up.
#[test]
fn the_enforcement_for_an_owner_is_answerable_before_spawning() {
    let spawner = lifetime_enforcement_for(LifetimeOwner::Spawner);
    let other = lifetime_enforcement_for(LifetimeOwner::Process(ProcessId::current()));

    assert_eq!(
        other,
        LifetimeEnforcement::Watcher,
        "no host enforces a binding to a process the spawner does not parent"
    );
    assert!(!other.is_kernel_enforced());

    #[cfg(target_os = "linux")]
    {
        assert_eq!(spawner, LifetimeEnforcement::ParentDeathSignal);
        assert!(spawner.is_kernel_enforced());
    }
    #[cfg(target_os = "windows")]
    {
        assert_eq!(spawner, LifetimeEnforcement::KernelContainer);
        assert!(spawner.is_kernel_enforced());
    }
    #[cfg(target_os = "macos")]
    {
        assert_eq!(
            spawner,
            LifetimeEnforcement::Watcher,
            "this host has neither a parent-death signal nor job objects"
        );
    }

    assert_eq!(
        SpawnSpec::new("does-not-run").lifetime_enforcement(),
        None,
        "an unbound spawn promises nothing rather than something weak"
    );
    assert_eq!(
        SpawnSpec::new("does-not-run")
            .kill_when_owner_dies(true)
            .lifetime_enforcement(),
        Some(spawner),
        "the shorthand is the same request as binding to the spawner"
    );
}

/// The capability soldr's daemon-per-root shape actually needs: an owner that
/// is not the parent, and a child that does not outlive it.
#[test]
fn a_child_bound_to_another_owner_dies_when_that_owner_does() {
    let mut owner = command_that_runs_long()
        .spawn()
        .expect("spawn the nominated owner");
    let owner_id = ProcessId::new(owner.id()).expect("pid in range");

    runtime()
        .run_within(
            async {
                let mut child = long_running_spec()
                    .stdin(StreamMode::Null)
                    .bind_lifetime(LifetimeOwner::Process(owner_id))
                    .spawn()
                    .await
                    .expect("spawn a child bound to the owner");

                assert_eq!(
                    child.lifetime_enforcement(),
                    Some(LifetimeEnforcement::Watcher),
                    "binding to a non-parent owner is watcher-enforced, and says so"
                );

                owner.kill().expect("end the owner");
                owner.wait().expect("reap the owner");

                child
                    .wait_within(Duration::from_secs(5))
                    .await
                    .expect("the child must not outlive its owner");
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

/// An owner that is already gone leaves nothing to watch, so the spawn fails
/// rather than producing exactly the orphan the caller asked to avoid.
#[test]
fn binding_to_an_owner_that_has_already_exited_fails_the_spawn() {
    let mut owner = command_that_exits_now()
        .spawn()
        .expect("spawn the nominated owner");
    let owner_id = ProcessId::new(owner.id()).expect("pid in range");
    owner.wait().expect("reap the owner");

    let outcome = runtime()
        .run_within(
            async {
                long_running_spec()
                    .stdin(StreamMode::Null)
                    .bind_lifetime(LifetimeOwner::Process(owner_id))
                    .spawn()
                    .await
                    .map(|_| ())
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    let error = outcome.expect_err("a dead owner cannot be bound to");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

/// A bounded run has nowhere to park a watcher, and says so instead of
/// quietly binding to a different owner than the one it was given.
#[test]
fn a_bounded_run_refuses_an_owner_it_cannot_watch() {
    let error = kernal_api::run_bounded_command(
        shell_spec("exit 0").bind_lifetime(LifetimeOwner::Process(ProcessId::current())),
        Duration::from_secs(5),
        1024,
    )
    .expect_err("a bounded run binds only to its spawner");
    assert!(
        matches!(&error, kernal_api::BoundedProcessError::Io(error)
            if error.kind() == std::io::ErrorKind::InvalidInput),
        "the refusal names the argument, not a timeout: {error}"
    );
}

/// A session is *designed* to be held past its child's exit -- that is what
/// the post-exit drain is for -- so its owner watch has to stop when the
/// child does rather than when the handle is finally dropped.
///
/// The pidfd is the observable, because on this host the watch *is* one:
/// counting the descriptors that name the owner asks the kernel whether the
/// watch is still open, rather than asking the facade to describe itself.
#[cfg(target_os = "linux")]
#[test]
fn a_session_that_waited_stops_watching_its_owner() {
    let mut owner = command_that_runs_long()
        .spawn()
        .expect("spawn the nominated owner");
    let owner_id = ProcessId::new(owner.id()).expect("pid in range");
    assert_eq!(
        open_watches_of(owner_id),
        0,
        "nothing watches the owner yet"
    );

    runtime()
        .run_within(
            async {
                let session = session_bound_to(owner_id, "exit 0").await;
                assert_eq!(
                    open_watches_of(owner_id),
                    1,
                    "the binding must have opened the watch this test is about"
                );

                session
                    .wait()
                    .await
                    .expect("waiting on the exited child must succeed");
                assert!(
                    watch_closes(owner_id).await,
                    "the child is gone, so its owner watch must be too -- while the \
             session itself is still held open to drain output"
                );

                assert!(
                    session
                        .poll()
                        .await
                        .expect("poll the reaped child")
                        .is_some(),
                    "the session stays usable after releasing its watch"
                );
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");

    owner.kill().expect("end the owner");
    owner.wait().expect("reap the owner");
}

/// The other terminal path reaches the same answer: after a kill there is no
/// child left for the owner's death to protect.
#[cfg(target_os = "linux")]
#[test]
fn a_session_that_killed_stops_watching_its_owner() {
    let mut owner = command_that_runs_long()
        .spawn()
        .expect("spawn the nominated owner");
    let owner_id = ProcessId::new(owner.id()).expect("pid in range");

    runtime()
        .run_within(
            async {
                let session = session_bound_to(owner_id, LONG_RUNNING).await;
                assert_eq!(
                    open_watches_of(owner_id),
                    1,
                    "the binding must have opened the watch this test is about"
                );

                session.kill().await.expect("kill the direct child");
                assert!(
                    watch_closes(owner_id).await,
                    "a killed child leaves nothing for the owner watch to protect"
                );
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");

    owner.kill().expect("end the owner");
    owner.wait().expect("reap the owner");
}

/// A streaming session whose child is bound to `owner`.
#[cfg(target_os = "linux")]
// Holds the session under the deprecated `WaitForEof`, the longest a
// supervisor can (#433).
#[allow(deprecated)]
async fn session_bound_to(owner: ProcessId, command: &str) -> kernal_api::ProcessSession {
    shell_spec(command)
        .stdin(StreamMode::Null)
        .stdout(StreamMode::Piped)
        .stderr(StreamMode::Piped)
        .bind_lifetime(LifetimeOwner::Process(owner))
        .spawn_session(ProcessSessionOptions {
            max_queued_chunks: 2,
            max_chunk_bytes: 64,
            // The drain policy a supervisor holds a session longest under.
            post_exit_drain: ProcessPostExitDrain::WaitForEof,
            kill_on_drop: true,
        })
        .await
        .expect("spawn a session bound to the owner")
}

/// Whether the owner watch closes, given the chance to.
///
/// Releasing cancels the watching task, and cancellation completes at that
/// task's next scheduling turn rather than inside the release, so this yields
/// rather than reading the descriptor table once and calling it settled.
#[cfg(target_os = "linux")]
async fn watch_closes(owner: ProcessId) -> bool {
    for _ in 0..100 {
        if open_watches_of(owner) == 0 {
            return true;
        }
        kernal_api::async_engine::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// How many pidfds naming `pid` this process holds.
///
/// The descriptor's `fdinfo` names the process it was opened against, which
/// is what separates an owner watch from the descriptors the substrate keeps
/// for the child itself.
#[cfg(target_os = "linux")]
fn open_watches_of(pid: ProcessId) -> usize {
    let watched = format!("Pid:\t{}", pid.get());
    std::fs::read_dir("/proc/self/fd")
        .expect("this host publishes its own descriptor table")
        .filter_map(Result::ok)
        .filter(|entry| {
            std::fs::read_link(entry.path())
                .is_ok_and(|target| target.as_os_str().as_encoded_bytes() == b"anon_inode:[pidfd]")
        })
        .filter(|entry| {
            let fdinfo = std::path::Path::new("/proc/self/fdinfo").join(entry.file_name());
            std::fs::read_to_string(fdinfo)
                .is_ok_and(|info| info.lines().any(|line| line == watched))
        })
        .count()
}
