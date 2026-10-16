//! Wire protocol for soldr-daemon — prost-encoded (Protocol Buffers),
//! length-prefixed frames. Each frame on the wire is:
//!
//! ```text
//! [u32 LE body_len][u32 LE protocol_version][prost body]
//! ```
//!
//! `body_len` does NOT include the 4-byte version field. The body is a
//! prost-encoded `WireRequest` (client → server) or `WireResponse`
//! (server → client) as defined in [`crate::daemon::wire`].
//!
//! ## Serialization (post-#603)
//!
//! Bincode is gone. Every binary surface — IPC body + redb persistent
//! rows — is prost-only. Pre-#580 redb rows are evicted on first
//! daemon startup by the migration in `crate::daemon::db::ensure_initialized`
//! and `crate::cache_lib::cook_index::ensure_initialized`; there is no
//! decode-time fallback anymore.

use serde::{Deserialize, Serialize};

/// Protocol version bump rationale:
///
/// * v1–v2: pre-PID-file lifecycle.
/// * v3: target-touch + build-session correlation + linked-zccache.
/// * v4: cook-index IPC (CookLookup, CookRecord, CookTouch) +
///   `cook_stats` on `StatusInfo`. Bincode-bodied wire frames.
/// * v5 (#580): wire body switches from bincode to prost (Protocol
///   Buffers). The header layout is unchanged so the IPC version
///   check still triggers on cross-version traffic.
/// * v5 retained in #603: the wire format did not change — #603 only
///   drops the bincode fallback at the redb layer. Old (v4) clients
///   trying to talk to a v5 daemon (and vice versa) still get a
///   clean "protocol version mismatch" error.
/// * v6 (#977 Phase 5 / #980 L1): adds `Request::Compile` and
///   `Response::CompileResponse` so the rustc-wrapper dispatches every
///   compile to the daemon's embedded `ZccacheService`. The legacy
///   fork-zccache.exe path is gone (#980 L1 second pass) — wrappers
///   that can't reach a v6 daemon hard-error instead of falling back.
/// * v7 (#983 Phase 5b): the daemon stops emitting the single-frame
///   `Response::Compile` for `Request::Compile`. Instead it streams a
///   sequence of `Response::CompileStdoutChunk` / `CompileStderrChunk`
///   frames followed by exactly one terminal `Response::CompileDone`
///   frame. Wrapper-side memory drops from "full rustc stdout+stderr
///   buffered before display" to "at most one chunk in flight". The
///   v6 `CompileResponse` slot is kept on the wire for one release
///   cycle so cross-version traffic still errors as a clean version
///   mismatch instead of a silent oneof mis-decode.
/// * v8 (#1286 F1): adds `Request::FlushCaches` so `soldr save` /
///   `soldr cache flush` can checkpoint the embedded zccache state
///   (artifact index, depgraph snapshot, metadata cache) to disk
///   without shutting the daemon down. Before v8 that state was
///   memory-only until a graceful daemon exit, so archives taken from
///   a live daemon restored with zero rustc hits.
/// * v9 (soldr#1368): adds `Request::CompileStats` so `soldr session
///   end` can read the embedded zccache service's cumulative
///   hit/miss/time-saved counters over IPC and diff them against a
///   session-start baseline — replacing the old `zccache session-end`
///   subprocess against the (removed) managed binary.
/// * v10 (soldr#820): extends `BuildRecord` with optional cache summary,
///   log/archive paths, and miss-reason rollups for `soldr logs`.
/// * v11 (soldr#1467): removes `Request::LinkZccache`,
///   `ZccacheDaemonLink`, and `StatusInfo.linked_zccache` — the
///   external managed-zccache daemon the link tracked was deleted in
///   soldr#1368, so there is nothing left to stop on daemon shutdown.
/// * v12 (soldr#1537): carries optional build-session lifecycle metadata
///   on `CompileRequest`, eliminating two hot-path telemetry IPC calls.
/// * v13 (soldr#1536): `Request::BuildSessionEnd` becomes
///   request-response — the daemon finalizes the session from its
///   in-memory per-session aggregate and replies [`Response::Ack`] once
///   the BuildRecord and every staged session event are durable. No
///   message body changed shape, but the interaction pattern did: a v13
///   client would wait on a reply a v12 daemon never sends, so the bump
///   makes cross-version traffic fail fast (and displace the stale
///   daemon) instead of stalling out the reply timeout.
/// * v14: `CompileStatsInfo` carries zccache phase-profile telemetry so
///   session reports can gate staged-output behavior.
/// * v15 (#1675): `BuildSessionStart` is request-response so persistence
///   failures cannot be acknowledged as successful.
/// * v16 (soldr#1735): a bounded Windows IPC queue reports backpressure
///   and exposes its burst counters through daemon status.
/// * v17: `FlushCaches` returns a structured persistence report instead
///   of an empty Ack, preserving pending/index drain failures and each
///   embedded zccache save step's completed/failed/timed-out outcome.
/// * v18: shutdown acknowledgements and status both carry the daemon
///   generation that accepted the request. Callers can now wait for that
///   exact responder without trusting a PID sampled before the request or
///   signalling a successor after PID reuse.
/// * v19 (soldr#1814 criterion 2 — the daemon becomes the single owner of
///   its own tables): adds `BuildLogInputs`, so the daemon serves the event
///   rows and build record `build_log` previously read by opening
///   `state.sqlite3` itself (slice 2a); and `ShouldWarnCargoDebugDefault`, so
///   the cargo-debug-default read-modify-write stops making every front-door
///   invocation another opener (slice 2c); and `AttachBuildLogHistory`, so the
///   front-door tail stops doing its own get/mutate/upsert of the build
///   record (slice 2d).
/// * v20 (soldr#1838 Phase 2): adds `Retiring`, so a daemon that is shutting
///   down can distinguish graceful retirement from an internal compile error.
///   Mandatory broker clients surface either condition as a hard failure.
/// * v21 (soldr#2023): `StatusInfo` publishes the compile limit the daemon
///   actually applied at startup, and `BuildSessionStart` is acknowledged
///   with `BuildSessionStarted` (carrying the same pair) instead of a bare
///   `Ack`. A running daemon keeps its startup limit, so changing
///   `SOLDR_JOBS` used to be a silent no-op; publishing the applied value is
///   what lets a client notice and say so.
/// * v22 (soldr#2251): daemon-owned target registry snapshot/removal IPC.
/// * v23 (soldr#2866): cook record/hit messages carry the prior compile and
///   archive-save timings used by the conservative restore cost gate.
/// * v24 (soldr#3024): adds a connection-scoped resident-capacity lease.
///   Acquisition reserves embedded compile permits until the same control
///   stream releases them or disconnects.
/// * v25 (soldr#2424): deletes the legacy direct-IPC compile verb. `Compile`,
///   its streamed `CompileStdoutChunk` / `CompileStderrChunk` / `CompileDone`
///   replies, the deprecated single-frame `Compile` reply, `Backpressure`, and
///   the status `ipc_burst_stats` counters only that verb populated are gone.
///   Every compile travels the broker SESSION route.
pub const PROTOCOL_VERSION: u32 = 25;

