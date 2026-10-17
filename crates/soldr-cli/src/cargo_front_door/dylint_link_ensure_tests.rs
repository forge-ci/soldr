//! soldr#3571: a *standalone* compiler pass in a configured lint crate must
//! get the managed `dylint-link` on the child cargo's PATH.
//!
//! The lint crate declares
//! `[target.'cfg(all())'] rustflags = ["-C", "linker=dylint-link"]` in its
//! own `.cargo/config.toml`, so Cargo hands every rustc that flag — but
//! before this fix nothing ever put `dylint-link` on PATH for a plain
//! `soldr cargo clippy`: the ensure lived behind `sub == "dylint"` inside
//! `append_subcommand_transitive_bin_dirs`, a function plain cargo verbs
//! never reach (they return early at the `lookup_by_cargo_subcommand`
//! miss). The compile died with `linker dylint-link not found` first.
//!
//! The fetch is exercised offline here by pre-seeding the soldr tool cache
//! at the exact layout `check_cache` reads
//! (`<bin>/dylint-link-<pin>/dylint-link`), so no network round trip is
//! involved and the assertion is purely "did the ensure fire".
//!
//! The fixtures are `#!/bin/sh` scripts (the smoke probe executes the
//! seeded binary), so each exec-touching test returns early on a Windows
//! host — a runtime `soldr-platform` fact check, never `#[cfg(unix)]`
//! (soldr#3284 / the #2493 platform-cfg boundary).

use super::*;
use crate::core::SoldrPaths;
use crate::EnvVarGuard;
use crate::TEST_PROCESS_ENV_LOCK as ENV_LOCK;
use std::path::{Path, PathBuf};

/// Whether this host can run the `#!/bin/sh` fixtures below.
fn shell_fixtures_supported() -> bool {
    crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows
}

fn argvec(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

fn pinned_dylint_link_version() -> &'static str {
    crate::fetch::known_tools::lookup_by_crate("dylint-link")
        .and_then(|spec| spec.pinned_version)
        .expect("dylint-link must have a registry pin")
}

/// Pre-seed `<paths.bin>/dylint-link-<pin>/dylint-link` with an executable
/// that answers the smoke probe, exactly where `check_cache` looks for an
/// already-downloaded prebuilt.
fn seed_cached_dylint_link(paths: &SoldrPaths) -> PathBuf {
    let dir = paths
        .bin
        .join(format!("dylint-link-{}", pinned_dylint_link_version()));
    std::fs::create_dir_all(&dir).expect("create seeded dylint-link dir");
    let binary = dir.join("dylint-link");
    std::fs::write(&binary, "#!/bin/sh\nexit 0\n").expect("write seeded dylint-link");
    crate::platform::fs::permissions::make_executable(&binary).expect("chmod seeded dylint-link");
    binary.parent().expect("bin dir").to_path_buf()
}

/// Write the soldr#3571 fixture: a project whose `.cargo/config.toml`
/// drives rustc with a bare `dylint-link`.
fn write_lint_crate_config(project: &Path) {
    std::fs::create_dir_all(project.join(".cargo")).expect("mkdir .cargo");
    std::fs::write(
        project.join(".cargo").join("config.toml"),
        "[target.'cfg(all())']\nrustflags = [\"-C\", \"linker=dylint-link\"]\n",
    )
    .expect("write .cargo/config.toml");
}

