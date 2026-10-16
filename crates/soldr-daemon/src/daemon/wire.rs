//! Rust ↔ wire conversions for the daemon wire schema (issue #580).
//!
//! The pure-data half — the hand-written prost message types
//! (`proto`), the `.proto` schema file, the redb row-tag helpers, and
//! `WireDecodeError` — lives in [`crate::core::wire`] (#1490 Phase 0,
//! edge E2) and is re-exported here at its historical paths. The
//! `.proto` schema lives beside the prost types as
//! `src/core/wire.proto`; keep the two in sync.
//!
//! ## Why hand-written instead of `prost-build`
//!
//! The repo already uses this pattern in `src/rust_plan_proto.rs`. It
//! avoids the build.rs / `prost-build` toolchain dep, keeps generated
//! source out of `OUT_DIR` (CI-visible), and matches the existing
//! style. The schema is small enough that drift between the .proto
//! and the .rs is caught by the round-trip unit tests below — every
//! Rust ↔ wire ↔ Rust conversion is asserted equal.
//!
//! ## Backwards compatibility
//!
//! * **Wire path**: bumped `PROTOCOL_VERSION` from 4 to 5 in
//!   [`crate::daemon::protocol`]. Old (v4 / bincode-bodied) frames
//!   are rejected by the IPC version check before they reach
//!   bincode-decode, so there is no silent format confusion.
//! * **Redb path**: the persistent rows added by this issue use a
//!   one-byte tag prefix — `0x01` means "prost-encoded body
//!   follows". The read path tries the tagged decode first; on tag
//!   mismatch (or any decode failure) it falls back to the legacy
//!   bincode decoder so rows written before this migration still
//!   round-trip.
//!
//! ## Sha32 type-safety
//!
//! proto3 has no fixed-length byte arrays — every hash is `bytes` on
//! the wire and `Vec<u8>` in the generated Rust. The conversion
//! helpers in this module validate exact-32-byte length on decode and
//! return [`crate::daemon::protocol::WireDecodeError`] on mismatch,
//! preserving the `[u8; 32]` type-level guarantee at the public API.

use crate::daemon::db::{Event, EventKind};
use crate::daemon::protocol::{
    BuildCacheSummary, BuildLogPaths, BuildMissReason, BuildRecord, CacheFlushInfo,
    CacheFlushStepInfo, CompileStatsInfo, CookStats, Request, Response, ShutdownAck,
    StagedProfileInfo, StatusInfo, TargetRegistryRow, WireDecodeError,
};

/// Back-compat re-exports: these moved to `core::wire` (#1490 Phase 0,
/// edge E2) so `cache_lib` can persist prost-tagged redb rows without
/// an upward edge into `daemon`.
pub use crate::core::wire::{prost_tagged_bytes, proto, REDB_TAG_PROST};

use prost::Message as _;

// =========================================================================
// Sha32 helpers
// =========================================================================

fn sha_to_vec(sha: &[u8; 32]) -> Vec<u8> {
    sha.to_vec()
}

fn vec_to_sha(bytes: &[u8]) -> Result<[u8; 32], WireDecodeError> {
    if bytes.len() != 32 {
        return Err(WireDecodeError::InvalidShaLength(bytes.len()));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(bytes);
    Ok(out)
}

fn vec_to_optional_sha(bytes: &[u8]) -> Result<Option<[u8; 32]>, WireDecodeError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    vec_to_sha(bytes).map(Some)
}

// =========================================================================
// Encode / decode at the Request + Response boundary (wire framing)
// =========================================================================

pub fn encode_request(req: &Request) -> Vec<u8> {
    let wire: proto::WireRequest = req.into();
    let mut out = Vec::with_capacity(wire.encoded_len());
    wire.encode(&mut out).expect("Vec write is infallible");
    out
}

pub fn decode_request(bytes: &[u8]) -> Result<Request, WireDecodeError> {
    let wire = proto::WireRequest::decode(bytes).map_err(WireDecodeError::Prost)?;
    Request::try_from(wire)
}

