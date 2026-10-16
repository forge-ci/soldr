//! Unit tests for `session_transport`, split out to keep that file under the
//! 1,000-line production-source ceiling.

use super::*;

#[test]
fn pre_output_error_is_attributed_without_output() {
    let err = SessionError::pre_output(io::Error::other("connect refused"));
    assert!(
        !err.output_started,
        "a setup failure must be identified as pre-output"
    );
}

#[test]
fn compile_session_uses_only_the_serialized_client_environment() {
    let start = compile_session_start(
        &["rustc".into(), "--version".into()],
        "/workspace".into(),
        vec![running_process::broker::protocol_v2::SessionEnvVar {
            key: "CLIENT_ONLY".into(),
            value: "present".into(),
        }],
    );

    assert!(start.clear_inherited_env);
    assert_eq!(
        start.environment_policy,
        running_process::broker::protocol_v2::EnvironmentPolicy::Clear as i32
    );
    assert_eq!(start.env.len(), 1);
    assert_eq!(start.env[0].key, "CLIENT_ONLY");
}

#[test]
fn busy_class_is_bounded_and_missing_endpoints_are_concrete() {
    assert!(broker_connect_is_busy(
        &io::Error::from_raw_os_error(231),
        Duration::from_millis(500)
    ));
    assert!(!broker_connect_is_busy(
        &io::Error::new(io::ErrorKind::NotFound, "absent"),
        Duration::ZERO
    ));
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        assert!(broker_connect_is_busy(
            &io::Error::from_raw_os_error(2),
            Duration::from_millis(1)
        ));
        assert!(!broker_connect_is_busy(
            &io::Error::from_raw_os_error(2),
            Duration::from_millis(51)
        ));
    }
}

#[test]
fn only_a_relay_lost_before_any_output_is_retried() {
    let lost = |output_started, kind, text: &str| SessionError {
        output_started,
        broker_unreachable: false,
        source: io::Error::new(kind, text.to_string()),
    };
    let eof = io::ErrorKind::UnexpectedEof;
    assert!(relay_lost_before_output(&lost(
        false,
        eof,
        "SESSION relay closed before Exit"
    )));
    // Output already reached the terminal: a retry would print it twice.
    assert!(!relay_lost_before_output(&lost(
        true,
        eof,
        "SESSION relay closed before Exit"
    )));
    // A policy refusal or a timeout is not a lost relay.
    assert!(!relay_lost_before_output(&lost(
        false,
        io::ErrorKind::Other,
        "broker refused the daemon route"
    )));
    assert!(!relay_lost_before_output(&lost(
        false,
        io::ErrorKind::TimedOut,
        "broker first-response deadline exceeded"
    )));
    let mut unreachable = lost(false, eof, "SESSION relay closed before Exit");
    unreachable.broker_unreachable = true;
    assert!(!relay_lost_before_output(&unreachable));
}

#[test]
fn hello_retry_is_limited_to_pre_reply_disconnects() {
    assert!(broker_hello_retryable(&io::Error::other(
        "broker closed before Hello reply"
    )));
    assert!(broker_hello_retryable(&io::Error::new(
        io::ErrorKind::ConnectionReset,
        "replaced broker"
    )));
    assert!(!broker_hello_retryable(&io::Error::other(
        "broker refused the daemon route"
    )));
    assert!(!broker_hello_retryable(&io::Error::new(
        io::ErrorKind::TimedOut,
        "route acquisition ceiling"
    )));
}

#[test]
fn relayed_diagnostic_suppresses_the_silent_fault_annotation() {
    assert!(mark_relayed_output(
        b"compiler terminated by a Unix signal\n"
    ));
    assert!(crate::exit_guard::spoke());
    assert!(!crate::exit_guard::needs_annotation(
        -1,
        crate::exit_guard::spoke()
    ));
}

