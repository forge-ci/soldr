//! Host-specific process metadata.

use crate::process::metadata::ProcessEntry;

/// Enumerate process leaders using kernel metadata only.
pub fn snapshot() -> Vec<ProcessEntry> {
    // The kernal-api snapshot in 0.1.40 refreshes every process with full
    // detail. Linux reads of unrelated processes' argv and environment can
    // block on their address-space locks, including during watchdog recovery.
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| process_entry(entry.file_name().to_str()?.parse().ok()?))
        .collect()
}

fn process_entry(pid: u32) -> Option<ProcessEntry> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    let stat = String::from_utf8_lossy(&stat);
    let (_, name_and_fields) = stat.split_once('(')?;
    let (name, fields) = name_and_fields.rsplit_once(") ")?;
    let mut fields = fields.split_whitespace();
    let running = !matches!(fields.next()?, "Z" | "T" | "t" | "X" | "x");
    let parent: u32 = fields.next()?.parse().ok()?;
    Some(ProcessEntry {
        pid,
        parent_pid: (parent != 0).then_some(parent),
        name: name.to_owned(),
        running,
    })
}

/// Whole Unix seconds, as stored by previous Soldr releases in broker leases.
pub fn start_time_unix(pid: u32) -> Option<u64> {
    let token = super::inspect::process_start_token(pid)?;
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let boot_time: u64 = stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .parse()
        .ok()?;
    // SAFETY: sysconf takes no pointers. A nonpositive result is invalid.
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let ticks_per_second = u64::try_from(ticks_per_second).ok()?;
    boot_time.checked_add(token.checked_div(ticks_per_second)?)
}

// The facade's display command line is flattened on Unix. Keep argument
// boundaries intact for recognizing `soldr broker serve` behind global flags.
/// Read argv and any available environment entries for this process.
pub fn arguments_and_environment(pid: u32) -> (Vec<String>, Vec<String>) {
    let read = |file| {
        std::fs::read(format!("/proc/{pid}/{file}"))
            .map(|bytes| nul_strings(&bytes).collect())
            .unwrap_or_default()
    };
    (read("cmdline"), read("environ"))
}

fn nul_strings(bytes: &[u8]) -> impl Iterator<Item = String> + '_ {
    bytes.split_inclusive(|byte| *byte == 0).map(|value| {
        String::from_utf8_lossy(value.strip_suffix(&[0]).unwrap_or(value)).into_owned()
    })
}
