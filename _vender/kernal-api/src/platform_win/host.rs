//! Windows host facts, directories, user identity, resources, and autostart.

use std::ffi::OsString;
use std::io;
use std::path::Path;

/// A privileged system identity this process may be running as.
///
/// The variants name the *answer*, not the mechanism: what a caller does with
/// "this process is the machine's system account" does not change with how the
/// host was asked. `None` from [`current_process_privilege`] means an ordinary
/// user, which is the only case most callers care to distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegedIdentity {
    /// Unix effective UID 0.
    UnixRoot,
    /// Windows LocalSystem account (`S-1-5-18`).
    WindowsLocalSystem,
}

impl std::fmt::Display for PrivilegedIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnixRoot => f.write_str("root (effective uid 0)"),
            Self::WindowsLocalSystem => f.write_str("Windows LocalSystem (S-1-5-18)"),
        }
    }
}

/// The privileged identity this process is running as, if any.
///
/// Windows has no single number to compare, so this reads the process token's
/// user SID and matches it against LocalSystem's well-known value.
pub fn current_process_privilege() -> io::Result<Option<PrivilegedIdentity>> {
    let sid = current_user_sid_bytes()?;
    Ok(is_local_system_sid(&sid).then_some(PrivilegedIdentity::WindowsLocalSystem))
}

fn current_user_sid_bytes() -> io::Result<Vec<u8>> {
    use std::ptr;
    use winapi::shared::winerror::ERROR_INSUFFICIENT_BUFFER;
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcessToken};
    use winapi::um::securitybaseapi::{GetLengthSid, GetTokenInformation, IsValidSid};
    use winapi::um::winnt::{TokenUser, HANDLE, TOKEN_QUERY, TOKEN_USER};

    // SAFETY: this follows the standard Windows token query pattern:
    // open the current process token, ask for the required TOKEN_USER
    // buffer size, then copy the SID bytes out while the buffer is
    // still alive.
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::other(format!(
                "OpenProcessToken failed (GetLastError={})",
                GetLastError()
            )));
        }
        let token = TokenHandle(token);

        let mut required_size = 0_u32;
        let ok = GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required_size);
        let last = GetLastError();
        if ok != 0 || last != ERROR_INSUFFICIENT_BUFFER {
            return Err(io::Error::other(format!(
                "GetTokenInformation size query failed (ok={ok}, GetLastError={last})"
            )));
        }
        if required_size == 0 {
            return Err(io::Error::other(
                "GetTokenInformation reported 0 required bytes",
            ));
        }

        let mut buf = vec![0_u8; required_size as usize];
        if GetTokenInformation(
            token.0,
            TokenUser,
            buf.as_mut_ptr().cast(),
            required_size,
            &mut required_size,
        ) == 0
        {
            return Err(io::Error::other(format!(
                "GetTokenInformation real query failed (GetLastError={})",
                GetLastError()
            )));
        }

        let token_user: *const TOKEN_USER = buf.as_ptr().cast();
        let sid = (*token_user).User.Sid;
        if sid.is_null() {
            return Err(io::Error::other("TOKEN_USER returned a null SID pointer"));
        }
        if IsValidSid(sid) == 0 {
            return Err(io::Error::other("IsValidSid returned false"));
        }

        let len = GetLengthSid(sid) as usize;
        if len == 0 || len > 1024 {
            return Err(io::Error::other(format!(
                "GetLengthSid returned implausible length {len}"
            )));
        }
        Ok(std::slice::from_raw_parts(sid as *const u8, len).to_vec())
    }
}

struct TokenHandle(winapi::um::winnt::HANDLE);

impl Drop for TokenHandle {
    fn drop(&mut self) {
        unsafe {
            winapi::um::handleapi::CloseHandle(self.0);
        }
    }
}

fn is_local_system_sid(sid: &[u8]) -> bool {
    const LOCAL_SYSTEM_SID: &[u8] = &[1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];
    sid == LOCAL_SYSTEM_SID
}


