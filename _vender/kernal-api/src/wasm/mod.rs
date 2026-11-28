//! Bounded admission of versioned core-Wasm sketches.
//!
//! Admission performs one private Cranelift compilation after the complete
//! binary contract has succeeded. The threaded-root profile can then start in
//! a fresh private store; no Wasmtime handle appears in the public API.

#[cfg(feature = "wasm-sketch-worker")]
mod worker;
#[cfg(all(feature = "wasm-sketch-worker", feature = "tauri-webview-test-support"))]
pub use worker::SketchWorkerTrace;
#[cfg(feature = "wasm-sketch-worker")]
#[allow(dead_code)] // Consumed by the phase-B private worker binary/supervisor.
mod worker_protocol;
#[cfg(feature = "wasm-sketch-worker")]
pub use worker::{
    SketchWorkerConfig, SketchWorkerExecutionSnapshot, SketchWorkerFailure, SketchWorkerStopReason,
    SketchWorkerTerminal,
};

// Regenerated explicitly by `tools/wasm-abi-generator`; ordinary facade
// builds consume this checked-in private host linker.
#[rustfmt::skip]
#[path = "generated/v1/wasmtime45_host_linker.rs"]
#[allow(dead_code, clippy::drop_non_drop)]
mod generated_v1;
#[rustfmt::skip]
#[path = "generated/v1/admission_contract.rs"]
mod generated_v1_contract;

use crate::operations::{self, OperationHub};

use std::cell::UnsafeCell;
use std::fmt;
use std::mem::align_of;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wasmparser::{CompositeInnerType, ExternalKind, Parser, Payload, TypeRef, ValType};
use wasmtime::{
    Caller, Config, Engine, InstancePre, Linker, MemoryType, Module, SharedMemory, Store, Strategy,
    UpdateDeadline,
};

#[cfg(test)]
#[test]
fn wasm_host_uses_the_root_shared_operation_authority_module() {
    assert_eq!(
        std::any::type_name::<OperationHub>(),
        "kernal_api::operations::OperationHub",
        "the Wasm host and native facade must not compile separate operation hubs"
    );
}

const PAGE_BYTES: u64 = 64 * 1024;
const ABI_MODULE: &str = generated_v1_contract::NAMESPACE;
const ABI_YIELD: &str = generated_v1_contract::KERNEL_YIELD;
const THREAD_MODULE: &str = "wasi";
const THREAD_SPAWN: &str = "thread-spawn";
const MEMORY_MODULE: &str = "env";
const MEMORY_NAME: &str = "memory";
const ENTRY: &str = "kernal-api-run";
const ABI_METADATA: &str = "kernal-api.abi";
const ABI_METADATA_VALUE: &[u8] = generated_v1_contract::METADATA;
const PROFILE_METADATA: &str = "kernal-api.profile";
const PROFILE_METADATA_VALUE: &[u8] = b"threaded-core-wasm-v1";
const VALIDATION_PROFILE_METADATA_VALUE: &[u8] = b"threaded-core-wasm-validation-v1";
const VALIDATION_REPORT: &str = "kernal-api-threaded-validation-report-v1";
const MAX_METADATA_BYTES: usize = 128;
const THREADED_RUST_INITIAL_PAGES: u32 = 17;
const THREADED_RUST_MAX_PAGES: u32 = 16_384;
const THREADED_RUST_RESERVATION_BYTES: u64 = (THREADED_RUST_MAX_PAGES as u64) * PAGE_BYTES;
const ERRNO_SUCCESS: i32 = 0;
const ERRNO_FAULT: i32 = 21;
const THREAD_SPAWN_REJECTED: i32 = -1;
const MAX_P1_IOVECS: usize = 1024;
/// Absolute v1 ceiling. This bounds native JoinHandles and the facade-owned
/// child-outcome vector independently of a caller's requested quota.
const MAX_GUEST_THREADS_V1: usize = 16;
const MAX_PENDING_OPERATIONS_V1: usize = 64;
const MAX_RESOURCES_V1: usize = 64;
const DEFAULT_MAX_GUEST_THREADS: usize = MAX_GUEST_THREADS_V1;
const EPOCH_PENDING: u8 = 0;
const EPOCH_CANCELLED: u8 = 1;
const EPOCH_DEADLINE_EXCEEDED: u8 = 2;
const EPOCH_COMPLETED: u8 = 3;

#[cfg(test)]
const GENERATED_V1_MANIFEST: &str = include_str!("generated/v1/kernal-api-v1.abi.toml");

#[cfg(test)]
#[test]
fn generated_v1_manifest_matches_the_closed_admission_contract() {
    // The explicit drift command proves the whole generated directory. This
    // focused assertion makes any admission-name/signature change fail in the
    // host test suite before Wasmtime can compile a module.
    assert!(
        GENERATED_V1_MANIFEST.contains(&format!("schema = \"{}\"", generated_v1_contract::SCHEMA))
    );
    assert!(GENERATED_V1_MANIFEST.contains(&format!(
        "schema_revision = {}",
        generated_v1_contract::SCHEMA_REVISION
    )));
    assert!(GENERATED_V1_MANIFEST.contains(&format!(
        "generator_revision = {}",
        generated_v1_contract::GENERATOR_REVISION
    )));
    assert!(GENERATED_V1_MANIFEST.contains(&format!(
        "abi_version = {}",
        generated_v1_contract::ABI_VERSION
    )));
    assert!(GENERATED_V1_MANIFEST.contains(&format!("namespace = \"{ABI_MODULE}\"")));
    assert!(GENERATED_V1_MANIFEST.contains(&format!("name = \"{ABI_YIELD}\"")));
    assert!(GENERATED_V1_MANIFEST.contains("params = []"));
    assert!(GENERATED_V1_MANIFEST.contains("results = [{ semantic = \"()\", abi = \"unit\" }]"));
    assert!(!GENERATED_V1_MANIFEST.contains("kernel-yield"));
    // Admission compares the complete declarative contract, rather than a
    // hand-maintained summary or a lossy fingerprint.  Keep this assertion
    // byte-for-byte so a generated manifest change cannot silently broaden
    // the accepted threaded guest ABI.
    assert_eq!(
        ABI_METADATA_VALUE,
        format!("capabilities=0\noperation_protocol_revision=11\n{GENERATED_V1_MANIFEST}")
            .as_bytes()
    );
}

/// Semantic admission contracts. The default remains the narrow synthetic
/// profile; Rust's standard threaded output is opt-in and versioned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SketchAdmissionProfile {
    SyntheticV1,
    ThreadedRustV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SketchCompilerConfig {
    max_wasm_stack_bytes: usize,
    execution_limits: SketchExecutionLimits,
}
impl SketchCompilerConfig {
    pub fn new(max_wasm_stack_bytes: usize) -> Result<Self, SketchCompilerError> {
        if max_wasm_stack_bytes == 0 {
            return Err(SketchCompilerError::InvalidStackLimit);
        }
        Ok(Self {
            max_wasm_stack_bytes,
            execution_limits: SketchExecutionLimits::default(),
        })
    }
    pub fn max_wasm_stack_bytes(self) -> usize {
        self.max_wasm_stack_bytes
    }
    /// Sets facade-owned aggregate limits for logical sketch sessions.
    pub fn with_execution_limits(
        mut self,
        limits: SketchExecutionLimits,
    ) -> Result<Self, SketchCompilerError> {
        if !limits.is_valid() {
            return Err(SketchCompilerError::InvalidExecutionLimits);
        }
        self.execution_limits = limits;
        Ok(self)
    }
    pub fn execution_limits(self) -> SketchExecutionLimits {
        self.execution_limits
    }
    /// Sets the compiler-global epoch broker policy.
    pub fn with_epoch_limits(
        mut self,
        limits: SketchEpochLimits,
    ) -> Result<Self, SketchCompilerError> {
        if !limits.is_valid() {
            return Err(SketchCompilerError::InvalidEpochLimits);
        }
        self.execution_limits.epoch_limits = limits;
        Ok(self)
    }
}
impl Default for SketchCompilerConfig {
    fn default() -> Self {
        Self {
            max_wasm_stack_bytes: 2 * 1024 * 1024,
            execution_limits: SketchExecutionLimits::default(),
        }
    }
}

/// Facade-owned aggregate bounds for one compiler's logical sketch sessions.
/// These are reservations, not Wasmtime Store or pooling limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SketchExecutionLimits {
    maximum_reserved_shared_memory_bytes: u64,
    maximum_active_root_executions: usize,
    fuel_limits: SketchFuelLimits,
    epoch_limits: SketchEpochLimits,
    blob_limits: SketchBlobLimits,
}
impl SketchExecutionLimits {
    pub fn new(
        maximum_reserved_shared_memory_bytes: u64,
        maximum_active_root_executions: usize,
    ) -> Result<Self, SketchCompilerError> {
        let limits = Self {
            maximum_reserved_shared_memory_bytes,
            maximum_active_root_executions,
            fuel_limits: SketchFuelLimits::default(),
            epoch_limits: SketchEpochLimits::default(),
            blob_limits: SketchBlobLimits::default(),
        };
        limits
            .is_valid()
            .then_some(limits)
            .ok_or(SketchCompilerError::InvalidExecutionLimits)
    }
    pub fn maximum_reserved_shared_memory_bytes(self) -> u64 {
        self.maximum_reserved_shared_memory_bytes
    }
    pub fn maximum_active_root_executions(self) -> usize {
        self.maximum_active_root_executions
    }
    /// Sets the fixed root/child partition for each root execution.
    pub fn with_fuel_limits(
        mut self,
        fuel_limits: SketchFuelLimits,
    ) -> Result<Self, SketchCompilerError> {
        if !fuel_limits.is_valid() {
            return Err(SketchCompilerError::InvalidFuelLimits);
        }
        self.fuel_limits = fuel_limits;
        Ok(self)
    }
    pub fn fuel_limits(self) -> SketchFuelLimits {
        self.fuel_limits
    }
    /// Sets bounded epoch polling for cancellation and wall-clock deadlines.
    pub fn with_epoch_limits(
        mut self,
        epoch_limits: SketchEpochLimits,
    ) -> Result<Self, SketchCompilerError> {
        if !epoch_limits.is_valid() {
            return Err(SketchCompilerError::InvalidEpochLimits);
        }
        self.epoch_limits = epoch_limits;
        Ok(self)
    }
    pub fn epoch_limits(self) -> SketchEpochLimits {
        self.epoch_limits
    }
    /// Sets per-logical-sketch opaque blob and pending-I/O limits.
    pub fn with_blob_limits(mut self, limits: SketchBlobLimits) -> Self {
        self.blob_limits = limits;
        self
    }
    pub fn blob_limits(self) -> SketchBlobLimits {
        self.blob_limits
    }
    fn is_valid(self) -> bool {
        self.maximum_reserved_shared_memory_bytes >= THREADED_RUST_RESERVATION_BYTES
            && self.maximum_active_root_executions != 0
            && self.fuel_limits.is_valid()
            && self.epoch_limits.is_valid()
    }
}
impl Default for SketchExecutionLimits {
    fn default() -> Self {
        Self {
            // One exact Rust 1.95 threaded profile. Callers that want more
            // concurrent logical sketches must explicitly reserve more.
            maximum_reserved_shared_memory_bytes: THREADED_RUST_RESERVATION_BYTES,
            maximum_active_root_executions: 1,
            fuel_limits: SketchFuelLimits::default(),
            epoch_limits: SketchEpochLimits::default(),
            blob_limits: SketchBlobLimits::default(),
        }
    }
}

/// Host-selected bounds for opaque blob storage and pending I/O.
/// Pending input and read-result byte budgets are each bounded by
/// `maximum_sketch_bytes`, with a separate combined hub-capacity limit.
/// Neither limit is a process-memory bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SketchBlobLimits {
    limits: operations::BlobLimits,
}
impl SketchBlobLimits {
    pub fn new(
        maximum_chunk_bytes: usize,
        maximum_blob_bytes: usize,
        maximum_sketch_bytes: usize,
        maximum_live_blobs: usize,
        maximum_pending_reads: usize,
        maximum_pending_writes: usize,
    ) -> Result<Self, SketchCompilerError> {
        let mut limits = operations::BlobLimits::new(
            maximum_chunk_bytes,
            maximum_blob_bytes,
            maximum_sketch_bytes,
        )
        .map_err(|_| SketchCompilerError::InvalidExecutionLimits)?;
        // The generated ABI encodes lengths in u32; do not accept a host
        // policy that requires an unrepresentable bounded request.
        if maximum_chunk_bytes > u32::MAX as usize {
            return Err(SketchCompilerError::InvalidExecutionLimits);
        }
        limits.maximum_live_blobs = maximum_live_blobs;
        limits.maximum_pending_reads = maximum_pending_reads;
        limits.maximum_pending_writes = maximum_pending_writes;
        Ok(Self { limits })
    }
    pub fn maximum_chunk_bytes(self) -> usize {
        self.limits.maximum_chunk_bytes
    }
    pub fn maximum_blob_bytes(self) -> usize {
        self.limits.maximum_blob_bytes
    }
    pub fn maximum_sketch_bytes(self) -> usize {
        self.limits.maximum_sketch_bytes
    }
    pub fn maximum_live_blobs(self) -> usize {
        self.limits.maximum_live_blobs
    }
    pub fn maximum_pending_reads(self) -> usize {
        self.limits.maximum_pending_reads
    }
    pub fn maximum_pending_writes(self) -> usize {
        self.limits.maximum_pending_writes
    }
    /// Bounds combined blob, pending-input, read-result and native-chunk capacity.
    /// One chunk is reserved for pull progress. Native chunks stay charged
    /// until dropped; allocator-internal scratch is not covered by this limit.
    pub fn with_maximum_transfer_bytes(
        mut self,
        maximum: usize,
    ) -> Result<Self, SketchCompilerError> {
        let minimum = self
            .limits
            .maximum_chunk_bytes
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(self.limits.maximum_sketch_bytes))
            .ok_or(SketchCompilerError::InvalidExecutionLimits)?;
        if maximum < minimum {
            return Err(SketchCompilerError::InvalidExecutionLimits);
        }
        self.limits.maximum_transfer_bytes = maximum;
        Ok(self)
    }
    pub fn maximum_transfer_bytes(self) -> usize {
        self.limits.maximum_transfer_bytes
    }
    /// Sets the maximum quiet interval for one pending blob transfer.
    ///
    /// This is reset only by bytes moving through that blob's own bounded
    /// stream; it is independent of the logical sketch wall-clock deadline.
    pub fn with_progress_idle_timeout(
        mut self,
        timeout: Duration,
    ) -> Result<Self, SketchCompilerError> {
        if timeout.is_zero() {
            return Err(SketchCompilerError::InvalidExecutionLimits);
        }
        self.limits.progress_idle_timeout = timeout;
        Ok(self)
    }
    pub fn progress_idle_timeout(self) -> Duration {
        self.limits.progress_idle_timeout
    }
}
impl Default for SketchBlobLimits {
    fn default() -> Self {
        Self::new(64 * 1024, 1024 * 1024, 4 * 1024 * 1024, 128, 128, 128)
            .expect("valid default blob limits")
    }
}

/// Facade-owned epoch polling policy for a logical sketch execution.
///
/// The Wasmtime engine clock is compiler-global; these limits bound only the
/// logical registrations consulted by Store callbacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SketchEpochLimits {
    wall_clock_deadline: Duration,
    tick_interval: Duration,
    maximum_active_registrations: usize,
}
impl SketchEpochLimits {
    pub fn new(
        wall_clock_deadline: Duration,
        tick_interval: Duration,
        maximum_active_registrations: usize,
    ) -> Result<Self, SketchCompilerError> {
        let limits = Self {
            wall_clock_deadline,
            tick_interval,
            maximum_active_registrations,
        };
        limits
            .is_valid()
            .then_some(limits)
            .ok_or(SketchCompilerError::InvalidEpochLimits)
    }
    pub fn wall_clock_deadline(self) -> Duration {
        self.wall_clock_deadline
    }
    pub fn tick_interval(self) -> Duration {
        self.tick_interval
    }
    pub fn maximum_active_registrations(self) -> usize {
        self.maximum_active_registrations
    }
    fn is_valid(self) -> bool {
        !self.wall_clock_deadline.is_zero()
            && !self.tick_interval.is_zero()
            && self.maximum_active_registrations != 0
    }
}
impl Default for SketchEpochLimits {
    fn default() -> Self {
        Self {
            wall_clock_deadline: Duration::from_secs(30),
            tick_interval: Duration::from_millis(10),
            maximum_active_registrations: MAX_GUEST_THREADS_V1 + 1,
        }
    }
}

/// Deterministic fuel partition for one root invocation and its bounded child
/// set. It is a semantic policy, not a Wasmtime type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SketchFuelLimits {
    total: u64,
    root_slice: u64,
    child_slice: u64,
}
impl SketchFuelLimits {
    pub fn new(total: u64, root_slice: u64, child_slice: u64) -> Result<Self, SketchCompilerError> {
        let limits = Self {
            total,
            root_slice,
            child_slice,
        };
        limits
            .is_valid()
            .then_some(limits)
            .ok_or(SketchCompilerError::InvalidFuelLimits)
    }
    pub fn total(self) -> u64 {
        self.total
    }
    pub fn root_slice(self) -> u64 {
        self.root_slice
    }
    pub fn child_slice(self) -> u64 {
        self.child_slice
    }
    fn is_valid(self) -> bool {
        self.root_slice != 0
            && self.child_slice != 0
            && self
                .total
                .checked_sub(self.root_slice)
                .is_some_and(|available| available >= self.child_slice)
    }
    fn child_slice_capacity(self) -> usize {
        // `is_valid` guarantees this is at least one. Cap it independently of
        // caller policy so the session's native registrations remain bounded.
        ((self.total - self.root_slice) / self.child_slice).min(MAX_GUEST_THREADS_V1 as u64)
            as usize
    }
}
impl Default for SketchFuelLimits {
    fn default() -> Self {
        // The exact values are a conservative compatibility default; callers
        // may lower or raise them only as a complete bounded partition.
        Self {
            total: 1_700_000,
            root_slice: 100_000,
            child_slice: 100_000,
        }
    }
}

/// Bounded semantic observations for compiler-owned logical sketch sessions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SketchExecutionSnapshot {
    reserved_shared_memory_bytes: u64,
    active_root_executions: usize,
    live_guest_threads: usize,
    live_stores: usize,
    live_instances: usize,
    active_epoch_registrations: usize,
}
impl SketchExecutionSnapshot {
    pub fn reserved_shared_memory_bytes(self) -> u64 {
        self.reserved_shared_memory_bytes
    }
    pub fn active_root_executions(self) -> usize {
        self.active_root_executions
    }
    pub fn live_guest_threads(self) -> usize {
        self.live_guest_threads
    }
    pub fn live_stores(self) -> usize {
        self.live_stores
    }
    pub fn live_instances(self) -> usize {
        self.live_instances
    }
    pub fn active_epoch_registrations(self) -> usize {
        self.active_epoch_registrations
    }
}

/// Facade-owned compiler for the selected core-Wasm profile.
#[derive(Clone)]
pub struct SketchCompiler {
    engine: Arc<Engine>,
    compilations: Arc<AtomicU64>,
    execution_ledger: Arc<ExecutionLedger>,
    epoch_broker: Arc<EpochBroker>,
    #[cfg(feature = "wasm-sketch-worker")]
    worker_config: SketchCompilerConfig,
    #[cfg(feature = "wasm-sketch-worker")]
    worker_ledger: Arc<worker::WorkerExecutionLedger>,
}
impl SketchCompiler {
    /// Creates a Cranelift, threads, and shared-memory profile. Pooling,
    /// Component Model, Winch, WASI linking, and an executor are absent.
    pub fn new(config: SketchCompilerConfig) -> Result<Self, SketchCompilerError> {
        let mut cfg = Config::new();
        cfg.strategy(Strategy::Cranelift);
        cfg.wasm_threads(true);
        cfg.shared_memory(true);
        cfg.wasm_memory64(false);
        cfg.wasm_multi_memory(false);
        cfg.wasm_shared_everything_threads(false);
        cfg.consume_fuel(true);
        // Epoch is Engine-global. Stores install per-generation callbacks so
        // an increment only interrupts the logical execution whose terminal
        // state has already been chosen by its ticker.
        cfg.epoch_interruption(true);
        cfg.max_wasm_stack(config.max_wasm_stack_bytes);
        let engine = Engine::new(&cfg).map_err(|_| SketchCompilerError::Unavailable)?;
        let engine = Arc::new(engine);
        Ok(Self {
            epoch_broker: Arc::new(EpochBroker::new(
                Arc::clone(&engine),
                config.execution_limits.epoch_limits,
            )),
            engine,
            compilations: Arc::new(AtomicU64::new(0)),
            execution_ledger: Arc::new(ExecutionLedger::new(config.execution_limits)),
            #[cfg(feature = "wasm-sketch-worker")]
            worker_config: config,
            #[cfg(feature = "wasm-sketch-worker")]
            worker_ledger: Arc::new(worker::WorkerExecutionLedger::default()),
        })
    }
    /// Preflights and then compiles one module. Rejected input cannot compile.
    pub fn admit(
        &self,
        bytes: &[u8],
        policy: SketchModulePolicy,
    ) -> Result<Arc<AdmittedSketch>, SketchModuleError> {
        if bytes.len() > policy.max_module_bytes {
            return Err(SketchModuleError::ModuleTooLarge {
                actual_bytes: bytes.len(),
                maximum_bytes: policy.max_module_bytes,
            });
        }
        let memory = match policy.profile {
            SketchAdmissionProfile::SyntheticV1 => preflight(bytes, policy)?,
            SketchAdmissionProfile::ThreadedRustV1 => {
                preflight_threaded_rust(bytes, policy, policy.validation)?
            }
        };
        let module =
            Module::new(&self.engine, bytes).map_err(|_| SketchModuleError::InvalidBinary)?;
        self.compilations.fetch_add(1, Ordering::Relaxed);
        Ok(Arc::new(AdmittedSketch {
            engine: Arc::clone(&self.engine),
            module,
            module_bytes: bytes.len(),
            shared_memory: memory,
            max_guest_threads: policy.max_guest_threads,
            profile: policy.profile,
            validation: policy.validation,
            #[cfg(feature = "wasm-sketch-worker")]
            worker_source: Arc::<[u8]>::from(bytes),
            #[cfg(feature = "wasm-sketch-worker")]
            worker_compiler_config: self.worker_config,
            #[cfg(feature = "wasm-sketch-worker")]
            worker_policy: policy,
            #[cfg(feature = "wasm-sketch-worker")]
            worker_ledger: Arc::clone(&self.worker_ledger),
            execution_ledger: Arc::clone(&self.execution_ledger),
            epoch_broker: Arc::clone(&self.epoch_broker),
            prepared_root: std::sync::Mutex::new(None),
            #[cfg(test)]
            preparation_count: AtomicU64::new(0),
        }))
    }
    /// Number of modules whose complete preflight reached private compilation.
    pub fn compiled_module_count(&self) -> u64 {
        self.compilations.load(Ordering::Relaxed)
    }
    /// Snapshot of aggregate logical resources owned by this compiler.
    pub fn execution_limits_snapshot(&self) -> SketchExecutionSnapshot {
        let mut snapshot = self.execution_ledger.snapshot();
        snapshot.active_epoch_registrations = self.epoch_broker.active_registrations();
        snapshot
    }
    pub fn execution_limits(&self) -> SketchExecutionLimits {
        self.execution_ledger.limits
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SketchModulePolicy {
    max_module_bytes: usize,
    max_shared_memory_pages: u32,
    max_guest_threads: usize,
    profile: SketchAdmissionProfile,
    validation: bool,
}
impl SketchModulePolicy {
    pub fn new(
        max_module_bytes: usize,
        max_shared_memory_pages: u32,
    ) -> Result<Self, SketchModuleError> {
        if max_module_bytes == 0 {
            return Err(SketchModuleError::InvalidModuleLimit);
        }
        if max_shared_memory_pages == 0 {
            return Err(SketchModuleError::InvalidSharedMemoryLimit);
        }
        Ok(Self {
            max_module_bytes,
            max_shared_memory_pages,
            max_guest_threads: DEFAULT_MAX_GUEST_THREADS,
            profile: SketchAdmissionProfile::SyntheticV1,
            validation: false,
        })
    }
    /// Selects the exact Rust 1.95 `wasm32-wasip1-threads` link profile.
    /// `max_shared_memory_pages` is explicit so callers can decline its 1 GiB
    /// (16,384 page) link contract with the usual typed quota failure.
    pub fn threaded_rust_v1(
        max_module_bytes: usize,
        max_shared_memory_pages: u32,
    ) -> Result<Self, SketchModuleError> {
        let mut policy = Self::new(max_module_bytes, max_shared_memory_pages)?;
        policy.profile = SketchAdmissionProfile::ThreadedRustV1;
        Ok(policy)
    }
    /// Crate-only artifact characterization contract. Applications cannot opt
    /// into this wider validation export surface.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn threaded_rust_validation_v1_for_test(
        max_module_bytes: usize,
        max_shared_memory_pages: u32,
    ) -> Result<Self, SketchModuleError> {
        let mut policy = Self::threaded_rust_v1(max_module_bytes, max_shared_memory_pages)?;
        policy.validation = true;
        Ok(policy)
    }
    pub fn max_module_bytes(self) -> usize {
        self.max_module_bytes
    }
    pub fn max_shared_memory_pages(self) -> u32 {
        self.max_shared_memory_pages
    }
    /// Bounds live guest-owned native threads for one admitted sketch. The
    /// limit is a semantic controller quota, not a Wasmtime pooling setting.
    /// V1 accepts at most 16 threads so the native
    /// registrations and bounded semantic child report cannot grow unbounded.
    pub fn with_max_guest_threads(mut self, maximum: usize) -> Result<Self, SketchModuleError> {
        if maximum == 0 {
            return Err(SketchModuleError::InvalidThreadLimit);
        }
        if maximum > MAX_GUEST_THREADS_V1 {
            return Err(SketchModuleError::ThreadLimitExceedsV1Maximum {
                requested: maximum,
                maximum: MAX_GUEST_THREADS_V1,
            });
        }
        self.max_guest_threads = maximum;
        Ok(self)
    }
    pub fn max_guest_threads(self) -> usize {
        self.max_guest_threads
    }
    pub fn profile(self) -> SketchAdmissionProfile {
        self.profile
    }
}

/// An admitted module. The backend object is private to the crate.
pub struct AdmittedSketch {
    engine: Arc<Engine>,
    execution_ledger: Arc<ExecutionLedger>,
    epoch_broker: Arc<EpochBroker>,
    module: Module,
    module_bytes: usize,
    shared_memory: SketchSharedMemory,
    max_guest_threads: usize,
    profile: SketchAdmissionProfile,
    validation: bool,
    #[cfg(feature = "wasm-sketch-worker")]
    #[allow(dead_code)] // Retained for the phase-D worker supervisor.
    worker_source: Arc<[u8]>,
    #[cfg(feature = "wasm-sketch-worker")]
    #[allow(dead_code)] // Retained for the phase-D worker supervisor.
    worker_compiler_config: SketchCompilerConfig,
    #[cfg(feature = "wasm-sketch-worker")]
    #[allow(dead_code)] // Retained for the phase-D worker supervisor.
    worker_policy: SketchModulePolicy,
    #[cfg(feature = "wasm-sketch-worker")]
    worker_ledger: Arc<worker::WorkerExecutionLedger>,
    prepared_root: std::sync::Mutex<Option<Arc<PreparedThreadedRoot>>>,
    // A unit-only observation belongs to the admitted sketch, not the cached
    // controller: it must survive any future controller replacement to prove
    // the cache did not prepare a second private prelink/shared memory.
    #[cfg(test)]
    preparation_count: AtomicU64,
}
impl AdmittedSketch {
    pub fn module_bytes(&self) -> usize {
        self.module_bytes
    }
    pub fn shared_memory(&self) -> SketchSharedMemory {
        self.shared_memory
    }
    /// Aggregate accounting owned by the compiler that admitted this sketch.
    pub fn execution_limits_snapshot(&self) -> SketchExecutionSnapshot {
        let mut snapshot = self.execution_ledger.snapshot();
        snapshot.active_epoch_registrations = self.epoch_broker.active_registrations();
        snapshot
    }
    /// Explicitly releases the cached logical session. Calling this while a
    /// root is executing is rejected; dropping an admitted sketch also
    /// releases its cached session once the last execution reference ends.
    pub fn close_threaded_root(&self) -> Result<(), SketchExecutionError> {
        let mut slot = self
            .prepared_root
            .lock()
            .map_err(|_| SketchExecutionError::PrelinkFailed)?;
        if let Some(prepared) = slot.as_ref() {
            // This lock is shared with root-permit acquisition.  A close either
            // observes an active root and leaves the cached session untouched,
            // or marks the session closed before a racing execution can obtain
            // its permit.  Merely cloning the Arc is not an execution lease.
            let mut session = prepared
                .controller
                .session
                .lock()
                .map_err(|_| SketchExecutionError::PrelinkFailed)?;
            if session.active_roots != 0 {
                return Err(SketchExecutionError::SessionBusy);
            }
            session.closing = true;
        }
        drop(slot.take());
        Ok(())
    }
    /// Instantiates the admitted module in one fresh root store, then invokes
    /// the admitted command ABI's exported `_start` exactly once. The Wasm
    /// start section runs during instantiation; Rust command artifacts use it
    /// only for memory initialization, while `_start` performs constructors,
    /// user entry, and the P1 exit path.
    /// Runs a threaded sketch on the caller-selected runtime's blocking lane.
    /// Cancellation and deadlines are cooperative epoch observations; native
    /// host calls and `atomic.wait` require the process containment boundary
    /// documented in ARCHITECTURE.md and tracked by #28.
    pub async fn execute_threaded_root(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
    ) -> Result<ThreadedRootOutcome, SketchExecutionError> {
        let never_cancelled = crate::async_engine::CancellationSource::new();
        self.execute_threaded_root_cancellable(runtime, never_cancelled.token())
            .await
    }
    /// Runs with a caller-owned cancellation capability.
    pub async fn execute_threaded_root_cancellable(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        cancellation: crate::async_engine::CancellationToken,
    ) -> Result<ThreadedRootOutcome, SketchExecutionError> {
        self.execute_threaded_root_with_grant(runtime, cancellation, RootGrants::default())
            .await
    }

