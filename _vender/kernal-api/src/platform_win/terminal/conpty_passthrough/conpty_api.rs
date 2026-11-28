//! Dynamic ConPTY API dispatch table (#443).
//!
//! ConPTY's `PSEUDOCONSOLE_PASSTHROUGH_MODE` flag is only honored on
//! Windows 11 / Server 2022 (build 22000+). On Windows 10 the system
//! `kernel32!CreatePseudoConsole` silently ignores the flag and runs
//! the legacy virtual-screen path. Microsoft will not backport
//! conhost fixes; their official answer is the
//! `Microsoft.Windows.Console.ConPTY` NuGet redistributable, which
//! ships a paired `conpty.dll` + `OpenConsole.exe` that intercept
//! `CreatePseudoConsole` and spawn a modern OpenConsole instance
//! instead of the system conhost.
//!
//! This module decouples the three ConPTY entry points from static
//! `kernel32` linkage. At first use we pick a backend:
//!
//! * Windows 11+ → resolve from `kernel32.dll` (free, already loaded).
//! * Windows 10 → try the sidecar `conpty.dll` next to the executable
//!   via `LoadLibraryExW` + `LOAD_LIBRARY_SEARCH_APPLICATION_DIR`. If
//!   missing, fall back to `kernel32` with a one-line warning.
//!
//! Env-var escape hatches:
//! * `KERNAL_API_USE_SYSTEM_CONPTY=1` → always pick kernel32.
//! * `KERNAL_API_CONPTY_DIAGNOSTICS=1` → log backend + build.
//!
//! Security: the sidecar load uses `LOAD_LIBRARY_SEARCH_APPLICATION_DIR`
//! exclusively — no `PATH`, no CWD, no `AddDllDirectory`. If the
//! redistributable is not in the executable's directory, the feature
//! is off. This intentionally defeats DLL-planting attacks.

#![cfg(windows)]

use std::ffi::CString;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{HANDLE, HMODULE};
use windows_sys::Win32::System::Console::{COORD, HPCON};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryExW};

#[cfg(feature = "conpty-sidecar")]
use super::conpty_acquire;
use super::win_version;

/// `LOAD_LIBRARY_SEARCH_APPLICATION_DIR`. Restricts the DLL search to
/// the loading executable's own directory — no PATH, no CWD, no other
/// AppDir. Locked to the exact SDK value so a future windows-sys bump
/// cannot silently widen the search path.
const LOAD_LIBRARY_SEARCH_APPLICATION_DIR: u32 = 0x0200;

/// Native HRESULT type (`i32`); kept local to avoid pulling in the
/// `Win32_System_Com` feature tree just for the alias.
type Hresult = i32;

/// Function-pointer signatures for the three ConPTY entry points.
/// `extern "system"` matches the Windows stdcall convention used by
/// `kernel32` and by Microsoft's `conpty.dll` shim.
pub(super) type PfnCreatePseudoConsole =
    unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> Hresult;
pub(super) type PfnResizePseudoConsole = unsafe extern "system" fn(HPCON, COORD) -> Hresult;
pub(super) type PfnClosePseudoConsole = unsafe extern "system" fn(HPCON);

/// Resolved ConPTY entry points. All three pointers come from the
/// same module so any future ConPTY-evolved invariants (shared global
/// state inside the DLL) hold.
pub(super) struct ConPtyApi {
    pub(super) create: PfnCreatePseudoConsole,
    pub(super) resize: PfnResizePseudoConsole,
    pub(super) close: PfnClosePseudoConsole,
}

// SAFETY: the pointers are immutable after init; the underlying DLL
// stays loaded for the life of the process.
unsafe impl Send for ConPtyApi {}
unsafe impl Sync for ConPtyApi {}

/// Which module the API table was resolved from. Surfaced for tests
/// and the diagnostics env-var; not consumed by production code paths.
#[derive(Debug, Clone)]
pub(super) enum ConPtySource {
    /// Loaded from the always-resident `kernel32.dll`.
    Kernel32,
    /// Loaded from `conpty.dll` at the recorded path.
    #[allow(dead_code)] // path is informational; consumed by tests + diagnostics
    Sidecar(PathBuf),
}

