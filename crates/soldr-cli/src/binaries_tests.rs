//! Unit coverage split from `binaries.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;
use crate::{EnvVarGuard, TEST_PROCESS_ENV_LOCK};

#[test]
fn managed_wrapper_shim_has_compiler_identity() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));

    let wrapper = rustc_wrapper_shim_binary(&paths).expect("materialize wrapper shim");

    assert!(wrapper.is_file(), "missing {}", wrapper.display());
    assert_eq!(
        wrapper.file_stem().and_then(std::ffi::OsStr::to_str),
        Some("rustc")
    );
    assert_eq!(
        wrapper.parent(),
        Some(paths.versioned_shims_dir().as_path())
    );
}

#[test]
fn dylint_wrapper_shim_has_dedicated_identity() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let wrapper = dylint_wrapper_shim_binary(&paths).expect("materialize Dylint wrapper");

    assert!(wrapper.is_file(), "missing {}", wrapper.display());
    assert_eq!(
        wrapper.file_stem().and_then(std::ffi::OsStr::to_str),
        Some("soldr-dylint")
    );
    assert_eq!(
        wrapper.parent(),
        Some(paths.versioned_shims_dir().as_path())
    );
}

#[test]
fn native_compiler_shim_is_version_scoped() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let wrapper = zccache_soldr_shim_binary_at(&paths).expect("materialize native compiler shim");

    assert!(wrapper.is_file(), "missing {}", wrapper.display());
    assert_eq!(
        wrapper.file_stem().and_then(std::ffi::OsStr::to_str),
        Some("zccache-soldr")
    );
    assert!(
        wrapper.starts_with(paths.versioned_shims_dir().join("images")),
        "native shim must live in the versioned, image-addressed tree: {}",
        wrapper.display()
    );
}

#[test]
fn native_compiler_shim_images_cannot_overwrite_each_other() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let first = root.path().join("soldr-image-a");
    let second = root.path().join("soldr-image-b");
    std::fs::write(&first, b"first independently linked soldr image").expect("first image");
    std::fs::write(&second, b"second independently linked soldr image").expect("second image");

    let first_target = zccache_soldr_shim_target(&paths, &first).expect("first target");
    let second_target = zccache_soldr_shim_target(&paths, &second).expect("second target");

    assert_ne!(first_target, second_target);
    assert!(first_target.starts_with(paths.versioned_shims_dir().join("images")));
    assert!(second_target.starts_with(paths.versioned_shims_dir().join("images")));
}

#[test]
fn relocated_runtime_alias_is_materialized_beside_current_exe() {
    let source = std::path::Path::new("/opt/package/bin/soldr");
    let current = std::path::Path::new("/tmp/runtime/hash/soldr");
    let (current_target, source_target) =
        runtime_alias_targets(source, current, "soldr-daemon").expect("alias targets");
    assert_eq!(
        current_target,
        std::path::Path::new("/tmp/runtime/hash/soldr-daemon")
    );
    assert_eq!(
        source_target.as_deref(),
        Some(std::path::Path::new("/opt/package/bin/soldr-daemon"))
    );
}

#[test]
fn ancestor_search_canonicalizes_the_user_home_boundary() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let nested = home.join("nested");
    std::fs::create_dir_all(home.join(".cargo")).expect("create global toolchain directory");
    std::fs::create_dir_all(&nested).expect("create nested directory");

    let alternate_home_spelling = nested.join("..");
    assert!(
        find_ancestor_dir_bounded(&alternate_home_spelling, ".cargo", Some(&home)).is_none(),
        "the user home's global toolchain directory is not repository-local"
    );

    let project = nested.join("project");
    let project_tools = project.join(".cargo");
    std::fs::create_dir_all(&project_tools).expect("create project-local toolchain directory");
    let canonical_project_tools =
        std::fs::canonicalize(&project_tools).expect("canonicalize project tool directory");
    assert_eq!(
        find_ancestor_dir_bounded(&project, ".cargo", Some(&home)).as_deref(),
        Some(canonical_project_tools.as_path()),
    );
}

