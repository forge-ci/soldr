//! Round-trip unit tests for the daemon wire encode/decode layer.
//! Extracted from `wire.rs` (soldr#1368) via `#[path = "wire_tests.rs"]`.

use super::*;
use crate::daemon::protocol::{
    BuildCacheSummary, BuildLogPaths, BuildMissReason, CacheFlushInfo, CacheFlushStepInfo,
    Response, ShutdownAck, StatusInfo,
};

#[test]
fn record_target_touch_round_trips() {
    let req = Request::RecordTargetTouch {
        path: "/tmp/target".into(),
        unix_seconds: 1_700_000_000,
    };
    let bytes = encode_request(&req);
    let decoded = decode_request(&bytes).expect("decode");
    assert!(matches!(
        decoded,
        Request::RecordTargetTouch { ref path, unix_seconds }
            if path == "/tmp/target" && unix_seconds == 1_700_000_000
    ));
}

#[test]
fn target_registry_verbs_round_trip() {
    use crate::daemon::protocol::TargetRegistryRow;

    assert!(matches!(
        decode_request(&encode_request(&Request::ListTargetRegistry)).expect("decode"),
        Request::ListTargetRegistry
    ));
    assert!(matches!(
        decode_request(&encode_request(&Request::RemoveTargetRegistry {
            paths: vec!["/w/target".into(), "/x/target".into()],
        }))
        .expect("decode"),
        Request::RemoveTargetRegistry { paths }
            if paths == vec!["/w/target".to_string(), "/x/target".to_string()]
    ));

    let rows = vec![TargetRegistryRow {
        path: "/w/target".into(),
        last_used: 1_700_000_000,
    }];
    assert!(matches!(
        decode_response(&encode_response(&Response::TargetRegistryRows(rows.clone())))
            .expect("decode"),
        Response::TargetRegistryRows(decoded) if decoded == rows
    ));
    assert!(matches!(
        decode_response(&encode_response(&Response::TargetRegistryRemoved {
            removed: 2
        }))
        .expect("decode"),
        Response::TargetRegistryRemoved { removed: 2 }
    ));
}

#[test]
fn status_request_round_trips() {
    let bytes = encode_request(&Request::Status);
    assert!(matches!(
        decode_request(&bytes).expect("decode"),
        Request::Status
    ));
}

#[test]
fn flush_caches_request_round_trips() {
    let bytes = encode_request(&Request::FlushCaches);
    assert!(matches!(
        decode_request(&bytes).expect("decode"),
        Request::FlushCaches
    ));
}

#[test]
fn resident_capacity_control_frames_round_trip() {
    let acquire = Request::AcquireResidentCapacity { permits: 3 };
    let bytes = encode_request(&acquire);
    assert!(matches!(
        decode_request(&bytes).expect("decode acquire"),
        Request::AcquireResidentCapacity { permits: 3 }
    ));

    let release = Request::ReleaseResidentCapacity;
    let bytes = encode_request(&release);
    assert!(matches!(
        decode_request(&bytes).expect("decode release"),
        Request::ReleaseResidentCapacity
    ));

    let acquired = Response::ResidentCapacityAcquired { permits: 3 };
    let bytes = encode_response(&acquired);
    assert!(matches!(
        decode_response(&bytes).expect("decode acquired"),
        Response::ResidentCapacityAcquired { permits: 3 }
    ));
}

#[test]
fn cache_flush_response_preserves_incomplete_step_details() {
    let info = CacheFlushInfo {
        complete: false,
        pending_writes_drained: true,
        index_writer_drained: true,
        steps: vec![
            CacheFlushStepInfo {
                step: "artifact_store".into(),
                status: "completed".into(),
                error: None,
            },
            CacheFlushStepInfo {
                step: "depgraph".into(),
                status: "failed".into(),
                error: Some("disk full".into()),
            },
        ],
        artifact_entries: 41,
        metadata_entries: 73,
    };
    let decoded =
        decode_response(&encode_response(&Response::CacheFlushed(info.clone()))).expect("decode");
    match decoded {
        Response::CacheFlushed(decoded) => assert_eq!(decoded, info),
        other => panic!("expected CacheFlushed, got {other:?}"),
    }
}

