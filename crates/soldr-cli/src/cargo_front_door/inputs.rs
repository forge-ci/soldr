//! Hashing inputs that `rust_plan` and zccache-rust-plan feed into the
//! build manifest:
//! - cargo profile / target / feature selection from argv
//!   ([`cargo_profile`], [`cargo_target_triple`], [`cargo_feature_inputs`])
//! - environment-variable snapshots
//!   ([`rustflags_inputs`], [`build_env_inputs`])
//! - workspace manifest and cargo-config file hashes
//!   ([`workspace_manifest_hashes`], [`cargo_config_hash`])
//! - small hashing / path helpers
//!   ([`file_hash_or_missing`], [`stable_hash_json`], [`path_string`]).

use crate::core::SoldrError;
use kernal_api::hash::Sha256Hasher;
use serde::Serialize;

use super::profile_debug::CargoProfileDebugDefault;
use super::subcommand::cargo_args_target_value;

pub(crate) fn cargo_profile(args: &[String]) -> &str {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--release" {
            return "release";
        }
        if arg == "--profile" {
            return iter.next().map(String::as_str).unwrap_or("debug");
        }
        if let Some(value) = arg.strip_prefix("--profile=") {
            return value;
        }
    }
    "debug"
}

pub(crate) fn cargo_target_triple(args: &[String], host: &str) -> String {
    cargo_args_target_value(args)
        .or_else(|| std::env::var("CARGO_BUILD_TARGET").ok())
        .unwrap_or_else(|| host.to_string())
}

pub(crate) fn cargo_feature_inputs(args: &[String]) -> Vec<String> {
    selected_cargo_args(
        args,
        &[
            "--features",
            "--all-features",
            "--no-default-features",
            "--package",
            "-p",
            "--workspace",
            "--exclude",
            "--all-targets",
            "--lib",
            "--bins",
            "--bin",
            "--examples",
            "--example",
            "--tests",
            "--test",
            "--benches",
            "--bench",
        ],
    )
}

pub(crate) fn selected_cargo_args(args: &[String], names: &[&str]) -> Vec<String> {
    let mut selected = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if names.contains(&arg.as_str()) {
            selected.push(arg.clone());
            if !matches!(
                arg.as_str(),
                "--all-features"
                    | "--no-default-features"
                    | "--workspace"
                    | "--all-targets"
                    | "--lib"
                    | "--bins"
                    | "--examples"
                    | "--tests"
                    | "--benches"
            ) {
                if let Some(value) = iter.next() {
                    selected.push(value.clone());
                }
            }
            continue;
        }
        if names
            .iter()
            .any(|name| arg.starts_with(&format!("{name}=")))
        {
            selected.push(arg.clone());
        }
    }
    selected
}

pub(crate) fn rustflags_inputs() -> Vec<(String, String)> {
    sorted_env_vars(|name| {
        name == "RUSTFLAGS"
            || name == "CARGO_ENCODED_RUSTFLAGS"
            || (name.starts_with("CARGO_TARGET_") && name.ends_with("_RUSTFLAGS"))
    })
}

pub(crate) fn build_env_inputs(
    cargo_profile_debug_default: Option<&CargoProfileDebugDefault>,
) -> Vec<(String, String)> {
    let mut vars = sorted_env_vars(|name| {
        name == "CARGO_BUILD_TARGET"
            || name == "CARGO_TARGET_DIR"
            || (name.starts_with("CARGO_TARGET_") && name.ends_with("_LINKER"))
            || matches!(name, "CC" | "CXX" | "AR" | "RANLIB" | "LD")
            || ["CC_", "CXX_", "AR_", "RANLIB_", "LD_"]
                .iter()
                .any(|prefix| name.starts_with(prefix))
            || name.starts_with("CARGO_PROFILE_")
            || name.starts_with("CARGO_CFG_")
    });
    if let Some(default) = cargo_profile_debug_default {
        if !vars.iter().any(|(name, _)| name == default.env_var) {
            vars.push((default.env_var.to_string(), default.value.to_string()));
        }
        vars.sort_by(|a, b| a.0.cmp(&b.0));
    }
    vars
}

pub(super) fn sorted_env_vars<F>(include: F) -> Vec<(String, String)>
where
    F: Fn(&str) -> bool,
{
    let mut vars = std::env::vars()
        .filter(|(name, _)| include(name))
        .collect::<Vec<_>>();
    vars.sort_by(|a, b| a.0.cmp(&b.0));
    vars
}

pub(crate) fn workspace_manifest_hashes(
    workspace_root: &std::path::Path,
) -> Result<Vec<String>, SoldrError> {
    let mut hashes = Vec::new();
    collect_manifest_hashes(workspace_root, workspace_root, &mut hashes)?;
    hashes.sort();
    Ok(hashes)
}

fn collect_manifest_hashes(
    workspace_root: &std::path::Path,
    dir: &std::path::Path,
    hashes: &mut Vec<String>,
) -> Result<(), SoldrError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if matches!(
                entry.file_name().to_str(),
                Some(".git" | "target" | ".soldr" | "node_modules")
            ) {
                continue;
            }
            collect_manifest_hashes(workspace_root, &path, hashes)?;
        } else if file_type.is_file() && entry.file_name() == std::ffi::OsStr::new("Cargo.toml") {
            let relative = path
                .strip_prefix(workspace_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            hashes.push(format!("{relative}:{}", file_hash_or_missing(&path)?));
        }
    }
    Ok(())
}

pub(crate) fn cargo_config_hash(workspace_root: &std::path::Path) -> Result<String, SoldrError> {
    let mut inputs = Vec::new();
    for relative in [".cargo/config.toml", ".cargo/config"] {
        let path = workspace_root.join(relative);
        if path.exists() {
            inputs.push(format!("{relative}:{}", file_hash_or_missing(&path)?));
        }
    }
    Ok(stable_hash_json(&inputs))
}

pub(crate) fn file_hash_or_missing(path: &std::path::Path) -> Result<String, SoldrError> {
    if !path.exists() {
        return Ok("missing".to_string());
    }
    Ok(sha256_bytes(&std::fs::read(path)?))
}

pub(crate) fn stable_hash_json<T: Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    sha256_bytes(&bytes)
}

pub(crate) fn sha256_bytes(bytes: &[u8]) -> String {
    Sha256Hasher::digest(bytes).to_hex()
}

pub(crate) fn path_string(path: &std::path::Path) -> String {
    path.display().to_string()
}
