//! Session orchestration: bounds, sampling, metrics (S15 / #644).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::ingest::{RawSample, SampleRing};
use super::symbolize::{Frame, FrameResolver};
use super::{DEFAULT_HZ, MAX_DURATION, MAX_HZ, MIN_HZ};

/// What a caller asked for, before clamping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileRequest {
    /// Requested sampling frequency, in hertz.
    pub hz: u32,
    /// Requested duration.
    pub duration: Duration,
}

impl Default for ProfileRequest {
    fn default() -> Self {
        Self {
            hz: DEFAULT_HZ,
            duration: Duration::from_secs(10),
        }
    }
}

impl ProfileRequest {
    /// Bring the request inside the enforced bounds.
    ///
    /// Clamps rather than refuses. An operator who typed `--duration 300`
    /// wants a profile, and giving them sixty seconds of one is a better
    /// answer than an error — but the ceiling is not negotiable, so the
    /// clamped values are reported back in [`ProfileMetrics`] rather than
    /// quietly substituted.
    pub fn clamped(self) -> Self {
        Self {
            hz: self.hz.clamp(MIN_HZ, MAX_HZ),
            duration: self.duration.min(MAX_DURATION),
        }
    }

    /// Nanoseconds between samples at the clamped frequency.
    pub fn period_nanos(self) -> u64 {
        let hz = u64::from(self.clamped().hz.max(MIN_HZ));
        1_000_000_000 / hz
    }

    /// Whether clamping changed anything.
    pub fn was_clamped(self) -> bool {
        self.clamped() != self
    }
}

/// What a session cost and covered.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProfileMetrics {
    /// Samples that reached the ring.
    pub samples_captured: u64,
    /// Samples discarded because the ring was full.
    pub samples_dropped: u64,
    /// Distinct OS threads observed at least once.
    pub threads_seen: u64,
    /// Threads the OS reported at session start.
    pub threads_at_start: u64,
    /// Total time the target spent suspended, in nanoseconds.
    ///
    /// The honest cost figure: this is time the profiled program did not run
    /// because it was being measured.
    pub pause_nanos: u64,
    /// Wall time the session actually ran, in nanoseconds.
    pub duration_nanos: u64,
    /// Effective frequency after clamping.
    pub hz: u32,
    /// Whether the request was reduced to fit the enforced bounds.
    pub clamped: bool,
    /// Whether sampling stopped early because the ring filled.
    ///
    /// Not the same claim as [`Self::fidelity`], which is the share of
    /// *offered* samples that were kept: this says the session stopped
    /// offering. When it is true, `duration_nanos` covers only the part of the
    /// requested window that was actually sampled, so the profile describes
    /// the start of the run and nothing after it. Size the ring for the
    /// target's thread count with [`ProfileSession::with_ring_capacity`] to
    /// cover a whole window.
    pub buffer_full: bool,
}

impl ProfileMetrics {
    /// Fraction of live threads the profile saw, 0.0 to 1.0.
    ///
    /// A low figure means the profile describes part of the program. Reported
    /// rather than hidden, because a flame graph covering two of eight threads
    /// looks exactly like one covering all of a two-threaded program.
    pub fn thread_coverage(&self) -> f64 {
        if self.threads_at_start == 0 {
            return 0.0;
        }
        (self.threads_seen as f64 / self.threads_at_start as f64).min(1.0)
    }

    /// Share of the session the target spent suspended, 0.0 to 1.0.
    pub fn overhead_ratio(&self) -> f64 {
        if self.duration_nanos == 0 {
            return 0.0;
        }
        self.pause_nanos as f64 / self.duration_nanos as f64
    }

    /// Share of offered samples that were kept, 0.0 to 1.0.
    pub fn fidelity(&self) -> f64 {
        let offered = self.samples_captured + self.samples_dropped;
        if offered == 0 {
            return 1.0;
        }
        self.samples_captured as f64 / offered as f64
    }
}