/// A stable identity for this user on this machine.
///
/// The current user's SID already scopes both, so nothing else is mixed in.
/// The `windows-sid:` prefix keeps this distinguishable from the
/// `uid:machine` shape other hosts produce, as defence against an accidental
/// cross-platform collision. Callers hash this; they do not parse it.
pub fn user_machine_identity() -> io::Result<String> {
    use std::ptr;
    use winapi::shared::winerror::ERROR_INSUFFICIENT_BUFFER;
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcessToken};
    use winapi::um::securitybaseapi::GetTokenInformation;
    use winapi::um::winnt::{TokenUser, HANDLE, TOKEN_QUERY, TOKEN_USER};

    // SAFETY: the chain of Windows API calls below follows the documented
    // pattern for retrieving the current process user SID. Every allocated
    // buffer is freed before returning, and we never expose raw pointers to
    // safe Rust.
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::other(format!(
                "OpenProcessToken failed (GetLastError={})",
                GetLastError()
            )));
        }
        let close_token = TokenHandle(token);

        // First call: query required buffer size.
        let mut required_size: u32 = 0;
        let ok = GetTokenInformation(
            close_token.0,
            TokenUser,
            ptr::null_mut(),
            0,
            &mut required_size,
        );
        // We expect this to fail with ERROR_INSUFFICIENT_BUFFER.
        let last = GetLastError();
        if ok != 0 || last != ERROR_INSUFFICIENT_BUFFER {
            return Err(io::Error::other(format!(
                "GetTokenInformation size query failed (ok={ok}, GetLastError={last})"
            )));
        }
        if required_size == 0 {
            return Err(io::Error::other(
                "GetTokenInformation reported 0 required bytes",
            ));
        }

        let mut buf: Vec<u8> = vec![0u8; required_size as usize];
        if GetTokenInformation(
            close_token.0,
            TokenUser,
            buf.as_mut_ptr().cast(),
            required_size,
            &mut required_size,
        ) == 0
        {
            return Err(io::Error::other(format!(
                "GetTokenInformation real query failed (GetLastError={})",
                GetLastError()
            )));
        }

        // The buffer starts with a TOKEN_USER struct whose `User.Sid` points
        // into the same allocation.
        let token_user: *const TOKEN_USER = buf.as_ptr().cast();
        let sid_ptr = (*token_user).User.Sid;
        if sid_ptr.is_null() {
            return Err(io::Error::other("TOKEN_USER returned a null SID pointer"));
        }
        sid_to_identity(sid_ptr)
    }
}

/// Format a SID as `windows-sid:<hex>`.
///
/// `ConvertSidToStringSidW` would give the textual `S-1-...` form, but winapi
/// gates it behind the `sddl` feature. The raw bytes are exactly what that
/// call formats, and an identity only has to be stable per user per machine,
/// which both spellings satisfy.
///
/// # Safety
/// `sid` must point at a valid SID for the lifetime of this call.
unsafe fn sid_to_identity(sid: winapi::um::winnt::PSID) -> io::Result<String> {
    use winapi::um::securitybaseapi::{GetLengthSid, IsValidSid};

    if IsValidSid(sid) == 0 {
        return Err(io::Error::other("IsValidSid returned false"));
    }
    let len = GetLengthSid(sid) as usize;
    if len == 0 || len > 1024 {
        return Err(io::Error::other(format!(
            "GetLengthSid returned implausible length {len}"
        )));
    }
    let slice = std::slice::from_raw_parts(sid as *const u8, len);
    let mut hex = String::with_capacity(len * 2);
    for b in slice {
        hex.push(nibble_to_hex(b >> 4));
        hex.push(nibble_to_hex(b & 0x0F));
    }
    Ok(format!("windows-sid:{hex}"))
}

fn nibble_to_hex(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + (n - 10)) as char,
        _ => unreachable!("nibble out of range"),
    }
}

// ---------------------------------------------------------------------------
// Host identity facts
// ---------------------------------------------------------------------------

