//! Logical-sketch operation and resource ownership.
//!
//! This is deliberately independent of Wasmtime.  A callback can retain this
//! hub and wake a waiter, but it can never retain or re-enter a Store.

use crate::async_engine::Notify;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
#[cfg(feature = "wasm-sketch-host")]
use std::fs::{self, File};
#[cfg(feature = "wasm-sketch-host")]
use std::io::Write;
#[cfg(feature = "wasm-sketch-host")]
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static NEXT_OPAQUE_TOKEN: AtomicU64 = AtomicU64::new(1);

#[path = "hash_resource.rs"]
mod hash_resource;

#[cfg(feature = "wasm-sketch-host")]
#[path = "process_resource.rs"]
mod process_resource;

#[cfg(all(
    feature = "wasm-sketch-host",
    feature = "wasm-component-compiler-experiment"
))]
#[path = "component_transfer.rs"]
mod component_transfer;
#[cfg(all(
    feature = "wasm-sketch-host",
    feature = "wasm-component-compiler-experiment"
))]
pub(crate) use component_transfer::{ComponentResourceBudget, ComponentResourceLease};

#[cfg(all(
    test,
    feature = "archive-auth-test-support",
    feature = "wasm-sketch-host"
))]
#[path = "archive_input.rs"]
pub(crate) mod archive_input;
#[cfg(all(
    test,
    feature = "archive-auth-test-support",
    feature = "wasm-sketch-host"
))]
#[path = "archive_inventory.rs"]
mod archive_inventory;
#[cfg(all(test, feature = "archive-auth-test-support"))]
#[path = "authenticated_archive.rs"]
mod authenticated_archive;
#[cfg(all(
    test,
    feature = "archive-auth-test-support",
    feature = "wasm-sketch-host"
))]
#[path = "authenticated_blob.rs"]
mod authenticated_blob;
static NEXT_LOGICAL_SCOPE: AtomicU64 = AtomicU64::new(1);
const MAX_CLOSED_TOMBSTONES: usize = 128;
const MAX_WIRE_TOKEN: u64 = (1_u64 << 56) - 1;

pub(crate) const OP_SYNTHETIC_YIELD: u32 = 1;
pub(crate) const OP_SYNTHETIC_RESOURCE_CREATE: u32 = 2;
pub(crate) const OP_SYNTHETIC_RESOURCE_USE: u32 = 3;
pub(crate) const OP_SYNTHETIC_RESOURCE_CLOSE: u32 = 4;
pub(crate) const OP_BLOB_CREATE: u32 = 5;
pub(crate) const OP_BLOB_WRITE: u32 = 6;
pub(crate) const OP_BLOB_READ: u32 = 7;
pub(crate) const OP_BLOB_READ_COLLECT: u32 = 8;
pub(crate) const OP_BLOB_SEAL: u32 = 9;
pub(crate) const OP_OUTPUT_COMMIT: u32 = 10;
pub(crate) const OP_OUTPUT_GRANT: u32 = 11;
pub(crate) const OP_CLOCK_SLEEP: u32 = 12;
pub(crate) const OP_WEBVIEW_URL_GRANT: u32 = 13;
pub(crate) const OP_WEBVIEW_OPEN: u32 = 14;
pub(crate) const OP_WEBVIEW_LOAD: u32 = 15;
pub(crate) const OP_WEBVIEW_CAPTURE: u32 = 16;
pub(crate) const OP_WEBVIEW_CLOSE: u32 = 17;
pub(crate) const OP_TRANSFER_ABANDON: u32 = 18;
pub(crate) const OP_BLOB_ABANDON: u32 = 19;
pub(crate) const OP_ENCRYPTED_INPUT_GRANT: u32 = 20;
pub(crate) const OP_ENCRYPTED_INPUT_HEADER: u32 = 21;
pub(crate) const OP_ENCRYPTED_INPUT_ABANDON: u32 = 22;
pub(crate) const OP_ENCRYPTED_INPUT_AUTHENTICATE: u32 = 23;
pub(crate) const OP_ARCHIVE_AUTHENTICATION_ABANDON: u32 = 24;
pub(crate) const OP_AUTHENTICATED_ARCHIVE_ABANDON: u32 = 25;
pub(crate) const OP_ARCHIVE_NEXT_ENTRY: u32 = 26;
pub(crate) const OP_ARCHIVE_ENTRY_METADATA: u32 = 27;
pub(crate) const OP_ARCHIVE_ENTRY_ABANDON: u32 = 28;
pub(crate) const OP_ARCHIVE_ENTRY_OPEN: u32 = 29;
pub(crate) const MAX_WEBVIEW_URL_BYTES: usize = 16 * 1024;
const SYNTHETIC_RESOURCE_KIND: u8 = 1;
pub(crate) const EXTERNAL_WEBVIEW_RESOURCE_KIND: u8 = 2;
const EXTERNAL_WEBVIEW_RIGHT_LOAD: u8 = 0b01;
const EXTERNAL_WEBVIEW_RIGHT_CAPTURE: u8 = 0b10;
// Native presentation control: resize, show, hide, and focus. Granted with
// every opened view; it confers no navigation, script, or IPC authority.
const EXTERNAL_WEBVIEW_RIGHT_WINDOW: u8 = 0b100;
// Replace the top-level page with a URL the native facade has already
// validated (`WebviewUrlGrant`). Separate from presentation so resizing a
// view never implies authority to move it to another page.
const EXTERNAL_WEBVIEW_RIGHT_NAVIGATE: u8 = 0b1000;
const BLOB_RESOURCE_KIND: u8 = 3;
const BLOB_RIGHT_READ: u8 = 0b01;
const BLOB_RIGHT_WRITE: u8 = 0b10;
const OUTPUT_RESOURCE_KIND: u8 = 4;
const OUTPUT_RIGHT_COMMIT: u8 = 0b01;
const WEBVIEW_URL_RESOURCE_KIND: u8 = 5;
const WEBVIEW_URL_RIGHT_OPEN: u8 = 1;
const STATUS_PENDING: u8 = 0;
const STATUS_COMPLETED: u8 = 1;
const STATUS_CANCELLED: u8 = 2;
const STATUS_TIMED_OUT: u8 = 3;
const STATUS_TRAPPED: u8 = 4;
const STATUS_OWNER_EXITED: u8 = 5;
const STATUS_CLOSED: u8 = 6;
const STATUS_REJECTED: u8 = 7;
const STATUS_ERROR: u8 = 0x80;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct OpaqueToken(u64);

impl OpaqueToken {
    pub(crate) fn from_wire(value: u64) -> Self {
        Self(value)
    }
    pub(crate) fn wire(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Terminal {
    Completed,
    Cancelled,
    TimedOut,
    Trapped,
    OwnerExited,
    Closed,
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TerminalResult {
    pub(super) terminal: Terminal,
    pub(super) resource: Option<OpaqueToken>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HubError {
    Quota,
    Invalid,
    Stale,
    WrongKind,
    WrongRights,
    Closed,
    Exhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HubSnapshot {
    #[cfg(all(test, feature = "archive-auth-test-support"))]
    pub(crate) archive_staging_bytes: u64,
    #[cfg(all(
        test,
        feature = "archive-auth-test-support",
        feature = "wasm-sketch-host"
    ))]
    pub(crate) active_archive_jobs: usize,
    pub(crate) scope: u64,
    pub(crate) pending_operations: usize,
    pub(crate) live_resources: usize,
    pub(crate) live_blobs: usize,
    pub(crate) pending_blob_reads: usize,
    pub(crate) pending_blob_writes: usize,
    pub(crate) suspends: u64,
    pub(crate) resumes: u64,
    pub(crate) buffered_blob_bytes: usize,
    pub(crate) peak_buffered_blob_bytes: usize,
    pub(crate) pending_write_bytes: usize,
    pub(crate) completed_read_bytes: usize,
    /// Transfer capacity charged to the hub: retained buffer capacities plus
    /// reserved compiler-output allowance, including uncollected results.
    pub(crate) retained_transfer_capacity: usize,
    /// High-water mark of hub-owned capacities, including copy overlap.
    pub(crate) peak_retained_transfer_capacity: usize,
    pub(crate) native_transfer_capacity: usize,
    #[cfg(feature = "wasm-sketch-host")]
    pub(crate) reserved_process_output_bytes: usize,
    #[cfg(feature = "wasm-sketch-host")]
    pub(crate) reserved_native_process_output_bytes: usize,
    pub(crate) active_output_jobs: usize,
    #[cfg(feature = "wasm-sketch-host")]
    pub(crate) retained_process_jobs: usize,
    pub(crate) active_native_captures: usize,
    pub(crate) active_native_opens: usize,
    pub(crate) active_clocks: usize,
}

/// Private limits for the opaque bulk-data boundary.  They deliberately live
/// beside operation/resource authority: a producer cannot bypass accounting
/// by choosing a different host queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BlobLimits {
    pub(crate) maximum_chunk_bytes: usize,
    pub(crate) maximum_blob_bytes: usize,
    pub(crate) maximum_sketch_bytes: usize,
    pub(crate) maximum_live_blobs: usize,
    pub(crate) maximum_pending_reads: usize,
    pub(crate) maximum_pending_writes: usize,
    pub(crate) maximum_transfer_bytes: usize,
    /// Maximum quiet interval for one pending blob transfer.  This is an idle
    /// budget, not a sketch wall-clock deadline.
    pub(crate) progress_idle_timeout: std::time::Duration,
}

impl BlobLimits {
    pub(crate) const fn new(
        maximum_chunk_bytes: usize,
        maximum_blob_bytes: usize,
        maximum_sketch_bytes: usize,
    ) -> Result<Self, HubError> {
        if maximum_chunk_bytes == 0
            || maximum_blob_bytes < maximum_chunk_bytes
            || maximum_sketch_bytes < maximum_blob_bytes
        {
            return Err(HubError::Quota);
        }
        Ok(Self {
            maximum_chunk_bytes,
            maximum_blob_bytes,
            maximum_sketch_bytes,
            maximum_live_blobs: 128,
            maximum_pending_reads: 128,
            maximum_pending_writes: 128,
            maximum_transfer_bytes: match maximum_sketch_bytes.checked_mul(3) {
                Some(bytes) => match bytes.checked_add(maximum_chunk_bytes) {
                    Some(bytes) => bytes,
                    None => return Err(HubError::Quota),
                },
                None => return Err(HubError::Quota),
            },
            progress_idle_timeout: std::time::Duration::from_secs(5),
        })
    }
}

const DEFAULT_BLOB_LIMITS: BlobLimits = BlobLimits {
    maximum_chunk_bytes: 64 * 1024,
    maximum_blob_bytes: 1024 * 1024,
    maximum_sketch_bytes: 4 * 1024 * 1024,
    maximum_live_blobs: 128,
    maximum_pending_reads: 128,
    maximum_pending_writes: 128,
    maximum_transfer_bytes: 12 * 1024 * 1024 + 64 * 1024,
    progress_idle_timeout: std::time::Duration::from_secs(5),
};

/// Private typed requests shared by the generated ABI and heavyweight native
/// backends.  It is intentionally closed: adding authority means adding a
/// request here, rather than a second registry or scheduler.
pub(crate) enum Request {
    BlobCreate,
    SyntheticYield,
    SyntheticCreate {
        kind: u8,
        rights: u8,
        shareable: bool,
    },
    SyntheticUse {
        resource: OpaqueToken,
        kind: u8,
        rights: u8,
    },
    Close {
        resource: OpaqueToken,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResourceIdentity {
    scope: u64,
    slot: u32,
    generation: u32,
    kind: u8,
    rights: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Owner {
    // An owner is an authorized Store/thread instance in this logical sketch.
    // It is not a Wasmtime identity and never leaves the private dispatcher.
    store: u64,
}

struct ResourceSlot {
    identity: ResourceIdentity,
    owner: Owner,
    shareable: bool,
    value: ResourceValue,
    reserved: bool,
    committing: bool,
    // One opaque blob is one independent transfer-progress domain.  Progress
    // on a different blob must never extend this blob's idle budget.
    progress: crate::async_engine::ProgressReporter,
}

enum ResourceValue {
    #[cfg(feature = "wasm-sketch-host")]
    CompilerGrant(process_resource::CompilerGrant),
    #[cfg(feature = "wasm-sketch-host")]
    CompilerProcess(process_resource::CompilerProcess),
    Blake3(Box<crate::hash::Blake3Hasher>),
    #[cfg(all(
        test,
        feature = "archive-auth-test-support",
        feature = "wasm-sketch-host"
    ))]
    ArchiveReader(archive_inventory::SharedArchive),
    #[cfg(all(
        test,
        feature = "archive-auth-test-support",
        feature = "wasm-sketch-host"
    ))]
    ArchiveEntry(archive_inventory::ArchiveEntry),
    #[cfg(all(
        test,
        feature = "archive-auth-test-support",
        feature = "wasm-sketch-host"
    ))]
    EncryptedInput(archive_input::EncryptedInput),
    #[cfg(all(test, feature = "archive-auth-test-support"))]
    AuthenticatedArchive(crate::archive::authenticated_staging::Authenticated),
    Synthetic,
    ExternalWebview,
    WebviewUrl(Arc<str>),
    Blob {
        buffer: VecDeque<u8>,
        sealed: bool,
    },
    #[cfg(feature = "wasm-sketch-host")]
    ExactOutput(PathBuf),
}

struct OperationSlot {
    #[cfg(feature = "wasm-sketch-host")]
    compiler_scalar: Option<process_resource::CompilerScalarKind>,
    #[cfg(feature = "wasm-sketch-host")]
    compiler_exit: Option<crate::ProcessSessionExit>,
    #[cfg(feature = "wasm-sketch-host")]
    is_compiler_read: bool,
    // Some(None) is a ready exhausted stream, not a pending receive. The
    // payload has no hub reference, so storing it here cannot form a cycle.
    #[cfg(feature = "wasm-sketch-host")]
    compiler_output: Option<Option<crate::ProcessOutputEvent>>,
    #[cfg(feature = "wasm-sketch-host")]
    is_compiler_spawn: bool,
    is_hash_operation: bool,
    #[cfg(all(test, feature = "archive-auth-test-support"))]
    is_archive_operation: bool,
    #[cfg(all(test, feature = "archive-auth-test-support"))]
    pending_authentication: Option<crate::archive::authenticated_staging::Authentication>,
    owner: Owner,
    resource: Option<OpaqueToken>,
    required_rights: u8,
    terminal: Option<TerminalResult>,
    // A generated close must not complete before its guest future has parked.
    // The scheduler may run its detached task before the synchronous import
    // returns, so retain the authority-free completion until `suspend` has
    // registered the guest waiter.
    deferred_completion: Option<DeferredCompletion>,
    suspended: bool,
    notify: Arc<Notify>,
    created_resource: Option<OpaqueToken>,
    pending_blob_write: Option<Vec<u8>>,
    pending_blob_read: Option<usize>,
    is_blob_read: bool,
    is_blob_write: bool,
    blob_read_result: Option<Vec<u8>>,
    producer_cancel: Option<crate::async_engine::CancellationSource>,
    progress_timeout_cancel: Option<crate::async_engine::CancellationSource>,
}

#[derive(Clone, Copy)]
struct DeferredCompletion {
    result: TerminalResult,
    revoke: Option<OpaqueToken>,
}

struct State {
    #[cfg(feature = "wasm-sketch-host")]
    reserved_native_process_output_bytes: usize,
    #[cfg(feature = "wasm-sketch-host")]
    reserved_process_output_bytes: usize,
    #[cfg(feature = "wasm-sketch-host")]
    process_jobs: Vec<crate::async_engine::Task<Result<(), HubError>>>,
    #[cfg(feature = "wasm-sketch-host")]
    process_io_jobs: Vec<crate::async_engine::Task<Result<(), HubError>>>,
    #[cfg(feature = "wasm-sketch-host")]
    process_job_failed: bool,
    #[cfg(feature = "wasm-sketch-host")]
    output_jobs: Vec<crate::async_engine::Task<()>>,
    resources: BTreeMap<OpaqueToken, ResourceSlot>,
    operations: BTreeMap<OpaqueToken, OperationSlot>,
    next_resource_slot: u32,
    resource_generations: BTreeMap<u32, u32>,
    free_resource_slots: Vec<u32>,
    closed_resources: BTreeSet<OpaqueToken>,
    closed_resource_order: VecDeque<OpaqueToken>,
    suspends: u64,
    resumes: u64,
    buffered_blob_bytes: usize,
    peak_buffered_blob_bytes: usize,
    peak_retained_transfer_capacity: usize,
    native_transfer_capacity: usize,
    closed: bool,
}

/// Native consumers borrow bytes without taking ownership of an unaccounted
/// Vec. The allocation stays charged even if its source resource is revoked.
pub(crate) struct NativeBlobChunk<'a> {
    hub: &'a OperationHub,
    bytes: Vec<u8>,
    progress: crate::async_engine::ProgressReporter,
}

impl std::ops::Deref for NativeBlobChunk<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for NativeBlobChunk<'_> {
    fn drop(&mut self) {
        let capacity = self.bytes.capacity();
        let observed_bytes = !self.bytes.is_empty();
        // Free the allocation before making its credits available to writers.
        drop(std::mem::take(&mut self.bytes));
        if let Ok(mut state) = self.hub.state.lock() {
            state.native_transfer_capacity =
                state.native_transfer_capacity.saturating_sub(capacity);
        }
        if observed_bytes {
            self.progress.report_progress();
        }
        let _ = self.hub.drive_blob_writes();
        let _ = self.hub.drive_blob_reads();
    }
}

/// Private logical authority shared only by explicitly authorized instances.
#[cfg(feature = "wasm-sketch-host")]
struct OutputJobLease(Arc<OperationHub>);

#[cfg(feature = "wasm-sketch-host")]
impl Drop for OutputJobLease {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.output_job_failed.store(true, Ordering::Release);
        }
        self.0.output_job_count.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Admission retained until queued UI work and its native callback are gone.
pub(crate) struct NativeCaptureLease(Arc<OperationHub>);

pub(crate) struct NativeOpenLease(Arc<OperationHub>);
impl Drop for NativeOpenLease {
    fn drop(&mut self) {
        self.0.native_open_count.fetch_sub(1, Ordering::AcqRel);
    }
}

struct ClockLease(Arc<OperationHub>);

impl Drop for ClockLease {
    fn drop(&mut self) {
        self.0.clock_count.fetch_sub(1, Ordering::AcqRel);
        self.0.clock_drained.notify_one();
    }
}

impl Drop for NativeCaptureLease {
    fn drop(&mut self) {
        self.0.native_capture_count.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Private logical authority shared only by explicitly authorized instances.
pub(crate) struct OperationHub {
    #[cfg(feature = "wasm-sketch-host")]
    process_join: crate::async_engine::Semaphore,
    #[cfg(all(test, feature = "wasm-sketch-host"))]
    process_spawn_checkpoint: Mutex<Option<process_resource::SpawnCheckpoint>>,
    #[cfg(all(test, feature = "wasm-sketch-host"))]
    process_cleanup_checkpoint: Mutex<Option<process_resource::CleanupCheckpoint>>,
    #[cfg(all(test, feature = "wasm-sketch-host"))]
    process_spawn_attempts: AtomicU64,
    #[cfg(all(
        test,
        feature = "archive-auth-test-support",
        feature = "wasm-sketch-host"
    ))]
    archive_jobs: Mutex<archive_input::ArchiveJobs>,
    #[cfg(all(test, feature = "archive-auth-test-support"))]
    staging_budget: crate::archive::authenticated_staging::StagingBudget,
    #[cfg(all(test, feature = "wasm-sketch-host"))]
    output_fault: Mutex<Option<OutputFault>>,
    output_job_failed: AtomicBool,
    output_job_count: AtomicU64,
    native_capture_count: AtomicU64,
    native_open_count: AtomicU64,
    clock_count: AtomicU64,
    clock_drained: Notify,
    scope: u64,
    maximum_operations: usize,
    maximum_resources: usize,
    blob_limits: BlobLimits,
    state: Mutex<State>,
}

