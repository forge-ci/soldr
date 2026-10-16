//! soldr#3276 §4/acceptance: the front door, `soldr build`, the PEP 517
//! path, and `soldr prepare` / `soldr toolchain prepare|ensure` must all
//! resolve the project linker choice through ONE shared function --
//! `linker::resolve_project_choice` (and its process-ambient wrapper
//! `resolve_project_choice_from_cwd`) -- never a hand-rolled duplicate of
//! the env > cargo-config > Cargo.toml-metadata > user-config > default
//! precedence chain.
//!
//! This is a static source lint, not a behavioral test: the precedence
//! chain itself (each declaration source, and that they agree) is unit-
//! tested directly against `resolve_project_choice` in
//! `crates/soldr-cli/src/linker_tests.rs`. What this guard proves is that
//! every one of the surfaces named in the issue actually calls that same
//! function rather than drifting into its own copy (the soldr#2945 lesson
//! this repo has already paid for once with the Dylint nightly).

use std::fs;
use std::path::Path;

use crate::common;

/// `resolve_project_choice` itself may be defined exactly once.
const RESOLVER_DEFINITION: &str = "pub fn resolve_project_choice(";

/// Every consuming surface must reach the resolver only through this
/// process-ambient wrapper (or the pure function directly, for prepare's
/// restore-report path, which also imports it -- both are covered below by
/// searching for either symbol).
const RESOLVER_CALL_MARKERS: &[&str] = &[
    "resolve_project_choice_from_cwd(",
    "resolve_project_choice(",
];

/// Files that must each call the shared resolver at least once: the cargo
/// front door's linker injection, the PEP 517 fallback path, and `soldr
/// prepare`'s reld fetch + restore report. `soldr toolchain prepare`/`ensure`
/// no longer resolves the choice itself: since soldr#3802 it provisions through
/// `cargo_front_door::provision`, which calls the front door's own
/// `target.rs` linker override, so it reaches the one resolver by delegation.
const REQUIRED_CALLER_FILES: &[&str] = &[
    "crates/soldr-cli/src/cargo_front_door/target.rs",
    "crates/soldr-cli/src/linker.rs",
    "crates/soldr-cli/src/prepare_cmd.rs",
];

fn repo_root() -> std::path::PathBuf {
    common::workspace_root()
}

#[test]
fn resolve_project_choice_is_defined_exactly_once() {
    let root = repo_root();
    let mut definitions = Vec::new();
    for entry in walk_rs_files(&root.join("crates/soldr-cli/src")) {
        let contents = fs::read_to_string(&entry).unwrap_or_default();
        if contents.contains(RESOLVER_DEFINITION) {
            definitions.push(entry);
        }
    }
    assert_eq!(
        definitions.len(),
        1,
        "expected exactly one `resolve_project_choice` definition, found: {definitions:?}"
    );
}

#[test]
fn every_linker_consuming_surface_calls_the_shared_resolver() {
    let root = repo_root();
    for relative in REQUIRED_CALLER_FILES {
        let path = root.join(relative);
        let contents =
            fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {relative}: {error}"));
        let calls_resolver = RESOLVER_CALL_MARKERS
            .iter()
            .any(|marker| contents.contains(marker));
        assert!(
            calls_resolver,
            "{relative} must resolve the project linker choice through \
             linker::resolve_project_choice[_from_cwd], not a duplicate"
        );
    }
}

fn walk_rs_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk_rs_files(&path));
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            files.push(path);
        }
    }
    files
}
