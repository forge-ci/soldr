//! Cooperative all-thread stack capture (#635, S6).
//!
//! # Cooperative, not external
//!
//! The probe thread lives *inside* the target process and walks its own
//! sibling threads. There is no ptrace, no debugger attach, and no OS
//! capability grant — those belong to the later `--force` external tier.
//!
//! # The suspend window is the whole design
//!
//! A suspended thread may hold *any* lock, including the allocator's. So while
//! a thread is suspended this code does exactly two things: read its registers
//! and `memcpy` a bounded slice of its stack into a **preallocated** buffer.
//! Then it resumes immediately.
//!
//! Nothing else happens in that window — no allocation, no symbolization, no
//! logging, no lock acquisition. Unwinding and symbolization run afterward,
//! against the copied bytes, when every thread is running again. Violating
//! this is how a stack profiler deadlocks the process it is profiling: suspend
//! a thread inside `malloc`, then call `malloc` yourself.
//!
//! Windows, Linux, and macOS capture x86_64/aarch64 sibling stacks. Each
//! backend resumes before deferred PE/ELF/Mach-O unwinding. Other platforms
//! return [`SnapshotError::Unsupported`] rather than an empty snapshot.

// Both Windows architectures are supported. The register names and the
// unwinder differ per arch (see `windows.rs` / `unwind.rs`); everything else --
// enumeration, suspend/resume sequencing, stack-copy bounds -- is shared.
// Attribution needs a per-object-format module inventory.
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
pub mod attribute;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
pub mod modules;

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
pub mod unwind;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

// Deliberately not platform-gated: the sink is pure Rust, so every CI lane
// exercises the backpressure contract rather than only Windows.
pub mod stream;

use std::time::Duration;

/// Upper bound on the stack bytes copied per thread.
///
/// Bounded because the copy happens with the thread suspended: an unbounded
/// read would extend the window in proportion to stack depth. 256 KiB covers
/// realistic call depths while keeping the window short and the buffer
/// preallocatable.
pub const MAX_STACK_BYTES: usize = 256 * 1024;

/// How the capture was obtained, and what remains to be done to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureKind {
    /// Registers plus raw stack bytes. Not yet unwound into return addresses.
    RawContext,
}

/// One thread's captured state.
#[derive(Clone, Debug)]
pub struct ThreadSample {
    /// OS thread id.
    pub os_tid: u64,
    /// Stack pointer at capture time.
    pub stack_pointer: u64,
    /// Instruction pointer at capture time.
    pub instruction_pointer: u64,
    /// Frame pointer at capture time.
    pub frame_pointer: u64,
    /// Link register, on architectures that have one (aarch64).
    ///
    /// Load-bearing there: a leaf frame's return address lives in LR rather
    /// than on the stack, so unwinding without it loses the first frame.
    /// `None` on x86_64, which has no such register.
    pub link_register: Option<u64>,
    /// Bytes copied from the stack, starting at `stack_pointer`.
    pub stack_bytes: Vec<u8>,
    /// True when the stack was longer than [`MAX_STACK_BYTES`], so the copy is
    /// a prefix. A consumer must not read a truncated capture as a complete
    /// one.
    pub truncated: bool,
    /// What stage this sample is at.
    pub kind: CaptureKind,
    /// Return addresses, once unwinding has run. Empty until then — check
    /// [`Snapshot::frames_resolved`] rather than inferring from emptiness,
    /// since a thread with an unwalkable stack also yields none.
    pub frames: Vec<u64>,
}

/// What a capture cost and covered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SnapshotStats {
    /// Sibling threads observed during enumeration.
    pub threads_total: u32,
    /// Threads successfully captured.
    pub threads_captured: u32,
    /// Threads that could not be captured (exited mid-capture, access denied).
    ///
    /// Non-zero means the snapshot is partial.
    pub threads_dropped: u32,
    /// Total time any thread spent suspended. The cost imposed on the target.
    pub pause_nanos: u64,
}

/// The result of one capture.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Per-thread samples.
    pub threads: Vec<ThreadSample>,
    /// Coverage and cost.
    pub stats: SnapshotStats,
    /// Whether `frames` have been resolved from the raw captures.
    ///
    /// False for now on every platform — unwinding lands separately. Exposed
    /// so a consumer cannot mistake raw register/stack captures for symbolized
    /// or even unwound frames.
    pub frames_resolved: bool,
}

