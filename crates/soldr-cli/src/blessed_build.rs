//! soldr#1012 PR 5 — blessed cross-compile sysroot preparation for
//! `soldr build --target <T>`.
//!
//! When `Commands::Build` (soldr#1013) detects a canonical cross-
//! compile target, this module materializes the per-target sysroot
//! from the soldr-toolchain catalogue, sets the env vars cargo + cc-rs
//! + linker tools need, and installs the clang shim ahead of the
//! system clang on PATH.
//!
//! ## Target coverage
//!
//! * `*-pc-windows-msvc` — xwin-cache + multicall clang shim (sidesteps
//!   the ring 0.17.14:563 oversight described in
//!   [`crate::fetch::xwin_cache`])
//! * `x86_64-pc-windows-gnu` on Windows x64 hosts - managed
//!   MinGW-w64 GCC from the soldr-toolchain catalogue, prepended to
//!   PATH with target-scoped Cargo/cc-rs env.
//! * `*-apple-darwin` — target-aware Apple SDK provisioning plus
//!   clang/SDK env injection through
//!   [`crate::fetch::apple_sdk::ensure_apple_sdk`].
//!
//! Linux GNU/musl preparation is layered on by `target_lifecycle`; this
//! low-level platform module remains responsible for SDK/catalogue families.
//!
//! ## There is no opt-out
//!
//! `SOLDR_USE_LEGACY_XWIN` / `SOLDR_USE_LEGACY_ZIGBUILD` used to suppress the
//! blessed prep and fall through to cargo-xwin's or cargo-zigbuild's own live
//! toolchain download. Both are removed (soldr#2519). The blessed asset is
//! pinned and sha256-verified per `docs/TRUST_BOUNDARIES.md`; the legacy paths
//! fetched an unpinned SDK from a third-party CDN, so "diagnostic toggle" was
//! really "leave the trust boundary". `target_lifecycle.rs` had already
//! scheduled the Zig half for removal in 0.9.0.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::core::{SoldrError, SoldrPaths};

pub(crate) mod darwin_arch;
mod links_provider;
mod lzma_override;
mod mimalloc_override;
mod openssl_override;

#[cfg(test)]
pub(crate) use links_provider::prime_metadata_for_test as prime_links_metadata_for_test;
pub(crate) use openssl_override::openssl_env_prefix;

/// soldr#1543 test seam: sleep this many milliseconds at the top of
/// [`prepare`] to simulate slow catalogue/SDK materialization, so the
/// dependency-prefetch overlap can be measured deterministically
/// without depending on live catalogue latency. Ignored when unset,
/// unparsable, or zero.
pub const TEST_PREP_DELAY_ENV_VAR: &str = "SOLDR_TEST_BUILD_PREP_DELAY_MS";

/// What the blessed-build prep accomplished, returned to the caller
/// so `Commands::Build` can log + set the resulting env vars on the
/// child cargo invocation.
#[derive(Debug, Clone, Default)]
pub struct BlessedPrep {
    /// Path to `XWIN_CACHE_DIR` value (for `*-pc-windows-msvc`).
    pub xwin_cache_dir: Option<PathBuf>,
    /// Path to `SDKROOT` value (for `*-apple-darwin`).
    pub sdkroot: Option<PathBuf>,
    /// Directory to prepend to `PATH` so the multicall clang shim wins
    /// when cc-rs (or ring) does a bare `which("clang")` lookup.
    pub shim_path_dir: Option<PathBuf>,
    /// Additional directories to prepend to `PATH` on the child cargo
    /// invocation (after `shim_path_dir`). Used for the managed
    /// cmake/ninja `bin/` dirs so CMake's Ninja generator resolves the
    /// managed ninja instead of whatever the system PATH serves.
    pub path_dirs: Vec<PathBuf>,
    /// Per-target env-var tuples to set on the child cargo
    /// invocation: `[(NAME, VALUE), ...]`. Includes `CC_<t>`,
    /// `CXX_<t>`, `AR_<t>`, `CARGO_TARGET_<T>_LINKER` for MSVC
    /// targets.
    pub env: Vec<(String, String)>,
    /// Extra cargo CLI args to inject into the `soldr build` cargo
    /// invocation. Used for target-scoped Cargo config that has no
    /// environment-variable equivalent, such as build-script overrides
    /// for crates with `links = "..."`
    pub cargo_args: Vec<String>,
}

impl BlessedPrep {
    /// PATH entries in precedence order. The clang shim must stay ahead of
    /// managed LLVM so bare `clang --target=*-pc-windows-msvc` invocations
    /// route through clang-cl before resolving the real compiler toolchain.
    pub(crate) fn path_prefix(&self) -> Vec<PathBuf> {
        self.shim_path_dir
            .iter()
            .cloned()
            .chain(self.path_dirs.iter().cloned())
            .collect()
    }
}