    /// Runs with one exact host-authorized output destination. The guest can
    /// discover only its opaque handle, never the path. Like the other
    /// in-process entry point, this is not a substitute for worker containment.
    pub async fn execute_threaded_root_with_output(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        cancellation: crate::async_engine::CancellationToken,
        destination: std::path::PathBuf,
    ) -> Result<ThreadedRootOutcome, SketchExecutionError> {
        self.execute_threaded_root_with_grant(
            runtime,
            cancellation,
            RootGrants {
                output: Some(destination),
                compiler: None,
                #[cfg(all(test, feature = "archive-auth-test-support"))]
                archive: None,
                #[cfg(feature = "tauri-webview")]
                webview: None,
            },
        )
        .await
    }

    /// Run with host-authorized URL and exact-output capabilities. The UI host
    /// must be driven on its creating thread using the same supplied runtime.
    /// This in-process entry is not a replacement for killable containment.
    #[cfg(feature = "tauri-webview")]
    pub async fn execute_threaded_root_with_webview(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        cancellation: crate::async_engine::CancellationToken,
        client: crate::webview::ExternalWebviewClient,
        url: crate::webview::WebviewUrlGrant,
        destination: std::path::PathBuf,
    ) -> Result<ThreadedRootOutcome, SketchExecutionError> {
        self.execute_threaded_root_with_grant(
            runtime,
            cancellation,
            RootGrants {
                output: Some(destination),
                webview: Some((client, url)),
                compiler: None,
                #[cfg(all(test, feature = "archive-auth-test-support"))]
                archive: None,
            },
        )
        .await
    }

    async fn execute_threaded_root_with_grant(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        cancellation: crate::async_engine::CancellationToken,
        grants: RootGrants,
    ) -> Result<ThreadedRootOutcome, SketchExecutionError> {
        if self.profile != SketchAdmissionProfile::ThreadedRustV1 {
            return Err(SketchExecutionError::ThreadedProfileRequired);
        }
        // Registration precedes every root Store/instance. It retains only
        // facade state and a weak broker entry, never a Store, Instance, or
        // guest memory.
        let registration = self
            .epoch_broker
            .register_root(runtime.clone(), cancellation)?;
        let sketch = Arc::clone(self);
        let logical = registration.logical();
        let blocking_runtime = runtime.clone();
        let blocking = runtime.launch_blocking(move || {
            blocking_runtime.block_on_wasm(sketch.execute_threaded_root_async(
                blocking_runtime.clone(),
                logical,
                grants,
            ))
        });
        let outcome = match blocking.await {
            Ok(outcome) => outcome,
            Err(_) => Err(SketchExecutionError::BlockingTaskFailed),
        };
        // Explicitly remove this Store registration before joining only the
        // ticker generation it made idle.  Keeping the registration alive
        // here would make the ticker wait for itself forever; joining a newer
        // generation would incorrectly couple independent sketches.
        if let Some(ticker) = registration.finish() {
            let _ = ticker.await;
        }
        outcome
    }
    #[expect(
        clippy::too_many_lines,
        reason = "Existing function complexity measured at ratchet adoption"
    )]
    async fn execute_threaded_root_async(
        &self,
        runtime: crate::async_engine::RuntimeHandle,
        logical_epoch: Arc<LogicalEpoch>,
        grants: RootGrants,
    ) -> Result<ThreadedRootOutcome, SketchExecutionError> {
        if self.profile != SketchAdmissionProfile::ThreadedRustV1 {
            return Err(SketchExecutionError::ThreadedProfileRequired);
        }
        let (prepared, root) = self.prepare_threaded_root_with_permit()?;
        if let Ok(mut identity) = prepared.controller.runtime_identity.lock() {
            *identity = Some(runtime.clone());
        }
        let _store_observation = CounterObservation::new(
            Arc::clone(&prepared.controller.execution_ledger),
            LedgerCounter::Stores,
        );
        // A logical root owns exactly one operation/resource authority.  It
        // is shared explicitly with authorized child Stores, never with a
        // cached Store or a different root execution.
        let operations = OperationHub::with_blob_limits(
            MAX_PENDING_OPERATIONS_V1,
            MAX_RESOURCES_V1,
            self.execution_ledger.limits.blob_limits.limits,
        )
        .map_err(|_| SketchExecutionError::PrelinkFailed)?;
        let mut grants = grants;
        let compiler = grants.compiler.take();
        let output = grants.output.take();
        // Grant before constructing or instantiating the root Store: even a
        // module start section sees only the already-authorized resource.
        let initial_output = output
            .as_deref()
            .map(|path| operations.grant_exact_output_wire(0, path))
            .transpose()
            .map_err(|_| SketchExecutionError::OutputGrantRejected)?;
        let operation_cleanup = Arc::clone(&operations);
        let initial_compiler = compiler
            .map(|RootCompilerGrant { spec, deadline }| {
                operations
                    .grant_compiler_with_cache(0, spec, deadline, None)
                    .map(|token| token.wire())
            })
            .transpose()
            .map_err(|_| SketchExecutionError::PrelinkFailed)?;
        #[cfg(all(test, feature = "archive-auth-test-support"))]
        let initial_archive = grants
            .archive
            .map(|input| {
                operations
                    .grant_encrypted_input(0, input)
                    .map(|token| token.wire())
            })
            .transpose()
            .map_err(|_| SketchExecutionError::PrelinkFailed)?;
        #[cfg(feature = "tauri-webview")]
        let (webviews, initial_url) = match grants.webview {
            Some((client, url)) => {
                let context = crate::tauri::sketch::SketchWebviews::new(
                    client,
                    Arc::clone(&operations),
                    &runtime,
                )
                .map_err(|_| SketchExecutionError::WebviewGrantRejected)?;
                let token = url
                    .bind(&operations, 0)
                    .map_err(|_| SketchExecutionError::WebviewGrantRejected)?;
                (Some(context), Some(token.wire()))
            }
            None => (None, None),
        };
        #[cfg(feature = "tauri-webview")]
        let webview_cleanup = webviews.clone();
        let mut store = Store::new(
            &self.engine,
            ThreadStoreState {
                controller: Arc::clone(&prepared.controller),
                runtime: Some(runtime),
                fuel_generation: root.fuel_generation,
                epoch: Arc::clone(&logical_epoch),
                operations,
                store_owner: 0,
                initial_output,
                initial_compiler,
                #[cfg(all(test, feature = "archive-auth-test-support"))]
                initial_archive,
                #[cfg(feature = "tauri-webview")]
                webviews,
                #[cfg(feature = "tauri-webview")]
                initial_url,
            },
        );
        install_epoch_deadline(&mut store);
        // Instantiation itself executes the module start section, so the
        // finite root slice must be installed before `instantiate` as well as
        // before the explicit command `_start` call below.
        // Do not return from this scope before finalization. A Wasm start
        // section can invoke thread-spawn while instantiation is still in
        // progress, and lookup/call failures after that must still close and
        // drain every accepted native child.
        let mut validation_getter = None;
        let mut _instance_observation = None;
        let outcome = async {
            logical_epoch.bind_operations(&operation_cleanup)?;
            if let Some(error) = epoch_error(&logical_epoch) {
                return Err(error);
            }
            if store.set_fuel(root.root_fuel).is_err() {
                Err(SketchExecutionError::PrelinkFailed)
            } else {
                let instance = match prepared.prelink.instantiate_async(&mut store).await {
                    Ok(instance) => instance,
                    Err(error) => return map_root_error(&error, &logical_epoch),
                };
                _instance_observation = Some(CounterObservation::new(
                    Arc::clone(&prepared.controller.execution_ledger),
                    LedgerCounter::Instances,
                ));
                let start = match instance.get_typed_func::<(), ()>(&mut store, "_start") {
                    Ok(start) => start,
                    Err(_) => return Err(SketchExecutionError::Trapped),
                };
                if self.validation {
                    validation_getter = Some(
                        instance
                            .get_typed_func::<(), i32>(&mut store, VALIDATION_REPORT)
                            .map_err(|_| SketchExecutionError::ValidationReportInvalid)?,
                    );
                }
                start
                    .call_async(&mut store, ())
                    .await
                    .map(|_| ThreadedRootOutcome::Started)
                    .or_else(|error| map_root_error(&error, &logical_epoch))
            }
        }
        .await;
        let outcome = if matches!(&outcome, Err(SketchExecutionError::OutOfFuel)) {
            outcome
        } else {
            epoch_error(&logical_epoch).map_or(outcome, Err)
        };
        // Joining happens after the root Store has returned from Wasm and the
        // workers mutex is not held. A child failure is part of the semantic
        // execution result rather than a detached native-thread panic.
        // Revoke before joining: a surviving child may be suspended on an
        // operation whose producer exited with the root. Instantiation/start
        // errors must reach this same finalization path.
        operation_cleanup.close_all(operations::Terminal::Closed);
        let children = prepared.controller.join_completed();
        let output_cleanup = operation_cleanup.join_output_jobs().await;
        let process_cleanup = operation_cleanup.join_process_jobs().await;
        #[cfg(all(test, feature = "archive-auth-test-support"))]
        let archive_cleanup = operation_cleanup.join_archive_jobs().await;
        operation_cleanup.join_clock_jobs().await;
        #[cfg(feature = "tauri-webview")]
        let webview_cleanup = match webview_cleanup {
            Some(context) => context
                .shutdown()
                .await
                .map_err(|_| SketchExecutionError::WebviewCleanupFailed),
            None => Ok(()),
        };
        let rejections = prepared.controller.take_thread_spawn_rejections();
        #[cfg(test)]
        if let Ok(mut snapshot) = prepared.controller.operation_snapshot.lock() {
            *snapshot = Some(operation_cleanup.snapshot());
        }
        // Finalization always drains children, but the root call is the
        // primary operation: its typed failure must not be masked by a
        // concurrent child or report diagnostic.
        let report = if self.validation && outcome.is_ok() && children.is_ok() {
            match validation_getter {
                Some(getter) => getter
                    .call_async(&mut store, ())
                    .await
                    .map_err(|_| SketchExecutionError::ValidationReportInvalid)
                    .and_then(|offset| validate_report(&prepared.controller.memory, offset)),
                None => Err(SketchExecutionError::ValidationReportInvalid),
            }
        } else {
            Ok(())
        };
        prepared
            .controller
            .last_root_remaining_fuel
            .store(store.get_fuel().unwrap_or(0), Ordering::Release);
        let result = resolve_threaded_result(outcome, children, report, rejections)?;
        output_cleanup.map_err(|_| SketchExecutionError::OutputCleanupFailed)?;
        process_cleanup.map_err(|_| SketchExecutionError::BlockingTaskFailed)?;
        #[cfg(all(test, feature = "archive-auth-test-support"))]
        archive_cleanup.map_err(|_| SketchExecutionError::BlockingTaskFailed)?;
        #[cfg(feature = "tauri-webview")]
        webview_cleanup?;
        Ok(result)
    }
    #[expect(
        clippy::too_many_lines,
        reason = "Existing function complexity measured at ratchet adoption"
    )]
    fn prepare_threaded_root_with_permit(
        &self,
    ) -> Result<(Arc<PreparedThreadedRoot>, LogicalRootPermit), SketchExecutionError> {
        let mut prepared = self
            .prepared_root
            .lock()
            .map_err(|_| SketchExecutionError::PrelinkFailed)?;
        if let Some(prepared) = prepared.as_ref() {
            let prepared = Arc::clone(prepared);
            let permit = prepared.controller.acquire_root()?;
            return Ok((prepared, permit));
        }
        // The Rust threaded link profile has a fixed maximum. Reserve its
        // complete address-space contract before asking Wasmtime to create the
        // engine-owned shared memory; committed pages are telemetry only.
        let reservation = self
            .execution_ledger
            .reserve_shared_memory(THREADED_RUST_RESERVATION_BYTES)?;
        let memory = SharedMemory::new(
            &self.engine,
            MemoryType::shared(
                self.shared_memory.minimum_pages,
                self.shared_memory.maximum_pages,
            ),
        )
        .map_err(|_| SketchExecutionError::SharedMemoryUnavailable)?;
        let controller = Arc::new(ThreadController {
            engine: Arc::clone(&self.engine),
            memory,
            prelink: OnceLock::new(),
            workers: Mutex::new(Workers {
                next_tid: 1,
                accepted: 0,
                live: 0,
                closing: false,
                maximum: self.max_guest_threads,
                capacity_rejections: 0,
                closing_rejections: 0,
                fuel_rejections: 0,
                epoch_rejections: 0,
                handles: Vec::new(),
                // At most `maximum` spawns are accepted before the controller
                // closes, so this facade report remains absolutely bounded.
                outcomes: Vec::with_capacity(self.max_guest_threads),
            }),
            kernel_yield_count: AtomicU64::new(0),
            runtime_handle_count: AtomicU64::new(0),
            runtime_identity: Mutex::new(None),
            runtime_identity_mismatches: AtomicU64::new(0),
            last_root_remaining_fuel: AtomicU64::new(0),
            last_child_remaining_fuel: AtomicU64::new(0),
            execution_ledger: Arc::clone(&self.execution_ledger),
            epoch_broker: Arc::clone(&self.epoch_broker),
            session: Mutex::new(SessionState::default()),
            #[cfg(test)]
            threaded_smoke_report: Mutex::new(None),
            #[cfg(test)]
            operation_snapshot: Mutex::new(None),
        });
        let mut linker = Linker::new(&self.engine);
        define_closed_imports(&mut linker)?;

        // SharedMemory is engine-owned, so this bootstrap store is only used to
        // register the import and never owns an instance or crosses a thread.
        let bootstrap = Store::new(
            &self.engine,
            ThreadStoreState {
                controller: Arc::clone(&controller),
                runtime: None,
                fuel_generation: 0,
                epoch: Arc::new(LogicalEpoch {
                    cancellation: crate::async_engine::CancellationSource::new().token(),
                    deadline: Instant::now() + self.epoch_broker.limits.wall_clock_deadline,
                    winner: AtomicU8::new(EPOCH_COMPLETED),
                    operations: Mutex::new(None),
                }),
                operations: OperationHub::new(MAX_PENDING_OPERATIONS_V1, MAX_RESOURCES_V1)
                    .map_err(|_| SketchExecutionError::PrelinkFailed)?,
                store_owner: 0,
                initial_output: None,
                initial_compiler: None,
                #[cfg(all(test, feature = "archive-auth-test-support"))]
                initial_archive: None,
                #[cfg(feature = "tauri-webview")]
                webviews: None,
                #[cfg(feature = "tauri-webview")]
                initial_url: None,
            },
        );
        linker
            .define(
                &bootstrap,
                MEMORY_MODULE,
                MEMORY_NAME,
                controller.memory.clone(),
            )
            .map_err(|_| SketchExecutionError::PrelinkFailed)?;
        let prelink = Arc::new(
            linker
                .instantiate_pre(&self.module)
                .map_err(|_| SketchExecutionError::PrelinkFailed)?,
        );
        controller
            .prelink
            .set(Arc::clone(&prelink))
            .map_err(|_| SketchExecutionError::PrelinkFailed)?;
        let prelink = Arc::clone(
            controller
                .prelink
                .get()
                .ok_or(SketchExecutionError::PrelinkFailed)?,
        );

        let prepared_root = Arc::new(PreparedThreadedRoot {
            controller,
            prelink,
            _reservation: reservation,
        });
        // Do not cache a newly prepared session until it also has a root
        // permit. A concurrent compiler-wide root cap rejection must roll back
        // this new reservation by ordinary RAII rather than parking 1 GiB in
        // an unusable cache entry.
        let permit = prepared_root.controller.acquire_root()?;
        #[cfg(test)]
        self.preparation_count.fetch_add(1, Ordering::Relaxed);
        *prepared = Some(Arc::clone(&prepared_root));
        Ok((prepared_root, permit))
    }
    #[cfg(test)]
    fn root_execution_observation_for_test(&self) -> Option<RootExecutionObservation> {
        let prepared = self.prepared_root.lock().ok()?.as_ref()?.clone();
        let controller = &prepared.controller;
        let workers = controller.workers.lock().ok()?;
        let operation_snapshot = *controller.operation_snapshot.lock().ok()?;
        Some(RootExecutionObservation {
            preparations: self.preparation_count.load(Ordering::Relaxed),
            kernel_yields: controller.kernel_yield_count.load(Ordering::Relaxed),
            supplied_runtime_handles: controller.runtime_handle_count.load(Ordering::Relaxed),
            runtime_identity_mismatches: controller
                .runtime_identity_mismatches
                .load(Ordering::Relaxed),
            last_root_remaining_fuel: controller.last_root_remaining_fuel.load(Ordering::Relaxed),
            last_child_remaining_fuel: controller.last_child_remaining_fuel.load(Ordering::Relaxed),
            accepted_child_registrations: workers.accepted,
            live_threads: workers.live,
            queued_join_handles: workers.handles.len(),
            operation_snapshot,
        })
    }
    #[allow(dead_code)]
    pub(crate) fn compiled_module(&self) -> &Module {
        &self.module
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SketchSharedMemory {
    minimum_pages: u32,
    maximum_pages: u32,
}
impl SketchSharedMemory {
    pub fn minimum_pages(self) -> u32 {
        self.minimum_pages
    }
    pub fn maximum_pages(self) -> u32 {
        self.maximum_pages
    }
    pub fn maximum_bytes(self) -> u64 {
        u64::from(self.maximum_pages) * PAGE_BYTES
    }
}

/// Result of executing the module's root start function.
///
/// This is intentionally semantic: backend stores, instances, and shared
/// memories stay private to the sketch host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadedRootOutcome {
    /// The module start completed without requesting process exit.
    Started,
    /// The root completed, but bounded guest thread-spawn requests were
    /// rejected. The guest ABI receives `-1`; this carries the typed host
    /// accounting without exposing a backend error.
    StartedWithThreadRejections(ThreadSpawnRejectionSummary),
    /// The module requested the normal `proc_exit(0)` completion path.
    Exited,
    /// The root requested normal process exit after bounded thread rejections.
    ExitedWithThreadRejections(ThreadSpawnRejectionSummary),
}

/// Bounded, facade-owned accounting for guest `wasi::thread-spawn` rejections.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThreadSpawnRejectionSummary {
    capacity: u32,
    closing: u32,
    fuel: u32,
    epoch: u32,
}
impl ThreadSpawnRejectionSummary {
    #[cfg(feature = "wasm-sketch-worker")]
    pub(crate) const fn from_worker_counts(
        capacity: u32,
        closing: u32,
        fuel: u32,
        epoch: u32,
    ) -> Self {
        Self {
            capacity,
            closing,
            fuel,
            epoch,
        }
    }
    pub fn capacity(self) -> u32 {
        self.capacity
    }
    pub fn closing(self) -> u32 {
        self.closing
    }
    pub fn fuel(self) -> u32 {
        self.fuel
    }
    pub fn epoch(self) -> u32 {
        self.epoch
    }
    pub fn is_empty(self) -> bool {
        self.capacity == 0 && self.closing == 0 && self.fuel == 0 && self.epoch == 0
    }
}

/// Bounded failures from private threaded-root setup and start execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SketchExecutionError {
    ThreadedProfileRequired,
    SharedMemoryLimitExceeded,
    RootExecutionLimitExceeded,
    SessionBusy,
    OutOfFuel,
    Cancelled,
    DeadlineExceeded,
    EpochRegistrationLimitExceeded,
    ForeignRuntime,
    BlockingTaskFailed,
    /// A host-blocked or atomic-wait guest requires the killable process
    /// containment boundary; an in-process epoch interrupt is insufficient.
    ContainmentRequired,
    SharedMemoryUnavailable,
    PrelinkFailed,
    OutputGrantRejected,
    OutputCleanupFailed,
    WebviewGrantRejected,
    WebviewCleanupFailed,
    NonzeroExit {
        code: i32,
    },
    ChildNonzeroExit {
        code: i32,
    },
    ChildTrapped,
    ChildPanicked,
    ChildOutcomes {
        outcomes: Vec<ThreadedChildOutcome>,
    },
    ValidationReportInvalid,
    SessionGenerationExhausted,
    Trapped,
}
impl SketchExecutionError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ThreadedProfileRequired => "threaded-profile-required",
            Self::SharedMemoryLimitExceeded => "shared-memory-limit-exceeded",
            Self::RootExecutionLimitExceeded => "root-execution-limit-exceeded",
            Self::SessionBusy => "session-busy",
            Self::OutOfFuel => "out-of-fuel",
            Self::Cancelled => "cancelled",
            Self::DeadlineExceeded => "deadline-exceeded",
            Self::EpochRegistrationLimitExceeded => "epoch-registration-limit-exceeded",
            Self::ForeignRuntime => "foreign-runtime",
            Self::BlockingTaskFailed => "blocking-task-failed",
            Self::ContainmentRequired => "containment-required",
            Self::SharedMemoryUnavailable => "shared-memory-unavailable",
            Self::PrelinkFailed => "prelink-failed",
            Self::OutputGrantRejected => "output-grant-rejected",
            Self::OutputCleanupFailed => "output-cleanup-failed",
            Self::WebviewGrantRejected => "webview-grant-rejected",
            Self::WebviewCleanupFailed => "webview-cleanup-failed",
            Self::NonzeroExit { .. } => "nonzero-exit",
            Self::ChildNonzeroExit { .. } => "child-nonzero-exit",
            Self::ChildTrapped => "child-trapped",
            Self::ChildPanicked => "child-panicked",
            Self::ChildOutcomes { .. } => "child-outcomes",
            Self::ValidationReportInvalid => "validation-report-invalid",
            Self::SessionGenerationExhausted => "session-generation-exhausted",
            Self::Trapped => "trapped",
        }
    }
}
impl fmt::Display for SketchExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "threaded sketch execution failed ({})", self.code())
    }
}
impl std::error::Error for SketchExecutionError {}

/// Private compiler-owned accounting. It deliberately has no relationship to
/// a Store limiter: shared memory belongs to the Engine and is visible to all
/// per-thread stores in a logical sketch session.
struct ExecutionLedger {
    limits: SketchExecutionLimits,
    state: Mutex<ExecutionLedgerState>,
}

#[derive(Default)]
struct ExecutionLedgerState {
    reserved_shared_memory_bytes: u64,
    active_root_executions: usize,
    live_guest_threads: usize,
    live_stores: usize,
    live_instances: usize,
}
impl ExecutionLedger {
    fn new(limits: SketchExecutionLimits) -> Self {
        Self {
            limits,
            state: Mutex::new(ExecutionLedgerState::default()),
        }
    }
    fn snapshot(&self) -> SketchExecutionSnapshot {
        let state = self.state.lock().expect("execution ledger mutex poisoned");
        SketchExecutionSnapshot {
            reserved_shared_memory_bytes: state.reserved_shared_memory_bytes,
            active_root_executions: state.active_root_executions,
            live_guest_threads: state.live_guest_threads,
            live_stores: state.live_stores,
            live_instances: state.live_instances,
            active_epoch_registrations: 0,
        }
    }
    fn reserve_shared_memory(
        self: &Arc<Self>,
        bytes: u64,
    ) -> Result<SharedMemoryReservation, SketchExecutionError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SketchExecutionError::PrelinkFailed)?;
        let Some(next) = state.reserved_shared_memory_bytes.checked_add(bytes) else {
            return Err(SketchExecutionError::SharedMemoryLimitExceeded);
        };
        if next > self.limits.maximum_reserved_shared_memory_bytes {
            return Err(SketchExecutionError::SharedMemoryLimitExceeded);
        }
        state.reserved_shared_memory_bytes = next;
        Ok(SharedMemoryReservation {
            ledger: Arc::clone(self),
            bytes,
        })
    }
    fn acquire_root(self: &Arc<Self>) -> Result<RootExecutionPermit, SketchExecutionError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SketchExecutionError::PrelinkFailed)?;
        if state.active_root_executions >= self.limits.maximum_active_root_executions {
            return Err(SketchExecutionError::RootExecutionLimitExceeded);
        }
        state.active_root_executions += 1;
        Ok(RootExecutionPermit {
            ledger: Arc::clone(self),
        })
    }
    fn increment(&self, counter: LedgerCounter) {
        let mut state = self.state.lock().expect("execution ledger mutex poisoned");
        let value = counter.get_mut(&mut state);
        *value = value.saturating_add(1);
    }
    fn decrement(&self, counter: LedgerCounter) {
        let mut state = self.state.lock().expect("execution ledger mutex poisoned");
        let value = counter.get_mut(&mut state);
        debug_assert_ne!(*value, 0, "execution ledger counter underflow");
        *value = value.saturating_sub(1);
    }
}

