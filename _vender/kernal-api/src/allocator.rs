//! One allocator and heap-profiler facade for every client process.
//!
//! The profiler is compiled in but dormant until [`start`] or
//! [`start_from_env`] is called. Applications still declare their own
//! `#[global_allocator]`; this crate supplies the exact shared allocator type
//! and dump implementation without attempting to install process-wide policy
//! from a library.

use std::alloc::{GlobalAlloc, Layout};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Facade-owned mimalloc allocator with dormant heap sampling support.
pub struct Allocator;

static INNER_ALLOCATOR: mimalloc_pprof::MiMalloc = mimalloc_pprof::MiMalloc;

impl Allocator {
    /// Construct the shared allocator for a `#[global_allocator]` static.
    pub const fn new() -> Self {
        Self
    }
}

impl Default for Allocator {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: every operation delegates unchanged layouts and pointers to the one
// process-wide mimalloc implementation selected by this facade.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { INNER_ALLOCATOR.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        unsafe { INNER_ALLOCATOR.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { INNER_ALLOCATOR.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        unsafe { INNER_ALLOCATOR.realloc(pointer, layout, size) }
    }
}

/// Default environment variable understood by [`start_from_env`].
pub const HEAP_PROFILE_ENV: &str = "KERNAL_API_HEAP_PROFILE";

/// mimalloc-pprof's low-overhead default sample rate (512 KiB).
pub const DEFAULT_SAMPLE_RATE: usize = 512 * 1024;

static DUMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Parse a heap-profile setting into a byte sampling interval.
pub fn sample_rate_from(value: &str) -> Option<usize> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "off" | "no" => None,
        "1" | "true" | "on" | "yes" => Some(DEFAULT_SAMPLE_RATE),
        other => other.parse::<usize>().ok().filter(|rate| *rate > 0),
    }
}

/// Start profiling from `env_var`, returning the selected sample rate.
pub fn start_from_named_env(env_var: &str) -> Option<usize> {
    let rate = sample_rate_from(&std::env::var(env_var).ok()?)?;
    mimalloc_pprof::prof::start(rate).then_some(rate)
}

/// Start profiling from [`HEAP_PROFILE_ENV`].
pub fn start_from_env() -> Option<usize> {
    start_from_named_env(HEAP_PROFILE_ENV)
}

/// Start profiling at the requested byte sampling interval.
pub fn start(sample_rate: usize) -> bool {
    sample_rate > 0 && mimalloc_pprof::prof::start(sample_rate)
}

/// Stop sampling. Retained samples remain dumpable.
pub fn stop() {
    mimalloc_pprof::prof::stop();
}

/// Whether the profiler is currently sampling.
pub fn is_enabled() -> bool {
    mimalloc_pprof::prof::is_enabled()
}

/// Number of currently live sampled allocations.
pub fn live_sample_count() -> usize {
    mimalloc_pprof::prof::stats().live_samples
}

/// Snapshot of the heap profiler's sampled counters plus the allocator's
/// exact heap counters, taken by [`stats`].
///
/// Every value except [`Self::heap`] is *sampled* and meaningful only while
/// (or after) the profiler runs; the heap counters are exact and valid even
/// while the profiler is dormant.
#[derive(Clone, Debug, Default)]
pub struct ProfilerStats {
    inner: mimalloc_pprof::prof::ProfStats,
}

impl ProfilerStats {
    /// Whether the profiler was sampling when the snapshot was taken.
    pub fn enabled(&self) -> bool {
        self.inner.enabled
    }

    /// Whether freed samples are retained as cumulative allocation history.
    pub fn accumulating(&self) -> bool {
        self.inner.accum
    }

    /// The byte sampling interval in effect (0 when never started).
    pub fn sample_rate(&self) -> usize {
        self.inner.sample_rate
    }

    /// Currently live sampled allocations.
    pub fn live_samples(&self) -> usize {
        self.inner.live_samples
    }

    /// Bytes attributed to currently live sampled allocations.
    pub fn live_bytes(&self) -> usize {
        self.inner.live_bytes
    }

    /// Cumulative sampled allocations, live or freed.
    pub fn accumulated_samples(&self) -> usize {
        self.inner.accum_samples
    }

    /// Bytes attributed to cumulative sampled allocations.
    pub fn accumulated_bytes(&self) -> usize {
        self.inner.accum_bytes
    }

    /// Distinct call stacks interned by the profiler.
    pub fn unique_stacks(&self) -> usize {
        self.inner.unique_stacks
    }

    /// Bytes the profiler itself has committed for its bookkeeping.
    pub fn profiler_committed_bytes(&self) -> usize {
        self.inner.arena_committed
    }

    /// Samples dropped because the stack table was full.
    pub fn stack_table_overflows(&self) -> usize {
        self.inner.stack_table_overflows
    }

    /// Every dropped sample, for any reason; never less than
    /// [`Self::stack_table_overflows`].
    pub fn dropped_samples(&self) -> usize {
        self.inner.dropped_samples
    }

