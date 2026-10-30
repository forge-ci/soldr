//! `soldr build-from-source <tool> --target <triple>` — source-build a
//! whitelisted soldr-managed tool (today: `crgx`, `cargo-chef`) for an
//! arbitrary Rust target triple and deposit the resulting binary into
//! `~/.soldr/bin/<tool>-from-source/<version>/<triple>/<tool>[.exe]`
//! with a sha256 sidecar.
//!
//! Sub-issue #859 of meta #853. Motivation: the release pipeline already
//! source-builds these tools in some lanes via inline shell because
//! upstream does not always ship prebuilt binaries for every target
//! (notably `aarch64-apple-darwin` for cargo-chef per CLAUDE.md). This
//! command lifts that shell into a first-class soldr verb so any
//! developer can locally produce a cross-compiled tool binary with:
//!
//! ```text
//! soldr build-from-source crgx --target aarch64-apple-darwin
//! soldr build-from-source cargo-chef --target aarch64-apple-darwin
//! ```
//!
//! Modelled on the former managed-zccache source-build path for
//! the retry budget, staging dir handling, and sha256 sidecar shape.
//!
//! ## Hard rules
//!
//! - **Whitelisted tools only.** Anything outside the whitelist errors
//!   with a directive listing the supported names. This keeps the verb
//!   focused on tools soldr actually bundles — generic `cargo install`
//!   passthrough belongs in `soldr cargo install`, not here.
//! - **Direct cargo invocation, cached rustc.** Resolves cargo via
//!   `binaries::resolve_toolchain_binary("cargo")` and clears inherited
//!   `RUSTC_WRAPPER` / `RUSTC_WORKSPACE_WRAPPER` so the spawn never
//!   re-enters soldr's cargo front-door machinery recursively — see
//!   `crates/soldr-cli/src/toolchain.rs`'s `cargo_install_plugin` for
//!   the same pattern. It then opts back into compile caching by
//!   pointing `RUSTC_WRAPPER` at soldr's compiler-named zccache
//!   wrapper shim (issue #1788), so tool source builds hit the shared
//!   object cache instead of recompiling every dependency cold on each
//!   fresh machine/container. `SOLDR_SOURCE_BUILD_CACHE=off` (or the
//!   standard `ZCCACHE_DISABLE=1`) restores the fully-uncached spawn.
//! - **Default version comes from the registry.** `version: None`
//!   resolves through `known_tools::lookup_by_crate(tool).pinned_version`
//!   so the default matches what every other soldr resolution path uses
//!   for that crate. An explicit `--version` wins.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::binaries::resolve_toolchain_binary;
use crate::core::{
    suppress_windows_console_window, InstallerWatchdogConfig, SoldrError, SoldrPaths, TargetTriple,
};
use crate::fetch::known_tools;

/// Retry budget for the source-build install loop. Previously borrowed
/// from the (now-deleted) managed-zccache install constants (soldr#1368);
/// this build path is generic (forge tool builds), so it keeps its own.
const SOURCE_BUILD_INSTALL_ATTEMPTS: u32 = 3;

/// Tools `soldr build-from-source <tool>` accepts. Kept tiny on purpose
/// (see module doc): generic crate source-build belongs in
/// `soldr cargo install`. New entries must be soldr-bundled tools where
/// upstream's release coverage misses a target soldr depends on.
pub const SUPPORTED_TOOLS: &[&str] = &["crgx", "cargo-chef", "cargo-dylint", "dylint-link"];

/// Initial back-off between failed `cargo install` retries. Mirrors
/// the previous managed-install 10s baseline so callers
/// don't see one retry budget here that disagrees with the rest of
/// soldr. Re-declared instead of imported so the cargo-chef / crgx
/// source-build path stays decoupled from zccache's constants.
const RETRY_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(10);
pub const CARGO_INSTALL_TIMEOUT_ENV_VAR: &str = "SOLDR_BUILD_FROM_SOURCE_INSTALL_TIMEOUT_SECS";

/// Directory under `SoldrPaths::bin` where source-built binaries land.
/// The full layout is
/// `<bin>/<tool>-from-source/<version>/<triple>/<tool>[.exe]` — keeps
/// per-(tool, version, triple) installs side-by-side so a host can
/// carry both an `x86_64-unknown-linux-gnu` build and an
/// `aarch64-apple-darwin` build at the same version without one
/// stomping the other.
pub const FROM_SOURCE_SUBDIR_SUFFIX: &str = "-from-source";

