//! soldr-fetch — binary resolution and download (#1490 Phase 3).
//!
//! The `fetch` tree stays nested as `pub mod fetch;` (not flattened)
//! so `crate::fetch::…` paths inside the moved files — and
//! `soldr_cli::fetch::…` via the facade re-export — resolve unchanged
//! (#1490 mechanics rule M1). The `core` re-export below satisfies the
//! tree's `crate::core::…` paths through the crate root (rule M2).
//!
//! `build.rs` + `embed/` moved with this crate: the build script
//! compresses `embed/manifest.json` into `OUT_DIR/manifest.json.zst`,
//! which `fetch::manifest_v6` pulls in via `include_bytes!` — `OUT_DIR`
//! is per-crate, so they must live together.

#![allow(dead_code)]

/// Neutral host-platform facade (#2493): the single selection site lives
/// in `soldr-platform`; this crate calls only `crate::platform::…`.
pub(crate) use soldr_platform as platform;

pub use soldr_core::core;

pub mod fetch;

pub use kernal_api::http::Response as RemoteBuildHttpResponse;

pub use fetch::stream_download::{open_signal_websocket, SignalWebSocketTransport};
