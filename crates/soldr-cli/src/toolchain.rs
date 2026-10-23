//! Rustup toolchain passthrough commands (`rustc`, `rustfmt`, etc.), the
//! `soldr rustup` front door, and `soldr toolchain install` / `prepare`.
//! Extracted from `main.rs` as part of issue #339.

use crate::core::{
    suppress_windows_console_window, InstallerWatchdogConfig, SoldrError, SoldrPaths,
};
use crate::{
    apply_implicit_toolchain_homes, resolve_toolchain_binary, resolve_toolchain_binary_for_channel,
    rustup_binary,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

/// Escape hatch for soldr#1766: allow a build to proceed with no
/// `rust-toolchain.toml` anywhere at or above the working directory.
///
/// Without a pin, soldr resolves `rustc` from `PATH`, which is the exact
/// failure the pin exists to prevent -- and it makes cache keys depend on
/// ambient `PATH` state, so two hosts can disagree about "identical" builds.
/// Opting out is supported; opting out *silently* is not.
pub const ALLOW_UNPINNED_ENV_VAR: &str = "SOLDR_ALLOW_UNPINNED";

/// rustup's own explicit-toolchain selector. Honoured as a pin by
/// [`require_toolchain_pin`] (soldr#1917 follow-up).
pub const RUSTUP_TOOLCHAIN_ENV_VAR: &str = "RUSTUP_TOOLCHAIN";

pub const TOOLCHAIN_COMMAND_TIMEOUT_ENV_VAR: &str = "SOLDR_TOOLCHAIN_COMMAND_TIMEOUT_SECS";
const CARGO_PREPARE_MEMO_SCHEMA_VERSION: u32 = 1;
const CARGO_PREPARE_MEMO_DIR: &str = "toolchain-prepare-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CargoPrepareMemoKey {
    schema_version: u32,
    channel: String,
    explicit_channel: Option<String>,
    profile: Option<String>,
    components: Vec<String>,
    targets: Vec<String>,
    rustup_home: PathBuf,
    rustup_binary: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FileIdentity {
    path: PathBuf,
    len: u64,
    modified_ns: Option<u128>,
    sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ToolchainIdentity {
    toolchain_dir: PathBuf,
    rustup_binary: FileIdentity,
    rustc_binary: FileIdentity,
    channel_manifest: FileIdentity,
    components_manifest: FileIdentity,
    /// soldr#3376: `(triple, libcore length)` for the host and declared targets.
    std_libs: Vec<(String, u64)>,
}

#[derive(Debug, Serialize)]
struct CargoPrepareFingerprint<'a> {
    key: &'a CargoPrepareMemoKey,
    identity: ToolchainIdentity,
}

/// Run a rustup-managed toolchain binary with pass-through args.
pub(crate) fn run_toolchain_passthrough(tool: &str, args: &[String]) -> Result<i32, SoldrError> {
    let binary = resolve_dylint_scoped_binary(tool)?;
    let mut command = std::process::Command::new(&binary);
    command.args(args);
    crate::binaries::apply_resolved_toolchain_homes(&mut command, &binary);
    suppress_windows_console_window(&mut command);
    let status = run_toolchain_command(&mut command, &format!("{tool} passthrough"))?;
    Ok(status.code().unwrap_or(1))
}

/// Run rustc-like toolchain binaries through the same wrapper path Cargo
/// uses when caching is enabled. The wrapper path owns zccache routing and
/// non-cacheable probe bypasses; cache-disabled direct invocations keep the
/// historical passthrough behavior.
pub(crate) fn run_rustc_like(
    tool: &str,
    args: &[String],
    cache_enabled: bool,
) -> Result<i32, SoldrError> {
    if !cache_enabled {
        return run_toolchain_passthrough(tool, args);
    }

    let binary = resolve_dylint_scoped_binary(tool)?;
    let mut raw_args = Vec::with_capacity(args.len() + 2);
    raw_args.push(
        crate::current_soldr_binary()?
            .to_string_lossy()
            .into_owned(),
    );
    raw_args.push(binary.to_string_lossy().into_owned());
    raw_args.extend(args.iter().cloned());
    crate::wrapper::run_rustc_wrapper(&raw_args, crate::startup_profile::WrapperProfile::new())
}

/// Run rustfmt directly for non-cacheable invocations, otherwise route
/// cacheable file-formatting calls through zccache's formatter path.
pub(crate) fn run_rustfmt(args: &[String], cache_enabled: bool) -> Result<i32, SoldrError> {
    if !cache_enabled || rustfmt_invocation_bypasses_format_cache(args) {
        return run_toolchain_passthrough("rustfmt", args);
    }

    let rustfmt = resolve_dylint_scoped_binary("rustfmt")?;
    let cache_root = if let Some(path) =
        crate::binaries::non_empty_env_path(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR)
    {
        crate::zccache::normalize_path_for_compare(&path)?
    } else {
        let paths = SoldrPaths::new()?;
        crate::zccache::managed_zccache_cache_dir(&paths)?
    };
    std::fs::create_dir_all(&cache_root)?;
    let cwd = std::env::current_dir()?;
    // soldr#2877: snapshot explicitly named source files before the daemon
    // lease is acquired, so a killed formatter child cannot be mistaken for
    // a clean failure.
    let snapshot = crate::rustfmt_admission::SourceSnapshot::capture(args, &cwd);
    let controller = crate::rustfmt_admission::DaemonRustfmtLeaseController {
        permits: crate::rustfmt_admission::RUSTFMT_RESIDENT_PERMITS,
    };
    // soldr#2899: the daemon-free `formatter` API, not the CLI dispatcher.
    // Soldr keeps ownership of child-process policy through the runner.
    let result = crate::rustfmt_admission::with_rustfmt_lease(&controller, || {
        zccache::formatter::run_rustfmt_cached_with_runner(
            &rustfmt,
            args,
            &cwd,
            Some(&cache_root),
            |command| {
                crate::binaries::apply_resolved_toolchain_homes(command, &rustfmt);
                apply_zccache_child_env(command)
                    .map_err(|err| std::io::Error::other(err.to_string()))?;
                suppress_windows_console_window(command);
                let status = run_toolchain_command(command, "embedded rustfmt formatter")
                    .map_err(|err| std::io::Error::other(err.to_string()))?;
                Ok(status.code().unwrap_or(1))
            },
        )
    });

    let succeeded = matches!(result, Ok(0));
    if let Err(err) = snapshot.restore_if_damaged(succeeded) {
        eprintln!(
            "soldr rustfmt: failed to verify source integrity after formatter failure: {err}"
        );
    }

    result.map_err(SoldrError::from)
}

/// Run rustdoc directly. zccache currently has rustc/clippy-driver
/// compile routes and a rustfmt formatter route, but no rustdoc driver
/// route. Cargo doc/doctest still cache their rustc compile units via
/// the cargo front door's `RUSTC_WRAPPER=soldr` injection.
pub(crate) fn run_rustdoc(args: &[String]) -> Result<i32, SoldrError> {
    run_toolchain_passthrough("rustdoc", args)
}

/// Run rust-analyzer itself as a direct rustup-managed LSP passthrough.
/// When caching is enabled, soldr does not wrap the long-lived language
/// server binary. Instead it gives rust-analyzer a scoped child environment
/// so any bare `cargo` / `rustc` / `rustfmt` / `clippy-driver` process it
/// spawns can re-enter Soldr and use the normal zccache cargo front door.
pub(crate) fn run_rust_analyzer(args: &[String], cache_enabled: bool) -> Result<i32, SoldrError> {
    let binary = resolve_toolchain_binary("rust-analyzer")?;
    let mut command = std::process::Command::new(&binary);
    command.args(args);
    crate::binaries::apply_resolved_toolchain_homes(&mut command, &binary);
    command.env(
        crate::cache_lib::CACHE_ENABLED_ENV_VAR,
        crate::cache_lib::cache_enabled_env_value(cache_enabled),
    );

    let _shim_guard = if cache_enabled {
        apply_zccache_child_env(&mut command)?;
        maybe_apply_child_shim_dir(&mut command, "rust-analyzer")
    } else {
        None
    };

    suppress_windows_console_window(&mut command);
    let status = run_toolchain_command(&mut command, "rust-analyzer passthrough")?;
    Ok(status.code().unwrap_or(1))
}

fn apply_zccache_child_env(command: &mut std::process::Command) -> Result<(), SoldrError> {
    if std::env::var_os(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR).is_none() {
        let paths = SoldrPaths::new()?;
        let cache_dir = crate::zccache::managed_zccache_cache_dir(&paths)?;
        std::fs::create_dir_all(&cache_dir)?;
        command.env(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR, &cache_dir);
        command.env(
            crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR,
            &cache_dir,
        );
    }
    command.env(
        crate::cache_lib::CACHE_ENABLED_ENV_VAR,
        crate::cache_lib::cache_enabled_env_value(true),
    );
    crate::zccache::ZccacheChildEnv::from_current_process()?.apply_to_command(command);
    Ok(())
}

fn maybe_apply_child_shim_dir(
    command: &mut std::process::Command,
    context: &str,
) -> Option<crate::shim_dir::ShimDirGuard> {
    if !crate::shim_dir::should_install_shims() {
        return None;
    }

    match crate::shim_dir::build_shim_dir() {
        Ok(guard) => {
            crate::shim_dir::apply_to_command(command, &guard.path);
            Some(guard)
        }
        Err(err) => {
            eprintln!(
                "soldr warning: failed to build child shim dir for {context}; \
                 nested cargo/rustc calls will bypass soldr: {err}"
            );
            None
        }
    }
}

fn rustfmt_invocation_bypasses_format_cache(args: &[String]) -> bool {
    !rustfmt_invocation_has_source_file(args)
}

fn rustfmt_invocation_has_source_file(args: &[String]) -> bool {
    const FLAGS_WITH_VALUE: &[&str] = &[
        "--edition",
        "--config-path",
        "--config",
        "--color",
        "--print-config",
        "--files-with-diff",
        "--file-lines",
    ];

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if matches!(arg, "--help" | "-h" | "--version" | "-V") {
            return false;
        }
        if FLAGS_WITH_VALUE.contains(&arg) {
            index += 2;
            continue;
        }
        if arg.starts_with("--") && arg.contains('=') {
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        if arg.ends_with(".rs") {
            return true;
        }
        index += 1;
    }
    false
}

/// Drop-in passthrough for `soldr rustup ...`.
///
/// Most invocations forward verbatim. The one exception is the "scoped"
/// pin: when the first positional argument is `target` or `component`
/// (the two rustup subcommands that mutate per-toolchain state) AND
/// `rust-toolchain.toml` declares a `channel`, soldr inserts
/// `--toolchain <channel>` immediately after the user's first positional
/// argument so the call lands on the pinned toolchain rather than the
/// rustup default. If the user already supplied `--toolchain` anywhere,
/// the injection is skipped.
pub(crate) fn run_rustup_passthrough(args: &[String]) -> Result<i32, SoldrError> {
    let dylint_channel = dylint_scoped_channel();
    let final_args = if let Some(channel) = dylint_channel.as_deref() {
        scope_rustup_args_to_dylint(args, channel)
    } else {
        scope_rustup_args_to_pin(args)?
    };
    if rustup_args_would_download(&final_args) {
        crate::core::forbid_toolchain_install_tripwire(&format!(
            "rustup {}",
            final_args.join(" ")
        ))?;
    }
    let mut command = std::process::Command::new(rustup_binary());
    if let Some(channel) = dylint_channel {
        command.env("RUSTUP_TOOLCHAIN", channel);
    }
    command.args(&final_args);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let status = run_toolchain_command(&mut command, "rustup passthrough")?;
    Ok(status.code().unwrap_or(1))
}

/// Whether a `soldr rustup` passthrough could download a toolchain,
/// component or target (soldr#3195): `toolchain install`, `component add`,
/// `target add`, and the verbs that install implicitly (`install`, `update`,
/// `default`). Flags and `+toolchain` shorthands are skipped to find the verb.
pub(crate) fn rustup_args_would_download(args: &[String]) -> bool {
    let mut positionals = args
        .iter()
        .map(String::as_str)
        .filter(|arg| !arg.starts_with('-') && !arg.starts_with('+'));
    matches!(
        (positionals.next(), positionals.next()),
        (Some("toolchain"), Some("install"))
            | (Some("component"), Some("add"))
            | (Some("target"), Some("add"))
            | (Some("install" | "update" | "default"), _)
    )
}

fn scope_rustup_args_to_dylint(args: &[String], channel: &str) -> Vec<String> {
    let mut scoped = Vec::with_capacity(args.len());
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--toolchain" {
            skip_next = true;
            continue;
        }
        if arg.starts_with("--toolchain=") || arg.starts_with('+') {
            continue;
        }
        scoped.push(arg.clone());
    }
    if let Some(run_index) = scoped.iter().position(|arg| arg == "run") {
        if run_index + 1 < scoped.len() {
            scoped[run_index + 1] = channel.to_string();
        } else {
            scoped.push(channel.to_string());
        }
    }
    scoped
}

