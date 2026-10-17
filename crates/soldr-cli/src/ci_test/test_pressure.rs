//! Run-time memory-pressure admission for Nextest execution (soldr#2885).
//!
//! Nextest fixes its concurrency for the whole run, so `ci-test` cannot lower
//! `--test-threads` once memory tightens. What it can do is stop *starting*
//! tests: every Unix test starts through the native Nextest wrapper
//! (`crates/soldr-nextest-wrapper`), which waits before launching its test
//! while `<admission dir>/paused` exists and another test is still running.
//!
//! This module owns that flag. A monitor thread samples available memory
//! (the same tighter-of-cgroup-and-host reading the plan used) and drives a
//! [`PressureController`] with hysteresis:
//!
//! * pause when available memory drops below one per-test budget -- there is
//!   no longer room to start another test;
//! * resume only once it is back to two budgets, so one test's worth of
//!   headroom exists after the resumed test starts;
//! * never transition twice within [`MIN_DWELL`], so a reading hovering at a
//!   mark cannot flap the gate. The wrapper additionally releases paused
//!   waiters one at a time.
//!
//! Tests already running are never touched by this controller; a per-test
//! ceiling (enforced by the wrapper) is what bounds a single runaway tree.
//! The flag, the per-test `active/<pid>` slots and the `infra/<test>` records
//! are presence-only files: no structured data crosses the process boundary.

use super::test_admission::{format_bytes, read_memory, MemoryReading, TestAdmission};
use crate::core::SoldrError;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub(crate) const ADMISSION_DIR_ENV: &str = "SOLDR_NEXTEST_ADMISSION_DIR";
pub(crate) const CEILING_ENV: &str = "SOLDR_NEXTEST_TEST_MEMORY_CEILING_BYTES";
pub(crate) const SUMMARY_ENV: &str = "SOLDR_NEXTEST_ADMISSION_SUMMARY";
/// Shared with `crates/soldr-nextest-wrapper/src/guard.rs`; renaming either
/// side breaks the gate.
pub(crate) const PAUSED_FLAG: &str = "paused";
pub(crate) const ACTIVE_DIR: &str = "active";
pub(crate) const INFRA_DIR: &str = "infra";

const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
/// Minimum time between two gate transitions.
pub(crate) const MIN_DWELL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Transition {
    Pause,
    Resume,
}

/// Hysteresis over available memory. Pure: time and readings are inputs.
#[derive(Debug)]
pub(crate) struct PressureController {
    pause_below: u64,
    resume_at: u64,
    min_dwell: Duration,
    paused: bool,
    last_transition: Option<Instant>,
}

impl PressureController {
    pub(crate) fn new(pause_below: u64, resume_at: u64, min_dwell: Duration) -> Self {
        Self {
            pause_below,
            // A band of zero width would flap; keep resume strictly above.
            resume_at: resume_at.max(pause_below.saturating_add(1)),
            min_dwell,
            paused: false,
            last_transition: None,
        }
    }

    pub(crate) fn paused(&self) -> bool {
        self.paused
    }

    /// Feed one reading; an unreadable one (`None`) never changes the gate.
    pub(crate) fn observe(&mut self, available: Option<u64>, now: Instant) -> Option<Transition> {
        let available = available?;
        if self
            .last_transition
            .is_some_and(|last| now.saturating_duration_since(last) < self.min_dwell)
        {
            return None;
        }
        let transition = if !self.paused && available < self.pause_below {
            Transition::Pause
        } else if self.paused && available >= self.resume_at {
            Transition::Resume
        } else {
            return None;
        };
        self.paused = transition == Transition::Pause;
        self.last_transition = Some(now);
        Some(transition)
    }
}

/// The admission directory and the stage environment that points at it.
#[derive(Debug)]
pub(crate) struct NextestAdmission {
    dir: PathBuf,
    admission: TestAdmission,
}

impl NextestAdmission {
    pub(crate) fn new(base: &Path, admission: TestAdmission) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        Self {
            dir: base.join(format!("nextest-admission-{}-{stamp}", std::process::id())),
            admission,
        }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Environment for the Nextest *execution* stage only.
    pub(crate) fn stage_env(&self) -> Vec<(&'static str, String)> {
        let mut env = vec![
            (ADMISSION_DIR_ENV, self.dir.display().to_string()),
            (SUMMARY_ENV, self.admission.summary()),
        ];
        if let Some(ceiling) = self.admission.per_test_ceiling_bytes {
            env.push((CEILING_ENV, ceiling.to_string()));
        }
        env
    }

    /// Create the directory and start watching live memory.
    pub(crate) fn start(&self) -> Result<PressureMonitor, SoldrError> {
        self.start_with(SAMPLE_INTERVAL, || read_memory(None))
    }

