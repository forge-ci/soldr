//! Input-keyed stage-skip marker for the `dylint-libraries` domain (soldr#2349).
//!
//! `soldr ci-test` unconditionally runs six serial `dylint-library-<lint>`
//! stages that each rebuild one lint-library cdylib, even on a warm tree
//! where nothing that affects the built libraries has changed. This module
//! is the same discipline `dylint_cook.rs` already uses for the dependency
//! layer (semantic input hash -> cache key -> marker file next to the
//! target tree, atomic replace, hit requires marker equality *and* real
//! payload on disk) applied to the library-build target tree instead.
//!
//! # What is hashed
//!
//! Everything under `dylints/**` that can change a built cdylib: each
//! lint's `src/**` sources, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`,
//! and `.cargo/config.toml` (the file that carries the
//! `-C linker=dylint-link` rustflag). `dylints/*/ui/**` fixtures are
//! deliberately **excluded** — they are compiled by the separate
//! `dylint-test-*` stages, not by the library build this marker gates, and
//! [`visit_semantic_files`] never descends into them (it only walks each
//! lint's `src/` directory, plus a fixed set of top-level manifest files).
//! The nightly compiler identity (channel + release + commit), the
//! [`relevant_environment`] slice (`RUSTFLAGS` / `CARGO_ENCODED_RUSTFLAGS` /
//! `CARGO_BUILD_TARGET` / `SOLDR_RUSTC_WRAPPER` / `CARGO_PROFILE_*` /
//! `CARGO_TARGET_*` -- the exact predicate `dylint_cook.rs::semantic_input_hash`
//! applies to its own marker, kept identical on purpose since both build
//! `--profile release` through the same soldr wrapper), and, best effort,
//! the resolved Dylint driver's identity round out the key.
//!
//! # The skip is OPT-IN (soldr#3038 review)
//!
//! `SOLDR_DYLINT_LIBRARY_SKIP=on` enables the stage skip; **absent or any
//! unrecognised value leaves it OFF**, which is the ordinary
//! soldr-owned-switch-that-defaults-off rule ([`crate::core::flag_value`],
//! soldr#2740).
//!
//! It ships off deliberately. Three consecutive warm CI runs of the Linux
//! x64 host lane failed with the skip on while the cold run passed, each a
//! different shape (a nested-cargo fixture test timing out, a compiler
//! SIGTERM'd at a 14.02 GiB cgroup peak with `oom_kill=0`, and an `ETXTBSY`
//! exec race in the shared `target/dylint/tests` tree). The mechanism was
//! proven every time -- the marker hit and the six stages were skipped --
//! but removing ~5 minutes of serialized library builds moved the Dylint
//! UI-test branch, whose dependency compiles are heavy and always cold,
//! into Fresh Nextest's resident-memory peak. The skip did not create that
//! hole; it removed the accidental staggering that was hiding it.
//!
//! # It stays opt-in: the five minutes it saved are gone (soldr#3132)
//!
//! soldr#3042 (Phase 3 of soldr#3039) has since landed the cook of the
//! `target/dylint/tests` dependency layer, and the flip was meant to follow a
//! warm host-lane run. Measuring first showed there is nothing left to buy.
//! On the three most recent green `main` gates the six `dylint-library-*`
//! stages took **7.1 s, 7.7 s and 11.4 s in total** (runs 34727854309,
//! 34730203487, 34732041931), and the 11.4 s run had missed the
//! `dylint-foundation` cache. The serialized minutes the skip was built to
//! remove are now paid by the per-unit zccache store and the cook instead.
//!
//! Turning the skip on by default would save at most about twelve seconds
//! while re-opening the exposure the three failed warm runs above showed:
//! finishing the library stages early pulls the Dylint UI-test branch forward
//! into Fresh Nextest's resident-memory peak. That is a bad trade, so the
//! default stays off. Revisit only if the library stages regress back to
//! minutes; the marker is still recorded on every run, so an opt-in run hits
//! immediately.

