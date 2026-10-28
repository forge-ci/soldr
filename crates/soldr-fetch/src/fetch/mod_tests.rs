//! Unit coverage split from `mod.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;

/// #1879: build a fake tool cache and confirm the fallback picks the
/// newest *usable* version without touching the network.
#[test]
fn newest_cached_tool_picks_the_highest_complete_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(dir.path().to_path_buf());
    paths.ensure_dirs().expect("ensure dirs");
    let target = TargetTriple::host().expect("host triple");
    let ext = target.binary_ext();

    for version in ["1.9.0", "1.10.0"] {
        let tool_dir = paths.bin.join(format!("maturin-{version}"));
        std::fs::create_dir_all(&tool_dir).expect("tool dir");
        std::fs::write(tool_dir.join(format!("maturin{ext}")), b"bin").expect("bin");
    }
    // Newest by version, but an interrupted extraction left it with
    // no binary — it must be skipped rather than returned.
    std::fs::create_dir_all(paths.bin.join("maturin-2.0.0")).expect("empty dir");
    // A different tool must not be considered.
    let other = paths.bin.join("crgx-9.9.9");
    std::fs::create_dir_all(&other).expect("other dir");
    std::fs::write(other.join(format!("crgx{ext}")), b"bin").expect("other bin");

    let found = newest_cached_tool(&paths, "maturin", &["maturin"], &target)
        .expect("a cached maturin must be found");
    // 1.10.0 > 1.9.0 under semver; plain string ordering would have
    // picked 1.9.0.
    assert_eq!(found.version, "1.10.0");
    assert!(found.cached);
}

#[test]
fn newest_cached_tool_returns_none_when_nothing_is_cached() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(dir.path().to_path_buf());
    paths.ensure_dirs().expect("ensure dirs");
    assert!(
        newest_cached_tool(
            &paths,
            "maturin",
            &["maturin"],
            &TargetTriple::host().expect("host triple")
        )
        .is_none(),
        "with no cache there is nothing to fall back to, so the \
             lookup failure must stay fatal",
    );
}

#[test]
fn transient_fetch_predicate_only_retries_network_and_not_found() {
    // `is_transient_fetch_error` gates the retry loop inside
    // `fetch_repo_binary_with_paths`. The two transient classes —
    // GitHub-Releases 404 during propagation, network hiccups — must
    // retry; everything else (malformed archive, missing asset for
    // the target triple, IO errors) is terminal.
    assert!(is_transient_fetch_error(&SoldrError::ToolNotFound(
        "no release found for yfedoseev/crgx".into(),
    )));
    assert!(is_transient_fetch_error(&SoldrError::Network(
        "github api unavailable".into(),
    )));
    assert!(!is_transient_fetch_error(&SoldrError::Archive(
        "corrupt archive".into(),
    )));
    assert!(!is_transient_fetch_error(&SoldrError::Other(
        "no asset matches target x86_64-pc-windows-msvc".into(),
    )));
    assert!(!is_transient_fetch_error(&SoldrError::UnsupportedPlatform(
        "wasm32".into(),
    )));
}

// ── Bundled single-binary tools ─────────────────────────────────
// Locks the contract used by the npm shim and setup-soldr action:
// when the env var points at a directory containing the requested
// binary (`tool.exe` on Windows), `resolve_local_single_binary`
// returns that path verbatim with `cached: true` and a
// `local-<version>` label.

fn platform_binary_name(binary_stem: &str) -> String {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        format!("{binary_stem}.exe")
    } else {
        binary_stem.to_string()
    }
}

