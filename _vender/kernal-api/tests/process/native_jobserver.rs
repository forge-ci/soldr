//! The host-owned GNU make jobserver: capability, construction, and priming.

use kernal_api::platform::process::{native_jobserver_supported, NativeJobserver};

#[test]
fn zero_capacity_is_invalid_on_every_host() {
    let error = NativeJobserver::create(0).expect_err("zero tokens is not a jobserver");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn construction_matches_the_reported_capability() {
    match NativeJobserver::create(4) {
        Ok(jobserver) => {
            assert!(native_jobserver_supported());
            let auth = jobserver.auth_string();
            let parts: Vec<&str> = auth.split(',').collect();
            assert_eq!(parts.len(), 2, "auth string is R,W: {auth:?}");
            assert!(parts.iter().all(|part| part.parse::<i32>().is_ok()));
        }
        Err(error) => {
            assert!(!native_jobserver_supported());
            assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        }
    }
}

/// The pipe is primed with exactly `capacity` tokens, and both ends are
/// close-on-exec so they reach a child only when a launcher hands them over.
#[cfg(unix)]
#[test]
fn unix_pipe_holds_exactly_capacity_tokens_and_is_close_on_exec() {
    let jobserver = NativeJobserver::create(3).expect("unix jobserver");
    let auth = jobserver.auth_string();
    let (read, write) = auth.split_once(',').expect("R,W");
    let (read, write): (i32, i32) = (read.parse().unwrap(), write.parse().unwrap());
    for fd in [read, write] {
        // SAFETY: `fd` is owned by `jobserver`, which outlives this query.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0, "fd {fd} is close-on-exec");
    }
    // SAFETY: `read` stays open for the duration of the call.
    let pending = {
        let mut available: libc::c_int = 0;
        let rc = unsafe { libc::ioctl(read, libc::FIONREAD, &mut available) };
        assert_eq!(rc, 0, "FIONREAD");
        available
    };
    assert_eq!(pending, 3, "one '+' token per unit of capacity");
    let mut tokens = [0_u8; 3];
    // SAFETY: `tokens` is writable for its length and `read` is open.
    let got = unsafe { libc::read(read, tokens.as_mut_ptr().cast(), tokens.len()) };
    assert_eq!(got, 3);
    assert_eq!(&tokens, b"+++");
}
