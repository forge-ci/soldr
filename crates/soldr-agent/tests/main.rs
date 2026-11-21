use ed25519_dalek::{Signature, VerifyingKey};
use futures_util::{SinkExt, StreamExt};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use soldr_agent::{
    backoff::Backoff, checks, config::Config, host_identity, proto::*, runtime, status::Status,
};
use std::{path::Path, time::Duration};
use tokio::{net::TcpListener, sync::watch};
use tokio_tungstenite::tungstenite::Message;

fn fixture(name: &str) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/signal-proto")
                .join(format!("{name}.json")),
        )
        .unwrap(),
    )
    .unwrap()
}
fn roundtrip<T: Serialize + DeserializeOwned>(name: &str) {
    let json = fixture(name);
    let typed: T = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), json, "{name}");
}
#[derive(Serialize, Deserialize)]
struct SignatureVector {
    seed_hex: String,
    public_key: String,
    nonce: String,
    name: String,
    signing_bytes_b64: String,
    sig: String,
}
#[test]
fn every_protocol_fixture_roundtrips() {
    roundtrip::<ToHost>("challenge");
    roundtrip::<ToHost>("ping");
    for name in ["hello", "heartbeat", "pong"] {
        roundtrip::<FromHost>(name);
    }
    roundtrip::<Health>("health");
    roundtrip::<HostView>("host_view");
    roundtrip::<HostEvent>("host_event");
    roundtrip::<DistNode>("dist_node");
    roundtrip::<SignatureVector>("signature");
    let names = std::fs::read_dir(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/signal-proto"),
    )
    .unwrap()
    .filter_map(|e| {
        let p = e.unwrap().path();
        (p.extension().is_some_and(|s| s == "json")).then_some(p)
    })
    .count();
    assert_eq!(
        names, 10,
        "add every newly copied fixture to this conformance test"
    );
}
fn vector_key(path: &Path) -> host_identity::HostKey {
    let vector = fixture("signature");
    let hex = vector["seed_hex"].as_str().unwrap();
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    std::fs::write(path, bytes).unwrap();
    host_identity::load_or_create(path).unwrap()
}
#[test]
fn signature_vector_and_shared_iroh_identity() {
    let dir = tempfile::tempdir().unwrap();
    let key = vector_key(&dir.path().join("host.key"));
    let v = fixture("signature");
    let nonce = v["nonce"].as_str().unwrap();
    let name = v["name"].as_str().unwrap();
    assert_eq!(encode(&signing_bytes(nonce, name)), v["signing_bytes_b64"]);
    assert_eq!(key.public_key(), v["public_key"]);
    assert_eq!(key.sign_challenge(nonce, name), v["sig"]);
    let public = ed25519_dalek::SigningKey::from_bytes(&key.iroh_secret_bytes()).verifying_key();
    let signature = Signature::from_slice(&decode(v["sig"].as_str().unwrap()).unwrap()).unwrap();
    public
        .verify_strict(&signing_bytes(nonce, name), &signature)
        .unwrap();
    assert!(public
        .verify_strict(&signing_bytes(nonce, "other-host"), &signature)
        .is_err());
}
#[test]
fn keygen_is_atomic_private_and_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("host.key");
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || host_identity::load_or_create(&path).unwrap().public_key())
        })
        .collect();
    let keys: Vec<_> = threads
        .into_iter()
        .map(|t| soldr_platform::wait::join_for(t, Duration::from_secs(5)).unwrap())
        .collect();
    assert!(keys.iter().all(|key| key == &keys[0]));
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 32);
    if let Some(mode) = soldr_platform::fs::permissions::mode(&path) {
        assert_eq!(mode & 0o777, 0o600);
    }
    std::fs::write(&path, b"invalid key").unwrap();
    assert!(host_identity::load_or_create(&path).is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"invalid key");
}
fn config(url: &str, root: &Path) -> Config {
    Config::parse(&format!("name = 'test-node'\nrole = 'shared'\nsignal_url = '{url}'\nhealth_bind = '127.0.0.1:0'\nstate_dir = '{}'\nsoldr_root = '{}'\n",root.display(),root.display())).unwrap()
}
#[test]
fn config_requires_secure_signal_and_lan_health() {
    let root = tempfile::tempdir().unwrap();
    let c = config("https://signal.forge-ci.com", root.path());
    assert_eq!(
        c.connect_url().unwrap().as_str(),
        "wss://signal.forge-ci.com/v1/hosts/connect"
    );
    for url in [
        "ws://192.168.1.2",
        "https://user:secret@example.com",
        "wss://example.com/?token=x",
    ] {
        assert!(Config::parse(&format!("name='node'\nrole='runner'\nsignal_url='{url}'")).is_err());
    }
    assert!(Config::parse(
        "name='node'\nrole='runner'\nsignal_url='wss://example.com'\nhealth_bind='8.8.8.8:9099'"
    )
    .is_err());
    assert!(Config::parse(
        "name='node'\nrole='runner'\nsignal_url='wss://example.com'\nunknown=true"
    )
    .is_err());
}
#[test]
fn backoff_schedule_uses_a_deterministic_clock_and_caps_at_30s() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        tokio::time::pause();
        let mut backoff = Backoff::default();
        for expected in [1, 2, 4, 8, 16, 30, 30] {
            let delay = backoff.next_delay(0);
            assert_eq!(delay, Duration::from_secs(expected));
            let start = tokio::time::Instant::now();
            tokio::time::sleep(delay).await;
            assert_eq!(start.elapsed(), delay);
        }
        backoff.reset();
        assert_eq!(backoff.next_delay(0), Duration::from_secs(1));
        assert!(backoff.next_delay(u32::MAX) <= Duration::from_secs(2));
    });
}
#[test]
fn loopback_challenge_hello_heartbeat_ping_pong_and_status() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let key = vector_key(&dir.path().join("host.key"));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let c = config(
            &format!("ws://{}", listener.local_addr().unwrap()),
            dir.path(),
        );
        let (stop, rx) = watch::channel(false);
        let agent = tokio::spawn(runtime::run(c, key, rx));
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_hdr_async(
            stream,
            |req: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                assert_eq!(req.uri().path(), "/v1/hosts/connect");
                Ok(response)
            },
        )
        .await
        .unwrap();
        ws.send(Message::Text(fixture("challenge").to_string().into()))
            .await
            .unwrap();
        let message = ws.next().await.unwrap().unwrap().into_text().unwrap();
        let FromHost::Hello(hello) = serde_json::from_str(&message).unwrap() else {
            panic!("expected hello")
        };
        let v = fixture("signature");
        let public: [u8; 32] = decode(v["public_key"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        VerifyingKey::from_bytes(&public)
            .unwrap()
            .verify_strict(
                &signing_bytes(v["nonce"].as_str().unwrap(), &hello.name),
                &Signature::from_slice(&decode(&hello.sig).unwrap()).unwrap(),
            )
            .unwrap();
        assert!(!hello.boot_id.is_empty());
        assert_eq!(hello.role, Role::Shared);
        let heartbeat = ws.next().await.unwrap().unwrap().into_text().unwrap();
        assert!(matches!(
            serde_json::from_str::<FromHost>(&heartbeat).unwrap(),
            FromHost::Heartbeat { .. }
        ));
        ws.send(Message::Text(fixture("ping").to_string().into()))
            .await
            .unwrap();
        let pong = ws.next().await.unwrap().unwrap().into_text().unwrap();
        let FromHost::Pong { id, checks } = serde_json::from_str(&pong).unwrap() else {
            panic!("expected pong")
        };
        assert_eq!(id, 42);
        assert!(checks.iter().any(|c| c.name == "disk:/"));
        assert_eq!(Status::read(dir.path()).unwrap().state, "connected");
        stop.send(true).unwrap();
        agent.await.unwrap().unwrap();
        assert_eq!(Status::read(dir.path()).unwrap().state, "stopped");
    });
}
#[test]
fn load_failure_reserves_all_cpus_and_health_is_fresh() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let mut c = config("ws://127.0.0.1:1", dir.path());
        let load = dir.path().join("load.json");
        c.host_load_file = Some(load.clone());
        let (l, _, checks) = checks::capacity(&c).await;
        assert_eq!(l.cpus, l.gh_cpus_reserved);
        assert!(!checks[0].ok);
        std::fs::write(
            load,
            r#"{"cpus":8,"gh_jobs_running":1,"gh_cpus_reserved":2}"#,
        )
        .unwrap();
        let (l, _, checks) = checks::capacity(&c).await;
        assert_eq!(l.gh_cpus_reserved, 2);
        assert!(checks.is_empty());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(runtime::health_server(listener, std::sync::Arc::new(c)));
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        s.read_to_string(&mut response).await.unwrap();
        let health: Health =
            serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let expected = if health.checks.iter().all(|c| c.ok) {
            "HTTP/1.1 200"
        } else {
            "HTTP/1.1 503"
        };
        assert!(response.starts_with(expected));
        assert!(health.checks.iter().any(|c| c.name == "disk:/"));
        server.abort();
    });
}

#[test]
fn probe_timeout_preserves_stderr_and_kills_descendants() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("escaped");
        let script = format!(
            "echo probe-stderr >&2; (sleep 1; touch '{}') & wait",
            marker.display()
        );
        let result = checks::probe("sh", &["-c".into(), script]).await.unwrap();
        assert!(!result.ok);
        assert!(result.detail.contains("timed out"));
        assert!(result.detail.contains("probe-stderr"));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!marker.exists(), "timeout must kill the descendant too");
        let result = checks::probe("sh", &["-c".into(), "echo diagnostic >&2; exit 7".into()])
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.detail.contains("diagnostic"));
    });
}
