//! Strict PCH input validation with content-verified mtime replay (#1952).
//!
//! A clang PCH records, for every input header, the spelling, size and mtime
//! (seconds) clang saw, plus an xxh3 hash of the content. A consumer that
//! loads the PCH compares those with the workspace in two places:
//!
//! 1. validity: a size or mtime mismatch invalidates the PCH, except that
//!    `-fpch-validate-input-files-content` forgives an mtime mismatch whose
//!    content hash still matches;
//! 2. the "already included" / `#pragma once` lookup, keyed on (size, mtime).
//!    The content flag does not relax this one.
//!
//! So a byte-identical header whose mtime differs from the record makes the
//! PCH load but lose that header's `#pragma once` state: it is parsed again
//! and the consumer fails with `redefinition of ...`. A compiler cache that
//! serves a PCH built earlier hits exactly that case whenever a header was
//! touched (checkout, `git stash`, an editor save) without changing.
//!
//! This module reads the records straight from the PCH ([`read_pch_inputs`])
//! and decides whether a cached PCH can be served into the current workspace
//! ([`plan_pch_serve`]):
//!
//! - path missing or not a regular file: refuse;
//! - size differs: refuse (checked before hashing);
//! - content differs from the recorded hash: refuse;
//! - content identical but mtime differs: set the header's mtime back to the
//!   recorded value ([`apply_pch_replay`]), then serve. Only files under the
//!   caller's replay root are ever touched, and only when replay is enabled.
//!
//! Every failure biases toward rebuilding: a refusal makes the caller compile
//! the PCH fresh, which is always correct.

mod bitstream;

use std::io::Read;
use std::path::Path;

use kernal_api::platform::fs::FileTime;
use crate::core::NormalizedPath;

/// One `INPUT_FILE` record of a clang PCH, with its `INPUT_FILE_HASH`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PchInput {
    /// The name clang resolves and stats when loading the PCH.
    pub path: String,
    /// The name as the include directive / command line requested it.
    pub as_requested: String,
    /// Size in bytes clang recorded.
    pub size: u64,
    /// Modification time (whole seconds since the epoch) clang recorded.
    pub mtime_secs: u64,
    /// The recorded content hash, when clang recorded one: xxh3-64 for
    /// clang >= 19, an unrecomputable `llvm::hash_code` before that.
    pub content_xxh3: Option<u64>,
    /// The input was a virtual (remapped) buffer, not a file on disk.
    pub overridden: bool,
    /// The input was transient (not expected to persist on disk).
    pub transient: bool,
    /// The input was a top-level file of the compile (the PCH's own header).
    pub top_level: bool,
    /// This is an imported PCH (a chained PCH's `-include-pch` base) from
    /// the `CONTROL_BLOCK` `IMPORT` record, not a header. clang records only
    /// its size and mtime; its content is part of the cache key, which
    /// hashes a PCH dependency's bytes on every lookup.
    pub imported_pch: bool,
}

