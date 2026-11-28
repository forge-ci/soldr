//! CPU profiling: bounded sampling, off-hot-path symbolization, exports
//! (S15 / #644).
//!
//! # The shape of the pipeline, and why it is that shape
//!
//! ```text
//!   sampler ──push──▶ [bounded ring] ──drain──▶ symbolizer ──▶ exporters
//!  (hot path)          drop + count            (worker thread)
//! ```
//!
//! The split at the ring is the whole design. Sampling suspends threads in the
//! target, so every microsecond spent on the sampling side is a microsecond
//! the profiled program is not running — and symbolization is the expensive
//! part: it parses PDB/DWARF/Mach-O and touches the filesystem. Doing it
//! between samples would make the profiler's own cost dominate the profile,
//! and the flame graph would be a picture of the profiler.
//!
//! So the sampler only ever pushes raw instruction pointers. Names are
//! resolved afterwards, off the hot path, and can even be resolved after the
//! target has exited — the addresses plus the module list are enough.
//!
//! # Backpressure is a dropped sample, never a blocked sampler
//!
//! The ring is fixed-size. When it fills, the sample is discarded and a
//! counter increments. Blocking would push back on the profiled process, and
//! growing would let a slow consumer turn a profile into an OOM. A dropped
//! sample is a small, *measured* loss of fidelity, reported in
//! [`ProfileMetrics::samples_dropped`] so the operator knows the profile is
//! thinned rather than discovering it in a misleading graph.
//!
//! A session's own sampling loop is the one producer that cannot be left to
//! drop indefinitely: nothing drains the ring until the session ends, so a
//! full ring stays full and every further tick would suspend the target's
//! threads for samples discarded on arrival. A session therefore ends when its
//! ring fills and reports [`ProfileMetrics::buffer_full`], so the window the
//! metrics describe is the window that was sampled.

use std::time::Duration;

pub mod async_profile;
#[cfg(feature = "tokio-console")]
pub(crate) mod async_tokio;
pub mod export;
pub mod ingest;
pub mod session;
pub mod symbolize;

#[cfg(test)]
mod tests;

/// Checked-in pprof message types.
///
/// Keeping these types in source avoids making every client compile a
/// protobuf generator merely to emit an established wire format.
pub mod pprof;

pub use ingest::{RawSample, SampleRing};
pub use session::{ProfileMetrics, ProfileRequest, ProfileSession, ResolvedSample, SessionResult};
pub use symbolize::{FrameResolver, ModuleResolver};

/// Hard ceiling on a profiling session.
///
/// Not a default a caller can raise — a cap. A profiler that suspends threads
/// is a cost the target pays continuously, and an unbounded session is one an
/// operator can start, forget, and leave degrading a production process
/// indefinitely. Sixty seconds is long enough to catch a steady-state hot
/// path; anything longer is a series of sessions, each of which someone chose.
pub const MAX_DURATION: Duration = Duration::from_secs(60);

/// Default sampling frequency, in hertz.
///
/// 99 rather than 100 on purpose: a profiler running at exactly 100 Hz
/// phase-locks with anything else on a 100 Hz timer (schedulers, animation
/// loops, poll intervals), so it repeatedly samples the same point in that
/// cycle and reports a periodic artifact as a hot path. A prime-ish offset
/// makes the sampler drift across the period instead.
pub const DEFAULT_HZ: u32 = 99;

/// Lowest accepted frequency.
pub const MIN_HZ: u32 = 1;

/// Highest accepted frequency.
///
/// Above roughly a kilohertz the suspend/resume cost per sample stops being
/// negligible against the interval itself, and the profile measures the
/// profiler.
pub const MAX_HZ: u32 = 1000;

/// Capacity of the raw-sample ring.
///
/// A session spends this budget at `hz × seconds × threads`, not per tick: one
/// sample is pushed for every thread that is running when the tick fires. That
/// covers a whole sixty-second session at the default frequency up to eleven
/// sibling threads, and a ten-second one up to sixty-six; beyond that the ring
/// fills first and the session ends there rather than sampling into a full
/// ring. A caller who knows the target's thread count can size the ring for
/// its own window with [`ProfileSession::with_ring_capacity`], trading memory
/// for coverage explicitly rather than having the window silently cut.
pub const RING_CAPACITY: usize = 1 << 16;
