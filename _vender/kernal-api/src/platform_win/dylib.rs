//! Shared libraries through `LoadLibraryW`/`GetProcAddress`.

use std::ffi::{c_void, CString, OsStr};
use std::io;
use std::os::windows::ffi::OsStrExt as _;

use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

pub(crate) struct Library(HMODULE);

// SAFETY: a module handle is process-global and the loader calls are thread-safe.
unsafe impl Send for Library {}
// SAFETY: as above; `symbol` only reads through the handle.
unsafe impl Sync for Library {}

impl Library {
    pub(crate) fn open(name: &OsStr) -> io::Result<Self> {
        if name.encode_wide().any(|unit| unit == 0) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "name contains NUL"));
        }
        let wide: Vec<u16> = name.encode_wide().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated wide name.
        let module = unsafe { LoadLibraryW(wide.as_ptr()) };
        if module.is_null() {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(io::ErrorKind::NotFound, error));
        }
        Ok(Self(module))
    }

    pub(crate) fn symbol(&self, name: &str) -> Option<*const c_void> {
        let name = CString::new(name).ok()?;
        // SAFETY: a live module handle and a NUL-terminated ANSI name.
        let address = unsafe { GetProcAddress(self.0, name.as_ptr().cast()) }?;
        Some(address as *const c_void)
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: the handle came from LoadLibraryW and is freed exactly once.
        unsafe { FreeLibrary(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel32_loads_and_exports_get_current_process_id() {
        let library = Library::open(OsStr::new("kernel32.dll")).expect("kernel32 loads");
        assert!(library.symbol("GetCurrentProcessId").is_some());
        assert!(library.symbol("kernal_api_no_such_symbol").is_none());
    }
}