/// This machine's name as the host reports it.
pub fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME").ok().filter(|n| !n.is_empty())
}

/// A durable per-machine identifier that survives reboots.
///
/// The cryptography MachineGuid is the value Windows itself treats as the
/// machine's installation identity. A host whose registry hive is unreadable
/// falls back to the computer name: weaker, but still machine-scoped, which is
/// what the caller is asking about.
pub fn machine_id() -> Option<String> {
    machine_guid().or_else(hostname)
}

/// An identifier that changes on every boot of this machine.
///
/// `None` means neither boot-counter source answered -- the caller decides
/// what an unknown boot means, because only it knows what it is comparing.
pub fn boot_id() -> Option<String> {
    boot_counter().map(|counter| format!("windows-boot-{counter}"))
}

/// The volume this path lives on, as the host identifies volumes.
pub fn filesystem_device_id(path: &Path) -> Option<u64> {
    volume_serial(path)
}

/// Windows has no process namespaces of the kind this identity distinguishes.
pub fn namespace_id() -> Option<String> {
    None
}

fn boot_counter() -> Option<u32> {
    select_boot_counter(registry_boot_counter(), process_boot_counter)
}

fn select_boot_counter(registry: Option<u32>, process: impl FnOnce() -> Option<u32>) -> Option<u32> {
    registry.or_else(process)
}

fn registry_boot_counter() -> Option<u32> {
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD,
    };

    let subkey = wide_str(
        "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Memory Management\\PrefetchParameters",
    );
    let value = wide_str("BootId");
    let mut ty = 0_u32;
    let mut counter = 0_u32;
    let mut bytes = std::mem::size_of::<u32>() as u32;
    // SAFETY: both name pointers are NUL-terminated wide strings that outlive
    // the call, and `ty`/`counter`/`bytes` are valid writable storage sized as
    // the DWORD request requires.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            &mut ty,
            (&mut counter as *mut u32).cast(),
            &mut bytes,
        )
    };
    registry_dword(status, ty, bytes, counter)
}

/// Accept a registry read only when it succeeded *and* returned exactly the
/// DWORD that was asked for. A wrong type or a short read is a miss, not a
/// value, so the caller falls through to the other source.
fn registry_dword(status: u32, ty: u32, bytes: u32, value: u32) -> Option<u32> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::REG_DWORD;

    (status == ERROR_SUCCESS && ty == REG_DWORD && bytes as usize == std::mem::size_of::<u32>())
        .then_some(value)
}

