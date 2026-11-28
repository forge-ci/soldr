//! Direct ConPTY backend with `PSEUDOCONSOLE_PASSTHROUGH_MODE`.
//!
//! See #150. The previous upstream backend hardcoded ConPTY flags to
//! `PSUEDOCONSOLE_INHERIT_CURSOR | PSEUDOCONSOLE_RESIZE_QUIRK |
//! PSEUDOCONSOLE_WIN32_INPUT_MODE` with no API to add
//! `PSEUDOCONSOLE_PASSTHROUGH_MODE = 0x8`. Without passthrough,
//! ConPTY runs a virtual screen: child writes are rendered into a
//! virtual buffer and only ConPTY's synthesized re-emission reaches
//! the master, breaking byte-exact ANSI propagation to the daemon
//! ring buffer (#150 M5 follow-up).
//!
//! This module ports portable-pty's three Windows source files to
//! `windows-sys` directly and adds the passthrough flag. Public
//! surface mirrors what `native_pty_process.rs` needs: an `openpty`
//! returning a [`ConPtyPair`] with a master that exposes
//! `try_clone_reader` / `take_writer` / `resize` and a slave that
//! accepts a `spawn` call returning a [`ConPtyChild`].
//!
//! # `PSEUDOCONSOLE_PASSTHROUGH_MODE` OS support
//!
//! The passthrough flag is **only honored on Windows 11 (build
//! 22000+) and Server 2022+**. On Windows 10 — including the latest
//! 22H2 (build 19045) — ConPTY silently ignores the flag and runs
//! the normal virtual-screen path, emitting synthesized DSR queries
//! (`\x1b[6n`) instead of forwarding the child's bytes verbatim.
//! Confirmed empirically: on Win 10.0.19045 my master pipe receives
//! exactly the 4-byte `\x1b[6n` cursor query from ConPTY and never
//! sees the child's actual output.
//!
//! The byte-exact tests in `daemon_tui_repaint_test.rs` and
//! `tests/test_pty_tui_repaint.py` detect the OS at runtime and
//! skip on Windows 10 with a clear message; on Win11 / Server 2022
//! they exercise the full passthrough chain. Cross-platform
//! coverage on Linux / macOS POSIX PTYs is byte-exact by design
//! (no virtual-screen layer).

#![cfg(windows)]

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::COORD;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

// PSEUDOCONSOLE_PASSTHROUGH_MODE is the whole point of this rewrite.
// windows-sys 0.59 does not expose this constant; declared locally
// from the Microsoft consoleapi.h header value.
pub(super) const PSEUDOCONSOLE_PASSTHROUGH_MODE: u32 = 0x8;

pub(super) mod child;
// The sidecar self-acquisition module pulls in `zstd`, `tar`,
// `sha2` and the WinHTTP bindings — all gated by the `conpty-sidecar` feature.
// Builds without it fall back to manual pre-stage + kernel32 on Win10.
#[cfg(feature = "conpty-sidecar")]
pub(super) mod conpty_acquire;
pub(super) mod conpty_api;
// #447: compile-time SHA-256 verification table for the Win10 sidecar.
// Gated to match `conpty_acquire`.
#[cfg(feature = "conpty-sidecar")]
pub(super) mod conpty_sidecar_hashes;
#[cfg(feature = "conpty-sidecar")]
pub(super) mod winhttp_get;
pub(super) mod pipes;
pub(super) mod proc_thread_attr;
pub(super) mod pseudoconsole;
pub(super) mod win_version;

use child::ConPtyChild;
use pipes::{create_pipe, PipeDirection};
use proc_thread_attr::ProcThreadAttributeList;
use pseudoconsole::PseudoConsole;

/// Caller-facing PTY dimensions. Pixel fields are ignored on Windows
/// (ConPTY only consumes rows/cols) but we mirror portable-pty's
/// `PtySize` shape so callers can pass them through unchanged.
#[derive(Debug, Clone, Copy)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl From<PtySize> for COORD {
    fn from(size: PtySize) -> Self {
        COORD {
            X: size.cols as i16,
            Y: size.rows as i16,
        }
    }
}

