//! soldr#3571: a *standalone* `soldr cargo clippy` in a configured lint
//! crate must put the managed `dylint-link` on the child cargo's PATH.
//!
//! End-to-end shape of the issue: the lint crate's own `.cargo/config.toml`
//! declares `[target.'cfg(all())'] rustflags = ["-C", "linker=dylint-link"]`,
//! the caller has no setup-soldr-exported tool dirs on PATH, and pre-fix the
//! front door ensured `dylint-link` only for the literal `cargo dylint`
//! subcommand — a plain cargo verb returned early before any ensure, so
//! rustc died with `linker dylint-link not found`.
//!
//! The run is offline: the soldr tool cache is pre-seeded at the exact
//! layout `check_cache` reads (`<SOLDR_CACHE_DIR>/bin/dylint-link-<pin>/`),
//! so the assertion is purely "did the ensure reach the child PATH". The
//! two phases mirror `cli_cargo_linker.rs`'s control pattern: phase 1
//! (ordinary crate, no declaration) proves the seed alone never leaks onto
//! PATH; phase 2 (configured lint crate) is the RED→GREEN assertion.
//!
//! Windows returns early: the seeded fixture is a `#!/bin/sh` script and
//! `check_cache` wants a `.exe` the smoke probe can actually exec — a
//! runtime `soldr-platform` fact check, never `#[cfg(unix)]` (soldr#3284).

#![allow(unused_imports)]

use crate::common;
use crate::common::*;
use std::fs;
use std::path::{Path, PathBuf};

/// Whether this host can run the `#!/bin/sh` fixtures in this file.
fn shell_fixtures_supported() -> bool {
    !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    )
}

fn pinned_dylint_link_version() -> &'static str {
    soldr_cli::fetch::known_tools::lookup_by_crate("dylint-link")
        .and_then(|spec| spec.pinned_version)
        .expect("dylint-link must have a registry pin")
}

/// Pre-seed `<SOLDR_CACHE_DIR>/bin/dylint-link-<pin>/dylint-link` with an
/// executable that answers the smoke probe — where `check_cache` looks for
/// an already-downloaded prebuilt, so the ensure never touches the network.
fn seed_dylint_link_cache(cache_root: &Path) -> PathBuf {
    let dir = cache_root
        .join("bin")
        .join(format!("dylint-link-{}", pinned_dylint_link_version()));
    fs::create_dir_all(&dir).expect("create seeded dylint-link dir");
    let binary = dir.join("dylint-link");
    fs::write(&binary, "#!/bin/sh\nexit 0\n").expect("write seeded dylint-link");
    soldr_platform::fs::permissions::make_executable(&binary).expect("chmod seeded dylint-link");
    dir
}

/// A fake cargo that records the PATH it was launched with (the one thing
/// the shared `fake_cargo_script` does not log) and answers the two probes
/// the front door and the cook-index hydrate may send it. Never links.
fn path_logging_cargo_script(log_path: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         if [ \"$1\" = \"--version\" ]; then\n\
           echo 'cargo 1.0.0-test'\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = \"metadata\" ]; then\n\
           echo '{{}}'\n\
           exit 0\n\
         fi\n\
         echo \"child_path=$PATH\" >> \"{0}\"\n\
         exit 0\n",
        log_path.display()
    )
}

#[test]
fn cargo_clippy_in_configured_lint_crate_gets_managed_dylint_link_on_path() {
    if !shell_fixtures_supported() {
        return;
    }

    let cache_root = unique_temp_dir("cargo-configured-dylint-link");
    let home_root = cache_root.join("home");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    // Replace the shared fake cargo with one that records its PATH.
    write_fake_script(&cargo, &path_logging_cargo_script(&log_path));
    let seeded = seed_dylint_link_cache(&cache_root);
    let daemon = common::isolated_daemon::IsolatedDaemon::spawn(
        &common::soldr_daemon_bin(),
        &cache_root,
        &home_root,
    );

    // Deliberately no Cargo.toml / rust-toolchain.toml: the fixture
    // directory is its own project root (unique_temp_dir lives outside any
    // workspace), exactly like the `cli_cargo_linker.rs` fixture.
    let project = cache_root.join("project");
    fs::create_dir_all(project.join(".cargo")).expect("create project/.cargo");
    let config_path = project.join(".cargo").join("config.toml");

    let run = |configured: bool| -> String {
        let _ = fs::remove_file(&log_path);
        if configured {
            fs::write(
                &config_path,
                "[target.'cfg(all())']\nrustflags = [\"-C\", \"linker=dylint-link\"]\n",
            )
            .expect("write lint-crate .cargo/config.toml");
        } else {
            let _ = fs::remove_file(&config_path);
        }
        let mut command = isolated_soldr_command();
        command.current_dir(&project);
        daemon.configure_client(&mut command);
        let output = command
            .args(["cargo", "clippy"])
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_CARGO_BIN", &cargo)
            .env("SOLDR_TEST_RUSTC_BIN", &rustc)
            // Pin PATH: no ambient `dylint-link` may satisfy the lookup
            // (the gate image's ~/.cargo/bin could otherwise), and
            // isolated_test_path's hermetic clang keeps the phase-1
            // automatic-linker probe resolvable (soldr#3578).
            .env("PATH", isolated_test_path())
            // No pin in the fixture: opt out of the repo-pin requirement
            // instead of letting rustup resolve anything ambient.
            .env("SOLDR_ALLOW_UNPINNED", "1")
            .env("SOLDR_NO_AUTO_COMPONENT", "1")
            .env_remove("CARGO_HOME")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env_remove("SOLDR_LINKER")
            .env_remove("CARGO_BUILD_TARGET")
            .env_remove("SOLDR_DYLINT_TOOLCHAIN")
            .env_remove("SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS")
            .env_remove("SOLDR_TARGET_CACHE_MODE")
            .env_remove("SOLDR_BUILD_CACHE_MODE")
            .output()
            .expect("failed to run soldr cargo clippy for the dylint-link fixture");
        assert!(
            output.status.success(),
            "soldr cargo clippy failed (configured={configured})\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        fs::read_to_string(&log_path).expect("failed to read fake cargo log")
    };

    // PHASE 1 (control): no `.cargo/config.toml` — an ordinary crate gains
    // nothing even though a seedable dylint-link cache sits right there.
    // This is what makes phase 2's presence meaningful: it can only come
    // from the config-declared ensure, not from ambient PATH or some
    // unrelated bin-dir plumbing.
    let ordinary = run(false);
    // The pinned managed bin dir's name, not a bare `dylint-link-` substring:
    // this fixture's own temp root is named `...-dylint-link-...`, and its
    // generated linker-shim dir is legitimately on the child PATH.
    let managed = format!("dylint-link-{}", pinned_dylint_link_version());
    assert!(
        !ordinary.contains(&managed),
        "an ordinary crate must never gain the dylint tools merely because \
         it invoked Clippy (soldr#3571): {ordinary}"
    );

    // PHASE 2 (the fix): the configured lint crate. The child cargo's PATH
    // must contain the managed `dylint-link-<pin>` bin dir — pre-fix this
    // assertion failed because plain `cargo clippy` returned early before
    // any dylint-link ensure could run.
    let configured = run(true);
    assert!(
        configured.contains(&*seeded.to_string_lossy()),
        "a configured lint crate's standalone `cargo clippy` must prepend the \
         managed dylint-link bin dir to the child PATH (soldr#3571); expected \
         {} in:\n{configured}",
        seeded.display()
    );
    assert!(
        configured.contains(&managed),
        "the PATH entry must name the pinned dylint-link version: {configured}"
    );
}
