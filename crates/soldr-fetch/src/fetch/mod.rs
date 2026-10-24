//! Binary resolution and download for soldr.
//!
//! Resolution chain (Phase 1 MVP):
//! 1. Local cache (`~/.soldr/bin/<tool>-<version>/`)
//! 2. GitHub Releases (repository URL from crates.io, or override from `known_tools`)
//! 3. QuickInstall prebuilt index (exact versions only; trust-unverified
//!    unless pinned) -- see [`quickinstall`] (soldr#3700); before it, the
//!    crate's own `[package.metadata.binstall]` pkg-url -- see [`binstall`]
//!    (soldr#3726)

pub mod binstall;
pub(crate) mod binstall_signing;
pub(crate) mod binstall_template;
pub mod known_tools;
pub mod quickinstall;
mod resolver_order;
pub(crate) use resolver_order::ensure_api_hop_permitted;
pub use resolver_order::{ResolverOrder, RESOLVER_ORDER_ENV_VAR};

pub use known_tools::{
    known_cargo_subcommands, lookup_by_cargo_subcommand, lookup_by_crate, wraps_inner_cargo_build,
    ToolSpec, CARGO_CHEF_PINNED_VERSION, CARGO_NEXTEST_PINNED_VERSION, KNOWN_TOOLS,
};

pub mod trust;

pub use trust::{
    sha256_of, verify_download, PinnedChecksumStore, TrustMode, VerifyOutcome,
    CHECKSUMS_FILE_ENV_VAR, TRUST_MODE_ENV_VAR,
};

pub mod rustup_init;

/// soldr#2132: shared retry-with-backoff for network fetches.
/// soldr#2310 — GitHub ref/release resolution for `soldr install`.
pub mod install_api;
mod net_guard;
pub(crate) mod retry;
pub(crate) mod segmented_download;
/// soldr#2310 — whole-source-tree acquisition via GitHub codeload zips.
pub mod source_zip;
pub(crate) mod stream_download;
pub use stream_download::pypi_has_version;

pub use rustup_init::{
    auto_bootstrap_if_missing, auto_bootstrap_if_missing_blocking, bootstrap_rustup,
    bootstrap_rustup_blocking, discover_rustup, managed_cargo_home, managed_rustup_home,
    managed_rustup_path, rustup_init_download_url, rustup_init_host_triple, AutoBootstrapOutcome,
    BootstrapReport, NO_BOOTSTRAP_ENV_VAR, RUSTUP_INIT_TRIPLE_ENV_VAR, RUSTUP_INIT_URL_ENV_VAR,
};

pub mod apple_sdk;
pub mod archive;
/// soldr#988 Phase 4 — on-demand builds via `zackees/forge` when the
/// toolchain catalogue lookup misses. See module doc for the env-var
/// contract and failure surface.
pub mod forge_dispatch;
pub mod github;
pub mod llvm;
pub mod llvm_background;
/// soldr#997 Phase A — LLVM-tools bundle from soldr-toolchain
/// `recipes/llvm-tools-linux-x64/`. See module doc for why this is
/// distinct from [`llvm`].
pub mod llvm_host_libs;
pub mod llvm_tools_bundle;
pub mod manifest_lookup;
pub mod manifest_v6;
/// soldr#997 Phase A — Node.js node.lib bundle for Windows MSVC cross (#944).
pub mod nodelib;
/// soldr#3246 — managed static, source-built OpenSSL sysroot for every
/// syslib shape, exposed to `openssl-sys` by `blessed_build::prepare`.
pub mod openssl_sysroot;
/// Target Python compatibility assets for older PyO3, embedding, and other
/// explicitly selected build shapes. Ordinary ABI3 extension cross-builds do
/// not fetch this bundle (soldr#1610).
pub mod python_sysroot;
mod toolchain_packaged;
pub use toolchain_packaged::ensure_dylint_driver;
/// soldr#1012 PR 5 — xwin-cache catalogue for blessed `*-pc-windows-msvc` cross.
pub mod xwin_cache;
pub mod xwin_case_overlay;
mod xwin_header_names;
// soldr#1064 Phase B — *-sys C library catalogue distribution.
// Each module is a stub-until-ingested consumer modeled on
// openssl_sysroot.rs. Once the soldr-toolchain forge dispatches
// land the assets, blessed_build::prepare wires the corresponding
// _SYSTEM / _OVERRIDE env vars onto the child cargo invocation so
// the *-sys crate's build.rs skips its vendored compile.
pub mod bzip2_sysroot;
/// Managed CMake + Ninja host-tool bundles — blessed builds export
/// `CMAKE` / `CMAKE_GENERATOR=Ninja` from these instead of trusting
/// whatever cmake/make the system PATH resolves. Stub-until-ingested
/// consumer like the *-sys sysroots below.
pub mod catalogue_linux_host;
pub mod cmake_tools;
pub mod gnu_linux_toolchain;
pub mod libuuid_sysroot;
pub mod lzma_sysroot;
pub mod mimalloc_sysroot;
pub mod mingw_w64_gcc;
/// soldr#2292 / soldr#1079 — managed MSVC toolset bundle, the
/// download-fallback half of native Windows MSVC host discovery.
/// Stub-until-ingested consumer like the *-sys sysroots and
/// [`cmake_tools`] above; see `soldr_cli::msvc_host` for the caller
/// that decides host-vs-download.
pub mod msvc_toolset;
pub mod musl_linux_toolchain;
pub mod reld;
pub mod sqlite_sysroot;
/// soldr#1064 Phase B — shared download + extract for the six
/// *-sys C library catalogue bundles. The per-lib modules each
/// call this helper with their own `(lib, version, slug)` tuple.
pub mod syslib_common;
/// soldr#2300 — tar extraction that materializes symlink entries with
/// the correct NTFS flavor on Windows (dir links must be `symlink_dir`
/// or they are non-traversable). Byte-identical to `tar::Archive::unpack`
/// on non-Windows.
pub mod tar_extract;
/// soldr#1264 follow-on — manual uv-provisioned maturin env (fallback
/// when the prebuilt maturin binary fetch misses). Hand-rolled on
/// purpose; see module doc for why not the `uv-iso-env` package.
pub mod uv_env;
/// soldr#1264 follow-on — managed `uv` bundle; first consumer: [`uv_env`].
pub mod uv_tool;
pub mod zig;
pub mod zlib_ng_sysroot;
pub mod zstd_sysroot;

