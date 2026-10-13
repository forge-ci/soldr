#![allow(unused_imports)]

use crate::common;

use crate::common::*;
use serde_json::Value;
use std::io::Write;
use std::process::Command;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// emitted by the fake cargo script. Returns the `<value>` of the first match.
fn extract_linker_env_value(log: &str) -> Option<String> {
    extract_cargo_target_env_value(log, "LINKER")
}

fn extract_rustflags_env_value(log: &str) -> Option<String> {
    extract_cargo_target_env_value(log, "RUSTFLAGS")
}

fn extract_cargo_target_env_value(log: &str, suffix: &str) -> Option<String> {
    extract_cargo_target_env_entry(log, suffix).map(|(_, value)| value)
}

/// Same search as `extract_cargo_target_env_value`, but also returns the full
/// `CARGO_TARGET_<...>_<suffix>` env var name so a caller can recover which
/// triple soldr resolved.
fn extract_cargo_target_env_entry(log: &str, suffix: &str) -> Option<(String, String)> {
    for line in log.lines() {
        let Some(rest) = line.strip_prefix("cargo_target_env ") else {
            continue;
        };
        let Some(eq_idx) = rest.find('=') else {
            continue;
        };
        let (name, value) = (&rest[..eq_idx], &rest[eq_idx + 1..]);
        if name.starts_with("CARGO_TARGET_") && name.ends_with(&format!("_{suffix}")) {
            return Some((name.to_string(), value.trim().to_string()));
        }
    }
    None
}

fn log_has_any_cargo_target_env(log: &str) -> bool {
    log.lines()
        .any(|line| line.starts_with("cargo_target_env "))
}

/// Install a fake `reld` on a PATH directory, so the `Fast`/default probe
/// sees it as available. Returns the directory to prepend to PATH.
///
/// The probe looks for `reld` + the platform executable suffix (`reld.exe` on
/// Windows — the name rustc can spawn), so the fake must use that exact name;
/// the `.cmd` from `fake_script_path` is deliberately invisible to it. The
/// fake cargo never links, so the file is never executed.
///
/// A fake `clang` sits beside it: on Linux and macOS reld is driven through a
/// clang driver shim, and since soldr#3477 the shim resolves clang when it is
/// rendered. A host with no compiler on PATH (the macOS Recovery release-gate
/// guest) otherwise fails before cargo runs with "the linker driver needs
/// `clang`". Never executed either.
fn install_fake_reld() -> PathBuf {
    let dir = unique_temp_dir("fake-reld");
    for tool in ["reld", "clang"] {
        let path = dir.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX));
        write_fake_script(&path, "#!/bin/sh\nexit 0\n");
    }
    dir
}

/// Prepend `dir` to `command`'s `PATH` so a fake binary there shadows the host's.
fn prepend_to_path(command: &mut Command, dir: &Path) {
    let mut paths =
        std::env::split_paths(&std::env::var("PATH").unwrap_or_default()).collect::<Vec<_>>();
    paths.insert(0, dir.to_path_buf());
    command.env("PATH", std::env::join_paths(paths).expect("join PATH"));
}

/// soldr#3262: reld is the default (and `fast`) linker. Assert it is injected
/// the way `resolve_for_target` prescribes for the host: through clang
/// `--ld-path=reld` on Linux (so the driver injects CRT) and macOS (rustc's
/// `darwin-cc` flavor passes clang-driver argv, soldr#3359), direct on Windows
/// (reld bridges to lld-link).
/// A generated linker shim is handed to Cargo by its content-addressed file
/// name, never an absolute path under the cache root: `-C linker=` is part of
/// every rustc's cache key, so a root-specific path made a cache restored
/// under another root miss everything (Perf Matrix cold-tar-untar-warm).
fn is_generated_shim_name(value: &str) -> bool {
    !value.contains('/')
        && !value.contains('\\')
        && (value.starts_with("clang-") || value.starts_with("cc-"))
}

fn assert_reld_injected(log: &str) {
    let linker = extract_linker_env_value(log).unwrap_or_else(|| {
        panic!("expected CARGO_TARGET_<triple>_LINKER in fake cargo log: {log}")
    });
    let rustflags = extract_rustflags_env_value(log);
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Linux | soldr_platform::host::facts::HostOs::MacOs
    ) {
        assert!(
            is_generated_shim_name(&linker),
            "linux/macos reld uses a generated clang driver shim: {log}"
        );
        assert!(
            rustflags.is_none(),
            "linux/macos linker selection must not replace [build] rustflags: {log}"
        );
    } else {
        assert_eq!(linker, "reld", "reld injected directly: {log}");
        assert!(rustflags.is_none(), "direct reld needs no rustflags: {log}");
    }
}