use crate::core::{SoldrError, SoldrPaths};
use crate::dylint_toolchain::DylintToolchainPlan;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 1;
const MARKER_NAME: &str = ".soldr-dylint-library-marker-v1.json";
const WRAPPER_IDENTITY: &str = "soldr-ci-test-dylint-library-marker-v1";

/// `SOLDR_DYLINT_LIBRARY_SKIP=on` enables the stage skip. Absent, or any
/// value that is not a recognised soldr-owned "on" spelling, keeps the six
/// `dylint-library-*` stages running -- the default-off shape of
/// [`crate::core::flag_value`] (soldr#2740). See the module docs for why
/// this stays off (soldr#3132).
pub(crate) const SKIP_ENV_VAR: &str = "SOLDR_DYLINT_LIBRARY_SKIP";

/// Is the opt-in skip switch on? Pure, taking the raw value rather than
/// reading the process environment, so the default can be unit-tested
/// without `std::env::set_var` (which is `unsafe` and racy across the
/// parallel test threads in this binary).
pub(crate) fn skip_enabled(value: Option<&str>) -> bool {
    value.is_some_and(crate::core::flag_value)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DylintLibraryMarker {
    schema_version: u32,
    cache_key: String,
    compiler_commit: String,
    target_directory: String,
}

/// Everything [`evaluate`] needs, gathered once by the executor
/// (`execute.rs`) so a second call never re-probes the Dylint driver.
#[derive(Debug, Clone)]
pub(crate) struct LibraryMarkerInputs {
    pub(crate) root: PathBuf,
    pub(crate) target_directory: PathBuf,
    pub(crate) toolchain: DylintToolchainPlan,
    pub(crate) driver_identity: Option<String>,
}

impl LibraryMarkerInputs {
    pub(crate) fn for_plan(
        plan: &super::model::CiTestPlan,
        toolchain: &DylintToolchainPlan,
    ) -> Self {
        Self {
            root: PathBuf::from(&plan.workspace_root),
            target_directory: PathBuf::from(&plan.dylint_target_trees.libraries),
            toolchain: toolchain.clone(),
            driver_identity: driver_identity(toolchain),
        }
    }
}

/// The result of comparing the current inputs against the marker on disk.
/// Carries the freshly computed marker so [`record`] writes exactly what was
/// checked, rather than recomputing after the six library stages have run
/// (which could silently fold in an edit made mid-run).
pub(crate) struct LibraryMarkerDecision {
    inputs: LibraryMarkerInputs,
    marker: DylintLibraryMarker,
    pub(crate) skip: bool,
}

/// Reads the opt-in switch from the process environment and delegates to
/// [`evaluate_with_skip_enabled`]. The marker is computed either way, so a
/// run with the skip off still *records* an accurate marker and a later
/// opt-in run can hit it.
pub(crate) fn evaluate(inputs: LibraryMarkerInputs) -> Result<LibraryMarkerDecision, SoldrError> {
    let raw = std::env::var(SKIP_ENV_VAR).ok();
    evaluate_with_skip_enabled(inputs, skip_enabled(raw.as_deref()))
}

/// A skip requires ALL THREE of: the opt-in switch on
/// (`SOLDR_DYLINT_LIBRARY_SKIP=on`, default off), marker equality (inputs
/// unchanged), and the libraries target tree actually holding a built
/// `release/` payload -- a marker with no payload (a fresh checkout, or a
/// tree wiped by GC between runs) must never read as a hit.
pub(crate) fn evaluate_with_skip_enabled(
    inputs: LibraryMarkerInputs,
    skip_enabled: bool,
) -> Result<LibraryMarkerDecision, SoldrError> {
    let marker = compute_marker(&inputs)?;
    let skip = skip_enabled
        && read_marker(&marker_path(&inputs)).as_ref() == Some(&marker)
        && target_has_library_payload(&inputs.target_directory);
    if skip {
        eprintln!(
            "soldr ci-test: Dylint library marker hit; skipping the six `dylint-library-*` build stages ({SKIP_ENV_VAR}=on is set; unset it to rebuild)"
        );
    }
    Ok(LibraryMarkerDecision {
        inputs,
        marker,
        skip,
    })
}

/// Write the marker after the six library stages have completed
/// successfully on a miss. Writes the same marker [`evaluate`] computed
/// before those stages ran, never a fresh recomputation.
pub(crate) fn record(decision: &LibraryMarkerDecision) -> Result<(), SoldrError> {
    let path = marker_path(&decision.inputs);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_marker(&path, &decision.marker)
}

/// Executor entry point (soldr#2349 review finding 3): builds the marker
/// inputs from the frozen plan and resolved toolchain and evaluates them,
/// so `execute.rs`'s call site is one line instead of constructing
/// [`LibraryMarkerInputs`] inline.
pub(crate) fn decide(
    plan: &super::model::CiTestPlan,
    toolchain: &DylintToolchainPlan,
) -> Result<LibraryMarkerDecision, SoldrError> {
    evaluate(LibraryMarkerInputs::for_plan(plan, toolchain))
}

/// Executor entry point for the post-stage half of the marker lifecycle
/// (soldr#2349 review finding 4): writes the marker on a miss, does nothing
/// on a hit.
///
/// Deliberately does not propagate a write error as a run failure.
/// `dylint_cook.rs::record`'s caller does propagate (`?`), because there the
/// marker write *is* the command's entire product; here it is a cache
/// side-effect of a validation run whose six library stages have already
/// succeeded by the time this runs. A missing marker only costs a rebuild
/// on the *next* `ci-test` invocation -- never a correctness risk -- so
/// failing an otherwise-green run over cache bookkeeping is the wrong
/// trade. A write failure is logged as a warning and the run's exit code is
/// left alone.
pub(crate) fn finish(decision: &LibraryMarkerDecision) {
    if decision.skip {
        return;
    }
    if let Err(error) = record(decision) {
        eprintln!(
            "warning: soldr ci-test: failed to write Dylint library marker (next run will rebuild the six `dylint-library-*` stages instead of skipping): {error}"
        );
    }
}

/// Executor entry point: the skip-branch banner printed when
/// `DylintBranch::compilation_from_plan` (execute.rs) fast-forwards past the
/// six `dylint-library-*` stages on a marker hit. Lives beside the module's
/// other marker-hit messaging rather than inline in the executor.
pub(crate) fn announce_skip(library_stage_names: &[&str]) {
    eprintln!(
        "soldr ci-test: skipping stages [{}] (Dylint library marker hit)",
        library_stage_names.join(", ")
    );
}

fn marker_path(inputs: &LibraryMarkerInputs) -> PathBuf {
    inputs.target_directory.join(MARKER_NAME)
}

fn compute_marker(inputs: &LibraryMarkerInputs) -> Result<DylintLibraryMarker, SoldrError> {
    let mut digest = Sha256::new();
    digest.update(b"soldr-ci-test-dylint-library-marker-v1\0");
    hash_field(&mut digest, inputs.toolchain.channel.as_bytes());
    hash_field(&mut digest, inputs.toolchain.compiler_release.as_bytes());
    hash_field(&mut digest, inputs.toolchain.compiler_commit.as_bytes());
    hash_field(
        &mut digest,
        inputs
            .driver_identity
            .as_deref()
            .unwrap_or("<dylint driver unreachable>")
            .as_bytes(),
    );
    hash_field(&mut digest, semantic_input_hash(&inputs.root)?.as_bytes());
    hash_field(
        &mut digest,
        environment_input_hash(std::env::vars())?.as_bytes(),
    );
    hash_field(&mut digest, WRAPPER_IDENTITY.as_bytes());
    let cache_key = hex::encode(digest.finalize());
    Ok(DylintLibraryMarker {
        schema_version: SCHEMA_VERSION,
        cache_key,
        compiler_commit: inputs.toolchain.compiler_commit.clone(),
        target_directory: inputs.target_directory.display().to_string(),
    })
}

fn hash_field(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
}

/// Best-effort: the driver is expected to be reachable by the time this runs
/// (`prepare_dylint` already ensured it), but a missing/unresolvable driver
/// must never fail marker evaluation -- it only means the marker folds in a
/// fixed placeholder instead of the driver's identity, so a subsequent run
/// with a real driver misses and rebuilds rather than silently trusting a
/// stale tree.
///
/// Keyed on content, not mtime: the CI workflow restores the driver from an
/// `actions/cache` tar and may re-fetch it, which changes mtime while the
/// binary is byte-identical. A content hash costs one SHA-256 pass over a
/// single binary -- trivial next to six cdylib builds -- and keeps this
/// library marker decoupled from the driver cache's storage mechanics.
fn driver_identity(toolchain: &DylintToolchainPlan) -> Option<String> {
    let paths = SoldrPaths::new().ok()?;
    let driver_path = crate::dylint_driver::require_prebuilt_driver(toolchain, &paths).ok()?;
    hash_driver_file(&driver_path)
}

/// Extracted from [`driver_identity`] for tests: path + length + content
/// hash, deliberately no mtime, so identity is stable across a re-fetch
/// that reproduces the same bytes and changes only when the bytes do.
fn hash_driver_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let mut digest = Sha256::new();
    digest.update(&bytes);
    Some(format!(
        "{}|{}|{}",
        path.display(),
        bytes.len(),
        hex::encode(digest.finalize())
    ))
}