pub use apple_sdk::{ensure_apple_sdk, MANAGED_APPLE_SDK_VERSION};
pub use llvm::{ensure_llvm_toolchain, MANAGED_LLVM_VERSION};
pub use llvm_background::{
    check_managed_llvm_host, llvm_fetch_wait_from_env, llvm_install_complete, managed_llvm_bin_dir,
    managed_llvm_install_dir, start_background_llvm_fetch, wait_for_managed_llvm,
    LLVM_FETCH_WAIT_ENV_VAR,
};
pub use manifest_lookup::{
    fetch_verified_catalogue_asset, supports_min_client_version, AssetTransport, ManifestEntry,
    ManifestIndex, Part, CATALOGUE_CAPABILITY, CATALOGUE_DOC_NAME, CATALOGUE_V2_DOC_NAME,
    DEFAULT_TOOLCHAIN_ORIGIN, MANIFEST_DISABLE_ENV_VAR, MANIFEST_FETCH_TIMEOUT,
    TOOLCHAIN_CATALOGUE_URL_ENV_VAR, TOOLCHAIN_CATALOGUE_V2_URL_ENV_VAR, TOOLCHAIN_ORIGIN_ENV_VAR,
};
pub use manifest_v6::{
    embedded_manifest, embedded_manifest_bytes, is_stable_version_tag, ManifestV6, V6Asset, V6Hit,
    V6Leaf,
};
pub use reld::{ensure_reld, MANAGED_RELD_VERSION, RELD_BIN_ENV_VAR};
pub use zig::{ensure_zig, MANAGED_ZIG_VERSION};

use crate::core::{SoldrError, SoldrPaths, TargetTriple};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum VersionSpec {
    Latest,
    Exact(String),
}

impl VersionSpec {
    pub fn parse(s: &str) -> Self {
        if s.is_empty() || s == "latest" {
            Self::Latest
        } else {
            Self::Exact(s.to_string())
        }
    }
}

#[derive(Debug)]
pub struct FetchResult {
    pub binary_path: PathBuf,
    pub version: String,
    pub cached: bool,
}

/// Back-compat re-export: the const lives in `core` (no `core`->`fetch` edge, #1490).
pub use crate::core::MANAGED_SHIM_VERSION;
/// Pinned crgx version that soldr's release pipeline source-builds and
/// bundles into the combined `.tar.zst` archive (see PR follow-up to
/// #434 — combined archive now ships zccache + crgx). Also surfaced
/// via `lookup_by_crate("crgx").pinned_version` so the
/// `fetch_repo_binary` fallback resolves the same version when the
/// bundled binary is not available.
pub const MANAGED_CRGX_VERSION: &str = "0.1.0";