pub fn encode_response(resp: &Response) -> Vec<u8> {
    let wire: proto::WireResponse = resp.into();
    let mut out = Vec::with_capacity(wire.encoded_len());
    wire.encode(&mut out).expect("Vec write is infallible");
    out
}

pub fn decode_response(bytes: &[u8]) -> Result<Response, WireDecodeError> {
    let wire = proto::WireResponse::decode(bytes).map_err(WireDecodeError::Prost)?;
    Response::try_from(wire)
}

// =========================================================================
// Conversions: persistent record types
// =========================================================================

pub fn build_record_to_wire(record: &BuildRecord) -> proto::WireBuildRecord {
    proto::WireBuildRecord {
        session_id: record.session_id,
        repo_root: record.repo_root.clone(),
        started_at_ms: record.started_at_ms,
        ended_at_ms: record.ended_at_ms,
        exit_code: record.exit_code,
        total_wall_ms: record.total_wall_ms,
        crate_count: record.crate_count,
        slowest_crate_us: record.slowest_crate_us,
        slowest_crate_name: record.slowest_crate_name.clone(),
        cache_summary: record
            .cache_summary
            .as_ref()
            .map(build_cache_summary_to_wire),
        log_paths: record.log_paths.as_ref().map(build_log_paths_to_wire),
        miss_reasons: record
            .miss_reasons
            .iter()
            .map(build_miss_reason_to_wire)
            .collect(),
    }
}

pub fn build_record_from_wire(wire: proto::WireBuildRecord) -> BuildRecord {
    BuildRecord {
        session_id: wire.session_id,
        repo_root: wire.repo_root,
        started_at_ms: wire.started_at_ms,
        ended_at_ms: wire.ended_at_ms,
        exit_code: wire.exit_code,
        total_wall_ms: wire.total_wall_ms,
        crate_count: wire.crate_count,
        slowest_crate_us: wire.slowest_crate_us,
        slowest_crate_name: wire.slowest_crate_name,
        cache_summary: wire.cache_summary.map(build_cache_summary_from_wire),
        log_paths: wire.log_paths.map(build_log_paths_from_wire),
        miss_reasons: wire
            .miss_reasons
            .into_iter()
            .map(build_miss_reason_from_wire)
            .collect(),
    }
}

fn build_cache_summary_to_wire(summary: &BuildCacheSummary) -> proto::WireBuildCacheSummary {
    proto::WireBuildCacheSummary {
        hits: summary.hits,
        misses: summary.misses,
        non_cacheable: summary.non_cacheable,
        errors: summary.errors,
        compilations: summary.compilations,
        time_saved_ms: summary.time_saved_ms,
    }
}

fn build_cache_summary_from_wire(wire: proto::WireBuildCacheSummary) -> BuildCacheSummary {
    BuildCacheSummary {
        hits: wire.hits,
        misses: wire.misses,
        non_cacheable: wire.non_cacheable,
        errors: wire.errors,
        compilations: wire.compilations,
        time_saved_ms: wire.time_saved_ms,
    }
}

fn build_log_paths_to_wire(paths: &BuildLogPaths) -> proto::WireBuildLogPaths {
    proto::WireBuildLogPaths {
        zccache_session_id: paths.zccache_session_id.clone(),
        cache_dir: paths.cache_dir.clone(),
        session_log_path: paths.session_log_path.clone(),
        journal_path: paths.journal_path.clone(),
        session_stats_path: paths.session_stats_path.clone(),
        compile_journal_path: paths.compile_journal_path.clone(),
        archived_session_log_path: paths.archived_session_log_path.clone(),
        archived_journal_path: paths.archived_journal_path.clone(),
        archived_session_stats_path: paths.archived_session_stats_path.clone(),
        archived_compile_journal_path: paths.archived_compile_journal_path.clone(),
        private_daemon_name: paths.private_daemon_name.clone(),
    }
}

