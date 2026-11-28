//! Private hooks for the ignored native containment proof.  This module is
//! feature-gated so ordinary worker builds cannot observe its environment.

const MARKER_ENV: &str = "KERNAL_API_WASM_WORKER_IDENTITY_MARKER";
const VERSION: &str = "kernal-api-worker-identity-v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WorkerIdentity {
    pub(super) pid: u32,
    pub(super) creation_a: u64,
    pub(super) creation_b: u64,
}

/// Capture an identity which is meaningful only for equality on this host.
pub(super) fn capture(pid: u32) -> std::io::Result<WorkerIdentity> {
    platform_capture(pid)
}

/// The parent alone reads this opt-in variable.  `spawn_contained_worker`
/// supplies an explicit empty environment, so the child cannot inherit it.
pub(super) fn publish_worker_identity(pid: u32) -> Result<(), ()> {
    let Some(path) = std::env::var_os(MARKER_ENV) else {
        return Ok(());
    };
    let identity = capture(pid).map_err(|_| ())?;
    let text = encode(identity);
    if text.len() > 256 {
        return Err(());
    }
    let path = std::path::PathBuf::from(path);
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temporary, text).map_err(|_| ())?;
    std::fs::rename(&temporary, &path).map_err(|_| {
        let _ = std::fs::remove_file(temporary);
    })
}

fn encode(identity: WorkerIdentity) -> String {
    format!(
        "{VERSION}\npid={}\ncreation-a={}\ncreation-b={}\n",
        identity.pid, identity.creation_a, identity.creation_b
    )
}

#[cfg(test)]
fn decode(text: &str) -> Option<WorkerIdentity> {
    let mut lines = text.lines();
    if lines.next()? != VERSION {
        return None;
    }
    let mut parse = |prefix| -> Option<u64> { lines.next()?.strip_prefix(prefix)?.parse().ok() };
    let identity = WorkerIdentity {
        pid: parse("pid=")?.try_into().ok()?,
        creation_a: parse("creation-a=")?,
        creation_b: parse("creation-b=")?,
    };
    if lines.next().is_some() {
        return None;
    }
    Some(identity)
}

#[cfg(target_os = "linux")]
fn platform_capture(pid: u32) -> std::io::Result<WorkerIdentity> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let start = linux_starttime(&text).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid /proc stat")
    })?;
    Ok(WorkerIdentity {
        pid,
        creation_a: start,
        creation_b: 0,
    })
}

/// Field 22 follows the final `)` of `comm`; command names may contain spaces
/// and parentheses, so whitespace splitting the whole record is incorrect.
#[cfg(target_os = "linux")]
fn linux_starttime(stat: &str) -> Option<u64> {
    let close = stat.rfind(')')?;
    let fields: Vec<_> = stat.get(close + 1..)?.split_whitespace().collect();
    // `fields[0]` is field 3 (state), therefore field 22 is offset 19.
    fields.get(19)?.parse().ok()
}

#[cfg(target_os = "windows")]
fn platform_capture(pid: u32) -> std::io::Result<WorkerIdentity> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: FILETIME is a plain Win32 output structure; all-zero is a
    // valid initial value before GetProcessTimes fills every field.
    let mut creation = unsafe { std::mem::zeroed() };
    // SAFETY: see the FILETIME initialization above.
    let mut exit = unsafe { std::mem::zeroed() };
    // SAFETY: see the FILETIME initialization above.
    let mut kernel = unsafe { std::mem::zeroed() };
    // SAFETY: see the FILETIME initialization above.
    let mut user = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetProcessTimes(
            process as HANDLE,
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    unsafe {
        CloseHandle(process as HANDLE);
    }
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let ticks = ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64;
    Ok(WorkerIdentity {
        pid,
        creation_a: ticks,
        creation_b: 0,
    })
}

// `proc_pidinfo(PROC_PIDTBSDINFO)` returns `proc_bsdinfo`; its start seconds
// and microseconds are copied as opaque equality components.  Keep the ABI
// declaration local because this is not a facade API.
#[cfg(target_os = "macos")]
fn platform_capture(pid: u32) -> std::io::Result<WorkerIdentity> {
    // Keep this private test-only identity capture ABI-identical to the
    // macOS verifier in `wasm_worker_containment`: a hand-written
    // `proc_bsdinfo` layout can silently differ by architecture and make the
    // worker reject marker publication before the proof can observe it.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let expected = i32::try_from(std::mem::size_of_val(&info)).expect("proc_bsdinfo size");
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            expected,
        )
    };
    if written != expected {
        return Err(std::io::Error::last_os_error());
    }
    Ok(WorkerIdentity {
        pid,
        creation_a: info.pbi_start_tvsec,
        creation_b: info.pbi_start_tvusec,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn marker_round_trip_and_rejects_extra_fields() {
        let value = WorkerIdentity {
            pid: 41,
            creation_a: 2,
            creation_b: 3,
        };
        assert_eq!(decode(&encode(value)), Some(value));
        assert_eq!(decode("wrong\npid=1\ncreation-a=2\ncreation-b=3\n"), None);
        assert_eq!(
            decode("kernal-api-worker-identity-v1\npid=1\ncreation-a=2\ncreation-b=3\nextra=x\n"),
            None
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_stat_parser_uses_final_comm_delimiter() {
        assert_eq!(
            linux_starttime("7 (has ) spaces) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19"),
            Some(19)
        );
    }
}