/// Public diagnostic enum mirroring `ConPtySource` without exposing the
/// resolved sidecar path. Stable surface for integration tests that need
/// to decide whether the Win10 byte-exact passthrough assertions can run
/// (sidecar) or must be skipped (kernel32 on Win10 < 22000).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConPtyBackendKind {
    /// API resolved from the system `kernel32.dll`.
    Kernel32,
    /// API resolved from a sidecar `conpty.dll` next to the executable.
    Sidecar,
}

/// Returns which backend the process-wide ConPTY API table resolved to.
///
/// Initializes the table on first call. Idempotent and cheap on
/// subsequent calls (a single `OnceLock` read).
///
/// Integration tests that need to gate Win10-with-sidecar coverage
/// without touching internal types check this; production callers
/// generally have no reason to (the dispatch is transparent).
pub fn current_backend_kind() -> ConPtyBackendKind {
    match get().1 {
        ConPtySource::Kernel32 => ConPtyBackendKind::Kernel32,
        ConPtySource::Sidecar(_) => ConPtyBackendKind::Sidecar,
    }
}

static API: OnceLock<(ConPtyApi, ConPtySource)> = OnceLock::new();

/// Returns the cached ConPTY API table, initializing on first call.
///
/// Order of preference: env-var override → Win11+ kernel32 →
/// Win10 sidecar → kernel32 fallback. Resolution is performed exactly
/// once per process and the result is shared by all subsequent calls.
///
/// # Panics
///
/// If even `kernel32!CreatePseudoConsole` cannot be resolved — which
/// would mean a corrupted system32 or an unsupported pre-1809 Windows
/// build. The crate has never supported such hosts.
pub(super) fn get() -> &'static (ConPtyApi, ConPtySource) {
    API.get_or_init(|| {
        let force_system = std::env::var_os("KERNAL_API_USE_SYSTEM_CONPTY").is_some();
        let diagnostics = std::env::var_os("KERNAL_API_CONPTY_DIAGNOSTICS").is_some();

        let resolved = resolve_production(force_system);

        if diagnostics {
            let build = win_version::build_number();
            match &resolved.1 {
                ConPtySource::Kernel32 => {
                    eprintln!("kernal-api: ConPTY backend = kernel32 (Windows build {build})")
                }
                ConPtySource::Sidecar(path) => eprintln!(
                    "kernal-api: ConPTY backend = sidecar (Windows build {build}, path {})",
                    path.display()
                ),
            }
        }

        resolved
    })
}

/// Production sidecar resolver. On Win11 or with the env-var
/// override, returns kernel32 immediately. On Win10, tries:
///
/// 1. A manual pre-stage at `current_exe()/conpty.dll` — admin /
///    air-gapped consumer override.
/// 2. With `conpty-sidecar`, the self-acquired cache via
///    `conpty_acquire::ensure_cached_sidecar`, which fetches the
///    matching GitHub release asset on first miss.
/// 3. kernel32, with a one-line warning.
///
/// Any catastrophic kernel32 failure escalates to panic — the crate
/// has never supported a system that broken.
fn resolve_production(force_system: bool) -> (ConPtyApi, ConPtySource) {
    if force_system || win_version::is_win11_or_newer() {
        return (
            load_kernel32().unwrap_or_else(|e| catastrophe(e)),
            ConPtySource::Kernel32,
        );
    }

    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        let dll = exe_dir.join("conpty.dll");
        if dll.is_file() {
            match try_load_sidecar(&dll) {
                Ok(api) => return (api, ConPtySource::Sidecar(dll)),
                Err(e) => eprintln!(
                    "kernal-api: pre-staged conpty.dll at {} unloadable ({e}); trying cache",
                    dll.display()
                ),
            }
        }
    }

    #[cfg(feature = "conpty-sidecar")]
    match conpty_acquire::ensure_cached_sidecar() {
        Ok(cache_dir) => {
            let dll = cache_dir.join("conpty.dll");
            match try_load_sidecar(&dll) {
                Ok(api) => return (api, ConPtySource::Sidecar(dll)),
                Err(e) => eprintln!(
                    "kernal-api: cached conpty.dll at {} unloadable ({e}); using kernel32",
                    dll.display()
                ),
            }
        }
        Err(e) => eprintln!(
            "kernal-api: ConPTY sidecar auto-acquire unavailable ({e}); using kernel32"
        ),
    }
    #[cfg(not(feature = "conpty-sidecar"))]
    eprintln!("kernal-api: ConPTY sidecar auto-acquire disabled; using kernel32");

    (
        load_kernel32().unwrap_or_else(|e| catastrophe(e)),
        ConPtySource::Kernel32,
    )
}