/// One ConPTY: master end held by the host process, slave end ready
/// to be spawned into a child.
pub(super) struct ConPtyPair {
    pub master: ConPtyMaster,
    pub slave: ConPtySlave,
}

/// Host-side handle. Shares ownership of the `HPCON` with the slave
/// via an `Arc<Mutex<...>>` so `Drop` order is deterministic — the
/// HPCON is only released when the last clone goes away.
pub struct ConPtyMaster {
    pseudo_console: Arc<Mutex<PseudoConsole>>,
    /// Host-side handle for reading child stdout/stderr. `None` after
    /// `try_clone_reader` has taken it.
    reader: Option<OwnedHandle>,
    /// Host-side handle for writing child stdin. `None` after
    /// `take_writer` has taken it.
    writer: Option<OwnedHandle>,
    /// Cached last-known dimensions. ConPTY has no live query API, so
    /// `get_size` returns this. Seeded by `openpty`, updated by `resize`.
    current_size: Mutex<PtySize>,
}

impl ConPtyMaster {
    /// Take the host-side stdout/stderr reader. Returns
    /// `AlreadyTaken` on the second call — matches portable-pty's
    /// `take_writer` semantics (we use the same shape for symmetry).
    pub(super) fn try_clone_reader(&mut self) -> io::Result<Box<dyn Read + Send>> {
        let handle = self
            .reader
            .take()
            .ok_or_else(|| io::Error::other("ConPtyMaster reader already taken"))?;
        // SAFETY: we hand off ownership; HandleReader closes on drop.
        Ok(Box::new(HandleReader::new(handle)))
    }

    pub(super) fn take_writer(&mut self) -> io::Result<Box<dyn Write + Send>> {
        let handle = self
            .writer
            .take()
            .ok_or_else(|| io::Error::other("ConPtyMaster writer already taken"))?;
        Ok(Box::new(HandleWriter::new(handle)))
    }

    pub(super) fn resize(&self, size: PtySize) -> io::Result<()> {
        let pc = self
            .pseudo_console
            .lock()
            .expect("conpty pseudo-console mutex poisoned");
        pc.resize(size.into())?;
        *self
            .current_size
            .lock()
            .expect("conpty size mutex poisoned") = size;
        Ok(())
    }

    pub(super) fn get_size(&self) -> PtySize {
        *self
            .current_size
            .lock()
            .expect("conpty size mutex poisoned")
    }
}

/// Slave-side spawn target. Holds only the shared `HPCON`; the
/// ConPTY-side pipe handles are closed in `openpty` right after
/// `CreatePseudoConsole` returns (matching portable-pty's flow:
/// they pass `FileDescriptor` by value to `PsuedoCon::new` and the
/// values are dropped at function exit). Holding our copies open
/// past `CreatePseudoConsole` desyncs ConPTY's internal pipe
/// reference counts and breaks the master-side reader.
pub struct ConPtySlave {
    pseudo_console: Arc<Mutex<PseudoConsole>>,
}