/// Pinned `maturin` release used by both the `soldr maturin <args>`
/// CLI dispatch and the PEP 517 build backend in `src/soldr/__init__.py`.
/// Surfaced via `lookup_by_crate("maturin").pinned_version` so every
/// resolution path lands on the same upstream version — keeping the
/// Python+Rust packaging behavior reproducible across machines.
///
/// Lockstep bookkeeping (per CLAUDE.md "Bumping managed_zccache_version"):
/// when bumping, also update `contracts/zccache-runtime.v1.json`'s
/// `maturin.managed_version`; the
/// `zccache_runtime_contract_matches_rust_constants` test asserts they
/// match and panics with a directive message if they drift.
pub const MANAGED_MATURIN_VERSION: &str = "1.14.1.post1";
pub const MANAGED_MATURIN_PYPI_PACKAGE: &str = "soldr-maturin";

/// Override the runtime crgx resolution: when set, soldr uses the
/// `crgx` (or `crgx.exe`) binary in this directory ahead of any
/// GitHub-Releases / crates.io fetch. The npm shim (`bin/soldr.js`)
/// and the setup-soldr action point this at the directory containing
/// the bundled binary so the first `soldr crgx ...` call needs no
/// network round trip. Mirrors `SOLDR_ZCCACHE_LOCAL_DIR`.
pub const CRGX_LOCAL_DIR_ENV_VAR: &str = "SOLDR_CRGX_LOCAL_DIR";

/// Override the runtime cargo-chef resolution: when set, soldr uses the
/// `cargo-chef` (or `cargo-chef.exe`) binary in this directory ahead of
/// any GitHub-Releases fetch. Soldr release archives and setup-soldr
/// point this at the bundled cargo-chef binary so `soldr cook` is
/// deterministic on targets upstream does not publish, notably
/// `aarch64-apple-darwin`.
pub const CARGO_CHEF_LOCAL_DIR_ENV_VAR: &str = "SOLDR_CARGO_CHEF_LOCAL_DIR";

/// Retry budget for the GitHub-Releases + crates.io fetch chain inside
/// `fetch_repo_binary_with_paths`. Transient errors
/// (`SoldrError::Network`, `SoldrError::ToolNotFound`) retry up to
/// `REPO_FETCH_ATTEMPTS` times with exponential backoff starting at
/// `REPO_FETCH_INITIAL_BACKOFF`. The same values previously lived as
/// `MANAGED_ZCCACHE_FETCH_*` constants used at a single call site; they
/// now apply to every fetch in soldr (zccache, crgx, cargo-chef, every
/// ecosystem tool) so users and CI don't have to babysit transient
/// GitHub API hiccups.
const REPO_FETCH_ATTEMPTS: u32 = 4;
const REPO_FETCH_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);

/// Fetch a tool binary for the current platform.
pub async fn fetch_tool(
    crate_name: &str,
    version: &VersionSpec,
) -> Result<FetchResult, SoldrError> {
    let paths = SoldrPaths::new()?;
    fetch_tool_with_paths(crate_name, version, &paths).await
}

/// Fetch with explicit paths (useful for testing).
pub async fn fetch_tool_with_paths(
    crate_name: &str,
    version: &VersionSpec,
    paths: &SoldrPaths,
) -> Result<FetchResult, SoldrError> {
    let target = TargetTriple::detect()?;
    fetch_tool_for_target_with_paths(crate_name, version, paths, target).await
}

/// Fetch a tool binary for the currently-running host, ignoring any project
/// build target override. This is for host-executed tools such as Cargo
/// subcommands; `cargo-zigbuild` must match the runner that executes it even
/// when the Rust build target is `aarch64-unknown-linux-musl`.
pub async fn fetch_tool_for_host_with_paths(
    crate_name: &str,
    version: &VersionSpec,
    paths: &SoldrPaths,
) -> Result<FetchResult, SoldrError> {
    let target = TargetTriple::host()?;
    fetch_tool_for_target_with_paths(crate_name, version, paths, target).await
}

