//! `SOLDR_LINKER` override (issue #285).
//!
//! Lets users pick the linker that `soldr cargo ...` injects for the active
//! build target. The choice can come from the `SOLDR_LINKER` env var or the
//! `linker = "..."` field in `~/.soldr/config.toml`; env wins.
//!
//! The selection is resolved into per-target Cargo env vars that the cargo
//! front door layers onto the spawned cargo process. Linux clang driver
//! arguments live in content-addressed linker shims, rather than
//! `CARGO_TARGET_<TRIPLE>_RUSTFLAGS`, because Cargo makes target rustflags
//! replace (not extend) a project's `[build] rustflags` (soldr#3277). The
//! shim digest in `CARGO_TARGET_*_LINKER` also gives the wrapper cache a
//! stable identity for its exact content.

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::str::FromStr;

const PEP517_LINKER_POLICY_ENV: &str = "SOLDR_PEP517_LINKER";
const PEP517_LINKER_FALLBACK_FILE: &str = "pep517-linker-fallback-v1.tsv";

/// User-facing linker choices accepted by `SOLDR_LINKER` and the
/// `linker = "..."` config field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkerChoice {
    /// Do nothing: leave whatever the rust-toolchain default is in place.
    Default,
    /// Use the platform's system linker (`ld` / `ld64` / `link.exe`). On
    /// every supported platform this is the platform default, so it is
    /// also a no-op injection.
    Ld,
    /// Use the [mold](https://github.com/rui314/mold) linker. Linux only.
    Mold,
    /// Use rustup's bundled `rust-lld`. Available on every supported
    /// platform.
    RustLld,
    /// Use the [reld](https://github.com/zackees/reld) linker. Available on
    /// every supported platform — a native ELF backend on Linux, and an
    /// `lld` bridge for Windows/COFF and macOS/Mach-O. Invoked directly on
    /// `PATH`.
    Reld,
    /// Pick the fastest available linker per platform: reld, driven through
    /// clang (`--ld-path=reld`) on Linux so the driver injects the CRT, and
    /// directly on Windows/macOS.
    Fast,
}

impl FromStr for LinkerChoice {
    type Err = SoldrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Ok(LinkerChoice::Default);
        }
        match trimmed.to_ascii_lowercase().as_str() {
            "default" => Ok(LinkerChoice::Default),
            "ld" => Ok(LinkerChoice::Ld),
            "mold" => Ok(LinkerChoice::Mold),
            "rust-lld" | "rustlld" | "rust_lld" => Ok(LinkerChoice::RustLld),
            "reld" => Ok(LinkerChoice::Reld),
            "fast" => Ok(LinkerChoice::Fast),
            other => Err(SoldrError::Other(format!(
                "invalid SOLDR_LINKER value `{other}` (expected one of: default, ld, mold, rust-lld, reld, fast)"
            ))),
        }
    }
}

/// The resolved per-target injection. `None` fields mean "do not set this
/// `CARGO_TARGET_<TRIPLE>_*` env var".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkerInjection {
    pub linker: Option<String>,
    pub rustflags: Option<String>,
}

/// State carried from maturin command preparation to its process runner.
/// `injected_env` contains only values soldr added, so an automatic retry can
/// remove them without clobbering an explicit project/caller linker setting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pep517LinkerState {
    pub automatic_fast: bool,
    pub explicit_fast: bool,
    pub cached_fallback: bool,
    pub cache_key: Option<String>,
    pub candidate: Option<String>,
    pub injected_env: Vec<String>,
}

impl Pep517LinkerState {
    pub fn should_retry(&self) -> bool {
        self.automatic_fast && !self.cached_fallback && !self.injected_env.is_empty()
    }

    pub fn clear_injected_env(&self, command: &mut Command) {
        for key in &self.injected_env {
            command.env_remove(key);
        }
    }
}

pub use crate::linker_shim::{
    inject_resolved_reld, materialize_linker_driver_shim, materialize_linker_driver_shim_resolving,
    FetchMode,
};