/// The built-library payload lives under `<target_directory>/release/`
/// (`--profile release` in the frozen plan's library stage command). Only the
/// marker file itself is excluded from counting as payload.
fn target_has_library_payload(target_directory: &Path) -> bool {
    fn contains_file(directory: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && contains_file(&path) {
                return true;
            }
            if path.is_file()
                && !matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some(MARKER_NAME)
                )
            {
                return true;
            }
        }
        false
    }
    contains_file(&target_directory.join("release"))
}

/// Same key predicate as `dylint_cook.rs::semantic_input_hash`
/// (dylint_cook.rs:617-627), kept identical on purpose -- both markers gate
/// the same eight lint cdylibs built with `--profile release` through the
/// soldr wrapper, so a changed `RUSTFLAGS` (etc.) changes what gets produced
/// even when no tracked file changed. Takes the pairs as a parameter rather
/// than reading `std::env::vars()` internally so tests can exercise it with
/// a synthetic environment instead of mutating process env.
fn relevant_environment(vars: impl Iterator<Item = (String, String)>) -> BTreeMap<String, String> {
    let mut environment = BTreeMap::new();
    for (key, value) in vars {
        if matches!(
            key.as_str(),
            "RUSTFLAGS" | "CARGO_ENCODED_RUSTFLAGS" | "CARGO_BUILD_TARGET" | "SOLDR_RUSTC_WRAPPER"
        ) || key.starts_with("CARGO_PROFILE_")
            || key.starts_with("CARGO_TARGET_")
        {
            environment.insert(key, value);
        }
    }
    environment
}

