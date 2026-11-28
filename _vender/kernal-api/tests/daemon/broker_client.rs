#![cfg(feature = "broker-client")]

//! Public contract for the facade-owned broker client adapter.

use std::error::Error;

use kernal_api::broker_client::{
    connect_backend, BackendRequest, BrokerClientError, BrokerRefusal, RefusalCode, RefusalKind,
    DISABLE_ENV, DISABLE_VALUE, FAKE_BACKEND_ENV,
};

const ALL_CODES: [(i32, RefusalCode); 10] = [
    (0, RefusalCode::Unspecified),
    (1, RefusalCode::VersionUnsupported),
    (2, RefusalCode::ServiceUnknown),
    (3, RefusalCode::BackendSpawnFailed),
    (4, RefusalCode::RateLimited),
    (5, RefusalCode::ShuttingDown),
    (6, RefusalCode::PeerRejected),
    (7, RefusalCode::Internal),
    (8, RefusalCode::VersionBlocked),
    (9, RefusalCode::FdPressure),
];

#[test]
fn refusal_codes_round_trip_the_frozen_wire_values() {
    for (wire, code) in ALL_CODES {
        assert_eq!(RefusalCode::from_wire(wire), code);
        assert_eq!(RefusalCode::from(wire), code);
        assert_eq!(i32::from(code), wire);
    }
    // A code newer than this build is preserved, never rejected.
    assert_eq!(RefusalCode::from_wire(999), RefusalCode::Unrecognized(999));
    assert_eq!(RefusalCode::Unrecognized(999).to_wire(), 999);
    assert_eq!(RefusalCode::from_wire(-1).to_wire(), -1);
}

#[test]
fn refusal_kinds_classify_actionable_codes_and_fold_the_rest_into_other() {
    let expected = |code| match code {
        RefusalCode::VersionUnsupported => RefusalKind::VersionUnsupported,
        RefusalCode::VersionBlocked => RefusalKind::VersionBlocked,
        RefusalCode::ServiceUnknown => RefusalKind::ServiceUnknown,
        RefusalCode::RateLimited => RefusalKind::RateLimited,
        RefusalCode::ShuttingDown => RefusalKind::ShuttingDown,
        other => RefusalKind::Other(other),
    };
    for (_, code) in ALL_CODES {
        assert_eq!(RefusalKind::from_code(code), expected(code));
    }
    assert_eq!(
        RefusalKind::from_code(RefusalCode::from_wire(999)),
        RefusalKind::Other(RefusalCode::Unrecognized(999))
    );
}

#[test]
fn refused_errors_carry_code_reason_and_retry_hint() {
    let error = BrokerClientError::refused(RefusalCode::RateLimited, "slow down", 2500);
    assert_eq!(error.refusal_kind(), Some(RefusalKind::RateLimited));
    let refusal = error.refusal().expect("refused error exposes its refusal");
    assert_eq!(refusal.code(), RefusalCode::RateLimited);
    assert_eq!(refusal.reason(), "slow down");
    assert_eq!(refusal.retry_after_ms(), 2500);
    assert_eq!(
        refusal,
        &BrokerRefusal::new(RefusalCode::RateLimited, "slow down", 2500)
    );
    assert!(error.to_string().contains("slow down"));
    assert!(error.source().is_none());

    assert_eq!(BrokerClientError::Disabled.refusal_kind(), None);
    assert!(BrokerClientError::Disabled
        .to_string()
        .contains(DISABLE_ENV));
}

#[test]
fn environment_contract_names_are_the_canonical_spellings() {
    assert_eq!(DISABLE_ENV, "RUNNING_PROCESS_DISABLE");
    assert_eq!(DISABLE_VALUE, "1");
    assert_eq!(FAKE_BACKEND_ENV, "RUNNING_PROCESS_FAKE_BACKEND");
}

#[cfg(unix)]
fn dead_endpoint(directory: &tempfile::TempDir) -> String {
    directory
        .path()
        .join("absent-broker.sock")
        .to_string_lossy()
        .into_owned()
}

#[cfg(windows)]
fn dead_endpoint(_directory: &tempfile::TempDir) -> String {
    format!("kernal-api-broker-client-absent-{}", std::process::id())
}

#[test]
fn unreachable_broker_is_a_transport_failure_not_a_refusal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let broker = dead_endpoint(&directory);
    let request = BackendRequest::new(&broker, "kernal-api-test", "1.0.0", "1.0.0");
    let error = connect_backend(&request).expect_err("no broker listens there");
    match &error {
        BrokerClientError::BrokerConnect { endpoint, .. } => assert_eq!(endpoint, &broker),
        other => panic!("expected BrokerConnect, got {other:?}"),
    }
    assert_eq!(error.refusal_kind(), None);
    assert!(error.source().is_some());
}

#[cfg(unix)]
mod unix_wire {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    use super::*;
    use kernal_api::broker_client::BackendRoute;