impl LinkerInjection {
    fn none() -> Self {
        Self::default()
    }

    fn clang_with_fuse(fuse: &str) -> Self {
        Self {
            linker: Some("clang".to_string()),
            rustflags: Some(format!("-C link-arg=-fuse-ld={fuse}")),
        }
    }

    /// Drive clang with `--ld-path` pointing at a specific linker binary.
    ///
    /// `-fuse-ld` only accepts a closed set of well-known linker names (bfd,
    /// gold, lld, mold, …) and rejects arbitrary ones like `reld`, so a
    /// third-party linker must be selected via `--ld-path=<name>` instead.
    fn clang_with_ld_path(path: &str) -> Self {
        Self {
            linker: Some("clang".to_string()),
            rustflags: Some(format!("-C link-arg=--ld-path={path}")),
        }
    }

    fn rust_lld_msvc() -> Self {
        Self {
            linker: Some("rust-lld".to_string()),
            rustflags: None,
        }
    }

    fn reld() -> Self {
        Self {
            linker: Some("reld".to_string()),
            rustflags: None,
        }
    }

    fn apple_fast_linker() -> Self {
        // Native macOS uses the platform's Mach-O ld64. Linux-hosted Apple
        // cross-builds use LLVM lld, which selects its Mach-O driver from the
        // target triple.
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::MacOs {
            Self::none()
        } else {
            Self::clang_with_fuse("lld")
        }
    }
}

/// Resolve a `LinkerChoice` from (in order): the env var if set, the
/// config string if set, otherwise `Fast` (reld is the default linker,
/// soldr#3262).
pub fn from_env_and_config(
    env: Option<&OsStr>,
    config: Option<&str>,
) -> Result<LinkerChoice, SoldrError> {
    if let Some(env) = env {
        let env = env
            .to_str()
            .ok_or_else(|| SoldrError::Other("SOLDR_LINKER is not valid UTF-8".to_string()))?;
        return LinkerChoice::from_str(env);
    }
    if let Some(config) = config {
        return LinkerChoice::from_str(config);
    }
    Ok(LinkerChoice::Fast)
}

pub use crate::linker_project::{
    project_config_declares_dylint_link, reld_fetch_error, resolve_project_choice,
    resolve_project_choice_from_cwd, resolve_project_choice_with_lint_roots, LinkerSource,
    ProjectLinkerSelection, ReldCargoConfig,
};