async fn fetch_tool_for_target_with_paths(
    crate_name: &str,
    version: &VersionSpec,
    paths: &SoldrPaths,
    target: TargetTriple,
) -> Result<FetchResult, SoldrError> {
    paths.ensure_dirs()?;

    // Bundled single-binary escape hatches: when the npm shim or
    // setup-soldr action ships a bundled tool, they export a local-dir
    // env var pointing at the directory holding the binary. Honor it
    // ahead of every other resolution path so first-use carries no
    // network round trip. Mirrors the SOLDR_ZCCACHE_LOCAL_DIR contract
    // used by the bundled zccache trio (#434).
    if crate_name == "cargo-chef" {
        if let Some(local_dir) = non_empty_env_path(CARGO_CHEF_LOCAL_DIR_ENV_VAR) {
            return resolve_local_single_binary(
                &local_dir,
                CARGO_CHEF_LOCAL_DIR_ENV_VAR,
                "cargo-chef",
                CARGO_CHEF_PINNED_VERSION,
            );
        }
    }
    if crate_name == "crgx" {
        if let Some(local_dir) = non_empty_env_path(CRGX_LOCAL_DIR_ENV_VAR) {
            return resolve_local_single_binary(
                &local_dir,
                CRGX_LOCAL_DIR_ENV_VAR,
                "crgx",
                MANAGED_CRGX_VERSION,
            );
        }
    }

    if let Some(spec) = lookup_by_crate(crate_name) {
        let repo = match spec.repo {
            Some((owner, name)) => github::RepoInfo {
                owner: owner.to_string(),
                repo: name.to_string(),
            },
            None => github::resolve_repo(crate_name).await?,
        };
        // Honor `spec.pinned_version` when the caller didn't pass an
        // explicit `@version` — mirrors the cargo-front-door's
        // existing pin substitution so `soldr crgx` and
        // `soldr cargo chef` resolve the same way from both entry
        // points. Lets the `External` dispatch in main.rs go through
        // the registry pin without each call site re-implementing it.
        let effective_version = match (version, spec.pinned_version) {
            (VersionSpec::Latest, Some(pin)) => VersionSpec::Exact(pin.to_string()),
            _ => version.clone(),
        };
        return fetch_repo_binary_with_paths(
            spec.crate_name,
            &[spec.binary_name],
            &repo,
            &effective_version,
            spec.tag_prefix,
            paths,
            target,
        )
        .await;
    }

    let repo = github::resolve_repo(crate_name).await?;
    fetch_repo_binary_with_paths(
        crate_name,
        &[crate_name],
        &repo,
        version,
        None,
        paths,
        target,
    )
    .await
}

/// Resolve a bundled single-binary tool from a local directory env var.
/// Returns the path verbatim — no copy, no content hashing — because
/// tools such as crgx and cargo-chef have no daemon/sidecar siblings
/// (contrast with `resolve_local_zccache`, which normalizes a trio plus
/// debug-info sidecars).
fn resolve_local_single_binary(
    local_dir: &Path,
    env_var: &str,
    binary_stem: &str,
    version: &str,
) -> Result<FetchResult, SoldrError> {
    if !local_dir.is_dir() {
        return Err(SoldrError::Other(format!(
            "{env_var}={} is not a directory",
            local_dir.display(),
        )));
    }
    let binary_name =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            format!("{binary_stem}.exe")
        } else {
            binary_stem.to_string()
        };
    let binary = local_dir.join(binary_name);
    if !binary.is_file() {
        return Err(SoldrError::Other(format!(
            "{env_var}={} but {} is missing",
            local_dir.display(),
            binary.display(),
        )));
    }
    Ok(FetchResult {
        binary_path: binary,
        version: format!("local-{version}"),
        cached: true,
    })
}