impl OperationHub {
    pub(crate) fn acquire_native_open(self: &Arc<Self>) -> Result<NativeOpenLease, HubError> {
        let state = self.state.lock().map_err(|_| HubError::Closed)?;
        if state.closed {
            return Err(HubError::Closed);
        }
        let limit = self.maximum_operations.min(4) as u64;
        self.native_open_count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < limit).then_some(active + 1)
            })
            .map_err(|_| HubError::Quota)?;
        Ok(NativeOpenLease(Arc::clone(self)))
    }
    pub(crate) fn external_webview_wire(&self, store: u64, token: u64) -> bool {
        self.state.lock().is_ok_and(|state| {
            state
                .resources
                .get(&OpaqueToken(token))
                .is_some_and(|resource| {
                    Self::validate_resource(resource, store, EXTERNAL_WEBVIEW_RESOURCE_KIND, 0)
                        .is_ok()
                })
        })
    }
    /// Keep native admission independent of consumed operation terminals.
    /// At most four captures may be queued or in flight in a logical hub.
    pub(crate) fn acquire_native_capture(self: &Arc<Self>) -> Result<NativeCaptureLease, HubError> {
        let state = self.state.lock().map_err(|_| HubError::Closed)?;
        if state.closed {
            return Err(HubError::Closed);
        }
        let limit = self.maximum_operations.min(4) as u64;
        self.native_capture_count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < limit).then_some(active + 1)
            })
            .map_err(|_| HubError::Quota)?;
        Ok(NativeCaptureLease(Arc::clone(self)))
    }

    pub(crate) fn new(
        maximum_operations: usize,
        maximum_resources: usize,
    ) -> Result<Arc<Self>, HubError> {
        Self::new_with_blob_limits(maximum_operations, maximum_resources, DEFAULT_BLOB_LIMITS)
    }

    fn new_with_blob_limits(
        maximum_operations: usize,
        maximum_resources: usize,
        blob_limits: BlobLimits,
    ) -> Result<Arc<Self>, HubError> {
        let scope = next(&NEXT_LOGICAL_SCOPE)?;
        Ok(Arc::new(Self {
            #[cfg(feature = "wasm-sketch-host")]
            process_join: crate::async_engine::Semaphore::new(1),
            #[cfg(all(test, feature = "wasm-sketch-host"))]
            process_spawn_checkpoint: Mutex::new(None),
            #[cfg(all(test, feature = "wasm-sketch-host"))]
            process_cleanup_checkpoint: Mutex::new(None),
            #[cfg(all(test, feature = "wasm-sketch-host"))]
            process_spawn_attempts: AtomicU64::new(0),
            #[cfg(all(
                test,
                feature = "archive-auth-test-support",
                feature = "wasm-sketch-host"
            ))]
            archive_jobs: Mutex::new(archive_input::ArchiveJobs::default()),
            #[cfg(all(test, feature = "archive-auth-test-support"))]
            staging_budget: crate::archive::authenticated_staging::StagingBudget::new(
                512 * 1024 * 1024,
            ),
            #[cfg(all(test, feature = "wasm-sketch-host"))]
            output_fault: Mutex::new(None),
            output_job_failed: AtomicBool::new(false),
            output_job_count: AtomicU64::new(0),
            native_capture_count: AtomicU64::new(0),
            native_open_count: AtomicU64::new(0),
            clock_count: AtomicU64::new(0),
            clock_drained: Notify::new(),
            scope,
            maximum_operations,
            maximum_resources,
            blob_limits,
            state: Mutex::new(State {
                #[cfg(feature = "wasm-sketch-host")]
                reserved_native_process_output_bytes: 0,
                #[cfg(feature = "wasm-sketch-host")]
                reserved_process_output_bytes: 0,
                #[cfg(feature = "wasm-sketch-host")]
                process_jobs: Vec::new(),
                #[cfg(feature = "wasm-sketch-host")]
                process_io_jobs: Vec::new(),
                #[cfg(feature = "wasm-sketch-host")]
                process_job_failed: false,
                #[cfg(feature = "wasm-sketch-host")]
                output_jobs: Vec::new(),
                resources: BTreeMap::new(),
                operations: BTreeMap::new(),
                next_resource_slot: 0,
                resource_generations: BTreeMap::new(),
                free_resource_slots: Vec::new(),
                closed_resources: BTreeSet::new(),
                closed_resource_order: VecDeque::new(),
                suspends: 0,
                resumes: 0,
                buffered_blob_bytes: 0,
                peak_buffered_blob_bytes: 0,
                peak_retained_transfer_capacity: 0,
                native_transfer_capacity: 0,
                closed: false,
            }),
        }))
    }

    /// Tighten the default bulk limits for a logical sketch before it has any
    /// resources.  This makes test/embedding quotas explicit without adding a
    /// public storage abstraction or a second resource table.
    pub(crate) fn with_blob_limits(
        maximum_operations: usize,
        maximum_resources: usize,
        limits: BlobLimits,
    ) -> Result<Arc<Self>, HubError> {
        Self::new_with_blob_limits(maximum_operations, maximum_resources, limits)
    }

    /// The sole scheduling entry point.  Both generated guest imports and a
    /// native backend submit this closed request type through the caller's
    /// RuntimeHandle; neither path can create an ambient runtime or bypass
    /// quota/identity accounting.
    #[expect(
        clippy::too_many_lines,
        reason = "Existing function complexity measured at ratchet adoption"
    )]
    pub(crate) fn dispatch(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        store: u64,
        request: Request,
    ) -> Result<OpaqueToken, HubError> {
        let (resource, created, close_after, kind, rights, completion) = match request {
            Request::BlobCreate => {
                let resource = self.create_resource_value(
                    store,
                    BLOB_RESOURCE_KIND,
                    BLOB_RIGHT_READ | BLOB_RIGHT_WRITE,
                    false,
                    ResourceValue::Blob {
                        buffer: VecDeque::new(),
                        sealed: false,
                    },
                )?;
                (
                    None,
                    true,
                    None,
                    BLOB_RESOURCE_KIND,
                    BLOB_RIGHT_READ | BLOB_RIGHT_WRITE,
                    TerminalResult {
                        terminal: Terminal::Completed,
                        resource: Some(resource),
                    },
                )
            }
            Request::SyntheticYield => (
                None,
                false,
                None,
                0,
                0,
                TerminalResult {
                    terminal: Terminal::Completed,
                    resource: None,
                },
            ),
            Request::SyntheticCreate {
                kind,
                rights,
                shareable,
            } => {
                // Reserve the resource before scheduling.  If operation
                // reservation fails, close it again below.
                let resource = self.create_resource(store, kind, rights, shareable)?;
                (
                    None,
                    true,
                    None,
                    kind,
                    rights,
                    TerminalResult {
                        terminal: Terminal::Completed,
                        resource: Some(resource),
                    },
                )
            }
            Request::SyntheticUse {
                resource,
                kind,
                rights,
            } => (
                Some(resource),
                false,
                None,
                kind,
                rights,
                TerminalResult {
                    terminal: Terminal::Completed,
                    resource: None,
                },
            ),
            Request::Close { resource } => {
                self.validate_close(store, resource)?;
                (
                    None,
                    false,
                    Some(resource),
                    0,
                    0,
                    TerminalResult {
                        // Closing is a successful operation. The resource
                        // becomes closed as its side effect; reporting that
                        // resource state as this operation's terminal error
                        // would make the generated semantic close future
                        // unusable.
                        terminal: Terminal::Completed,
                        resource: None,
                    },
                )
            }
        };
        let operation = match self.submit(store, resource, kind, rights) {
            Ok((operation, _)) => operation,
            Err(error) => {
                if created {
                    let resource = completion
                        .resource
                        .expect("create completion owns reservation");
                    let _ = self.close_resource(resource);
                }
                return Err(error);
            }
        };
        if created {
            self.attach_created_resource(
                operation,
                completion
                    .resource
                    .expect("create completion owns resource"),
            )?;
        }
        let hub = Arc::clone(self);
        runtime
            .launch(async move {
                // This is the deliberately authority-free #37 synthetic
                // operation. Real native work supplies the same terminal
                // handoff after its callback, never a Store callback.
                crate::async_engine::yield_now().await;
                let _ = hub.complete_after_suspend(operation, completion, close_after);
            })
            .detach();
        Ok(operation)
    }

    /// Called after revocation, which prevents any new output job admission.
    #[cfg(feature = "wasm-sketch-host")]
    pub(crate) async fn join_output_jobs(&self) -> Result<(), HubError> {
        let jobs = {
            let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
            if !state.closed {
                return Err(HubError::WrongRights);
            }
            std::mem::take(&mut state.output_jobs)
        };
        let mut failed = false;
        for job in jobs {
            failed |= job.await.is_err();
        }
        if failed || self.output_job_failed.load(Ordering::Acquire) {
            Err(HubError::Closed)
        } else {
            Ok(())
        }
    }

    /// Reserve a generation-safe external-webview resource and its open
    /// operation in the sole hub.  The caller must later publish either a
    /// terminal outcome or a successful resource activation from its native
    /// callback; it may never retain a Wasmtime Store.
    pub(crate) fn begin_external_webview_open(
        &self,
        store: u64,
    ) -> Result<(OpaqueToken, OpaqueToken), HubError> {
        let resource = self.create_resource_value(
            store,
            EXTERNAL_WEBVIEW_RESOURCE_KIND,
            EXTERNAL_WEBVIEW_RIGHT_LOAD
                | EXTERNAL_WEBVIEW_RIGHT_CAPTURE
                | EXTERNAL_WEBVIEW_RIGHT_WINDOW
                | EXTERNAL_WEBVIEW_RIGHT_NAVIGATE,
            false,
            ResourceValue::ExternalWebview,
        )?;
        let operation = match self.submit(store, None, 0, 0) {
            Ok((operation, _)) => operation,
            Err(error) => {
                let _ = self.close_resource(resource);
                return Err(error);
            }
        };
        self.attach_created_resource(operation, resource)?;
        Ok((resource, operation))
    }

    /// Store a URL already validated by the native facade. The guest receives
    /// only this scoped token; URL text never crosses the generated boundary.
    pub(crate) fn grant_webview_url(
        &self,
        store: u64,
        url: Arc<str>,
    ) -> Result<OpaqueToken, HubError> {
        if url.is_empty() || url.len() > MAX_WEBVIEW_URL_BYTES {
            return Err(HubError::Quota);
        }
        let resource = self.create_resource_value(
            store,
            WEBVIEW_URL_RESOURCE_KIND,
            WEBVIEW_URL_RIGHT_OPEN,
            false,
            ResourceValue::WebviewUrl(url),
        )?;
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        state
            .resources
            .get_mut(&resource)
            .ok_or(HubError::Closed)?
            .reserved = false;
        Ok(resource)
    }

    /// Reserve open against the URL grant itself, so grant revocation also
    /// cancels an in-flight open and reclaims its unpublished view resource.
    pub(crate) fn begin_granted_webview_open(
        &self,
        store: u64,
        grant: OpaqueToken,
    ) -> Result<(OpaqueToken, OpaqueToken, Arc<str>), HubError> {
        let url = {
            let state = self.state.lock().map_err(|_| HubError::Closed)?;
            let slot = state.resources.get(&grant).ok_or(HubError::Invalid)?;
            Self::validate_resource(
                slot,
                store,
                WEBVIEW_URL_RESOURCE_KIND,
                WEBVIEW_URL_RIGHT_OPEN,
            )?;
            let ResourceValue::WebviewUrl(url) = &slot.value else {
                return Err(HubError::WrongKind);
            };
            Arc::clone(url)
        };
        let resource = self.create_resource_value(
            store,
            EXTERNAL_WEBVIEW_RESOURCE_KIND,
            EXTERNAL_WEBVIEW_RIGHT_LOAD
                | EXTERNAL_WEBVIEW_RIGHT_CAPTURE
                | EXTERNAL_WEBVIEW_RIGHT_WINDOW
                | EXTERNAL_WEBVIEW_RIGHT_NAVIGATE,
            false,
            ResourceValue::ExternalWebview,
        )?;
        let operation = match self.submit(
            store,
            Some(grant),
            WEBVIEW_URL_RESOURCE_KIND,
            WEBVIEW_URL_RIGHT_OPEN,
        ) {
            Ok((operation, _)) => operation,
            Err(error) => {
                let _ = self.close_resource(resource);
                return Err(error);
            }
        };
        self.attach_created_resource(operation, resource)?;
        Ok((resource, operation, url))
    }

    /// Submit a load observation operation for an already activated webview.
    pub(crate) fn begin_external_webview_wait(
        &self,
        store: u64,
        resource: OpaqueToken,
    ) -> Result<OpaqueToken, HubError> {
        self.begin_external_webview_operation(store, resource, EXTERNAL_WEBVIEW_RIGHT_LOAD)
    }

    /// Reserve capture authority separately from load observation and close.
    pub(crate) fn begin_external_webview_capture(
        &self,
        store: u64,
        resource: OpaqueToken,
    ) -> Result<OpaqueToken, HubError> {
        self.begin_external_webview_operation(store, resource, EXTERNAL_WEBVIEW_RIGHT_CAPTURE)
    }

    /// Reserve one native presentation change (resize, show, hide, focus).
    /// Like close, physical work belongs to the private backend; the hub
    /// owns the operation and wakes it with the revocation's terminal reason.
    pub(crate) fn begin_external_webview_window(
        &self,
        store: u64,
        resource: OpaqueToken,
    ) -> Result<OpaqueToken, HubError> {
        self.begin_external_webview_operation(store, resource, EXTERNAL_WEBVIEW_RIGHT_WINDOW)
    }

    /// Reserve one top-level navigation of an open view. The URL itself was
    /// validated by the native facade; the hub owns only the operation.
    pub(crate) fn begin_external_webview_navigate(
        &self,
        store: u64,
        resource: OpaqueToken,
    ) -> Result<OpaqueToken, HubError> {
        self.begin_external_webview_operation(store, resource, EXTERNAL_WEBVIEW_RIGHT_NAVIGATE)
    }

    fn begin_external_webview_operation(
        &self,
        store: u64,
        resource: OpaqueToken,
        required_rights: u8,
    ) -> Result<OpaqueToken, HubError> {
        match self.submit(
            store,
            Some(resource),
            EXTERNAL_WEBVIEW_RESOURCE_KIND,
            required_rights,
        ) {
            Ok((operation, _)) => Ok(operation),
            // A revoked external handle has a bounded tombstone. Preserve
            // that semantic distinction for the facade instead of treating
            // an immediately repeated use as a malformed host request.
            Err(HubError::Invalid) if self.resource_was_closed(resource) => Err(HubError::Closed),
            Err(error) => Err(error),
        }
    }

    /// Validate and reserve a semantic close operation.  Physical teardown is
    /// performed by the private native backend; this hub remains the sole
    /// authority that revokes the resource generation.
    pub(crate) fn begin_external_webview_close(
        &self,
        store: u64,
        resource: OpaqueToken,
    ) -> Result<OpaqueToken, HubError> {
        self.validate_close(store, resource)?;
        self.submit(store, Some(resource), EXTERNAL_WEBVIEW_RESOURCE_KIND, 0)
            .map(|(operation, _)| operation)
    }

    pub(crate) fn finish_external_operation(&self, operation: OpaqueToken, terminal: Terminal) {
        let _ = self.terminal(
            operation,
            TerminalResult {
                terminal,
                resource: None,
            },
        );
        // A revoked capture can release a partially encoded reservation while
        // its native callback is still alive. Wake other bounded producers now.
        let _ = self.drive_blob_writes();
        let _ = self.drive_blob_reads();
    }

    pub(crate) fn finish_external_open(
        &self,
        operation: OpaqueToken,
        resource: OpaqueToken,
    ) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.closed
            || !state.resources.get(&resource).is_some_and(|slot| {
                slot.identity.kind == EXTERNAL_WEBVIEW_RESOURCE_KIND && slot.reserved
            })
            || !state.operations.get(&operation).is_some_and(|slot| {
                slot.terminal.is_none() && slot.created_resource == Some(resource)
            })
        {
            return false;
        }
        let Ok(wake) = Self::terminal_locked(
            &mut state,
            operation,
            TerminalResult {
                terminal: Terminal::Completed,
                resource: Some(resource),
            },
        ) else {
            return false;
        };
        drop(state);
        if let Some(wake) = wake {
            wake.notify_one();
        }
        true
    }

    /// Latches any terminal result for the native producer independently of
    /// the guest's one-consumer completion notification. Cancelling, consuming
    /// the result, or tearing down the root cannot strand queued producer work.
    pub(crate) fn bind_producer_cancellation(
        &self,
        store: u64,
        operation: OpaqueToken,
    ) -> Result<crate::async_engine::CancellationToken, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let slot = state
            .operations
            .get_mut(&operation)
            .ok_or(HubError::Invalid)?;
        if slot.owner.store != store {
            return Err(HubError::WrongRights);
        }
        if slot.producer_cancel.is_some() {
            return Err(HubError::Invalid);
        }
        let source = crate::async_engine::CancellationSource::new();
        let token = source.token();
        if slot.terminal.is_some() {
            source.cancel();
        } else {
            slot.producer_cancel = Some(source);
        }
        Ok(token)
    }

    pub(crate) fn observe_terminal(
        &self,
        store: u64,
        operation: OpaqueToken,
    ) -> Result<Option<TerminalResult>, HubError> {
        self.take_terminal(operation, store)
    }

    pub(crate) fn wait_external_operation(
        &self,
        store: u64,
        operation: OpaqueToken,
    ) -> Result<Arc<Notify>, HubError> {
        self.suspend(operation, store)
    }

    /// Generated scalar ABI adapter.  `kind` is a closed opcode; `arg0` and
    /// `arg1` never carry a pointer or a host object.  Resource create/use/
    /// close are operation variants, not additional imports.
    pub(crate) fn submit_wire(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        store: u64,
        kind: u32,
        arg0: u64,
        arg1: u64,
    ) -> Result<u64, HubError> {
        if kind == OP_CLOCK_SLEEP {
            if arg1 != 0 || arg0 > u64::from(u32::MAX) {
                return Err(HubError::Invalid);
            }
            return self.submit_clock_sleep(runtime, store, arg0).map(|op| op.0);
        }
        #[cfg(feature = "wasm-sketch-host")]
        if kind == OP_OUTPUT_COMMIT {
            return self
                .submit_output_commit(runtime, store, OpaqueToken(arg0), OpaqueToken(arg1))
                .map(|operation| operation.0);
        }
        if kind == OP_BLOB_SEAL {
            if arg1 != 0 {
                return Err(HubError::Invalid);
            }
            let blob = OpaqueToken(arg0);
            let (operation, _) =
                self.submit(store, Some(blob), BLOB_RESOURCE_KIND, BLOB_RIGHT_WRITE)?;
            let terminal = if self.seal_blob(store, blob).is_ok() {
                Terminal::Completed
            } else {
                Terminal::Rejected
            };
            self.terminal(
                operation,
                TerminalResult {
                    terminal,
                    resource: None,
                },
            )?;
            return Ok(operation.0);
        }
        if kind == OP_BLOB_READ {
            return self
                .submit_blob_read(
                    store,
                    OpaqueToken(arg0),
                    usize::try_from(arg1).map_err(|_| HubError::Quota)?,
                )
                .map(|token| token.0);
        }
        let request = match kind {
            OP_BLOB_CREATE if arg0 == 0 && arg1 == 0 => Request::BlobCreate,
            OP_SYNTHETIC_YIELD => Request::SyntheticYield,
            OP_SYNTHETIC_RESOURCE_CREATE => Request::SyntheticCreate {
                kind: SYNTHETIC_RESOURCE_KIND,
                rights: u8::try_from(arg1).map_err(|_| HubError::WrongRights)?,
                shareable: match arg0 {
                    0 => false,
                    1 => true,
                    _ => return Err(HubError::Invalid),
                },
            },
            OP_SYNTHETIC_RESOURCE_USE => Request::SyntheticUse {
                resource: OpaqueToken(arg0),
                kind: SYNTHETIC_RESOURCE_KIND,
                rights: u8::try_from(arg1).map_err(|_| HubError::WrongRights)?,
            },
            OP_SYNTHETIC_RESOURCE_CLOSE => Request::Close {
                resource: OpaqueToken(arg0),
            },
            _ => return Err(HubError::Invalid),
        };
        Ok(self.dispatch(runtime, store, request)?.0)
    }

    /// Called after root revocation so no clock can be admitted while draining.
    pub(crate) async fn join_clock_jobs(&self) {
        debug_assert!(self.state.lock().is_ok_and(|state| state.closed));
        while self.clock_count.load(Ordering::Acquire) != 0 {
            self.clock_drained.notified().await;
        }
    }

    fn submit_clock_sleep(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        store: u64,
        milliseconds: u64,
    ) -> Result<OpaqueToken, HubError> {
        // Count physical timer tasks independently of consumable operations:
        // cancel/poll/resubmit cannot grow a stalled runtime's task queue.
        self.clock_count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < self.maximum_operations as u64).then_some(active + 1)
            })
            .map_err(|_| HubError::Quota)?;
        let lease = ClockLease(Arc::clone(self));
        let (operation, _) = self.submit(store, None, 0, 0)?;
        let token = self.bind_producer_cancellation(store, operation)?;
        let hub = Arc::clone(self);
        runtime
            .launch(async move {
                let _lease = lease;
                if crate::async_engine::cancellable(
                    &token,
                    crate::async_engine::sleep(std::time::Duration::from_millis(milliseconds)),
                )
                .await
                .is_ok()
                {
                    let _ = hub.terminal(
                        operation,
                        TerminalResult {
                            terminal: Terminal::Completed,
                            resource: None,
                        },
                    );
                }
            })
            .detach();
        Ok(operation)
    }

    pub(crate) fn poll_wire(&self, store: u64, operation: u64) -> u64 {
        // Read completion must be collected with its bytes, never consumed
        // through the payload-free lifecycle poll.
        if self.state.lock().is_ok_and(|state| {
            state
                .operations
                .get(&OpaqueToken(operation))
                .is_some_and(|op| op.is_blob_read)
        }) {
            return pack(STATUS_ERROR, None);
        }
        match self.take_terminal(OpaqueToken(operation), store) {
            Ok(None) => pack(STATUS_PENDING, None),
            Ok(Some(result)) => pack(status(result.terminal), result.resource),
            Err(_) => pack(STATUS_ERROR, None),
        }
    }

    pub(crate) fn suspend_wire(&self, store: u64, operation: u64) -> Result<Arc<Notify>, HubError> {
        self.suspend(OpaqueToken(operation), store)
    }

    /// Park a generated blob transfer and enforce its own idle budget.  The
    /// timer watches only the transfer's resource progress domain; it is not
    /// the logical sketch's wall-clock deadline and cannot be reset by a
    /// different blob.
    pub(crate) fn suspend_stream_wire(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        store: u64,
        operation: u64,
    ) -> Result<Arc<Notify>, HubError> {
        let operation_token = OpaqueToken(operation);
        let notify = self.suspend(operation_token, store)?;
        let armed = {
            let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
            let operation = state
                .operations
                .get(&operation_token)
                .ok_or(HubError::Invalid)?;
            let arm = operation.owner.store == store
                && operation.terminal.is_none()
                && (operation.is_blob_read || operation.is_blob_write)
                && operation.progress_timeout_cancel.is_none();
            if !arm {
                None
            } else {
                let resource = operation.resource.ok_or(HubError::Invalid)?;
                let reporter = state
                    .resources
                    .get(&resource)
                    .ok_or(HubError::Invalid)?
                    .progress
                    .clone();
                let watch = reporter.watch();
                let cancel = crate::async_engine::CancellationSource::new();
                let token = cancel.token();
                state
                    .operations
                    .get_mut(&operation_token)
                    .ok_or(HubError::Invalid)?
                    .progress_timeout_cancel = Some(cancel);
                Some((reporter, watch, token))
            }
        };
        if let Some((reporter, watch, cancellation)) = armed {
            // A Blob can outlive one transfer. Start this operation's own
            // quiet interval now rather than inheriting the last activity of
            // a prior transfer on the same resource.
            reporter.restart_idle_window();
            let hub = Arc::clone(self);
            let idle = self.blob_limits.progress_idle_timeout;
            runtime
                .launch(async move {
                    let stalled = matches!(
                        crate::async_engine::cancellable(
                            &cancellation,
                            crate::async_engine::progress_timeout(
                                idle,
                                &watch,
                                std::future::pending::<()>(),
                            ),
                        )
                        .await,
                        Ok(Err(_))
                    );
                    if stalled {
                        let _ = hub.terminal(
                            operation_token,
                            TerminalResult {
                                terminal: Terminal::TimedOut,
                                resource: None,
                            },
                        );
                    }
                })
                .detach();
        }
        Ok(notify)
    }

    pub(crate) fn cancel_wire(&self, store: u64, operation: u64) -> Result<(), HubError> {
        #[cfg(feature = "wasm-sketch-host")]
        if self.cancel_compiler_output_wire(store, OpaqueToken(operation))? {
            return Ok(());
        }
        self.take_owner(OpaqueToken(operation), store)?;
        self.terminal(
            OpaqueToken(operation),
            TerminalResult {
                terminal: Terminal::Cancelled,
                resource: None,
            },
        )
    }

    /// A dropped transfer has no future consumer for its terminal result.
    /// Unlike cancellation, abandonment removes the operation and its retained
    /// bytes atomically, including a read which completed before guest Drop.
    pub(crate) fn abandon_transfer_wire(&self, store: u64, token: u64) -> Result<(), HubError> {
        let token = OpaqueToken(token);
        let operation = {
            let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
            let operation = state.operations.get(&token).ok_or(HubError::Invalid)?;
            if operation.owner.store != store {
                return Err(HubError::Stale);
            }
            if !operation.is_blob_read && !operation.is_blob_write {
                return Err(HubError::WrongKind);
            }
            state.operations.remove(&token).ok_or(HubError::Invalid)?
        };
        // Never cancel or notify while holding the authority mutex. Dropping
        // the removed slot releases pending-write and completed-read buffers.
        if let Some(cancel) = &operation.producer_cancel {
            cancel.cancel();
        }
        operation.notify.notify_one();
        drop(operation);
        self.drive_blob_writes()?;
        self.drive_blob_reads()
    }

    /// Revoke an owned blob without reserving an operation slot. Guest Drop
    /// must remain effective even when the pending-operation table is full.
    pub(crate) fn abandon_blob_wire(&self, store: u64, token: u64) -> Result<(), HubError> {
        let token = OpaqueToken(token);
        let notifications = {
            let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
            let resource = state.resources.get(&token).ok_or(HubError::Invalid)?;
            if resource.owner.store != store {
                return Err(HubError::WrongRights);
            }
            if resource.identity.kind != BLOB_RESOURCE_KIND {
                return Err(HubError::WrongKind);
            }
            Self::close_resource_with_terminal_locked(&mut state, token, Terminal::Closed)?
        };
        for notify in notifications {
            notify.notify_one();
        }
        self.drive_blob_writes()
    }

    fn validate_close(&self, store: u64, resource: OpaqueToken) -> Result<(), HubError> {
        let state = self.state.lock().map_err(|_| HubError::Closed)?;
        let slot = state.resources.get(&resource).ok_or(HubError::Invalid)?;
        if slot.owner.store != store && !slot.shareable {
            return Err(HubError::WrongRights);
        }
        Ok(())
    }

    fn resource_was_closed(&self, resource: OpaqueToken) -> bool {
        self.state
            .lock()
            .is_ok_and(|state| state.closed_resources.contains(&resource))
    }

    fn take_owner(&self, operation: OpaqueToken, store: u64) -> Result<(), HubError> {
        let state = self.state.lock().map_err(|_| HubError::Closed)?;
        let slot = state.operations.get(&operation).ok_or(HubError::Invalid)?;
        if slot.owner.store != store {
            return Err(HubError::Stale);
        }
        Ok(())
    }

    fn attach_created_resource(
        &self,
        operation: OpaqueToken,
        resource: OpaqueToken,
    ) -> Result<(), HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        // This handoff precedes publication of the operation token. A
        // borrowed grant may nevertheless be revoked concurrently. Reclaim
        // both unpublished reservations if terminalization won that race.
        if state
            .operations
            .get(&operation)
            .is_none_or(|slot| slot.terminal.is_some())
        {
            state.operations.remove(&operation);
            if let Some(slot) = state.resources.remove(&resource) {
                if let ResourceValue::Blob { buffer, .. } = &slot.value {
                    state.buffered_blob_bytes =
                        state.buffered_blob_bytes.saturating_sub(buffer.len());
                }
                state.free_resource_slots.push(slot.identity.slot);
            }
            return Err(HubError::Closed);
        }
        state
            .operations
            .get_mut(&operation)
            .ok_or(HubError::Invalid)?
            .created_resource = Some(resource);
        Ok(())
    }

    #[cfg(test)]
    fn activate_for_test(&self, resource: OpaqueToken) {
        self.state
            .lock()
            .unwrap()
            .resources
            .get_mut(&resource)
            .unwrap()
            .reserved = false;
    }

    pub(super) fn create_resource(
        &self,
        store: u64,
        kind: u8,
        rights: u8,
        shareable: bool,
    ) -> Result<OpaqueToken, HubError> {
        self.create_resource_value(store, kind, rights, shareable, ResourceValue::Synthetic)
    }

    /// Grant an opaque, bidirectional in-memory blob to one logical sketch.
    /// The buffer is host-owned; callers can only append or pull bounded
    /// chunks, so it cannot become a disguised whole-value ABI transport.
    pub(crate) fn create_blob(&self, store: u64) -> Result<OpaqueToken, HubError> {
        let token = self.create_resource_value(
            store,
            BLOB_RESOURCE_KIND,
            BLOB_RIGHT_READ | BLOB_RIGHT_WRITE,
            false,
            ResourceValue::Blob {
                buffer: VecDeque::new(),
                sealed: false,
            },
        )?;
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        state
            .resources
            .get_mut(&token)
            .ok_or(HubError::Invalid)?
            .reserved = false;
        Ok(token)
    }

    pub(crate) fn maximum_blob_chunk_bytes(&self) -> usize {
        self.blob_limits.maximum_chunk_bytes
    }

    /// Append exactly one quota-accounted chunk. A full blob or sketch budget
    /// rejects before copying, which is the synchronous reservation half of
    /// the generated capacity-awaited write operation.
    pub(crate) fn blob_write(
        &self,
        store: u64,
        blob: OpaqueToken,
        bytes: &[u8],
    ) -> Result<usize, HubError> {
        self.blob_write_inner(store, blob, bytes, false)
    }

    fn blob_write_inner(
        &self,
        store: u64,
        blob: OpaqueToken,
        bytes: &[u8],
        unpublished: bool,
    ) -> Result<usize, HubError> {
        if bytes.len() > self.blob_limits.maximum_chunk_bytes {
            return Err(HubError::Quota);
        }
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        if state.buffered_blob_bytes.saturating_add(bytes.len())
            > self.blob_limits.maximum_sketch_bytes
        {
            return Err(HubError::Quota);
        }
        let resource = state.resources.get_mut(&blob).ok_or(HubError::Invalid)?;
        if unpublished {
            if !resource.reserved {
                return Err(HubError::WrongRights);
            }
            Self::validate_resource_owner(resource, store, BLOB_RESOURCE_KIND, BLOB_RIGHT_WRITE)?;
        } else {
            Self::validate_resource(resource, store, BLOB_RESOURCE_KIND, BLOB_RIGHT_WRITE)?;
        }
        let ResourceValue::Blob { buffer, sealed } = &mut resource.value else {
            return Err(HubError::WrongKind);
        };
        if *sealed {
            return Err(HubError::Closed);
        }
        if buffer.len().saturating_add(bytes.len()) > self.blob_limits.maximum_blob_bytes {
            return Err(HubError::Quota);
        }
        Self::reserve_blob_capacity(
            &mut state,
            blob,
            bytes.len(),
            self.blob_limits.maximum_sketch_bytes,
            self.blob_limits.maximum_transfer_bytes - self.blob_limits.maximum_chunk_bytes,
        )?;
        if let ResourceValue::Blob { buffer, .. } = &mut state
            .resources
            .get_mut(&blob)
            .ok_or(HubError::Invalid)?
            .value
        {
            buffer.extend(bytes);
        }
        state.buffered_blob_bytes += bytes.len();
        state.peak_buffered_blob_bytes = state
            .peak_buffered_blob_bytes
            .max(state.buffered_blob_bytes);
        let progress = state
            .resources
            .get(&blob)
            .map(|resource| resource.progress.clone())
            .ok_or(HubError::Invalid)?;
        drop(state);
        progress.report_progress();
        self.drive_blob_reads()?;
        Ok(bytes.len())
    }

    /// Pull at most one configured chunk. Nothing is copied or produced until
    /// the logical guest explicitly asks, and capacity is released before the
    /// next producer attempt observes it.
    #[cfg(test)]
    pub(crate) fn blob_read(
        &self,
        store: u64,
        blob: OpaqueToken,
        maximum_bytes: usize,
    ) -> Result<Vec<u8>, HubError> {
        self.read_blob_chunk(store, blob, maximum_bytes, false)
            .map(|chunk| chunk.to_vec())
    }

    pub(crate) fn read_blob_chunk(
        &self,
        store: u64,
        blob: OpaqueToken,
        maximum_bytes: usize,
        committing: bool,
    ) -> Result<NativeBlobChunk<'_>, HubError> {
        if maximum_bytes > self.blob_limits.maximum_chunk_bytes {
            return Err(HubError::Quota);
        }
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let retained_before_read = Self::transfer_capacity(&state);
        let resource = state.resources.get_mut(&blob).ok_or(HubError::Invalid)?;
        if resource.committing != committing {
            return Err(HubError::WrongRights);
        }
        Self::validate_resource(resource, store, BLOB_RESOURCE_KIND, BLOB_RIGHT_READ)?;
        let ResourceValue::Blob { buffer, .. } = &mut resource.value else {
            return Err(HubError::WrongKind);
        };
        let count = maximum_bytes.min(buffer.len());
        if retained_before_read.saturating_add(count) > self.blob_limits.maximum_transfer_bytes {
            return Err(HubError::Quota);
        }
        let mut result = Vec::with_capacity(count);
        result.extend(buffer.drain(..count));
        if buffer.is_empty() {
            // An empty live resource must not retain a formerly full buffer
            // while its length-based byte ledger reports zero.
            *buffer = VecDeque::new();
        }
        let progress = resource.progress.clone();
        state.peak_retained_transfer_capacity = state
            .peak_retained_transfer_capacity
            .max(retained_before_read.saturating_add(result.capacity()));
        state.buffered_blob_bytes = state.buffered_blob_bytes.saturating_sub(count);
        state.native_transfer_capacity += result.capacity();
        let result = NativeBlobChunk {
            hub: self,
            bytes: result,
            progress,
        };
        drop(state);
        self.drive_blob_writes()?;
        Ok(result)
    }

    /// Reserve a capacity-awaited write in the shared operation table.
    pub(crate) fn submit_blob_write(
        &self,
        store: u64,
        blob: OpaqueToken,
        bytes: &[u8],
    ) -> Result<OpaqueToken, HubError> {
        self.submit_blob_write_from(store, blob, bytes.len(), || bytes.to_vec())
    }

    pub(crate) fn submit_blob_write_wire(
        &self,
        store: u64,
        blob: u64,
        length: usize,
        copy: impl FnOnce() -> Vec<u8>,
    ) -> Result<u64, HubError> {
        self.submit_blob_write_from(store, OpaqueToken(blob), length, copy)
            .map(|token| token.0)
    }

    fn submit_blob_write_from(
        &self,
        store: u64,
        blob: OpaqueToken,
        length: usize,
        copy: impl FnOnce() -> Vec<u8>,
    ) -> Result<OpaqueToken, HubError> {
        self.submit_blob_write_checked(store, blob, length, BLOB_RIGHT_WRITE, copy)
    }

    fn submit_blob_write_checked(
        &self,
        store: u64,
        blob: OpaqueToken,
        length: usize,
        required_rights: u8,
        copy: impl FnOnce() -> Vec<u8>,
    ) -> Result<OpaqueToken, HubError> {
        if length > self.blob_limits.maximum_chunk_bytes {
            return Err(HubError::Quota);
        }
        let (operation, _) = self.submit(store, Some(blob), BLOB_RESOURCE_KIND, required_rights)?;
        {
            let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
            if state
                .operations
                .values()
                .filter(|op| op.pending_blob_write.is_some())
                .count()
                >= self.blob_limits.maximum_pending_writes
            {
                state.operations.remove(&operation);
                return Err(HubError::Quota);
            }
            let pending: usize = state
                .operations
                .values()
                .filter_map(|op| op.pending_blob_write.as_ref())
                .map(Vec::len)
                .sum();
            if pending.saturating_add(length) > self.blob_limits.maximum_sketch_bytes
                || Self::transfer_capacity(&state).saturating_add(length)
                    > self.blob_limits.maximum_transfer_bytes - self.blob_limits.maximum_chunk_bytes
            {
                state.operations.remove(&operation);
                return Err(HubError::Quota);
            }
            let slot = state
                .operations
                .get_mut(&operation)
                .ok_or(HubError::Invalid)?;
            slot.is_blob_write = true;
            if slot.terminal.is_none() {
                // Copy only after authority and capacity checks, without
                // retaining the producer or guest-memory view in the hub.
                slot.pending_blob_write = Some(copy());
                Self::record_transfer_capacity(&mut state);
            }
        }
        self.drive_blob_writes()?;
        Ok(operation)
    }

    fn reserve_blob_capacity(
        state: &mut State,
        blob: OpaqueToken,
        additional: usize,
        maximum: usize,
        transfer_maximum: usize,
    ) -> Result<(), HubError> {
        let transfer_retained = Self::transfer_capacity(state);
        let retained = state.resources.values().fold(0_usize, |total, resource| {
            total.saturating_add(match &resource.value {
                ResourceValue::Blob { buffer, .. } => buffer.capacity(),
                _ => 0,
            })
        });
        let ResourceValue::Blob { buffer, .. } = &mut state
            .resources
            .get_mut(&blob)
            .ok_or(HubError::Invalid)?
            .value
        else {
            return Err(HubError::WrongKind);
        };
        let required = buffer
            .len()
            .checked_add(additional)
            .ok_or(HubError::Quota)?;
        let growth = required.saturating_sub(buffer.capacity());
        if retained.saturating_add(growth) > maximum
            || transfer_retained.saturating_add(growth) > transfer_maximum
        {
            return Err(HubError::Quota);
        }
        // Avoid VecDeque's geometric growth retaining uncharged spare bytes.
        buffer
            .try_reserve_exact(additional)
            .map_err(|_| HubError::Exhausted)?;
        // Pending input is still alive here. Measure before terminalization
        // drops it, so the copy's two backing allocations are both charged.
        Self::record_transfer_capacity(state);
        Ok(())
    }

    fn drive_blob_writes(&self) -> Result<(), HubError> {
        // Each successful pass terminalizes at least one operation. Revisit
        // writes after reads release capacity, without recursive pumping.
        while self.drive_blob_write_pass()? {}
        Ok(())
    }

    fn drive_blob_write_pass(&self) -> Result<bool, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let tokens: Vec<_> = state
            .operations
            .iter()
            .filter(|(_, op)| op.pending_blob_write.is_some() && op.terminal.is_none())
            .map(|(token, _)| *token)
            .collect();
        let mut wakes = Vec::new();
        let mut progress_reports = Vec::new();
        for token in tokens {
            let operation = &state.operations[&token];
            let resource = operation.resource.ok_or(HubError::Invalid)?;
            let count = operation
                .pending_blob_write
                .as_ref()
                .ok_or(HubError::Invalid)?
                .len();
            let mut terminal = match state.resources.get(&resource).map(|slot| &slot.value) {
                Some(ResourceValue::Blob { sealed: true, .. }) | None => Terminal::Closed,
                Some(ResourceValue::Blob { buffer, .. }) => {
                    if buffer.len().saturating_add(count) > self.blob_limits.maximum_blob_bytes
                        || state.buffered_blob_bytes.saturating_add(count)
                            > self.blob_limits.maximum_sketch_bytes
                    {
                        continue;
                    }
                    Terminal::Completed
                }
                _ => Terminal::Rejected,
            };
            if terminal == Terminal::Completed {
                match Self::reserve_blob_capacity(
                    &mut state,
                    resource,
                    count,
                    self.blob_limits.maximum_sketch_bytes,
                    self.blob_limits.maximum_transfer_bytes - self.blob_limits.maximum_chunk_bytes,
                ) {
                    Ok(()) => {}
                    Err(HubError::Quota) => continue,
                    Err(_) => terminal = Terminal::Rejected,
                }
            }
            if terminal == Terminal::Completed {
                let bytes = state
                    .operations
                    .get_mut(&token)
                    .unwrap()
                    .pending_blob_write
                    .take()
                    .unwrap();
                if let ResourceValue::Blob { buffer, .. } =
                    &mut state.resources.get_mut(&resource).unwrap().value
                {
                    buffer.extend(bytes);
                }
                state.buffered_blob_bytes += count;
                state.peak_buffered_blob_bytes = state
                    .peak_buffered_blob_bytes
                    .max(state.buffered_blob_bytes);
                let progress = state
                    .resources
                    .get(&resource)
                    .ok_or(HubError::Invalid)?
                    .progress
                    .clone();
                progress_reports.push(progress);
            }
            if let Some(wake) = Self::terminal_locked(
                &mut state,
                token,
                TerminalResult {
                    terminal,
                    resource: None,
                },
            )? {
                wakes.push(wake);
            }
        }
        drop(state);
        let progressed = !wakes.is_empty();
        for progress in progress_reports {
            progress.report_progress();
        }
        for wake in wakes {
            wake.notify_one();
        }
        Ok(self.drive_blob_read_pass()? || progressed)
    }

    pub(crate) fn submit_blob_read(
        &self,
        store: u64,
        blob: OpaqueToken,
        maximum: usize,
    ) -> Result<OpaqueToken, HubError> {
        if maximum == 0 || maximum > self.blob_limits.maximum_chunk_bytes {
            return Err(HubError::Quota);
        }
        let (token, _) = self.submit(store, Some(blob), BLOB_RESOURCE_KIND, BLOB_RIGHT_READ)?;
        {
            let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
            if state
                .operations
                .values()
                .filter(|op| op.pending_blob_read.is_some())
                .count()
                >= self.blob_limits.maximum_pending_reads
            {
                state.operations.remove(&token);
                return Err(HubError::Quota);
            }
            let operation = state.operations.get_mut(&token).ok_or(HubError::Invalid)?;
            if operation.terminal.is_none() {
                operation.pending_blob_read = Some(maximum);
            }
            operation.is_blob_read = true;
        }
        self.drive_blob_reads()?;
        self.drive_blob_writes()?;
        Ok(token)
    }

    fn drive_blob_reads(&self) -> Result<(), HubError> {
        self.drive_blob_read_pass().map(|_| ())
    }

    fn drive_blob_read_pass(&self) -> Result<bool, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let tokens: Vec<_> = state
            .operations
            .iter()
            .filter(|(_, op)| op.pending_blob_read.is_some() && op.terminal.is_none())
            .map(|(token, _)| *token)
            .collect();
        let mut wakes = Vec::new();
        let mut progress_reports = Vec::new();
        for token in tokens {
            let operation = &state.operations[&token];
            let resource = operation.resource.ok_or(HubError::Invalid)?;
            let maximum = operation.pending_blob_read.ok_or(HubError::Invalid)?;
            let retained_before_read = Self::transfer_capacity(&state);
            let retained: usize = state
                .operations
                .values()
                .filter_map(|op| op.blob_read_result.as_ref())
                .map(Vec::len)
                .sum();
            let Some(ResourceSlot {
                value: ResourceValue::Blob { buffer, sealed },
                progress,
                ..
            }) = state.resources.get_mut(&resource)
            else {
                continue;
            };
            if buffer.is_empty() && !*sealed {
                continue;
            }
            let count = maximum.min(buffer.len());
            if retained.saturating_add(count) > self.blob_limits.maximum_sketch_bytes
                || retained_before_read.saturating_add(count)
                    > self.blob_limits.maximum_transfer_bytes
            {
                continue;
            }
            let mut bytes = Vec::with_capacity(count);
            bytes.extend(buffer.drain(..count));
            if buffer.is_empty() {
                *buffer = VecDeque::new();
            }
            let progress = progress.clone();
            state.peak_retained_transfer_capacity = state
                .peak_retained_transfer_capacity
                .max(retained_before_read.saturating_add(bytes.capacity()));
            state.buffered_blob_bytes -= count;
            state
                .operations
                .get_mut(&token)
                .ok_or(HubError::Invalid)?
                .blob_read_result = Some(bytes);
            progress_reports.push(progress);
            if let Some(wake) = Self::terminal_locked(
                &mut state,
                token,
                TerminalResult {
                    terminal: Terminal::Completed,
                    resource: None,
                },
            )? {
                wakes.push(wake);
            }
        }
        drop(state);
        let progressed = !wakes.is_empty();
        for progress in progress_reports {
            progress.report_progress();
        }
        for wake in wakes {
            wake.notify_one();
        }
        Ok(progressed)
    }

    /// Copy a terminal bounded result during the collecting import. Invalid
    /// destinations and wrong owners leave the operation available to retry.
    pub(crate) fn collect_blob_read_wire(
        &self,
        store: u64,
        token: u64,
        capacity: usize,
        copy: impl FnOnce(&[u8]),
    ) -> Result<u64, HubError> {
        let token = OpaqueToken(token);
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        if state.closed {
            return Err(HubError::Closed);
        }
        let operation = state.operations.get(&token).ok_or(HubError::Invalid)?;
        if operation.owner.store != store {
            return Err(HubError::Stale);
        }
        if !operation.is_blob_read {
            return Err(HubError::WrongKind);
        }
        let Some(result) = operation.terminal else {
            return Ok(0);
        };
        let bytes = operation.blob_read_result.as_deref().unwrap_or_default();
        if capacity < bytes.len() {
            return Err(HubError::Quota);
        }
        let length = bytes.len();
        if result.terminal == Terminal::Completed {
            copy(bytes);
        }
        let packed = (length as u64) << 8 | u64::from(status(result.terminal));
        state.operations.remove(&token);
        state.resumes = state.resumes.saturating_add(1);
        drop(state);
        self.drive_blob_writes()?;
        self.drive_blob_reads()?;
        Ok(packed)
    }

    #[cfg(test)]
    pub(crate) fn take_blob_read(
        &self,
        store: u64,
        token: OpaqueToken,
    ) -> Result<Option<(Terminal, Vec<u8>)>, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        if state.closed {
            return Err(HubError::Closed);
        }
        let operation = state.operations.get(&token).ok_or(HubError::Invalid)?;
        if operation.owner.store != store {
            return Err(HubError::Stale);
        }
        if !operation.is_blob_read {
            return Err(HubError::WrongKind);
        }
        let Some(result) = operation.terminal else {
            return Ok(None);
        };
        let operation = state.operations.remove(&token).ok_or(HubError::Invalid)?;
        state.resumes = state.resumes.saturating_add(1);
        drop(state);
        self.drive_blob_writes()?;
        self.drive_blob_reads()?;
        Ok(Some((
            result.terminal,
            operation.blob_read_result.unwrap_or_default(),
        )))
    }

    /// Publish EOF explicitly. An empty, unsealed blob can still receive data.
    pub(crate) fn seal_blob(&self, store: u64, blob: OpaqueToken) -> Result<(), HubError> {
        self.seal_blob_checked(store, blob, BLOB_RIGHT_WRITE)
    }

    fn seal_blob_checked(
        &self,
        store: u64,
        blob: OpaqueToken,
        required_rights: u8,
    ) -> Result<(), HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let resource = state.resources.get_mut(&blob).ok_or(HubError::Invalid)?;
        Self::validate_resource(resource, store, BLOB_RESOURCE_KIND, required_rights)?;
        let ResourceValue::Blob { sealed, .. } = &mut resource.value else {
            return Err(HubError::WrongKind);
        };
        *sealed = true;
        drop(state);
        self.drive_blob_writes()?;
        self.drive_blob_reads()?;
        Ok(())
    }

    /// Canonicalize one host-authorized final destination and represent it
    /// only as an opaque resource. The generated guest ABI receives the
    /// returned token, never this path or a directory capability.
    #[cfg(feature = "wasm-sketch-host")]
    pub(crate) fn grant_exact_output_wire(
        &self,
        store: u64,
        destination: &Path,
    ) -> Result<u64, HubError> {
        self.grant_exact_output(store, destination)
            .map(|token| token.0)
    }

    #[cfg(feature = "wasm-sketch-host")]
    pub(crate) fn grant_exact_output(
        &self,
        store: u64,
        destination: &Path,
    ) -> Result<OpaqueToken, HubError> {
        let parent = destination.parent().ok_or(HubError::Invalid)?;
        let name = destination.file_name().ok_or(HubError::Invalid)?;
        let parent = fs::canonicalize(parent).map_err(|_| HubError::Invalid)?;
        let destination = parent.join(name);
        let token = self.create_resource_value(
            store,
            OUTPUT_RESOURCE_KIND,
            OUTPUT_RIGHT_COMMIT,
            false,
            ResourceValue::ExactOutput(destination),
        )?;
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        state
            .resources
            .get_mut(&token)
            .ok_or(HubError::Invalid)?
            .reserved = false;
        Ok(token)
    }

    /// Stream one opaque blob into an authorized sibling temporary file and
    /// atomically replace the exact final path only after flush and close.
    /// Bulk writes run without the hub lock. Final replacement is serialized
    /// with revocation; this synchronous helper belongs on a blocking lane.
    #[cfg(feature = "wasm-sketch-host")]
    pub(crate) fn commit_blob_to_output(
        &self,
        store: u64,
        blob: OpaqueToken,
        output: OpaqueToken,
    ) -> std::io::Result<()> {
        self.commit_blob_operation(store, blob, output, None)
    }

    #[cfg(feature = "wasm-sketch-host")]
    pub(crate) fn submit_output_commit(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        store: u64,
        blob: OpaqueToken,
        output: OpaqueToken,
    ) -> Result<OpaqueToken, HubError> {
        let (operation, _) = self.submit(
            store,
            Some(output),
            OUTPUT_RESOURCE_KIND,
            OUTPUT_RIGHT_COMMIT,
        )?;
        let hub = Arc::clone(self);
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        // Guest cancellation does not cancel a running filesystem syscall.
        // Bound queued/running jobs independently of consumed operations.
        state.output_jobs.retain(|job| !job.is_finished());
        if state.closed
            || self.output_job_count.load(Ordering::Acquire) >= self.maximum_operations as u64
        {
            state.operations.remove(&operation);
            return Err(if state.closed {
                HubError::Closed
            } else {
                HubError::Quota
            });
        }
        self.output_job_count.fetch_add(1, Ordering::AcqRel);
        let job_lease = OutputJobLease(Arc::clone(self));
        let job = runtime.launch_blocking(move || {
            let _job_lease = job_lease;
            if hub
                .commit_blob_operation(store, blob, output, Some(operation))
                .is_err()
            {
                let _ = hub.terminal(
                    operation,
                    TerminalResult {
                        terminal: Terminal::Rejected,
                        resource: None,
                    },
                );
            }
        });
        state.output_jobs.push(job);
        Ok(operation)
    }

    #[cfg(feature = "wasm-sketch-host")]
    fn commit_blob_operation(
        &self,
        store: u64,
        blob: OpaqueToken,
        output: OpaqueToken,
        operation: Option<OpaqueToken>,
    ) -> std::io::Result<()> {
        self.check_output_operation(store, blob, output, operation)?;
        let destination = self.exact_output_path(store, output)?;
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| hub_io_error(HubError::Closed))?;
            if state
                .operations
                .values()
                .any(|operation| operation.resource == Some(blob) && operation.terminal.is_none())
            {
                return Err(hub_io_error(HubError::WrongRights));
            }
            let resource = state
                .resources
                .get_mut(&blob)
                .ok_or_else(|| hub_io_error(HubError::Invalid))?;
            Self::validate_resource(resource, store, BLOB_RESOURCE_KIND, BLOB_RIGHT_READ)
                .map_err(hub_io_error)?;
            if resource.committing {
                return Err(hub_io_error(HubError::WrongRights));
            }
            if !matches!(resource.value, ResourceValue::Blob { sealed: true, .. }) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "blob producer has not published EOF",
                ));
            }
            resource.committing = true;
        }
        let _consumption = BlobCommitLease { hub: self, blob };
        let temporary = self.temporary_output_path(&destination)?;
        // Only clean up a file this operation successfully created. A
        // competing creator must never have its file removed on open failure.
        let mut temporary = TemporaryOutput::create(temporary)?;
        let result = (|| {
            loop {
                self.check_output_operation(store, blob, output, operation)?;
                let chunk = self
                    .read_blob_chunk(store, blob, self.blob_limits.maximum_chunk_bytes, true)
                    .map_err(hub_io_error)?;
                if chunk.is_empty() {
                    break;
                }
                temporary
                    .file
                    .as_mut()
                    .expect("open temporary")
                    .write_all(&chunk)?;
                #[cfg(feature = "wasm-sketch-worker-test-support")]
                pause_worker_output_for_containment_proof(&destination)?;
                #[cfg(test)]
                self.inject_output_fault(OutputFault::AfterWrite)?;
            }
            self.check_output_operation(store, blob, output, operation)?;
            #[cfg(test)]
            self.inject_output_fault(OutputFault::Sync)?;
            temporary
                .file
                .as_ref()
                .expect("open temporary")
                .sync_all()?;
            drop(temporary.file.take());
            self.replace_output_operation(store, blob, output, &temporary.path, operation)
        })();
        temporary.committed = result.is_ok();
        result
    }

    #[cfg(all(test, feature = "wasm-sketch-host"))]
    fn inject_output_fault(&self, stage: OutputFault) -> std::io::Result<()> {
        let mut fault = self.output_fault.lock().unwrap();
        if *fault == Some(stage) {
            *fault = None;
            return Err(std::io::Error::other("injected output I/O failure"));
        }
        Ok(())
    }

    /// Cooperative checkpoints do not interrupt an already-issued filesystem
    /// call; they stop cancelled work before another bounded chunk or sync.
    #[cfg(feature = "wasm-sketch-host")]
    fn check_output_operation(
        &self,
        store: u64,
        blob: OpaqueToken,
        output: OpaqueToken,
        operation: Option<OpaqueToken>,
    ) -> std::io::Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| hub_io_error(HubError::Closed))?;
        if state.closed {
            return Err(hub_io_error(HubError::Closed));
        }
        if let Some(token) = operation {
            let slot = state
                .operations
                .get(&token)
                .ok_or_else(|| hub_io_error(HubError::Closed))?;
            if slot.owner.store != store || slot.resource != Some(output) || slot.terminal.is_some()
            {
                return Err(hub_io_error(HubError::Closed));
            }
        }
        for (token, kind, rights) in [
            (blob, BLOB_RESOURCE_KIND, BLOB_RIGHT_READ),
            (output, OUTPUT_RESOURCE_KIND, OUTPUT_RIGHT_COMMIT),
        ] {
            let resource = state
                .resources
                .get(&token)
                .ok_or_else(|| hub_io_error(HubError::Closed))?;
            Self::validate_resource(resource, store, kind, rights).map_err(hub_io_error)?;
        }
        Ok(())
    }

    #[cfg(feature = "wasm-sketch-host")]
    fn replace_authorized_output(
        &self,
        store: u64,
        blob: OpaqueToken,
        output: OpaqueToken,
        temporary: &Path,
    ) -> std::io::Result<()> {
        self.replace_output_operation(store, blob, output, temporary, None)
    }

    #[cfg(feature = "wasm-sketch-host")]
    fn replace_output_operation(
        &self,
        store: u64,
        blob: OpaqueToken,
        output: OpaqueToken,
        temporary: &Path,
        operation: Option<OpaqueToken>,
    ) -> std::io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| hub_io_error(HubError::Closed))?;
        if state.closed {
            return Err(hub_io_error(HubError::Closed));
        }
        if let Some(token) = operation {
            let slot = state
                .operations
                .get(&token)
                .ok_or_else(|| hub_io_error(HubError::Closed))?;
            if slot.owner.store != store || slot.terminal.is_some() {
                return Err(hub_io_error(HubError::Closed));
            }
        }
        let resource = state
            .resources
            .get(&blob)
            .ok_or_else(|| hub_io_error(HubError::Closed))?;
        Self::validate_resource(resource, store, BLOB_RESOURCE_KIND, BLOB_RIGHT_READ)
            .map_err(hub_io_error)?;
        let resource = state
            .resources
            .get(&output)
            .ok_or_else(|| hub_io_error(HubError::Closed))?;
        Self::validate_resource(resource, store, OUTPUT_RESOURCE_KIND, OUTPUT_RIGHT_COMMIT)
            .map_err(hub_io_error)?;
        let ResourceValue::ExactOutput(destination) = &resource.value else {
            return Err(hub_io_error(HubError::WrongKind));
        };
        // Revocation cannot interleave between this last authority check and
        // the filesystem commit. No await, callback, or guest re-entry occurs.
        #[cfg(test)]
        self.inject_output_fault(OutputFault::Replace)?;
        crate::fs_replace_file(temporary, destination)?;
        let mut wakes = Vec::new();
        if let Some(token) = operation {
            if let Some(wake) = Self::terminal_locked(
                &mut state,
                token,
                TerminalResult {
                    terminal: Terminal::Completed,
                    resource: None,
                },
            )
            .map_err(hub_io_error)?
            {
                wakes.push(wake);
            }
        }
        wakes.extend(
            Self::close_resource_with_terminal_locked(&mut state, blob, Terminal::Closed)
                .map_err(hub_io_error)?,
        );
        wakes.extend(
            Self::close_resource_with_terminal_locked(&mut state, output, Terminal::Closed)
                .map_err(hub_io_error)?,
        );
        drop(state);
        for wake in wakes {
            wake.notify_one();
        }
        Ok(())
    }

    #[cfg(feature = "wasm-sketch-host")]
    fn exact_output_path(&self, store: u64, output: OpaqueToken) -> std::io::Result<PathBuf> {
        let state = self
            .state
            .lock()
            .map_err(|_| hub_io_error(HubError::Closed))?;
        let resource = state
            .resources
            .get(&output)
            .ok_or_else(|| hub_io_error(HubError::Invalid))?;
        Self::validate_resource(resource, store, OUTPUT_RESOURCE_KIND, OUTPUT_RIGHT_COMMIT)
            .map_err(hub_io_error)?;
        match &resource.value {
            ResourceValue::ExactOutput(path) => Ok(path.clone()),
            _ => Err(hub_io_error(HubError::WrongKind)),
        }
    }

    #[cfg(feature = "wasm-sketch-host")]
    fn temporary_output_path(&self, destination: &Path) -> std::io::Result<PathBuf> {
        let parent = destination
            .parent()
            .ok_or_else(|| hub_io_error(HubError::Invalid))?;
        let name = destination
            .file_name()
            .ok_or_else(|| hub_io_error(HubError::Invalid))?;
        for _ in 0..16 {
            let mut temporary_name = name.to_os_string();
            temporary_name.push(format!(
                ".kernal-api-{}.tmp",
                next_token().map_err(hub_io_error)?
            ));
            let temporary = parent.join(temporary_name);
            if !temporary.exists() {
                return Ok(temporary);
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not reserve a unique exact-output temporary path",
        ))
    }

    fn validate_resource(
        resource: &ResourceSlot,
        store: u64,
        kind: u8,
        rights: u8,
    ) -> Result<(), HubError> {
        if resource.reserved {
            return Err(HubError::Closed);
        }
        Self::validate_resource_owner(resource, store, kind, rights)
    }

    fn validate_resource_owner(
        resource: &ResourceSlot,
        store: u64,
        kind: u8,
        rights: u8,
    ) -> Result<(), HubError> {
        if resource.identity.kind != kind {
            return Err(HubError::WrongKind);
        }
        if resource.identity.rights & rights != rights {
            return Err(HubError::WrongRights);
        }
        if resource.owner.store != store && !resource.shareable {
            return Err(HubError::WrongRights);
        }
        Ok(())
    }

    fn create_resource_value(
        &self,
        store: u64,
        kind: u8,
        rights: u8,
        shareable: bool,
        value: ResourceValue,
    ) -> Result<OpaqueToken, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        self.create_resource_value_locked(&mut state, store, kind, rights, shareable, value)
    }

    fn create_resource_value_locked(
        &self,
        state: &mut State,
        store: u64,
        kind: u8,
        rights: u8,
        shareable: bool,
        value: ResourceValue,
    ) -> Result<OpaqueToken, HubError> {
        if state.closed {
            return Err(HubError::Closed);
        }
        if state.resources.len() >= self.maximum_resources {
            return Err(HubError::Quota);
        }
        if matches!(&value, ResourceValue::Blob { .. })
            && state
                .resources
                .values()
                .filter(|resource| matches!(&resource.value, ResourceValue::Blob { .. }))
                .count()
                >= self.blob_limits.maximum_live_blobs
        {
            return Err(HubError::Quota);
        }
        let slot = match state.free_resource_slots.pop() {
            Some(slot) => slot,
            None => {
                state.next_resource_slot = state
                    .next_resource_slot
                    .checked_add(1)
                    .ok_or(HubError::Exhausted)?;
                state.next_resource_slot
            }
        };
        let generation = state
            .resource_generations
            .get(&slot)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(HubError::Exhausted)?;
        state.resource_generations.insert(slot, generation);
        let token = OpaqueToken(next_token()?);
        state.resources.insert(
            token,
            ResourceSlot {
                identity: ResourceIdentity {
                    scope: self.scope,
                    slot,
                    generation,
                    kind,
                    rights,
                },
                owner: Owner { store },
                shareable,
                value,
                reserved: true,
                committing: false,
                progress: crate::async_engine::ProgressReporter::new(),
            },
        );
        Ok(token)
    }

    pub(super) fn submit(
        &self,
        store: u64,
        resource: Option<OpaqueToken>,
        kind: u8,
        required_rights: u8,
    ) -> Result<(OpaqueToken, Arc<Notify>), HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        self.submit_locked(&mut state, store, resource, kind, required_rights)
    }

    fn submit_locked(
        &self,
        state: &mut State,
        store: u64,
        resource: Option<OpaqueToken>,
        kind: u8,
        required_rights: u8,
    ) -> Result<(OpaqueToken, Arc<Notify>), HubError> {
        if state.closed {
            return Err(HubError::Closed);
        }
        if state.operations.len() >= self.maximum_operations {
            return Err(HubError::Quota);
        }
        if let Some(resource) = resource {
            let slot = state.resources.get(&resource).ok_or(HubError::Invalid)?;
            if slot.committing {
                return Err(HubError::WrongRights);
            }
            if slot.reserved {
                return Err(HubError::Closed);
            }
            if slot.identity.scope != self.scope {
                return Err(HubError::Stale);
            }
            if slot.identity.kind != kind {
                return Err(HubError::WrongKind);
            }
            if slot.identity.rights & required_rights != required_rights {
                return Err(HubError::WrongRights);
            }
            // A resource created by another Store is usable only through the
            // explicit sharing bit.
            if slot.owner.store != store && !slot.shareable {
                return Err(HubError::WrongRights);
            }
        }
        let token = OpaqueToken(next_token()?);
        let notify = Arc::new(Notify::new());
        state.operations.insert(
            token,
            OperationSlot {
                #[cfg(feature = "wasm-sketch-host")]
                compiler_scalar: None,
                #[cfg(feature = "wasm-sketch-host")]
                compiler_exit: None,
                #[cfg(feature = "wasm-sketch-host")]
                is_compiler_read: false,
                #[cfg(feature = "wasm-sketch-host")]
                compiler_output: None,
                #[cfg(feature = "wasm-sketch-host")]
                is_compiler_spawn: false,
                is_hash_operation: false,
                #[cfg(all(test, feature = "archive-auth-test-support"))]
                is_archive_operation: false,
                #[cfg(all(test, feature = "archive-auth-test-support"))]
                pending_authentication: None,
                owner: Owner { store },
                resource,
                required_rights,
                terminal: None,
                deferred_completion: None,
                suspended: false,
                notify: Arc::clone(&notify),
                created_resource: None,
                pending_blob_write: None,
                pending_blob_read: None,
                is_blob_read: false,
                is_blob_write: false,
                blob_read_result: None,
                producer_cancel: None,
                progress_timeout_cancel: None,
            },
        );
        Ok((token, notify))
    }

    pub(super) fn suspend(&self, token: OpaqueToken, store: u64) -> Result<Arc<Notify>, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let (notify, deferred) = {
            let operation = state.operations.get(&token).ok_or(HubError::Invalid)?;
            if operation.owner.store != store {
                return Err(HubError::Stale);
            }
            if operation.terminal.is_some() {
                // Poll and yield are separate guest imports. Native I/O can
                // complete between them. Preserve the result for the next
                // poll and return a ready waiter, not an import failure.
                let notify = Arc::clone(&operation.notify);
                state.suspends = state.suspends.saturating_add(1);
                drop(state);
                notify.notify_one();
                return Ok(notify);
            }
            (Arc::clone(&operation.notify), operation.deferred_completion)
        };
        let operation = state.operations.get_mut(&token).ok_or(HubError::Invalid)?;
        operation.suspended = true;
        operation.deferred_completion = None;
        state.suspends = state.suspends.saturating_add(1);
        drop(state);
        if let Some(deferred) = deferred {
            self.finish_deferred_completion(token, deferred)?;
        }
        Ok(notify)
    }

    /// Complete a synthetic operation only after its generated guest future
    /// has registered a suspension. Synthetic fixtures deliberately exercise
    /// a pending outcome; real native I/O may complete before suspension and
    /// is handled by the ready-waiter path above.
    fn complete_after_suspend(
        &self,
        token: OpaqueToken,
        result: TerminalResult,
        revoke: Option<OpaqueToken>,
    ) -> Result<(), HubError> {
        let deferred = DeferredCompletion { result, revoke };
        let ready = {
            let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
            let operation = state.operations.get_mut(&token).ok_or(HubError::Invalid)?;
            if operation.terminal.is_some() {
                return Ok(());
            }
            if !operation.suspended {
                operation.deferred_completion = Some(deferred);
                false
            } else {
                true
            }
        };
        if ready {
            self.finish_deferred_completion(token, deferred)?;
        }
        Ok(())
    }

    fn finish_deferred_completion(
        &self,
        token: OpaqueToken,
        deferred: DeferredCompletion,
    ) -> Result<(), HubError> {
        // Claim the terminal result and resource revocation under the one hub
        // mutex. A cancellation that wins first therefore leaves the resource
        // live; a close that wins first revokes it before either outcome is
        // observable. Notifications happen only after releasing the lock.
        let notifications = {
            let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
            if state
                .operations
                .get(&token)
                .ok_or(HubError::Invalid)?
                .terminal
                .is_some()
            {
                return Ok(());
            }
            let mut notifications = if let Some(resource) = deferred.revoke {
                Self::close_resource_with_terminal_locked(&mut state, resource, Terminal::Closed)?
            } else {
                Vec::new()
            };
            if let Some(notify) = Self::terminal_locked(&mut state, token, deferred.result)? {
                notifications.push(notify);
            }
            notifications
        };
        for notify in notifications {
            notify.notify_one();
        }
        self.drive_blob_writes()?;
        Ok(())
    }

    pub(super) fn take_terminal(
        &self,
        token: OpaqueToken,
        store: u64,
    ) -> Result<Option<TerminalResult>, HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let terminal = {
            let operation = state.operations.get(&token).ok_or(HubError::Invalid)?;
            if operation.owner.store != store {
                return Err(HubError::Stale);
            }
            #[cfg(feature = "wasm-sketch-host")]
            if operation.is_compiler_read
                || operation.compiler_scalar == Some(process_resource::CompilerScalarKind::Wait)
            {
                return Err(HubError::WrongKind);
            }
            operation.terminal
        };
        if terminal.is_some() {
            state.resumes = state.resumes.saturating_add(1);
            state.operations.remove(&token);
        }
        Ok(terminal)
    }

    pub(super) fn terminal(
        &self,
        token: OpaqueToken,
        result: TerminalResult,
    ) -> Result<(), HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let notify = Self::terminal_locked(&mut state, token, result)?;
        drop(state);
        if let Some(notify) = notify {
            // One generated guest future owns one operation. `notify_one`
            // preserves a completion that wins before waiter registration.
            notify.notify_one();
        }
        Ok(())
    }

    fn terminal_locked(
        state: &mut State,
        token: OpaqueToken,
        result: TerminalResult,
    ) -> Result<Option<Arc<Notify>>, HubError> {
        let (notify, created) = {
            let operation = state.operations.get_mut(&token).ok_or(HubError::Invalid)?;
            #[cfg(not(feature = "wasm-sketch-host"))]
            let replace_ready = false;
            #[cfg(feature = "wasm-sketch-host")]
            let replace_ready = (operation.is_compiler_read
                || operation.compiler_scalar == Some(process_resource::CompilerScalarKind::Wait))
                && operation
                    .terminal
                    .is_some_and(|old| old.terminal == Terminal::Completed)
                && result.terminal != Terminal::Completed;
            if operation.terminal.is_some() && !replace_ready {
                return Ok(None);
            }
            #[cfg(feature = "wasm-sketch-host")]
            if result.terminal != Terminal::Completed {
                drop(operation.compiler_output.take());
                operation.compiler_exit = None;
            }
            operation.terminal = Some(result);
            #[cfg(all(test, feature = "archive-auth-test-support"))]
            drop(operation.pending_authentication.take());
            if let Some(cancel) = operation.producer_cancel.take() {
                cancel.cancel();
            }
            if let Some(cancel) = operation.progress_timeout_cancel.take() {
                cancel.cancel();
            }
            operation.pending_blob_write = None;
            operation.pending_blob_read = None;
            (Arc::clone(&operation.notify), operation.created_resource)
        };
        if let Some(resource) = created {
            if result.terminal == Terminal::Completed {
                if let Some(resource) = state.resources.get_mut(&resource) {
                    resource.reserved = false;
                }
            } else if let Some(resource) = state.resources.remove(&resource) {
                if let ResourceValue::Blob { buffer, .. } = &resource.value {
                    state.buffered_blob_bytes =
                        state.buffered_blob_bytes.saturating_sub(buffer.len());
                }
                state.free_resource_slots.push(resource.identity.slot);
            }
        }
        Ok(Some(notify))
    }

    pub(super) fn close_resource(&self, token: OpaqueToken) -> Result<(), HubError> {
        self.close_resource_with_terminal(token, Terminal::Closed)
    }

    /// Revoke a native resource generation and wake every operation borrowing
    /// it with the callback's semantic terminal reason.  This preserves a
    /// rejected navigation or timeout instead of racing it into a generic
    /// `Closed` result.
    pub(crate) fn revoke_external_resource(
        &self,
        token: OpaqueToken,
        terminal: Terminal,
    ) -> Result<(), HubError> {
        self.close_resource_with_terminal(token, terminal)
    }

    fn close_resource_with_terminal(
        &self,
        token: OpaqueToken,
        terminal: Terminal,
    ) -> Result<(), HubError> {
        let mut state = self.state.lock().map_err(|_| HubError::Closed)?;
        let notifications = Self::close_resource_with_terminal_locked(&mut state, token, terminal)?;
        drop(state);
        for notify in notifications {
            notify.notify_one();
        }
        self.drive_blob_writes()?;
        Ok(())
    }

    fn close_resource_with_terminal_locked(
        state: &mut State,
        token: OpaqueToken,
        terminal: Terminal,
    ) -> Result<Vec<Arc<Notify>>, HubError> {
        let Some(resource) = state.resources.remove(&token) else {
            return if state.closed_resources.contains(&token) {
                Ok(Vec::new())
            } else {
                Err(HubError::Invalid)
            };
        };
        if let ResourceValue::Blob { buffer, .. } = &resource.value {
            state.buffered_blob_bytes = state.buffered_blob_bytes.saturating_sub(buffer.len());
        }
        state.closed_resources.insert(token);
        state.closed_resource_order.push_back(token);
        while state.closed_resource_order.len() > MAX_CLOSED_TOMBSTONES {
            if let Some(expired) = state.closed_resource_order.pop_front() {
                state.closed_resources.remove(&expired);
            }
        }
        state.free_resource_slots.push(resource.identity.slot);
        let mut notifications = Vec::new();
        let operations: Vec<_> = state
            .operations
            .iter()
            .filter(|(_, operation)| {
                #[cfg(not(feature = "wasm-sketch-host"))]
                let ready_read = false;
                #[cfg(feature = "wasm-sketch-host")]
                let ready_read = (operation.is_compiler_read
                    || operation.compiler_scalar
                        == Some(process_resource::CompilerScalarKind::Wait))
                    && operation
                        .terminal
                        .is_some_and(|result| result.terminal == Terminal::Completed);
                operation.resource == Some(token) && (operation.terminal.is_none() || ready_read)
            })
            .map(|(token, _)| *token)
            .collect();
        for operation in operations {
            if let Some(notify) = Self::terminal_locked(
                state,
                operation,
                TerminalResult {
                    terminal,
                    resource: None,
                },
            )? {
                notifications.push(notify);
            }
        }
        Ok(notifications)
    }

    pub(super) fn close_all(&self, terminal: Terminal) {
        let Ok(state) = self.state.lock() else {
            return;
        };
        Self::close_locked(state, terminal);
    }

    /// The shared epoch ticker must never wait behind a filesystem commit.
    #[cfg(feature = "wasm-sketch-host")]
    pub(super) fn try_close_all(&self, terminal: Terminal) -> bool {
        let Ok(state) = self.state.try_lock() else {
            return false;
        };
        Self::close_locked(state, terminal);
        true
    }

    fn close_locked(mut state: std::sync::MutexGuard<'_, State>, terminal: Terminal) {
        state.closed = true;
        state.resources.clear();
        state.buffered_blob_bytes = 0;
        state.free_resource_slots.clear();
        let mut notifications = Vec::new();
        for operation in state.operations.values_mut() {
            #[cfg(all(test, feature = "archive-auth-test-support"))]
            drop(operation.pending_authentication.take());
            if let Some(cancel) = operation.producer_cancel.take() {
                cancel.cancel();
            }
            if let Some(cancel) = operation.progress_timeout_cancel.take() {
                cancel.cancel();
            }
            operation.blob_read_result = None;
            #[cfg(not(feature = "wasm-sketch-host"))]
            let ready_read = false;
            #[cfg(feature = "wasm-sketch-host")]
            let ready_read = (operation.is_compiler_read
                || operation.compiler_scalar == Some(process_resource::CompilerScalarKind::Wait))
                && operation
                    .terminal
                    .is_some_and(|result| result.terminal == Terminal::Completed);
            #[cfg(feature = "wasm-sketch-host")]
            drop(operation.compiler_output.take());
            #[cfg(feature = "wasm-sketch-host")]
            {
                operation.compiler_exit = None;
            }
            if operation.terminal.is_none() || ready_read {
                operation.pending_blob_write = None;
                operation.pending_blob_read = None;
                operation.terminal = Some(TerminalResult {
                    terminal,
                    resource: None,
                });
                notifications.push(Arc::clone(&operation.notify));
            }
        }
        drop(state);
        for notify in notifications {
            notify.notify_one();
        }
    }

    fn transfer_capacity(state: &State) -> usize {
        let native_capacity = state.native_transfer_capacity;
        #[cfg(feature = "wasm-sketch-host")]
        let native_capacity = native_capacity
            .saturating_add(state.reserved_process_output_bytes)
            .saturating_add(state.reserved_native_process_output_bytes);
        state
            .resources
            .values()
            .map(|resource| match &resource.value {
                ResourceValue::Blob { buffer, .. } => buffer.capacity(),
                _ => 0,
            })
            .chain(state.operations.values().map(|operation| {
                let capacity = operation
                    .pending_blob_write
                    .as_ref()
                    .map_or(0, Vec::capacity)
                    .saturating_add(operation.blob_read_result.as_ref().map_or(0, Vec::capacity));
                #[cfg(feature = "wasm-sketch-host")]
                let capacity = capacity.saturating_add(if operation.compiler_output.is_some() {
                    process_resource::MAX_PROCESS_OUTPUT_CHUNK
                } else {
                    0
                });
                capacity
            }))
            .fold(native_capacity, usize::saturating_add)
    }

    fn record_transfer_capacity(state: &mut State) {
        state.peak_retained_transfer_capacity = state
            .peak_retained_transfer_capacity
            .max(Self::transfer_capacity(state));
    }

    pub(crate) fn snapshot(&self) -> HubSnapshot {
        let state = self.state.lock().expect("operation hub mutex poisoned");
        HubSnapshot {
            #[cfg(all(test, feature = "archive-auth-test-support"))]
            archive_staging_bytes: self.staging_budget.used(),
            #[cfg(all(
                test,
                feature = "archive-auth-test-support",
                feature = "wasm-sketch-host"
            ))]
            active_archive_jobs: self
                .archive_jobs
                .lock()
                .map(|jobs| jobs.active())
                .unwrap_or(usize::MAX),
            scope: self.scope,
            pending_operations: state
                .operations
                .values()
                .filter(|operation| operation.terminal.is_none())
                .count(),
            live_resources: state.resources.len(),
            live_blobs: state
                .resources
                .values()
                .filter(|resource| matches!(&resource.value, ResourceValue::Blob { .. }))
                .count(),
            pending_blob_reads: state
                .operations
                .values()
                .filter(|op| op.pending_blob_read.is_some())
                .count(),
            pending_blob_writes: state
                .operations
                .values()
                .filter(|op| op.pending_blob_write.is_some())
                .count(),
            suspends: state.suspends,
            resumes: state.resumes,
            buffered_blob_bytes: state.buffered_blob_bytes,
            peak_buffered_blob_bytes: state.peak_buffered_blob_bytes,
            pending_write_bytes: state
                .operations
                .values()
                .filter_map(|operation| operation.pending_blob_write.as_ref())
                .map(Vec::len)
                .sum(),
            completed_read_bytes: state
                .operations
                .values()
                .filter_map(|operation| operation.blob_read_result.as_ref())
                .map(Vec::len)
                .sum(),
            retained_transfer_capacity: Self::transfer_capacity(&state),
            peak_retained_transfer_capacity: state.peak_retained_transfer_capacity,
            native_transfer_capacity: state.native_transfer_capacity,
            #[cfg(feature = "wasm-sketch-host")]
            reserved_process_output_bytes: state.reserved_process_output_bytes.saturating_add(
                state
                    .operations
                    .values()
                    .filter(|operation| operation.compiler_output.is_some())
                    .count()
                    .saturating_mul(process_resource::MAX_PROCESS_OUTPUT_CHUNK),
            ),
            #[cfg(feature = "wasm-sketch-host")]
            reserved_native_process_output_bytes: state.reserved_native_process_output_bytes,
            active_output_jobs: self.output_job_count.load(Ordering::Acquire) as usize,
            #[cfg(feature = "wasm-sketch-host")]
            retained_process_jobs: state.process_jobs.len() + state.process_io_jobs.len(),
            active_native_captures: self.native_capture_count.load(Ordering::Acquire) as usize,
            active_native_opens: self.native_open_count.load(Ordering::Acquire) as usize,
            active_clocks: self.clock_count.load(Ordering::Acquire) as usize,
        }
    }
}

