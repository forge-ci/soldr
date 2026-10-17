//! Rust standard-library targets required by cross-target Dylint checks.

use fs2::FileExt;
use std::path::Path;

use crate::core::SoldrError;
use crate::dylint_toolchain_readiness::{dylint_toolchain_readiness_at, DylintToolchainReadiness};

pub(crate) fn requested_targets(args: &[String]) -> Result<Vec<String>, SoldrError> {
    let Some(separator) = args.iter().position(|arg| arg == "--") else {
        return Ok(Vec::new());
    };
    parse_targets(&args[separator + 1..], false)
}

pub(crate) fn requested_targets_for_cargo(args: &[String]) -> Result<Vec<String>, SoldrError> {
    if crate::cargo_front_door::first_cargo_subcommand(args) == Some("dylint") {
        requested_targets(args)
    } else {
        Ok(Vec::new())
    }
}

pub(crate) fn prepare_targets(args: &[String]) -> Result<Vec<String>, SoldrError> {
    parse_targets(args, true)
}

fn parse_targets(args: &[String], strict: bool) -> Result<Vec<String>, SoldrError> {
    let mut targets = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            if strict {
                return Err(SoldrError::Other(
                    "soldr dylint prepare accepts only --target <triple>".into(),
                ));
            }
            break;
        }
        let target = if arg == "--target" {
            Some(iter.next().map(String::as_str).unwrap_or(""))
        } else {
            arg.strip_prefix("--target=")
        };
        if let Some(target) = target {
            if target.is_empty() || target.starts_with('-') {
                return Err(SoldrError::Other(
                    "Dylint --target requires a target triple".into(),
                ));
            }
            if !targets.iter().any(|existing| existing == target) {
                targets.push(target.to_string());
            }
        } else if strict {
            return Err(SoldrError::Other(format!(
                "soldr dylint prepare accepts only --target <triple> (got {arg})"
            )));
        }
    }
    Ok(targets)
}

pub(crate) fn ensure_targets(channel: &str, targets: &[String]) -> Result<(), SoldrError> {
    if targets.is_empty() {
        return Ok(());
    }
    // The Dylint child is pinned to this same home (soldr#3567).
    let home = crate::dylint_rustup_home::dylint_rustup_home()?;
    for target in targets {
        if target == crate::pyo3_detect::host_triple() {
            continue;
        }
        ensure_target_at(&home, channel, target, || {
            crate::toolchain::rustup_target_add(channel, target)
        })?;
    }
    Ok(())
}

fn target_std_installed(toolchain_dir: &Path, target: &str) -> bool {
    let lib_dir = toolchain_dir.join("lib/rustlib").join(target).join("lib");
    std::fs::read_dir(lib_dir).is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|entry| {
            entry.file_name().to_string_lossy().starts_with("libcore-")
                && entry.path().extension().is_some_and(|ext| ext == "rlib")
        })
    })
}

fn ensure_target_at<F>(
    home: &Path,
    channel: &str,
    target: &str,
    install: F,
) -> Result<(), SoldrError>
where
    F: FnOnce() -> Result<i32, SoldrError>,
{
    // The lock covers the probe as well as rustup's mutation. A second Soldr
    // process cannot treat a partially extracted libcore as a warm target.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(home.join(".soldr-dylint-targets.lock"))?;
    crate::platform::wait::lock_exclusive_announced(
        &lock,
        crate::platform::wait::LONG_LOCK,
        "the dylint target install",
    )?;
    let result = ensure_target_locked(home, channel, target, install);
    FileExt::unlock(&lock)?;
    result
}