/// Read the kernel boot counter through process telemetry when the registry
/// value is missing, inaccessible, or has an unexpected type.
fn process_boot_counter() -> Option<u32> {
    use windows_sys::Wdk::System::Threading::{
        NtQueryInformationProcess, ProcessTelemetryIdInformation,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    #[repr(C)]
    struct ProcessTelemetryInfo {
        header_size: u32,
        process_id: u32,
        process_start_key: u64,
        create_time: u64,
        create_interrupt_time: u64,
        create_unbiased_interrupt_time: u64,
        process_sequence_number: u64,
        session_create_time: u64,
        session_id: u32,
        boot_id: u32,
        image_checksum: u32,
        image_time_date_stamp: u32,
        user_sid_offset: u32,
        image_path_offset: u32,
        package_name_offset: u32,
        relative_app_name_offset: u32,
        command_line_offset: u32,
    }

    let boot_id_offset = std::mem::offset_of!(ProcessTelemetryInfo, boot_id);
    let boot_id_end = boot_id_offset + std::mem::size_of::<u32>();
    const MAX_PROCESS_TELEMETRY_BYTES: usize = 1024 * 1024;

    // SAFETY: GetCurrentProcess returns a pseudo-handle owned by Windows; it
    // must not and will not be closed by this process.
    let process = unsafe { GetCurrentProcess() };
    let mut needed = 0_u32;
    // SAFETY: a zero-length probe with a null output buffer asks the kernel for
    // the required allocation size. `needed` is valid writable storage.
    unsafe {
        NtQueryInformationProcess(
            process,
            ProcessTelemetryIdInformation,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    };
    if (needed as usize) < boot_id_end || needed as usize > MAX_PROCESS_TELEMETRY_BYTES {
        return None;
    }

    // The telemetry header is followed by variable-length strings. Size the
    // buffer from the kernel's first response and retry once if it grows.
    for _ in 0..2 {
        let words = (needed as usize).div_ceil(std::mem::size_of::<u64>());
        let mut buffer = vec![0_u64; words];
        let capacity = buffer.len() * std::mem::size_of::<u64>();
        let mut returned = needed;
        // SAFETY: `buffer` is writable for `capacity` bytes and is aligned more
        // strictly than the telemetry header. The current-process pseudo-handle
        // remains valid for the lifetime of this call.
        let status = unsafe {
            NtQueryInformationProcess(
                process,
                ProcessTelemetryIdInformation,
                buffer.as_mut_ptr().cast(),
                capacity as u32,
                &mut returned,
            )
        };
        if status >= 0 && returned as usize >= boot_id_end {
            // SAFETY: the successful query reported at least `boot_id_end`
            // initialized bytes. `read_unaligned` avoids relying on the buffer's
            // alignment for the field read.
            let boot_id = unsafe {
                std::ptr::read_unaligned(
                    buffer
                        .as_ptr()
                        .cast::<u8>()
                        .add(boot_id_offset)
                        .cast::<u32>(),
                )
            };
            return Some(boot_id);
        }
        if returned as usize <= capacity || returned as usize > MAX_PROCESS_TELEMETRY_BYTES {
            return None;
        }
        needed = returned;
    }
    None
}

fn machine_guid() -> Option<String> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_LOCAL_MACHINE, REG_SZ, RRF_RT_REG_SZ,
    };

    let subkey = wide_str("SOFTWARE\\Microsoft\\Cryptography");
    let value = wide_str("MachineGuid");
    let mut ty = 0_u32;
    let mut buf = [0_u16; 128];
    let mut bytes = (buf.len() * std::mem::size_of::<u16>()) as u32;
    // SAFETY: the name pointers are NUL-terminated wide strings that outlive
    // the call, and `buf` is writable for the `bytes` capacity handed over.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            &mut ty,
            buf.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != ERROR_SUCCESS || ty != REG_SZ {
        return None;
    }

    let len = (bytes as usize / std::mem::size_of::<u16>()).min(buf.len());
    let nul = buf[..len].iter().position(|ch| *ch == 0).unwrap_or(len);
    let guid = String::from_utf16_lossy(&buf[..nul]).trim().to_string();
    if guid.is_empty() {
        None
    } else {
        Some(guid)
    }
}

fn volume_serial(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetVolumeInformationByHandleW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let probe = existing_volume_probe_path(path)?;
    let wide: Vec<u16> = probe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is a NUL-terminated path buffer alive for the call. The
    // open requests no access rights, so it succeeds on a directory handle
    // used only to identify the volume.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }

    let mut serial = 0_u32;
    // SAFETY: `handle` is a live handle from the call above and `serial` is
    // valid writable storage; every other out-parameter is explicitly declined.
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            handle,
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    // SAFETY: `handle` is owned here and is not used again after this close.
    unsafe {
        CloseHandle(handle);
    }
    if ok == 0 {
        None
    } else {
        Some(serial as u64)
    }
}

/// Volume identity is a property of the volume, not of the leaf, so a path
/// that does not exist yet is answered by its nearest existing ancestor.
fn existing_volume_probe_path(path: &Path) -> Option<std::path::PathBuf> {
    path.ancestors()
        .find(|candidate| !candidate.as_os_str().is_empty() && candidate.exists())
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
}

