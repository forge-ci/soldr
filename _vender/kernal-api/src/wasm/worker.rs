//! Facade vocabulary and parent-only bookkeeping for phase-D worker supervision.
//!
//! Process launch, protocol I/O, and forced containment remain intentionally
//! absent here; this module only owns semantic configuration and lifecycle data.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

mod output;

#[cfg(feature = "wasm-sketch-worker-test-support")]
mod test_support;

#[cfg(test)]
use super::worker_protocol::TerminalDetail;
use super::worker_protocol::{
    self, ExecuteMetadata, FinalCounters, Message, RootOutcome, TerminalKind,
};
use super::{
    AdmittedSketch, RootExecutionPermit, SketchCompilerConfig, SketchExecutionError,
    SketchModulePolicy, ThreadSpawnRejectionSummary, ThreadedRootOutcome,
};

const MAX_COOPERATIVE_CANCEL_GRACE: Duration = Duration::from_secs(60);

/// Explicit worker executable and bounded grace before a future supervisor
/// escalates to process containment. No executable discovery occurs here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SketchWorkerConfig {
    executable: PathBuf,
    cooperative_cancel_grace: Duration,
    output_destination: Option<PathBuf>,
    webview_url: Option<String>,
    #[cfg(feature = "tauri-webview-test-support")]
    trace: Option<SketchWorkerTrace>,
}
impl SketchWorkerConfig {
    pub fn new(
        executable: PathBuf,
        cooperative_cancel_grace: Duration,
    ) -> Result<Self, SketchWorkerFailure> {
        if executable.as_os_str().is_empty()
            || !executable.is_absolute()
            || cooperative_cancel_grace.is_zero()
            || cooperative_cancel_grace > MAX_COOPERATIVE_CANCEL_GRACE
        {
            return Err(SketchWorkerFailure::InvalidConfiguration);
        }
        Ok(Self {
            executable,
            cooperative_cancel_grace,
            output_destination: None,
            webview_url: None,
            #[cfg(feature = "tauri-webview-test-support")]
            trace: None,
        })
    }
    /// Authorize one exact final output. The worker receives only a private
    /// staging destination; publication follows successful execution and reap.
    pub fn with_output_destination(
        mut self,
        destination: PathBuf,
    ) -> Result<Self, SketchWorkerFailure> {
        if !destination.is_absolute() || destination.file_name().is_none() {
            return Err(SketchWorkerFailure::InvalidConfiguration);
        }
        self.output_destination = Some(destination);
        Ok(self)
    }
    pub fn executable(&self) -> &Path {
        &self.executable
    }
    /// Retain one bounded diagnostic batch without running caller code or
    /// writing files on the deadline-owning supervisor thread.
    #[cfg(feature = "tauri-webview-test-support")]
    pub fn with_trace(mut self, trace: SketchWorkerTrace) -> Self {
        self.trace = Some(trace);
        self
    }
    /// Authorize native capture of one validated URL into one exact output.
    /// The executable must be built with `tauri-webview`. Renderer processes
    /// remain inside the worker tree; Windows permits at most 16 processes
    /// for this mode. Unix process groups provide tree cleanup, not a count cap.
    #[cfg(feature = "tauri-webview")]
    pub fn with_webview_capture(
        self,
        grant: crate::webview::WebviewUrlGrant,
        destination: PathBuf,
    ) -> Result<Self, SketchWorkerFailure> {
        let mut config = self.with_output_destination(destination)?;
        config.webview_url = Some(grant.worker_url().to_owned());
        Ok(config)
    }

    fn process_limits(&self) -> crate::platform::process::WorkerLimits {
        crate::platform::process::WorkerLimits {
            active_processes: Some(if self.webview_url.is_some() { 16 } else { 1 }),
            ..Default::default()
        }
    }
    pub fn cooperative_cancel_grace(&self) -> Duration {
        self.cooperative_cancel_grace
    }
}