#[test]
fn resolve_local_crgx_returns_binary_path() {
    let tmp = tempfile::tempdir().unwrap();
    let stub = tmp.path().join(platform_binary_name("crgx"));
    std::fs::write(&stub, b"stub").unwrap();

    let result = resolve_local_single_binary(
        tmp.path(),
        CRGX_LOCAL_DIR_ENV_VAR,
        "crgx",
        MANAGED_CRGX_VERSION,
    )
    .expect("should resolve");
    assert_eq!(result.binary_path, stub);
    assert!(result.cached, "bundled path must report cached=true");
    assert!(
        result.version.starts_with("local-"),
        "version should be `local-<ver>`, got: {}",
        result.version
    );
    assert!(
        result.version.contains(MANAGED_CRGX_VERSION),
        "version should embed MANAGED_CRGX_VERSION, got: {}",
        result.version
    );
}

#[test]
fn resolve_local_cargo_chef_returns_binary_path() {
    let tmp = tempfile::tempdir().unwrap();
    let stub = tmp.path().join(platform_binary_name("cargo-chef"));
    std::fs::write(&stub, b"stub").unwrap();

    let result = resolve_local_single_binary(
        tmp.path(),
        CARGO_CHEF_LOCAL_DIR_ENV_VAR,
        "cargo-chef",
        CARGO_CHEF_PINNED_VERSION,
    )
    .expect("should resolve");
    assert_eq!(result.binary_path, stub);
    assert!(result.cached, "bundled path must report cached=true");
    assert_eq!(result.version, format!("local-{CARGO_CHEF_PINNED_VERSION}"));
}

#[test]
fn resolve_local_crgx_errors_when_dir_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let nonexistent = tmp.path().join("not-a-dir");

    let err = resolve_local_single_binary(
        &nonexistent,
        CRGX_LOCAL_DIR_ENV_VAR,
        "crgx",
        MANAGED_CRGX_VERSION,
    )
    .expect_err("missing dir should error");
    let msg = err.to_string();
    assert!(
        msg.contains("not a directory"),
        "error must explain the dir is missing, got: {msg}"
    );
}

#[test]
fn resolve_local_crgx_errors_when_binary_missing() {
    let tmp = tempfile::tempdir().unwrap();
    // Empty directory — no crgx binary inside.

    let err = resolve_local_single_binary(
        tmp.path(),
        CRGX_LOCAL_DIR_ENV_VAR,
        "crgx",
        MANAGED_CRGX_VERSION,
    )
    .expect_err("missing binary should error");
    let msg = err.to_string();
    assert!(
        msg.contains("is missing"),
        "error must explain the binary is missing, got: {msg}"
    );
    assert!(
        msg.contains(&platform_binary_name("crgx")),
        "error should name the expected binary, got: {msg}"
    );
}

#[test]
fn resolver_order_parses_default_and_subsets() {
    // Unset / empty / whitespace → all hops enabled.
    assert_eq!(ResolverOrder::parse(""), ResolverOrder::all());
    assert_eq!(ResolverOrder::parse("   "), ResolverOrder::all());
    // Subset selections — embed-only, live-only, api-only.
    let embed_only = ResolverOrder::parse("embed");
    assert!(embed_only.try_embed);
    assert!(!embed_only.try_live);
    assert!(!embed_only.try_api);
    let live_api = ResolverOrder::parse("live,api");
    assert!(!live_api.try_embed);
    assert!(live_api.try_live);
    assert!(live_api.try_api);
    let api_only = ResolverOrder::parse("api");
    assert!(!api_only.try_embed);
    assert!(!api_only.try_live);
    assert!(api_only.try_api);
    // Whitespace + case-insensitive + unknown-token tolerance.
    let mixed = ResolverOrder::parse(" EMBED , Live , garbage ");
    assert!(mixed.try_embed);
    assert!(mixed.try_live);
    assert!(!mixed.try_api);
}

#[test]
fn resolver_order_all_unknown_tokens_fall_back_to_all() {
    assert_eq!(ResolverOrder::parse("bogus"), ResolverOrder::all());
    assert_eq!(ResolverOrder::parse("embd, lvie"), ResolverOrder::all());
    assert_eq!(ResolverOrder::parse(",,"), ResolverOrder::all());
}

