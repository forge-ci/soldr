//! Black-box coverage for the opt-in direct-daemon facade.

#![cfg(feature = "daemon-identity")]

use std::path::Path;

#[cfg(unix)]
use std::io::{Read as _, Write as _};
#[cfg(unix)]
use std::os::unix::net::UnixListener;

use kernal_api::daemon_identity::{
    DaemonEndpoint, DaemonIdentity, DaemonIdentityHashPolicy, DaemonMuxError, LegacyPrefix,
    ProbeMuxResult, ProbeResponder,
};

const PRODUCT_PROTOCOL: u32 = 0xF412;

fn identity() -> DaemonIdentity {
    DaemonIdentity::current_process(
        DaemonEndpoint::new("kernal-api-test-namespace", "kernal-api-test-address"),
        Some(30),
    )
    .expect("current process identity")
}

fn framed(body: Vec<u8>) -> Vec<u8> {
    let mut wire = vec![1];
    wire.extend_from_slice(
        &u32::try_from(body.len())
            .expect("small test body")
            .to_le_bytes(),
    );
    wire.extend(body);
    wire
}

fn varint(mut value: u64, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push((value as u8) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn product_wire() -> Vec<u8> {
    let mut body = vec![0x08, 1, 0x10, 99, 0x18];
    varint(PRODUCT_PROTOCOL.into(), &mut body);
    body.extend([0x22, 7]);
    body.extend(b"payload");
    body.extend([0x28, 42, 0x30, 77, 0x38, 123, 0x42, 5]);
    body.extend(b"trace");
    body.extend([0x4a, 5]);
    body.extend(b"state");
    framed(body)
}

fn probe_wire(payload: &[u8]) -> Vec<u8> {
    let mut body = vec![0x08, 1, 0x18, 0xB2, 0xE4, 0x02, 0x22];
    varint(
        payload.len().try_into().expect("small probe payload"),
        &mut body,
    );
    body.extend(payload);
    body.extend([0x28, 7, 0x42, 5]);
    body.extend(b"trace");
    body.extend([0x4a, 5]);
    body.extend(b"state");
    framed(body)
}

#[test]
fn identity_sidecar_round_trips_and_retains_old_missing_sha_field() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let sidecar = directory.path().join("daemon-identity.json");
    let original = identity();

    original.write_sidecar(&sidecar).expect("write sidecar");
    assert_eq!(
        DaemonIdentity::read_sidecar(&sidecar),
        Some(original.clone())
    );
    assert_eq!(
        DaemonIdentity::try_read_sidecar(&sidecar).expect("strict sidecar read"),
        Some(original.clone())
    );
    assert_eq!(original.endpoint().namespace(), "kernal-api-test-namespace");
    assert_eq!(original.endpoint().address(), "kernal-api-test-address");

    let mut skipping_digest = false;
    let legacy = std::fs::read_to_string(&sidecar)
        .expect("read sidecar")
        .lines()
        .filter(|line| {
            if line.contains("legacy_exe_sha256") {
                skipping_digest = true;
                return false;
            }
            if skipping_digest {
                if line.trim() == "]," {
                    skipping_digest = false;
                }
                return false;
            }
            true
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&sidecar, legacy).expect("write old sidecar");
    let restored = DaemonIdentity::try_read_sidecar(&sidecar)
        .expect("old sidecar")
        .expect("sidecar exists");
    assert_eq!(restored.blake3_digest(), original.blake3_digest());
    assert_eq!(restored.legacy_sha256_digest(), &[0; 32]);
    assert_eq!(restored.endpoint(), original.endpoint());

    DaemonIdentity::remove_sidecar(&sidecar);
    assert_eq!(DaemonIdentity::read_sidecar(&sidecar), None);
}

#[test]
fn endpoint_spelling_is_verbatim() {
    let endpoint = DaemonEndpoint::new("a namespace / exactly", r"\\.\pipe\literal-name");
    let identity = DaemonIdentity::current_process(endpoint.clone(), None).expect("identity");
    assert_eq!(identity.endpoint(), endpoint);
    assert_eq!(identity.endpoint().namespace(), "a namespace / exactly");
    assert_eq!(identity.endpoint().address(), r"\\.\pipe\literal-name");
}

#[test]
fn blake3_only_policy_keeps_the_legacy_sidecar_and_probe_digest_zeroed() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let sidecar = directory.path().join("daemon-identity.json");
    let identity = DaemonIdentity::current_process_with_hash_policy(
        DaemonEndpoint::new("kernal-api-blake3-only", "kernal-api-blake3-only-address"),
        None,
        DaemonIdentityHashPolicy::Blake3Only,
    )
    .expect("current process identity");

    identity.write_sidecar(&sidecar).expect("write sidecar");
    let json = std::fs::read_to_string(&sidecar).expect("read sidecar");
    let legacy = json
        .split("\"legacy_exe_sha256\": [")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .expect("legacy digest array");
    let legacy = legacy
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| entry.parse::<u8>().expect("legacy digest byte"))
        .collect::<Vec<_>>();
    assert_eq!(legacy, vec![0; 32]);
    assert_eq!(identity.legacy_sha256_digest(), &[0; 32]);
}