/// Chunk granularity for streamed compile output (#983 Phase 5b), now
/// emitted as SESSION frames. 64 KiB is the same buffer size cargo's own pipe readers use
/// and matches the typical SO_SNDBUF on a Unix socket / named-pipe.
/// Each frame's prost overhead is a handful of bytes so chunk-size is
/// effectively the IPC frame size on the wire; rustc emits stdout /
/// stderr in much smaller increments, but the daemon coalesces those
/// into `CHUNK_BYTES`-sized frames before sending.
pub const CHUNK_BYTES: usize = 64 * 1024;

/// Maximum prost body size. 4 MiB is comfortably above the largest
/// realistic Compile response (rustc stdout/stderr — typically tens of
/// KiB; pathological warning storms a few hundred KiB; tests that
/// emit MB-scale output are vanishingly rare). Bumped from 64 KiB in
/// #977 Phase 5 to accommodate the `Compile` verb's stdout+stderr
/// payload; non-Compile requests stay tiny (hundreds of bytes).
///
/// Streaming the compile output is a Phase 5+ follow-up; until then
/// any compile that exceeds this cap returns an `InvalidData` error
/// to the wrapper which then transparently falls back to the legacy
/// fork-zccache path.
pub const MAX_BODY_BYTES: u32 = 4 * 1024 * 1024;

