//! macOS host resources: CPU topology via sysctl, available memory via Mach. The Win32
//! process/commit probes have no macOS analogue and answer `None`.

use std::sync::OnceLock;

/// cgroup v2 is a Linux facility.
pub fn cgroup_v2_dir() -> Option<std::path::PathBuf> {
    None
}

/// Physical CPU cores on this machine, or `None` when the topology could
/// not be read. Memoized: the daemon asks once at startup.
pub fn physical_cores() -> Option<usize> {
    static CACHED: OnceLock<Option<usize>> = OnceLock::new();
    *CACHED.get_or_init(|| detect_cores().filter(|cores| *cores > 0))
}

/// `hw.physicalcpu` is the count for *this* process's allowed set,
/// which is what we want; `hw.physicalcpu_max` would ignore a
/// restricted CPU affinity.
fn detect_cores() -> Option<usize> {
    let output = crate::platform::wait::output_for(
        std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.physicalcpu"]),
        crate::platform::wait::PROBE,
    )
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

/// The ToolHelp process walk is Windows-only; macOS callers get `None`
/// and keep only their neutral fields.
pub fn process_table() -> Option<Vec<(u32, String)>> {
    None
}

/// `GlobalMemoryStatusEx` is Windows-only; macOS callers get `None`.
pub fn commit_charge_mb() -> Option<(u64, u64)> {
    None
}

/// Memory the kernel could hand to a new process without paging, in bytes:
/// free + inactive + speculative + purgeable pages (soldr#2885). macOS has no
/// `/proc/meminfo`, so this is its counterpart of Linux's `MemAvailable`.
/// `None` when the kernel refuses the query -- never a guessed zero.
///
/// Read with `host_statistics64` directly rather than by running
/// `/usr/bin/vm_stat` (soldr#3427): the subprocess probe returned `None` in
/// every macOS Recovery replay while the Recovery harness itself reads memory
/// through `sysctl` because tools may be missing there. The Mach call needs no
/// file on disk.
pub fn available_physical_memory_bytes() -> Option<u64> {
    probe_available_memory().ok()
}

/// [`available_physical_memory_bytes`] with the reason for a `None`.
pub fn probe_available_memory() -> Result<u64, String> {
    // `mach_host_self` hands out a fresh send right on every call, so take it
    // once instead of leaking one per probe.
    static HOST: OnceLock<libc::mach_port_t> = OnceLock::new();
    // SAFETY: `mach_host_self` takes no arguments and only returns a port name.
    // `libc` marks the Mach bindings deprecated in favor of the `mach2` crate;
    // a new third-party dependency for one call is not worth it, and the
    // symbol is stable system API.
    #[allow(deprecated)]
    let host = *HOST.get_or_init(|| unsafe { libc::mach_host_self() });
    let mut stats = std::mem::MaybeUninit::<libc::vm_statistics64>::zeroed();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: `stats` provides `count` 32-bit words of storage for the
    // HOST_VM_INFO64 flavor, exactly as `HOST_VM_INFO64_COUNT` states.
    let status = unsafe {
        libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            stats.as_mut_ptr().cast::<libc::integer_t>(),
            &mut count,
        )
    };
    if status != libc::KERN_SUCCESS {
        return Err(format!("host_statistics64 failed: kern_return_t {status}"));
    }
    // SAFETY: the successful call initialized the structure.
    let stats = unsafe { stats.assume_init() };
    // SAFETY: `vm_page_size` is a plain integer the system library initializes
    // before `main`.
    let page_size = unsafe { libc::vm_page_size } as u64;
    available_bytes(
        [
            stats.free_count,
            stats.inactive_count,
            stats.speculative_count,
            stats.purgeable_count,
        ]
        .map(u64::from),
        page_size,
    )
    .ok_or_else(|| "host_statistics64 reported no reclaimable pages".to_string())
}

/// Sum the reclaimable page classes and scale by the page size. `None` when
/// the total is zero or overflows, which is unreadable rather than "no memory".
fn available_bytes(page_counts: [u64; 4], page_size: u64) -> Option<u64> {
    let pages = page_counts
        .iter()
        .try_fold(0u64, |total, count| total.checked_add(*count))?;
    if pages == 0 {
        return None;
    }
    pages.checked_mul(page_size)
}

/// Resident set size for `pid`, in bytes.
///
/// Shells out to `ps` rather than the `mach_task_self`/`task_info` FFI
/// pair: this file's `detect_cores` already establishes that a one-shot
/// subprocess probe is an accepted pattern here, and `ps -o rss=` needs no
/// new FFI surface or `libc` struct layout to keep in sync with the SDK.
/// `ps` reports `rss` in kB. `None` if the process has exited or the
/// subprocess could not be run.
pub fn process_rss_bytes(pid: u32) -> Option<u64> {
    let output = crate::platform::wait::output_for(
        std::process::Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &pid.to_string()]),
        crate::platform::wait::PROBE,
    )
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?
        .checked_mul(1024)
}

#[cfg(test)]
mod available_memory_tests {
    use super::*;

    #[test]
    fn reclaimable_page_classes_are_summed_and_scaled() {
        assert_eq!(available_bytes([10, 20, 3, 2], 16384), Some(35 * 16384));
    }

    #[test]
    fn an_unreadable_total_is_none_not_zero() {
        assert_eq!(available_bytes([0, 0, 0, 0], 16384), None);
        assert_eq!(available_bytes([u64::MAX, 1, 0, 0], 16384), None);
        assert_eq!(available_bytes([u64::MAX, 0, 0, 0], 16384), None);
    }

    #[test]
    fn live_mach_probe_reports_nonzero_available_memory() {
        let available = probe_available_memory().unwrap_or_else(|why| panic!("probe: {why}"));
        assert!(available > 0);
    }
}