/// Plan computed by [`resolve_plan`]. Materialising this struct never
/// touches the network and never invokes cargo — it just resolves the
/// host / target / version inputs. Production code chains [`run`] which
/// calls [`execute_plan`] to actually source-build; tests exercise
/// [`resolve_plan`] in isolation to verify the dispatch / resolution
/// path without a real install.
#[derive(Debug, Clone)]
pub struct BuildPlan {
    /// Canonical tool name (matches `known_tools` `crate_name`).
    pub tool: String,
    /// Resolved Rust target triple, e.g. `aarch64-apple-darwin`.
    pub target: String,
    /// Resolved upstream version without the leading `v`.
    pub version: String,
    /// Destination directory the cargo install root rolls into:
    /// `<bin>/<tool>-from-source/<version>/<triple>/`.
    pub install_dir: PathBuf,
    /// Final binary path (`<install_dir>/<tool>[.exe]`).
    pub final_binary: PathBuf,
}

/// Top-level CLI dispatch. Wired from `Commands::BuildFromSource` in
/// `main.rs`.
pub fn run(tool: &str, target: Option<String>, version: Option<String>) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let plan = resolve_plan(tool, target, version, &paths)?;
    let report = execute_plan(&plan)?;
    println!(
        "soldr build-from-source: wrote {} (sha256 {})",
        report.binary.display(),
        report.sha256,
    );
    Ok(())
}

/// Resolve the input triple into a [`BuildPlan`] without touching the
/// network or invoking cargo. Exposed for unit tests so the dispatch +
/// version-defaulting path is exercised without a real install.
pub fn resolve_plan(
    tool: &str,
    target: Option<String>,
    version: Option<String>,
    paths: &SoldrPaths,
) -> Result<BuildPlan, SoldrError> {
    if !SUPPORTED_TOOLS.contains(&tool) {
        return Err(SoldrError::Other(unsupported_tool_message(tool)));
    }

    let triple = match target {
        Some(t) => {
            // Validate the triple shape via `from_triple`. We only need
            // the triple string downstream, but rejecting garbage early
            // produces a clearer error than what cargo would emit.
            let parsed = TargetTriple::from_triple(&t)?;
            parsed.triple()
        }
        None => TargetTriple::detect()?.triple(),
    };

    let version = match version {
        Some(v) if !v.trim().is_empty() => v.trim().trim_start_matches('v').to_string(),
        _ => default_version_for(tool)?.to_string(),
    };

    let install_dir = paths
        .bin
        .join(format!("{tool}{FROM_SOURCE_SUBDIR_SUFFIX}"))
        .join(&version)
        .join(&triple);
    let binary_name = format!("{tool}{}", binary_ext_for_triple(&triple));
    let final_binary = install_dir.join(&binary_name);

    Ok(BuildPlan {
        tool: tool.to_string(),
        target: triple,
        version,
        install_dir,
        final_binary,
    })
}

/// Source-build result returned to [`run`] for printing.
/// Kill switch for routing source-build rustc invocations through
/// soldr's managed zccache wrapper. Falsy values (`0` / `false` / `no` /
/// `off`, case-insensitive) restore the historical fully-uncached
/// behavior. `ZCCACHE_DISABLE` (the standard cache kill switch) is
/// honored too.
pub(crate) const SOURCE_BUILD_CACHE_ENV_VAR: &str = "SOLDR_SOURCE_BUILD_CACHE";

fn source_build_cache_disabled() -> bool {
    let falsy = |value: std::ffi::OsString| crate::core::is_off_value(&value.to_string_lossy());
    if std::env::var_os(SOURCE_BUILD_CACHE_ENV_VAR).is_some_and(falsy) {
        return true;
    }
    // ZCCACHE_DISABLE is truthy-to-disable (opposite polarity).
    std::env::var_os("ZCCACHE_DISABLE")
        .is_some_and(|value| crate::core::flag_value(&value.to_string_lossy()))
}

