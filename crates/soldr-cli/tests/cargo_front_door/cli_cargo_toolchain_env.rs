//! soldr#3452: `RUSTUP_TOOLCHAIN` from `rust-toolchain.toml` must reach the
//! cargo child, and through it every rustc and linker wrapper, unless the
//! caller already set it.

use crate::common::*;
use std::fs;
use std::path::Path;

/// A fake `cargo` that records the `RUSTUP_TOOLCHAIN` it was launched with and
/// answers `cargo metadata` with an empty workspace, which the front door asks
/// for before any build verb.
fn install_env_recording_cargo(out: &Path, workspace: &Path) -> std::path::PathBuf {
    let dir = unique_temp_dir("fake-cargo-toolchain-env");
    let cargo = fake_script_path(&dir, "cargo");
    let windows = matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    );
    let root = workspace.display().to_string().replace('\\', "/");
    let metadata = format!(
        "{{\"packages\":[],\"workspace_members\":[],\"workspace_default_members\":[],\
         \"resolve\":null,\"target_directory\":\"{root}/target\",\"version\":1,\
         \"workspace_root\":\"{root}\",\"metadata\":null}}"
    );
    let body = if windows {
        let escaped = metadata.replace('"', "\\\"");
        format!(
            "@echo off\n<nul set /p=%RUSTUP_TOOLCHAIN%>\"{}\"\n\
             <nul set /p=%PATH%>\"{}\"\n\
             if \"%1\"==\"metadata\" echo {escaped}\n",
            out.display(),
            out.with_extension("path").display()
        )
    } else {
        format!(
            "#!/bin/sh\nprintf '%s' \"${{RUSTUP_TOOLCHAIN-<unset>}}\" > '{}'\n\
             printf '%s' \"$PATH\" > '{}'\n\
             if [ \"$1\" = metadata ]; then printf '%s\\n' '{metadata}'; fi\n",
            out.display(),
            out.with_extension("path").display()
        )
    };
    write_fake_script(&cargo, &body);
    cargo
}

fn recorded_toolchain(caller_value: Option<&str>, verb: &str, subdir: Option<&str>) -> String {
    let recorded = front_door_run(caller_value, verb, subdir);
    fs::read_to_string(&recorded).expect("the fake cargo must have recorded its environment")
}

