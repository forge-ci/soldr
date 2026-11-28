//! Private bridge from facade process semantics to the native substrate.
//!
//! # Error mapping
//!
//! Every lifecycle and spawn operation surfaces [`io::Error`]; the substrate's
//! error enum never crosses this boundary. [`process_error_to_io`] is the one
//! mapping, and it is stable:
//!
//! | Substrate failure                      | `io::ErrorKind`                  |
//! |----------------------------------------|----------------------------------|
//! | spawn or I/O failure carrying an error | that error, unchanged (kind and raw OS code preserved) |
//! | started twice                          | `AlreadyExists`                  |
//! | child no longer running                | `BrokenPipe`                     |
//! | stdin not piped or already closed      | `BrokenPipe`                     |
//! | no usable async runtime context        | `Other`                          |
//! | deadline elapsed                       | `TimedOut`                       |
//! | retained output exceeded its limit     | `FileTooLarge`                   |
//!
//! Bounded capture and bounded runs keep their typed errors
//! ([`ProcessCaptureError`], [`BoundedProcessError`]) for the two outcomes a
//! caller acts on, and use the same mapping for the rest.

use std::io;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use running_process::{
    AsyncProcess, AsyncProcessBuilder, AsyncProcessSessionControl, AsyncProcessSessionEvent,
    AsyncProcessSessionOptions, AsyncProcessSessionOutput, AsyncStdio, BoundedRunOptions,
    ProcessError, StreamKind,
};

use crate::platform::process::{
    lifetime_enforcement_for, LifetimeEnforcement, LifetimeOwner, ProcessInspectError,
    ProcessInspectErrorKind,
};
use crate::{
    BoundedProcessError, BoundedProcessOutput, PlatformChild, PriorityPolicy, ProcessCaptureError,
    ProcessExit, ProcessExitWatch, ProcessOutput, ProcessOutputChunk, ProcessOutputCompletion,
    ProcessOutputEvent, ProcessOutputFault, ProcessPostExitDrain, ProcessPriority, ProcessSession,
    ProcessSessionExit, ProcessSessionOptions, SpawnSpec, StreamMode,
};

/// Private child state from the selected native substrate.
pub(crate) struct ProcessAdapter {
    // Shared only with the owner watcher, which holds a `Weak` so it can never
    // extend the child's lifetime past this handle.
    process: Arc<AsyncProcess>,
    pid: Option<u32>,
}

/// Private state for the facade-owned streaming session.
pub(crate) struct ProcessSessionAdapter {
    // The native split keeps this lane as the sole terminal owner. Declaring
    // it before the receiver ensures drop activates lifecycle cleanup before
    // facade output detaches.
    control: Arc<AsyncProcessSessionControl>,
    // The mutex establishes the facade's single output consumer without
    // serializing lifecycle methods behind a pending pipe read.
    output: tokio::sync::Mutex<AsyncProcessSessionOutput>,
    pid: u32,
}

/// What a spawn obtained for the caller's requested owner binding.
///
/// The watcher handle lives here so that it is cancelled by the handle that
/// owns it: at the terminal answer that leaves nothing to protect, and at the
/// latest when that handle is dropped. A watch nobody can act on any more is
/// a pidfd and a task kept alive for nothing.
pub(crate) struct LifetimeBinding {
    enforcement: Option<LifetimeEnforcement>,
    /// Taken rather than read. Dropping a `Task` cancels it, which is exactly
    /// how the watch stops when the handle that could have acted on it does.
    /// The lock is what lets a session release it: a session reaches the same
    /// terminal answer as a child does, but holds only `&self` while doing it.
    watcher: Mutex<Option<crate::async_engine::Task<()>>>,
}

impl LifetimeBinding {
    pub(crate) const fn unbound() -> Self {
        Self {
            enforcement: None,
            watcher: Mutex::new(None),
        }
    }

    pub(crate) fn enforcement(&self) -> Option<LifetimeEnforcement> {
        self.enforcement
    }