/// Bounded native encoder sink. Its reserved blob cannot be read by a guest
/// until encoding succeeds; failure or abandonment revokes all stored bytes.
pub(crate) struct NativeBlobEncoder {
    hub: Arc<OperationHub>,
    store: u64,
    blob: Option<OpaqueToken>,
    bound_operation: Option<OpaqueToken>,
    maximum_bytes: usize,
    written: usize,
    position: usize,
    failure: Option<HubError>,
}

impl NativeBlobEncoder {
    pub(crate) fn failure(&self) -> Option<HubError> {
        self.failure
    }

    pub(crate) fn new(
        hub: Arc<OperationHub>,
        store: u64,
        maximum_bytes: usize,
    ) -> Result<Self, HubError> {
        if maximum_bytes == 0 || maximum_bytes > hub.blob_limits.maximum_blob_bytes {
            return Err(HubError::Quota);
        }
        let blob = hub.create_resource_value(
            store,
            BLOB_RESOURCE_KIND,
            BLOB_RIGHT_READ | BLOB_RIGHT_WRITE,
            false,
            ResourceValue::Blob {
                buffer: VecDeque::new(),
                sealed: false,
            },
        )?;
        Ok(Self {
            hub,
            store,
            blob: Some(blob),
            bound_operation: None,
            maximum_bytes,
            written: 0,
            position: 0,
            failure: None,
        })
    }