/// Route the source-build `cargo install`'s rustc invocations through
/// soldr's compiler-named zccache wrapper shim so explicit tool source builds
/// hit the shared object cache instead of recompiling every dependency from
/// scratch on each fresh machine/container (issue #1788). Best-effort image
/// preparation is retained, but once installed the cacheable compiler route is
/// mandatory and broker/daemon infrastructure failures are hard failures.
pub(crate) fn apply_source_build_cache_wrapper(command: &mut std::process::Command) {
    if source_build_cache_disabled() {
        return;
    }
    let Ok(paths) = SoldrPaths::new() else {
        return;
    };
    match crate::binaries::rustc_wrapper_shim_binary(&paths) {
        Ok(shim) => {
            // The shim alone is not a working route: each compile re-enters
            // soldr as the wrapper and resolves the broker daemon route by
            // SOLDR_BROKER_SERVICE. Nothing on this path had registered the
            // daemon image, so in a fresh root every rustc died with
            // "cannot resolve the broker daemon route (os error 2)" and the
            // whole source build failed (soldr#2492's audit). Same fix as
            // the maturin caller-wrapper branch (soldr#2451): register the
            // image and pass the service name down.
            match crate::zccache::register_broker_daemon_service() {
                Ok((_daemon, service_name)) => {
                    command.env(
                        crate::daemon::backend_handle_adoption::SOLDR_BROKER_SERVICE_ENV_VAR,
                        service_name,
                    );
                    crate::wrapper_identity::set_owned_rustc_wrapper(
                        command,
                        shim.as_os_str(),
                        crate::wrapper_identity::WrapperOrigin::SourceBuild,
                    );
                }
                Err(error) => {
                    eprintln!(
                        "soldr build-from-source: could not register the broker daemon                          route; building uncached: {error}"
                    );
                }
            }
        }
        Err(error) => {
            eprintln!(
                "soldr build-from-source: cache wrapper unavailable, building uncached: {error}"
            );
        }
    }
}

#[derive(Debug, Clone)]
pub struct BuildReport {
    pub binary: PathBuf,
    pub sha256: String,
    pub sidecar: PathBuf,
}

/// Validate a previously published source build before an automatic caller
/// reuses it. `execute_plan` writes the sidecar only after the binary copy is
/// complete, so a missing or mismatched sidecar also detects interrupted
/// publication.
pub(crate) fn cached_build_is_valid(plan: &BuildPlan) -> Result<bool, SoldrError> {
    if !plan.final_binary.is_file() {
        return Ok(false);
    }
    let sidecar = plan.final_binary.with_extension("sha256");
    let Ok(contents) = std::fs::read_to_string(&sidecar) else {
        return Ok(false);
    };
    let mut fields = contents.split_whitespace();
    let Some(expected_hash) = fields.next() else {
        return Ok(false);
    };
    let Some(expected_name) = fields.next() else {
        return Ok(false);
    };
    if fields.next().is_some()
        || expected_hash.len() != 64
        || !expected_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        || plan.final_binary.file_name().and_then(|name| name.to_str()) != Some(expected_name)
    {
        return Ok(false);
    }
    Ok(sha256_of_file(&plan.final_binary)? == expected_hash.to_ascii_lowercase())
}

