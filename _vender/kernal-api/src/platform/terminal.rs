//! Terminal, PTY, console, input, and terminal-I/O primitives.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

static INPUT_OWNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(any(windows, all(test, feature = "terminal-input")))]
pub(crate) struct InputQueue<T> {
    events: std::collections::VecDeque<(T, usize)>,
    bytes: usize,
}

#[cfg(any(windows, all(test, feature = "terminal-input")))]
impl<T> InputQueue<T> {
    pub(crate) fn new() -> Self {
        Self {
            events: std::collections::VecDeque::new(),
            bytes: 0,
        }
    }

    pub(crate) fn push(&mut self, event: T, bytes: usize) -> bool {
        if self.events.len() >= 256 || bytes > 65_536 - self.bytes {
            return false;
        }
        self.events.push_back((event, bytes));
        self.bytes += bytes;
        true
    }

    pub(crate) fn pop_front(&mut self) -> Option<T> {
        self.events.pop_front().map(|(event, bytes)| {
            self.bytes -= bytes;
            event
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.events.clear();
        self.bytes = 0;
    }

    pub(crate) fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.bytes = 0;
        self.events.drain(..).map(|(event, _)| event)
    }
}

/// Admission shared by this kernel instance's stdin capture and tty probes.
/// It cannot coordinate unrelated libraries or another process reading the tty.
pub(crate) struct InputLease(());

impl InputLease {
    pub(crate) fn acquire() -> io::Result<Self> {
        INPUT_OWNED
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::Acquire,
                std::sync::atomic::Ordering::Relaxed,
            )
            .map(|_| Self(()))
            .map_err(|_| {
                io::Error::new(io::ErrorKind::WouldBlock, "terminal input is already owned")
            })
    }
}

impl Drop for InputLease {
    fn drop(&mut self) {
        INPUT_OWNED.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Caller-facing PTY dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

/// One chunk read from the host terminal input source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyInputChunk {
    pub data: Vec<u8>,
    pub submit: bool,
}

/// Independently lockable writer used by the PTY session policy layer.
pub type SharedPtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

type PtyInterruptOperation = Box<dyn FnOnce(&SharedPtyWriter) -> io::Result<bool> + Send + 'static>;

/// Prepared, owned interrupt operation that can outlive a borrow of the PTY master.
///
/// Native descriptors and process-group identifiers remain captured inside the
/// selected concrete implementation rather than crossing this facade.
pub struct PtyInterruptTarget(PtyInterruptOperation);

impl PtyInterruptTarget {
    #[cfg(feature = "pty")]
    pub(crate) fn new(
        send: impl FnOnce(&SharedPtyWriter) -> io::Result<bool> + Send + 'static,
    ) -> Self {
        Self(Box::new(send))
    }

    pub fn send(self, writer: &SharedPtyWriter) -> io::Result<bool> {
        (self.0)(writer)
    }
}

/// Platform-neutral handle for the master side of a pseudo-terminal.
pub trait PtyMaster: Send + 'static {
    fn try_clone_reader(&mut self) -> io::Result<Box<dyn Read + Send>>;
    fn take_writer(&mut self) -> io::Result<Box<dyn Write + Send>>;
    fn resize(&self, size: PtySize) -> io::Result<()>;
    fn get_size(&self) -> io::Result<PtySize>;

    /// Legacy Unix process-group accessor retained for source compatibility.
    ///
    /// Platform mechanics do not consume this value; use facade operations for
    /// control. This method will be removed in the next major release.
    #[deprecated(note = "use facade PTY control operations; removal planned for 5.0")]
    fn process_group_leader(&self) -> Option<i32> {
        None
    }

    /// Legacy Unix descriptor accessor retained for source compatibility.
    ///
    /// The primitive representation avoids a host-native type in the neutral
    /// signature, and shared callers must not use it for platform mechanics.
    #[deprecated(note = "use facade PTY operations; removal planned for 5.0")]
    fn as_raw_fd(&self) -> Option<i32> {
        None
    }

