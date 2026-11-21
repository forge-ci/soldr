//! Unit coverage split from `client.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

#[test]
fn shutdown_compat_starts_with_the_immediately_previous_protocol() {
    assert_eq!(
        SHUTDOWN_COMPAT_PROTOCOL_VERSIONS.first().copied(),
        Some(crate::daemon::protocol::PROTOCOL_VERSION - 1)
    );
}

#[test]
fn reply_timeout_defaults_to_30_min() {
    // Unset / empty / non-numeric / zero all fall back to the generous
    // default so a legitimate slow release compile is never cut off.
    let default = Duration::from_secs(DEFAULT_REPLY_TIMEOUT_SECS);
    assert_eq!(parse_reply_timeout(None), default);
    assert_eq!(parse_reply_timeout(Some("")), default);
    assert_eq!(parse_reply_timeout(Some("nope")), default);
    assert_eq!(parse_reply_timeout(Some("0")), default);
}

#[test]
fn reply_timeout_env_override_fails_fast() {
    // #1364: an operator can opt into a short fail-fast budget.
    assert_eq!(parse_reply_timeout(Some("30")), Duration::from_secs(30));
    assert_eq!(parse_reply_timeout(Some("  5 ")), Duration::from_secs(5));
}

// ---- soldr#2844 follow-up: the missing-ack line must be sayable -------------

/// The diagnostic names the request kind and the reason.
///
/// soldr#2844 shipped this through `tracing::warn!`. The wrapper process
/// installs no subscriber -- only the daemon does -- so the warning was
/// emitted into nothing, which the integration test could not catch because it
/// asserts on the *call's* result and a silent warning changes neither.
#[test]
fn the_missing_ack_line_names_the_request_and_the_reason() {
    let request = Request::RecordTargetTouch {
        path: "/work/target".to_string(),
        unix_seconds: 1_700_000_000,
    };
    let line = missing_ack_message(&request, "no ack within 200ms");

    // Prefixed like every other client-side line, so it is greppable with them.
    assert!(line.starts_with("soldr: "), "{line}");
    assert!(line.contains("RecordTargetTouch"), "{line}");
    assert!(line.contains("no ack within 200ms"), "{line}");
    assert!(line.contains("delivery is unconfirmed"), "{line}");
    assert!(line.contains("soldr#2785"), "{line}");
    // One line: a multi-line diagnostic interleaves with concurrent compiler
    // output on the wrapper hot path.
    assert!(!line.contains('\n'), "{line}");
}

#[test]
fn the_missing_ack_line_does_not_carry_the_target_path() {
    let request = Request::RecordTargetTouch {
        path: "/some/very/long/workspace/target/directory".to_string(),
        unix_seconds: 1_700_000_000,
    };
    let line = missing_ack_message(&request, "reset");
    assert!(
        !line.contains("/some/very/long/workspace"),
        "the path is what the reader already knows and what makes it long: {line}"
    );
}

#[test]
fn a_cook_touch_is_named_distinctly() {
    let line = missing_ack_message(&Request::CookTouch { sha256: [0u8; 32] }, "reset");
    assert!(line.contains("CookTouch"), "{line}");
}

// ---- soldr#2955: a missing ack stays best-effort ----------------------------

/// A peer that takes the frame and answers nothing.
///
/// Reads report EOF, which is what a daemon that accepted the connection and
/// then went away looks like from the client side: no drop logged, no upsert
/// failure, no ack.
struct SilentPeer {
    written: usize,
}

impl Read for SilentPeer {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(0)
    }
}

impl Write for SilentPeer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// soldr#2558 bounded the ack wait rather than requiring it, and soldr#2785
/// stopped discarding its outcome. Those two must not collide: observing a
/// missing ack must not quietly promote it to a failure, or every pre-#2558
/// daemon becomes a hard error on the wrapper hot path.
#[test]
fn a_peer_that_never_acks_is_still_a_successful_submit() {
    let mut peer = SilentPeer { written: 0 };

    let result = write_awaiting_receipt_ack(
        &mut peer,
        &Request::RecordTargetTouch {
            path: "/some/workspace/target".to_string(),
            unix_seconds: 1_700_000_000,
        },
    );

    assert!(
        matches!(result, Ok(ReceiptAck::Unconfirmed(_))),
        "a missing ack must be reported as unconfirmed, not become an error: {result:?}"
    );
    assert!(
        peer.written > 0,
        "the request frame should still have been written to the peer"
    );
}