pub(crate) fn target_kind(target: &str) -> TargetKind {
    if target.contains("-windows-msvc") {
        TargetKind::WindowsMsvc
    } else if target.contains("-windows-gnu") {
        TargetKind::WindowsGnu
    } else if target.contains("-apple-") || target.contains("-darwin") {
        TargetKind::Apple
    } else if target.contains("-linux-") {
        TargetKind::Linux
    } else {
        TargetKind::Other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetKind {
    Linux,
    Apple,
    WindowsMsvc,
    WindowsGnu,
    Other,
}

/// Resolve a `LinkerChoice` plus an active target triple into the env
/// var injection that should be layered onto the cargo subprocess.
///
/// Errors when the choice is unsupported on the target (e.g.
/// `SOLDR_LINKER=mold` on macOS or Windows).
pub fn resolve_for_target(
    choice: LinkerChoice,
    target: &str,
) -> Result<LinkerInjection, SoldrError> {
    let reld_present = || reld_on_path();
    resolve_for_target_with_probe(choice, target, &reld_present)
}

/// Same as [`resolve_for_target`] but with the reld-on-PATH probe injected so
/// tests can exercise both branches of `fast`.
pub fn resolve_for_target_with_probe(
    choice: LinkerChoice,
    target: &str,
    reld_present: &dyn Fn() -> bool,
) -> Result<LinkerInjection, SoldrError> {
    let kind = target_kind(target);
    match choice {
        LinkerChoice::Default | LinkerChoice::Ld => Ok(LinkerInjection::none()),
        LinkerChoice::Mold => match kind {
            TargetKind::Linux => Ok(LinkerInjection::clang_with_fuse("mold")),
            _ => Err(SoldrError::Other(format!(
                "mold is not supported on `{target}`; use 'fast' for a portable fallback"
            ))),
        },
        LinkerChoice::RustLld => match kind {
            TargetKind::WindowsMsvc => Ok(LinkerInjection::rust_lld_msvc()),
            // Apple clang only accepts `-fuse-ld=lld` when the toolchain has
            // wired up a `ld64.lld` shim, and stock macOS toolchains do not.
            // Injecting `-fuse-ld=lld` breaks even `cc-rs` build-script
            // compilations (issue #509). Fall back to the platform default
            // linker silently on Apple targets.
            TargetKind::Apple => Ok(LinkerInjection::apple_fast_linker()),
            TargetKind::Linux | TargetKind::WindowsGnu | TargetKind::Other => {
                Ok(LinkerInjection::clang_with_fuse("lld"))
            }
        },
        // reld is the explicit choice (`SOLDR_LINKER=reld`): no probe, the
        // caller asked for reld specifically. On Linux its native ELF backend
        // does not inject the CRT startup objects, so a direct
        // `-C linker=reld` would link a binary with no `_start`: drive reld
        // through clang (`--ld-path=reld`) so the driver injects CRT and the
        // interpreter. Apple targets need the clang driver too (soldr#3359):
        // rustc's default Apple linker flavor is `darwin-cc`, which hands the
        // linker clang-driver arguments (`-mmacosx-version-min`, `-Wl,...`,
        // `-nodefaultlibs`) that ld64.lld cannot parse and that reld refuses
        // to translate (reld#192); clang turns them into raw ld64 argv and
        // supplies the SDK. On Windows reld bridges to lld-link, which takes
        // rustc's MSVC argv directly, so the bare `reld` injection is right.
        LinkerChoice::Reld => match kind {
            TargetKind::Linux | TargetKind::Apple => {
                Ok(LinkerInjection::clang_with_ld_path("reld"))
            }
            _ => Ok(LinkerInjection::reld()),
        },
        // `Fast` is the automatic/default choice (soldr#3262): prefer reld,
        // but reld is not yet bundled or universally installed, so probe for
        // it and fall back to the prior per-platform fast linker rather than
        // failing the link with `invalid linker name in argument
        // '--ld-path=reld'`.
        LinkerChoice::Fast => match kind {
            TargetKind::Linux => {
                if reld_present() {
                    Ok(LinkerInjection::clang_with_ld_path("reld"))
                } else {
                    Ok(LinkerInjection::clang_with_fuse("lld"))
                }
            }
            TargetKind::WindowsMsvc => {
                if reld_present() {
                    Ok(LinkerInjection::reld())
                } else {
                    Ok(LinkerInjection::rust_lld_msvc())
                }
            }
            TargetKind::Apple => {
                if reld_present() {
                    // Same `darwin-cc` constraint as the explicit arm above.
                    Ok(LinkerInjection::clang_with_ld_path("reld"))
                } else {
                    Ok(LinkerInjection::apple_fast_linker())
                }
            }
            TargetKind::WindowsGnu => {
                if reld_present() {
                    Ok(LinkerInjection::reld())
                } else {
                    // The blessed Windows GNU lifecycle provisions a
                    // relocatable GCC bundle with its matching binutils.
                    Ok(LinkerInjection::none())
                }
            }
            TargetKind::Other => {
                if reld_present() {
                    Ok(LinkerInjection::reld())
                } else {
                    Ok(LinkerInjection::clang_with_fuse("lld"))
                }
            }
        },
    }
}

/// Apply the automatic fast-linker policy used by the PEP backend. Direct
/// `soldr cargo` remains governed by `SOLDR_LINKER` / config.toml; the Python
/// backend opts into this policy with `SOLDR_PEP517_LINKER=auto`.
pub async fn apply_pep517_override(
    command: &mut Command,
    target: &str,
    paths: &SoldrPaths,
) -> Result<Pep517LinkerState, SoldrError> {
    // soldr#3276: `resolve_project_choice_from_cwd` is the one shared
    // resolver -- it already implements the env > cargo-config >
    // Cargo.toml-metadata > user-config > default precedence, including the
    // soldr#3277 suppression for a project's own `.cargo/config.toml`
    // linker/rustflags declaration (that case resolves to
    // `LinkerChoice::Default`, source `CargoConfig`).
    // The PEP 517 path is never a Dylint-scoped invocation, so `false`
    // keeps its per-lint config roots deliberately unconsulted (soldr#3483).
    let selection = resolve_project_choice_from_cwd(Some(target), paths, false)?;
    let automatic_fast = selection.source == LinkerSource::Default
        && std::env::var(PEP517_LINKER_POLICY_ENV)
            .ok()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("auto"));
    let choice = if automatic_fast {
        LinkerChoice::Fast
    } else {
        selection.choice
    };

    let rustflags_already_declared =
        selection.reld_cargo_config == Some(ReldCargoConfig::Rustflags);
    let mut injection = if rustflags_already_declared {
        // The project's own cargo config already drives a bare reld through
        // rustflags; leave the environment untouched rather than layering a
        // second, possibly-conflicting injection on top of it.
        LinkerInjection::default()
    } else {
        resolve_for_target(choice, target)?
    };
    if choice == LinkerChoice::Reld && !rustflags_already_declared {
        let reld = crate::fetch::ensure_reld(paths)
            .await
            .map_err(reld_fetch_error)?;
        inject_resolved_reld(&mut injection, &reld)?;
    }
    let rustc_lld = crate::linker_shim::rustc_lld::for_command(
        target,
        command,
        &command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        injection.rustflags.as_deref(),
    );
    // The PEP 517 backend has no soldr wrapper to wait for a background
    // fetch, so it awaits the download here (soldr#3801).
    materialize_linker_driver_shim_resolving(
        paths,
        target,
        &mut injection,
        rustc_lld,
        FetchMode::Await,
    )
    .await?;
    let prefix = cargo_target_env_prefix(target);
    let linker_key = format!("CARGO_TARGET_{prefix}_LINKER");
    let rustflags_key = format!("CARGO_TARGET_{prefix}_RUSTFLAGS");
    let mut injected_env = Vec::new();

    let cache_key = if automatic_fast && injection != LinkerInjection::default() {
        pep517_fallback_key(target, &injection)
    } else {
        None
    };
    let cached_fallback = cache_key
        .as_deref()
        .is_some_and(|key| fallback_cache_contains(paths, key));

    if !cached_fallback {
        if let Some(linker) = injection.linker.as_deref() {
            if !effective_command_env_is_non_empty(command, &linker_key) {
                let linker = crate::linker_shim::linker_value_for_command(command, linker)?;
                command.env(&linker_key, linker);
                injected_env.push(linker_key.clone());
            }
        }
        if let Some(rustflags) = injection.rustflags.as_deref() {
            if !effective_command_env_is_non_empty(command, &rustflags_key) {
                command.env(&rustflags_key, rustflags);
                injected_env.push(rustflags_key.clone());
            }
        }
    }

    let explicit_fast = !automatic_fast && choice == LinkerChoice::Fast;
    Ok(Pep517LinkerState {
        automatic_fast,
        explicit_fast,
        cached_fallback,
        cache_key,
        candidate: if injection == LinkerInjection::default() {
            None
        } else {
            Some(linker_description(&injection))
        },
        injected_env,
    })
}

/// Return whether a non-empty value will reach the cargo child. Values set
/// directly on `command` take precedence over the parent's environment,
/// including an explicit removal. This helper keeps linker injection from
/// clobbering a target-specific cross toolchain with SOLDR_LINKER's host
/// default.
pub(crate) fn effective_command_env_is_non_empty(command: &Command, key: &str) -> bool {
    if let Some(value) = command
        .get_envs()
        .find(|(candidate, _)| *candidate == OsStr::new(key))
        .map(|(_, value)| value)
    {
        return value.is_some_and(|value| !value.is_empty());
    }
    std::env::var_os(key).is_some_and(|value| !value.is_empty())
}

fn linker_description(injection: &LinkerInjection) -> String {
    match (&injection.linker, &injection.rustflags) {
        (Some(linker), Some(flags)) => format!("{linker} ({flags})"),
        (Some(linker), None) => linker.clone(),
        (None, Some(flags)) => flags.clone(),
        (None, None) => "platform default".to_string(),
    }
}

pub(crate) fn project_root(start: &Path) -> PathBuf {
    for directory in start.ancestors() {
        if directory.join("Cargo.toml").is_file() {
            return directory.to_path_buf();
        }
    }
    start.to_path_buf()
}

/// Read `[target.<triple>] <key>` from `root/.cargo/config.toml` (or legacy
/// `.cargo/config`). Exact-triple sections win; the universally-matching
/// `[target.'cfg(all())']` section is the fallback (see
/// [`target_config_value_in_files`] for why both matter). Other cfg
/// expressions are not evaluated — without a cfg matcher soldr cannot know
/// whether they apply to the active target, and guessing wrong in either
/// direction is worse than leaving them alone.
#[cfg(test)]
fn target_config_value_in_root(root: &Path, target: &str, key: &str) -> Option<String> {
    let config_files: Vec<PathBuf> = [".cargo/config.toml", ".cargo/config"]
        .iter()
        .map(|relative| root.join(relative))
        .collect();
    target_config_value_in_files(&config_files, target, key)
}

/// Read `<key>` from the first of `config_files` (in order) that exists,
/// parses, and declares it for `target`. Generalized out of
/// `target_config_value_in_root` (soldr#3276) so callers can layer a
/// project's `.cargo/config.toml` ahead of `$CARGO_HOME/config.toml`
/// without duplicating the TOML-walk logic.
///
/// Within one file an exact `[target.<triple>]` section wins over
/// `[target.'cfg(all())']`, matching Cargo's rule that a `<triple>` linker
/// takes precedence over a `<cfg>` one. The `cfg(all())` fallback exists
/// because soldr#3483's failure mode depended on it being missed: every
/// Dylint lint crate declares its linker as
/// `[target.'cfg(all())'] linker = "dylint-link"`, which applies to every
/// target yet used to be invisible here — so the guard fell through to the
/// automatic default, injected `CARGO_TARGET_<triple>_LINKER`, and (triple
/// beating cfg) silently disabled `dylint-link` for the lint build. Other
/// cfg expressions (`cfg(unix)`, …) are still not evaluated: soldr has no
/// cfg matcher, and a wrong guess would suppress — or fail to suppress —
/// the automatic default for an unrelated project build.
pub(crate) fn target_config_value_in_files(
    config_files: &[PathBuf],
    target: &str,
    key: &str,
) -> Option<String> {
    for path in config_files {
        let Ok(contents) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(value) = contents.parse::<toml::Value>() else {
            continue;
        };
        let configured = value.get("target").and_then(|targets| {
            targets
                .get(target)
                .and_then(|target_config| target_config.get(key))
                .or_else(|| {
                    targets
                        .get("cfg(all())")
                        .and_then(|target_config| target_config.get(key))
                })
        });
        if let Some(value) = configured {
            let text = match value {
                toml::Value::String(value) => value.clone(),
                toml::Value::Array(values) => values
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => value.to_string(),
            };
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

fn pep517_fallback_key(target: &str, injection: &LinkerInjection) -> Option<String> {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsStr::new("rustc").to_os_string());
    let rustc_identity = Command::new(&rustc).arg("-vV").output().ok()?;
    if !rustc_identity.status.success() {
        return None;
    }

    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"schema", b"pep517-linker-fallback-v1");
    hash_field(&mut hasher, b"target", target.as_bytes());
    hash_field(
        &mut hasher,
        b"injection",
        linker_description(injection).as_bytes(),
    );
    hash_field(
        &mut hasher,
        b"linker-identity",
        &linker_candidate_identity(injection),
    );
    hash_field(&mut hasher, b"rustc", &rustc_identity.stdout);
    for name in [
        "CARGO_BUILD_TARGET",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "PATH",
        "CC",
        "CXX",
        "AR",
        "CMAKE",
        "SDKROOT",
        "MACOSX_DEPLOYMENT_TARGET",
        "VCToolsInstallDir",
        "WindowsSdkDir",
        "LIB",
        "SOLDR_PEP517_PROFILE",
        "SOLDR_PEP517_PROJECT_ID",
        PEP517_LINKER_POLICY_ENV,
    ] {
        hash_field(
            &mut hasher,
            name.as_bytes(),
            std::env::var_os(name)
                .as_deref()
                .unwrap_or_else(|| OsStr::new(""))
                .to_string_lossy()
                .as_bytes(),
        );
    }

    let current = std::env::current_dir().ok()?;
    let root = project_root(&current);
    for relative in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        ".cargo/config.toml",
        ".cargo/config",
    ] {
        let path = root.join(relative);
        if let Ok(bytes) = std::fs::read(&path) {
            hash_field(&mut hasher, relative.as_bytes(), &bytes);
        }
    }
    Some(hex::encode(hasher.finalize()))
}

fn linker_candidate_identity(injection: &LinkerInjection) -> Vec<u8> {
    let mut candidates = Vec::new();
    if let Some(linker) = injection.linker.as_deref() {
        candidates.push(linker.to_string());
    }
    if let Some(rustflags) = injection.rustflags.as_deref() {
        for candidate in ["mold", "ld.lld"] {
            if rustflags.contains(&format!("-fuse-ld={candidate}")) {
                candidates.push(candidate.to_string());
            }
        }
        // reld on Linux is selected through `--ld-path=reld` (its name is
        // hidden in the rustflags, not the linker field), so probe the
        // `--ld-path` target too: a reld upgrade must invalidate the PEP 517
        // fallback cache (soldr#3262).
        if let Some(path) = extract_ld_path(rustflags) {
            candidates.push(path.to_string());
        }
    }

    let mut identity = Vec::new();
    for candidate in candidates {
        identity.extend_from_slice(candidate.as_bytes());
        identity.push(0);
        let mut command = Command::new(&candidate);
        command.arg("--version");
        suppress_windows_console_window(&mut command);
        match command.output() {
            Ok(output) => {
                identity.extend_from_slice(&output.stdout);
                identity.extend_from_slice(&output.stderr);
                identity.extend_from_slice(&output.status.code().unwrap_or(-1).to_le_bytes());
            }
            Err(err) => identity.extend_from_slice(err.to_string().as_bytes()),
        }
        identity.push(0xff);
    }
    identity
}

/// Extract the linker name from a `--ld-path=<name>` fragment of a
/// `-C link-arg=...` rustflags value, if present.
fn extract_ld_path(rustflags: &str) -> Option<&str> {
    extract_flag_value(rustflags, "--ld-path=")
}

/// Extract the value following the first occurrence of `marker` in `text`,
/// stopping at the next whitespace. Shared by `extract_ld_path` and the
/// project-linker resolver's bare-`reld` detection (soldr#3276).
pub(crate) fn extract_flag_value<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    let start = text.find(marker)? + marker.len();
    let end = text[start..]
        .find(char::is_whitespace)
        .map_or(text.len(), |offset| start + offset);
    let value = text[start..end].trim();
    (!value.is_empty()).then_some(value)
}