fn ensure_target_locked<F>(
    home: &Path,
    channel: &str,
    target: &str,
    install: F,
) -> Result<(), SoldrError>
where
    F: FnOnce() -> Result<i32, SoldrError>,
{
    let toolchain_dir = match dylint_toolchain_readiness_at(home, channel) {
        DylintToolchainReadiness::Ready { directory, .. } => directory,
        other => {
            return Err(SoldrError::Other(format!(
                "Dylint nightly {channel} is not ready for target {target}: {other:?}"
            )))
        }
    };
    if target_std_installed(&toolchain_dir, target) {
        return Ok(());
    }
    eprintln!("soldr: installing Dylint rust-std for {target} on {channel}");
    let code = install()?;
    if code != 0 {
        return Err(SoldrError::Other(format!(
            "rustup failed to install rust-std for Dylint target {target} on {channel} (exit {code})"
        )));
    }
    if !target_std_installed(&toolchain_dir, target) {
        return Err(SoldrError::Other(format!(
            "rustup reported success but rust-std for Dylint target {target} is missing from {} ({channel})",
            toolchain_dir.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_both_cross_target_forms_without_touching_host_only_runs() {
        assert!(
            requested_targets(&args(&["dylint", "--all", "--", "--workspace"]))
                .expect("host arguments")
                .is_empty()
        );
        assert_eq!(
            requested_targets(&args(&[
                "dylint",
                "--all",
                "--",
                "--target",
                "x86_64-pc-windows-msvc",
                "--target=x86_64-apple-darwin",
                "--target=x86_64-pc-windows-msvc"
            ]))
            .expect("target arguments"),
            ["x86_64-pc-windows-msvc", "x86_64-apple-darwin"]
        );
        assert!(requested_targets(&args(&["dylint", "--", "--target="])).is_err());
        assert_eq!(
            prepare_targets(&args(&["--target=x86_64-apple-darwin"])).unwrap(),
            ["x86_64-apple-darwin"]
        );
        assert_eq!(
            prepare_targets(&args(&["--target", "x86_64-pc-windows-msvc"])).unwrap(),
            ["x86_64-pc-windows-msvc"]
        );
        assert!(prepare_targets(&args(&["--", "ignored"])).is_err());
    }

    #[test]
    fn installs_only_missing_std_in_the_selected_nightly_home() {
        let home = tempfile::tempdir().unwrap();
        let channel = "nightly-2026-05-28";
        let target = "x86_64-pc-windows-msvc";
        let toolchain = home
            .path()
            .join("toolchains")
            .join(format!("{channel}-stub-host"));
        std::fs::create_dir_all(toolchain.join("bin")).unwrap();
        std::fs::create_dir_all(toolchain.join("lib/rustlib")).unwrap();
        std::fs::write(
            toolchain.join("lib/rustlib/multirust-channel-manifest.toml"),
            "",
        )
        .unwrap();
        std::fs::write(
            toolchain
                .join("bin")
                .join(crate::platform::executable::name::native("rustc")),
            "",
        )
        .unwrap();
        let expected = toolchain.join("lib/rustlib").join(target).join("lib");
        ensure_target_at(home.path(), channel, target, || {
            std::fs::create_dir_all(&expected).unwrap();
            std::fs::write(expected.join("libcore-test.rlib"), "").unwrap();
            Ok(0)
        })
        .unwrap();
        ensure_target_at(home.path(), channel, target, || {
            panic!("warm target must not reinstall")
        })
        .unwrap();
        assert!(!target_std_installed(&toolchain, "x86_64-apple-darwin"));
    }

    #[test]
    fn unavailable_target_fails_before_cargo() {
        let home = tempfile::tempdir().unwrap();
        let channel = "nightly-2026-05-28";
        let target = "x86_64-apple-darwin";
        let toolchain = home
            .path()
            .join("toolchains")
            .join(format!("{channel}-stub-host"));
        std::fs::create_dir_all(toolchain.join("bin")).unwrap();
        std::fs::create_dir_all(toolchain.join("lib/rustlib")).unwrap();
        std::fs::write(
            toolchain.join("lib/rustlib/multirust-channel-manifest.toml"),
            "",
        )
        .unwrap();
        std::fs::write(
            toolchain
                .join("bin")
                .join(crate::platform::executable::name::native("rustc")),
            "",
        )
        .unwrap();
        let error = ensure_target_at(home.path(), channel, target, || Ok(1)).unwrap_err();
        assert!(error.to_string().contains(channel));
        assert!(error.to_string().contains(target));
    }
}