fn environment_input_hash(
    vars: impl Iterator<Item = (String, String)>,
) -> Result<String, SoldrError> {
    let environment = relevant_environment(vars);
    let mut digest = Sha256::new();
    hash_field(
        &mut digest,
        &serde_json::to_vec(&environment).map_err(|error| {
            SoldrError::Other(format!(
                "soldr ci-test: Dylint library marker environment encoding failed: {error}"
            ))
        })?,
    );
    Ok(hex::encode(digest.finalize()))
}

fn semantic_input_hash(root: &Path) -> Result<String, SoldrError> {
    let mut entries = BTreeMap::<String, Vec<u8>>::new();
    visit_semantic_files(root, &mut |path, bytes| {
        if let Ok(relative) = path.strip_prefix(root) {
            entries.insert(normalize_path(relative), bytes.to_vec());
        }
    })?;
    let mut digest = Sha256::new();
    for (path, bytes) in entries {
        hash_field(&mut digest, path.as_bytes());
        hash_field(&mut digest, &bytes);
    }
    Ok(hex::encode(digest.finalize()))
}

/// Walks `dylints/<lint>/**` for every declared lint, deterministically
/// (lint directories are sorted before iteration; the final ordering is
/// owned by the `BTreeMap` in [`semantic_input_hash`] regardless). Only
/// `src/**`, plus a fixed set of per-lint top-level files, are visited:
///
/// * `src/**` -- the compiled cdylib sources.
/// * `Cargo.toml`, `Cargo.lock` -- dependency/version identity. `Cargo.lock`
///   in particular is a known blind spot in the current `actions/cache` key.
/// * `rust-toolchain.toml` -- the pinned nightly for this one lint.
/// * `.cargo/config.toml` (and the extensionless `.cargo/config`) -- carries
///   `-C linker=dylint-link`; also missing from the current cache key.
///
/// `dylints/*/ui/**` is intentionally never visited: those are UI-test
/// fixtures consumed by the separate `dylint-test-*` stages and do not affect
/// the built library, so a change there must not invalidate this marker.
/// `target/` under each lint (local build output) is likewise never visited
/// because nothing here descends into it.
fn visit_semantic_files(
    root: &Path,
    callback: &mut dyn FnMut(&Path, &[u8]),
) -> Result<(), SoldrError> {
    let dylints_dir = root.join("dylints");
    let mut lint_dirs: Vec<PathBuf> = match std::fs::read_dir(&dylints_dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(SoldrError::Other(format!(
                "soldr ci-test: cannot read {}: {error}",
                dylints_dir.display()
            )))
        }
    };
    lint_dirs.sort();
    for lint_dir in lint_dirs {
        for name in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"] {
            include_file(&lint_dir.join(name), callback)?;
        }
        for name in ["config.toml", "config"] {
            include_file(&lint_dir.join(".cargo").join(name), callback)?;
        }
        let src_dir = lint_dir.join("src");
        if src_dir.is_dir() {
            visit_directory_recursive(&src_dir, callback)?;
        }
    }
    Ok(())
}