fn build_log_paths_from_wire(wire: proto::WireBuildLogPaths) -> BuildLogPaths {
    BuildLogPaths {
        zccache_session_id: wire.zccache_session_id,
        cache_dir: wire.cache_dir,
        session_log_path: wire.session_log_path,
        journal_path: wire.journal_path,
        session_stats_path: wire.session_stats_path,
        compile_journal_path: wire.compile_journal_path,
        archived_session_log_path: wire.archived_session_log_path,
        archived_journal_path: wire.archived_journal_path,
        archived_session_stats_path: wire.archived_session_stats_path,
        archived_compile_journal_path: wire.archived_compile_journal_path,
        private_daemon_name: wire.private_daemon_name,
    }
}

fn build_miss_reason_to_wire(reason: &BuildMissReason) -> proto::WireBuildMissReason {
    proto::WireBuildMissReason {
        reason: reason.reason.clone(),
        count: reason.count,
    }
}

fn build_miss_reason_from_wire(wire: proto::WireBuildMissReason) -> BuildMissReason {
    BuildMissReason {
        reason: wire.reason,
        count: wire.count,
    }
}

pub fn event_to_wire(event: &Event) -> proto::WireEvent {
    proto::WireEvent {
        ts_ms: event.ts_ms,
        session_id: event.session_id,
        kind: event_kind_to_u32(&event.kind),
        crate_name: event.crate_name.clone(),
        duration_us: event.duration_us,
        target_dir: event.target_dir.clone(),
        exit_code: event.exit_code,
    }
}

pub fn event_from_wire(wire: proto::WireEvent) -> Result<Event, WireDecodeError> {
    Ok(Event {
        ts_ms: wire.ts_ms,
        session_id: wire.session_id,
        kind: u32_to_event_kind(wire.kind)?,
        crate_name: wire.crate_name,
        duration_us: wire.duration_us,
        target_dir: wire.target_dir,
        exit_code: wire.exit_code,
    })
}

fn event_kind_to_u32(kind: &EventKind) -> u32 {
    match kind {
        EventKind::SessionStart => 0,
        EventKind::SessionEnd => 1,
        EventKind::CompileStart => 2,
        EventKind::CompileEnd => 3,
    }
}

fn u32_to_event_kind(n: u32) -> Result<EventKind, WireDecodeError> {
    match n {
        0 => Ok(EventKind::SessionStart),
        1 => Ok(EventKind::SessionEnd),
        2 => Ok(EventKind::CompileStart),
        3 => Ok(EventKind::CompileEnd),
        other => Err(WireDecodeError::UnknownEventKind(other)),
    }
}

fn compile_stats_to_wire(info: &CompileStatsInfo) -> proto::WireCompileStats {
    proto::WireCompileStats {
        total_compilations: info.total_compilations,
        cache_hits: info.cache_hits,
        cache_misses: info.cache_misses,
        non_cacheable: info.non_cacheable,
        compile_errors: info.compile_errors,
        time_saved_ms: info.time_saved_ms,
        staged_profile: info
            .staged_profile
            .as_ref()
            .map(|profile| proto::WireStagedProfile {
                counters: profile.counters.clone(),
                timings_ns: profile.timings_ns.clone(),
                bytes: profile.bytes.clone(),
                failures: profile.failures.clone(),
            }),
    }
}

fn compile_stats_from_wire(wire: proto::WireCompileStats) -> CompileStatsInfo {
    CompileStatsInfo {
        total_compilations: wire.total_compilations,
        cache_hits: wire.cache_hits,
        cache_misses: wire.cache_misses,
        non_cacheable: wire.non_cacheable,
        compile_errors: wire.compile_errors,
        time_saved_ms: wire.time_saved_ms,
        staged_profile: wire.staged_profile.map(|profile| StagedProfileInfo {
            counters: profile.counters,
            timings_ns: profile.timings_ns,
            bytes: profile.bytes,
            failures: profile.failures,
        }),
    }
}