fn resolve_dylint_scoped_binary(tool: &str) -> Result<std::path::PathBuf, SoldrError> {
    let channel = dylint_scoped_channel();
    resolve_toolchain_binary_for_channel(tool, channel.as_deref())
}

fn dylint_scoped_channel() -> Option<String> {
    std::env::var(crate::dylint_toolchain::TOOLCHAIN_ENV_VAR)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn scope_rustup_args_to_pin(args: &[String]) -> Result<Vec<String>, SoldrError> {
    let start_dir = std::env::current_dir().map_err(SoldrError::from)?;
    let rustup_toolchain = std::env::var_os(RUSTUP_TOOLCHAIN_ENV_VAR);
    scope_rustup_args_to_pin_in(args, &start_dir, rustup_toolchain.as_deref())
}

fn scope_rustup_args_to_pin_in(
    args: &[String],
    start_dir: &std::path::Path,
    rustup_toolchain_env: Option<&std::ffi::OsStr>,
) -> Result<Vec<String>, SoldrError> {
    // soldr#3692: RUSTUP_TOOLCHAIN beats rust-toolchain.toml in rustup.
    if rustup_toolchain_value_set(rustup_toolchain_env) {
        return Ok(args.to_vec());
    }
    // Find the first non-flag positional. Anything before it (e.g.
    // `--verbose`) is preserved in place.
    let mut first_positional: Option<usize> = None;
    for (idx, arg) in args.iter().enumerate() {
        if !arg.starts_with('-') {
            first_positional = Some(idx);
            break;
        }
    }

    let Some(first_positional) = first_positional else {
        return Ok(args.to_vec());
    };

    let subcommand = args[first_positional].as_str();
    if subcommand != "target" && subcommand != "component" {
        return Ok(args.to_vec());
    }

    if rustup_args_specify_toolchain(args) {
        return Ok(args.to_vec());
    }

    // Walk ancestors like rustup/cargo (soldr#3633).
    let manifest = crate::core::read_rust_toolchain_manifest_from_ancestors(start_dir)?;
    let Some(channel) = manifest.channel else {
        return Ok(args.to_vec());
    };

    // Inject `--toolchain <channel>` after the subcommand/verb pair so a
    // call like `target add x86_64-unknown-linux-musl` becomes
    // `target add --toolchain <channel> x86_64-unknown-linux-musl`.
    // The verb is the next non-flag positional after `target`/`component`.
    let mut insertion_idx = first_positional + 1;
    for (offset, arg) in args[first_positional + 1..].iter().enumerate() {
        if !arg.starts_with('-') {
            insertion_idx = first_positional + 1 + offset + 1;
            break;
        }
    }

    let mut out = Vec::with_capacity(args.len() + 2);
    out.extend(args[..insertion_idx].iter().cloned());
    out.push("--toolchain".to_string());
    out.push(channel);
    out.extend(args[insertion_idx..].iter().cloned());
    Ok(out)
}

fn rustup_args_specify_toolchain(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--toolchain" || arg.starts_with("--toolchain="))
}

/// Implementation of `soldr toolchain install`.
pub(crate) fn run_toolchain_install() -> Result<i32, SoldrError> {
    let workspace_root = std::env::current_dir().map_err(SoldrError::from)?;
    // Walk ancestors like rustup/cargo (soldr#3633).
    let manifest = crate::core::read_rust_toolchain_manifest_from_ancestors(&workspace_root)?;
    let Some(channel) = manifest.channel.as_deref() else {
        eprintln!(
            "soldr: no rust-toolchain.toml channel found; nothing to install. \
             Create rust-toolchain.toml with a `[toolchain] channel = \"<version>\"` entry."
        );
        return Ok(0);
    };

    rustup_toolchain_install(channel)
}

/// Implementation of `soldr toolchain prepare`.
pub(crate) fn run_toolchain_prepare() -> Result<i32, SoldrError> {
    let workspace_root = std::env::current_dir().map_err(SoldrError::from)?;
    // Walk ancestors like rustup/cargo (soldr#3633).
    let manifest = crate::core::read_rust_toolchain_manifest_from_ancestors(&workspace_root)?;
    let Some(channel) = manifest.channel.as_deref() else {
        // soldr#3802: without a pin there is no rustup work, but the build
        // tools are still provisionable when the caller accepted an unpinned
        // build (`--allow-unpinned` / `RUSTUP_TOOLCHAIN`).
        if crate::toolchain_prepare::unpinned_provisioning_allowed() {
            eprintln!("soldr: no rust-toolchain.toml channel found; skipping rustup steps.");
            crate::toolchain_prepare::report_build_tools(
                &crate::toolchain_prepare::provision_build_tools()?,
            );
            return Ok(0);
        }
        eprintln!(
            "soldr: no rust-toolchain.toml channel found; nothing to prepare. \
             Create rust-toolchain.toml with a `[toolchain] channel = \"<version>\"` entry, \
             or pass --allow-unpinned to provision build tools without one."
        );
        return Ok(0);
    };

    let (code, _summary) = run_prepare_inner(channel, &manifest)?;
    Ok(code)
}

fn canonical_requirement_list(values: Option<&[String]>) -> Vec<String> {
    let mut values = values
        .unwrap_or_default()
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

/// The `RUSTUP_HOME` a rustup child launched through [`apply_implicit_toolchain_homes`]
/// actually runs under. Read it back from a prepared command rather than
/// re-deriving it, so a caller probing a toolchain directory looks in the home
/// the install wrote to (soldr#3051).
///
/// The probe command is never spawned and the homes applied to it do not
/// depend on its program, so it names a placeholder rather than resolving
/// (and possibly bootstrapping) the real rustup: the toolchain-binary cache
/// keys on this value on every lookup (soldr#3567).
pub(crate) fn effective_rustup_home() -> Option<PathBuf> {
    let mut command = std::process::Command::new("rustup");
    apply_implicit_toolchain_homes(&mut command);
    command
        .get_envs()
        .find_map(|(name, value)| {
            name.eq_ignore_ascii_case("RUSTUP_HOME")
                .then(|| value.map(PathBuf::from))
                .flatten()
        })
        .or_else(crate::core::resolve_rustup_home)
}

fn normalize_existing_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn cargo_prepare_memo_key(
    channel: &str,
    explicit_channel: Option<&str>,
    manifest: &crate::core::RustToolchainManifest,
) -> Option<CargoPrepareMemoKey> {
    let rustup_home = effective_rustup_home()?;
    let rustup_binary = rustup_binary();
    Some(CargoPrepareMemoKey {
        schema_version: CARGO_PREPARE_MEMO_SCHEMA_VERSION,
        channel: channel.to_owned(),
        explicit_channel: explicit_channel
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        profile: manifest
            .profile
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        components: canonical_requirement_list(manifest.components.as_deref()),
        targets: canonical_requirement_list(manifest.targets.as_deref()),
        rustup_home: normalize_existing_path(&rustup_home),
        rustup_binary: normalize_existing_path(&rustup_binary),
    })
}

fn cargo_prepare_memo_path(
    paths: &SoldrPaths,
    key: &CargoPrepareMemoKey,
    identity: ToolchainIdentity,
) -> Option<PathBuf> {
    let encoded = serde_json::to_vec(&CargoPrepareFingerprint { key, identity }).ok()?;
    let digest = Sha256::digest(encoded);
    Some(
        paths
            .cache
            .join(CARGO_PREPARE_MEMO_DIR)
            .join(format!("{digest:x}.ok")),
    )
}

fn file_identity(path: &Path, hash_contents: bool) -> Option<FileIdentity> {
    let metadata = std::fs::metadata(path).ok()?;
    // A content hash is authoritative. Rustup may touch its small
    // `lib/rustlib/components` manifest without changing the bytes, and
    // including that redundant mtime turns an unchanged warm invocation into
    // a false memo miss. Large, unhashed binaries retain the cheaper
    // size+mtime identity.
    let modified_ns = if hash_contents {
        None
    } else {
        Some(
            metadata
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_nanos(),
        )
    };
    let sha256 = if hash_contents {
        let digest = Sha256::digest(std::fs::read(path).ok()?);
        Some(format!("{digest:x}"))
    } else {
        None
    };
    Some(FileIdentity {
        path: normalize_existing_path(path),
        len: metadata.len(),
        modified_ns,
        sha256,
    })
}

fn rustc_binary_path(toolchain_dir: &Path) -> Option<PathBuf> {
    let plain = toolchain_dir.join("bin").join("rustc");
    if plain.is_file() {
        return Some(plain);
    }
    let windows = toolchain_dir.join("bin").join("rustc.exe");
    windows.is_file().then_some(windows)
}

fn toolchain_identity(
    key: &CargoPrepareMemoKey,
    toolchain_dir: &Path,
) -> Option<ToolchainIdentity> {
    if !matches!(
        crate::toolchain_readiness::classify_toolchain_dir(toolchain_dir),
        crate::toolchain_readiness::ToolchainReadiness::Ready
    ) {
        return None;
    }
    let rustc = rustc_binary_path(toolchain_dir)?;
    let channel_manifest =
        toolchain_dir.join(crate::toolchain_readiness::TOOLCHAIN_CHANNEL_MANIFEST);
    let components = toolchain_dir.join("lib").join("rustlib").join("components");
    let host = toolchain_host(toolchain_dir);
    let std_libs = crate::toolchain_std_libs::std_lib_fingerprint(
        toolchain_dir,
        host.as_deref(),
        &key.targets,
    )?;
    Some(ToolchainIdentity {
        toolchain_dir: normalize_existing_path(toolchain_dir),
        rustup_binary: file_identity(&key.rustup_binary, false)?,
        rustc_binary: file_identity(&rustc, false)?,
        channel_manifest: file_identity(&channel_manifest, true)?,
        components_manifest: file_identity(&components, true)?,
        std_libs,
    })
}

fn toolchain_host(toolchain_dir: &Path) -> Option<String> {
    crate::toolchain_std_libs::host_of_toolchain_dir(
        toolchain_dir,
        crate::pyo3_detect::host_triple(),
    )
}

fn memoized_toolchain_dir(paths: &SoldrPaths, key: &CargoPrepareMemoKey) -> Option<PathBuf> {
    let toolchain_dirs = discover_toolchain_dirs(key, None);
    if toolchain_dirs.len() != 1 {
        return None;
    }
    let toolchain_dir = toolchain_dirs.into_iter().next()?;
    let identity = toolchain_identity(key, &toolchain_dir)?;
    cargo_prepare_memo_path(paths, key, identity)
        .is_some_and(|path| path.is_file())
        .then_some(toolchain_dir)
}

fn installed_toolchain_name(output: &[u8], channel: &str) -> Option<String> {
    String::from_utf8_lossy(output).lines().find_map(|line| {
        let name = line.split_whitespace().next()?;
        (name == channel
            || name
                .strip_prefix(channel)
                .is_some_and(|suffix| suffix.starts_with('-')))
        .then(|| name.to_owned())
    })
}

fn discover_toolchain_dirs(
    key: &CargoPrepareMemoKey,
    installed_name: Option<&str>,
) -> Vec<PathBuf> {
    let toolchains = key.rustup_home.join("toolchains");
    if let Some(name) = installed_name {
        let exact = toolchains.join(name);
        if exact.is_dir() {
            return vec![exact];
        }
    }
    let Ok(entries) = std::fs::read_dir(toolchains) else {
        return Vec::new();
    };
    let mut matches = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name == key.channel
                || name
                    .strip_prefix(&key.channel)
                    .is_some_and(|suffix| suffix.starts_with('-'))
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    matches.sort();
    matches
}

fn write_cargo_prepare_memo(paths: &SoldrPaths, key: CargoPrepareMemoKey, toolchain_dir: &Path) {
    let Some(identity) = toolchain_identity(&key, toolchain_dir) else {
        return;
    };
    let Some(path) = cargo_prepare_memo_path(paths, &key, identity) else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&temp, []).is_err() {
        return;
    }
    if path.is_file() {
        let _ = std::fs::remove_file(&temp);
        return;
    }
    if std::fs::rename(&temp, &path).is_err() {
        let _ = std::fs::remove_file(&temp);
    }
}

/// True when the user has explicitly accepted an unpinned build, via
/// `SOLDR_ALLOW_UNPINNED` or the `--allow-unpinned` flag (which sets it).
///
/// Parsed with the canonical owned-switch reader [`crate::core::flag`]: only
/// `1`/`true`/`yes`/`on` enable it, so an unrecognised value such as
/// `disabled` never weakens the soldr#1766 pin guard (soldr#3634).
pub fn unpinned_allowed() -> bool {
    crate::core::flag(ALLOW_UNPINNED_ENV_VAR)
}

/// True when `RUSTUP_TOOLCHAIN` names an explicit toolchain.
///
/// This is rustup's own selection mechanism and it *overrides* any
/// `rust-toolchain.toml`, so treating it as a weaker signal than the manifest
/// has it backwards: it is unambiguous, needs no ancestor search, and cannot
/// be shadowed by a nearer file.
///
/// It is specifically not the hazard [`require_toolchain_pin`] guards against.
/// That error is about the *PATH rustc fallback*, which can resolve to a
/// mismatched-host toolchain; an explicitly selected rustup toolchain is the
/// opposite of that. Note `probe_direct_toolchain_binary` already defers to
/// rustup when this is set, so the resolution path agrees.
fn rustup_toolchain_pinned() -> bool {
    rustup_toolchain_value_set(std::env::var_os(RUSTUP_TOOLCHAIN_ENV_VAR).as_deref())
}

/// Pure predicate behind [`rustup_toolchain_pinned`]: set and non-blank.
fn rustup_toolchain_value_set(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| !value.to_string_lossy().trim().is_empty())
}

