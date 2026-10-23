//! Unit coverage split from `toolchain.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;
use std::ffi::{OsStr, OsString};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::remove_var(key);
        }
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

#[test]
fn toolchain_command_timeout_is_an_explicit_safety_ceiling() {
    let _lock = ENV_LOCK.lock().expect("env lock");
    {
        let _guard = EnvVarGuard::set(TOOLCHAIN_COMMAND_TIMEOUT_ENV_VAR, "23");
        assert_eq!(
            InstallerWatchdogConfig::from_env(TOOLCHAIN_COMMAND_TIMEOUT_ENV_VAR).safety_timeout,
            Duration::from_secs(23)
        );
    }
    for value in ["", "0", "-1", "abc"] {
        let _guard = EnvVarGuard::set(TOOLCHAIN_COMMAND_TIMEOUT_ENV_VAR, value);
        assert_eq!(
            InstallerWatchdogConfig::from_env(TOOLCHAIN_COMMAND_TIMEOUT_ENV_VAR).safety_timeout,
            Duration::from_secs(crate::core::DEFAULT_INSTALLER_SAFETY_TIMEOUT_SECS),
            "invalid override {value:?} should use default"
        );
    }
    let _guard = EnvVarGuard::remove(TOOLCHAIN_COMMAND_TIMEOUT_ENV_VAR);
    assert_eq!(
        InstallerWatchdogConfig::from_env(TOOLCHAIN_COMMAND_TIMEOUT_ENV_VAR).safety_timeout,
        Duration::from_secs(crate::core::DEFAULT_INSTALLER_SAFETY_TIMEOUT_SECS)
    );
}

#[test]
fn dylint_rustup_scope_replaces_every_toolchain_selector() {
    let args = vec![
        "+stable".to_string(),
        "run".to_string(),
        "nightly-old".to_string(),
        "cargo".to_string(),
        "--toolchain=stable".to_string(),
    ];
    assert_eq!(
        scope_rustup_args_to_dylint(&args, "nightly-2026-01-18"),
        vec!["run", "nightly-2026-01-18", "cargo"]
    );
    let component = vec![
        "component".to_string(),
        "add".to_string(),
        "rustc-dev".to_string(),
        "--toolchain".to_string(),
        "stable".to_string(),
    ];
    assert_eq!(
        scope_rustup_args_to_dylint(&component, "nightly-2026-01-18"),
        vec!["component", "add", "rustc-dev"]
    );
}

fn test_memo_key(root: &Path) -> CargoPrepareMemoKey {
    CargoPrepareMemoKey {
        schema_version: CARGO_PREPARE_MEMO_SCHEMA_VERSION,
        channel: "1.94.1".to_string(),
        explicit_channel: None,
        profile: Some("minimal".to_string()),
        components: vec!["clippy".to_string(), "rustfmt".to_string()],
        targets: vec!["wasm32-unknown-unknown".to_string()],
        rustup_home: root.join("rustup"),
        rustup_binary: root.join("rustup-bin"),
    }
}

/// Fake `libcore`/`libstd` so a fixture toolchain has the standard-library
/// files the memo identity now requires (soldr#3376).
fn write_fake_std(toolchain: &Path, triple: &str, with_std: bool) {
    let lib = toolchain
        .join("lib")
        .join("rustlib")
        .join(triple)
        .join("lib");
    std::fs::create_dir_all(&lib).expect("create target lib dir");
    std::fs::write(lib.join("libcore-0123456789abcdef.rlib"), b"core").expect("libcore");
    if with_std {
        std::fs::write(lib.join("libstd-0123456789abcdef.rlib"), b"std").expect("libstd");
    }
}

