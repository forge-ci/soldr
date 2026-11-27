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
    // FILETIME uses 100 ns intervals since 1601-01-01.
    (token / 10_000_000).checked_sub(11_644_473_600)
}

/// Read argv and any available environment entries for this process.
pub fn arguments_and_environment(pid: u32) -> (Vec<String>, Vec<String>) {
    // The process environment is private on Windows. The inventory can
    // still infer the home of installed brokers and daemons from their path.
    (windows_arguments(pid).unwrap_or_default(), Vec::new())
}

fn windows_arguments(pid: u32) -> Option<Vec<String>> {
    use windows_sys::Win32::{Foundation::LocalFree, UI::Shell::CommandLineToArgvW};

    let cmd = kernal_api::platform::process::read_process_cmdline(pid).ok()?;
    if cmd.is_empty() {
        return None;
    }
    let wide = widestring::U16CString::from_str(cmd).ok()?;
    let mut argc = 0;
    // SAFETY: wide is NUL-terminated and argc is a writable output.
    let argv = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut argc) };
    if argv.is_null() {
        return None;
    }
    // SAFETY: the API returns argc pointers to NUL-terminated strings in an
    // allocation owned by this call. All strings are copied before LocalFree.
    let args = unsafe {
        let args = std::slice::from_raw_parts(argv, argc as usize)
            .iter()
            .map(|&arg| widestring::U16CStr::from_ptr_str(arg).to_string_lossy())
            .collect();
        LocalFree(argv.cast());
        args
    };
    Some(args)
}