struct ScriptedControlStream {
    replies: Cursor<Vec<u8>>,
    writes: Arc<Mutex<Vec<u8>>>,
    dropped: Arc<AtomicBool>,
}

impl Read for ScriptedControlStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.replies.read(buf)
    }
}

impl Write for ScriptedControlStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writes
            .lock()
            .expect("writes lock")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for ScriptedControlStream {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

fn scripted_resident_peer() -> (BoxedControlStream, Arc<Mutex<Vec<u8>>>, Arc<AtomicBool>) {
    let mut replies = Vec::new();
    write_frame_sync(
        &mut replies,
        &Response::ResidentCapacityAcquired { permits: 2 },
    )
    .expect("encode acquired reply");
    write_frame_sync(&mut replies, &Response::Ack).expect("encode release ack");
    let writes = Arc::new(Mutex::new(Vec::new()));
    let dropped = Arc::new(AtomicBool::new(false));
    (
        Box::new(ScriptedControlStream {
            replies: Cursor::new(replies),
            writes: writes.clone(),
            dropped: dropped.clone(),
        }),
        writes,
        dropped,
    )
}

#[test]
fn resident_capacity_lease_retains_stream_until_explicit_finish() {
    let (stream, writes, dropped) = scripted_resident_peer();
    let lease = acquire_resident_capacity_on_stream(stream, 2).expect("acquire lease");
    assert!(!dropped.load(Ordering::SeqCst));

    lease.finish().expect("release lease");
    assert!(dropped.load(Ordering::SeqCst));

    let mut sent = Cursor::new(writes.lock().expect("writes lock").clone());
    assert!(matches!(
        read_frame_sync::<_, Request>(&mut sent).expect("acquire request"),
        Request::AcquireResidentCapacity { permits: 2 }
    ));
    assert!(matches!(
        read_frame_sync::<_, Request>(&mut sent).expect("release request"),
        Request::ReleaseResidentCapacity
    ));
}

#[test]
fn dropping_resident_capacity_lease_disconnects_without_a_release_frame() {
    let (stream, writes, dropped) = scripted_resident_peer();
    let lease = acquire_resident_capacity_on_stream(stream, 2).expect("acquire lease");
    drop(lease);
    assert!(dropped.load(Ordering::SeqCst));

    let mut sent = Cursor::new(writes.lock().expect("writes lock").clone());
    assert!(matches!(
        read_frame_sync::<_, Request>(&mut sent).expect("acquire request"),
        Request::AcquireResidentCapacity { permits: 2 }
    ));
    assert!(read_frame_sync::<_, Request>(&mut sent).is_err());
}

#[test]
fn receipt_retries_eagain_after_partial_frame_and_validates_ack() {
    struct RetryPeer {
        reply: Cursor<Vec<u8>>,
        step: usize,
    }
    impl Read for RetryPeer {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.step += 1;
            if self.step == 2 {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            let n = buf.len().min(2);
            self.reply.read(&mut buf[..n])
        }
    }
    impl Write for RetryPeer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut bytes = Vec::new();
    write_frame_sync(&mut bytes, &Response::Ack).unwrap();
    let mut peer = RetryPeer {
        reply: Cursor::new(bytes),
        step: 0,
    };
    let result = write_awaiting_receipt_ack(
        &mut peer,
        &Request::RecordTargetTouch {
            path: "target".into(),
            unix_seconds: 1,
        },
    );
    assert_eq!(result.unwrap(), ReceiptAck::Acknowledged);
}
#[test]
fn receipt_retry_has_a_deadline() {
    let result: std::io::Result<()> = retry_receipt(std::time::Instant::now(), || {
        Err(std::io::ErrorKind::WouldBlock.into())
    });
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}
