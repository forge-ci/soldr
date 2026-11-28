//! macOS PTY implementation.

#[cfg(feature = "pty")]
mod pty {
use crate::platform::terminal::{
    PtyBackend, PtyChild, PtyInterruptTarget, PtyMaster, PtySize, PtySlave,
};
use portable_pty::{
    native_pty_system, Child as PortableChild, CommandBuilder, MasterPty,
    PtySize as PortablePtySize, SlavePty,
};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::Path;

pub struct PortablePtyBackend;
pub struct PortablePtyMaster(Box<dyn MasterPty + Send>);
pub struct PortablePtySlave(Box<dyn SlavePty + Send>);
pub struct PortablePtyChild(Box<dyn PortableChild + Send + Sync>);

impl PtyBackend for PortablePtyBackend {
    type Master = PortablePtyMaster;
    type Slave = PortablePtySlave;

    fn openpty(size: PtySize) -> io::Result<(Self::Master, Self::Slave)> {
        let pair = native_pty_system()
            .openpty(PortablePtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
            })
            .map_err(io::Error::other)?;
        Ok((PortablePtyMaster(pair.master), PortablePtySlave(pair.slave)))
    }
}

impl PtyMaster for PortablePtyMaster {
    fn try_clone_reader(&mut self) -> io::Result<Box<dyn Read + Send>> {
        self.0.try_clone_reader().map_err(io::Error::other)
    }

    fn take_writer(&mut self) -> io::Result<Box<dyn Write + Send>> {
        self.0.take_writer().map_err(io::Error::other)
    }

    fn resize(&self, size: PtySize) -> io::Result<()> {
        self.0
            .resize(PortablePtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
            })
            .map_err(io::Error::other)
    }

    fn get_size(&self) -> io::Result<PtySize> {
        let size = self.0.get_size().map_err(io::Error::other)?;
        Ok(PtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: size.pixel_width,
            pixel_height: size.pixel_height,
        })
    }

    fn process_group_leader(&self) -> Option<i32> {
        self.0.process_group_leader()
    }

    fn as_raw_fd(&self) -> Option<i32> {
        self.0.as_raw_fd()
    }

    fn interrupt_target(&self) -> io::Result<PtyInterruptTarget> {
        if let Some(pid) = self.0.process_group_leader() {
            return Ok(PtyInterruptTarget::new(move |_writer| {
                super::super::unix_signal_process_group(
                    pid,
                    crate::platform::process::UnixSignalKind::Interrupt,
                )?;
                Ok(false)
            }));
        }

        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        let fd = self
            .0
            .as_raw_fd()
            .ok_or_else(|| io::Error::other("PTY master does not expose a Unix descriptor"))?;
        let duplicated = unsafe { libc::dup(fd) };
        if duplicated < 0 {
            return Err(io::Error::last_os_error());
        }
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(duplicated) };
        Ok(PtyInterruptTarget::new(move |writer| {
            let _writer = match writer.try_lock() {
                Ok(writer) => writer,
                Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(io::Error::other("pty writer mutex poisoned"));
                }
            };
            super::write_nonblocking_byte(owned.as_raw_fd(), 0x03)?;
            Ok(true)
        }))
    }

    fn prepare_for_teardown(&self) -> io::Result<()> {
        let Some(fd) = self.0.as_raw_fd() else {
            return Ok(());
        };
        let original = super::fd_flags(fd)?;
        super::set_fd_flags(fd, original | libc::O_NONBLOCK)
    }

    fn write_available(&self, bytes: &[u8], timeout: std::time::Duration) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let fd = self
            .0
            .as_raw_fd()
            .ok_or_else(|| io::Error::other("PTY master does not expose a Unix descriptor"))?;
        // Status flags belong to the open file description, so every duplicate
        // shares them. The non-blocking window is therefore restored before
        // returning rather than left on the session's own writer.
        let original = super::fd_flags(fd)?;
        super::set_fd_flags(fd, original | libc::O_NONBLOCK)?;
        let result = super::write_bounded(fd, bytes, timeout);
        let _ = super::set_fd_flags(fd, original);
        result
    }

    fn kill_process_group(&self) -> io::Result<()> {
        match self.0.process_group_leader() {
            Some(pid) => super::super::unix_signal_process_group(
                pid,
                crate::platform::process::UnixSignalKind::Kill,
            ),
            None => Ok(()),
        }
    }

    fn preferred_pid(&self, child: &dyn PtyChild) -> Option<u32> {
        self.0
            .process_group_leader()
            .and_then(|pid| u32::try_from(pid).ok())
            .or_else(|| Some(child.pid()))
    }
}