    /// Stop watching, because the child this was protecting has exited.
    ///
    /// Idempotent, and safe to call while another caller is inside a
    /// lifecycle method: the handle is taken out under the lock and cancelled
    /// after it, so a second release finds nothing left to take and no drop
    /// runs with the lock held. A poisoned lock is not a broken invariant
    /// here -- an `Option` is either holding the watch or not -- so releasing
    /// still releases.
    ///
    /// The enforcement report is left alone: it records what the spawn
    /// obtained, which does not stop being true once the child is gone.
    pub(crate) fn release(&self) {
        drop(self.take_watcher());
    }

    fn take_watcher(&self) -> Option<crate::async_engine::Task<()>> {
        self.watcher
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    /// Whether the owner watch is still held.
    #[cfg(test)]
    pub(crate) fn is_watching(&self) -> bool {
        self.watcher
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
    }
}

pub(crate) async fn spawn(spec: SpawnSpec) -> io::Result<PlatformChild> {
    let owner = spec.lifetime_owner;
    // Resolved before the child exists: an owner that has already exited must
    // fail the spawn rather than leave a child with nothing watching it.
    let watch = open_owner_watch(owner)?;
    let fallback = priority_fallback(&spec);
    let mut process = builder(spec).build();
    if let Err(error) = process.start().await {
        match fallback {
            Some(fallback) if priority_refused(&error) => {
                process = builder(fallback).build();
                process.start().await.map_err(process_error_to_io)?;
            }
            _ => return Err(process_error_to_io(error)),
        }
    }
    let pid = process.pid().await.map_err(process_error_to_io)?;
    let process = Arc::new(process);
    let lifetime = bind_lifetime(owner, watch, OwnedChild::Process(Arc::downgrade(&process)));
    Ok(PlatformChild::new(
        ProcessAdapter {
            process,
            pid: Some(pid),
        },
        lifetime,
    ))
}

/// The spec to retry with when a best-effort band is refused, if one could be.
///
/// Only a best-effort request that actually asks the host for a band has
/// something to drop; everything else fails exactly as the first attempt did.
fn priority_fallback(spec: &SpawnSpec) -> Option<SpawnSpec> {
    let droppable = spec.priority_policy == PriorityPolicy::BestEffort
        && spec.priority.substrate_nice().is_some();
    droppable.then(|| {
        let mut fallback = spec.clone();
        fallback.priority = ProcessPriority::Normal;
        fallback.priority_policy = PriorityPolicy::Required;
        fallback
    })
}

/// Whether a spawn failure is the host refusing a scheduling band.
///
/// Unix applies the band in the child before exec, so a refusal arrives as the
/// spawn's own `PermissionDenied`. An executable that is itself not permitted
/// reports the same kind; the one retry then fails the same way and that
/// second error is returned.
fn priority_refused(error: &ProcessError) -> bool {
    matches!(error, ProcessError::Spawn(error) if error.kind() == io::ErrorKind::PermissionDenied)
}

/// Take the owner's exit subscription before anything is spawned.
fn open_owner_watch(owner: Option<LifetimeOwner>) -> io::Result<Option<ProcessExitWatch>> {
    let Some(LifetimeOwner::Process(owner)) = owner else {
        return Ok(None);
    };
    ProcessExitWatch::open(owner)
        .map(Some)
        .map_err(|error| io::Error::new(inspect_error_kind(&error), error))
}

fn inspect_error_kind(error: &ProcessInspectError) -> io::ErrorKind {
    match error.kind {
        ProcessInspectErrorKind::InvalidPid => io::ErrorKind::InvalidInput,
        ProcessInspectErrorKind::NotFound => io::ErrorKind::NotFound,
        ProcessInspectErrorKind::Unsupported => io::ErrorKind::Unsupported,
        ProcessInspectErrorKind::Host => io::ErrorKind::Other,
    }
}

/// Record what the spawn obtained, and start the watcher where that is what
/// the binding is.
fn bind_lifetime(
    owner: Option<LifetimeOwner>,
    watch: Option<ProcessExitWatch>,
    child: OwnedChild,
) -> LifetimeBinding {
    let Some(owner) = owner else {
        return LifetimeBinding::unbound();
    };
    LifetimeBinding {
        enforcement: Some(lifetime_enforcement_for(owner)),
        watcher: Mutex::new(watch.map(|watch| watch_owner(watch, child))),
    }
}

/// The spawner's own handle to the child the watcher protects.
///
/// Killing through the handle that will reap the child is exact on every
/// host: the PID cannot be reused while its parent has not reaped it, and
/// the substrate knows whether it has. A PID-addressed kill needs a host
/// primitive that pins the generation, which macOS does not provide (#347).
enum OwnedChild {
    Process(Weak<AsyncProcess>),
    Session(Weak<AsyncProcessSessionControl>),
}

impl OwnedChild {
    async fn kill(&self) -> Result<(), ProcessError> {
        match self {
            Self::Process(process) => match process.upgrade() {
                Some(process) => process.kill().await,
                None => Ok(()),
            },
            Self::Session(control) => match control.upgrade() {
                Some(control) => control.kill().await,
                None => Ok(()),
            },
        }
    }
}

/// Terminate exactly this child when exactly that owner exits.
///
/// Both halves are pinned rather than re-resolved: the watch names the owner
/// process, and the kill goes through the spawner's own child handle, so
/// neither number being handed to someone else in between can redirect it. A
/// dropped handle already ran its own cleanup, and a child that exited first
/// leaves the kill with nothing to do.
fn watch_owner(watch: ProcessExitWatch, child: OwnedChild) -> crate::async_engine::Task<()> {
    crate::async_engine::launch(async move {
        // An error here is "the host stopped answering", not "the owner
        // died"; killing the child on it would reap for the wrong reason.
        if watch.exited().await.is_err() {
            return;
        }
        let _ = child.kill().await;
    })
}

/// Start the substrate's one-owner session and retain only facade-owned
/// lifecycle, output, and native-exit semantic types above this boundary.
pub(crate) async fn spawn_session(
    spec: SpawnSpec,
    options: ProcessSessionOptions,
) -> io::Result<ProcessSession> {
    let owner = spec.lifetime_owner;
    let watch = open_owner_watch(owner)?;
    let fallback = priority_fallback(&spec);
    let mut session = builder(spec).session(session_options(options));
    if let Err(error) = session.start().await {
        match fallback {
            Some(fallback) if priority_refused(&error) => {
                session = builder(fallback).session(session_options(options));
                session.start().await.map_err(process_error_to_io)?;
            }
            _ => return Err(process_error_to_io(error)),
        }
    }
    let (control, output) = session.into_parts().map_err(process_error_to_io)?;
    let pid = control.pid();
    let control = Arc::new(control);
    let lifetime = bind_lifetime(owner, watch, OwnedChild::Session(Arc::downgrade(&control)));
    Ok(ProcessSession::new(
        ProcessSessionAdapter {
            control,
            output: tokio::sync::Mutex::new(output),
            pid,
        },
        lifetime,
    ))
}

/// Run one short-lived command with contained, bounded capture.
///
/// The command conversion consumes program, argument, working-directory, and
/// environment fields without UTF-8 conversion. The bounded operation
/// intentionally owns stdio and containment: its native substrate forces null
/// stdin, separate piped stdout and stderr, and a child process group for
/// reliable timeout/overflow cleanup. Owner-death is forwarded to the same
/// native pre-exec containment path as the substrate's bounded runner.
pub(crate) fn run_bounded(
    spec: SpawnSpec,
    timeout: Option<Duration>,
    output_limit: usize,
) -> Result<BoundedProcessOutput, BoundedProcessError> {
    // A bounded run means a deadline: `None` is kept in the signature only so
    // existing `Some(budget)` callers compile, and is refused before any
    // native effect rather than waiting forever on a silent child (#433).
    if timeout.is_none() {
        return Err(BoundedProcessError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a bounded run requires a deadline",
        )));
    }
    // A bounded run is synchronous and owns no place to park a watcher task,
    // so the one owner it cannot serve is refused instead of being quietly
    // downgraded to the spawner it was not asked to bind to.
    if matches!(spec.lifetime_owner, Some(LifetimeOwner::Process(_))) {
        return Err(BoundedProcessError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a bounded run binds only to its spawner; use SpawnSpec::spawn for another owner",
        )));
    }
    // The substrate's bounded runner spawns internally with no admission
    // hook. Holding the permit around the whole run would turn a spawn-only
    // exclusion into a lifetime one, so the request is refused instead.
    if spec.admission.is_some() {
        return Err(BoundedProcessError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a bounded run cannot hold a spawn admission; use SpawnSpec::spawn_session",
        )));
    }
    let fallback = priority_fallback(&spec);
    match run_bounded_once(&spec, timeout, output_limit) {
        Err(error) if priority_refused(&error) => match fallback {
            Some(fallback) => run_bounded_once(&fallback, timeout, output_limit),
            None => Err(error),
        },
        result => result,
    }
    .map(|output| BoundedProcessOutput {
        exit: ProcessExit::new(output.exit_code),
        stdout: output.stdout,
        stderr: output.stderr,
    })
    .map_err(bounded_process_error)
}