/// Prepare the blessed sysroot for `target`. Returns `Ok(prep)` on
/// success; `Ok(BlessedPrep::default())` if `target` doesn't need
/// any prep (or if the caller opted into legacy via env vars);
/// `Err(_)` only on actual fetch / extract failures.
///
/// Caller is responsible for applying `prep.env` and prepending
/// `prep.shim_path_dir` to `PATH` on the child cargo invocation.
#[expect(clippy::cognitive_complexity, reason = "baseline, zackees/ci.yml#229")]
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub async fn prepare(
    paths: &SoldrPaths,
    target_triple: &str,
    feature_args: &[String],
) -> Result<BlessedPrep, SoldrError> {
    // soldr#1543 test seam — see TEST_PREP_DELAY_ENV_VAR.
    if let Some(delay_ms) = std::env::var(TEST_PREP_DELAY_ENV_VAR)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
    {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
    }

    let target_triple_canonical = target_triple.to_ascii_lowercase();
    let target_triple = target_triple_canonical.as_str();
    let mut prep = BlessedPrep::default();

    // ----------------------------- Windows MSVC ------------------------------
    if should_prepare_xwin_for_target(target_triple) {
        // Install the clang shim + set cc-rs env vars FIRST,
        // independent of xwin-cache state. `CC_<target>=clang-cl` is
        // what fixes cc-rs/ring MSVC `/imsvc...` flag handling; the
        // shim remains on PATH for literal `clang --target=...-msvc`
        // invocations that bypass CC/CXX env:
        // xwin-cache supplies the headers/libs used by the native
        // blessed cargo path, and also lets the cargo-xwin fallback
        // short-circuit its live MSVC download. The shim remains useful
        // independently because ring invokes `clang` directly.
        let target_u = target_triple.replace('-', "_");
        let target_u_upper = target_u.to_uppercase();

        let shim_dir = install_clang_shim(paths)?;
        prep.shim_path_dir = Some(shim_dir);

        // Put the managed LLVM binutils (clang-cl / lld-link / llvm-lib,
        // from zackees/clang-tool-chain-bins) on PATH so the AR /
        // linker picks below resolve without a host `apt install llvm`.
        let _ = ensure_llvm_on_path(paths, &mut prep, target_triple).await;

        add_msvc_tool_env(&mut prep, &target_u, &target_u_upper);

        // Now try to materialize xwin-cache too. If the catalogue row
        // for this arch is ingested, we set XWIN_CACHE_DIR so cargo-
        // xwin transparently uses our cache instead of triggering a
        // fresh live download, AND we inject the `/imsvc` include flags
        // + `/LIBPATH:` linker args via `CFLAGS_<t>` / `CXXFLAGS_<t>` /
        // `CARGO_TARGET_<T>_RUSTFLAGS` so the blessed path works
        // natively without cargo-xwin in the loop (soldr#1036 — make
        // soldr build a true cargo-xwin replacement, not just a wrapper
        // that delegates to it). On a transient catalogue failure,
        // `soldr build` can still fall through to cargo-xwin's live SDK
        // download; nextest archive callers require this cache explicitly.
        let (llvm_result, xwin_result) = tokio::join!(
            crate::fetch::ensure_llvm_toolchain(paths),
            crate::fetch::xwin_cache::ensure_xwin_cache(paths, target_triple)
        );

        let _ = apply_llvm_toolchain_result(&mut prep, target_triple, llvm_result);

        match xwin_result {
            Ok(cache_dir) => {
                prep.xwin_cache_dir = Some(cache_dir.clone());
                prep.env.push((
                    crate::fetch::xwin_cache::XWIN_CACHE_DIR_ENV_VAR.to_string(),
                    cache_dir.to_string_lossy().into_owned(),
                ));

                // Native cflags + rustflags injection (soldr#1036).
                // cc-rs reads `CFLAGS_<target>` / `CXXFLAGS_<target>`
                // and forwards them to the compiler invocation. The
                // `CC_<target>=clang-cl` env above makes clang-cl parse
                // `/imsvc<path>` MSVC-style include flags correctly.
                let cflags = xwin_msvc_cflags(&cache_dir);
                if !cflags.is_empty() {
                    prep.env
                        .push((format!("CFLAGS_{target_u}"), cflags.clone()));
                    prep.env.push((format!("CXXFLAGS_{target_u}"), cflags));
                }

                // Linker args via cargo's per-target RUSTFLAGS env var.
                // Each /LIBPATH: entry becomes a `-C link-arg=...` so
                // rustc passes it through to lld-link. Whitespace-
                // separated rustflags is cargo's documented contract.
                // soldr#2794: read the CRT policy off the caller here, at
                // dispatch, rather than baking dynamic into preparation.
                let linkage = requested_crt_linkage(target_triple);
                let link_args = xwin_msvc_link_args(&cache_dir, target_triple, linkage);
                if !link_args.is_empty() {
                    prep.env.push((
                        format!("CARGO_TARGET_{target_u_upper}_RUSTFLAGS"),
                        link_args,
                    ));
                }
            }
            Err(e) => {
                eprintln!("soldr build: catalogue xwin-cache unavailable for {target_triple}: {e}");
                eprintln!(
                    "soldr build: continuing without XWIN_CACHE_DIR — \
                     cargo-xwin's live download will produce the SDK \
                     (clang shim is still active for ring's compile fix)"
                );
            }
        }
    }

    // --------------------------- Windows GNU GCC ----------------------------
    if target_triple == crate::fetch::mingw_w64_gcc::MINGW_W64_GCC_TARGET {
        let (mingw_bin, mingw_env) =
            crate::fetch::mingw_w64_gcc::prepare_win_gnu_env(paths, target_triple).await?;
        prep.path_dirs.insert(0, mingw_bin);
        prep.env.extend(mingw_env);
    }

    // ------------------------------ Apple Darwin -----------------------------
    if target_triple.ends_with("-apple-darwin") {
        // soldr is a bootstrapping tool: it must provision the cross
        // toolchain itself, never lean on a host `apt install clang lld
        // llvm`. The darwin arm sets AR=llvm-ar / RANLIB=llvm-ranlib /
        // CC=clang / linker=clang -fuse-ld=lld — put the managed LLVM
        // binutils (from zackees/clang-tool-chain-bins) on PATH so those
        // resolve on a bare Linux runner. (msvc gets LLVM via its
        // downstream `cargo xwin` subcommand; darwin dispatches to plain
        // cargo, so without this it never gets llvm-ar — soldr#1309.)
        let managed_llvm_available = ensure_llvm_on_path(paths, &mut prep, target_triple).await;

        // Rust's packed Darwin debuginfo invokes `dsymutil` after linking.
        // Respect an explicit usable override before fetching the selective
        // managed LLVM-tools bundle. Besides honoring the documented escape
        // hatch, this keeps fixture-driven preparation fully offline.
        if dsymutil_override_dir().is_none() {
            if let Ok(bundle) = crate::fetch::llvm_tools_bundle::ensure_llvm_tools_bundle(
                paths,
                crate::fetch::cmake_tools::current_host_triple(),
            )
            .await
            {
                prep.path_dirs.push(bundle.join("bin"));
            }
        }
        // llvm-tools-preview does not ship llvm-dsymutil for every Rust
        // host (notably the zero-dependency musl bootstrap image). A Darwin
        // build can still link correctly without packed debug info, so keep
        // the cross toolchain usable and select that safe fallback below.
        let dsymutil_available = match ensure_dsymutil_on_path(&mut prep) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("soldr build: {error}; disabling packed Darwin debuginfo for this build");
                false
            }
        };

        // Apple SDK fetch is the same code path `soldr prepare` uses,
        // so this is reuse rather than new logic.
        match crate::fetch::apple_sdk::ensure_apple_sdk(paths, Some(target_triple)).await {
            Ok(sdk) => {
                let sdk_str = sdk.to_string_lossy().into_owned();
                prep.sdkroot = Some(sdk.clone());
                prep.env.push(("SDKROOT".to_string(), sdk_str.clone()));

                // Surface the COMPLETE Apple SDK to every C/C++ build
                // script in the dep tree by overriding cc-rs's per-
                // target compiler picks. Without this, anything that
                // probes platform headers hits zig's minimal bundled
                // darwin sysroot instead of the real SDK and fails the
                // cross-compile. With it, every C/C++ dep — ring,
                // zstd-sys, libsqlite3-sys, libz-ng-sys, bzip2-sys,
                // lzma-sys, libmimalloc-sys — sees the same headers a
                // native macOS build would. Pattern mirrors the
                // Windows MSVC arm above (lines 84-110).
                //
                // Linker: use clang as the driver with `-fuse-ld=lld`.
                // lld knows Mach-O; clang knows how to pass the right
                // -L / -F flags to satisfy darwin framework references
                // (Security, CoreFoundation, etc.) from the SDK's
                // System/Library/Frameworks/.
                let target_u = target_triple.replace('-', "_");
                let target_u_upper = target_u.to_uppercase();
                // Both per-arch — see `darwin_arch` for why the
                // deployment target was not (soldr#2146).
                let clang_arch_target = darwin_arch::clang_target(target_triple);
                let min_os = darwin_arch::deployment_target(target_triple);
                let clang_base =
                    format!("clang --target={clang_arch_target} -isysroot {sdk_str} -mmacosx-version-min={min_os}");
                let clangxx_base = format!(
                    "clang++ --target={clang_arch_target} -isysroot {sdk_str} -mmacosx-version-min={min_os} -stdlib=libc++"
                );
                // lld selection is critical for the C/C++ flags too:
                // cmake-based -sys crates (libz-ng-sys, etc.) test the
                // compiler with a tiny .c + link round-trip. Without it
                // they invoke clang → /usr/bin/ld which is the host's
                // Linux ld and can't produce Mach-O, so the cmake
                // configure step panics with "linker command failed".
                // Adding it to CFLAGS/CXXFLAGS routes the test link
                // through lld which knows Mach-O. The `--target=` flag
                // is also needed in CFLAGS so the compiler test
                // actually targets darwin (not the linux host).
                // If managed LLVM cannot be fetched on Linux, still ask
                // host clang for LLD. GitHub's cross lane installs lld,
                // and this is strictly better than falling back to GNU
                // ld, which cannot link Mach-O at all.
                let use_lld_linker = darwin_should_use_lld(managed_llvm_available);
                // soldr#3776: link-only, via a clang config file, so `-c`
                // compiles stay clean under -Werror.
                let lld_flag = if use_lld_linker {
                    let cfg = crate::darwin_lld_config::ensure_lld_config(paths)?;
                    crate::darwin_lld_config::link_only_lld_flag(&cfg)
                } else {
                    String::new()
                };
                let cflags = format!(
                    "--target={clang_arch_target} -isysroot {sdk_str} \
                     -mmacosx-version-min={min_os}{lld_flag}"
                );
                let cxxflags = format!(
                    "--target={clang_arch_target} -isysroot {sdk_str} \
                     -mmacosx-version-min={min_os} -stdlib=libc++{lld_flag}"
                );
                let mut rustflags = format!(
                    "-C link-arg=--target={clang_arch_target} \
                     -C link-arg=-isysroot \
                     -C link-arg={sdk_str} \
                     -C link-arg=-mmacosx-version-min={min_os}"
                );
                if use_lld_linker {
                    rustflags.push_str(" -C link-arg=-fuse-ld=lld");
                }
                if !dsymutil_available {
                    rustflags.push_str(" -C split-debuginfo=off");
                }
                let (ar_tool, ranlib_tool) = if use_lld_linker {
                    ("llvm-ar", "llvm-ranlib")
                } else {
                    ("ar", "ranlib")
                };
                prep.env
                    .push((format!("CC_{target_u}"), clang_base.clone()));
                prep.env.push((format!("CXX_{target_u}"), clangxx_base));
                prep.env
                    .push((format!("AR_{target_u}"), ar_tool.to_string()));
                prep.env
                    .push((format!("RANLIB_{target_u}"), ranlib_tool.to_string()));
                prep.env.push((format!("CFLAGS_{target_u}"), cflags));
                prep.env.push((format!("CXXFLAGS_{target_u}"), cxxflags));
                prep.env.push((
                    format!("CARGO_TARGET_{target_u_upper}_LINKER"),
                    "clang".to_string(),
                ));
                prep.env.push((
                    format!("CARGO_TARGET_{target_u_upper}_RUSTFLAGS"),
                    rustflags,
                ));
            }
            Err(e) => {
                eprintln!("soldr build: apple SDK unavailable for {target_triple}: {e}");
                // Don't hard-fail; zigbuild may still locate an SDK
                // via cargo-zigbuild's own mechanism.
            }
        }
    }

    // -------------------------- *-sys library overrides ----------------------
    // soldr#1064 Phase B. For each registered *-sys library, try to
    // materialize the catalogue sysroot for the active target. On
    // success, push the crate-specific override env vars so the *-sys
    // build.rs skips its vendored compile. On error (not yet ingested,
    // unsupported triple, network failure), log + fall through — the
    // crate's vendored compile path still works.
    if !legacy_vendored_sys_opt_out() {
        inject_sys_library_overrides(paths, target_triple, &mut prep, feature_args).await;
    }

    // ----------------------- managed cmake + ninja ---------------------------
    // Host-side tooling, independent of the compile target: cmake-based
    // *-sys build scripts (libz-ng-sys, zstd-sys, ...) must run a
    // known-good cmake with a sane generator, not whatever the system
    // PATH resolves. See inject_cmake_tooling for the full contract.
    inject_cmake_tooling(paths, &mut prep).await;

    Ok(prep)
}