impl PtySlave for PortablePtySlave {
    type Child = PortablePtyChild;

    fn spawn(
        self,
        argv: &[OsString],
        cwd: Option<&Path>,
        env: Option<&[(OsString, OsString)]>,
    ) -> io::Result<Self::Child> {
        if argv.is_empty() {
            return Err(io::Error::other("portable-pty spawn requires non-empty argv"));
        }
        let mut command = CommandBuilder::new(&argv[0]);
        for arg in &argv[1..] {
            command.arg(arg);
        }
        if let Some(cwd) = cwd {
            command.cwd(cwd);
        }
        if let Some(env) = env {
            command.env_clear();
            for (key, value) in env {
                command.env(key, value);
            }
        }
        let child = self.0.spawn_command(command).map_err(io::Error::other)?;
        Ok(PortablePtyChild(child))
    }
}

impl PtyChild for PortablePtyChild {
    fn pid(&self) -> u32 {
        self.0.process_id().unwrap_or(0)
    }

    fn try_wait(&mut self) -> io::Result<Option<u32>> {
        self.0
            .try_wait()
            .map(|status| status.map(|status| status.exit_code()))
    }

    fn wait(&mut self) -> io::Result<u32> {
        self.0.wait().map(|status| status.exit_code())
    }

    fn kill(&mut self) -> io::Result<()> {
        self.0.kill()
    }

}

pub type Backend = PortablePtyBackend;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConPtyBackendKind {
    Unavailable,
}

pub fn current_backend_kind() -> ConPtyBackendKind {
    ConPtyBackendKind::Unavailable
}
}

#[cfg(feature = "pty")]
pub use pty::*;

#[cfg(feature = "pty")]
use crate::platform::process::UnixSignalKind;
#[cfg(feature = "terminal-input")]
use crate::platform::terminal::PtyInputChunk;

#[cfg(feature = "pty")]
pub struct PtySpawnContext;

#[cfg(feature = "pty")]
pub struct PtyProcessGuard;

#[cfg(feature = "pty")]
impl PtyProcessGuard {
    pub fn assign_pid(&self, _pid: u32) -> std::io::Result<()> { Ok(()) }
}

#[cfg(feature = "pty")]
impl Drop for PtyProcessGuard {
    fn drop(&mut self) {}
}

#[cfg(feature = "pty")]
#[derive(Debug, Clone)]
pub struct ChildProcessInfo {
    pub pid: u32,
    pub name: String,
}

#[cfg(feature = "pty")]
#[derive(Debug, Clone)]
pub struct OrphanConhostInfo {
    pub pid: u32,
    pub parent_pid: u32,
    pub parent_name: String,
}

#[cfg(feature = "pty")]
pub fn before_pty_spawn() -> PtySpawnContext { PtySpawnContext }

#[cfg(feature = "pty")]
pub fn prepare_unmanaged_pty_child(
    _context: PtySpawnContext,
    _nice: Option<i32>,
) -> std::io::Result<PtyProcessGuard> { Ok(PtyProcessGuard) }

#[cfg(feature = "pty")]
pub fn input_payload(data: &[u8]) -> Vec<u8> { data.to_vec() }

#[cfg(feature = "pty")]
pub fn query_responses(_data: &[u8]) -> Vec<Vec<u8>> { Vec::new() }

#[cfg(feature = "pty")]
pub fn shell_argv(command: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), command.into()]
}

#[cfg(feature = "pty")]
pub fn wait_before_pty_close_supported() -> bool { true }

#[cfg(feature = "pty")]
pub fn is_ignorable_process_control_error(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput)
        || error.raw_os_error() == Some(libc::ESRCH)
}

#[cfg(feature = "pty")]
fn set_fd_flags(fd: i32, flags: libc::c_int) -> std::io::Result<()> {
    loop {
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } != -1 { return Ok(()); }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted { return Err(error); }
    }
}

