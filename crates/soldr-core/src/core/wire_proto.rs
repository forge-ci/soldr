//! Prost wire message + oneof definitions for the daemon IPC schema
//! (soldr#580). Extracted from `wire.rs` (soldr#1368) to keep files under
//! the 1,000-LOC guard; moved to `core` with the rest of the pure wire
//! schema in #1490 Phase 0. Declared as `#[path = "wire_proto.rs"] pub mod
//! proto;` from `core/wire.rs`; the `.proto` schema lives beside it as
//! `wire.proto`.

use prost::{Message, Oneof};

// -- Request ----------------------------------------------------------

#[derive(Clone, PartialEq, Message)]
pub struct WireRequest {
    #[prost(
        oneof = "WireRequestKind",
        tags = "1,2,3,4,5,7,8,9,10,11,12,14,15,16,17,18,19,20,21,22"
    )]
    pub kind: Option<WireRequestKind>,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum WireRequestKind {
    #[prost(message, tag = "1")]
    RecordTargetTouch(WireRecordTargetTouch),
    #[prost(message, tag = "2")]
    Status(WireUnit),
    #[prost(message, tag = "3")]
    Shutdown(WireUnit),
    #[prost(message, tag = "4")]
    BuildSessionStart(WireBuildSessionStart),
    #[prost(message, tag = "5")]
    BuildSessionEnd(WireBuildSessionEnd),
    // Tag 6 (RecordCompile) reserved — folded into Compile in soldr#1537.
    #[prost(message, tag = "7")]
    ListBuilds(WireListBuilds),
    #[prost(message, tag = "8")]
    ListSlowBuilds(WireListSlowBuilds),
    // Tag 9 (LinkZccache) reserved — deleted in soldr#1467.
    #[prost(message, tag = "10")]
    CookLookup(WireCookLookup),
    #[prost(message, tag = "11")]
    CookRecord(WireCookRecord),
    #[prost(message, tag = "12")]
    CookTouch(WireCookTouch),
    // Tag 13 (Compile) reserved — the legacy direct-IPC compile verb was
    // deleted in soldr#2424; compiles travel the broker SESSION route.
    /// #1286 F1 — checkpoint embedded zccache state to disk.
    #[prost(message, tag = "14")]
    FlushCaches(WireUnit),
    /// soldr#1368 — read the embedded zccache compile counters.
    #[prost(message, tag = "15")]
    CompileStats(WireUnit),
    /// v19 / soldr#1814 slice 2a — ask the daemon for the build-log inputs it
    /// already owns, instead of the CLI opening state.sqlite3 itself.
    #[prost(message, tag = "16")]
    BuildLogInputs(WireBuildLogInputsRequest),
    /// v19 / soldr#1814 slice 2c — cargo-debug-default warning decision, so
    /// the front door stops performing that read-modify-write itself.
    #[prost(message, tag = "17")]
    ShouldWarnCargoDebugDefault(WireShouldWarnCargoDebugDefault),
    /// v19 / soldr#1814 slice 2d — atomic build-record merge at the front-door
    /// tail, replacing the CLI's own get/mutate/upsert.
    #[prost(message, tag = "18")]
    AttachBuildLogHistory(WireBuildLogHistoryUpdate),
    #[prost(message, tag = "19")]
    ListTargetRegistry(WireUnit),
    #[prost(message, tag = "20")]
    RemoveTargetRegistry(WireRemoveTargetRegistry),
    #[prost(message, tag = "21")]
    AcquireResidentCapacity(WireAcquireResidentCapacity),
    #[prost(message, tag = "22")]
    ReleaseResidentCapacity(WireUnit),
}

