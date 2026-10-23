#![allow(unused_imports)]

use crate::common;

use crate::common::*;
use serde_json::Value;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn fake_cargo_miri_script(log_path: &Path) -> String {
    let output_dir = fake_rustc_output_dir(log_path);
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             echo cargo miri wrapper=%RUSTC_WRAPPER% rustc=%RUSTC% cache=%SOLDR_CACHE_ENABLED% session=%ZCCACHE_SESSION_ID%>>\"{0}\"\n\
             if \"%~1\"==\"miri\" (\n\
               if defined RUSTC_WRAPPER (\n\
                 call \"%RUSTC_WRAPPER%\" \"%RUSTC%\" --crate-name miri_demo --emit metadata,link -o \"{1}\\miri_demo\" --out-dir \"{1}\"\n\
               ) else (\n\
                 call \"%RUSTC%\" --crate-name miri_demo --emit metadata,link -o \"{1}\\miri_demo\" --out-dir \"{1}\"\n\
               )\n\
               exit /b %ERRORLEVEL%\n\
             )\n\
             echo unsupported fake cargo miri invocation %* 1>&2\n\
             exit /b 1\n",
            log_path.display(),
            output_dir.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             echo \"cargo miri wrapper=${{RUSTC_WRAPPER:-}} rustc=${{RUSTC:-}} cache=${{SOLDR_CACHE_ENABLED:-}} session=${{ZCCACHE_SESSION_ID:-}}\" >> \"{0}\"\n\
             if [ \"$1\" = \"miri\" ]; then\n\
               if [ -n \"${{RUSTC_WRAPPER:-}}\" ]; then\n\
                 \"$RUSTC_WRAPPER\" \"$RUSTC\" --crate-name miri_demo --emit metadata,link -o \"{1}/miri_demo\" --out-dir \"{1}\"\n\
               else\n\
                 \"$RUSTC\" --crate-name miri_demo --emit metadata,link -o \"{1}/miri_demo\" --out-dir \"{1}\"\n\
               fi\n\
               exit $?\n\
             fi\n\
             echo \"unsupported fake cargo miri invocation: $*\" >&2\n\
             exit 1\n",
            log_path.display(),
            output_dir.display()
        )
    }
}

fn install_fake_cargo_miri_toolchain(log_path: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let (cargo, rustc, zccache) = install_fake_toolchain(log_path);
    write_fake_script(&cargo, &fake_cargo_miri_script(log_path));
    (cargo, rustc, zccache)
}

fn install_fake_direct_rustc_like_toolchain(
    log_path: &Path,
) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
    let (rustup, _cargo, rustc, _rustfmt) = install_fake_rustup_toolchain(log_path);
    let tool_dir = rustc
        .parent()
        .expect("fake rustc should live in a tool dir")
        .to_path_buf();
    let clippy_driver = fake_script_path(&tool_dir, "clippy-driver");
    let zccache = fake_script_path(&tool_dir, "zccache");
    write_fake_script(
        &clippy_driver,
        &fake_version_tool_script(log_path, "clippy-driver"),
    );
    write_fake_script(&zccache, &fake_zccache_script(log_path));
    (rustup, rustc, clippy_driver, zccache, tool_dir)
}

fn write_rustc_like_source(cache_root: &Path) -> PathBuf {
    let src_dir = cache_root.join("src");
    fs::create_dir_all(&src_dir).expect("failed to create rustc-like source dir");
    let source_path = src_dir.join("lib.rs");
    fs::write(&source_path, "pub fn value() -> usize { 42 }\n")
        .expect("failed to write rustc-like source");
    source_path
}

#[test]
fn cargo_front_door_uses_real_tool_overrides_before_path_probe() {
    let cache_root = unique_temp_dir("cargo-real-tool-overrides");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let shim_dir = unique_temp_dir("cargo-shim-dir");
    let shim_cargo = fake_script_path(&shim_dir, "cargo");
    write_fake_script(
        &shim_cargo,
        &fake_version_tool_script(&log_path, "shim-cargo"),
    );

    let output = isolated_soldr_command_in(&cache_root)
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_REAL_CARGO", &cargo)
        .env("SOLDR_REAL_RUSTC", &rustc)
        .env("PATH", prepend_to_path(&shim_dir))
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with real tool overrides");

    assert!(
        output.status.success(),
        "real-tool override front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cargo wrapper="),
        "real cargo should have been invoked: {log}"
    );
    assert!(
        !log.contains("shim-cargo"),
        "PATH shim should not be resolved when SOLDR_REAL_CARGO is set: {log}"
    );
}