pub(crate) fn non_empty_env_path(env_var: &str) -> Option<PathBuf> {
    let value = std::env::var_os(env_var)?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

pub(super) async fn fetch_repo_binary_with_paths(
    cache_name: &str,
    binary_names: &[&str],
    repo: &github::RepoInfo,
    version: &VersionSpec,
    tag_prefix: Option<&str>,
    paths: &SoldrPaths,
    target: TargetTriple,
) -> Result<FetchResult, SoldrError> {
    let mut backoff = REPO_FETCH_INITIAL_BACKOFF;
    let mut attempt: u32 = 1;
    loop {
        match fetch_repo_binary_once(
            cache_name,
            binary_names,
            repo,
            version,
            tag_prefix,
            paths,
            &target,
        )
        .await
        {
            Ok(result) => return Ok(result),
            Err(err) if attempt < REPO_FETCH_ATTEMPTS && is_transient_fetch_error(&err) => {
                eprintln!(
                    "soldr: transient error fetching {cache_name} from {}/{} (attempt {attempt}/{}): {err}; retrying in {:?}",
                    repo.owner, repo.repo, REPO_FETCH_ATTEMPTS, backoff,
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2);
                attempt += 1;
            }
            Err(err) => {
                // #1879: reaching the release API is an optimization, not
                // a prerequisite. `VersionSpec::Latest` means "the newest
                // you can get"; when the listing is unreachable — GitHub
                // rate-limits unauthenticated CI runners at 60/hr per IP,
                // shared across every job on that runner, so 403 is
                // routine — the newest we can get is what is already on
                // disk. Failing the build instead turns a transient API
                // condition into a red build for a consumer whose change
                // had nothing to do with the tool.
                //
                // Only for `Latest`. An `Exact` request was already
                // checked against the cache before the network hop, so a
                // miss there means that specific version is genuinely
                // absent, and substituting a different one would silently
                // violate the request.
                if matches!(version, VersionSpec::Latest) {
                    if let Some(cached) =
                        newest_cached_tool(paths, cache_name, binary_names, &target)
                    {
                        eprintln!(
                            "soldr: {cache_name}: release lookup failed ({err}); \
                             falling back to cached v{} (set SOLDR_GITHUB_TOKEN to raise \
                             the GitHub API rate limit)",
                            cached.version,
                        );
                        return Ok(cached);
                    }
                }
                return Err(err);
            }
        }
    }
}

/// Newest already-extracted version of `cache_name` in `paths.bin`.
///
/// The cache lays tools out as `<bin>/<cache_name>-<version>/`, so this
/// enumerates that directory rather than consulting the network. Used
/// only as the #1879 fallback when the release listing is unreachable.
///
/// Versions are ordered with semver when both sides parse, so `0.10.0`
/// sorts above `0.9.0`. Non-semver tags (some tools ship `2024-01-05`
/// or bare `v3`) fall back to string ordering, which is not always
/// correct but is deterministic and only ever picks among versions the
/// user already downloaded.
fn newest_cached_tool(
    paths: &SoldrPaths,
    cache_name: &str,
    binary_names: &[&str],
    target: &TargetTriple,
) -> Option<FetchResult> {
    let prefix = format!("{cache_name}-");
    let mut versions: Vec<String> = std::fs::read_dir(&paths.bin)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            if !entry.file_type().ok()?.is_dir() {
                return None;
            }
            entry
                .file_name()
                .to_str()?
                .strip_prefix(&prefix)
                .map(str::to_string)
        })
        .collect();

    versions.sort_by(
        |a, b| match (semver::Version::parse(a), semver::Version::parse(b)) {
            (Ok(a), Ok(b)) => a.cmp(&b),
            _ => a.cmp(b),
        },
    );

    // Newest first, and take the newest that actually has its binaries —
    // an interrupted extraction can leave a version directory behind
    // with nothing usable in it.
    versions.into_iter().rev().find_map(|version| {
        check_cache(paths, cache_name, &version, binary_names, target)
            .ok()
            .flatten()
    })
}

/// Classify whether an error from the repo-fetch chain is worth retrying.
/// `ToolNotFound` covers release-propagation windows (a tag or asset not
/// published yet) + crates.io index-not-ready windows; a release-lookup 404
/// is `ReleaseNotFound` and terminal (soldr#3911); `Network` covers DNS
/// blips, connection resets, and TLS handshake failures. Anything else
/// (malformed JSON, archive extraction errors, IO errors) is treated as
/// terminal — retrying wouldn't help.
fn is_transient_fetch_error(err: &SoldrError) -> bool {
    retry::is_transient(err)
}