impl ConPtySlave {
    /// Spawn a process whose stdio is routed through this ConPTY.
    /// `argv[0]` is the executable, `argv[1..]` the arguments.
    /// `env`, if present, is the full environment (KEY,VALUE pairs);
    /// `None` means "inherit parent env".
    pub(super) fn spawn(
        self,
        argv: &[OsString],
        cwd: Option<&Path>,
        env: Option<&[(OsString, OsString)]>,
    ) -> io::Result<ConPtyChild> {
        if argv.is_empty() {
            return Err(io::Error::other("conpty spawn requires non-empty argv"));
        }

        // Build wide-string command line (CreateProcessW parses this
        // into argv inside the child — see Microsoft's
        // CommandLineToArgvW for the parser this is the inverse of).
        let cmdline = build_command_line(argv)?;
        let mut cmdline_w: Vec<u16> = OsStr::new(&cmdline).encode_wide().collect();
        cmdline_w.push(0);

        // NOTE: we pass NULL for lpApplicationName so CreateProcessW
        // parses the first token of lpCommandLine itself and PATH-
        // searches the way `std::process::Command` does. Passing
        // argv[0] explicitly here forces an absolute-path lookup
        // that breaks `python` / `cmd` / other PATH-resolved spawns.

        // Wide cwd, if any.
        let cwd_w: Option<Vec<u16>> = cwd.map(|p| {
            let mut v: Vec<u16> = p.as_os_str().encode_wide().collect();
            v.push(0);
            v
        });
        let cwd_ptr = cwd_w
            .as_ref()
            .map(|v| v.as_ptr())
            .unwrap_or(std::ptr::null());

        // Wide env block, if any.
        let env_block: Option<Vec<u16>> = env.map(build_env_block);
        let env_ptr = env_block
            .as_ref()
            .map(|v| v.as_ptr() as *mut std::ffi::c_void)
            .unwrap_or(std::ptr::null_mut());

        // Build STARTUPINFOEXW with the pseudo-console attribute.
        let hpc_handle = self
            .pseudo_console
            .lock()
            .expect("conpty pseudo-console mutex poisoned")
            .as_handle();
        let mut attr_list = ProcThreadAttributeList::with_pseudoconsole(hpc_handle)?;

        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        // Per portable-pty's spawn_command (which works in production
        // via wezterm): set STARTF_USESTDHANDLES with the stdio fields
        // marked as INVALID_HANDLE_VALUE. This prevents the child from
        // inheriting whatever stdio our host process happens to have
        // — the PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE attribute on
        // lpAttributeList then actually connects the child's stdio to
        // the pseudo-console.
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
        si.lpAttributeList = attr_list.as_mut_ptr();

        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT;
        // bInheritHandles = FALSE per the official sample. The
        // pseudo-console-internal handles are NOT inheritable; the
        // attribute list does the connection.
        let ok = unsafe {
            CreateProcessW(
                std::ptr::null(),
                cmdline_w.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0, // bInheritHandles = FALSE
                flags,
                env_ptr,
                cwd_ptr,
                &si.StartupInfo,
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: CreateProcessW returned success; hProcess and
        // hThread are owned and unique.
        let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess as RawHandle) };
        let main_thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread as RawHandle) };

        // ConPTY-side pipe handles can be closed in the host now —
        // the child inherited duplicates. Dropping `self` (which
        // owns `_conpty_input` / `_conpty_output`) at function-end
        // closes them.

        Ok(ConPtyChild::new(process, main_thread))
    }
}

pub(super) fn openpty(size: PtySize) -> io::Result<ConPtyPair> {
    // Input: host writes, child reads.
    let stdin_pipe = create_pipe(PipeDirection::HostWriteChildRead)?;
    // Output: child writes, host reads.
    let stdout_pipe = create_pipe(PipeDirection::HostReadChildWrite)?;

    let pseudo_console = PseudoConsole::new(
        size.into(),
        owned_to_handle(&stdin_pipe.child),
        owned_to_handle(&stdout_pipe.child),
    )?;
    // Close the ConPTY-side handles in the host process now —
    // CreatePseudoConsole internally duplicated them, and ConPTY's
    // dup is what's plumbed to the child via
    // PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE. Keeping our copies open
    // past this point breaks ConPTY's pipe-reference counting and
    // the master-side reader never sees any data. portable-pty
    // achieves the same drop implicitly by consuming FileDescriptor
    // by value into PsuedoCon::new.
    drop(stdin_pipe.child);
    drop(stdout_pipe.child);

    let pseudo_console = Arc::new(Mutex::new(pseudo_console));

    let master = ConPtyMaster {
        pseudo_console: Arc::clone(&pseudo_console),
        reader: Some(stdout_pipe.host),
        writer: Some(stdin_pipe.host),
        current_size: Mutex::new(size),
    };
    let slave = ConPtySlave { pseudo_console };
    Ok(ConPtyPair { master, slave })
}

fn owned_to_handle(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle() as HANDLE
}

// ── Command line + env block construction ───────────────────────────

/// Build a Windows command-line string from argv. Each argument is
/// quoted+escaped per the rules `CommandLineToArgvW` parses with.
fn build_command_line(argv: &[OsString]) -> io::Result<OsString> {
    let mut out = OsString::new();
    for (i, arg) in argv.iter().enumerate() {
        if i > 0 {
            out.push(" ");
        }
        out.push(quote_argument(arg)?);
    }
    Ok(out)
}