/// Back-compat re-export: the decode-error type moved to `core::wire`
/// alongside the prost message types (#1490 Phase 0, edge E2).
pub use crate::core::wire::WireDecodeError;

#[derive(Debug, Clone)]
pub enum Request {
    /// Fire-and-forget: stamp a workspace `target/` path with `unix_seconds`
    /// in the soldr target registry. The wrapper hot path uses this.
    RecordTargetTouch { path: String, unix_seconds: i64 },
    /// Request-response: snapshot daemon-owned target-registry rows for a
    /// CLI filesystem scan. The daemon remains the only state.sqlite3 opener.
    ListTargetRegistry,
    /// Request-response: remove registry rows after a CLI has confirmed its
    /// target directories are absent or successfully deleted.
    RemoveTargetRegistry { paths: Vec<String> },
    /// Request-response: return a small structured snapshot of daemon
    /// state. Used by `soldr daemon status`.
    Status,
    /// Request-response: ask the daemon to drain and exit. Used by
    /// `soldr daemon stop` and (Phase 3) linked-zccache shutdown.
    Shutdown,
    /// Request-response: open a build session. Issued by the cargo
    /// front door immediately before spawning cargo; the Ack means the
    /// start record and SessionStart event are durable.
    BuildSessionStart {
        session_id: u64,
        repo_root: String,
        started_at_ms: i64,
    },
    /// Request-response (since v13, soldr#1536): finalize a build
    /// session. Issued by the cargo front door after cargo exits. The
    /// daemon rolls the session up from its in-memory per-session
    /// aggregate (falling back to the historical event scan only when
    /// it did not observe the session from its start), persists the
    /// finalized BuildRecord, flushes staged session events, and
    /// replies [`Response::Ack`] — the wrapper can then trust the
    /// persisted aggregate instead of re-scanning the event table.
    BuildSessionEnd {
        session_id: u64,
        exit_code: i32,
        ended_at_ms: i64,
    },
    /// Request-response: return the most recent build records, newest
    /// first, up to `limit`. Optional `since_ms` filters to records
    /// whose `started_at_ms >= since_ms`.
    ListBuilds { limit: u32, since_ms: Option<i64> },
    /// Request-response: return finished build records whose
    /// `total_wall_ms >= threshold_ms`, sorted desc by `total_wall_ms`,
    /// capped at `limit`.
    ListSlowBuilds { threshold_ms: u64, limit: u32 },
    /// Request-response: probe the cook-artifact index for the given
    /// `(recipe_hash, target_triple, profile, channel, rustc_version)`
    /// tuple. On exact hit returns [`Response::CookHit`] with
    /// `exact_recipe_match = true`. On exact miss, the daemon may
    /// return a same-origin fallback [`Response::CookHit`] ranked by
    /// `branch_lineage`; otherwise it returns [`Response::CookMiss`].
    CookLookup {
        recipe_hash: [u8; 32],
        target_triple: String,
        profile: String,
        channel: String,
        rustc_version: String,
        origin_url_normalized: Option<String>,
        branch_lineage: Vec<String>,
    },
    /// Request-response: register a cook artifact written by PR 2's
    /// `soldr cook` worker at `~/.soldr/cache/cook/<sha256>.tar.zst`.
    /// Replies with [`Response::Ack`] on success.
    CookRecord {
        recipe_hash: [u8; 32],
        target_triple: String,
        profile: String,
        channel: String,
        rustc_version: String,
        sha256: [u8; 32],
        size_bytes: u64,
        origin_url_normalized: Option<String>,
        branch_name: Option<String>,
        cook_cmd_summary: String,
        /// Wall time of the compile this artifact can avoid; zero is unknown.
        compile_duration_ms: u64,
        /// Observed local archive-save wall time; zero is unknown.
        save_elapsed_ms: u64,
    },
    /// Fire-and-forget: bump the `last_used_unix_ms` field for the
    /// row whose `sha256` matches.
    CookTouch { sha256: [u8; 32] },
    /// Request-response: checkpoint the embedded zccache service's
    /// in-memory state (artifact index, depgraph snapshot, metadata
    /// cache, pending writes) to disk WITHOUT shutting down. Issued by
    /// `soldr save` and `soldr cache flush` before archiving so the
    /// on-disk cache tree is complete (#1286 F1). Replies with
    /// [`Response::CacheFlushed`].
    FlushCaches,
    /// Request-response: return the embedded zccache service's cumulative
    /// compile counters (hits/misses/time-saved/…). Used by `soldr
    /// session start` to capture a baseline and `soldr session end` to
    /// diff against it (soldr#1368). Replies with
    /// [`Response::CompileStats`].
    CompileStats,
    /// Request-response: return the build-log inputs the daemon already owns
    /// for `session_id` — its event rows plus the build record.
    ///
    /// soldr#1814 slice 2a. `build_log` used to read these by opening
    /// `state.sqlite3` itself, making the CLI a second opener of daemon-owned
    /// tables on every build. Two back-to-back `Required` opens (5 s budget
    /// each) is also what exceeded a 10 s test deadline under parallel test
    /// processes. Replies with [`Response::BuildLogInputs`].
    BuildLogInputs { session_id: u64 },
    /// Request-response: should the front door emit the cargo-debug-default
    /// warning for `repo_root`?
    ///
    /// soldr#1814 slice 2c. This is a read-modify-write against
    /// `state_db`'s tables — it records the repo and prunes expired rows — so
    /// having the front door perform it directly made every `soldr cargo`
    /// invocation another opener of `state.sqlite3`. Replies with
    /// [`Response::CargoDebugWarning`].
    ShouldWarnCargoDebugDefault { repo_root: String },
    /// Request-response: attach this build's log-history results to the
    /// daemon's record for `session_id`, creating the row if absent.
    ///
    /// soldr#1814 slice 2d — the last non-fallback CLI opener of the daemon
    /// tables. `persist_build_log_history_inner` used to do
    /// `get_build` → mutate → `upsert_build` (plus `aggregate_session`) itself,
    /// three opens at the cargo front-door tail on every build.
    ///
    /// Deliberately expressed as *intent* rather than as separate get/upsert
    /// verbs: splitting a read-modify-write across two IPC calls would let two
    /// processes interleave and lose one another's fields, reintroducing the
    /// race that single-ownership exists to remove. The daemon applies the
    /// whole merge under its own lock, preserving the same
    /// "first writer wins for ended_at_ms / exit_code" semantics the local
    /// path had. Replies with [`Response::Ack`].
    AttachBuildLogHistory(Box<BuildLogHistoryUpdate>),
    /// Begin a connection-scoped reservation of the embedded compiler's
    /// capacity. The daemon replies only after all requested permits are held.
    AcquireResidentCapacity { permits: u32 },
    /// End the resident-capacity reservation held by this connection.
    /// Valid only as the next frame after a successful acquisition.
    ReleaseResidentCapacity,
}