#[test]
fn cargo_prepare_memo_key_covers_every_requirement() {
    let root = tempfile::tempdir().expect("temp dir");
    let base = test_memo_key(root.path());
    let variants = [
        {
            let mut key = base.clone();
            key.channel = "1.95.0".to_string();
            key
        },
        {
            let mut key = base.clone();
            key.explicit_channel = Some("1.94.1".to_string());
            key
        },
        {
            let mut key = base.clone();
            key.profile = Some("default".to_string());
            key
        },
        {
            let mut key = base.clone();
            key.components.push("miri".to_string());
            key
        },
        {
            let mut key = base.clone();
            key.targets.push("x86_64-unknown-linux-musl".to_string());
            key
        },
        {
            let mut key = base.clone();
            key.rustup_home = root.path().join("other-rustup");
            key
        },
        {
            let mut key = base.clone();
            key.rustup_binary = root.path().join("other-rustup-bin");
            key
        },
    ];
    for variant in variants {
        assert_ne!(base, variant);
    }
}

#[test]
fn cargo_prepare_memo_rejects_changed_or_missing_toolchain() {
    let root = tempfile::tempdir().expect("temp dir");
    let key = test_memo_key(root.path());
    let toolchain = key.rustup_home.join("toolchains").join("1.94.1-test");
    std::fs::create_dir_all(toolchain.join("bin")).expect("create bin");
    std::fs::create_dir_all(toolchain.join("lib").join("rustlib")).expect("create rustlib");
    std::fs::write(&key.rustup_binary, b"rustup").expect("write rustup");
    std::fs::write(toolchain.join("bin").join("rustc"), b"rustc").expect("write rustc");
    let channel_manifest = toolchain.join(crate::toolchain_readiness::TOOLCHAIN_CHANNEL_MANIFEST);
    std::fs::write(&channel_manifest, b"manifest-version = '2'\n").expect("write channel manifest");
    let components = toolchain.join("lib").join("rustlib").join("components");
    std::fs::write(&components, b"rustc-test\n").expect("write components");

    assert!(
        toolchain_identity(&key, &toolchain).is_none(),
        "a declared target with no libcore must not memoize (soldr#3376)"
    );
    write_fake_std(&toolchain, "wasm32-unknown-unknown", false);
    let original = toolchain_identity(&key, &toolchain).expect("initial identity");
    filetime::set_file_mtime(
        &components,
        filetime::FileTime::from_unix_time(1_800_000_000, 0),
    )
    .expect("touch components without changing contents");
    let touched = toolchain_identity(&key, &toolchain).expect("touched identity");
    assert_eq!(
        original, touched,
        "content-hashed manifests must ignore mtime-only changes"
    );
    std::fs::write(&components, b"rustc-test\nrustfmt-preview-test\n").expect("change components");
    let changed = toolchain_identity(&key, &toolchain).expect("changed identity");
    assert_ne!(original, changed);

    std::fs::write(&channel_manifest, b"manifest-version = '3'\n")
        .expect("change channel manifest");
    let channel_changed = toolchain_identity(&key, &toolchain).expect("channel identity");
    assert_ne!(changed, channel_changed);
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    assert_ne!(
        cargo_prepare_memo_path(&paths, &key, changed),
        cargo_prepare_memo_path(&paths, &key, channel_changed),
        "a changed channel manifest must invalidate Cargo's warm-prepare memo"
    );

    std::fs::remove_dir_all(toolchain.join("lib/rustlib/wasm32-unknown-unknown"))
        .expect("delete the target's libraries, leaving `components` claiming them");
    assert!(
        toolchain_identity(&key, &toolchain).is_none(),
        "a claimed-but-missing target must be a memo miss, not a false hit"
    );

    std::fs::remove_dir_all(&toolchain).expect("remove fake toolchain");
    assert!(toolchain_identity(&key, &toolchain).is_none());
}

