#![cfg(feature = "ipc")]
//! Owner-only single-instance local IPC primitives in `platform::ipc`.
//!
//! These pin the transport contract zccache's daemon relies on: a pathname
//! socket bound mode `0o600` with observed peer credentials, or a named-pipe
//! instance with an owner-only DACL, first-instance exclusivity, and a
//! preserved pipe-busy error. Retry, pooling, and endpoint spelling are the
//! caller's; nothing here retires or unlinks an endpoint implicitly.

use std::future::{poll_fn, Future};
use std::io;
use std::task::{Context, Waker};

use kernal_api::platform::ipc::{
    retire_socket_endpoint, LocalPipeClient, LocalSocketListener, LocalSocketStream,
    OwnerOnlyPipeInstance,
};

#[cfg(unix)]
mod pathname_socket {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[tokio::test]
    async fn owner_mode_peer_credentials_and_byte_io() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("socket");
        let listener = LocalSocketListener::bind_owner_only(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut client = LocalSocketStream::connect(&path).await.unwrap();
        let (mut server, peer) = listener.accept().await.unwrap();
        assert!(peer.credentials_available());
        assert!(peer.is_current_user());
        if cfg!(target_os = "linux") {
            assert_eq!(peer.pid(), Some(std::process::id()));
        }
        assert_eq!(poll_fn(|cx| client.poll_write(cx, b"x")).await.unwrap(), 1);
        poll_fn(|cx| client.poll_flush(cx)).await.unwrap();
        let mut buffer = [0; 1];
        assert_eq!(
            poll_fn(|cx| server.poll_read(cx, &mut buffer))
                .await
                .unwrap(),
            1
        );
        assert_eq!(buffer, *b"x");
        drop(listener);
        assert!(
            path.exists(),
            "drop must not retire the endpoint implicitly"
        );
    }

    #[tokio::test]
    async fn canceled_accept_keeps_listener_usable_and_shutdown_delivers_eof() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cancel");
        let listener = LocalSocketListener::bind_owner_only(&path).unwrap();
        {
            let mut pending = Box::pin(listener.accept());
            let mut cx = Context::from_waker(Waker::noop());
            assert!(pending.as_mut().poll(&mut cx).is_pending());
        }
        let mut client = LocalSocketStream::connect(&path).await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();
        poll_fn(|cx| client.poll_shutdown(cx)).await.unwrap();
        let mut bytes = [0; 1];
        assert_eq!(
            poll_fn(|cx| server.poll_read(cx, &mut bytes))
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn binding_never_retires_an_existing_regular_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("keep");
        std::fs::write(&path, b"keep").unwrap();
        assert!(LocalSocketListener::bind_owner_only(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn retirement_removes_only_sockets() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing");
        retire_socket_endpoint(&missing).unwrap();

        let regular = directory.path().join("regular");
        std::fs::write(&regular, b"keep").unwrap();
        assert_eq!(
            retire_socket_endpoint(&regular).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(std::fs::read(&regular).unwrap(), b"keep");

        let target = directory.path().join("target.sock");
        let link = directory.path().join("link.sock");
        let listener = LocalSocketListener::bind_owner_only(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            retire_socket_endpoint(&link).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(target.exists());

        drop(listener);
        retire_socket_endpoint(&target).unwrap();
        assert!(!target.exists());
        let error = LocalSocketStream::connect(&target).await.err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn named_pipe_instances_are_unsupported_on_this_host() {
        assert_eq!(
            LocalPipeClient::open("kernal-api-unsupported")
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            OwnerOnlyPipeInstance::create("kernal-api-unsupported", true)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::Unsupported
        );
    }
}

#[cfg(windows)]
mod named_pipe {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn unique_endpoint(label: &str) -> String {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        format!(
            r"\\.\pipe\kernal-api-owner-only-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[tokio::test]
    async fn canceled_connect_retains_instance_until_owner_drops_it() {
        let endpoint = unique_endpoint("cancel");
        let server = OwnerOnlyPipeInstance::create(&endpoint, true).unwrap();
        {
            let mut pending = Box::pin(server.connect());
            let mut cx = Context::from_waker(Waker::noop());
            assert!(pending.as_mut().poll(&mut cx).is_pending());
        }
        assert!(OwnerOnlyPipeInstance::create(&endpoint, true).is_err());
        let client = LocalPipeClient::open(&endpoint).unwrap();
        server.connect().await.unwrap();
        drop(client);
        drop(server);
        // The canceled connect left an overlapped ConnectNamedPipe that the
        // runtime still owns; Windows keeps the instance (and so the name)
        // until that completion is reaped, which happens asynchronously after
        // the drop. Assert the name is released within a bounded time while the
        // runtime runs, not synchronously at drop.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let _replacement = loop {
            match OwnerOnlyPipeInstance::create(&endpoint, true) {
                Ok(replacement) => break replacement,
                Err(error) if std::time::Instant::now() < deadline => {
                    assert_eq!(error.raw_os_error(), Some(5), "unexpected error: {error}");
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(error) => panic!("pipe name not released after owner drop: {error}"),
            }
        };
    }

    #[tokio::test]
    async fn connected_client_preserves_busy_error_and_byte_io() {
        let endpoint = unique_endpoint("client");
        let mut server = OwnerOnlyPipeInstance::create(&endpoint, true).unwrap();
        let mut client = LocalPipeClient::open(&endpoint).unwrap();
        server.connect().await.unwrap();
        assert_eq!(
            LocalPipeClient::open(&endpoint)
                .err()
                .unwrap()
                .raw_os_error(),
            Some(231)
        );
        assert_eq!(poll_fn(|cx| client.poll_write(cx, b"x")).await.unwrap(), 1);
        let mut bytes = [0; 1];
        assert_eq!(
            poll_fn(|cx| server.poll_read(cx, &mut bytes))
                .await
                .unwrap(),
            1
        );
        assert_eq!(bytes, *b"x");
        poll_fn(|cx| client.poll_shutdown(cx)).await.unwrap();
    }

    #[tokio::test]
    async fn first_instance_is_exclusive_but_pool_instances_can_follow() {
        let endpoint = unique_endpoint("instance");
        let first = OwnerOnlyPipeInstance::create(&endpoint, true).unwrap();
        assert!(OwnerOnlyPipeInstance::create(&endpoint, true).is_err());
        let second = OwnerOnlyPipeInstance::create(&endpoint, false).unwrap();
        drop((first, second));
        let _replacement = OwnerOnlyPipeInstance::create(&endpoint, true).unwrap();
    }

    #[test]
    fn nul_is_rejected_before_native_creation() {
        assert_eq!(
            LocalPipeClient::open("invalid\0pipe").err().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            OwnerOnlyPipeInstance::create("invalid\0pipe", true)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[tokio::test]
    async fn pathname_sockets_are_unsupported_and_retirement_is_a_no_op() {
        let path = std::path::Path::new(r"C:\kernal-api-missing\endpoint.sock");
        retire_socket_endpoint(path).unwrap();
        assert_eq!(
            LocalSocketListener::bind_owner_only(path)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            LocalSocketStream::connect(path).await.err().unwrap().kind(),
            io::ErrorKind::Unsupported
        );
    }
}