fn hash_field(hasher: &mut Sha256, name: &[u8], value: &[u8]) {
    hasher.update((name.len() as u64).to_le_bytes());
    hasher.update(name);
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn fallback_cache_path(paths: &SoldrPaths) -> PathBuf {
    paths.cache.join(PEP517_LINKER_FALLBACK_FILE)
}

fn fallback_cache_contains(paths: &SoldrPaths, key: &str) -> bool {
    std::fs::read_to_string(fallback_cache_path(paths))
        .map(|contents| contents.lines().any(|line| line.trim() == key))
        .unwrap_or(false)
}

pub fn record_pep517_fallback(paths: &SoldrPaths, key: Option<&str>) -> Result<(), SoldrError> {
    let Some(key) = key else { return Ok(()) };
    paths.ensure_dirs()?;
    let path = fallback_cache_path(paths);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)?;
    file.lock_exclusive()?;
    file.seek(SeekFrom::Start(0))?;
    let mut existing = String::new();
    file.read_to_string(&mut existing)?;
    if !existing.lines().any(|line| line.trim() == key) {
        file.seek(SeekFrom::End(0))?;
        writeln!(file, "{key}")?;
    }
    file.unlock()?;
    Ok(())
}

/// Keep linker retry conservative: only output with an explicit linker
/// signal and a failure marker is eligible for the one-shot fallback.
/// Does this rustc invocation build a proc-macro crate?
///
/// Cargo spells the flag both ways, so both are matched.
fn builds_a_proc_macro(args: &[String]) -> bool {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--crate-type=") {
            if value.split(',').any(|v| v.trim() == "proc-macro") {
                return true;
            }
        } else if arg == "--crate-type"
            && iter
                .next()
                .is_some_and(|v| v.split(',').any(|v| v.trim() == "proc-macro"))
        {
            return true;
        }
    }
    false
}

