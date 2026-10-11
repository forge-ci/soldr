//! soldr-daemon — long-lived process that owns target/ tracking, build
//! event correlation (Phase 2), and linked zccache shutdown (Phase 3).
//!
//! Phase 1 scope (this module): IPC server + sync client + PID-based
//! lifecycle, fronting the existing `TargetRegistry` redb writes. The
//! wrapper hot path tries the daemon first via a fire-and-forget
//! socket/pipe write with a 50 ms timeout, then falls back to writing
//! the redb file directly so a missing or unhealthy daemon never blocks
//! a build.
//!
//! The daemon binary is `soldr-daemon` (declared in
//! `crates/soldr-cli/Cargo.toml`); its entry point at
//! `src/bin/soldr_daemon.rs` delegates straight into `server::run()`.
//!
//! `dead_code` is allowed module-wide because the bin and lib compile
//! `daemon::*` independently — symbols used only by the integration
//! tests (lib path) or only by the daemon entry point (bin path) would
//! otherwise be flagged in the other build.

#![allow(dead_code, unused_imports)]

pub mod backend_handle_adoption;
/// soldr#3053 — best-effort on-CPU / off-CPU / task-inventory breach artifacts.
pub(crate) mod breach_forensics;
pub mod bringup;
/// soldr#2224 — the three IPC handlers that touch `state.sqlite3`, split
/// out of the oversized `server.rs`.
pub mod build_session_ops;
pub mod client;
/// soldr#1857 — always-on JSONL record of compiles the daemon ran but
/// could not hand back to the wrapper. The artifact that distinguishes
/// "rustc rejected your code" from "soldr lost a finished compile".
pub mod compile_delivery;
/// Shared argv → `CompileRequest` parser (soldr#2388 Step 6). One parser for
/// the `RUSTC_WRAPPER` client (via `soldr_cli::compile_dispatch`) and the
/// daemon's SESSION codec-bridge, so both convert argv+env+cwd identically.
pub mod compile_request;
pub(crate) mod compile_sink;
/// Per-compile JSONL phase trace, gated by `SOLDR_DAEMON_TRACE`.
/// Diagnostic-only — see `compile_trace.rs` for format. Wired in by
/// soldr#981 to identify the per-compile dispatch bottleneck that
/// the zccache#939 buffer-elimination plan failed to find.
pub mod compile_trace;
pub mod db;
/// soldr#2224 — `spawn_blocking` wrappers so a contended `state.sqlite3`
/// open never parks a tokio worker thread.
pub mod db_async;
/// soldr#1857 — the compile/disconnect race and the durable record of
/// what a lost connection cost. Split out of `server.rs`.
pub mod disconnect;
/// L4 (issue soldr#980) — background batcher that coalesces
/// per-compile redb event writes into one fsync per 64 rows / 100 ms.
pub mod event_batcher;
pub(crate) mod generation_key;
pub mod history_gc;
pub mod image_hash;
/// soldr#3053 — process-global list of compiles currently executing inside
/// the embedded zccache service, so `rss_ceiling`'s breach dump can name
/// the units the daemon was holding and not just how much it held.
pub(crate) mod inflight_compiles;
pub mod ipc;
pub mod ipc_peer;
/// soldr#3053 — registry of pending/recent terminate and cancel decisions.
pub(crate) mod kill_decisions;
pub(crate) mod legacy_sha256_memo;
pub mod lifecycle;
pub mod maintenance;
pub(crate) mod other_generations;
pub mod protocol;
/// soldr#3561 — converge route requests on the generation's claimed daemon.
pub mod route_claim_convergence;
pub(crate) mod route_claim_prune;
/// soldr#3038 / soldr#3057 — optional `SOLDR_DAEMON_RSS_CEILING_BYTES`
/// watchdog: samples this process's own resident set on a short interval
/// (plus mimalloc's exact allocator counters), and on breach writes a
/// memory dump (sampled heap profile, exact counters, `/proc` snapshot)
/// then exits immediately -- fail-fast, not "record and keep running". Also
/// used by `soldr-cli`'s `broker_server.rs` to watch the broker's own RSS.
pub mod rss_ceiling;
/// soldr#3210: runtime threads must outlive the compilers they spawn.
pub(crate) mod runtime_threads;
pub mod server;
pub mod service_definition;
/// SESSION `0x5350` endpoint per-connection handler (soldr#2388 Step 6d/7 /
/// #2386 Option A): drives the `BackendEndpointMux` (probe + `0x5350`) and, on a
/// SESSION frame, replays the buffer into [`session_serve::serve_session_compile`].
/// `pub` because the endpoint helpers and serve entry points are consumed by
/// soldr-cli's broker route and SESSION end-to-end tests.
pub mod session_endpoint;
/// SESSION `0x5350` compile serve — the codec-bridge (soldr#2388 Step 6c):
/// SessionStart → shared parser → embedded zccache → `SessionFrame` output.
pub(crate) mod session_serve;
/// SESSION `0x5350` output sink (soldr#2388 Step 6) — renders a compile's
/// captured stdout/stderr/exit as running-process `SessionFrame`s for the
/// broker-relayed SESSION wire.
pub(crate) mod session_sink;
pub mod shutdown_signal;
pub mod unit_memory_history;
/// soldr#1838 Phase 1 -- progressive heartbeats so a long daemon wait
/// says what it is waiting on instead of going silent to the backstop.
pub(crate) mod wait_heartbeat;
pub mod wire;