#[test]
fn parse_tool_spec_defaults_to_latest_version() {
    let (tool, version) = parse_tool_spec("maturin");
    assert_eq!(tool, "maturin");
    assert!(matches!(version, VersionSpec::Latest));
}

#[test]
fn rustup_resolution_failure_appends_ci_guidance() {
    let error = rustup_resolution_failure(
        "rustc",
        b"error: toolchain '1.94.1-x86_64-pc-windows-msvc' is not installed",
    );

    let rendered = error.to_string();
    assert!(rendered.contains("failed to resolve rustc via rustup: error: toolchain '1.94.1-x86_64-pc-windows-msvc' is not installed"));
    assert!(rendered.contains("pins Rust in rust-toolchain.toml"));
    assert!(rendered.contains("generic stable toolchain"));
    assert!(rendered.contains("RUSTUP_TOOLCHAIN"));
    assert!(rendered.contains("setup-soldr action path"));
    assert!(rendered.contains("soldr bootstrap"));
    assert!(rendered.contains("SOLDR_NO_BOOTSTRAP"));
}

#[test]
fn channel_scoped_lookup_uses_the_installer_watchdog_not_generic_silence() {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Linux {
        return;
    }
    let _lock = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = tempfile::tempdir().expect("tempdir");
    let manager = root.path().join("fake-manager");
    let resolved = root.path().join("delayed-rustc");
    std::fs::write(&resolved, b"stub compiler").expect("resolved file");
    std::fs::write(
        &manager,
        format!(
            "#!/bin/sh\nsleep 2\nprintf '%s\\n' '{}'\n",
            resolved.display()
        ),
    )
    .expect("fake manager script");
    crate::platform::fs::permissions::make_executable(&manager).expect("make manager executable");

    let _manager = EnvVarGuard::set(TEST_RUSTUP_BIN_ENV_VAR, &manager);
    let _timeout = EnvVarGuard::set(crate::core::COMMAND_OUTPUT_TIMEOUT_ENV_VAR, "1");
    let _cache = EnvVarGuard::set(TOOLCHAIN_BIN_CACHE_ENV_VAR, "off");
    let result = resolve_toolchain_binary_with_optional_channel(
        "rustc",
        Some("nightly-manager-lock-test"),
        None,
    );
    assert_eq!(
        result.expect("lookup must survive two seconds of manager-lock silence"),
        resolved
    );
}

#[test]
fn known_subcommand_registry_recognizes_phase_two_tools() {
    for sub in ["nextest", "deny", "audit", "llvm-cov"] {
        let spec = crate::fetch::lookup_by_cargo_subcommand(sub)
            .unwrap_or_else(|| panic!("missing registry entry for cargo {sub}"));
        assert_eq!(spec.cargo_subcommand, Some(sub));
        assert!(spec.crate_name.starts_with("cargo-"));
    }
}

#[test]
fn known_subcommand_registry_recognizes_phase_three_tools() {
    for sub in ["udeps", "semver-checks", "expand", "watch"] {
        let spec = crate::fetch::lookup_by_cargo_subcommand(sub)
            .unwrap_or_else(|| panic!("missing registry entry for cargo {sub}"));
        assert_eq!(spec.cargo_subcommand, Some(sub));
        assert!(spec.crate_name.starts_with("cargo-"));
    }
}

#[test]
fn top_level_tools_are_not_cargo_subcommands() {
    for crate_name in [
        "cross",
        "mdbook",
        "cbindgen",
        "wasm-pack",
        "trunk",
        "sccache",
    ] {
        let spec = crate::fetch::lookup_by_crate(crate_name)
            .unwrap_or_else(|| panic!("missing registry entry for {crate_name}"));
        assert_eq!(spec.cargo_subcommand, None);
    }
}

