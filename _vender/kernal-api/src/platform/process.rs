//! Process spawning, containment, inspection, termination, and stdio.

pub use crate::{
    assign_child_to_windows_job, cancel_capture_reader, canonical_environment_pairs,
    capture_reader_done, compat_shell_command, configure_exact_trace, configure_process_command,
    configure_sync_contained_command, configure_sync_daemon_command, configure_trampoline_command,
    current_executable_build_id, exact_trace_capability, exit_code, monitor_console_windows,
    parent_has_console, prepare_capture_reader, run_bounded_command, run_bounded_command_async,
    set_process_name, shell_command, soft_terminate_process_group, spawn_sync, spawn_sync_daemon,
    start_descendant_monitor, start_exact_trace, sync_child_native_handle, trampoline_exit_code,
    unix_mark_extra_fds_close_on_exec, BoundedProcessAsyncError, BoundedProcessError,
    BoundedProcessOutput, CaptureCancellation, PlatformChild, ProcessCaptureError, ProcessExit,
    ProcessOutput, ProcessOutputChunk, ProcessOutputCompletion, ProcessOutputEvent,
    ProcessOutputFault, ProcessPostExitDrain, ProcessPriority, ProcessSession, ProcessSessionExit,
    ProcessSessionOptions, SpawnAdmission, SpawnSpec, StreamMode, TracedChild, WindowsJobHandle,
};

/// Host-neutral command options selected by the caller before spawning.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessCommandConfig {
    pub creation_flags: Option<u32>,
    pub create_process_group: bool,
    pub nice: Option<i32>,
    pub address_space_limit_bytes: Option<u64>,
}

/// Availability of an invasive, lossless launched-tree trace backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactTraceCapability {
    pub available: bool,
    pub backend: &'static str,
    pub reason: &'static str,
    pub non_invasive_backend: &'static str,
    pub non_invasive_grade: NonInvasiveObservationGrade,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NonInvasiveObservationGrade {
    KernelNotification,
    KernelHintReconciled,
    SnapshotInferred,
}

/// A raw, bounded spawning-thread capture collected while a tracee is stopped.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TraceOriginArtifact {
    pub origin_pid: u32,
    pub thread_id: u32,
    pub architecture: String,
    pub register_format: String,
    pub executable: Option<std::path::PathBuf>,
    pub registers: Vec<u8>,
    pub stack_pointer: Option<u64>,
    pub instruction_pointer: Option<u64>,
    pub stack: Vec<u8>,
    pub truncated: bool,
    pub module_map: Vec<u8>,
    pub module_map_truncated: bool,
}

/// Native launched-tree event produced by an exact trace backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactTraceEvent {
    pub sequence: u64,
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub parent_start_key: Option<u64>,
    pub start_key: Option<u64>,
    pub timestamp: std::time::SystemTime,
    pub kind: ExactTraceEventKind,
    pub executable: Option<std::path::PathBuf>,
    pub argv: Option<Vec<std::ffi::OsString>>,
    pub origin: Option<TraceOriginArtifact>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExactTraceEventKind {
    Spawn,
    Exec,
    Exit {
        exit_code: Option<i32>,
        signal: Option<i32>,
        raw_status: i64,
    },
    Loss {
        reason: String,
    },
}

/// A descendant lifecycle fact reported by the host monitor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescendantEvent {
    Started {
        pid: u32,
        /// Immediate parent of the new descendant, when the discovery
        /// mechanism knows it: the Linux `children`-file walk and the
        /// macOS process-snapshot inversion both do; the Windows job
        /// IOCP notification is PID-only, so it reports `None` rather
        /// than paying a racy toolhelp scan per event.
        parent_pid: Option<u32>,
    },
    Exited(u32),
    /// The platform backend has completed its final reconciliation and no
    /// further descendant events can arrive.
    Completed,
}

/// Shared cancellation handle for a host-native descendant monitor.
pub struct DescendantMonitorStop {
    stopped: std::sync::atomic::AtomicBool,
    mutex: std::sync::Mutex<()>,
    wake: std::sync::Condvar,
}

impl DescendantMonitorStop {
    /// Create an untriggered monitor cancellation handle.
    pub fn new() -> Self {
        Self {
            stopped: std::sync::atomic::AtomicBool::new(false),
            mutex: std::sync::Mutex::new(()),
            wake: std::sync::Condvar::new(),
        }
    }

    /// Report whether monitoring was cancelled.
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Cancel monitoring and wake a sleeping monitor immediately.
    pub fn stop(&self) {
        let _guard = self.mutex.lock().unwrap_or_else(|error| error.into_inner());
        if !self.stopped.swap(true, std::sync::atomic::Ordering::AcqRel) {
            self.wake.notify_all();
        }
    }

    /// Wait until cancelled or `timeout` expires, returning whether cancelled.
    pub fn wait_timeout(&self, timeout: std::time::Duration) -> bool {
        if self.is_stopped() {
            return true;
        }
        let guard = self.mutex.lock().unwrap_or_else(|error| error.into_inner());
        if self.is_stopped() {
            return true;
        }
        let (_guard, _wait_result) = self
            .wake
            .wait_timeout(guard, timeout)
            .unwrap_or_else(|error| error.into_inner());
        self.is_stopped()
    }
}

impl Default for DescendantMonitorStop {
    fn default() -> Self {
        Self::new()
    }
}

/// Identifies one captured child output stream.
#[derive(Clone, Copy)]
pub enum CaptureStream {
    Stdout,
    Stderr,
}

/// Metadata about one visible window observed by console-popup monitoring.
#[derive(Debug, Clone)]
pub struct ConsoleWindowInfo {
    pub pid: u32,
    pub title: String,
    pub hwnd: u64,
}

/// A process identifier this host could actually have issued.
///
/// `pid_t` is signed, and every native call that takes one reads a negative
/// value as something else entirely: `kill(-N, ..)` addresses process group
/// `N`, and `kill(-1, ..)` addresses every process the caller is permitted to
/// signal. A PID arriving as an unchecked `u32` -- from a PID file, a state
/// row, an environment variable, anywhere other than a live child handle --
/// therefore stops naming a process somewhere above `i32::MAX` and silently
/// becomes a broadcast.
///
/// The range is checked once, here, so no later call has to remember to: a
/// `ProcessId` cannot hold a value whose signed reading is anything but the
/// same single process. That is the difference between a rule tested for and
/// a rule that cannot be broken.
///
/// The accepted range is `1 ..= i32::MAX` on every host, Windows included,
/// even though Windows has no signed-PID problem of its own. One range keeps
/// a PID meaningful when one host writes it down and another reads it -- which
/// is what a PID file is for -- and costs nothing, because no supported host
/// issues a number above it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProcessId(u32);

impl ProcessId {
    /// Accept `pid` only if it could name one process on this host.
    ///
    /// Zero is rejected alongside the out-of-range values, and for the same
    /// reason: it is not a process address either. `kill(2)` reads it as the
    /// caller's own process group and `waitid(2)` as "any child".
    pub fn new(pid: u32) -> Result<Self, ProcessInspectError> {
        if pid == 0 || pid > i32::MAX as u32 {
            Err(ProcessInspectError::stated(
                ProcessInspectErrorKind::InvalidPid,
                "pid outside the range a host issues for a single process",
            ))
        } else {
            Ok(Self(pid))
        }
    }

