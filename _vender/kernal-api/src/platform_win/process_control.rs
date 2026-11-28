//! Windows child-control, standard-stream, and jobserver mechanics.
//!
//! The neutral contracts live in `crate::platform::process`; this file only
//! answers them for Windows.

use std::ffi::OsStr;
use std::io;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};

use crate::platform::process::{
    ProcessIdentity, ProcessIdentityAction, ProcessIdentityActionError,
    ProcessIdentityUnavailable,
};

/// Give a synchronous child its own console-control process group.
pub fn configure_session_leader_command(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
}

/// Windows has no kernel primitive that force-kills a numeric process group.
pub fn force_terminate_child_process_group(_child: &std::process::Child) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process-group termination is unavailable on Windows; use kill_tree",
    ))
}

/// Apply a scheduling band to exactly `identity`.
///
/// The generation check and the priority change go through one handle, and
/// an open handle pins the process object, so no reissued PID can be reached.
pub fn set_priority_identity(
    identity: ProcessIdentity,
    priority: crate::ProcessPriority,
) -> Result<ProcessIdentityAction, ProcessIdentityActionError> {
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, SetPriorityClass,
        BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
    };
    const STILL_ACTIVE: u32 = 259;

    let class = match priority {
        crate::ProcessPriority::Normal => None,
        crate::ProcessPriority::Low => Some(BELOW_NORMAL_PRIORITY_CLASS),
        crate::ProcessPriority::Idle => Some(IDLE_PRIORITY_CLASS),
        crate::ProcessPriority::High => Some(HIGH_PRIORITY_CLASS),
    };
    // SAFETY: scalar arguments; the handle is closed exactly once below.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_INFORMATION,
            0,
            identity.pid(),
        )
    };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(code) if code == ERROR_INVALID_PARAMETER as i32 => {
                Ok(ProcessIdentityAction::AlreadyExited)
            }
            Some(code) if code == ERROR_ACCESS_DENIED as i32 => Err(
                ProcessIdentityActionError::Unavailable(ProcessIdentityUnavailable::PermissionDenied),
            ),
            _ => Err(ProcessIdentityActionError::Host(error)),
        };
    }
    let result = (|| {
        // SAFETY: zeroed POD out-parameters, valid for the call.
        let mut times: [FILETIME; 4] = unsafe { std::mem::zeroed() };
        let [creation, exit, kernel, user] = &mut times;
        if unsafe { GetProcessTimes(handle, creation, exit, kernel, user) } == 0 {
            return Err(ProcessIdentityActionError::Host(io::Error::last_os_error()));
        }
        let key = [
            (u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime),
            0,
        ];
        if !identity.has_native_key(key) {
            return Err(ProcessIdentityActionError::StaleIdentity);
        }
        let mut code = 0_u32;
        // SAFETY: `code` is a valid out-parameter.
        if unsafe { GetExitCodeProcess(handle, &mut code) } != 0 && code != STILL_ACTIVE {
            return Ok(ProcessIdentityAction::AlreadyExited);
        }
        let Some(class) = class else {
            return Ok(ProcessIdentityAction::Performed);
        };
        // SAFETY: `handle` carries PROCESS_SET_INFORMATION.
        if unsafe { SetPriorityClass(handle, class) } != 0 {
            Ok(ProcessIdentityAction::Performed)
        } else {
            Err(ProcessIdentityActionError::Host(io::Error::last_os_error()))
        }
    })();
    // SAFETY: `handle` is owned here and closed exactly once.
    unsafe { CloseHandle(handle) };
    result
}

/// Replace this process's three standard handles with `NUL`.
pub fn detach_standard_streams() {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
    use windows_sys::Win32::System::Console::{
        STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    for (slot, access) in [
        (STD_INPUT_HANDLE, GENERIC_READ),
        (STD_OUTPUT_HANDLE, GENERIC_WRITE),
        (STD_ERROR_HANDLE, GENERIC_WRITE),
    ] {
        if let Some(handle) = open_standard_stream_file(OsStr::new("NUL"), access, OPEN_EXISTING) {
            replace_standard_handle(slot, handle);
        }
    }
}

/// Redirect stdin to `NUL` and stdout/stderr to an append log file.
pub fn redirect_standard_streams_to_log(path: &std::path::Path) -> bool {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{
        SetFilePointerEx, FILE_END, OPEN_ALWAYS, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    if let Some(handle) = open_standard_stream_file(OsStr::new("NUL"), GENERIC_READ, OPEN_EXISTING)
    {
        replace_standard_handle(STD_INPUT_HANDLE, handle);
    }
    let Some(log) = open_standard_stream_file(path.as_os_str(), GENERIC_WRITE, OPEN_ALWAYS) else {
        return false;
    };
    // SAFETY: `log` is a valid file handle; the new-position pointer may be null.
    let _ = unsafe { SetFilePointerEx(log, 0, std::ptr::null_mut(), FILE_END) };
    // One handle serves both slots, as a Unix `dup2` pair would.
    replace_standard_handle(STD_OUTPUT_HANDLE, log);
    replace_standard_handle(STD_ERROR_HANDLE, log);
    true
}

fn open_standard_stream_file(path: &OsStr, access: u32, disposition: u32) -> Option<HANDLE> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let path: Vec<u16> = path.encode_wide().chain(Some(0)).collect();
    // SAFETY: `path` is NUL-terminated UTF-16 that outlives the call.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            disposition,
            0,
            std::ptr::null_mut(),
        )
    };
    (!handle.is_null() && handle != INVALID_HANDLE_VALUE).then_some(handle)
}

fn replace_standard_handle(slot: u32, handle: HANDLE) {
    use windows_sys::Win32::System::Console::{GetStdHandle, SetStdHandle};

    // SAFETY: standard-handle slots are process-global scalars.
    let old = unsafe { GetStdHandle(slot) };
    let _ = unsafe { SetStdHandle(slot, handle) };
    // The stdout and stderr slots may already share one handle; close it only
    // once no slot refers to it any more.
    if !old.is_null() && old != INVALID_HANDLE_VALUE && old != handle && !is_standard_handle(old) {
        // SAFETY: the previous handle is no longer installed in any slot.
        let _ = unsafe { CloseHandle(old) };
    }
}

fn is_standard_handle(handle: HANDLE) -> bool {
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
        .into_iter()
        // SAFETY: standard-handle slots are process-global scalars.
        .any(|slot| unsafe { GetStdHandle(slot) } == handle)
}

/// Windows has no inheritable GNU make `R,W` descriptor-pair jobserver.
pub fn native_jobserver_supported() -> bool {
    false
}

/// Uninhabited on Windows: construction always fails.
#[derive(Debug)]
pub struct NativeJobserver {
    never: std::convert::Infallible,
}

impl NativeJobserver {
    /// Always fails on Windows; `capacity == 0` is still `InvalidInput`.
    pub fn create(capacity: usize) -> io::Result<Self> {
        if capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "jobserver capacity must be greater than zero",
            ));
        }
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "GNU make jobserver pipes are unavailable on Windows",
        ))
    }

    /// The GNU make `--jobserver-auth` value; no value exists on Windows.
    pub fn auth_string(&self) -> String {
        match self.never {}
    }
}