#[derive(Clone, Copy)]
enum LedgerCounter {
    GuestThreads,
    Stores,
    Instances,
}
impl LedgerCounter {
    fn get_mut(self, state: &mut ExecutionLedgerState) -> &mut usize {
        match self {
            Self::GuestThreads => &mut state.live_guest_threads,
            Self::Stores => &mut state.live_stores,
            Self::Instances => &mut state.live_instances,
        }
    }
}

struct SharedMemoryReservation {
    ledger: Arc<ExecutionLedger>,
    bytes: u64,
}
impl Drop for SharedMemoryReservation {
    fn drop(&mut self) {
        let mut state = self
            .ledger
            .state
            .lock()
            .expect("execution ledger mutex poisoned");
        debug_assert!(
            state.reserved_shared_memory_bytes >= self.bytes,
            "shared-memory reservation underflow"
        );
        state.reserved_shared_memory_bytes = state
            .reserved_shared_memory_bytes
            .saturating_sub(self.bytes);
    }
}

struct RootExecutionPermit {
    ledger: Arc<ExecutionLedger>,
}

struct CounterObservation {
    ledger: Arc<ExecutionLedger>,
    counter: LedgerCounter,
}
impl CounterObservation {
    fn new(ledger: Arc<ExecutionLedger>, counter: LedgerCounter) -> Self {
        ledger.increment(counter);
        Self { ledger, counter }
    }
}
impl Drop for CounterObservation {
    fn drop(&mut self) {
        self.ledger.decrement(self.counter);
    }
}
impl Drop for RootExecutionPermit {
    fn drop(&mut self) {
        let mut state = self
            .ledger
            .state
            .lock()
            .expect("execution ledger mutex poisoned");
        debug_assert_ne!(state.active_root_executions, 0, "root permit underflow");
        state.active_root_executions = state.active_root_executions.saturating_sub(1);
    }
}

#[cfg(test)]
mod execution_ledger_tests {
    use super::*;

    #[test]
    fn blob_limits_validate_and_survive_compiler_configuration() {
        for (chunk, blob, sketch) in [(0, 4, 8), (8, 4, 8), (4, 8, 4)] {
            assert!(SketchBlobLimits::new(chunk, blob, sketch, 1, 1, 1).is_err());
        }
        // Zero count limits intentionally disable the corresponding admission.
        let blobs = SketchBlobLimits::new(4, 8, 16, 0, 2, 3).unwrap();
        assert!(blobs.with_maximum_transfer_bytes(23).is_err());
        assert_eq!(
            blobs
                .with_maximum_transfer_bytes(24)
                .unwrap()
                .maximum_transfer_bytes(),
            24
        );
        assert!(blobs.with_progress_idle_timeout(Duration::ZERO).is_err());
        assert_eq!(
            blobs
                .with_progress_idle_timeout(Duration::from_millis(7))
                .unwrap()
                .progress_idle_timeout(),
            Duration::from_millis(7)
        );
        let compiler = SketchCompiler::new(
            SketchCompilerConfig::default()
                .with_execution_limits(SketchExecutionLimits::default().with_blob_limits(blobs))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(compiler.execution_limits().blob_limits(), blobs);
    }

    #[test]
    fn fuel_limits_require_a_complete_nonzero_root_and_child_partition() {
        assert_eq!(
            SketchFuelLimits::new(1, 1, 1),
            Err(SketchCompilerError::InvalidFuelLimits),
        );
        assert_eq!(
            SketchFuelLimits::new(2, 1, 1),
            Ok(SketchFuelLimits {
                total: 2,
                root_slice: 1,
                child_slice: 1,
            }),
        );
        assert_eq!(
            SketchFuelLimits::new(1_700_000, 0, 100_000),
            Err(SketchCompilerError::InvalidFuelLimits),
        );
        assert_eq!(
            SketchFuelLimits::new(1_700_000, 100_000, 0),
            Err(SketchCompilerError::InvalidFuelLimits),
        );
        assert_eq!(
            SketchFuelLimits::new(1_700_000, 100_000, 100_000),
            Ok(SketchFuelLimits::default()),
        );
    }

    #[test]
    fn execution_limits_reject_an_unreservable_profile_and_zero_root_permits() {
        assert_eq!(
            SketchExecutionLimits::new(THREADED_RUST_RESERVATION_BYTES - 1, 1),
            Err(SketchCompilerError::InvalidExecutionLimits)
        );
        assert_eq!(
            SketchExecutionLimits::new(THREADED_RUST_RESERVATION_BYTES, 0),
            Err(SketchCompilerError::InvalidExecutionLimits)
        );
    }

    #[test]
    fn reservation_is_exact_bounded_and_released_on_drop() {
        let ledger = Arc::new(ExecutionLedger::new(
            SketchExecutionLimits::new(THREADED_RUST_RESERVATION_BYTES, 1).expect("limits"),
        ));
        let reservation = ledger
            .reserve_shared_memory(THREADED_RUST_RESERVATION_BYTES)
            .expect("exact reservation");
        assert_eq!(
            ledger.snapshot().reserved_shared_memory_bytes(),
            THREADED_RUST_RESERVATION_BYTES
        );
        assert!(matches!(
            ledger.reserve_shared_memory(1),
            Err(SketchExecutionError::SharedMemoryLimitExceeded)
        ));
        drop(reservation);
        assert_eq!(ledger.snapshot(), SketchExecutionSnapshot::default());
    }

    #[test]
    fn root_permit_and_all_counter_observations_balance_on_unwind() {
        let ledger = Arc::new(ExecutionLedger::new(SketchExecutionLimits::default()));
        let result = std::panic::catch_unwind({
            let ledger = Arc::clone(&ledger);
            move || {
                let _root = ledger.acquire_root().expect("root permit");
                let _thread =
                    CounterObservation::new(Arc::clone(&ledger), LedgerCounter::GuestThreads);
                let _store = CounterObservation::new(Arc::clone(&ledger), LedgerCounter::Stores);
                let _instance = CounterObservation::new(ledger, LedgerCounter::Instances);
                panic!("simulated root failure");
            }
        });
        assert!(result.is_err());
        assert_eq!(ledger.snapshot(), SketchExecutionSnapshot::default());
    }

    #[test]
    fn concurrent_reservations_admit_exactly_one_default_threaded_session() {
        let ledger = Arc::new(ExecutionLedger::new(SketchExecutionLimits::default()));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let ledger = Arc::clone(&ledger);
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                ledger.reserve_shared_memory(THREADED_RUST_RESERVATION_BYTES)
            }));
        }
        barrier.wait();
        let mut reservations = Vec::new();
        let mut rejected = 0;
        for worker in workers {
            match worker.join().expect("worker") {
                Ok(reservation) => reservations.push(reservation),
                Err(SketchExecutionError::SharedMemoryLimitExceeded) => rejected += 1,
                Err(error) => panic!("unexpected reservation error: {}", error.code()),
            }
        }
        assert_eq!(reservations.len(), 1);
        assert_eq!(rejected, 1);
        drop(reservations);
        assert_eq!(ledger.snapshot(), SketchExecutionSnapshot::default());
    }
}

struct ThreadController {
    engine: Arc<Engine>,
    memory: SharedMemory,
    // Callbacks reach their controller through ThreadStoreState. Keeping the
    // prelink here breaks their otherwise cyclic construction without letting a
    // Store or Instance escape to another thread.
    prelink: OnceLock<Arc<InstancePre<ThreadStoreState>>>,
    kernel_yield_count: AtomicU64,
    runtime_handle_count: AtomicU64,
    runtime_identity: Mutex<Option<crate::async_engine::RuntimeHandle>>,
    runtime_identity_mismatches: AtomicU64,
    // Bounded private terminal telemetry. No Store or raw trap crosses the
    // facade; #42 only needs to retain the last execution's accounting.
    last_root_remaining_fuel: AtomicU64,
    last_child_remaining_fuel: AtomicU64,
    execution_ledger: Arc<ExecutionLedger>,
    epoch_broker: Arc<EpochBroker>,
    session: Mutex<SessionState>,
    workers: Mutex<Workers>,
    #[cfg(test)]
    threaded_smoke_report: Mutex<Option<[u32; 12]>>,
    #[cfg(test)]
    operation_snapshot: Mutex<Option<operations::HubSnapshot>>,
}

#[derive(Default)]
struct SessionState {
    active_roots: usize,
    closing: bool,
    next_fuel_generation: u64,
    fuel: Option<FuelExecution>,
}
struct FuelExecution {
    generation: u64,
    remaining_child_slices: usize,
    child_slice: u64,
}
struct Workers {
    next_tid: i32,
    // Total child registrations in the current root execution. Unlike `live`,
    // this never decreases until drain, so a fast-completing guest cannot grow
    // the bounded facade outcome vector by repeatedly spawning replacements.
    accepted: usize,
    live: usize,
    closing: bool,
    maximum: usize,
    capacity_rejections: u32,
    closing_rejections: u32,
    fuel_rejections: u32,
    epoch_rejections: u32,
    handles: Vec<JoinHandle<()>>,
    // Completion is inherently concurrent; retain the guest TID so reporting
    // is deterministic rather than depending on native scheduler order.
    outcomes: Vec<(i32, ChildOutcome)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChildOutcome {
    Completed,
    Cancelled,
    DeadlineExceeded,
    Exited,
    NonzeroExit(i32),
    OutOfFuel,
    Trapped,
    Panicked,
}

/// One cooperatively joined guest child outcome. This facade type contains no
/// native thread, Store, trap, or backend diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadedChildOutcome {
    pub tid: i32,
    pub kind: ThreadedChildOutcomeKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadedChildOutcomeKind {
    Completed,
    Cancelled,
    DeadlineExceeded,
    Exited,
    NonzeroExit { code: i32 },
    OutOfFuel,
    Trapped,
    Panicked,
}
impl ThreadController {
    fn acquire_root(self: &Arc<Self>) -> Result<LogicalRootPermit, SketchExecutionError> {
        let mut session = self
            .session
            .lock()
            .map_err(|_| SketchExecutionError::PrelinkFailed)?;
        if session.closing || session.active_roots != 0 {
            return Err(SketchExecutionError::SessionBusy);
        }
        let permit = self.execution_ledger.acquire_root()?;
        session.active_roots += 1;
        let fuel = self.execution_ledger.limits.fuel_limits;
        let generation = session
            .next_fuel_generation
            .checked_add(1)
            .ok_or(SketchExecutionError::SessionGenerationExhausted)?;
        session.next_fuel_generation = generation;
        session.fuel = Some(FuelExecution {
            generation,
            remaining_child_slices: fuel.child_slice_capacity(),
            child_slice: fuel.child_slice,
        });
        Ok(LogicalRootPermit {
            controller: Arc::clone(self),
            _permit: permit,
            root_fuel: fuel.root_slice,
            fuel_generation: generation,
        })
    }
    fn reserve_child_fuel(&self, generation: u64) -> Option<u64> {
        let mut session = self.session.lock().ok()?;
        let fuel = session.fuel.as_mut()?;
        if fuel.generation != generation {
            return None;
        }
        if fuel.remaining_child_slices == 0 {
            return None;
        }
        fuel.remaining_child_slices -= 1;
        Some(fuel.child_slice)
    }
    fn refund_child_fuel(&self) {
        if let Ok(mut session) = self.session.lock() {
            if let Some(fuel) = session.fuel.as_mut() {
                fuel.remaining_child_slices = fuel
                    .remaining_child_slices
                    .saturating_add(1)
                    .min(MAX_GUEST_THREADS_V1);
            }
        }
    }
    fn join_completed(&self) -> Result<(), SketchExecutionError> {
        // Mark closing before taking the queue. This prevents a child that is
        // still running from registering another native worker after the final
        // drain begins; no mutex remains held across `join` or guest Wasm.
        loop {
            let handles = self
                .workers
                .lock()
                .map_err(|_| SketchExecutionError::ChildPanicked)
                .map(|mut w| {
                    w.closing = true;
                    std::mem::take(&mut w.handles)
                })?;
            if handles.is_empty() {
                break;
            }
            for handle in handles {
                if handle.join().is_err() {
                    let mut workers = self
                        .workers
                        .lock()
                        .map_err(|_| SketchExecutionError::ChildPanicked)?;
                    if workers.outcomes.len() < workers.maximum {
                        workers.outcomes.push((i32::MAX, ChildOutcome::Panicked));
                    }
                }
            }
        }
        let mut workers = self
            .workers
            .lock()
            .map_err(|_| SketchExecutionError::ChildPanicked)?;
        let mut outcomes: Vec<_> = std::mem::take(&mut workers.outcomes);
        workers.accepted = 0;
        outcomes.sort_by_key(|(tid, _)| *tid);
        let report: Vec<_> = outcomes
            .into_iter()
            .map(|(tid, outcome)| ThreadedChildOutcome {
                tid,
                kind: match outcome {
                    ChildOutcome::Completed => ThreadedChildOutcomeKind::Completed,
                    ChildOutcome::Cancelled => ThreadedChildOutcomeKind::Cancelled,
                    ChildOutcome::DeadlineExceeded => ThreadedChildOutcomeKind::DeadlineExceeded,
                    ChildOutcome::Exited => ThreadedChildOutcomeKind::Exited,
                    ChildOutcome::NonzeroExit(code) => {
                        ThreadedChildOutcomeKind::NonzeroExit { code }
                    }
                    ChildOutcome::OutOfFuel => ThreadedChildOutcomeKind::OutOfFuel,
                    ChildOutcome::Trapped => ThreadedChildOutcomeKind::Trapped,
                    ChildOutcome::Panicked => ThreadedChildOutcomeKind::Panicked,
                },
            })
            .collect();
        workers.closing = false;
        if report.iter().all(|outcome| {
            matches!(
                outcome.kind,
                ThreadedChildOutcomeKind::Completed | ThreadedChildOutcomeKind::Exited
            )
        }) {
            Ok(())
        } else {
            Err(SketchExecutionError::ChildOutcomes { outcomes: report })
        }
    }

    fn take_thread_spawn_rejections(&self) -> ThreadSpawnRejectionSummary {
        let Ok(mut workers) = self.workers.lock() else {
            return ThreadSpawnRejectionSummary::default();
        };
        let summary = ThreadSpawnRejectionSummary {
            capacity: workers.capacity_rejections,
            closing: workers.closing_rejections,
            fuel: workers.fuel_rejections,
            epoch: workers.epoch_rejections,
        };
        workers.capacity_rejections = 0;
        workers.closing_rejections = 0;
        workers.fuel_rejections = 0;
        workers.epoch_rejections = 0;
        summary
    }
}

struct PreparedThreadedRoot {
    controller: Arc<ThreadController>,
    prelink: Arc<InstancePre<ThreadStoreState>>,
    _reservation: SharedMemoryReservation,
}

struct LogicalRootPermit {
    controller: Arc<ThreadController>,
    _permit: RootExecutionPermit,
    root_fuel: u64,
    fuel_generation: u64,
}
impl Drop for LogicalRootPermit {
    fn drop(&mut self) {
        let mut session = self
            .controller
            .session
            .lock()
            .expect("thread controller session mutex poisoned");
        debug_assert_ne!(session.active_roots, 0, "logical root permit underflow");
        session.active_roots = session.active_roots.saturating_sub(1);
        session.fuel = None;
    }
}

#[derive(Default)]
struct RootGrants {
    compiler: Option<RootCompilerGrant>,
    output: Option<std::path::PathBuf>,
    #[cfg(all(test, feature = "archive-auth-test-support"))]
    archive: Option<crate::operations::archive_input::EncryptedInput>,
    #[cfg(feature = "tauri-webview")]
    webview: Option<(
        crate::webview::ExternalWebviewClient,
        crate::webview::WebviewUrlGrant,
    )>,
}

struct RootCompilerGrant {
    spec: crate::SpawnSpec,
    deadline: std::time::Duration,
}

struct ThreadStoreState {
    controller: Arc<ThreadController>,
    runtime: Option<crate::async_engine::RuntimeHandle>,
    fuel_generation: u64,
    // The callback owns only an Arc to facade state. The broker retains weak
    // entries, never a Store, Instance, Caller, or guest memory.
    epoch: Arc<LogicalEpoch>,
    operations: Arc<OperationHub>,
    store_owner: u64,
    initial_output: Option<u64>,
    initial_compiler: Option<u64>,
    #[cfg(all(test, feature = "archive-auth-test-support"))]
    initial_archive: Option<u64>,
    #[cfg(feature = "tauri-webview")]
    initial_url: Option<u64>,
    #[cfg(feature = "tauri-webview")]
    webviews: Option<Arc<crate::tauri::sketch::SketchWebviews>>,
}

fn read_blob_stream_chunk(
    operations: &OperationHub,
    store_owner: u64,
    stream: u64,
    destination: &mut [u8],
) -> Result<usize, operations::HubError> {
    // Keep the quota-accounted native chunk scoped to this host callback. Its
    // Drop releases capacity before a subsequent guest call can observe it.
    let copied = {
        let chunk = operations.read_blob_chunk(
            store_owner,
            crate::operations::OpaqueToken::from_wire(stream),
            destination.len().min(operations.maximum_blob_chunk_bytes()),
            false,
        )?;
        let copied = chunk.len();
        destination[..copied].copy_from_slice(&chunk);
        copied
    };
    Ok(copied)
}

fn write_blob_stream_chunk(
    operations: &OperationHub,
    store_owner: u64,
    stream: u64,
    source: &[u8],
) -> Result<usize, operations::HubError> {
    let source = &source[..source.len().min(operations.maximum_blob_chunk_bytes())];
    operations.blob_write(
        store_owner,
        crate::operations::OpaqueToken::from_wire(stream),
        source,
    )
}

impl generated_v1::KernalApiV1Imports for ThreadStoreState {
    fn kernel_yield(&mut self) -> wasmtime::Result<()> {
        // The facade handle is deliberately supplied by the caller. This
        // scalar slice has no scheduler yet, so it must never construct one.
        let controller = &self.controller;
        controller
            .kernel_yield_count
            .fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        if let Some(marker) = std::env::var_os("KERNAL_API_EPOCH_HOST_BLOCK_MARKER") {
            // This test-only seam characterizes the exact generated host
            // boundary, after which an in-process epoch interrupt cannot
            // reclaim a host-blocked thread.
            use std::io::Write as _;
            if let Ok(mut file) = std::fs::File::create(marker) {
                let _ = file.write_all(b"entered");
                let _ = file.sync_all();
            }
            std::thread::park();
        }
        #[cfg(test)]
        if let Some(marker) = std::env::var_os("KERNAL_API_EPOCH_ATOMIC_WAIT_MARKER") {
            use std::io::Write as _;
            if let Ok(mut file) = std::fs::File::create(marker) {
                let _ = file.write_all(b"entered");
                let _ = file.sync_all();
            }
        }
        if let Some(actual) = &self.runtime {
            controller
                .runtime_handle_count
                .fetch_add(1, Ordering::Relaxed);
            let matches = controller
                .runtime_identity
                .lock()
                .ok()
                .and_then(|identity| identity.as_ref().cloned())
                .is_none_or(|expected| actual.same_runtime_for_wasm(&expected));
            if !matches {
                controller
                    .runtime_identity_mismatches
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        let _ = controller.prelink.get();
        Ok(())
    }

    fn stream_read(
        &mut self,
        stream: u64,
        destination: &mut [u8],
    ) -> wasmtime::Result<generated_v1::StreamTransfer> {
        // The generated linker validates the entire guest destination before
        // reaching this method.
        Ok(
            read_blob_stream_chunk(&self.operations, self.store_owner, stream, destination)
                .map(generated_v1::StreamTransfer::Transferred)
                .unwrap_or(generated_v1::StreamTransfer::Rejected),
        )
    }

    fn stream_write(
        &mut self,
        stream: u64,
        source: &[u8],
    ) -> wasmtime::Result<generated_v1::StreamTransfer> {
        // `source` is a bounded local copy made by the generated linker; no
        // guest pointer or guest-memory borrow reaches OperationHub.
        Ok(
            write_blob_stream_chunk(&self.operations, self.store_owner, stream, source)
                .map(generated_v1::StreamTransfer::Transferred)
                .unwrap_or(generated_v1::StreamTransfer::Rejected),
        )
    }

    fn stream_close(&mut self, stream: u64) -> wasmtime::Result<i32> {
        // Closing follows the same atomic Store-scoped kind, owner, and
        // generation validation as the legacy Blob abandon opcode.
        Ok(
            match self.operations.abandon_blob_wire(self.store_owner, stream) {
                Ok(()) => 0,
                Err(_) => 1,
            },
        )
    }

    fn resource_release_encrypted_archive(
        &mut self,
        archive: generated_v1::resources::EncryptedArchive,
    ) -> wasmtime::Result<i32> {
        #[cfg(all(test, feature = "archive-auth-test-support"))]
        {
            Ok(
                match self
                    .operations
                    .abandon_encrypted_input(self.store_owner, archive.0)
                {
                    Ok(()) => 0,
                    Err(_) => 1,
                },
            )
        }
        #[cfg(not(all(test, feature = "archive-auth-test-support")))]
        {
            let _ = archive;
            Ok(1)
        }
    }

    fn resource_release_authenticated_archive(
        &mut self,
        archive: generated_v1::resources::AuthenticatedArchive,
    ) -> wasmtime::Result<i32> {
        #[cfg(all(test, feature = "archive-auth-test-support"))]
        {
            Ok(
                match self
                    .operations
                    .abandon_authenticated_archive(self.store_owner, archive.0)
                {
                    Ok(()) => 0,
                    Err(_) => 1,
                },
            )
        }
        #[cfg(not(all(test, feature = "archive-auth-test-support")))]
        {
            let _ = archive;
            Ok(1)
        }
    }

    fn resource_release_archive_entry(
        &mut self,
        entry: generated_v1::resources::ArchiveEntry,
    ) -> wasmtime::Result<i32> {
        #[cfg(all(test, feature = "archive-auth-test-support"))]
        {
            Ok(
                match self
                    .operations
                    .abandon_archive_entry(self.store_owner, entry.0)
                {
                    Ok(()) => 0,
                    Err(_) => 1,
                },
            )
        }
        #[cfg(not(all(test, feature = "archive-auth-test-support")))]
        {
            let _ = entry;
            Ok(1)
        }
    }

    #[cfg_attr(
        test,
        expect(
            clippy::cognitive_complexity,
            reason = "Existing test dispatch complexity"
        )
    )]
    #[expect(clippy::too_many_lines, reason = "Existing dispatch line count")]
    fn operation_submit(&mut self, kind: u32, arg0: u64, arg1: u64) -> wasmtime::Result<u64> {
        if let Some(result) = compiler_dispatch::dispatch(self, kind, arg0, arg1) {
            return Ok(result);
        }
        if let Some(result) = hash_dispatch::dispatch(
            &self.operations,
            self.store_owner,
            &self.controller.memory,
            kind,
            arg0,
            arg1,
        ) {
            return Ok(result);
        }
        if kind == crate::operations::OP_ARCHIVE_NEXT_ENTRY {
            #[cfg(all(test, feature = "archive-auth-test-support"))]
            {
                if arg1 != 0 {
                    return Ok(0);
                }
                let Some(runtime) = self.runtime.clone() else {
                    return Ok(0);
                };
                return Ok(self
                    .operations
                    .submit_archive_next_entry(
                        runtime,
                        self.store_owner,
                        crate::operations::OpaqueToken::from_wire(arg0),
                    )
                    .map(|token| token.wire())
                    .unwrap_or(0));
            }
            #[cfg(not(all(test, feature = "archive-auth-test-support")))]
            return Ok(0);
        }
        if kind == crate::operations::OP_ARCHIVE_ENTRY_METADATA {
            #[cfg(all(test, feature = "archive-auth-test-support"))]
            {
                let capacity = (arg1 >> 32) as usize;
                let Some(cells) =
                    shared_range(&self.controller.memory, arg1 as u32 as i32, capacity)
                else {
                    return Ok(0x80);
                };
                return Ok(self
                    .operations
                    .read_archive_entry_metadata(self.store_owner, arg0, capacity, |bytes| {
                        for (cell, byte) in cells.iter().zip(bytes) {
                            // SAFETY: shared_range validates the shared byte cells;
                            // atomic stores remain safe under concurrent guest access.
                            unsafe { AtomicU8::from_ptr(cell.get()) }
                                .store(*byte, Ordering::Relaxed);
                        }
                    })
                    .map(|count| (count as u64) << 8 | 1)
                    .unwrap_or(0x80));
            }
            #[cfg(not(all(test, feature = "archive-auth-test-support")))]
            return Ok(0x80);
        }
        if kind == crate::operations::OP_ARCHIVE_ENTRY_OPEN {
            #[cfg(all(test, feature = "archive-auth-test-support"))]
            {
                if arg1 != 0 {
                    return Ok(0);
                }
                let Some(runtime) = self.runtime.clone() else {
                    return Ok(0);
                };
                return Ok(self
                    .operations
                    .submit_archive_entry_open(
                        runtime,
                        self.store_owner,
                        crate::operations::OpaqueToken::from_wire(arg0),
                    )
                    .map(|token| token.wire())
                    .unwrap_or(0));
            }
            #[cfg(not(all(test, feature = "archive-auth-test-support")))]
            return Ok(0);
        }
        if kind == crate::operations::OP_ARCHIVE_ENTRY_ABANDON {
            #[cfg(all(test, feature = "archive-auth-test-support"))]
            return Ok(u64::from(
                arg1 == 0
                    && self
                        .operations
                        .abandon_archive_entry(self.store_owner, arg0)
                        .is_ok(),
            ));
            #[cfg(not(all(test, feature = "archive-auth-test-support")))]
            return Ok(0);
        }
        if kind == crate::operations::OP_ENCRYPTED_INPUT_AUTHENTICATE {
            #[cfg(all(test, feature = "archive-auth-test-support"))]
            {
                let Ok(offset) = i32::try_from(arg1) else {
                    return Ok(0);
                };
                let Some(cells) = shared_range(&self.controller.memory, offset, 12) else {
                    return Ok(0);
                };
                let mut nonce = [0; 12];
                for (byte, cell) in nonce.iter_mut().zip(cells) {
                    // SAFETY: the validated shared range owns these byte cells;
                    // atomic loads avoid racing guest threads during the copy.
                    *byte = unsafe { AtomicU8::from_ptr(cell.get()) }.load(Ordering::Relaxed);
                }
                let Some(runtime) = self.runtime.clone() else {
                    return Ok(0);
                };
                return Ok(self
                    .operations
                    .submit_encrypted_authentication(
                        runtime,
                        self.store_owner,
                        crate::operations::OpaqueToken::from_wire(arg0),
                        nonce,
                    )
                    .map(|token| token.wire())
                    .unwrap_or(0));
            }
            #[cfg(not(all(test, feature = "archive-auth-test-support")))]
            return Ok(0);
        }
        if kind == crate::operations::OP_ARCHIVE_AUTHENTICATION_ABANDON {
            #[cfg(all(test, feature = "archive-auth-test-support"))]
            return Ok(u64::from(
                arg1 == 0
                    && self
                        .operations
                        .abandon_archive_authentication(self.store_owner, arg0)
                        .is_ok(),
            ));
            #[cfg(not(all(test, feature = "archive-auth-test-support")))]
            return Ok(0);
        }
        if kind == crate::operations::OP_AUTHENTICATED_ARCHIVE_ABANDON {
            #[cfg(all(test, feature = "archive-auth-test-support"))]
            return Ok(u64::from(
                arg1 == 0
                    && self
                        .operations
                        .abandon_authenticated_archive(self.store_owner, arg0)
                        .is_ok(),
            ));
            #[cfg(not(all(test, feature = "archive-auth-test-support")))]
            return Ok(0);
        }
        if kind == crate::operations::OP_ENCRYPTED_INPUT_GRANT {
            if arg0 != 0 || arg1 != 0 {
                return Ok(0);
            }
            #[cfg(all(test, feature = "archive-auth-test-support"))]
            let granted = self.initial_archive.take().unwrap_or(0);
            #[cfg(not(all(test, feature = "archive-auth-test-support")))]
            let granted = 0;
            return Ok(granted);
        }
        if kind == crate::operations::OP_ENCRYPTED_INPUT_HEADER {
            #[cfg(all(test, feature = "archive-auth-test-support"))]
            {
                let offset = arg1 as u32 as i32;
                let capacity = (arg1 >> 32) as usize;
                let Some(cells) = shared_range(&self.controller.memory, offset, capacity) else {
                    return Ok(0x80);
                };
                return Ok(self
                    .operations
                    .read_encrypted_header(self.store_owner, arg0, capacity, |bytes| {
                        for (cell, byte) in cells.iter().zip(bytes) {
                            // SAFETY: shared_range pins the shared byte cells;
                            // host accesses shared guest memory atomically.
                            unsafe { AtomicU8::from_ptr(cell.get()) }
                                .store(*byte, Ordering::Relaxed);
                        }
                    })
                    .map(|count| (count as u64) << 8 | 1)
                    .unwrap_or(0x80));
            }
            #[cfg(not(all(test, feature = "archive-auth-test-support")))]
            return Ok(0x80);
        }
        if kind == crate::operations::OP_ENCRYPTED_INPUT_ABANDON {
            #[cfg(all(test, feature = "archive-auth-test-support"))]
            return Ok(u64::from(
                arg1 == 0
                    && self
                        .operations
                        .abandon_encrypted_input(self.store_owner, arg0)
                        .is_ok(),
            ));
            #[cfg(not(all(test, feature = "archive-auth-test-support")))]
            return Ok(0);
        }
        #[cfg(feature = "tauri-webview-test-support")]
        if let Some(webviews) = &self.webviews {
            webviews.trace_abi("submit", Some(kind));
        }
        if kind == crate::operations::OP_WEBVIEW_URL_GRANT {
            #[cfg(feature = "tauri-webview")]
            if arg0 == 0 && arg1 == 0 {
                return Ok(self.initial_url.unwrap_or(0));
            }
            return Ok(0);
        }
        #[cfg(feature = "tauri-webview")]
        if let Some(webviews) = &self.webviews {
            let kind = if kind == crate::operations::OP_SYNTHETIC_RESOURCE_CLOSE
                && self
                    .operations
                    .external_webview_wire(self.store_owner, arg0)
            {
                crate::operations::OP_WEBVIEW_CLOSE
            } else {
                kind
            };
            if matches!(
                kind,
                crate::operations::OP_WEBVIEW_OPEN
                    | crate::operations::OP_WEBVIEW_LOAD
                    | crate::operations::OP_WEBVIEW_CAPTURE
                    | crate::operations::OP_WEBVIEW_CLOSE
            ) {
                return Ok(webviews
                    .submit(self.store_owner, kind, arg0, arg1)
                    .unwrap_or(0));
            }
        }
        if kind == crate::operations::OP_OUTPUT_GRANT {
            return Ok(if arg0 == 0 && arg1 == 0 {
                self.initial_output.unwrap_or(0)
            } else {
                0
            });
        }
        if kind == crate::operations::OP_BLOB_ABANDON {
            return Ok(u64::from(
                arg1 == 0
                    && self
                        .operations
                        .abandon_blob_wire(self.store_owner, arg0)
                        .is_ok(),
            ));
        }
        if kind == crate::operations::OP_TRANSFER_ABANDON {
            return Ok(u64::from(
                arg1 == 0
                    && self
                        .operations
                        .abandon_transfer_wire(self.store_owner, arg0)
                        .is_ok(),
            ));
        }
        let Some(runtime) = self.runtime.clone() else {
            return Ok(0);
        };
        if kind == crate::operations::OP_BLOB_READ_COLLECT {
            let offset = arg1 as u32 as i32;
            let capacity = (arg1 >> 32) as usize;
            let Some(cells) = shared_range(&self.controller.memory, offset, capacity) else {
                return Ok(0x80);
            };
            return Ok(self
                .operations
                .collect_blob_read_wire(self.store_owner, arg0, capacity, |bytes| {
                    for (cell, byte) in cells.iter().zip(bytes) {
                        // SAFETY: pinned shared bytes, accessed atomically.
                        unsafe { AtomicU8::from_ptr(cell.get()) }.store(*byte, Ordering::Relaxed);
                    }
                })
                .unwrap_or(0x80));
        }
        if kind == crate::operations::OP_BLOB_WRITE {
            let offset = arg1 as u32 as i32;
            let length = (arg1 >> 32) as usize;
            let Some(cells) = shared_range(&self.controller.memory, offset, length) else {
                return Ok(0);
            };
            return Ok(self
                .operations
                .submit_blob_write_wire(self.store_owner, arg0, length, || {
                    cells
                        .iter()
                        .map(|cell| {
                            // SAFETY: shared_range returns pinned byte cells;
                            // all host access uses atomics, as in write_shared.
                            unsafe { AtomicU8::from_ptr(cell.get()) }.load(Ordering::Relaxed)
                        })
                        .collect()
                })
                .unwrap_or(0));
        }
        if matches!(
            kind,
            crate::operations::OP_BLOB_READ
                | crate::operations::OP_CLOCK_SLEEP
                | crate::operations::OP_BLOB_SEAL
                | crate::operations::OP_OUTPUT_COMMIT
        ) {
            // Expected admission failures are guest-visible rejection, not
            // Wasmtime traps. Match the bounded-write submission contract.
            return Ok(self
                .operations
                .submit_wire(runtime, self.store_owner, kind, arg0, arg1)
                .unwrap_or(0));
        }
        self.operations
            .submit_wire(runtime, self.store_owner, kind, arg0, arg1)
            .map_err(|_| wasmtime::Error::msg("operation rejected"))
    }