fn cache_flush_to_wire(info: &CacheFlushInfo) -> proto::WireCacheFlush {
    proto::WireCacheFlush {
        complete: info.complete,
        pending_writes_drained: info.pending_writes_drained,
        index_writer_drained: info.index_writer_drained,
        steps: info
            .steps
            .iter()
            .map(|step| proto::WireCacheFlushStep {
                step: step.step.clone(),
                status: step.status.clone(),
                error: step.error.clone(),
            })
            .collect(),
        artifact_entries: info.artifact_entries,
        metadata_entries: info.metadata_entries,
    }
}

fn cache_flush_from_wire(wire: proto::WireCacheFlush) -> CacheFlushInfo {
    CacheFlushInfo {
        complete: wire.complete,
        pending_writes_drained: wire.pending_writes_drained,
        index_writer_drained: wire.index_writer_drained,
        steps: wire
            .steps
            .into_iter()
            .map(|step| CacheFlushStepInfo {
                step: step.step,
                status: step.status,
                error: step.error,
            })
            .collect(),
        artifact_entries: wire.artifact_entries,
        metadata_entries: wire.metadata_entries,
    }
}

fn cook_stats_to_wire(stats: &CookStats) -> proto::WireCookStats {
    proto::WireCookStats {
        entries: stats.entries,
        total_bytes: stats.total_bytes,
        hits_this_session: stats.hits_this_session,
    }
}

fn cook_stats_from_wire(wire: proto::WireCookStats) -> CookStats {
    CookStats {
        entries: wire.entries,
        total_bytes: wire.total_bytes,
        hits_this_session: wire.hits_this_session,
    }
}

pub fn status_info_to_wire(info: &StatusInfo) -> proto::WireStatusInfo {
    proto::WireStatusInfo {
        version: info.version,
        pid: info.pid,
        generation: info.generation,
        uptime_secs: info.uptime_secs,
        request_count: info.request_count,
        cook_stats: info.cook_stats.as_ref().map(cook_stats_to_wire),
        compile_backend: info.compile_backend.clone(),
        compile_jobs: info.compile_jobs,
        compile_jobs_source: info.compile_jobs_source.clone(),
        in_flight_compiles: info.in_flight_compiles,
    }
}

pub fn status_info_from_wire(wire: proto::WireStatusInfo) -> StatusInfo {
    StatusInfo {
        version: wire.version,
        pid: wire.pid,
        generation: wire.generation,
        uptime_secs: wire.uptime_secs,
        request_count: wire.request_count,
        cook_stats: wire.cook_stats.map(cook_stats_from_wire),
        compile_backend: wire.compile_backend,
        compile_jobs: wire.compile_jobs,
        compile_jobs_source: wire.compile_jobs_source,
        in_flight_compiles: wire.in_flight_compiles,
    }
}

// =========================================================================
// Conversions: Request
// =========================================================================