#[cfg(feature = "pty")]
fn fd_flags(fd: i32) -> std::io::Result<libc::c_int> {
    loop {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags != -1 {
            return Ok(flags);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Write as much of `bytes` as `fd` accepts before `timeout` elapses.
///
/// `fd` must already be non-blocking. Returns `0` when the timeout elapses with
/// the queue still full, which is the point: a caller watching for something
/// else regains control instead of parking in the kernel.
#[cfg(feature = "pty")]
fn write_bounded(fd: i32, bytes: &[u8], timeout: std::time::Duration) -> std::io::Result<usize> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if written >= 0 {
            return Ok(written as usize);
        }
        let error = std::io::Error::last_os_error();
        match error.kind() {
            std::io::ErrorKind::Interrupted => continue,
            std::io::ErrorKind::WouldBlock => {}
            _ => return Err(error),
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(0);
        }
        let millis = remaining.as_millis().min(i32::MAX as u128) as libc::c_int;
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            return Ok(0);
        }
    }
}

#[cfg(feature = "pty")]
fn write_nonblocking_byte(fd: i32, byte: u8) -> std::io::Result<()> {
    let original_flags = loop {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags != -1 { break flags; }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted { return Err(error); }
    };
    set_fd_flags(fd, original_flags | libc::O_NONBLOCK)?;
    let written = unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
    let result = if written == 1 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if written == -1 && matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) {
            Ok(())
        } else if written == -1 {
            Err(error)
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "PTY interrupt fallback wrote zero bytes"))
        }
    };
    set_fd_flags(fd, original_flags).and(result)
}

#[cfg(feature = "pty")]
pub fn terminate_pty_child(pid: u32) -> std::io::Result<bool> {
    super::unix_signal_process(pid, UnixSignalKind::Terminate)?;
    Ok(false)
}

#[cfg(feature = "pty")]
fn descendant_pids(system: &sysinfo::System, pid: sysinfo::Pid) -> Vec<sysinfo::Pid> {
    let mut children = std::collections::HashMap::<sysinfo::Pid, Vec<sysinfo::Pid>>::new();
    for (child_pid, process) in system.processes() {
        if let Some(parent) = process.parent() { children.entry(parent).or_default().push(*child_pid); }
    }
    let mut descendants = Vec::new();
    let mut stack = vec![pid];
    while let Some(current) = stack.pop() {
        if let Some(direct) = children.get(&current) {
            for &child in direct { descendants.push(child); stack.push(child); }
        }
    }
    descendants
}

#[cfg(feature = "pty")]
pub fn signal_pty_tree(pid: u32, force: bool) -> std::io::Result<bool> {
    let system = sysinfo::System::new_all();
    let root = sysinfo::Pid::from_u32(pid);
    if system.process(root).is_none() { return Ok(false); }
    let mut targets = descendant_pids(&system, root);
    targets.reverse();
    targets.push(root);
    let signal = if force { UnixSignalKind::Kill } else { UnixSignalKind::Terminate };
    for target in targets {
        if let Err(error) = super::unix_signal_process(target.as_u32(), signal) {
            if !is_ignorable_process_control_error(&error) { return Err(error); }
        }
    }
    Ok(false)
}

#[cfg(feature = "pty")]
pub fn resize_pty(
    master: &dyn crate::platform::terminal::PtyMaster,
    size: crate::platform::terminal::PtySize,
) -> std::io::Result<()> { master.resize(size) }

#[cfg(feature = "pty")]
pub fn find_child_processes(_parent_pid: u32) -> Vec<ChildProcessInfo> { Vec::new() }

#[cfg(feature = "pty")]
pub fn find_orphan_conhosts() -> Vec<OrphanConhostInfo> { Vec::new() }

#[cfg(feature = "terminal-input")]
pub struct TerminalInputSession {
    stdin_fd: i32,
    original_mode: libc::termios,
    _input_lease: crate::platform::terminal::InputLease,
}

#[cfg(feature = "terminal-input")]
impl TerminalInputSession {
    pub fn new() -> std::io::Result<Option<Self>> {
        Self::new_with_mode(true)
    }

    pub(crate) fn new_for_keys() -> std::io::Result<Option<Self>> {
        Self::new_with_mode(false)
    }