    fn operation_poll(&mut self, operation: u64) -> wasmtime::Result<u64> {
        #[cfg(feature = "tauri-webview-test-support")]
        if let Some(webviews) = &self.webviews {
            webviews.trace_abi("poll", None);
        }
        Ok(self.operations.poll_wire(self.store_owner, operation))
    }

    fn operation_yield(
        &mut self,
        operation: u64,
    ) -> wasmtime::Result<Arc<crate::async_engine::Notify>> {
        #[cfg(feature = "tauri-webview-test-support")]
        if let Some(webviews) = &self.webviews {
            webviews.trace_abi("yield", None);
        }
        // The generated future calls this only after submit/poll. The async
        // owner driver will replace this scalar acknowledgement with its
        // parked Wasmtime yield glue; no Caller escapes this boundary.
        let Some(runtime) = self.runtime.clone() else {
            return self
                .operations
                .suspend_wire(self.store_owner, operation)
                .map_err(|_| wasmtime::Error::msg("operation cannot suspend"));
        };
        self.operations
            .suspend_stream_wire(runtime, self.store_owner, operation)
            .map_err(|_| wasmtime::Error::msg("operation cannot suspend"))
    }

    fn operation_cancel(&mut self, operation: u64) -> wasmtime::Result<i32> {
        #[cfg(feature = "tauri-webview-test-support")]
        if let Some(webviews) = &self.webviews {
            webviews.trace_abi("cancel", None);
        }
        Ok(i32::from(
            self.operations
                .cancel_wire(self.store_owner, operation)
                .is_ok(),
        ))
    }
}

fn install_epoch_deadline(store: &mut Store<ThreadStoreState>) {
    store.set_epoch_deadline(1);
    // Wasmtime 45 installs this callback infallibly and the callback returns
    // `()`: UpdateDeadline is the callback result, not a fallible registration.
    store.epoch_deadline_callback(|state| {
        if state.data().epoch.winner.load(Ordering::Acquire) == EPOCH_PENDING {
            Ok(UpdateDeadline::Continue(1))
        } else {
            Ok(UpdateDeadline::Interrupt)
        }
    });
}

/// One engine-global ticker with bounded weak logical-execution entries.
struct EpochBroker {
    engine: Arc<Engine>,
    limits: SketchEpochLimits,
    state: Mutex<EpochBrokerState>,
    // Completed ticks, so a test can wait for the exact tick it asserts about
    // instead of sleeping for one and hoping the host scheduled it.
    #[cfg(test)]
    ticks: AtomicU64,
}
struct EpochBrokerState {
    runtime: Option<crate::async_engine::RuntimeHandle>,
    registrations: Vec<Weak<EpochEntry>>,
    generation: u64,
    ticker: Option<(u64, crate::async_engine::Task<()>)>,
}
struct LogicalEpoch {
    cancellation: crate::async_engine::CancellationToken,
    deadline: Instant,
    winner: AtomicU8,
    operations: Mutex<Option<Weak<OperationHub>>>,
}
impl LogicalEpoch {
    fn bind_operations(&self, operations: &Arc<OperationHub>) -> Result<(), SketchExecutionError> {
        let mut bound = self
            .operations
            .lock()
            .map_err(|_| SketchExecutionError::PrelinkFailed)?;
        *bound = Some(Arc::downgrade(operations));
        drop(bound);
        // Also cover interruption before the root finished creating grants.
        self.wake_interrupted_operations();
        Ok(())
    }

    fn wake_interrupted_operations(&self) {
        let terminal = match self.winner.load(Ordering::Acquire) {
            EPOCH_CANCELLED => operations::Terminal::Cancelled,
            EPOCH_DEADLINE_EXCEEDED => operations::Terminal::TimedOut,
            _ => return,
        };
        let operations = self
            .operations
            .lock()
            .ok()
            .and_then(|bound| bound.clone())
            .and_then(|bound| bound.upgrade());
        if let Some(operations) = operations {
            // Epoch interrupts executing Wasm, but cannot wake an async host
            // import parked on operation_yield. Revoke its shared authority
            // and cancel producers so both root and child waiters can drain.
            if operations.try_close_all(terminal) {
                if let Ok(mut bound) = self.operations.lock() {
                    *bound = None;
                }
            }
        }
    }
}
struct EpochEntry {
    logical: Arc<LogicalEpoch>,
}
struct EpochRegistration {
    broker: Arc<EpochBroker>,
    entry: Option<Arc<EpochEntry>>,
    completes_logical: bool,
}
impl EpochBroker {
    fn new(engine: Arc<Engine>, limits: SketchEpochLimits) -> Self {
        Self {
            engine,
            limits,
            state: Mutex::new(EpochBrokerState {
                runtime: None,
                registrations: Vec::with_capacity(limits.maximum_active_registrations),
                generation: 0,
                ticker: None,
            }),
            #[cfg(test)]
            ticks: AtomicU64::new(0),
        }
    }
    fn register_root(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        cancellation: crate::async_engine::CancellationToken,
    ) -> Result<EpochRegistration, SketchExecutionError> {
        let logical = Arc::new(LogicalEpoch {
            cancellation,
            deadline: Instant::now() + self.limits.wall_clock_deadline,
            winner: AtomicU8::new(EPOCH_PENDING),
            operations: Mutex::new(None),
        });
        self.register_entry(runtime, logical, true)
    }
    fn register_child(
        self: &Arc<Self>,
        logical: Arc<LogicalEpoch>,
    ) -> Result<EpochRegistration, SketchExecutionError> {
        let runtime = self
            .state
            .lock()
            .map_err(|_| SketchExecutionError::PrelinkFailed)?
            .runtime
            .clone()
            .ok_or(SketchExecutionError::ForeignRuntime)?;
        self.register_entry(runtime, logical, false)
    }
    fn register_entry(
        self: &Arc<Self>,
        runtime: crate::async_engine::RuntimeHandle,
        logical: Arc<LogicalEpoch>,
        completes_logical: bool,
    ) -> Result<EpochRegistration, SketchExecutionError> {
        let entry = Arc::new(EpochEntry {
            logical: Arc::clone(&logical),
        });
        let mut state = self
            .state
            .lock()
            .map_err(|_| SketchExecutionError::PrelinkFailed)?;
        state
            .registrations
            .retain(|entry| entry.strong_count() != 0);
        // Registration and terminal-state publication are serialized by this
        // mutex. A child can therefore never acquire a Store slot after its
        // logical execution has committed cancellation/deadline/completion.
        if logical.winner.load(Ordering::Acquire) != EPOCH_PENDING {
            return Err(SketchExecutionError::EpochRegistrationLimitExceeded);
        }
        if state.registrations.len() >= self.limits.maximum_active_registrations {
            return Err(SketchExecutionError::EpochRegistrationLimitExceeded);
        }
        if let Some(expected) = state.runtime.as_ref() {
            if !runtime.same_runtime_for_wasm(expected) {
                return Err(SketchExecutionError::ForeignRuntime);
            }
        } else {
            state.runtime = Some(runtime.clone());
        }
        state.registrations.push(Arc::downgrade(&entry));
        if state.ticker.is_none() {
            state.generation = state.generation.wrapping_add(1);
            let generation = state.generation;
            let broker = Arc::clone(self);
            state.ticker = Some((
                generation,
                runtime.launch(async move {
                    broker.tick(generation).await;
                }),
            ));
        }
        Ok(EpochRegistration {
            broker: Arc::clone(self),
            entry: Some(entry),
            completes_logical,
        })
    }
    async fn tick(self: Arc<Self>, generation: u64) {
        loop {
            crate::async_engine::sleep(self.limits.tick_interval).await;
            let entries = match self.state.lock() {
                Ok(mut state) => {
                    if state.generation != generation {
                        return;
                    }
                    state
                        .registrations
                        .retain(|entry| entry.strong_count() != 0);
                    let entries = state
                        .registrations
                        .iter()
                        .filter_map(Weak::upgrade)
                        .collect::<Vec<_>>();
                    for entry in &entries {
                        let logical = &entry.logical;
                        // Cancellation wins the same tick as deadline. The
                        // winner is published while registration is locked.
                        let terminal = if logical.cancellation.is_cancelled() {
                            EPOCH_CANCELLED
                        } else if Instant::now() >= logical.deadline {
                            EPOCH_DEADLINE_EXCEEDED
                        } else {
                            EPOCH_PENDING
                        };
                        if terminal != EPOCH_PENDING {
                            let _ = logical.winner.compare_exchange(
                                EPOCH_PENDING,
                                terminal,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            );
                        }
                    }
                    entries
                }
                Err(_) => Vec::new(),
            };
            if entries.is_empty() {
                return;
            }
            // Never take operation-hub locks while holding registration state.
            for entry in &entries {
                entry.logical.wake_interrupted_operations();
            }
            self.engine.increment_epoch();
            // Released after this tick published every terminal winner, so a
            // test that observes the count then reads a winner sees this tick.
            #[cfg(test)]
            self.ticks.fetch_add(1, Ordering::Release);
        }
    }
    #[cfg(test)]
    fn ticks(&self) -> u64 {
        self.ticks.load(Ordering::Acquire)
    }
    fn active_registrations(&self) -> usize {
        self.state
            .lock()
            .map(|state| {
                state
                    .registrations
                    .iter()
                    .filter(|entry| entry.strong_count() != 0)
                    .count()
            })
            .unwrap_or(0)
    }
    fn finish_registration(
        &self,
        entry: &Arc<EpochEntry>,
        completes_logical: bool,
    ) -> Option<crate::async_engine::Task<()>> {
        let mut state = self.state.lock().ok()?;
        if completes_logical {
            let _ = entry.logical.winner.compare_exchange(
                EPOCH_PENDING,
                EPOCH_COMPLETED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        state.registrations.retain(|candidate| {
            candidate
                .upgrade()
                .is_some_and(|live| !Arc::ptr_eq(&live, entry))
        });
        if state.registrations.is_empty() {
            state.generation = state.generation.wrapping_add(1);
            return state.ticker.take().map(|(_, ticker)| ticker);
        }
        None
    }
}
impl EpochRegistration {
    fn logical(&self) -> Arc<LogicalEpoch> {
        Arc::clone(
            &self
                .entry
                .as_ref()
                .expect("active epoch registration")
                .logical,
        )
    }
    fn finish(mut self) -> Option<crate::async_engine::Task<()>> {
        self.entry.take().and_then(|entry| {
            self.broker
                .finish_registration(&entry, self.completes_logical)
        })
    }
}
impl Drop for EpochRegistration {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            let _ = self
                .broker
                .finish_registration(&entry, self.completes_logical);
        }
    }
}

#[cfg(test)]
mod epoch_broker_tests {
    use super::*;

    /// Owns the deliberately blocked characterization child.  Every assertion
    /// after spawning remains safe: Drop kills and reaps the exact child and
    /// removes only its exact marker.
    struct ContainmentChildGuard {
        child: Option<std::process::Child>,
        marker: std::path::PathBuf,
    }
    impl ContainmentChildGuard {
        fn child_mut(&mut self) -> &mut std::process::Child {
            self.child
                .as_mut()
                .expect("containment child remains armed")
        }
        fn reap_and_disarm(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = std::fs::remove_file(&self.marker);
        }
    }
    impl Drop for ContainmentChildGuard {
        fn drop(&mut self) {
            self.reap_and_disarm();
        }
    }

    /// The readiness marker is complete only once it carries its full payload.
    /// The child writes it with `File::create` + `write_all`, so the path
    /// appears before the bytes do and a reader that polls the path alone can
    /// observe an empty file.
    fn marker_reports_entry(marker: &std::path::Path) -> bool {
        std::fs::read_to_string(marker).is_ok_and(|text| text == "entered")
    }

    /// The tick interval is short so a broker tick is observable, but the
    /// wall-clock deadline is inert: these tests assert that a registration
    /// stays runnable, and must not also race the deadline that would
    /// legitimately end it. A test that wants the deadline to fire builds its
    /// own limits.
    fn compiler_with_epochs(maximum: usize) -> SketchCompiler {
        let limits =
            SketchEpochLimits::new(Duration::from_secs(3600), Duration::from_millis(1), maximum)
                .expect("limits");
        SketchCompiler::new(
            SketchCompilerConfig::default()
                .with_epoch_limits(limits)
                .expect("config"),
        )
        .expect("compiler")
    }

