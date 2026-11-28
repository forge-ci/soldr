#![cfg(feature = "daemon-identity")]

//! Field-level identity records, frozen sidecar/probe bytes, explicit-endpoint
//! probes, and generation-safe control of a verified daemon process.

use std::path::PathBuf;
use std::time::Duration;

use kernal_api::daemon_identity::{
    DaemonEndpoint, DaemonIdentity, DaemonIdentityRecord, DaemonVerifyError, LegacyPrefix,
    ProbeMuxResult, ProbeResponder, ProbeSameEndpoint,
};
use kernal_api::platform::process::{ProcessIdentityAction, ProcessIdentityUnavailable};

const SLEEPER_ENV: &str = "KERNAL_API_DAEMON_CONTROL_SLEEPER";
const SLEEPER_TEST: &str = "daemon_identity_control::sleeper_child";

fn fixed_record() -> DaemonIdentityRecord {
    DaemonIdentityRecord {
        pid: 4242,
        executable_path: PathBuf::from("/opt/d"),
        blake3_digest: [0x11; 32],
        legacy_sha256_digest: [0; 32],
        boot_id: "boot-1".to_owned(),
        endpoint: DaemonEndpoint::new("ns", "addr"),
        started_at_unix_ms: 1000,
        idle_timeout_secs: Some(30),
    }
}

fn current(endpoint: DaemonEndpoint) -> DaemonIdentity {
    DaemonIdentity::current_process(endpoint, None).expect("current process identity")
}

