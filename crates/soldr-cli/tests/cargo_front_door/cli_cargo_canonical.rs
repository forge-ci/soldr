//! Canonical Cargo policy runs before the compiling child (soldr#4195).
use crate::common::*;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture {
    root: PathBuf,
    log: PathBuf,
    command: Command,
}

impl Fixture {
    fn new(enforce: &str, unify: &str, channel: &str) -> Self {
        let root = unique_temp_dir("canonical-profile");
        for name in ["a", "b"] {
            let member = root.join(name);
            fs::create_dir_all(member.join("src")).unwrap();
            fs::write(
                member.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            )
            .unwrap();
            fs::write(member.join("src/lib.rs"), "").unwrap();
        }
        fs::write(root.join("Cargo.toml"), format!("[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n[workspace.metadata.soldr.cargo]\nenforce = \"{enforce}\"\nunify = \"{unify}\"\ntarget = \"implicit\"\n")).unwrap();
        let log = root.join("cargo.log");
        let metadata_path = root.join("metadata.json");
        let root_text = root.to_string_lossy().replace('\\', "/");
        let packages = ["a", "b"].iter().map(|name| serde_json::json!({
            "name": name, "id": name, "version": "0.1.0", "features": {},
            "targets": [], "dependencies": [], "manifest_path": root.join(name).join("Cargo.toml"),
        })).collect::<Vec<_>>();
        let metadata = serde_json::json!({
            "packages": packages, "workspace_members": ["a", "b"], "workspace_default_members": ["a", "b"],
            "resolve": null, "target_directory": format!("{root_text}/target"), "version": 1,
            "workspace_root": root_text, "metadata": null,
        });
        fs::write(&metadata_path, metadata.to_string()).unwrap();
        let tools = root.join("tools");
        fs::create_dir(&tools).unwrap();
        let cargo = fake_script_path(&tools, "cargo");
        let windows = matches!(
            soldr_platform::host::facts::os(),
            soldr_platform::host::facts::HostOs::Windows
        );
        let body = if windows {
            "@echo off\nif \"%1\"==\"metadata\" (\n  type \"%CANONICAL_METADATA%\"\n  exit /b 0\n)\necho %*>>\"%CANONICAL_LOG%\"\necho unify=%CARGO_RESOLVER_FEATURE_UNIFICATION%>>\"%CANONICAL_LOG%\"\nexit /b 0\n"
        } else {
            "#!/bin/sh\nif [ \"$1\" = metadata ]; then cat \"$CANONICAL_METADATA\"; exit 0; fi\nprintf '%s\\n' \"$*\" \"unify=${CARGO_RESOLVER_FEATURE_UNIFICATION-}\" >> \"$CANONICAL_LOG\"\nexit 0\n"
        };
        write_fake_script(&cargo, body);
        let (_, rustc, _) = install_fake_toolchain(&root.join("rustc.log"));
        let rustup_home = root.join("rustup-home");
        let host = soldr_cli::core::TargetTriple::host().unwrap().triple();
        seed_fake_toolchain_dir(
            &rustup_home
                .join("toolchains")
                .join(format!("{channel}-{host}")),
            b"fake-rustc",
            b"rustc-test-host\n",
        );
        seed_rust_toolchain_toml(
            &root,
            &format!("[toolchain]\nchannel = \"{channel}\"\nprofile = \"minimal\"\n"),
        );
        let rustup = install_logging_fake_rustup(&root.join("rustup.log"));
        let path = std::env::join_paths(
            std::iter::once(tools.clone()).chain(std::env::split_paths(&isolated_test_path())),
        )
        .unwrap();
        let mut command = isolated_soldr_command();
        command
            .current_dir(&root)
            .env("SOLDR_CACHE_DIR", root.join("soldr-cache"))
            .env("SOLDR_TEST_CARGO_BIN", &cargo)
            .env("SOLDR_TEST_RUSTC_BIN", &rustc)
            .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
            .env("RUSTUP_HOME", &rustup_home)
            .env("CANONICAL_METADATA", &metadata_path)
            .env("CANONICAL_LOG", &log)
            .env("CARGO_TARGET_DIR", root.join("target"))
            .env("PATH", path)
            .env("SOLDR_LINKER", "default")
            .env("NO_COLOR", "1")
            .env_remove("SOLDR_ROOT")
            .env_remove("SOLDR_CANONICAL")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("CARGO_BUILD_TARGET")
            .env_remove("CARGO_RESOLVER_FEATURE_UNIFICATION");
        Self { root, log, command }
    }

