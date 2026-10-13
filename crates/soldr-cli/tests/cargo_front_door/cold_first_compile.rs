//! soldr#2424 P0: a real cold first compile on every front-door surface,
//! under the default (HOME-derived) root and an explicit `SOLDR_CACHE_DIR`.
//!
//! Before this module every real-compile test set an explicit cache root, so
//! the default root -- the one a user actually gets -- never had a cold
//! compile through the broker and daemon. Each case below starts from a
//! fresh HOME (or root) with no daemon running, compiles a dependency-free
//! crate with the real toolchain, and checks that the compiles were served
//! by the daemon: the front door's `cache N HIT, M MISS` summary reports at
//! least one miss. A fake compiler could not produce that line.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::common;

/// Where a cold run keeps its state.
enum Root {
    /// `SOLDR_CACHE_DIR` unset: the root derives from an isolated HOME.
    Default,
    /// An explicit, fresh `SOLDR_CACHE_DIR`.
    Explicit,
}

fn write_crate(dir: &Path) {
    fs::create_dir_all(dir.join("src")).expect("create src");
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"cold_first\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [profile.dev]\ndebug = false\n",
    )
    .expect("write Cargo.toml");
    fs::write(
        dir.join("src").join("lib.rs"),
        "pub fn answer() -> u32 { 42 }\n\n#[test]\nfn answers() { assert_eq!(answer(), 42); }\n",
    )
    .expect("write lib.rs");
}

/// The real rustup home, resolved before HOME is replaced, so an isolated
/// HOME still finds the installed toolchain.
fn real_rustup_home() -> Option<PathBuf> {
    std::env::var_os("RUSTUP_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| PathBuf::from(home).join(".rustup"))
        })
}

struct ColdRun {
    output: Output,
    home: PathBuf,
    cache_root: Option<PathBuf>,
}

fn cold_run(label: &str, root: Root, args: &[&str]) -> ColdRun {
    let home = common::unique_temp_dir(&format!("cold-home-{label}"));
    let project = common::unique_temp_dir(&format!("cold-crate-{label}"));
    write_crate(&project);
    let mut command = common::isolated_soldr_command();
    command
        .args(args)
        .current_dir(&project)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("CARGO_HOME", home.join("cargo-home"))
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("SOLDR_RUSTC_WRAPPER");
    if let Some(rustup_home) = real_rustup_home() {
        command.env("RUSTUP_HOME", rustup_home);
    }
    let cache_root = match root {
        Root::Default => {
            command.env_remove("SOLDR_CACHE_DIR");
            None
        }
        Root::Explicit => {
            let dir = common::unique_temp_dir(&format!("cold-root-{label}"));
            command.env("SOLDR_CACHE_DIR", &dir);
            Some(dir)
        }
    };
    let output = output_of(&mut command);
    ColdRun {
        output,
        home,
        cache_root,
    }
}

fn output_of(command: &mut Command) -> Output {
    command.output().expect("spawn soldr")
}

/// The run succeeded and its compiles went through the daemon.
fn assert_served_by_daemon(run: &ColdRun, surface: &str) {
    let stderr = String::from_utf8_lossy(&run.output.stderr);
    assert!(
        run.output.status.success(),
        "{surface}: cold first compile failed\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&run.output.stdout),
    );
    let summary = stderr
        .lines()
        .find(|line| line.contains("soldr: cache") && line.contains("MISS"))
        .unwrap_or_else(|| {
            panic!("{surface}: no daemon cache summary -- compiles bypassed the daemon?\n{stderr}")
        });
    let misses: u64 = summary
        .split_whitespace()
        .zip(summary.split_whitespace().skip(1))
        .find_map(|(count, word)| {
            word.trim_end_matches(',')
                .contains("MISS")
                .then(|| count.trim_start_matches("\u{1b}[33m").parse().ok())
                .flatten()
        })
        .unwrap_or(0);
    assert!(
        misses > 0,
        "{surface}: a cold build must miss at least once, got `{summary}`"
    );
}

/// Stop what the run started, so nothing outlives the test.
fn cleanup(run: &ColdRun) {
    let root = run
        .cache_root
        .clone()
        .unwrap_or_else(|| run.home.join(".soldr").join("cache"));
    common::stop_fixture_daemon_route(&root, &run.home);
    common::stop_fixture_broker(&root, &run.home);
}

#[test]
fn cargo_build_cold_on_the_default_root() {
    let run = cold_run("cargo-default", Root::Default, &["cargo", "build"]);
    cleanup(&run);
    assert_served_by_daemon(&run, "soldr cargo build (default root)");
    assert!(
        run.home.join(".soldr").is_dir(),
        "the default root must live under the isolated HOME"
    );
}

#[test]
fn cargo_build_cold_on_an_explicit_root() {
    let run = cold_run("cargo-explicit", Root::Explicit, &["cargo", "build"]);
    cleanup(&run);
    assert_served_by_daemon(&run, "soldr cargo build (explicit root)");
}

#[test]
fn bare_build_verb_cold_on_the_default_root() {
    let run = cold_run("bare-build", Root::Default, &["build"]);
    cleanup(&run);
    assert_served_by_daemon(&run, "soldr build (default root)");
}

#[test]
fn bare_test_verb_cold_on_an_explicit_root() {
    let run = cold_run("bare-test", Root::Explicit, &["test"]);
    cleanup(&run);
    assert_served_by_daemon(&run, "soldr test (explicit root)");
}