impl Snapshot {
    /// Whether every enumerated thread was captured.
    pub fn is_complete(&self) -> bool {
        self.stats.threads_dropped == 0
    }

    /// Total time threads spent suspended.
    pub fn pause(&self) -> Duration {
        Duration::from_nanos(self.stats.pause_nanos)
    }
}

/// Knobs for a capture.
#[derive(Clone, Copy, Debug)]
pub struct SnapshotConfig {
    /// Per-thread stack copy limit.
    pub max_stack_bytes: usize,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            max_stack_bytes: MAX_STACK_BYTES,
        }
    }
}

/// Why a capture could not run at all.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// No capture backend for this platform yet.
    #[error("cooperative snapshot is not implemented for this platform yet")]
    Unsupported,
    /// The OS refused an enumeration or capture call.
    #[error("snapshot failed: {0}")]
    Os(#[from] std::io::Error),
    /// A macOS Mach kernel operation failed.
    #[error("Mach operation {operation} failed with kernel code {code}")]
    Mach {
        /// Operation that failed.
        operation: &'static str,
        /// `kern_return_t` value.
        code: i32,
    },
}

/// Capture every sibling thread of the calling thread.
///
/// The calling thread is deliberately excluded: suspending yourself is an
/// immediate deadlock, and its stack is available directly anyway.
///
/// Returns [`SnapshotError::Unsupported`] on platforms whose backend has not
/// landed, rather than silently returning an empty snapshot that would read as
/// "this process has no threads".
pub fn capture_all_threads(config: &SnapshotConfig) -> Result<Snapshot, SnapshotError> {
    #[cfg(windows)]
    {
        windows::capture(config)
    }
    #[cfg(target_os = "linux")]
    {
        linux::capture(config)
    }
    #[cfg(target_os = "macos")]
    {
        macos::capture(config)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = config;
        Err(SnapshotError::Unsupported)
    }
}

/// Capture every sibling thread and resolve each capture to return addresses.
///
/// The two halves are separate functions because they have opposite
/// constraints — capture runs with threads suspended and must do almost
/// nothing, while unwinding runs afterwards and may allocate freely. Callers
/// that just want frames should not have to know that, or to remember that
/// resolving requires a module inventory taken from the same process.
///
/// Returns [`SnapshotError::Unsupported`] wherever [`capture_all_threads`]
/// does, so an unsupported platform is never mistaken for a thread-less
/// process.
pub fn capture_and_resolve(config: &SnapshotConfig) -> Result<Snapshot, SnapshotError> {
    #[cfg(windows)]
    {
        let mut snapshot = capture_all_threads(config)?;
        let modules = modules::enumerate_modules()?;
        unwind::resolve_frames(&mut snapshot, &modules);
        Ok(snapshot)
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let mut snapshot = capture_all_threads(config)?;
        unwind::resolve_frames_for_current_process(&mut snapshot)?;
        Ok(snapshot)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = config;
        Err(SnapshotError::Unsupported)
    }
}

/// A capture-and-resolve handle for callers that capture repeatedly.
///
/// [`capture_and_resolve`] is the one-shot form, and on Linux and macOS it
/// rebuilds the module inventory and unwinder on every call. That is fine once
/// and ruinous in a loop: measured on a debug `--all-features` build a single
/// call spent ~736 ms there, which caps a sampling session's rate at whatever
/// ~736 ms per capture allows regardless of the rate it requested (#131).
///
/// This type keeps that work between captures and rebuilds it only when the
/// mapped images change, so a session samples at the cadence it asked for
/// while still seeing a `dlopen`ed module. On Windows the inventory is
/// enumerated per capture, as it was before, because that platform builds it
/// from the toolhelp snapshot rather than from an executable's own images.
pub struct SessionResolver {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    resolver: unwind::FrameResolver,
    config: SnapshotConfig,
    modules: std::sync::Arc<Vec<modules::LoadedModule>>,
    // The Unix resolver consumes this; Windows still uses its existing
    // one-shot unwind path and never reads the cap.
    #[allow(dead_code)]
    unwind_limit: Option<usize>,
}

