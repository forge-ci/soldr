//! Unit coverage split from `broker_server.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;

#[test]
fn shutdown_signal_survives_before_accept_loop_waits() {
    let shutdown = crate::daemon::shutdown_signal::ShutdownSignal::default();
    request_shutdown(&shutdown);

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            tokio::time::timeout(std::time::Duration::from_millis(100), shutdown.wait())
                .await
                .expect("an early shutdown signal must survive until someone waits");
        });
}

/// soldr#3158: `serve_loop` fans one signal out to the accept loop, the route
/// reaper, and the RSS watchdog. Signalling it with `Notify::notify_one()`
/// reached exactly one of them -- the reaper, which registers first -- so the
/// accept loop that owns the process exit never broke and every
/// `soldr broker stop` fell through to its force-kill deadline.
#[test]
fn shutdown_request_reaches_every_serve_loop_waiter() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let shutdown = Arc::new(crate::daemon::shutdown_signal::ShutdownSignal::default());
    let woken = Arc::new(AtomicUsize::new(0));

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            // One per loop `serve_loop` parks on this signal.
            let waiters: Vec<_> = ["accept", "route-reaper", "rss-watchdog"]
                .into_iter()
                .map(|_| {
                    let shutdown = Arc::clone(&shutdown);
                    let woken = Arc::clone(&woken);
                    tokio::spawn(async move {
                        shutdown.wait().await;
                        woken.fetch_add(1, Ordering::Release);
                    })
                })
                .collect();
            tokio::task::yield_now().await;

            request_shutdown(&shutdown);

            for waiter in waiters {
                tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
                    .await
                    .expect("one shutdown request must reach every serve_loop waiter")
                    .expect("waiter task must not panic");
            }
        });

    assert_eq!(woken.load(std::sync::atomic::Ordering::Acquire), 3);
}

#[test]
fn direct_handoff_eligibility_requires_platform_capability_and_token() {
    use running_process::broker::capabilities::CAP_HANDLE_PASSING;

    assert_eq!(
        direct_handoff_eligible(CAP_HANDLE_PASSING, &[1]),
        crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows
    );
    assert!(!direct_handoff_eligible(0, &[1]));
    assert!(!direct_handoff_eligible(CAP_HANDLE_PASSING, &[]));
}

#[test]
fn windows_handoff_ready_uses_async_original_pipe() {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let endpoint = format!(
                "soldr-handoff-ready-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            );
            let listener = create_listener(&endpoint).expect("named-pipe listener");
            let client_name =
                running_process::broker::server::singleton_bind::wrap_socket_name(&endpoint)
                    .expect("named-pipe name");

            let server = async {
                let mut stream = listener.accept().await.expect("accept client");
                let duplicated = duplicate_handoff_stream(&stream)
                    .expect("duplicate overlapped named-pipe handle for handoff");
                drop(duplicated);

                let ack = running_process::broker::protocol::HandoffAck {
                    token: vec![1, 2, 3, 4],
                    accepted: true,
                    error_detail: String::new(),
                    correlation_id: 42,
                };
                write_handoff_ready_async(&mut stream, &ack)
                    .await
                    .expect("write ready event asynchronously on original pipe");
            };
            let client = async {
                let mut stream = crate::platform::ipc::connect::connect_local_socket(client_name)
                    .await
                    .expect("connect client");
                let body = read_frame_async(&mut stream)
                    .await
                    .expect("read ready event");
                let frame = Frame::decode(body.as_slice()).expect("decode ready frame");
                let ack =
                    running_process::broker::protocol::HandoffAck::decode(frame.payload.as_slice())
                        .expect("decode handoff acknowledgement");
                assert!(ack.accepted);
                assert_eq!(ack.correlation_id, 42);
                assert_eq!(ack.token, vec![1, 2, 3, 4]);
            };

            tokio::join!(server, client);
        });
}

#[test]
fn broker_instance_identity_includes_the_complete_image_digest() {
    let first = format_broker_instance_id("0.9.0", &"a".repeat(64));
    let second = format_broker_instance_id("0.9.0", &"b".repeat(64));
    assert_ne!(first, second, "same-version images must not alias");
    assert!(first.ends_with(&"a".repeat(64)));
}

#[test]
fn route_heartbeat_stays_inside_the_client_silence_budget() {
    assert_eq!(
        route_progress_heartbeat_interval(Duration::from_secs(5)),
        Duration::from_secs(1)
    );
    assert_eq!(
        route_progress_heartbeat_interval(Duration::from_millis(30)),
        Duration::from_millis(10)
    );
    assert!(
        route_progress_heartbeat_interval(Duration::from_millis(30)) < Duration::from_millis(30)
    );
}

#[test]
fn progress_and_attestation_are_protobuf_roundtrips() {
    let progress = RouteProgress {
        stage: "spawn".into(),
        attempt: 3,
        elapsed_ms: 42,
        latest_result: "waiting".into(),
        retry_after_ms: 7,
    };
    assert_eq!(
        RouteProgress::decode(progress.encode_to_vec().as_slice()).unwrap(),
        progress
    );
    let bytes = client_host_attestation();
    let attestation = ClientHostAttestation::decode(bytes.as_slice()).unwrap();
    assert!(!attestation.machine_id.is_empty());
    assert!(!attestation.boot_id.is_empty());
}

#[test]
fn deadline_env_values_are_positive_and_have_contract_defaults() {
    let deadlines = BrokerDeadlines::from_env();
    assert!(!deadlines.first_response.is_zero());
    assert!(!deadlines.progress_silence.is_zero());
    assert!(!deadlines.route_ceiling.is_zero());
}

#[test]
fn mismatched_machine_attestation_is_refused_as_shared_home() {
    let hello = Hello {
        client_lib_name: "soldr".into(),
        peer_attestation_nonce: ClientHostAttestation {
            machine_id: "another-machine".into(),
            boot_id: "another-boot".into(),
        }
        .encode_to_vec(),
        ..Default::default()
    };
    let request = Frame::request(CONTROL_PAYLOAD_PROTOCOL, hello.encode_to_vec());
    let reply = validate_client_host(&request).expect("foreign machine must be refused");
    let Some(hello_reply::Result::Refused(refused)) = reply.result else {
        panic!("expected refusal");
    };
    assert_eq!(
        ErrorCode::try_from(refused.code),
        Ok(ErrorCode::ErrorPeerRejected)
    );
    assert!(refused.reason.contains("shared Soldr home"));
    assert!(refused.details.contains_key("client_machine_id"));
    assert!(refused.details.contains_key("broker_machine_id"));
}

#[test]
fn macos_listener_restricts_socket_permissions_after_bind() {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::MacOs {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let socket = temp.path().join("soldr-broker.sock");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _context = runtime.enter();
    let listener = create_listener(socket.to_str().expect("UTF-8 socket path"))
        .expect("macOS broker listener");
    let mode = crate::platform::fs::permissions::mode(&socket).expect("socket mode") & 0o777;
    assert_eq!(mode, 0o600);
    drop(listener);
}

#[test]
fn in_flight_launch_refusal_retries_within_fifty_ms() {
    let ceiling = effective_retry_after_ms("backend spawn already in progress", 1_000);
    assert_eq!(ceiling, IN_FLIGHT_LAUNCH_RETRY_MAX_MS);
    for _ in 0..500 {
        let delay = route_retry_delay(ceiling);
        assert!(delay <= Duration::from_millis(50), "{delay:?}");
    }
}

#[test]
fn other_rate_limited_refusals_keep_the_server_hint() {
    assert_eq!(
        effective_retry_after_ms("backend spawn budget exhausted", 1_000),
        1_000
    );
}