/// Quote a single argument per Microsoft's rules. Conservative:
/// always wraps in double quotes and escapes embedded `"` and
/// trailing backslashes.
fn quote_argument(arg: &OsStr) -> io::Result<OsString> {
    // OsStr on Windows is WTF-16-ish but we convert to wide,
    // process, and re-wrap.
    let wide: Vec<u16> = arg.encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::other(
            "argv element contains a NUL byte; cannot pass to CreateProcessW",
        ));
    }
    // Empty args still need to be representable.
    if wide.is_empty() {
        return Ok(OsString::from("\"\""));
    }
    // Simple case: no whitespace and no quote chars — pass as-is.
    let needs_quoting = wide.iter().any(|&c| {
        c == b' ' as u16 || c == b'\t' as u16 || c == b'\n' as u16 || c == b'"' as u16 || c == 0x0B
    });
    if !needs_quoting {
        let s: OsString = std::os::windows::ffi::OsStringExt::from_wide(&wide);
        return Ok(s);
    }
    let mut out_w: Vec<u16> = Vec::with_capacity(wide.len() + 2);
    out_w.push(b'"' as u16);
    let mut backslashes = 0usize;
    for &c in &wide {
        if c == b'\\' as u16 {
            backslashes += 1;
            out_w.push(c);
        } else if c == b'"' as u16 {
            // Double each pending backslash, then add one to escape
            // the quote itself.
            for _ in 0..backslashes {
                out_w.push(b'\\' as u16);
            }
            backslashes = 0;
            out_w.push(b'\\' as u16);
            out_w.push(b'"' as u16);
        } else {
            backslashes = 0;
            out_w.push(c);
        }
    }
    // Double trailing backslashes so the closing quote isn't
    // accidentally escaped.
    for _ in 0..backslashes {
        out_w.push(b'\\' as u16);
    }
    out_w.push(b'"' as u16);
    Ok(std::os::windows::ffi::OsStringExt::from_wide(&out_w))
}

/// Build a UTF-16 environment block from `(key, value)` pairs.
/// Format is `K1=V1\0K2=V2\0...\0\0` (each pair NUL-terminated, full
/// block double-NUL-terminated).
fn build_env_block(env: &[(OsString, OsString)]) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::new();
    for (k, v) in env {
        for c in k.encode_wide() {
            out.push(c);
        }
        out.push(b'=' as u16);
        for c in v.encode_wide() {
            out.push(c);
        }
        out.push(0);
    }
    // Double-NUL terminator.
    out.push(0);
    out
}

// ── Handle-backed Read/Write wrappers ────────────────────────────────

struct HandleReader {
    handle: OwnedHandle,
}

impl HandleReader {
    fn new(handle: OwnedHandle) -> Self {
        Self { handle }
    }
}

impl Read for HandleReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use windows_sys::Win32::Storage::FileSystem::ReadFile;
        let mut read: u32 = 0;
        let ok = unsafe {
            ReadFile(
                self.handle.as_raw_handle() as HANDLE,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = io::Error::last_os_error();
            // ERROR_BROKEN_PIPE = 109 -> peer closed -> EOF.
            if err.raw_os_error() == Some(109) {
                return Ok(0);
            }
            return Err(err);
        }
        Ok(read as usize)
    }
}

struct HandleWriter {
    handle: OwnedHandle,
}

impl HandleWriter {
    fn new(handle: OwnedHandle) -> Self {
        Self { handle }
    }
}

impl Write for HandleWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        use windows_sys::Win32::Storage::FileSystem::WriteFile;
        let mut written: u32 = 0;
        let ok = unsafe {
            WriteFile(
                self.handle.as_raw_handle() as HANDLE,
                buf.as_ptr(),
                buf.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Anonymous pipes don't buffer in user-space; nothing to flush.
        Ok(())
    }
}

// Keep `IntoRawHandle` import in tree (otherwise rustc warns "unused
// import" on this conditional cfg-windows file when not exercised).
#[allow(dead_code)]
fn _silence_unused_into_raw_handle(h: OwnedHandle) -> RawHandle {
    h.into_raw_handle()
}