/// One attempt at the full repo-binary fetch pipeline. Cache check →
/// release lookup → asset match → download + extract. Wrapped in
/// `fetch_repo_binary_with_paths`'s retry loop so transient failures
/// don't bubble up to the user.
async fn fetch_repo_binary_once(
    cache_name: &str,
    binary_names: &[&str],
    repo: &github::RepoInfo,
    version: &VersionSpec,
    tag_prefix: Option<&str>,
    paths: &SoldrPaths,
    target: &TargetTriple,
) -> Result<FetchResult, SoldrError> {
    paths.ensure_dirs()?;
    if binary_names.is_empty() {
        return Err(SoldrError::Other(format!(
            "no binary names configured for {cache_name}"
        )));
    }

    if let VersionSpec::Exact(ref v) = version {
        if let Some(r) = check_cache(paths, cache_name, v, binary_names, target)? {
            return Ok(r);
        }
    }

    // Resolver order (issue #873): embed → live → api. All three
    // hops are gated by `SOLDR_RESOLVER_ORDER` for testing + debugging.
    // The embedded blob is a zstd-compressed checked-in snapshot (see
    // build.rs + manifest_v6), so a hit short-circuits all network
    // traffic. The live catalogue fetch remains as a safety net for
    // assets published after the snapshot was updated. Both feed the
    // same `archive::download_and_extract_with_pin` sha-pin verification
    // path.
    let order = ResolverOrder::from_env();
    if order.try_embed {
        if let VersionSpec::Exact(ref tag) = version {
            if let Some(result) = try_embedded_manifest_v6(
                paths,
                cache_name,
                binary_names,
                repo,
                tag,
                tag_prefix,
                target,
            )
            .await?
            {
                return Ok(result);
            }
        }
    }
    if order.try_live {
        // Manifest-first (live): when we have an exact version pinned,
        // consult soldr's published asset index on the `manifest`
        // branch. The manifest index carries pinned sha256s, so a hit
        // short-circuits both the release lookup AND the
        // permissive-mode "unverified" warning — the URL we install
        // from is the one the manifest publishes and the bytes are
        // verified against the manifest's pin. Misses + every failure
        // mode (404, parse error, network, SOLDR_MANIFEST_DISABLE=1)
        // fall through to the live GitHub Releases API path below.
        if let VersionSpec::Exact(ref tag) = version {
            if let Some(result) =
                toolchain_packaged::try_binary(paths, cache_name, binary_names, tag, target).await?
            {
                return Ok(result);
            }
            if let Some(result) = try_manifest_first(
                paths,
                cache_name,
                binary_names,
                repo,
                tag,
                tag_prefix,
                target,
            )
            .await?
            {
                return Ok(result);
            }
        }
    }

    // soldr#3700: after the API hop misses (or is excluded), an exact
    // version may still resolve through the QuickInstall hop.
    let api_result = match ensure_api_hop_permitted(order, cache_name) {
        Ok(()) => {
            fetch_via_release_api(
                cache_name,
                binary_names,
                repo,
                version,
                tag_prefix,
                paths,
                target,
            )
            .await
        }
        Err(err) => Err(err),
    };
    match api_result {
        Ok(result) => Ok(result),
        Err(err) => {
            // soldr#3726: the crate's own binstall metadata before QuickInstall.
            if let Some(result) = binstall::fallback_after_api(
                order,
                paths,
                cache_name,
                binary_names,
                version,
                tag_prefix,
                target,
            )
            .await?
            {
                return Ok(result);
            }
            quickinstall::fallback_after_api(
                order,
                err,
                paths,
                cache_name,
                binary_names,
                version,
                tag_prefix,
                target,
            )
            .await
        }
    }
}

/// The unpinned GitHub Releases API hop of [`fetch_repo_binary_once`].
async fn fetch_via_release_api(
    cache_name: &str,
    binary_names: &[&str],
    repo: &github::RepoInfo,
    version: &VersionSpec,
    tag_prefix: Option<&str>,
    paths: &SoldrPaths,
    target: &TargetTriple,
) -> Result<FetchResult, SoldrError> {
    let release = github::fetch_release(repo, version, tag_prefix)
        .await
        .map_err(|err| annotate_release_fetch_error(err, repo, version, target))?;

    if let Some(r) = check_cache(paths, cache_name, &release.version, binary_names, target)? {
        return Ok(r);
    }

    let asset =
        github::match_asset_for_binaries(&release.assets, target, binary_names).map_err(|err| {
            match err {
                SoldrError::UnsupportedPlatform(message) => {
                    SoldrError::UnsupportedPlatform(format!(
                        "asset matching failed for {}/{} version {} target {}: {message}",
                        repo.owner,
                        repo.repo,
                        release.version,
                        target.triple()
                    ))
                }
                other => other,
            }
        })?;

    // soldr#1790: time the actual network download (not the local-cache
    // hits above) for the always-on per-build XML log's download
    // section.
    let download_started_at_ms = current_unix_ms();
    let download_started = std::time::Instant::now();
    let binary_path = archive::download_and_extract(
        paths,
        cache_name,
        &release.version,
        &asset.download_url,
        target,
        binary_names,
    )
    .await?;
    soldr_core::build_log_meta::fetch_timing::record(
        soldr_core::build_log_meta::fetch_timing::FetchTiming {
            name: cache_name.to_string(),
            source: "github-release".to_string(),
            started_at_ms: download_started_at_ms,
            duration_ms: download_started.elapsed().as_millis() as u64,
        },
    );

    // soldr#936: post-extract smoke test. Confirms the binary is
    // actually executable + not a corrupted-shell-script masquerade
    // (the `4: Syntax error: ")" unexpected` failure mode that
    // motivated this issue). Failure deletes the extracted artifact
    // and bubbles up a clear error so the caller can retry or fail
    // loudly instead of silently caching a broken binary.
    smoke_test_or_evict(&binary_path, cache_name, target)?;

    Ok(FetchResult {
        binary_path,
        version: release.version,
        cached: false,
    })
}