fn varint(mut value: u64, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push((value as u8) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn field_bytes(tag: u8, bytes: &[u8], output: &mut Vec<u8>) {
    output.push(tag);
    varint(bytes.len() as u64, output);
    output.extend_from_slice(bytes);
}

fn framed(body: Vec<u8>) -> Vec<u8> {
    let mut wire = vec![1];
    wire.extend_from_slice(&u32::try_from(body.len()).expect("small body").to_le_bytes());
    wire.extend(body);
    wire
}

/// A digest as `serde_json` pretty-prints a top-level field's byte array.
fn json_bytes(bytes: &[u8; 32]) -> String {
    let lines = bytes
        .iter()
        .map(|byte| format!("    {byte}"))
        .collect::<Vec<_>>()
        .join(",\n");
    format!("[\n{lines}\n  ]")
}

#[test]
fn record_round_trips_every_field() {
    let identity = DaemonIdentity::from_record(fixed_record());
    assert_eq!(identity.to_record(), fixed_record());
    assert_eq!(identity.pid(), 4242);
    assert_eq!(identity.executable_path(), PathBuf::from("/opt/d"));
    assert_eq!(identity.blake3_digest(), &[0x11; 32]);
    assert_eq!(identity.legacy_sha256_digest(), &[0; 32]);
    assert_eq!(identity.boot_id(), "boot-1");
    assert_eq!(identity.endpoint(), DaemonEndpoint::new("ns", "addr"));
    assert_eq!(identity.started_at_unix_ms(), 1000);
    assert_eq!(identity.idle_timeout_secs(), Some(30));

    let captured = current(DaemonEndpoint::new("record-ns", "record-addr"));
    assert_eq!(DaemonIdentity::from_record(captured.to_record()), captured);
}

/// The sidecar is the lockfile JSON older binaries wrote with the
/// substrate's serde form. Its exact bytes are frozen so old and new
/// binaries read each other's files.
#[test]
fn sidecar_bytes_are_frozen() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let sidecar = directory.path().join("daemon.running-process.json");
    let mut record = fixed_record();
    record.legacy_sha256_digest = [0x22; 32];
    DaemonIdentity::from_record(record.clone())
        .write_sidecar(&sidecar)
        .expect("write sidecar");

    let expected = format!(
        "{{\n  \"pid\": 4242,\n  \"exe_path\": \"/opt/d\",\n  \"exe_hash_algorithm\": \"blake3\",\n  \
         \"exe_hash\": {},\n  \"legacy_exe_sha256\": {},\n  \"boot_id\": \"boot-1\",\n  \
         \"ipc_endpoint\": {{\n    \"namespace_id\": \"ns\",\n    \"path\": \"addr\"\n  }},\n  \
         \"started_at_unix_ms\": 1000,\n  \"idle_timeout_secs\": 30\n}}",
        json_bytes(&[0x11; 32]),
        json_bytes(&[0x22; 32]),
    );
    let written = std::fs::read_to_string(&sidecar).expect("read sidecar");
    assert_eq!(written, expected);

    // Compact JSON (any whitespace) and the pre-SHA field set both decode.
    let compact = format!(
        "{{\"pid\":4242,\"exe_path\":\"/opt/d\",\"exe_hash_algorithm\":\"blake3\",\
         \"exe_hash\":{:?},\"boot_id\":\"boot-1\",\
         \"ipc_endpoint\":{{\"namespace_id\":\"ns\",\"path\":\"addr\"}},\
         \"started_at_unix_ms\":1000,\"idle_timeout_secs\":null}}",
        [0x11_u8; 32]
    );
    std::fs::write(&sidecar, compact).expect("write compact sidecar");
    let restored = DaemonIdentity::try_read_sidecar(&sidecar)
        .expect("decode compact sidecar")
        .expect("sidecar exists");
    let mut expected_record = fixed_record();
    expected_record.idle_timeout_secs = None;
    assert_eq!(restored.to_record(), expected_record);

    std::fs::write(
        &sidecar,
        written.replace(
            "\"exe_hash_algorithm\": \"blake3\"",
            "\"exe_hash_algorithm\": \"sha256\"",
        ),
    )
    .expect("write foreign-algorithm sidecar");
    assert!(DaemonIdentity::try_read_sidecar(&sidecar).is_err());
    assert_eq!(DaemonIdentity::read_sidecar(&sidecar), None);
}

/// A probe reply is byte-for-byte the frozen v1 response: echoed nonce,
/// request id and trace context, then the prost identity with the legacy
/// tag-3 digest appended.
#[test]
fn probe_reply_bytes_are_frozen() {
    let responder = ProbeResponder::new(DaemonIdentity::from_record(fixed_record()), [0x7A63]);
    let nonce = [0xA5; 32];

    let mut request = vec![0x08, 1, 0x18];
    varint(0xB232, &mut request);
    field_bytes(0x22, &nonce, &mut request);
    request.extend([0x28, 7]);
    field_bytes(0x42, b"trace", &mut request);
    field_bytes(0x4a, b"state", &mut request);
    let request = framed(request);

    let mut endpoint = Vec::new();
    field_bytes(0x0a, b"ns", &mut endpoint);
    field_bytes(0x12, b"addr", &mut endpoint);
    let mut identity = vec![0x08];
    varint(4242, &mut identity);
    field_bytes(0x12, b"/opt/d", &mut identity);
    field_bytes(0x22, &endpoint, &mut identity);
    identity.push(0x28);
    varint(1000, &mut identity);
    field_bytes(0x32, b"boot-1", &mut identity);
    identity.extend([0x38, 30]);
    field_bytes(0x42, b"blake3", &mut identity);
    field_bytes(0x4a, &[0x11; 32], &mut identity);
    field_bytes(0x1a, &[0; 32], &mut identity);

    let mut payload = nonce.to_vec();
    payload.extend(identity);
    let mut response = vec![0x08, 1, 0x10, 1, 0x18];
    varint(0xB232, &mut response);
    field_bytes(0x22, &payload, &mut response);
    response.extend([0x28, 7]);
    field_bytes(0x42, b"trace", &mut response);
    field_bytes(0x4a, b"state", &mut response);
    let expected = framed(response);

    let ProbeMuxResult::ProbeReply { reply, consumed } = responder
        .poll(&request, LegacyPrefix::NotLegacy)
        .expect("answer probe")
    else {
        panic!("expected a probe reply");
    };
    assert_eq!(consumed, request.len());
    assert_eq!(reply, expected);
}

#[test]
fn probe_refuses_an_endpoint_other_than_the_recorded_one() {
    let identity = current(DaemonEndpoint::new("probe-ns", "probe-addr"));
    for other in [
        DaemonEndpoint::new("probe-ns", "other-addr"),
        DaemonEndpoint::new("other-ns", "probe-addr"),
    ] {
        assert_eq!(
            identity.probe_endpoint_blocking(&other),
            ProbeSameEndpoint::NotCurrent
        );
    }
}

#[cfg(unix)]
#[test]
fn blocking_probe_of_a_live_endpoint_is_current() {
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixListener;

    let directory = tempfile::tempdir().expect("temporary endpoint directory");
    let address = directory.path().join("blocking.sock");
    let endpoint = DaemonEndpoint::new("kernal-api-blocking-probe", address.to_string_lossy());
    let identity = current(endpoint.clone());
    let responder = ProbeResponder::new(identity.clone(), []);
    let listener = UnixListener::bind(&address).expect("bind endpoint");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept probe");
        let mut buffered = vec![0; 5];
        stream.read_exact(&mut buffered).expect("read header");
        let length = u32::from_le_bytes(buffered[1..].try_into().expect("length"));
        let mut body = vec![0; length as usize];
        stream.read_exact(&mut body).expect("read body");
        buffered.extend(body);
        let ProbeMuxResult::ProbeReply { reply, .. } = responder
            .poll(&buffered, LegacyPrefix::NotLegacy)
            .expect("answer probe")
        else {
            panic!("expected a probe reply");
        };
        stream.write_all(&reply).expect("write reply");
    });

    assert_eq!(
        identity.probe_endpoint_blocking(&endpoint),
        ProbeSameEndpoint::Current
    );
    server.join().expect("probe server exits");
}