fn wide_str(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------------
// Login environment
// ---------------------------------------------------------------------------

/// The logged-in user's environment, as a fresh login would see it.
///
/// Built from machine and user settings rather than copied from this process,
/// so variables that exist only here do not leak into it.
pub fn login_environment() -> io::Result<Vec<(OsString, OsString)>> {
    Ok(parse_environment_block(&login_environment_block()?))
}

/// The same environment in the double-NUL-terminated UTF-16 block form that
/// `CreateProcessW` takes, for callers driving that API themselves.
pub fn login_environment_block() -> io::Result<Vec<u16>> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY};
    use windows_sys::Win32::System::Environment::{
        CreateEnvironmentBlock, DestroyEnvironmentBlock,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // CreateEnvironmentBlock documents that the token needs TOKEN_QUERY,
    // TOKEN_DUPLICATE, and TOKEN_IMPERSONATE. With TOKEN_QUERY alone the
    // call still *succeeds* but silently omits the per-user dynamic
    // variables (USERNAME, USERDOMAIN) -- downstream consumers that key
    // behavior on USERNAME (e.g. soldr's daemon pipe name) then diverge
    // from processes holding the real login environment.
    let mut token = std::ptr::null_mut();
    // SAFETY: `token` is valid writable storage, and the pseudo-handle from
    // GetCurrentProcess is owned by Windows and never closed here.
    let opened = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE,
            &mut token,
        )
    };
    if opened == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut raw_block: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: `raw_block` is valid writable storage and `token` is the live
    // handle opened above.
    let created = unsafe { CreateEnvironmentBlock(&mut raw_block, token, 0) };
    let create_error = if created == 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    // SAFETY: `token` is owned here and is not used again after this close.
    unsafe {
        CloseHandle(token);
    }
    if let Some(error) = create_error {
        return Err(error);
    }

    // SAFETY: on success `raw_block` points at a Windows-owned, double-NUL
    // terminated UTF-16 block, which is exactly what the copy walks.
    let copied = unsafe { copy_environment_block(raw_block.cast::<u16>()) };
    // SAFETY: `raw_block` came from CreateEnvironmentBlock and is released
    // exactly once, after the copy has finished reading it.
    unsafe {
        DestroyEnvironmentBlock(raw_block);
    }
    Ok(copied)
}

/// # Safety
/// `cursor` must point at a double-NUL terminated UTF-16 block.
unsafe fn copy_environment_block(cursor: *const u16) -> Vec<u16> {
    let mut len = 0usize;
    loop {
        if *cursor.add(len) == 0 && *cursor.add(len + 1) == 0 {
            len += 2;
            break;
        }
        len += 1;
    }
    std::slice::from_raw_parts(cursor, len).to_vec()
}

fn parse_environment_block(block: &[u16]) -> Vec<(OsString, OsString)> {
    use std::os::windows::ffi::OsStringExt;

    let mut env = Vec::new();
    let mut offset = 0usize;
    while offset < block.len() && block[offset] != 0 {
        let Some(relative_end) = block[offset..].iter().position(|value| *value == 0) else {
            break;
        };
        let end = offset + relative_end;
        let entry = &block[offset..end];
        // Drive-current-directory pseudo variables have the shape
        // `=C:=C:\path`; skip index zero so their second '=' is the
        // key/value separator.
        if let Some(separator) = entry
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(index, value)| (*value == b'=' as u16).then_some(index))
        {
            let key = OsString::from_wide(&entry[..separator]);
            let value = OsString::from_wide(&entry[separator + 1..]);
            env.push((key, value));
        }
        offset = end + 1;
    }
    env
}

/// Windows environment variable names compare without regard to case.
pub fn environment_keys_are_case_insensitive() -> bool {
    true
}

/// Best-effort login name for the user running this process.
pub fn current_user() -> Option<String> {
    std::env::var("USERNAME").ok()
}

/// Best-effort home directory for the user running this process.
///
/// Reads `%USERPROFILE%`, falling back to `%HOME%` for the rare shell that
/// sets the Unix-style variable instead. A process whose environment sets
/// neither has no answer here; a caller that needs a directory even then
/// wants [`crate::fs_user_data_dir`] and its siblings, which synthesize a
/// fallback path instead of reporting `None`.
pub fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
}