    /// This process's own identifier.
    ///
    /// A host never issues itself a number it could not issue, so this cannot
    /// fail and does not make the caller pretend it might.
    #[must_use]
    pub fn current() -> Self {
        Self(std::process::id())
    }

    /// The number, for a caller that has to write it down or print it.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The number as the signed value a native call will read.
    ///
    /// Crate-private on purpose: the invariant is what makes this cast safe,
    /// and handing the result out would let a caller re-derive the very trap
    /// the type exists to close.
    ///
    /// Unix-only, because a signed process identifier is a Unix idea. Windows
    /// passes the number unsigned and has no negative reading to guard
    /// against; it keeps the same accepted range for the sake of a PID that
    /// one host writes down and another reads back, not for this cast.
    #[cfg(unix)]
    #[must_use]
    pub(crate) const fn native_signed(self) -> i32 {
        self.0 as i32
    }
}

impl TryFrom<u32> for ProcessId {
    type Error = ProcessInspectError;

    fn try_from(pid: u32) -> Result<Self, Self::Error> {
        Self::new(pid)
    }
}

impl std::fmt::Display for ProcessId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// One live process instance, rather than merely an address in the PID table.
///
/// The creation key is deliberately opaque. It is a Windows `FILETIME`, Linux
/// `/proc` start-tick value, or macOS `proc_bsdinfo` timestamp depending on
/// the host, and is meaningful only for equality during this boot session.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProcessIdentity {
    pid: u32,
    creation_key: [u64; 2],
}

impl ProcessIdentity {
    pub(crate) const fn from_native(pid: u32, creation_key: [u64; 2]) -> Self {
        Self { pid, creation_key }
    }

    /// The process-table address paired with this identity.
    pub const fn pid(self) -> u32 {
        self.pid
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn has_native_key(self, creation_key: [u64; 2]) -> bool {
        self.creation_key == creation_key
    }
}

/// A process was not observable well enough to capture a safe identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessIdentityUnavailable {
    /// The host denied access to the native creation key.
    PermissionDenied,
    /// This target does not provide the required native observation primitive.
    Unsupported,
}

/// The outcome of resolving a PID to a generation-safe process identity.
#[derive(Debug)]
pub enum ProcessIdentityCapture {
    /// A live process and its exact creation generation.
    Found(ProcessIdentity),
    /// No process currently owns this PID. This is not an identity.
    Exited,
    /// The process may exist, but the host could not obtain its creation key.
    Unavailable(ProcessIdentityUnavailable),
    /// The host failed while obtaining the creation key.
    Error(std::io::Error),
}

/// The result of an action addressed by [`ProcessIdentity`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessIdentityAction {
    /// The action was delivered to the exact process instance.
    Performed,
    /// The original process has already exited. No replacement was touched.
    AlreadyExited,
}

/// Why a generation-safe action was refused.
#[derive(Debug, thiserror::Error)]
pub enum ProcessIdentityActionError {
    /// The PID is now owned by a different process instance.
    #[error("process PID was reused by a different instance")]
    StaleIdentity,
    /// The host could no longer obtain an identity safely.
    #[error("process identity is unavailable: {0:?}")]
    Unavailable(ProcessIdentityUnavailable),
    /// The host failed before it could act.
    #[error("process identity action failed: {0}")]
    Host(#[source] std::io::Error),
}

/// Run a caller-owned command in the foreground and report its exit status.
///
/// Deliberately the *absence* of this facade's usual policy. Every other
/// spawning operation here configures process groups, descriptor inheritance,
/// consoles, owner-death behaviour or containment; this one configures nothing
/// and inherits the caller's launch context exactly, which is what a tool a
/// person is watching in their terminal needs. Standard streams keep whatever
/// the command already specifies -- unset means inherited, as with
/// [`std::process::Command::status`].
///
/// Use a contained or bounded operation when child ownership, timeouts or
/// captured output matter; nothing here reaps, terminates, or drains for the
/// caller.
pub fn foreground_status(
    command: &mut std::process::Command,
) -> std::io::Result<std::process::ExitStatus> {
    command.status()
}

/// Run a caller-owned command in the foreground and collect what it wrote.
///
/// The capture half of [`foreground_status`], with the same absence of policy:
/// stdout and stderr are captured unless the caller set them otherwise, and
/// both streams are drained concurrently, exactly as
/// [`std::process::Command::output`] does. Nothing bounds the output or the
/// runtime -- reach for a bounded operation when a runaway child is a concern
/// rather than an inherited terminal.
pub fn foreground_output(
    command: &mut std::process::Command,
) -> std::io::Result<std::process::Output> {
    command.output()
}

/// Capture a process identity from the host's strongest native creation key.
///
/// Callers must retain this value, not only its [`ProcessIdentity::pid`], for
/// any deferred inspection or mutation.
pub fn capture_identity(pid: u32) -> ProcessIdentityCapture {
    crate::platform_imp::capture_process_identity(pid)
}

/// Forcibly terminate exactly `identity`.
///
/// The host resolves the PID and creation key immediately before signaling.
/// A replacement PID produces [`ProcessIdentityActionError::StaleIdentity`]
/// and is never signaled.
pub fn force_kill(
    identity: ProcessIdentity,
) -> Result<ProcessIdentityAction, ProcessIdentityActionError> {
    crate::platform_imp::force_kill_identity(identity)
}

/// Ask exactly `identity` to terminate gracefully where the host supports it.
pub fn signal_terminate(
    identity: ProcessIdentity,
) -> Result<ProcessIdentityAction, ProcessIdentityActionError> {
    crate::platform_imp::signal_terminate_identity(identity)
}

/// Terminate the observed process and every discovered descendant.
///
/// Every discovered PID is captured as a [`ProcessIdentity`] and revalidated
/// before each signal attempt; a recycled member is skipped rather than
/// becoming a target.
pub fn kill_tree(
    identity: ProcessIdentity,
    timeout: std::time::Duration,
) -> Result<u32, ProcessIdentityActionError> {
    crate::platform_imp::kill_tree_identity(identity, timeout)
}

#[cfg(test)]
pub(crate) fn act_on_current_identity(
    identity: ProcessIdentity,
    capture: impl FnOnce(u32) -> ProcessIdentityCapture,
    action: impl FnOnce() -> Result<(), std::io::Error>,
) -> Result<ProcessIdentityAction, ProcessIdentityActionError> {
    match capture(identity.pid()) {
        ProcessIdentityCapture::Found(current) if current == identity => action()
            .map(|()| ProcessIdentityAction::Performed)
            .map_err(ProcessIdentityActionError::Host),
        ProcessIdentityCapture::Found(_) => Err(ProcessIdentityActionError::StaleIdentity),
        ProcessIdentityCapture::Exited => Ok(ProcessIdentityAction::AlreadyExited),
        ProcessIdentityCapture::Unavailable(reason) => {
            Err(ProcessIdentityActionError::Unavailable(reason))
        }
        ProcessIdentityCapture::Error(error) => Err(ProcessIdentityActionError::Host(error)),
    }
}

/// Private snapshot material for native tree discovery. It never crosses the
/// facade boundary; public callers use [`ProcessIdentity`] instead.
#[cfg(any(target_os = "macos", all(test, target_os = "linux")))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessSnapshot {
    pub(crate) identity: ProcessIdentity,
    pub(crate) parent_pid: u32,
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn recycled_pid_is_refused_and_never_touches_the_replacement() {
        let original = ProcessIdentity::from_native(41, [10, 0]);
        let replacement = ProcessIdentity::from_native(41, [11, 0]);
        let touched = Cell::new(false);
        let outcome = act_on_current_identity(
            original,
            |_| ProcessIdentityCapture::Found(replacement),
            || {
                touched.set(true);
                Ok(())
            },
        );
        assert!(matches!(
            outcome,
            Err(ProcessIdentityActionError::StaleIdentity)
        ));
        assert!(
            !touched.get(),
            "the replacement process must never be touched"
        );
    }

