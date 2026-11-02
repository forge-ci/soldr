//! Physical CPU topology and host memory/process pressure.

use std::path::Path;

/// `process_rss_bytes(pid)` reads one process's resident set size in bytes.
/// It is the per-process counterpart to [`HostResourceSnapshot`]'s
/// cgroup-wide fields — added for the daemon RSS ceiling (`SOLDR_DAEMON_RSS_
/// CEILING_BYTES`, see `soldr-daemon`'s maintenance module): a cgroup may
/// not exist at all (no `cgroup_v2_dir()`) or may be shared by more than one
/// process, so `cgroup_current_bytes` cannot answer "how much does *this*
/// daemon actually hold". `None` on a platform/host combination that cannot
/// answer (process gone, unreadable procfs, `ps`/API call failed).
///
/// `available_physical_memory_bytes()` is the OS-API counterpart of
/// `/proc/meminfo`'s `MemAvailable` on hosts that have no procfs: Windows
/// `ullAvailPhys`, macOS reclaimable pages (`host_statistics64`), and `None` on Linux,
/// whose answer is [`HostResourceSnapshot::system_available_bytes`].
pub use crate::platform_imp::host::resources::{
    available_physical_memory_bytes, cgroup_v2_dir, commit_charge_mb, physical_cores,
    process_rss_bytes, process_table,
};

const PROC_MEMINFO: &str = "/proc/meminfo";

/// One best-effort, portable observation of the resource boundary containing
/// Soldr. Cgroup fields are `None` off Linux or when the controller is not
/// readable; an unreadable value is never interpreted as zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostResourceSnapshot {
    /// Bytes charged to the current cgroup (`memory.current`).
    pub cgroup_current_bytes: Option<u64>,
    /// Reclaimable inactive page cache charged to the cgroup
    /// (`inactive_file` in `memory.stat`). Counted in `memory.current`, but
    /// the kernel drops it before reclaiming anon memory (soldr#4012).
    pub cgroup_inactive_file_bytes: Option<u64>,
    /// Lifetime peak bytes charged to the current cgroup (`memory.peak`).
    pub cgroup_peak_bytes: Option<u64>,
    /// Finite cgroup memory limit in bytes (`memory.max`).
    pub cgroup_limit_bytes: Option<u64>,
    /// Whether `memory.max` explicitly reports `max`.
    pub cgroup_limit_unbounded: bool,
    /// Swap bytes charged to the current cgroup (`memory.swap.current`).
    pub cgroup_swap_current_bytes: Option<u64>,
    /// Finite cgroup swap limit in bytes (`memory.swap.max`).
    pub cgroup_swap_limit_bytes: Option<u64>,
    /// Whether `memory.swap.max` explicitly reports `max`.
    pub cgroup_swap_limit_unbounded: bool,
    /// Sum of `oom_kill` and `oom_group_kill` from `memory.events`.
    pub cgroup_oom_kills: Option<u64>,
    /// Processes currently charged to the cgroup (`pids.current`).
    pub cgroup_pids_current: Option<u64>,
    /// Finite process limit for the cgroup (`pids.max`).
    pub cgroup_pids_limit: Option<u64>,
    /// Whether `pids.max` explicitly reports `max`.
    pub cgroup_pids_limit_unbounded: bool,
    /// Host-available memory parsed from `/proc/meminfo`, in bytes.
    pub system_available_bytes: Option<u64>,
}

impl HostResourceSnapshot {
    /// Capture the resource boundary containing this process.
    pub fn capture() -> Self {
        let meminfo = Path::new(PROC_MEMINFO);
        cgroup_v2_dir().map_or_else(
            || Self {
                system_available_bytes: read_text(meminfo).as_deref().and_then(mem_available),
                ..Self::default()
            },
            |cgroup_dir| Self::read_at(&cgroup_dir, meminfo),
        )
    }

    /// Read a snapshot from explicit fixture paths. This is also useful to
    /// callers observing a delegated cgroup rather than their own.
    pub fn read_at(cgroup_dir: &Path, meminfo_path: &Path) -> Self {
        let events = read_text(cgroup_dir.join("memory.events"));
        let (cgroup_limit_bytes, cgroup_limit_unbounded) =
            read_limit(cgroup_dir.join("memory.max"));
        let (cgroup_swap_limit_bytes, cgroup_swap_limit_unbounded) =
            read_limit(cgroup_dir.join("memory.swap.max"));
        let (cgroup_pids_limit, cgroup_pids_limit_unbounded) =
            read_limit(cgroup_dir.join("pids.max"));
        Self {
            cgroup_current_bytes: read_u64(cgroup_dir.join("memory.current")),
            cgroup_inactive_file_bytes: read_text(cgroup_dir.join("memory.stat"))
                .as_deref()
                .and_then(inactive_file),
            cgroup_peak_bytes: read_u64(cgroup_dir.join("memory.peak")),
            cgroup_limit_bytes,
            cgroup_limit_unbounded,
            cgroup_swap_current_bytes: read_u64(cgroup_dir.join("memory.swap.current")),
            cgroup_swap_limit_bytes,
            cgroup_swap_limit_unbounded,
            cgroup_oom_kills: events.as_deref().and_then(total_oom_kills),
            cgroup_pids_current: read_u64(cgroup_dir.join("pids.current")),
            cgroup_pids_limit,
            cgroup_pids_limit_unbounded,
            system_available_bytes: read_text(meminfo_path).as_deref().and_then(mem_available),
        }
    }

