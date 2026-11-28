//! Windows build-number detection for the ConPTY sidecar selector (#443).
//!
//! `GetVersionExW` lies about the OS version when the running binary
//! lacks a compatibility manifest entry for the current OS (Microsoft's
//! "AppCompat" shim, in place since Windows 8.1). `RtlGetVersion` from
//! `ntdll.dll` does not lie — it returns the real `dwBuildNumber`
//! regardless of manifest state. We use that to decide between the
//! system kernel32 ConPTY (Win11 build 22000+) and the sidecar
//! `conpty.dll` redistributable (Win10 build < 22000).
//!
//! The result is cached in a `OnceLock<u32>` so the dynamic lookup
//! happens at most once per process.

#![cfg(windows)]

use std::ffi::CString;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// First Windows 11 / Server 2022 build number. Builds at or above
/// this value honor `PSEUDOCONSOLE_PASSTHROUGH_MODE` natively.
pub(super) const WIN11_BUILD: u32 = 22000;

/// Layout of `RTL_OSVERSIONINFOW` per ntdll.h. We only read
/// `dwBuildNumber`, but the full struct shape must match exactly or
/// `RtlGetVersion` will reject the call via the `dwOSVersionInfoSize`
/// guard.
#[repr(C)]
struct RtlOsVersionInfoW {
    dw_os_version_info_size: u32,
    dw_major_version: u32,
    dw_minor_version: u32,
    dw_build_number: u32,
    dw_platform_id: u32,
    sz_csd_version: [u16; 128],
}

type RtlGetVersionFn = unsafe extern "system" fn(*mut RtlOsVersionInfoW) -> i32;

static BUILD_NUMBER: OnceLock<u32> = OnceLock::new();

/// Returns the Windows build number reported by `RtlGetVersion`.
/// Falls back to `0` if `ntdll!RtlGetVersion` cannot be resolved
/// (treat as "ancient Windows" — caller will pick the sidecar path,
/// which then fails to find conpty.dll and falls back to kernel32).
pub(super) fn build_number() -> u32 {
    *BUILD_NUMBER.get_or_init(|| unsafe {
        let ntdll_name: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
        let ntdll: HMODULE = LoadLibraryW(ntdll_name.as_ptr());
        if ntdll.is_null() {
            return 0;
        }
        let sym = CString::new("RtlGetVersion").expect("static literal contains no NUL");
        let proc = GetProcAddress(ntdll, sym.as_ptr() as *const u8);
        let Some(addr) = proc else {
            return 0;
        };
        let rtl_get_version: RtlGetVersionFn = std::mem::transmute(addr);

        let mut info: RtlOsVersionInfoW = std::mem::zeroed();
        info.dw_os_version_info_size = std::mem::size_of::<RtlOsVersionInfoW>() as u32;
        let status = rtl_get_version(&mut info);
        if status != 0 {
            return 0;
        }
        info.dw_build_number
    })
}

/// True on Windows 11 (build 22000) or newer — where the system
/// kernel32 ConPTY honors `PSEUDOCONSOLE_PASSTHROUGH_MODE`.
pub(super) fn is_win11_or_newer() -> bool {
    build_number() >= WIN11_BUILD
}