    #[test]
    fn an_exited_identity_is_idempotent_without_attempting_a_mutation() {
        let identity = ProcessIdentity::from_native(41, [10, 0]);
        let touched = Cell::new(false);
        let outcome = act_on_current_identity(
            identity,
            |_| ProcessIdentityCapture::Exited,
            || {
                touched.set(true);
                Ok(())
            },
        );
        assert!(matches!(outcome, Ok(ProcessIdentityAction::AlreadyExited)));
        assert!(!touched.get());
    }

    #[test]
    fn matching_identity_performs_the_requested_action() {
        let identity = ProcessIdentity::from_native(41, [10, 0]);
        let touched = Cell::new(false);
        let outcome = act_on_current_identity(
            identity,
            |_| ProcessIdentityCapture::Found(identity),
            || {
                touched.set(true);
                Ok(())
            },
        );
        assert!(matches!(outcome, Ok(ProcessIdentityAction::Performed)));
        assert!(touched.get());
    }
}

/// Environment base selected by the shared caller for a synchronous spawn.
///
/// Explicit `Command::env` additions and removals remain on the command and
/// are applied after this base by the selected platform implementation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncEnvironment {
    /// Start with the spawning process's ambient environment.
    Inherit,
    /// Start with this complete, caller-assembled base environment.
    Explicit(Vec<(std::ffi::OsString, std::ffi::OsString)>),
    /// Start with the logged-in user's baseline environment, as a fresh login
    /// would have it, instead of the spawning process's ambient environment.
    ///
    /// Resolved at spawn time through [`crate::platform::host::login_environment`]:
    /// Windows builds it from machine and user settings; Unix rebuilds it from
    /// the passwd entry (`USER`, `LOGNAME`, `HOME`, `SHELL`, the platform
    /// default `PATH`, and the carried-over locale, time-zone, and temporary
    /// directory variables). This is the base a long-lived daemon wants: it
    /// does not inherit whatever a particular launching shell or build tool
    /// happened to export. Values the daemon does need must be set
    /// explicitly on the command.
    UserBaseline,
}

impl SyncEnvironment {
    /// The complete base environment, or `None` to inherit the ambient one.
    pub(crate) fn into_base(
        self,
    ) -> std::io::Result<Option<Vec<(std::ffi::OsString, std::ffi::OsString)>>> {
        match self {
            Self::Inherit => Ok(None),
            Self::Explicit(base) => Ok(Some(base)),
            Self::UserBaseline => crate::platform::host::login_environment().map(Some),
        }
    }
}

/// Private, facade-owned bounds for one contained worker process tree.
///
/// The protocol layer selects these values; platform implementations translate
/// only the bounds their native containment primitive can enforce.
#[allow(dead_code)] // Phase-A foundation; the phase-B supervisor owns it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct WorkerLimits {
    pub(crate) active_processes: Option<u32>,
    pub(crate) process_memory_bytes: Option<u64>,
    pub(crate) job_memory_bytes: Option<u64>,
}

#[cfg(all(feature = "tauri-webview", feature = "wasm-sketch-worker"))]
pub(crate) fn configure_native_worker_environment(command: &mut std::process::Command) {
    crate::platform_imp::configure_native_worker_environment(command);
}

/// Semantic stage at which a contained-worker launch or cleanup failed.
#[allow(dead_code)] // Phase-A foundation; the phase-B supervisor owns it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkerStage {
    Pipe,
    ConfigureContainment,
    Create,
    AssignContainment,
    Resume,
    Terminate,
    Reap,
}

/// Private worker failure without exposing an OS handle or backend type.
#[allow(dead_code)] // Phase-A foundation; the phase-B supervisor owns it.
#[derive(Debug, thiserror::Error)]
#[error("contained worker failed at {stage:?}: {source}")]
pub(crate) struct WorkerError {
    stage: WorkerStage,
    source: std::io::Error,
}

#[allow(dead_code)] // Phase-A foundation; the phase-B supervisor owns it.
impl WorkerError {
    pub(crate) fn new(stage: WorkerStage, source: std::io::Error) -> Self {
        Self { stage, source }
    }

    pub(crate) fn stage(&self) -> WorkerStage {
        self.stage
    }
}

/// Platform-private lifecycle implementation for [`WorkerChild`].
#[allow(dead_code)] // Phase-A foundation; the phase-B supervisor owns it.
pub(crate) trait WorkerChildControl: Send {
    fn try_wait(&mut self) -> std::io::Result<Option<i32>>;
    fn force_and_reap(&mut self, timeout: std::time::Duration) -> Result<(), WorkerError>;
    fn shutdown(&mut self);
}

/// Owned pipes and lifecycle capability for the explicit Wasm worker path.
///
/// This is deliberately crate-private: callers receive protocol semantic
/// outcomes, never native child, Job, process-group, or descriptor handles.
#[allow(dead_code)] // Phase-A foundation; the phase-B supervisor owns it.
pub(crate) struct WorkerChild {
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<std::process::ChildStdout>,
    pid: u32,
    // `Option` permits the consuming split below without moving a field out
    // of a Drop type.  The sole remaining owner always performs containment.
    inner: Option<Box<dyn WorkerChildControl>>,
    contained: bool,
}

/// The lifecycle half of a split contained worker.
///
/// Pipes are intentionally not retained here: protocol I/O can be owned by
/// independent blocking reader/writer tasks without placing this native
/// lifecycle capability behind an async mutex.
#[allow(dead_code)] // Phase-D private worker supervisor owns it.
pub(crate) struct WorkerControl {
    pid: u32,
    inner: Option<Box<dyn WorkerChildControl>>,
    contained: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkerNormalReap {
    Clean,
    Nonzero,
}

#[allow(dead_code)] // Phase-A foundation; the phase-B supervisor owns it.
impl WorkerChild {
    pub(crate) fn new(
        stdin: Option<std::process::ChildStdin>,
        stdout: Option<std::process::ChildStdout>,
        pid: u32,
        inner: Box<dyn WorkerChildControl>,
    ) -> Self {
        Self {
            stdin,
            stdout,
            pid,
            inner: Some(inner),
            contained: false,
        }
    }