#[test]
fn cargo_front_door_default_injects_reld_when_available() {
    let cache_root = unique_temp_dir("cargo-default-linker");
    let home_root = cache_root.join("home");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let reld_dir = install_fake_reld();
    let daemon = common::isolated_daemon::IsolatedDaemon::spawn(
        &common::soldr_daemon_bin(),
        &cache_root,
        &home_root,
    );
    let mut command = isolated_soldr_command();
    // soldr#3203: run outside this crate, whose workspace target is the suite's own `target/`.
    command.current_dir(&cache_root);
    prepend_to_path(&mut command, &reld_dir);
    daemon.configure_client(&mut command);
    let output = command
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .env_remove("SOLDR_LINKER")
        .output()
        .expect("failed to run soldr cargo build with no SOLDR_LINKER");

    assert!(
        output.status.success(),
        "default-linker front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert_reld_injected(&log);
}

#[test]
fn cargo_front_door_default_falls_back_to_rust_lld_without_reld() {
    // reld is not yet bundled or universally installed (soldr#3262): when it is
    // absent from PATH the default (`Fast`) degrades to rust-lld rather than
    // failing the link. On Linux that is `clang -fuse-ld=lld`; on Windows it is
    // `rust-lld`; on macOS the platform default (no injection, issue #509).
    let cache_root = unique_temp_dir("cargo-default-linker-fallback");
    let home_root = cache_root.join("home");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let daemon = common::isolated_daemon::IsolatedDaemon::spawn(
        &common::soldr_daemon_bin(),
        &cache_root,
        &home_root,
    );
    let mut command = isolated_soldr_command();
    command.current_dir(&cache_root);
    daemon.configure_client(&mut command);
    let output = command
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .env_remove("SOLDR_LINKER")
        .output()
        .expect("failed to run soldr cargo build with no SOLDR_LINKER");

    assert!(
        output.status.success(),
        "default-linker fallback front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Linux
    ) {
        let linker_value = extract_linker_env_value(&log).unwrap_or_else(|| {
            panic!("expected CARGO_TARGET_<triple>_LINKER in fake cargo log: {log}")
        });
        assert!(
            is_generated_shim_name(&linker_value),
            "linux fallback uses a generated clang driver shim: {log}"
        );
        assert!(
            extract_rustflags_env_value(&log).is_none(),
            "linux fallback must preserve [build] rustflags: {log}"
        );
    } else if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        let linker_value = extract_linker_env_value(&log).unwrap_or_else(|| {
            panic!("expected CARGO_TARGET_<triple>_LINKER in fake cargo log: {log}")
        });
        assert_eq!(
            linker_value, "rust-lld",
            "windows fallback injects rust-lld directly: {log}"
        );
    } else {
        assert!(
            !log_has_any_cargo_target_env(&log),
            "macOS fallback should not inject any CARGO_TARGET_* env (issue #509): {log}"
        );
    }
}

#[test]
fn cargo_front_door_rust_lld_injects_target_linker_env() {
    let cache_root = unique_temp_dir("cargo-rust-lld-linker");
    let home_root = cache_root.join("home");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let daemon = common::isolated_daemon::IsolatedDaemon::spawn(
        &common::soldr_daemon_bin(),
        &cache_root,
        &home_root,
    );
    let mut command = isolated_soldr_command();
    // soldr#3203: run outside this crate, whose workspace target is the suite's own `target/`.
    command.current_dir(&cache_root);
    daemon.configure_client(&mut command);
    let output = command
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .env("SOLDR_LINKER", "rust-lld")
        .output()
        .expect("failed to run soldr cargo build with SOLDR_LINKER=rust-lld");

    assert!(
        output.status.success(),
        "rust-lld front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");

    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        let linker_value = extract_linker_env_value(&log).unwrap_or_else(|| {
            panic!("expected CARGO_TARGET_<triple>_LINKER in fake cargo log: {log}")
        });
        let rustflags_value = extract_rustflags_env_value(&log);
        assert_eq!(
            linker_value, "rust-lld",
            "windows-msvc rust-lld should inject rust-lld directly: {log}"
        );
        assert!(
            rustflags_value.is_none(),
            "windows-msvc rust-lld should not inject rustflags: {log}"
        );
    } else if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::MacOs
    ) {
        // Issue #509: `SOLDR_LINKER=rust-lld` must not inject anything on
        // macOS — Apple clang rejects `-fuse-ld=lld`.
        assert!(
            !log_has_any_cargo_target_env(&log),
            "macOS rust-lld should not inject any CARGO_TARGET_* env (issue #509): {log}"
        );
    } else {
        let linker_value = extract_linker_env_value(&log).unwrap_or_else(|| {
            panic!("expected CARGO_TARGET_<triple>_LINKER in fake cargo log: {log}")
        });
        let rustflags_value = extract_rustflags_env_value(&log);
        assert!(
            is_generated_shim_name(&linker_value),
            "non-windows non-macos rust-lld should use a clang driver shim: {log}"
        );
        assert!(
            rustflags_value.is_none(),
            "non-windows non-macos rust-lld must preserve [build] rustflags: {log}"
        );
    }
}