/// Reading a PCH's input records failed.
#[derive(Debug, thiserror::Error)]
pub enum PchInputsError {
    #[error("PCH ended before its input-file records")]
    Truncated,
    #[error("malformed PCH bitstream: {0}")]
    Malformed(&'static str),
    #[error("reading PCH: {0}")]
    Io(#[from] std::io::Error),
}

/// Bytes read first; the control block sits at the head of the file, so this
/// covers it for all but enormous input lists.
const INITIAL_READ_BYTES: u64 = 1 << 20;

/// Parse the input records of a clang PCH held in memory.
///
/// `Ok(None)` means the bytes are not a clang PCH (no `CPCH` magic).
pub fn parse_pch_inputs(bytes: &[u8]) -> Result<Option<Vec<PchInput>>, PchInputsError> {
    bitstream::parse(bytes)
}

/// Read the input records of the clang PCH at `path`, reading only the head
/// of the file unless the input list extends past it.
///
/// `Ok(None)` means the file is not a clang PCH (for example a GCC `.gch`).
pub fn read_pch_inputs(path: &Path) -> Result<Option<Vec<PchInput>>, PchInputsError> {
    let file = std::fs::File::open(path)?;
    let mut head = Vec::new();
    file.take(INITIAL_READ_BYTES).read_to_end(&mut head)?;
    match bitstream::parse(&head) {
        Err(PchInputsError::Truncated) if head.len() as u64 == INITIAL_READ_BYTES => {
            bitstream::parse(&std::fs::read(path)?)
        }
        other => other,
    }
}

/// xxh3-64 of `bytes`, the hash clang stores in `INPUT_FILE_HASH`.
#[must_use]
pub fn content_xxh3(bytes: &[u8]) -> u64 {
    twox_hash::XxHash3_64::oneshot(bytes)
}

/// Why a cached PCH cannot be served into the current workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PchRefusalReason {
    /// The recorded input is missing or not a regular file.
    Missing,
    /// The recorded path is relative; clang PCH inputs are absolute.
    RelativePath,
    /// The input's size differs from the record.
    SizeMismatch { recorded: u64, actual: u64 },
    /// The input's content hash differs from the record.
    ContentMismatch,
    /// The mtime differs and the content cannot be verified: clang recorded
    /// no hash, or (clang < 19) a hash that is not xxh3.
    MtimeUnverifiable,
    /// The mtime differs, content matches, but replay is turned off.
    ReplayDisabled,
    /// The mtime differs, content matches, but the file is outside the
    /// replay root, so zccache will not touch it.
    OutsideReplayRoot,
    /// The PCH's own header is not the header this compile names: the PCH
    /// was built in another checkout or from another spelling.
    TopLevelMismatch,
    /// Reading or hashing the input failed.
    Unreadable(String),
    /// Setting the recorded mtime failed.
    ReplayFailed(String),
    /// The input changed while its mtime was being replayed.
    ChangedDuringReplay,
}

impl std::fmt::Display for PchRefusalReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("missing"),
            Self::RelativePath => f.write_str("relative_path"),
            Self::SizeMismatch { recorded, actual } => {
                write!(f, "size_mismatch recorded={recorded} actual={actual}")
            }
            Self::ContentMismatch => f.write_str("content_mismatch"),
            Self::MtimeUnverifiable => f.write_str("mtime_mismatch_unverifiable"),
            Self::ReplayDisabled => f.write_str("mtime_mismatch_replay_disabled"),
            Self::OutsideReplayRoot => f.write_str("mtime_mismatch_outside_replay_root"),
            Self::TopLevelMismatch => f.write_str("top_level_mismatch"),
            Self::Unreadable(error) => write!(f, "unreadable: {error}"),
            Self::ReplayFailed(error) => write!(f, "replay_failed: {error}"),
            Self::ChangedDuringReplay => f.write_str("changed_during_replay"),
        }
    }
}

/// A refusal, naming the input that caused it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PchRefusal {
    pub path: String,
    pub reason: PchRefusalReason,
}

impl std::fmt::Display for PchRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.reason)
    }
}

/// An input whose content verified but whose mtime must be set back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingReplay {
    pub path: NormalizedPath,
    /// The mtime clang recorded, which the file will be given.
    pub recorded_secs: u64,
    /// The mtime the file had when it was verified.
    pub current_secs: i64,
    size: u64,
    /// `None` for an imported PCH, which is re-checked by size only.
    content_xxh3: Option<u64>,
}

/// The outcome of [`plan_pch_serve`]: the PCH may be served once every
/// pending replay has been applied.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PchServePlan {
    /// Inputs that were checked against the workspace.
    pub inputs_checked: usize,
    /// Inputs whose mtime must be set to the recorded value.
    pub replays: Vec<PendingReplay>,
}