fn run_bounded_once(
    spec: &SpawnSpec,
    timeout: Option<Duration>,
    output_limit: usize,
) -> Result<running_process::RunOutput, ProcessError> {
    let (command, kill_when_owner_dies, priority) = std_command(spec);
    running_process::run_std_command_bounded_with_options(
        command,
        timeout,
        output_limit,
        BoundedRunOptions::default()
            .kill_when_owner_dies(kill_when_owner_dies)
            .nice(priority.substrate_nice()),
    )
}

impl ProcessAdapter {
    pub(crate) fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.process.wait().await.map_err(process_error_to_io)?;
        self.pid = None;
        Ok(status)
    }

    pub(crate) async fn kill(&mut self) -> io::Result<()> {
        self.process.kill().await.map_err(process_error_to_io)?;
        self.pid = None;
        Ok(())
    }

    pub(crate) async fn terminate_group_soft(&self) -> io::Result<bool> {
        self.process
            .terminate_group_soft()
            .await
            .map_err(process_error_to_io)
    }

    pub(crate) async fn write_stdin(&self, bytes: &[u8]) -> io::Result<()> {
        self.process
            .write_stdin(bytes)
            .await
            .map_err(process_error_to_io)
    }

    pub(crate) async fn close_stdin(&self) -> io::Result<()> {
        self.process
            .close_stdin()
            .await
            .map_err(process_error_to_io)
    }

    pub(crate) async fn capture_bounded(
        &mut self,
        limit: usize,
    ) -> Result<ProcessOutput, ProcessCaptureError> {
        let result = self
            .process
            .capture_bounded(limit)
            .await
            .map_err(process_capture_error)
            .map(|output| ProcessOutput {
                status: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
            });
        if matches!(
            &result,
            Ok(_) | Err(ProcessCaptureError::OutputLimitExceeded { .. })
        ) {
            self.pid = None;
        }
        result
    }
}