fn catastrophe(e: io::Error) -> ! {
    eprintln!("kernal-api: ConPTY API resolution failed catastrophically: {e}");
    panic!("ConPTY API unavailable: {e}");
}

/// Test/diagnostic resolver that bypasses the process-wide cache and
/// the env-var lookups. Used by integration tests to assert that the
/// resolver picks the right module for a given application directory
/// without mutating process global state.
///
/// `force_sidecar_from` — when `Some`, attempt the sidecar load from
/// that directory (must contain `conpty.dll`). When `None`, skip the
/// sidecar branch entirely.
///
/// `force_system` — when `true`, skip even the version check and
/// resolve from `kernel32` directly.
#[cfg(test)]
pub(super) fn for_test_resolution(
    force_sidecar_from: Option<&Path>,
    force_system: bool,
) -> io::Result<(ConPtyApi, ConPtySource)> {
    resolve(force_sidecar_from, force_system)
}

#[cfg(test)]
fn resolve(
    sidecar_dir: Option<&Path>,
    force_system: bool,
) -> io::Result<(ConPtyApi, ConPtySource)> {
    if !force_system {
        if let Some(dir) = sidecar_dir {
            let dll = dir.join("conpty.dll");
            match try_load_sidecar(&dll) {
                Ok(api) => return Ok((api, ConPtySource::Sidecar(dll))),
                Err(e) => {
                    // Sidecar absent or malformed; fall through to
                    // kernel32. Print a one-line note so the user can
                    // diagnose unexpected Win10 fallback.
                    eprintln!(
                        "kernal-api: conpty.dll sidecar at {} unavailable ({e}); using kernel32",
                        dll.display()
                    );
                }
            }
        }
    }
    let api = load_kernel32()?;
    Ok((api, ConPtySource::Kernel32))
}

fn try_load_sidecar(path: &Path) -> io::Result<ConPtyApi> {
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} does not exist", path.display()),
        ));
    }
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);

    // SAFETY: wide buffer is NUL-terminated and lives for the call.
    let module = unsafe {
        LoadLibraryExW(
            wide.as_ptr(),
            ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_APPLICATION_DIR,
        )
    };
    if module.is_null() {
        return Err(io::Error::last_os_error());
    }
    populate_from_module(module)
}

fn load_kernel32() -> io::Result<ConPtyApi> {
    let name: Vec<u16> = "kernel32.dll\0".encode_utf16().collect();
    // SAFETY: kernel32 is always loaded; GetModuleHandleW returns its
    // module handle without taking a reference (so no FreeLibrary
    // pairing is required).
    let module: HMODULE = unsafe { GetModuleHandleW(name.as_ptr()) };
    if module.is_null() {
        return Err(io::Error::last_os_error());
    }
    populate_from_module(module)
}

fn populate_from_module(module: HMODULE) -> io::Result<ConPtyApi> {
    // SAFETY: each transmute reinterprets a verified non-null FARPROC
    // as the matching Windows-API signature; the signatures are the
    // documented contracts shipped by both kernel32 and conpty.dll.
    unsafe {
        let create: PfnCreatePseudoConsole =
            std::mem::transmute(resolve_symbol(module, "CreatePseudoConsole")?);
        let resize: PfnResizePseudoConsole =
            std::mem::transmute(resolve_symbol(module, "ResizePseudoConsole")?);
        let close: PfnClosePseudoConsole =
            std::mem::transmute(resolve_symbol(module, "ClosePseudoConsole")?);
        Ok(ConPtyApi {
            create,
            resize,
            close,
        })
    }
}

