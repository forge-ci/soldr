//! macOS per-process CPU and resident-memory readings from `proc_pid_rusage`.

/// A reaped child's accounting is gone, and its PID may be reissued.
pub const PEAK_RSS_READABLE_AFTER_EXIT: bool = false;

/// Upper bound on processes one tree reading visits.
pub const MAX_TREE_RSS_PROCESSES: usize = 4096;

fn rusage_v2(pid: i32) -> Option<libc::rusage_info_v2> {
    // SAFETY: zeroed POD filled by `proc_pid_rusage` for the matching flavor.
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, std::ptr::from_mut(&mut info).cast())
    };
    (result == 0).then_some(info)
}

/// `ri_user_time + ri_system_time`, an opaque monotonic unit.
pub fn cpu_ticks_for_pid(pid: u32) -> Option<u64> {
    let info = rusage_v2(i32::try_from(pid).ok()?)?;
    Some(info.ri_user_time.wrapping_add(info.ri_system_time))
}

/// The lifetime maximum physical footprint.
pub fn peak_rss_bytes_for_pid(pid: u32) -> Option<u64> {
    let pid = i32::try_from(pid).ok()?;
    // SAFETY: zeroed POD filled by `proc_pid_rusage` for the matching flavor.
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, std::ptr::from_mut(&mut info).cast())
    };
    (result == 0).then_some(info.ri_lifetime_max_phys_footprint)
}

/// Current `ri_resident_size` of `pid` plus every live descendant.
pub fn tree_rss_bytes_for_pid(pid: u32) -> Option<u64> {
    let root = i32::try_from(pid).ok()?;
    let mut total = rusage_v2(root)?.ri_resident_size;
    let mut seen = std::collections::HashSet::from([root]);
    let mut stack = vec![root];
    let mut buffer = vec![0_i32; 1024];
    while let Some(parent) = stack.pop() {
        let bytes = i32::try_from(std::mem::size_of_val(buffer.as_slice())).unwrap_or(i32::MAX);
        // SAFETY: `buffer` is writable for `bytes`; the call returns a PID count.
        let count =
            unsafe { libc::proc_listchildpids(parent, buffer.as_mut_ptr().cast(), bytes) };
        let Ok(count) = usize::try_from(count) else {
            continue;
        };
        for &child in &buffer[..count.min(buffer.len())] {
            if child <= 0 || seen.len() >= MAX_TREE_RSS_PROCESSES || !seen.insert(child) {
                continue;
            }
            if let Some(info) = rusage_v2(child) {
                total = total.saturating_add(info.ri_resident_size);
            }
            stack.push(child);
        }
    }
    Some(total)
}