    pub(crate) fn for_operation(
        hub: Arc<OperationHub>,
        store: u64,
        operation: OpaqueToken,
        maximum_bytes: usize,
    ) -> Result<Self, HubError> {
        let mut encoder = Self::new(hub, store, maximum_bytes)?;
        {
            let mut state = encoder.hub.state.lock().map_err(|_| HubError::Closed)?;
            Self::validate_capture_operation(&state, store, operation, None)?;
            state
                .operations
                .get_mut(&operation)
                .ok_or(HubError::Closed)?
                .created_resource = encoder.blob;
        }
        encoder.bound_operation = Some(operation);
        Ok(encoder)
    }

    fn validate_capture_operation(
        state: &State,
        store: u64,
        operation: OpaqueToken,
        blob: Option<OpaqueToken>,
    ) -> Result<(), HubError> {
        if state.closed {
            return Err(HubError::Closed);
        }
        let pending = state.operations.get(&operation).ok_or(HubError::Closed)?;
        if pending.owner.store != store {
            return Err(HubError::WrongRights);
        }
        if pending.terminal.is_some() {
            return Err(HubError::Closed);
        }
        if pending.created_resource.is_some() && pending.created_resource != blob {
            return Err(HubError::WrongRights);
        }
        if pending.required_rights != EXTERNAL_WEBVIEW_RIGHT_CAPTURE {
            return Err(HubError::WrongRights);
        }
        let view = pending.resource.ok_or(HubError::WrongKind)?;
        let view = state.resources.get(&view).ok_or(HubError::Closed)?;
        OperationHub::validate_resource(
            view,
            store,
            EXTERNAL_WEBVIEW_RESOURCE_KIND,
            EXTERNAL_WEBVIEW_RIGHT_CAPTURE,
        )
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), HubError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = (|| {
            let total = self
                .written
                .checked_add(bytes.len())
                .ok_or(HubError::Quota)?;
            if total > self.maximum_bytes {
                return Err(HubError::Quota);
            }
            for chunk in bytes.chunks(self.hub.blob_limits.maximum_chunk_bytes) {
                self.hub.blob_write_inner(
                    self.store,
                    self.blob.ok_or(HubError::Closed)?,
                    chunk,
                    true,
                )?;
            }
            self.written = total;
            Ok(())
        })();
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }

    fn write_at_position(&mut self, bytes: &[u8]) -> Result<(), HubError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let result = (|| {
            let end = self
                .position
                .checked_add(bytes.len())
                .ok_or(HubError::Quota)?;
            if end > self.maximum_bytes {
                return Err(HubError::Quota);
            }
            // Seeking allocates nothing. A later write fills any hole through
            // the same chunked admission path as normal encoding.
            while self.written < self.position {
                let zeros = [0_u8; 1024];
                let count = (self.position - self.written).min(zeros.len());
                self.append(&zeros[..count])?;
            }
            let overwrite = bytes.len().min(self.written - self.position);
            if overwrite > 0 {
                let mut state = self.hub.state.lock().map_err(|_| HubError::Closed)?;
                if state.closed {
                    return Err(HubError::Closed);
                }
                let resource = state
                    .resources
                    .get_mut(&self.blob.ok_or(HubError::Closed)?)
                    .ok_or(HubError::Closed)?;
                OperationHub::validate_resource_owner(
                    resource,
                    self.store,
                    BLOB_RESOURCE_KIND,
                    BLOB_RIGHT_WRITE,
                )?;
                if !resource.reserved {
                    return Err(HubError::WrongRights);
                }
                let ResourceValue::Blob {
                    buffer,
                    sealed: false,
                } = &mut resource.value
                else {
                    return Err(HubError::WrongKind);
                };
                for (index, byte) in bytes[..overwrite].iter().enumerate() {
                    *buffer
                        .get_mut(self.position + index)
                        .ok_or(HubError::Closed)? = *byte;
                }
            }
            self.append(&bytes[overwrite..])?;
            self.position = end;
            Ok(())
        })();
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }

    pub(crate) fn finish(self) -> Result<OpaqueToken, HubError> {
        self.finish_inner(None)
    }

    /// Atomically publish encoded storage and complete its native operation.
    /// Cancellation/close and publication choose one winner under the hub lock.
    pub(crate) fn finish_for_operation(
        self,
        operation: OpaqueToken,
    ) -> Result<OpaqueToken, HubError> {
        self.finish_inner(Some(operation))
    }

    fn finish_inner(mut self, operation: Option<OpaqueToken>) -> Result<OpaqueToken, HubError> {
        if self.bound_operation.is_some() && self.bound_operation != operation {
            return Err(HubError::WrongRights);
        }
        if let Some(error) = self.failure {
            return Err(error);
        }
        let token = self.blob.ok_or(HubError::Closed)?;
        let mut state = self.hub.state.lock().map_err(|_| HubError::Closed)?;
        if state.closed {
            return Err(HubError::Closed);
        }
        if let Some(operation) = operation {
            Self::validate_capture_operation(&state, self.store, operation, Some(token))?;
        }
        let resource = state.resources.get_mut(&token).ok_or(HubError::Closed)?;
        OperationHub::validate_resource_owner(
            resource,
            self.store,
            BLOB_RESOURCE_KIND,
            BLOB_RIGHT_WRITE,
        )?;
        if !resource.reserved {
            return Err(HubError::WrongRights);
        }
        let ResourceValue::Blob { sealed, .. } = &mut resource.value else {
            return Err(HubError::WrongKind);
        };
        *sealed = true;
        resource.identity.rights = BLOB_RIGHT_READ;
        resource.reserved = operation.is_some();
        let notify = if let Some(operation) = operation {
            state
                .operations
                .get_mut(&operation)
                .ok_or(HubError::Closed)?
                .created_resource = Some(token);
            OperationHub::terminal_locked(
                &mut state,
                operation,
                TerminalResult {
                    terminal: Terminal::Completed,
                    resource: Some(token),
                },
            )?
        } else {
            None
        };
        self.blob = None;
        drop(state);
        if let Some(notify) = notify {
            notify.notify_one();
        }
        Ok(token)
    }
}

