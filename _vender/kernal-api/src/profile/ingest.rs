//! The bounded raw-sample ring (S15 / #644).
//!
//! Sits between the sampler and the symbolizer. Fixed capacity, drop-and-count
//! on overflow — see the module docs on [`super`] for why that is the only
//! acceptable backpressure policy here.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use super::RING_CAPACITY;

/// One captured stack, before any name resolution.
///
/// Instruction pointers only. Deliberately nothing that requires a lookup:
/// everything here is read straight out of registers and stack memory while
/// the target's threads are suspended, and anything more would extend that
/// suspension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawSample {
    /// OS thread the stack belongs to.
    pub os_tid: u64,
    /// When it was taken, nanoseconds since the session started.
    pub since_start_nanos: u64,
    /// Return addresses, leaf first.
    pub stack: Vec<u64>,
    /// Whether the captured stack was cut short by the copy limit.
    ///
    /// Carried through to the export so a truncated stack is not silently
    /// folded together with a complete one that happens to share its prefix.
    pub truncated: bool,
}

/// A fixed-capacity sink for raw samples.
#[derive(Debug)]
pub struct SampleRing {
    samples: Mutex<Vec<RawSample>>,
    capacity: usize,
    dropped: AtomicU64,
    accepted: AtomicU64,
}

impl Default for SampleRing {
    fn default() -> Self {
        Self::with_capacity(RING_CAPACITY)
    }
}

impl SampleRing {
    /// A ring holding at most `capacity` samples.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            samples: Mutex::new(Vec::with_capacity(capacity.min(1024))),
            capacity,
            dropped: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
        }
    }

    /// Offer a sample. Returns whether it was kept.
    ///
    /// Never blocks on a full ring and never grows it. The caller is the
    /// sampling path, and making it wait would push the profiler's cost back
    /// onto the profiled program — which is the one thing a profiler must not
    /// do to the thing it is measuring.
    pub fn push(&self, sample: RawSample) -> bool {
        let mut samples = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        if samples.len() >= self.capacity {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        samples.push(sample);
        self.accepted.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Take everything buffered so far, leaving the ring empty.
    pub fn drain(&self) -> Vec<RawSample> {
        let mut samples = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *samples)
    }

    /// How many samples were discarded for want of room.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// How many samples were accepted over the ring's whole life.
    ///
    /// Distinct from the current length, which [`Self::drain`] resets: this is
    /// the denominator for a fidelity figure, and it has to survive draining.
    pub fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::Relaxed)
    }

    /// The most samples this ring will ever hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Whether the ring is holding all it can.
    ///
    /// The sampling loop's cue to stop. Nothing drains the ring while a
    /// session runs, so a full ring stays full: every further capture would
    /// suspend the target's threads to produce samples that are discarded on
    /// arrival.
    pub fn is_full(&self) -> bool {
        self.len() >= self.capacity
    }

    /// Samples currently buffered.
    pub fn len(&self) -> usize {
        self.samples.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether nothing is buffered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