/// The `RUSTUP_TOOLCHAIN` identity a validation probe of `cache_name` must
/// run under, if any.
///
/// Public so the cargo front door's PATH-pair validator uses this exact
/// identity instead of re-spelling the literal (soldr#3274).
pub fn smoke_rustup_toolchain(cache_name: &str, target: &TargetTriple) -> Option<String> {
    // `dylint-link` is a transparent linker wrapper. It requires this
    // variable before it will forward even `--version` / `--help` to the
    // underlying linker. The smoke probe does not compile anything, so a
    // target-qualified synthetic nightly identity is sufficient and avoids
    // rejecting the healthy official release binary.
    (cache_name == "dylint-link").then(|| format!("nightly-{}", target.triple()))
}

fn smoke_command(
    binary_path: &std::path::Path,
    cache_name: &str,
    target: &TargetTriple,
    arg: &str,
) -> std::process::Command {
    let mut command = std::process::Command::new(binary_path);
    command.arg(arg);
    if let Some(toolchain) = smoke_rustup_toolchain(cache_name, target) {
        command.env("RUSTUP_TOOLCHAIN", toolchain);
    }
    command
}

/// A `--version`/`--help` probe that cannot finish in this long is broken
/// (a hang, or a binary waiting on stdin); it is evicted like any failure.
const SMOKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Run one smoke probe with stdin closed under a wall-clock deadline; a raw
/// `.output()` here could hang the fetch forever (soldr#3836 audit).
fn smoke_output(
    binary_path: &std::path::Path,
    cache_name: &str,
    target: &TargetTriple,
    arg: &str,
) -> Result<std::process::Output, SoldrError> {
    let mut command = smoke_command(binary_path, cache_name, target, arg);
    command.stdin(std::process::Stdio::null());
    crate::core::command_output_with_timeout_duration(
        &mut command,
        &format!("smoke test {cache_name} {arg}"),
        SMOKE_TIMEOUT,
    )
}

/// Validate an installed tool binary and evict it when it cannot execute.
///
/// soldr#936: probe a freshly-extracted binary with `--version` (falling back
/// to [`smoke_help_argument`]). On failure the extracted file is deleted so the
/// next fetch attempt does a clean re-download rather than reading the corrupt
/// artifact back out of the cache.
///
/// Most callers use this immediately after extraction. It is public within
/// the unpublished workspace so higher-level selectors can also revalidate
/// cache and catalogue hits whose host compatibility may change independently
/// of their archive integrity.
pub fn smoke_test_or_evict(
    binary_path: &std::path::Path,
    cache_name: &str,
    target: &TargetTriple,
) -> Result<(), SoldrError> {
    if !binary_path.is_file() {
        return Err(SoldrError::Other(format!(
            "smoke test: {cache_name} binary at {} is not a file after extract",
            binary_path.display()
        )));
    }

    let output = smoke_output(binary_path, cache_name, target, "--version");

    let evict = |reason: &str| {
        eprintln!(
            "soldr: smoke test FAILED for {cache_name} at {}: {reason} — evicting from cache",
            binary_path.display()
        );
        let _ = std::fs::remove_file(binary_path);
    };

    match output {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            // Some tools (e.g. unusual subcommand stubs) don't support
            // --version cleanly. Fall through to --help as the
            // second-chance probe before evicting.
            let help_argument = smoke_help_argument(cache_name);
            let help_ok = smoke_output(binary_path, cache_name, target, help_argument)
                .map(|help| {
                    help.status.success()
                        || (cache_name == "dylint-link"
                            && dylint_link_help_output_is_valid(
                                help.status.code(),
                                &help.stdout,
                                &help.stderr,
                            ))
                })
                .unwrap_or(false);
            if help_ok {
                Ok(())
            } else {
                evict(&format!(
                    "exit status {} on --version (stderr: {})",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
                Err(SoldrError::Other(format!(
                    "smoke test failed: {cache_name} binary at {} \
                     did not respond to --version / --help — likely \
                     a corrupted download (see soldr#936)",
                    binary_path.display()
                )))
            }
        }
        Err(err) => {
            evict(&format!("exec failed: {err}"));
            Err(SoldrError::Other(format!(
                "smoke test failed: could not exec {cache_name} at {}: {err}",
                binary_path.display()
            )))
        }
    }
}

/// The second-chance help probe argument for `cache_name`.
///
/// `dylint-link` forwards arguments to the platform linker. MSVC `link.exe`
/// spells help `/?` and deliberately exits 1100 after printing its banner and
/// usage, so success there is determined from that recognizable output rather
/// than the process status alone (see [`dylint_link_help_output_is_valid`]).
/// Unix linkers normally pass the `--version` probe and never reach this
/// argument.
///
/// Public so the cargo front door's PATH-pair validator selects the same
/// argument this smoke test does — soldr#3274 was caused by a second
/// validator picking `--help`, which a correct `dylint-link` cannot pass.
pub fn smoke_help_argument(cache_name: &str) -> &'static str {
    if cache_name == "dylint-link" {
        "/?"
    } else {
        "--help"
    }
}

