//! Launch-time Linux `ptrace` process-tree supervision.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::process::Child;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::platform::process::{ExactTraceEvent, ExactTraceEventKind, TraceOriginArtifact};

const STACK_CAPTURE_BYTES: usize = 16 * 1024;
const MODULE_MAP_CAPTURE_BYTES: usize = 256 * 1024;

// libc follows the host C ABI here: glibc declares ptrace's request as an
// unsigned enum, while musl declares it as c_int. Keeping the wrapper's
// parameter identical to libc also keeps every PTRACE_* constant type-correct.
#[cfg(target_env = "musl")]
type PtraceRequest = libc::c_int;
#[cfg(not(target_env = "musl"))]
type PtraceRequest = libc::c_uint;

/// Arrange for a successful `exec` to stop the child before user code runs.
/// No pre-exec SIGSTOP is used: that would deadlock `Command::spawn`'s exec
/// error pipe.
pub fn configure_exact_trace(command: &mut std::process::Command) -> io::Result<()> {
    // SAFETY: this closure runs in the single-threaded post-fork child before
    // exec. PTRACE_TRACEME takes only scalar/null arguments and reports errors
    // through errno; it does not retain borrowed Rust memory.
    unsafe {
        command.pre_exec(|| {
            let result = libc::ptrace(
                libc::PTRACE_TRACEME,
                0,
                std::ptr::null_mut::<libc::c_void>(),
                std::ptr::null_mut::<libc::c_void>(),
            );
            if result == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

#[derive(Default)]
struct RootState {
    exit_code: Option<i32>,
    done: bool,
    tracer_error: Option<String>,
}

struct Shared {
    state: Mutex<RootState>,
    wake: Condvar,
}

/// Root-process control handle whose wait state is owned by the tracer.
pub struct TracedChild {
    pid: u32,
    shared: Arc<Shared>,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
}

impl TracedChild {
    pub fn id(&self) -> u32 {
        self.pid
    }

    pub fn try_wait_code(&self) -> io::Result<Option<i32>> {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(error) = state.tracer_error.as_ref() {
            return Err(io::Error::other(error.clone()));
        }
        Ok(state.exit_code)
    }

    pub fn kill(&mut self) -> io::Result<()> {
        // SAFETY: `pid` is the positive PID returned by `Command::spawn`; no
        // pointer arguments are involved and ESRCH is handled as already gone.
        if unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGKILL) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.stderr.take()
    }
}

pub fn start_exact_trace(
    mut command: std::process::Command,
    emit: Box<dyn Fn(ExactTraceEvent) + Send>,
    complete: Box<dyn FnOnce() + Send>,
) -> io::Result<TracedChild> {
    configure_exact_trace(&mut command)?;
    let shared = Arc::new(Shared {
        state: Mutex::new(RootState::default()),
        wake: Condvar::new(),
    });
    let thread_shared = Arc::clone(&shared);
    let (setup_tx, setup_rx) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("rp-linux-ptrace".to_owned())
        .spawn(move || {
            let child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    let _ = setup_tx.send(Err(error));
                    return;
                }
            };
            trace_loop(child, thread_shared, emit, complete, setup_tx);
        });
    if let Err(error) = spawned {
        return Err(io::Error::other(format!(
            "spawn ptrace supervisor: {error}"
        )));
    }
    let setup = setup_rx
        .recv()
        .map_err(|_| io::Error::other("ptrace supervisor ended during launch setup"))??;
    Ok(TracedChild {
        pid: setup.pid,
        shared,
        stdin: setup.stdin,
        stdout: setup.stdout,
        stderr: setup.stderr,
    })
}

struct TraceSetup {
    pid: u32,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
}

#[derive(Clone)]
struct Tracee {
    parent_pid: Option<u32>,
    parent_start_key: Option<u64>,
    start_key: Option<u64>,
    process_leader: bool,
    executable: Option<std::path::PathBuf>,
    argv: Option<Vec<OsString>>,
    origin: Option<TraceOriginArtifact>,
}

#[expect(
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
fn trace_loop(
    mut child: Child,
    shared: Arc<Shared>,
    emit: Box<dyn Fn(ExactTraceEvent) + Send>,
    complete: Box<dyn FnOnce() + Send>,
    setup: std::sync::mpsc::SyncSender<io::Result<TraceSetup>>,
) {
    let root_pid = child.id();
    let mut initial_status = 0;
    // SAFETY: `root_pid` was spawned by this same supervisor task after
    // PTRACE_TRACEME, and `initial_status` is valid writable storage.
    if unsafe { libc::waitpid(root_pid as libc::pid_t, &mut initial_status, libc::__WALL) } == -1
        || !libc::WIFSTOPPED(initial_status)
    {
        let message = "root did not reach its initial ptrace exec stop";
        let _ = setup.send(Err(io::Error::other(message)));
        cleanup_failed_setup(&mut child);
        return;
    }

    let options = libc::PTRACE_O_TRACEFORK
        | libc::PTRACE_O_TRACEVFORK
        | libc::PTRACE_O_TRACECLONE
        | libc::PTRACE_O_TRACEEXEC
        | libc::PTRACE_O_TRACEEXIT;
    if ptrace_value(libc::PTRACE_SETOPTIONS, root_pid, 0, options as usize).is_err() {
        let message = "PTRACE_SETOPTIONS was denied";
        let _ = setup.send(Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            message,
        )));
        cleanup_failed_setup(&mut child);
        return;
    }

    let mut sequence = 1u64;
    let mut tracees = HashMap::from([(
        root_pid,
        Tracee {
            parent_pid: None,
            parent_start_key: None,
            start_key: process_start_key(root_pid),
            process_leader: true,
            executable: read_executable(root_pid),
            argv: read_argv(root_pid),
            origin: None,
        },
    )]);
    if ptrace_value(libc::PTRACE_CONT, root_pid, 0, 0).is_err() {
        let message = "failed to continue root after initial exec stop";
        let _ = setup.send(Err(io::Error::other(message)));
        detach_all(tracees.keys().copied());
        cleanup_failed_setup(&mut child);
        return;
    }
    let setup_result = TraceSetup {
        pid: root_pid,
        stdin: child.stdin.take(),
        stdout: child.stdout.take(),
        stderr: child.stderr.take(),
    };
    if setup.send(Ok(setup_result)).is_err() {
        cleanup_failed_setup(&mut child);
        return;
    }

    while !tracees.is_empty() {
        let pids: Vec<u32> = tracees.keys().copied().collect();
        let mut progressed = false;
        for pid in pids {
            let mut status = 0;
            // SAFETY: `status` is valid writable storage and `pid` is a
            // tracee owned by this supervisor thread. `WNOHANG` keeps the
            // event pump responsive across the complete tracee set.
            let waited = unsafe {
                libc::waitpid(
                    pid as libc::pid_t,
                    &mut status,
                    libc::WNOHANG | libc::__WALL,
                )
            };
            if waited == 0 {
                continue;
            }
            if waited == -1 {
                let error = io::Error::last_os_error();
                let tracee = tracees.get(&pid).cloned();
                if error.raw_os_error() == Some(libc::ECHILD)
                    && tracee.as_ref().is_some_and(|item| !item.process_leader)
                {
                    // A non-leader thread can disappear with its process
                    // leader and leave a stale TID in the local set. Losing
                    // wait ownership for a process leader is different: it
                    // destroys exit coverage (and for the root would leave
                    // TracedChild waiting forever), so that is fatal below.
                    tracees.remove(&pid);
                    continue;
                }
                abort_runtime_trace(
                    &mut child,
                    &shared,
                    &emit,
                    sequence,
                    pid,
                    tracee.as_ref(),
                    &tracees,
                    format!("ptrace wait ownership lost: {error}"),
                );
                complete();
                return;
            }
            progressed = true;
            if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                if let Some(tracee) = tracees.remove(&pid) {
                    if tracee.process_leader {
                        let exit_code = libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status));
                        let signal = libc::WIFSIGNALED(status).then(|| libc::WTERMSIG(status));
                        if pid == root_pid {
                            let normalized = exit_code.unwrap_or_else(|| -signal.unwrap_or(0));
                            finish_root(&shared, normalized);
                        } else {
                            emit(event_for(
                                sequence,
                                pid,
                                &tracee,
                                ExactTraceEventKind::Exit {
                                    exit_code,
                                    signal,
                                    raw_status: i64::from(status),
                                },
                            ));
                            sequence += 1;
                        }
                    }
                }
                continue;
            }
            if !libc::WIFSTOPPED(status) {
                continue;
            }

            let signal = libc::WSTOPSIG(status);
            let ptrace_event = status >> 16;
            match ptrace_event {
                libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK | libc::PTRACE_EVENT_CLONE => {
                    let mut child_pid = 0usize;
                    match ptrace_value(
                        libc::PTRACE_GETEVENTMSG,
                        pid,
                        0,
                        (&raw mut child_pid) as usize,
                    ) {
                        Ok(_) => {
                        let child_pid = child_pid as u32;
                        let process_leader = if matches!(
                            ptrace_event,
                            libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK
                        ) {
                            true
                        } else {
                            match thread_group_id(child_pid) {
                                Some(thread_group) => thread_group == child_pid,
                                None => {
                                    // The auto-attached child is stopped but
                                    // cannot safely be classified as a thread
                                    // or process. Include it in fatal cleanup;
                                    // defaulting to a thread would later hide
                                    // process-leader ECHILD as a stale TID.
                                    tracees.insert(
                                        child_pid,
                                        Tracee {
                                            parent_pid: None,
                                            parent_start_key: None,
                                            start_key: process_start_key(child_pid),
                                            process_leader: true,
                                            executable: None,
                                            argv: None,
                                            origin: None,
                                        },
                                    );
                                    abort_runtime_trace(
                                        &mut child,
                                        &shared,
                                        &emit,
                                        sequence,
                                        child_pid,
                                        tracees.get(&child_pid),
                                        &tracees,
                                        "cannot classify PTRACE_EVENT_CLONE child".to_owned(),
                                    );
                                    complete();
                                    return;
                                }
                            }
                        };
                        let parent_pid =
                            process_leader.then(|| thread_group_id(pid).unwrap_or(pid));
                        let tracee = Tracee {
                            parent_pid,
                            parent_start_key: parent_pid.and_then(process_start_key),
                            start_key: process_start_key(child_pid),
                            process_leader,
                            executable: None,
                            argv: None,
                            origin: process_leader.then(|| capture_origin(pid)),
                        };
                        let spawn_event = process_leader.then(|| {
                            event_for(
                                sequence,
                                child_pid,
                                &tracee,
                                ExactTraceEventKind::Spawn,
                            )
                        });
                        tracees.insert(child_pid, tracee);
                        if let Err(error) = ptrace_value(libc::PTRACE_CONT, pid, 0, 0) {
                            abort_runtime_trace(
                                &mut child,
                                &shared,
                                &emit,
                                sequence,
                                pid,
                                tracees.get(&pid),
                                &tracees,
                                format!("continue spawning tracee: {error}"),
                            );
                            complete();
                            return;
                        }
                        let mut child_status = 0;
                        // SAFETY: the kernel has just reported this auto-attached
                        // child and guarantees an initial ptrace stop. This
                        // supervisor thread is its tracer and owns the wait.
                        let waited = unsafe {
                            libc::waitpid(
                                child_pid as libc::pid_t,
                                &mut child_status,
                                libc::__WALL,
                            )
                        };
                        if waited != child_pid as libc::pid_t
                            || !libc::WIFSTOPPED(child_status)
                        {
                            abort_runtime_trace(
                                &mut child,
                                &shared,
                                &emit,
                                sequence,
                                child_pid,
                                tracees.get(&child_pid),
                                &tracees,
                                "new tracee did not reach its initial stop".to_owned(),
                            );
                            complete();
                            return;
                        }
                        if let Err(error) = ptrace_value(libc::PTRACE_CONT, child_pid, 0, 0) {
                            abort_runtime_trace(
                                &mut child,
                                &shared,
                                &emit,
                                sequence,
                                child_pid,
                                tracees.get(&child_pid),
                                &tracees,
                                format!("continue new tracee: {error}"),
                            );
                            complete();
                            return;
                        }
                        // Both stopped tasks are running before delivery can do
                        // any file I/O or deferred-symbolization queueing.
                        if let Some(spawn_event) = spawn_event {
                            emit(spawn_event);
                            sequence += 1;
                        }
                        }
                        Err(error) => {
                            abort_runtime_trace(
                                &mut child,
                                &shared,
                                &emit,
                                sequence,
                                pid,
                                tracees.get(&pid),
                                &tracees,
                                format!("read fork/clone event child pid: {error}"),
                            );
                            complete();
                            return;
                        }
                    }
                }
                libc::PTRACE_EVENT_EXEC => {
                    let exec_event = if let Some(tracee) = tracees.get_mut(&pid) {
                        tracee.executable = read_executable(pid);
                        tracee.argv = read_argv(pid);
                        tracee
                            .process_leader
                            .then(|| event_for(sequence, pid, tracee, ExactTraceEventKind::Exec))
                    } else {
                        None
                    };
                    if let Err(error) = ptrace_value(libc::PTRACE_CONT, pid, 0, 0) {
                        abort_runtime_trace(
                            &mut child,
                            &shared,
                            &emit,
                            sequence,
                            pid,
                            tracees.get(&pid),
                            &tracees,
                            format!("continue after exec event: {error}"),
                        );
                        complete();
                        return;
                    }
                    if let Some(exec_event) = exec_event {
                        emit(exec_event);
                        sequence += 1;
                    }
                }
                libc::PTRACE_EVENT_EXIT => {
                    if let Err(error) = ptrace_value(libc::PTRACE_CONT, pid, 0, 0) {
                        abort_runtime_trace(
                            &mut child,
                            &shared,
                            &emit,
                            sequence,
                            pid,
                            tracees.get(&pid),
                            &tracees,
                            format!("continue after exit event: {error}"),
                        );
                        complete();
                        return;
                    }
                }
                _ => {
                    let forwarded = if signal == libc::SIGTRAP {
                        0
                    } else {
                        signal
                    };
                    if let Err(error) =
                        ptrace_value(libc::PTRACE_CONT, pid, 0, forwarded as usize)
                    {
                        abort_runtime_trace(
                            &mut child,
                            &shared,
                            &emit,
                            sequence,
                            pid,
                            tracees.get(&pid),
                            &tracees,
                            format!("continue after signal stop: {error}"),
                        );
                        complete();
                        return;
                    }
                }
            }
        }
        if !progressed {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    drop(child);
    complete();
}

