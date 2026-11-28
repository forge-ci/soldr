use super::*;
use std::io::Read as _;
use std::process::Stdio;

struct TracedChildCleanup(TracedChild);

impl std::ops::Deref for TracedChildCleanup {
    type Target = TracedChild;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for TracedChildCleanup {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for TracedChildCleanup {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match self.0.try_wait_code() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
}

#[test]
fn proc_helpers_describe_the_current_process_and_reject_missing_pids() {
    let pid = std::process::id();
    assert!(read_executable(pid).is_some());
    assert!(read_argv(pid).is_some_and(|args| !args.is_empty()));
    assert!(process_start_key(pid).is_some());
    assert_eq!(thread_group_id(pid), Some(pid));
    let _ = read_syscall_pointers(pid);

    let missing = u32::MAX;
    assert!(read_executable(missing).is_none());
    assert!(read_argv(missing).is_none());
    assert!(process_start_key(missing).is_none());
    assert!(thread_group_id(missing).is_none());
    assert_eq!(read_syscall_pointers(missing), (None, None));
}

#[test]
fn event_and_loss_helpers_preserve_trace_identity() {
    let tracee = Tracee {
        parent_pid: Some(7),
        parent_start_key: Some(70),
        start_key: Some(420),
        process_leader: true,
        executable: Some("/bin/echo".into()),
        argv: Some(vec![OsString::from("echo"), OsString::from("hello")]),
        origin: Some(TraceOriginArtifact::default()),
    };
    let event = event_for(3, 42, &tracee, ExactTraceEventKind::Exec);
    assert_eq!(event.sequence, 3);
    assert_eq!(event.pid, 42);
    assert_eq!(event.parent_pid, Some(7));
    assert_eq!(event.executable.as_deref(), Some(std::path::Path::new("/bin/echo")));

    let emitted = Mutex::new(Vec::new());
    emit_loss(
        &|event| emitted.lock().unwrap().push(event),
        4,
        43,
        Some(&tracee),
        "coverage loss".into(),
    );
    let emitted = emitted.into_inner().unwrap();
    assert_eq!(emitted.len(), 1);
    assert!(matches!(
        &emitted[0].kind,
        ExactTraceEventKind::Loss { reason } if reason == "coverage loss"
    ));

    let without_tracee = Mutex::new(Vec::new());
    emit_loss(
        &|event| without_tracee.lock().unwrap().push(event),
        5,
        44,
        None,
        "unknown".into(),
    );
    assert_eq!(without_tracee.into_inner().unwrap()[0].parent_pid, None);
}

#[test]
fn origin_capture_handles_live_and_missing_processes() {
    let pid = std::process::id();
    let live = capture_origin(pid);
    assert_eq!(live.origin_pid, pid);
    assert_eq!(live.thread_id, pid);
    assert!(!live.architecture.is_empty());
    assert!(live.register_format.starts_with("linux-nt-prstatus-"));
    assert!(live.executable.is_some());
    assert!(!live.module_map.is_empty());

    let missing = 2_147_483_647;
    let absent = capture_origin(missing);
    assert_eq!(absent.origin_pid, missing);
    assert_eq!(absent.thread_id, missing);
    assert!(absent.executable.is_none());
    assert!(absent.registers.is_empty());
    assert!(absent.stack.is_empty());
    assert!(absent.module_map.is_empty());
    assert!(!absent.module_map_truncated);
}

#[test]
fn root_completion_and_ptrace_errors_are_observable() {
    let shared = Shared {
        state: Mutex::new(RootState::default()),
        wake: Condvar::new(),
    };
    finish_root(&shared, -9);
    let state = shared.state.lock().unwrap();
    assert_eq!(state.exit_code, Some(-9));
    assert!(state.done);
    drop(state);

    assert!(ptrace_value(PtraceRequest::MAX, u32::MAX, 0, 0).is_err());
    detach_all(std::iter::empty());
}

#[test]
fn fatal_trace_cleanup_reports_loss_reaps_root_and_wakes_waiters() {
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 30"])
        .spawn()
        .expect("spawn trace-cleanup fixture");
    let root_pid = child.id();
    let missing_pid = 2_147_483_647;
    let tracee = Tracee {
        parent_pid: Some(root_pid),
        parent_start_key: None,
        start_key: None,
        process_leader: true,
        executable: None,
        argv: None,
        origin: None,
    };
    let tracees = HashMap::from([(root_pid, tracee.clone()), (missing_pid, tracee.clone())]);
    let shared = Shared {
        state: Mutex::new(RootState::default()),
        wake: Condvar::new(),
    };
    let events = Mutex::new(Vec::new());
    abort_runtime_trace(
        &mut child,
        &shared,
        &|event| events.lock().unwrap().push(event),
        9,
        missing_pid,
        Some(&tracee),
        &tracees,
        "forced coverage cleanup".into(),
    );
    assert!(child.try_wait().unwrap().is_some());
    let state = shared.state.lock().unwrap();
    assert!(state.done);
    assert_eq!(state.tracer_error.as_deref(), Some("forced coverage cleanup"));
    drop(state);
    assert!(matches!(
        &events.into_inner().unwrap()[0].kind,
        ExactTraceEventKind::Loss { reason } if reason == "forced coverage cleanup"
    ));

    let handle = TracedChild {
        pid: missing_pid,
        shared: Arc::new(shared),
        stdin: None,
        stdout: None,
        stderr: None,
    };
    assert_eq!(
        handle.try_wait_code().unwrap_err().to_string(),
        "forced coverage cleanup"
    );
}

#[test]
fn exact_trace_round_trip_exposes_stdio_exit_and_idempotent_kill() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut command = std::process::Command::new("/bin/sh");
    command
        .args([
            "-c",
            "printf trace-out; printf trace-err >&2; /bin/sh -c 'exit 0' & wait; exit 7",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let event_sink = Arc::clone(&events);
    let complete_sink = Arc::clone(&completed);
    let mut child = start_exact_trace(
        command,
        Box::new(move |event| event_sink.lock().unwrap().push(event)),
        Box::new(move || {
            complete_sink.store(true, std::sync::atomic::Ordering::Release);
        }),
    )
    .expect("start exact trace");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    assert_ne!(child.id(), 0);
    drop(child.take_stdin());
    let exit_code = loop {
        let code = child.try_wait_code().expect("trace state");
        if let Some(code) = code.filter(|_| {
            completed.load(std::sync::atomic::Ordering::Acquire)
        }) {
            break code;
        }
        assert!(std::time::Instant::now() < deadline, "trace did not complete");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(exit_code, 7);

    let mut stdout = child.take_stdout().expect("stdout");
    let mut stderr = child.take_stderr().expect("stderr");
    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    stdout.read_to_string(&mut stdout_text).unwrap();
    stderr.read_to_string(&mut stderr_text).unwrap();
    assert_eq!(stdout_text, "trace-out");
    assert_eq!(stderr_text, "trace-err");
    child.kill().expect("already exited is harmless");
    assert!(events
        .lock()
        .unwrap()
        .iter()
        .any(|event| matches!(event.kind, ExactTraceEventKind::Exec)));
}

#[test]
fn exact_trace_can_kill_a_running_root() {
    let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let completed_sink = Arc::clone(&completed);
    let mut command = std::process::Command::new("/bin/sleep");
    command.arg("30");
    let mut child = TracedChildCleanup(start_exact_trace(
        command,
        Box::new(|_| {}),
        Box::new(move || {
            completed_sink.store(true, std::sync::atomic::Ordering::Release);
        }),
    )
    .expect("start killable exact trace"));
    child.kill().expect("kill traced root");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(code) = child.try_wait_code().expect("trace state") {
            assert_eq!(code, -9);
            break;
        }
        assert!(std::time::Instant::now() < deadline, "trace kill did not complete");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(completed.load(std::sync::atomic::Ordering::Acquire));
}

#[test]
fn failed_setup_cleanup_terminates_and_reaps_the_child() {
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 30"])
        .spawn()
        .expect("spawn child");
    cleanup_failed_setup(&mut child);
    assert!(child.try_wait().unwrap().is_some());
}