#[test]
fn soldr_itself_is_registered_for_self_trampoline() {
    let spec = crate::fetch::lookup_by_crate("soldr")
        .expect("soldr should be registered in known_tools for --as trampoline");
    assert_eq!(spec.binary_name, "soldr");
    assert_eq!(spec.repo, Some(("zackees", "soldr")));
    assert_eq!(spec.cargo_subcommand, None);
}

// -----------------------------------------------------------------
// Channel-scoped rustup-which disk cache (nested dylint re-entry
// overhead reduction). These use the `_in` variants with an
// injected `cache_root` tempdir so they never touch the real
// `~/.soldr/cache`.
// -----------------------------------------------------------------

fn test_scope(home: &Path, host: &str) -> ToolchainBinCacheScope {
    ToolchainBinCacheScope {
        rustup_home: home.to_path_buf(),
        host_triple: host.to_string(),
    }
}

#[test]
fn sanitize_toolchain_for_path_replaces_unsafe_characters() {
    assert_eq!(
        sanitize_toolchain_for_path("nightly-2026-01-18"),
        "nightly-2026-01-18"
    );
    assert_eq!(
        sanitize_toolchain_for_path("weird/../channel:name"),
        "weird___channel_name"
    );
}

#[test]
fn disk_cache_lookup_returns_none_when_uncached() {
    let root = tempfile::tempdir().expect("tempdir");
    let scope = test_scope(root.path(), "x86_64-unknown-linux-gnu");
    assert!(disk_cache_lookup_in(root.path(), &scope, "nightly-2026-01-18", "rustc").is_none());
}

#[test]
fn disk_cache_round_trips_a_resolved_path() {
    let root = tempfile::tempdir().expect("tempdir");
    // The disk cache only trusts entries whose target file still
    // exists, so materialize a real file to point at.
    let resolved = root.path().join("rustc-real");
    std::fs::write(&resolved, b"stub").expect("write stub binary");
    let scope = test_scope(root.path(), "x86_64-unknown-linux-gnu");

    disk_cache_store_in(
        root.path(),
        &scope,
        "nightly-2026-01-18",
        "rustc",
        &resolved,
    );
    let looked_up = disk_cache_lookup_in(root.path(), &scope, "nightly-2026-01-18", "rustc")
        .expect("cache hit after store");
    assert_eq!(looked_up, resolved);

    let cache_file =
        toolchain_bin_disk_cache_path_in(root.path(), &scope, "nightly-2026-01-18", "rustc");
    assert!(cache_file.is_file());
    assert!(cache_file.starts_with(root.path().join("toolchain-bins").join("v2")));
}

#[test]
fn disk_cache_ignores_stale_entry_whose_target_is_gone() {
    let root = tempfile::tempdir().expect("tempdir");
    let resolved = root.path().join("rustc-real");
    std::fs::write(&resolved, b"stub").expect("write stub binary");
    let scope = test_scope(root.path(), "x86_64-unknown-linux-gnu");
    disk_cache_store_in(
        root.path(),
        &scope,
        "nightly-2026-01-18",
        "rustc",
        &resolved,
    );

    std::fs::remove_file(&resolved).expect("remove target to simulate staleness");

    assert!(
        disk_cache_lookup_in(root.path(), &scope, "nightly-2026-01-18", "rustc").is_none(),
        "a cache entry pointing at a missing file must not be trusted"
    );
}

#[test]
fn toolchain_bin_memo_revalidates_and_evicts_stale_paths() {
    let root = tempfile::tempdir().expect("tempdir");
    let scope = test_scope(root.path(), "x86_64-unknown-linux-gnu");
    let path = root.path().join("compiler");
    std::fs::write(&path, b"stub").expect("write binary");
    assert!(toolchain_bin_memo_lookup(&scope, "memo-test-channel", "rustc").is_none());
    toolchain_bin_memo_store(&scope, "memo-test-channel", "rustc", path.clone());
    assert_eq!(
        toolchain_bin_memo_lookup(&scope, "memo-test-channel", "rustc"),
        Some(path.clone())
    );
    std::fs::remove_file(path).expect("remove memoized binary");
    assert!(toolchain_bin_memo_lookup(&scope, "memo-test-channel", "rustc").is_none());
}

