//! Off-process symbolization for native captures.
//!
//! # Why this is a separate process
//!
//! Symbol files are attacker-adjacent input: a PDB, DWARF section, or minidump
//! can be malformed in ways that crash a parser outright rather than returning
//! an error. Isolation here is therefore a **process** boundary, not a
//! `catch_unwind` — a parser that segfaults takes down only this PID, and the
//! daemon observes the exit status and carries on.
//!
//! Isolation is link-time as well as run-time. This library contains only the
//! stable wire schema and the worker client. Parser modules are compiled only
//! into the `kernal-symbolize` executable behind the `symbolize-worker`
//! feature; dependent applications neither compile nor link them.
//!
//! # Never fabricate a symbol
//!
//! Every degradation path preserves the module and offset and reports a
//! [`wire::FrameStatus`] saying how far resolution got. A wrong function name
//! is worse than no name: it sends whoever reads the report looking in the
//! wrong place, and nothing in the output would contradict them.

#![deny(missing_docs)]

mod client;
// Post-link symbol carving is a build-time operation, not a run-time one, and
// it reads object files -- so it stays behind its own feature rather than
// riding along with the wire schema every consumer already compiles.
#[cfg(all(
    feature = "symbolize-split",
    any(target_os = "linux", target_os = "macos", windows)
))]
pub mod split;
pub mod wire;

pub use client::{default_worker_path, SymbolizerWorker, WorkerError, SYMBOLIZER_WORKER_ENV};
#[cfg(all(
    feature = "symbolize-split",
    any(target_os = "linux", target_os = "macos", windows)
))]
pub use split::{
    split_debug_symbols, DebugSplit, DebugSplitError, DebugSplitRequest, SplitMechanism,
    VerifiedResolution,
};
pub use wire::{RawCapture, SymbolReport};