/// Whether this process's token carries UAC elevation.
///
/// This is a different question from [`current_process_privilege`]: a
/// non-elevated interactive administrator session answers `None` there (the
/// account is not LocalSystem) but `true` here, and a UAC-elevated ordinary
/// user answers the opposite split. Both facts are real; a caller asking "can
/// I write to `Program Files`" wants this one.
pub fn is_elevated() -> io::Result<bool> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = std::ptr::null_mut();
    // SAFETY: `token` is valid writable storage, and the pseudo-handle from
    // GetCurrentProcess is owned by Windows and never closed here.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0_u32;
    // SAFETY: `elevation` is live writable storage sized exactly to what
    // TokenElevation queries write, and `token` is the handle opened above.
    let queried = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    let query_error = io::Error::last_os_error();
    // SAFETY: `token` is owned here and is not used again after this close.
    unsafe {
        CloseHandle(token);
    }
    if queried == 0 {
        return Err(query_error);
    }
    Ok(elevation.TokenIsElevated != 0)
}

/// Ordered compatibility-key feature subset this process can see.
///
/// Arch dispatch lives in the concrete host trees, not the neutral facade:
/// `target_arch` is one of the selectors the platform-boundary rule bans
/// outside them (#152). The three trees carry the same list on purpose --
/// the probe is host-independent, and duplicating it is the price of keeping
/// every `cfg` selector inside a selected tree.
///
/// Order and spellings are contractual: callers hash them, so an entry may be
/// appended but never renamed or reordered.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn cpu_compatibility_features() -> Vec<&'static str> {
    [
        ("sse2", std::arch::is_x86_feature_detected!("sse2")),
        ("sse4.2", std::arch::is_x86_feature_detected!("sse4.2")),
        ("avx", std::arch::is_x86_feature_detected!("avx")),
        ("avx2", std::arch::is_x86_feature_detected!("avx2")),
        ("avx512f", std::arch::is_x86_feature_detected!("avx512f")),
        ("fma", std::arch::is_x86_feature_detected!("fma")),
        ("bmi1", std::arch::is_x86_feature_detected!("bmi1")),
        ("bmi2", std::arch::is_x86_feature_detected!("bmi2")),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect()
}