impl From<&Request> for proto::WireRequest {
    #[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
    fn from(req: &Request) -> Self {
        let kind = match req {
            Request::RecordTargetTouch { path, unix_seconds } => {
                proto::WireRequestKind::RecordTargetTouch(proto::WireRecordTargetTouch {
                    path: path.clone(),
                    unix_seconds: *unix_seconds,
                })
            }
            Request::Status => proto::WireRequestKind::Status(proto::WireUnit {}),
            Request::Shutdown => proto::WireRequestKind::Shutdown(proto::WireUnit {}),
            Request::FlushCaches => proto::WireRequestKind::FlushCaches(proto::WireUnit {}),
            Request::CompileStats => proto::WireRequestKind::CompileStats(proto::WireUnit {}),
            Request::ListTargetRegistry => {
                proto::WireRequestKind::ListTargetRegistry(proto::WireUnit {})
            }
            Request::RemoveTargetRegistry { paths } => {
                proto::WireRequestKind::RemoveTargetRegistry(proto::WireRemoveTargetRegistry {
                    paths: paths.clone(),
                })
            }
            Request::AcquireResidentCapacity { permits } => {
                proto::WireRequestKind::AcquireResidentCapacity(
                    proto::WireAcquireResidentCapacity { permits: *permits },
                )
            }
            Request::ReleaseResidentCapacity => {
                proto::WireRequestKind::ReleaseResidentCapacity(proto::WireUnit {})
            }
            Request::BuildLogInputs { session_id } => {
                proto::WireRequestKind::BuildLogInputs(proto::WireBuildLogInputsRequest {
                    session_id: *session_id,
                })
            }
            Request::AttachBuildLogHistory(update) => {
                proto::WireRequestKind::AttachBuildLogHistory(proto::WireBuildLogHistoryUpdate {
                    session_id: update.session_id,
                    repo_root: update.repo_root.clone(),
                    started_at_ms: update.started_at_ms,
                    ended_at_ms: update.ended_at_ms,
                    exit_code: update.exit_code,
                    daemon_finalized: update.daemon_finalized,
                    cache_summary: update
                        .cache_summary
                        .as_ref()
                        .map(build_cache_summary_to_wire),
                    miss_reasons: update
                        .miss_reasons
                        .iter()
                        .map(build_miss_reason_to_wire)
                        .collect(),
                    log_paths: update
                        .log_paths
                        .as_ref()
                        .map(build_log_paths_to_wire)
                        .map(Box::new),
                })
            }
            Request::ShouldWarnCargoDebugDefault { repo_root } => {
                proto::WireRequestKind::ShouldWarnCargoDebugDefault(
                    proto::WireShouldWarnCargoDebugDefault {
                        repo_root: repo_root.clone(),
                    },
                )
            }
            Request::BuildSessionStart {
                session_id,
                repo_root,
                started_at_ms,
            } => proto::WireRequestKind::BuildSessionStart(proto::WireBuildSessionStart {
                session_id: *session_id,
                repo_root: repo_root.clone(),
                started_at_ms: *started_at_ms,
            }),
            Request::BuildSessionEnd {
                session_id,
                exit_code,
                ended_at_ms,
            } => proto::WireRequestKind::BuildSessionEnd(proto::WireBuildSessionEnd {
                session_id: *session_id,
                exit_code: *exit_code,
                ended_at_ms: *ended_at_ms,
            }),
            Request::ListBuilds { limit, since_ms } => {
                proto::WireRequestKind::ListBuilds(proto::WireListBuilds {
                    limit: *limit,
                    since_ms: *since_ms,
                })
            }
            Request::ListSlowBuilds {
                threshold_ms,
                limit,
            } => proto::WireRequestKind::ListSlowBuilds(proto::WireListSlowBuilds {
                threshold_ms: *threshold_ms,
                limit: *limit,
            }),
            Request::CookLookup {
                recipe_hash,
                target_triple,
                profile,
                channel,
                rustc_version,
                origin_url_normalized,
                branch_lineage,
            } => proto::WireRequestKind::CookLookup(proto::WireCookLookup {
                recipe_hash: sha_to_vec(recipe_hash),
                target_triple: target_triple.clone(),
                profile: profile.clone(),
                channel: channel.clone(),
                rustc_version: rustc_version.clone(),
                origin_url_normalized: origin_url_normalized.clone(),
                branch_lineage: branch_lineage.clone(),
            }),
            Request::CookRecord {
                recipe_hash,
                target_triple,
                profile,
                channel,
                rustc_version,
                sha256,
                size_bytes,
                origin_url_normalized,
                branch_name,
                cook_cmd_summary,
                compile_duration_ms,
                save_elapsed_ms,
            } => proto::WireRequestKind::CookRecord(proto::WireCookRecord {
                recipe_hash: sha_to_vec(recipe_hash),
                target_triple: target_triple.clone(),
                profile: profile.clone(),
                channel: channel.clone(),
                rustc_version: rustc_version.clone(),
                sha256: sha_to_vec(sha256),
                size_bytes: *size_bytes,
                origin_url_normalized: origin_url_normalized.clone(),
                cook_cmd_summary: cook_cmd_summary.clone(),
                branch_name: branch_name.clone(),
                compile_duration_ms: *compile_duration_ms,
                save_elapsed_ms: *save_elapsed_ms,
            }),
            Request::CookTouch { sha256 } => {
                proto::WireRequestKind::CookTouch(proto::WireCookTouch {
                    sha256: sha_to_vec(sha256),
                })
            }
        };
        Self { kind: Some(kind) }
    }
}