impl std::io::Write for NativeBlobEncoder {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.write_at_position(bytes)
            .map_err(|error| std::io::Error::other(format!("native blob encoding: {error:?}")))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Seek for NativeBlobEncoder {
    fn seek(&mut self, from: std::io::SeekFrom) -> std::io::Result<u64> {
        let position = match from {
            std::io::SeekFrom::Start(position) => i128::from(position),
            std::io::SeekFrom::Current(offset) => self.position as i128 + i128::from(offset),
            std::io::SeekFrom::End(offset) => self.written as i128 + i128::from(offset),
        };
        if position < 0 || position > self.maximum_bytes as i128 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "native blob seek exceeds encoding bounds",
            ));
        }
        self.position = position as usize;
        Ok(self.position as u64)
    }
}

impl Drop for NativeBlobEncoder {
    fn drop(&mut self) {
        if let Some(blob) = self.blob.take() {
            let _ = self.hub.close_resource(blob);
        }
    }
}

#[cfg(all(test, feature = "wasm-sketch-host"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputFault {
    AfterWrite,
    Sync,
    Replace,
}

// Deliberately absent from ordinary worker builds. The native proof arms this
// inside its own parent-owned staging directory before guest output starts.
// Holding a real open, partially written file here forces the supervisor to
// kill the worker: cooperative guest cancellation cannot finish this I/O job.
#[cfg(feature = "wasm-sketch-worker-test-support")]
fn pause_worker_output_for_containment_proof(destination: &Path) -> std::io::Result<()> {
    let Some(parent) = destination.parent() else {
        return Ok(());
    };
    let armed = parent.join(".proof-pause-output");
    if armed.is_file() {
        fs::write(parent.join(".proof-output-paused"), b"partial-write")?;
        while armed.exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    Ok(())
}

#[cfg(feature = "wasm-sketch-host")]
struct TemporaryOutput {
    file: Option<File>,
    path: PathBuf,
    committed: bool,
}

#[cfg(feature = "wasm-sketch-host")]
impl TemporaryOutput {
    fn create(path: PathBuf) -> std::io::Result<Self> {
        let file = File::options().write(true).create_new(true).open(&path)?;
        Ok(Self {
            file: Some(file),
            path,
            committed: false,
        })
    }
}

#[cfg(feature = "wasm-sketch-host")]
impl Drop for TemporaryOutput {
    fn drop(&mut self) {
        // Windows also requires the handle to be closed before removal.
        drop(self.file.take());
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(feature = "wasm-sketch-host")]
struct BlobCommitLease<'a> {
    hub: &'a OperationHub,
    blob: OpaqueToken,
}

#[cfg(feature = "wasm-sketch-host")]
impl Drop for BlobCommitLease<'_> {
    fn drop(&mut self) {
        let _ = self.hub.close_resource(self.blob);
    }
}

fn next(counter: &AtomicU64) -> Result<u64, HubError> {
    counter
        .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        })
        .map(|value| value + 1)
        .map_err(|_| HubError::Exhausted)
}

fn next_token() -> Result<u64, HubError> {
    let value = next(&NEXT_OPAQUE_TOKEN)?;
    if value > MAX_WIRE_TOKEN {
        return Err(HubError::Exhausted);
    }
    Ok(value)
}

fn status(terminal: Terminal) -> u8 {
    match terminal {
        Terminal::Completed => STATUS_COMPLETED,
        Terminal::Cancelled => STATUS_CANCELLED,
        Terminal::TimedOut => STATUS_TIMED_OUT,
        Terminal::Trapped => STATUS_TRAPPED,
        Terminal::OwnerExited => STATUS_OWNER_EXITED,
        Terminal::Closed => STATUS_CLOSED,
        Terminal::Rejected => STATUS_REJECTED,
    }
}

fn pack(status: u8, payload: Option<OpaqueToken>) -> u64 {
    let payload = payload.map_or(0, |token| token.0);
    debug_assert!(payload <= MAX_WIRE_TOKEN);
    (payload << 8) | u64::from(status)
}