#[test]
fn accepted_relay_that_never_negotiates_is_bounded() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let (mut client, _server) = tokio::io::duplex(64);
        let error = read_negotiated_with_deadlines(
            &mut client,
            1,
            crate::broker_deadlines::BrokerDeadlines {
                busy_budget: Duration::from_millis(10),
                first_response: Duration::from_millis(20),
                progress_silence: Duration::from_millis(20),
                route_ceiling: Duration::from_secs(1),
            },
        )
        .await
        .expect_err("a silent relay must not wait forever");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let text = error.to_string();
        assert!(text.contains("first-response deadline"), "{text}");
        assert!(text.contains("SOLDR_BROKER_FIRST_RESPONSE_MS"), "{text}");
    });
}

#[test]
fn continuous_progress_is_still_bounded_by_route_ceiling() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let writer = tokio::spawn(async move {
            let mut elapsed_ms = 1_u64;
            loop {
                let progress = crate::broker_server::RouteProgress {
                    stage: "probe".into(),
                    attempt: 3,
                    elapsed_ms,
                    latest_result: "daemon still starting".into(),
                    retry_after_ms: 0,
                };
                let frame = Frame {
                    envelope_version: running_process::broker::protocol::PROTOCOL_VERSION,
                    kind: FrameKind::Event as i32,
                    payload_protocol: crate::broker_server::ROUTE_PROGRESS_PAYLOAD_PROTOCOL,
                    payload: progress.encode_to_vec(),
                    request_id: 2476,
                    payload_encoding: PayloadEncoding::None as i32,
                    deadline_unix_ms: 0,
                    traceparent: String::new(),
                    tracestate: String::new(),
                };
                let bytes = encode_framed(&frame).expect("encode progress");
                if server.write_all(&bytes).await.is_err() {
                    break;
                }
                elapsed_ms += 5;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let error = read_negotiated_with_deadlines(
            &mut client,
            2476,
            crate::broker_deadlines::BrokerDeadlines {
                busy_budget: Duration::from_millis(100),
                first_response: Duration::from_millis(200),
                // 40x the 5ms progress cadence: a contended runner's late
                // scheduler wake must never turn the expected *ceiling*
                // timeout into a *silence* timeout (four Windows-lane
                // failures on 2026-08-16 with the old 20ms budget).
                progress_silence: Duration::from_millis(200),
                route_ceiling: Duration::from_millis(400),
            },
        )
        .await
        .expect_err("progress must not defeat the absolute ceiling");
        writer.abort();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("route acquisition ceiling"));
    });
}

#[test]
fn compile_service_that_never_publishes_is_bounded() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let (mut client, _server) = tokio::io::duplex(64);
        let err = pump_session_output_with_timeout(
            &mut client,
            std::time::Duration::from_millis(20),
            &mut RelayProbe::disabled(),
        )
        .await
        .expect_err("a silent compile service must time out");
        assert_eq!(err.source.kind(), io::ErrorKind::TimedOut);
        assert!(!err.output_started);
    });
}

/// A broker that has accepted the connection but answers `delay` after the
/// Hello: the reply frame a busy-but-alive broker eventually sends.
async fn late_negotiated_reply(mut server: tokio::io::DuplexStream, delay: Duration) {
    use prost::Message as _;
    use running_process::broker::protocol::{
        hello_reply, Negotiated, PayloadEncoding, CONTROL_PAYLOAD_PROTOCOL,
    };
    tokio::time::sleep(delay).await;
    let reply = HelloReply {
        result: Some(hello_reply::Result::Negotiated(Negotiated {
            backend_pipe: "late-pipe".to_string(),
            daemon_version: "9.9.9".to_string(),
            ..Default::default()
        })),
    };
    let frame = Frame {
        envelope_version: ENVELOPE_VERSION as u32,
        kind: FrameKind::Response as i32,
        payload_protocol: CONTROL_PAYLOAD_PROTOCOL,
        payload: reply.encode_to_vec(),
        request_id: 1,
        payload_encoding: PayloadEncoding::None as i32,
        ..Default::default()
    };
    let bytes = encode_framed(&frame).expect("encode reply");
    server.write_all(&bytes).await.expect("write reply");
    server.flush().await.expect("flush reply");
    // Hold the stream open so EOF never races the client's read.
    tokio::time::sleep(Duration::from_secs(60)).await;
}