#[test]
fn api_hop_refused_when_try_api_false() {
    let order = ResolverOrder::parse("embed,live");
    let err = ensure_api_hop_permitted(order, "cargo-nextest").unwrap_err();
    assert!(
        err.to_string().contains("no resolver hop permitted"),
        "{err}"
    );
    assert!(err.to_string().contains("SOLDR_RESOLVER_ORDER"));
    assert!(ensure_api_hop_permitted(ResolverOrder::all(), "x").is_ok());
}

#[test]
fn resolver_order_env_var_skips_embed_when_unset() {
    // Defensive: ensure the canonical "all on" form keeps embed on.
    // The actual env-var override is exercised in the integration
    // test under `crates/soldr-cli/tests/fetch_tools/embed_first_resolver.rs`
    // (touching the live
    // process env from a unit test is racy).
    let order = ResolverOrder::all();
    assert!(order.try_embed && order.try_live && order.try_api);
}

#[test]
fn try_embedded_manifest_v6_misses_on_empty_blob() {
    use crate::core::SoldrPaths;
    // The shipped embed (before this PR's build-time refresh ran)
    // can legitimately be the empty envelope on a fresh checkout.
    // Either way, the lookup for a tool that doesn't exist must
    // return Ok(None) — never an error, never a panic.
    let tmp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let repo = github::RepoInfo {
        owner: "definitely-does-not-exist".to_string(),
        repo: "nor-does-this".to_string(),
    };
    let target = TargetTriple {
        arch: crate::core::Arch::X86_64,
        os: crate::core::Os::Linux,
        env: crate::core::Env::Gnu,
    };
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(try_embedded_manifest_v6(
            &paths,
            "nonesuch",
            &["nonesuch"],
            &repo,
            "1.0.0",
            None,
            &target,
        ))
        .expect("must return Ok, not Err");
    assert!(result.is_none(), "unknown tool must miss");
}

#[test]
fn annotate_release_fetch_error_includes_version_and_target() {
    let repo = github::RepoInfo {
        owner: "LukeMathWalker".to_string(),
        repo: "cargo-chef".to_string(),
    };
    let target = TargetTriple {
        arch: crate::core::Arch::Aarch64,
        os: crate::core::Os::MacOs,
        env: crate::core::Env::None,
    };

    let err = annotate_release_fetch_error(
        SoldrError::ToolNotFound("GitHub release lookup failed: HTTP 404 Not Found".to_string()),
        &repo,
        &VersionSpec::Exact("0.1.73".to_string()),
        &target,
    );
    let msg = err.to_string();

    assert!(msg.contains("LukeMathWalker/cargo-chef"));
    assert!(msg.contains("0.1.73"));
    assert!(msg.contains("aarch64-apple-darwin"));
    assert!(msg.contains("release lookup"));
}

#[test]
fn darwin_x64_catalogue_mappings_cover_blessed_mac_x86() {
    let target = "x86_64-apple-darwin";
    assert_eq!(zstd_sysroot::catalogue_slug_for(target), Some("darwin-x64"));
    assert_eq!(
        sqlite_sysroot::catalogue_slug_for(target),
        Some("darwin-x64")
    );
    assert_eq!(
        mimalloc_sysroot::catalogue_slug_for(target),
        Some("darwin-x64")
    );
    assert_eq!(
        zlib_ng_sysroot::catalogue_slug_for(target),
        Some("darwin-x64")
    );
    assert_eq!(lzma_sysroot::catalogue_slug_for(target), Some("darwin-x64"));
    assert_eq!(
        bzip2_sysroot::catalogue_slug_for(target),
        Some("darwin-x64")
    );
    assert_eq!(
        python_sysroot::catalogue_slug_for(target),
        Some("darwin-x64")
    );
    assert_eq!(cmake_tools::host_slug_for(target), Some("darwin-x64"));
    assert_eq!(uv_tool::host_slug_for(target), Some("darwin-x64"));
}

// soldr#936 smoke-test-or-evict regression tests.