#[test]
fn cargo_prepare_memo_rejects_ambiguous_alias_toolchains() {
    let root = tempfile::tempdir().expect("temp dir");
    let key = test_memo_key(root.path());
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    std::fs::write(&key.rustup_binary, b"rustup").expect("write rustup");

    for host in ["x86_64-pc-windows-msvc", "x86_64-pc-windows-gnu"] {
        let toolchain = key
            .rustup_home
            .join("toolchains")
            .join(format!("1.94.1-{host}"));
        std::fs::create_dir_all(toolchain.join("bin")).expect("create bin");
        std::fs::create_dir_all(toolchain.join("lib").join("rustlib")).expect("create rustlib");
        std::fs::write(toolchain.join("bin").join("rustc"), host).expect("write rustc");
        std::fs::write(
            toolchain.join(crate::toolchain_readiness::TOOLCHAIN_CHANNEL_MANIFEST),
            b"manifest-version = '2'\n",
        )
        .expect("write channel manifest");
        std::fs::write(
            toolchain.join("lib").join("rustlib").join("components"),
            format!("rustc-{host}\n"),
        )
        .expect("write components");
    }

    let first = key
        .rustup_home
        .join("toolchains")
        .join("1.94.1-x86_64-pc-windows-msvc");
    write_cargo_prepare_memo(&paths, key.clone(), &first);
    assert!(
        memoized_toolchain_dir(&paths, &key).is_none(),
        "an alias with multiple installed hosts must prepare conservatively"
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn dylint_blessed_and_cargo_share_the_readiness_matrix() {
    use crate::dylint_toolchain_readiness::{
        dylint_toolchain_readiness_at, DylintToolchainReadiness,
    };
    use crate::toolchain_readiness::{
        native_rustc_path, probe_toolchain_state, toolchain_dir_name, ToolchainReadiness,
        TOOLCHAIN_CHANNEL_MANIFEST,
    };

    let root = tempfile::tempdir().expect("temp dir");
    let channel = "nightly-2026-01-18";
    let host = crate::pyo3_detect::host_triple();
    let mut key = test_memo_key(root.path());
    key.channel = channel.to_string();
    std::fs::write(&key.rustup_binary, b"rustup").expect("write rustup");
    let toolchain = key
        .rustup_home
        .join("toolchains")
        .join(toolchain_dir_name(channel, host));

    for (name, manifest, rustc, components, expected) in [
        (
            "directory-only",
            false,
            false,
            false,
            ToolchainReadiness::Partial(crate::toolchain_readiness::MissingToolchainEvidence {
                channel_manifest: true,
                native_rustc: true,
                host_std: false,
            }),
        ),
        (
            "rustc-only",
            false,
            true,
            false,
            ToolchainReadiness::Partial(crate::toolchain_readiness::MissingToolchainEvidence {
                channel_manifest: true,
                native_rustc: false,
                host_std: false,
            }),
        ),
        (
            "manifest-only",
            true,
            false,
            false,
            ToolchainReadiness::Partial(crate::toolchain_readiness::MissingToolchainEvidence {
                channel_manifest: false,
                native_rustc: true,
                host_std: false,
            }),
        ),
        (
            "components-only",
            false,
            false,
            true,
            ToolchainReadiness::Partial(crate::toolchain_readiness::MissingToolchainEvidence {
                channel_manifest: true,
                native_rustc: true,
                host_std: false,
            }),
        ),
        ("complete", true, true, true, ToolchainReadiness::Ready),
    ] {
        let _ = std::fs::remove_dir_all(&toolchain);
        std::fs::create_dir_all(&toolchain).expect("create toolchain dir");
        // Host std is present in every row: the rows vary the base evidence,
        // and the host-std case is exercised separately below.
        write_fake_std(&toolchain, host, true);
        if manifest {
            std::fs::create_dir_all(toolchain.join("lib/rustlib")).expect("manifest parent");
            std::fs::write(
                toolchain.join(TOOLCHAIN_CHANNEL_MANIFEST),
                b"manifest = '2'\n",
            )
            .expect("channel manifest");
        }
        if rustc {
            let path = native_rustc_path(&toolchain);
            std::fs::create_dir_all(path.parent().expect("rustc parent")).expect("rustc parent");
            std::fs::write(path, b"rustc").expect("rustc");
        }
        if components {
            let path = toolchain.join("lib/rustlib/components");
            std::fs::create_dir_all(path.parent().expect("components parent"))
                .expect("components parent");
            std::fs::write(path, b"rustc\n").expect("components");
            write_fake_std(&toolchain, "wasm32-unknown-unknown", false);
        }

        assert_eq!(
            probe_toolchain_state(&key.rustup_home, channel, host),
            expected,
            "blessed readiness for {name}"
        );
        match (
            expected,
            dylint_toolchain_readiness_at(&key.rustup_home, channel),
        ) {
            (ToolchainReadiness::Ready, DylintToolchainReadiness::Ready { .. }) => {}
            (ToolchainReadiness::Partial(_), DylintToolchainReadiness::Partial { .. }) => {}
            (_, actual) => panic!("Dylint readiness drifted for {name}: {actual:?}"),
        }
        assert_eq!(
            toolchain_identity(&key, &toolchain).is_some(),
            matches!(expected, ToolchainReadiness::Ready),
            "Cargo memo must require base Ready plus components for {name}"
        );
    }

    // soldr#3376: base evidence and `components` all present, host std gone
    // (a poisoned cache, an interrupted extraction). Every path must agree it is
    // not ready, and cargo's memo must not accept it.
    let host_std_dir = toolchain.join("lib/rustlib").join(host);
    std::fs::remove_dir_all(&host_std_dir).expect("delete the host std files");
    let expected_partial =
        ToolchainReadiness::Partial(crate::toolchain_readiness::MissingToolchainEvidence {
            channel_manifest: false,
            native_rustc: false,
            host_std: true,
        });
    assert_eq!(
        probe_toolchain_state(&key.rustup_home, channel, host),
        expected_partial
    );
    assert!(matches!(
        dylint_toolchain_readiness_at(&key.rustup_home, channel),
        DylintToolchainReadiness::Partial { .. }
    ));
    assert!(toolchain_identity(&key, &toolchain).is_none());

    let _ = std::fs::remove_dir_all(&toolchain);
    assert_eq!(
        probe_toolchain_state(&key.rustup_home, channel, host),
        ToolchainReadiness::Missing
    );
    assert!(matches!(
        dylint_toolchain_readiness_at(&key.rustup_home, channel),
        DylintToolchainReadiness::Missing
    ));
    assert!(toolchain_identity(&key, &toolchain).is_none());
}

#[cfg(test)]
mod pin_requirement_tests {
    use super::*;

    // soldr#1766. The gate refuses a build when no rust-toolchain.toml exists
    // at or above the working directory, instead of silently resolving rustc
    // from PATH.

    /// Turn the strict pin guard on (soldr#3887: it is off by default) and
    /// clear the other way a build can be considered pinned.
    ///
    /// `RUSTUP_TOOLCHAIN` has to be cleared too now that it counts as a pin:
    /// otherwise `unpinned_workspace_is_refused` and friends pass or fail
    /// depending on the ambient environment, and they would silently stop
    /// testing anything on a runner that exports it — which is precisely the
    /// thin-v2 lane that motivated this change.
    fn without_opt_out<T>(body: impl FnOnce() -> T) -> T {
        let _lock = crate::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os(ALLOW_UNPINNED_ENV_VAR);
        let previous_toolchain = std::env::var_os(RUSTUP_TOOLCHAIN_ENV_VAR);
        std::env::set_var(ALLOW_UNPINNED_ENV_VAR, "0");
        std::env::remove_var(RUSTUP_TOOLCHAIN_ENV_VAR);
        let out = body();
        match previous_toolchain {
            Some(value) => std::env::set_var(RUSTUP_TOOLCHAIN_ENV_VAR, value),
            None => std::env::remove_var(RUSTUP_TOOLCHAIN_ENV_VAR),
        }
        match previous {
            Some(value) => std::env::set_var(ALLOW_UNPINNED_ENV_VAR, value),
            None => std::env::remove_var(ALLOW_UNPINNED_ENV_VAR),
        }
        out
    }

    #[test]
    fn unpinned_is_allowed_unless_explicitly_turned_off() {
        // soldr#3887: unpinned is the default. Only the explicit off spellings
        // restore the soldr#1766 guard; absent, empty, and unrecognised values
        // must not refuse a build in a workspace with no rust-toolchain.toml.
        let _lock = crate::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os(ALLOW_UNPINNED_ENV_VAR);
        std::env::remove_var(ALLOW_UNPINNED_ENV_VAR);
        let absent = unpinned_allowed();
        let mut outcomes = Vec::new();
        for value in ["1", "true", "YES", "on", "", "disabled", "2"] {
            std::env::set_var(ALLOW_UNPINNED_ENV_VAR, value);
            outcomes.push((value, true, unpinned_allowed()));
        }
        for value in ["0", "false", "NO", "off"] {
            std::env::set_var(ALLOW_UNPINNED_ENV_VAR, value);
            outcomes.push((value, false, unpinned_allowed()));
        }
        match previous {
            Some(value) => std::env::set_var(ALLOW_UNPINNED_ENV_VAR, value),
            None => std::env::remove_var(ALLOW_UNPINNED_ENV_VAR),
        }
        assert!(absent, "an absent switch must allow an unpinned build");
        for (value, expected, actual) in outcomes {
            assert_eq!(actual, expected, "SOLDR_ALLOW_UNPINNED={value:?}");
        }
    }

    #[test]
    fn unpinned_workspace_is_refused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let err = without_opt_out(|| require_toolchain_pin(temp.path()).unwrap_err());
        let rendered = err.to_string();
        assert!(
            rendered.contains("no rust-toolchain.toml found"),
            "error must name the missing pin: {rendered}"
        );
        assert!(
            rendered.contains("unset SOLDR_ALLOW_UNPINNED"),
            "error must tell the user how to drop the strict guard: {rendered}"
        );
    }

    #[test]
    fn pin_in_an_ancestor_satisfies_a_subdirectory_build() {
        // The trap this test exists for: reading only the cwd would reject
        // every build launched from a subdirectory of a pinned repo --
        // including this workspace's own tests, whose cwd is the package dir
        // while the pin lives at the workspace root.
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("rust-toolchain.toml"),
            b"[toolchain]\nchannel = \"stable\"\n",
        )
        .expect("write pin");
        let nested = temp.path().join("crates").join("inner").join("src");
        std::fs::create_dir_all(&nested).expect("nested dirs");

        without_opt_out(|| {
            require_toolchain_pin(&nested).expect("an ancestor pin must satisfy the requirement")
        });
    }

    #[test]
    fn opt_out_env_var_permits_an_unpinned_build() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _lock = crate::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os(ALLOW_UNPINNED_ENV_VAR);
        std::env::set_var(ALLOW_UNPINNED_ENV_VAR, "1");
        let result = require_toolchain_pin(temp.path());
        match previous {
            Some(value) => std::env::set_var(ALLOW_UNPINNED_ENV_VAR, value),
            None => std::env::remove_var(ALLOW_UNPINNED_ENV_VAR),
        }
        result.expect("SOLDR_ALLOW_UNPINNED must permit an unpinned build");
    }

    // soldr#1917 follow-up: RUSTUP_TOOLCHAIN is a pin, not an absence of one.

    #[test]
    fn an_explicit_rustup_toolchain_satisfies_the_pin_requirement() {
        // The thin-v2 verifier builds a synthesized crate under $RUNNER_TEMP,
        // outside any manifest, and selects the toolchain with
        // RUSTUP_TOOLCHAIN=1.94.1. That is a pinned build, and refusing it
        // sent the lane to the one workaround that is actively wrong:
        // SOLDR_ALLOW_UNPINNED=1, which disables the check for a caller who
        // *is* pinned.
        let temp = tempfile::tempdir().expect("tempdir");
        let _lock = crate::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous_opt_out = std::env::var_os(ALLOW_UNPINNED_ENV_VAR);
        let previous_toolchain = std::env::var_os(RUSTUP_TOOLCHAIN_ENV_VAR);
        std::env::set_var(ALLOW_UNPINNED_ENV_VAR, "0");
        std::env::set_var(RUSTUP_TOOLCHAIN_ENV_VAR, "1.94.1");

        let result = require_toolchain_pin(temp.path());

        match previous_opt_out {
            Some(value) => std::env::set_var(ALLOW_UNPINNED_ENV_VAR, value),
            None => std::env::remove_var(ALLOW_UNPINNED_ENV_VAR),
        }
        match previous_toolchain {
            Some(value) => std::env::set_var(RUSTUP_TOOLCHAIN_ENV_VAR, value),
            None => std::env::remove_var(RUSTUP_TOOLCHAIN_ENV_VAR),
        }
        result.expect("an explicitly selected rustup toolchain is a pin");
    }

    #[test]
    fn a_blank_rustup_toolchain_is_not_a_pin() {
        // Exported-but-empty is how a shell leaves a variable it meant to
        // unset. It selects nothing, so it must not read as a pin -- same
        // reasoning as `explicit_falsey_opt_out_still_requires_a_pin`.
        let temp = tempfile::tempdir().expect("tempdir");
        let _lock = crate::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous_opt_out = std::env::var_os(ALLOW_UNPINNED_ENV_VAR);
        let previous_toolchain = std::env::var_os(RUSTUP_TOOLCHAIN_ENV_VAR);
        std::env::set_var(ALLOW_UNPINNED_ENV_VAR, "0");

        let mut outcomes = Vec::new();
        for blank in ["", "   "] {
            std::env::set_var(RUSTUP_TOOLCHAIN_ENV_VAR, blank);
            outcomes.push((blank, require_toolchain_pin(temp.path()).is_err()));
        }

        match previous_opt_out {
            Some(value) => std::env::set_var(ALLOW_UNPINNED_ENV_VAR, value),
            None => std::env::remove_var(ALLOW_UNPINNED_ENV_VAR),
        }
        match previous_toolchain {
            Some(value) => std::env::set_var(RUSTUP_TOOLCHAIN_ENV_VAR, value),
            None => std::env::remove_var(RUSTUP_TOOLCHAIN_ENV_VAR),
        }
        for (blank, refused) in outcomes {
            assert!(
                refused,
                "RUSTUP_TOOLCHAIN={blank:?} must not count as a pin"
            );
        }
    }

    #[test]
    fn explicit_falsey_opt_out_still_requires_a_pin() {
        // soldr#3887: an explicit off spelling turns the strict guard on.
        let temp = tempfile::tempdir().expect("tempdir");
        let _lock = crate::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os(ALLOW_UNPINNED_ENV_VAR);
        // Also clear RUSTUP_TOOLCHAIN: it now satisfies the pin on its own, so
        // an ambient one would make these calls succeed and this test would
        // fail for a reason unrelated to the opt-out switch it is checking.
        let previous_toolchain = std::env::var_os(RUSTUP_TOOLCHAIN_ENV_VAR);
        std::env::remove_var(RUSTUP_TOOLCHAIN_ENV_VAR);
        let mut outcomes = Vec::new();
        for disabled in ["0", "false", "no", "off"] {
            std::env::set_var(ALLOW_UNPINNED_ENV_VAR, disabled);
            outcomes.push((disabled, require_toolchain_pin(temp.path()).is_err()));
        }
        match previous {
            Some(value) => std::env::set_var(ALLOW_UNPINNED_ENV_VAR, value),
            None => std::env::remove_var(ALLOW_UNPINNED_ENV_VAR),
        }
        match previous_toolchain {
            Some(value) => std::env::set_var(RUSTUP_TOOLCHAIN_ENV_VAR, value),
            None => std::env::remove_var(RUSTUP_TOOLCHAIN_ENV_VAR),
        }
        for (disabled, refused) in outcomes {
            assert!(
                refused,
                "SOLDR_ALLOW_UNPINNED={disabled:?} must turn the pin guard on"
            );
        }
    }
}