    pub(crate) fn id(&self) -> u32 {
        self.pid
    }
    pub(crate) fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.stdin.take()
    }
    pub(crate) fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.stdout.take()
    }
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        let exit = self
            .inner
            .as_mut()
            .map_or(Ok(None), |inner| inner.try_wait())?;
        if exit.is_some() {
            self.contained = true;
        }
        Ok(exit)
    }

    /// Close the control writer before hard containment, then reap within the
    /// caller-selected bound. Repeated calls are delegated to the platform
    /// owner and remain idempotent.
    pub(crate) fn force_and_reap(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<(), WorkerError> {
        drop(self.stdin.take());
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        inner.force_and_reap(timeout)?;
        self.contained = true;
        Ok(())
    }

    /// Separates protocol pipes from the sole native lifecycle owner.
    ///
    /// `WorkerControl` remains responsible for best-effort containment even
    /// after both pipe owners have been dropped.  This consumes `self`, so no
    /// second Drop implementation can kill or reap the same process tree.
    pub(crate) fn into_parts(
        mut self,
    ) -> (
        WorkerControl,
        Option<std::process::ChildStdin>,
        Option<std::process::ChildStdout>,
    ) {
        let control = WorkerControl {
            pid: self.pid,
            inner: self.inner.take(),
            contained: self.contained,
        };
        let stdin = self.stdin.take();
        let stdout = self.stdout.take();
        (control, stdin, stdout)
    }
}

impl Drop for WorkerChild {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if !self.contained {
            if let Some(inner) = self.inner.as_mut() {
                inner.shutdown();
            }
        }
    }
}

#[allow(dead_code)] // Phase-D private worker supervisor owns it.
impl WorkerControl {
    pub(crate) fn id(&self) -> u32 {
        self.pid
    }

    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        let exit = self
            .inner
            .as_mut()
            .map_or(Ok(None), |inner| inner.try_wait())?;
        if exit.is_some() {
            self.contained = true;
        }
        Ok(exit)
    }

    /// Reap a child which is expected to exit normally.  This never sends a
    /// termination signal: callers decide separately whether containment is
    /// required after a timeout or an abnormal exit.
    pub(crate) fn reap_clean(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<WorkerNormalReap, WorkerError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self
                .try_wait()
                .map_err(|source| WorkerError::new(WorkerStage::Reap, source))?
            {
                Some(0) => {
                    self.contained = true;
                    return Ok(WorkerNormalReap::Clean);
                }
                Some(code) => {
                    let _ = code;
                    self.contained = true;
                    return Ok(WorkerNormalReap::Nonzero);
                }
                None if std::time::Instant::now() >= deadline => {
                    return Err(WorkerError::new(
                        WorkerStage::Reap,
                        std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "worker clean reap timed out",
                        ),
                    ));
                }
                None => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        }
    }

    /// Force containment and reap on a caller-selected blocking lane.  A
    /// failure deliberately retains the backend owner for a later retry or
    /// bounded Drop cleanup.
    pub(crate) fn force_and_reap(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<(), WorkerError> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        inner.force_and_reap(timeout)?;
        self.contained = true;
        Ok(())
    }
}

impl Drop for WorkerControl {
    fn drop(&mut self) {
        if !self.contained {
            if let Some(inner) = self.inner.as_mut() {
                inner.shutdown();
            }
        }
    }
}

