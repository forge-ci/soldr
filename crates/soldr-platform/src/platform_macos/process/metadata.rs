//! Host-specific process metadata.

use crate::process::metadata::ProcessEntry;

/// Enumerate process leaders through the host facade.
pub fn snapshot() -> Vec<ProcessEntry> {
    kernal_api::platform::host_processes::snapshot()
        .into_iter()
        .filter(|process| process.pid() != 0)
        .map(|process| ProcessEntry {
            pid: process.pid(),
            parent_pid: process.parent_pid(),
            name: process.name().to_owned(),
            running: process.running(),
        })
        .collect()
}

/// Whole Unix seconds, as stored by previous Soldr releases in broker leases.
pub fn start_time_unix(pid: u32) -> Option<u64> {
    let token = super::inspect::process_start_token(pid)?;
    Some(token / 1_000_000)
}

/// Read argv and any available environment entries for this process.
pub fn arguments_and_environment(pid: u32) -> (Vec<String>, Vec<String>) {
    read_procargs(pid).unwrap_or_default()
}

fn read_procargs(pid: u32) -> Option<(Vec<String>, Vec<String>)> {
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROCARGS2,
        i32::try_from(pid).ok()?,
    ];
    let mut len = 0;
    let mut read = |buf, len: &mut usize| {
        // SAFETY: the MIB is initialized. The caller passes either null for
        // the size probe or a writable buffer of the size in len.
        unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf, len, std::ptr::null_mut(), 0) }
    };
    if read(std::ptr::null_mut(), &mut len) != 0 {
        return None;
    }
    let mut bytes = vec![0u8; len];
    if read(bytes.as_mut_ptr().cast(), &mut len) != 0 {
        return None;
    }
    bytes.truncate(len);
    let argc = usize::try_from(i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?)).ok()?;
    let data = bytes.get(4..)?;
    let exec_path_end = data.iter().position(|&byte| byte == 0)?;
    // Skip the executable path and the kernel's padding before argv[0].
    let argv_start = data[exec_path_end..].iter().position(|&byte| byte != 0)?;
    let mut entries = nul_strings(&data[exec_path_end + argv_start..]);
    let argv = entries.by_ref().take(argc).collect();
    Some((argv, entries.collect()))
}

fn nul_strings(bytes: &[u8]) -> impl Iterator<Item = String> + '_ {
    bytes.split_inclusive(|byte| *byte == 0).map(|value| {
        String::from_utf8_lossy(value.strip_suffix(&[0]).unwrap_or(value)).into_owned()
    })
}
