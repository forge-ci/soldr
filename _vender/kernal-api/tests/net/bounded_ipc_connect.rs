#![cfg(feature = "ipc")]
//! `platform::ipc::Stream` offers a connect bounded by a timeout.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use kernal_api::platform::ipc::{Endpoint, Listener, Stream};

#[test]
fn a_bounded_connect_reaches_a_live_listener() {
    let endpoint = Endpoint::test("bounded-connect").unwrap();
    let listener = Listener::bind(&endpoint).unwrap();
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap();
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte).unwrap();
        byte[0]
    });
    let mut client = Stream::connect_within(&endpoint, Duration::from_secs(30)).unwrap();
    client.write_all(&[7]).unwrap();
    assert_eq!(server.join().unwrap(), 7);
}

#[test]
fn a_bounded_connect_to_a_missing_endpoint_fails_without_waiting() {
    let endpoint = Endpoint::test("bounded-connect-missing").unwrap();
    let started = Instant::now();
    let error = Stream::connect_within(&endpoint, Duration::from_secs(30))
        .expect_err("nothing listens on a fresh endpoint");
    assert_ne!(error.kind(), io::ErrorKind::TimedOut, "{error}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[cfg(feature = "ipc-async")]
#[tokio::test]
async fn bounded_async_ipc_operations_time_out_on_a_silent_peer() {
    use kernal_api::platform::ipc::{AsyncListener, AsyncStream};

    let endpoint = Endpoint::test("bounded-async").unwrap();
    let listener = AsyncListener::bind(&endpoint).unwrap();
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.unwrap();
        let mut byte = [0_u8; 1];
        stream
            .read_exact_within(&mut byte, Duration::from_secs(1))
            .await
            .unwrap();
        stream
            .write_all_within(&byte, Duration::from_secs(1))
            .await
            .unwrap();
        // Stay connected so the client's later read sees silence, not EOF.
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let mut client = AsyncStream::connect_within(&endpoint, Duration::from_secs(30))
        .await
        .unwrap();
    client
        .write_all_within(&[9], Duration::from_secs(30))
        .await
        .unwrap();
    let mut reply = [0_u8; 1];
    client
        .read_exact_within(&mut reply, Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(reply, [9]);

    let started = Instant::now();
    let error = client
        .read_within(&mut reply, Duration::from_millis(100))
        .await
        .expect_err("the peer sends nothing more");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(10));
    server.abort();
}

/// A peer that never reads fills the transport buffer; a send timeout turns
/// the writer's otherwise endless block into an error. Windows named pipes
/// support no I/O timeouts and report that instead.
#[test]
fn a_send_timeout_bounds_a_writer_whose_peer_never_reads() {
    let endpoint = Endpoint::test("send-timeout").unwrap();
    let listener = Listener::bind(&endpoint).unwrap();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let _stream = listener.accept().unwrap();
        released.recv().ok();
    });
    let mut client = Stream::connect_within(&endpoint, Duration::from_secs(30)).unwrap();
    match client.set_send_timeout(Some(Duration::from_millis(100))) {
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {}
        Err(error) => panic!("set_send_timeout: {error}"),
        Ok(()) => {
            let chunk = vec![0_u8; 64 * 1024];
            let started = Instant::now();
            let error = loop {
                if let Err(error) = client.write(&chunk) {
                    break error;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "writes never blocked"
                );
            };
            assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ),
                "{error}"
            );
        }
    }
    release.send(()).unwrap();
    server.join().unwrap();
}

#[cfg(feature = "ipc-async")]
#[tokio::test]
async fn bounded_split_halves_flush_and_shutdown() {
    use kernal_api::platform::ipc::{AsyncListener, AsyncStream};

    let endpoint = Endpoint::test("bounded-halves").unwrap();
    let listener = AsyncListener::bind(&endpoint).unwrap();
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.unwrap();
        let mut byte = [0_u8; 1];
        stream
            .read_exact_within(&mut byte, Duration::from_secs(1))
            .await
            .unwrap();
        stream
            .write_all_within(&byte, Duration::from_secs(1))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let budget = Duration::from_secs(30);
    let client = AsyncStream::connect_within(&endpoint, budget)
        .await
        .unwrap();
    let (mut reader, mut writer) = client.into_split();
    writer.write_all_within(&[5], budget).await.unwrap();
    writer.flush_within(budget).await.unwrap();
    let mut reply = [0_u8; 1];
    reader.read_exact_within(&mut reply, budget).await.unwrap();
    assert_eq!(reply, [5]);

    let started = Instant::now();
    let error = reader
        .read_within(&mut reply, Duration::from_millis(100))
        .await
        .expect_err("the peer sends nothing more");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(10));
    writer.shutdown_within(budget).await.unwrap();
    server.abort();
}