    #[test]
    fn registrations_are_per_store_bounded_and_last_generation_is_joined() {
        // The helper's deadline is inert, so the only way this root's winner
        // may leave `EPOCH_PENDING` below is a child release wrongly
        // publishing logical completion.
        let compiler = compiler_with_epochs(2);
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime
            .run_within(
                async {
                    let source = crate::async_engine::CancellationSource::new();
                    let root = compiler
                        .epoch_broker
                        .register_root(runtime.handle(), source.token())
                        .expect("root");
                    let child = compiler
                        .epoch_broker
                        .register_child(root.logical())
                        .expect("child");
                    assert_eq!(
                        compiler
                            .execution_limits_snapshot()
                            .active_epoch_registrations(),
                        2
                    );
                    assert!(matches!(
                        compiler.epoch_broker.register_child(root.logical()),
                        Err(SketchExecutionError::EpochRegistrationLimitExceeded)
                    ));
                    drop(child);
                    // Release is synchronous with the registration: the count drops
                    // inside `Drop`, without waiting on the ticker.
                    assert_eq!(
                        compiler
                            .execution_limits_snapshot()
                            .active_epoch_registrations(),
                        1
                    );
                    // A child Store's normal release must not publish logical
                    // completion. Wait for the exact broker tick that the assertion
                    // below is about rather than sleeping for the length of one.
                    let before = compiler.epoch_broker.ticks();
                    let tick_deadline = Instant::now() + Duration::from_secs(5);
                    while compiler.epoch_broker.ticks() == before && Instant::now() < tick_deadline
                    {
                        crate::async_engine::sleep(compiler.epoch_broker.limits.tick_interval())
                            .await;
                    }
                    assert!(
                        compiler.epoch_broker.ticks() > before,
                        "the broker must tick over the surviving root registration"
                    );
                    assert_eq!(root.logical().winner.load(Ordering::Acquire), EPOCH_PENDING);
                    let ticker = root.finish().expect("the last release returns its ticker");
                    // The final release is also synchronous: the ledger reads zero
                    // before the ticker task is joined, not because of it.
                    assert_eq!(
                        compiler
                            .execution_limits_snapshot()
                            .active_epoch_registrations(),
                        0
                    );
                    let _ = ticker.await;
                    assert_eq!(
                        compiler
                            .execution_limits_snapshot()
                            .active_epoch_registrations(),
                        0
                    );
                    let state = compiler.epoch_broker.state.lock().expect("broker state");
                    assert_eq!(
                        state.generation, 2,
                        "one start and one exact-once final removal"
                    );
                    assert!(state.ticker.is_none());
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    #[test]
    fn broker_rejects_a_foreign_runtime_but_accepts_the_same_live_runtime() {
        let compiler = compiler_with_epochs(2);
        let first = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("first");
        let second = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("second");
        let source = crate::async_engine::CancellationSource::new();
        first
            .run_within(
                async {
                    let root = compiler
                        .epoch_broker
                        .register_root(first.handle(), source.token())
                        .expect("root");
                    let same = compiler
                        .epoch_broker
                        .register_child(root.logical())
                        .expect("same runtime");
                    drop(same);
                    if let Some(ticker) = root.finish() {
                        let _ = ticker.await;
                    }
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert!(matches!(
            compiler
                .epoch_broker
                .register_root(second.handle(), source.token()),
            Err(SketchExecutionError::ForeignRuntime)
        ));
    }

    #[test]
    fn interrupted_epochs_wake_host_operations_before_or_after_binding() {
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        for (winner, status) in [(EPOCH_CANCELLED, 2), (EPOCH_DEADLINE_EXCEEDED, 3)] {
            for interrupt_before_binding in [false, true] {
                let epoch = LogicalEpoch {
                    cancellation: crate::async_engine::CancellationSource::new().token(),
                    deadline: Instant::now(),
                    winner: AtomicU8::new(EPOCH_PENDING),
                    operations: Mutex::new(None),
                };
                let hub = OperationHub::new(2, 1).unwrap();
                let (operation, _) = hub.submit(0, None, 0, 0).unwrap();
                let wake = hub.suspend_wire(0, operation.wire()).unwrap();
                if interrupt_before_binding {
                    epoch.winner.store(winner, Ordering::Release);
                    epoch.wake_interrupted_operations();
                }
                epoch.bind_operations(&hub).unwrap();
                if !interrupt_before_binding {
                    assert_eq!(hub.poll_wire(0, operation.wire()), 0);
                    epoch.winner.store(winner, Ordering::Release);
                    epoch.wake_interrupted_operations();
                }
                runtime
                    .run_within(
                        async {
                            crate::async_engine::timeout(Duration::from_secs(1), wake.notified())
                                .await
                                .expect("interrupted host operation must wake");
                        },
                        std::time::Duration::from_secs(60),
                    )
                    .expect("test runtime budget");
                assert_eq!(hub.poll_wire(0, operation.wire()), status);
                assert!(hub.submit(0, None, 0, 0).is_err());
                assert_eq!(hub.snapshot().pending_operations, 0);
                assert!(epoch.operations.lock().unwrap().is_none());
                epoch.wake_interrupted_operations();
            }
        }
    }

    #[test]
    fn cancellation_wins_a_same_tick_deadline_without_reclassifying_fuel() {
        let epoch = LogicalEpoch {
            cancellation: crate::async_engine::CancellationSource::new().token(),
            deadline: Instant::now(),
            winner: AtomicU8::new(EPOCH_CANCELLED),
            operations: Mutex::new(None),
        };
        assert_eq!(epoch_error(&epoch), Some(SketchExecutionError::Cancelled));
        let fuel = wasmtime::Error::new(wasmtime::Trap::OutOfFuel);
        assert_eq!(
            map_root_error(&fuel, &epoch),
            Err(SketchExecutionError::OutOfFuel)
        );
        assert_eq!(map_child_error(&fuel, &epoch), ChildOutcome::OutOfFuel);
    }

    #[test]
    fn containment_required_atomic_wait_is_killed_only_in_a_subprocess() {
        const MODE: &str = "KERNAL_API_EPOCH_ATOMIC_WAIT_CHILD";
        const MARKER: &str = "KERNAL_API_EPOCH_ATOMIC_WAIT_MARKER";
        if std::env::var_os(MODE).is_some() {
            let bytes = super::threaded_root_observation_tests::atomic_wait_fixture();
            let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
            let sketch = compiler
                .admit(
                    &bytes,
                    SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES)
                        .expect("policy"),
                )
                .expect("admission");
            let runtime = crate::async_engine::RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            // The fixture yields once to publish readiness, then enters a real
            // guest atomic.wait. The parent is the only process that kills it.
            runtime
                .run_within(
                    async {
                        let _ = sketch.execute_threaded_root(runtime.handle()).await;
                    },
                    std::time::Duration::from_secs(60),
                )
                .expect("test runtime budget");
            return;
        }
        let executable = std::env::current_exe().expect("test executable");
        let marker = std::env::temp_dir().join(format!(
            "kernal-api-epoch-atomic-wait-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&marker);
        let child = std::process::Command::new(executable)
            .arg("--exact")
            .arg("wasm::epoch_broker_tests::containment_required_atomic_wait_is_killed_only_in_a_subprocess")
            .arg("--nocapture")
            .env(MODE, "1")
            .env(MARKER, &marker)
            .spawn()
            .expect("containment child");
        let mut guard = ContainmentChildGuard {
            child: Some(child),
            marker,
        };
        let readiness_deadline = Instant::now() + Duration::from_secs(5);
        // `File::create` publishes the marker path before its payload lands,
        // so an existence poll can observe a torn, still-empty marker. Wait
        // for the payload itself; the readiness bound is unchanged.
        while !marker_reports_entry(&guard.marker) && Instant::now() < readiness_deadline {
            assert!(
                guard
                    .child_mut()
                    .try_wait()
                    .expect("child status")
                    .is_none(),
                "atomic-wait child exited before reaching the fixture"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            marker_reports_entry(&guard.marker),
            "child must reach the Wasm atomic.wait fixture before containment classification"
        );
        assert_eq!(
            std::fs::read_to_string(&guard.marker).expect("read readiness marker"),
            "entered"
        );
        // Several clock cadence intervals must elapse without pretending the
        // guest atomic wait became interruptible.
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            guard
                .child_mut()
                .try_wait()
                .expect("child status")
                .is_none(),
            "blocked child must outlive multiple epoch intervals"
        );
        let classification = SketchExecutionError::ContainmentRequired;
        assert_eq!(classification.code(), "containment-required");
        guard.reap_and_disarm();
    }

    #[test]
    fn containment_required_host_block_is_killed_only_in_a_subprocess() {
        const MODE: &str = "KERNAL_API_EPOCH_HOST_BLOCK_CHILD";
        const MARKER: &str = "KERNAL_API_EPOCH_HOST_BLOCK_MARKER";
        if std::env::var_os(MODE).is_some() {
            let bytes = super::threaded_root_observation_tests::threaded_yield_fixture();
            let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
            let sketch = compiler
                .admit(
                    &bytes,
                    SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES)
                        .expect("policy"),
                )
                .expect("admission");
            let runtime = crate::async_engine::RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            // The actual Wasm import callback writes readiness and parks.
            runtime
                .run_within(
                    async {
                        let _ = sketch.execute_threaded_root(runtime.handle()).await;
                    },
                    std::time::Duration::from_secs(60),
                )
                .expect("test runtime budget");
            return;
        }
        let marker = std::env::temp_dir().join(format!(
            "kernal-api-epoch-host-block-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&marker);
        let child = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("wasm::epoch_broker_tests::containment_required_host_block_is_killed_only_in_a_subprocess")
            .arg("--nocapture").env(MODE, "1").env(MARKER, &marker).spawn().expect("host-block child");
        let mut guard = ContainmentChildGuard {
            child: Some(child),
            marker,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        // See the atomic-wait proof: poll the payload, not the path.
        while !marker_reports_entry(&guard.marker) && Instant::now() < deadline {
            assert!(
                guard
                    .child_mut()
                    .try_wait()
                    .expect("child status")
                    .is_none(),
                "host-block child exited before callback readiness"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            marker_reports_entry(&guard.marker),
            "child must enter the real Wasm host callback"
        );
        assert_eq!(
            std::fs::read_to_string(&guard.marker).expect("read marker"),
            "entered"
        );
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            guard
                .child_mut()
                .try_wait()
                .expect("child status")
                .is_none(),
            "host block must outlive epoch ticks"
        );
        assert_eq!(
            SketchExecutionError::ContainmentRequired.code(),
            "containment-required"
        );
        guard.reap_and_disarm();
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RootExecutionObservation {
    preparations: u64,
    kernel_yields: u64,
    supplied_runtime_handles: u64,
    runtime_identity_mismatches: u64,
    last_root_remaining_fuel: u64,
    last_child_remaining_fuel: u64,
    accepted_child_registrations: usize,
    live_threads: usize,
    queued_join_handles: usize,
    operation_snapshot: Option<operations::HubSnapshot>,
}

#[derive(Debug, thiserror::Error)]
#[error("private kernal-api proc_exit sentinel ({0})")]
struct ProcExitSentinel(i32);

#[expect(
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
fn define_closed_imports(
    linker: &mut Linker<ThreadStoreState>,
) -> Result<(), SketchExecutionError> {
    generated_v1::link_kernal_api_v1(linker).map_err(|_| SketchExecutionError::PrelinkFailed)?;
    linker
        .func_wrap(
            THREAD_MODULE,
            THREAD_SPAWN,
            |caller: Caller<'_, ThreadStoreState>, arg: i32| -> i32 {
                let controller = Arc::clone(&caller.data().controller);
                let runtime = caller.data().runtime.clone();
                // Fuel must be reserved before a guest-visible TID, native
                // thread, Store, or instance exists. A failed reservation is
                // the same closed ABI sentinel as other rejected spawns, with
                // a separate bounded semantic reason.
                let generation = caller.data().fuel_generation;
                let epoch = Arc::clone(&caller.data().epoch);
                let operations = Arc::clone(&caller.data().operations);
                #[cfg(feature = "tauri-webview")]
                let webviews = caller.data().webviews.clone();
                if epoch.winner.load(Ordering::Acquire) != EPOCH_PENDING {
                    if let Ok(mut workers) = controller.workers.lock() {
                        workers.epoch_rejections = workers.epoch_rejections.saturating_add(1);
                    }
                    return THREAD_SPAWN_REJECTED;
                }
                // A child consumes an independently bounded Store epoch slot
                // before fuel, TID, native-thread, Store, or instance state.
                // RAII refunds that slot on every later rejection path.
                let child_epoch = match controller.epoch_broker.register_child(Arc::clone(&epoch)) {
                    Ok(registration) => registration,
                    Err(_) => {
                        if let Ok(mut workers) = controller.workers.lock() {
                            workers.epoch_rejections = workers.epoch_rejections.saturating_add(1);
                        }
                        return THREAD_SPAWN_REJECTED;
                    }
                };
                let Some(child_fuel) = controller.reserve_child_fuel(generation) else {
                    if let Ok(mut workers) = controller.workers.lock() {
                        workers.fuel_rejections = workers.fuel_rejections.saturating_add(1);
                    }
                    return THREAD_SPAWN_REJECTED;
                };
                let tid = match controller.workers.lock() {
                    Ok(mut w) => {
                        if w.closing {
                            w.closing_rejections = w.closing_rejections.saturating_add(1);
                            controller.refund_child_fuel();
                            return THREAD_SPAWN_REJECTED;
                        }
                        if w.live >= w.maximum
                            || w.accepted >= w.maximum
                            || w.next_tid > 0x1fff_ffff
                        {
                            w.capacity_rejections = w.capacity_rejections.saturating_add(1);
                            controller.refund_child_fuel();
                            return THREAD_SPAWN_REJECTED;
                        }
                        let tid = w.next_tid;
                        w.next_tid += 1;
                        w.accepted += 1;
                        w.live += 1;
                        tid
                    }
                    _ => {
                        controller.refund_child_fuel();
                        return THREAD_SPAWN_REJECTED;
                    }
                };
                let child = Arc::clone(&controller);
                let spawned = std::thread::Builder::new().spawn(move || {
                    let _epoch_registration = child_epoch;
                    let _thread_observation = CounterObservation::new(
                        Arc::clone(&child.execution_ledger),
                        LedgerCounter::GuestThreads,
                    );
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _store_observation = CounterObservation::new(
                            Arc::clone(&child.execution_ledger),
                            LedgerCounter::Stores,
                        );
                        let Some(store_runtime) = runtime.clone() else {
                            return ChildOutcome::Trapped;
                        };
                        let mut store = Store::new(
                            &child.engine,
                            ThreadStoreState {
                                controller: Arc::clone(&child),
                                runtime,
                                fuel_generation: generation,
                                epoch: Arc::clone(&epoch),
                                operations,
                                store_owner: u64::try_from(tid).unwrap_or(u64::MAX),
                                initial_output: None,
                                initial_compiler: None,
                                #[cfg(all(test, feature = "archive-auth-test-support"))]
                                initial_archive: None,
                                #[cfg(feature = "tauri-webview")]
                                webviews,
                                #[cfg(feature = "tauri-webview")]
                                initial_url: None,
                            },
                        );
                        install_epoch_deadline(&mut store);
                        let outcome = (|| {
                            if store.set_fuel(child_fuel).is_err() {
                                return ChildOutcome::Trapped;
                            }
                            let Some(prelink) = child.prelink.get() else {
                                return ChildOutcome::Trapped;
                            };
                            let instance = match store_runtime
                                .block_on_wasm(prelink.instantiate_async(&mut store))
                            {
                                Ok(instance) => instance,
                                Err(error) => return map_child_error(&error, &epoch),
                            };
                            let _instance_observation = CounterObservation::new(
                                Arc::clone(&child.execution_ledger),
                                LedgerCounter::Instances,
                            );
                            let entry = instance
                                .get_typed_func::<(i32, i32), ()>(&mut store, "wasi_thread_start")
                                .map_err(|_| ())
                                .ok();
                            let Some(entry) = entry else {
                                return ChildOutcome::Trapped;
                            };
                            match store_runtime
                                .block_on_wasm(entry.call_async(&mut store, (tid, arg)))
                            {
                                Ok(()) => ChildOutcome::Completed,
                                Err(error) => map_child_error(&error, &epoch),
                            }
                        })();
                        // Sample the Store after every entry outcome, including
                        // OutOfFuel. This remains private bounded telemetry and
                        // never exposes a Store through the facade.
                        child
                            .last_child_remaining_fuel
                            .store(store.get_fuel().unwrap_or(0), Ordering::Release);
                        outcome
                    }));
                    let outcome = match result {
                        Ok(outcome) => outcome,
                        Err(_) => ChildOutcome::Panicked,
                    };
                    if let Ok(mut w) = child.workers.lock() {
                        w.live = w.live.saturating_sub(1);
                        if w.outcomes.len() < w.maximum {
                            w.outcomes.push((tid, outcome));
                        }
                    }
                });
                match spawned {
                    Ok(handle) => {
                        if let Ok(mut w) = controller.workers.lock() {
                            w.handles.push(handle);
                            tid
                        } else {
                            // Do not detach a worker if bookkeeping became
                            // poisoned after reservation. We cannot safely
                            // expose its positive TID, so wait before failing.
                            let _ = handle.join();
                            THREAD_SPAWN_REJECTED
                        }
                    }
                    Err(_) => {
                        if let Ok(mut w) = controller.workers.lock() {
                            w.live = w.live.saturating_sub(1);
                            w.accepted = w.accepted.saturating_sub(1);
                        }
                        controller.refund_child_fuel();
                        THREAD_SPAWN_REJECTED
                    }
                }
            },
        )
        .map_err(|_| SketchExecutionError::PrelinkFailed)?;

    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "clock_time_get",
            |caller: Caller<'_, ThreadStoreState>, _id: i32, _precision: i64, output: i32| -> i32 {
                write_shared(
                    &caller.data().controller.memory,
                    output,
                    &0_u64.to_le_bytes(),
                )
            },
        )
        .map_err(|_| SketchExecutionError::PrelinkFailed)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "environ_get",
            |caller: Caller<'_, ThreadStoreState>, _entries: i32, _buffer: i32| -> i32 {
                // Empty environment: no guest memory is dereferenced.
                let _ = &caller.data().controller.memory;
                ERRNO_SUCCESS
            },
        )
        .map_err(|_| SketchExecutionError::PrelinkFailed)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "environ_sizes_get",
            |caller: Caller<'_, ThreadStoreState>, count: i32, bytes: i32| -> i32 {
                let memory = &caller.data().controller.memory;
                // Validate both output ranges before either write so a bad
                // second pointer never causes a partial guest-memory update.
                if shared_range(memory, count, 4).is_none()
                    || shared_range(memory, bytes, 4).is_none()
                {
                    return ERRNO_FAULT;
                }
                write_shared(memory, count, &0_u32.to_le_bytes());
                write_shared(memory, bytes, &0_u32.to_le_bytes())
            },
        )
        .map_err(|_| SketchExecutionError::PrelinkFailed)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_write",
            |caller: Caller<'_, ThreadStoreState>,
             _fd: i32,
             _iovecs: i32,
             _iovecs_len: i32,
             written: i32|
             -> i32 {
                // Output is intentionally discarded, but P1 still requires a
                // complete validation of the iovec table and payload ranges.
                let memory = &caller.data().controller.memory;
                let result = validate_iovecs(memory, _iovecs, _iovecs_len);
                if result != ERRNO_SUCCESS {
                    return result;
                }
                #[cfg(test)]
                if let Some(report) = capture_threaded_smoke_report(memory, _iovecs, _iovecs_len) {
                    if let Ok(mut slot) = caller.data().controller.threaded_smoke_report.lock() {
                        *slot = Some(report);
                    }
                }
                write_shared(memory, written, &0_u32.to_le_bytes())
            },
        )
        .map_err(|_| SketchExecutionError::PrelinkFailed)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "proc_exit",
            |_caller: Caller<'_, ThreadStoreState>, _code: i32| -> wasmtime::Result<()> {
                Err(wasmtime::Error::new(ProcExitSentinel(_code)))
            },
        )
        .map_err(|_| SketchExecutionError::PrelinkFailed)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "sched_yield",
            |_caller: Caller<'_, ThreadStoreState>| -> i32 { ERRNO_SUCCESS },
        )
        .map_err(|_| SketchExecutionError::PrelinkFailed)?;
    Ok(())
}

#[cfg(test)]
mod threaded_root_observation_tests {
    use super::*;

    #[test]
    fn bounded_blob_stream_controls_use_one_scoped_registry_authority() {
        let hub = OperationHub::new(4, 4).expect("hub");
        let stream = hub.create_blob(7).expect("stream");
        assert_eq!(
            write_blob_stream_chunk(&hub, 7, stream.wire(), b"ping").expect("write"),
            4
        );

        let mut destination = [0; 4];
        assert_eq!(
            read_blob_stream_chunk(&hub, 7, stream.wire(), &mut destination).expect("read"),
            4
        );
        assert_eq!(&destination, b"ping");
        assert_eq!(hub.snapshot().native_transfer_capacity, 0);

        assert_eq!(hub.abandon_blob_wire(7, stream.wire()), Ok(()));
        assert!(write_blob_stream_chunk(&hub, 7, stream.wire(), b"next").is_err());
    }

    #[test]
    fn blob_stream_controls_short_transfer_to_the_configured_chunk_quota() {
        let limits = operations::BlobLimits::new(2, 8, 16).expect("limits");
        let hub = OperationHub::with_blob_limits(4, 4, limits).expect("hub");
        let stream = hub.create_blob(7).expect("stream");
        assert_eq!(
            write_blob_stream_chunk(&hub, 7, stream.wire(), b"ping").expect("write"),
            2
        );
        assert_eq!(
            write_blob_stream_chunk(&hub, 7, stream.wire(), b"ng").expect("write"),
            2
        );

        let mut destination = [0; 4];
        assert_eq!(
            read_blob_stream_chunk(&hub, 7, stream.wire(), &mut destination).expect("read"),
            2
        );
        assert_eq!(&destination[..2], b"pi");
    }

    #[test]
    fn compiler_owned_ledger_reserves_the_exact_threaded_contract_and_releases_once() {
        let limits = SketchExecutionLimits::new(THREADED_RUST_RESERVATION_BYTES, 1)
            .expect("exact one-session limit");
        let compiler = SketchCompiler::new(
            SketchCompilerConfig::default()
                .with_execution_limits(limits)
                .expect("limits"),
        )
        .expect("compiler");
        let bytes = threaded_yield_fixture();
        let sketch = compiler
            .admit(
                &bytes,
                SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES)
                    .expect("policy"),
            )
            .expect("admission");
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime
            .run_within(
                async {
                    assert_eq!(
                        sketch.execute_threaded_root(runtime.handle()).await,
                        Ok(ThreadedRootOutcome::Started)
                    );
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(
            compiler
                .execution_limits_snapshot()
                .reserved_shared_memory_bytes(),
            THREADED_RUST_RESERVATION_BYTES,
        );
        sketch.close_threaded_root().expect("explicit close");
        assert_eq!(
            compiler.execution_limits_snapshot(),
            SketchExecutionSnapshot::default()
        );
    }

    #[test]
    fn compiler_ledger_rejects_a_second_preparation_before_shared_memory_allocation() {
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
        let bytes = threaded_yield_fixture();
        let policy = SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES)
            .expect("policy");
        let first = compiler.admit(&bytes, policy).expect("first admission");
        let second = compiler.admit(&bytes, policy).expect("second admission");
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime
            .run_within(
                async {
                    first
                        .execute_threaded_root(runtime.handle())
                        .await
                        .expect("first preparation");
                    assert_eq!(
                        second.execute_threaded_root(runtime.handle()).await,
                        Err(SketchExecutionError::SharedMemoryLimitExceeded),
                    );
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        first.close_threaded_root().expect("close first");
        runtime
            .run_within(
                async {
                    second
                        .execute_threaded_root(runtime.handle())
                        .await
                        .expect("released reservation admits second");
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
    }

    #[test]
    fn close_and_root_permit_are_linearized_by_the_cached_session_lock() {
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
        let bytes = threaded_yield_fixture();
        let sketch = compiler
            .admit(
                &bytes,
                SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES)
                    .expect("policy"),
            )
            .expect("admission");

        // The permit is acquired while `prepared_root` remains locked. A
        // concurrent close can therefore neither detach this session before
        // admission nor release its reservation underneath the root.
        let (prepared, permit) = sketch
            .prepare_threaded_root_with_permit()
            .expect("prepared root permit");
        assert_eq!(
            sketch.close_threaded_root(),
            Err(SketchExecutionError::SessionBusy)
        );
        assert_eq!(
            compiler
                .execution_limits_snapshot()
                .active_root_executions(),
            1
        );
        drop(permit);
        sketch
            .close_threaded_root()
            .expect("close after root drain");
        drop(prepared);
        assert_eq!(
            compiler.execution_limits_snapshot(),
            SketchExecutionSnapshot::default()
        );
    }

    #[test]
    fn barrier_race_between_close_and_execute_keeps_exactly_one_live_session() {
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
        let bytes = threaded_yield_fixture();
        let sketch = compiler
            .admit(
                &bytes,
                SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES)
                    .expect("policy"),
            )
            .expect("admission");
        let (prepared, permit) = sketch
            .prepare_threaded_root_with_permit()
            .expect("initial preparation");
        drop(permit);
        drop(prepared);

        // The barrier makes both contenders race on the same cached session.
        // Close may run after the short-lived permit has drained, in which
        // case it legitimately releases the newly prepared cache. The stable
        // invariant is therefore that no more than one 1 GiB reservation is
        // live, not that one remains cached after the race.
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let close_result = Mutex::new(None);
        std::thread::scope(|scope| {
            let close_barrier = Arc::clone(&barrier);
            let close_result = &close_result;
            let sketch = &sketch;
            scope.spawn(move || {
                close_barrier.wait();
                *close_result.lock().expect("result mutex") = Some(sketch.close_threaded_root());
            });
            barrier.wait();
            let (prepared, permit) = sketch
                .prepare_threaded_root_with_permit()
                .expect("racing root permit");
            drop(permit);
            drop(prepared);
        });
        assert!(matches!(
            close_result.lock().expect("result mutex").take(),
            Some(Ok(())) | Some(Err(SketchExecutionError::SessionBusy))
        ));
        let snapshot = compiler.execution_limits_snapshot();
        assert!(snapshot.reserved_shared_memory_bytes() <= THREADED_RUST_RESERVATION_BYTES);
        assert_eq!(snapshot.active_root_executions(), 0);
        sketch.close_threaded_root().expect("final close");
        assert_eq!(
            compiler.execution_limits_snapshot(),
            SketchExecutionSnapshot::default()
        );
    }

    #[test]
    fn thread_policy_rejects_requests_above_the_v1_absolute_cap() {
        let policy =
            SketchModulePolicy::threaded_rust_v1(1, THREADED_RUST_MAX_PAGES).expect("policy");
        assert_eq!(
            policy.with_max_guest_threads(MAX_GUEST_THREADS_V1 + 1),
            Err(SketchModuleError::ThreadLimitExceedsV1Maximum {
                requested: MAX_GUEST_THREADS_V1 + 1,
                maximum: MAX_GUEST_THREADS_V1,
            })
        );
    }

    #[test]
    fn repeated_roots_reuse_one_preparation_and_receive_the_supplied_runtime() {
        let bytes = threaded_yield_fixture();
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
        let sketch = compiler
            .admit(
                &bytes,
                SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, 16_384).expect("policy"),
            )
            .expect("admission");
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let handle = runtime.handle();
        runtime
            .run_within(
                async {
                    assert_eq!(
                        sketch.execute_threaded_root(handle.clone()).await,
                        Ok(ThreadedRootOutcome::Started)
                    );
                    assert_eq!(
                        sketch.execute_threaded_root(handle).await,
                        Ok(ThreadedRootOutcome::Started)
                    );
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(compiler.compiled_module_count(), 1);
        let observation = sketch
            .root_execution_observation_for_test()
            .expect("prepared root observation");
        assert_eq!(
            observation,
            RootExecutionObservation {
                preparations: 1,
                kernel_yields: 2,
                supplied_runtime_handles: 2,
                runtime_identity_mismatches: 0,
                last_root_remaining_fuel: 99_997,
                accepted_child_registrations: 0,
                live_threads: 0,
                queued_join_handles: 0,
                operation_snapshot: observation.operation_snapshot,
                ..RootExecutionObservation::default()
            }
        );
        let operations = observation
            .operation_snapshot
            .expect("root records lifecycle cleanup");
        assert_eq!(operations.pending_operations, 0);
        assert_eq!(operations.live_resources, 0);
        assert_eq!(operations.active_clocks, 0);
    }

    #[test]
    fn cap_rejection_drains_private_registrations_and_is_reusable() {
        let bytes = threaded_cap_fixture();
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
        let policy = SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES)
            .expect("policy")
            .with_max_guest_threads(1)
            .expect("cap");
        let sketch = compiler.admit(&bytes, policy).expect("admission");
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        for expected_yields in [0, 0] {
            let outcome = runtime
                .run_within(
                    async { sketch.execute_threaded_root(runtime.handle()).await },
                    std::time::Duration::from_secs(60),
                )
                .expect("test runtime budget");
            assert!(matches!(
                outcome,
                Ok(ThreadedRootOutcome::StartedWithThreadRejections(summary))
                    if summary.capacity() == 1 && summary.closing() == 0
            ));
            let observation = sketch
                .root_execution_observation_for_test()
                .expect("prepared observation");
            assert_eq!(observation.kernel_yields, expected_yields);
            assert_eq!(observation.accepted_child_registrations, 0);
            assert_eq!(observation.live_threads, 0);
            assert_eq!(observation.queued_join_handles, 0);
        }
    }

    fn fuel_compiler(root_slice: u64, child_slice: u64) -> SketchCompiler {
        let total = root_slice + child_slice * MAX_GUEST_THREADS_V1 as u64;
        fuel_compiler_with_total(total, root_slice, child_slice)
    }

    fn fuel_compiler_with_total(total: u64, root_slice: u64, child_slice: u64) -> SketchCompiler {
        let fuel = SketchFuelLimits::new(total, root_slice, child_slice).expect("fuel limits");
        let limits = SketchExecutionLimits::default()
            .with_fuel_limits(fuel)
            .expect("execution limits");
        SketchCompiler::new(
            SketchCompilerConfig::default()
                .with_execution_limits(limits)
                .expect("compiler config"),
        )
        .expect("compiler")
    }

    fn execute_fuel_fixture(
        bytes: Vec<u8>,
        root_slice: u64,
        child_slice: u64,
    ) -> Result<ThreadedRootOutcome, SketchExecutionError> {
        execute_fuel_fixture_with_observation(bytes, root_slice, child_slice).0
    }

    fn execute_fuel_fixture_with_observation(
        bytes: Vec<u8>,
        root_slice: u64,
        child_slice: u64,
    ) -> (
        Result<ThreadedRootOutcome, SketchExecutionError>,
        RootExecutionObservation,
    ) {
        let compiler = fuel_compiler(root_slice, child_slice);
        let sketch = compiler
            .admit(
                &bytes,
                SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES)
                    .expect("policy"),
            )
            .expect("admission");
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let outcome = runtime
            .run_within(
                async { sketch.execute_threaded_root(runtime.handle()).await },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        let observation = sketch
            .root_execution_observation_for_test()
            .expect("prepared observation after terminal result");
        sketch
            .close_threaded_root()
            .expect("terminal execution releases cache");
        assert_eq!(
            compiler.execution_limits_snapshot(),
            SketchExecutionSnapshot::default()
        );
        (outcome, observation)
    }

    #[test]
    fn finite_fuel_exhausts_the_module_start_before_root_execution() {
        assert_eq!(
            execute_fuel_fixture(start_fuel_fixture(), 1, 1),
            Err(SketchExecutionError::OutOfFuel)
        );
    }

    #[test]
    fn finite_fuel_exhausts_root_start_with_a_typed_error() {
        assert_eq!(
            execute_fuel_fixture(root_fuel_fixture(), 10, 1),
            Err(SketchExecutionError::OutOfFuel)
        );
    }

    #[test]
    fn finite_child_slice_reports_a_typed_ordered_child_outcome() {
        let (outcome, observation) =
            execute_fuel_fixture_with_observation(child_fuel_fixture(), 100, 10);
        assert_eq!(
            outcome,
            Err(SketchExecutionError::ChildOutcomes {
                outcomes: vec![ThreadedChildOutcome {
                    tid: 1,
                    kind: ThreadedChildOutcomeKind::OutOfFuel,
                }],
            })
        );
        // The child Store was sampled after its OutOfFuel terminal path rather
        // than only after a successful ABI entry call.
        assert_eq!(observation.last_child_remaining_fuel, 0);
    }

    #[test]
    fn root_store_is_sampled_after_a_start_section_fuel_failure() {
        let (outcome, observation) =
            execute_fuel_fixture_with_observation(start_fuel_fixture(), 1, 1);
        assert_eq!(outcome, Err(SketchExecutionError::OutOfFuel));
        assert_eq!(observation.last_root_remaining_fuel, 0);
    }

    #[test]
    fn fuel_generation_exhaustion_is_typed_and_never_wraps() {
        let bytes = threaded_yield_fixture();
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
        let sketch = compiler
            .admit(
                &bytes,
                SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES)
                    .expect("policy"),
            )
            .expect("admission");
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        assert_eq!(
            runtime
                .run_within(
                    async { sketch.execute_threaded_root(runtime.handle()).await },
                    std::time::Duration::from_secs(60)
                )
                .expect("test runtime budget"),
            Ok(ThreadedRootOutcome::Started)
        );
        let prepared = sketch
            .prepared_root
            .lock()
            .expect("prepared slot")
            .as_ref()
            .expect("prepared root")
            .clone();
        prepared
            .controller
            .session
            .lock()
            .expect("session")
            .next_fuel_generation = u64::MAX;
        assert_eq!(
            runtime
                .run_within(
                    async { sketch.execute_threaded_root(runtime.handle()).await },
                    std::time::Duration::from_secs(60)
                )
                .expect("test runtime budget"),
            Err(SketchExecutionError::SessionGenerationExhausted)
        );
        let session = prepared.controller.session.lock().expect("session");
        assert_eq!(session.next_fuel_generation, u64::MAX);
        assert!(session.fuel.is_none());
    }

    #[test]
    fn exhausted_child_credit_rejects_before_spawn_and_the_session_is_reusable() {
        let bytes = fuel_rejection_fixture();
        let compiler = fuel_compiler_with_total(110, 100, 10);
        let sketch = compiler
            .admit(
                &bytes,
                SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES)
                    .expect("policy"),
            )
            .expect("admission");
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        for _ in 0..2 {
            assert!(matches!(
                runtime.run_within(async { sketch.execute_threaded_root(runtime.handle()).await }, std::time::Duration::from_secs(60)).expect("test runtime budget"),
                Ok(ThreadedRootOutcome::StartedWithThreadRejections(summary))
                    if summary.fuel() == 1 && summary.capacity() == 0 && summary.closing() == 0
            ));
            let observation = sketch
                .root_execution_observation_for_test()
                .expect("prepared observation");
            assert_eq!(observation.accepted_child_registrations, 0);
            assert_eq!(observation.live_threads, 0);
            assert_eq!(observation.queued_join_handles, 0);
            assert!(observation.last_root_remaining_fuel <= 100);
            assert!(observation.last_child_remaining_fuel <= 10);
        }
    }