#[test]
fn toolchain_bin_cache_scope_separates_homes_and_hosts() {
    let root = tempfile::tempdir().expect("tempdir");
    let home_a = root.path().join("home-a");
    let home_b = root.path().join("home-b");
    let a = test_scope(&home_a, "x86_64-unknown-linux-gnu");
    let b = test_scope(&home_b, "x86_64-unknown-linux-gnu");
    let c = test_scope(&home_a, "aarch64-unknown-linux-gnu");
    assert_ne!(a.stable_key(), b.stable_key());
    assert_ne!(a.stable_key(), c.stable_key());
}

#[test]
fn toolchain_bin_cache_scope_absolutizes_relative_homes_per_cwd() {
    let root = tempfile::tempdir().expect("tempdir");
    let cwd_a = root.path().join("checkout-a");
    let cwd_b = root.path().join("checkout-b");
    std::fs::create_dir_all(&cwd_a).expect("cwd a");
    std::fs::create_dir_all(&cwd_b).expect("cwd b");
    let relative = PathBuf::from(".rustup");
    let a = ToolchainBinCacheScope::from_home(
        relative.clone(),
        "x86_64-unknown-linux-gnu".to_string(),
        &cwd_a,
    )
    .expect("scope a");
    let b =
        ToolchainBinCacheScope::from_home(relative, "x86_64-unknown-linux-gnu".to_string(), &cwd_b)
            .expect("scope b");
    assert!(a.rustup_home.is_absolute());
    assert!(b.rustup_home.is_absolute());
    assert_ne!(a.stable_key(), b.stable_key());
}

#[test]
fn toolchain_bin_cache_disabled_only_on_off_value() {
    // Test seam: mutate the process-global env var, observe the
    // gate, then restore whatever was there before so this test
    // does not leak state into others in the binary.
    let previous = std::env::var_os(TOOLCHAIN_BIN_CACHE_ENV_VAR);

    std::env::set_var(TOOLCHAIN_BIN_CACHE_ENV_VAR, "off");
    assert!(toolchain_bin_cache_disabled());

    std::env::set_var(TOOLCHAIN_BIN_CACHE_ENV_VAR, "OFF");
    assert!(
        toolchain_bin_cache_disabled(),
        "gate must be case-insensitive"
    );

    std::env::set_var(TOOLCHAIN_BIN_CACHE_ENV_VAR, "on");
    assert!(!toolchain_bin_cache_disabled());

    std::env::remove_var(TOOLCHAIN_BIN_CACHE_ENV_VAR);
    assert!(
        !toolchain_bin_cache_disabled(),
        "unset must default to enabled"
    );

    match previous {
        Some(value) => std::env::set_var(TOOLCHAIN_BIN_CACHE_ENV_VAR, value),
        None => std::env::remove_var(TOOLCHAIN_BIN_CACHE_ENV_VAR),
    }
}

// soldr#1799: `home_origin` is the discriminant telemetry records and CI
// asserts on, so its classification is pinned here rather than left to be
// re-derived from the branch it replaced.

#[test]
fn a_binary_inside_the_managed_cargo_home_is_managed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().to_path_buf());
    let managed = crate::fetch::managed_cargo_home(&paths);
    let binary = managed.join("bin").join("cargo");
    std::fs::create_dir_all(binary.parent().expect("parent")).expect("mkdir");
    std::fs::write(&binary, b"").expect("write");

    assert_eq!(home_origin_for_binary(&binary, &paths), HomeOrigin::Managed);
}