/// soldr#3449: a broker that answers 3.4 s after the Hello -- what a loaded
/// 4-vCPU Windows runner produced -- completes under the default budgets. Time
/// is paused, so the wait costs nothing.
#[test]
fn a_busy_broker_answering_after_3_4_seconds_completes_under_the_defaults() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let (mut client, server) = tokio::io::duplex(4096);
        tokio::spawn(late_negotiated_reply(server, Duration::from_millis(3400)));
        let route = read_negotiated_with_deadlines(
            &mut client,
            1,
            crate::broker_deadlines::BrokerDeadlines::defaults(),
        )
        .await
        .expect("a busy broker must not be treated as dead");
        assert_eq!(route.backend_pipe, "late-pipe");
    });
}

/// The other half: a broker that never answers still fails, at the
/// first-response deadline, well inside the route ceiling.
#[test]
fn a_broker_that_never_answers_still_fails_at_the_first_response_deadline() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let deadlines = crate::broker_deadlines::BrokerDeadlines::defaults();
        let (mut client, _server) = tokio::io::duplex(64);
        let started = tokio::time::Instant::now();
        let error = read_negotiated_with_deadlines(&mut client, 1, deadlines)
            .await
            .expect_err("a silent broker must fail");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            error.to_string().contains("first-response deadline"),
            "{error}"
        );
        let waited = started.elapsed();
        assert!(waited >= deadlines.first_response, "{waited:?}");
        assert!(waited < deadlines.route_ceiling, "{waited:?}");
    });
}

#[test]
fn relay_probe_records_first_and_metadata_frames_only_when_enabled() {
    let mut off = RelayProbe::disabled();
    off.on_frame();
    off.on_output(br#"{"emit":"metadata"}"#);
    assert!(off.first_frame_ms.is_none() && off.metadata_frame_ms.is_none());

    let mut on = RelayProbe::disabled();
    on.enabled = true;
    on.on_frame();
    on.on_output(b"warning: unused\n");
    assert!(on.first_frame_ms.is_some());
    assert!(on.metadata_frame_ms.is_none());
    on.on_output(br#"{"artifact":"x.rmeta","emit":"metadata"}"#);
    let first = on.metadata_frame_ms;
    assert!(first.is_some());
    on.on_output(br#"{"emit":"metadata"}"#);
    assert_eq!(
        on.metadata_frame_ms, first,
        "only the first metadata frame counts"
    );
}

/// soldr#3815: a lost or unreachable daemon connection ends in a stand-alone
/// error naming the route, elapsed time, and recovery commands.
#[test]
fn connection_loss_is_a_loud_error_with_recovery_steps() {
    let err = SessionError::broker_unreachable(io::Error::new(
        io::ErrorKind::ConnectionRefused,
        "nothing listening",
    ));
    assert!(is_connection_failure(&err));
    let plain = connection_failure_message(&err, "soldr-daemon-x", Duration::from_secs(30), false);
    assert!(plain.contains("could not connect") && plain.contains("soldr-daemon-x"));
    assert!(plain.contains("30.0s") && plain.contains("soldr daemon status"));
    assert!(plain.contains("soldr daemon stop") && plain.contains("soldr logs paths"));
    assert!(!plain.contains('\x1b'));
    assert!(connection_failure_message(&err, "x", Duration::ZERO, true).starts_with("\x1b[1;31m"));
    let eof = SessionError {
        output_started: true,
        broker_unreachable: false,
        source: io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "SESSION relay closed before Exit",
        ),
    };
    assert!(connection_failure_message(&eof, "x", Duration::ZERO, false)
        .contains("lost the connection"));
}

/// A long compile on a live daemon is not a connection failure.
#[test]
fn reply_timeout_is_not_a_connection_failure() {
    let err = SessionError {
        output_started: false,
        broker_unreachable: false,
        source: io::Error::new(
            io::ErrorKind::TimedOut,
            "SESSION compile reply did not complete within 1800s",
        ),
    };
    assert!(!is_connection_failure(&err));
}