impl ProcessSessionAdapter {
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) async fn next_output(&self) -> Option<ProcessOutputEvent> {
        let mut output = self.output.lock().await;
        output.next_output().await.map(output_event)
    }

    pub(crate) async fn shutdown_output(&self) -> io::Result<()> {
        // A parked next_output owns this mutex. Wake its producer before
        // waiting for the lane; acquiring the mutex first would deadlock.
        self.control.request_output_shutdown();
        let mut output = self.output.lock().await;
        output.shutdown().await.map_err(process_error_to_io)
    }

    pub(crate) async fn wait(&self) -> io::Result<ProcessSessionExit> {
        self.control
            .wait()
            .await
            .map(native_exit)
            .map_err(process_error_to_io)
    }

    pub(crate) async fn poll(&self) -> io::Result<Option<ProcessSessionExit>> {
        self.control
            .poll()
            .await
            .map(|status| status.map(native_exit))
            .map_err(process_error_to_io)
    }

    pub(crate) async fn kill(&self) -> io::Result<()> {
        self.control.kill().await.map_err(process_error_to_io)
    }

    pub(crate) async fn terminate_group_soft(&self) -> io::Result<bool> {
        self.control
            .terminate_group_soft()
            .await
            .map_err(process_error_to_io)
    }

    pub(crate) async fn write_stdin(&self, bytes: &[u8]) -> io::Result<()> {
        self.control
            .write_stdin(bytes)
            .await
            .map_err(process_error_to_io)
    }

    pub(crate) async fn close_stdin(&self) -> io::Result<()> {
        self.control
            .close_stdin()
            .await
            .map_err(process_error_to_io)
    }

    pub(crate) async fn cpu_time(&self) -> io::Result<Option<Duration>> {
        self.control.cpu_time().await.map_err(process_error_to_io)
    }
}