#[cfg(test)]
mod worker_child_tests {
    use super::{WorkerChild, WorkerChildControl, WorkerError, WorkerStage};
    use std::io;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Default)]
    struct Counts {
        waits: AtomicUsize,
        forces: AtomicUsize,
        shutdowns: AtomicUsize,
        failed_forces_remaining: AtomicUsize,
        observed_exit: AtomicBool,
    }

    struct FakeControl {
        counts: Arc<Counts>,
    }

    impl WorkerChildControl for FakeControl {
        fn try_wait(&mut self) -> io::Result<Option<i32>> {
            self.counts.waits.fetch_add(1, Ordering::Relaxed);
            Ok(self
                .counts
                .observed_exit
                .load(Ordering::Relaxed)
                .then_some(0))
        }

        fn force_and_reap(&mut self, _timeout: Duration) -> Result<(), WorkerError> {
            self.counts.forces.fetch_add(1, Ordering::Relaxed);
            if self
                .counts
                .failed_forces_remaining
                .try_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(WorkerError::new(
                    WorkerStage::Reap,
                    io::Error::new(io::ErrorKind::TimedOut, "fake reap timeout"),
                ));
            }
            Ok(())
        }

        fn shutdown(&mut self) {
            self.counts.shutdowns.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn fake_worker(counts: Arc<Counts>) -> WorkerChild {
        WorkerChild::new(None, None, 77, Box::new(FakeControl { counts }))
    }

    #[test]
    fn unsplit_drop_contains_exactly_once() {
        let counts = Arc::new(Counts::default());
        drop(fake_worker(Arc::clone(&counts)));
        assert_eq!(counts.shutdowns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn split_pipes_do_not_contain_before_control_drop() {
        let counts = Arc::new(Counts::default());
        let (control, stdin, stdout) = fake_worker(Arc::clone(&counts)).into_parts();
        drop(stdin);
        drop(stdout);
        assert_eq!(counts.shutdowns.load(Ordering::Relaxed), 0);
        drop(control);
        assert_eq!(counts.shutdowns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn successful_force_is_not_repeated_by_drop() {
        let counts = Arc::new(Counts::default());
        let (mut control, _stdin, _stdout) = fake_worker(Arc::clone(&counts)).into_parts();
        control
            .force_and_reap(Duration::from_millis(1))
            .expect("fake force succeeds");
        drop(control);
        assert_eq!(counts.forces.load(Ordering::Relaxed), 1);
        assert_eq!(counts.shutdowns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn timed_out_force_retains_control_for_retry_and_drop() {
        let counts = Arc::new(Counts::default());
        counts.failed_forces_remaining.store(1, Ordering::Relaxed);
        let (mut control, _stdin, _stdout) = fake_worker(Arc::clone(&counts)).into_parts();
        let error = control
            .force_and_reap(Duration::from_millis(1))
            .expect_err("first fake force times out");
        assert_eq!(error.stage(), WorkerStage::Reap);
        assert_eq!(control.try_wait().expect("fake wait"), None);
        control
            .force_and_reap(Duration::from_millis(1))
            .expect("retry keeps the backend owner");
        drop(control);
        assert_eq!(counts.forces.load(Ordering::Relaxed), 2);
        assert_eq!(counts.waits.load(Ordering::Relaxed), 1);
        assert_eq!(counts.shutdowns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn clean_observed_exit_never_forces() {
        let counts = Arc::new(Counts::default());
        counts.observed_exit.store(true, Ordering::Relaxed);
        let (mut control, _stdin, _stdout) = fake_worker(Arc::clone(&counts)).into_parts();
        assert_eq!(control.try_wait().expect("fake exit"), Some(0));
        drop(control);
        assert_eq!(counts.forces.load(Ordering::Relaxed), 0);
        assert_eq!(counts.shutdowns.load(Ordering::Relaxed), 0);
    }
}

/// Caller-supplied stdio bindings for a contained synchronous child.
///
/// Each stream is independently configured. `drain_timeout` bounds how long
/// wrapper-owned pipe ends remain open after the child exits; `None` leaves
/// pipe closure entirely to the caller. `show_console` only affects Windows.
pub struct SpawnStdio<'a> {
    /// Child standard input source.
    pub stdin: StdioSource<'a>,
    /// Child standard output destination.
    pub stdout: StdioSource<'a>,
    /// Child standard error destination.
    pub stderr: StdioSource<'a>,
    /// Maximum post-exit pipe drain interval.
    pub drain_timeout: Option<std::time::Duration>,
    /// Whether a Windows child may inherit or allocate a visible console.
    pub show_console: bool,
}

impl Default for SpawnStdio<'_> {
    fn default() -> Self {
        Self {
            stdin: StdioSource::Null,
            stdout: StdioSource::Parent,
            stderr: StdioSource::Parent,
            drain_timeout: Some(std::time::Duration::from_secs(2)),
            show_console: false,
        }
    }
}

/// Caller-supplied output bindings for a detached synchronous child.
///
/// Detached children may write only to the platform null device or to a
/// caller-owned file. Parent stdio and anonymous pipes are intentionally not
/// available because either can retain or depend on the launching process.
pub struct DaemonStdio<'a> {
    /// Child standard output destination.
    pub stdout: DaemonStdioSource<'a>,
    /// Child standard error destination.
    pub stderr: DaemonStdioSource<'a>,
}

impl Default for DaemonStdio<'_> {
    fn default() -> Self {
        Self {
            stdout: DaemonStdioSource::Null,
            stderr: DaemonStdioSource::Null,
        }
    }
}

/// Output destination accepted by the detached-child path.
pub enum DaemonStdioSource<'a> {
    /// Route output to the platform null device.
    Null,
    /// Duplicate a caller-owned file into the child.
    File(&'a std::fs::File),
}

/// Standard-stream source or destination for a contained child.
pub enum StdioSource<'a> {
    /// Route the stream to the platform null device.
    Null,
    /// Inherit the matching stream from the parent process.
    Parent,
    /// Duplicate a caller-owned file into the child.
    File(&'a std::fs::File),
    /// Create and return an anonymous parent/child pipe pair.
    Pipe,
}

/// Poll `try_wait` with capped backoff until it reports an exit or `timeout`
/// elapses.
fn wait_within(
    timeout: std::time::Duration,
    mut try_wait: impl FnMut() -> std::io::Result<Option<i32>>,
) -> std::io::Result<i32> {
    let started = std::time::Instant::now();
    let mut pause = std::time::Duration::from_millis(1);
    loop {
        if let Some(code) = try_wait()? {
            return Ok(code);
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "child did not exit within its timeout",
            ));
        }
        std::thread::sleep(pause.min(timeout - elapsed));
        pause = (pause * 2).min(std::time::Duration::from_millis(20));
    }
}

/// Handle for a detached child that is not terminated when dropped.
pub struct DaemonChild {
    pub(crate) pid: u32,
    pub(crate) inner: Box<dyn DaemonChildControl>,
}

pub(crate) trait DaemonChildControl:
    Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
{
    fn kill(&mut self) -> std::io::Result<()>;
    fn wait(&mut self) -> std::io::Result<i32>;
    fn try_wait(&mut self) -> std::io::Result<Option<i32>>;
}

impl DaemonChild {
    /// Return the operating-system process identifier.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Terminate the child process.
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.inner.kill()
    }

    /// Wait for the child and return its numeric exit code.
    #[deprecated(note = "waits forever for a live child; use `wait_within` (#433)")]
    pub fn wait(&mut self) -> std::io::Result<i32> {
        self.inner.wait()
    }

    /// Wait at most `timeout` for the child and return its numeric exit code.
    ///
    /// Returns [`std::io::ErrorKind::TimedOut`] if the child is still running
    /// when the timeout elapses; the child is left running and unreaped.
    pub fn wait_within(&mut self, timeout: std::time::Duration) -> std::io::Result<i32> {
        wait_within(timeout, || self.inner.try_wait())
    }

    /// Return the exit code if the child has finished without blocking.
    pub fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        self.inner.try_wait()
    }
}

/// Handle and optional parent pipe ends for a contained child.
///
/// Dropping this value shuts down the contained process group.
pub struct SpawnedChild {
    /// Writable parent end when standard input was configured as a pipe.
    pub stdin: Option<std::process::ChildStdin>,
    /// Readable parent end when standard output was configured as a pipe.
    pub stdout: Option<std::process::ChildStdout>,
    /// Readable parent end when standard error was configured as a pipe.
    pub stderr: Option<std::process::ChildStderr>,
    pub(crate) pid: u32,
    pub(crate) inner: Box<dyn SpawnedChildControl>,
}

pub(crate) trait SpawnedChildControl:
    Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
{
    fn kill(&mut self) -> std::io::Result<()>;
    fn wait(&mut self) -> std::io::Result<i32>;
    fn try_wait(&mut self) -> std::io::Result<Option<i32>>;
    fn shutdown(&mut self);
}

impl SpawnedChild {
    /// Transfer the private process owner and the parent protocol pipes to a
    /// more specialized contained-child facade without running this wrapper's
    /// shutdown-on-drop path.
    #[allow(dead_code)] // Used only by Unix platform worker adapters.
    pub(crate) fn into_worker_parts(
        self,
    ) -> (
        Option<std::process::ChildStdin>,
        Option<std::process::ChildStdout>,
        u32,
        Box<dyn SpawnedChildControl>,
    ) {
        let mut child = std::mem::ManuallyDrop::new(self);
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        drop(child.stderr.take());
        let pid = child.pid;
        // SAFETY: `child` is ManuallyDrop so its Drop implementation cannot
        // shut down `inner`; this is the one ownership transfer of `inner`.
        let inner = unsafe { std::ptr::read(&child.inner) };
        (stdin, stdout, pid, inner)
    }

    /// Return the operating-system process identifier.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Forcibly terminate the child on a best-effort basis.
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.inner.kill()
    }

    /// Wait for the child and return its numeric exit code.
    #[deprecated(note = "waits forever for a live child; use `wait_within` (#433)")]
    pub fn wait(&mut self) -> std::io::Result<i32> {
        self.inner.wait()
    }

    /// Wait at most `timeout` for the child and return its numeric exit code.
    ///
    /// Returns [`std::io::ErrorKind::TimedOut`] if the child is still running
    /// when the timeout elapses; the child is left running and unreaped.
    pub fn wait_within(&mut self, timeout: std::time::Duration) -> std::io::Result<i32> {
        wait_within(timeout, || self.inner.try_wait())
    }

    /// Return the exit code if the child has finished without blocking.
    pub fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        self.inner.try_wait()
    }
}

impl Drop for SpawnedChild {
    fn drop(&mut self) {
        self.inner.shutdown();
    }
}