/// Actually invoke `cargo install <tool>@<version> --target <triple>
/// --root <staging>` and move the resulting binary into the per-(tool,
/// version, triple) install dir. Resolves cargo through
/// [`resolve_toolchain_binary`] directly, scrubs inherited wrappers, and
/// then opts into soldr's own compiler-named zccache wrapper (unless
/// `SOLDR_SOURCE_BUILD_CACHE=off`) so the build's rustc invocations are
/// cached without re-entering the cargo front door.
pub fn execute_plan(plan: &BuildPlan) -> Result<BuildReport, SoldrError> {
    // Honour the standard tool-fetch retry budget so transient crates.io /
    // registry hiccups don't spuriously fail the source-build verb.
    let cargo = resolve_toolchain_binary("cargo")?;
    let parent = plan.install_dir.parent().ok_or_else(|| {
        SoldrError::Other(format!(
            "build-from-source: install dir has no parent: {}",
            plan.install_dir.display()
        ))
    })?;
    std::fs::create_dir_all(parent)?;

    let staging = tempfile::tempdir_in(parent).map_err(|e| {
        SoldrError::Other(format!(
            "build-from-source: failed to create staging dir under {}: {e}",
            parent.display()
        ))
    })?;
    let staging_root = staging.path().to_path_buf();
    let staging_bin = staging_root.join("bin");

    // Mirror zccache_install: prepend the staging bin to PATH so cargo
    // doesn't emit the "be sure to add `<root>/bin` to your PATH"
    // warning that's noise for our staging-then-move pipeline.
    let staging_path_env = match std::env::var_os("PATH") {
        Some(existing) => {
            let mut dirs: Vec<PathBuf> = vec![staging_bin.clone()];
            dirs.extend(std::env::split_paths(&existing));
            std::env::join_paths(dirs).map_err(|e| {
                SoldrError::Other(format!(
                    "build-from-source: failed to extend PATH for cargo install: {e}"
                ))
            })?
        }
        None => staging_bin.clone().into_os_string(),
    };

    let mut backoff = RETRY_INITIAL_BACKOFF;
    let mut attempt = 1u32;
    loop {
        let mut command = std::process::Command::new(&cargo);
        command
            .arg("install")
            .arg(format!("{}@{}", plan.tool, plan.version))
            .arg("--target")
            .arg(&plan.target)
            .arg("--locked")
            .arg("--root")
            .arg(&staging_root)
            .arg("--force")
            // Source acquisition must not inherit the caller workspace's
            // rust-toolchain.toml or .cargo/config.toml. Besides making a
            // managed tool build depend on unrelated project policy, rustup
            // proxies can race while auto-installing listed components when
            // Cargo starts parallel build scripts. The staging root is an
            // intentionally manifest-free, neutral working directory.
            .current_dir(&staging_root)
            .env("PATH", &staging_path_env)
            // Strip stale jobserver env so the nested cargo doesn't try
            // to attach to fds it cannot see (see soldr #283).
            .env_remove("MAKEFLAGS")
            .env_remove("CARGO_MAKEFLAGS")
            .env_remove("RUSTC_WORKSPACE_WRAPPER");
        // An accidentally-inherited wrapper env must never leak into this
        // spawn; caching, when enabled, is opted into explicitly below with
        // soldr's own compiler-named wrapper shim. Scrub the identity
        // mirror together with `RUSTC_WRAPPER` (soldr#2545) — removing one
        // while the other leaks through is exactly the drift the wrapper
        // re-entry assertion rejects.
        crate::wrapper_identity::remove_owned_rustc_wrapper(&mut command);
        apply_source_build_cache_wrapper(&mut command);
        suppress_windows_console_window(&mut command);

        let status = run_cargo_install_attempt(&mut command, plan)?;

        if status.success() {
            break;
        }
        if attempt >= SOURCE_BUILD_INSTALL_ATTEMPTS {
            return Err(SoldrError::Other(format!(
                "build-from-source: cargo install {}@{} --target {} failed with status {status}",
                plan.tool, plan.version, plan.target,
            )));
        }
        eprintln!(
            "soldr build-from-source: cargo install {}@{} --target {} failed (attempt {attempt}/{}); retrying in {:?}",
            plan.tool, plan.version, plan.target, SOURCE_BUILD_INSTALL_ATTEMPTS, backoff,
        );
        std::thread::sleep(backoff);
        backoff = backoff.saturating_mul(2);
        attempt += 1;
    }

    let binary_name = format!("{}{}", plan.tool, binary_ext_for_triple(&plan.target));
    let installed = staging_bin.join(&binary_name);
    if !installed.is_file() {
        return Err(SoldrError::Other(format!(
            "build-from-source: cargo install completed but {} not produced (staging: {})",
            installed.display(),
            staging_root.display(),
        )));
    }

    // Wipe any prior install at this (tool, version, triple) so a
    // re-run is idempotent and doesn't leave a stale sidecar behind.
    if plan.install_dir.exists() {
        std::fs::remove_dir_all(&plan.install_dir)?;
    }
    std::fs::create_dir_all(&plan.install_dir)?;
    std::fs::copy(&installed, &plan.final_binary).map_err(|e| {
        SoldrError::Other(format!(
            "build-from-source: failed to move {} -> {}: {e}",
            installed.display(),
            plan.final_binary.display(),
        ))
    })?;

    crate::platform::fs::permissions::make_executable(&plan.final_binary)?;

    let sha256 = sha256_of_file(&plan.final_binary)?;
    let sidecar = plan.final_binary.with_extension("sha256");
    std::fs::write(&sidecar, format!("{sha256}  {binary_name}\n"))?;

    drop(staging);
    Ok(BuildReport {
        binary: plan.final_binary.clone(),
        sha256,
        sidecar,
    })
}

