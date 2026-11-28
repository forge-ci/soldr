//! `STARTUPINFOEXW`-friendly proc-thread attribute list (#150 W2).
//!
//! Ports `portable-pty-0.9.0/src/win/procthreadattr.rs` to use
//! `windows-sys` directly. The single attribute we set is
//! `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`, which routes the spawned
//! child's stdio through our `HPCON` instead of inheriting the
//! parent's console.
//!
//! Lifetime invariant per MSDN: the `lpValue` buffer must remain
//! valid until `DeleteProcThreadAttributeList`. We satisfy this by
//! storing the `HPCON` value inside the wrapper in a `Box` (stable
//! address across moves of the outer struct) and pointing the
//! attribute list at `&*self.hpc_storage`.

#![cfg(windows)]

use std::ffi::c_void;
use std::io;
use std::ptr;

use windows_sys::Win32::System::Console::HPCON;
use windows_sys::Win32::System::Threading::{
    DeleteProcThreadAttributeList, InitializeProcThreadAttributeList, UpdateProcThreadAttribute,
    LPPROC_THREAD_ATTRIBUTE_LIST,
};

const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x00020016;

pub(super) struct ProcThreadAttributeList {
    /// Backing storage for the attribute list itself. Cast to
    /// `LPPROC_THREAD_ATTRIBUTE_LIST` when handed to Win32.
    buffer: Vec<u8>,
}

impl ProcThreadAttributeList {
    /// Build an attribute list with a single
    /// `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` entry referencing `hpc`.
    pub(super) fn with_pseudoconsole(hpc: HPCON) -> io::Result<Self> {
        // Probe call to get required buffer size. Per MSDN this call
        // returns FALSE with last_os_error == ERROR_INSUFFICIENT_BUFFER
        // (122), which we deliberately ignore — we only care about
        // the size written through the out parameter.
        let mut size: usize = 0;
        unsafe {
            let _ = InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size);
        }
        if size == 0 {
            return Err(io::Error::other(
                "InitializeProcThreadAttributeList size probe returned 0",
            ));
        }

        let mut buffer = vec![0u8; size];
        let list_ptr = buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        let ok = unsafe { InitializeProcThreadAttributeList(list_ptr, 1, 0, &mut size) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        // HPCON is passed BY VALUE as `lpValue` — NOT by pointer.
        // Both Microsoft's official ConPTY sample (samples/ConPTY/
        // EchoCon, `hPC` passed directly) and portable-pty 0.9
        // (`procthreadattr.rs::set_pty`) do this, even though MSDN
        // phrases `lpValue` as "a pointer to the attribute value".
        // HPCON is itself a HANDLE-typed pointer, and ConPTY
        // reinterprets `lpValue` as the HPCON directly. Cast HPCON
        // (`isize` in windows-sys 0.59) to `*const c_void` to satisfy
        // the FFI signature; `cbsize` stays `size_of::<HPCON>()`.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                list_ptr,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                hpc as *const c_void,
                std::mem::size_of::<HPCON>(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if ok == 0 {
            let err = io::Error::last_os_error();
            unsafe { DeleteProcThreadAttributeList(list_ptr) };
            return Err(err);
        }

        Ok(Self { buffer })
    }

    pub(super) fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST
    }
}

impl Drop for ProcThreadAttributeList {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST)
        };
    }
}