impl SessionResolver {
    /// Prepare a resolver. The inventory is built by the first
    /// [`Self::capture`], not here.
    pub fn new(config: &SnapshotConfig) -> Self {
        Self {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            resolver: unwind::FrameResolver::new(),
            config: *config,
            modules: std::sync::Arc::new(Vec::new()),
            unwind_limit: None,
        }
    }

    /// Crash snapshots bound changed-stack unwinds per tick; ordinary
    /// snapshot sessions continue resolving every captured thread.
    #[cfg(feature = "crash")]
    pub(crate) fn for_crash(config: &SnapshotConfig) -> Self {
        Self {
            unwind_limit: Some(16),
            ..Self::new(config)
        }
    }

    /// Capture every sibling thread and resolve each capture to frames.
    pub fn capture(&mut self) -> Result<Snapshot, SnapshotError> {
        let mut snapshot = capture_all_threads(&self.config)?;
        #[cfg(windows)]
        {
            self.modules = std::sync::Arc::new(modules::enumerate_modules()?);
            unwind::resolve_frames(&mut snapshot, &self.modules);
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.resolver
                .resolve_with_limit(&mut snapshot, self.unwind_limit)?;
            self.modules = self.resolver.modules();
        }
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
        return Err(SnapshotError::Unsupported);
        Ok(snapshot)
    }

    /// Capture, resolve and attribute frames using one module inventory.
    pub fn capture_attributed(&mut self) -> Result<attribute::AttributedCapture, SnapshotError> {
        let snapshot = self.capture()?;
        Ok(attribute::attribute(&snapshot, &self.modules))
    }