#[derive(Clone, Copy)]
pub enum ObserverScope {
    SystemWide,
    LaunchedProcessTree,
}
#[derive(Clone, Copy)]
pub enum ObserverCategory {
    File,
    Network,
    Process,
}
#[derive(Clone, Copy)]
pub enum ObserverSupport {
    Supported,
    Partial,
    Unavailable,
}
#[derive(Clone, Copy)]
pub struct ObserverBackend {
    pub support: ObserverSupport,
    pub backend: &'static str,
    pub reason: &'static str,
}
pub use crate::platform_imp::observer_backend;
pub use crate::platform_imp::read_process_cmdline;
/// Every open file the host is willing to name for one process, spelled the
/// way that host spells it.
///
/// The answer is a point-in-time snapshot and not a stream: a descriptor can
/// close, and another open, between the walk and the return. More
/// importantly, the three hosts answer three different questions here, so a
/// caller comparing these strings against a path it built itself has to know
/// which question it asked.
///
/// On Linux every entry of `/proc/<pid>/fd` is reported, with the `readlink`
/// target kept verbatim and decoded lossily. Anonymous kernel objects
/// therefore sit beside real paths: `socket:[12345]`, `pipe:[67890]` and
/// `anon_inode:...` appear as themselves, and a file unlinked since it was
/// opened appears as `/its/path (deleted)`. An entry whose `readlink` fails
/// -- the descriptor closed underneath the walk -- is skipped.
///
/// On macOS only vnode-backed descriptors are reported. The list from
/// `proc_pidinfo(PROC_PIDLISTFDS)` is filtered to `PROX_FDTYPE_VNODE`, which
/// covers regular files, directories and devices, and each survivor is
/// resolved to an absolute POSIX path. Sockets, pipes, kqueues and every
/// other descriptor kind are dropped with no marker of any kind, so "is
/// anything still holding my socket?" is answered no here whatever the
/// truth. A vnode whose path cannot be read, or whose path comes back empty,
/// is dropped as well.
///
/// On Windows the system-wide handle table is filtered to the target
/// process, each of its handles is duplicated into this process, and only
/// those whose NT object *type* name is `File` are named. That type is
/// broader than files on disk. The name is whatever
/// `NtQueryObject(ObjectNameInformation)` returned, verbatim, which is a
/// name in the NT object namespace rather than the DOS one:
/// `\Device\HarddiskVolume3\Users\me\cache.db`, or `\Device\NamedPipe\...`
/// for a pipe. Nothing on this path converts an NT device name to a drive
/// letter, so a `C:\Users\me\cache.db` built from a `Path` never compares
/// equal to anything returned here, and a caller that tests for its own file
/// by string equality silently never finds it. A handle that cannot be
/// duplicated, whose type cannot be read, or that comes back with an empty
/// name is dropped rather than failing the whole snapshot.
///
/// # Errors
///
/// `pid == 0` is rejected as `InvalidInput` on all three hosts. Otherwise
/// Linux needs only to read `/proc/<pid>/fd` and macOS only to be allowed
/// `proc_pidinfo`, while Windows must first open the target with
/// `PROCESS_DUP_HANDLE` and returns the operating system's error when it
/// cannot -- so the same question can fail outright on Windows for a process
/// the other two hosts answer for.
///
/// An `Ok` that is empty, or that omits a descriptor kind this host does not
/// report, is indistinguishable from a truthful "nothing is open".
pub use crate::platform_imp::read_process_file_handles;

/// Platform-neutral Unix signal selectors used by the compatibility facade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixSignalKind {
    Interrupt,
    Terminate,
    Kill,
}

pub use crate::{
    unix_set_priority, unix_signal_process, unix_signal_process_group, unix_signal_raw,
};

/// What this host installed so a child outlives its owner no longer than it
/// should.
///
/// The variants name the *guarantee*, not the call that produced it. A caller
/// deciding whether to spawn a supervisor cares that the kernel will not do
/// the reaping for it; whether the kernel would have used a parent-death
/// signal or a job object is not a distinction it can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerDeathCleanup {
    /// The kernel signals this process when its owner exits.
    OwnerDeathSignal,
    /// This process belongs to a container the kernel destroys with its owner.
    KillOnOwnerHandleClose,
    /// This process was already in such a container, installed by someone else.
    AlreadyContained,
    /// The host offers no kernel mechanism; a supervisor must do the reaping.
    SupervisorRequired,
    /// The host offers nothing and no supervisor contract is defined here.
    Unsupported,
}

/// Which step of installing owner-death containment failed.
///
/// The caller's operator-facing messages distinguish these, and rightly: not
/// being allowed to *build* a container is a different situation from
/// building one and not being allowed to *join* it. Collapsing both into one
/// error would make the two indistinguishable in a log, so the stage travels
/// with the error rather than being inferred from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerDeathCleanupStage {
    /// Asking the kernel to signal this process when its owner exits.
    RequestSignal,
    /// Creating the container that the kernel destroys with its owner.
    CreateContainer,
    /// Placing this process inside that container.
    JoinContainer,
}

/// A failure to install owner-death containment, and the step it failed at.
#[derive(Debug, thiserror::Error)]
#[error("{stage:?}: {source}")]
pub struct OwnerDeathCleanupError {
    /// The step that failed.
    pub stage: OwnerDeathCleanupStage,
    /// What the host reported.
    pub source: std::io::Error,
}

pub use crate::{
    process_install_owner_death_cleanup as install_owner_death_cleanup,
    process_owner_death_cleanup_target as owner_death_cleanup_target,
};

/// Why a host could not answer a question about a process.
///
/// The three named cases are the ones a caller can act on: a PID that could
/// never name a process, a process that is not there, and a question this
/// host does not answer. Everything else is the host's own report, kept
/// whole rather than flattened into one of the three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessInspectErrorKind {
    /// The PID is outside the range this host issues.
    InvalidPid,
    /// No process on this host currently has that PID.
    NotFound,
    /// This host has no such primitive.
    Unsupported,
    /// The host was asked and refused, or failed.
    Host,
}

/// A failure to inspect or signal a process, and what kind of failure it was.
#[derive(Debug, thiserror::Error)]
#[error("{kind:?}: {source}")]
pub struct ProcessInspectError {
    /// Which of the four situations this is.
    pub kind: ProcessInspectErrorKind,
    /// What the host reported.
    pub source: std::io::Error,
}

impl ProcessInspectError {
    /// Build an error of `kind` carrying the host's last reported error.
    pub fn last_os_error(kind: ProcessInspectErrorKind) -> Self {
        Self {
            kind,
            source: std::io::Error::last_os_error(),
        }
    }

    /// Build an error of `kind` with a message this crate composed itself.
    pub fn stated(kind: ProcessInspectErrorKind, message: &str) -> Self {
        Self {
            kind,
            source: std::io::Error::other(message.to_string()),
        }
    }
}

pub use crate::{process_same_executable_path as same_executable_path, ProcessLiveness};

/// What a host was able to say about an exit once it had happened.
///
/// The variants are not two degrees of success. Every host reports *that* the
/// process exited; only some report *how*. Linux and macOS hand over a status
/// for a process this one parented and nothing for any other, because the
/// status is consumed by whoever reaps it and nobody else. Windows keeps an
/// exit code readable through any handle. Folding that into an
/// `Option<ExitStatus>` would let a caller read "no status" as "still
/// running", which is the one thing it never means here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessExitObservation {
    /// The process exited and the host reported how.
    Reported(ProcessSessionExit),
    /// The process exited; this host does not report the status of a process
    /// the observer did not parent.
    Unreported,
}