fn ptrace_value(
    request: PtraceRequest,
    pid: u32,
    address: usize,
    data: usize,
) -> io::Result<libc::c_long> {
    // SAFETY: callers provide a live tracee owned by this supervisor. Pointer
    // values are either null, kernel-defined scalar payloads, or addresses of
    // writable storage whose lifetime covers this synchronous syscall.
    let value = unsafe {
        libc::ptrace(
            request,
            pid as libc::pid_t,
            address as *mut libc::c_void,
            data as *mut libc::c_void,
        )
    };
    if value == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}

fn emit_loss(
    emit: &dyn Fn(ExactTraceEvent),
    sequence: u64,
    pid: u32,
    tracee: Option<&Tracee>,
    reason: String,
) {
    emit(ExactTraceEvent {
        sequence,
        pid,
        parent_pid: tracee.and_then(|tracee| tracee.parent_pid),
        parent_start_key: tracee.and_then(|tracee| tracee.parent_start_key),
        start_key: tracee.and_then(|tracee| tracee.start_key),
        timestamp: std::time::SystemTime::now(),
        kind: ExactTraceEventKind::Loss { reason },
        executable: None,
        argv: None,
        origin: None,
    });
}

#[allow(clippy::too_many_arguments)]
fn abort_runtime_trace(
    child: &mut Child,
    shared: &Shared,
    emit: &dyn Fn(ExactTraceEvent),
    sequence: u64,
    pid: u32,
    tracee: Option<&Tracee>,
    tracees: &HashMap<u32, Tracee>,
    reason: String,
) {
    emit_loss(emit, sequence, pid, tracee, reason.clone());
    for tracee_pid in tracees.keys().copied().filter(|item| *item != child.id()) {
        // SAFETY: each value is a positive kernel-reported tracee PID. Fatal
        // supervision failure chooses deterministic termination over leaving
        // an unserviced ptrace stop that can wedge indefinitely.
        unsafe {
            libc::kill(tracee_pid as libc::pid_t, libc::SIGKILL);
            libc::waitpid(tracee_pid as libc::pid_t, std::ptr::null_mut(), libc::__WALL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let mut state = shared.state.lock().unwrap_or_else(|error| error.into_inner());
    state.tracer_error = Some(reason);
    state.done = true;
    shared.wake.notify_all();
}

fn capture_origin(thread_id: u32) -> TraceOriginArtifact {
    let origin_pid = thread_group_id(thread_id).unwrap_or(thread_id);
    let mut registers = vec![0u8; 1024];
    let mut iov = libc::iovec {
        iov_base: registers.as_mut_ptr().cast(),
        iov_len: registers.len(),
    };
    if ptrace_value(
        libc::PTRACE_GETREGSET,
        thread_id,
        libc::NT_PRSTATUS as usize,
        (&raw mut iov) as usize,
    )
    .is_ok()
    {
        registers.truncate(iov.iov_len);
    } else {
        registers.clear();
    }
    let (stack_pointer, instruction_pointer) = read_syscall_pointers(thread_id);
    let mut stack = vec![0u8; STACK_CAPTURE_BYTES];
    if let Some(stack_pointer) = stack_pointer {
        let local = libc::iovec {
            iov_base: stack.as_mut_ptr().cast(),
            iov_len: stack.len(),
        };
        let remote = libc::iovec {
            iov_base: stack_pointer as *mut libc::c_void,
            iov_len: stack.len(),
        };
        // SAFETY: local points to the owned `stack` allocation for its full
        // lifetime; remote is a bounded address range in the stopped tracee.
        // `process_vm_readv` copies at most the declared local length.
        let read = unsafe {
            libc::process_vm_readv(thread_id as libc::pid_t, &local, 1, &remote, 1, 0)
        };
        if read >= 0 {
            stack.truncate(read as usize);
        } else {
            stack.clear();
        }
    } else {
        stack.clear();
    }
    let mut module_map = std::fs::read(format!("/proc/{origin_pid}/maps")).unwrap_or_default();
    let module_map_truncated = module_map.len() > MODULE_MAP_CAPTURE_BYTES;
    module_map.truncate(MODULE_MAP_CAPTURE_BYTES);
    TraceOriginArtifact {
        origin_pid,
        thread_id,
        architecture: std::env::consts::ARCH.to_owned(),
        register_format: format!("linux-nt-prstatus-{}", std::env::consts::ARCH),
        executable: read_executable(origin_pid),
        registers,
        stack_pointer,
        instruction_pointer,
        truncated: stack.len() == STACK_CAPTURE_BYTES,
        stack,
        module_map,
        module_map_truncated,
    }
}

fn read_syscall_pointers(pid: u32) -> (Option<u64>, Option<u64>) {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/syscall")) else {
        return (None, None);
    };
    let fields: Vec<&str> = text.split_ascii_whitespace().collect();
    if fields.len() < 2 {
        return (None, None);
    }
    let parse = |value: &str| u64::from_str_radix(value.trim_start_matches("0x"), 16).ok();
    (parse(fields[fields.len() - 2]), parse(fields[fields.len() - 1]))
}

fn read_executable(pid: u32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

fn read_argv(pid: u32) -> Option<Vec<OsString>> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        bytes
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| OsString::from_vec(part.to_vec()))
            .collect(),
    )
}

fn process_start_key(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.get(stat.rfind(')')? + 1..)?
        .split_ascii_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

fn thread_group_id(pid: u32) -> Option<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("Tgid:")?.trim().parse().ok())
}

fn event_for(
    sequence: u64,
    pid: u32,
    tracee: &Tracee,
    kind: ExactTraceEventKind,
) -> ExactTraceEvent {
    ExactTraceEvent {
        sequence,
        pid,
        parent_pid: tracee.parent_pid,
        parent_start_key: tracee.parent_start_key,
        start_key: tracee.start_key,
        timestamp: std::time::SystemTime::now(),
        kind,
        executable: tracee.executable.clone(),
        argv: tracee.argv.clone(),
        origin: tracee.origin.clone(),
    }
}

fn detach_all(tracees: impl IntoIterator<Item = u32>) {
    for pid in tracees {
        let _ = ptrace_value(libc::PTRACE_DETACH, pid, 0, 0);
    }
}

fn cleanup_failed_setup(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn finish_root(shared: &Shared, exit_code: i32) {
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.exit_code = Some(exit_code);
    state.done = true;
    shared.wake.notify_all();
}

#[cfg(test)]
#[path = "tests/platform_linux_trace_coverage.rs"]
mod coverage_tests;