/// soldr#1766: refuse to build when no `rust-toolchain.toml` exists at or
/// above `workspace_root`.
///
/// The search walks ancestors deliberately. A cwd-only check would reject
/// every build launched from a subdirectory of a pinned repo.
pub fn require_toolchain_pin(workspace_root: &Path) -> Result<(), SoldrError> {
    if crate::core::find_rust_toolchain_manifest(workspace_root).is_some()
        || rustup_toolchain_pinned()
        || unpinned_allowed()
    {
        return Ok(());
    }
    Err(SoldrError::Other(format!(
        "no rust-toolchain.toml found in {} or any parent directory
         soldr requires a repo-pinned toolchain; the PATH rustc fallback is          disabled because it breaks the cache contract and can resolve to a          mismatched-host toolchain. Fix one of:
           - create rust-toolchain.toml:  printf '[toolchain]\nchannel = \"stable\"\n' > rust-toolchain.toml
           - or install the pin helper:   soldr toolchain
           - or select one explicitly:    RUSTUP_TOOLCHAIN=<channel>
           - or explicitly opt out:       SOLDR_ALLOW_UNPINNED=1 (or --allow-unpinned)",
        workspace_root.display()
    )))
}

/// Prepare the channel needed by the cargo front door using the long
/// toolchain-command timeout. Plugins remain exclusive to prepare/ensure.
pub(crate) fn ensure_cargo_toolchain(explicit_channel: Option<&str>) -> Result<(), SoldrError> {
    crate::musl_host::warn_when_missing_prerequisites();
    let workspace_root = std::env::current_dir()?;
    let manifest = crate::core::read_rust_toolchain_manifest_from_ancestors(&workspace_root)?;
    let channel = explicit_channel
        .map(str::trim)
        .filter(|channel| !channel.is_empty())
        .or_else(|| {
            manifest
                .channel
                .as_deref()
                .map(str::trim)
                .filter(|channel| !channel.is_empty())
        });
    let Some(channel) = channel else {
        // soldr#1766: no channel means no pin took effect. Refuse rather than
        // silently degrading to whatever rustc is first on PATH.
        require_toolchain_pin(&workspace_root)?;
        return Ok(());
    };
    if crate::system_toolchain_pin::satisfies_manifest(channel, &manifest) {
        return Ok(());
    }
    let paths = SoldrPaths::new()?;
    let memo_key = cargo_prepare_memo_key(channel, explicit_channel, &manifest);
    if memo_key
        .as_ref()
        .and_then(|key| memoized_toolchain_dir(&paths, key))
        .is_some()
    {
        return Ok(());
    }

    let mut list = std::process::Command::new(rustup_binary());
    list.args(["toolchain", "list"]);
    apply_implicit_toolchain_homes(&mut list);
    let installed_name = crate::core::command_output_with_timeout(&mut list, "toolchain list")
        .ok()
        .and_then(|output| installed_toolchain_name(&output.stdout, channel));
    if installed_name.is_none() {
        crate::system_toolchain_pin::with_mismatch_note(channel, || {
            match rustup_toolchain_install_with_profile(channel, manifest.profile.as_deref())? {
                0 => Ok(()),
                code => Err(SoldrError::Other(format!(
                    "toolchain install {channel} exited with code {code}"
                ))),
            }
        })?;
    }
    if let Some(components) = manifest.components.as_deref() {
        for component in components {
            let code = rustup_component_add(channel, component)?;
            if code != 0 {
                return Err(SoldrError::Other(format!(
                    "component add {component} exited with code {code}"
                )));
            }
        }
    }
    if let Some(targets) = manifest.targets.as_deref() {
        for target in targets {
            let code = rustup_target_add(channel, target)?;
            if code != 0 {
                return Err(SoldrError::Other(format!(
                    "target add {target} exited with code {code}"
                )));
            }
        }
    }
    if let Some(key) = memo_key {
        let toolchain_dirs = discover_toolchain_dirs(&key, None);
        if toolchain_dirs.len() == 1 {
            // soldr#3376: rustup's component list is a claim; check the files.
            crate::toolchain_std_libs::repair_missing_std_libs(
                channel,
                &toolchain_dirs[0],
                toolchain_host(&toolchain_dirs[0]).as_deref(),
                &key.targets,
                std::env::var_os(crate::core::RUSTUP_HOME_ENV_VAR)
                    .is_some_and(|home| !home.is_empty()),
                &mut |triple| crate::toolchain_std_libs::reinstall_rust_std(channel, triple),
            )?;
            write_cargo_prepare_memo(&paths, key, &toolchain_dirs[0]);
        }
    }
    Ok(())
}