/// The RED→GREEN seam: on main, `soldr cargo clippy`'s argv
/// (`clippy --all-targets --locked`) hits the `lookup_by_cargo_subcommand`
/// early return and produces an empty bootstrap — no bin dir, so rustc's
/// PATH lookup for `dylint-link` fails. Post-fix the managed dir is there.
#[test]
fn configured_lint_crate_clippy_ensures_managed_dylint_link() {
    if !shell_fixtures_supported() {
        return;
    }
    let _lock = ENV_LOCK.lock();
    let _toolchain_env = EnvVarGuard::remove(crate::dylint_toolchain::TOOLCHAIN_ENV_VAR);
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _force_managed = EnvVarGuard::remove(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("project");
    write_lint_crate_config(&project);
    let paths = SoldrPaths::with_root(tmp.path().join("soldr"));
    let seeded = seed_cached_dylint_link(&paths);
    // A PATH with no `dylint-link` anywhere: the standalone-invocation
    // shape from the issue (no setup-soldr-exported tool dirs).
    let bare = tmp.path().join("bare-bin");
    std::fs::create_dir_all(&bare).expect("mkdir bare bin");
    let _path = EnvVarGuard::set("PATH", &bare);
    let _cargo_home = EnvVarGuard::set("CARGO_HOME", tmp.path().join("cargo-home"));
    let _cwd = crate::CwdGuard::enter(&project);

    let bootstrap = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(ensure_known_subcommand_tool(
            &argvec("clippy --all-targets --locked"),
            &paths,
        ))
        .expect("ensure_known_subcommand_tool");

    assert_eq!(
        bootstrap.bin_dirs,
        vec![seeded.clone()],
        "a configured lint crate's plain `cargo clippy` must prepend the \
         managed dylint-link bin dir (soldr#3571); got {:?}",
        bootstrap.bin_dirs
    );
    assert!(
        bootstrap.env.is_empty(),
        "the ensure adds a PATH dir, never env overrides: {:?}",
        bootstrap.env
    );
}

/// The ordinary-crate contract, seam level: with no `dylint-link` declared
/// anywhere (even with a perfectly seedable cache sitting right there),
/// `cargo clippy` gains nothing — no download, no PATH entry.
#[test]
fn ordinary_crate_clippy_needs_no_dylint_bootstrap() {
    if !shell_fixtures_supported() {
        return;
    }
    let _lock = ENV_LOCK.lock();
    let _toolchain_env = EnvVarGuard::remove(crate::dylint_toolchain::TOOLCHAIN_ENV_VAR);
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _force_managed = EnvVarGuard::remove(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(project.join(".cargo")).expect("mkdir .cargo");
    std::fs::write(
        project.join(".cargo").join("config.toml"),
        "[target.'cfg(all())']\nrustflags = [\"-C\", \"target-cpu=native\"]\n",
    )
    .expect("write unrelated .cargo/config.toml");
    let paths = SoldrPaths::with_root(tmp.path().join("soldr"));
    seed_cached_dylint_link(&paths);
    let bare = tmp.path().join("bare-bin");
    std::fs::create_dir_all(&bare).expect("mkdir bare bin");
    let _path = EnvVarGuard::set("PATH", &bare);
    let _cargo_home = EnvVarGuard::set("CARGO_HOME", tmp.path().join("cargo-home"));
    let _cwd = crate::CwdGuard::enter(&project);

    let bootstrap = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(ensure_known_subcommand_tool(
            &argvec("clippy --all-targets --locked"),
            &paths,
        ))
        .expect("ensure_known_subcommand_tool");

    assert!(
        bootstrap.bin_dirs.is_empty(),
        "an ordinary crate must never gain the dylint tools merely because \
         it invoked Clippy (soldr#3571); got {:?}",
        bootstrap.bin_dirs
    );
}

/// An absolute linker path is the caller's decision: rustc execs it
/// directly, so PATH is irrelevant and soldr must not fetch over it.
#[test]
fn absolute_linker_config_gains_no_dylint_ensure() {
    if !shell_fixtures_supported() {
        return;
    }
    let _lock = ENV_LOCK.lock();
    let _toolchain_env = EnvVarGuard::remove(crate::dylint_toolchain::TOOLCHAIN_ENV_VAR);
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _force_managed = EnvVarGuard::remove(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(project.join(".cargo")).expect("mkdir .cargo");
    std::fs::write(
        project.join(".cargo").join("config.toml"),
        "[target.'cfg(all())']\nlinker = \"/opt/dylint/bin/dylint-link\"\n",
    )
    .expect("write absolute-linker .cargo/config.toml");
    let paths = SoldrPaths::with_root(tmp.path().join("soldr"));
    seed_cached_dylint_link(&paths);
    let bare = tmp.path().join("bare-bin");
    std::fs::create_dir_all(&bare).expect("mkdir bare bin");
    let _path = EnvVarGuard::set("PATH", &bare);
    let _cargo_home = EnvVarGuard::set("CARGO_HOME", tmp.path().join("cargo-home"));
    let _cwd = crate::CwdGuard::enter(&project);

    let bootstrap = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(ensure_known_subcommand_tool(
            &argvec("clippy --all-targets --locked"),
            &paths,
        ))
        .expect("ensure_known_subcommand_tool");

    assert!(
        bootstrap.bin_dirs.is_empty(),
        "an absolute-path linker must not trigger the managed ensure: {:?}",
        bootstrap.bin_dirs
    );
}

/// A `dylint-link` already resolvable on PATH wins over the project
/// declaration (the issue #816 defer discipline): no managed fetch, no
/// second binary on PATH.
#[test]
fn path_provided_dylint_link_wins_and_fetches_nothing() {
    if !shell_fixtures_supported() {
        return;
    }
    let _lock = ENV_LOCK.lock();
    let _toolchain_env = EnvVarGuard::remove(crate::dylint_toolchain::TOOLCHAIN_ENV_VAR);
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _force_managed = EnvVarGuard::remove(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("project");
    write_lint_crate_config(&project);
    let paths = SoldrPaths::with_root(tmp.path().join("soldr"));
    seed_cached_dylint_link(&paths);
    let with_tool = tmp.path().join("path-with-dylint-link");
    std::fs::create_dir_all(&with_tool).expect("mkdir tool bin");
    let fake = with_tool.join("dylint-link");
    std::fs::write(&fake, "#!/bin/sh\nexit 0\n").expect("write fake dylint-link");
    crate::platform::fs::permissions::make_executable(&fake).expect("chmod fake dylint-link");
    let _path = EnvVarGuard::set("PATH", &with_tool);
    let _cargo_home = EnvVarGuard::set("CARGO_HOME", tmp.path().join("cargo-home"));
    let _cwd = crate::CwdGuard::enter(&project);

    let bootstrap = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(ensure_known_subcommand_tool(
            &argvec("clippy --all-targets --locked"),
            &paths,
        ))
        .expect("ensure_known_subcommand_tool");

    assert!(
        bootstrap.bin_dirs.is_empty(),
        "a PATH-provided dylint-link must be deferred to, not shadowed by a \
         managed fetch (issue #816); got {:?}",
        bootstrap.bin_dirs
    );
}

/// Parity guard for the original condition: `cargo dylint` itself still
/// ensures the managed `dylint-link` with no project config involved. The
/// `cargo-dylint` binary is put on PATH as a passing fake so this exercises
/// the deferred branch without any network hop.
#[test]
fn cargo_dylint_still_ensures_the_managed_dylint_link() {
    if !shell_fixtures_supported() {
        return;
    }
    let _lock = ENV_LOCK.lock();
    let _toolchain_env = EnvVarGuard::remove(crate::dylint_toolchain::TOOLCHAIN_ENV_VAR);
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _force_managed = EnvVarGuard::remove(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).expect("mkdir project");
    let paths = SoldrPaths::with_root(tmp.path().join("soldr"));
    let seeded = seed_cached_dylint_link(&paths);
    let with_cargo_dylint = tmp.path().join("path-with-cargo-dylint");
    std::fs::create_dir_all(&with_cargo_dylint).expect("mkdir tool bin");
    let fake = with_cargo_dylint.join("cargo-dylint");
    std::fs::write(&fake, "#!/bin/sh\nexit 0\n").expect("write fake cargo-dylint");
    crate::platform::fs::permissions::make_executable(&fake).expect("chmod fake cargo-dylint");
    let _path = EnvVarGuard::set("PATH", &with_cargo_dylint);
    let _cargo_home = EnvVarGuard::set("CARGO_HOME", tmp.path().join("cargo-home"));
    let _cwd = crate::CwdGuard::enter(&project);

    let bootstrap = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(ensure_known_subcommand_tool(
            &argvec("dylint --all"),
            &paths,
        ))
        .expect("ensure_known_subcommand_tool");

    assert!(
        bootstrap.bin_dirs.contains(&seeded),
        "`cargo dylint` must keep ensuring the managed dylint-link; got {:?}",
        bootstrap.bin_dirs
    );
}