fn add_msvc_tool_env(prep: &mut BlessedPrep, target_u: &str, target_u_upper: &str) {
    prep.env
        .push((format!("CC_{target_u}"), "clang-cl".to_string()));
    prep.env
        .push((format!("CXX_{target_u}"), "clang-cl".to_string()));
    prep.env
        .push((format!("AR_{target_u}"), "llvm-lib".to_string()));
    prep.env.push((
        format!("CARGO_TARGET_{target_u_upper}_LINKER"),
        "lld-link".to_string(),
    ));
}

fn darwin_should_use_lld(managed_llvm_available: bool) -> bool {
    managed_llvm_available
        || crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Linux
}

const SOLDR_DSYMUTIL_ENV_VAR: &str = "SOLDR_DSYMUTIL";

fn dsymutil_override_dir() -> Option<PathBuf> {
    std::env::var_os(SOLDR_DSYMUTIL_ENV_VAR)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .and_then(|path| path.parent().map(Path::to_path_buf))
}

/// The dsymutil search names for the host (`.exe`-suffixed on Windows).
fn dsymutil_names() -> &'static [&'static str] {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        &["dsymutil.exe", "llvm-dsymutil.exe"]
    } else {
        &["dsymutil", "llvm-dsymutil"]
    }
}

fn ensure_dsymutil_on_path(prep: &mut BlessedPrep) -> Result<(), SoldrError> {
    let names: &[&str] = dsymutil_names();
    if let Some(parent) = dsymutil_override_dir() {
        prep.path_dirs.push(parent);
        return Ok(());
    }

    for dir in &prep.path_dirs {
        for name in names {
            let path = dir.join(name);
            if path.is_file() {
                return Ok(());
            }
        }
    }

    if let Some(path) = std::env::var_os("PATH").and_then(|value| {
        std::env::split_paths(&value).find_map(|dir| {
            names
                .iter()
                .map(|name| dir.join(name))
                .find(|path| path.is_file())
        })
    }) {
        if let Some(parent) = path.parent() {
            prep.path_dirs.push(parent.to_path_buf());
            return Ok(());
        }
    }

    if let Some(path) = find_dsymutil_in_rustup() {
        if let Some(parent) = path.parent() {
            prep.path_dirs.push(parent.to_path_buf());
            return Ok(());
        }
    }

    let channel = crate::core::read_rust_toolchain_manifest(
        &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    )
    .ok()
    .and_then(|manifest| manifest.channel);
    crate::core::forbid_toolchain_install_tripwire("rustup component add llvm-tools-preview")?;
    let mut command = Command::new(crate::binaries::rustup_binary());
    command.args(["component", "add", "llvm-tools-preview"]);
    if let Some(channel) = channel.as_deref() {
        command.args(["--toolchain", channel]);
    }
    crate::core::apply_implicit_toolchain_homes(&mut command, None);
    // reason: only stdout is discarded; rustup reports progress and every error on
    // stderr, which stays attached to the terminal, and the exit status is checked.
    #[cfg_attr(
        dylint_lib = "ban_swallowed_child_stdio",
        allow(ban_swallowed_child_stdio)
    )]
    command.stdout(Stdio::null());
    let status = crate::platform::wait::status_for(&mut command, crate::platform::wait::LONG_CHILD)
        .map_err(|error| {
            SoldrError::Other(format!(
                "unable to provision dsymutil: failed to invoke rustup: {error}"
            ))
        })?;
    if status.success() {
        if let Some(path) = find_dsymutil_in_rustup() {
            if let Some(parent) = path.parent() {
                prep.path_dirs.push(parent.to_path_buf());
                return Ok(());
            }
        }
    }

    Err(SoldrError::Other(
        "Darwin packed debuginfo requires dsymutil, but it is unavailable. \
         Install the rustup llvm-tools-preview component or set SOLDR_DSYMUTIL \
         to a compatible dsymutil executable."
            .into(),
    ))
}

