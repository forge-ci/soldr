//! What else is running on this host, and how busy it is.
//!
//! This is observation, not control: enumerating processes to find one by
//! name, and sampling aggregate CPU load. Acting on a process -- signalling,
//! waiting, killing a tree -- is `platform::process`, which works from an
//! identity that cannot be reused out from under the caller. Nothing here
//! returns something you can act on for that reason: a pid observed here is
//! already potentially stale, and the caller must re-capture an identity
//! before doing anything with it.

use std::time::Duration;

/// One process this host is running, as observed at enumeration time.
///
/// This is a snapshot. The process may already have exited by the time the
/// caller reads it, which is inherent to enumerating rather than a gap in
/// this type -- see the module note about re-capturing an identity before
/// acting.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessSummary {
    pid: u32,
    parent_pid: Option<u32>,
    name: String,
    running: bool,
    cpu_usage_percent: f32,
    memory_bytes: u64,
}

impl ProcessSummary {
    /// The process identifier, as this host reports it.
    ///
    /// Only useful for display or for re-capturing an identity through
    /// `platform::process`; a pid on its own is reusable and so is not
    /// something to act on directly.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The parent's process identifier, when this host reports one.
    ///
    /// `None` means the host did not name a parent, which is how an orphan
    /// presents once its parent has been reaped. Note the other orphan shape:
    /// a parent pid that is present but no longer in this enumeration. A
    /// caller reaping orphans has to treat both as orphaned, and only one of
    /// them looks like it.
    pub fn parent_pid(&self) -> Option<u32> {
        self.parent_pid
    }

    /// The executable name, lossily decoded.
    ///
    /// Lossy because a process name is host bytes rather than text, and a
    /// caller matching on it -- which is the reason to enumerate at all --
    /// needs a string it can compare rather than an error it cannot act on.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the host still considers this process runnable, as opposed to
    /// a zombie or stopped entry that enumeration still reports.
    pub fn running(&self) -> bool {
        self.running
    }

    /// Recent CPU usage as a percentage of one core, so a process using two
    /// cores fully reports around 200.
    pub fn cpu_usage_percent(&self) -> f32 {
        self.cpu_usage_percent
    }

    /// Resident memory in bytes.
    pub fn memory_bytes(&self) -> u64 {
        self.memory_bytes
    }
}

/// Every process this host will tell us about, as of now.
///
/// The result is unordered and unfiltered: callers looking for one program
/// match on [`ProcessSummary::name`] themselves, because what counts as a
/// match is theirs -- an exact name, a name with a `.exe` suffix, a substring.
///
/// Named `snapshot` rather than `running_processes` on purpose. The obvious
/// name contains the substring `running_process`, which is the backend crate
/// this facade exists to keep out of its public surface, and
/// `tests/facade_policy.rs` scans public items for exactly that string. It
/// cannot tell a coincidence from a leak, and it should not have to: the
/// cheap fix is here, not in loosening a check that guards the boundary.
/// `snapshot` also says the thing that matters -- this is a moment, already
/// potentially stale when read.
pub fn snapshot() -> Vec<ProcessSummary> {
    let system = sysinfo::System::new_all();
    system
        .processes()
        .iter()
        .map(|(pid, process)| ProcessSummary {
            pid: pid.as_u32(),
            parent_pid: process.parent().map(sysinfo::Pid::as_u32),
            name: process.name().to_string_lossy().into_owned(),
            // Only a zombie, stopped or dead entry is not running. A live
            // process can be sampled in any other state -- disk sleep under
            // I/O load, waking, parked -- and is still running.
            running: !matches!(
                process.status(),
                sysinfo::ProcessStatus::Zombie
                    | sysinfo::ProcessStatus::Stop
                    | sysinfo::ProcessStatus::Tracing
                    | sysinfo::ProcessStatus::Dead
            ),
            cpu_usage_percent: process.cpu_usage(),
            memory_bytes: process.memory(),
        })
        .collect()
}

/// Samples this host's aggregate CPU load over time.
///
/// CPU usage is a rate, so it cannot be read once: every sample is the work
/// done since the previous one. That is why this is a type that is kept and
/// asked repeatedly rather than a function that answers immediately, and why
/// [`CpuSampler::minimum_interval`] exists -- sampling faster than the host
/// updates its own counters reports noise, not load.
#[derive(Debug)]
pub struct CpuSampler {
    system: sysinfo::System,
}

impl CpuSampler {
    /// Start sampling.
    ///
    /// The first [`sample`](CpuSampler::sample) after this establishes the
    /// baseline; treat its result as unreliable and wait at least
    /// [`minimum_interval`](CpuSampler::minimum_interval) before the one you
    /// intend to use.
    pub fn new() -> Self {
        let mut system = sysinfo::System::new();
        system.refresh_cpu_usage();
        Self { system }
    }

    /// The shortest gap between samples this host can distinguish.
    ///
    /// Sampling more often than this returns whatever the previous sample
    /// returned, or noise, depending on the platform.
    pub fn minimum_interval() -> Duration {
        sysinfo::MINIMUM_CPU_UPDATE_INTERVAL
    }

    /// Refresh and report mean usage across every CPU, as a percentage.
    ///
    /// Averaged across cores rather than summed, so the result is `0..=100`
    /// on every host regardless of core count, and clamped because hosts do
    /// occasionally report slightly outside that range.
    pub fn sample(&mut self) -> f32 {
        self.system.refresh_cpu_usage();
        let cpus = self.system.cpus();
        if cpus.is_empty() {
            return 0.0;
        }
        let mean = cpus.iter().map(|cpu| cpu.cpu_usage()).sum::<f32>() / cpus.len() as f32;
        mean.clamp(0.0, 100.0)
    }
}

impl Default for CpuSampler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This process is running, so enumeration must find it, and the entry
    /// must carry a usable name rather than an empty string.
    #[test]
    fn enumeration_finds_this_process() {
        let processes = snapshot();
        assert!(!processes.is_empty(), "a host always runs something");

        let me = std::process::id();
        let mine = processes
            .iter()
            .find(|process| process.pid() == me)
            .expect("enumeration must include the calling process");
        assert!(
            !mine.name().is_empty(),
            "a process the host reports must have a name to match on"
        );
        assert!(
            mine.running(),
            "the calling process is running by definition"
        );
    }

    /// A sample is a proportion, so it stays in range whatever the host is
    /// doing -- including the first one, which has no previous sample to
    /// measure against.
    #[test]
    fn a_sample_is_a_percentage_of_one_core() {
        let mut sampler = CpuSampler::new();
        let first = sampler.sample();
        assert!(
            (0.0..=100.0).contains(&first),
            "even the baseline sample must be in range, got {first}"
        );

        std::thread::sleep(CpuSampler::minimum_interval());
        let second = sampler.sample();
        assert!(
            (0.0..=100.0).contains(&second),
            "a sample must be a percentage, got {second}"
        );
    }

    /// The minimum interval is a real wait, not zero: a caller that trusted a
    /// zero here would spin.
    #[test]
    fn the_minimum_interval_is_nonzero() {
        assert!(CpuSampler::minimum_interval() > Duration::ZERO);
    }
}
