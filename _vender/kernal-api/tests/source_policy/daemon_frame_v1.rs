//! Public contract for the opt-in frozen daemon Frame v1 facade.

#![cfg(feature = "daemon-frame-v1")]

use std::path::Path;

use kernal_api::daemon_frame_v1::{
    DaemonFrame, DaemonFrameCodec, DaemonFrameDecode, DaemonFrameError, DaemonFrameKind,
    DaemonPayloadEncoding,
};

const PRODUCT_PROTOCOL: u32 = 0x7A63;

kernal_api::register_daemon_frame_payload_protocol! {
    /// A product-selected protocol proves the facade macro does not own IDs.
    const TEST_PRODUCT_PROTOCOL: u32 = PRODUCT_PROTOCOL;
}

#[test]
fn request_and_response_wire_bytes_are_frozen() {
    let request = DaemonFrame::request(PRODUCT_PROTOCOL, b"ping".to_vec())
        .with_request_id(0x0102_0304_0506_0708);
    assert_eq!(
        DaemonFrameCodec::encode(&request).expect("encode fixed request"),
        [
            0x01, 0x16, 0x00, 0x00, 0x00, 0x08, 0x01, 0x18, 0xE3, 0xF4, 0x01, 0x22, 0x04, b'p',
            b'i', b'n', b'g', 0x28, 0x88, 0x8E, 0x98, 0xA8, 0xC0, 0xE0, 0x80, 0x81, 0x01,
        ]
    );

    let response = DaemonFrame::response_to(
        &DaemonFrame::request(PRODUCT_PROTOCOL, Vec::new()).with_request_id(7),
        b"pong".to_vec(),
    );
    assert_eq!(
        DaemonFrameCodec::encode(&response).expect("encode fixed response"),
        [
            0x01, 0x10, 0x00, 0x00, 0x00, 0x08, 0x01, 0x10, 0x01, 0x18, 0xE3, 0xF4, 0x01, 0x22,
            0x04, b'p', b'o', b'n', b'g', 0x28, 0x07,
        ]
    );
}

#[test]
fn incremental_decode_preserves_metadata_raw_discriminants_and_request_echo() {
    let request = DaemonFrame::request(TEST_PRODUCT_PROTOCOL, b"request".to_vec())
        .with_request_id(41)
        .with_raw_kind(37)
        .with_raw_payload_encoding(-7)
        .with_deadline_unix_ms(123_456)
        .with_trace_context("0123456789abcdef0123456789abcdef", "0123456789abcdef")
        .with_trace_state("vendor=one");
    let wire = DaemonFrameCodec::encode(&request).expect("encode metadata frame");
    let mut trailing = wire.clone();
    trailing.extend_from_slice(b"next-message");

    let DaemonFrameDecode::Frame { frame, consumed } =
        DaemonFrameCodec::decode(&trailing).expect("decode complete frame")
    else {
        panic!("expected a decoded frame");
    };
    assert_eq!(consumed, wire.len());
    assert_eq!(&trailing[consumed..], b"next-message");
    assert_eq!(frame.kind(), 37);
    assert_eq!(frame.payload_encoding(), -7);
    assert_eq!(frame.kind_classification(), DaemonFrameKind::Unknown(37));
    assert_eq!(
        frame.payload_encoding_classification(),
        DaemonPayloadEncoding::Unknown(-7)
    );
    assert_eq!(frame.payload_protocol(), TEST_PRODUCT_PROTOCOL);
    assert_eq!(frame.payload(), b"request");
    assert_eq!(frame.request_id(), 41);
    assert_eq!(frame.deadline_unix_ms(), 123_456);
    assert_eq!(frame.trace_id(), Some("0123456789abcdef0123456789abcdef"));
    assert_eq!(frame.span_id(), Some("0123456789abcdef"));
    assert_eq!(frame.trace_state(), "vendor=one");
    assert_eq!(
        DaemonFrameCodec::encode(&frame).expect("re-encode decoded metadata"),
        wire,
        "the facade must retain the exact private W3C wire fields"
    );

    let response = DaemonFrame::response_to(&frame, b"response".to_vec());
    assert_eq!(response.request_id(), 41);
    assert_eq!(response.payload_protocol(), TEST_PRODUCT_PROTOCOL);
    assert_eq!(response.trace_id(), frame.trace_id());
    assert_eq!(response.span_id(), frame.span_id());
    assert_eq!(response.trace_state(), "vendor=one");

    let ordinary_request = DaemonFrame::request(TEST_PRODUCT_PROTOCOL, Vec::new());
    assert_eq!(
        ordinary_request.kind_classification(),
        DaemonFrameKind::Request
    );
    assert_eq!(
        ordinary_request.payload_encoding_classification(),
        DaemonPayloadEncoding::None
    );
    assert_eq!(
        DaemonFrame::response_to(&ordinary_request, Vec::new()).kind_classification(),
        DaemonFrameKind::Response
    );
}

#[test]
fn partial_foreign_malformed_and_oversize_buffers_keep_the_frozen_contract() {
    assert_eq!(
        DaemonFrameCodec::decode(&[]).expect("empty is partial"),
        DaemonFrameDecode::NeedMoreBytes
    );
    assert_eq!(
        DaemonFrameCodec::decode(&[1, 1, 0, 0]).expect("partial header"),
        DaemonFrameDecode::NeedMoreBytes
    );
    assert_eq!(
        DaemonFrameCodec::decode(&[1, 1, 0, 0, 0]).expect("partial body"),
        DaemonFrameDecode::NeedMoreBytes
    );
    assert!(matches!(
        DaemonFrameCodec::decode(&[2]),
        Err(DaemonFrameError::UnsupportedFrameVersion {
            received: 2,
            expected: 1,
        })
    ));
    assert!(matches!(
        DaemonFrameCodec::decode(&[1, 1, 0, 0, 0, 0xFF]),
        Err(DaemonFrameError::MalformedFrame)
    ));

    let mut oversized = vec![1];
    oversized.extend_from_slice(&(16_u32 * 1024 * 1024 + 1).to_le_bytes());
    assert!(matches!(
        DaemonFrameCodec::decode(&oversized),
        Err(DaemonFrameError::FrameTooLarge {
            body_length,
            maximum,
        }) if body_length == 16 * 1024 * 1024 + 1 && maximum == 16 * 1024 * 1024
    ));
}

#[test]
fn facade_public_source_does_not_name_backend_or_runtime_types() {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/daemon_frame_v1.rs"),
    )
    .expect("read facade source");
    for line in source
        .lines()
        .map(str::trim_start)
        .filter(|line| line.starts_with("pub "))
    {
        for forbidden in [
            "running_process",
            "prost",
            "BytesMut",
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