#[test]
fn build_log_inputs_verb_round_trips() {
    // soldr#1814 slice 2a. Every `Event` field must survive, because the
    // build log's compile timeline is derived entirely from them — a field
    // lost on the wire silently produces an incomplete log rather than an
    // error.
    use crate::daemon::db::{Event, EventKind};

    assert!(matches!(
        decode_request(&encode_request(&Request::BuildLogInputs { session_id: 42 }))
            .expect("decode"),
        Request::BuildLogInputs { session_id: 42 }
    ));

    let events = vec![
        Event {
            ts_ms: 1_700_000_000_000,
            session_id: Some(42),
            kind: EventKind::CompileStart,
            crate_name: Some("indexmap".into()),
            duration_us: None,
            target_dir: Some("/w/target".into()),
            exit_code: None,
        },
        Event {
            ts_ms: 1_700_000_001_000,
            session_id: Some(42),
            kind: EventKind::CompileEnd,
            crate_name: Some("indexmap".into()),
            duration_us: Some(987_654),
            target_dir: None,
            exit_code: Some(0),
        },
    ];
    let resp = Response::BuildLogInputs {
        events: events.clone(),
        record: None,
    };
    match decode_response(&encode_response(&resp)).expect("decode") {
        Response::BuildLogInputs {
            events: decoded,
            record,
        } => {
            assert_eq!(decoded, events);
            assert!(record.is_none(), "absent record must stay absent");
        }
        other => panic!("expected BuildLogInputs, got {other:?}"),
    }
}

#[test]
fn attach_build_log_history_round_trips() {
    // soldr#1814 slice 2d. Every field must survive: the daemon reconstructs
    // the build record from this payload alone, so anything dropped on the
    // wire silently degrades `soldr logs` rather than erroring.
    use crate::daemon::protocol::{
        BuildCacheSummary, BuildLogHistoryUpdate, BuildLogPaths, BuildMissReason,
    };

    let update = BuildLogHistoryUpdate {
        session_id: 7,
        repo_root: "/w/repo".into(),
        started_at_ms: 1_700_000_000_000,
        ended_at_ms: 1_700_000_060_000,
        exit_code: 0,
        daemon_finalized: false,
        cache_summary: Some(BuildCacheSummary {
            hits: 23,
            misses: 5,
            non_cacheable: 1,
            errors: 0,
            compilations: 29,
            time_saved_ms: 12_345,
        }),
        miss_reasons: vec![BuildMissReason {
            reason: "key_mismatch".into(),
            count: 5,
        }],
        log_paths: Some(BuildLogPaths {
            zccache_session_id: Some("abc".into()),
            cache_dir: Some("/w/cache".into()),
            session_log_path: None,
            journal_path: None,
            session_stats_path: Some("/w/stats.json".into()),
            compile_journal_path: Some("/w/cj.jsonl".into()),
            archived_session_log_path: None,
            archived_journal_path: None,
            archived_session_stats_path: Some("/w/hist/stats.json".into()),
            archived_compile_journal_path: Some("/w/hist/cj.jsonl".into()),
            private_daemon_name: None,
        }),
    };

    match decode_request(&encode_request(&Request::AttachBuildLogHistory(Box::new(
        update.clone(),
    ))))
    .expect("decode")
    {
        Request::AttachBuildLogHistory(decoded) => assert_eq!(*decoded, update),
        other => panic!("expected AttachBuildLogHistory, got {other:?}"),
    }
}

#[test]
fn cargo_debug_warning_verb_round_trips() {
    // soldr#1814 slice 2c. Both boolean outcomes must survive: `false` is the
    // throttled answer, and a decode that collapsed it to the default would
    // re-emit a warning the daemon already suppressed.
    match decode_request(&encode_request(&Request::ShouldWarnCargoDebugDefault {
        repo_root: "/w/repo".into(),
    }))
    .expect("decode")
    {
        Request::ShouldWarnCargoDebugDefault { repo_root } => assert_eq!(repo_root, "/w/repo"),
        other => panic!("expected ShouldWarnCargoDebugDefault, got {other:?}"),
    }
    for emit in [true, false] {
        match decode_response(&encode_response(&Response::CargoDebugWarning { emit }))
            .expect("decode")
        {
            Response::CargoDebugWarning { emit: decoded } => assert_eq!(decoded, emit),
            other => panic!("expected CargoDebugWarning, got {other:?}"),
        }
    }
}