    /// Return finite cgroup memory headroom, saturating at zero.
    ///
    /// Reclaimable inactive page cache does not count as used: build I/O
    /// grows `memory.current` with file cache the kernel drops on demand, and
    /// counting it starved admission until the daemon restarted (soldr#4012).
    pub fn cgroup_headroom(&self) -> Option<u64> {
        let current = self.cgroup_current_bytes?;
        let used = current.saturating_sub(self.cgroup_inactive_file_bytes.unwrap_or(0));
        Some(self.cgroup_limit_bytes?.saturating_sub(used))
    }
}

fn read_text(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    read_text(path)?.trim().parse().ok()
}

fn read_limit(path: impl AsRef<Path>) -> (Option<u64>, bool) {
    let Some(raw) = read_text(path) else {
        return (None, false);
    };
    match raw.trim() {
        "max" => (None, true),
        raw => (raw.parse().ok(), false),
    }
}

fn mem_available(raw: &str) -> Option<u64> {
    raw.lines().find_map(|line| {
        let value = line.strip_prefix("MemAvailable:")?;
        value
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()?
            .checked_mul(1024)
    })
}

fn inactive_file(raw: &str) -> Option<u64> {
    raw.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next()? == "inactive_file").then_some(())?;
        fields.next()?.parse().ok()
    })
}

fn total_oom_kills(raw: &str) -> Option<u64> {
    let mut total = None;
    for line in raw.lines() {
        let mut fields = line.split_whitespace();
        let (Some(name), Some(value)) = (fields.next(), fields.next()) else {
            continue;
        };
        if name != "oom_kill" && name != "oom_group_kill" {
            continue;
        }
        if let Ok(value) = value.parse::<u64>() {
            total = Some(total.unwrap_or(0u64).saturating_add(value));
        }
    }
    total
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn snapshot_canonically_captures_memory_swap_pids_and_group_oom_kills() {
        let dir = tempfile::tempdir().expect("cgroup fixture");
        for (name, value) in [
            ("memory.current", "1073741824\n"),
            ("memory.peak", "4294967296\n"),
            ("memory.max", "8589934592\n"),
            ("memory.swap.current", "536870912\n"),
            ("memory.swap.max", "2147483648\n"),
            ("pids.current", "37\n"),
            ("pids.max", "512\n"),
            ("memory.events", "oom_kill 1\noom_group_kill 2\n"),
        ] {
            std::fs::write(dir.path().join(name), value).unwrap();
        }
        let meminfo = dir.path().join("meminfo");
        std::fs::write(&meminfo, "MemAvailable: 7000000 kB\n").unwrap();

        let snapshot = HostResourceSnapshot::read_at(dir.path(), &meminfo);
        assert_eq!(snapshot.cgroup_current_bytes, Some(1 << 30));
        assert_eq!(snapshot.cgroup_peak_bytes, Some(4 << 30));
        assert_eq!(snapshot.cgroup_limit_bytes, Some(8 << 30));
        assert_eq!(snapshot.cgroup_swap_current_bytes, Some(1 << 29));
        assert_eq!(snapshot.cgroup_swap_limit_bytes, Some(2 << 30));
        assert_eq!(snapshot.cgroup_oom_kills, Some(3));
        assert_eq!(snapshot.cgroup_pids_current, Some(37));
        assert_eq!(snapshot.cgroup_pids_limit, Some(512));
        assert_eq!(snapshot.system_available_bytes, Some(7_000_000 * 1024));
    }

    #[test]
    fn max_is_unbounded_and_missing_oom_keys_are_unknown() {
        let dir = tempfile::tempdir().expect("cgroup fixture");
        std::fs::write(dir.path().join("memory.max"), "max\n").unwrap();
        std::fs::write(dir.path().join("memory.events"), "oom 4\n").unwrap();
        let snapshot = HostResourceSnapshot::read_at(dir.path(), &dir.path().join("missing"));
        assert!(snapshot.cgroup_limit_unbounded);
        assert_eq!(snapshot.cgroup_oom_kills, None);
    }

    #[test]
    fn inactive_page_cache_is_headroom_not_usage() {
        // soldr#4012 host: 16 GiB limit, 15.6 GB charged, 11.6 GB inactive file.
        let dir = tempfile::tempdir().expect("cgroup fixture");
        std::fs::write(dir.path().join("memory.max"), "17179869184\n").unwrap();
        std::fs::write(dir.path().join("memory.current"), "15580610560\n").unwrap();
        std::fs::write(
            dir.path().join("memory.stat"),
            "anon 2931073024\nfile 12390547456\nactive_file 801099776\ninactive_file 11589472256\n",
        )
        .unwrap();
        let snapshot = HostResourceSnapshot::read_at(dir.path(), &dir.path().join("meminfo"));
        assert_eq!(snapshot.cgroup_inactive_file_bytes, Some(11_589_472_256));
        assert_eq!(
            snapshot.cgroup_headroom(),
            Some(17_179_869_184 - (15_580_610_560 - 11_589_472_256))
        );
    }
}