fn builder(spec: SpawnSpec) -> AsyncProcessBuilder {
    let SpawnSpec {
        program,
        args,
        current_dir,
        env,
        clear_env,
        stdin,
        stdout,
        stderr,
        create_process_group,
        lifetime_owner,
        priority,
        priority_policy: _,
        admission,
    } = spec;
    let kill_when_owner_dies = binds_to_spawner(lifetime_owner);
    let mut builder = AsyncProcessBuilder::new(program).args(args);
    if let Some(admission) = admission {
        builder = builder.spawn_admission(admission.into_inner());
    }
    if let Some(current_dir) = current_dir {
        builder = builder.current_dir(current_dir);
    }
    if clear_env {
        builder = builder.clear_env(true);
    }
    for (key, value) in env {
        builder = builder.env(key, value);
    }
    builder
        .stdin(stdio(stdin))
        .stdout(stdio(stdout))
        .stderr(stdio(stderr))
        .create_process_group(create_process_group)
        .kill_when_owner_dies(kill_when_owner_dies)
        .nice(priority.substrate_nice())
}

/// Whether the substrate's own pre-exec containment is the binding asked for.
///
/// It is the only owner the substrate can express, and the only one a kernel
/// mechanism exists for.
const fn binds_to_spawner(owner: Option<LifetimeOwner>) -> bool {
    matches!(owner, Some(LifetimeOwner::Spawner))
}

// Maps the deprecated `WaitForEof` until it is removed (#433).
#[allow(deprecated)]
fn session_options(options: ProcessSessionOptions) -> AsyncProcessSessionOptions {
    AsyncProcessSessionOptions {
        max_queued_chunks: options.max_queued_chunks,
        max_chunk_bytes: options.max_chunk_bytes,
        post_exit_grace: match options.post_exit_drain {
            ProcessPostExitDrain::WaitForEof => None,
            ProcessPostExitDrain::AbandonAfter(grace) => Some(grace),
        },
        kill_on_drop: options.kill_on_drop,
        kill_tree_on_drop: false,
    }
}

fn output_event(event: AsyncProcessSessionEvent) -> ProcessOutputEvent {
    match event {
        AsyncProcessSessionEvent::Chunk(chunk) => ProcessOutputEvent::Chunk(match chunk.stream {
            StreamKind::Stdout => ProcessOutputChunk::Stdout(chunk.bytes),
            StreamKind::Stderr => ProcessOutputChunk::Stderr(chunk.bytes),
        }),
        AsyncProcessSessionEvent::StreamEof(stream) => {
            ProcessOutputEvent::Completion(match stream {
                StreamKind::Stdout => ProcessOutputCompletion::StdoutEof,
                StreamKind::Stderr => ProcessOutputCompletion::StderrEof,
            })
        }
        AsyncProcessSessionEvent::StreamAbandoned(stream) => {
            ProcessOutputEvent::Completion(match stream {
                StreamKind::Stdout => ProcessOutputCompletion::StdoutAbandoned,
                StreamKind::Stderr => ProcessOutputCompletion::StderrAbandoned,
            })
        }
        AsyncProcessSessionEvent::StreamError {
            stream,
            kind,
            message,
            raw_os_error,
        } => {
            let fault = ProcessOutputFault::new(kind, message, raw_os_error);
            ProcessOutputEvent::Completion(match stream {
                StreamKind::Stdout => ProcessOutputCompletion::StdoutError(fault),
                StreamKind::Stderr => ProcessOutputCompletion::StderrError(fault),
            })
        }
    }
}