#[test]
fn a_binary_inside_the_managed_rustup_home_is_managed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().to_path_buf());
    let managed = crate::fetch::managed_rustup_home(&paths);
    let binary = managed
        .join("toolchains")
        .join("nightly")
        .join("bin")
        .join("rustc");
    std::fs::create_dir_all(binary.parent().expect("parent")).expect("mkdir");
    std::fs::write(&binary, b"").expect("write");

    assert_eq!(home_origin_for_binary(&binary, &paths), HomeOrigin::Managed);
}

#[test]
fn a_host_binary_never_reports_managed() {
    // The regression that made this a named concept: a host-resolved
    // cargo/rustfmt executing under soldr's default-less managed
    // RUSTUP_HOME. rustup then reports no default toolchain (#1768), and
    // more insidiously the home flip changes which rustc is used, so warm
    // builds recompile the world with nothing appearing to fail.
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().to_path_buf());
    let host = temp.path().join("host-toolchain").join("bin").join("cargo");
    std::fs::create_dir_all(host.parent().expect("parent")).expect("mkdir");
    std::fs::write(&host, b"").expect("write");

    assert_eq!(
        home_origin_for_binary(&host, &paths),
        HomeOrigin::Caller,
        "a binary outside the managed homes must keep the caller's context"
    );
}

#[test]
fn home_origin_strings_are_stable() {
    // CI assertions and log consumers key on these exact spellings.
    assert_eq!(HomeOrigin::Caller.as_str(), "caller");
    assert_eq!(HomeOrigin::Managed.as_str(), "managed");
    assert_eq!(HomeOrigin::RepoLocal.as_str(), "repo-local");
    assert_eq!(HomeOrigin::Dylint.as_str(), "dylint");
}

/// soldr#3567: the toolchain-binary cache must key on the home its
/// `rustup which` runs under. It keyed on the caller's `~/.rustup` while the
/// lookup ran under the managed home, so a path resolved before the managed
/// home existed kept answering for the Dylint nightly provisioned there.
#[test]
fn toolchain_bin_cache_scope_keys_on_the_home_the_lookup_runs_under() {
    let _lock = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = tempfile::tempdir().expect("tempdir");
    let managed = root.path().join("rustup");
    std::fs::create_dir_all(&managed).expect("managed rustup home");
    let _root = EnvVarGuard::set(crate::core::SOLDR_CACHE_DIR_ENV_VAR, root.path());
    let _home = EnvVarGuard::remove(crate::core::RUSTUP_HOME_ENV_VAR);

    let mut lookup = std::process::Command::new("rustup");
    apply_implicit_toolchain_homes(&mut lookup);
    let lookup_home = lookup
        .get_envs()
        .find_map(|(name, value)| (name == crate::core::RUSTUP_HOME_ENV_VAR).then_some(value))
        .flatten()
        .map(PathBuf::from)
        .expect("the lookup runs under the managed home");

    let scope = ToolchainBinCacheScope::current().expect("cache scope");
    assert_eq!(
        scope.rustup_home,
        std::fs::canonicalize(&lookup_home).expect("canonical managed home")
    );
}

// soldr#1799: repo-local is a third origin, not a flavour of caller.

#[test]
fn a_repo_local_rustup_reports_repo_local_not_caller() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));
    let repo = temp.path().join("repo");
    let binary = repo
        .join(".rustup")
        .join("toolchains")
        .join("1.94.1")
        .join("bin")
        .join("cargo");
    std::fs::create_dir_all(binary.parent().expect("parent")).expect("mkdir");
    std::fs::write(&binary, b"").expect("write");
    let nested = repo.join("crates").join("inner");
    std::fs::create_dir_all(&nested).expect("nested");

    assert_eq!(
        home_origin_for_binary_from(&binary, &paths, Some(&nested)),
        HomeOrigin::RepoLocal,
        "an ancestor .rustup is neither the caller's homes nor soldr's managed ones"
    );
}

