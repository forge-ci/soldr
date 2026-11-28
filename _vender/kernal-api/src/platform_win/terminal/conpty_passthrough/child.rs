//! Spawned-process handle wrapper for ConPTY children (#150 W2).
//!
//! Narrower than the upstream PTY child trait — we own both sides of the
//! abstraction so we only expose what `native_pty_process.rs`
//! actually calls: `pid`, `try_wait`, `wait`, `kill`, `as_raw_handle`.
//!
//! Both the process and main-thread handles are stored as
//! [`std::os::windows::io::OwnedHandle`] so `Drop` closes them via
//! the standard library's `CloseHandle` wrapper.

#![cfg(windows)]

use std::io;
use std::os::windows::io::{AsRawHandle, OwnedHandle, RawHandle};

use windows_sys::Win32::Foundation::{GetLastError, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessId, TerminateProcess, WaitForSingleObject, INFINITE,
};

/// Return code GetExitCodeProcess reports while the process is still
/// running. Equals `STATUS_PENDING` from ntstatus.h.
const STILL_ACTIVE: u32 = 259;

pub struct ConPtyChild {
    process: OwnedHandle,
    _main_thread: OwnedHandle,
}

impl ConPtyChild {
    pub(crate) fn new(process: OwnedHandle, main_thread: OwnedHandle) -> Self {
        Self {
            process,
            _main_thread: main_thread,
        }
    }

    fn process_handle(&self) -> HANDLE {
        self.process.as_raw_handle() as HANDLE
    }

    pub(crate) fn pid(&self) -> u32 {
        unsafe { GetProcessId(self.process_handle()) }
    }

    /// Returns `Ok(Some(exit_code))` if the process has exited,
    /// `Ok(None)` if it's still running.
    pub(crate) fn try_wait(&self) -> io::Result<Option<u32>> {
        let mut code: u32 = 0;
        let ok = unsafe { GetExitCodeProcess(self.process_handle(), &mut code) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if code == STILL_ACTIVE {
            return Ok(None);
        }
        Ok(Some(code))
    }

    /// Blocks until the process exits, then returns the exit code.
    pub(crate) fn wait(&self) -> io::Result<u32> {
        let r = unsafe { WaitForSingleObject(self.process_handle(), INFINITE) };
        if r != WAIT_OBJECT_0 {
            let err = unsafe { GetLastError() };
            return Err(io::Error::other(format!(
                "WaitForSingleObject returned {r}, last_error = {err}"
            )));
        }
        let mut code: u32 = 0;
        let ok = unsafe { GetExitCodeProcess(self.process_handle(), &mut code) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(code)
    }

    /// `TerminateProcess(handle, 1)`. Best-effort hard kill; the
    /// Job-Object cleanup in the outer layer covers the case where
    /// the child has already exited.
    pub(crate) fn kill(&self) -> io::Result<()> {
        let ok = unsafe { TerminateProcess(self.process_handle(), 1) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Raw process handle for integration with Job Object assignment
    /// and process-priority APIs that take a HANDLE directly.
    pub(crate) fn as_raw_handle(&self) -> RawHandle {
        self.process.as_raw_handle()
    }
}
