//! Bounded sample sink: drop and count, never block (#635).
//!
//! # The app must not pay for a slow consumer
//!
//! Samples flow from the probe thread to the daemon. If the daemon reads
//! slowly — or stops reading — the producer must keep running. Blocking would
//! push the consumer's latency back into the profiled application, which
//! inverts the entire point of a low-overhead probe: the observation would
//! change the thing being observed, and a wedged daemon would become a wedged
//! app.
//!
//! So the sink is bounded, and when it is full [`SampleSink::offer`] drops the
//! sample and increments a counter rather than waiting. Dropping is a normal,
//! *reported* outcome, not an error — a consumer that sees `dropped > 0` knows
//! its view is incomplete and by how much.
//!
//! # Why counted rather than silent
//!
//! A profile missing samples looks exactly like a profile of a less busy
//! program. Reporting the drop count is what lets a reader tell "the app was
//! idle" from "we couldn't keep up".

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Default queue depth.
///
/// Deep enough to absorb a brief consumer stall, shallow enough that a
/// persistently slow consumer is reported quickly rather than hidden behind a
/// large backlog — and that memory stays bounded regardless of producer rate.
pub const DEFAULT_CAPACITY: usize = 256;

/// What the sink has seen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SinkStats {
    /// Samples accepted into the queue.
    pub accepted: u64,
    /// Samples dropped because the queue was full.
    ///
    /// Non-zero means the consumer fell behind and the stream is incomplete.
    pub dropped: u64,
}

impl SinkStats {
    /// Total samples offered, accepted or not.
    pub fn offered(&self) -> u64 {
        self.accepted + self.dropped
    }

    /// Whether every offered sample was accepted.
    pub fn is_complete(&self) -> bool {
        self.dropped == 0
    }
}

/// A bounded, non-blocking sink shared between producer and consumer.
///
/// Cloning shares the same underlying queue.
#[derive(Clone, Debug)]
pub struct SampleSink<T> {
    inner: Arc<Inner<T>>,
}

#[derive(Debug)]
struct Inner<T> {
    queue: Mutex<VecDeque<T>>,
    capacity: usize,
    accepted: AtomicU64,
    dropped: AtomicU64,
}