    #[test]
    fn two_children_complete_within_the_fixed_fuel_partition() {
        let bytes = fuel_rejection_fixture();
        // 100 credits for the root plus two fixed 10-credit child slices. The
        // root requests exactly two children, so the complete partition is
        // within the configured 120-credit total.
        let compiler = fuel_compiler_with_total(120, 100, 10);
        let sketch = compiler
            .admit(
                &bytes,
                SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES)
                    .expect("policy"),
            )
            .expect("admission");
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        assert_eq!(
            runtime
                .run_within(
                    async { sketch.execute_threaded_root(runtime.handle()).await },
                    std::time::Duration::from_secs(60)
                )
                .expect("test runtime budget"),
            Ok(ThreadedRootOutcome::Started),
        );
        let observation = sketch
            .root_execution_observation_for_test()
            .expect("prepared observation");
        assert_eq!(observation.accepted_child_registrations, 0);
        assert_eq!(observation.live_threads, 0);
        assert_eq!(observation.queued_join_handles, 0);
        assert!(observation.last_root_remaining_fuel <= 100);
        assert!(observation.last_child_remaining_fuel <= 10);
    }

    fn epoch_compiler(deadline: Duration, registrations: usize) -> SketchCompiler {
        let fuel = SketchFuelLimits::new(1_700_000_000_000, 100_000_000_000, 100_000_000_000)
            .expect("high fuel partition");
        let limits = SketchExecutionLimits::default()
            .with_fuel_limits(fuel)
            .expect("fuel")
            .with_epoch_limits(
                SketchEpochLimits::new(deadline, Duration::from_millis(1), registrations)
                    .expect("epoch"),
            )
            .expect("epoch limits");
        SketchCompiler::new(
            SketchCompilerConfig::default()
                .with_execution_limits(limits)
                .expect("config"),
        )
        .expect("compiler")
    }

    fn concurrent_epoch_compiler(deadline: Duration, registrations: usize) -> SketchCompiler {
        let fuel = SketchFuelLimits::new(1_700_000_000_000, 100_000_000_000, 100_000_000_000)
            .expect("high fuel partition");
        let reserved = THREADED_RUST_RESERVATION_BYTES
            .checked_mul(2)
            .expect("two exact reservations");
        let limits = SketchExecutionLimits::new(reserved, 2)
            .expect("two roots")
            .with_fuel_limits(fuel)
            .expect("fuel")
            .with_epoch_limits(
                SketchEpochLimits::new(deadline, Duration::from_millis(1), registrations)
                    .expect("epoch"),
            )
            .expect("epoch limits");
        SketchCompiler::new(
            SketchCompilerConfig::default()
                .with_execution_limits(limits)
                .expect("config"),
        )
        .expect("compiler")
    }

    fn admit_epoch_fixture(compiler: &SketchCompiler, bytes: Vec<u8>) -> Arc<AdmittedSketch> {
        compiler
            .admit(
                &bytes,
                SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES)
                    .expect("policy"),
            )
            .expect("admission")
    }

    #[test]
    fn epoch_deadline_interrupts_looping_module_start_and_root_with_high_fuel() {
        for bytes in [start_fuel_fixture(), root_fuel_fixture()] {
            let compiler = epoch_compiler(Duration::from_millis(5), MAX_GUEST_THREADS_V1 + 1);
            let sketch = admit_epoch_fixture(&compiler, bytes);
            let runtime = crate::async_engine::RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            assert_eq!(
                runtime
                    .run_within(
                        async { sketch.execute_threaded_root(runtime.handle()).await },
                        std::time::Duration::from_secs(60)
                    )
                    .expect("test runtime budget"),
                Err(SketchExecutionError::DeadlineExceeded)
            );
            sketch.close_threaded_root().expect("close");
            assert_eq!(
                compiler.execution_limits_snapshot(),
                SketchExecutionSnapshot::default()
            );
        }
    }

    #[test]
    fn epoch_deadline_releases_every_store_slot_when_root_or_child_observes_it() {
        let compiler = epoch_compiler(Duration::from_millis(5), MAX_GUEST_THREADS_V1 + 1);
        let sketch = admit_epoch_fixture(&compiler, child_fuel_fixture());
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        // The epoch ticker can interrupt either the root while it is spawning the child or
        // the child after it begins running.  Both are the documented deadline result; which
        // store observes the tick is scheduler-dependent, so do not make the test depend on
        // that race.  The cleanup assertions below are the contract under test.
        let result = runtime
            .run_within(
                async { sketch.execute_threaded_root(runtime.handle()).await },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        match result {
            Err(SketchExecutionError::DeadlineExceeded) => {}
            Err(SketchExecutionError::ChildOutcomes { outcomes }) => assert_eq!(
                outcomes,
                vec![ThreadedChildOutcome {
                    tid: 1,
                    kind: ThreadedChildOutcomeKind::DeadlineExceeded,
                }]
            ),
            other => panic!("expected a root or child deadline, got {other:?}"),
        }
        sketch.close_threaded_root().expect("close");
        let snapshot = compiler.execution_limits_snapshot();
        assert_eq!(snapshot.active_epoch_registrations(), 0);
        assert_eq!(snapshot.live_stores(), 0);
        assert_eq!(snapshot.live_instances(), 0);
        assert_eq!(snapshot.active_root_executions(), 0);
    }

    #[test]
    fn cancellation_wins_deadline_and_current_thread_drives_the_blocking_lane() {
        let compiler = epoch_compiler(Duration::from_millis(50), MAX_GUEST_THREADS_V1 + 1);
        let sketch = admit_epoch_fixture(&compiler, root_fuel_fixture());
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime
            .run_within(
                async {
                    let source = crate::async_engine::CancellationSource::new();
                    let task = runtime.handle().launch({
                        let sketch = Arc::clone(&sketch);
                        let token = source.token();
                        let handle = runtime.handle();
                        async move {
                            sketch
                                .execute_threaded_root_cancellable(handle, token)
                                .await
                        }
                    });
                    crate::async_engine::sleep(Duration::from_millis(1)).await;
                    source.cancel();
                    assert_eq!(
                        task.await.expect("join"),
                        Err(SketchExecutionError::Cancelled)
                    );
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        sketch.close_threaded_root().expect("close");
        assert_eq!(
            compiler.execution_limits_snapshot(),
            SketchExecutionSnapshot::default()
        );
    }

    #[test]
    fn multi_thread_runtime_and_unrelated_same_engine_sketch_remain_isolated() {
        // Cancellation, not the deadline, must end the looping sketch. A 50ms
        // deadline raced the unrelated sketch's run on a slow hosted Intel Mac
        // and reported DeadlineExceeded before cancel was ever sent.
        let compiler = concurrent_epoch_compiler(Duration::from_secs(30), MAX_GUEST_THREADS_V1 + 1);
        let looping = admit_epoch_fixture(&compiler, root_fuel_fixture());
        let normal = admit_epoch_fixture(&compiler, threaded_yield_fixture());
        let runtime = crate::async_engine::RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        runtime
            .run_within(
                async {
                    let source = crate::async_engine::CancellationSource::new();
                    let loop_task = runtime.handle().launch({
                        let sketch = Arc::clone(&looping);
                        let token = source.token();
                        let handle = runtime.handle();
                        async move {
                            sketch
                                .execute_threaded_root_cancellable(handle, token)
                                .await
                        }
                    });
                    crate::async_engine::sleep(Duration::from_millis(1)).await;
                    assert_eq!(
                        normal.execute_threaded_root(runtime.handle()).await,
                        Ok(ThreadedRootOutcome::Started)
                    );
                    source.cancel();
                    assert_eq!(
                        loop_task.await.expect("join"),
                        Err(SketchExecutionError::Cancelled)
                    );
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        looping.close_threaded_root().expect("close looping");
        normal.close_threaded_root().expect("close normal");
        assert_eq!(
            compiler.execution_limits_snapshot(),
            SketchExecutionSnapshot::default()
        );
    }

    #[test]
    fn validation_profile_requires_its_metadata_and_rejects_precompile() {
        let bytes = threaded_yield_fixture();
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
        let policy = SketchModulePolicy::threaded_rust_validation_v1_for_test(
            bytes.len() + 1,
            THREADED_RUST_MAX_PAGES,
        )
        .expect("policy");
        let error = match compiler.admit(&bytes, policy) {
            Ok(_) => panic!("metadata is required"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            SketchModuleError::MissingMetadata {
                name: PROFILE_METADATA
            }
        );
        assert_eq!(compiler.compiled_module_count(), 0);
    }

    #[test]
    fn threaded_profile_rejects_generated_abi_metadata_mutations_precompile() {
        let absent = threaded_fixture_with_abi_metadata(None);
        let mut future_abi = ABI_METADATA_VALUE.to_vec();
        replace_metadata_byte(&mut future_abi, b"abi_version = 1\n", b'2');
        let mut schema_skew = ABI_METADATA_VALUE.to_vec();
        replace_metadata_byte(&mut schema_skew, b"schema_revision = 1\n", b'2');
        let mut capability_skew = ABI_METADATA_VALUE.to_vec();
        replace_metadata_byte(&mut capability_skew, b"capabilities=0\n", b'1');
        let operation_skew = String::from_utf8(ABI_METADATA_VALUE.to_vec())
            .unwrap()
            .replace(
                "operation_protocol_revision=11\n",
                "operation_protocol_revision=12\n",
            )
            .into_bytes();
        let malformed = b"capabilities=0\nnot a TOML ABI contract".to_vec();
        let previous_operations = String::from_utf8(ABI_METADATA_VALUE.to_vec())
            .unwrap()
            .replace(
                "operation_protocol_revision=11\n",
                "operation_protocol_revision=10\n",
            )
            .into_bytes();
        assert!(previous_operations
            .windows(b"operation_protocol_revision=10\n".len())
            .any(|window| window == b"operation_protocol_revision=10\n"));
        let legacy_operations = String::from_utf8(ABI_METADATA_VALUE.to_vec())
            .unwrap()
            .replace("operation_protocol_revision=11\n", "");
        let duplicate = {
            let mut bytes = threaded_yield_fixture();
            custom(ABI_METADATA, ABI_METADATA_VALUE, &mut bytes);
            bytes
        };

        let cases = [
            (
                "previous operation protocol",
                threaded_fixture_with_abi_metadata(Some(&previous_operations)),
                SketchModuleError::MetadataMismatch { name: ABI_METADATA },
            ),
            (
                "future operation protocol",
                threaded_fixture_with_abi_metadata(Some(&operation_skew)),
                SketchModuleError::MetadataMismatch { name: ABI_METADATA },
            ),
            (
                "legacy unversioned operation protocol",
                threaded_fixture_with_abi_metadata(Some(legacy_operations.as_bytes())),
                SketchModuleError::MetadataMismatch { name: ABI_METADATA },
            ),
            (
                "absent",
                absent,
                SketchModuleError::MissingMetadata { name: ABI_METADATA },
            ),
            (
                "future ABI version",
                threaded_fixture_with_abi_metadata(Some(&future_abi)),
                SketchModuleError::MetadataMismatch { name: ABI_METADATA },
            ),
            (
                "schema skew",
                threaded_fixture_with_abi_metadata(Some(&schema_skew)),
                SketchModuleError::MetadataMismatch { name: ABI_METADATA },
            ),
            (
                "capability skew",
                threaded_fixture_with_abi_metadata(Some(&capability_skew)),
                SketchModuleError::MetadataMismatch { name: ABI_METADATA },
            ),
            (
                "duplicate",
                duplicate,
                SketchModuleError::DuplicateMetadata { name: ABI_METADATA },
            ),
            (
                "malformed",
                threaded_fixture_with_abi_metadata(Some(&malformed)),
                SketchModuleError::MetadataMismatch { name: ABI_METADATA },
            ),
        ];

        for (case, bytes, expected) in cases {
            let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
            let policy =
                SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES)
                    .expect("policy");
            let error = match compiler.admit(&bytes, policy) {
                Ok(_) => panic!("{case} metadata mutation admitted"),
                Err(error) => error,
            };
            assert_eq!(error, expected, "{case}");
            assert_eq!(compiler.compiled_module_count(), 0, "{case}");
        }
    }

    #[test]
    #[expect(
        clippy::cognitive_complexity,
        clippy::too_many_lines,
        reason = "Existing function complexity measured at ratchet adoption"
    )]
    fn supplied_threaded_artifact_admits_and_executes_the_public_profile() {
        // This is deliberately supplied by the explicit diagnostic workflow:
        // ordinary source-only test runs must neither build a cross-target
        // guest nor turn a missing Soldr materialization into a false green.
        // When present, however, this is the end-to-end acceptance proof for
        // #34: admission is followed by the private root path, not merely a
        // fixture-shaped prelink.
        let Ok(path) = std::env::var("KERNAL_API_THREADED_ARTIFACT_WASM") else {
            // The real Rust guest is assembled only by the explicit diagnostic
            // workflow; normal unit runs remain source-only.
            return;
        };
        let bytes = std::fs::read(path).expect("read threaded artifact");
        let transfer_limits = SketchExecutionLimits::default()
            .with_fuel_limits(
                SketchFuelLimits::new(1_700_000_000_000, 100_000_000_000, 100_000_000_000)
                    .expect("finite transfer fuel"),
            )
            .expect("transfer limits")
            .with_blob_limits(
                SketchBlobLimits::new(64 * 1024, 1024 * 1024, 2 * 1024 * 1024, 2, 2, 4)
                    .unwrap()
                    .with_maximum_transfer_bytes(3 * 1024 * 1024)
                    .unwrap(),
            );
        let manifest = threaded_artifact_manifest_for_test(&bytes).expect("artifact manifest");
        assert_eq!(
            manifest,
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/threaded_rust_artifact_manifest_v1.txt"
            ))
        );
        let compiler = SketchCompiler::new(
            SketchCompilerConfig::default()
                .with_execution_limits(transfer_limits)
                .expect("transfer configuration"),
        )
        .expect("compiler");
        let sketch = compiler
            .admit(
                &bytes,
                SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES)
                    .expect("policy"),
            )
            .expect("public threaded artifact admission");
        assert_eq!(compiler.compiled_module_count(), 1);
        assert_eq!(
            sketch.shared_memory().minimum_pages(),
            THREADED_RUST_INITIAL_PAGES
        );
        assert_eq!(
            sketch.shared_memory().maximum_pages(),
            THREADED_RUST_MAX_PAGES
        );
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let output_directory = tempfile::tempdir().expect("output directory");
        let output_path = output_directory.path().join("exact-output");
        std::fs::write(&output_path, b"original").expect("existing output");
        let outcome = runtime
            .run_within(
                async {
                    sketch
                        .execute_threaded_root_with_output(
                            crate::async_engine::RuntimeHandle::current().expect("handle"),
                            crate::async_engine::CancellationSource::new().token(),
                            output_path.clone(),
                        )
                        .await
                },
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(
            outcome,
            Ok(ThreadedRootOutcome::Started),
            "the real closed-profile artifact must return from its Wasmtime start through the private root path"
        );
        assert_eq!(
            compiler.compiled_module_count(),
            1,
            "execution must reuse admission compilation"
        );
        assert_eq!(
            sketch.execution_limits_snapshot().active_root_executions(),
            0,
            "root execution permit must be released after completion"
        );
        let prepared = sketch
            .prepared_root
            .lock()
            .expect("prepared session")
            .as_ref()
            .expect("root execution prepared one session")
            .clone();
        let observation = sketch
            .root_execution_observation_for_test()
            .expect("prepared observation");
        assert_eq!(observation.preparations, 1);
        // The root yields after joining and both child closures yield.
        assert_eq!(observation.kernel_yields, 3);
        assert_eq!(observation.supplied_runtime_handles, 3);
        assert_eq!(observation.runtime_identity_mismatches, 0);
        assert_eq!(observation.accepted_child_registrations, 0);
        assert_eq!(observation.live_threads, 0);
        assert_eq!(observation.queued_join_handles, 0);
        let operations = observation
            .operation_snapshot
            .expect("root records lifecycle cleanup after the real artifact exits");
        assert_eq!(operations.pending_operations, 0);
        assert_eq!(operations.live_resources, 0);
        assert_eq!(operations.active_clocks, 0);
        // One create and two child uses must each prove a real Pending -> async
        // yield wake -> one terminal poll transition. `Blob::close` is the
        // generated synchronous `stream_close` release, not an operation. The
        // two guest-owned bounded streams add ten terminal operations; their
        // individual submissions may complete before or after waiter
        // registration, so only the suspension count remains a range.
        // Output rejection, successful output completion, and the clock may
        // win before waiter registration; each adds at most one suspension,
        // but exactly one consumed result. The rejected unsealed commit must
        // leave both authorities usable by the subsequent successful retry.
        // The 128 additional blob creates each require the ordinary deferred
        // create transition; their synchronous drops add no suspension.
        assert!(
            (7 + 128 + 4..=10 + 128 + 10).contains(&operations.suspends),
            "{operations:?}"
        );
        // Each of the 128 create/drop iterations consumes one create result;
        // synchronous Drop itself allocates and consumes no operation slot.
        // The two child streams consume 44 results: create, sixteen fills,
        // one scheduler yield, one resumed capacity write, one cancelled
        // write, one read, and seal each. The cancellation itself still has
        // one terminal result, even though it never publishes bytes. The
        // parent's and children's `Blob::close` calls are synchronous
        // `stream_close` releases and consume no operation result.
        assert_eq!(
            operations.resumes,
            11 + 2 * 1024 + 37 + 1 + 1 + 128 + 44,
            "{operations:?}"
        );
        assert_eq!(std::fs::read(&output_path).unwrap(), b"guest exact output");
        assert_eq!(
            std::fs::read_dir(output_directory.path()).unwrap().count(),
            1
        );
        // Each guest child reaches its independent 1 MiB blob capacity before
        // a bounded read releases its awaited write. Scheduling can interleave
        // the two children at any chunk boundary, but the two-blob policy
        // remains a hard ceiling.
        assert!(
            (1024 * 1024..=2 * 1024 * 1024).contains(&operations.peak_buffered_blob_bytes),
            "{operations:?}"
        );
        // The pressure phase overlaps full blobs, pending inputs, and bounded
        // pull results. The policy's 3 MiB combined-transfer cap applies to
        // every scheduling interleaving; measure allocations, not payload.
        // The floor is one child's full blob plus its two chunk allocations:
        // the children may run back to back rather than overlap (CI observed
        // 1_900_544 bytes on Linux x86-64; 2_293_760 when they overlapped).
        assert!(
            (1024 * 1024 + 2 * 64 * 1024..=3 * 1024 * 1024)
                .contains(&operations.peak_retained_transfer_capacity),
            "{operations:?}"
        );
        assert_eq!(operations.retained_transfer_capacity, 0);
        assert_eq!(operations.buffered_blob_bytes, 0);
        assert_eq!(
            *prepared
                .controller
                .threaded_smoke_report
                .lock()
                .expect("threaded smoke report"),
            Some([
                0x4b52_5331, // magic
                1,           // schema version
                48,          // bounded record size
                1,           // ready, acquired before remaining fields
                2,
                2,
                2,
                2,
                2,
                2,
                14,
                2, // bounded streams completed by guest children
            ]),
            "root and both children must publish the shared atomic result"
        );
        let snapshot = sketch.execution_limits_snapshot();
        assert_eq!(snapshot.live_guest_threads(), 0);
        assert_eq!(snapshot.live_stores(), 0);
        assert_eq!(snapshot.live_instances(), 0);
        assert_eq!(snapshot.active_epoch_registrations(), 0);
    }

    #[test]
    fn validation_profile_mutations_reject_before_compilation() {
        let policy = |bytes: usize| {
            SketchModulePolicy::threaded_rust_validation_v1_for_test(
                bytes + 1,
                THREADED_RUST_MAX_PAGES,
            )
            .expect("policy")
        };
        for (metadata, expected) in [
            (
                vec![
                    VALIDATION_PROFILE_METADATA_VALUE,
                    VALIDATION_PROFILE_METADATA_VALUE,
                ],
                SketchModuleError::DuplicateMetadata {
                    name: PROFILE_METADATA,
                },
            ),
            (
                vec![b"wrong-profile"],
                SketchModuleError::MetadataMismatch {
                    name: PROFILE_METADATA,
                },
            ),
        ] {
            let mut bytes = threaded_yield_fixture();
            for value in metadata {
                custom(PROFILE_METADATA, value, &mut bytes);
            }
            let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
            let error = match compiler.admit(&bytes, policy(bytes.len())) {
                Ok(_) => panic!("mutation must reject"),
                Err(error) => error,
            };
            assert_eq!(error, expected);
            assert_eq!(compiler.compiled_module_count(), 0);
        }

        let mut bytes = threaded_yield_fixture();
        custom(
            PROFILE_METADATA,
            VALIDATION_PROFILE_METADATA_VALUE,
            &mut bytes,
        );
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
        let error = match compiler.admit(&bytes, policy(bytes.len())) {
            Ok(_) => panic!("report export is required"),
            Err(error) => error,
        };
        assert!(matches!(error, SketchModuleError::ExportNotAllowed { .. }));
        assert_eq!(compiler.compiled_module_count(), 0);
    }

    fn validation_export_mutation(name: &str, kind: u8, index: u32) -> Vec<u8> {
        let bytes = threaded_yield_fixture();
        let mut section = 8;
        loop {
            if bytes[section] == 7 {
                break;
            }
            let mut at = section + 1;
            let mut length = 0_usize;
            let mut shift = 0;
            loop {
                let byte = bytes[at];
                at += 1;
                length |= usize::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            section = at + length;
        }
        let mut at = section + 1;
        let mut length = 0_usize;
        let mut shift = 0;
        loop {
            let byte = bytes[at];
            at += 1;
            length |= usize::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        let body = &bytes[at..at + length];
        let mut replacement = vec![6];
        replacement.extend_from_slice(&body[1..]);
        text(name, &mut replacement);
        replacement.push(kind);
        leb(index, &mut replacement);
        let mut output = bytes[..section].to_vec();
        output.push(7);
        leb(replacement.len() as u32, &mut output);
        output.extend(replacement);
        output.extend_from_slice(&bytes[at + length..]);
        custom(
            PROFILE_METADATA,
            VALIDATION_PROFILE_METADATA_VALUE,
            &mut output,
        );
        output
    }

    #[test]
    fn validation_report_export_mutations_reject_precompile() {
        let cases = [
            ("wrong-name", 0, 9),
            (VALIDATION_REPORT, 2, 0),
            (VALIDATION_REPORT, 0, 8),
            ("extra", 0, 9),
        ];
        for (name, kind, index) in cases {
            let bytes = validation_export_mutation(name, kind, index);
            let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
            let policy = SketchModulePolicy::threaded_rust_validation_v1_for_test(
                bytes.len() + 1,
                THREADED_RUST_MAX_PAGES,
            )
            .expect("policy");
            assert!(compiler.admit(&bytes, policy).is_err());
            assert_eq!(compiler.compiled_module_count(), 0);
        }
    }

    // Keep import-section mutation local to the raw fixture. It proves the
    // validation profile remains closed before Wasmtime compilation rather
    // than relying on linker failure after an artifact is admitted.
    fn validation_fixture_with_extra_import() -> Vec<u8> {
        let bytes = threaded_yield_fixture();
        let mut section = 8;
        while bytes[section] != 2 {
            let mut at = section + 1;
            while bytes[at] & 0x80 != 0 {
                at += 1;
            }
            let length = usize::from(bytes[at] & 0x7f);
            section = at + 1 + length;
        }
        let mut body_at = section + 1;
        let mut length = 0_usize;
        let mut shift = 0;
        loop {
            let byte = bytes[body_at];
            body_at += 1;
            length |= usize::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        let end = body_at + length;
        let mut body = bytes[body_at..end].to_vec();
        body[0] = 10; // the raw fixture has nine imports; add one.
        let mut extra = Vec::new();
        text("unexpected", &mut extra);
        text("import", &mut extra);
        extra.extend([0, 0]);
        body.extend(extra);
        let mut bytes = bytes[..section].to_vec();
        bytes.push(2);
        leb(body.len() as u32, &mut bytes);
        bytes.extend(body);
        bytes.extend_from_slice(&threaded_yield_fixture()[end..]);
        custom(
            PROFILE_METADATA,
            VALIDATION_PROFILE_METADATA_VALUE,
            &mut bytes,
        );
        bytes
    }

    #[test]
    fn validation_extra_import_rejects_before_compilation() {
        let bytes = validation_fixture_with_extra_import();
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("compiler");
        let policy = SketchModulePolicy::threaded_rust_validation_v1_for_test(
            bytes.len() + 1,
            THREADED_RUST_MAX_PAGES,
        )
        .expect("policy");
        let error = match compiler.admit(&bytes, policy) {
            Ok(_) => panic!("extra import admitted"),
            Err(error) => error,
        };
        assert!(
            matches!(error, SketchModuleError::ForbiddenImport { .. }),
            "unexpected rejection: {error:?}"
        );
        assert_eq!(compiler.compiled_module_count(), 0);
    }

    fn leb(mut value: u32, output: &mut Vec<u8>) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            output.push(byte);
            if value == 0 {
                return;
            }
        }
    }
    fn text(value: &str, output: &mut Vec<u8>) {
        leb(value.len() as u32, output);
        output.extend(value.as_bytes());
    }
    fn section(id: u8, body: Vec<u8>, output: &mut Vec<u8>) {
        output.push(id);
        leb(body.len() as u32, output);
        output.extend(body);
    }
    fn custom(name: &str, value: &[u8], output: &mut Vec<u8>) {
        let mut body = Vec::new();
        text(name, &mut body);
        body.extend(value);
        section(0, body, output);
    }

    fn encoded_custom(name: &str, value: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        custom(name, value, &mut output);
        output
    }

    fn threaded_fixture_with_abi_metadata(value: Option<&[u8]>) -> Vec<u8> {
        let mut bytes = threaded_yield_fixture();
        let original = encoded_custom(ABI_METADATA, ABI_METADATA_VALUE);
        assert!(
            bytes.ends_with(&original),
            "fixture ABI metadata is terminal"
        );
        bytes.truncate(bytes.len() - original.len());
        if let Some(value) = value {
            custom(ABI_METADATA, value, &mut bytes);
        }
        bytes
    }

    fn replace_metadata_byte(metadata: &mut [u8], declaration: &[u8], replacement: u8) {
        let declaration_at = metadata
            .windows(declaration.len())
            .position(|window| window == declaration)
            .expect("generated metadata declaration");
        metadata[declaration_at + declaration.len() - 2] = replacement;
    }

    // Minimal exact ThreadedRustV1 profile whose start calls kernel_yield.
    // Keeping it in this private unit module prevents test instrumentation
    // from becoming a public sketch-host API.
    pub(super) fn threaded_yield_fixture() -> Vec<u8> {
        let mut wasm = b"\0asm\x01\0\0\0".to_vec();
        let mut types = Vec::new();
        leb(9, &mut types);
        types.extend([
            0x60, 0, 0, 0x60, 1, 0x7f, 1, 0x7f, 0x60, 3, 0x7f, 0x7e, 0x7f, 1, 0x7f, 0x60, 2, 0x7f,
            0x7f, 1, 0x7f, 0x60, 4, 0x7f, 0x7f, 0x7f, 0x7f, 1, 0x7f, 0x60, 1, 0x7f, 0, 0x60, 0, 1,
            0x7f, 0x60, 0, 1, 0x7f, 0x60, 2, 0x7f, 0x7f, 0,
        ]);
        section(1, types, &mut wasm);
        let mut imports = Vec::new();
        leb(9, &mut imports);
        text("env", &mut imports);
        text("memory", &mut imports);
        imports.extend([2, 3]);
        leb(17, &mut imports);
        leb(16_384, &mut imports);
        text(ABI_MODULE, &mut imports);
        text(ABI_YIELD, &mut imports);
        imports.extend([0, 0]);
        text(THREAD_MODULE, &mut imports);
        text(THREAD_SPAWN, &mut imports);
        imports.extend([0, 1]);
        for (name, ty) in [
            ("clock_time_get", 2),
            ("environ_get", 3),
            ("environ_sizes_get", 3),
            ("fd_write", 4),
            ("proc_exit", 5),
            ("sched_yield", 6),
        ] {
            text("wasi_snapshot_preview1", &mut imports);
            text(name, &mut imports);
            imports.push(0);
            leb(ty, &mut imports);
        }
        section(2, imports, &mut wasm);
        let mut functions = Vec::new();
        leb(5, &mut functions);
        for ty in [0, 7, 8, 7, 0] {
            leb(ty, &mut functions);
        }
        section(3, functions, &mut wasm);
        let mut exports = Vec::new();
        leb(5, &mut exports);
        text("memory", &mut exports);
        exports.push(2);
        leb(0, &mut exports);
        for (name, index) in [
            ("_start", 8),
            ("__main_void", 9),
            ("wasi_thread_start", 10),
            (ENTRY, 11),
        ] {
            text(name, &mut exports);
            exports.push(0);
            leb(index, &mut exports);
        }
        section(7, exports, &mut wasm);
        section(8, vec![12], &mut wasm);
        let mut code = Vec::new();
        leb(5, &mut code);
        code.extend([
            2, 0, 0x0b, 4, 0, 0x41, 0, 0x0b, 2, 0, 0x0b, 4, 0, 0x41, 0, 0x0b, 4, 0, 0x10, 0, 0x0b,
        ]);
        section(10, code, &mut wasm);
        let mut features = Vec::new();
        let names = [
            "atomics",
            "bulk-memory",
            "bulk-memory-opt",
            "call-indirect-overlong",
            "extended-const",
            "multivalue",
            "mutable-globals",
            "nontrapping-fptoint",
            "reference-types",
            "sign-ext",
        ];
        leb(names.len() as u32, &mut features);
        for name in names {
            features.push(b'+');
            text(name, &mut features);
        }
        custom("target_features", &features, &mut wasm);
        custom(ABI_METADATA, ABI_METADATA_VALUE, &mut wasm);
        wasm
    }

    fn threaded_cap_fixture() -> Vec<u8> {
        let bytes = threaded_yield_fixture();
        let mut section_offset = 8;
        loop {
            let id = bytes[section_offset];
            let mut body_at = section_offset + 1;
            let mut length = 0_usize;
            let mut shift = 0;
            loop {
                let byte = bytes[body_at];
                body_at += 1;
                length |= usize::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            let end = body_at + length;
            if id == 10 {
                let mut code = Vec::new();
                leb(5, &mut code);
                // `_start`: first spawn must be positive, second exactly -1;
                // the child exits normally after the host has accepted it.
                code.extend([
                    32, 0, 0x41, 0, 0x10, 1, 0x41, 0, 0x4a, 0x45, 0x04, 0x40, 0x41, 6, 0x10, 6,
                    0x0b, 0x41, 1, 0x10, 1, 0x41, 0x7f, 0x46, 0x45, 0x04, 0x40, 0x41, 7, 0x10, 6,
                    0x0b, 0x0b, // root
                    4, 0, 0x41, 0, 0x0b, // __main_void
                    6, 0, 0x20, 1, 0x10, 6, 0x0b, // wasi_thread_start
                    4, 0, 0x41, 0, 0x0b, // kernal-api-run
                    2, 0, 0x0b, // unexported module start
                ]);
                let mut output = bytes[..section_offset].to_vec();
                section(10, code, &mut output);
                output.extend_from_slice(&bytes[end..]);
                return output;
            }
            section_offset = end;
        }
    }

    fn threaded_code_fixture(bodies: [Vec<u8>; 5]) -> Vec<u8> {
        let bytes = threaded_yield_fixture();
        let mut section_offset = 8;
        loop {
            let id = bytes[section_offset];
            let mut body_at = section_offset + 1;
            let mut length = 0_usize;
            let mut shift = 0;
            loop {
                let byte = bytes[body_at];
                body_at += 1;
                length |= usize::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            let end = body_at + length;
            if id == 10 {
                let mut code = Vec::new();
                leb(bodies.len() as u32, &mut code);
                for body in bodies {
                    leb(body.len() as u32, &mut code);
                    code.extend(body);
                }
                let mut output = bytes[..section_offset].to_vec();
                section(10, code, &mut output);
                output.extend_from_slice(&bytes[end..]);
                return output;
            }
            section_offset = end;
        }
    }

    fn empty_body() -> Vec<u8> {
        vec![0, 0x0b]
    }

    fn i32_zero_body() -> Vec<u8> {
        vec![0, 0x41, 0, 0x0b]
    }

    fn yield_body() -> Vec<u8> {
        vec![0, 0x10, 0, 0x0b]
    }

    fn infinite_loop_body() -> Vec<u8> {
        // `loop { br 0 }`; the explicit branch makes every iteration consume
        // Wasmtime fuel rather than relying on host-side timing.
        vec![0, 0x03, 0x40, 0x0c, 0, 0x0b, 0x0b]
    }

    pub(super) fn atomic_wait_fixture() -> Vec<u8> {
        // Signal the subprocess parent through the closed yield import, then
        // execute `memory.atomic.wait32(0, 0, -1)`. This never runs in the
        // parent process; its reaping boundary belongs to #28.
        threaded_code_fixture([
            vec![
                0, 0x10, 0, 0x41, 0, 0x41, 0, 0x42, 0x7f, 0xfe, 0x01, 0x02, 0, 0x1a, 0x0b,
            ],
            i32_zero_body(),
            empty_body(),
            i32_zero_body(),
            empty_body(),
        ])
    }

    fn start_fuel_fixture() -> Vec<u8> {
        threaded_code_fixture([
            empty_body(),
            i32_zero_body(),
            empty_body(),
            i32_zero_body(),
            infinite_loop_body(),
        ])
    }

    #[test]
    fn instantiation_trap_revokes_output_and_records_final_cleanup() {
        let bytes = threaded_code_fixture([
            empty_body(),
            i32_zero_body(),
            empty_body(),
            i32_zero_body(),
            vec![0, 0x00, 0x0b], // unreachable in the module start section
        ]);
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).unwrap();
        let sketch = compiler
            .admit(
                &bytes,
                SketchModulePolicy::threaded_rust_v1(bytes.len() + 1, THREADED_RUST_MAX_PAGES)
                    .unwrap(),
            )
            .unwrap();
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("unchanged");
        std::fs::write(&destination, b"original").unwrap();
        let result = runtime
            .run_within(
                sketch.execute_threaded_root_with_output(
                    runtime.handle(),
                    crate::async_engine::CancellationSource::new().token(),
                    destination.clone(),
                ),
                std::time::Duration::from_secs(60),
            )
            .expect("test runtime budget");
        assert_eq!(result, Err(SketchExecutionError::Trapped));
        let prepared = sketch
            .prepared_root
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone();
        let snapshot = prepared
            .controller
            .operation_snapshot
            .lock()
            .unwrap()
            .expect("instantiation failures must finalize the hub");
        assert_eq!(snapshot.live_resources, 0);
        assert_eq!(snapshot.pending_operations, 0);
        assert_eq!(std::fs::read(&destination).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    fn root_fuel_fixture() -> Vec<u8> {
        threaded_code_fixture([
            infinite_loop_body(),
            i32_zero_body(),
            empty_body(),
            i32_zero_body(),
            yield_body(),
        ])
    }

    fn child_fuel_fixture() -> Vec<u8> {
        threaded_code_fixture([
            // `_start`: ask the closed host ABI for one child and discard its
            // positive TID. The child entry below then burns its own slice.
            vec![0, 0x41, 0, 0x10, 1, 0x1a, 0x0b],
            i32_zero_body(),
            infinite_loop_body(),
            i32_zero_body(),
            yield_body(),
        ])
    }

    fn fuel_rejection_fixture() -> Vec<u8> {
        threaded_code_fixture([
            // `_start`: the first child consumes the only configured child
            // slice; the second must receive `-1` before it gets a TID,
            // Store, Instance, or JoinHandle.
            vec![0, 0x41, 0, 0x10, 1, 0x1a, 0x41, 0, 0x10, 1, 0x1a, 0x0b],
            i32_zero_body(),
            empty_body(),
            i32_zero_body(),
            yield_body(),
        ])
    }
}

fn write_shared(memory: &SharedMemory, offset: i32, bytes: &[u8]) -> i32 {
    let Some(cells) = shared_range(memory, offset, bytes.len()) else {
        return ERRNO_FAULT;
    };
    for (cell, byte) in cells.iter().zip(bytes) {
        // SAFETY: UnsafeCell<u8> has alignment 1, matching AtomicU8. The
        // SharedMemory owns and pins this backing allocation for its lifetime,
        // and every host access to concurrently reachable guest bytes in this
        // module uses AtomicU8 (never a plain dereference).
        unsafe { AtomicU8::from_ptr(cell.get()) }.store(*byte, Ordering::Relaxed);
    }
    ERRNO_SUCCESS
}

fn validate_iovecs(memory: &SharedMemory, iovecs: i32, iovecs_len: i32) -> i32 {
    let Ok(iovecs_len) = usize::try_from(iovecs_len) else {
        return ERRNO_FAULT;
    };
    if iovecs_len > MAX_P1_IOVECS {
        return ERRNO_FAULT;
    }
    let Some(descriptor_bytes) = iovecs_len.checked_mul(8) else {
        return ERRNO_FAULT;
    };
    if shared_range(memory, iovecs, descriptor_bytes).is_none() {
        return ERRNO_FAULT;
    }
    let base = match usize::try_from(iovecs) {
        Ok(base) => base,
        Err(_) => return ERRNO_FAULT,
    };
    for index in 0..iovecs_len {
        let Some(offset) = index
            .checked_mul(8)
            .and_then(|delta| base.checked_add(delta))
        else {
            return ERRNO_FAULT;
        };
        let Ok(offset) = i32::try_from(offset) else {
            return ERRNO_FAULT;
        };
        let Some(payload) = read_shared_u32(memory, offset) else {
            return ERRNO_FAULT;
        };
        let Some(length) = read_shared_u32(memory, offset.saturating_add(4)) else {
            return ERRNO_FAULT;
        };
        if shared_range(memory, payload as i32, length as usize).is_none() {
            return ERRNO_FAULT;
        }
    }
    ERRNO_SUCCESS
}

fn read_shared_u32(memory: &SharedMemory, offset: i32) -> Option<u32> {
    read_shared_u32_with_ordering(memory, offset, Ordering::Relaxed)
}

fn read_shared_u32_with_ordering(
    memory: &SharedMemory,
    offset: i32,
    ordering: Ordering,
) -> Option<u32> {
    let cells = shared_range(memory, offset, 4)?;
    let mut bytes = [0_u8; 4];
    for (destination, cell) in bytes.iter_mut().zip(cells) {
        // SAFETY: UnsafeCell<u8> has AtomicU8-compatible alignment and belongs
        // to the live SharedMemory. This module's host-side shared-byte access
        // invariant is atomic-only; AtomicU8 avoids a plain concurrent load.
        *destination = unsafe { AtomicU8::from_ptr(cell.get()) }.load(ordering);
    }
    Some(u32::from_le_bytes(bytes))
}

fn load_shared_atomic_u32(cells: &[UnsafeCell<u8>], ordering: Ordering) -> Option<u32> {
    if cells.len() != 4 {
        return None;
    }
    let pointer = cells.as_ptr().cast::<AtomicU32>();
    if pointer.addr() % align_of::<AtomicU32>() != 0 {
        return None;
    }
    // SAFETY: callers provide exactly four live SharedMemory cells and the
    // computed backing address is checked for AtomicU32 alignment. The shared
    // report protocol permits only atomic accesses; this reference is used for
    // one load and never escapes the function.
    Some(unsafe { (&*pointer).load(ordering) })
}

// Validation reports are intentionally private: the guest returns only a
// bounded offset and the facade consumes the fixed 64-byte schema itself.
fn validate_report(memory: &SharedMemory, offset: i32) -> Result<(), SketchExecutionError> {
    if offset < 0 || offset % 4 != 0 || shared_range(memory, offset, 64).is_none() {
        return Err(SketchExecutionError::ValidationReportInvalid);
    }
    let load = |word: i32, ordering| -> Option<u32> {
        let cells = shared_range(memory, offset.checked_add(word.checked_mul(4)?)?, 4)?;
        load_shared_atomic_u32(cells, ordering)
    };
    // The guest stores ready with Release after all record fields; acquire it
    // before copying the remaining words.
    if load(3, Ordering::Acquire) != Some(1) {
        return Err(SketchExecutionError::ValidationReportInvalid);
    }
    let words: Option<Vec<u32>> = (0..16).map(|word| load(word, Ordering::Relaxed)).collect();
    let Some(words) = words else {
        return Err(SketchExecutionError::ValidationReportInvalid);
    };
    if words[0] != 0x4b_52_56_31
        || words[1] != 1
        || words[2] != 64
        || words[4] != 2
        || words[5] != 2
        || words[6] != 2
        || words[7] != 2
        || words[8] != 2
        || words[9] != 2
        || words[10] != 2
        || words[11] != 2
        || words[12] != 0
        || words[13] != 0
        || words[14] != 0
        || words[15] != 0
    {
        return Err(SketchExecutionError::ValidationReportInvalid);
    }
    Ok(())
}

#[cfg(test)]
fn capture_threaded_smoke_report(
    memory: &SharedMemory,
    iovecs: i32,
    iovecs_len: i32,
) -> Option<[u32; 12]> {
    const MAGIC: u32 = 0x4b52_5331; // "KRS1"
    const VERSION: u32 = 1;
    const BYTES: u32 = 48;
    const WORDS: usize = 12;
    if iovecs_len != 1 {
        return None;
    }
    let record_offset = read_shared_u32(memory, iovecs)?;
    if read_shared_u32(memory, iovecs.checked_add(4)?)? != BYTES || record_offset > i32::MAX as u32
    {
        return None;
    }
    let record_offset = record_offset as i32;
    let word = |index: usize, ordering| {
        let offset = record_offset.checked_add(i32::try_from(index * 4).ok()?)?;
        load_shared_atomic_u32(shared_range(memory, offset, 4)?, ordering)
    };
    if word(0, Ordering::Relaxed) != Some(MAGIC)
        || word(1, Ordering::Relaxed) != Some(VERSION)
        || word(2, Ordering::Relaxed) != Some(BYTES)
        || word(3, Ordering::Acquire) != Some(1)
    {
        return None;
    }
    let mut report = [0_u32; WORDS];
    for (index, value) in report.iter_mut().enumerate() {
        *value = word(index, Ordering::Relaxed)?;
    }
    Some(report)
}

mod compiler_dispatch;
#[cfg(feature = "wasm-component-compiler-experiment")]
mod component_compiler;
mod hash_dispatch;

#[cfg(test)]
mod validation_report_tests {
    use super::*;

    fn memory() -> SharedMemory {
        let compiler = SketchCompiler::new(SketchCompilerConfig::default()).expect("engine");
        SharedMemory::new(&compiler.engine, MemoryType::shared(17, 16_384)).expect("memory")
    }

    fn write_report(memory: &SharedMemory, offset: i32, words: [u32; 16]) {
        for (index, value) in words.into_iter().enumerate() {
            let cells = shared_range(memory, offset + (index as i32 * 4), 4).expect("range");
            // SAFETY: test offsets are aligned/in-range and this helper only
            // writes the atomic record representation used by validate_report.
            unsafe { (&*cells.as_ptr().cast::<AtomicU32>()).store(value, Ordering::Release) };
        }
    }

    fn valid() -> [u32; 16] {
        [0x4b_52_56_31, 1, 64, 1, 2, 2, 2, 2, 2, 2, 2, 2, 0, 0, 0, 0]
    }

    #[test]
    fn validation_report_rejects_bad_pointer_and_schema() {
        let memory = memory();
        assert_eq!(
            validate_report(&memory, -1),
            Err(SketchExecutionError::ValidationReportInvalid)
        );
        assert_eq!(
            validate_report(&memory, 2),
            Err(SketchExecutionError::ValidationReportInvalid)
        );
        assert_eq!(
            validate_report(&memory, i32::MAX - 2),
            Err(SketchExecutionError::ValidationReportInvalid)
        );
        write_report(&memory, 0, valid());
        assert_eq!(validate_report(&memory, 0), Ok(()));
        for index in [0, 1, 2, 3, 12, 13, 14, 15] {
            let mut words = valid();
            words[index] = if index == 3 { 0 } else { 9 };
            write_report(&memory, 0, words);
            assert_eq!(
                validate_report(&memory, 0),
                Err(SketchExecutionError::ValidationReportInvalid)
            );
        }
    }

    #[test]
    fn atomic_report_load_rejects_a_misaligned_computed_address() {
        let bytes: [UnsafeCell<u8>; 8] = std::array::from_fn(|_| UnsafeCell::new(0));
        let base = bytes.as_ptr().addr();
        let misaligned = (0..4)
            .find(|offset| !(base + offset).is_multiple_of(align_of::<AtomicU32>()))
            .expect("one of four adjacent byte addresses is misaligned");
        assert_eq!(
            load_shared_atomic_u32(&bytes[misaligned..misaligned + 4], Ordering::Relaxed),
            None
        );
    }
}

fn shared_range(
    memory: &SharedMemory,
    offset: i32,
    length: usize,
) -> Option<&[std::cell::UnsafeCell<u8>]> {
    let offset = usize::try_from(offset).ok()?;
    let end = offset.checked_add(length)?;
    memory.data().get(offset..end)
}

fn epoch_error(epoch: &LogicalEpoch) -> Option<SketchExecutionError> {
    match epoch.winner.load(Ordering::Acquire) {
        EPOCH_CANCELLED => Some(SketchExecutionError::Cancelled),
        EPOCH_DEADLINE_EXCEEDED => Some(SketchExecutionError::DeadlineExceeded),
        _ => None,
    }
}

fn map_root_error(
    error: &wasmtime::Error,
    epoch: &LogicalEpoch,
) -> Result<ThreadedRootOutcome, SketchExecutionError> {
    if matches!(
        error.downcast_ref::<wasmtime::Trap>(),
        Some(wasmtime::Trap::OutOfFuel)
    ) {
        return Err(SketchExecutionError::OutOfFuel);
    }
    if let Some(error) = epoch_error(epoch) {
        return Err(error);
    }
    if let Some(exit) = error.downcast_ref::<ProcExitSentinel>() {
        return if exit.0 == 0 {
            Ok(ThreadedRootOutcome::Exited)
        } else {
            Err(SketchExecutionError::NonzeroExit { code: exit.0 })
        };
    }
    Err(SketchExecutionError::Trapped)
}

fn map_child_error(error: &wasmtime::Error, epoch: &LogicalEpoch) -> ChildOutcome {
    if matches!(
        error.downcast_ref::<wasmtime::Trap>(),
        Some(wasmtime::Trap::OutOfFuel)
    ) {
        return ChildOutcome::OutOfFuel;
    }
    match epoch.winner.load(Ordering::Acquire) {
        EPOCH_CANCELLED => return ChildOutcome::Cancelled,
        EPOCH_DEADLINE_EXCEEDED => return ChildOutcome::DeadlineExceeded,
        _ => {}
    }
    if let Some(exit) = error.downcast_ref::<ProcExitSentinel>() {
        return if exit.0 == 0 {
            ChildOutcome::Exited
        } else {
            ChildOutcome::NonzeroExit(exit.0)
        };
    }
    ChildOutcome::Trapped
}

fn resolve_threaded_result(
    root: Result<ThreadedRootOutcome, SketchExecutionError>,
    children: Result<(), SketchExecutionError>,
    report: Result<(), SketchExecutionError>,
    rejections: ThreadSpawnRejectionSummary,
) -> Result<ThreadedRootOutcome, SketchExecutionError> {
    // All callers already drained children before this pure precedence step.
    // Root remains primary; ordered child aggregation is next; report schema
    // failures are only meaningful after successful guest execution.
    let outcome = root?;
    children?;
    report?;
    Ok(match outcome {
        ThreadedRootOutcome::Started if !rejections.is_empty() => {
            ThreadedRootOutcome::StartedWithThreadRejections(rejections)
        }
        ThreadedRootOutcome::Exited if !rejections.is_empty() => {
            ThreadedRootOutcome::ExitedWithThreadRejections(rejections)
        }
        outcome => outcome,
    })
}

#[cfg(test)]
mod result_precedence_tests {
    use super::*;

    fn child_error() -> SketchExecutionError {
        SketchExecutionError::ChildOutcomes {
            outcomes: vec![
                ThreadedChildOutcome {
                    tid: 1,
                    kind: ThreadedChildOutcomeKind::Trapped,
                },
                ThreadedChildOutcome {
                    tid: 2,
                    kind: ThreadedChildOutcomeKind::NonzeroExit { code: 9 },
                },
            ],
        }
    }

    #[test]
    fn root_child_and_report_precedence_is_semantic() {
        assert_eq!(
            resolve_threaded_result(
                Err(SketchExecutionError::NonzeroExit { code: 7 }),
                Err(child_error()),
                Err(SketchExecutionError::ValidationReportInvalid),
                ThreadSpawnRejectionSummary::default(),
            ),
            Err(SketchExecutionError::NonzeroExit { code: 7 }),
        );
        assert_eq!(
            resolve_threaded_result(
                Ok(ThreadedRootOutcome::Started),
                Err(child_error()),
                Err(SketchExecutionError::ValidationReportInvalid),
                ThreadSpawnRejectionSummary::default(),
            ),
            Err(child_error()),
        );
        assert_eq!(
            resolve_threaded_result(
                Ok(ThreadedRootOutcome::Started),
                Ok(()),
                Err(SketchExecutionError::ValidationReportInvalid),
                ThreadSpawnRejectionSummary::default(),
            ),
            Err(SketchExecutionError::ValidationReportInvalid),
        );
    }

    #[test]
    fn thread_rejection_summary_surfaces_only_after_primary_success() {
        let summary = ThreadSpawnRejectionSummary {
            capacity: 1,
            closing: 0,
            fuel: 0,
            epoch: 0,
        };
        assert_eq!(
            resolve_threaded_result(Ok(ThreadedRootOutcome::Started), Ok(()), Ok(()), summary,),
            Ok(ThreadedRootOutcome::StartedWithThreadRejections(summary)),
        );
        assert_eq!(
            resolve_threaded_result(
                Err(SketchExecutionError::NonzeroExit { code: 7 }),
                Ok(()),
                Ok(()),
                summary,
            ),
            Err(SketchExecutionError::NonzeroExit { code: 7 }),
        );
    }

    #[test]
    fn fuel_exhaustion_is_mapped_without_exposing_a_wasmtime_trap() {
        let error = wasmtime::Error::new(wasmtime::Trap::OutOfFuel);
        let epoch = LogicalEpoch {
            cancellation: crate::async_engine::CancellationSource::new().token(),
            deadline: Instant::now() + Duration::from_secs(1),
            winner: AtomicU8::new(EPOCH_PENDING),
            operations: Mutex::new(None),
        };
        assert_eq!(
            map_root_error(&error, &epoch),
            Err(SketchExecutionError::OutOfFuel)
        );
        assert_eq!(map_child_error(&error, &epoch), ChildOutcome::OutOfFuel);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SketchCompilerError {
    #[error("the Wasm stack limit must be nonzero")]
    InvalidStackLimit,
    #[error("execution limits must admit one 1 GiB threaded Rust session and one root")]
    InvalidExecutionLimits,
    #[error("epoch limits must have a nonzero deadline, tick, and quota")]
    InvalidEpochLimits,
    #[error("fuel limits must reserve nonzero root and all bounded child slices")]
    InvalidFuelLimits,
    #[error("the sketch compiler is unavailable")]
    Unavailable,
}
/// Stable bounded diagnostics; no parser or engine message crosses this facade.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SketchModuleError {
    ModuleTooLarge {
        actual_bytes: usize,
        maximum_bytes: usize,
    },
    InvalidBinary,
    InvalidModuleLimit,
    InvalidSharedMemoryLimit,
    InvalidThreadLimit,
    ThreadLimitExceedsV1Maximum {
        requested: usize,
        maximum: usize,
    },
    ForbiddenImport {
        module: String,
        name: String,
    },
    ImportTypeMismatch {
        module: String,
        name: String,
    },
    MissingRequiredImport {
        module: &'static str,
        name: &'static str,
    },
    MissingSharedMemory,
    MultipleMemoryImports,
    DefinedMemoryForbidden,
    UnsharedMemory,
    Memory64,
    UnsupportedMemoryPageSize,
    SharedMemoryWithoutMaximum,
    MemoryInitialExceedsMaximum {
        minimum_pages: u64,
        maximum_pages: u64,
    },
    SharedMemoryExceedsPolicy {
        minimum_pages: u64,
        maximum_pages: u64,
        policy_pages: u32,
    },
    ThreadedMemoryMismatch {
        minimum_pages: u32,
        maximum_pages: u32,
    },
    MissingMetadata {
        name: &'static str,
    },
    DuplicateMetadata {
        name: &'static str,
    },
    MetadataMismatch {
        name: &'static str,
    },
    MetadataTooLarge {
        name: &'static str,
    },
    StartFunctionForbidden,
    ExportNotAllowed {
        name: String,
    },
    EntrypointMismatch,
    ForbiddenCustomSection {
        name: String,
    },
    MissingTargetFeatures,
    TargetFeaturesMismatch,
    StartMismatch,
    MemoryExportMismatch,
}
impl SketchModuleError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ModuleTooLarge { .. } => "module-too-large",
            Self::InvalidBinary => "invalid-binary",
            Self::InvalidModuleLimit => "invalid-module-limit",
            Self::InvalidSharedMemoryLimit => "invalid-shared-memory-limit",
            Self::InvalidThreadLimit => "invalid-thread-limit",
            Self::ThreadLimitExceedsV1Maximum { .. } => "thread-limit-exceeds-v1-maximum",
            Self::ForbiddenImport { .. } => "forbidden-import",
            Self::ImportTypeMismatch { .. } => "import-type-mismatch",
            Self::MissingRequiredImport { .. } => "missing-required-import",
            Self::MissingSharedMemory => "missing-shared-memory",
            Self::MultipleMemoryImports => "multiple-memory-imports",
            Self::DefinedMemoryForbidden => "defined-memory-forbidden",
            Self::UnsharedMemory => "unshared-memory",
            Self::Memory64 => "memory64",
            Self::UnsupportedMemoryPageSize => "unsupported-memory-page-size",
            Self::SharedMemoryWithoutMaximum => "shared-memory-without-maximum",
            Self::MemoryInitialExceedsMaximum { .. } => "memory-initial-exceeds-maximum",
            Self::SharedMemoryExceedsPolicy { .. } => "shared-memory-exceeds-policy",
            Self::ThreadedMemoryMismatch { .. } => "threaded-memory-mismatch",
            Self::MissingMetadata { .. } => "missing-metadata",
            Self::DuplicateMetadata { .. } => "duplicate-metadata",
            Self::MetadataMismatch { .. } => "metadata-mismatch",
            Self::MetadataTooLarge { .. } => "metadata-too-large",
            Self::StartFunctionForbidden => "start-function-forbidden",
            Self::ExportNotAllowed { .. } => "export-not-allowed",
            Self::EntrypointMismatch => "entrypoint-mismatch",
            Self::ForbiddenCustomSection { .. } => "forbidden-custom-section",
            Self::MissingTargetFeatures => "missing-target-features",
            Self::TargetFeaturesMismatch => "target-features-mismatch",
            Self::StartMismatch => "start-mismatch",
            Self::MemoryExportMismatch => "memory-export-mismatch",
        }
    }
}
impl fmt::Display for SketchModuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sketch admission failed ({})", self.code())
    }
}
impl std::error::Error for SketchModuleError {}