/// How [`plan_pch_serve`] treats the workspace.
#[derive(Clone, Copy, Debug)]
pub struct PchServeOptions<'a> {
    /// Only files under this directory may have their mtime replayed.
    pub replay_root: &'a Path,
    /// The header this compile precompiles. When set, the PCH's top-level
    /// input must be this same file.
    pub expected_top_level: Option<&'a Path>,
    /// Whether an mtime-only mismatch may be repaired by replay.
    pub replay_enabled: bool,
}

fn refuse(input: &PchInput, reason: PchRefusalReason) -> PchRefusal {
    PchRefusal {
        path: input.path.clone(),
        reason,
    }
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    FileTime::from_last_modification_time(meta).unix_seconds()
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Check every recorded input of a cached PCH against the workspace and
/// return the mtime replays needed to serve it, or the first refusal.
///
/// Nothing is modified here; [`apply_pch_replay`] performs the replays.
pub fn plan_pch_serve(
    inputs: &[PchInput],
    options: PchServeOptions<'_>,
) -> Result<PchServePlan, PchRefusal> {
    if let Some(expected) = options.expected_top_level {
        let mut top_level = inputs.iter().filter(|input| input.top_level).peekable();
        if top_level.peek().is_some()
            && !top_level.any(|input| same_file(Path::new(&input.path), expected))
        {
            let first = inputs.iter().find(|input| input.top_level);
            return Err(PchRefusal {
                path: first.map_or_else(String::new, |input| input.path.clone()),
                reason: PchRefusalReason::TopLevelMismatch,
            });
        }
    }
    // Pass 1: existence, size, and the content hash of every input.
    let mut checked = Vec::new();
    for input in inputs {
        if input.overridden || input.transient {
            continue;
        }
        let path = Path::new(&input.path);
        if !path.is_absolute() {
            return Err(refuse(input, PchRefusalReason::RelativePath));
        }
        let meta = match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() => meta,
            _ => return Err(refuse(input, PchRefusalReason::Missing)),
        };
        if meta.len() != input.size {
            return Err(refuse(
                input,
                PchRefusalReason::SizeMismatch {
                    recorded: input.size,
                    actual: meta.len(),
                },
            ));
        }
        let xxh3_matches = match input.content_xxh3 {
            Some(recorded) => {
                let bytes = std::fs::read(path).map_err(|error| {
                    refuse(input, PchRefusalReason::Unreadable(error.to_string()))
                })?;
                Some(content_xxh3(&bytes) == recorded)
            }
            None => None,
        };
        checked.push((input, mtime_secs(&meta), xxh3_matches));
    }
    // clang >= 19 stores xxh3-64 in INPUT_FILE_HASH; older clang stores an
    // `llvm::hash_code`, which cannot be recomputed here. One matching input
    // proves the PCH uses xxh3 (a 64-bit coincidence is negligible), so every
    // mismatch is then a real content change. Otherwise the hashes are
    // unusable: inputs fall back to clang's own size + mtime check and an
    // mtime mismatch cannot be verified, so it is refused (never replayed).
    let hashes_are_xxh3 = checked.iter().any(|(_, _, matches)| *matches == Some(true));

    // Pass 2: decide each input.
    let mut plan = PchServePlan {
        inputs_checked: checked.len(),
        replays: Vec::new(),
    };
    for (input, current_secs, xxh3_matches) in checked {
        // clang skips the timestamp check for a zero recorded mtime.
        let mtime_matches =
            input.mtime_secs == 0 || u64::try_from(current_secs).ok() == Some(input.mtime_secs);
        let verified_hash = match (xxh3_matches, hashes_are_xxh3) {
            (Some(false), true) => {
                return Err(refuse(input, PchRefusalReason::ContentMismatch));
            }
            (Some(true), true) => input.content_xxh3,
            _ if mtime_matches => None,
            // An imported PCH has no content hash; its bytes are part of
            // the cache key, which hashes PCH dependencies on every lookup.
            (None, _) if input.imported_pch => None,
            _ => return Err(refuse(input, PchRefusalReason::MtimeUnverifiable)),
        };
        if mtime_matches {
            continue;
        }
        if !options.replay_enabled {
            return Err(refuse(input, PchRefusalReason::ReplayDisabled));
        }
        let path = Path::new(&input.path);
        if !path.starts_with(options.replay_root) {
            return Err(refuse(input, PchRefusalReason::OutsideReplayRoot));
        }
        plan.replays.push(PendingReplay {
            path: NormalizedPath::new(path),
            recorded_secs: input.mtime_secs,
            current_secs,
            size: input.size,
            content_xxh3: verified_hash,
        });
    }
    Ok(plan)
}