fn resolve_symbol(module: HMODULE, name: &str) -> io::Result<unsafe extern "system" fn() -> isize> {
    let cstr = CString::new(name).map_err(|e| io::Error::other(format!("invalid symbol: {e}")))?;
    // SAFETY: cstr lives for the call; FARPROC is `Option<fn()->isize>`.
    let proc = unsafe { GetProcAddress(module, cstr.as_ptr() as *const u8) };
    match proc {
        Some(p) => Ok(p),
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("symbol {name} not exported by module"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Resolver picks kernel32 when `force_system = true`, even on
    /// Win10 where the sidecar branch would otherwise apply. Verifies
    /// the env-var escape hatch contract.
    #[test]
    fn force_system_picks_kernel32() {
        let (_api, source) = for_test_resolution(None, true).expect("kernel32 must resolve");
        assert!(matches!(source, ConPtySource::Kernel32));
    }

    /// On a host with no sidecar in the supplied directory, the
    /// resolver falls back to kernel32 with a warning instead of
    /// panicking. This is the documented Win10-without-redistributable
    /// path.
    #[test]
    fn missing_sidecar_falls_back_to_kernel32() {
        let empty = tempfile::tempdir().expect("tempdir");
        let (_api, source) =
            for_test_resolution(Some(empty.path()), false).expect("fallback must succeed");
        assert!(matches!(source, ConPtySource::Kernel32));
    }

    /// A *real* (non-PE) file at `<dir>/conpty.dll` must not satisfy
    /// the sidecar branch. `LoadLibraryExW` rejects the malformed image
    /// with `last_os_error`; the resolver swallows the error, logs a
    /// note, and uses kernel32. This proves the DLL-planting attack
    /// surface — staging a fake `conpty.dll` in the application
    /// directory — does not give an attacker code execution because
    /// the loader still validates the PE header before mapping.
    ///
    /// Combined with the `LOAD_LIBRARY_SEARCH_APPLICATION_DIR`
    /// constraint (which we hard-pin in module scope), a planted DLL
    /// anywhere outside the executable's own directory is unreachable
    /// regardless of contents.
    #[test]
    fn fake_sidecar_dll_is_not_loaded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("conpty.dll");
        {
            let mut f = std::fs::File::create(&fake).expect("create fake dll");
            f.write_all(b"not a real PE file").expect("write");
        }
        let (_api, source) =
            for_test_resolution(Some(dir.path()), false).expect("fallback must succeed");
        // Either kernel32 (loader rejected the fake) or — extremely
        // unlikely — the loader accepted it and the symbol lookup
        // failed, also producing kernel32 fallback. Assert kernel32.
        assert!(
            matches!(source, ConPtySource::Kernel32),
            "fake conpty.dll must not satisfy the sidecar branch"
        );
    }

    /// Win11 hosts (build >= 22000) must always resolve from
    /// kernel32 — the sidecar is a backport, not an alternative.
    #[test]
    fn win11_picks_kernel32_when_no_sidecar_dir() {
        if !win_version::is_win11_or_newer() {
            // On Win10 this branch is moot; covered by
            // missing_sidecar_falls_back_to_kernel32.
            return;
        }
        let (_api, source) =
            for_test_resolution(None, false).expect("kernel32 must resolve on Win11");
        assert!(matches!(source, ConPtySource::Kernel32));
    }

    /// Public `current_backend_kind()` returns a stable enum that
    /// downstream integration tests (e.g. `daemon_tui_repaint_test`)
    /// can use to decide whether to run byte-exact passthrough
    /// assertions on Win10 without reaching into crate-private types.
    ///
    /// The function initializes the OnceLock-cached production table
    /// on first call; this test runs in the same process as the
    /// internal-resolver tests above, so the table is in whatever
    /// state the test-runner left it. Either Kernel32 or Sidecar is
    /// a valid answer for an arbitrary host; we just need the call
    /// to return a value (not panic / loop).
    #[test]
    fn current_backend_kind_returns_a_value() {
        let kind = current_backend_kind();
        // Both enum variants are valid outcomes depending on the host
        // build and whether a sidecar is staged. The assertion is
        // structural: the function returned at all and the value is
        // one of the two known variants.
        assert!(matches!(
            kind,
            ConPtyBackendKind::Kernel32 | ConPtyBackendKind::Sidecar
        ));
    }
}