/// Run the front door once against the recording fake cargo and return the file
/// the fake cargo wrote its `RUSTUP_TOOLCHAIN` to; its search path is recorded
/// beside it with the extension `path`.
fn front_door_run(
    caller_value: Option<&str>,
    verb: &str,
    subdir: Option<&str>,
) -> std::path::PathBuf {
    let workspace = unique_temp_dir("cargo-toolchain-env");
    let soldr_root = workspace.join("soldr-root");
    let rustup_home = workspace.join("rustup-home");
    let rustup_log = workspace.join("rustup.log");
    let cargo_log = workspace.join("cargo.log");
    let recorded = workspace.join("recorded-toolchain.txt");
    let host = soldr_cli::core::TargetTriple::host()
        .expect("detect test host triple")
        .triple();
    seed_fake_toolchain_dir(
        &rustup_home
            .join("toolchains")
            .join(format!("1.94.1-{host}")),
        b"fake-rustc",
        b"rustc-test-host\n",
    );
    let rustup = install_logging_fake_rustup(&rustup_log);
    let cargo = install_env_recording_cargo(&recorded, &workspace);
    let (_, rustc, _) = install_fake_toolchain(&cargo_log);
    seed_rust_toolchain_toml(
        &workspace,
        "[toolchain]\nchannel = \"1.94.1\"\nprofile = \"minimal\"\n",
    );

    let cwd = match subdir {
        Some(subdir) => {
            let nested = workspace.join(subdir);
            fs::create_dir_all(&nested).expect("create nested working directory");
            nested
        }
        None => workspace.clone(),
    };
    let mut command = isolated_soldr_command();
    command
        .args(["--no-cache", "cargo", verb])
        .current_dir(&cwd)
        .env("SOLDR_CACHE_DIR", &soldr_root)
        .env_remove("SOLDR_ROOT")
        .env("RUSTUP_HOME", &rustup_home)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("CARGO_TARGET_DIR", workspace.join("target"))
        .env("PATH", isolated_test_path())
        // soldr#3823: the fake toolchain links nothing, and its fake rustc
        // cannot prove it ships lld, so the default fast linker would demand a
        // clang driver that a clang-less runner without the live catalogue
        // cannot provide. This test is about RUSTUP_TOOLCHAIN, not linking.
        .env("SOLDR_LINKER", "default")
        .env_remove("RUSTUP_TOOLCHAIN");
    if let Some(value) = caller_value {
        command.env("RUSTUP_TOOLCHAIN", value);
    }
    let output = command.output().expect("run soldr cargo front door");
    assert!(
        output.status.success(),
        "front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    recorded
}

/// The pinned channel reaches the child for the verbs that run rustc and a
/// linker, not only for `--version`.
#[test]
fn manifest_channel_reaches_the_cargo_child_for_test_and_build() {
    for verb in ["test", "build"] {
        let seen = recorded_toolchain(None, verb, None);
        assert!(
            seen.trim().starts_with("1.94.1"),
            "`soldr cargo {verb}` must export the pinned channel, saw {seen:?}"
        );
    }
}

#[test]
fn a_caller_set_toolchain_is_honored_not_overwritten() {
    let seen = recorded_toolchain(Some("caller-choice"), "test", None);
    assert_eq!(seen.trim(), "caller-choice");
}

/// soldr#3376: prepare once (memo written), confirm the warm run spawns no
/// rustup, then delete a declared target's library files while `components`
/// still claims them. Before the fix the memo still hit and the build failed
/// later with `E0463`; now the memo misses, the toolchain is checked again, and a
/// caller-selected `RUSTUP_HOME` (never repaired automatically) fails closed
/// naming the target and the recovery commands.
#[test]
fn a_target_whose_std_files_vanish_defeats_the_memo_and_fails_closed() {
    const TARGET: &str = "wasm32-unknown-unknown";
    let workspace = unique_temp_dir("cargo-toolchain-std-vanish");
    let soldr_root = workspace.join("soldr-root");
    let rustup_home = workspace.join("rustup-home");
    let rustup_log = workspace.join("rustup.log");
    let cargo_log = workspace.join("cargo.log");
    let host = soldr_cli::core::TargetTriple::host()
        .expect("detect test host triple")
        .triple();
    let toolchain = rustup_home
        .join("toolchains")
        .join(format!("1.94.1-{host}"));
    seed_fake_toolchain_dir(&toolchain, b"fake-rustc", b"rustc-test-host\n");
    let rustup = install_logging_fake_rustup(&rustup_log);
    let cargo = install_logging_fake_cargo(&cargo_log);
    let (_, rustc, _) = install_fake_toolchain(&cargo_log);
    seed_rust_toolchain_toml(
        &workspace,
        &format!(
            "[toolchain]\nchannel = \"1.94.1\"\nprofile = \"minimal\"\ntargets = [\"{TARGET}\"]\n"
        ),
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
            .env("CARGO_TARGET_DIR", workspace.join("target"))
            .env("PATH", isolated_test_path())
            .env_remove("RUSTUP_TOOLCHAIN")
            .output()
            .expect("run soldr cargo front door")
    };

    let first = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    fs::write(&rustup_log, b"").expect("clear rustup log");
    let warm = run();
    assert!(
        warm.status.success(),
        "{}",
        String::from_utf8_lossy(&warm.stderr)
    );
    assert!(
        read_logged_rustup_invocations(&rustup_log).is_empty(),
        "an unchanged toolchain must hit the memo and spawn no rustup"
    );

    fs::remove_dir_all(toolchain.join("lib").join("rustlib").join(TARGET))
        .expect("delete the target's library files");
    let broken = run();
    assert!(
        !broken.status.success(),
        "a claimed-but-missing target must not pass as a memo hit"
    );
    let stderr = String::from_utf8_lossy(&broken.stderr);
    assert!(
        stderr.contains(TARGET),
        "the error must name the target: {stderr}"
    );
    assert!(
        stderr.contains("target add") && stderr.contains("caller-selected"),
        "the error must give the recovery commands for a caller-selected home: {stderr}"
    );
}

/// soldr#3452: a build launched from a subdirectory of a pinned repo (the
/// Dylint layout: `dylints/<lint>/` under a root pin) is still a pinned build.
/// Reading only the current directory exported nothing for it.
#[test]
fn a_pin_in_an_ancestor_directory_is_exported_to_the_cargo_child() {
    let seen = recorded_toolchain(None, "test", Some("dylints/ban_something"));
    assert!(
        seen.trim().starts_with("1.94.1"),
        "a subdirectory build must export the ancestor's pinned channel, saw {seen:?}"
    );
}

/// soldr#3452 / #3394: a tool that runs `env -u RUSTUP_TOOLCHAIN cargo build`
/// from a temporary directory (`dylint_testing`) reaches whichever `cargo` comes
/// first on the child's search path. Soldr puts the real toolchain cargo there,
/// which is not rustup's proxy and so never re-exports the variable; the
/// driver's build script then died with "environment variable not found:
/// RUSTUP_TOOLCHAIN". A shim in front of it must restore the variable and reach
/// the real cargo.
#[test]
fn a_nested_cargo_on_the_child_search_path_still_sees_the_toolchain_after_env_u() {
    let recorded = front_door_run(None, "build", None);
    let search_path = fs::read_to_string(recorded.with_extension("path"))
        .expect("the fake cargo must have recorded its search path");
    let shim_dir = std::env::split_paths(&search_path)
        .find(|dir| dir.components().any(|c| c.as_os_str() == "cargo-shims"))
        .unwrap_or_else(|| {
            panic!("no cargo shim directory on the child's search path: {search_path}")
        });
    let windows = matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    );
    let shim = shim_dir.join(if windows { "cargo.cmd" } else { "cargo" });
    assert!(shim.is_file(), "missing shim {}", shim.display());

    fs::remove_file(&recorded).expect("clear the recorded value");
    let status = std::process::Command::new(&shim)
        .env_remove("RUSTUP_TOOLCHAIN")
        .arg("build")
        .status()
        .expect("run the shim");
    assert!(status.success());
    let seen = fs::read_to_string(&recorded).expect("the real cargo ran behind the shim");
    assert!(
        seen.trim().starts_with("1.94.1"),
        "the shim must restore the pinned toolchain for a nested cargo, saw {seen:?}"
    );
}