/// Deterministically describes the semantic sections of an untrusted Wasm
/// artifact for the threaded-smoke RED test. This is intentionally an
/// inspection seam, not a loader or an execution API.
#[cfg(test)]
fn threaded_artifact_manifest_for_test(bytes: &[u8]) -> Result<String, SketchModuleError> {
    let mut facts = Vec::new();
    let mut types = Vec::<TypeEntry>::new();
    let mut imported_function_types = Vec::<u32>::new();
    let mut functions = Vec::<u32>::new();
    let mut exports = Vec::<(String, ExternalKind, u32)>::new();
    let mut start = None;
    let mut type_index = 0_u32;
    for item in Parser::new(0).parse_all(bytes) {
        match item.map_err(|_| SketchModuleError::InvalidBinary)? {
            Payload::TypeSection(reader) => {
                for group in reader {
                    let group = group.map_err(|_| SketchModuleError::InvalidBinary)?;
                    for ty in group.types() {
                        let fact =
                            if let CompositeInnerType::Func(function) = &ty.composite_type.inner {
                                types.push(TypeEntry::Function {
                                    params: function.params().to_vec(),
                                    results: function.results().to_vec(),
                                });
                                format!(
                                "type index={type_index} kind=function params={:?} results={:?}",
                                function.params(),
                                function.results(),
                            )
                            } else {
                                types.push(TypeEntry::NonFunction);
                                format!("type index={type_index} kind=non-function")
                            };
                        facts.push(fact);
                        type_index += 1;
                    }
                }
            }
            Payload::ImportSection(reader) => {
                for import in reader.into_imports() {
                    let import = import.map_err(|_| SketchModuleError::InvalidBinary)?;
                    facts.push(format!(
                        "import module={} name={} kind={}",
                        bounded(import.module),
                        bounded(import.name),
                        import_kind(import.ty),
                    ));
                    if let TypeRef::Func(index) | TypeRef::FuncExact(index) = import.ty {
                        imported_function_types.push(index);
                    }
                }
            }
            Payload::FunctionSection(reader) => {
                for index in reader {
                    functions.push(index.map_err(|_| SketchModuleError::InvalidBinary)?);
                }
            }
            Payload::MemorySection(reader) => {
                for memory in reader {
                    let memory = memory.map_err(|_| SketchModuleError::InvalidBinary)?;
                    facts.push(memory_fact(
                        "defined-memory",
                        memory.initial,
                        memory.maximum,
                        memory.shared,
                        memory.memory64,
                        memory.page_size_log2,
                    ));
                }
            }
            Payload::ExportSection(reader) => {
                for export in reader {
                    let export = export.map_err(|_| SketchModuleError::InvalidBinary)?;
                    exports.push((bounded(export.name), export.kind, export.index));
                }
            }
            Payload::StartSection { func, .. } => start = Some(func),
            // Custom-section acceptance remains part of admission. Its byte
            // count is intentionally not part of this ABI manifest: compiler
            // name metadata is not semantic and varies across supported hosts.
            Payload::CustomSection(_) => {}
            _ => {}
        }
    }
    for (name, kind, index) in exports {
        let type_index = match kind {
            ExternalKind::Func | ExternalKind::FuncExact => {
                function_type(index, &imported_function_types, &functions)
            }
            _ => None,
        };
        facts.push(format!(
            "export name={name} kind={} index={index} type={type_index:?}",
            external_kind(kind),
        ));
    }
    if let Some(index) = start {
        let type_index = function_type(index, &imported_function_types, &functions);
        facts.push(format!("start function index={index} type={type_index:?}"));
    }
    facts.sort_unstable();
    Ok(format!(
        "threaded-artifact-manifest-v1\n{}\n",
        facts.join("\n")
    ))
}