    fn run(&mut self, args: &[&str]) -> Output {
        let output = self.command.args(args).output().unwrap();
        assert!(output.status.code().is_some(), "{output:?}");
        output
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

#[test]
fn canonical_refuse_exits_two_without_spawning_compiling_cargo() {
    let mut fixture = Fixture::new("refuse", "off", "1.98.1");
    let output = fixture.run(&["cargo", "test", "-p", "a"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        fixture.log().is_empty(),
        "compiling child was spawned: {}",
        fixture.log()
    );
    assert!(
        stderr.contains("soldr: error: off-profile cargo invocation refused (soldr#4195)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("    differs in: packages (-p a vs --workspace)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("soldr cargo nextest run --workspace --all-targets -E 'package(a)'"),
        "{stderr}"
    );
}

#[test]
fn canonical_warn_spawns_original_selection_and_prints_header() {
    let mut fixture = Fixture::new("warn", "off", "1.98.1");
    let output = fixture.run(&["cargo", "test", "-p", "a"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("soldr: warning: off-profile cargo invocation (soldr#4195)"),
        "{stderr}"
    );
    assert!(fixture.log().contains("test -p a"), "{}", fixture.log());
    assert!(!fixture.log().contains("--workspace"), "{}", fixture.log());
}

#[test]
fn canonical_environment_override_off_spawns_and_refuse_blocks_shorthand() {
    let mut fixture = Fixture::new("refuse", "off", "1.98.1");
    fixture.command.env("SOLDR_CANONICAL", "off");
    let output = fixture.run(&["cargo", "test", "-p", "a"]);
    assert!(output.status.success(), "{output:?}");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("off-profile"));
    assert!(fixture.log().contains("test -p a"));
    let mut fixture = Fixture::new("warn", "off", "1.98.1");
    fixture.command.env("SOLDR_CANONICAL", "refuse");
    let output = fixture.run(&["test", "-p", "a"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(fixture.log().is_empty());
}

#[test]
fn canonical_unification_nightly_auto_and_explicit_workspace_reach_child() {
    for (unify, channel) in [("auto", "nightly-2026-08-15"), ("workspace", "1.98.1")] {
        let mut fixture = Fixture::new("refuse", unify, channel);
        let output = fixture.run(&["cargo", "test", "-p", "a"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(!stderr.contains("off-profile"), "{stderr}");
        assert!(
            fixture.log().contains("-Zfeature-unification test -p a"),
            "{}",
            fixture.log()
        );
        assert!(
            fixture.log().contains("unify=workspace"),
            "{}",
            fixture.log()
        );
    }
}

#[test]
fn canonical_unification_auto_stable_and_off_never_emit_unstable_flag() {
    for (unify, channel) in [("auto", "1.98.1"), ("off", "nightly-2026-08-15")] {
        let mut fixture = Fixture::new("refuse", unify, channel);
        let output = fixture.run(&["cargo", "test", "--workspace"]);
        assert!(output.status.success(), "{output:?}");
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("off-profile"),
            "{output:?}"
        );
        assert!(
            !fixture.log().contains("-Zfeature-unification"),
            "{}",
            fixture.log()
        );
        assert!(
            !fixture.log().contains("unify=workspace"),
            "{}",
            fixture.log()
        );
    }
}

#[test]
fn canonical_all_members_and_declared_alternate_are_accepted() {
    let mut fixture = Fixture::new("refuse", "off", "1.98.1");
    let output = fixture.run(&["cargo", "test", "-p", "a", "-p", "b"]);
    assert!(output.status.success(), "{output:?}");
    let mut fixture = Fixture::new("refuse", "off", "1.98.1");
    let manifest = fixture.root.join("Cargo.toml");
    let mut body = fs::read_to_string(&manifest).unwrap();
    body.push_str("\n[workspace.metadata.soldr.cargo.alternates.focused]\npackages = [\"a\"]\n");
    fs::write(manifest, body).unwrap();
    let output = fixture.run(&["cargo", "test", "-p", "a"]);
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn canonical_uncached_and_non_compiling_commands_bypass_policy() {
    for args in [
        vec!["--no-cache", "cargo", "test", "-p", "a"],
        vec!["cargo", "metadata", "--no-deps"],
        vec!["cargo", "tree"],
    ] {
        let mut fixture = Fixture::new("refuse", "off", "1.98.1");
        let output = fixture.run(&args);
        assert!(output.status.success(), "{output:?}");
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("off-profile"),
            "{output:?}"
        );
    }
}

#[test]
fn canonical_explicit_host_accepts_cargo_build_target_environment() {
    let mut fixture = Fixture::new("refuse", "off", "1.98.1");
    let manifest = fixture.root.join("Cargo.toml");
    let body = fs::read_to_string(&manifest)
        .unwrap()
        .replace("target = \"implicit\"", "target = \"explicit-host\"");
    fs::write(manifest, body).unwrap();
    fixture.command.env(
        "CARGO_BUILD_TARGET",
        soldr_cli::core::TargetTriple::host().unwrap().triple(),
    );
    let output = fixture.run(&["cargo", "test", "--workspace"]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("off-profile"),
        "{output:?}"
    );
}

#[test]
fn canonical_empty_wrapper_bypasses_enforcement() {
    let mut fixture = Fixture::new("refuse", "off", "1.98.1");
    fixture.command.env("RUSTC_WRAPPER", "");
    let output = fixture.run(&["cargo", "test", "-p", "a"]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("off-profile"),
        "{output:?}"
    );
    assert!(fixture.log().contains("test -p a"));
}

#[test]
fn canonical_uncached_invocations_keep_the_profile_unification_mode() {
    for empty_wrapper in [false, true] {
        let mut fixture = Fixture::new("refuse", "auto", "nightly-2026-08-15");
        let args = if empty_wrapper {
            fixture.command.env("RUSTC_WRAPPER", "");
            vec!["cargo", "test", "-p", "a"]
        } else {
            vec!["--no-cache", "cargo", "test", "-p", "a"]
        };
        let output = fixture.run(&args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(!stderr.contains("off-profile"), "{stderr}");
        assert!(
            fixture.log().contains("-Zfeature-unification test -p a"),
            "{}",
            fixture.log()
        );
        assert!(
            fixture.log().contains("unify=workspace"),
            "{}",
            fixture.log()
        );
    }
}