    #[cfg(test)]
    #[allow(dead_code)] // The inventory attribution test runs only on Linux.
    pub(crate) fn module_inventory(&self) -> &[modules::LoadedModule] {
        &self.modules
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_the_documented_cap() {
        assert_eq!(SnapshotConfig::default().max_stack_bytes, MAX_STACK_BYTES);
    }

    /// The combined entry point must resolve, not just capture.
    ///
    /// `capture_all_threads` alone leaves `frames` empty and
    /// `frames_resolved` false; this is the difference between the two.
    #[cfg(windows)]
    #[test]
    fn capture_and_resolve_produces_resolved_frames() {
        let snapshot = capture_and_resolve(&SnapshotConfig::default()).expect("capture");
        assert!(
            snapshot.frames_resolved,
            "the combined path must run the unwinder"
        );
        assert!(
            snapshot.threads.iter().any(|t| !t.frames.is_empty()),
            "at least one captured thread should yield frames"
        );
    }

    #[test]
    fn a_snapshot_with_drops_is_not_complete() {
        let mut snap = Snapshot::default();
        assert!(snap.is_complete());
        snap.stats.threads_dropped = 1;
        assert!(
            !snap.is_complete(),
            "a dropped thread must make the snapshot partial"
        );
    }

    /// Raw captures must never be mistaken for unwound frames.
    #[test]
    fn raw_captures_report_frames_unresolved() {
        let snap = Snapshot::default();
        assert!(!snap.frames_resolved);
    }

    #[test]
    fn pause_is_reported_in_wall_clock_terms() {
        let snap = Snapshot {
            stats: SnapshotStats {
                pause_nanos: 1_500_000,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(snap.pause(), Duration::from_micros(1500));
    }

    /// #635 known-stack acceptance: the named blocked function must survive
    /// capture and deferred unwinding as a raw return address.
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "Existing function complexity measured at ratchet adoption"
    )]
    fn known_blocked_stack_contains_marker_frame() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::sync::Arc;

        #[cfg(windows)]
        #[allow(unsafe_code)]
        fn current_tid() -> u64 {
            u64::from(unsafe { winapi::um::processthreadsapi::GetCurrentThreadId() })
        }
        #[cfg(target_os = "linux")]
        #[allow(unsafe_code)]
        fn current_tid() -> u64 {
            unsafe { libc::syscall(libc::SYS_gettid) as u64 }
        }
        #[cfg(target_os = "macos")]
        #[allow(unsafe_code)]
        fn current_tid() -> u64 {
            let mut tid = 0u64;
            let result = unsafe { libc::pthread_threadid_np(0, &mut tid) };
            assert_eq!(result, 0, "pthread_threadid_np");
            tid
        }

        #[cfg(not(target_arch = "x86_64"))]
        #[inline(never)]
        fn blocked_leaf(ready: &AtomicBool, stop: &AtomicBool) -> bool {
            ready.store(true, Ordering::Release);
            while !stop.load(Ordering::Acquire) {
                std::hint::spin_loop();
                std::hint::black_box(());
            }
            stop.load(Ordering::Acquire)
        }

        /// Keep every post-ready sample point inside this function on Windows.
        ///
        /// In an unoptimized MSVC build, `AtomicBool::load` may be emitted as
        /// an out-of-line helper. Sampling in that helper makes the fixture
        /// depend on unwinding compiler support code before it can find the
        /// marker. The single-byte load is atomic on x86_64, and x86 loads
        /// already have acquire ordering.
        #[cfg(all(target_arch = "x86_64", windows))]
        #[inline(never)]
        #[allow(unsafe_code)]
        fn blocked_leaf(ready: &AtomicBool, stop: &AtomicBool) -> bool {
            ready.store(true, Ordering::Release);
            let observed: u8;
            unsafe {
                std::arch::asm!(
                    "2:",
                    "mov {observed}, byte ptr [{stop}]",
                    "test {observed}, {observed}",
                    "je 2b",
                    stop = in(reg) stop.as_ptr(),
                    observed = out(reg_byte) observed,
                    options(nostack),
                );
            }
            observed != 0
        }

        /// A fixed SysV frame gives the unwinder a stable metadata and
        /// frame-pointer fallback case.
        ///
        /// Coverage instrumentation can otherwise add CFI-sensitive wrapper
        /// code around even a deterministic inline-assembly loop.
        #[cfg(all(target_arch = "x86_64", not(windows)))]
        #[unsafe(naked)]
        #[allow(unsafe_code)]
        extern "C" fn blocked_leaf(_ready: &AtomicBool, _stop: &AtomicBool) -> bool {
            std::arch::naked_asm!(
                ".cfi_startproc",
                "push rbp",
                ".cfi_def_cfa_offset 16",
                ".cfi_offset rbp, -16",
                "mov rbp, rsp",
                ".cfi_def_cfa_register rbp",
                "mov byte ptr [rdi], 1",
                "2:",
                "mov al, byte ptr [rsi]",
                "test al, al",
                "je 2b",
                "pop rbp",
                ".cfi_def_cfa rsp, 8",
                "ret",
                ".cfi_endproc",
            );
        }

        #[inline(never)]
        fn blocked_marker(ready: &AtomicBool, stop: &AtomicBool) {
            let observed = blocked_leaf(ready, stop);
            // This observable store depends on the leaf's return value, so it
            // cannot be hoisted or removed. That makes blocked_marker a real
            // caller frame even with coverage instrumentation.
            ready.store(observed, Ordering::Release);
        }

        let ready = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(1);
        let worker = {
            let ready = Arc::clone(&ready);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("blocked_marker".into())
                .spawn(move || {
                    tx.send(current_tid()).unwrap();
                    blocked_marker(&ready, &stop);
                })
                .unwrap()
        };
        let tid = rx.recv().unwrap();
        while !ready.load(Ordering::Acquire) {
            std::thread::yield_now();
        }

        let snapshot = capture_and_resolve(&SnapshotConfig::default()).expect("capture + unwind");
        stop.store(true, Ordering::Release);
        worker.join().unwrap();

        let sample = snapshot
            .threads
            .iter()
            .find(|sample| sample.os_tid == tid)
            .unwrap_or_else(|| panic!("named worker {tid} absent from snapshot"));
        let marker = blocked_marker as *const () as usize as u64;
        assert!(
            sample
                .frames
                .iter()
                .skip(1)
                .any(|frame| frame.abs_diff(marker) < 4096),
            "unwound caller frames did not contain blocked_marker near {marker:#x} \
             (captured ip={:#x}): {:?}",
            sample.instruction_pointer,
            sample.frames,
        );
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    #[test]
    fn unimplemented_platforms_report_unsupported_not_empty() {
        // An empty Ok(Snapshot) would read as "no threads", which is a very
        // different claim from "not implemented here".
        assert!(matches!(
            capture_all_threads(&SnapshotConfig::default()),
            Err(SnapshotError::Unsupported)
        ));
    }
}