impl TryFrom<proto::WireRequest> for Request {
    type Error = WireDecodeError;

    fn try_from(wire: proto::WireRequest) -> Result<Self, Self::Error> {
        let kind = wire.kind.ok_or(WireDecodeError::EmptyOneof("Request"))?;
        Ok(match kind {
            proto::WireRequestKind::RecordTargetTouch(m) => Request::RecordTargetTouch {
                path: m.path,
                unix_seconds: m.unix_seconds,
            },
            proto::WireRequestKind::Status(_) => Request::Status,
            proto::WireRequestKind::Shutdown(_) => Request::Shutdown,
            proto::WireRequestKind::FlushCaches(_) => Request::FlushCaches,
            proto::WireRequestKind::CompileStats(_) => Request::CompileStats,
            proto::WireRequestKind::ListTargetRegistry(_) => Request::ListTargetRegistry,
            proto::WireRequestKind::RemoveTargetRegistry(m) => {
                Request::RemoveTargetRegistry { paths: m.paths }
            }
            proto::WireRequestKind::AcquireResidentCapacity(m) => {
                Request::AcquireResidentCapacity { permits: m.permits }
            }
            proto::WireRequestKind::ReleaseResidentCapacity(_) => Request::ReleaseResidentCapacity,
            proto::WireRequestKind::BuildLogInputs(m) => Request::BuildLogInputs {
                session_id: m.session_id,
            },
            proto::WireRequestKind::AttachBuildLogHistory(m) => Request::AttachBuildLogHistory(
                Box::new(crate::daemon::protocol::BuildLogHistoryUpdate {
                    session_id: m.session_id,
                    repo_root: m.repo_root,
                    started_at_ms: m.started_at_ms,
                    ended_at_ms: m.ended_at_ms,
                    exit_code: m.exit_code,
                    daemon_finalized: m.daemon_finalized,
                    cache_summary: m.cache_summary.map(build_cache_summary_from_wire),
                    miss_reasons: m
                        .miss_reasons
                        .into_iter()
                        .map(build_miss_reason_from_wire)
                        .collect(),
                    log_paths: m.log_paths.map(|p| build_log_paths_from_wire(*p)),
                }),
            ),
            proto::WireRequestKind::ShouldWarnCargoDebugDefault(m) => {
                Request::ShouldWarnCargoDebugDefault {
                    repo_root: m.repo_root,
                }
            }
            proto::WireRequestKind::BuildSessionStart(m) => Request::BuildSessionStart {
                session_id: m.session_id,
                repo_root: m.repo_root,
                started_at_ms: m.started_at_ms,
            },
            proto::WireRequestKind::BuildSessionEnd(m) => Request::BuildSessionEnd {
                session_id: m.session_id,
                exit_code: m.exit_code,
                ended_at_ms: m.ended_at_ms,
            },
            proto::WireRequestKind::ListBuilds(m) => Request::ListBuilds {
                limit: m.limit,
                since_ms: m.since_ms,
            },
            proto::WireRequestKind::ListSlowBuilds(m) => Request::ListSlowBuilds {
                threshold_ms: m.threshold_ms,
                limit: m.limit,
            },
            proto::WireRequestKind::CookLookup(m) => Request::CookLookup {
                recipe_hash: vec_to_sha(&m.recipe_hash)?,
                target_triple: m.target_triple,
                profile: m.profile,
                channel: m.channel,
                rustc_version: m.rustc_version,
                origin_url_normalized: m.origin_url_normalized,
                branch_lineage: m.branch_lineage,
            },
            proto::WireRequestKind::CookRecord(m) => Request::CookRecord {
                recipe_hash: vec_to_sha(&m.recipe_hash)?,
                target_triple: m.target_triple,
                profile: m.profile,
                channel: m.channel,
                rustc_version: m.rustc_version,
                sha256: vec_to_sha(&m.sha256)?,
                size_bytes: m.size_bytes,
                origin_url_normalized: m.origin_url_normalized,
                branch_name: m.branch_name,
                cook_cmd_summary: m.cook_cmd_summary,
                compile_duration_ms: m.compile_duration_ms,
                save_elapsed_ms: m.save_elapsed_ms,
            },
            proto::WireRequestKind::CookTouch(m) => Request::CookTouch {
                sha256: vec_to_sha(&m.sha256)?,
            },
        })
    }
}