#[test]
fn a_repo_local_cargo_home_is_also_repo_local() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));
    let repo = temp.path().join("repo");
    let binary = repo.join(".cargo").join("bin").join("cargo");
    std::fs::create_dir_all(binary.parent().expect("parent")).expect("mkdir");
    std::fs::write(&binary, b"").expect("write");

    assert_eq!(
        home_origin_for_binary_from(&binary, &paths, Some(&repo)),
        HomeOrigin::RepoLocal
    );
}

#[test]
fn managed_still_wins_over_a_surrounding_repo_local_dir() {
    // Precedence matters: the homes-application branch keys on Managed,
    // so a managed binary that happens to sit under some ancestor
    // `.cargo` must not be reclassified out of it.
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("repo").join(".cargo").join("soldr-root");
    let paths = SoldrPaths::with_root(root.clone());
    let binary = crate::fetch::managed_rustup_home(&paths)
        .join("toolchains")
        .join("1.94.1")
        .join("bin")
        .join("cargo");
    std::fs::create_dir_all(binary.parent().expect("parent")).expect("mkdir");
    std::fs::write(&binary, b"").expect("write");

    assert_eq!(
        home_origin_for_binary_from(&binary, &paths, Some(&root)),
        HomeOrigin::Managed,
        "managed must keep precedence or apply_resolved_toolchain_homes changes behaviour"
    );
}

#[test]
fn repo_local_still_runs_under_the_callers_homes() {
    // The whole point of keeping this a reporting-only distinction: the
    // homes decision is `== Managed`, so RepoLocal must behave exactly
    // like Caller there. If this ever flips, a repo-local toolchain would
    // start executing under soldr's managed homes -- the #1768 regression.
    assert_ne!(HomeOrigin::RepoLocal, HomeOrigin::Managed);
}

#[test]
fn managed_toolchain_keeps_its_library_path_and_the_callers_entries() {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Linux {
        return;
    }
    let _lock = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));
    let toolchain = crate::fetch::managed_rustup_home(&paths)
        .join("toolchains")
        .join("1.94.1");
    let binary = toolchain.join("bin").join("cargo");
    let library_dir = toolchain.join("lib");
    std::fs::create_dir_all(binary.parent().expect("binary parent")).expect("bin dir");
    std::fs::create_dir_all(&library_dir).expect("lib dir");
    std::fs::write(&binary, b"").expect("binary");

    let nix_library_dir = temp.path().join("nix-libs");
    let caller_library_dir = temp.path().join("caller-libs");
    let _nix_library_path = EnvVarGuard::set(
        "NIX_LD_LIBRARY_PATH",
        std::env::join_paths([nix_library_dir.as_path()]).expect("path"),
    );
    let mut command = std::process::Command::new("cargo");
    command.env(
        "LD_LIBRARY_PATH",
        std::env::join_paths([caller_library_dir.as_path()]).expect("path"),
    );
    apply_managed_toolchain_library_path_if_available(&mut command, &binary, &paths);

    let loader_path = command
        .get_envs()
        .find_map(|(key, value)| (key == "LD_LIBRARY_PATH").then_some(value))
        .flatten()
        .expect("managed command loader path");
    assert_eq!(
        std::env::split_paths(loader_path).collect::<Vec<_>>(),
        vec![
            library_dir.clone(),
            nix_library_dir,
            caller_library_dir.clone()
        ],
    );

    std::env::set_var("NIX_LD_LIBRARY_PATH", "");
    let mut command = std::process::Command::new("cargo");
    command.env(
        "LD_LIBRARY_PATH",
        std::env::join_paths([caller_library_dir.as_path()]).expect("path"),
    );
    apply_managed_toolchain_library_path_if_available(&mut command, &binary, &paths);
    let loader_path = command
        .get_envs()
        .find_map(|(key, value)| (key == "LD_LIBRARY_PATH").then_some(value))
        .flatten()
        .expect("managed command loader path");
    assert_eq!(
        std::env::split_paths(loader_path).collect::<Vec<_>>(),
        vec![library_dir.clone(), caller_library_dir.clone()],
        "an empty NIX_LD_LIBRARY_PATH must not add an empty loader-path entry",
    );

    std::env::remove_var("NIX_LD_LIBRARY_PATH");
    let mut command = std::process::Command::new("cargo");
    command.env(
        "LD_LIBRARY_PATH",
        std::env::join_paths([caller_library_dir.as_path()]).expect("path"),
    );
    apply_managed_toolchain_library_path_if_available(&mut command, &binary, &paths);
    let loader_path = command
        .get_envs()
        .find_map(|(key, value)| (key == "LD_LIBRARY_PATH").then_some(value))
        .flatten()
        .expect("managed command loader path");
    assert_eq!(
        std::env::split_paths(loader_path).collect::<Vec<_>>(),
        vec![library_dir, caller_library_dir],
        "an absent NIX_LD_LIBRARY_PATH must not add an empty loader-path entry",
    );
}