impl<T> SampleSink<T> {
    /// Create a sink holding at most `capacity` samples.
    ///
    /// A zero capacity is treated as one: a sink that can never accept
    /// anything would report 100% drops and hide whether the consumer was ever
    /// working.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                queue: Mutex::new(VecDeque::with_capacity(capacity.max(1))),
                capacity: capacity.max(1),
                accepted: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
            }),
        }
    }

    /// Offer a sample.
    ///
    /// Returns `true` if it was queued, `false` if it was dropped. Never
    /// blocks waiting for the consumer — the whole contract of this type.
    pub fn offer(&self, sample: T) -> bool {
        let mut queue = match self.inner.queue.lock() {
            Ok(q) => q,
            // A poisoned lock means a consumer panicked. Count the sample as
            // dropped rather than propagating a panic into the probe thread,
            // which would take down the application being profiled.
            Err(poisoned) => poisoned.into_inner(),
        };

        if queue.len() >= self.inner.capacity {
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        queue.push_back(sample);
        self.inner.accepted.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Take everything queued so far.
    pub fn drain(&self) -> Vec<T> {
        let mut queue = match self.inner.queue.lock() {
            Ok(q) => q,
            Err(poisoned) => poisoned.into_inner(),
        };
        queue.drain(..).collect()
    }

    /// Samples currently queued.
    pub fn len(&self) -> usize {
        match self.inner.queue.lock() {
            Ok(q) => q.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    /// Whether nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Accepted and dropped counts.
    pub fn stats(&self) -> SinkStats {
        SinkStats {
            accepted: self.inner.accepted.load(Ordering::Relaxed),
            dropped: self.inner.dropped.load(Ordering::Relaxed),
        }
    }
}

impl<T> Default for SampleSink<T> {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    #[test]
    fn accepts_up_to_capacity_then_drops() {
        let sink = SampleSink::with_capacity(4);
        for i in 0..4 {
            assert!(sink.offer(i), "sample {i} should fit");
        }
        assert!(!sink.offer(99), "the 5th sample must be dropped");

        let stats = sink.stats();
        assert_eq!(stats.accepted, 4);
        assert_eq!(stats.dropped, 1);
        assert_eq!(stats.offered(), 5);
        assert!(!stats.is_complete());
    }

    #[test]
    fn draining_makes_room_again() {
        let sink = SampleSink::with_capacity(2);
        sink.offer(1);
        sink.offer(2);
        assert!(!sink.offer(3));

        assert_eq!(sink.drain(), vec![1, 2]);
        assert!(
            sink.offer(4),
            "a drained sink must accept again — drops are transient, not terminal"
        );
    }

    #[test]
    fn drain_preserves_order() {
        let sink = SampleSink::with_capacity(8);
        for i in 0..5 {
            sink.offer(i);
        }
        assert_eq!(sink.drain(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn zero_capacity_is_treated_as_one() {
        let sink = SampleSink::with_capacity(0);
        assert!(
            sink.offer(1),
            "a sink that can never accept would report 100% drops and hide \
             whether the consumer ever worked"
        );
    }

    /// The contract: a stalled consumer must not slow the producer.
    #[test]
    fn producer_never_blocks_on_a_stalled_consumer() {
        let sink: SampleSink<u64> = SampleSink::with_capacity(8);

        // Consumer never drains. Producer must still finish promptly.
        let start = Instant::now();
        for i in 0..10_000 {
            sink.offer(i);
        }
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(2),
            "producing 10k samples into a full sink took {elapsed:?}; offer() must not wait"
        );

        let stats = sink.stats();
        assert_eq!(stats.accepted, 8, "only capacity should be retained");
        assert_eq!(stats.dropped, 9_992);
        assert_eq!(stats.offered(), 10_000, "every offer must be accounted for");
    }

    /// Throttled reader: drops are counted, nothing is lost silently.
    #[test]
    fn slow_consumer_causes_counted_drops_not_blocking() {
        let sink: SampleSink<u64> = SampleSink::with_capacity(16);
        let stop = Arc::new(AtomicBool::new(false));

        let consumer = {
            let sink = sink.clone();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut seen = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    seen += sink.drain().len() as u64;
                    // Deliberately slower than the producer.
                    std::thread::sleep(Duration::from_millis(5));
                }
                seen + sink.drain().len() as u64
            })
        };

        for i in 0..5_000 {
            sink.offer(i);
        }
        stop.store(true, Ordering::Relaxed);
        let consumed = consumer.join().unwrap();

        let stats = sink.stats();
        assert_eq!(
            stats.offered(),
            5_000,
            "accepted + dropped must equal what was offered"
        );
        assert!(
            stats.dropped > 0,
            "a consumer sleeping 5ms per batch cannot keep up with 5k offers"
        );
        assert!(
            consumed <= stats.accepted,
            "consumed {consumed} exceeds accepted {}",
            stats.accepted
        );
    }

    /// Cloned handles share one queue and one set of counters.
    #[test]
    fn clones_share_the_same_queue_and_counters() {
        let a = SampleSink::with_capacity(4);
        let b = a.clone();
        a.offer(1);
        b.offer(2);
        assert_eq!(b.len(), 2);
        assert_eq!(a.stats().accepted, 2);
        assert_eq!(a.drain(), vec![1, 2]);
        assert!(b.is_empty());
    }

    /// A panicking consumer must not take the producer with it.
    #[test]
    fn a_poisoned_lock_does_not_panic_the_producer() {
        let sink: SampleSink<u64> = SampleSink::with_capacity(4);
        let poisoner = {
            let sink = sink.clone();
            std::thread::spawn(move || {
                let _guard = sink.inner.queue.lock().unwrap();
                panic!("consumer died holding the lock");
            })
        };
        assert!(poisoner.join().is_err(), "the helper thread should panic");

        // The probe thread must keep working; taking down the profiled
        // application because a consumer panicked would be unacceptable.
        assert!(sink.offer(1));
        assert_eq!(sink.stats().accepted, 1);
    }
}