/// soldr#3195: which `soldr rustup` passthroughs the download guard stops.
#[test]
fn rustup_passthrough_download_verbs_are_recognised() {
    let args = |words: &[&str]| words.iter().map(|w| (*w).to_string()).collect::<Vec<_>>();
    for downloading in [
        &["toolchain", "install", "nightly-2026-05-26"][..],
        &["component", "add", "--toolchain", "1.95.0", "rustfmt"],
        &[
            "target",
            "add",
            "x86_64-unknown-linux-musl",
            "--toolchain",
            "1.95.0",
        ],
        &["+nightly", "component", "add", "rust-src"],
        &["install", "stable"],
        &["update"],
        &["default", "nightly"],
    ] {
        assert!(
            super::rustup_args_would_download(&args(downloading)),
            "{downloading:?}"
        );
    }
    for read_only in [
        &["toolchain", "list"][..],
        &["component", "list", "--installed"],
        &["target", "list"],
        &["which", "rustc"],
        &["show"],
        &["--version"],
        &["toolchain", "uninstall", "nightly-2026-05-26"],
    ] {
        assert!(
            !super::rustup_args_would_download(&args(read_only)),
            "{read_only:?}"
        );
    }
}

fn pin_args(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_string()).collect()
}

#[test]
fn scope_rustup_args_to_pin_walks_ancestors_for_pin() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.98.1\"\n",
    )
    .unwrap();
    let sub = root.path().join("a/b");
    std::fs::create_dir_all(&sub).unwrap();
    let got = scope_rustup_args_to_pin_in(&pin_args(&["target", "add", "x"]), &sub, None).unwrap();
    assert_eq!(
        got,
        pin_args(&["target", "add", "--toolchain", "1.98.1", "x"])
    );
}

