#![cfg(feature = "broker-v2")]

//! Public contract for the broker v2 daemon lifecycle facade.

use std::io::{Read, Write};
use std::time::Duration;

use kernal_api::broker_v2::{
    backend_endpoint, connect, connect_published_within, daemon_identity_path, publish, published,
    BrokerV2Error, Route,
};
use kernal_api::platform::ipc::{Endpoint, Listener};

fn unique_service(tag: &str) -> String {
    format!("kapi-test-{tag}-{}", std::process::id())
}

#[test]
fn unpublished_service_reports_daemon_not_running() {
    let service = unique_service("absent");
    let error =
        connect_published_within(&service, Duration::from_secs(5)).expect_err("nothing published");
    assert!(matches!(error, BrokerV2Error::NotPublished));
    assert!(error.daemon_not_running());
}

#[test]
fn published_daemon_round_trips_through_connect() {
    let service = unique_service("echo");
    let address = backend_endpoint(&service).expect("endpoint name");
    let endpoint = Endpoint::new(address.clone()).expect("endpoint");
    endpoint
        .ensure_owner_private_parent()
        .expect("private parent");
    endpoint.retire().expect("retire stale endpoint");
    let listener = Listener::bind_owner_only(&endpoint).expect("bind");
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().expect("accept");
        let mut buffer = [0_u8; 4];
        stream.read_exact(&mut buffer).expect("read");
        stream.write_all(&buffer).expect("echo");
    });

    let publication = publish(&service, &address, Some(30)).expect("publish");
    assert_eq!(
        publication.path(),
        daemon_identity_path(&service).unwrap().as_path()
    );
    let seen = published(&service).expect("read").expect("present");
    assert_eq!(seen.endpoint().address(), address);
    assert_eq!(seen.pid(), std::process::id());

    // No v2 broker is bound for this program, so connect falls back to the
    // published identity.
    let mut connection =
        connect(&service, &service, "0.0.0", Duration::from_millis(500)).expect("connect");
    assert_eq!(connection.route(), Route::Published);
    assert_eq!(connection.endpoint(), address);
    connection.write_all(b"ping").expect("write");
    let mut reply = [0_u8; 4];
    connection.read_exact(&mut reply).expect("read reply");
    assert_eq!(&reply, b"ping");
    server.join().expect("server thread");

    let path = publication.path().to_path_buf();
    drop(publication);
    assert!(
        !path.exists(),
        "dropping the publication removes the identity file"
    );
}

#[test]
fn a_bounded_published_connect_reports_daemon_not_running() {
    let service = unique_service("absent-within");
    let error =
        connect_published_within(&service, Duration::from_secs(30)).expect_err("nothing published");
    assert!(matches!(error, BrokerV2Error::NotPublished));
}

#[test]
fn a_spent_dial_budget_does_not_dial() {
    let service = unique_service("spent");
    let address = backend_endpoint(&service).expect("endpoint name");
    let endpoint = Endpoint::new(address.clone()).expect("endpoint");
    endpoint
        .ensure_owner_private_parent()
        .expect("private parent");
    endpoint.retire().expect("retire stale endpoint");
    let _listener = Listener::bind_owner_only(&endpoint).expect("bind");
    let _publication = publish(&service, &address, Some(30)).expect("publish");

    // A spent budget must not dial, even though a daemon is listening.
    let error = connect_published_within(&service, Duration::ZERO)
        .expect_err("a spent budget leaves no time to dial");
    assert!(
        matches!(&error, BrokerV2Error::BackendConnect(source) if source.kind() == std::io::ErrorKind::TimedOut),
        "{error}"
    );
}