#[cfg(unix)]
fn native_exit(status: ExitStatus) -> ProcessSessionExit {
    use std::os::unix::process::ExitStatusExt as _;

    let exit_code = status.code();
    let signal = status.signal();
    ProcessSessionExit::from_native(exit_code, signal, status.into_raw() as u32)
}

#[cfg(windows)]
fn native_exit(status: ExitStatus) -> ProcessSessionExit {
    let exit_code = status.code();
    // Preserve every bit of Windows' DWORD exit result even when its signed
    // i32 convenience representation is negative.
    let native_status = exit_code.unwrap_or(1) as u32;
    ProcessSessionExit::from_native(exit_code, None, native_status)
}

fn stdio(mode: StreamMode) -> AsyncStdio {
    match mode {
        StreamMode::Inherit => AsyncStdio::Inherit,
        StreamMode::Piped => AsyncStdio::Piped,
        StreamMode::Null => AsyncStdio::Null,
    }
}

fn std_command(spec: &SpawnSpec) -> (std::process::Command, bool, crate::ProcessPriority) {
    let SpawnSpec {
        program,
        args,
        current_dir,
        env,
        clear_env,
        stdin: _,
        stdout: _,
        stderr: _,
        create_process_group: _,
        lifetime_owner,
        priority,
        priority_policy: _,
        admission: _,
    } = spec;
    let kill_when_owner_dies = binds_to_spawner(*lifetime_owner);
    // An APE image runs through its planned loader on a host that cannot
    // exec it. Planned here rather than after a refusal: the bounded
    // runner's containment hooks route std through `execvp`, which would
    // hand a refused image to `/bin/sh` without reporting the refusal.
    let options = crate::ape::ApeOptions::with_overrides(
        *clear_env,
        env.iter()
            .map(|(key, value)| (key.as_os_str(), Some(value.as_os_str()))),
    );
    let mut ape_path = None;
    let mut command = match crate::ape::plan_launch(program, current_dir.as_deref(), &options) {
        Some(launch) => {
            ape_path = launch.child_path(options.path.as_deref());
            let mut command = std::process::Command::new(launch.loader());
            command.args(launch.args(args));
            command
        }
        None => {
            let mut command = std::process::Command::new(program);
            command.args(args);
            command
        }
    };
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    if *clear_env {
        command.env_clear();
    }
    command.envs(env.iter().map(|(key, value)| (key, value)));
    // Last, so the loader's `ape` directory leads whatever search path the
    // caller set: APE programs the child spawns in turn find a loader there.
    if let Some(path) = ape_path {
        command.env("PATH", path);
    }
    (command, kill_when_owner_dies, *priority)
}

fn process_capture_error(error: ProcessError) -> ProcessCaptureError {
    match error {
        ProcessError::OutputLimitExceeded { limit } => {
            ProcessCaptureError::OutputLimitExceeded { limit }
        }
        error => ProcessCaptureError::Io(process_error_to_io(error)),
    }
}

fn bounded_process_error(error: ProcessError) -> BoundedProcessError {
    match error {
        ProcessError::Timeout => BoundedProcessError::TimedOut,
        ProcessError::OutputLimitExceeded { limit } => {
            BoundedProcessError::OutputLimitExceeded { limit }
        }
        ProcessError::Spawn(error) => BoundedProcessError::Spawn(error),
        ProcessError::Io(error) => BoundedProcessError::Io(error),
        ProcessError::AlreadyStarted => BoundedProcessError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "bounded command was already started",
        )),
        ProcessError::NotRunning | ProcessError::StdinUnavailable => {
            BoundedProcessError::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "bounded command is no longer running",
            ))
        }
        ProcessError::RuntimeContext => BoundedProcessError::Io(io::Error::other(
            "bounded command requires an unavailable runtime context",
        )),
    }
}