#[test]
fn scope_rustup_args_to_pin_keeps_explicit_toolchain() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.98.1\"\n",
    )
    .unwrap();
    let input = pin_args(&["target", "add", "--toolchain", "nightly", "x"]);
    let got = scope_rustup_args_to_pin_in(&input, root.path(), None).unwrap();
    assert_eq!(got, input);
}

#[test]
fn scope_rustup_args_to_pin_unchanged_without_manifest() {
    let root = tempfile::tempdir().unwrap();
    let sub = root.path().join("a");
    std::fs::create_dir_all(&sub).unwrap();
    let input = pin_args(&["target", "add", "x"]);
    let got = scope_rustup_args_to_pin_in(&input, &sub, None).unwrap();
    assert_eq!(got, input);
}

#[test]
fn scope_rustup_args_to_pin_defers_to_rustup_toolchain_env() {
    // soldr#3692: RUSTUP_TOOLCHAIN overrides rust-toolchain.toml in rustup's
    // precedence, so soldr must not inject the manifest pin over it.
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.98.1\"\n",
    )
    .unwrap();
    let input = pin_args(&["target", "add", "wasm32-unknown-unknown"]);
    let env = std::ffi::OsString::from("nightly");
    let got = scope_rustup_args_to_pin_in(&input, root.path(), Some(&env)).unwrap();
    assert_eq!(got, input);
    // Empty / whitespace values are treated as unset: the pin still applies.
    let blank = std::ffi::OsString::from("  ");
    let got = scope_rustup_args_to_pin_in(&input, root.path(), Some(&blank)).unwrap();
    assert_eq!(
        got,
        pin_args(&[
            "target",
            "add",
            "--toolchain",
            "1.98.1",
            "wasm32-unknown-unknown"
        ])
    );
}