#[test]
fn cargo_llvm_cov_smoke_uses_cargo_subcommand_protocol() {
    let target = TargetTriple::host().unwrap();
    let command = smoke_command(
        std::path::Path::new("cargo-llvm-cov"),
        "cargo-llvm-cov",
        &target,
        "--version",
    );
    let args: Vec<_> = command.get_args().collect();
    assert_eq!(args, ["llvm-cov", "--version"]);
    let help = smoke_command(
        std::path::Path::new("cargo-llvm-cov"),
        "cargo-llvm-cov",
        &target,
        "--help",
    );
    assert_eq!(help.get_args().collect::<Vec<_>>(), ["llvm-cov", "--help"]);
}

#[test]
fn smoke_test_missing_file_errors() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let bogus = tmp.path().join("not-a-binary");
    let err = smoke_test_or_evict(&bogus, "fake-tool", &TargetTriple::host().unwrap())
        .expect_err("missing file must fail smoke");
    assert!(
        err.to_string().contains("not a file after extract"),
        "expected 'not a file after extract' in error, got: {err}"
    );
}

#[test]
fn smoke_test_corrupt_shell_evicts_file() {
    // soldr#936's actual motivating case: a downloaded artifact
    // is actually a corrupted shell script that fails with
    // `4: Syntax error: ")" unexpected`. Smoke runs --version,
    // detects the failure, deletes the bogus file.
    let tmp = tempfile::tempdir().expect("tmpdir");
    let bogus_bin = tmp.path().join("cargo-zigbuild");
    std::fs::write(
        &bogus_bin,
        "#!/bin/sh\nsomething_that_will_definitely_fail_to_exec_or_be_a_command\nexit 1\n",
    )
    .unwrap();
    let source = std::fs::metadata(&bogus_bin).unwrap().permissions();
    crate::platform::fs::permissions::make_executable_from(&bogus_bin, &source).unwrap();

    let result = smoke_test_or_evict(&bogus_bin, "cargo-zigbuild", &TargetTriple::host().unwrap());
    // On Windows the shebang fails to execute at all → exec error
    // path. On Unix the script runs but exits 1 → exit-status
    // path. Both paths must evict + error.
    assert!(result.is_err(), "smoke test must fail for corrupt binary");
    assert!(
        !bogus_bin.is_file(),
        "smoke failure must evict the corrupted binary at {bogus_bin:?}"
    );
}

#[test]
fn dylint_link_smoke_sets_target_qualified_toolchain() {
    let target = TargetTriple::from_triple("x86_64-unknown-linux-gnu").unwrap();
    assert_eq!(
        smoke_rustup_toolchain("dylint-link", &target).as_deref(),
        Some("nightly-x86_64-unknown-linux-gnu")
    );
    assert_eq!(smoke_rustup_toolchain("cargo-dylint", &target), None);
}

#[test]
fn dylint_link_accepts_msvc_help_exit() {
    let output = b"Microsoft (R) Incremental Linker Version 14.44\r\n\
                       usage: LINK [options] [files]";
    assert!(dylint_link_help_output_is_valid(Some(1), output, b""));
    assert!(dylint_link_help_output_is_valid(Some(1100), output, b""));
    assert!(!dylint_link_help_output_is_valid(Some(2), output, b""));
    assert!(!dylint_link_help_output_is_valid(
        Some(1100),
        b"unrelated failure",
        b""
    ));
}

#[test]
fn smoke_help_argument_is_msvc_help_only_for_dylint_link() {
    // soldr#3274: the cargo front door's PATH validator must pick the same
    // argument this smoke test does. `--help` is forwarded to link.exe and
    // rejected by a healthy dylint-link.
    assert_eq!(smoke_help_argument("dylint-link"), "/?");
    assert_eq!(smoke_help_argument("cargo-dylint"), "--help");
    assert_eq!(smoke_help_argument("cargo-nextest"), "--help");
}
