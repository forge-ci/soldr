//! Owned pseudo-terminal session facade.

#[cfg(not(windows))]
use crate::platform::terminal::PtyChild;
use crate::{
    platform::terminal::{PtyBackend, PtyMaster, PtySize, PtySlave},
    Backend,
};
use std::{
    ffi::OsString,
    io::{self, Read, Write},
    path::PathBuf,
    time::Duration,
};

/// Caller-selected process program and arguments for a native PTY session.
#[derive(Debug, Clone)]
pub struct PtyCommand {
    pub program: OsString,
    pub arguments: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub environment: Option<Vec<(OsString, OsString)>>,
}

impl PtyCommand {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            cwd: None,
            environment: None,
        }
    }
}

/// An owned child process and its pseudo-terminal.
pub struct PtySession {
    master: <Backend as PtyBackend>::Master,
    child: <<Backend as PtyBackend>::Slave as PtySlave>::Child,
    writer: Box<dyn Write + Send>,
}

impl PtySession {
    pub fn spawn(command: PtyCommand, size: PtySize) -> io::Result<(Self, Box<dyn Read + Send>)> {
        let (mut master, slave) = Backend::openpty(size)?;
        let reader = master.try_clone_reader()?;
        let writer = master.take_writer()?;
        let mut argv = Vec::with_capacity(command.arguments.len() + 1);
        argv.push(command.program);
        argv.extend(command.arguments);
        let child = slave.spawn(
            &argv,
            command.cwd.as_deref(),
            command.environment.as_deref(),
        )?;
        Ok((
            Self {
                master,
                child,
                writer,
            },
            reader,
        ))
    }

    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer
            .write_all(bytes)
            .and_then(|_| self.writer.flush())
    }

    /// Write as much of `bytes` as the terminal accepts before `timeout`.
    ///
    /// Returns the number of bytes written; `0` means the input queue stayed
    /// full for the whole timeout, so the caller regains control and can watch
    /// for something else — a disconnected transport, a shutdown — and stop.
    ///
    /// Prefer this over [`PtySession::write`] wherever the caller has anything
    /// to observe. `write` parks in the kernel until the queue drains, which is
    /// unbounded once the foreground program stops reading, and that state
    /// cannot be interrupted: not by cancelling the thread, and not by ending
    /// the process holding the other end of the terminal.
    ///
    /// Backends without a bounded write fall back to the blocking write, so the
    /// method is always usable; only the interruption guarantee is Unix-only.
    pub fn write_available(&mut self, bytes: &[u8], timeout: Duration) -> io::Result<usize> {
        match self.master.write_available(bytes, timeout) {
            Ok(written) => Ok(written),
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                self.write(bytes)?;
                Ok(bytes.len())
            }
            Err(error) => Err(error),
        }
    }
    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        self.master.resize(size)
    }

    /// The externally meaningful process identifier for this session's child,
    /// suitable for [`crate::platform::terminal::signal_pty_tree`].
    ///
    /// This is informational and control-oriented, not an escape from a blocked
    /// write. Signalling the process tree does **not** release a write parked on
    /// a full terminal input queue: the kernel keeps that write blocked even
    /// after every process holding the slave has been killed, so the parked
    /// thread returns only when something finally drains the queue. Escaping
    /// that state needs a non-blocking write on the master, not a signal.
    pub fn pid(&self) -> Option<u32> {
        self.master.preferred_pid(&self.child)
    }
    pub fn try_wait(&mut self) -> io::Result<Option<u32>> {
        self.child.try_wait()
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.master.kill_process_group();
        let _ = self.child.kill();
        let _ = self.child.wait();
        // The writer's own drop sends a newline and the terminal's EOF character
        // through the master. That write blocks while the input queue is full,
        // which hangs teardown for exactly the session that filled the queue —
        // the one whose client gave up mid-write and most needs releasing.
        // Fields drop after this body, so this is the last moment the master is
        // still ours to prepare.
        let _ = self.master.prepare_for_teardown();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Spawn a child that has stopped reading its terminal, and wait until it has.
    ///
    /// The queue only stops accepting input once the child reaches its
    /// non-reading phase in raw mode. Before `stty` runs, the line discipline is
    /// still cooked, where input is consumed rather than queued and the queue
    /// never fills — so filling on a timer races the child's startup and
    /// silently leaves room. Waiting for the marker removes the race.
    fn spawn_non_reading_session() -> PtySession {
        let mut command = PtyCommand::new("/bin/sh");
        command.arguments = vec![
            "-c".into(),
            "stty raw -echo; printf READY; exec sleep 30".into(),
        ];
        let (session, reader) = PtySession::spawn(
            command,
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .expect("spawn a child that stops reading its terminal");

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut seen: Vec<u8> = Vec::new();
            let mut buffer = [0_u8; 64];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        seen.extend_from_slice(&buffer[..count]);
                        if seen.windows(5).any(|window| window == b"READY") {
                            break;
                        }
                    }
                }
            }
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the child never reported terminal readiness");
        session
    }

    /// Fill the terminal's input queue until it refuses more.
    fn fill_terminal_queue(session: &mut PtySession) {
        let chunk = vec![b'x'; 4096];
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            match session.write_available(&chunk, Duration::from_millis(200)) {
                Ok(0) => return,
                Ok(_) => {}
                Err(error) => panic!("bounded write while filling: {error}"),
            }
        }
        panic!("the terminal input queue never filled");
    }

    #[test]
    fn session_owns_a_shell_command_and_its_pty_io() {
        let mut command = PtyCommand::new("/bin/sh");
        command.arguments = vec!["-c".into(), "printf kernal-pty-session".into()];
        let (mut session, mut reader) = PtySession::spawn(
            command,
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .expect("spawn shell in PTY");

        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 64];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => bytes.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                // Linux PTYs report EIO when the slave closes normally.
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) => panic!("read PTY output: {error}"),
            }
        }
        assert!(String::from_utf8_lossy(&bytes).contains("kernal-pty-session"));

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = session.try_wait().expect("reap shell") {
                assert_eq!(status, 0);
                break;
            }
            assert!(Instant::now() < deadline, "shell did not exit promptly");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Dropping a session whose input queue is full must return.
    ///
    /// The writer's own drop sends a newline and the terminal's EOF character
    /// through the master. That write blocks while the queue is full, so
    /// teardown hangs for exactly the session that filled it — the one whose
    /// client gave up mid-write and most needs releasing. The drop runs on its
    /// own thread so a regression fails the test instead of hanging it.
    #[cfg(unix)]
    #[test]
    fn teardown_does_not_hang_on_a_full_input_queue() {
        let mut session = spawn_non_reading_session();

        fill_terminal_queue(&mut session);

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(session);
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "dropping a session with a full input queue did not return"
        );
    }

    /// A bounded write gives up rather than parking, which is the whole reason
    /// it exists: it is the only way a caller with something else to watch ever
    /// regains control from a terminal nobody is reading.
    #[cfg(unix)]
    #[test]
    fn bounded_write_returns_instead_of_parking_on_a_full_queue() {
        let mut session = spawn_non_reading_session();

        // The queue is full once a write is refused, which is the state under
        // test. A blocking write would park here forever; this one returns.
        fill_terminal_queue(&mut session);
        let chunk = vec![b'x'; 4096];

        let started = Instant::now();
        let written = session
            .write_available(&chunk, Duration::from_millis(200))
            .expect("bounded write against a full queue");
        let elapsed = started.elapsed();
        assert_eq!(written, 0, "a full queue should accept nothing");
        assert!(
            elapsed < Duration::from_secs(2),
            "bounded write did not respect its timeout: {elapsed:?}"
        );
    }

    /// Signalling the reported pid must end the child, not merely identify it.
    ///
    /// This covers what the pid accessor actually promises. It deliberately does
    /// not cover releasing a parked write: killing the tree does not do that,
    /// and asserting otherwise would encode a false guarantee.
    #[cfg(unix)]
    #[test]
    fn reported_pid_terminates_the_session() {
        let mut command = PtyCommand::new("/bin/sh");
        command.arguments = vec!["-c".into(), "sleep 30".into()];
        let (mut session, _reader) = PtySession::spawn(
            command,
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .expect("spawn a long-lived child in a PTY");

        let pid = session
            .pid()
            .expect("a spawned session reports its child pid");
        assert!(pid > 0, "pid must be a real identifier, got {pid}");
        assert!(
            session.try_wait().expect("poll child").is_none(),
            "the child should still be running before it is signalled"
        );

        crate::platform::terminal::signal_pty_tree(pid, true).expect("signal the session tree");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if session.try_wait().expect("reap signalled child").is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "signalling the reported pid did not end the session"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