pub use crate::ProcessExitWatch;

/// Whose lifetime a spawned process is asked not to outlive.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LifetimeOwner {
    /// The process doing the spawning.
    ///
    /// This is the only owner a kernel mechanism can express, because both
    /// kernel mechanisms are relationships the spawn itself creates: a
    /// parent-death signal names the parent, and a job handle is held by the
    /// spawner. It is also, for a broker-spawned daemon, usually *not* the
    /// process anyone cares about -- the broker exits as soon as it has
    /// handed the daemon over, and the session the daemon should follow is
    /// somewhere else entirely. Such a caller wants [`Self::Process`] and the
    /// weaker enforcement that comes with it.
    Spawner,
    /// A nominated process that is not necessarily this one.
    ///
    /// No host will enforce this in the kernel on the spawner's say-so, so it
    /// always resolves to [`LifetimeEnforcement::Watcher`].
    Process(ProcessId),
}

/// Which enforcement a lifetime binding actually obtained.
///
/// A caller that needs a hard guarantee reads this and refuses, rather than
/// assuming the strongest and discovering otherwise from a machine full of
/// orphans. The three are genuinely different promises and the asymmetry
/// between hosts is real, so it is reported rather than smoothed over.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LifetimeEnforcement {
    /// The kernel destroys the child along with its owner.
    ///
    /// Windows job objects with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The
    /// strongest of the three: it covers descendants, and it holds however the
    /// owner dies, including a `TerminateProcess` that runs no user code.
    KernelContainer,
    /// The kernel signals the child when its *parent* exits.
    ///
    /// Linux `PR_SET_PDEATHSIG`. Nothing in user space has to still be alive
    /// for this to fire, but it covers the direct child only, and it is tied
    /// to the parent -- more precisely to the spawning *thread*, so a spawn
    /// performed on a pool thread that later retires fires it early. Spawn
    /// from a thread that lives as long as the owner should.
    ParentDeathSignal,
    /// Something in user space watches the owner and terminates the child.
    ///
    /// The reaping is only as reliable as the watcher: if the watching
    /// process is killed too, nothing runs. Reuse-safe all the same -- the
    /// watch is a [`ProcessExitWatch`], not a repeated question about a
    /// number, and the kill is addressed by [`ProcessIdentity`].
    Watcher,
}

impl LifetimeEnforcement {
    /// Whether the guarantee survives the loss of every user-space participant.
    ///
    /// True for both kernel mechanisms and false for the watcher. This is the
    /// predicate a caller needing a hard guarantee should branch on; which of
    /// the two kernel mechanisms it got is not something it can act on.
    #[must_use]
    pub const fn is_kernel_enforced(self) -> bool {
        matches!(self, Self::KernelContainer | Self::ParentDeathSignal)
    }
}

/// What this host would enforce for `owner`, without spawning anything.
///
/// Ask before committing: a caller that cannot accept
/// [`LifetimeEnforcement::Watcher`] should find that out before it has a
/// child to clean up.
#[must_use]
pub fn lifetime_enforcement_for(owner: LifetimeOwner) -> LifetimeEnforcement {
    match owner {
        LifetimeOwner::Spawner => crate::process_spawner_lifetime_enforcement(),
        LifetimeOwner::Process(_) => LifetimeEnforcement::Watcher,
    }
}

/// A standing request from the host that this process shut down.
///
/// Hosts deliver this differently -- a POSIX signal, a Windows console
/// control event injected on a thread of the OS's choosing -- but both arrive
/// in a context where almost nothing is safe to do. A handler may not
/// allocate, log, take a lock, or join a thread. So neither host runs the
/// caller's code: each sets one flag, and the caller reads it whenever it is
/// somewhere it can act.
///
/// That is why this is a poll rather than a callback. A callback would invite
/// exactly the work the delivery context forbids.
pub struct ShutdownRequest {
    flag: &'static std::sync::atomic::AtomicBool,
}

impl ShutdownRequest {
    /// Build a handle watching a flag the caller already owns.
    ///
    /// The host implementations use this to hand out a view of their own
    /// static. It is public because a caller that already has a shutdown flag
    /// -- one set by a supervisor protocol, or by a test -- can present it
    /// through the same type rather than the loop it feeds needing two shapes
    /// of "should I stop".
    ///
    /// `'static` is not incidental: a handler set by the OS outlives any
    /// scope, so the flag it writes has to as well.
    pub fn watching(flag: &'static std::sync::atomic::AtomicBool) -> Self {
        Self { flag }
    }

    /// Whether the host has asked this process to shut down.
    ///
    /// Latching, not edge-triggered: once true it stays true, so a caller that
    /// checks between two pieces of work cannot miss a request delivered while
    /// it was busy.
    pub fn requested(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl std::fmt::Debug for ShutdownRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShutdownRequest")
            .field("requested", &self.requested())
            .finish()
    }
}

pub use crate::process_install_shutdown_request_handler as install_shutdown_request_handler;

/// Whether this host can replace the running image with another program.
///
/// Unix can: `execve` keeps the process -- its PID, its open descriptors,
/// its place in the process tree -- and swaps the program underneath.
/// Windows has no equivalent; the nearest thing is starting a successor and
/// exiting, which is a *different* process with a different PID and does not
/// keep anything a parent or supervisor was holding onto.
///
/// Callers that can accept a successor should ask this and fall back. Callers
/// that genuinely need the same process to continue have no fallback, and
/// should treat `false` as unsupported rather than approximating it.
pub use crate::{
    process_can_replace_current_image as can_replace_current_image,
    process_replace_current_image as replace_current_image,
};

/// Make a synchronous child lead its own process group before it starts.
///
/// Unix calls `setsid` in the child, so it leads a new session and process
/// group whose id is its PID. Windows sets `CREATE_NEW_PROCESS_GROUP`, the
/// console-control group; on Windows this *replaces* any creation flags the
/// command already carries, as [`std::os::windows::process::CommandExt`]
/// does. No descriptor, stdio, owner-death, or resource policy is added.
pub fn configure_session_leader_command(command: &mut std::process::Command) {
    crate::process_configure_session_leader_command(command);
}

/// Force-kill the process group led by `child`, an unreaped child of this
/// process configured with [`configure_session_leader_command`].
///
/// The target is proven rather than trusted. Borrowing the owner's
/// [`std::process::Child`] -- not a bare PID -- and checking with a
/// non-consuming `waitid(WNOWAIT)` that the child is still unreaped is what
/// keeps its PID, and therefore the group id it leads, from having been
/// reissued: a numeric PID read from anywhere else is exactly the recycled
/// target [`DaemonIdentity::verify_for_control`] exists to refuse, so no
/// PID-only spelling of this operation is offered. Call it before reaping the
/// child; once reaped it reports [`std::io::ErrorKind::NotFound`] and signals
/// nothing. A group that has already fully exited is `Ok(())`.
///
/// Windows has no numeric process-group kill primitive and returns
/// [`std::io::ErrorKind::Unsupported`]; capture the child's
/// [`ProcessIdentity`] and use [`kill_tree`] there.
///
/// [`DaemonIdentity::verify_for_control`]: https://docs.rs/kernal-api/latest/kernal_api/daemon_identity/struct.DaemonIdentity.html#method.verify_for_control
pub fn force_terminate_process_group(child: &std::process::Child) -> std::io::Result<()> {
    crate::process_force_terminate_child_process_group(child)
}

