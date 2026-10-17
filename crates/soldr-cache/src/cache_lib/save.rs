//! `soldr save` / `soldr load`: produce and consume single-file archives
//! that bundle (a) a build-cache directory's contents and (b) a snapshot
//! of source-file mtimes the next compiler invocation will fingerprint.
//!
//! ## Why this exists
//!
//! Two real problems we kept hitting in the setup-soldr GitHub Action,
//! split out as soldr-side primitives so any project that builds from a
//! fresh checkout can opt in without the action wrapper:
//!
//! 1. **Cache-archive transport.** Bespoke tar+zstd plumbing in TS picked
//!    up subtle bugs (decompressing back into `<cacheDir>` instead of
//!    `dirname(<cacheDir>)` produced silent double-nesting and made every
//!    restore look fine while zccache's lookups missed). One Rust impl,
//!    one test suite, no shell quoting drama.
//!
//! 2. **Source-file mtimes.** `actions/checkout` (and any equivalent
//!    sandbox restore) rewrites every source file's mtime on every run,
//!    so Cargo's fingerprint files invalidate on the first stat. We snap
//!    cold's mtimes + content-hashes into the archive, then on load we
//!    only replay an mtime if the current file is byte-identical to the
//!    snapshot — never underbuilds the way a blind `touch -d <commit
//!    time>` does.
//!
//! ## Format
//!
//! A single `.tar.zst` (zstd-compressed POSIX tar). Inside:
//!
//! - `SOLDR_MANIFEST.pb` at the archive root — the protobuf manifest.
//! - `cache/...` — the build cache directory's contents.
//!
//! `cache/` is always exactly one level deep at the archive root so
//! readers never have to guess the basename. Manifest carries
//! `cache_dir_name = "cache"` for forward compat.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};

use prost::Message as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use soldr_platform::sync::Mutex;
use thiserror::Error;
// soldr#3289: source-mtime replay delegates to zccache's content-verified
// per-file replay. Only the types are imported; zccache's `replay_one` fn is
// always called fully qualified so it cannot collide with soldr's adapter.
use zccache::fingerprint::mtime_replay::{MtimeEntry, ReplayOutcome};

pub mod proto {
    //! Hand-written prost types corresponding to `manifest.proto` in this
    //! directory.
    //!
    //! We write these by hand rather than running `prost-build` from a
    //! build script because the codegen pulls in `protoc` (the C++
    //! protobuf compiler) at build time, and we don't want to require
    //! every CI runner / contributor to install it. The schema is tiny
    //! and stable; the `.proto` file is preserved as the source of
    //! truth for anyone reading the wire format. If you change one,
    //! change the other — and bump `Manifest::version` if the change
    //! is breaking.

    use prost::{Enumeration, Message};

    #[derive(Clone, PartialEq, Message)]
    pub struct Manifest {
        #[prost(uint32, tag = "1")]
        pub version: u32,
        #[prost(int64, tag = "2")]
        pub saved_at_ms: i64,
        #[prost(string, tag = "3")]
        pub workspace: ::prost::alloc::string::String,
        #[prost(string, tag = "4")]
        pub cache_dir_name: ::prost::alloc::string::String,
        #[prost(message, repeated, tag = "5")]
        pub files: ::prost::alloc::vec::Vec<SourceFile>,
        #[prost(uint64, tag = "6")]
        pub source_file_count: u64,
        #[prost(uint64, tag = "7")]
        pub cache_file_count: u64,
        #[prost(enumeration = "CacheLayerKind", tag = "8")]
        pub cache_layer_kind: i32,
        #[prost(message, repeated, tag = "9")]
        pub cache_files: ::prost::alloc::vec::Vec<CacheFile>,
        #[prost(bytes = "vec", tag = "10")]
        pub base_manifest_blake3: ::prost::alloc::vec::Vec<u8>,
        #[prost(string, repeated, tag = "11")]
        pub deleted_cache_paths: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
        #[prost(message, repeated, tag = "12")]
        pub cache_symlinks: ::prost::alloc::vec::Vec<SymlinkEntry>,
    }

