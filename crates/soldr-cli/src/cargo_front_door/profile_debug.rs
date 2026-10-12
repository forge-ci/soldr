//! Cargo `[profile.<X>].debug` default detection and injection.
//!
//! When the user invokes a debug-default cargo subcommand
//! (`build`/`check`/`test`/etc.) without an explicit profile pin in
//! either CLI flags, `--config` overrides, the manifest, or a discovered
//! `.cargo/config.toml`, soldr injects a low-cost debug-info default and emits
//! a warning whose once-per-repository decision is owned by the daemon. MSVC
//! targets retain line tables so their cached PDBs remain symbolizable; other
//! targets keep the historical `false` default.
//!
//! All of these helpers are argv-and-filesystem only — they do not
//! touch the running process beyond environment-variable reads and TOML
//! parses, which keeps the cargo front door cheap when the early-out
//! `cargo_profile_debug_default_for_args` returns `None`.

use crate::core::{SoldrError, SoldrPaths};
use crate::{CARGO_PROFILE_DEV_DEBUG_ENV_VAR, CARGO_PROFILE_TEST_DEBUG_ENV_VAR};
use std::collections::BTreeSet;

use super::subcommand::first_cargo_subcommand;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CargoProfileDebugDefault {
    pub(crate) profile: &'static str,
    pub(crate) env_var: &'static str,
    pub(crate) value: &'static str,
}

impl CargoProfileDebugDefault {
    fn for_profile(profile: &str, value: &'static str) -> Option<Self> {
        match profile {
            "dev" | "debug" => Some(Self {
                profile: "dev",
                env_var: CARGO_PROFILE_DEV_DEBUG_ENV_VAR,
                value,
            }),
            "test" => Some(Self {
                profile: "test",
                env_var: CARGO_PROFILE_TEST_DEBUG_ENV_VAR,
                value,
            }),
            _ => None,
        }
    }

    fn lookup_profiles(self) -> &'static [&'static str] {
        match self.profile {
            "test" => &["test", "dev"],
            _ => &["dev"],
        }
    }
}

pub(super) fn maybe_apply_cargo_profile_debug_default(
    command: &mut std::process::Command,
    args: &[String],
    paths: &SoldrPaths,
    target: Option<&str>,
) -> Result<Option<UnrecordedDebugWarning>, SoldrError> {
    let msvc_target = cargo_invocation_targets_msvc(args, target)?;
    let Some(default) = cargo_profile_debug_default_for_args(args, msvc_target) else {
        return Ok(None);
    };
    if cargo_profile_debug_is_specified(args, default)? {
        return Ok(None);
    }

    command.env(default.env_var, default.value);
    let repo_path = cargo_invocation_repo_path(args);
    let decision = cargo_debug_default_warning_decision(paths, &repo_path);
    if decision != WarningDecision::Suppress {
        eprintln!(
            "soldr: warning: Cargo profile.{}.debug is unspecified for {}; setting {}={} for this invocation. Set `debug` explicitly under `[profile.{}]` in Cargo.toml or .cargo/config.toml to override this default.",
            default.profile,
            repo_path.display(),
            default.env_var,
            default.value,
            default.profile
        );
    }

    Ok((decision == WarningDecision::EmitUnrecorded)
        .then_some(UnrecordedDebugWarning { repo_path }))
}

/// A debug-default warning printed while the daemon was unreachable, so
/// nothing recorded that this repository has been warned (soldr#3740).
///
/// A cold first build reaches this check before any compile has started the
/// daemon, so its warning fails open; without a record, the next build asked
/// the now-running daemon, which had never seen the repository and warned
/// again. The front door records it once the build has run.
#[derive(Debug)]
pub(super) struct UnrecordedDebugWarning {
    repo_path: std::path::PathBuf,
}