/// One sample after name resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSample {
    /// OS thread the stack belongs to.
    pub os_tid: u64,
    /// Frames, leaf first.
    pub frames: Vec<Frame>,
    /// Whether the captured stack was cut short.
    pub truncated: bool,
}

/// A finished session: samples, metrics, and when it ran.
#[derive(Clone, Debug, Default)]
pub struct SessionResult {
    /// Resolved samples.
    pub samples: Vec<ResolvedSample>,
    /// Cost and coverage.
    pub metrics: ProfileMetrics,
    /// Session start, nanoseconds since the Unix epoch.
    pub start_unix_nanos: i64,
    /// Nanoseconds each sample is taken to represent.
    pub period_nanos: u64,
}

impl SessionResult {
    /// Fold samples into unique stacks with counts, leaf-last.
    ///
    /// The shared representation behind every export: collapsed stacks are it
    /// verbatim, and both pprof and the Firefox format are built from the same
    /// folding. One folding means the three exports cannot disagree about what
    /// was hot.
    pub fn folded(&self) -> Vec<(Vec<String>, u64)> {
        use std::collections::BTreeMap;
        let mut counts: BTreeMap<Vec<String>, u64> = BTreeMap::new();
        for sample in &self.samples {
            // Root-first is the direction every flame graph draws, and the
            // direction the collapsed format is defined in.
            let stack: Vec<String> = sample
                .frames
                .iter()
                .rev()
                .map(|frame| frame.function.clone())
                .collect();
            if stack.is_empty() {
                continue;
            }
            *counts.entry(stack).or_insert(0) += 1;
        }
        let mut folded: Vec<(Vec<String>, u64)> = counts.into_iter().collect();
        // Hottest first, then lexicographic, so output is deterministic and a
        // reader's eye lands on what matters.
        folded.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        folded
    }
}

/// Runs one bounded profiling session against the current process.
///
/// Self-profiling rather than cross-process: the cooperative capture path
/// suspends *sibling* threads, which is what makes it safe. A profiler that
/// reached into another process would need the debug privileges the whole
/// probe design avoids requiring.
#[derive(Debug)]
pub struct ProfileSession {
    // What the caller asked for, kept alongside the clamped bounds the session
    // runs under. Storing only the clamped request would destroy the evidence
    // that anything was substituted, and `ProfileMetrics::clamped` exists
    // precisely to report that substitution.
    requested: ProfileRequest,
    request: ProfileRequest,
    ring: Arc<SampleRing>,
    stop: Arc<AtomicBool>,
}