    /// A symlink below the cache dir (#1548). Manifest-only: symlinks are
    /// never carried as tar entries, so pre-#1548 loaders simply skip this
    /// unknown field and behave exactly as before (no link restored).
    #[derive(Clone, PartialEq, Message)]
    pub struct SymlinkEntry {
        /// Cache-dir-relative POSIX path of the link itself.
        #[prost(string, tag = "1")]
        pub path: ::prost::alloc::string::String,
        /// Raw link target, forward-slashed. Always RELATIVE — absolute or
        /// root-escaping targets are rejected at save time and re-rejected
        /// at load time before any link is created.
        #[prost(string, tag = "2")]
        pub target: ::prost::alloc::string::String,
        /// Whether the target resolved to a directory at save time. Used on
        /// Windows to pick symlink_dir vs symlink_file at restore.
        #[prost(bool, tag = "3")]
        pub is_dir: bool,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct SourceFile {
        #[prost(string, tag = "1")]
        pub path: ::prost::alloc::string::String,
        #[prost(int64, tag = "2")]
        pub mtime_ms: i64,
        /// Exact source mtime; absent in archives written before #3851.
        #[prost(int64, optional, tag = "5")]
        pub mtime_ns: Option<i64>,
        #[prost(uint64, tag = "3")]
        pub size: u64,
        #[prost(bytes = "vec", tag = "4")]
        pub blake3: ::prost::alloc::vec::Vec<u8>,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct CacheFile {
        #[prost(string, tag = "1")]
        pub path: ::prost::alloc::string::String,
        #[prost(int64, tag = "2")]
        pub mtime_ns: i64,
        #[prost(uint64, tag = "3")]
        pub size: u64,
        #[prost(bytes = "vec", tag = "4")]
        pub blake3: ::prost::alloc::vec::Vec<u8>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Enumeration)]
    #[repr(i32)]
    pub enum CacheLayerKind {
        Complete = 0,
        Base = 1,
        Delta = 2,
    }
}

pub use proto::{CacheFile, CacheLayerKind, Manifest, SourceFile, SymlinkEntry};

/// Well-known filename for the manifest inside an archive.
pub const MANIFEST_NAME: &str = "SOLDR_MANIFEST.pb";

/// Directory name for the bundled cache contents inside an archive.
pub const CACHE_DIR_NAME: &str = "cache";

/// Current manifest schema version. Bump only on breaking format changes.
pub const MANIFEST_VERSION: u32 = 1;

/// Environment variable that overrides the parallel-extract worker count
/// inside `soldr load` (#575). Accepts a positive integer; clamps to at
/// least 1. When unset or unparseable, falls back to `--threads` (if the
/// caller passed it) and then to rayon's default (num_cpus). The override
/// also sizes the rayon pool used by `load()` so concurrent mtime-replay
/// and cache-file extraction share the same parallelism budget.
pub const LOAD_WORKERS_ENV: &str = "SOLDR_LOAD_WORKERS";

/// Fallback worker count for `soldr load` when `--threads` is unset, the
/// env override is unset, and we can't probe rayon's default. Effectively
/// dead code on supported platforms (rayon always returns a positive
/// `current_num_threads()`), but documents the floor explicitly. Used by
/// the parallel cache-file extractor in `load()`.
pub const DEFAULT_LOAD_WORKERS: usize = 4;
const REPLAY_WORKER_RECV_TIMEOUT: Duration = Duration::from_secs(60);
/// How long one extraction worker holds the shared receiver while polling it,
/// and how long the others wait for it before re-polling. The receiver is
/// behind a `soldr_platform::sync::Mutex`, whose critical-section watchdog
/// aborts a hold or wait past 15 s (soldr#3836), so the poll stays short; a
/// timeout only means the driver is slow and the worker simply polls again.
const EXTRACT_WORKER_RECV_TIMEOUT: Duration = Duration::from_secs(1);

/// Default zstd compression level — matches what setup-soldr's TS impl
/// has been using (level 3 gives ~0.26 ratio on Rust artifact caches at
/// roughly 1 GB/s on a Linux CI runner).
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;

/// Environment variable that selects the default `soldr save` profile
/// when the CLI does not pass `--ci` / `--minimal`.
pub const SAVE_PROFILE_ENV: &str = "SOLDR_SAVE_PROFILE";

/// Payload profile for `soldr save`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SaveProfile {
    /// Historical behavior: archive every regular file below `cache_dir`.
    #[default]
    Full,
    /// CI/minimal v1: archive cache payload needed for warm hits while
    /// excluding logs, runtime scratch, sockets, lock files, zccache
    /// runtime binaries, and soldr managed binary/toolchain trees that
    /// do not participate in rustc hits.
    Ci,
    /// Cook payload (soldr#2996): archive the dependency graph of a cargo
    /// target directory while excluding the linked products tier 3 forbids.
    ///
    /// `soldr cook`'s own output is a dependency tree, but the setup-soldr
    /// cook layer archives the target dir from a *post* step -- by which
    /// point the real build has written its binaries and test executables
    /// into the same tree. On one measured run that turned an 83 MiB cook
    /// slice into a 1.62 GB entry (zackees/setup-soldr#499). This profile
    /// drops what the build added and keeps what cook produced.
    Cook,
}