/// Payload of [`Request::AttachBuildLogHistory`] (soldr#1814 slice 2d).
///
/// Boxed at the call site so this wide struct does not inflate every
/// `Request`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildLogHistoryUpdate {
    pub session_id: u64,
    /// Used only when the daemon holds no row yet and must create one.
    pub repo_root: String,
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    pub exit_code: i32,
    /// When false, the daemon recomputes the crate-count / slowest-crate
    /// aggregate from its event table — mirroring the soldr#1536 rule that a
    /// daemon-acknowledged `BuildSessionEnd` already finalized those.
    pub daemon_finalized: bool,
    pub cache_summary: Option<BuildCacheSummary>,
    pub miss_reasons: Vec<BuildMissReason>,
    pub log_paths: Option<BuildLogPaths>,
}

/// A compile for the daemon's embedded zccache service, built from a SESSION
/// `SessionStart`. Carries the full `rustc` argv plus the
/// surrounding environment so the daemon's embedded service can rebuild
/// the cache key + dispatch the inner compile.
///
/// * `args[0]` is the rustc binary path (e.g. `.../rustc`); the daemon
///   does not re-resolve it, the wrapper has already done the
///   `rustup which rustc` work.
/// * `args[1..]` is the rustc argument list as cargo passed it to the
///   wrapper.
/// * `stdin` is typically empty — rustc reads source from disk. The
///   wrapper's stdin-spill logic (`wrapper::spill_stdin_to_content_addressed_file`)
///   converts cargo's `-` source-on-stdin probes into a file argument
///   *before* the Compile request is built, so the daemon never has to
///   shuffle a multi-MB stdin payload across the IPC boundary.
#[derive(Debug, Clone)]
pub struct CompileRequest {
    pub args: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub stdin: Vec<u8>,
    pub lifecycle: Option<CompileLifecycle>,
    /// Number of transient `ERROR_PIPE_BUSY` retries before this client
    /// connected.
    pub ipc_busy_retries: u32,
}

