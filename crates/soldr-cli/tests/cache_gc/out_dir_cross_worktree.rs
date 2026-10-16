//! Registry crates whose build script feeds `include!(concat!(env!("OUT_DIR"), ..))`
//! must be shared across worktrees (zccache#1955).
//!
//! rustc records `OUT_DIR` as an env dependency with its absolute value, and
//! that value names the worktree's own `target/`. zccache used to fold it into
//! the artifact key verbatim, so every fresh worktree recompiled every such
//! crate -- serde, serde_core, thiserror, prost-generated protocols -- while
//! `ZCCACHE_PATH_REMAP=auto` shared everything else. Only a hand-written
//! allow-list of exact crate versions escaped, and it went stale on the next
//! patch release.
//!
//! The fixture is the smallest shape that reproduces it: two git worktrees,
//! each with its own in-tree `target/`, depending on one vendored registry
//! crate that `include!`s a build-script file. A second vendored crate that
//! embeds `env!("OUT_DIR")` as a string is the correctness control: its output
//! really does differ per worktree, so it must keep missing.

use crate::agent_worktree_share::{git, FixtureGuard};
use crate::common;

use crate::common::unique_temp_dir;
use std::fs;
use std::path::Path;

const CHECKSUM: &str =
    r#"{"files":{},"package":"0000000000000000000000000000000000000000000000000000000000000000"}"#;

#[test]
fn include_only_out_dir_crate_hits_in_a_fresh_worktree() {
    let workdir = unique_temp_dir("out-dir-cross-worktree");
    let cache_dir = workdir.join("shared-cache");
    // soldr#3823: an isolated home, so the builds use this fixture's own
    // broker. On a CI runner the default home already holds a broker from an
    // earlier step (a different Soldr image); routed through it, the compile
    // journal this test tails for HIT/MISS lines is never written.
    let home = workdir.join("home");
    fs::create_dir_all(&home).expect("create isolated home");
    let _broker = common::BrokerHomeGuard::new(&cache_dir, &home);
    let guard = FixtureGuard::new(workdir.clone(), cache_dir.clone()).with_home(home.clone());
    let vendor = workdir.join("vendor");
    write_vendored_crates(&vendor);
    let repo = workdir.join("repo");
    write_app(&repo, &vendor);

    git(&["init", "-q"], &repo);
    git(&["add", "."], &repo);
    git(
        &[
            "-c",
            "user.email=test@soldr.invalid",
            "-c",
            "user.name=test",
            "commit",
            "-q",
            "-m",
            "initial",
        ],
        &repo,
    );
    let first = workdir.join("wt-first");
    let second = workdir.join("wt-second");
    for worktree in [&first, &second] {
        git(
            &[
                "worktree",
                "add",
                "-q",
                worktree.to_str().expect("utf-8 worktree path"),
                "HEAD",
            ],
            &repo,
        );
    }

    let cold = soldr_cargo_check(&first, &cache_dir, &home);
    assert_eq!(
        crate_outcome(&cold, "gen_dep"),
        Some(Outcome::Miss),
        "{cold}"
    );

    // The fresh worktree differs from the first only in where it lives.
    let warm = soldr_cargo_check(&second, &cache_dir, &home);
    assert_eq!(
        crate_outcome(&warm, "gen_dep"),
        Some(Outcome::Hit),
        "an include-only OUT_DIR crate must be shared across worktrees: {warm}"
    );
    assert_eq!(
        crate_outcome(&warm, "embed_dep"),
        Some(Outcome::Miss),
        "a crate that embeds its OUT_DIR path must never be served across worktrees: {warm}"
    );

    // A hit must hand Cargo dep-info for *this* worktree's OUT_DIR; otherwise
    // Cargo sees a changed env dependency and recompiles on every build.
    let rerun = soldr_cargo_check(&second, &cache_dir, &home);
    assert_eq!(
        crate_outcome(&rerun, "gen_dep"),
        None,
        "cargo must consider the replayed crate fresh: {rerun}"
    );
    guard.stop_and_assert_exited();
}

fn write_vendored_crates(vendor: &Path) {
    write_vendored(
        vendor,
        "gen-dep",
        r#"include!(concat!(env!("OUT_DIR"), "/generated.rs"));
pub fn value() -> u32 {
    GENERATED
}
"#,
    );
    write_vendored(
        vendor,
        "embed-dep",
        r#"pub const OUT_DIR: &str = env!("OUT_DIR");
"#,
    );
}

fn write_vendored(vendor: &Path, name: &str, lib: &str) {
    let dir = vendor.join(name);
    fs::create_dir_all(dir.join("src")).expect("create vendored src");
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n"
        ),
    )
    .expect("write vendored manifest");
    fs::write(
        dir.join("build.rs"),
        r#"fn main() {
    let out = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    std::fs::write(
        std::path::Path::new(&out).join("generated.rs"),
        "pub const GENERATED: u32 = 7;\n",
    )
    .expect("write generated.rs");
    println!("cargo:rerun-if-changed=build.rs");
}
"#,
    )
    .expect("write vendored build.rs");
    fs::write(dir.join("src").join("lib.rs"), lib).expect("write vendored lib.rs");
    fs::write(dir.join(".cargo-checksum.json"), CHECKSUM).expect("write checksum");
}