pub(crate) use crate::toolchain_prepare::{run_prepare_inner, LinkerSummary, PrepareSummary};

/// Install one plugin declared under `[soldr.plugins]` via the
/// resolved cargo binary (so installs respect soldr-managed
/// `$CARGO_HOME`). This is bootstrap/dev-tool acquisition, not a
/// project compile, so it deliberately clears `RUSTC_WRAPPER` /
/// `RUSTC_WORKSPACE_WRAPPER` instead of routing through zccache. The
/// active cargo already honors `rust-toolchain.toml` at exec time, so
/// no explicit channel is passed.
pub(crate) fn cargo_install_plugin(
    name: &str,
    spec: &crate::core::PluginSpec,
) -> Result<i32, SoldrError> {
    crate::build_from_source_cmd::forbid_source_build_tripwire("toolchain plugin cargo install")?;
    let cargo = resolve_toolchain_binary("cargo")?;
    let mut command = std::process::Command::new(&cargo);
    command.arg("install").arg(name);

    let (version, locked, features, no_default_features) = match spec {
        crate::core::PluginSpec::Version(value) => (Some(value.as_str()), None, None, None),
        crate::core::PluginSpec::Detailed {
            version,
            locked,
            features,
            no_default_features,
        } => (
            version.as_deref(),
            *locked,
            features.as_deref(),
            *no_default_features,
        ),
    };

    if let Some(version) = version {
        let trimmed = version.trim();
        if !trimmed.is_empty() && trimmed != "*" {
            command.arg("--version").arg(trimmed);
        }
    }
    if locked == Some(true) {
        command.arg("--locked");
    }
    if no_default_features == Some(true) {
        command.arg("--no-default-features");
    }
    if let Some(features) = features {
        let joined = features.join(",");
        if !joined.is_empty() {
            command.arg("--features").arg(joined);
        }
    }

    crate::binaries::apply_resolved_toolchain_homes(&mut command, &cargo);
    crate::binaries::apply_managed_cargo_home_if_available(&mut command);
    command
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER");
    suppress_windows_console_window(&mut command);
    let status = run_toolchain_command(&mut command, &format!("cargo install {name}"))?;
    Ok(status.code().unwrap_or(1))
}