fn include_file(path: &Path, callback: &mut dyn FnMut(&Path, &[u8])) -> Result<(), SoldrError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            callback(path, &bytes);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(SoldrError::Other(format!(
            "soldr ci-test: cannot read {}: {error}",
            path.display()
        ))),
    }
}

fn visit_directory_recursive(
    directory: &Path,
    callback: &mut dyn FnMut(&Path, &[u8]),
) -> Result<(), SoldrError> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            visit_directory_recursive(&path, callback)?;
        } else if kind.is_file() {
            let bytes = std::fs::read(&path)?;
            callback(&path, &bytes);
        }
    }
    Ok(())
}

fn normalize_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn read_marker(path: &Path) -> Option<DylintLibraryMarker> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_marker(path: &Path, marker: &DylintLibraryMarker) -> Result<(), SoldrError> {
    let bytes = serde_json::to_vec(marker).map_err(|error| {
        SoldrError::Other(format!(
            "soldr ci-test: Dylint library marker encoding failed: {error}"
        ))
    })?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    replace_marker_file(&temporary, path, |from, to| std::fs::rename(from, to))?;
    Ok(())
}

/// Same remove-then-rename fallback as `dylint_cook.rs::replace_marker_file`:
/// Windows `rename` does not replace an existing destination.
fn replace_marker_file(
    temporary: &Path,
    path: &Path,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if let Err(error) = rename(temporary, path) {
        if path.exists() {
            std::fs::remove_file(path)?;
            rename(temporary, path)?;
        } else {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "dylint_library_marker_tests.rs"]
mod tests;
