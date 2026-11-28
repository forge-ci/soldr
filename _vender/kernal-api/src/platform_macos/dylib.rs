//! Shared libraries through `dlopen`/`dlsym`.

use std::ffi::{c_void, CStr, CString, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt as _;

pub(crate) struct Library(*mut c_void);

// SAFETY: a dlopen handle is process-global and the dl* calls are thread-safe.
unsafe impl Send for Library {}
// SAFETY: as above; `symbol` only reads through the handle.
unsafe impl Sync for Library {}

fn nul_free(text: &[u8]) -> io::Result<CString> {
    CString::new(text).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains NUL"))
}

impl Library {
    pub(crate) fn open(name: &OsStr) -> io::Result<Self> {
        let name = nul_free(name.as_bytes())?;
        // SAFETY: a NUL-terminated name and standard flags.
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
        if handle.is_null() {
            // SAFETY: dlerror returns null or a NUL-terminated thread-local message.
            let reason = unsafe { libc::dlerror() };
            let reason = if reason.is_null() {
                "dlopen failed".to_owned()
            } else {
                // SAFETY: non-null, NUL-terminated, valid until the next dl* call.
                unsafe { CStr::from_ptr(reason) }.to_string_lossy().into_owned()
            };
            return Err(io::Error::new(io::ErrorKind::NotFound, reason));
        }
        Ok(Self(handle))
    }

    pub(crate) fn symbol(&self, name: &str) -> Option<*const c_void> {
        let name = nul_free(name.as_bytes()).ok()?;
        // SAFETY: a live handle and a NUL-terminated name.
        let address = unsafe { libc::dlsym(self.0, name.as_ptr()) };
        (!address.is_null()).then_some(address.cast_const())
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: the handle came from dlopen and is closed exactly once.
        unsafe { libc::dlclose(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libc_loads_and_exports_getpid() {
        let library = Library::open(OsStr::new(LIBC)).expect("libc loads");
        assert!(library.symbol("getpid").is_some());
        assert!(library.symbol("kernal_api_no_such_symbol").is_none());
    }

    const LIBC: &str = "/usr/lib/libSystem.B.dylib";
}