    /// Exact allocator heap counters taken with this snapshot.
    pub fn heap(&self) -> HeapStats {
        HeapStats {
            inner: self.inner.heap.clone(),
        }
    }
}

/// Exact (not sampled) allocator heap counters from a [`ProfilerStats`].
#[derive(Clone, Debug, Default)]
pub struct HeapStats {
    inner: mimalloc_pprof::prof::HeapStats,
}

impl HeapStats {
    /// Bytes currently committed from the OS.
    pub fn committed(&self) -> usize {
        self.inner.committed
    }

    /// Bytes currently reserved from the OS; never less than
    /// [`Self::committed`].
    pub fn reserved(&self) -> usize {
        self.inner.reserved
    }

    /// Bytes the application requested and still holds.
    ///
    /// Maintained only when [`Self::detailed`] is true; otherwise 0.
    pub fn malloc_requested(&self) -> usize {
        self.inner.malloc_requested
    }

    /// Live allocator pages.
    pub fn pages(&self) -> usize {
        self.inner.pages
    }

    /// Pages abandoned by exited threads.
    pub fn pages_abandoned(&self) -> usize {
        self.inner.pages_abandoned
    }

    /// Live first-class heaps.
    pub fn heaps(&self) -> usize {
        self.inner.heaps
    }

    /// Live thread-local heaps, excluding the main thread's static heap.
    pub fn thread_heaps(&self) -> usize {
        self.inner.theaps
    }

    /// Cumulative bytes purged back to the OS.
    pub fn purged(&self) -> usize {
        self.inner.purged
    }

    /// Whether detailed statistics are compiled in, which is what makes
    /// [`Self::malloc_requested`] meaningful. Without it, "allocated
    /// nothing" and "not tracked" are indistinguishable.
    pub fn detailed(&self) -> bool {
        self.inner.detailed
    }
}

/// Read the profiler and allocator counters.
///
/// Returns all-zero counters if the linked allocator rejects the stats
/// request; it never fails.
pub fn stats() -> ProfilerStats {
    ProfilerStats {
        inner: mimalloc_pprof::prof::stats(),
    }
}

/// A collision-resistant default filename for one heap snapshot.
pub fn next_dump_name() -> String {
    let sequence = DUMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    format!("heap-{}-{millis}-{sequence}.pb", std::process::id())
}

/// Write the retained heap samples to an explicit pprof protobuf path.
pub fn dump_to(path: impl AsRef<Path>) -> std::io::Result<()> {
    mimalloc_pprof::prof::dump_proto_file(path.as_ref())
}

/// Write the retained heap samples to `path` in the legacy text heap-profile
/// format (`heap profile:` header plus `MAPPED_LIBRARIES:`), readable by
/// `pprof`/`jeprof` with the matching binary.
///
/// Prefer [`dump_to`] (binary `profile.proto`) for new consumers; this form
/// serves tooling that already reads text heap profiles.
///
/// # Errors
///
/// Returns `InvalidInput` for a non-UTF-8 path or one containing NUL, and
/// the OS error when the file cannot be written.
pub fn dump_file(path: impl AsRef<Path>) -> std::io::Result<()> {
    mimalloc_pprof::prof::dump_file(path.as_ref())
}

/// Serialize the retained heap samples to an in-memory pprof protobuf buffer.
///
/// Use this when a caller needs the snapshot bytes directly — for example to
/// hand them to an API response or an in-process test assertion — rather
/// than writing them to disk first. Returns an empty buffer if the profiler
/// failed to serialize the snapshot; it never returns an error.
pub fn dump_to_vec() -> Vec<u8> {
    mimalloc_pprof::prof::dump_proto_to_vec()
}

/// Create `directory` asynchronously and write a uniquely named pprof dump.
pub async fn dump_in(directory: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    let directory = directory.as_ref();
    tokio::fs::create_dir_all(directory).await?;
    let path = directory.join(next_dump_name());
    let dump_path = path.clone();
    tokio::task::spawn_blocking(move || dump_to(&dump_path))
        .await
        .map_err(std::io::Error::other)??;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_explicit_and_zero_never_means_sample_everything() {
        for off in ["", "0", "00", "false", "off", "no"] {
            assert_eq!(sample_rate_from(off), None, "{off:?}");
        }
        for on in ["1", "true", "on", "yes"] {
            assert_eq!(sample_rate_from(on), Some(DEFAULT_SAMPLE_RATE), "{on:?}");
        }
        assert_eq!(sample_rate_from("65536"), Some(65_536));
    }

    #[test]
    fn dump_names_do_not_collide() {
        let first = next_dump_name();
        let second = next_dump_name();
        assert_ne!(first, second);
        assert!(first.ends_with(".pb"));
    }

    // `dump_to_vec` needs samples captured through mimalloc's allocator, which
    // means a `#[global_allocator]` declaration. This lib unit-test binary has
    // none (declaring one here would switch every other unit test in the
    // crate onto mimalloc too), so that behavior is exercised instead by the
    // dedicated `tests/allocator_heap_profile.rs` integration test, which
    // mirrors the real zccache consumer: a separate final executable that
    // installs `Allocator` as its global allocator before profiling.
}
