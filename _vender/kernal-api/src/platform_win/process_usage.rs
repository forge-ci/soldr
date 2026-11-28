//! Windows per-process CPU and working-set readings.

use winapi::shared::minwindef::FILETIME;
use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
use winapi::um::processthreadsapi::{GetProcessTimes, OpenProcess};
use winapi::um::psapi::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use winapi::um::winnt::{HANDLE, PROCESS_QUERY_LIMITED_INFORMATION};

/// A retained child handle keeps the process object, its PID, and its final
/// peak working set readable after exit.
pub const PEAK_RSS_READABLE_AFTER_EXIT: bool = true;

/// Upper bound on processes one tree reading visits.
pub const MAX_TREE_RSS_PROCESSES: usize = 4096;

/// Run `read` against a query-only handle to `pid`, closing it afterwards.
fn with_query_handle<T>(pid: u32, read: impl FnOnce(HANDLE) -> Option<T>) -> Option<T> {
    // SAFETY: scalar arguments; the handle is closed exactly once below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let value = read(handle);
    // SAFETY: `handle` is owned here.
    unsafe { CloseHandle(handle) };
    value
}

/// Kernel plus user time in 100 ns units.
pub fn cpu_ticks_for_pid(pid: u32) -> Option<u64> {
    with_query_handle(pid, |handle| {
        // SAFETY: zeroed POD out-parameters, valid for the call.
        let mut times: [FILETIME; 4] = unsafe { std::mem::zeroed() };
        let [creation, exit, kernel, user] = &mut times;
        if unsafe { GetProcessTimes(handle, creation, exit, kernel, user) } == 0 {
            return None;
        }
        let value =
            |time: &FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
        Some(value(&times[2]).wrapping_add(value(&times[3])))
    })
}

fn memory_counters(pid: u32) -> Option<PROCESS_MEMORY_COUNTERS> {
    with_query_handle(pid, |handle| {
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // SAFETY: zeroed POD with `cb` set, as the API requires.
        let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        counters.cb = size;
        (unsafe { K32GetProcessMemoryInfo(handle, &mut counters, size) } != 0).then_some(counters)
    })
}

/// `PeakWorkingSetSize`.
pub fn peak_rss_bytes_for_pid(pid: u32) -> Option<u64> {
    memory_counters(pid).map(|counters| counters.PeakWorkingSetSize as u64)
}

/// Parent-to-children edges from one Toolhelp32 snapshot.
fn children_by_parent() -> std::collections::HashMap<u32, Vec<u32>> {
    use winapi::um::tlhelp32::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let mut children: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    // SAFETY: the snapshot handle is checked and closed exactly once; the
    // entry is a sized POD whose `dwSize` is set before use.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
            return children;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            if entry.th32ProcessID != entry.th32ParentProcessID {
                children
                    .entry(entry.th32ParentProcessID)
                    .or_default()
                    .push(entry.th32ProcessID);
            }
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    children
}

/// Current `WorkingSetSize` of `pid` plus every live descendant.
pub fn tree_rss_bytes_for_pid(pid: u32) -> Option<u64> {
    let mut total = memory_counters(pid)?.WorkingSetSize as u64;
    let children = children_by_parent();
    let mut seen = std::collections::HashSet::from([pid]);
    let mut stack = vec![pid];
    while let Some(parent) = stack.pop() {
        for &child in children.get(&parent).map(Vec::as_slice).unwrap_or_default() {
            if seen.len() >= MAX_TREE_RSS_PROCESSES || !seen.insert(child) {
                continue;
            }
            if let Some(counters) = memory_counters(child) {
                total = total.saturating_add(counters.WorkingSetSize as u64);
            }
            stack.push(child);
        }
    }
    Some(total)
}