    fn varint(out: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            out.push((value as u8) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    fn varint_field(out: &mut Vec<u8>, field: u64, value: u64) {
        varint(out, field << 3);
        varint(out, value);
    }

    fn bytes_field(out: &mut Vec<u8>, field: u64, value: &[u8]) {
        varint(out, (field << 3) | 2);
        varint(out, value.len() as u64);
        out.extend_from_slice(value);
    }

    /// A frozen v1 `Frame { envelope_version: 1, kind: RESPONSE,
    /// payload_protocol: CONTROL, payload: HelloReply { refused } }`, framed.
    fn refused_hello_reply(code: i32, reason: &str, retry_after_ms: u64) -> Vec<u8> {
        let mut refused = Vec::new();
        bytes_field(&mut refused, 1, reason.as_bytes());
        varint_field(&mut refused, 4, code as u64);
        varint_field(&mut refused, 6, retry_after_ms);
        let mut reply = Vec::new();
        bytes_field(&mut reply, 2, &refused);
        let mut frame = Vec::new();
        varint_field(&mut frame, 1, 1);
        varint_field(&mut frame, 2, 1);
        bytes_field(&mut frame, 4, &reply);
        varint_field(&mut frame, 5, 1);
        let mut framed = vec![1];
        framed.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        framed.extend_from_slice(&frame);
        framed
    }

    fn read_one_frame(stream: &mut impl Read) {
        let mut header = [0_u8; 5];
        stream.read_exact(&mut header).expect("frame header");
        assert_eq!(header[0], 1, "frozen v1 framing byte");
        let length = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
        let mut body = vec![0_u8; length as usize];
        stream.read_exact(&mut body).expect("frame body");
    }

    /// A broker that declines over the real frozen wire is classified with its
    /// code, reason, and retry hint intact.
    #[test]
    fn broker_refusal_on_the_wire_is_classified() {
        let directory = tempfile::tempdir().expect("tempdir");
        let broker = directory.path().join("broker.sock");
        let listener = UnixListener::bind(&broker).expect("bind fake broker");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept Hello");
            read_one_frame(&mut stream);
            stream
                .write_all(&refused_hello_reply(4, "slow down", 7777))
                .expect("write refusal");
        });

        let request = BackendRequest::new(
            broker.to_string_lossy(),
            "kernal-api-test",
            "2.0.0",
            "1.0.0",
        )
        .client_library("kernal-api-test", "0.0.1");
        let error = connect_backend(&request).expect_err("broker refuses");
        server.join().expect("fake broker");
        assert_eq!(error.refusal_kind(), Some(RefusalKind::RateLimited));
        let refusal = error.refusal().expect("refusal");
        assert_eq!(refusal.code(), RefusalCode::RateLimited);
        assert_eq!(refusal.reason(), "slow down");
        assert_eq!(refusal.retry_after_ms(), 7777);
    }

    /// A cached endpoint with matching versions skips Hello entirely and hands
    /// back an ordinary byte stream to the backend.
    #[test]
    fn cached_backend_endpoint_skips_hello_and_streams_bytes() {
        let directory = tempfile::tempdir().expect("tempdir");
        let backend = directory.path().join("backend.sock");
        let listener = UnixListener::bind(&backend).expect("bind backend");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0_u8; 4];
            stream.read_exact(&mut request).expect("read ping");
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").expect("write pong");
        });

        let backend_endpoint = backend.to_string_lossy().into_owned();
        let request = BackendRequest::new(
            dead_endpoint(&directory),
            "kernal-api-test",
            "1.0.0",
            "1.0.0",
        )
        .cached_backend_endpoint(&backend_endpoint);
        let mut connection = connect_backend(&request).expect("cached backend connects");
        assert_eq!(connection.route(), BackendRoute::HelloSkip);
        assert_eq!(connection.endpoint(), backend_endpoint);
        connection.write_all(b"ping").expect("write ping");
        connection.flush().expect("flush");
        let mut reply = [0_u8; 4];
        connection.read_exact(&mut reply).expect("read pong");
        assert_eq!(&reply, b"pong");
        server.join().expect("backend");
    }

    /// A backend that accepts and then stays silent cannot hold a reader
    /// past its read timeout.
    #[test]
    fn a_read_timeout_bounds_a_silent_backend() {
        let directory = tempfile::tempdir().expect("tempdir");
        let backend = directory.path().join("silent.sock");
        let listener = UnixListener::bind(&backend).expect("bind backend");
        let (release, released) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().expect("accept");
            released.recv().ok();
        });

        let backend_endpoint = backend.to_string_lossy().into_owned();
        let request = BackendRequest::new(
            dead_endpoint(&directory),
            "kernal-api-test",
            "1.0.0",
            "1.0.0",
        )
        .cached_backend_endpoint(&backend_endpoint);
        let mut connection = connect_backend(&request).expect("cached backend connects");
        connection
            .set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .expect("set read timeout");
        let started = std::time::Instant::now();
        let error = connection
            .read(&mut [0_u8; 1])
            .expect_err("a silent backend sends nothing");
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
            "{error}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        release.send(()).unwrap();
        server.join().expect("backend");
    }
}