fn unsupported_tool_message(tool: &str) -> String {
    let supported = SUPPORTED_TOOLS.join(", ");
    format!(
        "build-from-source: tool `{tool}` is not supported. Supported tools: {supported}. \
         Use `soldr cargo install <crate>` for generic source builds."
    )
}

fn default_version_for(tool: &str) -> Result<&'static str, SoldrError> {
    let spec = known_tools::lookup_by_crate(tool).ok_or_else(|| {
        SoldrError::Other(format!(
            "build-from-source: tool `{tool}` is in the supported list but not registered in known_tools — this is a soldr bug",
        ))
    })?;
    spec.pinned_version.ok_or_else(|| {
        SoldrError::Other(format!(
            "build-from-source: tool `{tool}` has no pinned_version in known_tools; pass --version explicitly",
        ))
    })
}

fn binary_ext_for_triple(triple: &str) -> &'static str {
    if triple.contains("-pc-windows-") {
        ".exe"
    } else {
        ""
    }
}

/// Test-only tripwire (soldr#2436 phase 1, D9): when set truthy, every
/// source-build chokepoint errors instead of spawning cargo. The dylint
/// containment tests set it on every invocation so "no soldr code path
/// compiles implicitly" is asserted structurally rather than resting on
/// one regression test. Never set outside tests.
pub(crate) const FORBID_SOURCE_BUILD_ENV_VAR: &str = "SOLDR_TEST_FORBID_SOURCE_BUILD";

pub(crate) fn forbid_source_build_tripwire(chokepoint: &str) -> Result<(), SoldrError> {
    let tripped = std::env::var(FORBID_SOURCE_BUILD_ENV_VAR)
        .map(|value| crate::core::flag_value(&value))
        .unwrap_or(false);
    if tripped {
        return Err(SoldrError::Other(format!(
            "test tripwire: source-build chokepoint reached ({chokepoint}) with \
             {FORBID_SOURCE_BUILD_ENV_VAR} set — an implicit compile path survived"
        )));
    }
    Ok(())
}

fn run_cargo_install_attempt(
    command: &mut std::process::Command,
    plan: &BuildPlan,
) -> Result<std::process::ExitStatus, SoldrError> {
    forbid_source_build_tripwire("build-from-source cargo install")?;
    crate::exit_guard::run_child_command(
        command,
        &format!(
            "build-from-source: cargo install {}@{} --target {}",
            plan.tool, plan.version, plan.target,
        ),
        "source-build",
        InstallerWatchdogConfig::from_env(CARGO_INSTALL_TIMEOUT_ENV_VAR),
    )
}