/// Whether a `dylint-link` help probe's result is the healthy MSVC response.
///
/// `dylint-link` forwards to `link.exe`, which prints its banner plus
/// `usage: LINK` and exits non-zero for `/?`, so a *correct* binary cannot
/// exit 0 here. Public because the cargo front door's PATH-pair validator
/// must use this one predicate rather than a generic `--version`/`--help`
/// success check (soldr#3274); a duplicated rule is exactly what regressed.
pub fn dylint_link_help_output_is_valid(
    status_code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
) -> bool {
    // MSVC itself returns 1100; dylint-link's command adapter normalizes that
    // non-zero child status to 1 while preserving the banner and usage text.
    if !matches!(status_code, Some(1 | 1100)) {
        return false;
    }
    let output = format!(
        "{}\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
    output.contains("Microsoft (R)")
        && output.contains("Linker")
        && output.to_ascii_lowercase().contains("usage: link")
}

// Attempt to resolve `(repo, tag)` for the current target via the
// embedded v6 manifest blob baked into the binary at build time.
//
// Same trust posture as [`try_manifest_first`]: a hit is sha-pinned
// against the embed's entry; a sha mismatch is a hard error regardless
// of `SOLDR_TRUST_MODE`. A miss (no matching tool / triple / version)
// returns `Ok(None)` so the caller falls through to the live fetch.
async fn try_embedded_manifest_v6(
    paths: &SoldrPaths,
    cache_name: &str,
    binary_names: &[&str],
    repo: &github::RepoInfo,
    tag: &str,
    tag_prefix: Option<&str>,
    target: &TargetTriple,
) -> Result<Option<FetchResult>, SoldrError> {
    let Some(manifest) = manifest_v6::embedded_manifest() else {
        return Ok(None);
    };
    if manifest.is_empty() {
        return Ok(None);
    }
    // Normalize the version pin: callers may pass `v1.12.10` (from
    // `known_tools`) but v6 keys are bare semver (`1.12.10`).
    let bare = tag.trim_start_matches('v').to_string();
    let triple_owned = target.triple();
    let triple: &str = &triple_owned;
    let hit = match manifest.lookup(&repo.owner, &repo.repo, triple, Some(&bare)) {
        Some(h) => h,
        None => return Ok(None),
    };
    let version = hit.version.clone();

    if let Some(r) = check_cache(paths, cache_name, &version, binary_names, target)? {
        return Ok(Some(r));
    }

    // Synthesize an asset name from the URL's final path component so
    // the trust-pin log line matches the existing live-fetch shape.
    let asset_name = hit
        .asset
        .href
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(hit.asset.href.as_str());
    if !github::asset_name_matches_any_binary(asset_name, binary_names) {
        return Ok(None);
    }

    eprintln!(
        "soldr: embed-first hit for {}/{} v{} → {}",
        repo.owner, repo.repo, version, asset_name
    );

    let _ = tag_prefix; // accepted for signature symmetry; unused by the v6 lookup.

    // soldr#1790: time the embedded-manifest asset download.
    let download_started_at_ms = current_unix_ms();
    let download_started = std::time::Instant::now();
    let binary_path = archive::download_and_extract_with_pin(
        paths,
        cache_name,
        &version,
        &hit.asset.href,
        target,
        binary_names,
        Some((asset_name, hit.asset.sha256.as_str())),
    )
    .await?;
    soldr_core::build_log_meta::fetch_timing::record(
        soldr_core::build_log_meta::fetch_timing::FetchTiming {
            name: cache_name.to_string(),
            source: "manifest-embed".to_string(),
            started_at_ms: download_started_at_ms,
            duration_ms: download_started.elapsed().as_millis() as u64,
        },
    );

    Ok(Some(FetchResult {
        binary_path,
        version,
        cached: false,
    }))
}

// Attempt to resolve `(repo, tag)` for the current target via soldr's
// published asset index on the `manifest` branch.
//
// Returns:
//   * `Ok(Some(FetchResult))` — manifest hit, sha256 verified,
//     download + extract succeeded. The caller short-circuits.
//   * `Ok(None)` — manifest miss (no matching entry, manifest empty,
//     manifest disabled, parse failure, network failure). The caller
//     falls through to the live GitHub Releases API.
//   * `Err(_)` — manifest hit but sha256 verification or download
//     failed. This is a hard error — falling back to the live API
//     would mask a tampered manifest entry. Same trust posture as
//     `apple_sdk` / `zig`.
include!("manifest_resolution.rs");
#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