fn rustup_toolchain_install(channel: &str) -> Result<i32, SoldrError> {
    rustup_toolchain_install_with_profile(channel, None)
}

pub(crate) fn rustup_toolchain_install_with_profile(
    channel: &str,
    profile: Option<&str>,
) -> Result<i32, SoldrError> {
    crate::core::forbid_toolchain_install_tripwire(&format!("rustup toolchain install {channel}"))?;
    let mut command = std::process::Command::new(rustup_binary());
    command.args([
        "toolchain",
        "install",
        channel,
        "--profile",
        profile.unwrap_or("minimal"),
        "--no-self-update",
    ]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let status =
        run_toolchain_command(&mut command, &format!("rustup toolchain install {channel}"))?;
    Ok(status.code().unwrap_or(1))
}

pub(crate) fn rustup_component_add(channel: &str, component: &str) -> Result<i32, SoldrError> {
    crate::core::forbid_toolchain_install_tripwire(&format!(
        "rustup component add {component} --toolchain {channel}"
    ))?;
    let mut command = std::process::Command::new(rustup_binary());
    command.args(["component", "add", "--toolchain", channel, component]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let status = run_toolchain_command(
        &mut command,
        &format!("rustup component add {component} --toolchain {channel}"),
    )?;
    Ok(status.code().unwrap_or(1))
}

pub(crate) fn rustup_target_add(channel: &str, target: &str) -> Result<i32, SoldrError> {
    crate::core::forbid_toolchain_install_tripwire(&format!(
        "rustup target add {target} --toolchain {channel}"
    ))?;
    let mut command = std::process::Command::new(rustup_binary());
    command.args(["target", "add", "--toolchain", channel, target]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let status = run_toolchain_command(
        &mut command,
        &format!("rustup target add {target} --toolchain {channel}"),
    )?;
    Ok(status.code().unwrap_or(1))
}

pub(crate) fn run_toolchain_command(
    command: &mut std::process::Command,
    context: &str,
) -> Result<std::process::ExitStatus, SoldrError> {
    crate::exit_guard::run_child_command(
        command,
        context,
        "toolchain-prepare",
        InstallerWatchdogConfig::from_env(TOOLCHAIN_COMMAND_TIMEOUT_ENV_VAR),
    )
}

#[cfg(test)]
#[path = "toolchain_tests.rs"]
mod tests;