impl UnrecordedDebugWarning {
    /// Record the warning with the daemon the build started. Best effort:
    /// still unreachable means the next build warns again, as before.
    pub(super) fn record(&self, paths: &SoldrPaths) {
        let sock = crate::daemon::client::default_sock_path(paths);
        let _ = crate::daemon::client::should_warn_cargo_debug_default(&sock, &self.repo_path);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WarningDecision {
    /// The daemon recorded the repository and said to warn.
    Emit,
    /// The daemon was unreachable: warn, but nothing recorded it.
    EmitUnrecorded,
    /// Already warned for this repository.
    Suppress,
}

fn cargo_profile_debug_default_for_args(
    args: &[String],
    msvc_target: bool,
) -> Option<CargoProfileDebugDefault> {
    let subcommand = first_cargo_subcommand(args)?;
    let value = if msvc_target {
        // soldr#2148: zccache retains the PDB beside an MSVC image, but a PDB
        // produced with debug=false contains public symbols without source
        // lines. Keep the inexpensive line tables so cached dev/test builds
        // remain symbolizable without restoring Cargo's full debug=2 default.
        "line-tables-only"
    } else {
        "false"
    };

    if subcommand == "nextest" {
        return if cargo_args_contain_release(args) {
            None
        } else {
            CargoProfileDebugDefault::for_profile("test", value)
        };
    }

    if cargo_args_contain_release(args) {
        return None;
    }

    if let Some(profile) = cargo_profile_arg_value(args) {
        return CargoProfileDebugDefault::for_profile(&profile, value);
    }

    match subcommand {
        "t" | "test" => CargoProfileDebugDefault::for_profile("test", value),
        "install" if cargo_install_args_contain_debug(args) => {
            CargoProfileDebugDefault::for_profile("dev", value)
        }
        "install" | "bench" => None,
        "b" | "build" | "c" | "check" | "d" | "doc" | "r" | "run" | "rustc" | "clippy" | "fix" => {
            CargoProfileDebugDefault::for_profile("dev", value)
        }
        _ => None,
    }
}

fn cargo_invocation_targets_msvc(
    args: &[String],
    known_target: Option<&str>,
) -> Result<bool, SoldrError> {
    let explicit_targets = cargo_args_target_values(args);
    if !explicit_targets.is_empty() {
        return Ok(explicit_targets.iter().any(|target| is_msvc_target(target)));
    }
    let cwd = std::env::current_dir()?;
    configured_targets_msvc(args, known_target, &cwd)
}

fn configured_targets_msvc(
    args: &[String],
    known_target: Option<&str>,
    cwd: &std::path::Path,
) -> Result<bool, SoldrError> {
    let mut configured = cargo_config_build_targets(cwd);
    if let Some(target) = known_target {
        merge_configured_targets(
            &mut configured,
            ConfiguredTargets::Scalar(is_msvc_target(target)),
        );
    }
    if let Some(command_line) = cargo_config_args_build_targets(args)? {
        merge_configured_targets(&mut configured, command_line);
    }
    Ok(configured.is_some_and(ConfiguredTargets::contains_msvc))
}

fn is_msvc_target(target: &str) -> bool {
    target.trim().ends_with("-pc-windows-msvc")
}

fn cargo_args_target_values(args: &[String]) -> Vec<String> {
    let mut targets = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--target" {
            if let Some(target) = iter.next() {
                targets.push(target.clone());
            }
            continue;
        }
        if let Some(target) = arg.strip_prefix("--target=") {
            targets.push(target.to_string());
        }
    }
    targets
}

fn cargo_config_args_build_targets(
    args: &[String],
) -> Result<Option<ConfiguredTargets>, SoldrError> {
    let cwd = std::env::current_dir()?;
    let mut selected = None;
    for value in cargo_config_arg_values(args) {
        let raw = value.trim();
        let path = std::path::Path::new(raw);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        let parsed = if path.is_file() {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|text| toml_text_build_msvc_target(&text))
        } else {
            toml_text_build_msvc_target(raw)
        };
        if let Some(setting) = parsed {
            merge_configured_targets(&mut selected, setting);
        }
    }
    Ok(selected)
}

fn cargo_config_build_targets(start_dir: &std::path::Path) -> Option<ConfiguredTargets> {
    let mut selected = None;
    if let Some(cargo_home) = cargo_home_dir_for_config() {
        if let Some(path) = cargo_config_path_in(&cargo_home) {
            if let Ok(text) = std::fs::read_to_string(path) {
                toml_text_build_msvc_target(&text)
                    .into_iter()
                    .for_each(|setting| merge_configured_targets(&mut selected, setting));
            }
        }
    }

    let mut hierarchy = Vec::new();
    let mut current = Some(start_dir.to_path_buf());
    while let Some(dir) = current {
        hierarchy.push(dir);
        current = hierarchy
            .last()
            .and_then(|dir| dir.parent().map(std::path::Path::to_path_buf));
    }
    for dir in hierarchy.iter().rev() {
        if let Some(path) = cargo_config_path_in(&dir.join(".cargo")) {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Some(setting) = toml_text_build_msvc_target(&text) {
                    merge_configured_targets(&mut selected, setting);
                }
            }
        }
    }
    selected
}