/// Build-history metadata attached to a session-owned compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileLifecycle {
    pub session_id: u64,
    pub crate_name: String,
    pub target_dir: String,
    pub started_at_ms: i64,
}

/// A completed compile's result from the embedded service. Carries the captured rustc
/// output verbatim so the wrapper can replay it onto its own stdout /
/// stderr before exiting with `exit_code`. `cache_outcome` mirrors
/// `zccache::embedded::CacheOutcome` (1=Hit, 2=Miss, 3=Error); the
/// wrapper does not act on it today but it is forwarded so future
/// audit/profiling work can correlate.
#[derive(Debug, Clone)]
pub struct CompileResponseBody {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub cached: bool,
    /// 1=Hit, 2=Miss, 3=Error. Matches the zccache `CacheOutcome`
    /// integer encoding so we don't introduce a soldr-side enum.
    pub cache_outcome: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetRegistryRow {
    pub path: String,
    pub last_used: i64,
}

#[derive(Debug, Clone)]
pub enum Response {
    Status(StatusInfo),
    TargetRegistryRows(Vec<TargetRegistryRow>),
    TargetRegistryRemoved {
        removed: u32,
    },
    /// Acknowledges graceful shutdown and identifies the daemon that accepted
    /// the request. Callers must wait on this responder, not a PID sampled
    /// before the request.
    ShuttingDown(ShutdownAck),
    Builds(Vec<BuildRecord>),
    Error(String),
    /// The daemon is retiring and will not serve this request.
    ///
    /// soldr#1838 Phase 2. Distinct from [`Response::Error`], which means the
    /// daemon encountered an internal compile-service error. The mandatory
    /// broker route reports both as hard failures while preserving the
    /// distinction for diagnostics and graceful-drain attribution.
    /// #1837 narrowed that window by releasing the Windows pipe instance
    /// early; this closes it, for any request that still lands inside it.
    Retiring,
    /// Reply to [`Request::CookLookup`] on hit. Carries the on-disk
    /// path to the `<sha256>.tar.zst` artifact, the recorded sha256
    /// (PR 3 verifies it before extraction), the byte size for
    /// hydration reporting, the recorded origin URL hint, and whether
    /// the hit was exact or a same-origin fallback seed.
    CookHit {
        sha256: [u8; 32],
        path: String,
        size_bytes: u64,
        origin_url_normalized: Option<String>,
        matched_recipe_hash: Option<[u8; 32]>,
        exact_recipe_match: bool,
        branch_name: Option<String>,
        compile_duration_ms: u64,
        save_elapsed_ms: u64,
    },
    /// Reply to [`Request::CookLookup`] when neither an exact nor
    /// fallback artifact is available. `previous_origin_recipe_hashes`
    /// is a diagnostic: at most 8 prior recipe hashes recorded under
    /// the same `(origin, triple, profile, channel, rustc)` matrix,
    /// newest-first.
    CookMiss {
        previous_origin_recipe_hashes: Vec<[u8; 32]>,
    },
    /// Generic ack for fire-and-forget-style request/response calls
    /// that don't carry a payload (used by [`Request::CookRecord`]).
    Ack,
    /// Acknowledges [`Request::BuildSessionStart`] and reports the compile
    /// limit the answering daemon is running with (soldr#2023).
    ///
    /// A dedicated variant rather than more fields on [`Response::Ack`]:
    /// `Ack` is the shared no-payload reply for several requests, and this
    /// payload is meaningful for exactly one of them. It rides on this
    /// request because the front door already issues it once per build, so
    /// the client learns the daemon's limit for free — a fresh
    /// [`Request::Status`] round-trip on the build path would add IPC to a
    /// path already carrying a documented fixed overhead (#1843).
    BuildSessionStarted {
        compile_jobs: u32,
        compile_jobs_source: String,
    },
    /// Reply to [`Request::CompileStats`] (soldr#1368): the embedded
    /// zccache service's cumulative compile counters.
    CompileStats(CompileStatsInfo),
    /// Reply to [`Request::FlushCaches`]. `complete` is false if any
    /// pending write, index update, or named persistence step failed to
    /// finish successfully before its bound.
    CacheFlushed(CacheFlushInfo),
    /// Reply to [`Request::BuildLogInputs`] (soldr#1814 slice 2a).
    ///
    /// `record` is `None` when the daemon holds no row for the session, which
    /// is a normal outcome rather than an error — the CLI renders the log
    /// without it, exactly as the direct-redb path did.
    BuildLogInputs {
        events: Vec<crate::daemon::db::Event>,
        /// Boxed so this variant does not dominate the size of every
        /// `Response`: `BuildRecord` is wide (several `String`s plus a
        /// `Vec<BuildMissReason>`), and inlining it here tripped
        /// `clippy::large_enum_variant`. `Vec<BuildRecord>` in
        /// [`Response::Builds`] is fine for the same reason a `Box` is —
        /// the payload lives behind a pointer.
        record: Option<Box<BuildRecord>>,
    },
    /// Reply to [`Request::ShouldWarnCargoDebugDefault`] (soldr#1814 slice 2c).
    ///
    /// `emit` is true when the caller should print the warning. The daemon
    /// has already recorded the repo, so a second identical request inside
    /// the throttle window answers false.
    CargoDebugWarning {
        emit: bool,
    },
    /// The requested resident compile permits are now held by this connection.
    ResidentCapacityAcquired {
        permits: u32,
    },
}

/// Identity of the daemon generation that accepted a shutdown request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownAck {
    pub pid: u32,
    pub generation: u64,
}