impl SaveProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Ci => "ci",
            Self::Cook => "cook",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "full" | "default" | "complete" => Some(Self::Full),
            "ci" | "minimal" => Some(Self::Ci),
            "cook" => Some(Self::Cook),
            _ => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum SaveLoadError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("io error: {0}")]
    BareIo(#[from] std::io::Error),
    #[error("manifest encode failed: {0}")]
    Encode(#[from] prost::EncodeError),
    #[error("manifest decode failed: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("archive is missing the {MANIFEST_NAME} entry")]
    MissingManifest,
    #[error("invalid path inside archive: {0}")]
    BadArchivePath(String),
    #[error("zstd error: {0}")]
    Zstd(std::io::Error),
    #[error("walk error at {path}: {message}")]
    Walk { path: PathBuf, message: String },
}

fn io(path: impl Into<PathBuf>, source: std::io::Error) -> SaveLoadError {
    SaveLoadError::Io {
        path: path.into(),
        source,
    }
}

/// True when `err` is an I/O `NotFound` — i.e. the path disappeared
/// between two passes over a tree a live daemon is still writing
/// (#3533). Scoped to `NotFound` only: a permissions error or a bad disk
/// still fails loudly, because those mean the archive would be silently
/// incomplete.
fn save_error_is_not_found(err: &SaveLoadError) -> bool {
    match err {
        SaveLoadError::Io { source, .. } | SaveLoadError::BareIo(source) => {
            source.kind() == std::io::ErrorKind::NotFound
        }
        _ => false,
    }
}

pub type Result<T> = std::result::Result<T, SaveLoadError>;

// ---------- helpers shared by save + load ----------

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn ns_to_systime(ns: i64) -> SystemTime {
    if ns < 0 {
        UNIX_EPOCH
    } else {
        UNIX_EPOCH + Duration::from_nanos(ns as u64)
    }
}

include!("save_inventory.rs");
include!("save_projection.rs");
include!("save_archive.rs");
include!("load_extract.rs");
include!("load_replay.rs");
#[cfg(test)]
#[path = "save_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "load_extract_tests.rs"]
mod load_extract_tests;
#[cfg(test)]
#[path = "load_replay_tests.rs"]
mod load_replay_tests;
#[cfg(test)]
#[path = "save_archive_mode_tests.rs"]
mod save_archive_mode_tests;

#[cfg(test)]
#[path = "source_mtime_tests.rs"]
mod source_mtime_tests;