    /// Prepare an owned interrupt operation using the selected host's PTY mechanics.
    #[cfg(feature = "pty")]
    fn interrupt_target(&self) -> io::Result<PtyInterruptTarget> {
        Ok(PtyInterruptTarget::new(|writer| {
            let mut writer = writer
                .lock()
                .map_err(|_| io::Error::other("pty writer mutex poisoned"))?;
            writer.write_all(&[0x03])?;
            writer.flush()?;
            Ok(true)
        }))
    }

    /// Kill the selected host's PTY process group, when one exists.
    #[cfg(feature = "pty")]
    fn kill_process_group(&self) -> io::Result<()> {
        Ok(())
    }

    /// Prepare the master for the writes that teardown performs.
    ///
    /// Dropping the session's writer sends a newline and the terminal's EOF
    /// character through the master. On a terminal whose input queue is full
    /// that write does not return, so teardown would hang for exactly the
    /// session that filled the queue and then lost its client. Backends make
    /// that write give up instead of waiting.
    #[cfg(feature = "pty")]
    fn prepare_for_teardown(&self) -> io::Result<()> {
        Ok(())
    }

    /// Write as much of `bytes` as the terminal accepts within `timeout`.
    ///
    /// Returns the number of bytes written; `0` means the input queue stayed
    /// full for the whole timeout. A caller that has to observe something
    /// other than the terminal — a disconnected client, a shutdown — uses this
    /// instead of [`std::io::Write::write_all`], which parks until the queue
    /// drains and cannot be interrupted: not by a signal, and not by ending the
    /// process holding the other end.
    ///
    /// Backends without a bounded write report
    /// [`std::io::ErrorKind::Unsupported`] rather than pretending to honour the
    /// timeout by blocking through it.
    #[cfg(feature = "pty")]
    fn write_available(&self, _bytes: &[u8], _timeout: std::time::Duration) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "bounded PTY writes are not implemented for this backend",
        ))
    }

    /// Select the externally meaningful PID for this PTY.
    #[cfg(feature = "pty")]
    fn preferred_pid(&self, child: &dyn PtyChild) -> Option<u32> {
        Some(child.pid())
    }
}

/// Platform-neutral handle for a child process running inside a PTY.
pub trait PtyChild: Send + 'static {
    fn pid(&self) -> u32;
    fn try_wait(&mut self) -> io::Result<Option<u32>>;
    fn wait(&mut self) -> io::Result<u32>;
    fn kill(&mut self) -> io::Result<()>;

    /// Legacy Windows process-handle accessor retained for source compatibility.
    ///
    /// Platform mechanics do not consume this value. The facade-owned process
    /// preparation operation keeps native handles inside the concrete tree.
    #[deprecated(note = "use facade PTY operations; removal planned for 5.0")]
    fn as_raw_handle(&self) -> Option<*mut std::ffi::c_void> {
        None
    }

    /// Apply selected-host containment and priority mechanics after spawn.
    #[cfg(feature = "pty")]
    fn prepare_process(
        &self,
        context: PtySpawnContext,
        nice: Option<i32>,
    ) -> io::Result<PtyProcessGuard> {
        crate::prepare_unmanaged_pty_child(context, nice)
    }
}

/// Platform-neutral handle for the slave side of a pseudo-terminal.
pub trait PtySlave: Send + 'static {
    type Child: PtyChild;

    fn spawn(
        self,
        argv: &[OsString],
        cwd: Option<&Path>,
        env: Option<&[(OsString, OsString)]>,
    ) -> io::Result<Self::Child>;
}

/// Factory trait implemented by the selected host PTY backend.
pub trait PtyBackend {
    type Master: PtyMaster;
    type Slave: PtySlave;

    fn openpty(size: PtySize) -> io::Result<(Self::Master, Self::Slave)>;
}

/// Raw replies returned by a bounded active terminal-graphics probe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalGraphicsProbe {
    pub sixel_xtsmgraphics: Option<String>,
    pub sixel_da1: Option<String>,
    pub kitty_graphics: Option<String>,
    pub iterm2_capabilities: Option<String>,
}

/// Probe the controlling terminal without exposing terminal descriptors.
pub fn active_graphics_probe(timeout: std::time::Duration) -> TerminalGraphicsProbe {
    crate::active_graphics_probe(timeout)
}

pub mod input {
    pub use crate::terminal_input::*;
}