fn process_error_to_io(error: ProcessError) -> io::Error {
    match error {
        ProcessError::Spawn(error) | ProcessError::Io(error) => error,
        ProcessError::AlreadyStarted => io::Error::new(io::ErrorKind::AlreadyExists, error),
        ProcessError::NotRunning => io::Error::new(io::ErrorKind::BrokenPipe, error),
        ProcessError::RuntimeContext => io::Error::other(error),
        ProcessError::StdinUnavailable => io::Error::new(io::ErrorKind::BrokenPipe, error),
        ProcessError::Timeout => io::Error::new(io::ErrorKind::TimedOut, error),
        ProcessError::OutputLimitExceeded { .. } => {
            io::Error::new(io::ErrorKind::FileTooLarge, error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{output_event, LifetimeBinding};
    use crate::async_engine::{launch, RuntimeBuilder};
    use crate::{ProcessOutputCompletion, ProcessOutputEvent};
    use running_process::{AsyncProcessSessionEvent, StreamKind};
    use std::sync::Mutex;

    /// A binding holding a watch that never finishes on its own, so that what
    /// stops it is observably the release rather than the watched exit.
    fn watching_binding() -> LifetimeBinding {
        LifetimeBinding {
            enforcement: None,
            watcher: Mutex::new(Some(launch(std::future::pending::<()>()))),
        }
    }

    /// The idempotence the release documents, since both terminal paths of a
    /// child and of a session can reach it for the same child.
    #[test]
    fn releasing_a_binding_twice_leaves_the_watch_gone_rather_than_failing() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("facade runtime");

        runtime
            .run_within(
                async {
                    let binding = watching_binding();
                    assert!(binding.is_watching());

                    binding.release();
                    assert!(!binding.is_watching());
                    binding.release();
                    assert!(!binding.is_watching());
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    /// Two callers releasing at once is the shape a session makes reachable:
    /// its lifecycle methods hold `&self`, so nothing serializes them for it.
    #[test]
    fn releasing_from_two_callers_at_once_still_stops_the_watch() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("facade runtime");

        runtime
            .run_within(
                async {
                    let binding = watching_binding();
                    std::thread::scope(|scope| {
                        for _ in 0..2 {
                            scope.spawn(|| binding.release());
                        }
                    });
                    assert!(!binding.is_watching());
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    /// The documented mapping is part of the facade contract: clients that
    /// used to match the substrate's error enum now match these kinds.
    #[test]
    fn process_errors_map_to_stable_io_kinds() {
        use super::process_error_to_io;
        use running_process::ProcessError;
        use std::io::ErrorKind;

        let spawn = process_error_to_io(ProcessError::Spawn(std::io::Error::from_raw_os_error(2)));
        assert_eq!(spawn.raw_os_error(), Some(2));
        let io = process_error_to_io(ProcessError::Io(std::io::Error::new(
            ErrorKind::Interrupted,
            "fixture",
        )));
        assert_eq!(io.kind(), ErrorKind::Interrupted);
        for (error, kind) in [
            (ProcessError::AlreadyStarted, ErrorKind::AlreadyExists),
            (ProcessError::NotRunning, ErrorKind::BrokenPipe),
            (ProcessError::StdinUnavailable, ErrorKind::BrokenPipe),
            (ProcessError::RuntimeContext, ErrorKind::Other),
            (ProcessError::Timeout, ErrorKind::TimedOut),
            (
                ProcessError::OutputLimitExceeded { limit: 1 },
                ErrorKind::FileTooLarge,
            ),
        ] {
            assert_eq!(process_error_to_io(error).kind(), kind);
        }
    }

    #[test]
    fn stream_fault_preserves_kind_message_and_native_error() {
        let event = output_event(AsyncProcessSessionEvent::StreamError {
            stream: StreamKind::Stderr,
            kind: std::io::ErrorKind::PermissionDenied,
            message: "access denied by fixture".to_owned(),
            raw_os_error: Some(5),
        });

        let ProcessOutputEvent::Completion(ProcessOutputCompletion::StderrError(fault)) = event
        else {
            panic!("stderr fault must remain a terminal stderr completion");
        };
        assert_eq!(fault.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(fault.message(), "access denied by fixture");
        assert_eq!(fault.raw_os_error(), Some(5));
    }
}
