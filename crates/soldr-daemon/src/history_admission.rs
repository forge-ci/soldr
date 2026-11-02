//! Admission that spends each unit's measured memory history (soldr#3152 step 4b).
//!
//! The owner chose a per-unit history over a command-line-feature estimate.
//! Scored on #3250's CI run, every unit measuring >= 256 MiB was within 1.35x
//! of its remembered tree peak; the only large errors were first sightings of
//! short compiles (a single spawn-instant reading just above the 8 MiB floor).
//!
//! This first step is **additive**: history can make a unit exclusive, never
//! the reverse, so no unit loses the protection today's classifier gives it.
//! A unit becomes exclusive when its headroomed remembered peak exceeds half
//! of the memory available right now, i.e. two such units could not run side
//! by side. Replacing the slot semaphore with memory-sized permits and
//! retiring the name lists come after this has run in CI.

use soldr_platform::host::resources::HostResourceSnapshot;

use crate::daemon::unit_memory_history::UnitMemory;

/// Measurements a unit needs before its history drives admission. A first
/// sighting can be a spawn-instant reading (CI: `proc_macro2` remembered 9.2
/// MiB, measured 166.7 MiB).
pub(crate) const MIN_TRUSTED_SAMPLES: u64 = 2;

/// Headroom applied to the remembered tree peak, as a ratio `NUM / DEN`.
/// Covers the worst heavy-unit ratio seen in CI (1.35x).
const HEADROOM_NUM: u64 = 3;
const HEADROOM_DEN: u64 = 2;

/// Remembered peaks below this never probe host memory: a unit this small
/// cannot need half of any machine soldr builds on, and the probe reads cgroup
/// files and `/proc/meminfo`.
pub(crate) const PROBE_THRESHOLD_BYTES: u64 = 256 * 1024 * 1024;

fn trusted(unit: Option<UnitMemory>) -> Option<UnitMemory> {
    unit.filter(|unit| unit.samples >= MIN_TRUSTED_SAMPLES)
}

/// Whether a remembered unit is large enough to be worth a memory probe.
pub(crate) fn worth_probing(unit: Option<UnitMemory>) -> bool {
    trusted(unit).is_some_and(|unit| unit.tree_peak_rss_bytes >= PROBE_THRESHOLD_BYTES)
}

/// Memory available to compilers right now: the smaller of cgroup headroom
/// (reclaimable page cache excluded, soldr#4012) and `MemAvailable`, either
/// alone when only one can be read. `None` when neither can be read.
pub(crate) fn available_bytes(snapshot: &HostResourceSnapshot) -> Option<u64> {
    match (snapshot.cgroup_headroom(), snapshot.system_available_bytes) {
        (Some(cgroup), Some(host)) => Some(cgroup.min(host)),
        (cgroup, host) => cgroup.or(host),
    }
}

/// Additive rule: exclusive when the trusted, headroomed remembered peak is
/// more than half of `available`. Never true without trusted history or a
/// memory reading.
pub(crate) fn history_requires_exclusive(unit: Option<UnitMemory>, available: Option<u64>) -> bool {
    let (Some(unit), Some(available)) = (trusted(unit), available) else {
        return false;
    };
    let needed = unit.tree_peak_rss_bytes.saturating_mul(HEADROOM_NUM) / HEADROOM_DEN;
    needed > available / 2
}

#[cfg(test)]
#[path = "history_admission_tests.rs"]
mod tests;