fn cargo_config_path_in(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let legacy = dir.join("config");
    if legacy.is_file() {
        return Some(legacy);
    }
    let toml = dir.join("config.toml");
    toml.is_file().then_some(toml)
}

#[derive(Clone, Copy)]
enum ConfiguredTargets {
    Scalar(bool),
    Array(bool),
}

impl ConfiguredTargets {
    fn contains_msvc(self) -> bool {
        match self {
            Self::Scalar(msvc) | Self::Array(msvc) => msvc,
        }
    }
}

fn merge_configured_targets(selected: &mut Option<ConfiguredTargets>, setting: ConfiguredTargets) {
    match setting {
        ConfiguredTargets::Scalar(msvc) => *selected = Some(ConfiguredTargets::Scalar(msvc)),
        ConfiguredTargets::Array(msvc) => {
            let merged = selected.is_some_and(|prior| prior.contains_msvc()) || msvc;
            *selected = Some(ConfiguredTargets::Array(merged));
        }
    }
}

fn toml_text_build_msvc_target(text: &str) -> Option<ConfiguredTargets> {
    let value: toml::Value = text.parse().ok()?;
    // Cargo's include graph can contribute build.target from another file.
    // Resolve conservatively here: retaining line tables for an include that
    // ultimately selects only non-MSVC is cheap; stripping them from an MSVC
    // target is the correctness failure soldr#2148 fixes.
    if value.get("include").is_some() {
        return Some(ConfiguredTargets::Array(true));
    }
    let target = value.get("build")?.get("target")?;
    if let Some(target) = target.as_str() {
        return Some(ConfiguredTargets::Scalar(is_msvc_target(target)));
    }
    target.as_array().map(|targets| {
        ConfiguredTargets::Array({
            targets
                .iter()
                .filter_map(toml::Value::as_str)
                .any(is_msvc_target)
        })
    })
}

fn cargo_profile_debug_is_specified(
    args: &[String],
    default: CargoProfileDebugDefault,
) -> Result<bool, SoldrError> {
    let profiles = default.lookup_profiles();
    if profiles.iter().any(|profile| {
        cargo_profile_debug_env_var(profile)
            .is_some_and(|env_var| std::env::var_os(env_var).is_some())
    }) {
        return Ok(true);
    }

    if cargo_config_args_specify_profile_debug(args, profiles)? {
        return Ok(true);
    }

    let cwd = std::env::current_dir()?;
    let manifest_start_dir = cargo_profile_lookup_start_dir_from(args, &cwd);
    if cargo_manifest_specifies_profile_debug(&manifest_start_dir, profiles) {
        return Ok(true);
    }
    if cargo_config_files_specify_profile_debug(&cwd, profiles) {
        return Ok(true);
    }

    Ok(false)
}

fn cargo_profile_debug_env_var(profile: &str) -> Option<&'static str> {
    match profile {
        "dev" => Some(CARGO_PROFILE_DEV_DEBUG_ENV_VAR),
        "test" => Some(CARGO_PROFILE_TEST_DEBUG_ENV_VAR),
        _ => None,
    }
}

fn cargo_args_contain_release(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--release")
}

fn cargo_profile_arg_value(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--profile" {
            return iter.next().cloned();
        }
        if let Some(value) = arg.strip_prefix("--profile=") {
            return Some(value.to_string());
        }
    }
    None
}

fn cargo_install_args_contain_debug(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--debug")
}

fn cargo_config_args_specify_profile_debug(
    args: &[String],
    profiles: &[&str],
) -> Result<bool, SoldrError> {
    let cwd = std::env::current_dir()?;
    for value in cargo_config_arg_values(args) {
        if cargo_config_arg_specifies_profile_debug(&value, &cwd, profiles) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn cargo_config_arg_values(args: &[String]) -> Vec<String> {
    let mut values = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--config" {
            if let Some(value) = iter.next() {
                values.push(value.clone());
            }
            continue;
        }
        if let Some(value) = arg.strip_prefix("--config=") {
            values.push(value.to_string());
        }
    }
    values
}