    pub(crate) fn start_with(
        &self,
        interval: Duration,
        mut probe: impl FnMut() -> MemoryReading + Send + 'static,
    ) -> Result<PressureMonitor, SoldrError> {
        std::fs::create_dir_all(self.dir.join(ACTIVE_DIR)).map_err(|error| {
            SoldrError::Other(format!(
                "soldr ci-test: cannot create the Nextest admission directory {}: {error}",
                self.dir.display()
            ))
        })?;
        eprintln!(
            "soldr ci-test: Nextest admission: {}; pause new tests below {} available, resume at {}; per-test ceiling {}",
            self.admission.summary(),
            format_bytes(Some(self.admission.pause_below_available_bytes)),
            format_bytes(Some(self.admission.resume_at_available_bytes)),
            self.admission
                .per_test_ceiling_bytes
                .map_or_else(|| "disabled".to_string(), |bytes| format_bytes(Some(bytes)))
        );
        let stop = Arc::new(AtomicBool::new(false));
        let dir = self.dir.clone();
        let mut controller = PressureController::new(
            self.admission.pause_below_available_bytes,
            self.admission.resume_at_available_bytes,
            MIN_DWELL,
        );
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("ci-test-memory-pressure".into())
            .spawn(move || {
                let mut stats = MonitorStats::default();
                while !thread_stop.load(Ordering::Relaxed) {
                    let reading = probe();
                    stats.record(reading.available_bytes);
                    if let Some(transition) =
                        controller.observe(reading.available_bytes, Instant::now())
                    {
                        apply_transition(&dir, transition, &reading, &mut stats);
                    }
                    std::thread::sleep(interval);
                }
                if controller.paused() {
                    stats.close_pause();
                }
                stats
            })
            .map_err(|error| {
                SoldrError::Other(format!(
                    "soldr ci-test: cannot start the memory-pressure monitor: {error}"
                ))
            })?;
        Ok(PressureMonitor {
            dir: self.dir.clone(),
            stop,
            handle: Some(handle),
        })
    }
}

#[derive(Debug, Default)]
pub(crate) struct MonitorStats {
    pub(crate) pauses: u32,
    pub(crate) paused_total: Duration,
    paused_since: Option<Instant>,
    pub(crate) lowest_available: Option<u64>,
}

impl MonitorStats {
    fn record(&mut self, available: Option<u64>) {
        if let Some(available) = available {
            self.lowest_available = Some(
                self.lowest_available
                    .map_or(available, |low| low.min(available)),
            );
        }
    }

    fn close_pause(&mut self) {
        if let Some(since) = self.paused_since.take() {
            self.paused_total += since.elapsed();
        }
    }
}

fn apply_transition(
    dir: &Path,
    transition: Transition,
    reading: &MemoryReading,
    stats: &mut MonitorStats,
) {
    let flag = dir.join(PAUSED_FLAG);
    let running = running_tests(dir);
    let available = format_bytes(reading.available_bytes);
    let source = reading.source.describe();
    match transition {
        Transition::Pause => {
            stats.pauses += 1;
            stats.paused_since = Some(Instant::now());
            if let Err(error) = std::fs::write(&flag, b"") {
                eprintln!(
                    "warning: soldr ci-test: could not pause Nextest admissions ({}): {error}",
                    flag.display()
                );
            }
            eprintln!(
                "soldr ci-test: memory pressure: {available} available ({source}); pausing new Nextest tests while {running} running test(s) drain"
            );
        }
        Transition::Resume => {
            let paused_for = stats.paused_since.map(|since| since.elapsed());
            stats.close_pause();
            let _ = std::fs::remove_file(&flag);
            eprintln!(
                "soldr ci-test: memory recovered: {available} available ({source}); resuming Nextest admissions after {} ms",
                paused_for.unwrap_or_default().as_millis()
            );
        }
    }
}

/// Live `active/<pid>` slots; a SIGKILLed wrapper's stale slot is ignored.
pub(crate) fn running_tests(dir: &Path) -> usize {
    std::fs::read_dir(dir.join(ACTIVE_DIR))
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
                .filter(|pid| crate::platform::process::inspect::is_alive(*pid))
                .count()
        })
        .unwrap_or(0)
}

/// Test identities the wrapper recorded as infrastructure failures.
pub(crate) fn infra_failures(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.join(INFRA_DIR))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| percent_decode(&entry.file_name().to_string_lossy()))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Inverse of the Nextest wrapper's `guard::infra_record_name`.
fn percent_decode(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let hex = |byte: u8| char::from(byte).to_digit(16);
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Stops the monitor, reports, and removes the admission directory.
#[derive(Debug)]
pub(crate) struct PressureMonitor {
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<MonitorStats>>,
}

impl PressureMonitor {
    pub(crate) fn finish(mut self) -> (MonitorStats, Vec<String>) {
        let stats = self.stop_thread();
        let failures = infra_failures(&self.dir);
        eprintln!(
            "soldr ci-test: Nextest memory admission: paused {} time(s) for {} ms; lowest available memory {}",
            stats.pauses,
            stats.paused_total.as_millis(),
            format_bytes(stats.lowest_available)
        );
        if !failures.is_empty() {
            eprintln!(
                "soldr ci-test: {} Nextest test(s) failed from memory or process exhaustion, not from assertions (each printed a `nextest memory:` diagnostic):",
                failures.len()
            );
            for failure in &failures {
                eprintln!("soldr ci-test:   {failure}");
            }
        }
        self.remove_dir();
        (stats, failures)
    }

    fn stop_thread(&mut self) -> MonitorStats {
        self.stop.store(true, Ordering::Relaxed);
        self.handle
            .take()
            .and_then(|handle| {
                crate::platform::wait::join_for(handle, crate::platform::wait::HELPER_JOIN).ok()
            })
            .unwrap_or_default()
    }

    fn remove_dir(&self) {
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "warning: soldr ci-test: could not remove {}: {error}",
                    self.dir.display()
                );
            }
        }
    }
}

impl Drop for PressureMonitor {
    fn drop(&mut self) {
        if self.handle.is_some() {
            self.stop_thread();
            self.remove_dir();
        }
    }
}

#[cfg(test)]
#[path = "test_pressure_tests.rs"]
mod tests;