/// Move exactly `identity` into a scheduling band after it has started.
///
/// Prefer [`SpawnSpec::priority`](crate::SpawnSpec::priority) where the band
/// is known at launch. [`ProcessPriority::Normal`] leaves the process's
/// current policy untouched. The generation is revalidated first: a reissued
/// PID is [`ProcessIdentityActionError::StaleIdentity`] and an exited one is
/// [`ProcessIdentityAction::AlreadyExited`]. Windows checks and adjusts
/// through one handle, which pins the process. Unix has no descriptor-based
/// `setpriority`, so a process that exits, is reaped, and has its PID
/// reissued between the check and the call could be re-prioritised instead.
/// That window is tolerated here because a scheduling band is reversible and
/// non-destructive; termination offers no such window ([`force_kill`]).
pub fn set_priority(
    identity: ProcessIdentity,
    priority: crate::ProcessPriority,
) -> Result<ProcessIdentityAction, ProcessIdentityActionError> {
    crate::process_set_priority_identity(identity, priority)
}

/// Resolve the on-disk image of the process `process` refers to.
///
/// Addressed through a retained [`ProcessLiveness`] rather than a bare PID:
/// the path is read and then the same live reference is asked whether its
/// process is still running. A process that is still alive after the read
/// cannot have handed its PID to another image during it. An exited process
/// reports [`std::io::ErrorKind::NotFound`]. The path is the host's answer
/// and may name a deleted or replaced file.
pub fn executable_path(process: &ProcessLiveness) -> std::io::Result<std::path::PathBuf> {
    // Liveness is re-checked on both outcomes: some hosts (Windows) fail the
    // image query itself once the process has exited, with an uncategorized
    // OS error, and that must surface as the same NotFound as a read that
    // raced the exit.
    let read = crate::process_executable_path(process.pid());
    if !process.is_alive() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "process exited while its image path was read",
        ));
    }
    read
}

/// Replace this process's standard input, output, and error with the null
/// device.
///
/// Mutates process-global state. Intended only for a daemon's early bootstrap
/// path, before other threads use the standard streams.
pub fn detach_standard_streams() {
    crate::process_detach_standard_streams();
}

/// Point standard input at the null device and standard output and error at
/// `path`, opened for append (created if missing).
///
/// Returns `false`, leaving output and error unchanged, when `path` cannot be
/// opened; input has already been detached by then. Mutates process-global
/// state, like [`detach_standard_streams`].
#[must_use]
pub fn redirect_standard_streams_to_log(path: &std::path::Path) -> bool {
    crate::process_redirect_standard_streams_to_log(path)
}

/// Whether [`NativeJobserver::create`] can succeed on this host.
///
/// True on Linux and macOS (a GNU make `R,W` descriptor pair); false on
/// Windows, where GNU make's named-semaphore form is not provided.
#[must_use]
pub fn native_jobserver_supported() -> bool {
    crate::process_native_jobserver_supported()
}

/// A host-owned GNU make jobserver, primed with `capacity` tokens.
///
/// `NativeJobserver::create(capacity)` returns `InvalidInput` for zero and
/// `Unsupported` where [`native_jobserver_supported`] is false;
/// `auth_string()` is the `--jobserver-auth=R,W` descriptor pair. Both
/// descriptors are close-on-exec: a launcher hands them to a child
/// deliberately rather than leaking them into every spawn.
pub use crate::NativeJobserver;

/// Opaque, monotonically increasing CPU time consumed by `pid`.
///
/// Only differences between two readings of the same process mean anything
/// (Linux clock ticks, macOS `rusage` time, Windows 100 ns units). `None`
/// when the process is gone or the host denies the reading. Addressed by
/// PID: read-only telemetry, sound for a child the caller has not reaped.
#[must_use]
pub fn cpu_ticks_for_pid(pid: u32) -> Option<u64> {
    crate::process_cpu_ticks_for_pid(pid)
}

/// Resident-memory high-water mark of `pid`, in bytes.
///
/// Linux reads `VmHWM`, macOS the lifetime maximum physical footprint, and
/// Windows `PeakWorkingSetSize`. `None` when the process is gone or its
/// accounting was already torn down (a Unix zombie); see
/// [`PEAK_RSS_READABLE_AFTER_EXIT`]. Read-only telemetry addressed by PID,
/// sound for a child the caller has not reaped.
#[must_use]
pub fn peak_rss_bytes_for_pid(pid: u32) -> Option<u64> {
    crate::process_peak_rss_bytes_for_pid(pid)
}

/// Current resident bytes of `pid` plus every live descendant.
///
/// A current figure, not a high-water mark (Linux `VmRSS`, macOS
/// `ri_resident_size`, Windows `WorkingSetSize`); sampling repeatedly and
/// keeping the maximum yields a sampled peak for the whole tree, which is how
/// a grandchild such as a linker becomes visible. Descendants come from
/// `/proc/<pid>/task/*/children`, `proc_listchildpids`, or one Toolhelp32
/// snapshot, and the walk visits at most [`MAX_TREE_RSS_PROCESSES`]. `None`
/// when `pid` itself cannot be read.
#[must_use]
pub fn tree_rss_bytes_for_pid(pid: u32) -> Option<u64> {
    crate::process_tree_rss_bytes_for_pid(pid)
}

/// Upper bound on processes one [`tree_rss_bytes_for_pid`] call visits, so a
/// fork bomb cannot turn a sample into an unbounded walk.
pub const MAX_TREE_RSS_PROCESSES: usize = crate::PROCESS_MAX_TREE_RSS_PROCESSES;

/// Whether [`peak_rss_bytes_for_pid`] stays exact for a child that has
/// exited while its owner still holds it. True on Windows, where the retained
/// handle keeps the process object readable; false on Unix, where a zombie's
/// memory accounting is gone.
pub const PEAK_RSS_READABLE_AFTER_EXIT: bool = crate::PROCESS_PEAK_RSS_READABLE_AFTER_EXIT;

/// Whether a process with this PID is running right now (#545).
///
/// A one-shot probe for a PID read from somewhere else, such as a daemon's
/// pid file: it opens a [`ProcessLiveness`] reference, asks it, and drops
/// it, with no process-table snapshot. A process this one may not inspect
/// still exists, so a refusal from the host reports `true`; an out-of-range
/// or unused PID reports `false`. The answer can be stale the moment it
/// returns, and a recycled PID reads as alive: keep the [`ProcessLiveness`]
/// itself when the same process must be observed more than once.
pub fn is_alive(pid: u32) -> bool {
    match ProcessLiveness::open(pid) {
        Ok(process) => process.is_alive(),
        Err(error) => matches!(error.kind, ProcessInspectErrorKind::Host),
    }
}