pub(crate) fn sha256_of_file(path: &Path) -> Result<String, SoldrError> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)
        .map_err(|e| SoldrError::Other(format!("open {}: {e}", path.display())))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .map_err(|e| SoldrError::Other(format!("hash {}: {e}", path.display())))?;
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest.iter() {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::known_tools::{lookup_by_crate, CARGO_CHEF_PINNED_VERSION};
    use crate::fetch::MANAGED_CRGX_VERSION;
    use crate::platform::sync::Mutex;
    use std::ffi::{OsStr, OsString};

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn synthetic_paths(tmp: &Path) -> SoldrPaths {
        let root = tmp.join("soldr-home");
        std::fs::create_dir_all(&root).unwrap();
        let paths = SoldrPaths::with_root(root);
        std::fs::create_dir_all(&paths.bin).unwrap();
        paths
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn cargo_install_timeout_is_an_explicit_safety_ceiling() {
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        {
            let _guard = EnvVarGuard::set(CARGO_INSTALL_TIMEOUT_ENV_VAR, "11");
            assert_eq!(
                InstallerWatchdogConfig::from_env(CARGO_INSTALL_TIMEOUT_ENV_VAR).safety_timeout,
                Duration::from_secs(11)
            );
        }

        for value in ["0", "-1", "not-a-number"] {
            let _guard = EnvVarGuard::set(CARGO_INSTALL_TIMEOUT_ENV_VAR, value);
            assert_eq!(
                InstallerWatchdogConfig::from_env(CARGO_INSTALL_TIMEOUT_ENV_VAR).safety_timeout,
                Duration::from_secs(crate::core::DEFAULT_INSTALLER_SAFETY_TIMEOUT_SECS)
            );
        }

        let _guard = EnvVarGuard::remove(CARGO_INSTALL_TIMEOUT_ENV_VAR);
        assert_eq!(
            InstallerWatchdogConfig::from_env(CARGO_INSTALL_TIMEOUT_ENV_VAR).safety_timeout,
            Duration::from_secs(crate::core::DEFAULT_INSTALLER_SAFETY_TIMEOUT_SECS)
        );
    }

    #[test]
    fn unsupported_tool_errors_with_directive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = synthetic_paths(tmp.path());
        let err = resolve_plan(
            "cargo-foo",
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
            &paths,
        )
        .expect_err("unsupported tool must error");
        let msg = format!("{err}");
        assert!(msg.contains("crgx"), "directive must list crgx, got: {msg}");
        assert!(
            msg.contains("cargo-chef"),
            "directive must list cargo-chef, got: {msg}"
        );
        assert!(
            msg.contains("cargo-foo"),
            "directive must mention the bad input, got: {msg}"
        );
    }

    #[test]
    fn default_target_resolves_to_host() {
        // `target: None` must not panic and must produce a non-empty
        // triple matching the auto-detected host. We don't assert the
        // exact triple since the test harness host varies across CI
        // matrices, but the result should round-trip through
        // `TargetTriple::from_triple` (i.e. be a real triple).
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = synthetic_paths(tmp.path());
        let plan = resolve_plan("crgx", None, None, &paths).expect("resolve must succeed");
        let host = TargetTriple::detect().expect("host detect").triple();
        assert_eq!(plan.target, host, "default target must equal detected host");
        // Roundtrip validates the triple shape.
        TargetTriple::from_triple(&plan.target).expect("triple is well-formed");
        // install_dir must be under bin/<tool>-from-source/<version>/<triple>/
        let expected_parent = paths
            .bin
            .join(format!("crgx{FROM_SOURCE_SUBDIR_SUFFIX}"))
            .join(&plan.version)
            .join(&plan.target);
        assert_eq!(plan.install_dir, expected_parent);
    }

    #[test]
    fn version_defaults_to_known_tools_pinned() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = synthetic_paths(tmp.path());

        let crgx = resolve_plan(
            "crgx",
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
            &paths,
        )
        .expect("crgx resolve");
        assert_eq!(
            crgx.version, MANAGED_CRGX_VERSION,
            "crgx default version must come from MANAGED_CRGX_VERSION via known_tools",
        );
        assert_eq!(
            lookup_by_crate("crgx")
                .and_then(|spec| spec.pinned_version)
                .map(str::to_string),
            Some(crgx.version.clone()),
            "known_tools::lookup_by_crate must agree with resolve_plan",
        );

        let chef = resolve_plan(
            "cargo-chef",
            Some("aarch64-apple-darwin".to_string()),
            None,
            &paths,
        )
        .expect("cargo-chef resolve");
        assert_eq!(
            chef.version, CARGO_CHEF_PINNED_VERSION,
            "cargo-chef default version must match the registry pin",
        );
        let dylint = resolve_plan(
            "cargo-dylint",
            Some("x86_64-pc-windows-msvc".to_string()),
            None,
            &paths,
        )
        .expect("cargo-dylint resolve");
        assert_eq!(dylint.version, "6.2.0");
        let dylint_link = resolve_plan(
            "dylint-link",
            Some("x86_64-pc-windows-msvc".to_string()),
            None,
            &paths,
        )
        .expect("dylint-link resolve");
        assert_eq!(dylint_link.version, dylint.version);

        // Explicit --version still wins.
        let explicit = resolve_plan(
            "cargo-chef",
            Some("aarch64-apple-darwin".to_string()),
            Some("0.1.99".to_string()),
            &paths,
        )
        .expect("cargo-chef explicit version");
        assert_eq!(explicit.version, "0.1.99");
        // A leading `v` is stripped so callers can pass either form.
        let v_prefixed = resolve_plan(
            "cargo-chef",
            Some("aarch64-apple-darwin".to_string()),
            Some("v0.1.42".to_string()),
            &paths,
        )
        .expect("cargo-chef v-prefixed version");
        assert_eq!(v_prefixed.version, "0.1.42");
    }

    #[test]
    fn cached_build_validation_rejects_partial_missing_and_mismatched_sidecars() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = synthetic_paths(tmp.path());
        let plan = resolve_plan(
            "dylint-link",
            Some("x86_64-unknown-linux-gnu".to_string()),
            None,
            &paths,
        )
        .expect("resolve");
        std::fs::create_dir_all(&plan.install_dir).unwrap();
        std::fs::write(&plan.final_binary, b"partial").unwrap();

        assert!(
            !cached_build_is_valid(&plan).unwrap(),
            "a partial binary without its commit sidecar must be rejected"
        );

        let sidecar = plan.final_binary.with_extension("sha256");
        std::fs::write(
            &sidecar,
            format!(
                "{}  {}\n",
                "0".repeat(64),
                plan.final_binary.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();
        assert!(
            !cached_build_is_valid(&plan).unwrap(),
            "a mismatched sidecar must be rejected"
        );

        let digest = sha256_of_file(&plan.final_binary).unwrap();
        std::fs::write(
            &sidecar,
            format!(
                "{digest}  {}\n",
                plan.final_binary.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();
        assert!(cached_build_is_valid(&plan).unwrap());

        std::fs::write(&plan.final_binary, b"tampered-after-sidecar").unwrap();
        assert!(
            !cached_build_is_valid(&plan).unwrap(),
            "a binary changed after publication must be rejected"
        );
    }

    #[test]
    fn windows_triple_uses_exe_suffix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = synthetic_paths(tmp.path());
        let plan = resolve_plan(
            "crgx",
            Some("x86_64-pc-windows-msvc".to_string()),
            None,
            &paths,
        )
        .expect("resolve");
        assert!(
            plan.final_binary
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.ends_with(".exe"))
                .unwrap_or(false),
            "windows triple must produce a .exe binary path, got: {}",
            plan.final_binary.display(),
        );
    }

    #[test]
    fn invalid_triple_rejected_at_parse_time() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = synthetic_paths(tmp.path());
        let err = resolve_plan("crgx", Some("not-a-real-triple".to_string()), None, &paths)
            .expect_err("bogus triple must error");
        let msg = format!("{err}");
        assert!(
            msg.to_lowercase().contains("triple") || msg.to_lowercase().contains("target"),
            "error should mention target/triple, got: {msg}",
        );
    }

    #[test]
    fn source_build_cache_gate_honors_both_kill_switches() {
        // Serialize against other env-mutating tests in this binary.
        let restore = |key: &str, previous: Option<std::ffi::OsString>| match previous {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        };
        let prev_cache = std::env::var_os(SOURCE_BUILD_CACHE_ENV_VAR);
        let prev_disable = std::env::var_os("ZCCACHE_DISABLE");

        std::env::remove_var(SOURCE_BUILD_CACHE_ENV_VAR);
        std::env::remove_var("ZCCACHE_DISABLE");
        assert!(!source_build_cache_disabled(), "default must be cached");

        std::env::set_var(SOURCE_BUILD_CACHE_ENV_VAR, "off");
        assert!(source_build_cache_disabled());
        std::env::set_var(SOURCE_BUILD_CACHE_ENV_VAR, "0");
        assert!(source_build_cache_disabled());
        std::env::set_var(SOURCE_BUILD_CACHE_ENV_VAR, "on");
        assert!(!source_build_cache_disabled());
        std::env::remove_var(SOURCE_BUILD_CACHE_ENV_VAR);

        std::env::set_var("ZCCACHE_DISABLE", "1");
        assert!(
            source_build_cache_disabled(),
            "the standard zccache kill switch must also disable source-build caching"
        );

        restore(SOURCE_BUILD_CACHE_ENV_VAR, prev_cache);
        restore("ZCCACHE_DISABLE", prev_disable);
    }
}