#[cfg(feature = "pty")]
pub use crate::{
    Backend, ChildProcessInfo, ConPtyBackendKind, OrphanConhostInfo, PtyProcessGuard,
    PtySpawnContext,
};

#[cfg(feature = "terminal-input")]
pub use crate::TerminalInputSession;

#[cfg(feature = "pty")]
pub use crate::current_backend_kind;

#[cfg(feature = "pty")]
pub fn before_pty_spawn() -> PtySpawnContext {
    crate::before_pty_spawn()
}

#[cfg(feature = "pty")]
pub fn prepare_pty_child(
    context: PtySpawnContext,
    child: &dyn PtyChild,
    nice: Option<i32>,
) -> io::Result<PtyProcessGuard> {
    child.prepare_process(context, nice)
}

#[cfg(feature = "pty")]
pub fn input_payload(data: &[u8]) -> Vec<u8> {
    crate::input_payload(data)
}

#[cfg(feature = "pty")]
pub fn query_responses(data: &[u8]) -> Vec<Vec<u8>> {
    crate::query_responses(data)
}

#[cfg(feature = "pty")]
pub fn shell_argv(command: &str) -> Vec<String> {
    crate::shell_argv(command)
}

#[cfg(feature = "pty")]
pub fn wait_before_close_supported() -> bool {
    crate::wait_before_pty_close_supported()
}

#[cfg(feature = "pty")]
pub fn is_ignorable_process_control_error(error: &io::Error) -> bool {
    crate::is_ignorable_process_control_error(error)
}

#[cfg(feature = "pty")]
pub fn send_pty_interrupt(
    target: PtyInterruptTarget,
    writer: &SharedPtyWriter,
) -> io::Result<bool> {
    target.send(writer)
}

#[cfg(feature = "pty")]
pub fn kill_pty_process_group(master: &dyn PtyMaster) -> io::Result<()> {
    master.kill_process_group()
}

#[cfg(feature = "pty")]
pub fn terminate_pty_child(pid: u32) -> io::Result<bool> {
    crate::terminate_pty_child(pid)
}

#[cfg(feature = "pty")]
pub fn signal_pty_tree(pid: u32, force: bool) -> io::Result<bool> {
    crate::signal_pty_tree(pid, force)
}

#[cfg(feature = "pty")]
pub fn resize_pty(master: &dyn PtyMaster, size: PtySize) -> io::Result<()> {
    crate::resize_pty(master, size)
}

#[cfg(feature = "pty")]
pub fn preferred_pty_pid(master: &dyn PtyMaster, child: &dyn PtyChild) -> Option<u32> {
    master.preferred_pid(child)
}

#[cfg(feature = "pty")]
pub fn find_child_processes(parent_pid: u32) -> Vec<ChildProcessInfo> {
    crate::find_child_processes(parent_pid)
}

#[cfg(feature = "pty")]
pub fn find_orphan_conhosts() -> Vec<OrphanConhostInfo> {
    crate::find_orphan_conhosts()
}

#[cfg(all(test, feature = "terminal-input"))]
mod ownership_tests {
    #[test]
    fn capture_queue_enforces_event_and_byte_limits() {
        let mut queue = super::InputQueue::new();
        for value in 0..256 {
            assert!(queue.push(value, 1));
        }
        assert!(!queue.push(999, 1));
        assert_eq!(queue.pop_front(), Some(0));
        assert!(queue.push(256, 1));
        queue.clear();
        assert!(queue.push(1, 65_536));
        assert!(!queue.push(2, 1));
        assert_eq!(queue.pop_front(), Some(1));
        assert!(!queue.push(3, 65_537));
        assert!(queue.is_empty());
        assert!(queue.push(4, 1));
        assert_eq!(queue.drain().collect::<Vec<_>>(), [4]);
        assert!(queue.push(5, 65_536));
    }

    #[test]
    fn input_ownership_rejects_overlap_and_releases_on_drop() {
        let first = super::InputLease::acquire().unwrap();
        assert_eq!(
            super::InputLease::acquire().err().unwrap().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(first);
        let next = super::InputLease::acquire().unwrap();
        drop(next);
    }
}