fn cargo_config_arg_specifies_profile_debug(
    value: &str,
    cwd: &std::path::Path,
    profiles: &[&str],
) -> bool {
    let raw = value.trim();
    if raw.is_empty() {
        return false;
    }

    let path = std::path::Path::new(raw);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    if path.is_file() {
        return toml_file_specifies_profile_debug(&path, profiles);
    }

    toml_text_specifies_profile_debug(raw, profiles)
        .unwrap_or_else(|| raw_may_specify_profile_debug(raw, profiles))
}

fn raw_may_specify_profile_debug(raw: &str, profiles: &[&str]) -> bool {
    let lowered = raw.to_ascii_lowercase();
    profiles.iter().any(|profile| {
        lowered.contains(&format!("profile.{profile}.debug"))
            || (lowered.contains(&format!("[profile.{profile}]")) && lowered.contains("debug"))
    })
}

fn cargo_manifest_specifies_profile_debug(start_dir: &std::path::Path, profiles: &[&str]) -> bool {
    find_workspace_manifest_path(start_dir)
        .is_some_and(|manifest| toml_file_specifies_profile_debug(&manifest, profiles))
}

fn cargo_config_files_specify_profile_debug(
    start_dir: &std::path::Path,
    profiles: &[&str],
) -> bool {
    cargo_config_paths(start_dir)
        .iter()
        .any(|path| toml_file_specifies_profile_debug(path, profiles))
}

fn cargo_config_paths(start_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths = BTreeSet::new();
    let mut current = Some(start_dir.to_path_buf());
    while let Some(dir) = current {
        for relative in [".cargo/config.toml", ".cargo/config"] {
            let path = dir.join(relative);
            if path.is_file() {
                paths.insert(path);
            }
        }
        current = dir.parent().map(std::path::Path::to_path_buf);
    }

    if let Some(cargo_home) = cargo_home_dir_for_config() {
        for name in ["config.toml", "config"] {
            let path = cargo_home.join(name);
            if path.is_file() {
                paths.insert(path);
            }
        }
    }

    paths.into_iter().collect()
}

fn cargo_home_dir_for_config() -> Option<std::path::PathBuf> {
    std::env::var_os(crate::core::CARGO_HOME_ENV_VAR)
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            crate::core::user_home_dir()
                .ok()
                .map(|home| home.join(".cargo"))
        })
}

fn toml_file_specifies_profile_debug(path: &std::path::Path, profiles: &[&str]) -> bool {
    match std::fs::read_to_string(path) {
        Ok(text) => toml_text_specifies_profile_debug(&text, profiles).unwrap_or(true),
        Err(_) => true,
    }
}

fn toml_text_specifies_profile_debug(text: &str, profiles: &[&str]) -> Option<bool> {
    let value: toml::Value = text.parse().ok()?;
    // An included config may explicitly choose this profile's debug policy.
    // Missing that choice would let Soldr's env injection override Cargo's
    // merged configuration, so includes conservatively suppress the default.
    if value.get("include").is_some() {
        return Some(true);
    }
    let Some(profile_table) = value.get("profile") else {
        return Some(false);
    };
    Some(profiles.iter().any(|profile| {
        profile_table
            .get(*profile)
            .and_then(|section| section.get("debug"))
            .is_some()
    }))
}

fn cargo_profile_lookup_start_dir(args: &[String]) -> Result<std::path::PathBuf, SoldrError> {
    let cwd = std::env::current_dir()?;
    Ok(cargo_profile_lookup_start_dir_from(args, &cwd))
}

fn cargo_profile_lookup_start_dir_from(
    args: &[String],
    cwd: &std::path::Path,
) -> std::path::PathBuf {
    let Some(manifest_path) = cargo_manifest_path_arg(args) else {
        return cwd.to_path_buf();
    };
    let manifest_path = if manifest_path.is_absolute() {
        manifest_path
    } else {
        cwd.join(manifest_path)
    };
    manifest_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| cwd.to_path_buf())
}

fn cargo_manifest_path_arg(args: &[String]) -> Option<std::path::PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--manifest-path" {
            return iter.next().map(std::path::PathBuf::from);
        }
        if let Some(value) = arg.strip_prefix("--manifest-path=") {
            return Some(std::path::PathBuf::from(value));
        }
    }
    None
}