#[test]
fn compile_stats_verb_round_trips() {
    use crate::daemon::protocol::{CompileStatsInfo, StagedProfileInfo};
    assert!(matches!(
        decode_request(&encode_request(&Request::CompileStats)).expect("decode"),
        Request::CompileStats
    ));
    let info = CompileStatsInfo {
        total_compilations: 100,
        cache_hits: 73,
        cache_misses: 21,
        non_cacheable: 4,
        compile_errors: 2,
        time_saved_ms: 123_456,
        staged_profile: Some(StagedProfileInfo {
            counters: [("published".to_string(), 7)].into(),
            ..Default::default()
        }),
    };
    match decode_response(&encode_response(&Response::CompileStats(info.clone()))).expect("decode")
    {
        Response::CompileStats(decoded) => assert_eq!(decoded, info),
        other => panic!("expected CompileStats, got {other:?}"),
    }
}

#[test]
fn cook_lookup_round_trips_with_all_fields() {
    let req = Request::CookLookup {
        recipe_hash: [0x42; 32],
        target_triple: "x86_64-pc-windows-msvc".into(),
        profile: "release".into(),
        channel: "1.94.1".into(),
        rustc_version: "rustc 1.94.1".into(),
        origin_url_normalized: Some("https://github.com/zackees/soldr".into()),
        branch_lineage: vec!["feature/cook".into(), "main".into()],
    };
    let bytes = encode_request(&req);
    let decoded = decode_request(&bytes).expect("decode");
    match decoded {
        Request::CookLookup {
            recipe_hash,
            target_triple,
            profile,
            channel,
            rustc_version,
            origin_url_normalized,
            branch_lineage,
        } => {
            assert_eq!(recipe_hash, [0x42; 32]);
            assert_eq!(target_triple, "x86_64-pc-windows-msvc");
            assert_eq!(profile, "release");
            assert_eq!(channel, "1.94.1");
            assert_eq!(rustc_version, "rustc 1.94.1");
            assert_eq!(
                origin_url_normalized.as_deref(),
                Some("https://github.com/zackees/soldr")
            );
            assert_eq!(branch_lineage, vec!["feature/cook", "main"]);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn cook_record_round_trips_with_sha_validation() {
    let req = Request::CookRecord {
        recipe_hash: [0x11; 32],
        target_triple: "x86_64-unknown-linux-gnu".into(),
        profile: "release".into(),
        channel: "1.94.1".into(),
        rustc_version: "rustc 1.94.1".into(),
        sha256: [0xAA; 32],
        size_bytes: 4_096,
        origin_url_normalized: None,
        branch_name: Some("main".into()),
        cook_cmd_summary: "cook --release".into(),
        compile_duration_ms: 123_000,
        save_elapsed_ms: 4_000,
    };
    let bytes = encode_request(&req);
    let Request::CookRecord {
        recipe_hash,
        sha256,
        size_bytes,
        branch_name,
        compile_duration_ms,
        save_elapsed_ms,
        ..
    } = decode_request(&bytes).expect("decode")
    else {
        panic!("expected CookRecord");
    };
    assert_eq!(recipe_hash, [0x11; 32]);
    assert_eq!(sha256, [0xAA; 32]);
    assert_eq!(size_bytes, 4_096);
    assert_eq!(branch_name.as_deref(), Some("main"));
    assert_eq!(compile_duration_ms, 123_000);
    assert_eq!(save_elapsed_ms, 4_000);
}

#[test]
fn status_response_round_trips_with_cook_stats() {
    let info = StatusInfo {
        version: 7,
        pid: 4242,
        generation: 1_700_000_000_123,
        uptime_secs: 60,
        request_count: 17,
        cook_stats: Some(CookStats {
            entries: 3,
            total_bytes: 9_999,
            hits_this_session: 1,
        }),
        compile_backend: "embedded".to_string(),
        compile_jobs: 12,
        compile_jobs_source: "SOLDR_JOBS".to_string(),
        in_flight_compiles: 5,
    };
    let resp = Response::Status(info.clone());
    let bytes = encode_response(&resp);
    let decoded = decode_response(&bytes).expect("decode");
    match decoded {
        Response::Status(decoded_info) => assert_eq!(decoded_info, info),
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn build_session_started_round_trips_the_daemons_applied_limit() {
    // soldr#2023. The limit has to survive the wire intact: the client's
    // only job with it is an equality check against its own resolution, so
    // a value mangled in transit becomes a warning on every build (or,
    // worse, silence on a build that genuinely drifted).
    let response = Response::BuildSessionStarted {
        compile_jobs: 6,
        compile_jobs_source: "config.toml [jobs].max_parallel_compiles".to_string(),
    };
    let bytes = encode_response(&response);
    match decode_response(&bytes).expect("decode") {
        Response::BuildSessionStarted {
            compile_jobs,
            compile_jobs_source,
        } => {
            assert_eq!(compile_jobs, 6);
            assert_eq!(
                compile_jobs_source,
                "config.toml [jobs].max_parallel_compiles"
            );
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn shutdown_ack_round_trips_responder_generation() {
    let response = Response::ShuttingDown(ShutdownAck {
        pid: 4242,
        generation: 1_700_000_000_123,
    });
    let bytes = encode_response(&response);
    assert!(matches!(
        decode_response(&bytes).expect("decode"),
        Response::ShuttingDown(ShutdownAck {
            pid: 4242,
            generation: 1_700_000_000_123,
        })
    ));
}

#[test]
fn legacy_empty_shutdown_ack_decodes_with_zero_identity() {
    // v17 encoded WireResponse.shutting_down as an empty nested message.
    // v18 must continue to decode that shape so the client can pair it with
    // the immediately preceding v17 Status response.
    let legacy_bytes = [0x12, 0x00];
    assert!(matches!(
        decode_response(&legacy_bytes).expect("decode legacy shutdown ack"),
        Response::ShuttingDown(ShutdownAck {
            pid: 0,
            generation: 0,
        })
    ));
}

#[test]
fn cook_hit_response_round_trips() {
    let resp = Response::CookHit {
        sha256: [0xCC; 32],
        path: "/home/runner/.soldr/cache/cook/abcd.tar.zst".into(),
        size_bytes: 4_096,
        origin_url_normalized: Some("https://github.com/zackees/soldr".into()),
        matched_recipe_hash: Some([0x11; 32]),
        exact_recipe_match: false,
        branch_name: Some("main".into()),
        compile_duration_ms: 123_000,
        save_elapsed_ms: 4_000,
    };
    let bytes = encode_response(&resp);
    match decode_response(&bytes).expect("decode") {
        Response::CookHit {
            sha256,
            path,
            size_bytes,
            origin_url_normalized,
            matched_recipe_hash,
            exact_recipe_match,
            branch_name,
            compile_duration_ms,
            save_elapsed_ms,
        } => {
            assert_eq!(sha256, [0xCC; 32]);
            assert!(path.ends_with(".tar.zst"));
            assert_eq!(size_bytes, 4_096);
            assert_eq!(
                origin_url_normalized.as_deref(),
                Some("https://github.com/zackees/soldr")
            );
            assert_eq!(matched_recipe_hash, Some([0x11; 32]));
            assert!(!exact_recipe_match);
            assert_eq!(branch_name.as_deref(), Some("main"));
            assert_eq!(compile_duration_ms, 123_000);
            assert_eq!(save_elapsed_ms, 4_000);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn cook_miss_response_round_trips_with_multiple_hashes() {
    let resp = Response::CookMiss {
        previous_origin_recipe_hashes: vec![[1u8; 32], [2u8; 32], [3u8; 32]],
    };
    let bytes = encode_response(&resp);
    match decode_response(&bytes).expect("decode") {
        Response::CookMiss {
            previous_origin_recipe_hashes,
        } => {
            assert_eq!(previous_origin_recipe_hashes.len(), 3);
            assert_eq!(previous_origin_recipe_hashes[0], [1u8; 32]);
            assert_eq!(previous_origin_recipe_hashes[1], [2u8; 32]);
            assert_eq!(previous_origin_recipe_hashes[2], [3u8; 32]);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn retiring_response_round_trips() {
    let bytes = encode_response(&Response::Retiring);
    assert!(matches!(
        decode_response(&bytes).expect("decode"),
        Response::Retiring
    ));
}

// soldr#1838: `Retiring` must not be confusable with `Error` on the wire.
// Their whole purpose is that the client treats them oppositely -- degrade to
// direct rustc vs. hard-fail the build -- so a decode that blurred them would
// reintroduce #1837 silently.
#[test]
fn retiring_and_error_do_not_decode_to_each_other() {
    let retiring = encode_response(&Response::Retiring);
    let error = encode_response(&Response::Error("boom".into()));
    assert_ne!(
        retiring, error,
        "distinct variants must not encode identically"
    );
    assert!(!matches!(
        decode_response(&retiring).expect("decode"),
        Response::Error(_)
    ));
    assert!(!matches!(
        decode_response(&error).expect("decode"),
        Response::Retiring
    ));
}

#[test]
fn builds_response_round_trips_with_all_optional_fields() {
    let record = BuildRecord {
        session_id: 42,
        repo_root: "/r".into(),
        started_at_ms: 100,
        ended_at_ms: Some(500),
        exit_code: Some(0),
        total_wall_ms: Some(400),
        crate_count: 7,
        slowest_crate_us: Some(123_456),
        slowest_crate_name: Some("zccache".into()),
        cache_summary: Some(BuildCacheSummary {
            hits: 13,
            misses: 5,
            non_cacheable: 2,
            errors: 1,
            compilations: 21,
            time_saved_ms: 900,
        }),
        log_paths: Some(BuildLogPaths {
            zccache_session_id: Some("session-abc".into()),
            cache_dir: Some("/cache/zccache".into()),
            session_log_path: Some("/cache/zccache/logs/last-session.log".into()),
            journal_path: Some("/cache/zccache/logs/last-session.jsonl".into()),
            session_stats_path: Some("/cache/zccache/logs/last-session-stats.json".into()),
            compile_journal_path: Some("/cache/zccache/logs/compile_journal.jsonl".into()),
            archived_session_log_path: Some("/cache/zccache/history/42/last-session.log".into()),
            archived_journal_path: Some("/cache/zccache/history/42/last-session.jsonl".into()),
            archived_session_stats_path: Some(
                "/cache/zccache/history/42/last-session-stats.json".into(),
            ),
            archived_compile_journal_path: Some(
                "/cache/zccache/history/42/compile_journal.jsonl".into(),
            ),
            private_daemon_name: Some("soldr-dev-demo".into()),
        }),
        miss_reasons: vec![BuildMissReason {
            reason: "key_mismatch".into(),
            count: 5,
        }],
    };
    let resp = Response::Builds(vec![record.clone()]);
    let bytes = encode_response(&resp);
    match decode_response(&bytes).expect("decode") {
        Response::Builds(items) => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0], record);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn invalid_sha_length_is_a_decode_error() {
    // Build a WireCookTouch by hand with a bogus 16-byte sha.
    let bad = proto::WireRequest {
        kind: Some(proto::WireRequestKind::CookTouch(proto::WireCookTouch {
            sha256: vec![0u8; 16],
        })),
    };
    let mut bytes = Vec::new();
    prost::Message::encode(&bad, &mut bytes).expect("encode");
    let err = decode_request(&bytes).expect_err("must error");
    assert!(matches!(err, WireDecodeError::InvalidShaLength(16)));
}

#[test]
fn empty_request_oneof_is_a_decode_error() {
    let bytes = Vec::new(); // empty WireRequest with no oneof
    let err = decode_request(&bytes).expect_err("must error");
    assert!(matches!(err, WireDecodeError::EmptyOneof("Request")));
}

#[test]
fn event_kind_round_trips_through_u32() {
    for kind in [
        EventKind::SessionStart,
        EventKind::SessionEnd,
        EventKind::CompileStart,
        EventKind::CompileEnd,
    ] {
        let n = event_kind_to_u32(&kind);
        assert_eq!(u32_to_event_kind(n).unwrap(), kind);
    }
    assert!(matches!(
        u32_to_event_kind(99).unwrap_err(),
        WireDecodeError::UnknownEventKind(99)
    ));
}

#[test]
fn prost_tagged_bytes_prepends_the_tag() {
    let payload = proto::WireUnit {};
    let bytes = prost_tagged_bytes(&payload);
    assert_eq!(bytes.first().copied(), Some(REDB_TAG_PROST));
}