/// The target this invocation compiles for: an explicit `--target`, else the
/// host.
fn effective_target(args: &[String], host: &str) -> String {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--target=") {
            return value.to_string();
        }
        if arg == "--target" {
            if let Some(value) = iter.next() {
                return value.clone();
            }
        }
    }
    host.to_string()
}

/// Drop an injected `-C linker=rust-lld` / `-C linker=reld` when this
/// invocation builds a proc-macro for an MSVC target (soldr#1992, soldr#3262).
///
/// `SOLDR_LINKER=fast` (and the reld default) inject the linker through
/// `CARGO_TARGET_<TRIPLE>_LINKER`, which cargo applies to *every* crate for
/// that target. Proc-macros are the one kind that cannot take it: they build
/// as DLLs via `-C prefer-dynamic`, and rust-lld — and reld, which bridges to
/// lld-link on MSVC — reliably fails that link on `x86_64-pc-windows-msvc`.
/// The build dies with a bare `exit code: 1`.
///
/// Because the env var is per-invocation, the exclusion cannot live at the
/// injection site -- cargo, not soldr, decides which crates receive it. The
/// wrapper is the first place that sees a *per-crate* argv, so the flag is
/// removed here, leaving rustc to use the platform default exactly as
/// `SOLDR_LINKER=default` would.
///
/// Scoped to MSVC deliberately. rust-lld links proc-macro dylibs fine
/// elsewhere, and stripping it there would silently forfeit the fast linker
/// for every derive crate.
pub fn strip_fast_linker_for_proc_macro<'a>(args: &'a [String], host: &str) -> Cow<'a, [String]> {
    if !builds_a_proc_macro(args) || !effective_target(args, host).ends_with("-pc-windows-msvc") {
        return Cow::Borrowed(args);
    }
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut iter = args.iter().peekable();
    let mut removed = false;
    while let Some(arg) = iter.next() {
        // `-Clinker=rust-lld` / `-Clinker=reld`
        if arg
            .strip_prefix("-C")
            .and_then(|rest| rest.strip_prefix("linker="))
            .is_some_and(is_injected_linker_to_strip)
        {
            removed = true;
            continue;
        }
        // `-C linker=rust-lld` / `-C linker=reld` as two arguments.
        if arg == "-C"
            && iter.peek().is_some_and(|next| {
                next.strip_prefix("linker=")
                    .is_some_and(is_injected_linker_to_strip)
            })
        {
            iter.next();
            removed = true;
            continue;
        }
        out.push(arg.clone());
    }
    if removed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(args)
    }
}