// =========================================================================
// Conversions: Response
// =========================================================================

impl From<&Response> for proto::WireResponse {
    fn from(resp: &Response) -> Self {
        let kind = match resp {
            Response::Status(info) => proto::WireResponseKind::Status(status_info_to_wire(info)),
            Response::TargetRegistryRows(rows) => {
                proto::WireResponseKind::TargetRegistryRows(proto::WireTargetRegistryRows {
                    rows: rows
                        .iter()
                        .map(|row| proto::WireTargetRegistryRow {
                            path: row.path.clone(),
                            last_used: row.last_used,
                        })
                        .collect(),
                })
            }
            Response::TargetRegistryRemoved { removed } => {
                proto::WireResponseKind::TargetRegistryRemoved(proto::WireTargetRegistryRemoved {
                    removed: *removed,
                })
            }
            Response::ResidentCapacityAcquired { permits } => {
                proto::WireResponseKind::ResidentCapacityAcquired(
                    proto::WireResidentCapacityAcquired { permits: *permits },
                )
            }
            Response::ShuttingDown(ack) => {
                proto::WireResponseKind::ShuttingDown(proto::WireShuttingDown {
                    pid: ack.pid,
                    generation: ack.generation,
                })
            }
            Response::Builds(rows) => proto::WireResponseKind::Builds(proto::WireBuilds {
                items: rows.iter().map(build_record_to_wire).collect(),
            }),
            Response::Error(msg) => proto::WireResponseKind::Error(msg.clone()),
            Response::BuildSessionStarted {
                compile_jobs,
                compile_jobs_source,
            } => proto::WireResponseKind::BuildSessionStarted(proto::WireBuildSessionStarted {
                compile_jobs: *compile_jobs,
                compile_jobs_source: compile_jobs_source.clone(),
            }),
            Response::Retiring => proto::WireResponseKind::Retiring(proto::WireRetiring {}),
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
            } => proto::WireResponseKind::CookHit(proto::WireCookHit {
                sha256: sha_to_vec(sha256),
                path: path.clone(),
                size_bytes: *size_bytes,
                origin_url_normalized: origin_url_normalized.clone(),
                matched_recipe_hash: matched_recipe_hash
                    .as_ref()
                    .map(sha_to_vec)
                    .unwrap_or_default(),
                exact_recipe_match: *exact_recipe_match,
                branch_name: branch_name.clone(),
                compile_duration_ms: *compile_duration_ms,
                save_elapsed_ms: *save_elapsed_ms,
            }),
            Response::CookMiss {
                previous_origin_recipe_hashes,
            } => proto::WireResponseKind::CookMiss(proto::WireCookMiss {
                previous_origin_recipe_hashes: previous_origin_recipe_hashes
                    .iter()
                    .map(sha_to_vec)
                    .collect(),
            }),
            Response::Ack => proto::WireResponseKind::Ack(proto::WireUnit {}),
            Response::CompileStats(info) => {
                proto::WireResponseKind::CompileStats(compile_stats_to_wire(info))
            }
            Response::CacheFlushed(info) => {
                proto::WireResponseKind::CacheFlushed(cache_flush_to_wire(info))
            }
            Response::BuildLogInputs { events, record } => {
                proto::WireResponseKind::BuildLogInputs(proto::WireBuildLogInputs {
                    events: events.iter().map(event_to_wire).collect(),
                    record: record.as_deref().map(build_record_to_wire).map(Box::new),
                })
            }
            Response::CargoDebugWarning { emit } => {
                proto::WireResponseKind::CargoDebugWarning(proto::WireCargoDebugWarning {
                    emit: *emit,
                })
            }
        };
        Self { kind: Some(kind) }
    }
}

