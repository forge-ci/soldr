//! `HPCON` (pseudo-console handle) wrapper (#150 W2 + #443).
//!
//! Ports `portable-pty-0.9.0/src/win/psuedocon.rs` to use windows-sys
//! directly. The key difference from portable-pty: the flags include
//! [`super::PSEUDOCONSOLE_PASSTHROUGH_MODE`] (0x8), which tells
//! ConPTY to forward the child's bytes verbatim instead of rendering
//! them into a virtual screen and re-emitting deltas. This is the
//! whole point of the #150 rewrite — without it the daemon's ring
//! buffer only sees ConPTY's synthesized DSR queries, not the
//! child's actual ANSI output.
//!
//! #443 decoupled the three ConPTY entry points from static
//! `kernel32` linkage; all calls below go through the dispatch table
//! in `super::conpty_api`, which on Windows 10 transparently picks
//! the bundled `conpty.dll` sidecar (where present) instead of the
//! shim-prone system ConPTY.
//!
//! `HPCON` is just a `*mut c_void` and is freely sendable across
//! threads (Windows pseudo-console handles are reference-counted by
//! the kernel; only `Close` mutates ownership state).

#![cfg(windows)]

use std::io;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Console::{COORD, HPCON, PSEUDOCONSOLE_INHERIT_CURSOR};

use super::conpty_api::{self, ConPtyApi};
use super::PSEUDOCONSOLE_PASSTHROUGH_MODE;

/// PSEUDOCONSOLE flag constants that windows-sys 0.59 does not yet
/// expose. Values lifted from `consoleapi.h` (Windows SDK).
const PSEUDOCONSOLE_RESIZE_QUIRK: u32 = 0x2;
const PSEUDOCONSOLE_WIN32_INPUT_MODE: u32 = 0x4;

/// Owned wrapper around an `HPCON`. Drops via `ClosePseudoConsole`.
pub(super) struct PseudoConsole {
    handle: HPCON,
    /// Reference to the dispatch table used to construct this handle.
    /// Captured so `Drop` reaches the same `Close` entry-point that
    /// allocated the handle, even if the global resolution were ever
    /// to be re-pointed at a different backend (today it cannot).
    api: &'static ConPtyApi,
}

// SAFETY: HPCON is a kernel-managed handle (just a HANDLE under the
// hood); the only thread-affinity concern is teardown, which we
// serialize via the &mut self requirement on Drop.
unsafe impl Send for PseudoConsole {}
unsafe impl Sync for PseudoConsole {}

impl PseudoConsole {
    /// Create a new pseudo-console of `size`, plumbed to read from
    /// `input` (child's stdin source) and write to `output` (child's
    /// stdout/stderr sink).
    ///
    /// The caller retains ownership of `input` and `output` — we only
    /// borrow them long enough for `CreatePseudoConsole` to dup what
    /// it needs internally.
    pub(super) fn new(size: COORD, input: HANDLE, output: HANDLE) -> io::Result<Self> {
        let (api, _source) = conpty_api::get();
        // windows-sys 0.59 types HPCON as `isize` (handle is opaque),
        // so a "null" sentinel is 0 rather than a null pointer.
        let mut hpc: HPCON = 0;
        let flags = PSEUDOCONSOLE_INHERIT_CURSOR
            | PSEUDOCONSOLE_RESIZE_QUIRK
            | PSEUDOCONSOLE_WIN32_INPUT_MODE
            | PSEUDOCONSOLE_PASSTHROUGH_MODE;
        // CreatePseudoConsole returns HRESULT (S_OK == 0).
        let hr = unsafe { (api.create)(size, input, output, flags, &mut hpc) };
        if hr != 0 {
            return Err(io::Error::other(format!(
                "CreatePseudoConsole failed: HRESULT 0x{:08x}",
                hr as u32
            )));
        }
        if hpc == 0 {
            return Err(io::Error::other(
                "CreatePseudoConsole returned S_OK but null HPCON",
            ));
        }
        Ok(Self { handle: hpc, api })
    }

    pub(super) fn as_handle(&self) -> HPCON {
        self.handle
    }

    pub(super) fn resize(&self, size: COORD) -> io::Result<()> {
        let hr = unsafe { (self.api.resize)(self.handle, size) };
        if hr != 0 {
            return Err(io::Error::other(format!(
                "ResizePseudoConsole failed: HRESULT 0x{:08x}",
                hr as u32
            )));
        }
        Ok(())
    }
}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        if self.handle != 0 {
            // ClosePseudoConsole returns void.
            unsafe { (self.api.close)(self.handle) };
            self.handle = 0;
        }
    }
}