fn find_dsymutil_in_rustup() -> Option<PathBuf> {
    let names: &[&str] = dsymutil_names();
    let rustc = crate::binaries::resolve_toolchain_binary("rustc").ok()?;
    let toolchain_root = rustc.parent()?.parent()?;
    let host_bin = toolchain_root.join("lib").join("rustlib");
    let entries = std::fs::read_dir(host_bin).ok()?;
    for entry in entries.flatten() {
        for name in names {
            let candidate = entry.path().join("bin").join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
fn add_mingw_w64_gcc_env(
    prep: &mut BlessedPrep,
    target_triple: &str,
    mingw_root: &std::path::Path,
) {
    prep.path_dirs
        .insert(0, crate::fetch::mingw_w64_gcc::bin_dir(mingw_root));
    prep.env.extend(crate::fetch::mingw_w64_gcc::env_for_target(
        mingw_root,
        target_triple,
    ));
}

/// Env var that opts out of the managed cmake/ninja injection and
/// leaves cmake resolution to the system PATH. Set to any non-empty
/// value (other than `"0"`) to trigger. Unlike the removed
/// `SOLDR_USE_LEGACY_{XWIN,ZIGBUILD}` toggles, this one stays inside the
/// trust boundary: it selects a system cmake rather than an unpinned
/// third-party SDK download.
pub const USE_SYSTEM_CMAKE_ENV_VAR: &str = "SOLDR_USE_SYSTEM_CMAKE";

fn system_cmake_opt_out() -> bool {
    std::env::var(USE_SYSTEM_CMAKE_ENV_VAR)
        .map(|v| crate::core::flag_value(&v))
        .unwrap_or(false)
}

/// Inject the managed CMake + Ninja tooling env onto the prep.
///
/// What gets set (all only on success — every failure logs and falls
/// through to system cmake, stub-until-ingested style):
///
/// * `CMAKE=<bundle>/bin/cmake` — the cmake-rs crate (used by every
///   cmake-based *-sys build script) resolves the cmake binary from
///   this env var before falling back to PATH.
/// * `CMAKE_GENERATOR=Ninja` — cmake-rs forwards this as `-G`;
///   only set when the managed ninja materialized. Ninja is immune
///   to the "MSYS Makefiles + mangled MSVC flags" failure mode that
///   a stray GNU make on PATH produces, and it needs no VS generator
///   registry probing.
/// * `prep.path_dirs` gets both bundles' `bin/` dirs — CMake's Ninja
///   generator discovers `ninja` via PATH (there is no env-var
///   equivalent of the `CMAKE_MAKE_PROGRAM` cache variable).
///
/// Skips (with no log noise) when the caller already set `CMAKE` or
/// `CMAKE_GENERATOR` — explicit user env always wins — and when
/// [`USE_SYSTEM_CMAKE_ENV_VAR`] opts out.
pub async fn inject_cmake_tooling(paths: &SoldrPaths, prep: &mut BlessedPrep) {
    if system_cmake_opt_out() {
        return;
    }
    // Respect explicit user config: a pre-set CMAKE or CMAKE_GENERATOR
    // means someone already made this decision.
    for user_var in ["CMAKE", "CMAKE_GENERATOR"] {
        if std::env::var_os(user_var).is_some_and(|v| !v.is_empty()) {
            return;
        }
    }

    let host = crate::fetch::cmake_tools::current_host_triple();

    let cmake_root = match crate::fetch::cmake_tools::ensure_cmake_bundle(paths, host).await {
        Ok(root) => root,
        Err(e) => {
            log_cmake_unavailable("cmake", host, &e);
            return;
        }
    };
    let cmake_bin = crate::fetch::cmake_tools::cmake_exe(&cmake_root);
    if !cmake_bin.is_file() {
        eprintln!(
            "soldr: managed cmake bundle at {} has no {} — \
             falling back to system cmake",
            cmake_root.display(),
            cmake_bin.display()
        );
        return;
    }

    prep.env.push((
        "CMAKE".to_string(),
        cmake_bin.to_string_lossy().into_owned(),
    ));
    prep.path_dirs.push(cmake_root.join("bin"));

    // Ninja is the generator upgrade, but the managed cmake alone is
    // already a win — don't tie the two together. Without ninja we
    // leave the generator choice to cmake-rs (Visual Studio on MSVC
    // hosts, Unix Makefiles elsewhere).
    //
    // Acquisition ladder (mirrors the maturin provisioning ladder):
    // catalogue bundle first, then the PyPI `ninja` wheel provisioned
    // into a manual uv-managed isolated env. The fallback covers
    // hosts whose ninja bundle is missing or not yet ingested (e.g.
    // musl hosts — the uv bundle exists there, ninja's doesn't).
    let ninja_bin = match crate::fetch::cmake_tools::ensure_ninja_bundle(paths, host).await {
        Ok(ninja_root) => {
            let bin = crate::fetch::cmake_tools::ninja_exe(&ninja_root);
            if bin.is_file() {
                Some(bin)
            } else {
                eprintln!(
                    "soldr: managed ninja bundle at {} has no {} — \
                     trying the uv-provisioned ninja fallback",
                    ninja_root.display(),
                    bin.display()
                );
                ninja_via_uv_fallback(paths).await
            }
        }
        Err(e) => {
            log_cmake_unavailable("ninja", host, &e);
            ninja_via_uv_fallback(paths).await
        }
    };
    if let Some(ninja_bin) = ninja_bin {
        prep.env
            .push(("CMAKE_GENERATOR".to_string(), "Ninja".to_string()));
        if let Some(dir) = ninja_bin.parent() {
            prep.path_dirs.push(dir.to_path_buf());
        }
        // #1257 fallout guard: a target/ tree cached from BEFORE the
        // managed-Ninja switch (restored CI caches, existing dev
        // checkouts) holds cmake build dirs configured with a
        // different generator ("Unix Makefiles" / "Visual Studio").
        // cmake-rs re-runs configure in the same `-B` dir (it emits
        // rerun-if-env-changed for CMAKE_GENERATOR, so the build
        // script re-fires the moment we inject Ninja) and CMake
        // hard-errors "Does not match the generator used previously".
        // Sweep those dirs away up front — they would error anyway,
        // and a deleted build dir just means a fresh configure.
        // Observed live on the aarch64/x86_64-apple-darwin CI lanes
        // (run 28574946937).
        sweep_mismatched_cmake_build_dirs(&cargo_target_root(), "Ninja");
    }
}

/// Resolve the cargo target root the upcoming child build will use, through the
/// shared resolver (soldr#3203); `./target` only when it finds nothing.
fn cargo_target_root() -> PathBuf {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| {
            crate::core::cargo_target_dir::resolve_cargo_target_dir(
                &crate::core::cargo_target_dir::CargoTargetDirInputs::from_process(&cwd, &[]),
            )
        })
        .unwrap_or_else(|| PathBuf::from("target"))
}

/// Delete every cmake-rs build dir under `target_root` whose cached
/// generator differs from `generator`. Layouts scanned (both the host
/// and the per-triple trees, any profile name):
///
/// ```text
/// target/<profile>/build/<crate-hash>/out/build/CMakeCache.txt
/// target/<triple>/<profile>/build/<crate-hash>/out/build/CMakeCache.txt
/// ```
///
/// Only the `out/build` dir is removed — cargo's fingerprint for the
/// build script is invalidated by the CMAKE_GENERATOR env change
/// anyway, so the script re-runs and configures the fresh dir.
fn sweep_mismatched_cmake_build_dirs(target_root: &std::path::Path, generator: &str) {
    let needle = format!("CMAKE_GENERATOR:INTERNAL={generator}");
    let mut roots: Vec<PathBuf> = Vec::new();
    let Ok(level1) = std::fs::read_dir(target_root) else {
        return;
    };
    for entry in level1.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        // `p` is either a profile dir (has build/) or a triple dir
        // (whose children are profile dirs).
        roots.push(p.clone());
        if let Ok(level2) = std::fs::read_dir(&p) {
            roots.extend(
                level2
                    .flatten()
                    .map(|e| e.path())
                    .filter(|c| c.is_dir() && c.join("build").is_dir()),
            );
        }
    }

    for profile_dir in roots {
        let build_dir = profile_dir.join("build");
        let Ok(crates) = std::fs::read_dir(&build_dir) else {
            continue;
        };
        for crate_dir in crates.flatten() {
            let out_build = crate_dir.path().join("out").join("build");
            let cache = out_build.join("CMakeCache.txt");
            let Ok(contents) = std::fs::read_to_string(&cache) else {
                continue;
            };
            let mismatch = contents
                .lines()
                .any(|l| l.starts_with("CMAKE_GENERATOR:INTERNAL=") && l.trim() != needle);
            if mismatch {
                eprintln!(
                    "soldr: sweeping stale cmake build dir (generator != {generator}): {}",
                    out_build.display()
                );
                let _ = std::fs::remove_dir_all(&out_build);
            }
        }
    }
}

/// PyPI-ninja-via-uv fallback rung. Returns the ninja exe on success;
/// logs + returns `None` on failure (the caller keeps cmake's default
/// generator — never fatal).
async fn ninja_via_uv_fallback(paths: &SoldrPaths) -> Option<PathBuf> {
    match crate::fetch::uv_env::provision_ninja_via_uv(paths).await {
        Ok(ninja) => Some(ninja),
        Err(e) => {
            eprintln!("soldr: uv-provisioned ninja fallback unavailable: {e}");
            eprintln!("soldr: keeping cmake's default generator");
            None
        }
    }
}

fn log_cmake_unavailable(tool: &str, host: &str, err: &SoldrError) {
    eprintln!("soldr: managed {tool} unavailable for host {host}: {err}");
    eprintln!("soldr: continuing with system {tool} from PATH");
}

/// Env var that opts out of the entire `*-sys` catalogue-substitution
/// path and falls through to each crate's vendored compile. Set to any
/// non-empty value (other than `"0"`) to trigger. Unlike the removed
/// `SOLDR_USE_LEGACY_{XWIN,ZIGBUILD}` toggles, this one stays inside the
/// trust boundary: it falls back to each crate's own vendored source
/// rather than to an unpinned third-party SDK download.
pub const USE_LEGACY_VENDORED_SYS_ENV_VAR: &str = "SOLDR_USE_LEGACY_VENDORED_SYS";

fn legacy_vendored_sys_opt_out() -> bool {
    std::env::var(USE_LEGACY_VENDORED_SYS_ENV_VAR)
        .map(|v| crate::core::flag_value(&v))
        .unwrap_or(false)
}

/// Per-*-sys-crate catalogue substitution. Each block follows the same
/// shape: call the consumer module's `ensure_<lib>_sysroot`, and on
/// success push the override env vars. Failures are logged but never
/// fatal — the *-sys crate's vendored compile remains the safety net.
pub(crate) async fn inject_sys_library_overrides(
    paths: &SoldrPaths,
    target_triple: &str,
    prep: &mut BlessedPrep,
    feature_args: &[String],
) {
    // All syslib env vars below are **target-scoped** via the
    // `PKG_CONFIG_PATH_<triple>` naming convention pkg-config-rs
    // honors. Unscoped `PKG_CONFIG_PATH` would leak into host
    // build-script compilations (cargo runs build scripts with the
    // same env it spawns cargo with), causing the host-side
    // build_script_build link step for soldr-cli to try linking
    // against the cross-target sysroot — e.g. `rust-lld: unable to
    // find library -lzstd_static` when the catalogue path points
    // at a Windows-format zstd dir but the host link runs on Linux.
    //
    // We also no longer set `ZSTD_SYS_USE_PKG_CONFIG=1` /
    // `LIBSQLITE3_SYS_USE_PKG_CONFIG=1` / `LZMA_API_STATIC=1`
    // unconditionally — those flags are NOT target-scoped, so
    // setting them globally forces every consumer (including the
    // host build-script side of the same crate) to attempt
    // pkg-config probing, which then matches against the leaked
    // unscoped `PKG_CONFIG_PATH` and re-introduces the same link
    // bug. The catalogue sysroot is still surfaced via the
    // target-scoped `PKG_CONFIG_PATH_<triple>`; -sys crates that
    // natively probe pkg-config-rs will find it for the cross
    // target without us having to flip a global feature flag.

    // zstd-sys → PKG_CONFIG_PATH_<triple>
    match crate::fetch::zstd_sysroot::ensure_zstd_sysroot(paths, target_triple).await {
        Ok(sysroot) => prepend_pkg_config_path_for_target(prep, target_triple, &sysroot),
        Err(e) => log_sys_unavailable("zstd", target_triple, &e),
    }

    // libsqlite3-sys → PKG_CONFIG_PATH_<triple>
    match crate::fetch::sqlite_sysroot::ensure_sqlite_sysroot(paths, target_triple).await {
        Ok(sysroot) => prepend_pkg_config_path_for_target(prep, target_triple, &sysroot),
        Err(e) => log_sys_unavailable("sqlite", target_triple, &e),
    }

    // libmimalloc-sys → Cargo build-script override (not pkg-config;
    // it has no target-scoped env hook). Gated on who actually
    // provides `links = "mimalloc"` — see the module docs.
    mimalloc_override::inject(paths, target_triple, prep, feature_args).await;

    // libz-ng-sys → PKG_CONFIG_PATH_<triple>
    match crate::fetch::zlib_ng_sysroot::ensure_zlib_ng_sysroot(paths, target_triple).await {
        Ok(sysroot) => prepend_pkg_config_path_for_target(prep, target_triple, &sysroot),
        Err(e) => log_sys_unavailable("zlib-ng", target_triple, &e),
    }

    // lzma-sys → PKG_CONFIG_PATH_<triple>
    lzma_override::inject(paths, target_triple, prep, feature_args).await;

    // bzip2-sys → PKG_CONFIG_PATH_<triple>
    match crate::fetch::bzip2_sysroot::ensure_bzip2_sysroot(paths, target_triple).await {
        Ok(sysroot) => prepend_pkg_config_path_for_target(prep, target_triple, &sysroot),
        Err(e) => log_sys_unavailable("bzip2", target_triple, &e),
    }

    // openssl-sys → target-scoped `<T>_OPENSSL_{DIR,NO_VENDOR,STATIC}`
    // (+ PKG_CONFIG_PATH_<triple> off MSVC). Gated on `openssl-sys`
    // providing `links = "openssl"`; see the module docs (soldr#3246).
    openssl_override::inject(paths, target_triple, prep, feature_args).await;
}

/// Prepend `<sysroot>/lib/pkgconfig` to the **target-scoped**
/// `PKG_CONFIG_PATH_<triple>` env var (pkg-config-rs convention).
/// Unscoped `PKG_CONFIG_PATH` is deliberately NOT used — see the
/// rationale in `inject_sys_library_overrides`.
fn prepend_pkg_config_path_for_target(
    prep: &mut BlessedPrep,
    target_triple: &str,
    sysroot: &std::path::Path,
) {
    let var_name = format!("PKG_CONFIG_PATH_{target_triple}");
    let pkgconfig_dir = sysroot.join("lib").join("pkgconfig");
    let new_entry = pkgconfig_dir.to_string_lossy().into_owned();
    if let Some(existing) = prep.env.iter_mut().find(|(name, _)| name == &var_name) {
        let sep = crate::platform::host::facts::path_list_separator();
        existing.1 = format!("{new_entry}{sep}{}", existing.1);
    } else {
        prep.env.push((var_name, new_entry));
    }
}

fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn log_sys_unavailable(lib_name: &str, target_triple: &str, err: &SoldrError) {
    eprintln!(
        "soldr build: catalogue {lib_name} sysroot unavailable for \
         {target_triple}: {err}"
    );
    eprintln!(
        "soldr build: continuing — {lib_name}-sys will compile from its \
         vendored C source as usual"
    );
}

fn should_prepare_xwin_for_target(target_triple: &str) -> bool {
    target_triple
        .to_ascii_lowercase()
        .ends_with("-pc-windows-msvc")
        && crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Linux
}

/// Install `soldr` under the multicall clang names at
/// `<paths.bin>/clang-shim/clang(.exe)` and matching siblings
/// (`clang++`). Idempotent via hardlink/copy materialization.
///
/// Returns the directory the shim was installed into, ready for
/// PATH prepending.
/// Fetch the managed LLVM toolchain (clang / clang++ / clang-cl / lld /
/// ld.lld / lld-link / llvm-ar / llvm-lib / llvm-ranlib, from
/// `zackees/clang-tool-chain-bins`) and prepend its `bin/` to the child
/// cargo's PATH via `prep.path_dirs`.
///
/// This is what lets `soldr build --target {*-apple-darwin,
/// *-pc-windows-msvc}` run on a bare Linux runner with **no host
/// `apt install clang lld llvm`** — soldr is a bootstrapping tool, so it
/// uses the right toolchain rather than requiring the host to
/// pre-install it (soldr#1309).
///
/// Non-fatal on miss: logs and falls through so a host-provided LLVM
/// still works if the catalogue asset is unavailable for the host arch.
async fn ensure_llvm_on_path(
    paths: &SoldrPaths,
    prep: &mut BlessedPrep,
    target_triple: &str,
) -> bool {
    let result = crate::fetch::ensure_llvm_toolchain(paths).await;
    apply_llvm_toolchain_result(prep, target_triple, result)
}

fn apply_llvm_toolchain_result(
    prep: &mut BlessedPrep,
    target_triple: &str,
    result: Result<PathBuf, SoldrError>,
) -> bool {
    match result {
        Ok(bin_dir) => {
            // Prepend so the managed clang/llvm-ar/lld win over any host
            // copies; the clang shim (msvc) is prepended even earlier by
            // the caller and only shadows `clang`/`clang++`, so clang-cl
            // / lld-link / llvm-lib still resolve here.
            prep.path_dirs.insert(0, bin_dir);
            true
        }
        Err(e) => {
            eprintln!("soldr build: managed LLVM toolchain unavailable for {target_triple}: {e}");
            eprintln!("soldr build: continuing — relying on any host clang/llvm/lld on PATH");
            false
        }
    }
}

fn install_clang_shim(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    paths.ensure_dirs()?;

    let shim_dir = paths.bin.join("clang-shim");
    std::fs::create_dir_all(&shim_dir)?;

    let shim_src = crate::shim_materialize::soldr_binary_source()?;

    for name in clang_shim_names() {
        let dst = shim_dir.join(&name);
        crate::shim_materialize::materialize_executable(&shim_src, &dst).map_err(|e| {
            SoldrError::Other(format!(
                "failed to install multicall clang shim at {}: {e}",
                dst.display()
            ))
        })?;
    }

    Ok(shim_dir)
}

include!("blessed_build_xwin.rs");
include!("blessed_build_crt.rs");
#[cfg(test)]
#[path = "blessed_build_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "blessed_build_crt_tests.rs"]
mod crt_tests;