/// No x86 compatibility-key features are reported on this architecture.
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
pub fn cpu_compatibility_features() -> Vec<&'static str> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_system_sid_is_detected() {
        assert!(is_local_system_sid(&[1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0]));
        assert!(!is_local_system_sid(&[
            1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 32, 2, 0, 0
        ]));
    }

    /// A CI runner is never LocalSystem, so elevation and process privilege
    /// answer independent questions here: privilege is `None` and elevation
    /// is whatever the runner's own token carries, but the query itself must
    /// always succeed.
    #[test]
    fn is_elevated_answers_without_erroring() {
        assert!(is_elevated().is_ok());
    }

    /// A test process resolves to some login name and home directory on a
    /// normal Windows session -- `%USERNAME%`/`%USERPROFILE%` are set by
    /// every login session and by cargo's own test harness.
    #[test]
    fn current_user_and_home_dir_answer_on_a_normal_session() {
        assert!(current_user().is_some_and(|name| !name.is_empty()));
        assert!(home_dir().is_some_and(|dir| !dir.as_os_str().is_empty()));
    }

    /// The machine id must be the installation GUID, not a restatement of the
    /// computer name -- the fallback exists, but a healthy host never uses it.
    #[test]
    fn windows_identity_uses_machine_and_volume_ids() {
        let cwd = std::env::current_dir().unwrap();
        assert_ne!(machine_id(), hostname());
        assert_ne!(filesystem_device_id(&cwd), Some(0));
        assert!(filesystem_device_id(&cwd).is_some());
    }

    #[test]
    fn windows_boot_id_is_the_stable_os_boot_counter() {
        use windows_sys::Win32::Foundation::ERROR_SUCCESS;
        use windows_sys::Win32::System::Registry::{
            RegGetValueW, HKEY_LOCAL_MACHINE, REG_DWORD, RRF_RT_REG_DWORD,
        };

        let subkey = wide_str(
            "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Memory Management\\PrefetchParameters",
        );
        let value = wide_str("BootId");
        let mut ty = 0_u32;
        let mut counter = 0_u32;
        let mut bytes = std::mem::size_of::<u32>() as u32;
        // SAFETY: the same documented RegGetValueW contract the implementation
        // uses; every pointer here outlives the call.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                subkey.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_DWORD,
                &mut ty,
                (&mut counter as *mut u32).cast(),
                &mut bytes,
            )
        };
        assert_eq!(status, ERROR_SUCCESS, "Windows must expose its BootId");
        assert_eq!(ty, REG_DWORD);
        assert_eq!(bytes as usize, std::mem::size_of::<u32>());

        let expected = Some(format!("windows-boot-{counter}"));
        for _ in 0..1_000 {
            assert_eq!(boot_id(), expected);
        }
    }

    #[test]
    fn windows_process_telemetry_boot_counter_is_stable() {
        let expected = process_boot_counter().expect("Windows process telemetry BootId");
        assert_ne!(expected, 0);
        for _ in 0..1_000 {
            assert_eq!(process_boot_counter(), Some(expected));
        }
    }

    #[test]
    fn windows_boot_counter_falls_back_for_missing_or_wrong_registry_value() {
        use windows_sys::Win32::Foundation::ERROR_SUCCESS;
        use windows_sys::Win32::System::Registry::REG_SZ;

        let process_counter = Some(42);
        assert_eq!(
            select_boot_counter(registry_dword(2, 0, 0, 0), || process_counter),
            process_counter
        );
        assert_eq!(
            select_boot_counter(
                registry_dword(ERROR_SUCCESS, REG_SZ, std::mem::size_of::<u32>() as u32, 7),
                || process_counter
            ),
            process_counter
        );
    }

    /// Windows reports no namespace, and that is a fact rather than a gap: the
    /// identity consumer must not read an empty string as "same namespace".
    #[test]
    fn windows_reports_no_process_namespace() {
        assert_eq!(namespace_id(), None);
    }

    #[test]
    fn parser_preserves_drive_current_directory_entries() {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;

        let block: Vec<u16> = OsStr::new("=C:=C:\\work")
            .encode_wide()
            .chain(std::iter::once(0))
            .chain(OsStr::new("Path=C:\\Windows").encode_wide())
            .chain(std::iter::once(0))
            .chain(std::iter::once(0))
            .collect();
        assert_eq!(
            parse_environment_block(&block),
            vec![
                (OsString::from("=C:"), OsString::from("C:\\work")),
                (OsString::from("Path"), OsString::from("C:\\Windows")),
            ]
        );
    }

    #[test]
    fn live_user_baseline_is_double_nul_terminated() {
        let block = login_environment_block().unwrap();
        assert!(block.len() >= 2);
        assert_eq!(&block[block.len() - 2..], &[0, 0]);
    }

    /// Regression: with TOKEN_QUERY-only access, CreateEnvironmentBlock
    /// succeeds but silently drops the per-user dynamic variables. The
    /// baseline must contain USERNAME (and it must match the live value
    /// when the current process has one).
    #[test]
    fn live_user_baseline_contains_username() {
        let env = login_environment().unwrap();
        let username = env
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("USERNAME"))
            .map(|(_, value)| value.clone())
            .expect("baseline environment must contain USERNAME");
        assert!(!username.is_empty(), "USERNAME must be non-empty");
        if let Ok(live) = std::env::var("USERNAME") {
            assert_eq!(
                username.to_string_lossy(),
                live,
                "baseline USERNAME must match the live login USERNAME"
            );
        }
    }
}
