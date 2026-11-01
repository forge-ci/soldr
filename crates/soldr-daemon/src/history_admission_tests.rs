//! RED -> GREEN coverage for the additive history admission rule.

use super::*;
use crate::daemon::unit_memory_history::UnitMemory;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

fn unit(tree_peak_bytes: u64, samples: u64) -> UnitMemory {
    UnitMemory {
        peak_rss_bytes: tree_peak_bytes,
        tree_peak_rss_bytes: tree_peak_bytes,
        updated_ms: 1,
        samples,
    }
}

#[test]
fn a_unit_that_cannot_run_beside_another_is_exclusive() {
    // 1.5 x 3 GiB = 4.5 GiB headroomed, more than half of 8 GiB available.
    assert!(history_requires_exclusive(
        Some(unit(3 * GIB, 2)),
        Some(8 * GIB)
    ));
}

#[test]
fn a_unit_that_fits_twice_is_not_exclusive() {
    // 1.5 x 1 GiB = 1.5 GiB, well under half of 8 GiB.
    assert!(!history_requires_exclusive(
        Some(unit(GIB, 5)),
        Some(8 * GIB)
    ));
}

#[test]
fn a_first_sighting_is_not_trusted() {
    // CI scoring: the two large history errors were single short-compile
    // readings, so one measurement never drives admission.
    assert!(!history_requires_exclusive(
        Some(unit(7 * GIB, 1)),
        Some(8 * GIB)
    ));
}

#[test]
fn no_history_or_no_memory_reading_never_adds_exclusivity() {
    assert!(!history_requires_exclusive(None, Some(8 * GIB)));
    assert!(!history_requires_exclusive(Some(unit(7 * GIB, 3)), None));
}

#[test]
fn headroom_is_applied_before_comparing() {
    // 2.9 GiB alone is under half of 6 GiB, but 1.5 x 2.9 = 4.35 GiB is not.
    assert!(history_requires_exclusive(
        Some(unit(2_900 * MIB, 2)),
        Some(6 * GIB)
    ));
}

#[test]
fn available_memory_prefers_cgroup_headroom_over_mem_available() {
    let bounded = soldr_platform::host::resources::HostResourceSnapshot {
        cgroup_limit_bytes: Some(16 * GIB),
        cgroup_current_bytes: Some(10 * GIB),
        system_available_bytes: Some(60 * GIB),
        ..Default::default()
    };
    assert_eq!(available_bytes(&bounded), Some(6 * GIB));

    let unbounded = soldr_platform::host::resources::HostResourceSnapshot {
        cgroup_limit_unbounded: true,
        system_available_bytes: Some(12 * GIB),
        ..Default::default()
    };
    assert_eq!(available_bytes(&unbounded), Some(12 * GIB));

    assert_eq!(
        available_bytes(&soldr_platform::host::resources::HostResourceSnapshot::default()),
        None
    );
}

#[test]
fn reclaimable_page_cache_does_not_force_exclusive_admission() {
    // soldr#4012: the cgroup is nearly full, but mostly with inactive page
    // cache. A 2.4 GiB unit must still be allowed to run beside another.
    let cached = soldr_platform::host::resources::HostResourceSnapshot {
        cgroup_limit_bytes: Some(16 * GIB),
        cgroup_current_bytes: Some(15 * GIB),
        cgroup_inactive_file_bytes: Some(11 * GIB),
        system_available_bytes: Some(66 * GIB),
        ..Default::default()
    };
    assert_eq!(available_bytes(&cached), Some(12 * GIB));
    assert!(!history_requires_exclusive(
        Some(unit(2_446 * MIB, 3)),
        available_bytes(&cached)
    ));
}

#[test]
fn a_short_host_caps_a_roomy_cgroup() {
    let short_host = soldr_platform::host::resources::HostResourceSnapshot {
        cgroup_limit_bytes: Some(16 * GIB),
        cgroup_current_bytes: Some(GIB),
        system_available_bytes: Some(4 * GIB),
        ..Default::default()
    };
    assert_eq!(available_bytes(&short_host), Some(4 * GIB));
}

#[test]
fn small_units_skip_the_resource_probe() {
    // The probe reads cgroup files and /proc/meminfo; a unit remembered below
    // the probe threshold can never need the whole machine, so it is skipped.
    assert!(!worth_probing(Some(unit(100 * MIB, 9))));
    assert!(
        !worth_probing(Some(unit(4 * GIB, 1))),
        "first sightings are not trusted"
    );
    assert!(worth_probing(Some(unit(PROBE_THRESHOLD_BYTES, 2))));
    assert!(!worth_probing(None));
}