/// Structured checkpoint result from the embedded zccache service.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheFlushInfo {
    pub complete: bool,
    pub pending_writes_drained: bool,
    pub index_writer_drained: bool,
    pub steps: Vec<CacheFlushStepInfo>,
    pub artifact_entries: u64,
    pub metadata_entries: u64,
}

impl CacheFlushInfo {
    /// Recompute completeness from every constituent field instead of trusting
    /// only the peer-provided summary bit.
    pub fn is_complete(&self) -> bool {
        self.complete
            && self.pending_writes_drained
            && self.index_writer_drained
            && self.steps.iter().all(|step| step.status == "completed")
    }

    pub fn incomplete_reason(&self) -> String {
        let mut reasons = Vec::new();
        if !self.pending_writes_drained {
            reasons.push("pending_writes=timed_out".to_owned());
        }
        if !self.index_writer_drained {
            reasons.push("index_writer=timed_out".to_owned());
        }
        reasons.extend(
            self.steps
                .iter()
                .filter(|step| step.status != "completed")
                .map(|step| match &step.error {
                    Some(error) => format!("{}={} ({error})", step.step, step.status),
                    None => format!("{}={}", step.step, step.status),
                }),
        );
        if reasons.is_empty() && !self.complete {
            reasons.push("daemon_reported_incomplete".to_owned());
        }
        reasons.join(", ")
    }
}

