//! Local TLS mechanism tests. The fixture keys are public test data, never a
//! production identity. Trust is added to one test client, not the OS store.
//!
//! Two fixtures: a CA-signed `localhost` leaf (`chain-*`), which both TLS
//! backends must accept once its root is trusted, and a self-signed CA used as
//! its own leaf (`cert.pem`), which native TLS accepts and rustls must refuse.

use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;

const SELF_SIGNED_CA: &[u8] = include_bytes!("../tests/fixtures/http-tls/cert.pem");
const SELF_SIGNED_IDENTITY: &str = include_str!("../tests/fixtures/http-tls/identity.p12.hex");
const CHAIN_ROOT: &[u8] = include_bytes!("../tests/fixtures/http-tls/chain-root.pem");
const CHAIN_IDENTITY: &str = include_str!("../tests/fixtures/http-tls/chain-leaf.p12.hex");

fn fixture(identity_hex: &str, response: Vec<u8>) -> (u16, JoinHandle<bool>) {
    let hex: String = identity_hex.split_whitespace().collect();
    let identity: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    let acceptor = native_tls::TlsAcceptor::new(
        native_tls::Identity::from_pkcs12(&identity, "fixture").unwrap(),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let worker = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let socket: TcpStream = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "TLS client never connected"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("TLS fixture accept: {error}"),
            }
        };
        // Winsock inherits the listener's nonblocking mode on accept. This
        // worker drives blocking native TLS with bounded socket timeouts.
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let Ok(mut tls) = acceptor.accept(socket) else {
            return false;
        };
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            if tls.read_exact(&mut byte).is_err() {
                return false;
            }
            request.push(byte[0]);
            assert!(request.len() < 8192);
        }
        tls.write_all(&response).unwrap();
        // Send TLS close-notify before dropping the stream. OpenSSL 3's client
        // path can otherwise surface an EOF as a generic SSL error while
        // reading this otherwise complete local response.
        tls.shutdown().unwrap();
        true
    });
    (port, worker)
}

fn limits() -> Limits {
    Limits {
        max_redirects: 1,
        total_timeout: Duration::from_secs(5),
        ..Limits::default()
    }
}

fn trusting_client(root: &[u8]) -> Client {
    Client::with_test_root(limits(), root).unwrap()
}

async fn joined(worker: JoinHandle<bool>) -> bool {
    tokio::task::spawn_blocking(move || worker.join().unwrap())
        .await
        .unwrap()
}

fn ok_response() -> Vec<u8> {
    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec()
}

#[tokio::test]
async fn trusted_local_tls_preserves_body() {
    let (port, worker) = fixture(CHAIN_IDENTITY, ok_response());
    let response = trusting_client(CHAIN_ROOT)
        .get(&format!("https://localhost:{port}/"))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.version(), Version::Http11);
    assert_eq!(response.into_bytes().await.unwrap(), b"ok");
    assert!(joined(worker).await);
}

#[tokio::test]
async fn untrusted_local_certificate_never_receives_http_request() {
    let (port, worker) = fixture(CHAIN_IDENTITY, ok_response());
    let result = Client::new(limits())
        .unwrap()
        .get(&format!("https://localhost:{port}/"))
        .await;
    assert!(result.is_err(), "untrusted certificate accepted");
    assert!(!joined(worker).await);
}

#[tokio::test]
async fn trusted_certificate_with_wrong_hostname_never_receives_http_request() {
    // The certificate has DNS:localhost only, not an IP address SAN.
    let (port, worker) = fixture(CHAIN_IDENTITY, ok_response());
    let result = trusting_client(CHAIN_ROOT)
        .get(&format!("https://127.0.0.1:{port}/"))
        .await;
    assert!(result.is_err(), "wrong certificate hostname accepted");
    assert!(!joined(worker).await);
}

#[tokio::test]
async fn trusted_https_downgrade_is_rejected_before_plaintext_connection() {
    let plaintext = TcpListener::bind("127.0.0.1:0").unwrap();
    plaintext.set_nonblocking(true).unwrap();
    let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: http://{}/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        plaintext.local_addr().unwrap()
    );
    let (port, worker) = fixture(CHAIN_IDENTITY, response.into_bytes());
    let result = trusting_client(CHAIN_ROOT)
        .get(&format!("https://localhost:{port}/"))
        .await;
    assert!(
        joined(worker).await,
        "TLS handshake/request must succeed first"
    );
    assert_eq!(result.err().unwrap().kind(), io::ErrorKind::InvalidData);
    assert_eq!(
        plaintext.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

/// Native TLS (selected whenever `http-client` is enabled) accepts a trusted
/// self-signed CA presented as its own leaf.
#[cfg(feature = "http-client")]
#[tokio::test]
async fn native_tls_accepts_trusted_ca_presented_as_leaf() {
    let (port, worker) = fixture(SELF_SIGNED_IDENTITY, ok_response());
    let response = trusting_client(SELF_SIGNED_CA)
        .get(&format!("https://localhost:{port}/"))
        .await
        .unwrap();
    assert_eq!(response.into_bytes().await.unwrap(), b"ok");
    assert!(joined(worker).await);
}

/// rustls (`http-client-rustls` alone) refuses a CA certificate presented as
/// an end-entity certificate, even when that same certificate is a trusted root.
#[cfg(not(feature = "http-client"))]
#[tokio::test]
async fn rustls_refuses_trusted_ca_presented_as_leaf() {
    let (port, worker) = fixture(SELF_SIGNED_IDENTITY, ok_response());
    let result = trusting_client(SELF_SIGNED_CA)
        .get(&format!("https://localhost:{port}/"))
        .await;
    assert!(result.is_err(), "rustls accepted a CA presented as a leaf");
    assert!(!joined(worker).await);
}