impl ProfileSession {
    /// Prepare a session for `request`, clamped to the enforced bounds.
    pub fn new(request: ProfileRequest) -> Self {
        Self {
            requested: request,
            request: request.clamped(),
            ring: Arc::new(SampleRing::default()),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Prepare a session whose ring holds at most `capacity` samples.
    ///
    /// One tick pushes a sample per running thread, so the budget a session
    /// spends is `hz × seconds × threads` rather than the per-tick figure the
    /// default capacity is stated in. A caller who knows roughly how many
    /// threads the target runs can size the ring for its whole window here;
    /// otherwise the session ends when the ring fills and reports
    /// [`ProfileMetrics::buffer_full`].
    pub fn with_ring_capacity(request: ProfileRequest, capacity: usize) -> Self {
        Self {
            requested: request,
            request: request.clamped(),
            ring: Arc::new(SampleRing::with_capacity(capacity)),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The bounds this session will actually run under.
    pub fn request(&self) -> ProfileRequest {
        self.request
    }

    /// The sink samples are pushed into.
    pub fn ring(&self) -> &Arc<SampleRing> {
        &self.ring
    }

    /// Ask a running session to stop early.
    pub fn stop_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Sample this process until the duration elapses or the stop flag is set.
    ///
    /// Returns raw metrics; call [`Self::resolve`] to attach names.
    pub fn run(&self) -> ProfileMetrics {
        use crate::snapshot::{SessionResolver, SnapshotConfig};

        let started = Instant::now();
        let period = Duration::from_nanos(self.request.period_nanos());
        let config = SnapshotConfig::default();
        // One resolver for the session. Resolving per tick rebuilt the module
        // inventory and unwinder on every capture, which capped the effective
        // rate near a sample per second no matter what `hz` asked for (#131).
        // It builds its inventory on the first tick rather than here, so a
        // session that stops before sampling -- a ring that is already full --
        // pays nothing for one it would never resolve against, and it still
        // rebuilds when the mapped images change, so a module loaded
        // mid-session is attributed rather than missed.
        let mut resolver = SessionResolver::new(&config);

        let mut buffer_full = false;
        let mut threads_at_start = 0u64;
        let mut pause_nanos = 0u64;
        let mut seen: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();

        let mut next = started;
        while started.elapsed() < self.request.duration && !self.stop.load(Ordering::Relaxed) {
            // A tick pushes one sample per running thread, so the ring's real
            // budget is hz × seconds × threads and a many-threaded target can
            // exhaust it mid-window. Nothing drains it until the session ends,
            // so a full ring stays full: every further tick would suspend
            // every sibling thread to produce samples discarded on arrival,
            // and the profile would describe the beginning of the window while
            // `duration_nanos` claimed all of it. Stop here instead. The
            // profile then covers exactly the window the metrics report, the
            // target stops paying for samples nobody keeps, and
            // `buffer_full` tells the caller the window was cut short.
            if self.ring.is_full() {
                buffer_full = true;
                break;
            }

            // Sample on a fixed schedule rather than sleeping a fixed period
            // after each capture. Adding the period to "now" would let the
            // capture's own cost push the interval out, so the effective rate
            // would silently be lower than the one reported in the metrics.
            next += period;

            if let Ok(snapshot) = resolver.capture() {
                pause_nanos = pause_nanos.saturating_add(snapshot.stats.pause_nanos);
                threads_at_start = threads_at_start.max(u64::from(snapshot.stats.threads_total));

                let since_start_nanos = started.elapsed().as_nanos() as u64;
                for thread in &snapshot.threads {
                    if thread.frames.is_empty() {
                        continue;
                    }
                    seen.insert(thread.os_tid);
                    self.ring.push(RawSample {
                        os_tid: thread.os_tid,
                        since_start_nanos,
                        stack: thread.frames.clone(),
                        truncated: thread.truncated,
                    });
                }
            }

            // A capture can fill the ring on the final tick. Record that
            // terminal condition before the duration check ends the loop.
            if self.ring.is_full() {
                buffer_full = true;
                break;
            }

            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                // Behind schedule: skip ahead rather than trying to catch up
                // with a burst, which would sample the same instant repeatedly
                // and overweight whatever was running then.
                next = now;
            }
        }

        ProfileMetrics {
            samples_captured: self.ring.accepted(),
            samples_dropped: self.ring.dropped(),
            threads_seen: seen.len() as u64,
            threads_at_start,
            pause_nanos,
            duration_nanos: started.elapsed().as_nanos() as u64,
            hz: self.request.hz,
            clamped: self.requested.was_clamped(),
            buffer_full,
        }
    }

    /// Drain the ring and attach names.
    pub fn resolve<R: FrameResolver>(
        &self,
        resolver: &mut R,
        metrics: ProfileMetrics,
    ) -> SessionResult {
        let raw = self.ring.drain();
        let samples = raw
            .into_iter()
            .map(|sample| ResolvedSample {
                os_tid: sample.os_tid,
                frames: sample
                    .stack
                    .iter()
                    .map(|address| resolver.resolve(*address))
                    .collect(),
                truncated: sample.truncated,
            })
            .collect();

        SessionResult {
            samples,
            metrics,
            start_unix_nanos: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_nanos() as i64)
                .unwrap_or(0),
            period_nanos: self.request.period_nanos(),
        }
    }
}