#[test]
fn mux_keeps_legacy_first_and_preserves_product_frame_fields() {
    let responder = ProbeResponder::new(identity(), [PRODUCT_PROTOCOL]);
    assert_eq!(
        responder
            .poll(&[], LegacyPrefix::NotLegacy)
            .expect("empty buffer"),
        ProbeMuxResult::NeedMoreBytes
    );
    assert_eq!(
        responder
            .poll(&[1, 0, 0], LegacyPrefix::NotLegacy)
            .expect("partial frame"),
        ProbeMuxResult::NeedMoreBytes
    );
    let legacy_prefix = [1, 0, 0, 0, 15, 0, 0, 0];
    assert_eq!(
        responder
            .poll(&legacy_prefix, LegacyPrefix::Legacy)
            .expect("legacy wins even when it begins with v1"),
        ProbeMuxResult::Legacy
    );

    let wire = product_wire();
    let ProbeMuxResult::ProductFrame { frame, consumed } = responder
        .poll(&wire, LegacyPrefix::NotLegacy)
        .expect("product frame")
    else {
        panic!("expected product frame");
    };
    assert_eq!(consumed, wire.len());
    assert_eq!(frame.envelope_version, 1);
    assert_eq!(frame.kind, 99);
    assert_eq!(frame.payload_protocol, PRODUCT_PROTOCOL);
    assert_eq!(frame.payload, b"payload");
    assert_eq!(frame.request_id, 42);
    assert_eq!(frame.payload_encoding, 77);
    assert_eq!(frame.deadline_unix_ms, 123);
    assert_eq!(frame.traceparent, "trace");
    assert_eq!(frame.tracestate, "state");
}

#[test]
fn mux_answers_the_frozen_probe_and_rejects_bad_or_oversized_frames() {
    let responder = ProbeResponder::new(identity(), [PRODUCT_PROTOCOL]);
    let nonce = [9; 32];
    let probe = probe_wire(&nonce);
    let ProbeMuxResult::ProbeReply { reply, consumed } = responder
        .poll(&probe, LegacyPrefix::NotLegacy)
        .expect("answer probe")
    else {
        panic!("expected probe reply");
    };
    assert_eq!(consumed, probe.len());
    assert_eq!(reply[0], 1, "frozen v1 framing byte");
    assert!(reply.windows(nonce.len()).any(|window| window == nonce));
    assert!(reply
        .windows(b"trace".len())
        .any(|window| window == b"trace"));
    assert!(reply
        .windows(b"state".len())
        .any(|window| window == b"state"));

    let malformed = probe_wire(&[0; 31]);
    assert!(matches!(
        responder.poll(&malformed, LegacyPrefix::NotLegacy),
        Err(DaemonMuxError::MalformedProbe)
    ));

    let mut oversized = vec![1];
    oversized.extend_from_slice(&(16_u32 * 1024 * 1024 + 1).to_le_bytes());
    assert!(matches!(
        responder.poll(&oversized, LegacyPrefix::NotLegacy),
        Err(DaemonMuxError::FrameTooLarge { maximum, .. }) if maximum == 16 * 1024 * 1024
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn same_endpoint_probe_is_current_once_and_unbound_endpoint_is_not_current() {
    let directory = tempfile::tempdir().expect("temporary endpoint directory");
    let address = directory.path().join("current.sock");
    let endpoint = DaemonEndpoint::new("kernal-api-live-probe", address.to_string_lossy());
    let identity = DaemonIdentity::current_process(endpoint, Some(30)).expect("identity");
    let responder = ProbeResponder::new(identity.clone(), []);
    let listener = UnixListener::bind(&address).expect("bind application-owned endpoint");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept one identity probe");
        let mut header = [0; 5];
        stream.read_exact(&mut header).expect("read frozen header");
        let body_length = u32::from_le_bytes(header[1..].try_into().expect("header length"));
        let mut buffered = header.to_vec();
        let mut body = vec![0; body_length as usize];
        stream.read_exact(&mut body).expect("read frozen body");
        buffered.extend(body);
        let ProbeMuxResult::ProbeReply { reply, consumed } = responder
            .poll(&buffered, LegacyPrefix::NotLegacy)
            .expect("answer live identity probe")
        else {
            panic!("expected a probe reply");
        };
        assert_eq!(consumed, buffered.len());
        stream.write_all(&reply).expect("write exactly one reply");
        listener
    });

    assert_eq!(
        identity.probe_same_endpoint().await,
        kernal_api::daemon_identity::ProbeSameEndpoint::Current
    );
    let listener = server.join().expect("probe server exits");
    listener
        .set_nonblocking(true)
        .expect("inspect listener queue after the probe");
    assert_eq!(
        listener
            .accept()
            .expect_err("probe must not add another connection")
            .kind(),
        std::io::ErrorKind::WouldBlock
    );

    let absent = DaemonIdentity::current_process(
        DaemonEndpoint::new(
            "kernal-api-live-probe",
            directory.path().join("absent.sock").to_string_lossy(),
        ),
        Some(30),
    )
    .expect("identity for unbound endpoint");
    assert_eq!(
        absent.probe_same_endpoint().await,
        kernal_api::daemon_identity::ProbeSameEndpoint::NotCurrent
    );
}

#[test]
fn facade_public_source_does_not_name_implementation_types() {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/daemon_identity.rs"),
    )
    .expect("read facade module");
    for line in source
        .lines()
        .map(str::trim_start)
        .filter(|line| line.starts_with("pub "))
    {
        for forbidden in [
            "running_process",
            "prost",
            "interprocess",
            "tokio",
            "RawFd",
            "RawHandle",
        ] {
            assert!(
                !line.contains(forbidden),
                "public facade declaration leaks {forbidden:?}: {line}"
            );
        }
    }
}
