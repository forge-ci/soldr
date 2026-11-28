#![cfg(all(unix, feature = "terminal-input"))]

use std::io;
use std::io::Write;
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn stdin_mode() -> libc::termios {
    let mut mode = std::mem::MaybeUninit::uninit();
    // SAFETY: mode is writable and stdin is a live PTY slave in the child.
    assert_eq!(unsafe { libc::tcgetattr(0, mode.as_mut_ptr()) }, 0);
    // SAFETY: successful tcgetattr initialized mode.
    unsafe { mode.assume_init() }
}

#[test]
#[expect(
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
fn native_session_rejects_overlap_and_restores_mode() {
    if std::env::var_os("KERNAL_INPUT_OWNERSHIP_CHILD").is_some() {
        let before = stdin_mode();
        assert_ne!(before.c_lflag & libc::ICANON, 0);
        let session = kernal_api::TerminalInputSession::new().unwrap().unwrap();
        assert_eq!(stdin_mode().c_lflag & libc::ICANON, 0);
        assert_eq!(
            kernal_api::TerminalInputSession::new()
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        // The probe must not steal input or change modes while capture owns it.
        let probe = kernal_api::platform::terminal::active_graphics_probe(Duration::ZERO);
        assert!(probe.kitty_graphics.is_none());
        assert_eq!(stdin_mode().c_lflag & libc::ICANON, 0);
        drop(session);
        let after = stdin_mode();
        // Restoration is judged against what the kernel itself reports once
        // the original mode is applied, not against the first read: a host
        // may add kernel-owned state on tcsetattr (Darwin sets PENDIN in
        // c_lflag, #347). Re-applying `before` directly isolates that from a
        // failed restore by the session.
        // SAFETY: stdin is this child's live PTY and before is initialized.
        let reapplied = unsafe { libc::tcsetattr(0, libc::TCSANOW, &before) };
        let reapply_error = io::Error::last_os_error();
        let kernel = stdin_mode();
        eprintln!(
            "termios before iflag={:#x} oflag={:#x} cflag={:#x} lflag={:#x}; \
             after iflag={:#x} oflag={:#x} cflag={:#x} lflag={:#x}; \
             reapplied rc={reapplied} ({reapply_error}) iflag={:#x} lflag={:#x}",
            before.c_iflag,
            before.c_oflag,
            before.c_cflag,
            before.c_lflag,
            after.c_iflag,
            after.c_oflag,
            after.c_cflag,
            after.c_lflag,
            kernel.c_iflag,
            kernel.c_lflag,
        );
        assert_eq!(reapplied, 0, "re-applying the original mode must succeed");
        assert_eq!(kernel.c_iflag, after.c_iflag);
        assert_eq!(kernel.c_oflag, after.c_oflag);
        assert_eq!(kernel.c_cflag, after.c_cflag);
        assert_eq!(kernel.c_lflag, after.c_lflag);
        assert_eq!(kernel.c_cc, after.c_cc);
        // Every documented input mode must survive exactly; only bits the
        // kernel itself refuses to round-trip may differ from the first read.
        let documented = libc::IGNBRK
            | libc::BRKINT
            | libc::IGNPAR
            | libc::PARMRK
            | libc::INPCK
            | libc::ISTRIP
            | libc::INLCR
            | libc::IGNCR
            | libc::ICRNL
            | libc::IXON
            | libc::IXOFF
            | libc::IXANY
            | libc::IMAXBEL
            | libc::IUTF8;
        assert_eq!(before.c_iflag & documented, after.c_iflag & documented);
        assert_eq!(before.c_oflag, after.c_oflag);
        assert_eq!(before.c_cflag, after.c_cflag);
        // PENDIN is kernel-owned state, not a mode: Darwin sets it when
        // canonical mode resumes with input still queued, as this PTY's
        // pre-written line is (#347). Every other local mode must survive.
        let modes = !libc::PENDIN;
        assert_eq!(before.c_lflag & modes, after.c_lflag & modes);
        assert_eq!(before.c_cc, after.c_cc);
        drop(kernal_api::TerminalInputSession::new().unwrap().unwrap());
        let mut keys = kernal_api::keys::TerminalKeys::new().unwrap().unwrap();
        let key_mode = stdin_mode();
        assert_eq!(before.c_lflag & libc::ISIG, key_mode.c_lflag & libc::ISIG);
        assert_eq!(before.c_oflag, key_mode.c_oflag);
        assert_eq!(before.c_iflag, key_mode.c_iflag);
        assert_eq!(key_mode.c_cc[libc::VMIN], 0);
        assert_eq!(key_mode.c_cc[libc::VTIME], 0);
        assert_eq!(
            key_mode.c_lflag & (libc::ICANON | libc::ECHO | libc::ECHONL),
            0
        );
        use kernal_api::keys::Key;
        for expected in [
            Key::Other,
            Key::Character(' '),
            Key::Character('é'),
            Key::Enter,
        ] {
            assert_eq!(keys.poll(Duration::ZERO).unwrap().unwrap().key, expected);
        }
        assert!(keys.poll(Duration::ZERO).unwrap().is_none());
        let mut empty = [0u8; 1];
        // SAFETY: stdin is this child's live PTY and empty is writable. This
        // read must return zero without poll, modeling readiness invalidation.
        assert_eq!(unsafe { libc::read(0, empty.as_mut_ptr().cast(), 1) }, 0);
        assert_eq!(
            keys.poll(Duration::from_secs(1)).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        drop(keys);
        assert_eq!(before.c_lflag & modes, stdin_mode().c_lflag & modes);
        return;
    }

    let mut master = -1;
    let mut slave = -1;
    // SAFETY: valid output pointers; null optional arguments request defaults.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // SAFETY: successful openpty returns two fresh owned descriptors.
    let mut master = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(master) });
    // This complete canonical line is queued before the child starts. Raw
    // capture uses TCSANOW, preserving bytes for zero-wait decoding tests.
    master.write_all(b"\x1b[12 z \xc3\xa9\r").unwrap();
    // SAFETY: slave is the other fresh descriptor, transferred to child stdin.
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "terminal_input_ownership::native_session_rejects_overlap_and_restores_mode",
            "--nocapture",
        ])
        .env("KERNAL_INPUT_OWNERSHIP_CHILD", "1")
        .stdin(Stdio::from(slave))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("terminal ownership child timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
