//! Default-on crash capture (#636).
//!
//! Calling [`install`] is the only arming point. Linking this crate installs
//! no constructor and touches no signal or exception state. Once armed, a
//! normal sampler thread keeps a bounded all-thread snapshot ready. The fatal
//! callback copies only the raw platform context into that preallocated
//! record and emits it with one OS write before returning `Handled(false)` so
//! the previously installed application handler still runs.
//!
//! ## Migrating from a direct `crash-handler` dependency (#72)
//!
//! A client that previously called `crash_handler::CrashHandler::attach` with
//! `crash_handler::make_crash_event` finds none of that vocabulary here.
//! `kernal_api::crash` replaces it with a facade-owned surface that is
//! stricter than a design that hands the client a callback: no client code
//! runs in signal or structured-exception context at all. The fatal path
//! copies only fixed-size platform fields into a preallocated record and
//! performs one bounded OS write; every other step — decoding, labeling,
//! formatting a human-readable report — happens later in ordinary
//! (non-signal) context via [`spool::parse`]. This sidesteps the exact
//! async-signal-unsafety tradeoff (allocating to format a report from inside
//! the handler) that a backend-callback design has to accept.
//!
//! | Direct `crash-handler` vocabulary | `kernal_api::crash` replacement |
//! | --- | --- |
//! | `CrashHandler::attach` + `make_crash_event` + `CrashEventResult::Handled(false)` | [`install`] |
//! | The `CrashGuard` returned by a hand-rolled `install(bin_stem)` wrapping `Option<CrashHandler>` | [`CrashGuard`] |
//! | `&CrashContext` read out of the fatal callback | [`spool::RawCrashReport`], decoded from the spool file via [`spool::parse`] outside signal context |
//! | Per-platform `context_label(&CrashContext)` (`ctx.siginfo.ssi_signo`, `ctx.exception.kind`, `ctx.exception_pointers`) | [`spool::RawCrashReport::fault_code`] — see that field's docs for the platform-specific caveats before writing a label decoder |
//! | Per-platform `context_summary(&CrashContext)` (`si_addr`, `tid`/`thread`/`thread_id`) | [`spool::RawCrashReport::fault_address`] and [`spool::RawCrashReport::tid`] |
//! | A caller-chosen binary stem interpolated into every dump filename | [`spool::CrashMetadata::app_name`] (and `app_class`/`instance_name`/`app_version`), registered once via [`install`] |
//! | The backend error returned by a failed `attach` | [`InstallError::Handler`], carrying the classified [`HandlerError`] |
//!
//! `kernal_api::crash` does not format a human-readable text report; it spools
//! a fixed-layout binary record ([`spool::RECORD_SIZE`] bytes) to an
//! owner-private directory ([`spool::spool_dir`]) and lets a normal-context
//! reader turn that into whatever report shape the application wants by
//! calling [`spool::parse`]. A client that previously wrote its own text dump
//! synchronously from inside the signal handler instead reads pending
//! `.rpcrash` records at a convenient later point (for example, at the next
//! process start, mirroring how it already checks for previous crash dumps).

#![allow(unsafe_code)]

pub mod spool;

use std::cell::UnsafeCell;
use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use crash_handler::{CrashContext, CrashEventResult, CrashHandler};

use self::spool::{CrashFrame, CrashModule, CrashThread, RECORD_SIZE};

/// The metadata `install` takes, re-exported so callers can name it.
///
/// It is defined in [`spool`] because that is what writes it into a
/// record, but a client reaching `install` should not have to know that:
/// the argument type of a public function belongs in the same module as
/// the function.
pub use self::spool::CrashMetadata;

/// Crash interception policy. Calling `install` arms it by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CrashPolicy {
    /// Install native crash interception.
    #[default]
    On,
    /// Leave all native handlers untouched.
    Off,
}

/// Environment opt-out checked before any crash state is created.
pub const NO_CRASH_HANDLER_ENV: &str = "KERNAL_API_NO_CRASH_HANDLER";

/// Sampling cadence for native crash snapshots. The default starts at 100 ms
/// and backs off exponentially to 1 s while captures are unchanged. A changed
/// capture resets the cadence. The sampler can wait longer than
/// `max_interval` when needed to preserve its one-percent CPU duty budget.
#[derive(Clone, Copy, Debug)]
pub struct CrashSamplerConfig {
    /// Time before the first capture and between captures after a change.
    pub interval: Duration,
    /// Longest adaptive delay between unchanged captures, before the CPU
    /// duty-cycle floor is applied.
    pub max_interval: Duration,
}

impl Default for CrashSamplerConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(100),
            max_interval: Duration::from_secs(1),
        }
    }
}