#[test]
fn this_process_verifies_live_and_for_control() {
    let identity = current(DaemonEndpoint::new("verify-ns", "verify-addr"));
    let live = identity.verify_live().expect("this process verifies");
    assert_eq!(live.pid(), std::process::id());
    assert!(live.is_alive());
    assert!(!live.has_exited().expect("observe a live daemon"));
    assert!(matches!(
        live.force_kill(),
        Err(DaemonVerifyError::ControlNotRetained { pid }) if pid == std::process::id()
    ));

    let control = identity
        .verify_for_control()
        .expect("this process verifies for control");
    assert!(control.is_alive());
}

#[test]
fn verification_rejects_every_mismatched_field() {
    let identity = current(DaemonEndpoint::new("mismatch-ns", "mismatch-addr"));
    let pid = identity.pid();

    let mut record = identity.to_record();
    record.pid = 0;
    assert!(matches!(
        DaemonIdentity::from_record(record).verify_live(),
        Err(DaemonVerifyError::InvalidPid(0))
    ));

    let mut record = identity.to_record();
    record.boot_id = "a-boot-that-never-happened".to_owned();
    assert!(matches!(
        DaemonIdentity::from_record(record).verify_live(),
        Err(DaemonVerifyError::BootIdMismatch { expected, .. })
            if expected == "a-boot-that-never-happened"
    ));

    let mut record = identity.to_record();
    record.boot_id.clear();
    assert!(
        DaemonIdentity::from_record(record).verify_live().is_ok(),
        "an identity recorded before boot tracking is not a boot mismatch"
    );

    let mut record = identity.to_record();
    record.executable_path = std::env::temp_dir().join("kernal-api-not-this-daemon");
    assert!(matches!(
        DaemonIdentity::from_record(record).verify_live(),
        Err(DaemonVerifyError::ExecutablePathMismatch { pid: actual, .. }) if actual == pid
    ));

    let mut record = identity.to_record();
    record.blake3_digest[0] ^= 0xFF;
    assert!(matches!(
        DaemonIdentity::from_record(record).verify_for_control(),
        Err(DaemonVerifyError::ExecutableHashMismatch { pid: actual }) if actual == pid
    ));
}

/// Re-executed by [`verified_child_is_killed_exactly_once`]; a no-op otherwise.
#[test]
#[ignore = "helper process for verified_child_is_killed_exactly_once"]
fn sleeper_child() {
    if std::env::var_os(SLEEPER_ENV).is_some() {
        std::thread::sleep(Duration::from_secs(60));
    }
}

#[test]
fn verified_child_is_killed_exactly_once() {
    let executable = std::env::current_exe().expect("test executable");
    let mut child = std::process::Command::new(&executable)
        .args([SLEEPER_TEST, "--exact", "--ignored", "--nocapture"])
        .env(SLEEPER_ENV, "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn sleeper");

    let mut record = current(DaemonEndpoint::new("child-ns", "child-addr")).to_record();
    record.pid = child.id();
    let identity = DaemonIdentity::from_record(record);

    let verified = identity
        .verify_for_control()
        .expect("sleeper verifies for control");
    assert_eq!(verified.pid(), child.id());
    assert!(verified.is_alive());
    assert!(!verified.has_exited().expect("observe the running sleeper"));
    let first = verified.force_kill();
    if cfg!(target_os = "macos") {
        // macOS has no handle that pins a process generation, and a check
        // followed by kill(pid) can hit a reused PID, so control refuses
        // explicitly rather than racing (#347). The refusal must not signal.
        assert!(
            matches!(
                first,
                Err(DaemonVerifyError::ControlUnavailable {
                    reason: ProcessIdentityUnavailable::Unsupported,
                    ..
                })
            ),
            "macOS refuses identity-bound termination: {first:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            child.try_wait().expect("poll sleeper").is_none(),
            "a refused kill leaves the verified process running"
        );
        assert!(!verified.has_exited().expect("observe the running sleeper"));
        child.kill().expect("parent kills its own sleeper");
        child.wait().expect("reap sleeper");
        return;
    }
    assert_eq!(
        first.expect("kill verified sleeper"),
        ProcessIdentityAction::Performed
    );
    // A real daemon is not this process's child, so nothing reaps it here:
    // the exit must be observable through the retained reference alone,
    // before (and regardless of) any wait by a parent.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !verified
        .has_exited()
        .expect("observe the killed, unreaped sleeper")
    {
        assert!(
            std::time::Instant::now() < deadline,
            "a killed daemon must be observed as exited"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = child.wait().expect("reap sleeper");
    assert!(!status.success());
    assert!(!verified.is_alive(), "the retained reference sees the exit");
    assert!(
        verified.has_exited().expect("observe the killed sleeper"),
        "the fallible observation reports the exit, not an error"
    );
    assert!(
        !matches!(verified.force_kill(), Ok(ProcessIdentityAction::Performed)),
        "a second kill never reaches another process"
    );
    assert!(identity.verify_live().is_err());
}