#[test]
fn caller_toolchain_does_not_receive_managed_library_path() {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Linux {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));
    let binary = temp.path().join("caller").join("bin").join("cargo");
    std::fs::create_dir_all(binary.parent().expect("binary parent")).expect("bin dir");
    std::fs::write(&binary, b"").expect("binary");

    let mut command = std::process::Command::new("cargo");
    apply_managed_toolchain_library_path_if_available(&mut command, &binary, &paths);

    assert!(
        command.get_envs().all(|(key, _)| key != "LD_LIBRARY_PATH"),
        "caller-owned toolchains must not inherit a managed loader path"
    );
}

/// soldr#3359: a binary inside a rustup toolchain maps to that toolchain's
/// own `lib` directory (the entry rustup's proxy prepends); anything else maps
/// to nothing.
#[test]
fn rustup_toolchain_library_dir_is_the_callers_own_toolchain_lib() {
    let root = tempfile::tempdir().expect("tempdir");
    let toolchain = root
        .path()
        .join(".rustup")
        .join("toolchains")
        .join("stable-aarch64-apple-darwin");
    std::fs::create_dir_all(toolchain.join("bin")).unwrap();
    let rustc = toolchain.join("bin").join("rustc");

    assert_eq!(rustup_toolchain_library_dir(&rustc), None, "no lib dir yet");
    std::fs::create_dir_all(toolchain.join("lib")).unwrap();
    assert_eq!(
        rustup_toolchain_library_dir(&rustc),
        Some(toolchain.join("lib"))
    );

    let cargo_home_proxy = root.path().join(".cargo").join("bin").join("cargo");
    assert_eq!(rustup_toolchain_library_dir(&cargo_home_proxy), None);
    let loose = root.path().join("bin").join("rustc");
    assert_eq!(rustup_toolchain_library_dir(&loose), None);
}

#[test]
fn prepend_loader_library_path_keeps_existing_entries_after_the_toolchain() {
    let mut command = std::process::Command::new("unused");
    let existing =
        std::env::join_paths([PathBuf::from("/first"), PathBuf::from("/second")]).expect("join");
    command.env("SOLDR_TEST_LOADER_PATH", &existing);

    prepend_loader_library_path(
        &mut command,
        "SOLDR_TEST_LOADER_PATH",
        PathBuf::from("/toolchain/lib"),
    );

    let value = command
        .get_envs()
        .find(|(key, _)| *key == "SOLDR_TEST_LOADER_PATH")
        .and_then(|(_, value)| value)
        .expect("variable set");
    assert_eq!(
        std::env::split_paths(value).collect::<Vec<_>>(),
        vec![
            PathBuf::from("/toolchain/lib"),
            PathBuf::from("/first"),
            PathBuf::from("/second")
        ]
    );
}