/// Local failure while arming crash capture.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// The owner-private spool could not be prepared.
    #[error("cannot prepare crash spool: {0}")]
    Spool(#[source] io::Error),
    /// The platform crash handler could not be attached.
    #[error("cannot attach native crash handler: {0}")]
    Handler(#[source] HandlerError),
    /// The all-thread sampler could not be started.
    #[error("cannot start crash snapshot sampler: {0}")]
    Sampler(#[source] io::Error),
    /// The sampling intervals must be positive and ordered.
    #[error("crash sampler intervals must be positive, with max_interval >= interval")]
    InvalidSamplerConfig,
    /// The platform SIGABRT predecessor chain could not be installed.
    #[cfg(any(windows, target_os = "macos"))]
    #[error("cannot chain the platform abort handler: {0}")]
    AbortChain(#[source] io::Error),
    /// A post-fork child must exec before installing its own crash runtime.
    #[error("crash capture was inherited across fork; exec before reinstalling")]
    ForkedProcess,
}

/// Why the platform refused to arm native crash interception.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HandlerErrorKind {
    /// Another crash handler already owns the process-wide slot.
    AlreadyInstalled,
    /// The handler's fixed-size scratch memory could not be reserved.
    OutOfMemory,
    /// The operating system rejected the attach.
    Os,
}

/// Facade-owned detail behind [`InstallError::Handler`].
///
/// The attach failure is classified once, here, so a caller never names the
/// native handler backend to tell "somebody else owns the slot" apart from a
/// resource or OS failure.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct HandlerError {
    kind: HandlerErrorKind,
    message: String,
}

impl HandlerError {
    /// The classified reason the attach failed.
    pub fn kind(&self) -> HandlerErrorKind {
        self.kind
    }

    /// The platform's own rendering of the failure.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Classify a backend attach failure. Private: the argument type is the
    /// backend vocabulary this error exists to keep off the public surface.
    fn from_backend(error: crash_handler::Error) -> Self {
        let kind = match &error {
            crash_handler::Error::HandlerAlreadyInstalled => HandlerErrorKind::AlreadyInstalled,
            crash_handler::Error::OutOfMemory => HandlerErrorKind::OutOfMemory,
            crash_handler::Error::Io(_) => HandlerErrorKind::Os,
        };
        Self {
            kind,
            message: error.to_string(),
        }
    }
}

/// Keeps the native handler and sampler armed.
pub struct CrashGuard {
    runtime: Option<Arc<Runtime>>,
    registration_id: Option<u64>,
}

impl CrashGuard {
    /// An inert guard used by both opt-out paths.
    pub fn inert() -> Self {
        Self {
            runtime: None,
            registration_id: None,
        }
    }

    /// Whether this guard contributes to an armed handler.
    pub fn is_armed(&self) -> bool {
        self.runtime.as_ref().is_some_and(|runtime| {
            runtime.pid == std::process::id() && runtime.handler_armed.load(Ordering::Acquire)
        })
    }

    /// Whether the background sampler has produced at least one snapshot.
    pub fn sample_ready(&self) -> bool {
        self.runtime
            .as_ref()
            .is_some_and(|runtime| runtime.shared.sample_ready.load(Ordering::Acquire))
    }

    /// Thread count in the latest bounded pre-crash sample.
    pub fn sample_thread_count(&self) -> usize {
        self.runtime.as_ref().map_or(0, |runtime| {
            runtime.shared.sample_thread_count.load(Ordering::Acquire)
        })
    }

    /// Pending spool path, exposed for diagnostic tests and operators.
    pub fn spool_path(&self) -> Option<&std::path::Path> {
        self.runtime.as_ref().map(|runtime| runtime.path.as_path())
    }
}

impl Drop for CrashGuard {
    fn drop(&mut self) {
        if let (Some(runtime), Some(id)) = (&self.runtime, self.registration_id.take()) {
            runtime.remove_registration(id);
        }
    }
}

static RUNTIME: OnceLock<Mutex<Weak<Runtime>>> = OnceLock::new();
static OWNER_PID: AtomicU32 = AtomicU32::new(0);
static HANDLER_TRANSITION: Mutex<()> = Mutex::new(());
#[cfg(test)]
static TEST_RESUME_PAUSE: Mutex<Option<TestResumePause>> = Mutex::new(None);
#[cfg(test)]
static TEST_SAMPLER_DISABLED: Mutex<bool> = Mutex::new(false);

#[cfg(test)]
struct TestResumePause {
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
fn set_test_resume_pause(pause: TestResumePause) -> TestResumePauseGuard {
    let mut slot = match TEST_RESUME_PAUSE.lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    assert!(
        slot.replace(pause).is_none(),
        "a test resume pause is already active"
    );
    TestResumePauseGuard
}

#[cfg(test)]
fn take_test_resume_pause() -> Option<TestResumePause> {
    match TEST_RESUME_PAUSE.lock() {
        Ok(mut slot) => slot.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    }
}

#[cfg(test)]
struct TestResumePauseGuard;

#[cfg(test)]
impl Drop for TestResumePauseGuard {
    fn drop(&mut self) {
        let mut slot = match TEST_RESUME_PAUSE.lock() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };
        slot.take();
    }
}

#[cfg(test)]
fn disable_test_sampler() -> TestSamplerDisabledGuard {
    let mut disabled = match TEST_SAMPLER_DISABLED.lock() {
        Ok(disabled) => disabled,
        Err(poisoned) => poisoned.into_inner(),
    };
    assert!(!*disabled, "a test sampler disable guard is already active");
    *disabled = true;
    TestSamplerDisabledGuard
}

#[cfg(test)]
fn test_sampler_disabled() -> bool {
    match TEST_SAMPLER_DISABLED.lock() {
        Ok(disabled) => *disabled,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

#[cfg(test)]
struct TestSamplerDisabledGuard;

#[cfg(test)]
impl Drop for TestSamplerDisabledGuard {
    fn drop(&mut self) {
        let mut disabled = match TEST_SAMPLER_DISABLED.lock() {
            Ok(disabled) => disabled,
            Err(poisoned) => poisoned.into_inner(),
        };
        *disabled = false;
    }
}

/// Arm native crash capture unless policy or environment opts out.
pub fn install(policy: CrashPolicy, metadata: CrashMetadata) -> Result<CrashGuard, InstallError> {
    install_with_sampler_config(policy, metadata, CrashSamplerConfig::default())
}

/// Arm native crash capture with a caller-selected sampling cadence.
///
/// The first live registration sets the process-wide cadence until its final
/// guard is dropped. An opt-out policy does not validate or start a sampler.
pub fn install_with_sampler_config(
    policy: CrashPolicy,
    metadata: CrashMetadata,
    config: CrashSamplerConfig,
) -> Result<CrashGuard, InstallError> {
    if policy == CrashPolicy::Off || env_opted_out() {
        return Ok(CrashGuard::inert());
    }
    if config.interval.is_zero() || config.max_interval < config.interval {
        return Err(InstallError::InvalidSamplerConfig);
    }

    let pid = std::process::id();
    match OWNER_PID.compare_exchange(0, pid, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {}
        Err(owner) if owner == pid => {}
        Err(_) => {
            // Do not touch a possibly locked mutex inherited from a vanished
            // thread. The inherited callback is PID-gated and therefore inert.
            return Err(InstallError::ForkedProcess);
        }
    }

    let _transition = match HANDLER_TRANSITION.lock() {
        Ok(transition) => transition,
        Err(poisoned) => poisoned.into_inner(),
    };
    let slot = RUNTIME.get_or_init(|| Mutex::new(Weak::new()));
    let mut weak = match slot.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(runtime) = weak.upgrade() {
        if let Err(error) = runtime.resume_handler() {
            // This local upgrade can be the final Arc if the pre-existing last
            // guard drops concurrently. Release both normal-context gates
            // before dropping it, since Runtime::drop joins HANDLER_TRANSITION
            // to serialize teardown against a new first attachment.
            drop(weak);
            drop(_transition);
            drop(runtime);
            return Err(error);
        }
        let registration_id = runtime.add_registration(metadata);
        return Ok(CrashGuard {
            runtime: Some(runtime),
            registration_id: Some(registration_id),
        });
    }

    let (runtime, registration_id) = Runtime::new(metadata, config)?;
    *weak = Arc::downgrade(&runtime);
    Ok(CrashGuard {
        runtime: Some(runtime),
        registration_id: Some(registration_id),
    })
}

fn env_opted_out() -> bool {
    std::env::var_os(NO_CRASH_HANDLER_ENV).is_some_and(|value| {
        let text = value.to_string_lossy();
        text == "1" || text.eq_ignore_ascii_case("true") || text.eq_ignore_ascii_case("yes")
    })
}

struct Runtime {
    shared: Arc<Shared>,
    handler: Mutex<Option<CrashHandler>>,
    handler_armed: AtomicBool,
    sampler: Option<JoinHandle<()>>,
    path: PathBuf,
    pid: u32,
    registrations: Mutex<RegistrationState>,
}

impl Runtime {
    fn new(
        metadata: CrashMetadata,
        config: CrashSamplerConfig,
    ) -> Result<(Arc<Self>, u64), InstallError> {
        let (file, path, template) = spool::create_sink(&metadata).map_err(InstallError::Spool)?;
        let shared = Arc::new(Shared::new(file, template));

        let handler = match attach_handler(&shared) {
            Ok(handler) => handler,
            Err(error) => {
                // Arming failed before ownership could move into `Runtime`.
                // Close the pre-opened sink before removing it so Windows can
                // delete it too; otherwise the daemon would retain an empty
                // pending record forever.
                drop(shared);
                let _ = std::fs::remove_file(&path);
                return Err(error);
            }
        };

        #[cfg(test)]
        let sampler_disabled = test_sampler_disabled();
        #[cfg(not(test))]
        let sampler_disabled = false;
        let sampler = if sampler_disabled {
            None
        } else {
            Some(match start_sampler(&shared, config) {
                Ok(sampler) => sampler,
                Err(error) => {
                    #[cfg(windows)]
                    uninstall_windows_abort_chain();
                    #[cfg(target_os = "macos")]
                    uninstall_macos_abort_chain();
                    drop(handler);
                    drop(shared);
                    let _ = std::fs::remove_file(&path);
                    return Err(InstallError::Sampler(error));
                }
            })
        };

        let registration_id = 1;
        let mut entries = BTreeMap::new();
        entries.insert(registration_id, metadata);
        Ok((
            Arc::new(Self {
                shared,
                handler: Mutex::new(Some(handler)),
                handler_armed: AtomicBool::new(true),
                sampler,
                path,
                pid: std::process::id(),
                registrations: Mutex::new(RegistrationState {
                    next_id: registration_id + 1,
                    generation: 1,
                    entries,
                }),
            }),
            registration_id,
        ))
    }

    fn add_registration(&self, metadata: CrashMetadata) -> u64 {
        let mut state = match self.registrations.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1).max(1);
        state.entries.insert(id, metadata);
        id
    }

    fn remove_registration(&self, id: u64) {
        let selected = {
            let mut state = match self.registrations.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            let was_selected = state.entries.first_key_value().map(|(key, _)| *key) == Some(id);
            state.entries.remove(&id);
            if was_selected {
                state.generation = state.generation.wrapping_add(1).max(1);
                state
                    .entries
                    .first_key_value()
                    .map(|(_, value)| (state.generation, value.clone()))
            } else {
                None
            }
        };
        if let Some((generation, metadata)) = selected {
            self.shared.set_metadata(generation, &metadata);
        }
    }

    fn suspend_handler(&self) -> bool {
        let mut handler = match self.handler.lock() {
            Ok(handler) => handler,
            Err(poisoned) => poisoned.into_inner(),
        };
        if handler.is_none() {
            return false;
        }
        self.handler_armed.store(false, Ordering::Release);
        #[cfg(windows)]
        uninstall_windows_abort_chain();
        #[cfg(target_os = "macos")]
        uninstall_macos_abort_chain();
        handler.take();
        true
    }

    fn resume_handler(&self) -> Result<(), InstallError> {
        let mut handler = match self.handler.lock() {
            Ok(handler) => handler,
            Err(poisoned) => poisoned.into_inner(),
        };
        #[cfg(test)]
        if let Some(pause) = take_test_resume_pause() {
            // This one-shot channel gate is a test seam for the exact point
            // where installation owns the transition and handler locks. The
            // release sender closing during test unwinding also wakes this
            // thread, so a failed assertion cannot retain global pause state.
            let _ = pause.entered.send(());
            let _ = pause.release.recv();
        }
        if handler.is_none() {
            *handler = Some(attach_handler(&self.shared)?);
            self.handler_armed.store(true, Ordering::Release);
        }
        Ok(())
    }
}

fn start_sampler(shared: &Arc<Shared>, config: CrashSamplerConfig) -> io::Result<JoinHandle<()>> {
    let sampler_state = Arc::clone(shared);
    std::thread::Builder::new()
        .name("rp-crash-sampler".into())
        .spawn(move || sampler_loop(sampler_state, config))
}

struct RegistrationState {
    next_id: u64,
    generation: u64,
    entries: BTreeMap<u64, CrashMetadata>,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let handler = match self.handler.get_mut() {
            Ok(handler) => handler,
            Err(poisoned) => poisoned.into_inner(),
        };
        if self.pid != std::process::id() {
            // A fork copied JoinHandle/handler bookkeeping but not the sampler
            // thread. Joining or detaching here could deadlock on locks held by
            // vanished threads. Leak those process-local registrations until
            // the child execs/exits; callbacks are PID-gated and inert.
            if let Some(handler) = handler.take() {
                std::mem::forget(handler);
            }
            if let Some(sampler) = self.sampler.take() {
                std::mem::forget(sampler);
            }
            return;
        }
        let _transition = match HANDLER_TRANSITION.lock() {
            Ok(transition) => transition,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Uninstall first, so no callback can begin while the sampler and sink
        // are being torn down.
        #[cfg(windows)]
        uninstall_windows_abort_chain();
        #[cfg(target_os = "macos")]
        uninstall_macos_abort_chain();
        self.handler_armed.store(false, Ordering::Release);
        handler.take();
        self.shared.request_stop();
        if let Some(handle) = self.sampler.take() {
            let detach = {
                let mut handoff = match self.shared.handoff.lock() {
                    Ok(handoff) => handoff,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let detach = !handoff.done && self.shared.sampling.load(Ordering::Acquire);
                if detach {
                    handoff.sink = Some(self.path.clone());
                }
                detach
            };
            if detach {
                // The first sample can spend ~1 s symbolizing a debug binary;
                // do not make process exit wait for it. The sampler removes
                // the sink after dropping its handle to it.
                return;
            }
            let _ = handle.join();
        }
        // A cleanly dropped process never wrote the pre-opened file.
        let _ = std::fs::remove_file(&self.path);
    }
}

fn attach_handler(shared: &Arc<Shared>) -> Result<CrashHandler, InstallError> {
    #[cfg(windows)]
    let previous_abort = windows_previous_abort_handler().map_err(InstallError::AbortChain)?;
    #[cfg(target_os = "macos")]
    let previous_abort = macos_previous_abort_action().map_err(InstallError::AbortChain)?;

    let callback_state = Arc::clone(shared);
    // SAFETY: `handle_crash` performs only atomic operations, raw copies,
    // clock_gettime/GetSystemTimeAsFileTime, and one OS write. It neither
    // allocates nor locks.
    let event = unsafe {
        crash_handler::make_crash_event(move |context| {
            callback_state.handle_crash(context);
            CrashEventResult::Handled(false)
        })
    };
    let handler = CrashHandler::attach(event)
        .map_err(|error| InstallError::Handler(HandlerError::from_backend(error)))?;

    #[cfg(windows)]
    if let Err(error) = install_windows_abort_chain(shared, previous_abort) {
        drop(handler);
        return Err(InstallError::AbortChain(error));
    }
    #[cfg(target_os = "macos")]
    if let Err(error) = install_macos_abort_chain(shared, previous_abort) {
        drop(handler);
        return Err(InstallError::AbortChain(error));
    }

    Ok(handler)
}

/// Run a normal-context handler installation beneath the native crash layer.
///
/// Some runtimes install their own fatal handlers lazily. Temporarily
/// detaching here lets the external handler become our new predecessor, so
/// later native teardown restores it instead of the older process default.
/// The native handler is re-armed before this function returns, even if the
/// callback unwinds.
pub fn with_handler_suspended<R>(install: impl FnOnce() -> R) -> Result<R, InstallError> {
    let pid = std::process::id();
    let owner_before_lock = OWNER_PID.load(Ordering::Acquire);
    if owner_before_lock != 0 && owner_before_lock != pid {
        return Err(InstallError::ForkedProcess);
    }

    // Declared before the transition guard so an unwind releases the gate
    // before dropping the final possible Runtime Arc. Runtime::drop joins the
    // same gate to serialize teardown against a new first attachment.
    let runtime: Option<Arc<Runtime>>;
    let transition = match HANDLER_TRANSITION.lock() {
        Ok(transition) => transition,
        Err(poisoned) => poisoned.into_inner(),
    };
    let owner = OWNER_PID.load(Ordering::Acquire);
    if owner != 0 && owner != pid {
        return Err(InstallError::ForkedProcess);
    }
    runtime = if owner == 0 {
        None
    } else {
        let slot = RUNTIME.get_or_init(|| Mutex::new(Weak::new()));
        let weak = match slot.lock() {
            Ok(weak) => weak,
            Err(poisoned) => poisoned.into_inner(),
        };
        weak.upgrade()
    };

    let Some(runtime) = runtime.as_ref() else {
        return Ok(install());
    };
    runtime.resume_handler()?;
    let suspended = runtime.suspend_handler();
    let mut resume = ResumeHandler {
        runtime: Arc::clone(runtime),
        suspended,
    };
    let result = install();
    let resumed = resume.finish();
    drop(resume);
    drop(transition);
    resumed?;
    Ok(result)
}

struct ResumeHandler {
    runtime: Arc<Runtime>,
    suspended: bool,
}

impl ResumeHandler {
    fn finish(&mut self) -> Result<(), InstallError> {
        if self.suspended {
            self.runtime.resume_handler()?;
            self.suspended = false;
        }
        Ok(())
    }
}

impl Drop for ResumeHandler {
    fn drop(&mut self) {
        if self.suspended {
            let _ = self.runtime.resume_handler();
        }
    }
}

struct Shared {
    buffers: UnsafeCell<[[u8; RECORD_SIZE]; 2]>,
    template: Mutex<[u8; RECORD_SIZE]>,
    publish: Mutex<()>,
    metadata_generation: AtomicU64,
    active: AtomicUsize,
    reading: AtomicBool,
    in_handler: AtomicBool,
    stop: AtomicBool,
    sample_ready: AtomicBool,
    sample_thread_count: AtomicUsize,
    // Teardown wakes the sampler out of its cadence wait instead of letting
    // process exit pay for a sleep it cannot interrupt. `stop_wait` guards the
    // `stop` transition itself, so a request can never land between the
    // sampler's check and its wait. The fatal callback touches neither.
    stop_wait: Mutex<()>,
    stop_signal: Condvar,
    // True only while the sampler is inside its allocating capture, which
    // symbolizes against the process binary and cannot be interrupted.
    sampling: AtomicBool,
    // Teardown never joins a sampler that is mid-capture: it hands the sink
    // path here and detaches, and the sampler removes the sink itself once it
    // has released `file`. `done` closes the race with a sampler that exits
    // between teardown's check and its hand-off.
    handoff: Mutex<SamplerHandoff>,
    file: File,
    pid: u32,
}

#[derive(Default)]
struct SamplerHandoff {
    done: bool,
    sink: Option<PathBuf>,
}

// The sampler is the sole normal writer. The handler sets `reading` before
// touching the active buffer, and the sampler never overwrites either buffer
// while that flag is set. The active index is published with release/acquire.
unsafe impl Sync for Shared {}

impl Shared {
    fn new(file: File, template: [u8; RECORD_SIZE]) -> Self {
        Self {
            buffers: UnsafeCell::new([template, template]),
            template: Mutex::new(template),
            publish: Mutex::new(()),
            metadata_generation: AtomicU64::new(1),
            active: AtomicUsize::new(0),
            reading: AtomicBool::new(false),
            in_handler: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            sample_ready: AtomicBool::new(false),
            sample_thread_count: AtomicUsize::new(0),
            stop_wait: Mutex::new(()),
            stop_signal: Condvar::new(),
            sampling: AtomicBool::new(false),
            handoff: Mutex::new(SamplerHandoff::default()),
            file,
            pid: std::process::id(),
        }
    }

    /// Ask the sampler to stop and wake it out of its cadence wait.
    fn request_stop(&self) {
        let _wait = match self.stop_wait.lock() {
            Ok(wait) => wait,
            Err(poisoned) => poisoned.into_inner(),
        };
        self.stop.store(true, Ordering::Release);
        self.stop_signal.notify_all();
    }

    /// Park until `cadence` elapses or teardown asks the sampler to stop, and
    /// report whether a stop was requested. A spurious wakeup only shortens a
    /// cadence tick, which the loop handles by sampling again.
    fn wait_for_stop(&self, cadence: Duration) -> bool {
        let wait = match self.stop_wait.lock() {
            Ok(wait) => wait,
            Err(poisoned) => poisoned.into_inner(),
        };
        if self.stop.load(Ordering::Acquire) {
            return true;
        }
        let (wait, _timeout) = match self.stop_signal.wait_timeout(wait, cadence) {
            Ok(result) => result,
            Err(poisoned) => poisoned.into_inner(),
        };
        let stopped = self.stop.load(Ordering::Acquire);
        drop(wait);
        stopped
    }

    fn set_metadata(&self, generation: u64, metadata: &CrashMetadata) {
        let _publish = match self.publish.lock() {
            Ok(publish) => publish,
            Err(poisoned) => poisoned.into_inner(),
        };
        if generation <= self.metadata_generation.load(Ordering::Acquire) {
            return;
        }
        let mut template = match self.template.lock() {
            Ok(template) => template,
            Err(poisoned) => poisoned.into_inner(),
        };
        spool::put_metadata(&mut template, metadata);
        let inactive = 1 - self.active.load(Ordering::Acquire);
        // SAFETY: `publish` serializes normal writers. The callback can only
        // read the currently active buffer, while this writes the inactive
        // one and publishes it afterward.
        let target = unsafe { &mut (*self.buffers.get())[inactive] };
        *target = *template;
        self.metadata_generation
            .store(generation, Ordering::Release);
        self.active.store(inactive, Ordering::Release);
    }

    fn handle_crash(&self, context: &CrashContext) {
        if self.pid != std::process::id() {
            return;
        }
        if self
            .in_handler
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        self.reading.store(true, Ordering::SeqCst);
        let index = self.active.load(Ordering::Acquire);
        // SAFETY: `reading` excludes the sampler from this active buffer.
        let record = unsafe { (*self.buffers.get())[index].as_mut_ptr() };
        with_platform_fields(context, |fields| {
            // SAFETY: the context lives for this callback, and `record` names
            // the exclusive active fixed buffer.
            unsafe {
                spool::put_crash(
                    record,
                    fields.tid,
                    fields.code,
                    fields.address,
                    fields.raw,
                    fields.raw_len,
                );
                write_once(&self.file, record, RECORD_SIZE);
            }
        });
        self.reading.store(false, Ordering::Release);
        self.in_handler.store(false, Ordering::Release);
    }

    #[cfg(any(windows, target_os = "macos"))]
    fn handle_abort(&self, tid: u64, code: i64) {
        if self.pid != std::process::id() {
            return;
        }
        if self
            .in_handler
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        self.reading.store(true, Ordering::SeqCst);
        let index = self.active.load(Ordering::Acquire);
        // SAFETY: `reading` excludes the sampler from this active buffer.
        let record = unsafe { (*self.buffers.get())[index].as_mut_ptr() };
        // SAFETY: fixed active buffer; the CRT abort path supplies no
        // EXCEPTION_POINTERS, so the bounded all-thread sample is the register
        // evidence for this synthetic fatal event.
        unsafe {
            spool::put_crash(record, tid, code, 0, std::ptr::null(), 0);
            write_once(&self.file, record, RECORD_SIZE);
        }
        self.reading.store(false, Ordering::Release);
        self.in_handler.store(false, Ordering::Release);
    }
}

#[cfg(windows)]
static WINDOWS_ABORT_SHARED: std::sync::atomic::AtomicPtr<Shared> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
#[cfg(windows)]
static WINDOWS_PREVIOUS_ABORT: AtomicUsize = AtomicUsize::new(0);
#[cfg(windows)]
static WINDOWS_ABORT_READERS: AtomicUsize = AtomicUsize::new(0);

#[cfg(windows)]
fn windows_previous_abort_handler() -> io::Result<usize> {
    // `signal` has no read-only query. Swap to default and immediately restore
    // in ordinary install context, before crash-handler attaches.
    let previous = unsafe { libc::signal(libc::SIGABRT, libc::SIG_DFL) };
    if previous == usize::MAX {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        libc::signal(libc::SIGABRT, previous);
    }
    Ok(previous)
}

#[cfg(windows)]
fn install_windows_abort_chain(shared: &Arc<Shared>, previous: usize) -> io::Result<()> {
    // crash-handler captures the pre-existing abort handler but its Windows
    // CRT shim does not invoke it for `Handled(false)`. Replace only that shim
    // with a tiny wrapper that writes our record and then calls the handler
    // that existed before crash-handler attached (notably Python faulthandler).
    WINDOWS_PREVIOUS_ABORT.store(previous, Ordering::Release);
    WINDOWS_ABORT_SHARED.store(Arc::as_ptr(shared).cast_mut(), Ordering::Release);
    let replaced =
        unsafe { libc::signal(libc::SIGABRT, windows_abort_handler as *const () as usize) };
    if replaced == usize::MAX {
        WINDOWS_ABORT_SHARED.store(std::ptr::null_mut(), Ordering::Release);
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn uninstall_windows_abort_chain() {
    WINDOWS_ABORT_SHARED.store(std::ptr::null_mut(), Ordering::SeqCst);
    // A callback that acquired the old pointer announces itself before the
    // load. Wait in normal teardown context until it has finished using
    // `Shared`; callbacks arriving after the null publication never deref it.
    while WINDOWS_ABORT_READERS.load(Ordering::SeqCst) != 0 {
        std::hint::spin_loop();
    }
    let previous = WINDOWS_PREVIOUS_ABORT.swap(0, Ordering::AcqRel);
    if previous != 0 {
        unsafe {
            libc::signal(libc::SIGABRT, previous);
        }
    }
}

#[cfg(windows)]
unsafe extern "C" fn windows_abort_handler(signal: i32, _subcode: i32) {
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;

    WINDOWS_ABORT_READERS.fetch_add(1, Ordering::SeqCst);
    let shared = WINDOWS_ABORT_SHARED.load(Ordering::SeqCst);
    if let Some(shared) = unsafe { shared.as_ref() } {
        shared.handle_abort(
            u64::from(GetCurrentThreadId()),
            i64::from(crash_handler::ExceptionCode::Abort as i32),
        );
    }
    WINDOWS_ABORT_READERS.fetch_sub(1, Ordering::SeqCst);

    let previous = WINDOWS_PREVIOUS_ABORT.load(Ordering::Acquire);
    // Let the CRT invoke the predecessor using its exact private ABI. Calling
    // CPython faulthandler's pointer directly is not equivalent on Windows and
    // turns an abort into a secondary access violation.
    unsafe {
        libc::signal(libc::SIGABRT, previous);
        libc::raise(signal);
        libc::signal(libc::SIGABRT, windows_abort_handler as *const () as usize);
    }
}

#[cfg(target_os = "macos")]
static MACOS_PREVIOUS_ABORT: std::sync::atomic::AtomicPtr<libc::sigaction> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
#[cfg(target_os = "macos")]
static MACOS_ABORT_SHARED: std::sync::atomic::AtomicPtr<Shared> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
#[cfg(target_os = "macos")]
static MACOS_ABORT_READERS: AtomicUsize = AtomicUsize::new(0);

#[cfg(target_os = "macos")]
fn macos_previous_abort_action() -> io::Result<libc::sigaction> {
    let mut previous = std::mem::MaybeUninit::uninit();
    let result = unsafe { libc::sigaction(libc::SIGABRT, std::ptr::null(), previous.as_mut_ptr()) };
    if result == 0 {
        Ok(unsafe { previous.assume_init() })
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn install_macos_abort_chain(shared: &Arc<Shared>, previous: libc::sigaction) -> io::Result<()> {
    // crash-handler's macOS SIGABRT shim reports the synthetic Mach event but
    // does not dispatch the predecessor when our callback returns
    // `Handled(false)`. Replace only that shim so Python faulthandler and
    // application-owned abort actions retain their exact sigaction ABI.
    //
    // A signal delivery can select this wrapper before teardown restores the
    // predecessor, yet enter after the reader count was observed at zero.
    // Publish the new immutable generation before retiring the old one. The
    // callback increments its hazard count before loading this pointer, so a
    // delayed entry loads the new generation while every callback that could
    // have loaded the old one is included in the drain below.
    let previous = Box::into_raw(Box::new(previous));
    let retired = MACOS_PREVIOUS_ABORT.swap(previous, Ordering::SeqCst);
    while MACOS_ABORT_READERS.load(Ordering::SeqCst) != 0 {
        std::hint::spin_loop();
    }
    if !retired.is_null() {
        unsafe {
            drop(Box::from_raw(retired));
        }
    }
    MACOS_ABORT_SHARED.store(Arc::as_ptr(shared).cast_mut(), Ordering::Release);

    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaddset(&mut action.sa_mask, libc::SIGABRT);
    }
    action.sa_sigaction = macos_abort_handler as *const () as usize;
    action.sa_flags = libc::SA_SIGINFO;
    if unsafe { libc::sigaction(libc::SIGABRT, &action, std::ptr::null_mut()) } != 0 {
        let error = io::Error::last_os_error();
        // A delayed callback from the retired wrapper can have observed the
        // newly published Shared. Clear it and drain before Runtime::new is
        // allowed to drop the final Arc on this error path.
        MACOS_ABORT_SHARED.store(std::ptr::null_mut(), Ordering::SeqCst);
        while MACOS_ABORT_READERS.load(Ordering::SeqCst) != 0 {
            std::hint::spin_loop();
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_macos_abort_chain() {
    // Close handler entry before checking the reader hazard count. Leaving the
    // wrapper installed until after a zero read would allow a fresh callback
    // to race the next generation's predecessor publication.
    unsafe {
        let previous = MACOS_PREVIOUS_ABORT.load(Ordering::Acquire);
        debug_assert!(!previous.is_null());
        libc::sigaction(libc::SIGABRT, previous, std::ptr::null_mut());
    }
    MACOS_ABORT_SHARED.store(std::ptr::null_mut(), Ordering::SeqCst);
    while MACOS_ABORT_READERS.load(Ordering::SeqCst) != 0 {
        std::hint::spin_loop();
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn macos_abort_handler(
    signal: i32,
    _info: *mut libc::siginfo_t,
    _context: *mut std::ffi::c_void,
) {
    MACOS_ABORT_READERS.fetch_add(1, Ordering::SeqCst);
    let previous = MACOS_PREVIOUS_ABORT.load(Ordering::SeqCst);
    let shared = MACOS_ABORT_SHARED.load(Ordering::SeqCst);
    if let Some(shared) = unsafe { shared.as_ref() } {
        // Avoid pthread introspection in signal context; the already-published
        // all-thread sample carries the platform thread identifiers.
        shared.handle_abort(0, i64::from(signal));
    }

    // Restore and re-raise instead of calling the function pointer directly:
    // this preserves SA_SIGINFO, SA_RESETHAND, masks, and default/ignore
    // actions for CPython and arbitrary application predecessors.
    unsafe {
        libc::sigaction(signal, previous, std::ptr::null_mut());
    }
    MACOS_ABORT_READERS.fetch_sub(1, Ordering::SeqCst);
    unsafe {
        libc::raise(signal);
    }
}

struct PlatformFields {
    tid: u64,
    code: i64,
    address: u64,
    raw: *const u8,
    raw_len: usize,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn with_platform_fields<R>(
    context: &CrashContext,
    callback: impl FnOnce(PlatformFields) -> R,
) -> R {
    callback(PlatformFields {
        tid: context.tid as u64,
        code: i64::from(context.siginfo.ssi_signo),
        address: context.siginfo.ssi_addr,
        raw: context.as_bytes().as_ptr(),
        raw_len: context.as_bytes().len(),
    })
}

#[cfg(windows)]
fn with_platform_fields<R>(
    context: &CrashContext,
    callback: impl FnOnce(PlatformFields) -> R,
) -> R {
    let mut address = 0u64;
    let mut raw = (context as *const CrashContext).cast::<u8>();
    let mut raw_len = std::mem::size_of::<CrashContext>();
    // SAFETY: crash-handler supplies live EXCEPTION_POINTERS for the duration
    // of the callback. Prefer the full register CONTEXT over the pointer-only
    // wrapper.
    unsafe {
        if let Some(pointers) = context.exception_pointers.as_ref() {
            if let Some(exception) = pointers.ExceptionRecord.as_ref() {
                address = exception.ExceptionAddress as usize as u64;
            }
            if let Some(registers) = pointers.ContextRecord.as_ref() {
                raw = std::ptr::from_ref(registers).cast::<u8>();
                raw_len = std::mem::size_of_val(registers);
            }
        }
    }
    callback(PlatformFields {
        tid: u64::from(context.thread_id),
        code: i64::from(context.exception_code),
        address,
        raw,
        raw_len,
    })
}

#[cfg(target_os = "macos")]
fn with_platform_fields<R>(
    context: &CrashContext,
    callback: impl FnOnce(PlatformFields) -> R,
) -> R {
    use mach2::kern_return::KERN_SUCCESS;
    use mach2::thread_act::thread_get_state;

    let mut identifier: libc::thread_identifier_info_data_t = unsafe { std::mem::zeroed() };
    let mut identifier_count = libc::THREAD_IDENTIFIER_INFO_COUNT;
    // SAFETY: the exception context supplies a live Mach thread port, and the
    // flavor/count pair matches thread_identifier_info_data_t.
    let identifier_result = unsafe {
        libc::thread_info(
            context.thread,
            libc::THREAD_IDENTIFIER_INFO as libc::thread_flavor_t,
            (&raw mut identifier).cast(),
            &raw mut identifier_count,
        )
    };
    let tid = if identifier_result == KERN_SUCCESS {
        identifier.thread_id
    } else {
        u64::from(context.thread)
    };
    let code = context
        .exception
        .map(|exception| i64::from(exception.kind))
        .unwrap_or(0);
    let address = context
        .exception
        .and_then(|exception| exception.subcode)
        .unwrap_or(0);

    #[cfg(target_arch = "x86_64")]
    {
        use mach2::structs::x86_thread_state64_t;
        use mach2::thread_status::x86_THREAD_STATE64;
        let mut state = x86_thread_state64_t::new();
        let mut count = x86_thread_state64_t::count();
        // SAFETY: crash-handler supplies a live suspended Mach thread port,
        // and the state/count pair matches x86_THREAD_STATE64.
        let result = unsafe {
            thread_get_state(
                context.thread,
                x86_THREAD_STATE64,
                (&raw mut state).cast(),
                &raw mut count,
            )
        };
        if result == KERN_SUCCESS && count >= x86_thread_state64_t::count() {
            return callback(PlatformFields {
                tid,
                code,
                address,
                raw: std::ptr::from_ref(&state).cast(),
                raw_len: std::mem::size_of_val(&state),
            });
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        use mach2::structs::arm_thread_state64_t;
        use mach2::thread_status::ARM_THREAD_STATE64;
        let mut state = arm_thread_state64_t::new();
        let mut count = arm_thread_state64_t::count();
        // SAFETY: crash-handler supplies a live suspended Mach thread port,
        // and the state/count pair matches ARM_THREAD_STATE64.
        let result = unsafe {
            thread_get_state(
                context.thread,
                ARM_THREAD_STATE64,
                (&raw mut state).cast(),
                &raw mut count,
            )
        };
        if result == KERN_SUCCESS && count >= arm_thread_state64_t::count() {
            return callback(PlatformFields {
                tid,
                code,
                address,
                raw: std::ptr::from_ref(&state).cast(),
                raw_len: std::mem::size_of_val(&state),
            });
        }
    }

    // Unsupported architecture or a failed Mach state read still preserves
    // the exception metadata, but never mistakes pointer-only data for a
    // register context.
    callback(PlatformFields {
        tid,
        code,
        address,
        raw: std::ptr::null(),
        raw_len: 0,
    })
}

fn sampler_loop(shared: Arc<Shared>, config: CrashSamplerConfig) {
    let mut resolver =
        crate::snapshot::SessionResolver::for_crash(&crate::snapshot::SnapshotConfig::default());
    let mut previous = None;
    let mut cadence = config.interval;
    while !shared.stop.load(Ordering::Acquire) {
        if shared.wait_for_stop(cadence) {
            break;
        }
        if !shared.reading.load(Ordering::Acquire) {
            let tick_start = std::time::Instant::now();
            shared.sampling.store(true, Ordering::Release);
            let sample = capture_sample(&mut resolver);
            shared.sampling.store(false, Ordering::Release);
            // Recheck after the allocating capture: the callback may have
            // started while capture was in progress.
            if let Some(sample) = sample {
                cadence = if previous.as_ref() == Some(&sample) {
                    cadence.saturating_mul(2).min(config.max_interval)
                } else {
                    config.interval
                };
                if !shared.reading.load(Ordering::Acquire) {
                    let _publish = match shared.publish.lock() {
                        Ok(publish) => publish,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    let mut template = match shared.template.lock() {
                        Ok(template) => template,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    spool::put_sample(&mut template, &sample.modules, &sample.threads);
                    let inactive = 1 - shared.active.load(Ordering::Acquire);
                    // SAFETY: sampler is the only normal writer and writes
                    // only the inactive buffer while holding `publish`.
                    let target = unsafe { &mut (*shared.buffers.get())[inactive] };
                    *target = *template;
                    shared.active.store(inactive, Ordering::Release);
                    shared
                        .sample_thread_count
                        .store(sample.threads.len(), Ordering::Release);
                    shared.sample_ready.store(true, Ordering::Release);
                    previous = Some(sample);
                }
            }
            // Preserve a one-percent duty-cycle headroom even when snapshots
            // differ on every tick. This bounds sampler CPU without dropping
            // threads from the last complete pre-crash capture.
            cadence = cadence.max(tick_start.elapsed().saturating_mul(100));
        }
    }
    let sink = {
        let mut handoff = match shared.handoff.lock() {
            Ok(handoff) => handoff,
            Err(poisoned) => poisoned.into_inner(),
        };
        handoff.done = true;
        handoff.sink.take()
    };
    if let Some(sink) = sink {
        // Teardown detached instead of joining; Windows only deletes the sink
        // once every handle is gone, so release ours first.
        drop(shared);
        let _ = std::fs::remove_file(sink);
    }
}

#[cfg(all(
    any(windows, target_os = "linux", target_os = "macos"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn capture_sample(resolver: &mut crate::snapshot::SessionResolver) -> Option<CrashSample> {
    let Ok(attributed) = resolver.capture_attributed() else {
        return None;
    };
    // Every sibling can miss the signal deadline under load (#418). An empty
    // sample is no sample: publishing it would replace the last good one.
    if attributed.threads.is_empty() {
        return None;
    }
    Some(CrashSample {
        modules: attributed
            .modules
            .into_iter()
            .map(|module| CrashModule {
                identity: module.path.unwrap_or(module.name),
            })
            .collect(),
        threads: attributed
            .threads
            .into_iter()
            .map(|thread| CrashThread {
                os_tid: thread.os_tid,
                frames: thread
                    .frames
                    .into_iter()
                    .map(|frame| CrashFrame {
                        module_index: frame.module_index,
                        relative_address: frame.relative_address,
                    })
                    .collect(),
            })
            .collect(),
    })
}

#[cfg(not(all(
    any(windows, target_os = "linux", target_os = "macos"),
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn capture_sample(_resolver: &mut crate::snapshot::SessionResolver) -> Option<CrashSample> {
    None
}

#[derive(Default, PartialEq, Eq)]
struct CrashSample {
    modules: Vec<CrashModule>,
    threads: Vec<CrashThread>,
}

/// One bounded file write. Partial writes remain parse-invalid and are left
/// for forensic inspection rather than retried from a compromised context.
///
/// # Safety
///
/// `record` must name `length` readable bytes.
#[cfg(unix)]
unsafe fn write_once(file: &File, record: *const u8, length: usize) {
    use std::os::fd::AsRawFd as _;
    // SAFETY: caller contract; `write` is async-signal-safe.
    let _ = unsafe { libc::write(file.as_raw_fd(), record.cast(), length) };
}

/// One bounded Windows kernel write.
///
/// # Safety
///
/// `record` must name `length` readable bytes.
#[cfg(windows)]
unsafe fn write_once(file: &File, record: *const u8, length: usize) {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::WriteFile;
    let mut written = 0u32;
    // SAFETY: caller contract and a live pre-opened file handle.
    let _ = unsafe {
        WriteFile(
            file.as_raw_handle() as _,
            record.cast(),
            length as u32,
            &raw mut written,
            std::ptr::null_mut(),
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    static PROCESS_STATE: Mutex<()> = Mutex::new(());

    fn process_state() -> std::sync::MutexGuard<'static, ()> {
        match PROCESS_STATE.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    #[test]
    fn attach_failures_are_classified_into_facade_kinds() {
        let already = HandlerError::from_backend(crash_handler::Error::HandlerAlreadyInstalled);
        assert_eq!(already.kind(), HandlerErrorKind::AlreadyInstalled);
        assert_eq!(
            HandlerError::from_backend(crash_handler::Error::OutOfMemory).kind(),
            HandlerErrorKind::OutOfMemory
        );
        let os =
            HandlerError::from_backend(crash_handler::Error::Io(io::Error::from_raw_os_error(1)));
        assert_eq!(os.kind(), HandlerErrorKind::Os);
        assert!(!os.message().is_empty());
        assert_eq!(
            InstallError::Handler(already.clone()).to_string(),
            format!("cannot attach native crash handler: {}", already.message())
        );
    }

    #[test]
    fn off_policy_is_inert() {
        let _state = process_state();
        let guard = install(
            CrashPolicy::Off,
            CrashMetadata {
                app_class: "a".into(),
                app_name: "a".into(),
                app_version: "1".into(),
                instance_name: String::new(),
                creation_time_ms: 1,
                cwd: "/test".into(),
            },
        )
        .unwrap();
        assert!(!guard.is_armed());
        assert!(guard.spool_path().is_none());
    }

    #[test]
    fn opt_out_values_are_strict_and_documented() {
        let _state = process_state();
        assert!(!matches!(CrashPolicy::default(), CrashPolicy::Off));
    }

    fn metadata(name: &str) -> CrashMetadata {
        CrashMetadata {
            app_class: name.into(),
            app_name: name.into(),
            app_version: "1".into(),
            instance_name: String::new(),
            creation_time_ms: 1,
            cwd: "/test".into(),
        }
    }

    #[test]
    fn independent_guards_reselect_oldest_live_metadata() {
        let _state = process_state();
        let first = install(CrashPolicy::On, metadata("first")).unwrap();
        let second = install(CrashPolicy::On, metadata("second")).unwrap();
        assert!(first.is_armed());
        assert!(second.is_armed());
        drop(first);
        let template = match second.runtime.as_ref().unwrap().shared.template.lock() {
            Ok(template) => *template,
            Err(poisoned) => *poisoned.into_inner(),
        };
        assert_eq!(
            spool::parse(&template).unwrap().metadata.app_class,
            "second"
        );
        drop(second);
    }

    #[test]
    fn external_handler_transitions_are_serialized_and_rearm() {
        let _state = process_state();
        let guard = install(CrashPolicy::On, metadata("transition-owner")).unwrap();
        let start = Arc::new(std::sync::Barrier::new(3));
        let inside = Arc::new(AtomicUsize::new(0));
        let overlapped = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let start = Arc::clone(&start);
            let inside = Arc::clone(&inside);
            let overlapped = Arc::clone(&overlapped);
            threads.push(std::thread::spawn(move || {
                start.wait();
                with_handler_suspended(|| {
                    if inside.fetch_add(1, Ordering::AcqRel) != 0 {
                        overlapped.store(true, Ordering::Release);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                    inside.fetch_sub(1, Ordering::AcqRel);
                })
                .unwrap();
            }));
        }
        start.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(!overlapped.load(Ordering::Acquire));
        assert!(guard.is_armed());
    }

    #[test]
    fn install_waits_for_external_handler_transition() {
        let _state = process_state();
        let live_runtime = RUNTIME
            .get()
            .and_then(|slot| slot.lock().ok())
            .and_then(|weak| weak.upgrade());
        assert!(
            live_runtime.is_none(),
            "the first-attach race requires no existing runtime"
        );
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let transition = std::thread::spawn(move || {
            with_handler_suspended(|| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .unwrap();
        });
        entered_rx.recv().unwrap();

        let (installed_tx, installed_rx) = std::sync::mpsc::channel();
        let installer = std::thread::spawn(move || {
            let guard = install(CrashPolicy::On, metadata("transition-first")).unwrap();
            installed_tx.send(guard).unwrap();
        });
        assert!(
            installed_rx
                .recv_timeout(Duration::from_millis(30))
                .is_err(),
            "install returned while native interception was suspended"
        );
        release_tx.send(()).unwrap();
        transition.join().unwrap();
        let first = installed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        installer.join().unwrap();
        assert!(first.is_armed());
    }

    #[test]
    fn failed_external_handler_rearm_is_reported_unarmed() {
        let _state = process_state();
        let guard = install(CrashPolicy::On, metadata("failed-rearm")).unwrap();
        let foreign = Arc::new(Mutex::new(None));
        let foreign_slot = Arc::clone(&foreign);
        let result = with_handler_suspended(|| {
            // SAFETY: the test callback is allocation-free and intentionally
            // does nothing; this handler only occupies crash-handler's global
            // slot so the runtime's reattach attempt has a stable failure.
            let event =
                unsafe { crash_handler::make_crash_event(|_| CrashEventResult::Handled(false)) };
            let handler = CrashHandler::attach(event).unwrap();
            match foreign_slot.lock() {
                Ok(mut slot) => *slot = Some(handler),
                Err(poisoned) => *poisoned.into_inner() = Some(handler),
            }
        });
        assert!(matches!(result, Err(InstallError::Handler(_))));
        assert!(!guard.is_armed());

        // Release the deliberately competing global handler before this
        // runtime is dropped, leaving subsequent tests a clean process.
        match foreign.lock() {
            Ok(mut slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };

        with_handler_suspended(|| {}).unwrap();
        assert!(
            guard.is_armed(),
            "the next transition must retry a failed reattach"
        );

        let foreign_slot = Arc::clone(&foreign);
        let second_failure = with_handler_suspended(|| {
            // SAFETY: same inert test callback as above.
            let event =
                unsafe { crash_handler::make_crash_event(|_| CrashEventResult::Handled(false)) };
            let handler = CrashHandler::attach(event).unwrap();
            match foreign_slot.lock() {
                Ok(mut slot) => *slot = Some(handler),
                Err(poisoned) => *poisoned.into_inner() = Some(handler),
            }
        });
        assert!(matches!(second_failure, Err(InstallError::Handler(_))));
        match foreign.lock() {
            Ok(mut slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        let second = install(CrashPolicy::On, metadata("retried-install")).unwrap();
        assert!(
            guard.is_armed() && second.is_armed(),
            "an ordinary install must retry a failed reattach"
        );
        drop(second);
        drop(guard);
    }

    #[test]
    fn failed_install_rearm_drops_last_runtime_after_transition_gate() {
        let _state = process_state();
        // This regression exercises only the transition/handler teardown
        // ordering. A live sampler can be resolving a process-wide capture
        // while unrelated tests run, which would turn its normal join into a
        // timing-dependent part of this lock-order assertion.
        let _sampler = disable_test_sampler();
        let guard = install(CrashPolicy::On, metadata("last-guard")).unwrap();
        let foreign = Arc::new(Mutex::new(None));
        let foreign_slot = Arc::clone(&foreign);
        let failure = with_handler_suspended(|| {
            // SAFETY: inert callback used only to occupy the global handler.
            let event =
                unsafe { crash_handler::make_crash_event(|_| CrashEventResult::Handled(false)) };
            let handler = CrashHandler::attach(event).unwrap();
            match foreign_slot.lock() {
                Ok(mut slot) => *slot = Some(handler),
                Err(poisoned) => *poisoned.into_inner() = Some(handler),
            }
        });
        assert!(matches!(failure, Err(InstallError::Handler(_))));
        assert!(!guard.is_armed());

        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let _pause = set_test_resume_pause(TestResumePause {
            entered: entered_tx,
            release: release_rx,
        });
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let installer = std::thread::spawn(move || {
            result_tx
                .send(install(CrashPolicy::On, metadata("racer")))
                .unwrap();
        });
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("install never reached the forced reattach point");

        // The installer now owns the only other Runtime Arc and is paused
        // while holding both the transition and handler locks.
        drop(guard);
        release_tx.send(()).expect("release paused installer");
        let result = result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("failed reattach deadlocked while dropping the final Runtime Arc");
        assert!(matches!(result, Err(InstallError::Handler(_))));
        installer.join().unwrap();

        match foreign.lock() {
            Ok(mut slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
    }

    #[cfg(unix)]
    #[test]
    fn post_fork_child_is_detected_before_touching_inherited_locks() {
        let _state = process_state();
        let guard = install(CrashPolicy::On, metadata("fork-owner")).unwrap();
        // SAFETY: the child performs only the PID-gated install check and
        // `_exit`; it never runs inherited Rust destructors.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed");
        if child == 0 {
            let rejected = matches!(
                install(CrashPolicy::On, metadata("fork-owner")),
                Err(InstallError::ForkedProcess)
            );
            unsafe { libc::_exit(i32::from(!rejected)) };
        }
        let mut status = 0;
        // SAFETY: `child` is the live pid returned by fork.
        assert_eq!(unsafe { libc::waitpid(child, &raw mut status, 0) }, child);
        assert_eq!(status, 0, "child touched inherited crash state");
        drop(guard);
    }

    #[test]
    fn teardown_detaches_from_a_sampler_that_is_mid_capture() {
        let _state = process_state();
        let _no_sampler = disable_test_sampler();
        let (runtime, _id) =
            Runtime::new(metadata("sampler-detach"), CrashSamplerConfig::default()).unwrap();
        let mut runtime = Arc::try_unwrap(runtime).ok().expect("sole runtime owner");
        let path = runtime.path.clone();
        // Stand in for the first sample's uninterruptible symbolization: the
        // thread reports it is capturing, holds that for well over the
        // ceiling below, then runs the real loop, which sees the stop request
        // and performs the sink hand-off.
        let capturing = Arc::clone(&runtime.shared);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        runtime.sampler = Some(std::thread::spawn(move || {
            capturing.sampling.store(true, Ordering::Release);
            entered_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(1500));
            capturing.sampling.store(false, Ordering::Release);
            sampler_loop(capturing, CrashSamplerConfig::default());
        }));
        entered_rx.recv().unwrap();

        let start = std::time::Instant::now();
        drop(runtime);
        let teardown = start.elapsed();
        assert!(
            teardown < Duration::from_millis(500),
            "teardown joined an in-flight capture instead of detaching ({teardown:?})"
        );
        // The detached sampler removes the sink once it releases its handle.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            !path.exists(),
            "detached sampler left the crash sink behind"
        );
    }

    #[test]
    fn shutdown_wakes_the_sampler_instead_of_waiting_out_its_cadence() {
        let _state = process_state();
        let (file, path, template) = spool::create_sink(&metadata("sampler-shutdown")).unwrap();
        let shared = Arc::new(Shared::new(file, template));
        // A capture in progress is what the sampler skips, so the thread parks
        // in the cadence wait instead of resolving a snapshot. The cadence is
        // far longer than the teardown ceiling below, so a sampler that slept
        // it out could not pass.
        shared.reading.store(true, Ordering::Release);
        let cadence = Duration::from_secs(30);
        let sampler = start_sampler(
            &shared,
            CrashSamplerConfig {
                interval: cadence,
                max_interval: cadence,
            },
        )
        .unwrap();
        // Not an assertion: this only gives the thread time to reach the wait
        // so an uninterruptible sleep reproduces as a failure rather than a
        // race against the loop's own stop check.
        std::thread::sleep(Duration::from_millis(100));

        shared.request_stop();
        let start = std::time::Instant::now();
        sampler.join().unwrap();
        let teardown = start.elapsed();
        assert!(
            teardown < Duration::from_secs(5),
            "sampler teardown waited out its cadence instead of being woken ({teardown:?})"
        );

        // The spool file is owned by `Shared`; Windows only deletes it once
        // every handle is gone.
        drop(shared);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sampler_preserves_thread_frames_and_module_indices() {
        let _state = process_state();
        let mut resolver = crate::snapshot::SessionResolver::for_crash(
            &crate::snapshot::SnapshotConfig::default(),
        );
        if crate::snapshot::capture_all_threads(&crate::snapshot::SnapshotConfig::default())
            .is_err()
        {
            return; // Unsupported host/architecture has no native sampler.
        }
        // A parked sibling guarantees a thread to sample. Under load a capture
        // may still miss every signal deadline (#418), which `capture_sample`
        // reports as no sample; a few attempts separate that from frame loss.
        let (stop, parked) = std::sync::mpsc::channel::<()>();
        let sibling = std::thread::spawn(move || {
            let _ = parked.recv();
        });
        let sample = (0..5).find_map(|_| capture_sample(&mut resolver));
        drop(stop);
        sibling.join().expect("parked sibling");
        let sample = sample.expect("five captures each missed every thread");
        assert!(!sample.threads.is_empty(), "sampler dropped every thread");
        assert!(sample
            .threads
            .iter()
            .all(|thread| !thread.frames.is_empty()));
        assert!(sample
            .modules
            .iter()
            .all(|module| !module.identity.is_empty()));
        for thread in &sample.threads {
            for frame in &thread.frames {
                assert!(frame.module_index.is_none_or(|index| {
                    usize::try_from(index).is_ok_and(|index| index < sample.modules.len())
                }));
            }
        }
    }
}