fn find_workspace_manifest_path(start_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut current = start_dir.to_path_buf();
    let mut nearest_manifest = None;
    let mut workspace_manifest = None;

    loop {
        let candidate = current.join("Cargo.toml");
        if candidate.is_file() {
            if nearest_manifest.is_none() {
                nearest_manifest = Some(candidate.clone());
            }
            if cargo_manifest_declares_workspace(&candidate) {
                workspace_manifest = Some(candidate);
            }
        }
        if !current.pop() {
            break;
        }
    }

    workspace_manifest.or(nearest_manifest)
}

fn cargo_manifest_declares_workspace(path: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = text.parse::<toml::Value>() else {
        return false;
    };
    value.get("workspace").is_some()
}

pub(super) fn cargo_invocation_repo_path(args: &[String]) -> std::path::PathBuf {
    let start_dir = cargo_profile_lookup_start_dir(args)
        .or_else(|_| std::env::current_dir().map_err(SoldrError::from))
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    find_git_root(&start_dir)
        .or_else(|| {
            find_workspace_manifest_path(&start_dir)
                .and_then(|manifest| manifest.parent().map(std::path::Path::to_path_buf))
        })
        .unwrap_or(start_dir)
}

fn find_git_root(start_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut current = start_dir.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn cargo_debug_default_warning_decision(
    paths: &SoldrPaths,
    repo_path: &std::path::Path,
) -> WarningDecision {
    // soldr#1814 slice 2c: ask the daemon, which owns state_db's tables,
    // rather than making every front-door invocation another opener of
    // state.sqlite3. When it is unavailable, fail open: repeating a warning is
    // preferable to making the front door a second state-database opener.
    let sock = crate::daemon::client::default_sock_path(paths);
    match crate::daemon::client::should_warn_cargo_debug_default(&sock, repo_path) {
        Ok(true) => WarningDecision::Emit,
        Ok(false) => WarningDecision::Suppress,
        Err(_) => WarningDecision::EmitUnrecorded,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        cargo_config_build_targets, cargo_config_files_specify_profile_debug,
        cargo_invocation_targets_msvc, cargo_profile_debug_default_for_args,
        cargo_profile_lookup_start_dir_from, configured_targets_msvc,
        toml_text_specifies_profile_debug, ConfiguredTargets,
    };

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn msvc_test_default_retains_source_line_tables() {
        let default = cargo_profile_debug_default_for_args(&args(&["test"]), true)
            .expect("test profile should receive a debug default");

        assert_eq!(default.profile, "test");
        assert_eq!(default.env_var, "CARGO_PROFILE_TEST_DEBUG");
        assert_eq!(default.value, "line-tables-only");
    }

    #[test]
    fn non_msvc_default_stays_debug_free() {
        let default = cargo_profile_debug_default_for_args(&args(&["build"]), false)
            .expect("dev profile should receive a debug default");

        assert_eq!(default.profile, "dev");
        assert_eq!(default.env_var, "CARGO_PROFILE_DEV_DEBUG");
        assert_eq!(default.value, "false");
    }

    #[test]
    fn explicit_msvc_target_beats_known_environment_target() {
        assert!(cargo_invocation_targets_msvc(
            &args(&["build", "--target", "x86_64-pc-windows-msvc"]),
            Some("x86_64-unknown-linux-gnu"),
        )
        .expect("target resolution should succeed"));
    }

    #[test]
    fn command_line_config_can_select_msvc_target() {
        assert!(cargo_invocation_targets_msvc(
            &args(&[
                "build",
                "--config",
                "build.target='aarch64-pc-windows-msvc'",
            ]),
            None,
        )
        .expect("config target resolution should succeed"));
    }

    #[test]
    fn repeated_targets_retain_lines_if_any_target_is_msvc() {
        for targets in [
            ["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"],
            ["x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu"],
        ] {
            assert!(cargo_invocation_targets_msvc(
                &args(&["build", "--target", targets[0], "--target", targets[1],]),
                None,
            )
            .expect("target resolution should succeed"));
        }
    }

    #[test]
    fn repeated_config_target_arrays_are_merged() {
        assert!(cargo_invocation_targets_msvc(
            &args(&[
                "build",
                "--config",
                "build.target=['x86_64-unknown-linux-gnu']",
                "--config",
                "build.target=['aarch64-pc-windows-msvc']",
            ]),
            None,
        )
        .expect("config target resolution should succeed"));
    }

    #[test]
    fn later_scalar_config_target_overrides_earlier_scalar() {
        assert!(!cargo_invocation_targets_msvc(
            &args(&[
                "build",
                "--config",
                "build.target='aarch64-pc-windows-msvc'",
                "--config",
                "build.target='x86_64-unknown-linux-gnu'",
            ]),
            None,
        )
        .expect("config target resolution should succeed"));
    }

    #[test]
    fn command_line_array_merges_with_discovered_array() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cargo_dir = dir.path().join(".cargo");
        std::fs::create_dir(&cargo_dir).expect("create .cargo");
        std::fs::write(
            cargo_dir.join("config.toml"),
            "[build]\ntarget = ['aarch64-pc-windows-msvc']\n",
        )
        .expect("write config");

        assert!(configured_targets_msvc(
            &args(&[
                "build",
                "--config",
                "build.target=['x86_64-unknown-linux-gnu']",
            ]),
            None,
            dir.path(),
        )
        .expect("config target resolution should succeed"));
    }

    #[test]
    fn legacy_config_wins_when_both_config_names_exist() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cargo_dir = dir.path().join(".cargo");
        std::fs::create_dir(&cargo_dir).expect("create .cargo");

        std::fs::write(
            cargo_dir.join("config"),
            "[build]\ntarget = 'aarch64-pc-windows-msvc'\n",
        )
        .expect("write legacy config");
        std::fs::write(
            cargo_dir.join("config.toml"),
            "[build]\ntarget = 'x86_64-unknown-linux-gnu'\n",
        )
        .expect("write toml config");
        assert!(matches!(
            cargo_config_build_targets(dir.path()),
            Some(ConfiguredTargets::Scalar(true))
        ));

        std::fs::write(
            cargo_dir.join("config"),
            "[build]\ntarget = 'x86_64-unknown-linux-gnu'\n",
        )
        .expect("rewrite legacy config");
        std::fs::write(
            cargo_dir.join("config.toml"),
            "[build]\ntarget = 'aarch64-pc-windows-msvc'\n",
        )
        .expect("rewrite toml config");
        assert!(matches!(
            cargo_config_build_targets(dir.path()),
            Some(ConfiguredTargets::Scalar(false))
        ));
    }

    #[test]
    fn manifest_path_does_not_move_cargo_config_discovery() {
        let root = tempfile::tempdir().expect("temp dir");
        let caller = root.path().join("caller");
        let project = root.path().join("project");
        std::fs::create_dir_all(caller.join(".cargo")).expect("create caller config dir");
        std::fs::create_dir_all(project.join(".cargo")).expect("create project config dir");
        std::fs::write(
            caller.join(".cargo/config.toml"),
            "[profile.dev]\ndebug = false\n",
        )
        .expect("write caller config");

        let manifest_args = args(&["build", "--manifest-path", "../project/Cargo.toml"]);
        assert_eq!(
            cargo_profile_lookup_start_dir_from(&manifest_args, &caller)
                .canonicalize()
                .expect("canonical manifest parent"),
            project.canonicalize().expect("canonical project dir")
        );
        assert!(cargo_config_files_specify_profile_debug(&caller, &["dev"]));
        assert!(!cargo_config_files_specify_profile_debug(
            &project,
            &["dev"]
        ));

        std::fs::remove_file(caller.join(".cargo/config.toml")).expect("remove caller config");
        std::fs::write(
            project.join(".cargo/config.toml"),
            "[profile.dev]\ndebug = false\n",
        )
        .expect("write project config");
        assert!(!cargo_config_files_specify_profile_debug(&caller, &["dev"]));
        assert!(cargo_config_files_specify_profile_debug(&project, &["dev"]));
    }

    #[test]
    fn included_config_suppresses_default_for_either_debug_value() {
        for include in [
            "include = 'debug-false.toml'",
            "include = ['base.toml', 'debug-true.toml']",
        ] {
            assert_eq!(
                toml_text_specifies_profile_debug(include, &["dev"]),
                Some(true)
            );
        }
    }
}