impl TryFrom<proto::WireResponse> for Response {
    type Error = WireDecodeError;

    fn try_from(wire: proto::WireResponse) -> Result<Self, WireDecodeError> {
        let kind = wire.kind.ok_or(WireDecodeError::EmptyOneof("Response"))?;
        Ok(match kind {
            proto::WireResponseKind::Status(m) => Response::Status(status_info_from_wire(m)),
            proto::WireResponseKind::TargetRegistryRows(m) => Response::TargetRegistryRows(
                m.rows
                    .into_iter()
                    .map(|row| TargetRegistryRow {
                        path: row.path,
                        last_used: row.last_used,
                    })
                    .collect(),
            ),
            proto::WireResponseKind::TargetRegistryRemoved(m) => {
                Response::TargetRegistryRemoved { removed: m.removed }
            }
            proto::WireResponseKind::ResidentCapacityAcquired(m) => {
                Response::ResidentCapacityAcquired { permits: m.permits }
            }
            proto::WireResponseKind::ShuttingDown(reply) => Response::ShuttingDown(ShutdownAck {
                pid: reply.pid,
                generation: reply.generation,
            }),
            proto::WireResponseKind::Builds(m) => {
                Response::Builds(m.items.into_iter().map(build_record_from_wire).collect())
            }
            proto::WireResponseKind::Error(msg) => Response::Error(msg),
            proto::WireResponseKind::BuildSessionStarted(m) => Response::BuildSessionStarted {
                compile_jobs: m.compile_jobs,
                compile_jobs_source: m.compile_jobs_source,
            },
            proto::WireResponseKind::Retiring(_) => Response::Retiring,
            proto::WireResponseKind::CookHit(m) => Response::CookHit {
                sha256: vec_to_sha(&m.sha256)?,
                path: m.path,
                size_bytes: m.size_bytes,
                origin_url_normalized: m.origin_url_normalized,
                matched_recipe_hash: vec_to_optional_sha(&m.matched_recipe_hash)?,
                exact_recipe_match: m.exact_recipe_match,
                branch_name: m.branch_name,
                compile_duration_ms: m.compile_duration_ms,
                save_elapsed_ms: m.save_elapsed_ms,
            },
            proto::WireResponseKind::CookMiss(m) => {
                let mut hashes = Vec::with_capacity(m.previous_origin_recipe_hashes.len());
                for raw in m.previous_origin_recipe_hashes {
                    hashes.push(vec_to_sha(&raw)?);
                }
                Response::CookMiss {
                    previous_origin_recipe_hashes: hashes,
                }
            }
            proto::WireResponseKind::Ack(_) => Response::Ack,
            proto::WireResponseKind::CompileStats(m) => {
                Response::CompileStats(compile_stats_from_wire(m))
            }
            proto::WireResponseKind::CacheFlushed(m) => {
                Response::CacheFlushed(cache_flush_from_wire(m))
            }
            proto::WireResponseKind::BuildLogInputs(m) => Response::BuildLogInputs {
                events: m
                    .events
                    .into_iter()
                    .map(event_from_wire)
                    .collect::<Result<Vec<_>, _>>()?,
                record: m.record.map(|r| Box::new(build_record_from_wire(*r))),
            },
            proto::WireResponseKind::CargoDebugWarning(m) => {
                Response::CargoDebugWarning { emit: m.emit }
            }
        })
    }
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