#[test]
fn cargo_front_door_mold_on_non_linux_returns_clear_error() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Linux
    ) {
        return;
    }
    let cache_root = unique_temp_dir("cargo-mold-non-linux");
    let home_root = cache_root.join("home");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let daemon = common::isolated_daemon::IsolatedDaemon::spawn(
        &common::soldr_daemon_bin(),
        &cache_root,
        &home_root,
    );
    let mut command = isolated_soldr_command();
    daemon.configure_client(&mut command);
    let output = command
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .env("SOLDR_LINKER", "mold")
        .output()
        .expect("failed to run soldr cargo build with SOLDR_LINKER=mold on non-linux");

    assert!(
        !output.status.success(),
        "SOLDR_LINKER=mold should fail on non-linux hosts; stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("mold is not supported"),
        "non-linux mold error message should mention 'mold is not supported': {stderr}"
    );
}

/// `SOLDR_LINKER=fast` resolves to reld when it is available (soldr#3262):
/// clang `--ld-path=reld` on Linux, direct `reld` on Windows/macOS. Same as
/// the default, so this asserts the explicit `fast` spelling reaches the same
/// injection when a fake `reld` is on PATH.
#[test]
fn cargo_front_door_fast_uses_reld() {
    let cache_root = unique_temp_dir("cargo-fast-linker");
    let home_root = cache_root.join("home");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let reld_dir = install_fake_reld();
    let daemon = common::isolated_daemon::IsolatedDaemon::spawn(
        &common::soldr_daemon_bin(),
        &cache_root,
        &home_root,
    );
    let mut command = isolated_soldr_command();
    // soldr#3203: run outside this crate, whose workspace target is the suite's own `target/`.
    command.current_dir(&cache_root);
    prepend_to_path(&mut command, &reld_dir);
    daemon.configure_client(&mut command);
    let output = command
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .env("SOLDR_LINKER", "fast")
        .output()
        .expect("failed to run soldr cargo build with SOLDR_LINKER=fast");

    assert!(
        output.status.success(),
        "fast-linker front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert_reld_injected(&log);
}

/// soldr#3277: a project's own `[target.<triple>] linker` in
/// `.cargo/config.toml` is the user's decision, not soldr's to override with
/// its own reld/rust-lld default. An explicit `SOLDR_LINKER` request is still
/// honored — that is also a user decision, just a more direct one.
#[test]
fn cargo_front_door_respects_project_target_linker_config() {
    let cache_root = unique_temp_dir("cargo-project-target-linker");
    let home_root = cache_root.join("home");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let reld_dir = install_fake_reld();
    let daemon = common::isolated_daemon::IsolatedDaemon::spawn(
        &common::soldr_daemon_bin(),
        &cache_root,
        &home_root,
    );

    // Deliberately no Cargo.toml here: `linker.rs::project_root` walks
    // ancestors looking for one and falls back to the start directory when
    // none is found, and `unique_temp_dir` lives under `std::env::temp_dir()`
    // (outside any crate's workspace), so this fixture directory is itself
    // the project root that `linker::resolve_project_choice_from_cwd` reads.
    let project = cache_root.join("project");
    fs::create_dir_all(project.join(".cargo")).expect("create project/.cargo");

    let run = |linker_env: Option<&str>| -> String {
        let _ = fs::remove_file(&log_path);
        let mut command = isolated_soldr_command();
        command.current_dir(&project);
        prepend_to_path(&mut command, &reld_dir);
        daemon.configure_client(&mut command);
        command
            .args(["cargo", "build"])
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_CARGO_BIN", &cargo)
            .env("SOLDR_TEST_RUSTC_BIN", &rustc)
            .env_remove("SOLDR_TARGET_CACHE_MODE")
            .env_remove("SOLDR_BUILD_CACHE_MODE");
        if let Some(value) = linker_env {
            command.env("SOLDR_LINKER", value);
        } else {
            command.env_remove("SOLDR_LINKER");
        }
        let output = command
            .output()
            .expect("failed to run soldr cargo build for project-target-linker fixture");
        assert!(
            output.status.success(),
            "project-target-linker front door failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        fs::read_to_string(&log_path).expect("failed to read fake tool log")
    };

    // PHASE 1 (control): no `.cargo/config.toml` yet, SOLDR_LINKER unset —
    // soldr injects its own default linker. This is what makes phase 2
    // meaningful on every host: without it, an absence of injection there
    // could just mean the default never injects anything on this platform.
    let control = run(None);
    let (env_name, _) = extract_cargo_target_env_entry(&control, "LINKER").unwrap_or_else(|| {
        panic!(
            "expected the control run (no project config) to inject a \
             CARGO_TARGET_<triple>_LINKER: {control}"
        )
    });

    // Resolve the active triple from the injected env var name rather than
    // re-detecting it in the test process — the test process and the soldr
    // child can resolve rustc/host differently.
    let prefix = env_name
        .strip_prefix("CARGO_TARGET_")
        .and_then(|rest| rest.strip_suffix("_LINKER"))
        .unwrap_or_else(|| panic!("unexpected CARGO_TARGET_*_LINKER env name shape: {env_name}"));
    let triple = ["x86_64", "aarch64"]
        .into_iter()
        .flat_map(|arch| {
            [
                "-pc-windows-msvc",
                "-pc-windows-gnu",
                "-unknown-linux-gnu",
                "-unknown-linux-musl",
                "-apple-darwin",
            ]
            .into_iter()
            .map(move |suffix| format!("{arch}{suffix}"))
        })
        .find(|candidate| soldr_cli::linker::cargo_target_env_prefix(candidate) == prefix)
        .unwrap_or_else(|| {
            panic!("no known triple matches observed CARGO_TARGET_ prefix {prefix}: {control}")
        });

    // PHASE 2 (the fix): the project now declares its own linker for the
    // active triple. soldr must leave it alone — no CARGO_TARGET_* injection
    // at all.
    let config_path = project.join(".cargo").join("config.toml");
    fs::write(
        &config_path,
        format!("[target.{triple}]\nlinker = \"cc\"\n"),
    )
    .expect("write project .cargo/config.toml");
    let with_project_config = run(None);
    // Assert on the two env vars this fix owns rather than on
    // `log_has_any_cargo_target_env`: the fake cargo logs *every* env var
    // starting with `CARGO_TARGET_`, and `CARGO_TARGET_DIR` is a legitimate
    // ambient value (`ci/perf_local.py` exports `CARGO_TARGET_DIR=/target`
    // into the Docker dev loop, and `isolated_soldr_command` only scrubs
    // route-selecting vars). A blanket check would fail there for a reason
    // that has nothing to do with soldr#3277.
    for suffix in ["LINKER", "RUSTFLAGS"] {
        assert!(
            extract_cargo_target_env_entry(&with_project_config, suffix).is_none(),
            "soldr#3277: soldr must not override a project's declared \
             [target.{triple}] linker from .cargo/config.toml, but it injected a \
             CARGO_TARGET_<triple>_{suffix}: {with_project_config}"
        );
    }

    // PHASE 3 (precedence): an explicit SOLDR_LINKER request is a user
    // decision too, and still wins even with the project config present.
    // reld injects on every platform when a fake `reld` is on PATH, unlike
    // rust-lld, which is a no-op on macOS (issue #509) — so reld is the
    // unambiguous choice to prove precedence on every host.
    let with_explicit_request = run(Some("reld"));
    assert!(
        extract_linker_env_value(&with_explicit_request).is_some(),
        "explicit SOLDR_LINKER=reld should still inject despite project config: {with_explicit_request}"
    );
}