/// Whether a `-C linker=<name>` value is one soldr injects for the MSVC
/// proc-macro exclusion: rust-lld, and reld (soldr#3262), which bridges to
/// lld-link and has the same proc-macro-DLL failure mode.
fn is_injected_linker_to_strip(value: &str) -> bool {
    matches!(value.trim(), "rust-lld" | "reld")
}

/// Report the outcome of the automatic standard-linker retry.
///
/// soldr#1992 / soldr#1999 rule 1. The retry warning is printed *before* the
/// fallback build runs, so when the fallback also fails the user's last screen
/// is the fallback's own output -- including rustc's "the Visual Studio build
/// tools may need to be repaired" note, which is advice to reinstall a healthy
/// toolchain. The warning that would explain it is a full build's worth of
/// output further up, and nothing at the failure connects the two.
///
/// So the failure branch restates it at the point of failure. Both branches
/// live here rather than at the call site so the wording is assertable and so
/// linker policy stays in the linker module.
pub fn report_fallback_outcome(
    fallback: &Output,
    paths: Option<&SoldrPaths>,
    cache_key: Option<&str>,
    candidate: &str,
) {
    if fallback.status.success() {
        if let Some(paths) = paths {
            if let Err(err) = record_pep517_fallback(paths, cache_key) {
                soldr_core::warning_log::warn(format!(
                    "soldr warning: could not persist the working linker fallback: {err}"
                ));
            }
        }
        eprintln!(
            "soldr: standard linker fallback succeeded; future equivalent PEP 517 builds will reuse it"
        );
        return;
    }
    eprintln!("{}", fallback_also_failed_note(candidate));
}