/// Acceptance-only storage for at most one 64-KiB batch. Use one recorder per
/// concurrent execution. A later batch replaces a previously retained batch.
#[cfg(feature = "tauri-webview-test-support")]
#[derive(Clone, Debug, Default)]
pub struct SketchWorkerTrace(Arc<std::sync::Mutex<Option<String>>>);
#[cfg(feature = "tauri-webview-test-support")]
impl PartialEq for SketchWorkerTrace {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
#[cfg(feature = "tauri-webview-test-support")]
impl Eq for SketchWorkerTrace {}
#[cfg(feature = "tauri-webview-test-support")]
impl SketchWorkerTrace {
    pub fn take(&self) -> Option<String> {
        self.0.lock().expect("worker trace lock poisoned").take()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SketchWorkerStopReason {
    Cancelled,
    DeadlineExceeded,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SketchWorkerFailure {
    InvalidConfiguration,
    Launch,
    Protocol,
    UnexpectedExit,
    ContainmentCleanup,
    ChildFailure,
    WorkerReportedFailure,
    WorkerForcedContainment,
    OutputGrant,
    OutputCommit,
    /// Staging cleanup failed without publishing a new final file.
    OutputCleanup,
    /// The final file was atomically replaced, but staging cleanup failed.
    OutputCommittedCleanup,
}
impl SketchWorkerFailure {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidConfiguration => "worker-invalid-configuration",
            Self::Launch => "worker-launch",
            Self::Protocol => "worker-protocol",
            Self::UnexpectedExit => "worker-unexpected-exit",
            Self::ContainmentCleanup => "worker-containment-cleanup",
            Self::ChildFailure => "worker-child-failure",
            Self::WorkerReportedFailure => "worker-reported-failure",
            Self::WorkerForcedContainment => "worker-forced-containment",
            Self::OutputGrant => "worker-output-grant",
            Self::OutputCommit => "worker-output-commit",
            Self::OutputCleanup => "worker-output-cleanup",
            Self::OutputCommittedCleanup => "worker-output-committed-cleanup",
        }
    }
}
impl fmt::Display for SketchWorkerFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for SketchWorkerFailure {}

/// Semantic terminal vocabulary for a future worker supervisor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SketchWorkerTerminal {
    Completed(ThreadedRootOutcome),
    Stopped(SketchWorkerStopReason),
    ForcedContainment { trigger: SketchWorkerStopReason },
    Execution(SketchExecutionError),
    Failure(SketchWorkerFailure),
}
impl SketchWorkerTerminal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Completed(_) => "worker-completed",
            Self::Stopped(SketchWorkerStopReason::Cancelled) => "cancelled",
            Self::Stopped(SketchWorkerStopReason::DeadlineExceeded) => "deadline-exceeded",
            Self::ForcedContainment {
                trigger: SketchWorkerStopReason::Cancelled,
            } => "forced-containment-cancelled",
            Self::ForcedContainment {
                trigger: SketchWorkerStopReason::DeadlineExceeded,
            } => "forced-containment-deadline-exceeded",
            Self::Execution(error) => error.code(),
            Self::Failure(error) => error.code(),
        }
    }
}
impl fmt::Display for SketchWorkerTerminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// Parent-only counters. They never merge into the compiler/guest ledger.
#[allow(dead_code)] // Consumed by the phase-D supervisor; unit tests cover its accounting now.
#[derive(Default)]
pub(crate) struct WorkerExecutionLedger {
    spawned: AtomicU64,
    cancel_sent: AtomicU64,
    grace_expired: AtomicU64,
    forced: AtomicU64,
    reaped: AtomicU64,
    protocol_failures: AtomicU64,
    live_workers: AtomicU64,
    live_protocol_tasks: AtomicU64,
    pending_root_leases: AtomicU64,
}
#[allow(dead_code)] // Snapshot remains private until the phase-D supervisor owns it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SketchWorkerExecutionSnapshot {
    pub spawned: u64,
    pub cancel_sent: u64,
    pub grace_expired: u64,
    pub forced: u64,
    pub reaped: u64,
    pub protocol_failures: u64,
    pub live_workers: u64,
    pub live_protocol_tasks: u64,
    pub pending_root_leases: u64,
}
#[allow(dead_code)] // See WorkerExecutionLedger.
impl WorkerExecutionLedger {
    pub(crate) fn snapshot(&self) -> SketchWorkerExecutionSnapshot {
        SketchWorkerExecutionSnapshot {
            spawned: self.spawned.load(Ordering::Relaxed),
            cancel_sent: self.cancel_sent.load(Ordering::Relaxed),
            grace_expired: self.grace_expired.load(Ordering::Relaxed),
            forced: self.forced.load(Ordering::Relaxed),
            reaped: self.reaped.load(Ordering::Relaxed),
            protocol_failures: self.protocol_failures.load(Ordering::Relaxed),
            live_workers: self.live_workers.load(Ordering::Relaxed),
            live_protocol_tasks: self.live_protocol_tasks.load(Ordering::Relaxed),
            pending_root_leases: self.pending_root_leases.load(Ordering::Relaxed),
        }
    }
    pub(crate) fn record_spawned(&self) {
        self.spawned.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn record_cancel_sent(&self) {
        self.cancel_sent.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn record_grace_expired(&self) {
        self.grace_expired.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn record_forced(&self) {
        self.forced.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn record_reaped(&self) {
        self.reaped.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn record_protocol_failure(&self) {
        self.protocol_failures.fetch_add(1, Ordering::Relaxed);
    }
    fn live_worker(self: &Arc<Self>) -> WorkerGauge {
        self.live_workers.fetch_add(1, Ordering::Relaxed);
        WorkerGauge {
            ledger: Arc::clone(self),
            kind: GaugeKind::Worker,
            #[cfg(test)]
            drop_notification: None,
        }
    }
    fn live_protocol(self: &Arc<Self>) -> WorkerGauge {
        self.live_protocol_tasks.fetch_add(1, Ordering::Relaxed);
        WorkerGauge {
            ledger: Arc::clone(self),
            kind: GaugeKind::Protocol,
            #[cfg(test)]
            drop_notification: None,
        }
    }
    fn live_lease(self: &Arc<Self>) -> WorkerGauge {
        self.pending_root_leases.fetch_add(1, Ordering::Relaxed);
        WorkerGauge {
            ledger: Arc::clone(self),
            kind: GaugeKind::Lease,
            #[cfg(test)]
            drop_notification: None,
        }
    }
}
enum GaugeKind {
    Worker,
    Protocol,
    Lease,
}
struct WorkerGauge {
    ledger: Arc<WorkerExecutionLedger>,
    kind: GaugeKind,
    #[cfg(test)]
    drop_notification: Option<std::sync::mpsc::Sender<()>>,
}
impl Drop for WorkerGauge {
    fn drop(&mut self) {
        match self.kind {
            GaugeKind::Worker => {
                self.ledger.live_workers.fetch_sub(1, Ordering::Relaxed);
            }
            GaugeKind::Protocol => {
                self.ledger
                    .live_protocol_tasks
                    .fetch_sub(1, Ordering::Relaxed);
            }
            GaugeKind::Lease => {
                self.ledger
                    .pending_root_leases
                    .fetch_sub(1, Ordering::Relaxed);
            }
        }
        #[cfg(test)]
        if let Some(notification) = self.drop_notification.take() {
            let _ = notification.send(());
        }
    }
}

/// A logical parent admission permit only; it never prepares Wasmtime state.
#[allow(dead_code)] // Phase-D acquires this parent-only logical root permit.
pub(crate) struct WorkerParentRootLease {
    _permit: RootExecutionPermit,
}
#[allow(dead_code)] // Retained source/config/policy are consumed by the phase-D worker supervisor.
impl AdmittedSketch {
    pub fn worker_execution_snapshot(&self) -> SketchWorkerExecutionSnapshot {
        self.worker_ledger.snapshot()
    }
    pub async fn execute_threaded_root_contained(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        config: &SketchWorkerConfig,
    ) -> SketchWorkerTerminal {
        let source = crate::async_engine::CancellationSource::new();
        self.execute_threaded_root_contained_cancellable(runtime, config, source.token())
            .await
    }
    pub async fn execute_threaded_root_contained_cancellable(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        config: &SketchWorkerConfig,
        cancellation: crate::async_engine::CancellationToken,
    ) -> SketchWorkerTerminal {
        let sketch = Arc::clone(self);
        let config = config.clone();
        match runtime
            .launch_blocking(move || supervise(&sketch, config, cancellation))
            .await
        {
            Ok(value) => value,
            Err(_) => SketchWorkerTerminal::Failure(SketchWorkerFailure::UnexpectedExit),
        }
    }
    pub(crate) fn worker_source(&self) -> Arc<[u8]> {
        Arc::clone(&self.worker_source)
    }
    pub(crate) fn worker_compiler_config(&self) -> SketchCompilerConfig {
        self.worker_compiler_config
    }
    pub(crate) fn worker_policy(&self) -> SketchModulePolicy {
        self.worker_policy
    }
    pub(crate) fn acquire_worker_parent_root_lease(
        &self,
    ) -> Result<WorkerParentRootLease, SketchExecutionError> {
        Ok(WorkerParentRootLease {
            _permit: self.execution_ledger.acquire_root()?,
        })
    }
}

enum WriterCommand {
    Hello {
        request_id: u64,
    },
    Upload {
        request_id: u64,
        source: Arc<[u8]>,
        metadata: Box<ExecuteMetadata>,
    },
    Cancel {
        request_id: u64,
    },
    Close,
}
enum WriterEvent {
    Hello(Result<(), ()>),
    Upload(Result<(), ()>),
    Cancel(Result<(), ()>),
}

/// A worker whose pipe fails is usually a worker that is exiting: on every
/// supported platform a child's descriptors close while it tears down, before
/// the parent can observe its exit status.  This bound covers only that
/// in-kernel transition, so a worker that closes its pipe yet keeps running is
/// still forced and contained once it elapses.
const EXIT_OBSERVATION_BOUND: Duration = Duration::from_secs(1);

/// The parent-side protocol plumbing for one worker request: both helper
/// threads and the channels carrying their results.  Keeping them together
/// lets every terminal path drain what the worker already reported before it
/// classifies the outcome.
struct ProtocolLanes {
    write_tx: std::sync::mpsc::Sender<WriterCommand>,
    write_done_rx: std::sync::mpsc::Receiver<WriterEvent>,
    read_rx: std::sync::mpsc::Receiver<Result<Message, ()>>,
    writer: std::thread::JoinHandle<()>,
    reader: std::thread::JoinHandle<()>,
}

impl ProtocolLanes {
    /// Close the writer lane and join both helpers.  Callers reach this only
    /// once the worker is gone or contained, so the reader observes its pipe
    /// EOF and finishes on its own.
    fn close(self) {
        let _ = self.write_tx.send(WriterCommand::Close);
        let _ = self.writer.join();
        let _ = self.reader.join();
    }
}

/// Parent-side receive sequencing for one worker request.  This is deliberately
/// separate from cancellation/deadline selection: it accepts only protocol
/// facts, then the supervisor decides how to contain a rejected worker.
struct ParentReceivePhase {
    request_id: u64,
    hello_acked: bool,
    upload_queued: bool,
    upload_complete: bool,
    execute_ack_deferred: bool,
    execute_acked: bool,
    trace_received: bool,
    #[cfg(feature = "tauri-webview-test-support")]
    trace: Option<SketchWorkerTrace>,
}

#[derive(Debug, Eq, PartialEq)]
enum ParentReceiveAction {
    QueueUpload,
    AwaitingUpload,
    ExecuteAcknowledged,
    TraceReceived,
    Terminal(Box<Message>),
}

impl ParentReceivePhase {
    fn new(request_id: u64) -> Self {
        Self {
            request_id,
            hello_acked: false,
            upload_queued: false,
            upload_complete: false,
            execute_ack_deferred: false,
            execute_acked: false,
            trace_received: false,
            #[cfg(feature = "tauri-webview-test-support")]
            trace: None,
        }
    }

    fn upload_queued(&mut self) {
        self.upload_queued = true;
    }

    fn upload_complete(&self) -> bool {
        self.upload_complete
    }

    fn is_upload_queued(&self) -> bool {
        self.upload_queued
    }

    /// Records the writer result. An upload error is terminal regardless of
    /// what the read side has already received.
    fn upload(&mut self, result: Result<(), ()>) -> Result<(), SketchWorkerFailure> {
        if result.is_err() || !self.hello_acked || !self.upload_queued || self.upload_complete {
            return Err(SketchWorkerFailure::Protocol);
        }
        self.upload_complete = true;
        if self.execute_ack_deferred {
            self.execute_acked = true;
        }
        Ok(())
    }

    /// Accepts only the worker-to-parent sequence for this request. In
    /// particular, `ExecuteAck` proves the worker assembled the complete
    /// module; a terminal before it is always a protocol failure.
    fn receive(&mut self, message: Message) -> Result<ParentReceiveAction, SketchWorkerFailure> {
        if message.request_id() != self.request_id {
            return Err(SketchWorkerFailure::Protocol);
        }
        match message {
            Message::Trace { text, .. } if self.execute_acked && !self.trace_received => {
                self.trace_received = true;
                #[cfg(feature = "tauri-webview-test-support")]
                if let Some(trace) = &self.trace {
                    *trace.0.lock().expect("worker trace lock poisoned") = Some(text);
                }
                #[cfg(not(feature = "tauri-webview-test-support"))]
                let _ = text;
                Ok(ParentReceiveAction::TraceReceived)
            }
            Message::HelloAck { .. } if !self.hello_acked => {
                self.hello_acked = true;
                Ok(ParentReceiveAction::QueueUpload)
            }
            Message::ExecuteAck { .. }
                if self.hello_acked
                    && self.upload_queued
                    && !self.execute_ack_deferred
                    && !self.execute_acked =>
            {
                if self.upload_complete {
                    self.execute_acked = true;
                    Ok(ParentReceiveAction::ExecuteAcknowledged)
                } else {
                    self.execute_ack_deferred = true;
                    Ok(ParentReceiveAction::AwaitingUpload)
                }
            }
            message @ Message::Terminal { .. } if self.execute_acked => {
                Ok(ParentReceiveAction::Terminal(Box::new(message)))
            }
            _ => Err(SketchWorkerFailure::Protocol),
        }
    }
}

/// All parent-side resources whose lifetime must extend through deferred
/// containment.  Keeping these together prevents a failed force attempt from
/// synchronously dropping the native control or releasing admission early.
struct ExecutionOwnership {
    control: crate::platform::process::WorkerControl,
    output: Option<output::StagedOutput>,
    _lease: WorkerParentRootLease,
    _lease_gauge: WorkerGauge,
    _worker_gauge: WorkerGauge,
}

struct ActiveOwnership {
    ownership: Option<ExecutionOwnership>,
    cleanup: CleanupDispatcher,
}

impl std::ops::Deref for ActiveOwnership {
    type Target = crate::platform::process::WorkerControl;

    fn deref(&self) -> &Self::Target {
        &self
            .ownership
            .as_ref()
            .expect("active worker ownership")
            .control
    }
}

impl std::ops::DerefMut for ActiveOwnership {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self
            .ownership
            .as_mut()
            .expect("active worker ownership")
            .control
    }
}

impl ActiveOwnership {
    fn take(&mut self) -> ExecutionOwnership {
        self.ownership.take().expect("active worker ownership")
    }
}

struct CleanupJob {
    ownership: ExecutionOwnership,
    writer_tx: Option<std::sync::mpsc::Sender<WriterCommand>>,
    writer: Option<std::thread::JoinHandle<()>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

struct CleanupDispatcher {
    sender: std::sync::mpsc::Sender<CleanupJob>,
}

impl CleanupDispatcher {
    fn start(ledger: Arc<WorkerExecutionLedger>) -> Result<Self, ()> {
        let (sender, receiver) = std::sync::mpsc::channel::<CleanupJob>();
        std::thread::Builder::new()
            .name("kernal-worker-cleanup".into())
            .spawn(move || {
                while let Ok(mut job) = receiver.recv() {
                    // A failed force leaves the contained process and both
                    // pipe owners alive. Retry until containment completes;
                    // only then can joining the helpers be bounded.
                    while job
                        .ownership
                        .control
                        .force_and_reap(Duration::from_secs(5))
                        .is_err()
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    ledger.record_forced();
                    ledger.record_reaped();
                    if let Some(writer_tx) = job.writer_tx.take() {
                        let _ = writer_tx.send(WriterCommand::Close);
                    }
                    if let Some(writer) = job.writer.take() {
                        let _ = writer.join();
                    }
                    if let Some(reader) = job.reader.take() {
                        let _ = reader.join();
                    }
                    // `job` drops here, releasing the root lease and gauges.
                }
            })
            .map_err(|_| ())?;
        Ok(Self { sender })
    }

    fn hand_off(&self, job: CleanupJob) {
        if let Err(error) = self.sender.send(job) {
            // The dispatcher is intentionally durable. A dead receiver is an
            // impossible terminal condition; leaking is safer than allowing
            // `WorkerControl::drop` to synchronously invoke shutdown.
            std::mem::forget(error.0);
        }
    }

    fn hand_off_pre_protocol(&self, ownership: ExecutionOwnership) {
        self.hand_off(CleanupJob {
            ownership,
            writer_tx: None,
            writer: None,
            reader: None,
        });
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
fn supervise(
    sketch: &AdmittedSketch,
    config: SketchWorkerConfig,
    cancellation: crate::async_engine::CancellationToken,
) -> SketchWorkerTerminal {
    #[cfg(feature = "tauri-webview-test-support")]
    if let Some(trace) = &config.trace {
        let _ = trace.take();
    }
    // This authority begins before admission, spawning, and the handshake.
    let deadline = std::time::Instant::now()
        + sketch
            .worker_compiler_config()
            .execution_limits()
            .epoch_limits()
            .wall_clock_deadline();
    if cancellation.is_cancelled() {
        return SketchWorkerTerminal::Stopped(SketchWorkerStopReason::Cancelled);
    }
    if validate_worker_module_len(sketch.module_bytes()).is_err() {
        return SketchWorkerTerminal::Failure(SketchWorkerFailure::InvalidConfiguration);
    }
    let _lease = match sketch.acquire_worker_parent_root_lease() {
        Ok(lease) => lease,
        Err(error) => return SketchWorkerTerminal::Execution(error),
    };
    let _lease_gauge = sketch.worker_ledger.live_lease();
    if let Some(trigger) = selected_stop(&cancellation, deadline) {
        return SketchWorkerTerminal::Stopped(trigger);
    }
    let cleanup = match CleanupDispatcher::start(Arc::clone(&sketch.worker_ledger)) {
        Ok(dispatcher) => dispatcher,
        Err(()) => return SketchWorkerTerminal::Failure(SketchWorkerFailure::Launch),
    };
    let mut command = std::process::Command::new(config.executable());
    #[cfg(feature = "tauri-webview")]
    if config.webview_url.is_some() {
        crate::platform::process::configure_native_worker_environment(&mut command);
    }
    let output = match config
        .output_destination
        .as_deref()
        .map(output::StagedOutput::new)
        .transpose()
    {
        Ok(output) => output,
        Err(_) => return SketchWorkerTerminal::Failure(SketchWorkerFailure::OutputGrant),
    };
    let child = match spawn_worker(&mut command, config.process_limits()) {
        Ok(child) => child,
        Err(_) => return SketchWorkerTerminal::Failure(SketchWorkerFailure::Launch),
    };
    sketch.worker_ledger.record_spawned();
    let worker_gauge = sketch.worker_ledger.live_worker();
    let (native_control, stdin, stdout) = child.into_parts();
    let mut control = ActiveOwnership {
        ownership: Some(ExecutionOwnership {
            control: native_control,
            output,
            _lease,
            _lease_gauge,
            _worker_gauge: worker_gauge,
        }),
        cleanup,
    };
    let terminal = (|| {
        #[cfg(feature = "wasm-sketch-worker-test-support")]
        if let Err(()) = test_support::publish_worker_identity(control.id()) {
            // The marker is an all-or-nothing test observation point.  Do not
            // start protocol work when its observer cannot identify this exact
            // worker; the existing pre-protocol path contains and reaps it.
            return force_pre_protocol(sketch, &mut control, SketchWorkerFailure::UnexpectedExit);
        }
        let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
            return force_pre_protocol(sketch, &mut control, SketchWorkerFailure::Protocol);
        };
        let Some(id) = next_request_id() else {
            drop(stdin);
            drop(stdout);
            return force_pre_protocol(sketch, &mut control, SketchWorkerFailure::Protocol);
        };
        let (write_tx, write_rx) = std::sync::mpsc::channel();
        let (write_done_tx, write_done_rx) = std::sync::mpsc::channel();
        let writer_ledger = Arc::clone(&sketch.worker_ledger);
        let writer = std::thread::spawn(move || {
            let _gauge = writer_ledger.live_protocol();
            let mut input = stdin;
            while let Ok(command) = write_rx.recv() {
                let event = match command {
                    WriterCommand::Hello { request_id } => WriterEvent::Hello(
                        worker_protocol::write_message(&mut input, &Message::Hello { request_id })
                            .map_err(|_| ()),
                    ),
                    WriterCommand::Upload {
                        request_id,
                        source,
                        metadata,
                    } => {
                        let result = write_upload(&mut input, request_id, source, *metadata);
                        WriterEvent::Upload(result)
                    }
                    WriterCommand::Cancel { request_id } => WriterEvent::Cancel(
                        worker_protocol::write_message(&mut input, &Message::Cancel { request_id })
                            .map_err(|_| ()),
                    ),
                    WriterCommand::Close => break,
                };
                if write_done_tx.send(event).is_err() {
                    break;
                }
            }
        });
        let (read_tx, read_rx) = std::sync::mpsc::channel();
        let reader_ledger = Arc::clone(&sketch.worker_ledger);
        let reader = std::thread::spawn(move || {
            let _gauge = reader_ledger.live_protocol();
            forward_worker_responses(stdout, read_tx);
        });
        let lanes = ProtocolLanes {
            write_tx,
            write_done_rx,
            read_rx,
            writer,
            reader,
        };
        if lanes
            .write_tx
            .send(WriterCommand::Hello { request_id: id })
            .is_err()
        {
            return force_join_result(sketch, &mut control, lanes, SketchWorkerFailure::Protocol);
        }
        let mut cancel_written = false;
        let mut cancel_queued = false;
        let mut receive_phase = ParentReceivePhase::new(id);
        #[cfg(feature = "tauri-webview-test-support")]
        {
            receive_phase.trace = config.trace.clone();
        }
        let mut selected = None;
        let mut grace_deadline = None;
        loop {
            if selected.is_none() {
                selected = selected_stop(&cancellation, deadline);
            }
            if let Some(trigger) = selected {
                if !receive_phase.upload_complete() {
                    return force_join_terminal(sketch, &mut control, lanes, trigger);
                }
            }
            if let Ok(event) = lanes.write_done_rx.try_recv() {
                match event {
                    WriterEvent::Hello(Ok(())) => {}
                    WriterEvent::Upload(result) => {
                        // A failed write is a pipe fact, not a protocol fact: the
                        // worker may already have died under it.  A rejected but
                        // successful write is a real protocol violation.
                        let write_failed = result.is_err();
                        if receive_phase.upload(result).is_err() {
                            return if write_failed {
                                pipe_failure_result(
                                    sketch,
                                    &mut control,
                                    lanes,
                                    &mut receive_phase,
                                    id,
                                    selected,
                                )
                            } else {
                                force_join_result(
                                    sketch,
                                    &mut control,
                                    lanes,
                                    SketchWorkerFailure::Protocol,
                                )
                            };
                        }
                    }
                    WriterEvent::Cancel(Ok(())) => {
                        cancel_written = true;
                        sketch.worker_ledger.record_cancel_sent();
                        grace_deadline =
                            Some(std::time::Instant::now() + config.cooperative_cancel_grace());
                    }
                    WriterEvent::Hello(Err(())) | WriterEvent::Cancel(Err(())) => {
                        return pipe_failure_result(
                            sketch,
                            &mut control,
                            lanes,
                            &mut receive_phase,
                            id,
                            selected,
                        )
                    }
                }
            }
            if let Ok(result) = lanes.read_rx.try_recv() {
                let message = match result {
                    Ok(message) => message,
                    Err(()) => {
                        return pipe_failure_result(
                            sketch,
                            &mut control,
                            lanes,
                            &mut receive_phase,
                            id,
                            selected,
                        )
                    }
                };
                match receive_phase.receive(message) {
                    Ok(ParentReceiveAction::QueueUpload) => {
                        if lanes
                            .write_tx
                            .send(WriterCommand::Upload {
                                request_id: id,
                                source: sketch.worker_source(),
                                metadata: {
                                    let mut metadata = metadata(sketch, deadline);
                                    metadata.webview_url = config.webview_url.clone();
                                    metadata.staged_output = control
                                        .ownership
                                        .as_ref()
                                        .and_then(|owner| owner.output.as_ref())
                                        .map(output::StagedOutput::worker_destination);
                                    Box::new(metadata)
                                },
                            })
                            .is_err()
                        {
                            return force_join_result(
                                sketch,
                                &mut control,
                                lanes,
                                SketchWorkerFailure::Protocol,
                            );
                        }
                        receive_phase.upload_queued();
                    }
                    Ok(ParentReceiveAction::AwaitingUpload) => {}
                    Ok(ParentReceiveAction::ExecuteAcknowledged) => {}
                    Ok(ParentReceiveAction::TraceReceived) => {}
                    Ok(ParentReceiveAction::Terminal(message)) => {
                        let mapped = map_terminal(*message, id);
                        let mapped = match mapped {
                            Ok(value) => value,
                            Err(error) => {
                                return force_join_result(sketch, &mut control, lanes, error)
                            }
                        };
                        if matches!(
                            mapped,
                            SketchWorkerTerminal::Failure(SketchWorkerFailure::Protocol)
                        ) {
                            sketch.worker_ledger.record_protocol_failure();
                        }
                        match control.reap_clean(Duration::from_secs(5)) {
                            Ok(crate::platform::process::WorkerNormalReap::Clean) => {
                                lanes.close();
                                sketch.worker_ledger.record_reaped();
                                return selected.map_or(mapped, SketchWorkerTerminal::Stopped);
                            }
                            Ok(crate::platform::process::WorkerNormalReap::Nonzero) => {
                                lanes.close();
                                sketch.worker_ledger.record_reaped();
                                return SketchWorkerTerminal::Failure(
                                    SketchWorkerFailure::UnexpectedExit,
                                );
                            }
                            Err(_) => {
                                return force_join_result(
                                    sketch,
                                    &mut control,
                                    lanes,
                                    SketchWorkerFailure::ContainmentCleanup,
                                )
                            }
                        }
                    }
                    Err(_) => {
                        return force_join_result(
                            sketch,
                            &mut control,
                            lanes,
                            SketchWorkerFailure::Protocol,
                        )
                    }
                }
            }
            match control.try_wait() {
                Ok(Some(code)) => {
                    return exited_join_result(
                        sketch,
                        lanes,
                        &mut receive_phase,
                        id,
                        selected,
                        code == 0,
                    )
                }
                Ok(None) => {}
                Err(_) => {
                    return force_join_result(
                        sketch,
                        &mut control,
                        lanes,
                        SketchWorkerFailure::UnexpectedExit,
                    )
                }
            }
            if let Some(trigger) = selected {
                if !receive_phase.upload_complete() {
                    return force_join_terminal(sketch, &mut control, lanes, trigger);
                }
                if !cancel_written && !cancel_queued && receive_phase.is_upload_queued() {
                    if lanes
                        .write_tx
                        .send(WriterCommand::Cancel { request_id: id })
                        .is_err()
                    {
                        return force_join_result(
                            sketch,
                            &mut control,
                            lanes,
                            SketchWorkerFailure::Protocol,
                        );
                    }
                    cancel_queued = true;
                    grace_deadline =
                        Some(std::time::Instant::now() + config.cooperative_cancel_grace());
                }
                if let Some(grace_deadline) = grace_deadline {
                    if std::time::Instant::now() >= grace_deadline {
                        sketch.worker_ledger.record_grace_expired();
                        return force_join_terminal(sketch, &mut control, lanes, trigger);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    })();
    // Every terminal path either reaped the worker or handed the entire owner
    // to the durable cleanup dispatcher. Never extract staging from the latter.
    let output = control
        .ownership
        .as_mut()
        .and_then(|owner| owner.output.take());
    finish_output(output, terminal, || selected_stop(&cancellation, deadline))
}

fn finish_output(
    output: Option<output::StagedOutput>,
    terminal: SketchWorkerTerminal,
    mut stop: impl FnMut() -> Option<SketchWorkerStopReason>,
) -> SketchWorkerTerminal {
    let Some(output) = output else {
        return terminal;
    };
    let terminal = if matches!(terminal, SketchWorkerTerminal::Completed(_)) {
        stop().map_or(terminal, SketchWorkerTerminal::Stopped)
    } else {
        terminal
    };
    if matches!(terminal, SketchWorkerTerminal::Completed(_)) {
        match output.commit(stop) {
            Ok(finalized) if finalized.cleanup.is_ok() => finalized
                .stop
                .map_or(terminal, SketchWorkerTerminal::Stopped),
            Ok(finalized) if finalized.stop.is_some() => {
                SketchWorkerTerminal::Failure(SketchWorkerFailure::OutputCleanup)
            }
            Ok(_) => SketchWorkerTerminal::Failure(SketchWorkerFailure::OutputCommittedCleanup),
            Err(_) => SketchWorkerTerminal::Failure(SketchWorkerFailure::OutputCommit),
        }
    } else if output.discard().is_err() {
        SketchWorkerTerminal::Failure(SketchWorkerFailure::OutputCleanup)
    } else {
        terminal
    }
}

/// Validate the private process-transport limit without constraining direct
/// in-process admission/execution policy.
fn validate_worker_module_len(module_bytes: usize) -> Result<(), SketchWorkerFailure> {
    (u64::try_from(module_bytes)
        .ok()
        .filter(|bytes| *bytes <= worker_protocol::WORKER_PROTOCOL_MAX_MODULE_BYTES)
        .is_some())
    .then_some(())
    .ok_or(SketchWorkerFailure::InvalidConfiguration)
}

fn selected_stop(
    cancellation: &crate::async_engine::CancellationToken,
    deadline: std::time::Instant,
) -> Option<SketchWorkerStopReason> {
    select_stop(
        cancellation.is_cancelled(),
        std::time::Instant::now() >= deadline,
    )
}
fn select_stop(cancelled: bool, deadline_elapsed: bool) -> Option<SketchWorkerStopReason> {
    if cancelled {
        Some(SketchWorkerStopReason::Cancelled)
    } else if deadline_elapsed {
        Some(SketchWorkerStopReason::DeadlineExceeded)
    } else {
        None
    }
}
fn write_upload(
    input: &mut std::process::ChildStdin,
    request_id: u64,
    source: Arc<[u8]>,
    metadata: ExecuteMetadata,
) -> Result<(), ()> {
    worker_protocol::write_message(
        input,
        &Message::ExecuteStart {
            request_id,
            module_len: source.len() as u64,
            metadata,
        },
    )
    .map_err(|_| ())?;
    for (sequence, bytes) in source
        .chunks(worker_protocol::MAX_FRAME_PAYLOAD - 12)
        .enumerate()
    {
        let sequence = u32::try_from(sequence).map_err(|_| ())?;
        worker_protocol::write_message(
            input,
            &Message::ModuleChunk {
                request_id,
                sequence,
                bytes: bytes.to_vec(),
            },
        )
        .map_err(|_| ())?;
    }
    worker_protocol::write_message(input, &Message::ExecuteEnd { request_id }).map_err(|_| ())
}
fn force_result(
    ledger: &WorkerExecutionLedger,
    control: &mut crate::platform::process::WorkerControl,
    failure: SketchWorkerFailure,
) -> (SketchWorkerTerminal, bool) {
    if failure == SketchWorkerFailure::Protocol {
        ledger.record_protocol_failure();
    }
    match control.force_and_reap(Duration::from_secs(5)) {
        Ok(()) => {
            ledger.record_forced();
            ledger.record_reaped();
            (SketchWorkerTerminal::Failure(failure), true)
        }
        Err(_) => (
            SketchWorkerTerminal::Failure(SketchWorkerFailure::ContainmentCleanup),
            false,
        ),
    }
}
fn force_pre_protocol(
    sketch: &AdmittedSketch,
    control: &mut ActiveOwnership,
    failure: SketchWorkerFailure,
) -> SketchWorkerTerminal {
    force_pre_protocol_with_ledger(&sketch.worker_ledger, control, failure)
}
fn force_pre_protocol_with_ledger(
    ledger: &WorkerExecutionLedger,
    control: &mut ActiveOwnership,
    failure: SketchWorkerFailure,
) -> SketchWorkerTerminal {
    let (result, forced) = force_result(ledger, &mut *control, failure);
    if !forced {
        let ownership = control.take();
        control.cleanup.hand_off_pre_protocol(ownership);
    }
    result
}
fn force_join_result(
    sketch: &AdmittedSketch,
    control: &mut ActiveOwnership,
    lanes: ProtocolLanes,
    failure: SketchWorkerFailure,
) -> SketchWorkerTerminal {
    force_join_result_with_ledger(&sketch.worker_ledger, control, lanes, failure)
}
fn force_join_result_with_ledger(
    ledger: &WorkerExecutionLedger,
    control: &mut ActiveOwnership,
    lanes: ProtocolLanes,
    failure: SketchWorkerFailure,
) -> SketchWorkerTerminal {
    let (result, forced) = force_result(ledger, &mut *control, failure);
    if !forced {
        // The dispatcher owns the unreaped child and both helpers from this
        // point. `ContainmentCleanup` means cleanup remains pending; gauges
        // and the root lease intentionally stay live until its retry succeeds.
        let ownership = control.take();
        control.cleanup.hand_off(CleanupJob {
            ownership,
            writer_tx: Some(lanes.write_tx),
            writer: Some(lanes.writer),
            reader: Some(lanes.reader),
        });
        return result;
    }
    lanes.close();
    result
}
/// Classify a worker that has already been observed to exit.
///
/// The reader and writer helpers run concurrently with the supervisor loop, so
/// a worker that reported its outcome and then exited can be observed as gone
/// while that report is still in flight between the pipe and `read_rx`.
/// Joining both helpers first and then draining what they delivered keeps a
/// reported outcome from being demoted to `UnexpectedExit` purely because a
/// helper thread lost a scheduling race.  A worker that exited nonzero or by
/// signal remains an unexpected exit whatever it reported, as does a clean
/// exit with no well-formed terminal report.
fn exited_join_result(
    sketch: &AdmittedSketch,
    lanes: ProtocolLanes,
    receive_phase: &mut ParentReceivePhase,
    request_id: u64,
    selected: Option<SketchWorkerStopReason>,
    clean_exit: bool,
) -> SketchWorkerTerminal {
    let ProtocolLanes {
        write_tx,
        write_done_rx,
        read_rx,
        writer,
        reader,
    } = lanes;
    let _ = write_tx.send(WriterCommand::Close);
    let _ = writer.join();
    let _ = reader.join();
    sketch.worker_ledger.record_reaped();
    // Both helpers have finished, so every message they ever produced is now
    // queued.  Replay the writer results first: the receive phase defers an
    // `ExecuteAck` until the upload it acknowledges is confirmed written.
    while let Ok(event) = write_done_rx.try_recv() {
        if let WriterEvent::Upload(result) = event {
            if receive_phase.upload(result).is_err() {
                return SketchWorkerTerminal::Failure(SketchWorkerFailure::UnexpectedExit);
            }
        }
    }
    let mut reported = None;
    while let Ok(Ok(message)) = read_rx.try_recv() {
        match receive_phase.receive(message) {
            Ok(ParentReceiveAction::Terminal(message)) => {
                reported = Some(*message);
                break;
            }
            // The worker is already gone, so an upload can no longer be
            // queued; only its terminal report still carries information.
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let Some(message) = reported.filter(|_| clean_exit) else {
        return SketchWorkerTerminal::Failure(SketchWorkerFailure::UnexpectedExit);
    };
    match map_terminal(message, request_id) {
        Ok(mapped) => {
            if matches!(
                mapped,
                SketchWorkerTerminal::Failure(SketchWorkerFailure::Protocol)
            ) {
                sketch.worker_ledger.record_protocol_failure();
            }
            selected.map_or(mapped, SketchWorkerTerminal::Stopped)
        }
        Err(SketchWorkerFailure::Protocol) => {
            sketch.worker_ledger.record_protocol_failure();
            SketchWorkerTerminal::Failure(SketchWorkerFailure::Protocol)
        }
        Err(failure) => SketchWorkerTerminal::Failure(failure),
    }
}

/// Classify a worker whose pipe failed, either as reader EOF or as a failed
/// write.  A pipe failure is not itself a protocol fact: on every supported
/// platform the child's descriptors close during process teardown, before its
/// exit status becomes observable, so a single non-blocking `try_wait` here
/// reports a dead worker as a live one and mislabels its death as
/// `worker-protocol`.  Observe the exit within a bound first; only a worker
/// that keeps the pipe closed while still running is a protocol violation.
fn pipe_failure_result(
    sketch: &AdmittedSketch,
    control: &mut ActiveOwnership,
    lanes: ProtocolLanes,
    receive_phase: &mut ParentReceivePhase,
    request_id: u64,
    selected: Option<SketchWorkerStopReason>,
) -> SketchWorkerTerminal {
    match control.reap_clean(EXIT_OBSERVATION_BOUND) {
        Ok(reap) => exited_join_result(
            sketch,
            lanes,
            receive_phase,
            request_id,
            selected,
            matches!(reap, crate::platform::process::WorkerNormalReap::Clean),
        ),
        Err(_) => force_join_result(sketch, control, lanes, SketchWorkerFailure::Protocol),
    }
}
fn force_join_terminal(
    sketch: &AdmittedSketch,
    control: &mut ActiveOwnership,
    lanes: ProtocolLanes,
    trigger: SketchWorkerStopReason,
) -> SketchWorkerTerminal {
    let (result, forced) = force_result(
        &sketch.worker_ledger,
        &mut *control,
        SketchWorkerFailure::UnexpectedExit,
    );
    if !forced {
        let ownership = control.take();
        control.cleanup.hand_off(CleanupJob {
            ownership,
            writer_tx: Some(lanes.write_tx),
            writer: Some(lanes.writer),
            reader: Some(lanes.reader),
        });
        return result;
    }
    lanes.close();
    match result {
        SketchWorkerTerminal::Failure(SketchWorkerFailure::ContainmentCleanup) => result,
        _ => SketchWorkerTerminal::ForcedContainment { trigger },
    }
}

fn spawn_worker(
    command: &mut std::process::Command,
    limits: crate::platform::process::WorkerLimits,
) -> Result<crate::platform::process::WorkerChild, crate::platform::process::WorkerError> {
    crate::platform_imp::spawn_contained_worker(command, limits)
}

/// The entire response is at most four messages, not an open-ended stream. Keep the
/// channel nonblocking for teardown joins, but bound what its producer can
/// enqueue even when the supervisor is not scheduled. Module payloads are
/// never valid in this direction. The phase machine still validates ordering.
fn forward_worker_responses(
    mut output: impl std::io::Read,
    responses: std::sync::mpsc::Sender<Result<Message, ()>>,
) {
    for _ in 0..4 {
        let message = worker_protocol::read_message(&mut output)
            .map_err(|_| ())
            .and_then(|message| match message {
                Message::HelloAck { .. }
                | Message::ExecuteAck { .. }
                | Message::Trace { .. }
                | Message::Terminal { .. } => Ok(message),
                _ => Err(()),
            });
        let finished = message.is_err() || matches!(message, Ok(Message::Terminal { .. }));
        if responses.send(message).is_err() || finished {
            return;
        }
    }
    // Four nonterminal messages exhaust the response budget. Do not read or
    // allocate another frame, and wake the supervisor to force containment.
    let _ = responses.send(Err(()));
}
fn metadata(sketch: &AdmittedSketch, deadline: std::time::Instant) -> ExecuteMetadata {
    let config = sketch.worker_compiler_config();
    let limits = config.execution_limits();
    let fuel = limits.fuel_limits();
    let blobs = limits.blob_limits();
    let epoch = limits.epoch_limits();
    let remaining = deadline
        .saturating_duration_since(std::time::Instant::now())
        .max(Duration::from_millis(1));
    let policy = sketch.worker_policy();
    ExecuteMetadata {
        webview_url: None,
        staged_output: None,
        blob_limits: [
            blobs.maximum_chunk_bytes() as u64,
            blobs.maximum_blob_bytes() as u64,
            blobs.maximum_sketch_bytes() as u64,
            blobs.maximum_live_blobs() as u64,
            blobs.maximum_pending_reads() as u64,
            blobs.maximum_pending_writes() as u64,
            blobs.maximum_transfer_bytes() as u64,
        ],
        blob_progress_idle_timeout_secs: blobs.progress_idle_timeout().as_secs(),
        blob_progress_idle_timeout_nanos: u64::from(blobs.progress_idle_timeout().subsec_nanos()),
        max_wasm_stack_bytes: config.max_wasm_stack_bytes() as u64,
        reserved_memory_bytes: limits.maximum_reserved_shared_memory_bytes(),
        maximum_active_roots: limits.maximum_active_root_executions() as u64,
        total_fuel: fuel.total(),
        root_fuel: fuel.root_slice(),
        child_fuel: fuel.child_slice(),
        epoch_deadline_millis: remaining.as_millis().try_into().unwrap_or(u64::MAX),
        epoch_tick_millis: epoch
            .tick_interval()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
        maximum_epoch_registrations: epoch.maximum_active_registrations() as u64,
        max_module_bytes: policy.max_module_bytes() as u64,
        max_shared_memory_pages: policy.max_shared_memory_pages(),
        max_guest_threads: policy.max_guest_threads() as u64,
    }
}
fn map_terminal(message: Message, id: u64) -> Result<SketchWorkerTerminal, SketchWorkerFailure> {
    let Message::Terminal {
        request_id,
        kind,
        detail,
        counters,
        ..
    } = message
    else {
        return Err(SketchWorkerFailure::Protocol);
    };
    if request_id != id || !zero(counters) {
        return Err(SketchWorkerFailure::Protocol);
    }
    let rejected = ThreadSpawnRejectionSummary::from_worker_counts(
        detail.capacity_rejections,
        detail.closing_rejections,
        detail.fuel_rejections,
        detail.epoch_rejections,
    );
    let terminal = match kind {
        TerminalKind::Completed => match detail.root_outcome {
            RootOutcome::Started => SketchWorkerTerminal::Completed(ThreadedRootOutcome::Started),
            RootOutcome::Exited => SketchWorkerTerminal::Completed(ThreadedRootOutcome::Exited),
            RootOutcome::StartedWithThreadRejections => SketchWorkerTerminal::Completed(
                ThreadedRootOutcome::StartedWithThreadRejections(rejected),
            ),
            RootOutcome::ExitedWithThreadRejections => SketchWorkerTerminal::Completed(
                ThreadedRootOutcome::ExitedWithThreadRejections(rejected),
            ),
            RootOutcome::None => return Err(SketchWorkerFailure::Protocol),
        },
        TerminalKind::Cancelled => SketchWorkerTerminal::Stopped(SketchWorkerStopReason::Cancelled),
        TerminalKind::DeadlineExceeded => {
            SketchWorkerTerminal::Stopped(SketchWorkerStopReason::DeadlineExceeded)
        }
        TerminalKind::OutOfFuel => SketchWorkerTerminal::Execution(SketchExecutionError::OutOfFuel),
        TerminalKind::Trapped => SketchWorkerTerminal::Execution(SketchExecutionError::Trapped),
        TerminalKind::NonzeroExit => {
            SketchWorkerTerminal::Execution(SketchExecutionError::NonzeroExit {
                code: detail.status_code.ok_or(SketchWorkerFailure::Protocol)?,
            })
        }
        TerminalKind::ChildFailure => match detail.status_code {
            Some(code) => {
                SketchWorkerTerminal::Execution(SketchExecutionError::ChildNonzeroExit { code })
            }
            None => SketchWorkerTerminal::Failure(SketchWorkerFailure::ChildFailure),
        },
        TerminalKind::WorkerFailure => {
            SketchWorkerTerminal::Failure(SketchWorkerFailure::WorkerReportedFailure)
        }
        TerminalKind::ProtocolFailure => {
            SketchWorkerTerminal::Failure(SketchWorkerFailure::Protocol)
        }
        TerminalKind::ForcedContainment => {
            SketchWorkerTerminal::Failure(SketchWorkerFailure::WorkerForcedContainment)
        }
    };
    Ok(terminal)
}
fn zero(c: FinalCounters) -> bool {
    c.active_roots == 0
        && c.live_stores == 0
        && c.live_instances == 0
        && c.active_epoch_registrations == 0
        && c.live_threads == 0
}
static REQUEST_ID: AtomicU64 = AtomicU64::new(1);
fn next_request_id() -> Option<u64> {
    let value = REQUEST_ID
        .try_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
        .ok()?;
    (value != 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::super::{SketchCompiler, SketchExecutionSnapshot, THREADED_RUST_MAX_PAGES};
    use super::*;
    use crate::platform::process::{WorkerChild, WorkerChildControl, WorkerError, WorkerStage};
    use std::io;
    use std::sync::{mpsc, Condvar, Mutex};

    #[test]
    fn output_cleanup_failure_reports_whether_publication_occurred() {
        for publish in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let destination = directory.path().join("final");
            std::fs::write(&destination, b"original").unwrap();
            let output = output::StagedOutput::new(&destination)
                .unwrap()
                .with_cleanup_failure();
            std::fs::write(output.worker_destination(), b"completed").unwrap();
            let terminal = if publish {
                SketchWorkerTerminal::Completed(ThreadedRootOutcome::Started)
            } else {
                SketchWorkerTerminal::Execution(SketchExecutionError::Trapped)
            };
            let result = finish_output(Some(output), terminal, || None);
            assert_eq!(
                result.code(),
                if publish {
                    "worker-output-committed-cleanup"
                } else {
                    "worker-output-cleanup"
                }
            );
            assert_eq!(
                std::fs::read(&destination).unwrap(),
                if publish {
                    &b"completed"[..]
                } else {
                    &b"original"[..]
                }
            );
        }
    }

    #[test]
    fn parent_output_discards_on_failure_or_stop_and_commits_only_success() {
        for (terminal, stop, publish) in [
            (
                SketchWorkerTerminal::Completed(ThreadedRootOutcome::Started),
                None,
                true,
            ),
            (
                SketchWorkerTerminal::Completed(ThreadedRootOutcome::Started),
                Some(SketchWorkerStopReason::Cancelled),
                false,
            ),
            (
                SketchWorkerTerminal::Completed(ThreadedRootOutcome::Started),
                Some(SketchWorkerStopReason::DeadlineExceeded),
                false,
            ),
            (
                SketchWorkerTerminal::Execution(SketchExecutionError::Trapped),
                None,
                false,
            ),
            (
                SketchWorkerTerminal::ForcedContainment {
                    trigger: SketchWorkerStopReason::Cancelled,
                },
                None,
                false,
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let destination = directory.path().join("final");
            std::fs::write(&destination, b"original").unwrap();
            let output = output::StagedOutput::new(&destination).unwrap();
            std::fs::write(output.worker_destination(), b"completed").unwrap();
            let expected = if matches!(terminal, SketchWorkerTerminal::Completed(_)) {
                stop.map_or(terminal.clone(), SketchWorkerTerminal::Stopped)
            } else {
                terminal.clone()
            };
            assert_eq!(finish_output(Some(output), terminal, || stop), expected);
            assert_eq!(
                std::fs::read(&destination).unwrap(),
                if publish {
                    &b"completed"[..]
                } else {
                    &b"original"[..]
                }
            );
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        }
    }

    const TEST_WAIT: Duration = Duration::from_secs(2);

    #[test]
    fn cancellation_after_parent_sync_preserves_output() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("final");
        std::fs::write(&destination, b"original").unwrap();
        let cancellation = crate::async_engine::CancellationSource::new();
        let cancel_at_publication = cancellation.clone();
        let output = output::StagedOutput::new(&destination)
            .unwrap()
            .with_before_publish(move || cancel_at_publication.cancel());
        std::fs::write(output.worker_destination(), b"completed").unwrap();
        let deadline = std::time::Instant::now() + TEST_WAIT;
        let token = cancellation.token();
        let terminal = finish_output(
            Some(output),
            SketchWorkerTerminal::Completed(ThreadedRootOutcome::Started),
            || selected_stop(&token, deadline),
        );
        assert_eq!(
            terminal,
            SketchWorkerTerminal::Stopped(SketchWorkerStopReason::Cancelled)
        );
        assert_eq!(std::fs::read(&destination).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn deadline_after_parent_sync_preserves_output_and_reports_cleanup_failure() {
        for cleanup_fails in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let destination = directory.path().join("final");
            std::fs::write(&destination, b"original").unwrap();
            // Advance the deadline predicate only at the post-sync boundary,
            // independent of host scheduling and without a timing-based sleep.
            let expired = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let expire_at_publication = Arc::clone(&expired);
            let mut output = output::StagedOutput::new(&destination)
                .unwrap()
                .with_before_publish(move || {
                    expire_at_publication.store(true, Ordering::Release);
                });
            if cleanup_fails {
                output = output.with_cleanup_failure();
            }
            std::fs::write(output.worker_destination(), b"completed").unwrap();
            let terminal = finish_output(
                Some(output),
                SketchWorkerTerminal::Completed(ThreadedRootOutcome::Started),
                || select_stop(false, expired.load(Ordering::Acquire)),
            );
            assert!(
                expired.load(Ordering::Acquire),
                "post-sync boundary not reached"
            );
            assert_eq!(
                terminal,
                if cleanup_fails {
                    SketchWorkerTerminal::Failure(SketchWorkerFailure::OutputCleanup)
                } else {
                    SketchWorkerTerminal::Stopped(SketchWorkerStopReason::DeadlineExceeded)
                }
            );
            assert_eq!(std::fs::read(&destination).unwrap(), b"original");
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        }
    }

    struct DeferredFake {
        calls: Arc<AtomicU64>,
        shutdown_threads: Arc<Mutex<Vec<std::thread::ThreadId>>>,
        drop_threads: Arc<Mutex<Vec<std::thread::ThreadId>>>,
        first_failure: Mutex<Option<mpsc::Sender<()>>>,
        retry_gate: Arc<(Mutex<bool>, Condvar)>,
        successful_reap: Mutex<Option<mpsc::Sender<()>>>,
    }

    impl WorkerChildControl for DeferredFake {
        fn try_wait(&mut self) -> io::Result<Option<i32>> {
            Ok(None)
        }

        fn force_and_reap(&mut self, _timeout: Duration) -> Result<(), WorkerError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                if let Some(first_failure) = self
                    .first_failure
                    .lock()
                    .expect("first failure lock")
                    .take()
                {
                    let _ = first_failure.send(());
                }
                return Err(WorkerError::new(
                    WorkerStage::Reap,
                    io::Error::new(io::ErrorKind::TimedOut, "controlled first failure"),
                ));
            }
            let (released, wake) = &*self.retry_gate;
            let mut released = released.lock().expect("retry gate lock");
            while !*released {
                released = wake.wait(released).expect("retry gate poisoned");
            }
            if let Some(successful_reap) = self
                .successful_reap
                .lock()
                .expect("successful reap lock")
                .take()
            {
                let _ = successful_reap.send(());
            }
            Ok(())
        }

        fn shutdown(&mut self) {
            self.shutdown_threads
                .lock()
                .expect("shutdown threads lock")
                .push(std::thread::current().id());
        }
    }

    impl Drop for DeferredFake {
        fn drop(&mut self) {
            self.drop_threads
                .lock()
                .expect("drop threads lock")
                .push(std::thread::current().id());
        }
    }

    fn deferred_ownership(
        ledger: &Arc<WorkerExecutionLedger>,
        fake: DeferredFake,
        released: Option<mpsc::Sender<()>>,
    ) -> ExecutionOwnership {
        let execution_ledger = Arc::new(super::super::ExecutionLedger::new(
            super::super::SketchExecutionLimits::default(),
        ));
        let lease = WorkerParentRootLease {
            _permit: execution_ledger.acquire_root().expect("test root lease"),
        };
        let (control, _, _) = WorkerChild::new(None, None, 77, Box::new(fake)).into_parts();
        let mut worker_gauge = ledger.live_worker();
        worker_gauge.drop_notification = released;
        ExecutionOwnership {
            control,
            output: None,
            _lease: lease,
            _lease_gauge: ledger.live_lease(),
            _worker_gauge: worker_gauge,
        }
    }

    fn release_retry(retry_gate: &Arc<(Mutex<bool>, Condvar)>) {
        let (released, wake) = &**retry_gate;
        *released.lock().expect("retry gate lock") = true;
        wake.notify_all();
    }

    #[test]
    fn failed_force_handoff_returns_without_caller_shutdown() {
        let ledger = Arc::new(WorkerExecutionLedger::default());
        let (first_failure_tx, first_failure_rx) = mpsc::channel();
        let retry_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let shutdown_threads = Arc::new(Mutex::new(Vec::new()));
        let drop_threads = Arc::new(Mutex::new(Vec::new()));
        let ownership = deferred_ownership(
            &ledger,
            DeferredFake {
                calls: Arc::new(AtomicU64::new(0)),
                shutdown_threads: Arc::clone(&shutdown_threads),
                drop_threads: Arc::clone(&drop_threads),
                first_failure: Mutex::new(Some(first_failure_tx)),
                retry_gate: Arc::clone(&retry_gate),
                successful_reap: Mutex::new(None),
            },
            None,
        );
        let cleanup = CleanupDispatcher::start(Arc::clone(&ledger)).expect("cleanup dispatcher");
        let mut active = ActiveOwnership {
            ownership: Some(ownership),
            cleanup,
        };
        let caller = std::thread::current().id();
        let (writer_tx, writer_rx) = mpsc::channel();
        let (writer_closed_tx, writer_closed_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            assert!(matches!(writer_rx.recv(), Ok(WriterCommand::Close)));
            let _ = writer_closed_tx.send(());
        });
        let reader = std::thread::spawn(|| {});

        let (_write_done_tx, write_done_rx) = mpsc::channel();
        let (_read_tx, read_rx) = mpsc::channel();
        let terminal = force_join_result_with_ledger(
            &ledger,
            &mut active,
            ProtocolLanes {
                write_tx: writer_tx,
                write_done_rx,
                read_rx,
                writer,
                reader,
            },
            SketchWorkerFailure::Protocol,
        );
        assert_eq!(
            terminal,
            SketchWorkerTerminal::Failure(SketchWorkerFailure::ContainmentCleanup)
        );
        assert!(active.ownership.is_none());
        assert!(first_failure_rx.recv_timeout(TEST_WAIT).is_ok());
        assert!(shutdown_threads
            .lock()
            .expect("shutdown threads lock")
            .iter()
            .all(|id| *id != caller));
        assert!(drop_threads
            .lock()
            .expect("drop threads lock")
            .iter()
            .all(|id| *id != caller));
        release_retry(&retry_gate);
        assert!(writer_closed_rx.recv_timeout(TEST_WAIT).is_ok());
    }

    #[test]
    fn dispatcher_retry_releases_ownership_and_records_one_forced_reap() {
        let ledger = Arc::new(WorkerExecutionLedger::default());
        let (first_failure_tx, first_failure_rx) = mpsc::channel();
        let (successful_reap_tx, successful_reap_rx) = mpsc::channel();
        let (released_tx, released_rx) = mpsc::channel();
        let retry_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let calls = Arc::new(AtomicU64::new(0));
        let mut ownership = deferred_ownership(
            &ledger,
            DeferredFake {
                calls: Arc::clone(&calls),
                shutdown_threads: Arc::new(Mutex::new(Vec::new())),
                drop_threads: Arc::new(Mutex::new(Vec::new())),
                first_failure: Mutex::new(Some(first_failure_tx)),
                retry_gate: Arc::clone(&retry_gate),
                successful_reap: Mutex::new(Some(successful_reap_tx)),
            },
            Some(released_tx),
        );
        let directory = tempfile::tempdir().unwrap();
        let final_path = directory.path().join("final");
        std::fs::write(&final_path, b"original").unwrap();
        let output = output::StagedOutput::new(&final_path).unwrap();
        let staged = output.worker_destination();
        std::fs::write(&staged, b"uncommitted").unwrap();
        ownership.output = Some(output);
        let cleanup = CleanupDispatcher::start(Arc::clone(&ledger)).expect("cleanup dispatcher");
        cleanup.hand_off_pre_protocol(ownership);
        assert!(first_failure_rx.recv_timeout(TEST_WAIT).is_ok());
        assert!(staged.exists(), "staging must survive an unreaped worker");
        release_retry(&retry_gate);
        assert!(successful_reap_rx.recv_timeout(TEST_WAIT).is_ok());
        assert!(released_rx.recv_timeout(TEST_WAIT).is_ok());
        assert!(!staged.exists(), "staging must be discarded after reap");
        assert_eq!(std::fs::read(&final_path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(ledger.snapshot().forced, 1);
        assert_eq!(ledger.snapshot().reaped, 1);
        assert_eq!(ledger.snapshot().live_workers, 0);
        assert_eq!(ledger.snapshot().live_protocol_tasks, 0);
        assert_eq!(ledger.snapshot().pending_root_leases, 0);
    }

    #[test]
    fn closed_cleanup_receiver_leaks_whole_job_without_caller_shutdown() {
        let ledger = Arc::new(WorkerExecutionLedger::default());
        let shutdown_threads = Arc::new(Mutex::new(Vec::new()));
        let drop_threads = Arc::new(Mutex::new(Vec::new()));
        let retry_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let ownership = deferred_ownership(
            &ledger,
            DeferredFake {
                calls: Arc::new(AtomicU64::new(0)),
                shutdown_threads: Arc::clone(&shutdown_threads),
                drop_threads: Arc::clone(&drop_threads),
                first_failure: Mutex::new(None),
                retry_gate,
                successful_reap: Mutex::new(None),
            },
            None,
        );
        let (sender, receiver) = mpsc::channel();
        drop(receiver);
        let cleanup = CleanupDispatcher { sender };
        let caller = std::thread::current().id();
        cleanup.hand_off_pre_protocol(ownership);
        assert!(shutdown_threads
            .lock()
            .expect("shutdown threads lock")
            .iter()
            .all(|id| *id != caller));
        assert!(drop_threads
            .lock()
            .expect("drop threads lock")
            .iter()
            .all(|id| *id != caller));
        assert_eq!(ledger.snapshot().live_workers, 1);
        assert_eq!(ledger.snapshot().pending_root_leases, 1);
    }

    #[test]
    fn pre_protocol_failed_force_hands_off_without_pipe_helpers() {
        let ledger = Arc::new(WorkerExecutionLedger::default());
        let (first_failure_tx, first_failure_rx) = mpsc::channel();
        let retry_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let ownership = deferred_ownership(
            &ledger,
            DeferredFake {
                calls: Arc::new(AtomicU64::new(0)),
                shutdown_threads: Arc::new(Mutex::new(Vec::new())),
                drop_threads: Arc::new(Mutex::new(Vec::new())),
                first_failure: Mutex::new(Some(first_failure_tx)),
                retry_gate: Arc::clone(&retry_gate),
                successful_reap: Mutex::new(None),
            },
            None,
        );
        let cleanup = CleanupDispatcher::start(Arc::clone(&ledger)).expect("cleanup dispatcher");
        let mut active = ActiveOwnership {
            ownership: Some(ownership),
            cleanup,
        };
        let terminal =
            force_pre_protocol_with_ledger(&ledger, &mut active, SketchWorkerFailure::Protocol);
        assert_eq!(
            terminal,
            SketchWorkerTerminal::Failure(SketchWorkerFailure::ContainmentCleanup)
        );
        assert!(active.ownership.is_none());
        assert!(first_failure_rx.recv_timeout(TEST_WAIT).is_ok());
        release_retry(&retry_gate);
    }
    #[test]
    fn worker_config_requires_absolute_path_and_bounded_nonzero_grace() {
        assert_eq!(
            SketchWorkerConfig::new(PathBuf::from("worker"), Duration::from_secs(1)),
            Err(SketchWorkerFailure::InvalidConfiguration)
        );
        let absolute = std::env::current_dir()
            .expect("current directory")
            .join("worker");
        assert_eq!(
            SketchWorkerConfig::new(absolute.clone(), Duration::ZERO),
            Err(SketchWorkerFailure::InvalidConfiguration)
        );
        assert!(SketchWorkerConfig::new(absolute, Duration::from_millis(1)).is_ok());
    }

    #[test]
    fn response_reader_bounds_a_flood_without_waiting_for_the_consumer() {
        let frame = worker_protocol::encode(&Message::HelloAck { request_id: 1 }).unwrap();
        let input = frame.repeat(1_000);
        let mut cursor = std::io::Cursor::new(input);
        let (sender, receiver) = std::sync::mpsc::channel();
        forward_worker_responses(&mut cursor, sender);
        assert_eq!(cursor.position() as usize, 4 * frame.len());
        let queued: Vec<_> = receiver.try_iter().collect();
        assert_eq!(queued.len(), 5);
        assert!(queued[..4].iter().all(Result::is_ok));
        assert_eq!(queued[4], Err(()));
    }

    #[test]
    fn trace_requires_execution_ack_and_rejects_duplicates_or_wrong_request() {
        let trace = || Message::Trace {
            request_id: 1,
            text: "bounded trace\n".into(),
        };
        let mut phase = ParentReceivePhase::new(1);
        #[cfg(feature = "tauri-webview-test-support")]
        let recorder = SketchWorkerTrace::default();
        #[cfg(feature = "tauri-webview-test-support")]
        {
            phase.trace = Some(recorder.clone());
        }
        assert!(phase.receive(trace()).is_err());
        assert_eq!(
            phase.receive(Message::HelloAck { request_id: 1 }).unwrap(),
            ParentReceiveAction::QueueUpload
        );
        phase.upload_queued();
        phase.upload(Ok(())).unwrap();
        phase
            .receive(Message::ExecuteAck { request_id: 1 })
            .unwrap();
        assert!(phase
            .receive(Message::Trace {
                request_id: 2,
                text: "wrong request".into()
            })
            .is_err());
        assert_eq!(
            phase.receive(trace()).unwrap(),
            ParentReceiveAction::TraceReceived
        );
        assert!(phase.receive(trace()).is_err());
        assert!(matches!(
            phase.receive(terminal(1)).unwrap(),
            ParentReceiveAction::Terminal(_)
        ));
        #[cfg(feature = "tauri-webview-test-support")]
        {
            assert_eq!(recorder.take().as_deref(), Some("bounded trace\n"));
            assert!(recorder.take().is_none());
        }
    }

    #[test]
    fn response_reader_rejects_wrong_direction_and_stops_at_terminal() {
        let frame = worker_protocol::encode(&Message::ModuleChunk {
            request_id: 1,
            sequence: 0,
            bytes: vec![0; 1024],
        })
        .unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        forward_worker_responses(frame.as_slice(), sender);
        assert_eq!(receiver.try_iter().collect::<Vec<_>>(), vec![Err(())]);

        let expected = vec![
            Message::HelloAck { request_id: 1 },
            Message::ExecuteAck { request_id: 1 },
            terminal(1),
        ];
        let input: Vec<_> = expected
            .iter()
            .flat_map(|message| worker_protocol::encode(message).unwrap())
            .collect();
        let mut cursor = std::io::Cursor::new(input.clone());
        let (sender, receiver) = std::sync::mpsc::channel();
        forward_worker_responses(&mut cursor, sender);
        assert_eq!(cursor.position() as usize, input.len());
        assert_eq!(
            receiver.try_iter().collect::<Vec<_>>(),
            expected.into_iter().map(Ok).collect::<Vec<_>>()
        );
    }
    #[test]
    fn terminal_codes_preserve_semantic_categories() {
        assert_eq!(
            SketchWorkerTerminal::Stopped(SketchWorkerStopReason::Cancelled).code(),
            "cancelled"
        );
        assert_eq!(
            SketchWorkerTerminal::ForcedContainment {
                trigger: SketchWorkerStopReason::Cancelled
            }
            .code(),
            "forced-containment-cancelled"
        );
        assert_eq!(
            SketchWorkerTerminal::ForcedContainment {
                trigger: SketchWorkerStopReason::DeadlineExceeded
            }
            .code(),
            "forced-containment-deadline-exceeded"
        );
        assert_eq!(
            SketchWorkerTerminal::Failure(SketchWorkerFailure::Protocol).code(),
            "worker-protocol"
        );
        assert_eq!(
            SketchWorkerFailure::InvalidConfiguration.code(),
            "worker-invalid-configuration"
        );
    }
    #[test]
    #[cfg(feature = "tauri-webview")]
    fn native_capture_configuration_keeps_plain_worker_limits_and_exact_authority() {
        let directory = std::env::temp_dir();
        let plain =
            SketchWorkerConfig::new(directory.join("worker"), Duration::from_secs(1)).unwrap();
        assert_eq!(plain.process_limits().active_processes, Some(1));
        assert!(plain.webview_url.is_none());
        let grant = crate::webview::WebviewUrlGrant::new("https://example.test/exact").unwrap();
        assert!(plain
            .clone()
            .with_webview_capture(grant.clone(), PathBuf::from("relative.png"))
            .is_err());
        let output = directory.join("exact.png");
        let native = plain
            .clone()
            .with_webview_capture(grant, output.clone())
            .unwrap();
        assert_eq!(native.process_limits().active_processes, Some(16));
        assert_eq!(
            native.webview_url.as_deref(),
            Some("https://example.test/exact")
        );
        assert_eq!(native.output_destination, Some(output));
        assert_eq!(plain.process_limits().active_processes, Some(1));
    }
    #[test]
    fn worker_protocol_module_ceiling_is_checked_without_allocating_a_fixture() {
        let cap: usize = worker_protocol::WORKER_PROTOCOL_MAX_MODULE_BYTES
            .try_into()
            .expect("host usize");
        assert_eq!(validate_worker_module_len(cap), Ok(()));
        assert_eq!(
            validate_worker_module_len(cap + 1),
            Err(SketchWorkerFailure::InvalidConfiguration)
        );
    }
    fn terminal(request_id: u64) -> Message {
        Message::Terminal {
            request_id,
            kind: TerminalKind::WorkerFailure,
            detail: TerminalDetail::none(),
            diagnostic: String::new(),
            counters: FinalCounters {
                active_roots: 0,
                live_stores: 0,
                live_instances: 0,
                active_epoch_registrations: 0,
                live_threads: 0,
            },
        }
    }

    #[test]
    fn parent_receive_phase_accepts_only_the_complete_happy_path() {
        let mut phase = ParentReceivePhase::new(41);
        assert!(matches!(
            phase.receive(Message::HelloAck { request_id: 41 }),
            Ok(ParentReceiveAction::QueueUpload)
        ));
        phase.upload_queued();
        assert_eq!(phase.upload(Ok(())), Ok(()));
        assert!(matches!(
            phase.receive(Message::ExecuteAck { request_id: 41 }),
            Ok(ParentReceiveAction::ExecuteAcknowledged)
        ));
        assert!(matches!(
            phase.receive(terminal(41)),
            Ok(ParentReceiveAction::Terminal(_))
        ));
    }

    #[test]
    fn parent_receive_phase_rejects_out_of_order_duplicate_and_wrong_id_messages() {
        let mut phase = ParentReceivePhase::new(41);
        assert_eq!(
            phase.receive(Message::HelloAck { request_id: 42 }),
            Err(SketchWorkerFailure::Protocol)
        );
        assert_eq!(
            phase.receive(Message::ExecuteAck { request_id: 41 }),
            Err(SketchWorkerFailure::Protocol)
        );
        assert_eq!(
            phase.receive(terminal(41)),
            Err(SketchWorkerFailure::Protocol)
        );

        let mut phase = ParentReceivePhase::new(41);
        assert!(matches!(
            phase.receive(Message::HelloAck { request_id: 41 }),
            Ok(ParentReceiveAction::QueueUpload)
        ));
        assert_eq!(
            phase.receive(Message::HelloAck { request_id: 41 }),
            Err(SketchWorkerFailure::Protocol)
        );
        phase.upload_queued();
        assert_eq!(phase.upload(Ok(())), Ok(()));
        assert_eq!(
            phase.receive(terminal(41)),
            Err(SketchWorkerFailure::Protocol)
        );
        assert!(matches!(
            phase.receive(Message::ExecuteAck { request_id: 41 }),
            Ok(ParentReceiveAction::ExecuteAcknowledged)
        ));
        assert_eq!(
            phase.receive(Message::ExecuteAck { request_id: 41 }),
            Err(SketchWorkerFailure::Protocol)
        );
    }

    #[test]
    fn parent_receive_phase_defers_only_a_crossed_execute_ack() {
        let mut phase = ParentReceivePhase::new(41);
        assert!(matches!(
            phase.receive(Message::HelloAck { request_id: 41 }),
            Ok(ParentReceiveAction::QueueUpload)
        ));
        phase.upload_queued();
        assert!(matches!(
            phase.receive(Message::ExecuteAck { request_id: 41 }),
            Ok(ParentReceiveAction::AwaitingUpload)
        ));
        assert_eq!(
            phase.receive(terminal(41)),
            Err(SketchWorkerFailure::Protocol)
        );
        assert_eq!(phase.upload(Ok(())), Ok(()));
        assert!(matches!(
            phase.receive(terminal(41)),
            Ok(ParentReceiveAction::Terminal(_))
        ));
    }

    #[test]
    fn parent_receive_phase_rejects_upload_failure_and_duplicate_deferred_ack() {
        let mut phase = ParentReceivePhase::new(41);
        assert!(matches!(
            phase.receive(Message::HelloAck { request_id: 41 }),
            Ok(ParentReceiveAction::QueueUpload)
        ));
        phase.upload_queued();
        assert!(matches!(
            phase.receive(Message::ExecuteAck { request_id: 41 }),
            Ok(ParentReceiveAction::AwaitingUpload)
        ));
        assert_eq!(
            phase.receive(Message::ExecuteAck { request_id: 41 }),
            Err(SketchWorkerFailure::Protocol)
        );
        assert_eq!(phase.upload(Err(())), Err(SketchWorkerFailure::Protocol));
        assert_eq!(
            phase.receive(terminal(41)),
            Err(SketchWorkerFailure::Protocol)
        );
    }
    #[test]
    fn cancellation_wins_a_same_tick_deadline() {
        assert_eq!(
            select_stop(true, true),
            Some(SketchWorkerStopReason::Cancelled)
        );
        assert_eq!(
            select_stop(false, true),
            Some(SketchWorkerStopReason::DeadlineExceeded)
        );
        assert_eq!(select_stop(false, false), None);
    }
    #[test]
    fn parent_lifecycle_ledger_is_separate_and_bounded() {
        let ledger = WorkerExecutionLedger::default();
        ledger.record_spawned();
        ledger.record_cancel_sent();
        ledger.record_grace_expired();
        ledger.record_forced();
        ledger.record_reaped();
        ledger.record_protocol_failure();
        assert_eq!(
            ledger.snapshot(),
            SketchWorkerExecutionSnapshot {
                spawned: 1,
                cancel_sent: 1,
                grace_expired: 1,
                forced: 1,
                reaped: 1,
                protocol_failures: 1,
                live_workers: 0,
                live_protocol_tasks: 0,
                pending_root_leases: 0,
            }
        );
    }
    #[test]
    fn admitted_worker_retains_one_shared_source_allocation() {
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
        let bytes = super::super::threaded_root_observation_tests::threaded_yield_fixture();
        let policy = SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES)
            .expect("policy");
        let sketch = compiler.admit(&bytes, policy).expect("admission");
        let first = sketch.worker_source();
        let second = sketch.worker_source();
        assert_eq!(&*first, bytes.as_slice());
        assert!(Arc::ptr_eq(&first, &second));
        assert!(Arc::strong_count(&first) >= 3);
        assert_eq!(
            sketch.worker_compiler_config(),
            SketchCompilerConfig::default()
        );
        assert_eq!(sketch.worker_policy(), policy);
    }
    #[test]
    fn parent_root_lease_never_prepares_guest_state() {
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
        let bytes = super::super::threaded_root_observation_tests::threaded_yield_fixture();
        let policy = SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES)
            .expect("policy");
        let sketch = compiler.admit(&bytes, policy).expect("admission");
        assert_eq!(
            compiler.execution_limits_snapshot(),
            SketchExecutionSnapshot::default()
        );
        let lease = sketch
            .acquire_worker_parent_root_lease()
            .expect("parent lease");
        let during = compiler.execution_limits_snapshot();
        assert_eq!(during.active_root_executions(), 1);
        assert_eq!(during.reserved_shared_memory_bytes(), 0);
        assert_eq!(during.live_stores(), 0);
        assert_eq!(during.live_instances(), 0);
        assert_eq!(during.live_guest_threads(), 0);
        assert_eq!(during.active_epoch_registrations(), 0);
        drop(lease);
        assert_eq!(
            compiler.execution_limits_snapshot(),
            SketchExecutionSnapshot::default()
        );
    }
}