#[cfg(test)]
fn import_kind(import: TypeRef) -> String {
    match import {
        TypeRef::Func(index) | TypeRef::FuncExact(index) => format!("function type={index}"),
        TypeRef::Table(_) => "table".to_owned(),
        TypeRef::Memory(memory) => memory_fact(
            "memory",
            memory.initial,
            memory.maximum,
            memory.shared,
            memory.memory64,
            memory.page_size_log2,
        ),
        TypeRef::Global(_) => "global".to_owned(),
        TypeRef::Tag(_) => "tag".to_owned(),
    }
}

#[cfg(test)]
fn memory_fact(
    prefix: &str,
    initial: u64,
    maximum: Option<u64>,
    shared: bool,
    memory64: bool,
    page_size_log2: Option<u32>,
) -> String {
    format!(
        "{prefix} initial={initial} maximum={maximum:?} shared={shared} memory64={memory64} page-size-log2={page_size_log2:?}"
    )
}

#[cfg(test)]
fn external_kind(kind: ExternalKind) -> &'static str {
    match kind {
        ExternalKind::Func => "function",
        ExternalKind::FuncExact => "function-exact",
        ExternalKind::Table => "table",
        ExternalKind::Memory => "memory",
        ExternalKind::Global => "global",
        ExternalKind::Tag => "tag",
    }
}

#[derive(Clone, Copy)]
struct Signature {
    params: &'static [ValType],
    results: &'static [ValType],
}
const EMPTY: &[ValType] = &[];
const I32: &[ValType] = &[ValType::I32];

/// The type section may contain GC composite types. They are not empty
/// function signatures: keeping that distinction makes ABI type references
/// fail at preflight instead of being mistaken for `() -> ()`.
enum TypeEntry {
    Function {
        params: Vec<ValType>,
        results: Vec<ValType>,
    },
    NonFunction,
}

#[expect(
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
fn preflight(
    bytes: &[u8],
    policy: SketchModulePolicy,
) -> Result<SketchSharedMemory, SketchModuleError> {
    let mut types = Vec::<TypeEntry>::new();
    let mut functions = Vec::<u32>::new();
    let mut imported_functions = 0_u32;
    let mut memory = None;
    let mut saw_thread = false;
    let mut saw_yield = false;
    let mut exports = Vec::<(String, ExternalKind, u32)>::new();
    let mut abi = 0;
    let mut profile = 0;
    for item in Parser::new(0).parse_all(bytes) {
        match item.map_err(|_| SketchModuleError::InvalidBinary)? {
            Payload::TypeSection(reader) => {
                for group in reader {
                    let group = group.map_err(|_| SketchModuleError::InvalidBinary)?;
                    for ty in group.types() {
                        if let CompositeInnerType::Func(function) = &ty.composite_type.inner {
                            types.push(TypeEntry::Function {
                                params: function.params().to_vec(),
                                results: function.results().to_vec(),
                            });
                        } else {
                            types.push(TypeEntry::NonFunction);
                        }
                    }
                }
            }
            Payload::ImportSection(reader) => {
                for import in reader.into_imports() {
                    let import = import.map_err(|_| SketchModuleError::InvalidBinary)?;
                    match (import.module, import.name, import.ty) {
                        (MEMORY_MODULE, MEMORY_NAME, TypeRef::Memory(ty)) => {
                            if memory.is_some() {
                                return Err(SketchModuleError::MultipleMemoryImports);
                            }
                            memory = Some(check_memory(
                                ty.initial,
                                ty.maximum,
                                ty.shared,
                                ty.memory64,
                                ty.page_size_log2,
                                policy,
                            )?);
                        }
                        (THREAD_MODULE, THREAD_SPAWN, TypeRef::Func(i) | TypeRef::FuncExact(i)) => {
                            check_signature(
                                &types,
                                i,
                                Signature {
                                    params: I32,
                                    results: I32,
                                },
                                THREAD_MODULE,
                                THREAD_SPAWN,
                            )?;
                            saw_thread = true;
                            imported_functions += 1;
                        }
                        (ABI_MODULE, ABI_YIELD, TypeRef::Func(i) | TypeRef::FuncExact(i)) => {
                            check_signature(
                                &types,
                                i,
                                Signature {
                                    params: generated_v1_contract::KERNEL_YIELD_PARAMS,
                                    results: generated_v1_contract::KERNEL_YIELD_RESULTS,
                                },
                                ABI_MODULE,
                                ABI_YIELD,
                            )?;
                            saw_yield = true;
                            imported_functions += 1;
                        }
                        (module, name, _) => {
                            return Err(SketchModuleError::ForbiddenImport {
                                module: bounded(module),
                                name: bounded(name),
                            })
                        }
                    }
                }
            }
            Payload::FunctionSection(reader) => {
                for index in reader {
                    functions.push(index.map_err(|_| SketchModuleError::InvalidBinary)?);
                }
            }
            // This profile has exactly one shared memory, supplied by the host.
            // Rejecting a defined memory here also guarantees that no second
            // memory shape reaches Wasmtime before the semantic boundary.
            Payload::MemorySection(_) => return Err(SketchModuleError::DefinedMemoryForbidden),
            Payload::ExportSection(reader) => {
                for export in reader {
                    let e = export.map_err(|_| SketchModuleError::InvalidBinary)?;
                    exports.push((bounded(e.name), e.kind, e.index));
                }
            }
            Payload::StartSection { .. } => return Err(SketchModuleError::StartFunctionForbidden),
            Payload::CustomSection(section) => {
                let (count, expected, name) = if section.name() == ABI_METADATA {
                    (&mut abi, ABI_METADATA_VALUE, ABI_METADATA)
                } else if section.name() == PROFILE_METADATA {
                    (&mut profile, PROFILE_METADATA_VALUE, PROFILE_METADATA)
                } else {
                    continue;
                };
                *count += 1;
                if *count > 1 {
                    return Err(SketchModuleError::DuplicateMetadata { name });
                }
                // The generated ABI contract is intentionally admitted as
                // complete TOML bytes, which is larger than the compact
                // profile marker.  Retain the small DoS ceiling for ordinary
                // metadata, while allowing precisely the checked-in contract
                // to set its own bounded size.
                if section.data().len() > expected.len().max(MAX_METADATA_BYTES) {
                    return Err(SketchModuleError::MetadataTooLarge { name });
                }
                if section.data() != expected {
                    return Err(SketchModuleError::MetadataMismatch { name });
                }
            }
            _ => {}
        }
    }
    let memory = memory.ok_or(SketchModuleError::MissingSharedMemory)?;
    if !saw_thread {
        return Err(SketchModuleError::MissingRequiredImport {
            module: THREAD_MODULE,
            name: THREAD_SPAWN,
        });
    }
    if !saw_yield {
        return Err(SketchModuleError::MissingRequiredImport {
            module: ABI_MODULE,
            name: ABI_YIELD,
        });
    }
    if abi == 0 {
        return Err(SketchModuleError::MissingMetadata { name: ABI_METADATA });
    }
    if profile == 0 {
        return Err(SketchModuleError::MissingMetadata {
            name: PROFILE_METADATA,
        });
    }
    if exports.len() != 1 || exports[0].0 != ENTRY || exports[0].1 != ExternalKind::Func {
        return Err(SketchModuleError::ExportNotAllowed {
            name: exports.first().map(|e| e.0.clone()).unwrap_or_default(),
        });
    }
    let index = exports[0]
        .2
        .checked_sub(imported_functions)
        .ok_or(SketchModuleError::EntrypointMismatch)? as usize;
    let ty = *functions
        .get(index)
        .ok_or(SketchModuleError::EntrypointMismatch)?;
    check_entrypoint_signature(&types, ty)?;
    Ok(memory)
}
fn check_memory(
    initial: u64,
    maximum: Option<u64>,
    shared: bool,
    memory64: bool,
    page_size_log2: Option<u32>,
    policy: SketchModulePolicy,
) -> Result<SketchSharedMemory, SketchModuleError> {
    if memory64 {
        return Err(SketchModuleError::Memory64);
    }
    if !shared {
        return Err(SketchModuleError::UnsharedMemory);
    }
    if page_size_log2.is_some() {
        return Err(SketchModuleError::UnsupportedMemoryPageSize);
    }
    let maximum = maximum.ok_or(SketchModuleError::SharedMemoryWithoutMaximum)?;
    if initial > maximum {
        return Err(SketchModuleError::MemoryInitialExceedsMaximum {
            minimum_pages: initial,
            maximum_pages: maximum,
        });
    }
    if initial > u64::from(policy.max_shared_memory_pages)
        || maximum > u64::from(policy.max_shared_memory_pages)
    {
        return Err(SketchModuleError::SharedMemoryExceedsPolicy {
            minimum_pages: initial,
            maximum_pages: maximum,
            policy_pages: policy.max_shared_memory_pages,
        });
    }
    Ok(SketchSharedMemory {
        minimum_pages: initial as u32,
        maximum_pages: maximum as u32,
    })
}

// Rust's wasi1 threads target has a deliberately closed compatibility import
// surface. These names are recorded here, but this admission slice links none
// of them and grants no filesystem, clock, environment, or process authority.
#[expect(
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
fn preflight_threaded_rust(
    bytes: &[u8],
    policy: SketchModulePolicy,
    validation: bool,
) -> Result<SketchSharedMemory, SketchModuleError> {
    let mut types = Vec::<TypeEntry>::new();
    let mut functions = Vec::<u32>::new();
    let mut imported_function_types = Vec::<u32>::new();
    let mut memory = None;
    let mut memory_export = None;
    let mut exports = Vec::<(String, ExternalKind, u32)>::new();
    let mut start = None;
    let mut target_features = None;
    let mut validation_metadata = 0_u8;
    let mut abi_metadata = 0_u8;
    let mut seen = std::collections::BTreeSet::new();
    for item in Parser::new(0).parse_all(bytes) {
        match item.map_err(|_| SketchModuleError::InvalidBinary)? {
            Payload::TypeSection(reader) => {
                for group in reader {
                    let group = group.map_err(|_| SketchModuleError::InvalidBinary)?;
                    for ty in group.types() {
                        if let CompositeInnerType::Func(function) = &ty.composite_type.inner {
                            types.push(TypeEntry::Function {
                                params: function.params().to_vec(),
                                results: function.results().to_vec(),
                            });
                        } else {
                            types.push(TypeEntry::NonFunction);
                        }
                    }
                }
            }
            Payload::ImportSection(reader) => {
                for import in reader.into_imports() {
                    let import = import.map_err(|_| SketchModuleError::InvalidBinary)?;
                    match import.ty {
                        TypeRef::Memory(ty)
                            if import.module == MEMORY_MODULE && import.name == MEMORY_NAME =>
                        {
                            if memory.is_some() {
                                return Err(SketchModuleError::MultipleMemoryImports);
                            }
                            memory = Some(check_memory(
                                ty.initial,
                                ty.maximum,
                                ty.shared,
                                ty.memory64,
                                ty.page_size_log2,
                                policy,
                            )?);
                        }
                        TypeRef::Func(index) | TypeRef::FuncExact(index) => {
                            if !threaded_import_signature(
                                import.module,
                                import.name,
                                &types,
                                index,
                            )? {
                                return Err(SketchModuleError::ForbiddenImport {
                                    module: bounded(import.module),
                                    name: bounded(import.name),
                                });
                            }
                            if !seen.insert((import.module, import.name)) {
                                return Err(SketchModuleError::ForbiddenImport {
                                    module: bounded(import.module),
                                    name: bounded(import.name),
                                });
                            }
                            imported_function_types.push(index);
                        }
                        _ => {
                            return Err(SketchModuleError::ForbiddenImport {
                                module: bounded(import.module),
                                name: bounded(import.name),
                            })
                        }
                    }
                }
            }
            Payload::FunctionSection(reader) => {
                for index in reader {
                    functions.push(index.map_err(|_| SketchModuleError::InvalidBinary)?);
                }
            }
            Payload::MemorySection(_) => return Err(SketchModuleError::DefinedMemoryForbidden),
            Payload::ExportSection(reader) => {
                for export in reader {
                    let export = export.map_err(|_| SketchModuleError::InvalidBinary)?;
                    if export.name == "memory" {
                        memory_export = Some((export.kind, export.index));
                    }
                    exports.push((bounded(export.name), export.kind, export.index));
                }
            }
            Payload::StartSection { func, .. } => start = Some(func),
            Payload::CustomSection(section) => match section.name() {
                ABI_METADATA => {
                    abi_metadata += 1;
                    if abi_metadata > 1 {
                        return Err(SketchModuleError::DuplicateMetadata { name: ABI_METADATA });
                    }
                    if section.data() != ABI_METADATA_VALUE {
                        return Err(SketchModuleError::MetadataMismatch { name: ABI_METADATA });
                    }
                }
                PROFILE_METADATA if validation => {
                    validation_metadata += 1;
                    if validation_metadata > 1 {
                        return Err(SketchModuleError::DuplicateMetadata {
                            name: PROFILE_METADATA,
                        });
                    }
                    if section.data() != VALIDATION_PROFILE_METADATA_VALUE {
                        return Err(SketchModuleError::MetadataMismatch {
                            name: PROFILE_METADATA,
                        });
                    }
                }
                "target_features" => {
                    if target_features
                        .replace(parse_target_features(section.data())?)
                        .is_some()
                    {
                        return Err(SketchModuleError::TargetFeaturesMismatch);
                    }
                }
                "name" | "producers" | ".debug_abbrev" | ".debug_info" | ".debug_line"
                | ".debug_ranges" | ".debug_str" => {
                    if section.data().len() > MAX_METADATA_BYTES * 1024 {
                        return Err(SketchModuleError::MetadataTooLarge { name: "debug" });
                    }
                }
                name => {
                    return Err(SketchModuleError::ForbiddenCustomSection {
                        name: bounded(name),
                    })
                }
            },
            _ => {}
        }
    }
    let memory = memory.ok_or(SketchModuleError::MissingSharedMemory)?;
    if abi_metadata == 0 {
        return Err(SketchModuleError::MissingMetadata { name: ABI_METADATA });
    }
    if memory.minimum_pages != THREADED_RUST_INITIAL_PAGES
        || memory.maximum_pages != THREADED_RUST_MAX_PAGES
    {
        return Err(SketchModuleError::ThreadedMemoryMismatch {
            minimum_pages: memory.minimum_pages,
            maximum_pages: memory.maximum_pages,
        });
    }
    if policy.max_shared_memory_pages < THREADED_RUST_MAX_PAGES {
        return Err(SketchModuleError::SharedMemoryExceedsPolicy {
            minimum_pages: memory.minimum_pages.into(),
            maximum_pages: THREADED_RUST_MAX_PAGES.into(),
            policy_pages: policy.max_shared_memory_pages,
        });
    }
    if memory_export != Some((ExternalKind::Memory, 0)) {
        return Err(SketchModuleError::MemoryExportMismatch);
    }
    // Every present import already passed the exact name/signature allowlist
    // above. Do not require dead stdlib imports from the smoke application's
    // particular link graph: a screenshot guest need not spawn a child, read
    // a compatibility clock, or use the legacy authority-free yield.
    // Require either the legacy kernel boundary or the complete generated
    // lifecycle, and continue rejecting partial submit/poll/yield protocols.
    let lifecycle = [
        (ABI_MODULE, "operation_submit"),
        (ABI_MODULE, "operation_poll"),
        (ABI_MODULE, "operation_yield"),
    ];
    let lifecycle_present = lifecycle.iter().filter(|pair| seen.contains(*pair)).count();
    let cancellation_present = usize::from(seen.contains(&(ABI_MODULE, "operation_cancel")));
    if (lifecycle_present == 0 && !seen.contains(&(ABI_MODULE, ABI_YIELD)))
        || !matches!(lifecycle_present, 0 | 3)
        || (cancellation_present != 0 && lifecycle_present != 3)
    {
        return Err(SketchModuleError::MissingRequiredImport {
            module: "threaded-rust-v1",
            name: "closed-import-set",
        });
    }
    let features = target_features.ok_or(SketchModuleError::MissingTargetFeatures)?;
    let expected_features = [
        "atomics",
        "bulk-memory",
        "bulk-memory-opt",
        "call-indirect-overlong",
        "extended-const",
        "multivalue",
        "mutable-globals",
        "nontrapping-fptoint",
        "reference-types",
        "sign-ext",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if features != expected_features {
        return Err(SketchModuleError::TargetFeaturesMismatch);
    }
    let mut allowed = vec![
        ("memory", ExternalKind::Memory),
        ("_start", ExternalKind::Func),
        ("__main_void", ExternalKind::Func),
        ("wasi_thread_start", ExternalKind::Func),
        (ENTRY, ExternalKind::Func),
    ];
    if validation {
        allowed.push((VALIDATION_REPORT, ExternalKind::Func));
        if validation_metadata != 1 {
            return Err(SketchModuleError::MissingMetadata {
                name: PROFILE_METADATA,
            });
        }
    }
    if exports.len() != allowed.len() {
        return Err(SketchModuleError::ExportNotAllowed {
            name: exports.first().map(|e| e.0.clone()).unwrap_or_default(),
        });
    }
    if let Some((name, _, _)) = exports
        .iter()
        .find(|(name, kind, _)| !allowed.contains(&(name.as_str(), *kind)))
    {
        return Err(SketchModuleError::ExportNotAllowed { name: name.clone() });
    }
    let mut by_name = std::collections::BTreeMap::new();
    for (name, _, index) in exports {
        by_name.insert(name, index);
    }
    let mut signatures = vec![
        (
            "_start",
            Signature {
                params: EMPTY,
                results: EMPTY,
            },
        ),
        (
            "__main_void",
            Signature {
                params: EMPTY,
                results: I32,
            },
        ),
        (
            "wasi_thread_start",
            Signature {
                params: &[ValType::I32, ValType::I32],
                results: EMPTY,
            },
        ),
        (
            ENTRY,
            Signature {
                params: EMPTY,
                results: I32,
            },
        ),
    ];
    if validation {
        signatures.push((
            VALIDATION_REPORT,
            Signature {
                params: EMPTY,
                results: I32,
            },
        ));
    }
    for (name, signature) in signatures {
        let index = *by_name
            .get(name)
            .ok_or(SketchModuleError::EntrypointMismatch)?;
        let type_index = function_type(index, &imported_function_types, &functions)
            .ok_or(SketchModuleError::EntrypointMismatch)?;
        check_signature(&types, type_index, signature, ABI_MODULE, name)?;
    }
    // Rust emits a dedicated () -> () start function. It is intentionally not
    // required to equal exported `_start`: import admission, not that identity,
    // defines the authority boundary, and the deterministic Rust artifact uses
    // distinct functions.
    let start = start.ok_or(SketchModuleError::StartMismatch)?;
    let start_type = function_type(start, &imported_function_types, &functions)
        .ok_or(SketchModuleError::StartMismatch)?;
    let Some(TypeEntry::Function { params, results }) = types.get(start_type as usize) else {
        return Err(SketchModuleError::StartMismatch);
    };
    if params.as_slice() != EMPTY || results.as_slice() != EMPTY {
        return Err(SketchModuleError::StartMismatch);
    }
    Ok(memory)
}

fn function_type(index: u32, imported: &[u32], defined: &[u32]) -> Option<u32> {
    if (index as usize) < imported.len() {
        imported.get(index as usize).copied()
    } else {
        defined.get(index as usize - imported.len()).copied()
    }
}

fn threaded_import_signature(
    module: &str,
    name: &str,
    types: &[TypeEntry],
    index: u32,
) -> Result<bool, SketchModuleError> {
    let signature = match (module, name) {
        (ABI_MODULE, ABI_YIELD) => Signature {
            params: generated_v1_contract::KERNEL_YIELD_PARAMS,
            results: generated_v1_contract::KERNEL_YIELD_RESULTS,
        },
        // These are the closed scalar imports generated from the admitted v1
        // manifest. Blob exposes bounded caller-memory stream controls while
        // its established operation opcodes remain available to the guest
        // facade for capacity-awaited transfer futures.
        (ABI_MODULE, "operation_submit") => Signature {
            params: &[ValType::I32, ValType::I64, ValType::I64],
            results: &[ValType::I64],
        },
        (ABI_MODULE, "operation_poll") => Signature {
            params: &[ValType::I64],
            results: &[ValType::I64],
        },
        (ABI_MODULE, "operation_yield") | (ABI_MODULE, "operation_cancel") => Signature {
            params: &[ValType::I64],
            results: I32,
        },
        (ABI_MODULE, "resource_release_encrypted_archive")
        | (ABI_MODULE, "resource_release_authenticated_archive")
        | (ABI_MODULE, "resource_release_archive_entry") => Signature {
            params: &[ValType::I64],
            results: I32,
        },
        (ABI_MODULE, "stream_read") | (ABI_MODULE, "stream_write") => Signature {
            params: &[ValType::I64, ValType::I32, ValType::I32],
            results: I32,
        },
        (ABI_MODULE, "stream_close") => Signature {
            params: &[ValType::I64],
            results: I32,
        },
        (THREAD_MODULE, THREAD_SPAWN) => Signature {
            params: I32,
            results: I32,
        },
        ("wasi_snapshot_preview1", "clock_time_get") => Signature {
            params: &[ValType::I32, ValType::I64, ValType::I32],
            results: I32,
        },
        ("wasi_snapshot_preview1", "environ_get")
        | ("wasi_snapshot_preview1", "environ_sizes_get") => Signature {
            params: &[ValType::I32, ValType::I32],
            results: I32,
        },
        ("wasi_snapshot_preview1", "fd_write") => Signature {
            params: &[ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            results: I32,
        },
        ("wasi_snapshot_preview1", "proc_exit") => Signature {
            params: I32,
            results: EMPTY,
        },
        ("wasi_snapshot_preview1", "sched_yield") => Signature {
            params: EMPTY,
            results: I32,
        },
        _ => return Ok(false),
    };
    check_signature(types, index, signature, module, name)?;
    Ok(true)
}

fn parse_target_features(
    data: &[u8],
) -> Result<std::collections::BTreeSet<String>, SketchModuleError> {
    let (count, mut offset) = read_leb(data, 0)?;
    let mut result = std::collections::BTreeSet::new();
    for _ in 0..count {
        if offset >= data.len() {
            return Err(SketchModuleError::TargetFeaturesMismatch);
        }
        let prefix = data[offset];
        offset += 1;
        if prefix != b'+' {
            return Err(SketchModuleError::TargetFeaturesMismatch);
        }
        let (length, next) = read_leb(data, offset)?;
        offset = next;
        let end = offset
            .checked_add(length as usize)
            .ok_or(SketchModuleError::TargetFeaturesMismatch)?;
        let name = std::str::from_utf8(
            data.get(offset..end)
                .ok_or(SketchModuleError::TargetFeaturesMismatch)?,
        )
        .map_err(|_| SketchModuleError::TargetFeaturesMismatch)?;
        if !result.insert(bounded(name)) {
            return Err(SketchModuleError::TargetFeaturesMismatch);
        }
        offset = end;
    }
    if offset != data.len() {
        return Err(SketchModuleError::TargetFeaturesMismatch);
    }
    Ok(result)
}

fn read_leb(data: &[u8], mut offset: usize) -> Result<(u32, usize), SketchModuleError> {
    let mut value = 0_u32;
    for shift in (0..35).step_by(7) {
        let byte = *data
            .get(offset)
            .ok_or(SketchModuleError::TargetFeaturesMismatch)?;
        offset += 1;
        value |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, offset));
        }
    }
    Err(SketchModuleError::TargetFeaturesMismatch)
}
fn check_signature(
    types: &[TypeEntry],
    index: u32,
    expected: Signature,
    module: &str,
    name: &str,
) -> Result<(), SketchModuleError> {
    let Some(TypeEntry::Function { params, results }) = types.get(index as usize) else {
        return Err(SketchModuleError::ImportTypeMismatch {
            module: bounded(module),
            name: bounded(name),
        });
    };
    if params.as_slice() != expected.params || results.as_slice() != expected.results {
        return Err(SketchModuleError::ImportTypeMismatch {
            module: bounded(module),
            name: bounded(name),
        });
    }
    Ok(())
}

fn check_entrypoint_signature(types: &[TypeEntry], index: u32) -> Result<(), SketchModuleError> {
    let Some(TypeEntry::Function { params, results }) = types.get(index as usize) else {
        return Err(SketchModuleError::EntrypointMismatch);
    };
    if params.as_slice() != EMPTY || results.as_slice() != EMPTY {
        // Preserve the existing stable error for a function-valued entrypoint
        // whose ABI is wrong; only a non-function type gets the distinct
        // entrypoint-type diagnostic above.
        return Err(SketchModuleError::ImportTypeMismatch {
            module: ABI_MODULE.to_owned(),
            name: ENTRY.to_owned(),
        });
    }
    Ok(())
}
fn bounded(value: &str) -> String {
    value.chars().take(96).collect()
}
