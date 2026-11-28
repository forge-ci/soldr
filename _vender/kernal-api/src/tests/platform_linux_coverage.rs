use super::*;
use crate::platform::process::{
    CaptureStream, NonInvasiveObservationGrade, ObserverCategory, ObserverScope, ObserverSupport,
    ProcessCommandConfig, UnixSignalKind,
};
use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

// `ObserverScope`, `ObserverCategory`, and `ObserverSupport` are `Copy` value
// enums without `Debug`, so name their variants here rather than widening the
// public API purely to make an assertion message readable.
fn scope_label(scope: ObserverScope) -> &'static str {
    match scope {
        ObserverScope::SystemWide => "SystemWide",
        ObserverScope::LaunchedProcessTree => "LaunchedProcessTree",
    }
}

fn category_label(category: ObserverCategory) -> &'static str {
    match category {
        ObserverCategory::File => "File",
        ObserverCategory::Network => "Network",
        ObserverCategory::Process => "Process",
    }
}

fn support_label(support: ObserverSupport) -> &'static str {
    match support {
        ObserverSupport::Supported => "Supported",
        ObserverSupport::Partial => "Partial",
        ObserverSupport::Unavailable => "Unavailable",
    }
}

#[test]
fn capability_environment_and_trace_backend_are_complete() {
    let pairs = vec![("A".into(), "1".into()), ("A".into(), "2".into())];
    assert_eq!(canonical_environment_pairs(pairs.clone()), pairs);
    assert!(monitor_console_windows(Duration::ZERO).is_empty());
    assert!(process_snapshot()
        .iter()
        .any(|snapshot| snapshot.identity.pid() == std::process::id()));
    assert!(process_snapshot_for_pid(std::process::id()).is_some());
    assert!(!parent_has_console());

    let capability = exact_trace_capability();
    assert!(capability.available);
    assert_eq!(capability.backend, "linux-ptrace");
    assert_eq!(
        capability.non_invasive_grade,
        NonInvasiveObservationGrade::SnapshotInferred
    );
}

#[test]
fn unix_signal_numbers_are_positive_and_distinct() {
    let interrupt = unix_signal_raw(UnixSignalKind::Interrupt);
    let terminate = unix_signal_raw(UnixSignalKind::Terminate);
    let kill = unix_signal_raw(UnixSignalKind::Kill);
    assert!(interrupt > 0 && terminate > 0 && kill > 0);
    assert_ne!(interrupt, terminate);
    assert_ne!(terminate, kill);
}

#[test]
fn observer_matrix_reports_every_scope_and_category() {
    for (scope, category, support, backend) in [
        (
            ObserverScope::SystemWide,
            ObserverCategory::File,
            ObserverSupport::Unavailable,
            "seccomp-user-notify",
        ),
        (
            ObserverScope::SystemWide,
            ObserverCategory::Network,
            ObserverSupport::Unavailable,
            "ebpf",
        ),
        (
            ObserverScope::SystemWide,
            ObserverCategory::Process,
            ObserverSupport::Unavailable,
            "seccomp-user-notify",
        ),
        (
            ObserverScope::LaunchedProcessTree,
            ObserverCategory::File,
            ObserverSupport::Partial,
            "proc-fd-snapshot",
        ),
        (
            ObserverScope::LaunchedProcessTree,
            ObserverCategory::Network,
            ObserverSupport::Unavailable,
            "none",
        ),
        (
            ObserverScope::LaunchedProcessTree,
            ObserverCategory::Process,
            ObserverSupport::Supported,
            "subreaper-proc-poll",
        ),
    ] {
        let row = format!("{}/{}", scope_label(scope), category_label(category));
        let result = observer_backend(scope, category);
        assert_eq!(
            support_label(result.support),
            support_label(support),
            "{row}"
        );
        assert_eq!(result.backend, backend, "{row}");
        assert!(!result.reason.is_empty(), "{row}");
    }
}

#[test]
fn absent_process_operations_report_their_posix_errors() {
    let absent = i32::MAX as u32;
    assert!(unix_signal_process(absent, UnixSignalKind::Kill).is_err());
    assert!(unix_set_priority(absent, 0).is_err());
}

/// The absent-group case alone cannot separate "an absent group is a tolerated
/// no-op" from "this function never signals anything", because
/// `soft_terminate_process_group` swallows `ESRCH` by design. Pair it with a
/// live child-owned group so the two halves together pin the behavior down.
#[test]
fn soft_terminate_signals_an_owned_group_and_tolerates_an_absent_one() {
    let mut child = {
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", "exec sleep 30"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        configure_process_command(
            &mut command,
            ProcessCommandConfig {
                create_process_group: true,
                ..Default::default()
            },
        )
        .unwrap();
        command.spawn().unwrap()
    };
    let pid = child.id();

    // `spawn` returns only once the child has executed, so its `setpgid` has
    // already run: the child leads a group whose id is its own pid, which makes
    // a signal to `-pid` reach exactly this fixture.
    assert_eq!(
        unsafe { libc::getpgid(pid as i32) },
        pid as i32,
        "create_process_group should make the child its own group leader"
    );

    soft_terminate_process_group(pid).unwrap();

    // A loaded runner can take a while to schedule the signalled child, so the
    // bound is generous; only "never terminates at all" should fail here.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait().unwrap() {
            Some(status) => break status,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the child-owned group outlived soft_terminate_process_group");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(libc::SIGTERM),
        "the owned group should end on SIGTERM, not on any other disposition"
    );

    // `i32::MAX` is above every reachable `pid_max`, so no live group can be
    // hit here. The `Ok` below is specifically the swallowed `ESRCH` that the
    // sibling raw group signal reports as an error.
    let absent = i32::MAX as u32;
    assert!(soft_terminate_process_group(absent).is_ok());
    let error = unix_signal_process_group(i32::MAX, UnixSignalKind::Terminate)
        .expect_err("an absent group has no raw signal target");
    assert_eq!(error.raw_os_error(), Some(libc::ESRCH));
}