/// The note printed when the standard-linker retry fails too.
///
/// Split out so the wording is testable without running a build.
pub fn fallback_also_failed_note(candidate: &str) -> String {
    format!(
        "soldr: the standard-linker retry also failed, so `{candidate}` was not the cause.
         soldr: the errors above are from that second attempt, with soldr's linker          selection already removed -- any \"repair your build tools\" advice in them is          about the fallback build, not about soldr (soldr#1992)."
    )
}

pub fn looks_like_linker_failure(output: &Output) -> bool {
    if output.status.success() {
        return false;
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    looks_like_linker_failure_text(&text)
}

fn looks_like_linker_failure_text(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    let linker_signal = [
        "linking with",
        "linker",
        "link.exe",
        "lld-link",
        "fuse-ld",
        "mold",
        "ld returned",
    ];
    let failure_signal = [
        "failed",
        "error",
        "not found",
        "cannot",
        "could not",
        "exit status",
    ];
    linker_signal.iter().any(|needle| text.contains(needle))
        && failure_signal.iter().any(|needle| text.contains(needle))
}

/// Whether the `reld` linker is available on `PATH`. `Fast`/the default linker
/// falls back to rust-lld (or the platform default) when reld is not installed,
/// since reld is not yet bundled or universally shipped (soldr#3262).
///
/// A file lookup rather than spawning `reld --version`: this runs on every
/// `soldr cargo` invocation, so it must not cost a process spawn. It looks for
/// `reld` plus the platform executable suffix only — never PATHEXT — because
/// rustc spawns the linker the same way (`reld.exe` on Windows); a `reld.cmd`
/// it could not run must not count as present.
fn reld_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let name = format!("reld{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(&path).any(|dir| !dir.as_os_str().is_empty() && dir.join(&name).is_file())
}

/// Convert a target triple to the uppercase underscore form Cargo uses
/// for per-target env vars. `x86_64-unknown-linux-gnu` becomes
/// `X86_64_UNKNOWN_LINUX_GNU`, so the corresponding linker env var name
/// is `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER`.
pub fn cargo_target_env_prefix(triple: &str) -> String {
    triple.replace('-', "_").to_ascii_uppercase()
}

#[cfg(test)]
#[path = "linker_tests.rs"]
mod tests;