#[derive(Clone, PartialEq, Message)]
pub struct WireAcquireResidentCapacity {
    #[prost(uint32, tag = "1")]
    pub permits: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireRemoveTargetRegistry {
    #[prost(string, repeated, tag = "1")]
    pub paths: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireBuildLogHistoryUpdate {
    #[prost(uint64, tag = "1")]
    pub session_id: u64,
    #[prost(string, tag = "2")]
    pub repo_root: String,
    #[prost(int64, tag = "3")]
    pub started_at_ms: i64,
    #[prost(int64, tag = "4")]
    pub ended_at_ms: i64,
    #[prost(int32, tag = "5")]
    pub exit_code: i32,
    #[prost(bool, tag = "6")]
    pub daemon_finalized: bool,
    #[prost(message, optional, tag = "7")]
    pub cache_summary: Option<WireBuildCacheSummary>,
    #[prost(message, repeated, tag = "8")]
    pub miss_reasons: Vec<WireBuildMissReason>,
    #[prost(message, optional, boxed, tag = "9")]
    pub log_paths: Option<Box<WireBuildLogPaths>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireBuildLogInputsRequest {
    #[prost(uint64, tag = "1")]
    pub session_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireShouldWarnCargoDebugDefault {
    #[prost(string, tag = "1")]
    pub repo_root: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireCargoDebugWarning {
    #[prost(bool, tag = "1")]
    pub emit: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireUnit {}

#[derive(Clone, PartialEq, Message)]
pub struct WireRecordTargetTouch {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(int64, tag = "2")]
    pub unix_seconds: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireBuildSessionStart {
    #[prost(uint64, tag = "1")]
    pub session_id: u64,
    #[prost(string, tag = "2")]
    pub repo_root: String,
    #[prost(int64, tag = "3")]
    pub started_at_ms: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireBuildSessionEnd {
    #[prost(uint64, tag = "1")]
    pub session_id: u64,
    #[prost(int32, tag = "2")]
    pub exit_code: i32,
    #[prost(int64, tag = "3")]
    pub ended_at_ms: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireListBuilds {
    #[prost(uint32, tag = "1")]
    pub limit: u32,
    #[prost(int64, optional, tag = "2")]
    pub since_ms: Option<i64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireListSlowBuilds {
    #[prost(uint64, tag = "1")]
    pub threshold_ms: u64,
    #[prost(uint32, tag = "2")]
    pub limit: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireCookLookup {
    #[prost(bytes = "vec", tag = "1")]
    pub recipe_hash: Vec<u8>,
    #[prost(string, tag = "2")]
    pub target_triple: String,
    #[prost(string, tag = "3")]
    pub profile: String,
    #[prost(string, tag = "4")]
    pub channel: String,
    #[prost(string, tag = "5")]
    pub rustc_version: String,
    #[prost(string, optional, tag = "6")]
    pub origin_url_normalized: Option<String>,
    #[prost(string, repeated, tag = "7")]
    pub branch_lineage: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireCookRecord {
    #[prost(bytes = "vec", tag = "1")]
    pub recipe_hash: Vec<u8>,
    #[prost(string, tag = "2")]
    pub target_triple: String,
    #[prost(string, tag = "3")]
    pub profile: String,
    #[prost(string, tag = "4")]
    pub channel: String,
    #[prost(string, tag = "5")]
    pub rustc_version: String,
    #[prost(bytes = "vec", tag = "6")]
    pub sha256: Vec<u8>,
    #[prost(uint64, tag = "7")]
    pub size_bytes: u64,
    #[prost(string, optional, tag = "8")]
    pub origin_url_normalized: Option<String>,
    #[prost(string, tag = "9")]
    pub cook_cmd_summary: String,
    #[prost(string, optional, tag = "10")]
    pub branch_name: Option<String>,
    #[prost(uint64, tag = "11")]
    pub compile_duration_ms: u64,
    #[prost(uint64, tag = "12")]
    pub save_elapsed_ms: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireCookTouch {
    #[prost(bytes = "vec", tag = "1")]
    pub sha256: Vec<u8>,
}

// -- Response ---------------------------------------------------------

#[derive(Clone, PartialEq, Message)]
pub struct WireResponse {
    #[prost(
        oneof = "WireResponseKind",
        // soldr#1838: a tag missing from this list decodes as EmptyOneof even
        // though the variant exists on the enum below -- prost only accepts
        // tags enumerated here. Keep it in sync when adding a variant.
        tags = "1,2,3,4,5,6,7,12,14,15,16,17,18,19,20,21"
    )]
    pub kind: Option<WireResponseKind>,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum WireResponseKind {
    #[prost(message, tag = "1")]
    Status(WireStatusInfo),
    #[prost(message, tag = "2")]
    ShuttingDown(WireShuttingDown),
    #[prost(message, tag = "3")]
    Builds(WireBuilds),
    #[prost(string, tag = "4")]
    Error(String),
    #[prost(message, tag = "5")]
    CookHit(WireCookHit),
    #[prost(message, tag = "6")]
    CookMiss(WireCookMiss),
    #[prost(message, tag = "7")]
    Ack(WireUnit),
    // Tags 8-11 (CompileResponse, CompileStdoutChunk, CompileStderrChunk,
    // CompileDone) reserved — deleted with the direct-IPC compile verb in
    // soldr#2424.
    /// soldr#1368 — reply to CompileStats.
    #[prost(message, tag = "12")]
    CompileStats(WireCompileStats),
    // Tag 13 (Backpressure) reserved — deleted in soldr#2424.
    /// v17 — structured reply to FlushCaches.
    #[prost(message, tag = "14")]
    CacheFlushed(WireCacheFlush),
    /// v19 / soldr#1814 slice 2a — build-log inputs served by the daemon so
    /// the CLI stops opening the daemon tables in state.sqlite3 directly.
    #[prost(message, tag = "15")]
    BuildLogInputs(WireBuildLogInputs),
    /// v19 / soldr#1814 slice 2c — reply to ShouldWarnCargoDebugDefault.
    #[prost(message, tag = "16")]
    CargoDebugWarning(WireCargoDebugWarning),
    /// v20 / soldr#1838 Phase 2 — the daemon is retiring and will not serve
    /// this request.
    #[prost(message, tag = "17")]
    Retiring(WireRetiring),
    /// v21 / soldr#2023 — the acknowledgement for `BuildSessionStart`,
    /// carrying the daemon's applied compile limit. Replaces the bare
    /// `Ack` on that one request so the front door learns the running
    /// daemon's limit without paying for a second round-trip on a path
    /// that already has a documented fixed-overhead problem (#1843).
    #[prost(message, tag = "18")]
    BuildSessionStarted(WireBuildSessionStarted),
    #[prost(message, tag = "19")]
    TargetRegistryRows(WireTargetRegistryRows),
    #[prost(message, tag = "20")]
    TargetRegistryRemoved(WireTargetRegistryRemoved),
    #[prost(message, tag = "21")]
    ResidentCapacityAcquired(WireResidentCapacityAcquired),
}

#[derive(Clone, PartialEq, Message)]
pub struct WireResidentCapacityAcquired {
    #[prost(uint32, tag = "1")]
    pub permits: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireTargetRegistryRow {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(int64, tag = "2")]
    pub last_used: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireTargetRegistryRows {
    #[prost(message, repeated, tag = "1")]
    pub rows: Vec<WireTargetRegistryRow>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireTargetRegistryRemoved {
    #[prost(uint32, tag = "1")]
    pub removed: u32,
}

/// soldr#1838. Empty today; a message rather than a bare bool so a reason or
/// ETA can be added later without consuming another oneof slot.
#[derive(Clone, PartialEq, Message)]
pub struct WireRetiring {}

/// soldr#2023. The compile limit the answering daemon is actually running
/// with, so a client whose own resolution differs can say so.
#[derive(Clone, PartialEq, Message)]
pub struct WireBuildSessionStarted {
    #[prost(uint32, tag = "1")]
    pub compile_jobs: u32,
    #[prost(string, tag = "2")]
    pub compile_jobs_source: String,
}

/// soldr#1814 slice 2a. `record` is absent when the daemon has no row for the
/// session, which is a normal outcome rather than an error.
#[derive(Clone, PartialEq, Message)]
pub struct WireBuildLogInputs {
    #[prost(message, repeated, tag = "1")]
    pub events: Vec<WireEvent>,
    /// Boxed for the same reason as `Response::BuildLogInputs::record`:
    /// `WireBuildRecord` is wide, and inlining it makes this variant dominate
    /// the size of every `WireResponseKind`.
    #[prost(message, optional, boxed, tag = "2")]
    pub record: Option<Box<WireBuildRecord>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireShuttingDown {
    #[prost(uint32, tag = "1")]
    pub pid: u32,
    #[prost(uint64, tag = "2")]
    pub generation: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireCacheFlush {
    #[prost(bool, tag = "1")]
    pub complete: bool,
    #[prost(bool, tag = "2")]
    pub pending_writes_drained: bool,
    #[prost(bool, tag = "3")]
    pub index_writer_drained: bool,
    #[prost(message, repeated, tag = "4")]
    pub steps: Vec<WireCacheFlushStep>,
    #[prost(uint64, tag = "5")]
    pub artifact_entries: u64,
    #[prost(uint64, tag = "6")]
    pub metadata_entries: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireCacheFlushStep {
    #[prost(string, tag = "1")]
    pub step: String,
    #[prost(string, tag = "2")]
    pub status: String,
    #[prost(string, optional, tag = "3")]
    pub error: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireCompileStats {
    #[prost(uint64, tag = "1")]
    pub total_compilations: u64,
    #[prost(uint64, tag = "2")]
    pub cache_hits: u64,
    #[prost(uint64, tag = "3")]
    pub cache_misses: u64,
    #[prost(uint64, tag = "4")]
    pub non_cacheable: u64,
    #[prost(uint64, tag = "5")]
    pub compile_errors: u64,
    #[prost(uint64, tag = "6")]
    pub time_saved_ms: u64,
    #[prost(message, optional, tag = "7")]
    pub staged_profile: Option<WireStagedProfile>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireStagedProfile {
    #[prost(btree_map = "string, uint64", tag = "1")]
    pub counters: std::collections::BTreeMap<String, u64>,
    #[prost(btree_map = "string, uint64", tag = "2")]
    pub timings_ns: std::collections::BTreeMap<String, u64>,
    #[prost(btree_map = "string, uint64", tag = "3")]
    pub bytes: std::collections::BTreeMap<String, u64>,
    #[prost(btree_map = "string, uint64", tag = "4")]
    pub failures: std::collections::BTreeMap<String, u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireBuilds {
    #[prost(message, repeated, tag = "1")]
    pub items: Vec<WireBuildRecord>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireCookHit {
    #[prost(bytes = "vec", tag = "1")]
    pub sha256: Vec<u8>,
    #[prost(string, tag = "2")]
    pub path: String,
    #[prost(uint64, tag = "3")]
    pub size_bytes: u64,
    #[prost(string, optional, tag = "4")]
    pub origin_url_normalized: Option<String>,
    #[prost(bytes = "vec", tag = "5")]
    pub matched_recipe_hash: Vec<u8>,
    #[prost(bool, tag = "6")]
    pub exact_recipe_match: bool,
    #[prost(string, optional, tag = "7")]
    pub branch_name: Option<String>,
    #[prost(uint64, tag = "8")]
    pub compile_duration_ms: u64,
    #[prost(uint64, tag = "9")]
    pub save_elapsed_ms: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireCookMiss {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub previous_origin_recipe_hashes: Vec<Vec<u8>>,
}

// -- Shared / persistent ----------------------------------------------

#[derive(Clone, PartialEq, Message)]
pub struct WireStatusInfo {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(uint32, tag = "2")]
    pub pid: u32,
    #[prost(uint64, tag = "3")]
    pub uptime_secs: u64,
    #[prost(uint64, tag = "4")]
    pub request_count: u64,
    // Tag 5 (linked_zccache) reserved — deleted in soldr#1467.
    #[prost(message, optional, tag = "6")]
    pub cook_stats: Option<WireCookStats>,
    /// #977 / #980 L1 — always `"embedded"` since the legacy
    /// wrapped fork-zccache.exe backend was removed in #980 L1's
    /// second pass. Field retained on the wire so telemetry
    /// consumers reading v6 status snapshots stay stable.
    #[prost(string, tag = "7")]
    pub compile_backend: String,
    // Tag 8 (ipc_burst_stats) reserved — deleted in soldr#2424.
    /// v18 — process-start generation shared with `WireShuttingDown`.
    #[prost(uint64, tag = "9")]
    pub generation: u64,
    /// v21 (soldr#2023) — the compile-concurrency limit this daemon
    /// actually applied at startup, and the precedence tier it came
    /// from. Reported rather than re-resolved: the point is to expose a
    /// limit that has drifted from what the environment now resolves to.
    #[prost(uint32, tag = "10")]
    pub compile_jobs: u32,
    #[prost(string, tag = "11")]
    pub compile_jobs_source: String,
    /// soldr#3815 — compiles in flight in this daemon right now.
    #[prost(uint32, tag = "12")]
    pub in_flight_compiles: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireCookStats {
    #[prost(uint64, tag = "1")]
    pub entries: u64,
    #[prost(uint64, tag = "2")]
    pub total_bytes: u64,
    #[prost(uint64, tag = "3")]
    pub hits_this_session: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireBuildRecord {
    #[prost(uint64, tag = "1")]
    pub session_id: u64,
    #[prost(string, tag = "2")]
    pub repo_root: String,
    #[prost(int64, tag = "3")]
    pub started_at_ms: i64,
    #[prost(int64, optional, tag = "4")]
    pub ended_at_ms: Option<i64>,
    #[prost(int32, optional, tag = "5")]
    pub exit_code: Option<i32>,
    #[prost(uint64, optional, tag = "6")]
    pub total_wall_ms: Option<u64>,
    #[prost(uint32, tag = "7")]
    pub crate_count: u32,
    #[prost(uint64, optional, tag = "8")]
    pub slowest_crate_us: Option<u64>,
    #[prost(string, optional, tag = "9")]
    pub slowest_crate_name: Option<String>,
    #[prost(message, optional, tag = "10")]
    pub cache_summary: Option<WireBuildCacheSummary>,
    #[prost(message, optional, tag = "11")]
    pub log_paths: Option<WireBuildLogPaths>,
    #[prost(message, repeated, tag = "12")]
    pub miss_reasons: Vec<WireBuildMissReason>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireBuildCacheSummary {
    #[prost(uint64, tag = "1")]
    pub hits: u64,
    #[prost(uint64, tag = "2")]
    pub misses: u64,
    #[prost(uint64, tag = "3")]
    pub non_cacheable: u64,
    #[prost(uint64, tag = "4")]
    pub errors: u64,
    #[prost(uint64, tag = "5")]
    pub compilations: u64,
    #[prost(uint64, tag = "6")]
    pub time_saved_ms: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireBuildLogPaths {
    #[prost(string, optional, tag = "1")]
    pub zccache_session_id: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub cache_dir: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub session_log_path: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub journal_path: Option<String>,
    #[prost(string, optional, tag = "5")]
    pub session_stats_path: Option<String>,
    #[prost(string, optional, tag = "6")]
    pub compile_journal_path: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub archived_session_log_path: Option<String>,
    #[prost(string, optional, tag = "8")]
    pub archived_journal_path: Option<String>,
    #[prost(string, optional, tag = "9")]
    pub archived_session_stats_path: Option<String>,
    #[prost(string, optional, tag = "10")]
    pub archived_compile_journal_path: Option<String>,
    #[prost(string, optional, tag = "11")]
    pub private_daemon_name: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireBuildMissReason {
    #[prost(string, tag = "1")]
    pub reason: String,
    #[prost(uint64, tag = "2")]
    pub count: u64,
}

/// soldr#3152 step 4: one unit's last measured peak memory.
#[derive(Clone, PartialEq, Message)]
pub struct WireUnitMemory {
    #[prost(uint64, tag = "1")]
    pub peak_rss_bytes: u64,
    #[prost(uint64, tag = "2")]
    pub tree_peak_rss_bytes: u64,
    #[prost(int64, tag = "3")]
    pub updated_ms: i64,
    #[prost(uint64, tag = "4")]
    pub samples: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireEvent {
    #[prost(int64, tag = "1")]
    pub ts_ms: i64,
    #[prost(uint64, optional, tag = "2")]
    pub session_id: Option<u64>,
    #[prost(uint32, tag = "3")]
    pub kind: u32,
    #[prost(string, optional, tag = "4")]
    pub crate_name: Option<String>,
    #[prost(uint64, optional, tag = "5")]
    pub duration_us: Option<u64>,
    #[prost(string, optional, tag = "6")]
    pub target_dir: Option<String>,
    #[prost(int32, optional, tag = "7")]
    pub exit_code: Option<i32>,
}

// -- cook_index persistent rows ---------------------------------------

#[derive(Clone, PartialEq, Message)]
pub struct WireCookKey {
    #[prost(bytes = "vec", tag = "1")]
    pub recipe_hash: Vec<u8>,
    #[prost(string, tag = "2")]
    pub target_triple: String,
    #[prost(string, tag = "3")]
    pub profile: String,
    #[prost(string, tag = "4")]
    pub channel: String,
    #[prost(string, tag = "5")]
    pub rustc_version: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireCookEntry {
    #[prost(bytes = "vec", tag = "1")]
    pub sha256: Vec<u8>,
    #[prost(uint64, tag = "2")]
    pub size_bytes: u64,
    #[prost(uint64, tag = "3")]
    pub created_unix_ms: u64,
    #[prost(uint64, tag = "4")]
    pub last_used_unix_ms: u64,
    #[prost(string, optional, tag = "5")]
    pub origin_url_normalized: Option<String>,
    #[prost(string, tag = "6")]
    pub cook_cmd_summary: String,
    #[prost(string, optional, tag = "7")]
    pub branch_name: Option<String>,
    #[prost(uint64, tag = "8")]
    pub compile_duration_ms: u64,
    #[prost(uint64, tag = "9")]
    pub save_elapsed_ms: u64,
}