/// Result of one named embedded-cache persistence step.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheFlushStepInfo {
    pub step: String,
    /// Stable lowercase identifier: `completed`, `failed`, or `timed_out`.
    pub status: String,
    pub error: Option<String>,
}

/// Cumulative compile counters from the embedded zccache service
/// (soldr#1368). A monotonic snapshot; `soldr session end` diffs two
/// snapshots (start baseline vs. end) to report per-session hit/miss
/// figures. Mirrors the fields of `zccache::embedded::ServiceStats` that
/// soldr surfaces; kept as a plain struct so the daemon layer never has
/// to import `zccache::embedded::*`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompileStatsInfo {
    pub total_compilations: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub non_cacheable: u64,
    pub compile_errors: u64,
    pub time_saved_ms: u64,
    #[serde(default)]
    pub staged_profile: Option<StagedProfileInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedProfileInfo {
    pub counters: std::collections::BTreeMap<String, u64>,
    pub timings_ns: std::collections::BTreeMap<String, u64>,
    pub bytes: std::collections::BTreeMap<String, u64>,
    pub failures: std::collections::BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusInfo {
    pub version: u32,
    pub pid: u32,
    /// Process-start generation shared with [`ShutdownAck`].
    pub generation: u64,
    pub uptime_secs: u64,
    pub request_count: u64,
    /// Cook-index aggregate stats (issue #576).
    pub cook_stats: Option<CookStats>,
    /// Compile-dispatch backend label. Always [`COMPILE_BACKEND_EMBEDDED`]
    /// since #980 L1's second pass — the legacy "wrapped" fork-zccache.exe
    /// backend is gone. Field retained on the wire so v6 status snapshots
    /// stay stable for telemetry consumers.
    #[serde(default)]
    pub compile_backend: String,
    /// soldr#2023: the compile-concurrency limit this daemon applied when it
    /// started, and the precedence tier it came from.
    ///
    /// Deliberately the *applied* value, not a fresh resolution. A daemon
    /// outlives the environment that spawned it, so re-resolving here would
    /// report the limit a new daemon would pick — which is exactly the value
    /// a caller wants to compare *against*, and useless as both halves of
    /// the comparison.
    #[serde(default)]
    pub compile_jobs: u32,
    #[serde(default)]
    pub compile_jobs_source: String,
    /// soldr#3815: compiles in flight right now. The broker consults this
    /// before reaping a route whose requesters have exited. `None` means the
    /// daemon did not report it (an image that predates the field): unknown,
    /// never idle.
    #[serde(default)]
    pub in_flight_compiles: Option<u32>,
}

/// Canonical `compile_backend` value emitted by the daemon. The
/// "wrapped" label and its associated dispatch path were deleted in
/// #980 L1 second pass; the daemon now always advertises this value.
pub const COMPILE_BACKEND_EMBEDDED: &str = "embedded";

impl StatusInfo {
    /// Resolve `cook_stats` to a concrete value, treating `None` as
    /// the zero state.
    pub fn cook_stats_or_zero(&self) -> CookStats {
        self.cook_stats.clone().unwrap_or_default()
    }
}

/// Aggregate counts for the cook-artifact index. Surfaced through
/// [`Request::Status`] and rendered by `soldr daemon status` /
/// `soldr doctor`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookStats {
    /// Number of rows currently in `cook_index_v2`.
    pub entries: u64,
    /// Sum of `size_bytes` across all rows.
    pub total_bytes: u64,
    /// Number of [`Request::CookLookup`] hits served by the running
    /// daemon since its last startup. Resets across daemon restarts.
    pub hits_this_session: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildCacheSummary {
    pub hits: u64,
    pub misses: u64,
    pub non_cacheable: u64,
    pub errors: u64,
    pub compilations: u64,
    pub time_saved_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildLogPaths {
    pub zccache_session_id: Option<String>,
    pub cache_dir: Option<String>,
    pub session_log_path: Option<String>,
    pub journal_path: Option<String>,
    pub session_stats_path: Option<String>,
    pub compile_journal_path: Option<String>,
    pub archived_session_log_path: Option<String>,
    pub archived_journal_path: Option<String>,
    pub archived_session_stats_path: Option<String>,
    pub archived_compile_journal_path: Option<String>,
    pub private_daemon_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildMissReason {
    pub reason: String,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildRecord {
    pub session_id: u64,
    pub repo_root: String,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub exit_code: Option<i32>,
    pub total_wall_ms: Option<u64>,
    pub crate_count: u32,
    pub slowest_crate_us: Option<u64>,
    pub slowest_crate_name: Option<String>,
    pub cache_summary: Option<BuildCacheSummary>,
    pub log_paths: Option<BuildLogPaths>,
    pub miss_reasons: Vec<BuildMissReason>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Deliberately pinned to a literal: bumping the constant must be a
    // conscious act, because peers at different versions reject each other.
    // soldr#2023 renamed this from the v20 spelling when the daemon began
    // publishing its applied compile limit.
    #[test]
    fn protocol_version_is_v25_after_deleting_the_direct_ipc_compile_verb() {
        assert_eq!(PROTOCOL_VERSION, 25);
    }

    #[test]
    fn chunk_bytes_is_64_kib() {
        // #983 Phase 5b — declared in the protocol so the daemon and
        // wrapper agree on the frame-size budget without re-importing
        // an opaque constant from each other's modules.
        assert_eq!(CHUNK_BYTES, 64 * 1024);
    }

    #[test]
    fn cache_flush_completeness_is_derived_not_blindly_trusted() {
        let mut report = CacheFlushInfo {
            complete: true,
            pending_writes_drained: true,
            index_writer_drained: true,
            steps: vec![CacheFlushStepInfo {
                step: "depgraph".into(),
                status: "completed".into(),
                error: None,
            }],
            ..Default::default()
        };
        assert!(report.is_complete());
        report.steps[0].status = "failed".into();
        assert!(!report.is_complete());
    }

    #[test]
    fn cook_stats_or_zero_defaults_to_zero() {
        let info = StatusInfo {
            version: PROTOCOL_VERSION,
            pid: 1,
            generation: 2,
            uptime_secs: 0,
            request_count: 0,
            cook_stats: None,
            compile_backend: COMPILE_BACKEND_EMBEDDED.to_string(),
            compile_jobs: 8,
            compile_jobs_source: "default".to_string(),
            in_flight_compiles: Some(0),
        };
        assert_eq!(info.cook_stats_or_zero(), CookStats::default());
    }
}