    fn new_with_mode(raw: bool) -> std::io::Result<Option<Self>> {
        let stdin_fd = libc::STDIN_FILENO;
        if unsafe { libc::isatty(stdin_fd) } != 1 { return Ok(None); }
        let input_lease = crate::platform::terminal::InputLease::acquire()?;
        let mut original_mode = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(stdin_fd, original_mode.as_mut_ptr()) } != 0 { return Err(std::io::Error::last_os_error()); }
        let original_mode = unsafe { original_mode.assume_init() };
        let mut raw_mode = original_mode;
        if raw {
            // SAFETY: raw_mode is an initialized writable termios value.
            unsafe { libc::cfmakeraw(&mut raw_mode) };
        } else {
            raw_mode.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ECHONL);
            // Signals can invalidate readiness by flushing the input queue.
            raw_mode.c_cc[libc::VMIN] = 0;
            raw_mode.c_cc[libc::VTIME] = 0;
        }
        if unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &raw_mode) } != 0 { return Err(std::io::Error::last_os_error()); }
        Ok(Some(Self { stdin_fd, original_mode, _input_lease: input_lease }))
    }

    pub fn read_chunk(&self, timeout: std::time::Duration) -> std::io::Result<Option<PtyInputChunk>> {
        let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let mut pollfd = libc::pollfd { fd: self.stdin_fd, events: libc::POLLIN, revents: 0 };
        let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            return if error.kind() == std::io::ErrorKind::Interrupted { Ok(None) } else { Err(error) };
        }
        if ready == 0 || pollfd.revents & libc::POLLIN == 0 { return Ok(None); }
        let mut buffer = vec![0_u8; 65536];
        let count = unsafe { libc::read(self.stdin_fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if count <= 0 { return Ok(None); }
        buffer.truncate(count as usize);
        Ok(Some(PtyInputChunk { submit: buffer.iter().any(|byte| matches!(*byte, b'\r' | b'\n')), data: buffer }))
    }
}

#[cfg(feature = "terminal-input")]
impl Drop for TerminalInputSession {
    fn drop(&mut self) { unsafe { libc::tcsetattr(self.stdin_fd, libc::TCSANOW, &self.original_mode); } }
}

pub fn active_graphics_probe(
    timeout: std::time::Duration,
) -> crate::platform::terminal::TerminalGraphicsProbe {
    use std::fs::OpenOptions;
    use std::io::{Read as _, Write as _};
    use std::os::fd::AsRawFd as _;
    use std::time::Instant;

    let Ok(_input_lease) = crate::platform::terminal::InputLease::acquire() else {
        return crate::platform::terminal::TerminalGraphicsProbe::default();
    };
    let Ok(mut tty) = OpenOptions::new().read(true).write(true).open("/dev/tty") else {
        return crate::platform::terminal::TerminalGraphicsProbe::default();
    };
    let fd = tty.as_raw_fd();
    let mut old_termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    let have_termios = unsafe { libc::tcgetattr(fd, old_termios.as_mut_ptr()) == 0 };
    let old_termios = have_termios.then(|| unsafe { old_termios.assume_init() });
    if let Some(mut raw) = old_termios {
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        let _ = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };
    }
    let old_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if old_flags >= 0 {
        let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, old_flags | libc::O_NONBLOCK) };
    }

    let _ = tty.write_all(
        b"\x1b[c\x1b[?2;1;0S\x1b_Gi=running-process-probe,a=q;\x1b\\\x1b]1337;Capabilities\x07",
    );
    let _ = tty.flush();
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    while Instant::now() < deadline {
        let mut chunk = [0_u8; 512];
        match tty.read(&mut chunk) {
            Ok(0) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
    if old_flags >= 0 {
        let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, old_flags) };
    }
    if let Some(old) = old_termios {
        let _ = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &old) };
    }
    let reply = String::from_utf8_lossy(&bytes).into_owned();
    crate::platform::terminal::TerminalGraphicsProbe {
        sixel_xtsmgraphics: reply.contains('S').then(|| reply.clone()),
        sixel_da1: reply.contains("[?").then(|| reply.clone()),
        kitty_graphics: reply.contains("_G").then(|| reply.clone()),
        iterm2_capabilities: reply.contains("Capabilities=").then_some(reply),
    }
}