#[cfg(feature = "wasm-sketch-host")]
fn hub_io_error(error: HubError) -> std::io::Error {
    let kind = match error {
        HubError::Quota => std::io::ErrorKind::WouldBlock,
        HubError::Invalid | HubError::Stale | HubError::WrongKind | HubError::WrongRights => {
            std::io::ErrorKind::PermissionDenied
        }
        HubError::Closed => std::io::ErrorKind::BrokenPipe,
        HubError::Exhausted => std::io::ErrorKind::Other,
    };
    std::io::Error::new(
        kind,
        format!("opaque resource operation rejected: {error:?}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_open_admission_outlives_semantic_cancellation() {
        let hub = OperationHub::new(8, 8).unwrap();
        let mut leases = Vec::new();
        for _ in 0..4 {
            leases.push(hub.acquire_native_open().unwrap());
            let (_, op) = hub.begin_external_webview_open(7).unwrap();
            hub.cancel_wire(7, op.0).unwrap();
            hub.observe_terminal(7, op).unwrap().unwrap();
        }
        assert_eq!(hub.snapshot().pending_operations, 0);
        assert_eq!(hub.snapshot().live_resources, 0);
        assert_eq!(hub.snapshot().active_native_opens, 4);
        assert!(matches!(hub.acquire_native_open(), Err(HubError::Quota)));
        hub.close_all(Terminal::Closed);
        assert_eq!(hub.snapshot().active_native_opens, 4);
        drop(leases);
        assert_eq!(hub.snapshot().active_native_opens, 0);
        assert!(matches!(hub.acquire_native_open(), Err(HubError::Closed)));
    }

    #[test]
    fn producer_terminal_signal_survives_guest_consumption() {
        let hub = OperationHub::new(4, 4).unwrap();
        for terminal in [
            Terminal::Completed,
            Terminal::Cancelled,
            Terminal::Closed,
            Terminal::TimedOut,
            Terminal::Trapped,
            Terminal::OwnerExited,
            Terminal::Rejected,
        ] {
            let (operation, _) = hub.submit(7, None, 0, 0).unwrap();
            assert!(matches!(
                hub.bind_producer_cancellation(8, operation),
                Err(HubError::WrongRights)
            ));
            let token = hub.bind_producer_cancellation(7, operation).unwrap();
            assert!(!token.is_cancelled());
            assert!(matches!(
                hub.bind_producer_cancellation(7, operation),
                Err(HubError::Invalid)
            ));
            hub.finish_external_operation(operation, terminal);
            assert!(token.is_cancelled());
            // A producer binding after completion also observes the latch.
            let late = hub.bind_producer_cancellation(7, operation).unwrap();
            assert!(late.is_cancelled());
            assert_eq!(
                hub.observe_terminal(7, operation)
                    .unwrap()
                    .unwrap()
                    .terminal,
                terminal
            );
            assert!(token.is_cancelled());
            assert!(late.is_cancelled());
        }
        let (operation, _) = hub.submit(7, None, 0, 0).unwrap();
        let token = hub.bind_producer_cancellation(7, operation).unwrap();
        hub.close_all(Terminal::Closed);
        assert!(token.is_cancelled());
        hub.observe_terminal(7, operation).unwrap().unwrap();
        assert_eq!(hub.snapshot().pending_operations, 0);
    }

    #[test]
    fn native_open_completion_reports_only_the_authorized_terminal_winner() {
        let hub = OperationHub::new(4, 4).unwrap();
        let (view, open) = hub.begin_external_webview_open(7).unwrap();
        let (other, other_open) = hub.begin_external_webview_open(7).unwrap();
        assert!(
            !hub.finish_external_open(open, other),
            "cannot publish a different reservation"
        );
        assert!(hub.observe_terminal(7, open).unwrap().is_none());
        let winners = std::thread::scope(|scope| {
            let barrier = Arc::new(std::sync::Barrier::new(8));
            let attempts: Vec<_> = (0..8)
                .map(|_| {
                    let hub = Arc::clone(&hub);
                    let barrier = Arc::clone(&barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        hub.finish_external_open(open, view)
                    })
                })
                .collect();
            attempts
                .into_iter()
                .map(|attempt| usize::from(attempt.join().unwrap()))
                .sum::<usize>()
        });
        assert_eq!(winners, 1);
        assert!(
            !hub.finish_external_open(open, view),
            "completion wins exactly once"
        );
        assert_eq!(
            hub.observe_terminal(7, open).unwrap().unwrap().resource,
            Some(view)
        );
        hub.cancel_wire(7, other_open.0).unwrap();
        assert!(!hub.finish_external_open(other_open, other));
        assert_eq!(
            hub.observe_terminal(7, other_open)
                .unwrap()
                .unwrap()
                .terminal,
            Terminal::Cancelled
        );
        hub.close_resource(view).unwrap();
        assert_eq!(hub.snapshot().live_resources, 0);
        assert_eq!(hub.snapshot().pending_operations, 0);
    }

    #[test]
    fn grant_revocation_before_resource_attachment_reclaims_both_reservations() {
        let hub = OperationHub::new(4, 4).unwrap();
        let grant = hub
            .grant_webview_url(7, Arc::from("https://example.test/"))
            .unwrap();
        let view = hub
            .create_resource_value(
                7,
                EXTERNAL_WEBVIEW_RESOURCE_KIND,
                EXTERNAL_WEBVIEW_RIGHT_LOAD,
                false,
                ResourceValue::ExternalWebview,
            )
            .unwrap();
        let (operation, _) = hub
            .submit(
                7,
                Some(grant),
                WEBVIEW_URL_RESOURCE_KIND,
                WEBVIEW_URL_RIGHT_OPEN,
            )
            .unwrap();
        hub.close_resource(grant).unwrap();
        assert_eq!(
            hub.attach_created_resource(operation, view),
            Err(HubError::Closed)
        );
        assert_eq!(hub.snapshot().live_resources, 0);
        assert_eq!(hub.snapshot().pending_operations, 0);
    }

    #[test]
    fn url_grant_open_is_scoped_and_revocation_reclaims_the_reserved_view() {
        let hub = OperationHub::new(4, 4).unwrap();
        let other = OperationHub::new(4, 4).unwrap();
        let grant = hub
            .grant_webview_url(7, Arc::from("https://example.test/exact?q=1"))
            .unwrap();
        assert!(matches!(
            hub.begin_granted_webview_open(8, grant),
            Err(HubError::WrongRights)
        ));
        assert!(matches!(
            other.begin_granted_webview_open(7, grant),
            Err(HubError::Invalid)
        ));
        assert_eq!(hub.snapshot().live_resources, 1);
        let (view, operation, url) = hub.begin_granted_webview_open(7, grant).unwrap();
        assert_eq!(&*url, "https://example.test/exact?q=1");
        assert_eq!(hub.snapshot().live_resources, 2);
        hub.close_resource(grant).unwrap();
        assert_eq!(hub.snapshot().live_resources, 0);
        hub.finish_external_open(operation, view);
        let terminal = hub.observe_terminal(7, operation).unwrap().unwrap();
        assert_eq!(terminal.terminal, Terminal::Closed);
        assert_eq!(terminal.resource, None);
        assert!(hub.begin_external_webview_wait(7, view).is_err());
        assert!(hub.begin_granted_webview_open(7, grant).is_err());
        assert_eq!(hub.snapshot().pending_operations, 0);
    }

    #[test]
    fn url_grant_has_fixed_storage_and_operation_quota_rollback() {
        let hub = OperationHub::new(0, 2).unwrap();
        assert!(matches!(
            hub.grant_webview_url(7, Arc::from("")),
            Err(HubError::Quota)
        ));
        assert!(matches!(
            hub.grant_webview_url(7, Arc::from("x".repeat(MAX_WEBVIEW_URL_BYTES + 1))),
            Err(HubError::Quota)
        ));
        let grant = hub
            .grant_webview_url(7, Arc::from("https://example.test/"))
            .unwrap();
        assert!(matches!(
            hub.begin_granted_webview_open(7, grant),
            Err(HubError::Quota)
        ));
        assert_eq!(
            hub.snapshot().live_resources,
            1,
            "failed open must roll back its view reservation"
        );
        hub.close_all(Terminal::Cancelled);
        assert_eq!(hub.snapshot().live_resources, 0);
    }

    #[test]
    fn cancelling_url_open_does_not_revoke_the_authorized_url() {
        let hub = OperationHub::new(4, 4).unwrap();
        let grant = hub
            .grant_webview_url(7, Arc::from("https://example.test/"))
            .unwrap();
        let (view, operation, _) = hub.begin_granted_webview_open(7, grant).unwrap();
        hub.cancel_wire(7, operation.0).unwrap();
        hub.finish_external_open(operation, view);
        assert_eq!(
            hub.observe_terminal(7, operation)
                .unwrap()
                .unwrap()
                .terminal,
            Terminal::Cancelled
        );
        assert_eq!(hub.snapshot().live_resources, 1);
        let (view, operation, _) = hub.begin_granted_webview_open(7, grant).unwrap();
        hub.finish_external_open(operation, view);
        assert_eq!(
            hub.observe_terminal(7, operation)
                .unwrap()
                .unwrap()
                .resource,
            Some(view)
        );
        hub.close_resource(grant).unwrap();
        assert_eq!(
            hub.snapshot().live_resources,
            1,
            "completed open owns independent view authority"
        );
        hub.close_resource(view).unwrap();
        assert_eq!(hub.snapshot().live_resources, 0);
    }

    #[test]
    fn generated_clock_waits_on_kernel_time_and_resumes_its_owner() {
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hub = OperationHub::new(2, 2).unwrap();
        runtime
            .run_within(
                async {
                    let started = std::time::Instant::now();
                    let operation = hub
                        .submit_wire(runtime.handle(), 7, OP_CLOCK_SLEEP, 25, 0)
                        .unwrap();
                    assert_eq!(hub.poll_wire(7, operation), pack(STATUS_PENDING, None));
                    assert!(hub.suspend_wire(8, operation).is_err());
                    let wake = hub.suspend_wire(7, operation).unwrap();
                    crate::async_engine::timeout(
                        std::time::Duration::from_secs(2),
                        wake.notified(),
                    )
                    .await
                    .unwrap();
                    assert!(started.elapsed() >= std::time::Duration::from_millis(25));
                    assert_eq!(hub.poll_wire(7, operation), pack(STATUS_COMPLETED, None));
                    assert_eq!(hub.snapshot().active_clocks, 0);
                    assert_eq!(hub.snapshot().pending_operations, 0);
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    #[test]
    fn generated_clock_cancel_keeps_admission_until_task_drain() {
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hub = OperationHub::new(1, 1).unwrap();
        // Do not drive the runtime until after cancellation and result
        // consumption: this deterministically retains the queued task.
        let operation = hub
            .submit_wire(runtime.handle(), 7, OP_CLOCK_SLEEP, u64::from(u32::MAX), 0)
            .unwrap();
        assert!(hub.cancel_wire(8, operation).is_err());
        hub.cancel_wire(7, operation).unwrap();
        assert_eq!(hub.poll_wire(7, operation), pack(STATUS_CANCELLED, None));
        assert_eq!(hub.snapshot().pending_operations, 0);
        assert_eq!(hub.snapshot().active_clocks, 1);
        assert_eq!(
            hub.submit_wire(runtime.handle(), 7, OP_CLOCK_SLEEP, 0, 0),
            Err(HubError::Quota)
        );
        runtime
            .run_within(
                async {
                    crate::async_engine::timeout(std::time::Duration::from_secs(2), async {
                        while hub.snapshot().active_clocks != 0 {
                            crate::async_engine::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        let operation = hub
            .submit_wire(runtime.handle(), 7, OP_CLOCK_SLEEP, u64::from(u32::MAX), 0)
            .unwrap();
        hub.close_all(Terminal::Closed);
        assert_eq!(hub.poll_wire(7, operation), pack(STATUS_CLOSED, None));
        runtime
            .run_within(
                async {
                    crate::async_engine::timeout(
                        std::time::Duration::from_secs(2),
                        hub.join_clock_jobs(),
                    )
                    .await
                    .unwrap();
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(
            hub.submit_wire(runtime.handle(), 7, OP_CLOCK_SLEEP, 0, 0),
            Err(HubError::Closed)
        );
        assert_eq!(hub.snapshot().active_clocks, 0);
    }

    #[test]
    fn generated_clock_rejects_noncanonical_arguments_without_reservation() {
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hub = OperationHub::new(1, 1).unwrap();
        for (milliseconds, reserved) in [(u64::MAX, 0), (0, 1)] {
            assert_eq!(
                hub.submit_wire(runtime.handle(), 7, OP_CLOCK_SLEEP, milliseconds, reserved),
                Err(HubError::Invalid)
            );
        }
        assert_eq!(hub.snapshot().pending_operations, 0);
        assert_eq!(hub.snapshot().active_clocks, 0);
    }

    #[test]
    fn native_capture_admission_survives_operation_consumption_and_teardown() {
        let hub = OperationHub::new(8, 8).unwrap();
        let mut leases = Vec::new();
        for _ in 0..4 {
            leases.push(hub.acquire_native_capture().unwrap());
            let (operation, _) = hub.submit(7, None, 0, 0).unwrap();
            hub.finish_external_operation(operation, Terminal::Cancelled);
            hub.observe_terminal(7, operation).unwrap().unwrap();
        }
        assert_eq!(hub.snapshot().pending_operations, 0);
        assert_eq!(hub.snapshot().active_native_captures, 4);
        assert!(matches!(hub.acquire_native_capture(), Err(HubError::Quota)));
        drop(leases.pop());
        let last = hub.acquire_native_capture().unwrap();
        hub.close_all(Terminal::Closed);
        assert!(matches!(
            hub.acquire_native_capture(),
            Err(HubError::Closed)
        ));
        assert_eq!(
            hub.snapshot().active_native_captures,
            4,
            "teardown must not hide live callbacks"
        );
        drop(last);
        drop(leases);
        assert_eq!(hub.snapshot().active_native_captures, 0);
    }

    #[test]
    fn native_encoder_seek_overwrites_and_fills_holes_with_bounded_storage() {
        use std::io::{Seek as _, SeekFrom, Write as _};
        let hub =
            OperationHub::with_blob_limits(4, 3, BlobLimits::new(4, 16, 16).unwrap()).unwrap();
        let mut encoder = NativeBlobEncoder::new(Arc::clone(&hub), 7, 16).unwrap();
        encoder.write_all(b"header").unwrap();
        encoder.seek(SeekFrom::Start(0)).unwrap();
        encoder.write_all(b"HEADER").unwrap();
        assert_eq!(hub.snapshot().buffered_blob_bytes, 6);
        encoder.seek(SeekFrom::End(2)).unwrap();
        assert_eq!(
            hub.snapshot().buffered_blob_bytes,
            6,
            "seek must not allocate a hole"
        );
        encoder.write_all(b"tail").unwrap();
        assert_eq!(encoder.stream_position().unwrap(), 12);
        assert!(encoder.seek(SeekFrom::Start(17)).is_err());
        assert!(encoder.seek(SeekFrom::Current(-13)).is_err());
        assert_eq!(encoder.stream_position().unwrap(), 12);
        let blob = encoder.finish().unwrap();
        let mut bytes = Vec::new();
        loop {
            let chunk = hub.blob_read(7, blob, 4).unwrap();
            if chunk.is_empty() {
                break;
            }
            bytes.extend(chunk);
        }
        assert_eq!(bytes, b"HEADER\0\0tail");
        hub.close_resource(blob).unwrap();
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
    }

    #[test]
    fn capture_revocation_reclaims_partial_encoding_before_callback_drop() {
        use std::io::Write as _;
        for close_view in [false, true] {
            let hub =
                OperationHub::with_blob_limits(4, 3, BlobLimits::new(4, 8, 8).unwrap()).unwrap();
            let (view, open) = hub.begin_external_webview_open(7).unwrap();
            hub.finish_external_open(open, view);
            hub.observe_terminal(7, open).unwrap().unwrap();
            let operation = hub.begin_external_webview_capture(7, view).unwrap();
            let mut encoder =
                NativeBlobEncoder::for_operation(Arc::clone(&hub), 7, operation, 8).unwrap();
            encoder.write_all(b"partial").unwrap();
            if close_view {
                hub.close_resource(view).unwrap();
            } else {
                hub.finish_external_operation(operation, Terminal::Cancelled);
            }
            // The native callback still owns its encoder, but the revoked
            // reservation must no longer retain any hub-owned storage.
            assert_eq!(hub.snapshot().live_blobs, 0);
            assert_eq!(hub.snapshot().buffered_blob_bytes, 0);
            assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
            assert!(encoder.write_all(b"x").is_err());
            drop(encoder);
            hub.observe_terminal(7, operation).unwrap().unwrap();
            hub.close_all(Terminal::Closed);
        }
    }

    #[test]
    fn native_encoder_rejects_load_operation_as_capture_authority() {
        use std::io::Write as _;
        let hub = OperationHub::with_blob_limits(4, 3, BlobLimits::new(4, 8, 8).unwrap()).unwrap();
        let (view, open) = hub.begin_external_webview_open(7).unwrap();
        hub.finish_external_open(open, view);
        hub.observe_terminal(7, open).unwrap().unwrap();
        let load = hub.begin_external_webview_wait(7, view).unwrap();
        let mut encoder = NativeBlobEncoder::new(Arc::clone(&hub), 7, 8).unwrap();
        encoder.write_all(b"encoded").unwrap();
        assert_eq!(
            encoder.finish_for_operation(load),
            Err(HubError::WrongRights)
        );
        assert!(hub.observe_terminal(7, load).unwrap().is_none());
        assert_eq!(hub.snapshot().live_blobs, 0);
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
        hub.close_all(Terminal::Closed);
    }

    #[test]
    fn native_capture_rejects_foreign_and_revoked_view_authority() {
        let hub = OperationHub::with_blob_limits(4, 3, BlobLimits::new(4, 8, 8).unwrap()).unwrap();
        let (view, open) = hub.begin_external_webview_open(7).unwrap();
        assert_eq!(
            hub.begin_external_webview_capture(7, view),
            Err(HubError::Closed)
        );
        hub.finish_external_open(open, view);
        hub.observe_terminal(7, open).unwrap().unwrap();
        assert_eq!(
            hub.begin_external_webview_capture(8, view),
            Err(HubError::WrongRights)
        );
        let capture = hub.begin_external_webview_capture(7, view).unwrap();
        let encoder = NativeBlobEncoder::new(Arc::clone(&hub), 8, 8).unwrap();
        assert_eq!(
            encoder.finish_for_operation(capture),
            Err(HubError::WrongRights)
        );
        assert!(hub.observe_terminal(7, capture).unwrap().is_none());
        assert_eq!(hub.snapshot().live_blobs, 0);
        hub.close_resource(view).unwrap();
        assert_eq!(
            hub.begin_external_webview_capture(7, view),
            Err(HubError::Closed)
        );
        assert_eq!(
            hub.observe_terminal(7, capture).unwrap().unwrap().terminal,
            Terminal::Closed
        );
        assert_eq!(hub.snapshot().pending_operations, 0);
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
    }

    #[test]
    fn native_encoder_operation_publication_has_one_terminal_winner() {
        use std::io::Write as _;
        for revoke in [0, 1, 2] {
            let hub =
                OperationHub::with_blob_limits(4, 3, BlobLimits::new(4, 8, 8).unwrap()).unwrap();
            let (view, open) = hub.begin_external_webview_open(7).unwrap();
            hub.finish_external_open(open, view);
            hub.observe_terminal(7, open).unwrap().unwrap();
            let operation = hub.begin_external_webview_capture(7, view).unwrap();
            let mut encoder = NativeBlobEncoder::new(Arc::clone(&hub), 7, 8).unwrap();
            encoder.write_all(b"encoded").unwrap();
            match revoke {
                1 => hub.finish_external_operation(operation, Terminal::Cancelled),
                2 => {
                    hub.close_resource(view).unwrap();
                }
                _ => {}
            }
            let published = encoder.finish_for_operation(operation);
            let terminal = hub.observe_terminal(7, operation).unwrap().unwrap();
            if revoke == 0 {
                let blob = published.unwrap();
                assert_eq!(terminal.terminal, Terminal::Completed);
                assert_eq!(terminal.resource, Some(blob));
                assert_eq!(hub.blob_read(7, blob, 4).unwrap(), b"enco");
                hub.close_resource(blob).unwrap();
            } else {
                assert_eq!(published, Err(HubError::Closed));
                assert_eq!(
                    terminal.terminal,
                    if revoke == 1 {
                        Terminal::Cancelled
                    } else {
                        Terminal::Closed
                    }
                );
                assert_eq!(terminal.resource, None);
            }
            hub.close_all(Terminal::Closed);
            assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
            assert_eq!(hub.snapshot().live_blobs, 0);
            assert_eq!(hub.snapshot().pending_operations, 0);
        }
    }

    #[test]
    fn native_encoder_publishes_only_sealed_read_only_quota_accounted_blob() {
        use std::io::Write as _;
        let hub = OperationHub::with_blob_limits(4, 2, BlobLimits::new(4, 8, 8).unwrap()).unwrap();
        let mut encoder = NativeBlobEncoder::new(Arc::clone(&hub), 7, 8).unwrap();
        let unpublished = encoder.blob.unwrap();
        encoder.write_all(b"12345678").unwrap();
        assert_eq!(hub.blob_read(7, unpublished, 4), Err(HubError::Closed));
        assert_eq!(hub.snapshot().buffered_blob_bytes, 8);
        let blob = encoder.finish().unwrap();
        assert_eq!(hub.blob_read(8, blob, 4), Err(HubError::WrongRights));
        assert_eq!(hub.blob_read(7, blob, 4).unwrap(), b"1234");
        assert_eq!(hub.blob_write(7, blob, b"x"), Err(HubError::WrongRights));
        assert_eq!(hub.blob_read(7, blob, 4).unwrap(), b"5678");
        hub.close_resource(blob).unwrap();
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
        assert_eq!(hub.snapshot().live_blobs, 0);
    }

    #[test]
    fn native_encoder_failure_or_teardown_cannot_publish_partial_bytes() {
        use std::io::Write as _;
        for teardown in [false, true] {
            let hub =
                OperationHub::with_blob_limits(4, 2, BlobLimits::new(4, 8, 8).unwrap()).unwrap();
            let mut encoder = NativeBlobEncoder::new(Arc::clone(&hub), 7, 6).unwrap();
            encoder.write_all(b"1234").unwrap();
            if teardown {
                hub.close_all(Terminal::Cancelled);
            } else {
                assert!(encoder.write_all(b"567").is_err());
                assert_eq!(hub.snapshot().buffered_blob_bytes, 4);
                assert!(encoder.write_all(b"5").is_err(), "failure must be sticky");
            }
            assert!(encoder.finish().is_err());
            assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
            assert_eq!(hub.snapshot().live_blobs, 0);
        }
    }

    #[cfg(feature = "wasm-sketch-host")]
    #[test]
    fn wire_output_commit_uses_only_the_scoped_host_grant() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("authorized");
        std::fs::write(&path, b"previous").unwrap();
        let hub = OperationHub::new(8, 4).unwrap();
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let blob = hub.create_blob(1).unwrap();
        hub.blob_write(1, blob, b"replacement").unwrap();
        hub.seal_blob(1, blob).unwrap();
        let output = hub.grant_exact_output(1, &path).unwrap();
        assert!(hub
            .submit_wire(runtime.handle(), 2, OP_OUTPUT_COMMIT, blob.0, output.0)
            .is_err());
        assert!(hub
            .submit_wire(runtime.handle(), 1, OP_OUTPUT_COMMIT, blob.0, u64::MAX)
            .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"previous");
        let operation = hub
            .submit_wire(runtime.handle(), 1, OP_OUTPUT_COMMIT, blob.0, output.0)
            .unwrap();
        runtime
            .run_within(
                async {
                    crate::async_engine::timeout(std::time::Duration::from_secs(2), async {
                        loop {
                            let status = hub.poll_wire(1, operation) as u8;
                            if status != STATUS_PENDING {
                                assert_eq!(status, STATUS_COMPLETED);
                                break;
                            }
                            hub.suspend_wire(1, operation).unwrap().notified().await;
                        }
                    })
                    .await
                    .unwrap();
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        assert_eq!(hub.snapshot().live_resources, 0);
    }

    #[cfg(feature = "wasm-sketch-host")]
    #[test]
    fn injected_output_io_failures_preserve_final_and_reclaim_temp_and_buffers() {
        for stage in [
            OutputFault::AfterWrite,
            OutputFault::Sync,
            OutputFault::Replace,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("final");
            fs::write(&path, b"original").unwrap();
            let hub =
                OperationHub::with_blob_limits(4, 2, BlobLimits::new(4, 8, 8).unwrap()).unwrap();
            let runtime = crate::async_engine::RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .unwrap();
            let blob = hub.create_blob(1).unwrap();
            hub.blob_write(1, blob, b"part").unwrap();
            hub.blob_write(1, blob, b"rest").unwrap();
            hub.seal_blob(1, blob).unwrap();
            let output = hub.grant_exact_output(1, &path).unwrap();
            *hub.output_fault.lock().unwrap() = Some(stage);
            let operation = hub
                .submit_output_commit(runtime.handle(), 1, blob, output)
                .unwrap();
            runtime
                .run_within(
                    async {
                        crate::async_engine::timeout(std::time::Duration::from_secs(2), async {
                            loop {
                                if let Some(result) = hub.take_terminal(operation, 1).unwrap() {
                                    assert_eq!(result.terminal, Terminal::Rejected, "{stage:?}");
                                    break;
                                }
                                crate::async_engine::yield_now().await;
                            }
                            hub.close_all(Terminal::Closed);
                            hub.join_output_jobs().await.unwrap();
                        })
                        .await
                        .unwrap();
                    },
                    std::time::Duration::from_secs(60),
                )
                .expect("test runtime budget");
            assert_eq!(
                *hub.output_fault.lock().unwrap(),
                None,
                "fault must be reached"
            );
            assert_eq!(fs::read(&path).unwrap(), b"original", "{stage:?}");
            assert_eq!(
                fs::read_dir(directory.path()).unwrap().count(),
                1,
                "{stage:?}"
            );
            let snapshot = hub.snapshot();
            assert_eq!(snapshot.live_resources, 0);
            assert_eq!(snapshot.pending_operations, 0);
            assert_eq!(snapshot.active_output_jobs, 0);
            assert_eq!(snapshot.retained_transfer_capacity, 0);
        }
    }

    #[cfg(feature = "wasm-sketch-host")]
    #[test]
    fn temporary_output_unwind_closes_and_removes_only_its_owned_file() {
        for close_before_panic in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let final_path = directory.path().join("final");
            let temporary_path = directory.path().join("temporary");
            fs::write(&final_path, b"original").unwrap();
            let panic = std::panic::catch_unwind(|| {
                let mut temporary = TemporaryOutput::create(temporary_path.clone()).unwrap();
                temporary
                    .file
                    .as_mut()
                    .unwrap()
                    .write_all(b"partial")
                    .unwrap();
                if close_before_panic {
                    temporary.file.as_ref().unwrap().sync_all().unwrap();
                    drop(temporary.file.take());
                }
                panic!("injected before atomic replacement");
            });
            assert!(panic.is_err());
            assert!(!temporary_path.exists());
            assert_eq!(fs::read(&final_path).unwrap(), b"original");
            fs::write(&temporary_path, b"another creator").unwrap();
            assert!(TemporaryOutput::create(temporary_path.clone()).is_err());
            assert_eq!(fs::read(&temporary_path).unwrap(), b"another creator");
        }
    }

    #[cfg(feature = "wasm-sketch-host")]
    #[test]
    fn output_job_panic_survives_completed_handle_pruning() {
        let hub = OperationHub::new(1, 1).unwrap();
        hub.output_job_count.fetch_add(1, Ordering::AcqRel);
        let lease = OutputJobLease(Arc::clone(&hub));
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _lease = lease;
                panic!("injected output job panic");
            }))
            .is_err()
        );
        assert_eq!(hub.snapshot().active_output_jobs, 0);
        hub.close_all(Terminal::Closed);
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(
            runtime
                .run_within(hub.join_output_jobs(), std::time::Duration::from_secs(60))
                .expect("test runtime budget"),
            Err(HubError::Closed)
        );
    }

    #[cfg(feature = "wasm-sketch-host")]
    #[test]
    fn output_job_quota_and_teardown_track_work_until_native_buffer_release() {
        let directory = tempfile::tempdir().unwrap();
        let hub = OperationHub::new(1, 2).unwrap();
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let blob = hub.create_blob(1).unwrap();
        hub.blob_write(1, blob, b"data").unwrap();
        let output = hub
            .grant_exact_output(1, &directory.path().join("output"))
            .unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        hub.output_job_count.fetch_add(1, Ordering::AcqRel);
        let lease = OutputJobLease(Arc::clone(&hub));
        let worker_hub = Arc::clone(&hub);
        let job = runtime.handle().launch_blocking(move || {
            let _lease = lease;
            let _chunk = worker_hub.read_blob_chunk(1, blob, 4, false).unwrap();
            started_tx.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
        });
        hub.state.lock().unwrap().output_jobs.push(job);
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert_eq!(hub.snapshot().active_output_jobs, 1);
        assert_eq!(
            hub.submit_output_commit(runtime.handle(), 1, blob, output),
            Err(HubError::Quota)
        );
        assert_eq!(hub.snapshot().pending_operations, 0);
        hub.close_all(Terminal::Cancelled);
        runtime
            .run_within(
                async {
                    let joining_hub = Arc::clone(&hub);
                    let joining = runtime
                        .handle()
                        .launch(async move { joining_hub.join_output_jobs().await });
                    crate::async_engine::yield_now().await;
                    assert!(!joining.is_finished());
                    assert_eq!(hub.snapshot().active_output_jobs, 1);
                    assert_eq!(hub.snapshot().native_transfer_capacity, 4);
                    release_tx.send(()).unwrap();
                    assert_eq!(joining.await.unwrap(), Ok(()));
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(hub.snapshot().active_output_jobs, 0);
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
    }

    #[cfg(feature = "wasm-sketch-host")]
    #[test]
    fn output_commit_runs_on_supplied_runtime_and_cancellation_wins_before_rename() {
        let directory = tempfile::tempdir().unwrap();
        let final_path = directory.path().join("final");
        let hub = OperationHub::new(8, 8).unwrap();
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let blob = hub.create_blob(1).unwrap();
        hub.blob_write(1, blob, b"complete").unwrap();
        hub.seal_blob(1, blob).unwrap();
        let output = hub.grant_exact_output(1, &final_path).unwrap();
        let commit = hub
            .submit_output_commit(runtime.handle(), 1, blob, output)
            .unwrap();
        runtime
            .run_within(
                async {
                    crate::async_engine::timeout(std::time::Duration::from_secs(2), async {
                        loop {
                            if let Some(result) = hub.take_terminal(commit, 1).unwrap() {
                                assert_eq!(result.terminal, Terminal::Completed);
                                break;
                            }
                            crate::async_engine::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(std::fs::read(&final_path).unwrap(), b"complete");
        let blob = hub.create_blob(1).unwrap();
        hub.blob_write(1, blob, b"cancelled").unwrap();
        hub.seal_blob(1, blob).unwrap();
        let output = hub.grant_exact_output(1, &final_path).unwrap();
        let (commit, _) = hub
            .submit(1, Some(output), OUTPUT_RESOURCE_KIND, OUTPUT_RIGHT_COMMIT)
            .unwrap();
        hub.cancel_wire(1, commit.0).unwrap();
        assert!(hub
            .commit_blob_operation(1, blob, output, Some(commit))
            .is_err());
        assert_eq!(
            hub.take_terminal(commit, 1).unwrap().unwrap().terminal,
            Terminal::Cancelled
        );
        assert_eq!(std::fs::read(&final_path).unwrap(), b"complete");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        assert_eq!(
            hub.blob_read(1, blob, 64).unwrap(),
            b"cancelled",
            "cancellation before job start must not consume the source blob"
        );
    }

    #[cfg(feature = "wasm-sketch-host")]
    #[test]
    fn output_commit_refuses_a_blob_with_a_pending_reader() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("final");
        let hub = OperationHub::new(4, 4).unwrap();
        let blob = hub.create_blob(1).unwrap();
        let output = hub.grant_exact_output(1, &destination).unwrap();
        let read = hub.submit_blob_read(1, blob, 4).unwrap();
        assert!(hub.commit_blob_to_output(1, blob, output).is_err());
        assert!(!destination.exists());
        assert_eq!(hub.take_blob_read(1, read), Ok(None));
    }

    #[test]
    fn commit_consumption_excludes_other_readers_and_generated_submissions() {
        let hub = OperationHub::new(4, 4).unwrap();
        let blob = hub.create_blob(1).unwrap();
        hub.blob_write(1, blob, b"data").unwrap();
        hub.seal_blob(1, blob).unwrap();
        hub.state
            .lock()
            .unwrap()
            .resources
            .get_mut(&blob)
            .unwrap()
            .committing = true;
        assert_eq!(hub.blob_read(1, blob, 4), Err(HubError::WrongRights));
        assert_eq!(hub.submit_blob_read(1, blob, 4), Err(HubError::WrongRights));
        assert_eq!(&*hub.read_blob_chunk(1, blob, 4, true).unwrap(), b"data");
    }

    #[cfg(feature = "wasm-sketch-host")]
    #[test]
    fn revoked_output_cannot_replace_final_after_the_temporary_is_flushed() {
        for teardown in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let final_path = directory.path().join("final");
            let temporary = directory.path().join("staged");
            std::fs::write(&final_path, b"original").unwrap();
            std::fs::write(&temporary, b"replacement").unwrap();
            let hub = OperationHub::new(4, 4).unwrap();
            let blob = hub.create_blob(1).unwrap();
            let output = hub.grant_exact_output(1, &final_path).unwrap();
            hub.seal_blob(1, blob).unwrap();
            if teardown {
                hub.close_all(Terminal::Cancelled);
            } else {
                hub.close_resource(output).unwrap();
            }
            assert!(hub
                .replace_authorized_output(1, blob, output, &temporary)
                .is_err());
            assert_eq!(std::fs::read(&final_path).unwrap(), b"original");
            assert_eq!(std::fs::read(&temporary).unwrap(), b"replacement");
        }
    }

    #[test]
    fn read_submission_releases_capacity_before_its_result_is_collected() {
        let hub = OperationHub::with_blob_limits(4, 2, BlobLimits::new(4, 4, 4).unwrap()).unwrap();
        let blob = hub.create_blob(1).unwrap();
        hub.blob_write(1, blob, b"full").unwrap();
        let write = hub.submit_blob_write(1, blob, b"next").unwrap();
        assert_eq!(hub.take_terminal(write, 1), Ok(None));
        let read = hub.submit_blob_read(1, blob, 4).unwrap();
        assert_eq!(
            hub.take_terminal(write, 1).unwrap().unwrap().terminal,
            Terminal::Completed
        );
        assert_eq!(
            hub.take_blob_read(1, read),
            Ok(Some((Terminal::Completed, b"full".to_vec())))
        );
        assert_eq!(hub.blob_read(1, blob, 4).unwrap(), b"next");
    }

    #[test]
    fn generated_seal_completes_pending_eof_and_rejects_later_writes() {
        let hub = OperationHub::new(4, 1).unwrap();
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let blob = hub.create_blob(1).unwrap();
        let read = hub.submit_blob_read(1, blob, 1).unwrap();
        assert_eq!(
            hub.collect_blob_read_wire(1, read.0, 1, |_| panic!("not EOF yet")),
            Ok(0)
        );
        assert!(hub
            .submit_wire(runtime.handle(), 2, OP_BLOB_SEAL, blob.0, 0)
            .is_err());
        let seal = hub
            .submit_wire(runtime.handle(), 1, OP_BLOB_SEAL, blob.0, 0)
            .unwrap();
        assert_eq!(hub.poll_wire(1, seal) as u8, STATUS_COMPLETED);
        assert_eq!(
            hub.collect_blob_read_wire(1, read.0, 1, |bytes| assert!(bytes.is_empty())),
            Ok(u64::from(STATUS_COMPLETED))
        );
        let write = hub.submit_blob_write(1, blob, b"x").unwrap();
        assert_eq!(hub.poll_wire(1, write.0) as u8, STATUS_CLOSED);
    }

    #[test]
    #[cfg(feature = "wasm-sketch-host")]
    fn epoch_revocation_retries_a_busy_hub_without_blocking() {
        let hub = OperationHub::new(1, 1).unwrap();
        let (operation, _) = hub.submit(0, None, 0, 0).unwrap();
        let held = hub.state.lock().unwrap();
        assert!(!hub.try_close_all(Terminal::Cancelled));
        drop(held);
        assert_eq!(hub.poll_wire(0, operation.0), 0);
        assert!(hub.try_close_all(Terminal::Cancelled));
        assert_eq!(hub.poll_wire(0, operation.0), 2);
        assert!(hub.submit(0, None, 0, 0).is_err());
    }

    #[test]
    fn native_chunk_holds_budget_until_drop_and_then_wakes_writer() {
        let mut limits = BlobLimits::new(4, 8, 8).unwrap();
        limits.maximum_transfer_bytes = 16;
        let hub = OperationHub::with_blob_limits(8, 1, limits).unwrap();
        let blob = hub.create_blob(1).unwrap();
        hub.blob_write(1, blob, b"full").unwrap();
        hub.blob_write(1, blob, b"full").unwrap();
        let write = hub.submit_blob_write(1, blob, b"next").unwrap();
        let chunk = hub.read_blob_chunk(1, blob, 4, false).unwrap();
        assert_eq!(&*chunk, b"full");
        assert_eq!(hub.snapshot().native_transfer_capacity, 4);
        assert_eq!(hub.snapshot().retained_transfer_capacity, 16);
        assert_eq!(hub.take_terminal(write, 1).unwrap(), None);
        drop(chunk);
        assert_eq!(
            hub.take_terminal(write, 1).unwrap().unwrap().terminal,
            Terminal::Completed
        );
        assert_eq!(hub.snapshot().native_transfer_capacity, 0);

        let chunk = hub.read_blob_chunk(1, blob, 4, false).unwrap();
        hub.close_all(Terminal::Cancelled);
        assert_eq!(hub.snapshot().live_blobs, 0);
        assert_eq!(
            hub.snapshot().retained_transfer_capacity,
            4,
            "teardown must not release a still-live native allocation"
        );
        drop(chunk);
        assert_eq!(hub.snapshot().native_transfer_capacity, 0);
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
        assert_eq!(hub.snapshot().peak_retained_transfer_capacity, 16);
    }

    #[test]
    fn combined_transfer_quota_preserves_pull_headroom_and_resumes_after_collection() {
        let mut limits = BlobLimits::new(4, 8, 8).unwrap();
        limits.maximum_transfer_bytes = 16;
        let hub = OperationHub::with_blob_limits(8, 1, limits).unwrap();
        let blob = hub.create_blob(1).unwrap();
        hub.blob_write(1, blob, b"full").unwrap();
        hub.blob_write(1, blob, b"full").unwrap();
        let write = hub.submit_blob_write(1, blob, b"next").unwrap();
        assert_eq!(
            hub.submit_blob_write_from(1, blob, 4, || panic!(
                "read headroom must not be copied away"
            )),
            Err(HubError::Quota)
        );
        let read = hub.submit_blob_read(1, blob, 4).unwrap();
        assert_eq!(hub.snapshot().retained_transfer_capacity, 16);
        assert_eq!(hub.take_terminal(write, 1).unwrap(), None);
        assert_eq!(
            hub.collect_blob_read_wire(1, read.0, 4, |bytes| assert_eq!(bytes, b"full"))
                .unwrap(),
            4 << 8 | u64::from(STATUS_COMPLETED)
        );
        assert_eq!(
            hub.take_terminal(write, 1).unwrap().unwrap().terminal,
            Terminal::Completed
        );
        assert_eq!(hub.snapshot().peak_retained_transfer_capacity, 16);
        hub.close_all(Terminal::Closed);
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
    }

    #[test]
    fn live_blob_quota_is_independent_of_other_resources_and_released_on_close() {
        let mut limits = BlobLimits::new(4, 4, 8).unwrap();
        limits.maximum_live_blobs = 1;
        let hub = OperationHub::with_blob_limits(8, 8, limits).unwrap();
        let first = hub.create_blob(1).unwrap();
        assert_eq!(hub.create_blob(1), Err(HubError::Quota));
        assert!(hub
            .create_resource(1, SYNTHETIC_RESOURCE_KIND, 1, false)
            .is_ok());
        hub.close_resource(first).unwrap();
        assert!(hub.create_blob(1).is_ok());
    }

    #[test]
    fn pending_io_count_quotas_reject_before_copy_and_cancel_releases_them() {
        let mut limits = BlobLimits::new(4, 4, 16).unwrap();
        limits.maximum_pending_reads = 1;
        limits.maximum_pending_writes = 1;
        let hub = OperationHub::with_blob_limits(16, 4, limits).unwrap();
        let empty = hub.create_blob(1).unwrap();
        let full = hub.create_blob(1).unwrap();
        hub.blob_write(1, full, b"full").unwrap();
        let read = hub.submit_blob_read(1, empty, 4).unwrap();
        assert_eq!(hub.submit_blob_read(1, empty, 4), Err(HubError::Quota));
        let write = hub.submit_blob_write(1, full, b"next").unwrap();
        assert_eq!(
            hub.submit_blob_write_from(1, full, 1, || panic!("quota must reject before copying")),
            Err(HubError::Quota)
        );
        assert_eq!(hub.snapshot().pending_operations, 2);
        assert_eq!(hub.snapshot().pending_blob_reads, 1);
        assert_eq!(hub.snapshot().pending_blob_writes, 1);
        hub.cancel_wire(1, read.0).unwrap();
        hub.cancel_wire(1, write.0).unwrap();
        assert_eq!(hub.snapshot().pending_blob_reads, 0);
        assert_eq!(hub.snapshot().pending_blob_writes, 0);
        // Cancelled terminal results have not been collected. They no longer
        // hold pending-I/O permits, but still count against the operation table.
        assert!(hub.submit_blob_read(1, empty, 4).is_ok());
        assert!(hub.submit_blob_write(1, full, b"next").is_ok());
        hub.close_all(Terminal::Closed);
        assert_eq!(hub.snapshot().pending_operations, 0);
        assert_eq!(hub.snapshot().live_blobs, 0);
        assert_eq!(hub.snapshot().pending_blob_reads, 0);
        assert_eq!(hub.snapshot().pending_blob_writes, 0);
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
    }

    #[test]
    fn abandoned_blob_releases_capacity_without_an_operation_slot() {
        let hub = OperationHub::with_blob_limits(1, 2, BlobLimits::new(4, 4, 8).unwrap()).unwrap();
        let blob = hub.create_blob(1).unwrap();
        let other = hub
            .create_resource(1, SYNTHETIC_RESOURCE_KIND, 0, false)
            .unwrap();
        assert_eq!(hub.abandon_blob_wire(1, other.0), Err(HubError::WrongKind));
        hub.close_resource(other).unwrap();
        hub.blob_write(1, blob, b"full").unwrap();
        let pending = hub.submit_blob_write(1, blob, b"next").unwrap();
        assert_eq!(hub.abandon_blob_wire(2, blob.0), Err(HubError::WrongRights));
        assert_eq!(hub.snapshot().live_blobs, 1);
        hub.abandon_blob_wire(1, blob.0).unwrap();
        assert_eq!(hub.snapshot().live_blobs, 0);
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
        assert_eq!(hub.poll_wire(1, pending.0) as u8, STATUS_CLOSED);
        assert_eq!(hub.abandon_blob_wire(1, blob.0), Err(HubError::Invalid));
        assert!(hub.create_blob(1).is_ok());
    }

    #[test]
    fn abandoned_transfers_reclaim_operation_quota_and_completed_read_bytes() {
        let hub = OperationHub::with_blob_limits(1, 2, BlobLimits::new(4, 4, 8).unwrap()).unwrap();
        let blob = hub.create_blob(1).unwrap();
        let (ordinary, _) = hub.submit(1, None, 0, 0).unwrap();
        assert_eq!(
            hub.abandon_transfer_wire(1, ordinary.0),
            Err(HubError::WrongKind)
        );
        hub.cancel_wire(1, ordinary.0).unwrap();
        assert_eq!(hub.poll_wire(1, ordinary.0) as u8, STATUS_CANCELLED);
        for _ in 0..32 {
            let pending = hub.submit_blob_read(1, blob, 4).unwrap();
            assert_eq!(
                hub.abandon_transfer_wire(2, pending.0),
                Err(HubError::Stale)
            );
            hub.abandon_transfer_wire(1, pending.0).unwrap();
            assert_eq!(
                hub.abandon_transfer_wire(1, pending.0),
                Err(HubError::Invalid)
            );

            hub.blob_write(1, blob, b"data").unwrap();
            let completed = hub.submit_blob_read(1, blob, 4).unwrap();
            hub.abandon_transfer_wire(1, completed.0).unwrap();
            assert_eq!(hub.snapshot().retained_transfer_capacity, 0);

            hub.blob_write(1, blob, b"full").unwrap();
            let write = hub.submit_blob_write(1, blob, b"next").unwrap();
            hub.abandon_transfer_wire(1, write.0).unwrap();
            assert_eq!(hub.blob_read(1, blob, 4).unwrap(), b"full");
            assert_eq!(hub.state.lock().unwrap().operations.len(), 0);
        }
        hub.close_resource(blob).unwrap();
        assert_eq!(hub.snapshot().live_blobs, 0);
    }

    #[test]
    fn competing_host_submissions_cannot_overbook_pending_write_count() {
        let mut limits = BlobLimits::new(4, 4, 64).unwrap();
        limits.maximum_pending_writes = 1;
        let hub = OperationHub::with_blob_limits(32, 2, limits).unwrap();
        let blob = hub.create_blob(1).unwrap();
        hub.blob_write(1, blob, b"full").unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let hub = Arc::clone(&hub);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    hub.submit_blob_write(1, blob, b"next")
                })
            })
            .collect();
        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == Err(HubError::Quota))
                .count(),
            7
        );
        assert_eq!(hub.snapshot().pending_blob_writes, 1);
        assert_eq!(hub.snapshot().pending_operations, 1);
        assert_eq!(hub.snapshot().pending_write_bytes, 4);
        hub.close_resource(blob).unwrap();
        assert_eq!(hub.snapshot().pending_blob_writes, 0);
    }

    #[test]
    fn transfer_peak_records_copy_overlap_and_survives_teardown() {
        let hub = OperationHub::with_blob_limits(4, 1, BlobLimits::new(1024, 1024, 1024).unwrap())
            .unwrap();
        let blob = hub.create_blob(1).unwrap();
        let write = hub.submit_blob_write(1, blob, &[7; 1024]).unwrap();
        assert_eq!(hub.snapshot().retained_transfer_capacity, 1024);
        assert_eq!(hub.snapshot().peak_retained_transfer_capacity, 2048);
        hub.take_terminal(write, 1).unwrap().unwrap();
        let read = hub.submit_blob_read(1, blob, 1024).unwrap();
        assert_eq!(hub.snapshot().retained_transfer_capacity, 1024);
        assert_eq!(hub.snapshot().peak_retained_transfer_capacity, 2048);
        hub.take_blob_read(1, read).unwrap().unwrap();
        hub.close_all(Terminal::Closed);
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
        assert_eq!(hub.snapshot().peak_retained_transfer_capacity, 2048);
    }

    #[test]
    fn retained_blob_capacity_blocks_other_blob_growth_until_released() {
        let hub = OperationHub::with_blob_limits(4, 2, BlobLimits::new(1024, 1024, 1024).unwrap())
            .unwrap();
        let first = hub.create_blob(1).unwrap();
        let second = hub.create_blob(1).unwrap();
        hub.blob_write(1, first, &[7; 1024]).unwrap();
        drop(hub.blob_read(1, first, 512).unwrap());
        assert_eq!(hub.blob_write(1, second, &[8; 512]), Err(HubError::Quota));
        let pending = hub.submit_blob_write(1, second, &[8; 512]).unwrap();
        assert_eq!(hub.take_terminal(pending, 1), Ok(None));
        drop(hub.blob_read(1, first, 512).unwrap());
        assert_eq!(
            hub.take_terminal(pending, 1).unwrap().unwrap().terminal,
            Terminal::Completed
        );
        assert_eq!(hub.blob_read(1, second, 512).unwrap(), [8; 512]);
    }

    #[test]
    fn capacity_snapshot_includes_partial_blob_and_uncollected_read_allocations() {
        let hub = OperationHub::with_blob_limits(4, 1, BlobLimits::new(1024, 1024, 1024).unwrap())
            .unwrap();
        let blob = hub.create_blob(1).unwrap();
        hub.blob_write(1, blob, &[7; 1024]).unwrap();
        let read = hub.submit_blob_read(1, blob, 512).unwrap();
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.buffered_blob_bytes, 512);
        assert_eq!(snapshot.completed_read_bytes, 512);
        assert!(
            snapshot.retained_transfer_capacity >= 1536,
            "partial drain retains backing capacity plus the read result"
        );
        drop(hub.take_blob_read(1, read).unwrap());
        drop(hub.blob_read(1, blob, 512).unwrap());
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
        hub.close_all(Terminal::Closed);
        assert_eq!(hub.snapshot().retained_transfer_capacity, 0);
    }

    #[test]
    fn fully_drained_blobs_release_their_backing_allocation() {
        for asynchronous in [false, true] {
            let hub =
                OperationHub::with_blob_limits(4, 2, BlobLimits::new(1024, 1024, 1024).unwrap())
                    .unwrap();
            let blob = hub.create_blob(1).unwrap();
            hub.blob_write(1, blob, &[7; 1024]).unwrap();
            if asynchronous {
                let read = hub.submit_blob_read(1, blob, 1024).unwrap();
                hub.take_blob_read(1, read).unwrap().unwrap();
            } else {
                hub.blob_read(1, blob, 1024).unwrap();
            }
            let state = hub.state.lock().unwrap();
            let ResourceValue::Blob { buffer, .. } = &state.resources[&blob].value else {
                panic!("blob");
            };
            assert_eq!(
                buffer.capacity(),
                0,
                "empty live blobs must not retain unaccounted allocation"
            );
            assert_eq!(state.buffered_blob_bytes, 0);
        }
    }

    #[test]
    fn wire_read_collection_is_bounded_typed_and_consumed_once() {
        let hub = OperationHub::with_blob_limits(4, 1, BlobLimits::new(4, 4, 4).unwrap()).unwrap();
        let blob = hub.create_blob(1).unwrap();
        let read = hub.submit_blob_read(1, blob, 4).unwrap();
        assert_eq!(
            hub.collect_blob_read_wire(1, read.0, 4, |_| panic!("pending copy")),
            Ok(0)
        );
        hub.blob_write(1, blob, b"data").unwrap();
        assert_eq!(hub.poll_wire(1, read.0) as u8, STATUS_ERROR);
        assert_eq!(
            hub.collect_blob_read_wire(2, read.0, 4, |_| panic!("wrong owner")),
            Err(HubError::Stale)
        );
        assert_eq!(
            hub.collect_blob_read_wire(1, read.0, 3, |_| panic!("short destination")),
            Err(HubError::Quota)
        );
        let mut copied = Vec::new();
        assert_eq!(
            hub.collect_blob_read_wire(1, read.0, 4, |bytes| copied.extend_from_slice(bytes)),
            Ok((4 << 8) | u64::from(STATUS_COMPLETED))
        );
        assert_eq!(copied, b"data");
        assert_eq!(hub.snapshot().completed_read_bytes, 0);
        assert!(hub
            .collect_blob_read_wire(1, read.0, 4, |_| panic!("double collection"))
            .is_err());
        let write = hub.submit_blob_write(1, blob, b"next").unwrap();
        assert_eq!(
            hub.collect_blob_read_wire(1, write.0, 4, |_| panic!("wrong kind")),
            Err(HubError::WrongKind)
        );
        assert_eq!(hub.poll_wire(1, write.0) as u8, STATUS_COMPLETED);
    }

    #[test]
    fn guest_write_rejects_before_copy_and_retains_only_owned_bytes() {
        let hub = OperationHub::with_blob_limits(4, 1, BlobLimits::new(4, 4, 4).unwrap()).unwrap();
        let blob = hub.create_blob(1).unwrap();
        assert_eq!(
            hub.submit_blob_write_wire(1, blob.0, 5, || panic!("oversized copy")),
            Err(HubError::Quota)
        );
        assert_eq!(
            hub.submit_blob_write_wire(2, blob.0, 4, || panic!("unauthorized copy")),
            Err(HubError::WrongRights)
        );
        let mut guest_bytes = *b"data";
        let write = hub
            .submit_blob_write_wire(1, blob.0, 4, || guest_bytes.to_vec())
            .unwrap();
        guest_bytes.fill(0);
        assert_eq!(hub.poll_wire(1, write) as u8, STATUS_COMPLETED);
        assert_eq!(hub.blob_read(1, blob, 4).unwrap(), b"data");
    }

    #[test]
    fn wire_blob_creation_is_scoped_and_cancelled_creation_reclaims_capacity() {
        let hub = OperationHub::new(4, 1).unwrap();
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(
            hub.submit_wire(runtime.handle(), 1, OP_BLOB_CREATE, 1, 0),
            Err(HubError::Invalid)
        );
        let cancelled = hub
            .submit_wire(runtime.handle(), 1, OP_BLOB_CREATE, 0, 0)
            .unwrap();
        hub.cancel_wire(1, cancelled).unwrap();
        assert_eq!(hub.snapshot().live_resources, 0);
        assert_eq!(hub.poll_wire(1, cancelled) as u8, STATUS_CANCELLED);

        let operation = hub
            .submit_wire(runtime.handle(), 1, OP_BLOB_CREATE, 0, 0)
            .unwrap();
        let wake = hub.suspend_wire(1, operation).unwrap();
        runtime
            .run_within(
                async { wake.notified().await },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        let result = hub.poll_wire(1, operation);
        assert_eq!(result as u8, STATUS_COMPLETED);
        let blob = OpaqueToken(result >> 8);
        assert_eq!(
            hub.blob_write(2, blob, b"foreign"),
            Err(HubError::WrongRights)
        );
        assert_eq!(hub.blob_write(1, blob, b"owned"), Ok(5));
        let close = hub
            .submit_wire(runtime.handle(), 1, OP_SYNTHETIC_RESOURCE_CLOSE, blob.0, 0)
            .unwrap();
        let wake = hub.suspend_wire(1, close).unwrap();
        runtime
            .run_within(
                async { wake.notified().await },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(hub.poll_wire(1, close) as u8, STATUS_COMPLETED);
        assert_eq!(hub.snapshot().live_resources, 0);
        assert_eq!(hub.snapshot().buffered_blob_bytes, 0);
    }

    #[test]
    fn generated_create_at_operation_quota_reclaims_its_reservation() {
        let hub = OperationHub::new(1, 1).unwrap();
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (occupied, _) = hub.submit(1, None, 0, 0).unwrap();
        assert_eq!(
            hub.submit_wire(runtime.handle(), 1, OP_SYNTHETIC_RESOURCE_CREATE, 0, 1),
            Err(HubError::Quota)
        );
        assert_eq!(hub.snapshot().live_resources, 0);
        hub.cancel_wire(1, occupied.0).unwrap();
        hub.take_terminal(occupied, 1).unwrap();
        assert!(hub
            .submit_wire(runtime.handle(), 1, OP_SYNTHETIC_RESOURCE_CREATE, 0, 1)
            .is_ok());
    }

    #[test]
    fn teardown_reclaims_unconsumed_read_results_and_pending_write_buffers() {
        for reason in [
            Terminal::Cancelled,
            Terminal::Trapped,
            Terminal::TimedOut,
            Terminal::OwnerExited,
            Terminal::Closed,
        ] {
            let hub =
                OperationHub::with_blob_limits(8, 2, BlobLimits::new(4, 4, 4).unwrap()).unwrap();
            let blob = hub.create_blob(1).unwrap();
            hub.blob_write(1, blob, b"read").unwrap();
            let read = hub.submit_blob_read(1, blob, 4).unwrap();
            hub.blob_write(1, blob, b"full").unwrap();
            let write = hub.submit_blob_write(1, blob, b"wait").unwrap();
            let before = hub.snapshot();
            assert_eq!(before.completed_read_bytes, 4);
            assert_eq!(before.pending_write_bytes, 4);
            assert_eq!(before.buffered_blob_bytes, 4);
            hub.close_all(reason);
            let after = hub.snapshot();
            assert_eq!(
                (
                    after.completed_read_bytes,
                    after.pending_write_bytes,
                    after.buffered_blob_bytes,
                    after.live_resources,
                    after.pending_operations
                ),
                (0, 0, 0, 0, 0)
            );
            assert_eq!(hub.take_blob_read(1, read), Err(HubError::Closed));
            assert_eq!(
                hub.take_terminal(write, 1).unwrap().unwrap().terminal,
                reason
            );
        }
    }

    #[test]
    fn pending_reads_distinguish_data_eof_and_cancellation() {
        let hub = OperationHub::with_blob_limits(4, 2, BlobLimits::new(4, 4, 4).unwrap()).unwrap();
        let blob = hub.create_blob(1).unwrap();
        let read = hub.submit_blob_read(1, blob, 4).unwrap();
        assert_eq!(hub.take_blob_read(1, read), Ok(None));
        let wake = hub.suspend(read, 1).unwrap();
        hub.submit_blob_write(1, blob, b"data").unwrap();
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    crate::async_engine::timeout(
                        std::time::Duration::from_secs(1),
                        wake.notified(),
                    )
                    .await
                    .unwrap();
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(
            hub.take_blob_read(1, read),
            Ok(Some((Terminal::Completed, b"data".to_vec())))
        );
        let cancelled = hub.submit_blob_read(1, blob, 4).unwrap();
        hub.cancel_wire(1, cancelled.0).unwrap();
        assert_eq!(
            hub.take_blob_read(1, cancelled),
            Ok(Some((Terminal::Cancelled, vec![])))
        );
        let eof = hub.submit_blob_read(1, blob, 4).unwrap();
        assert_eq!(hub.take_blob_read(1, eof), Ok(None));
        hub.seal_blob(1, blob).unwrap();
        assert_eq!(
            hub.take_blob_read(1, eof),
            Ok(Some((Terminal::Completed, vec![])))
        );
    }

    #[test]
    fn pending_blob_write_resumes_after_pull_and_cancellation_reclaims_input() {
        let hub = OperationHub::with_blob_limits(4, 2, BlobLimits::new(4, 4, 4).unwrap()).unwrap();
        let blob = hub.create_blob(1).unwrap();
        hub.blob_write(1, blob, b"full").unwrap();
        let write = hub.submit_blob_write(1, blob, b"next").unwrap();
        assert_eq!(hub.take_terminal(write, 1), Ok(None));
        let wake = hub.suspend(write, 1).unwrap();
        assert_eq!(hub.blob_read(1, blob, 4).unwrap(), b"full");
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    crate::async_engine::timeout(
                        std::time::Duration::from_secs(1),
                        wake.notified(),
                    )
                    .await
                    .unwrap();
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(
            hub.take_terminal(write, 1).unwrap().unwrap().terminal,
            Terminal::Completed
        );
        let cancelled = hub.submit_blob_write(1, blob, b"lost").unwrap();
        hub.cancel_wire(1, cancelled.0).unwrap();
        assert!(hub.state.lock().unwrap().operations[&cancelled]
            .pending_blob_write
            .is_none());
        assert_eq!(hub.blob_read(1, blob, 4).unwrap(), b"next");
        assert!(hub.blob_read(1, blob, 4).unwrap().is_empty());
        assert_eq!(
            hub.take_terminal(cancelled, 1).unwrap().unwrap().terminal,
            Terminal::Cancelled
        );
        hub.close_all(Terminal::Closed);
        assert_eq!(hub.snapshot().pending_operations, 0);
        assert_eq!(hub.snapshot().buffered_blob_bytes, 0);
    }

    #[test]
    fn pending_blob_transfer_expires_after_its_own_idle_budget() {
        let mut limits = BlobLimits::new(4, 4, 4).unwrap();
        limits.progress_idle_timeout = std::time::Duration::from_millis(10);
        let hub = OperationHub::with_blob_limits(4, 2, limits).unwrap();
        let blob = hub.create_blob(1).unwrap();
        hub.blob_write(1, blob, b"full").unwrap();
        let write = hub.submit_blob_write(1, blob, b"next").unwrap();
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let wake = hub
            .suspend_stream_wire(runtime.handle(), 1, write.wire())
            .unwrap();
        runtime
            .run_within(
                async {
                    crate::async_engine::timeout(
                        std::time::Duration::from_secs(1),
                        wake.notified(),
                    )
                    .await
                    .expect("stalled blob transfer must wake its guest waiter");
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(hub.poll_wire(1, write.wire()) as u8, STATUS_TIMED_OUT);
        assert_eq!(hub.snapshot().pending_blob_writes, 0);
    }

    #[test]
    fn a_new_blob_transfer_does_not_inherit_a_previous_idle_window() {
        let mut limits = BlobLimits::new(4, 4, 4).unwrap();
        limits.progress_idle_timeout = std::time::Duration::from_millis(10);
        let hub = OperationHub::with_blob_limits(4, 2, limits).unwrap();
        let blob = hub.create_blob(1).unwrap();
        hub.blob_write(1, blob, b"full").unwrap();
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async {
                    crate::async_engine::sleep(std::time::Duration::from_millis(20)).await;
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        let write = hub.submit_blob_write(1, blob, b"next").unwrap();
        let wake = hub
            .suspend_stream_wire(runtime.handle(), 1, write.wire())
            .unwrap();
        runtime
            .run_within(
                async {
                    crate::async_engine::sleep(std::time::Duration::from_millis(5)).await;
                    assert_eq!(hub.poll_wire(1, write.wire()) as u8, STATUS_PENDING);
                    crate::async_engine::timeout(
                        std::time::Duration::from_secs(1),
                        wake.notified(),
                    )
                    .await
                    .expect("the new transfer must eventually report its own idle timeout");
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(hub.poll_wire(1, write.wire()) as u8, STATUS_TIMED_OUT);
    }

    #[test]
    fn blob_progress_keeps_a_pending_transfer_alive_past_one_idle_interval() {
        let mut limits = BlobLimits::new(4, 4, 4).unwrap();
        limits.progress_idle_timeout = std::time::Duration::from_millis(10);
        let hub = OperationHub::with_blob_limits(4, 2, limits).unwrap();
        let blob = hub.create_blob(1).unwrap();
        hub.blob_write(1, blob, b"full").unwrap();
        let write = hub.submit_blob_write(1, blob, b"next").unwrap();
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let wake = hub
            .suspend_stream_wire(runtime.handle(), 1, write.wire())
            .unwrap();
        runtime
            .run_within(
                async {
                    // Each one-byte pull is observable progress on this *same* blob.
                    // The pending four-byte write cannot yet complete, so crossing
                    // three idle intervals distinguishes a resettable idle deadline
                    // from a total worker or operation deadline.
                    for expected in *b"ful" {
                        crate::async_engine::sleep(std::time::Duration::from_millis(7)).await;
                        assert_eq!(hub.blob_read(1, blob, 1).unwrap(), [expected]);
                        assert_eq!(hub.poll_wire(1, write.wire()) as u8, STATUS_PENDING);
                    }
                    crate::async_engine::sleep(std::time::Duration::from_millis(7)).await;
                    assert_eq!(hub.blob_read(1, blob, 1).unwrap(), b"l");
                    crate::async_engine::timeout(
                        std::time::Duration::from_secs(1),
                        wake.notified(),
                    )
                    .await
                    .expect("the now-admitted writer must wake its guest waiter");
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(hub.poll_wire(1, write.wire()) as u8, STATUS_COMPLETED);
    }

    #[test]
    fn generated_close_resumes_another_blobs_capacity_waiter() {
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hub = OperationHub::with_blob_limits(4, 2, BlobLimits::new(4, 4, 4).unwrap()).unwrap();
        let first = hub.create_blob(1).unwrap();
        let second = hub.create_blob(1).unwrap();
        hub.blob_write(1, first, b"full").unwrap();
        let write = hub.submit_blob_write(1, second, b"next").unwrap();
        let close = hub
            .submit_wire(runtime.handle(), 1, OP_SYNTHETIC_RESOURCE_CLOSE, first.0, 0)
            .unwrap();
        let wake = hub.suspend_wire(1, close).unwrap();
        runtime
            .run_within(
                async { wake.notified().await },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(hub.poll_wire(1, close) as u8, STATUS_COMPLETED);
        assert!(
            hub.take_terminal(write, 1).unwrap().is_some(),
            "generated close must drive writes"
        );
    }

    #[test]
    fn read_progress_drives_previously_skipped_writers_without_collection() {
        let hub = OperationHub::with_blob_limits(8, 4, BlobLimits::new(4, 4, 8).unwrap()).unwrap();
        let a = hub.create_blob(1).unwrap();
        let b = hub.create_blob(1).unwrap();
        let c = hub.create_blob(1).unwrap();
        let retained = hub.create_blob(1).unwrap();
        hub.blob_write(1, a, b"full").unwrap();
        hub.blob_write(1, retained, b"full").unwrap();
        hub.submit_blob_write(1, b, b"next").unwrap();
        let last = hub.submit_blob_write(1, c, b"last").unwrap();
        // Release backing allocations, not merely payload-length credits.
        hub.submit_blob_read(1, b, 4).unwrap();
        drop(hub.blob_read(1, a, 4).unwrap());
        assert!(
            hub.take_terminal(last, 1).unwrap().is_some(),
            "read progress must revisit skipped writers"
        );
    }

    #[test]
    fn closing_a_blob_releases_shared_capacity_to_another_pending_writer() {
        let hub = OperationHub::with_blob_limits(4, 2, BlobLimits::new(4, 4, 4).unwrap()).unwrap();
        let first = hub.create_blob(1).unwrap();
        let second = hub.create_blob(1).unwrap();
        hub.blob_write(1, first, b"full").unwrap();
        let write = hub.submit_blob_write(1, second, b"next").unwrap();
        assert_eq!(hub.take_terminal(write, 1), Ok(None));
        hub.close_resource(first).unwrap();
        assert_eq!(
            hub.take_terminal(write, 1).unwrap().unwrap().terminal,
            Terminal::Completed
        );
        assert_eq!(hub.blob_read(1, second, 4).unwrap(), b"next");
    }

    #[test]
    fn stale_cross_owner_and_close_races_are_typed_and_bounded() {
        let hub = OperationHub::new(1, 1).unwrap();
        let resource = hub.create_resource(0, 7, 0b11, true).unwrap();
        hub.activate_for_test(resource);
        let (operation, _) = hub.submit(0, Some(resource), 7, 0b01).unwrap();
        assert!(matches!(hub.submit(0, None, 0, 0), Err(HubError::Quota)));
        assert_eq!(hub.take_terminal(operation, 2), Err(HubError::Stale));
        hub.close_resource(resource).unwrap();
        hub.terminal(
            operation,
            TerminalResult {
                terminal: Terminal::Completed,
                resource: None,
            },
        )
        .unwrap();
        assert_eq!(
            hub.take_terminal(operation, 0),
            Ok(Some(TerminalResult {
                terminal: Terminal::Closed,
                resource: None
            }))
        );
        assert_eq!(hub.snapshot().pending_operations, 0);
        assert_eq!(hub.snapshot().live_resources, 0);
    }

    #[test]
    fn nonshareable_resource_rejects_another_store_before_operation_creation() {
        let hub = OperationHub::new(2, 1).unwrap();
        let resource = hub.create_resource(0, 1, 1, false).unwrap();
        hub.activate_for_test(resource);
        assert!(matches!(
            hub.submit(1, Some(resource), 1, 1),
            Err(HubError::WrongRights)
        ));
        assert_eq!(hub.snapshot().pending_operations, 0);
    }

    #[test]
    fn terminal_consumption_reclaims_quota_and_close_is_idempotent() {
        let hub = OperationHub::new(1, 1).unwrap();
        let resource = hub.create_resource(3, 9, 1, false).unwrap();
        hub.activate_for_test(resource);
        let (first, _) = hub.submit(3, Some(resource), 9, 1).unwrap();
        hub.terminal(
            first,
            TerminalResult {
                terminal: Terminal::Completed,
                resource: Some(resource),
            },
        )
        .unwrap();
        assert!(hub.take_terminal(first, 3).unwrap().is_some());
        assert!(hub.submit(3, Some(resource), 9, 1).is_ok());
        hub.close_resource(resource).unwrap();
        hub.close_resource(resource).unwrap();
        let replacement = hub.create_resource(3, 9, 1, false).unwrap();
        assert_ne!(resource, replacement, "stale token cannot name reused slot");
    }

    #[test]
    fn external_webview_uses_the_same_generation_safe_authority() {
        let hub = OperationHub::new(4, 2).unwrap();
        let (resource, open) = hub.begin_external_webview_open(41).unwrap();
        hub.finish_external_open(open, resource);
        assert_eq!(
            hub.observe_terminal(41, open),
            Ok(Some(TerminalResult {
                terminal: Terminal::Completed,
                resource: Some(resource),
            }))
        );
        let wait = hub.begin_external_webview_wait(41, resource).unwrap();
        assert_eq!(
            hub.begin_external_webview_wait(42, resource),
            Err(HubError::WrongRights),
            "another logical instance cannot borrow this external resource"
        );
        hub.finish_external_operation(wait, Terminal::Completed);
        assert_eq!(
            hub.observe_terminal(41, wait),
            Ok(Some(TerminalResult {
                terminal: Terminal::Completed,
                resource: None,
            }))
        );
        let close = hub.begin_external_webview_close(41, resource).unwrap();
        hub.finish_external_operation(close, Terminal::Completed);
        assert_eq!(
            hub.observe_terminal(41, close),
            Ok(Some(TerminalResult {
                terminal: Terminal::Completed,
                resource: None,
            }))
        );
        hub.close_resource(resource).unwrap();
        assert_eq!(
            hub.begin_external_webview_wait(41, resource),
            Err(HubError::Closed),
            "closed native backing cannot revive its generation"
        );
    }

    #[test]
    fn native_terminal_cleanup_reclaims_every_handle_and_rejects_stale_use() {
        for terminal in [Terminal::TimedOut, Terminal::Cancelled, Terminal::Closed] {
            let hub = OperationHub::new(4, 1).unwrap();
            let (resource, open) = hub.begin_external_webview_open(7).unwrap();
            hub.finish_external_open(open, resource);
            assert!(matches!(
                hub.observe_terminal(7, open),
                Ok(Some(TerminalResult {
                    terminal: Terminal::Completed,
                    resource: Some(_),
                }))
            ));
            let waiter = hub.begin_external_webview_wait(7, resource).unwrap();
            hub.revoke_external_resource(resource, terminal).unwrap();
            assert_eq!(
                hub.observe_terminal(7, waiter),
                Ok(Some(TerminalResult {
                    terminal,
                    resource: None,
                }))
            );
            assert_eq!(
                hub.begin_external_webview_wait(7, resource),
                Err(HubError::Closed),
                "a terminal native event must not revive the old generation"
            );
            let snapshot = hub.snapshot();
            assert_eq!(snapshot.pending_operations, 0, "{terminal:?}");
            assert_eq!(snapshot.live_resources, 0, "{terminal:?}");
        }
    }

    #[test]
    fn window_presentation_operations_need_their_own_right_and_a_live_generation() {
        let hub = OperationHub::new(4, 2).unwrap();
        let (resource, open) = hub.begin_external_webview_open(7).unwrap();
        hub.finish_external_open(open, resource);
        assert!(hub.observe_terminal(7, open).unwrap().is_some());
        let window = hub.begin_external_webview_window(7, resource).unwrap();
        assert_eq!(
            hub.begin_external_webview_window(8, resource),
            Err(HubError::WrongRights),
            "another logical instance cannot resize, show, hide, or focus this view"
        );
        hub.finish_external_operation(window, Terminal::Completed);
        assert_eq!(
            hub.observe_terminal(7, window),
            Ok(Some(TerminalResult {
                terminal: Terminal::Completed,
                resource: None,
            }))
        );
        // A view reserved without the window right (load-only authority)
        // cannot be repositioned through presentation operations.
        let load_only = hub
            .create_resource_value(
                7,
                EXTERNAL_WEBVIEW_RESOURCE_KIND,
                EXTERNAL_WEBVIEW_RIGHT_LOAD,
                false,
                ResourceValue::ExternalWebview,
            )
            .unwrap();
        hub.state
            .lock()
            .unwrap()
            .resources
            .get_mut(&load_only)
            .unwrap()
            .reserved = false;
        assert_eq!(
            hub.begin_external_webview_window(7, load_only),
            Err(HubError::WrongRights)
        );
        hub.close_resource(load_only).unwrap();
        // A pending presentation operation observes the revocation's typed
        // terminal, and the stale generation then reports Closed.
        let pending = hub.begin_external_webview_window(7, resource).unwrap();
        hub.revoke_external_resource(resource, Terminal::Closed)
            .unwrap();
        assert_eq!(
            hub.observe_terminal(7, pending),
            Ok(Some(TerminalResult {
                terminal: Terminal::Closed,
                resource: None,
            }))
        );
        assert_eq!(
            hub.begin_external_webview_window(7, resource),
            Err(HubError::Closed)
        );
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.pending_operations, 0);
        assert_eq!(snapshot.live_resources, 0);
    }

    #[test]
    fn navigation_needs_its_own_right_and_a_live_generation() {
        let hub = OperationHub::new(4, 2).unwrap();
        let (resource, open) = hub.begin_external_webview_open(7).unwrap();
        hub.finish_external_open(open, resource);
        assert!(hub.observe_terminal(7, open).unwrap().is_some());
        let navigate = hub.begin_external_webview_navigate(7, resource).unwrap();
        assert_eq!(
            hub.begin_external_webview_navigate(8, resource),
            Err(HubError::WrongRights),
            "another logical instance cannot navigate this view"
        );
        hub.finish_external_operation(navigate, Terminal::Completed);
        assert!(hub.observe_terminal(7, navigate).unwrap().is_some());
        // Presentation authority alone does not confer navigation.
        let window_only = hub
            .create_resource_value(
                7,
                EXTERNAL_WEBVIEW_RESOURCE_KIND,
                EXTERNAL_WEBVIEW_RIGHT_LOAD | EXTERNAL_WEBVIEW_RIGHT_WINDOW,
                false,
                ResourceValue::ExternalWebview,
            )
            .unwrap();
        hub.state
            .lock()
            .unwrap()
            .resources
            .get_mut(&window_only)
            .unwrap()
            .reserved = false;
        assert_eq!(
            hub.begin_external_webview_navigate(7, window_only),
            Err(HubError::WrongRights)
        );
        hub.close_resource(window_only).unwrap();
        hub.revoke_external_resource(resource, Terminal::Closed)
            .unwrap();
        assert_eq!(
            hub.begin_external_webview_navigate(7, resource),
            Err(HubError::Closed)
        );
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.pending_operations, 0);
        assert_eq!(snapshot.live_resources, 0);
    }

    #[test]
    fn terminal_between_guest_poll_and_yield_still_wakes_the_owner() {
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        for terminal in [Terminal::Completed, Terminal::Cancelled, Terminal::Closed] {
            let hub = OperationHub::new(1, 1).unwrap();
            let (operation, _) = hub.submit(4, None, 0, 0).unwrap();
            assert_eq!(hub.poll_wire(4, operation.0), 0);
            hub.terminal(
                operation,
                TerminalResult {
                    terminal,
                    resource: None,
                },
            )
            .unwrap();
            assert!(matches!(
                hub.suspend_wire(5, operation.0),
                Err(HubError::Stale)
            ));
            let wake = hub
                .suspend_wire(4, operation.0)
                .expect("completion must not reject yield");
            runtime
                .run_within(
                    async {
                        crate::async_engine::timeout(
                            std::time::Duration::from_secs(1),
                            wake.notified(),
                        )
                        .await
                        .expect("terminal operation must wake immediately");
                    },
                    std::time::Duration::from_secs(60),
                )
                .expect("test runtime budget");
            assert_eq!(hub.poll_wire(4, operation.0) as u8, status(terminal));
            assert!(hub.suspend_wire(4, operation.0).is_err());
        }
    }

    #[test]
    fn completion_before_waiter_registration_is_not_lost() {
        let hub = OperationHub::new(1, 1).unwrap();
        let (operation, _) = hub.submit(4, None, 0, 0).unwrap();
        let wake = hub.suspend(operation, 4).unwrap();
        hub.terminal(
            operation,
            TerminalResult {
                terminal: Terminal::Cancelled,
                resource: None,
            },
        )
        .unwrap();
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .run_within(
                async { wake.notified().await },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(
            hub.take_terminal(operation, 4),
            Ok(Some(TerminalResult {
                terminal: Terminal::Cancelled,
                resource: None,
            }))
        );
        assert_eq!(hub.snapshot().pending_operations, 0);
    }

    #[test]
    fn common_dispatch_reserves_resource_before_scheduling_and_returns_it_once() {
        let hub = OperationHub::new(2, 1).unwrap();
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let operation = hub
            .dispatch(
                runtime.handle(),
                0,
                Request::SyntheticCreate {
                    kind: 5,
                    rights: 1,
                    shareable: false,
                },
            )
            .unwrap();
        let wake = hub.suspend(operation, 0).unwrap();
        runtime
            .run_within(
                async { wake.notified().await },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        let result = hub.take_terminal(operation, 0).unwrap().unwrap();
        assert_eq!(result.terminal, Terminal::Completed);
        assert!(result.resource.is_some());
        assert_eq!(hub.snapshot().live_resources, 1);
    }

    #[test]
    fn generated_close_stays_pending_until_its_guest_waiter_parks() {
        let hub = OperationHub::new(2, 1).unwrap();
        let resource = hub.create_resource(0, 5, 1, false).unwrap();
        hub.activate_for_test(resource);
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();

        let operation = hub
            .dispatch(runtime.handle(), 0, Request::Close { resource })
            .unwrap();

        // Drive the detached completion past its scheduler yield before the
        // generated synchronous `operation_yield` import is invoked. The old
        // implementation terminalized here, making the subsequent suspend
        // fail nondeterministically in the real Wasm artifact.
        runtime
            .run_within(
                async {
                    crate::async_engine::yield_now().await;
                    crate::async_engine::yield_now().await;
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(hub.take_terminal(operation, 0), Ok(None));
        assert_eq!(hub.snapshot().live_resources, 1);

        let wake = hub.suspend(operation, 0).unwrap();
        runtime
            .run_within(
                async { wake.notified().await },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(
            hub.take_terminal(operation, 0),
            Ok(Some(TerminalResult {
                terminal: Terminal::Completed,
                resource: None,
            }))
        );
        assert_eq!(
            hub.begin_external_webview_wait(0, resource),
            Err(HubError::Closed),
            "close revokes the generation only after the guest is parked"
        );
        assert_eq!(hub.snapshot().suspends, 1);
        assert_eq!(hub.snapshot().resumes, 1);
        assert_eq!(hub.snapshot().pending_operations, 0);
        assert_eq!(hub.snapshot().live_resources, 0);
    }

    #[test]
    fn cancelled_deferred_close_cannot_revoke_its_resource() {
        let hub = OperationHub::new(2, 1).unwrap();
        let resource = hub.create_resource(0, 5, 1, false).unwrap();
        hub.activate_for_test(resource);
        let (operation, _) = hub.submit(0, None, 0, 0).unwrap();
        let deferred = DeferredCompletion {
            result: TerminalResult {
                terminal: Terminal::Completed,
                resource: None,
            },
            revoke: Some(resource),
        };

        hub.cancel_wire(0, operation.0).unwrap();
        // This models a detached close completion that was copied from the
        // deferred slot just as cancellation won. It must be a no-op: a
        // cancelled close does not acquire authority to revoke its resource.
        hub.finish_deferred_completion(operation, deferred).unwrap();

        assert_eq!(
            hub.take_terminal(operation, 0),
            Ok(Some(TerminalResult {
                terminal: Terminal::Cancelled,
                resource: None,
            }))
        );
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.pending_operations, 0);
        assert_eq!(snapshot.live_resources, 1);
        assert!(hub.submit(0, Some(resource), 5, 1).is_ok());
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.pending_operations, 1);
        assert_eq!(snapshot.live_resources, 1);
    }

    #[test]
    fn wire_submit_and_poll_use_one_packed_terminal_result() {
        let hub = OperationHub::new(2, 1).unwrap();
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let operation = hub
            .submit_wire(runtime.handle(), 0, OP_SYNTHETIC_RESOURCE_CREATE, 0, 1)
            .unwrap();
        let wake = hub.suspend_wire(0, operation).unwrap();
        runtime
            .run_within(
                async { wake.notified().await },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        let packed = hub.poll_wire(0, operation);
        assert_eq!(packed & 0xff, u64::from(STATUS_COMPLETED));
        assert_ne!(
            packed >> 8,
            0,
            "create payload is the opaque resource token"
        );
        assert_eq!(hub.poll_wire(0, operation) & 0xff, u64::from(STATUS_ERROR));
    }

    #[test]
    fn opaque_blob_transfers_sixty_four_mebibytes_in_bounded_pull_chunks() {
        let limits = BlobLimits::new(64 * 1024, 64 * 1024, 64 * 1024).unwrap();
        let hub = OperationHub::with_blob_limits(4, 2, limits).unwrap();
        let blob = hub.create_blob(7).unwrap();
        let chunk = vec![0xA5; limits.maximum_chunk_bytes];
        let mut transferred = 0;
        while transferred < 64 * 1024 * 1024 {
            assert_eq!(hub.blob_write(7, blob, &chunk), Ok(chunk.len()));
            assert_eq!(hub.blob_read(7, blob, chunk.len()), Ok(chunk.clone()));
            transferred += chunk.len();
            assert_eq!(hub.snapshot().buffered_blob_bytes, 0);
        }
        let snapshot = hub.snapshot();
        assert_eq!(
            snapshot.peak_buffered_blob_bytes,
            limits.maximum_chunk_bytes
        );
        assert_eq!(snapshot.live_resources, 1);
    }

    #[test]
    fn blob_capacity_is_released_only_by_a_bounded_pull_and_close_reclaims_it() {
        let limits = BlobLimits::new(4, 8, 8).unwrap();
        let hub = OperationHub::with_blob_limits(2, 2, limits).unwrap();
        let blob = hub.create_blob(1).unwrap();
        assert_eq!(hub.blob_write(1, blob, b"1234"), Ok(4));
        assert_eq!(hub.blob_write(1, blob, b"5678"), Ok(4));
        assert_eq!(hub.blob_write(1, blob, b"x"), Err(HubError::Quota));
        assert_eq!(hub.blob_read(1, blob, 4), Ok(b"1234".to_vec()));
        assert_eq!(hub.blob_write(1, blob, b"x"), Ok(1));
        assert_eq!(hub.snapshot().buffered_blob_bytes, 5);
        hub.close_resource(blob).unwrap();
        assert_eq!(hub.snapshot().buffered_blob_bytes, 0);
        assert_eq!(hub.blob_read(1, blob, 1), Err(HubError::Invalid));
    }

    #[test]
    fn blob_scope_and_chunk_limits_reject_before_copying() {
        let limits = BlobLimits::new(4, 8, 8).unwrap();
        let hub = OperationHub::with_blob_limits(2, 2, limits).unwrap();
        let blob = hub.create_blob(1).unwrap();
        assert_eq!(hub.blob_write(2, blob, b"a"), Err(HubError::WrongRights));
        assert_eq!(hub.blob_write(1, blob, b"12345"), Err(HubError::Quota));
        assert_eq!(hub.snapshot().buffered_blob_bytes, 0);
        hub.close_all(Terminal::Trapped);
        assert_eq!(hub.snapshot().buffered_blob_bytes, 0);
        assert_eq!(hub.snapshot().live_resources, 0);
    }

    #[cfg(feature = "wasm-sketch-host")]
    #[test]
    fn exact_output_commits_only_the_authorized_path_after_bounded_blob_flush() {
        let directory = tempfile::tempdir().unwrap();
        let final_path = directory.path().join("capture.png");
        std::fs::write(&final_path, b"old").unwrap();
        let limits = BlobLimits::new(4, 8, 8).unwrap();
        let hub = OperationHub::with_blob_limits(4, 4, limits).unwrap();
        let blob = hub.create_blob(11).unwrap();
        let output = hub.grant_exact_output(11, &final_path).unwrap();
        hub.blob_write(11, blob, b"png-").unwrap();
        hub.blob_write(11, blob, b"byte").unwrap();
        hub.seal_blob(11, blob).unwrap();
        hub.commit_blob_to_output(11, blob, output).unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), b"png-byte");
        assert_eq!(hub.snapshot().buffered_blob_bytes, 0);
        assert_eq!(hub.snapshot().live_resources, 0);
        assert!(std::fs::read_dir(directory.path())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("kernal-api-")));
    }

    #[cfg(feature = "wasm-sketch-host")]
    #[test]
    fn exact_output_handle_cannot_be_used_by_another_instance() {
        let directory = tempfile::tempdir().unwrap();
        let final_path = directory.path().join("capture.png");
        let hub = OperationHub::with_blob_limits(4, 4, BlobLimits::new(4, 8, 8).unwrap()).unwrap();
        let blob = hub.create_blob(1).unwrap();
        let output = hub.grant_exact_output(1, &final_path).unwrap();
        hub.blob_write(1, blob, b"data").unwrap();
        let error = hub.commit_blob_to_output(2, blob, output).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(!final_path.exists());
        assert_eq!(hub.snapshot().buffered_blob_bytes, 4);
    }

    #[cfg(feature = "wasm-sketch-host")]
    #[test]
    fn failed_exact_output_replace_removes_the_temporary_and_never_replaces_final() {
        let directory = tempfile::tempdir().unwrap();
        // A directory is a canonical, exact host object but not a replaceable
        // file. It gives this test a real platform rename failure without a
        // test-only filesystem side channel.
        let final_path = directory.path().join("not-a-file");
        std::fs::create_dir(&final_path).unwrap();
        let hub = OperationHub::with_blob_limits(4, 4, BlobLimits::new(4, 8, 8).unwrap()).unwrap();
        let blob = hub.create_blob(1).unwrap();
        let output = hub.grant_exact_output(1, &final_path).unwrap();
        hub.blob_write(1, blob, b"data").unwrap();
        hub.seal_blob(1, blob).unwrap();
        assert!(hub.commit_blob_to_output(1, blob, output).is_err());
        assert!(
            final_path.is_dir(),
            "a failed commit must not replace final"
        );
        assert!(std::fs::read_dir(directory.path())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("kernal-api-")));
        hub.close_all(Terminal::Cancelled);
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.live_resources, 0);
        assert_eq!(snapshot.buffered_blob_bytes, 0);
        assert_eq!(snapshot.pending_operations, 0);
    }

    #[cfg(feature = "wasm-sketch-host")]
    #[test]
    fn empty_unsealed_blob_cannot_commit_a_truncated_output() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("output");
        std::fs::write(&destination, b"previous").unwrap();
        let hub = OperationHub::new(4, 4).unwrap();
        let blob = hub.create_blob(1).unwrap();
        let output = hub.grant_exact_output(1, &destination).unwrap();
        assert_eq!(
            hub.commit_blob_to_output(1, blob, output)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(std::fs::read(&destination).unwrap(), b"previous");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        hub.blob_write(1, blob, b"complete").unwrap();
        hub.seal_blob(1, blob).unwrap();
        assert_eq!(hub.blob_write(1, blob, b"late"), Err(HubError::Closed));
        hub.commit_blob_to_output(1, blob, output).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"complete");
    }
}