#[test]
fn gnu_note_parser_accepts_build_ids_and_rejects_malformed_notes() {
    let mut note = Vec::new();
    note.extend_from_slice(&4_u32.to_ne_bytes());
    note.extend_from_slice(&3_u32.to_ne_bytes());
    note.extend_from_slice(&3_u32.to_ne_bytes());
    note.extend_from_slice(b"GNU\0");
    note.extend_from_slice(&[1, 2, 3, 0]);
    assert_eq!(gnu_build_id_from_notes(&note), Some(&[1, 2, 3][..]));

    let mut other = note.clone();
    other[8..12].copy_from_slice(&1_u32.to_ne_bytes());
    assert_eq!(gnu_build_id_from_notes(&other), None);
    assert_eq!(gnu_build_id_from_notes(&note[..note.len() - 1]), None);
    assert_eq!(gnu_build_id_from_notes(&[]), None);
}

#[test]
fn exit_status_and_shell_spec_preserve_linux_conventions() {
    let terminate = unix_signal_raw(UnixSignalKind::Terminate);
    let exited = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 7"])
        .status()
        .unwrap();
    let signaled = std::process::Command::new("/bin/sh")
        .args(["-c", "kill -TERM $$"])
        .status()
        .unwrap();
    assert_eq!(exit_code(exited), 7);
    assert_eq!(trampoline_exit_code(exited), 7);
    assert_eq!(exit_code(signaled), -terminate);
    assert_eq!(trampoline_exit_code(signaled), 128 + terminate);

    let spec = shell_spec(OsStr::new("printf coverage"));
    assert_eq!(spec.program, OsStr::new("/bin/sh"));
    assert_eq!(spec.args, [OsStr::new("-c"), OsStr::new("printf coverage")]);
}

#[test]
fn linux_only_stubs_and_shell_builders_are_explicit() {
    let command_text = "printf platform-coverage";
    for command in [shell_command(command_text), compat_shell_command(command_text)] {
        assert_eq!(command.get_program(), OsStr::new("/bin/sh"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [OsStr::new("-lc"), OsStr::new(command_text)]
        );
    }

    let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
    let error = match assign_child_to_windows_job(&child, child.id(), None, None) {
        Ok(_) => panic!("Linux cannot create a Windows Job Object"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert_eq!(sync_child_native_handle(&child), 0);
    assert!(child.wait().unwrap().success());
}

#[test]
fn capture_readers_deliver_data_and_wake_on_cancellation() {
    let cancellation = Arc::new(CaptureCancellation::default());
    let (mut writer, reader) = UnixStream::pair().unwrap();
    let mut prepared =
        prepare_capture_reader(reader, &cancellation, CaptureStream::Stdout).unwrap();
    assert_eq!(prepared.read(&mut []).unwrap(), 0);
    writer.write_all(b"output").unwrap();
    let mut bytes = [0_u8; 6];
    prepared.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"output");
    capture_reader_done(&cancellation, CaptureStream::Stdout);

    let (_writer, reader) = UnixStream::pair().unwrap();
    let mut blocked =
        prepare_capture_reader(reader, &cancellation, CaptureStream::Stderr).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut byte = [0_u8; 1];
        tx.send(blocked.read(&mut byte).unwrap_err().kind()).unwrap();
    });
    cancel_capture_reader(&cancellation);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        io::ErrorKind::Interrupted
    );
    reader_thread.join().unwrap();
    capture_reader_done(&cancellation, CaptureStream::Stderr);
    assert!(set_nonblocking(-1).is_err());
}

#[test]
fn reviewed_command_configuration_executes_on_short_lived_children() {
    set_process_name("running-process-platform-name-is-truncated");

    let mut plain = std::process::Command::new("/bin/true");
    configure_trampoline_command(&mut plain);
    configure_process_command(&mut plain, ProcessCommandConfig::default()).unwrap();
    assert!(plain.status().unwrap().success());

    let mut configured = std::process::Command::new("/bin/true");
    configure_process_command(
        &mut configured,
        ProcessCommandConfig {
            create_process_group: true,
            // 19 can only lower (or preserve) inherited priority, so this
            // branch never requires CAP_SYS_NICE on a pre-reniced runner.
            nice: Some(19),
            address_space_limit_bytes: Some(512 * 1024 * 1024),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(configured.status().unwrap().success());

    let mut daemon = std::process::Command::new("/bin/true");
    configure_sync_daemon_command(&mut daemon).unwrap();
    assert!(daemon.status().unwrap().success());

    let mut contained = std::process::Command::new("/bin/true");
    configure_sync_contained_command(&mut contained).unwrap();
    assert!(contained.status().unwrap().success());

}
