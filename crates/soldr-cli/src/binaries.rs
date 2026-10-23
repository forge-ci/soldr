//! Toolchain / rustup / zccache binary resolution helpers. Extracted from
//! `main.rs` as part of issue #339.

use crate::core::{
    command_output_with_timeout, run_installer_command_output, suppress_windows_console_window,
    InstallerWatchdogConfig, SoldrError, SoldrPaths, TargetTriple,
};
use crate::fetch::VersionSpec;
use crate::platform::sync::Mutex;
use crate::{
    REAL_TOOLCHAIN_BINARY_ENV_PREFIX, TEST_CARGO_BIN_ENV_VAR, TEST_RUSTC_BIN_ENV_VAR,
    TEST_RUSTUP_BIN_ENV_VAR,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Escape hatch: `SOLDR_TOOLCHAIN_BIN_CACHE=off` disables both the
/// in-process memo and the on-disk cache for channel-scoped
/// `rustup which` resolution (see [`resolve_toolchain_binary_for_channel`]).
pub(crate) const TOOLCHAIN_BIN_CACHE_ENV_VAR: &str = "SOLDR_TOOLCHAIN_BIN_CACHE";

pub(crate) fn resolve_toolchain_binary(tool: &str) -> Result<std::path::PathBuf, SoldrError> {
    if let Some(path) = toolchain_binary_override(tool) {
        return Ok(path);
    }

    let start_dir = std::env::current_dir().ok();
    if let Some(path) = probe_direct_toolchain_binary(tool, start_dir.as_deref()) {
        return Ok(path);
    }
    if let Some(path) =
        crate::system_toolchain_pin::pinned_system_binary(tool, start_dir.as_deref())
    {
        return Ok(path);
    }

    resolve_toolchain_binary_with_optional_channel(tool, None, start_dir.as_deref())
}

/// Resolve `tool` for an explicit, immutable `channel` (e.g.
/// `nightly-2026-01-18`), memoizing the result in-process and on disk
/// so nested cargo-dylint re-entries don't each pay a fresh `rustup
/// which` subprocess spawn. The ambient-default lookup (no channel) is
/// intentionally NOT disk-cached: it is not keyed by anything stable —
/// the default toolchain can change without any cache key changing.
pub(crate) fn resolve_toolchain_binary_for_channel(
    tool: &str,
    channel: Option<&str>,
) -> Result<std::path::PathBuf, SoldrError> {
    let Some(channel) = channel.map(str::trim).filter(|channel| !channel.is_empty()) else {
        return resolve_toolchain_binary(tool);
    };

    if let Some(path) = toolchain_binary_override(tool) {
        return Ok(path);
    }
    if let Some(dir) = crate::system_toolchain_pin::matching_bin_dir(channel) {
        if let Some(path) = executable_in_dir(dir, tool) {
            return Ok(path);
        }
    }

    let cache_scope = (!toolchain_bin_cache_disabled())
        .then(ToolchainBinCacheScope::current)
        .flatten();

    if let Some(scope) = cache_scope.as_ref() {
        if let Some(path) = toolchain_bin_memo_lookup(scope, channel, tool) {
            return Ok(path);
        }
        if let Some(path) = disk_cache_lookup(scope, channel, tool) {
            toolchain_bin_memo_store(scope, channel, tool, path.clone());
            return Ok(path);
        }
    }

    let resolved = resolve_toolchain_binary_with_optional_channel(tool, Some(channel), None)?;

    if let Some(scope) = cache_scope.as_ref().filter(|_| resolved.is_file()) {
        toolchain_bin_memo_store(scope, channel, tool, resolved.clone());
        disk_cache_store(scope, channel, tool, &resolved);
    }

    Ok(resolved)
}

fn toolchain_bin_cache_disabled() -> bool {
    std::env::var_os(TOOLCHAIN_BIN_CACHE_ENV_VAR)
        .map(|value| value.to_string_lossy().trim().eq_ignore_ascii_case("off"))
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ToolchainBinCacheScope {
    rustup_home: PathBuf,
    host_triple: String,
}

impl ToolchainBinCacheScope {
    /// Keyed on the home the memoized `rustup which` actually runs under
    /// (soldr#3567). Keying on the caller's `RUSTUP_HOME`/`~/.rustup` while
    /// the lookup ran under Soldr's managed home let a path resolved before
    /// that home existed answer for it forever, so a Dylint nightly
    /// provisioned in the managed home was compiled from `~/.rustup`'s copy.
    fn current() -> Option<Self> {
        Self::from_home(
            crate::toolchain::effective_rustup_home()?,
            TargetTriple::host().ok()?.triple().to_string(),
            &std::env::current_dir().ok()?,
        )
    }

    fn from_home(rustup_home: PathBuf, host_triple: String, cwd: &Path) -> Option<Self> {
        let absolute = if rustup_home.is_absolute() {
            rustup_home
        } else {
            cwd.join(rustup_home)
        };
        let rustup_home = std::fs::canonicalize(&absolute).unwrap_or(absolute);
        rustup_home.is_absolute().then_some(Self {
            rustup_home,
            host_triple,
        })
    }

    fn stable_key(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.rustup_home.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(self.host_triple.as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

type ToolchainBinMemoKey = (ToolchainBinCacheScope, String, String);
type ToolchainBinMemo = Mutex<HashMap<ToolchainBinMemoKey, PathBuf>>;

fn toolchain_bin_memo() -> &'static ToolchainBinMemo {
    static MEMO: OnceLock<ToolchainBinMemo> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

fn toolchain_bin_memo_lookup(
    scope: &ToolchainBinCacheScope,
    channel: &str,
    tool: &str,
) -> Option<PathBuf> {
    let key = (scope.clone(), channel.to_string(), tool.to_string());
    let mut memo = toolchain_bin_memo().lock();
    let candidate = memo.get(&key).cloned()?;
    if candidate.is_file() {
        Some(candidate)
    } else {
        memo.remove(&key);
        None
    }
}

fn toolchain_bin_memo_store(
    scope: &ToolchainBinCacheScope,
    channel: &str,
    tool: &str,
    path: PathBuf,
) {
    {
        let mut memo = toolchain_bin_memo().lock();
        memo.insert((scope.clone(), channel.to_string(), tool.to_string()), path);
    }
}

/// Sanitize a toolchain channel string for use as a single path
/// component. Rustup channel names are already path-safe in practice
/// (`nightly-2026-01-18`, `1.94.1-x86_64-pc-windows-msvc`) but this
/// defends against any unexpected separator/traversal characters.
fn sanitize_toolchain_for_path(channel: &str) -> String {
    // Collapse `..` before the per-character pass. The result is used as a
    // directory component in `toolchain_bin_disk_cache_path_in`, so a channel
    // of `..` would otherwise walk out of the cache root. Dots have to stay
    // legal individually or version channels like `1.94.1` lose their
    // readable directory name, which is why this can't just deny '.'.
    channel
        .replace("..", "_")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// `<cache_root>/toolchain-bins/v2/<scope>/<sanitized-toolchain>/<tool>.path`
/// Split out with an explicit `cache_root` parameter so unit tests can
/// point it at a tempdir instead of the real `~/.soldr/cache`.
fn toolchain_bin_disk_cache_path_in(
    cache_root: &Path,
    scope: &ToolchainBinCacheScope,
    channel: &str,
    tool: &str,
) -> PathBuf {
    cache_root
        .join("toolchain-bins")
        .join("v2")
        .join(scope.stable_key())
        .join(sanitize_toolchain_for_path(channel))
        .join(format!("{tool}.path"))
}

fn disk_cache_lookup_in(
    cache_root: &Path,
    scope: &ToolchainBinCacheScope,
    channel: &str,
    tool: &str,
) -> Option<PathBuf> {
    let cache_file = toolchain_bin_disk_cache_path_in(cache_root, scope, channel, tool);
    let contents = std::fs::read_to_string(&cache_file).ok()?;
    let candidate = std::path::PathBuf::from(contents.trim());
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

fn disk_cache_store_in(
    cache_root: &Path,
    scope: &ToolchainBinCacheScope,
    channel: &str,
    tool: &str,
    resolved: &Path,
) {
    let cache_file = toolchain_bin_disk_cache_path_in(cache_root, scope, channel, tool);
    let Some(parent) = cache_file.parent() else {
        return;
    };
    // Best-effort: a write failure here must never fail resolution —
    // the caller already has a good `resolved` path in hand.
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let _ = std::fs::write(&cache_file, format!("{}\n", resolved.display()));
}

fn disk_cache_lookup(scope: &ToolchainBinCacheScope, channel: &str, tool: &str) -> Option<PathBuf> {
    let paths = SoldrPaths::new().ok()?;
    disk_cache_lookup_in(&paths.cache, scope, channel, tool)
}

fn disk_cache_store(scope: &ToolchainBinCacheScope, channel: &str, tool: &str, resolved: &Path) {
    let Ok(paths) = SoldrPaths::new() else {
        return;
    };
    disk_cache_store_in(&paths.cache, scope, channel, tool, resolved);
}

fn resolve_toolchain_binary_with_optional_channel(
    tool: &str,
    channel: Option<&str>,
    fallback_start_dir: Option<&std::path::Path>,
) -> Result<std::path::PathBuf, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.arg("which");
    if let Some(channel) = channel {
        command.args(["--toolchain", channel]);
    }
    command.arg(tool);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let context = match channel {
        Some(channel) => format!("rustup which --toolchain {channel} {tool}"),
        None => format!("rustup which {tool}"),
    };
    // A channel-scoped manager lookup can wait behind an in-flight install
    // lock. It normally returns in milliseconds, but its worst case is the
    // install duration; use the installer watchdog while retaining captured
    // stdout as the resolved path. Unscoped lookups remain short probes.
    let output = match channel {
        Some(_) => run_installer_command_output(
            &mut command,
            &context,
            "manager-which",
            InstallerWatchdogConfig::from_env(crate::toolchain::TOOLCHAIN_COMMAND_TIMEOUT_ENV_VAR),
        ),
        None => command_output_with_timeout(&mut command, &context),
    };

    match output {
        Ok(output) if output.status.success() => {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(path.into());
            }
        }
        Ok(output) => {
            if channel.is_none() {
                if let Some(path) = crate::core::probe_toolchain_binary(tool, fallback_start_dir) {
                    return Ok(path);
                }
            }
            return Err(rustup_resolution_failure(tool, &output.stderr));
        }
        Err(err) => {
            if channel.is_none() {
                if let Some(path) = crate::core::probe_toolchain_binary(tool, fallback_start_dir) {
                    return Ok(path);
                }
            }
            return Err(SoldrError::Other(format!(
                "failed to invoke rustup while resolving {tool}: {err}"
            )));
        }
    }

    if channel.is_none() {
        if let Some(path) = crate::core::probe_toolchain_binary(tool, fallback_start_dir) {
            return Ok(path);
        }
    }

    Err(SoldrError::Other(format!(
        "rustup did not return a path for {tool}"
    )))
}

fn probe_direct_toolchain_binary(
    tool: &str,
    start_dir: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    if std::env::var_os("RUSTUP_TOOLCHAIN").is_some_and(|value| !value.is_empty()) {
        return None;
    }

    explicit_rustup_toolchain_binary(tool)
        .or_else(|| repo_local_rustup_toolchain_binary(tool, start_dir))
        .or_else(|| explicit_cargo_home_binary(tool))
        .or_else(|| repo_local_cargo_home_binary(tool, start_dir))
}

fn explicit_cargo_home_binary(tool: &str) -> Option<std::path::PathBuf> {
    non_empty_env_path("CARGO_HOME").and_then(|path| executable_in_dir(&path.join("bin"), tool))
}

fn repo_local_cargo_home_binary(
    tool: &str,
    start_dir: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    find_ancestor_dir(start_dir, ".cargo")
        .and_then(|path| executable_in_dir(&path.join("bin"), tool))
}

fn explicit_rustup_toolchain_binary(tool: &str) -> Option<std::path::PathBuf> {
    non_empty_env_path("RUSTUP_HOME")
        .and_then(|path| rustup_home_single_toolchain_binary(&path, tool))
}

fn repo_local_rustup_toolchain_binary(
    tool: &str,
    start_dir: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    find_ancestor_dir(start_dir, ".rustup")
        .and_then(|path| rustup_home_single_toolchain_binary(&path, tool))
}

fn rustup_home_single_toolchain_binary(
    rustup_home: &std::path::Path,
    tool: &str,
) -> Option<std::path::PathBuf> {
    let mut candidates = std::fs::read_dir(rustup_home.join("toolchains"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("bin"))
        .filter_map(|dir| executable_in_dir(&dir, tool))
        .collect::<Vec<_>>();
    if candidates.len() == 1 {
        candidates.pop()
    } else {
        None
    }
}

fn find_ancestor_dir(
    start_dir: Option<&std::path::Path>,
    relative: &str,
) -> Option<std::path::PathBuf> {
    let user_home = crate::core::user_home_dir().ok();
    find_ancestor_dir_bounded(start_dir?, relative, user_home.as_deref())
}

fn find_ancestor_dir_bounded(
    start_dir: &std::path::Path,
    relative: &str,
    user_home: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    // Normalize both sides before comparing. This is load-bearing on Windows,
    // where temp paths may use an 8.3 alias (RUNNER~1) while USERPROFILE uses
    // the long spelling; lexical equality would walk straight through the
    // intended boundary into the runner's global toolchain homes.
    let mut current = std::fs::canonicalize(start_dir).unwrap_or_else(|_| start_dir.to_path_buf());
    let user_home =
        user_home.map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
    loop {
        // A project may live below the user's home, but the home directory's
        // own `.cargo` / `.rustup` is global state, not a repository-local
        // toolchain. Stop before inspecting it so an explicit rustup resolver
        // (including the test seam) can choose the intended tool instead.
        if user_home.as_deref() == Some(current.as_path()) {
            return None;
        }
        let candidate = current.join(relative);
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

pub(crate) fn executable_in_dir(dir: &std::path::Path, tool: &str) -> Option<std::path::PathBuf> {
    let candidate = dir.join(tool);
    if candidate.is_file() {
        return Some(candidate);
    }
    // Windows: also try the PATHEXT suffixes (no-op list on other hosts).
    for suffix in crate::platform::executable::search::candidate_extensions() {
        let candidate = dir.join(format!("{tool}{suffix}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub(crate) fn apply_implicit_toolchain_homes(command: &mut std::process::Command) {
    let start_dir = std::env::current_dir().ok();
    crate::core::apply_implicit_toolchain_homes(command, start_dir.as_deref());
    apply_managed_toolchain_homes_if_available(command, start_dir.as_deref());
}

/// Apply homes that match a toolchain binary Soldr already resolved.
///
/// Rustup discovery and bootstrap intentionally use Soldr's managed homes,
/// but a concrete host binary must keep the caller's host Rustup context.
/// Mixing a host Cargo/rustfmt proxy with Soldr's default-less managed
/// `RUSTUP_HOME` makes Rustup report that no default toolchain is configured.
/// Which toolchain homes an execution runs under (soldr#1799).
///
/// The invariant this names: **one canonical pair of homes per execution,
/// chosen by where the resolved binary lives — never by ambient env leakage.**
/// Mixing a host cargo/rustfmt proxy with soldr's default-less managed
/// `RUSTUP_HOME` makes rustup report no default toolchain (#1768), and the
/// quieter failure is worse — flipping homes between runs changes which rustc
/// is used, invalidating cargo fingerprints and zccache keys, so warm builds
/// silently recompile the world.
///
/// Exposed as a discriminant rather than left implicit in a branch because it
/// is the value telemetry records and CI asserts on: a host-resolved tool must
/// never report [`HomeOrigin::Managed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HomeOrigin {
    /// The caller's own homes are used unchanged.
    Caller,
    /// soldr's managed homes, because the binary physically lives in them.
    Managed,
    /// A repo-local `.rustup` / `.cargo`, found by walking ancestors of the
    /// working directory (soldr#1799).
    ///
    /// Runs under the caller's homes exactly like [`HomeOrigin::Caller`] —
    /// only the *reporting* differs. It is a distinct value because the CI
    /// guard #1799 describes asserts that host-resolved tools report
    /// `caller`; folding repo-local into that would let the guard pass while
    /// the log had lost the distinction, and repo-local `.cargo`/`.rustup` is
    /// a first-class resolution path (`probe_direct_toolchain_binary`), so
    /// the guard would be weakest exactly where such repos build.
    RepoLocal,
    /// A Dylint-scoped child: `RUSTUP_HOME` is pinned to the home the Dylint
    /// nightly and its targets were provisioned in, while the resolved binary
    /// lives outside it (soldr#3567). Never reported as `managed`, because
    /// soldr#1799's guard rightly rejects that for a host binary.
    Dylint,
}

impl HomeOrigin {
    /// The stable string used in logs and CI assertions.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            HomeOrigin::Caller => "caller",
            HomeOrigin::Managed => "managed",
            HomeOrigin::RepoLocal => "repo-local",
            HomeOrigin::Dylint => "dylint",
        }
    }
}

/// Classify which homes `binary` must execute under.
///
/// Managed **only** when the binary physically lives inside a managed home.
/// Anything else — a host rustup proxy, a repo-local `.cargo` tool, a PATH
/// binary — keeps the caller's context.
pub(crate) fn home_origin_for_binary(binary: &std::path::Path, paths: &SoldrPaths) -> HomeOrigin {
    home_origin_for_binary_from(binary, paths, std::env::current_dir().ok().as_deref())
}

/// [`home_origin_for_binary`] against an explicit ancestor-search root.
///
/// Split out so the repo-local classification is testable without depending
/// on the process working directory, which is shared mutable state across a
/// test binary (soldr#1927).
fn home_origin_for_binary_from(
    binary: &std::path::Path,
    paths: &SoldrPaths,
    start_dir: Option<&std::path::Path>,
) -> HomeOrigin {
    let managed_cargo_home = crate::fetch::managed_cargo_home(paths);
    let managed_rustup_home = crate::fetch::managed_rustup_home(paths);
    // Managed keeps precedence: a managed home nested under some ancestor
    // `.cargo` is still managed, and that is the classification the
    // homes-application branch depends on.
    if path_is_within(binary, &managed_cargo_home) || path_is_within(binary, &managed_rustup_home) {
        return HomeOrigin::Managed;
    }
    for relative in [".rustup", ".cargo"] {
        let Some(repo_local) = find_ancestor_dir(start_dir, relative) else {
            continue;
        };
        if path_is_within(binary, &repo_local) {
            return HomeOrigin::RepoLocal;
        }
    }
    HomeOrigin::Caller
}

/// [`home_origin_for_binary`] for callers that have no `SoldrPaths` in hand.
///
/// `None` when the soldr root cannot be resolved -- telemetry then records
/// the origin as absent rather than guessing. A wrong `home_origin` is worse
/// than a missing one, because soldr#1799's CI check keys on it and a
/// fabricated `caller` would mask exactly the leak it exists to catch.
pub(crate) fn home_origin_for_binary_opt(binary: &std::path::Path) -> Option<HomeOrigin> {
    let paths = SoldrPaths::new().ok()?;
    Some(home_origin_for_binary(binary, &paths))
}

pub(crate) fn apply_resolved_toolchain_homes(
    command: &mut std::process::Command,
    binary: &std::path::Path,
) {
    let start_dir = std::env::current_dir().ok();
    crate::core::apply_implicit_toolchain_homes(command, start_dir.as_deref());

    let Ok(paths) = SoldrPaths::new() else {
        return;
    };
    if home_origin_for_binary(binary, &paths) == HomeOrigin::Managed {
        apply_managed_toolchain_homes_if_available(command, start_dir.as_deref());
        apply_managed_toolchain_library_path_if_available(command, binary, &paths);
    } else {
        apply_rustup_toolchain_library_path_on_macos(command, binary);
    }
}

/// Restore, for a binary Soldr resolved inside a caller-owned rustup
/// toolchain, the loader path that rustup's own proxy would have given it
/// (soldr#3359).
///
/// Soldr resolves `cargo`/`rustc` to the toolchain's real executables instead
/// of running them through rustup's proxies, and the proxies are what prepend
/// `<toolchain>/lib` to `DYLD_FALLBACK_LIBRARY_PATH` (rustup
/// `Toolchain::set_ldpath`). Since Rust 1.98 the Apple toolchains link their
/// companion tools (`rust-objcopy`, `rust-lld`) against `@rpath/libLLVM.dylib`
/// with an rpath of `lib/rustlib/<host>/lib`, while the `rustc` component ships
/// the library only as `<toolchain>/lib/libLLVM.dylib` (rust-lang/rust#157205).
/// Without the proxy's entry, rustc's requested `rust-objcopy` strip aborts in
/// dyld and Soldr (rightly) refuses to publish the unstripped artifact.
///
/// The directory added is the caller's own toolchain library directory, never
/// a Soldr-managed one, so this does not leak Soldr's loader path into a
/// caller-owned toolchain. Other hosts keep their current environment: their
/// dist tools resolve LLVM through their own rpath.
fn apply_rustup_toolchain_library_path_on_macos(
    command: &mut std::process::Command,
    binary: &std::path::Path,
) {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::MacOs {
        return;
    }
    if let Some(library_dir) = rustup_toolchain_library_dir(binary) {
        prepend_loader_library_path(command, "DYLD_FALLBACK_LIBRARY_PATH", library_dir);
    }
}

/// `<rustup-home>/toolchains/<name>/lib` for a binary at
/// `<rustup-home>/toolchains/<name>/bin/<tool>`, when that directory exists.
fn rustup_toolchain_library_dir(binary: &Path) -> Option<PathBuf> {
    let bin = binary.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    let toolchain = bin.parent()?;
    if toolchain.parent()?.file_name()? != "toolchains" {
        return None;
    }
    let library_dir = toolchain.join("lib");
    library_dir.is_dir().then_some(library_dir)
}

fn prepend_loader_library_path(
    command: &mut std::process::Command,
    variable: &str,
    library_dir: PathBuf,
) {
    let existing = command
        .get_envs()
        .find(|(key, _)| *key == variable)
        .map(|(_, value)| value.map(std::ffi::OsStr::to_os_string))
        .unwrap_or_else(|| std::env::var_os(variable));
    let mut entries = vec![library_dir];
    if let Some(existing) = existing.filter(|value| !value.is_empty()) {
        entries.extend(std::env::split_paths(&existing));
    }
    entries.dedup();
    if let Ok(value) = std::env::join_paths(entries) {
        command.env(variable, value);
    }
}

fn path_is_within(path: &std::path::Path, root: &std::path::Path) -> bool {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    path.starts_with(root)
}

fn apply_managed_toolchain_homes_if_available(
    command: &mut std::process::Command,
    start_dir: Option<&std::path::Path>,
) {
    let Ok(paths) = SoldrPaths::new() else {
        return;
    };
    apply_managed_cargo_home_if_available_for_paths(command, start_dir, &paths);
    apply_managed_rustup_home_if_available_for_paths(command, start_dir, &paths);
}

/// Give Rust companion tools (such as `rust-objcopy`) access to the shared
/// libraries shipped with their managed toolchain.  This is deliberately
/// separate from the homes helper: caller-owned and repo-local binaries must
/// never inherit a Soldr-managed loader path.
fn apply_managed_toolchain_library_path_if_available(
    command: &mut std::process::Command,
    binary: &std::path::Path,
    paths: &SoldrPaths,
) {
    let loader_library_path = match crate::platform::host::facts::os() {
        crate::platform::host::facts::HostOs::Linux => "LD_LIBRARY_PATH",
        crate::platform::host::facts::HostOs::MacOs => "DYLD_FALLBACK_LIBRARY_PATH",
        crate::platform::host::facts::HostOs::Windows => return,
    };
    let Some(library_dir) = managed_toolchain_library_dir(binary, paths) else {
        return;
    };
    let existing = command
        .get_envs()
        .find(|(key, _)| *key == loader_library_path)
        .map(|(_, value)| value.map(std::ffi::OsStr::to_os_string))
        .unwrap_or_else(|| std::env::var_os(loader_library_path));
    let mut entries = vec![library_dir];
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Linux {
        if let Some(nix_library_path) =
            std::env::var_os("NIX_LD_LIBRARY_PATH").filter(|value| !value.is_empty())
        {
            entries.extend(
                std::env::split_paths(&nix_library_path)
                    .filter(|path| !path.as_os_str().is_empty()),
            );
        }
    }
    if let Some(existing) = existing {
        entries.extend(std::env::split_paths(&existing));
    }
    entries.dedup();
    if let Ok(value) = std::env::join_paths(entries) {
        command.env(loader_library_path, value);
    }
}

/// Locate `<managed-rustup>/toolchains/<channel>/lib` for a binary inside
/// that toolchain.  A managed cargo-home proxy has no Rust shared-library
/// directory of its own, so it intentionally returns `None`.
fn managed_toolchain_library_dir(binary: &Path, paths: &SoldrPaths) -> Option<PathBuf> {
    let toolchains = crate::fetch::managed_rustup_home(paths).join("toolchains");
    let binary = std::fs::canonicalize(binary).unwrap_or_else(|_| binary.to_path_buf());
    let toolchains = std::fs::canonicalize(&toolchains).unwrap_or(toolchains);
    let relative = binary.strip_prefix(&toolchains).ok()?;
    let std::path::Component::Normal(channel) = relative.components().next()? else {
        return None;
    };
    let library_dir = toolchains.join(channel).join("lib");
    library_dir.is_dir().then_some(library_dir)
}

/// Apply only Soldr's managed Cargo home when it is implicit and available.
///
/// Tool-acquisition paths use this after [`apply_resolved_toolchain_homes`]:
/// plugins still install into Soldr's managed Cargo root, while a host-owned
/// Cargo binary keeps its host Rustup context.
pub(crate) fn apply_managed_cargo_home_if_available(command: &mut std::process::Command) {
    let start_dir = std::env::current_dir().ok();
    let Ok(paths) = SoldrPaths::new() else {
        return;
    };
    apply_managed_cargo_home_if_available_for_paths(command, start_dir.as_deref(), &paths);
}

fn apply_managed_cargo_home_if_available_for_paths(
    command: &mut std::process::Command,
    start_dir: Option<&std::path::Path>,
    paths: &SoldrPaths,
) {
    if std::env::var_os(crate::core::CARGO_HOME_ENV_VAR).is_none()
        && find_ancestor_dir(start_dir, ".cargo").is_none()
    {
        let managed = crate::fetch::managed_cargo_home(paths);
        if managed.is_dir() {
            command.env(crate::core::CARGO_HOME_ENV_VAR, managed);
        }
    }
}

fn apply_managed_rustup_home_if_available_for_paths(
    command: &mut std::process::Command,
    start_dir: Option<&std::path::Path>,
    paths: &SoldrPaths,
) {
    if std::env::var_os(crate::core::RUSTUP_HOME_ENV_VAR).is_none()
        && find_ancestor_dir(start_dir, ".rustup").is_none()
    {
        let managed = crate::fetch::managed_rustup_home(paths);
        if managed.is_dir() {
            command.env(crate::core::RUSTUP_HOME_ENV_VAR, managed);
        }
    }
}

pub(crate) fn rustup_resolution_failure(tool: &str, stderr: &[u8]) -> SoldrError {
    let raw_failure = String::from_utf8_lossy(stderr).trim().to_string();
    SoldrError::Other(format!(
        "failed to resolve {tool} via rustup: {raw_failure}\n\
CI hint: if this repository pins Rust in rust-toolchain.toml, preinstall that exact channel instead of a generic stable toolchain.\n\
CI hint: export RUSTUP_TOOLCHAIN to that exact channel for later cargo, rustc, and soldr cargo steps, or use the documented setup-soldr action path (uses: zackees/soldr@<ref> or uses: ./).\n\
Bootstrap hint: if rustup itself is missing on this host, run `soldr bootstrap` to install it into soldr's managed bin dir (or unset SOLDR_NO_BOOTSTRAP and re-run if you previously opted out)."
    ))
}

pub(crate) fn parse_tool_spec(spec: &str) -> (String, VersionSpec) {
    if let Some((name, version)) = spec.split_once('@') {
        (name.to_string(), VersionSpec::parse(version))
    } else {
        (spec.to_string(), VersionSpec::Latest)
    }
}

fn toolchain_binary_override(tool: &str) -> Option<std::path::PathBuf> {
    let env_var = match tool {
        "cargo" => TEST_CARGO_BIN_ENV_VAR,
        "rustc" => TEST_RUSTC_BIN_ENV_VAR,
        _ => return real_toolchain_binary_override(tool),
    };
    non_empty_env_path(env_var).or_else(|| real_toolchain_binary_override(tool))
}

fn real_toolchain_binary_override(tool: &str) -> Option<std::path::PathBuf> {
    non_empty_env_path(&real_toolchain_binary_env_var(tool))
}

fn real_toolchain_binary_env_var(tool: &str) -> String {
    let mut value = String::from(REAL_TOOLCHAIN_BINARY_ENV_PREFIX);
    for ch in tool.chars() {
        if ch.is_ascii_alphanumeric() {
            value.push(ch.to_ascii_uppercase());
        } else {
            value.push('_');
        }
    }
    value
}

/// Resolve the rustup binary to spawn for any subprocess call. Always
/// honours `SOLDR_TEST_RUSTUP_BIN` for tests. Then prefers `rustup` already on
/// `PATH` or the soldr-managed copy under `<SoldrPaths::bin>/rustup`. If
/// neither exists and the user has not opted out via `SOLDR_NO_BOOTSTRAP=1`,
/// triggers a one-shot bootstrap (`rustup-init`) that installs rustup into
/// the soldr-managed bin dir before returning that path.
pub(crate) fn rustup_binary() -> std::path::PathBuf {
    if let Some(path) = non_empty_env_path(TEST_RUSTUP_BIN_ENV_VAR) {
        return path;
    }
    if let Ok(paths) = SoldrPaths::new() {
        if let Some(existing) = crate::fetch::discover_rustup(&paths) {
            return existing;
        }
        match crate::fetch::auto_bootstrap_if_missing_blocking(&paths) {
            Ok(crate::fetch::AutoBootstrapOutcome::AlreadyInstalled(p)) => return p,
            Ok(crate::fetch::AutoBootstrapOutcome::Installed(report)) => return report.rustup_path,
            Ok(crate::fetch::AutoBootstrapOutcome::OptedOut) => {
                // Fall through to the bare "rustup" path; the subsequent
                // resolution failure surfaces the standard diagnostic plus
                // the `SOLDR_NO_BOOTSTRAP` hint.
            }
            Err(err) => {
                eprintln!(
                    "soldr: auto-bootstrap failed ({err}). Falling back to `rustup` on PATH. \
                     Run `soldr bootstrap` manually or unset {} to retry.",
                    crate::fetch::NO_BOOTSTRAP_ENV_VAR
                );
            }
        }
    }
    "rustup".into()
}

pub(crate) fn non_empty_env_path(env_var: &str) -> Option<std::path::PathBuf> {
    let value = std::env::var_os(env_var)?;
    if value.is_empty() {
        return None;
    }
    Some(value.into())
}

pub(crate) fn current_soldr_binary() -> Result<std::path::PathBuf, SoldrError> {
    std::env::current_exe().map_err(SoldrError::from)
}

/// Materialize a compiler-named multicall shim for use as Cargo's managed
/// wrapper. The current executable may itself be a Cargo multicall hardlink,
/// so it is not a safe wrapper path: Cargo would invoke that alias with the
/// compiler path and re-enter the front door. A versioned compiler identity
/// makes the wrapper contract explicit and keeps soldr versions isolated.
pub(crate) fn rustc_wrapper_shim_binary(
    paths: &SoldrPaths,
) -> Result<std::path::PathBuf, SoldrError> {
    let target = paths
        .versioned_shims_dir()
        .join(format!("rustc{}", std::env::consts::EXE_SUFFIX));
    let source = crate::shim_materialize::soldr_binary_source()?;
    crate::shim_materialize::materialize_executable(&source, &target)?;
    Ok(target)
}

/// Materialize the dedicated Dylint wrapper identity used when cargo-dylint
/// nests its workspace driver inside `RUSTC_WRAPPER`.
pub(crate) fn dylint_wrapper_shim_binary(
    paths: &SoldrPaths,
) -> Result<std::path::PathBuf, SoldrError> {
    let target = paths
        .versioned_shims_dir()
        .join(format!("soldr-dylint{}", std::env::consts::EXE_SUFFIX));
    let source = crate::shim_materialize::soldr_binary_source()?;
    crate::shim_materialize::materialize_executable(&source, &target)?;
    Ok(target)
}

/// Materialize the daemon's stable process/service identity next to soldr.
pub(crate) fn soldr_daemon_binary() -> Result<std::path::PathBuf, SoldrError> {
    materialize_runtime_alias("soldr-daemon")
}

/// Ensure compiler-side daemon recovery has a canonically named executable.
///
/// The managed Cargo front door normally injects this handoff once for every
/// compiler child. Direct `RUSTC_WRAPPER` / `zccache-soldr` invocations do not
/// have that parent, so recover it lazily after the first failed daemon probe.
/// Reuse an existing sibling without hashing; only first use materializes the
/// multicall alias.
pub(crate) fn ensure_daemon_executable_handoff() -> Result<std::path::PathBuf, SoldrError> {
    let env_var = crate::daemon::lifecycle::SOLDR_DAEMON_EXE_ENV_VAR;
    if let Some(configured) = non_empty_env_path(env_var).filter(|path| {
        path.is_file()
            && path
                .file_stem()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|stem| stem.eq_ignore_ascii_case("soldr-daemon"))
    }) {
        return Ok(configured);
    }

    let current = std::env::current_exe().map_err(SoldrError::from)?;
    let sibling = current
        .parent()
        .map(|parent| crate::platform::executable::name::sibling(parent, "soldr-daemon"));
    let daemon = sibling
        .filter(|path| path.is_file())
        .map(Ok)
        .unwrap_or_else(soldr_daemon_binary)?;
    std::env::set_var(env_var, &daemon);
    Ok(daemon)
}

fn materialize_runtime_alias(stem: &str) -> Result<std::path::PathBuf, SoldrError> {
    let source = crate::shim_materialize::soldr_binary_source()?;
    let current = std::env::current_exe().map_err(SoldrError::from)?;
    let file = crate::platform::executable::name::native(stem);
    let (current_target, source_target) = runtime_alias_targets(&source, &current, &file)?;
    crate::shim_materialize::materialize_executable(&source, &current_target)?;

    // Self-relocated wrapper invocations run from a GC-managed runtime dir,
    // while `soldr_binary_source()` intentionally points at the package-owned
    // original. Keep both layouts complete: lifecycle resolves beside
    // `current_exe`, and package commands resolve beside the original.
    if let Some(source_target) = source_target {
        crate::shim_materialize::materialize_executable(&source, &source_target)?;
    }
    Ok(current_target)
}

fn runtime_alias_targets(
    source: &std::path::Path,
    current: &std::path::Path,
    file: &str,
) -> Result<(std::path::PathBuf, Option<std::path::PathBuf>), SoldrError> {
    let current_parent = current.parent().ok_or_else(|| {
        SoldrError::Other(format!(
            "running soldr has no parent: {}",
            current.display()
        ))
    })?;
    let current_target = current_parent.join(file);
    let source_target = source
        .parent()
        .map(|parent| parent.join(file))
        .filter(|target| *target != current_target);
    Ok((current_target, source_target))
}

/// Materialize the `zccache-soldr` RUSTC_WRAPPER/CC shim name
/// (soldr#1081) from the main `soldr` binary. Native-C caching
/// injects this stable basename into `CC`/`CXX` so cc-rs build-script
/// compiles route through the soldr-daemon embedded zccache service
/// over the broker SESSION route.
///
/// The shim must be version-scoped just like the Rust compiler shim. A test
/// process running an older bootstrapped Soldr can overlap the source-built
/// CI process; an unversioned `~/.soldr/bin/zccache-soldr` let those versions
/// replace each other's native-compiler protocol endpoint mid-build.
pub(crate) fn zccache_soldr_shim_binary() -> Result<std::path::PathBuf, SoldrError> {
    let paths = SoldrPaths::new()?;
    paths.ensure_dirs()?;
    zccache_soldr_shim_binary_at(&paths)
}

fn zccache_soldr_shim_binary_at(paths: &SoldrPaths) -> Result<std::path::PathBuf, SoldrError> {
    let file = crate::platform::executable::name::native("zccache-soldr");
    let source = crate::shim_materialize::soldr_binary_source()?;
    let target = zccache_soldr_shim_target(paths, &source)?.join(file);
    crate::shim_materialize::materialize_executable(&source, &target)?;
    Ok(target)
}

fn zccache_soldr_shim_target(
    paths: &SoldrPaths,
    source: &std::path::Path,
) -> Result<std::path::PathBuf, SoldrError> {
    let cache = crate::daemon::image_hash::machine_scoped_cache_dir(
        &paths.cache.join("native-shim-image-hash"),
    );
    let digest = crate::daemon::image_hash::cached_blake3_hex(&cache, source)?;
    Ok(paths.versioned_shims_dir().join("images").join(digest))
}

/// Resolve `<stem>` as a sibling of the running executable (adding
/// `.exe` on Windows), falling back to the bare stem when the sibling
/// is absent so a PATH lookup can still find it.
fn sibling_binary(stem: &str) -> std::path::PathBuf {
    let file = crate::platform::executable::name::native(stem);
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(&file);
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    std::path::PathBuf::from(stem)
}

#[cfg(test)]
#[path = "binaries_tests.rs"]
mod tests;