/// Whether every recorded input still has the size and mtime clang
/// recorded, so loading this PCH needs no repair. Stats only, no hashing:
/// the cheap check run before every compile that loads a PCH.
#[must_use]
pub fn inputs_stat_match(inputs: &[PchInput]) -> bool {
    inputs
        .iter()
        .filter(|input| !input.overridden && !input.transient)
        .all(|input| {
            std::fs::metadata(&input.path).is_ok_and(|meta| {
                meta.is_file()
                    && meta.len() == input.size
                    && (input.mtime_secs == 0
                        || u64::try_from(mtime_secs(&meta)).ok() == Some(input.mtime_secs))
            })
        })
}

/// Apply the replays of a [`PchServePlan`]: give each input its recorded
/// mtime, then re-verify its size and content.
///
/// If an input changed between verification and replay, its mtime is set to
/// now so build tools see it as newer than anything built from it, and the
/// PCH must not be served. Returns the replays applied.
pub fn apply_pch_replay(plan: &PchServePlan) -> Result<Vec<PendingReplay>, PchRefusal> {
    apply_pch_replay_with(plan, kernal_api::platform::fs::set_file_mtime)
}

fn apply_pch_replay_with(
    plan: &PchServePlan,
    set_time: impl Fn(&Path, FileTime) -> std::io::Result<()>,
) -> Result<Vec<PendingReplay>, PchRefusal> {
    let refusal = |replay: &PendingReplay, reason| PchRefusal {
        path: replay.path.to_string_lossy().into_owned(),
        reason,
    };
    for replay in &plan.replays {
        let recorded =
            FileTime::from_unix_time(i64::try_from(replay.recorded_secs).unwrap_or(i64::MAX), 0);
        set_time(&replay.path, recorded)
            .map_err(|error| refusal(replay, PchRefusalReason::ReplayFailed(error.to_string())))?;
        let unchanged = match replay.content_xxh3 {
            Some(hash) => std::fs::read(&replay.path).is_ok_and(|bytes| {
                bytes.len() as u64 == replay.size && content_xxh3(&bytes) == hash
            }),
            None => std::fs::metadata(&replay.path).is_ok_and(|meta| meta.len() == replay.size),
        };
        if !unchanged {
            // Bias toward rebuilding: whatever changed must look newer than
            // every output built from the old content.
            if let Err(error) = set_time(&replay.path, FileTime::now()) {
                tracing::warn!(
                    path = %replay.path.display(),
                    %error,
                    "could not mark a PCH input changed during replay as new"
                );
            }
            return Err(refusal(replay, PchRefusalReason::ChangedDuringReplay));
        }
    }
    Ok(plan.replays.clone())
}

/// Whether the client environment turns PCH input mtime replay off.
///
/// `ZCCACHE_PCH_MTIME_REPLAY` is on by default; `0`, `false`, `off` or `no`
/// (any case) turn it off, and a cached PCH whose inputs carry a different
/// mtime is then rebuilt instead of served.
#[must_use]
pub fn pch_mtime_replay_enabled(value: Option<&str>) -> bool {
    !value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

/// The environment variable read by [`pch_mtime_replay_enabled`].
pub const PCH_MTIME_REPLAY_ENV: &str = "ZCCACHE_PCH_MTIME_REPLAY";

#[cfg(test)]
mod tests;