/// The app resolves both crates through a crates-io source replacement, so
/// Cargo treats them exactly like registry packages: `cwd` is the vendored
/// package and `-C metadata` is identical in every worktree.
fn write_app(repo: &Path, vendor: &Path) {
    fs::create_dir_all(repo.join("src")).expect("create app src");
    fs::create_dir_all(repo.join(".cargo")).expect("create .cargo");
    fs::write(
        repo.join("Cargo.toml"),
        r#"[package]
name = "out_dir_app"
version = "0.1.0"
edition = "2021"

[dependencies]
gen-dep = "0.1.0"
embed-dep = "0.1.0"
"#,
    )
    .expect("write app manifest");
    fs::write(
        repo.join("src").join("main.rs"),
        "fn main() {\n    println!(\"{} {}\", gen_dep::value(), embed_dep::OUT_DIR);\n}\n",
    )
    .expect("write app main.rs");
    fs::write(
        repo.join(".cargo").join("config.toml"),
        format!(
            "[source.crates-io]\nreplace-with = \"fixture\"\n\n[source.fixture]\ndirectory = \"{}\"\n",
            common::toml_string(vendor)
        ),
    )
    .expect("write cargo config");
}

/// Runs `soldr cargo check` with the target dir left inside the worktree, the
/// layout every real checkout uses, and returns soldr's combined output.
fn soldr_cargo_check(worktree: &Path, cache_dir: &Path, home: &Path) -> String {
    let mut command = common::isolated_soldr_command();
    command
        .args(["cargo", "check", "--offline"])
        .current_dir(worktree)
        .env("SOLDR_CACHE_DIR", cache_dir)
        .env("HOME", home)
        .env("USERPROFILE", home)
        // Keep the caller's toolchain; the isolated home has no rustup state.
        .env("RUSTUP_HOME", caller_rustup_home())
        .env("CARGO_HOME", home.join("cargo"))
        .env("SOLDR_ALLOW_UNPINNED", "1")
        .env_remove("CARGO_TARGET_DIR");
    let started = std::time::Instant::now();
    let output = command.output().expect("spawn soldr cargo check");
    eprintln!(
        "out_dir_cross_worktree: soldr cargo check in {} took {:.1}s",
        worktree.display(),
        started.elapsed().as_secs_f64()
    );
    let rendered = format!(
        "stdout={}; stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.status.success(),
        "soldr cargo check failed in {}: {rendered}",
        worktree.display(),
    );
    rendered
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Hit,
    Miss,
}

/// The outcome soldr reported for `crate_name`'s library compile, from its
/// `soldr[cache] <crate> [HIT!]` / `[MISS]` lines; `None` when it never ran.
fn crate_outcome(output: &str, crate_name: &str) -> Option<Outcome> {
    let tag = format!("soldr[cache] {crate_name} [");
    // soldr colours the `[HIT]`/`[MISS]` tag when it decides colour is on (it
    // does on GitHub Actions): `gen_dep \x1b[33m[MISS]`. Match on the text.
    let output = strip_ansi(output);
    output.lines().find_map(|line| {
        let rest = &line[line.find(&tag)? + tag.len()..];
        if rest.starts_with("HIT") {
            Some(Outcome::Hit)
        } else if rest.starts_with("MISS") {
            Some(Outcome::Miss)
        } else {
            None
        }
    })
}

/// Drop CSI escape sequences (`ESC [ ... <final byte>`), e.g. SGR colour codes.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The rustup home the test process itself uses, so a soldr child with an
/// isolated `HOME` still finds the pinned toolchain.
fn caller_rustup_home() -> std::path::PathBuf {
    if let Some(value) = std::env::var_os("RUSTUP_HOME").filter(|value| !value.is_empty()) {
        return std::path::PathBuf::from(value);
    }
    let real_home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .expect("test process has a home directory");
    std::path::PathBuf::from(real_home).join(".rustup")
}

#[test]
fn crate_outcome_reads_colorized_and_plain_tags() {
    let colored = "0.61 soldr[cache] gen_dep \u{1b}[33m[MISS]\u{1b}[0m";
    assert_eq!(crate_outcome(colored, "gen_dep"), Some(Outcome::Miss));
    let hit = "soldr[cache] gen_dep \u{1b}[32m[HIT!]\u{1b}[0m";
    assert_eq!(crate_outcome(hit, "gen_dep"), Some(Outcome::Hit));
    assert_eq!(
        crate_outcome("soldr[cache] gen_dep [MISS]", "gen_dep"),
        Some(Outcome::Miss)
    );
    assert_eq!(crate_outcome(colored, "embed_dep"), None);
}