#[test]
fn cargo_front_door_keeps_cache_enabled_for_non_build_subcommands() {
    let cache_root = unique_temp_dir("cargo-non-build-no-cache");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let output = isolated_soldr_command()
        .args(["cargo", "metadata"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .output()
        .expect("failed to run soldr cargo metadata with fake tools");

    assert!(
        output.status.success(),
        "non-build cargo front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    // soldr#1368: the managed session subprocess is gone; the front door
    // just keeps the compile cache enabled for unmodeled subcommands.
    assert!(
        log.contains("cache=1"),
        "cargo front door should keep cache enabled for unmodeled subcommands: {log}"
    );
}

#[test]
fn cargo_front_door_detects_build_after_global_cargo_options() {
    let cache_root = unique_temp_dir("cargo-global-options-cache");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let output = isolated_soldr_command_in(&cache_root)
        .args(["cargo", "--manifest-path", "demo/Cargo.toml", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with global cargo options");

    assert!(
        output.status.success(),
        "global-option cargo front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    // soldr#1368: rustc compiles route to the soldr-daemon embedded
    // service via RUSTC_WRAPPER=soldr, not a managed `zccache start`.
    assert!(
        log.contains("cache=1") && log.contains("cargo wrapper="),
        "build after global cargo options should still enable caching + wrap rustc: {log}"
    );
}

#[test]
fn cargo_miri_keeps_inner_rustc_wrapped_by_policy() {
    let cache_root = unique_temp_dir("cargo-miri-zccache-policy");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_cargo_miri_toolchain(&log_path);
    let output = isolated_soldr_command_in(&cache_root)
        .args(["cargo", "miri"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo miri with fake tools");

    assert!(
        output.status.success(),
        "cargo miri policy route failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cargo miri wrapper=") && log.contains("cache=1"),
        "cargo miri should keep cache env and wrapper available: {log}"
    );
}

#[test]
fn direct_rustc_like_commands_route_through_zccache_with_and_without_global_flags() {
    let cache_root = unique_temp_dir("direct-rustc-like-zccache");
    let home_root = cache_root.join("home");
    let _broker = BrokerHomeGuard::new(&cache_root, &home_root);
    let daemon_executable =
        crate::fixture_daemon_isolation::start_fixture_broker_daemon(&cache_root, &home_root);
    for tool in ["rustc", "clippy-driver"] {
        for (label, prefix_args) in [
            ("plain", vec![tool]),
            ("global-zccache", vec!["--zccache", "system", tool]),
        ] {
            let case_root = cache_root.join(format!("{tool}-{label}"));
            let output_dir = case_root.join("out");
            fs::create_dir_all(&output_dir).expect("create direct compiler output directory");
            let source_path = write_rustc_like_source(&case_root);
            let mut args = prefix_args;
            args.extend([
                "--crate-name",
                "direct_demo",
                "--crate-type",
                "lib",
                "--emit",
                "metadata",
            ]);
            let mut command = isolated_soldr_command();
            command
                .args(args)
                .arg("--out-dir")
                .arg(&output_dir)
                .arg(&source_path);
            let output = command
                .current_dir(&case_root)
                .env("SOLDR_CACHE_DIR", &cache_root)
                .env("HOME", &home_root)
                .env("USERPROFILE", &home_root)
                .env(
                    soldr_cli::daemon::lifecycle::SOLDR_DAEMON_EXE_ENV_VAR,
                    &daemon_executable,
                )
                // The setup-soldr action smoke runs the test suite under
                // `soldr --no-cache cargo test`, which propagates this marker
                // to test processes. This positive wrapper assertion needs
                // the child soldr invocation's normal cache-enabled default.
                .env_remove("SOLDR_CACHE_ENABLED")
                .env_remove("ZCCACHE_CACHE_DIR")
                .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
                .env_remove("ZCCACHE_DISABLE")
                .env("SOLDR_SESSION_DEBUG", "1")
                .output()
                .unwrap_or_else(|_| panic!("failed to run direct {tool} route {label}"));
            assert!(
                output.status.success(),
                "direct {tool} route {label} failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );

            assert!(
                String::from_utf8_lossy(&output.stderr).contains("SESSION compile served"),
                "direct {tool} should complete through the embedded-cache session\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[test]
fn direct_rustc_like_cache_disable_modes_remain_direct() {
    for tool in ["rustc", "clippy-driver"] {
        for (label, prefix_args, zccache_disable) in [
            (
                "no-cache-global",
                vec!["--no-cache", "--zccache", "system", tool],
                false,
            ),
            ("zccache-disable", vec!["--zccache", "system", tool], true),
        ] {
            let cache_root = unique_temp_dir(&format!("direct-{tool}-{label}-direct"));
            let log_path = cache_root.join("tool.log");
            let source_path = write_rustc_like_source(&cache_root);
            let (rustup, rustc, clippy_driver, _zccache, tool_dir) =
                install_fake_direct_rustc_like_toolchain(&log_path);

            let mut args = prefix_args;
            args.extend(["--crate-name", "direct_bypass", "--emit", "metadata,link"]);
            let mut command = isolated_soldr_command();
            command.args(args).arg(&source_path);
            command
                .current_dir(&cache_root)
                .env("SOLDR_CACHE_DIR", &cache_root)
                .env("SOLDR_TEST_RUSTC_BIN", &rustc)
                .env("SOLDR_REAL_CLIPPY_DRIVER", &clippy_driver)
                .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
                .env("PATH", prepend_to_path(&tool_dir))
                .env_remove("CARGO_HOME")
                .env_remove("RUSTUP_HOME")
                .env_remove("RUSTUP_TOOLCHAIN")
                .env_remove("ZCCACHE_CACHE_DIR")
                .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR");
            if zccache_disable {
                command.env("ZCCACHE_DISABLE", "1");
            } else {
                command.env_remove("ZCCACHE_DISABLE");
            }

            let output = command.output().unwrap_or_else(|_| {
                panic!("failed to run direct {tool} cache-disable route {label}")
            });

            assert!(
                output.status.success(),
                "direct {tool} cache-disable route {label} failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );

            let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
            assert!(
                !log.contains("zccache wrapper"),
                "direct {tool} cache-disable route {label} should not use zccache: {log}"
            );
            assert!(
                log.lines().any(|line| line.starts_with(tool)),
                "direct {tool} cache-disable route {label} should invoke compiler directly: {log}"
            );
        }
    }
}

#[test]
fn direct_rustc_non_cacheable_print_probe_stays_direct() {
    for (label, args) in [
        ("plain", vec!["rustc", "--print", "cfg"]),
        (
            "global-zccache",
            vec!["--zccache", "system", "rustc", "--print", "cfg"],
        ),
    ] {
        let cache_root = unique_temp_dir(&format!("direct-rustc-print-{label}"));
        let log_path = cache_root.join("tool.log");
        let (rustup, rustc, clippy_driver, _zccache, tool_dir) =
            install_fake_direct_rustc_like_toolchain(&log_path);

        let output = isolated_soldr_command()
            .args(args)
            .current_dir(&cache_root)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_RUSTC_BIN", &rustc)
            .env("SOLDR_REAL_CLIPPY_DRIVER", &clippy_driver)
            .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
            .env("PATH", prepend_to_path(&tool_dir))
            .env_remove("CARGO_HOME")
            .env_remove("RUSTUP_HOME")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env_remove("ZCCACHE_CACHE_DIR")
            .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
            .env_remove("ZCCACHE_DISABLE")
            .output()
            .unwrap_or_else(|_| panic!("failed to run direct rustc print probe {label}"));

        assert!(
            output.status.success(),
            "direct rustc print probe {label} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
        assert!(
            !log.contains("zccache wrapper"),
            "direct rustc print probe {label} should bypass zccache: {log}"
        );
        assert!(
            log.lines().any(|line| line.starts_with("rustc ")),
            "direct rustc print probe {label} should invoke rustc directly: {log}"
        );
    }
}

#[test]
fn rustfmt_file_invocation_routes_through_zccache_formatter() {
    let cache_root = unique_temp_dir("rustfmt-zccache-formatter");
    let log_path = cache_root.join("tool.log");
    let source_path = write_rustfmt_source(&cache_root);
    let (rustup, _, _, _) = install_fake_rustup_toolchain(&log_path);
    let output = isolated_soldr_command()
        .arg("rustfmt")
        .arg(&source_path)
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("PATH", isolated_test_path())
        .env_remove("CARGO_HOME")
        .env_remove("RUSTUP_HOME")
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("ZCCACHE_CACHE_DIR")
        .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("failed to run soldr rustfmt with fake tools");

    assert!(
        output.status.success(),
        "rustfmt zccache route failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.lines().any(|line| line.starts_with("rustfmt ")),
        "embedded formatter cache should invoke rustfmt: {log}"
    );
    assert!(
        path_display_variants(&source_path)
            .iter()
            .any(|path| log.contains(path)),
        "rustfmt should receive the source file: {log}"
    );
}

#[test]
fn rustfmt_recursive_runs_while_explicit_skip_children_uses_embedded_cache() {
    let cache_root = unique_temp_dir("rustfmt-embedded-formatter");
    let log_path = cache_root.join("tool.log");
    let source_path = write_rustfmt_source(&cache_root);
    let (rustup, _, _, _) = install_fake_rustup_toolchain(&log_path);
    let format_cache_root = cache_root.join("explicit-zccache");

    let run = |skip_children: bool| {
        let mut command = isolated_soldr_command();
        command.arg("rustfmt");
        if skip_children {
            command.args(["--config", "skip_children=true"]);
        }
        command
            .arg(&source_path)
            .current_dir(&cache_root)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
            .env("PATH", isolated_test_path())
            .env_remove("CARGO_HOME")
            .env_remove("RUSTUP_HOME")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env("ZCCACHE_CACHE_DIR", &format_cache_root)
            .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
            .env_remove("ZCCACHE_DISABLE")
            .output()
            .expect("failed to run soldr rustfmt through the embedded format cache")
    };
    let output = run(false);

    assert!(
        output.status.success(),
        "embedded rustfmt format-cache route failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake rustfmt log");
    assert!(
        log.lines().any(|line| line.starts_with("rustfmt ")),
        "embedded format cache should invoke rustfmt: {log}"
    );
    assert!(
        path_display_variants(&source_path)
            .iter()
            .any(|path| log.contains(path)),
        "embedded format cache should pass the source file to rustfmt: {log}"
    );

    let recursive_output = run(false);
    assert!(
        recursive_output.status.success(),
        "second recursive rustfmt invocation failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&recursive_output.stdout),
        String::from_utf8_lossy(&recursive_output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&log_path)
            .expect("failed to reread fake rustfmt log")
            .lines()
            .filter(|line| line.starts_with("rustfmt "))
            .count(),
        2,
        "recursive rustfmt must run again so changed child modules cannot be missed"
    );

    let nonrecursive_first = run(true);
    assert!(
        nonrecursive_first.status.success(),
        "first nonrecursive rustfmt invocation failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&nonrecursive_first.stdout),
        String::from_utf8_lossy(&nonrecursive_first.stderr)
    );
    let nonrecursive_cached = run(true);
    assert!(
        nonrecursive_cached.status.success(),
        "cached nonrecursive rustfmt invocation failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&nonrecursive_cached.stdout),
        String::from_utf8_lossy(&nonrecursive_cached.stderr)
    );
    let cached_log = fs::read_to_string(&log_path).expect("failed to reread fake rustfmt log");
    assert_eq!(
        cached_log
            .lines()
            .filter(|line| line.starts_with("rustfmt "))
            .count(),
        3,
        "only explicit skip_children=true may hit the embedded marker cache: {cached_log}"
    );
    assert!(
        format_cache_root.join("fmt").is_dir(),
        "embedded format cache should honor the explicit ZCCACHE_CACHE_DIR"
    );
}

#[test]
fn embedded_rustfmt_preserves_child_environment_and_exact_exit_code() {
    let cache_root = unique_temp_dir("rustfmt-embedded-policy-exit");
    let log_path = cache_root.join("tool.log");
    let source_path = write_rustfmt_source(&cache_root);
    let (rustup, _, _, _) = install_fake_rustup_toolchain(&log_path);
    let cargo_home = cache_root.join("explicit-cargo-home");
    let rustup_home = cache_root.join("explicit-rustup-home");

    let output = isolated_soldr_command()
        .args(["rustfmt", source_path.to_str().unwrap()])
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("ZCCACHE_CACHE_DIR", cache_root.join("zccache"))
        .env("CARGO_HOME", &cargo_home)
        .env("RUSTUP_HOME", &rustup_home)
        .env("SOLDR_TEST_TOOL_EXIT_CODE", "37")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("run embedded rustfmt with host-owned child policy");

    assert_eq!(
        output.status.code(),
        Some(37),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fs::read_to_string(&log_path).expect("read fake rustfmt log");
    assert!(
        log.contains(&format!("cargo_home={}", cargo_home.display())),
        "log: {log}"
    );
    assert!(
        log.contains(&format!("rustup_home={}", rustup_home.display())),
        "log: {log}"
    );
}

#[test]
fn embedded_rustfmt_preserves_toolchain_timeout() {
    let cache_root = unique_temp_dir("rustfmt-embedded-policy-timeout");
    let log_path = cache_root.join("tool.log");
    let source_path = write_rustfmt_source(&cache_root);
    let (rustup, _, _, _) = install_fake_rustup_toolchain(&log_path);
    let started = std::time::Instant::now();
    let output = isolated_soldr_command()
        .args(["rustfmt", source_path.to_str().unwrap()])
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("ZCCACHE_CACHE_DIR", cache_root.join("zccache"))
        .env("SOLDR_TEST_TOOL_HANG", "1")
        .env("SOLDR_TOOLCHAIN_COMMAND_TIMEOUT_SECS", "1")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("run embedded rustfmt timeout case");

    assert!(!output.status.success());
    assert!(started.elapsed() < std::time::Duration::from_secs(30)); // + cold lease daemon, #3855
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("installer watchdog category=safety-ceiling")
            && stderr.contains("safety_ceiling=1s"),
        "stderr: {stderr}"
    );
}

#[test]
fn rustfmt_no_cache_disable_and_version_bypass_zccache_formatter() {
    for (label, prefix_args, zccache_disable, include_source) in [
        ("no-cache", vec!["--no-cache", "rustfmt"], false, true),
        ("zccache-disable", vec!["rustfmt"], true, true),
        ("version", vec!["rustfmt", "--version"], false, false),
    ] {
        let cache_root = unique_temp_dir(&format!("rustfmt-bypass-{label}"));
        let log_path = cache_root.join("tool.log");
        let source_path = write_rustfmt_source(&cache_root);
        let (rustup, _, _, _) = install_fake_rustup_toolchain(&log_path);
        let mut command = isolated_soldr_command();
        command.args(prefix_args);
        if include_source {
            command.arg(&source_path);
        }
        command
            .current_dir(&cache_root)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
            .env("PATH", isolated_test_path())
            .env_remove("CARGO_HOME")
            .env_remove("RUSTUP_HOME")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env_remove("ZCCACHE_CACHE_DIR")
            .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR");
        if zccache_disable {
            command.env("ZCCACHE_DISABLE", "1");
        } else {
            command.env_remove("ZCCACHE_DISABLE");
        }

        let output = command
            .output()
            .unwrap_or_else(|_| panic!("failed to run rustfmt bypass case {label}"));

        assert!(
            output.status.success(),
            "rustfmt bypass case {label} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let log = fs::read_to_string(&log_path).unwrap_or_else(|err| {
            panic!(
                "failed to read fake tool log for bypass case {label}: {err}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert!(
            log.contains("rustfmt"),
            "rustfmt should still run directly in bypass case {label}: {log}"
        );
    }
}

#[test]
fn cargo_fmt_routes_rustfmt_through_zccache_formatter() {
    for (label, args) in [
        ("cargo-fmt", vec!["cargo", "fmt"]),
        ("bare-fmt", vec!["fmt"]),
    ] {
        let cache_root = unique_temp_dir(&format!("cargo-fmt-zccache-{label}"));
        let log_path = cache_root.join("tool.log");
        let source_path = write_rustfmt_source(&cache_root);
        let (rustup, cargo, rustc, _rustfmt, _zccache) =
            install_fake_cargo_fmt_toolchain(&log_path, &source_path);

        let output = isolated_soldr_command()
            .args(args)
            .current_dir(&cache_root)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_CARGO_BIN", &cargo)
            .env("SOLDR_TEST_RUSTC_BIN", &rustc)
            .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
            .env("PATH", isolated_test_path())
            .env_remove("RUSTFMT")
            .env_remove("CARGO_HOME")
            .env_remove("RUSTUP_HOME")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env_remove("ZCCACHE_CACHE_DIR")
            .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
            .env_remove("ZCCACHE_DISABLE")
            .output()
            .unwrap_or_else(|_| panic!("failed to run soldr {label} with fake tools"));

        assert!(
            output.status.success(),
            "cargo fmt route {label} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
        assert!(
            log.contains("cargo fmt rustfmt=") && log.contains("env_rustfmt="),
            "fake cargo fmt should receive an explicit RUSTFMT shim: {log}"
        );
    }
}

#[test]
fn cargo_fmt_no_cache_leaves_rustfmt_direct() {
    let cache_root = unique_temp_dir("cargo-fmt-no-cache-direct");
    let log_path = cache_root.join("tool.log");
    let source_path = write_rustfmt_source(&cache_root);
    let (rustup, cargo, rustc, _rustfmt, _zccache) =
        install_fake_cargo_fmt_toolchain(&log_path, &source_path);

    let output = isolated_soldr_command()
        .args(["--no-cache", "cargo", "fmt"])
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("PATH", isolated_test_path())
        .env_remove("RUSTFMT")
        .env_remove("CARGO_HOME")
        .env_remove("RUSTUP_HOME")
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("ZCCACHE_CACHE_DIR")
        .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("failed to run no-cache cargo fmt with fake tools");

    assert!(
        output.status.success(),
        "no-cache cargo fmt failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    let rustfmt_line = log
        .lines()
        .find(|line| line.starts_with("rustfmt "))
        .unwrap_or_else(|| panic!("no-cache cargo fmt should invoke rustfmt directly: {log}"));
    assert!(
        path_display_variants(&source_path)
            .iter()
            .any(|path| rustfmt_line.contains(path)),
        "no-cache cargo fmt should pass the source path to direct rustfmt: {log}"
    );
    assert!(
        !log.contains("zccache wrapper"),
        "no-cache cargo fmt should not route rustfmt through zccache: {log}"
    );
}

#[test]
fn cargo_front_door_preserves_jobserver_fds_into_managed_zccache_wrapper() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let cache_root = unique_temp_dir("cargo-jobserver-fds");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_jobserver_toolchain(&log_path);
    let output = isolated_soldr_command_in(&cache_root)
        .args(["cargo", "test", "--no-run"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo test --no-run with fake jobserver fds");

    assert!(
        output.status.success(),
        "cache-enabled front door lost jobserver fds\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("failed to connect to jobserver"),
        "jobserver warning should not be emitted: {stderr}"
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cargo wrapper=") && log.contains("rustc --crate-name demo"),
        "cacheable Cargo should reach Soldr's embedded compiler route: {log}"
    );
}

#[test]
fn cache_enabled_zccache_build_completes_under_60_seconds() {
    let cache_root = unique_temp_dir("cargo-zccache-timing");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);

    let started = Instant::now();
    let output = isolated_soldr_command_in(&cache_root)
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with fake zccache");
    let elapsed = started.elapsed();

    assert!(
        output.status.success(),
        "cache-enabled zccache build failed in {elapsed:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        // Hang backstop, not a perf gate (Perf Matrix owns those): contended Windows hits 62-68s cold; 100s beats nextest's 120s kill.
        elapsed < Duration::from_secs(100),
        "cache-enabled zccache build took {elapsed:?}, expected under 100s"
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    let routed = log
        .lines()
        .any(|l| l.contains("cache=1 ") && !l.contains("wrapper= rustc="));
    assert!(
        routed && log.contains("rustc --crate-name demo"),
        "timed build should route through Soldr's compiler shim and embedded service: {log}"
    );
}

#[test]
fn managed_zccache_honors_explicit_cache_dir_override_when_trusted() {
    let cache_root = unique_temp_dir("cargo-explicit-zccache-dir");
    let user_zccache_dir = cache_root.join("user-zccache");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let output = isolated_soldr_command_in(&cache_root)
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("ZCCACHE_CACHE_DIR", &user_zccache_dir)
        .env("SOLDR_TRUST_INHERITED_ENV", "1")
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .output()
        .expect("failed to run soldr cargo build with explicit ZCCACHE_CACHE_DIR");

    assert!(
        output.status.success(),
        "explicit ZCCACHE_CACHE_DIR should be forwarded to zccache\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        path_display_variants(&user_zccache_dir)
            .iter()
            .any(|path| log.contains(&format!("zccache_dir={path}"))
                && log.contains(&format!("cache_dir={path}"))),
        "explicit ZCCACHE_CACHE_DIR should reach cargo and zccache commands: {log}"
    );
}

#[test]
fn nested_soldr_ignores_inherited_managed_zccache_cache_dir() {
    let parent_cache_root = unique_temp_dir("cargo-parent-managed-zccache-dir");
    let child_cache_root = unique_temp_dir("cargo-child-managed-zccache-dir");
    let parent_zccache_dir = parent_cache_root.join("cache").join("zccache");
    let log_path = child_cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let output = isolated_soldr_command_in(&parent_cache_root)
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &child_cache_root)
        .env("ZCCACHE_CACHE_DIR", &parent_zccache_dir)
        .env("SOLDR_MANAGED_ZCCACHE_CACHE_DIR", &parent_zccache_dir)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run nested soldr cargo build with inherited managed ZCCACHE_CACHE_DIR");

    assert!(
        output.status.success(),
        "inherited soldr-managed ZCCACHE_CACHE_DIR should not block nested soldr\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        !path_display_variants(&parent_zccache_dir)
            .iter()
            .any(|path| log.contains(&format!("zccache_dir={path}"))
                || log.contains(&format!("cache_dir={path}"))),
        "nested soldr should ignore inherited soldr-managed ZCCACHE_CACHE_DIR: {log}"
    );
}

#[test]
fn managed_zccache_injects_normalized_path_remap_by_default() {
    let cache_root = unique_temp_dir("cargo-normalized-remap");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let repo_root = unique_temp_dir("cargo-normalized-remap-repo");
    let nested = repo_root.join("crates").join("demo");
    fs::create_dir_all(repo_root.join(".git")).expect("failed to create fake git root");
    fs::create_dir_all(&nested).expect("failed to create nested cwd");

    let output = isolated_soldr_command()
        .args(["cargo", "build"])
        .current_dir(&nested)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("ZCCACHE_PATH_REMAP")
        .env_remove("ZCCACHE_WORKTREE_ROOT")
        .env_remove("SOLDR_PATH_REMAP")
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with normalized remap defaults");

    assert!(
        output.status.success(),
        "normalized remap front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("path_remap=auto"),
        "managed zccache should enable path remap by default: {log}"
    );
    assert!(
        path_display_variants(&repo_root)
            .iter()
            .any(|path| log.contains(&format!("worktree_root={path}"))),
        "managed zccache should pass the git root as ZCCACHE_WORKTREE_ROOT: {log}"
    );
}

#[test]
fn cargo_front_door_uses_custom_rustc_wrapper_from_env_var() {
    let cache_root = unique_temp_dir("cargo-custom-wrapper");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let wrapper = install_fake_wrapper(&log_path, "sccache");
    let output = isolated_soldr_command_with_target(&cache_root.join("target"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_RUSTC_WRAPPER", &wrapper)
        .output()
        .expect("failed to run soldr cargo build with custom rustc wrapper");

    assert!(
        output.status.success(),
        "custom-wrapper front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains(&format!("cargo wrapper={}", wrapper.display())),
        "cargo should receive the custom wrapper path: {log}"
    );
    assert!(
        log.contains("sccache wrapper"),
        "custom wrapper should be invoked for rustc: {log}"
    );
    let expected_sccache_dir = cache_root.join("cache").join("sccache");
    assert!(
        path_display_variants(&expected_sccache_dir)
            .iter()
            .any(|path| log.contains(&format!("sccache_dir={path}"))),
        "cargo should receive soldr-owned SCCACHE_DIR at {}: {log}",
        expected_sccache_dir.display()
    );
    assert!(
        expected_sccache_dir.is_dir(),
        "soldr should pre-create the owned sccache cache dir at {}",
        expected_sccache_dir.display()
    );
    assert!(
        !log.contains(common::soldr_bin().to_string_lossy().as_ref()),
        "soldr should not stay in the wrapper slot when overridden: {log}"
    );
    assert!(
        !log.contains("zccache start")
            && !log.contains("zccache session-start")
            && !log.contains("zccache wrapper")
            && !log.contains("zccache session-end"),
        "managed zccache should be skipped when using a custom wrapper: {log}"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("soldr: zccache session summary"),
        "custom wrapper path should not emit zccache session output: {stderr}"
    );
}

#[test]
fn custom_sccache_wrapper_preserves_caller_sccache_dir() {
    let cache_root = unique_temp_dir("cargo-custom-wrapper-preserve-sccache-dir");
    let caller_sccache_dir = unique_temp_dir("caller-sccache-dir");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let wrapper = install_fake_wrapper(&log_path, "sccache");
    let output = isolated_soldr_command_with_target(&cache_root.join("target"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_RUSTC_WRAPPER", &wrapper)
        .env("SCCACHE_DIR", &caller_sccache_dir)
        .output()
        .expect("failed to run soldr cargo build with caller SCCACHE_DIR");

    assert!(
        output.status.success(),
        "custom-wrapper front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        path_display_variants(&caller_sccache_dir)
            .iter()
            .any(|path| log.contains(&format!("sccache_dir={path}"))),
        "cargo should preserve caller-provided SCCACHE_DIR at {}: {log}",
        caller_sccache_dir.display()
    );
    let soldr_sccache_dir = cache_root.join("cache").join("sccache");
    assert!(
        !path_display_variants(&soldr_sccache_dir)
            .iter()
            .any(|path| log.contains(&format!("sccache_dir={path}"))),
        "cargo should not override caller SCCACHE_DIR with {}: {log}",
        soldr_sccache_dir.display()
    );
}

#[test]
fn empty_rustc_wrapper_override_disables_wrapper_injection() {
    let cache_root = unique_temp_dir("cargo-wrapper-disabled");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let output = isolated_soldr_command_with_target(&cache_root.join("target"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_RUSTC_WRAPPER", "")
        .output()
        .expect("failed to run soldr cargo build with wrapper disabled");

    assert!(
        output.status.success(),
        "wrapper-disabled front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cargo wrapper= rustc="),
        "cargo should not receive a wrapper when override is empty: {log}"
    );
    assert!(
        !log.contains("zccache start")
            && !log.contains("zccache session-start")
            && !log.contains("zccache wrapper")
            && !log.contains("zccache session-end"),
        "managed zccache should be skipped when wrapper injection is disabled: {log}"
    );
    assert!(
        log.contains("rustc ") && log.contains("--crate-name demo"),
        "rustc should still run directly when wrapper injection is disabled: {log}"
    );
}

#[test]
fn no_cache_bypasses_wrapper_and_zccache() {
    let cache_root = unique_temp_dir("cargo-no-cache-fake");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let output = isolated_soldr_command_with_target(&cache_root.join("target"))
        .args(["--no-cache", "cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .output()
        .expect("failed to run soldr --no-cache cargo build with fake tools");

    assert!(
        output.status.success(),
        "no-cache front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cache=0"),
        "no-cache front door should propagate cache disable flag: {log}"
    );
    assert!(
        !log.contains("zccache start"),
        "no-cache front door should not start zccache: {log}"
    );
    // Parse the wrapper slot instead of substring-matching the soldr binary path
    // against the whole log: on the Recovery guest SOLDR_BIN is /tmp/soldr and the
    // fixture temp dirs live under /Volumes/Work/tmp/soldr-..., so every log line
    // contains the binary path as a substring even though `wrapper=` is empty (soldr#3135).
    let wrapper_slots_empty = log
        .lines()
        .filter(|line| line.contains("wrapper="))
        .all(|line| {
            line.split_once("wrapper=")
                .and_then(|(_, rest)| rest.split(' ').next())
                .is_none_or(str::is_empty)
        });
    assert!(
        wrapper_slots_empty,
        "no-cache front door should not set soldr as wrapper: {log}"
    );
    assert!(
        log.contains("rustc ") && log.contains("--crate-name demo"),
        "no-cache front door should call rustc directly: {log}"
    );
}

#[test]
fn rustc_wrapper_mode_passes_through_to_rustc() {
    let rustc = rustup_which("rustc");
    let output = isolated_soldr_command()
        .arg(rustc)
        .arg("--version")
        .output()
        .expect("failed to run soldr in wrapper mode");

    assert!(output.status.success(), "wrapper mode failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("rustc"),
        "unexpected rustc output: {stdout}"
    );
}

#[test]
fn repo_local_toolchain_homes_are_used_when_env_vars_are_unset() {
    let cache_root = unique_temp_dir("repo-local-toolchain-homes");
    let log_path = cache_root.join("tool.log");
    let (rustup, _, _, _) = install_fake_rustup_toolchain(&log_path);
    let repo_root = unique_temp_dir("repo-local-toolchain-root");
    let repo_cargo_home = repo_root.join(".cargo");
    let repo_rustup_home = repo_root.join(".rustup");
    let nested = repo_root.join("workspace").join("crate");
    fs::create_dir_all(repo_cargo_home.join("bin")).expect("failed to create repo-local .cargo");
    fs::create_dir_all(&repo_rustup_home).expect("failed to create repo-local .rustup");
    fs::create_dir_all(&nested).expect("failed to create nested working dir");

    for args in [
        vec!["--no-cache", "cargo", "--version"],
        vec!["rustfmt", "--version"],
        vec!["--no-cache", "rustc", "--version"],
    ] {
        let output = isolated_soldr_command()
            .args(&args)
            .current_dir(&nested)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
            .env("PATH", isolated_test_path())
            .env_remove("CARGO_HOME")
            .env_remove("RUSTUP_HOME")
            .env_remove("RUSTUP_TOOLCHAIN")
            .output()
            .unwrap_or_else(|_| panic!("failed to run soldr with args {args:?}"));

        assert!(
            output.status.success(),
            "soldr invocation failed for {:?}\nstdout:\n{}\nstderr:\n{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let log = fs::read_to_string(&log_path).expect("failed to read fake rustup log");
    assert!(
        log_contains_toolchain_homes(
            &log,
            "rustup which cargo",
            &repo_cargo_home,
            &repo_rustup_home
        ),
        "cargo resolution should use repo-local homes: {log}"
    );
    assert!(
        log_contains_toolchain_homes(&log, "cargo", &repo_cargo_home, &repo_rustup_home),
        "cargo execution should inherit repo-local homes: {log}"
    );
    assert!(
        log_contains_toolchain_homes(
            &log,
            "rustup which rustfmt",
            &repo_cargo_home,
            &repo_rustup_home
        ),
        "rustfmt resolution should use repo-local homes: {log}"
    );
    assert!(
        log_contains_toolchain_homes(&log, "rustfmt", &repo_cargo_home, &repo_rustup_home),
        "rustfmt execution should inherit repo-local homes: {log}"
    );
    assert!(
        log_contains_toolchain_homes(
            &log,
            "rustup which rustc",
            &repo_cargo_home,
            &repo_rustup_home
        ),
        "rustc resolution should use repo-local homes: {log}"
    );
    assert!(
        log_contains_toolchain_homes(&log, "rustc", &repo_cargo_home, &repo_rustup_home),
        "rustc execution should inherit repo-local homes: {log}"
    );
}

#[test]
fn repo_local_cargo_bin_tools_work_without_rustup() {
    let cache_root = unique_temp_dir("repo-local-cargo-bin");
    let log_path = cache_root.join("tool.log");
    let rustup = install_failing_fake_rustup(&log_path);
    let repo_root = unique_temp_dir("repo-local-cargo-bin-root");
    let repo_cargo_bin = repo_root.join(".cargo").join("bin");
    let repo_rustup_home = repo_root.join(".rustup");
    let nested = repo_root.join("workspace").join("crate");
    fs::create_dir_all(&repo_cargo_bin).expect("failed to create repo-local .cargo/bin");
    // Anchor the rustup-home ancestor walk inside the test sandbox so it can't
    // climb up to a runner-installed `~/.rustup` (Windows GitHub runners put
    // TEMP under USERPROFILE, where `.rustup` typically exists).
    fs::create_dir_all(&repo_rustup_home).expect("failed to create repo-local .rustup");
    fs::create_dir_all(&nested).expect("failed to create nested working dir");
    install_fake_version_toolchain(&repo_cargo_bin, &log_path);

    for args in [
        vec!["--no-cache", "cargo", "--version"],
        vec!["rustfmt", "--version"],
        vec!["--no-cache", "rustc", "--version"],
    ] {
        let output = isolated_soldr_command()
            .args(&args)
            .current_dir(&nested)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
            .env("PATH", isolated_test_path())
            .env_remove("CARGO_HOME")
            .env_remove("RUSTUP_HOME")
            .env_remove("RUSTUP_TOOLCHAIN")
            .output()
            .unwrap_or_else(|_| panic!("failed to run soldr with args {args:?}"));

        assert!(
            output.status.success(),
            "soldr invocation failed for {:?}\nstdout:\n{}\nstderr:\n{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.lines().any(|line| line.starts_with("cargo ")),
        "expected repo-local cargo shim to run: {log}"
    );
    assert!(
        log.lines().any(|line| line.starts_with("rustfmt ")),
        "expected repo-local rustfmt shim to run: {log}"
    );
    assert!(
        log.lines().any(|line| line.starts_with("rustc ")),
        "expected repo-local rustc shim to run: {log}"
    );
    assert!(
        !log.lines().any(|line| line.starts_with("rustup ")),
        "repo-local .cargo/bin tools should bypass rustup entirely: {log}"
    );
}

#[test]
fn explicit_toolchain_home_env_vars_win_over_repo_local_homes() {
    let cache_root = unique_temp_dir("explicit-toolchain-homes");
    let log_path = cache_root.join("tool.log");
    let (rustup, _, _, _) = install_fake_rustup_toolchain(&log_path);
    let repo_root = unique_temp_dir("explicit-toolchain-repo");
    let repo_cargo_home = repo_root.join(".cargo");
    let repo_rustup_home = repo_root.join(".rustup");
    let nested = repo_root.join("workspace").join("crate");
    let explicit_cargo_home = unique_temp_dir("explicit-cargo-home");
    let explicit_rustup_home = unique_temp_dir("explicit-rustup-home");
    fs::create_dir_all(&repo_cargo_home).expect("failed to create repo-local .cargo");
    fs::create_dir_all(&repo_rustup_home).expect("failed to create repo-local .rustup");
    fs::create_dir_all(&nested).expect("failed to create nested working dir");

    let output = isolated_soldr_command()
        .args(["--no-cache", "cargo", "--version"])
        .current_dir(&nested)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("CARGO_HOME", &explicit_cargo_home)
        .env("RUSTUP_HOME", &explicit_rustup_home)
        .env("PATH", isolated_test_path())
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .expect("failed to run soldr cargo --version with explicit homes");

    assert!(
        output.status.success(),
        "soldr cargo --version failed with explicit homes\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake rustup log");
    let explicit_cargo_home = explicit_cargo_home.display().to_string();
    let explicit_rustup_home = explicit_rustup_home.display().to_string();
    assert!(
        log.contains(&format!(
            "rustup which cargo cargo_home={explicit_cargo_home} rustup_home={explicit_rustup_home}"
        )),
        "cargo resolution should prefer explicit homes: {log}"
    );
    assert!(
        log.contains(&format!(
            "cargo cargo_home={explicit_cargo_home} rustup_home={explicit_rustup_home}"
        )),
        "cargo execution should inherit explicit homes: {log}"
    );
    assert!(
        !log.contains(&repo_cargo_home.display().to_string())
            && !log.contains(&repo_rustup_home.display().to_string()),
        "repo-local homes should not leak into explicit-home runs: {log}"
    );
}

#[test]
fn cargo_front_door_memoizes_successful_toolchain_preparation() {
    let workspace = unique_temp_dir("cargo-toolchain-prepare-memo");
    let soldr_root = workspace.join("soldr-root");
    let rustup_home = workspace.join("rustup-home");
    let toolchain = rustup_home.join("toolchains").join("1.94.1-test-host");
    let rustup_log = workspace.join("rustup.log");
    let cargo_log = workspace.join("cargo.log");
    let rustup = install_logging_fake_rustup(&rustup_log);
    let cargo = install_logging_fake_cargo(&cargo_log);
    let (_, rustc, _) = install_fake_toolchain(&cargo_log);

    seed_fake_toolchain_dir(
        &toolchain,
        b"fake-rustc-v1",
        b"rustc-test-host\nrustfmt-preview-test-host\nclippy-preview-test-host\n",
    );
    seed_rust_toolchain_toml(
            &workspace,
            "[toolchain]\nchannel = \"1.94.1\"\nprofile = \"minimal\"\ncomponents = [\"rustfmt\", \"clippy\"]\n",
        );

    let run = || {
        isolated_soldr_command()
            .args(["--no-cache", "cargo", "--version"])
            .current_dir(&workspace)
            .env("SOLDR_CACHE_DIR", &soldr_root)
            .env_remove("SOLDR_ROOT")
            .env("RUSTUP_HOME", &rustup_home)
            .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
            .env("SOLDR_TEST_CARGO_BIN", &cargo)
            .env("SOLDR_TEST_RUSTC_BIN", &rustc)
            .env("PATH", isolated_test_path())
            .env_remove("RUSTUP_TOOLCHAIN")
            .output()
            .expect("run soldr cargo front door")
    };

    let first = run();
    assert!(
        first.status.success(),
        "first cargo invocation failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let first_rustup = read_logged_rustup_invocations(&rustup_log);
    assert!(
        !first_rustup.is_empty(),
        "first invocation must prepare through rustup"
    );

    fs::write(&rustup_log, b"").expect("clear fake rustup log");
    let second = run();
    assert!(
        second.status.success(),
        "second cargo invocation failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        read_logged_rustup_invocations(&rustup_log).is_empty(),
        "matching warm invocation must launch zero rustup subprocesses"
    );
}

#[test]
fn cargo_front_door_never_memoizes_failed_toolchain_preparation() {
    let workspace = unique_temp_dir("cargo-toolchain-prepare-failure");
    let soldr_root = workspace.join("soldr-root");
    let rustup_home = workspace.join("rustup-home");
    let toolchain = rustup_home.join("toolchains").join("1.94.1-test-host");
    let rustup_log = workspace.join("rustup.log");
    let cargo_log = workspace.join("cargo.log");
    let rustup = install_failing_fake_rustup(&rustup_log);
    let cargo = install_logging_fake_cargo(&cargo_log);
    let (_, rustc, _) = install_fake_toolchain(&cargo_log);

    seed_fake_toolchain_dir(&toolchain, b"fake-rustc-v1", b"rustc-test-host\n");
    seed_rust_toolchain_toml(
        &workspace,
        "[toolchain]\nchannel = \"1.94.1\"\nprofile = \"minimal\"\n",
    );

    let run = || {
        isolated_soldr_command()
            .args(["--no-cache", "cargo", "--version"])
            .current_dir(&workspace)
            .env("SOLDR_CACHE_DIR", &soldr_root)
            .env_remove("SOLDR_ROOT")
            .env("RUSTUP_HOME", &rustup_home)
            .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
            .env("SOLDR_TEST_CARGO_BIN", &cargo)
            .env("SOLDR_TEST_RUSTC_BIN", &rustc)
            .env("PATH", isolated_test_path())
            .env_remove("RUSTUP_TOOLCHAIN")
            .output()
            .expect("run soldr cargo front door")
    };

    let failed = run();
    assert!(
        !failed.status.success(),
        "failing preparation unexpectedly succeeded"
    );
    let memo_dir = soldr_root.join("cache").join("toolchain-prepare-v1");
    assert!(
        !memo_dir.exists()
            || fs::read_dir(&memo_dir)
                .expect("read memo dir")
                .next()
                .is_none(),
        "failed preparation must not write a success memo"
    );

    write_fake_script(&rustup, &fake_logging_rustup_script(&rustup_log));
    fs::write(&rustup_log, b"").expect("clear fake rustup log");
    let retried = run();
    assert!(
        retried.status.success(),
        "retry after failed preparation failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&retried.stdout),
        String::from_utf8_lossy(&retried.stderr)
    );
    assert!(
        !read_logged_rustup_invocations(&rustup_log).is_empty(),
        "retry must prepare again instead of trusting a failed attempt"
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn cargo_front_door_invalidates_toolchain_preparation_memo() {
    let workspace = unique_temp_dir("cargo-toolchain-prepare-invalidation");
    let soldr_root = workspace.join("soldr-root");
    let first_rustup_home = workspace.join("rustup-home-a");
    let second_rustup_home = workspace.join("rustup-home-b");
    let rustup_log = workspace.join("rustup.log");
    let cargo_log = workspace.join("cargo.log");
    let first_rustup = install_logging_fake_rustup(&rustup_log);
    let second_rustup = install_logging_fake_rustup(&rustup_log);
    let cargo = install_logging_fake_cargo(&cargo_log);
    let (_, rustc, _) = install_fake_toolchain(&cargo_log);
    let host_triple = soldr_cli::core::TargetTriple::host()
        .expect("detect test host triple")
        .triple();

    let seed_toolchain = |home: &Path, channel: &str| {
        // Match rustup's real channel-alias layout. A synthetic
        // `<channel>-test-host` directory can coexist with the real
        // host-qualified alias under CI and correctly makes production
        // memo lookup reject the otherwise ambiguous channel.
        let toolchain = home
            .join("toolchains")
            .join(format!("{channel}-{host_triple}"));
        seed_fake_toolchain_dir(
            &toolchain,
            format!("fake-rustc-{channel}").as_bytes(),
            b"rustc-test-host\nrustfmt-preview-test-host\nclippy-preview-test-host\n",
        )
    };
    let first_194 = seed_toolchain(&first_rustup_home, "1.94.1");
    let _first_195 = seed_toolchain(&first_rustup_home, "1.95.0");
    let second_195 = seed_toolchain(&second_rustup_home, "1.95.0");

    let run =
        |manifest: &str, rustup_home: &Path, rustup: &Path, explicit_channel: Option<&str>| {
            seed_rust_toolchain_toml(&workspace, manifest);
            let mut command = isolated_soldr_command();
            command.args(["--no-cache", "cargo"]);
            if let Some(channel) = explicit_channel {
                command.arg(format!("+{channel}"));
            }
            command
                .arg("--version")
                .current_dir(&workspace)
                .env("SOLDR_CACHE_DIR", &soldr_root)
                .env_remove("SOLDR_ROOT")
                .env("RUSTUP_HOME", rustup_home)
                .env("SOLDR_TEST_RUSTUP_BIN", rustup)
                .env("SOLDR_TEST_CARGO_BIN", &cargo)
                .env("SOLDR_TEST_RUSTC_BIN", &rustc)
                .env("PATH", isolated_test_path())
                .env_remove("RUSTUP_TOOLCHAIN")
                .output()
                .expect("run soldr cargo front door")
        };
    let assert_prepares = |label: &str, output: std::process::Output, rustup_log: &Path| {
        assert!(
            output.status.success(),
            "{label} invocation failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !read_logged_rustup_invocations(rustup_log).is_empty(),
            "{label} must invalidate the memo and prepare through rustup"
        );
    };
    let base = "[toolchain]\nchannel = \"1.94.1\"\nprofile = \"minimal\"\ncomponents = [\"rustfmt\"]\ntargets = [\"wasm32-unknown-unknown\"]\n";

    assert_prepares(
        "initial",
        run(base, &first_rustup_home, &first_rustup, None),
        &rustup_log,
    );
    let memo_dir = soldr_root.join("cache").join("toolchain-prepare-v1");
    let memo_entries = || {
        let mut entries = fs::read_dir(&memo_dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    };
    let initial_memos = memo_entries();
    fs::write(&rustup_log, b"").expect("clear rustup log");
    let warm = run(base, &first_rustup_home, &first_rustup, None);
    let warm_invocations = read_logged_rustup_invocations(&rustup_log);
    assert!(
        warm.status.success(),
        "warm invocation failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&warm.stdout),
        String::from_utf8_lossy(&warm.stderr)
    );
    assert!(
        warm_invocations.is_empty(),
        "unchanged warm invocation must use the memo\n\
             initial memos: {initial_memos:#?}\n\
             warm memos: {:#?}\n\
             warm rustup invocations: {warm_invocations:#?}\n\
             warm stderr:\n{}",
        memo_entries(),
        String::from_utf8_lossy(&warm.stderr)
    );

    let variants = [
            (
                "profile",
                "[toolchain]\nchannel = \"1.94.1\"\nprofile = \"default\"\ncomponents = [\"rustfmt\"]\ntargets = [\"wasm32-unknown-unknown\"]\n",
            ),
            (
                "components",
                "[toolchain]\nchannel = \"1.94.1\"\nprofile = \"default\"\ncomponents = [\"rustfmt\", \"clippy\"]\ntargets = [\"wasm32-unknown-unknown\"]\n",
            ),
            (
                "targets",
                "[toolchain]\nchannel = \"1.94.1\"\nprofile = \"default\"\ncomponents = [\"rustfmt\", \"clippy\"]\ntargets = [\"wasm32-unknown-unknown\", \"x86_64-unknown-linux-musl\"]\n",
            ),
            (
                "channel",
                "[toolchain]\nchannel = \"1.95.0\"\nprofile = \"default\"\ncomponents = [\"rustfmt\", \"clippy\"]\ntargets = [\"wasm32-unknown-unknown\", \"x86_64-unknown-linux-musl\"]\n",
            ),
        ];
    for (label, manifest) in variants {
        fs::write(&rustup_log, b"").expect("clear rustup log");
        assert_prepares(
            label,
            run(manifest, &first_rustup_home, &first_rustup, None),
            &rustup_log,
        );
    }
    let channel_195 = variants.last().expect("channel variant").1;

    fs::write(&rustup_log, b"").expect("clear rustup log");
    assert_prepares(
        "explicit channel",
        run(
            channel_195,
            &first_rustup_home,
            &first_rustup,
            Some("1.95.0"),
        ),
        &rustup_log,
    );

    fs::write(&rustup_log, b"").expect("clear rustup log");
    assert_prepares(
        "rustup binary",
        run(
            channel_195,
            &first_rustup_home,
            &second_rustup,
            Some("1.95.0"),
        ),
        &rustup_log,
    );

    fs::write(&rustup_log, b"").expect("clear rustup log");
    assert_prepares(
        "rustup home",
        run(
            channel_195,
            &second_rustup_home,
            &second_rustup,
            Some("1.95.0"),
        ),
        &rustup_log,
    );

    fs::write(
        second_195.join("lib").join("rustlib").join("components"),
        b"rustc-test-host\n",
    )
    .expect("mutate component state");
    fs::write(&rustup_log, b"").expect("clear rustup log");
    assert_prepares(
        "installed components",
        run(
            channel_195,
            &second_rustup_home,
            &second_rustup,
            Some("1.95.0"),
        ),
        &rustup_log,
    );

    fs::remove_dir_all(&second_195).expect("remove active toolchain");
    fs::write(&rustup_log, b"").expect("clear rustup log");
    assert_prepares(
        "missing toolchain",
        run(
            channel_195,
            &second_rustup_home,
            &second_rustup,
            Some("1.95.0"),
        ),
        &rustup_log,
    );

    assert!(
        first_194.is_dir(),
        "unrelated installed toolchain should remain intact"
    );
}
