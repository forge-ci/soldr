//! Local-only JIT acceleration: Rustc and Soldr's CI gates remain authoritative.

use std::ffi::OsStr;
use std::process::Command;

use soldr_core::core::env_flag;

use crate::cli_args::Commands;
use crate::core::SoldrError;

pub fn refusal(ci: bool, github_actions: bool, allow_ci: bool) -> Option<&'static str> {
    if (ci || github_actions) && !allow_ci {
        Some("soldr blaze refuses to run in CI: blaze's borrow checker finds about two thirds of rustc's borrow errors and bypasses the pinned toolchain; cargo and CI remain the authority. Use soldr blaze --allow-ci <args> to explicitly override this refusal.")
    } else {
        None
    }
}

pub fn env_routing(enabled: bool, verb: &str) -> bool {
    enabled && matches!(verb, "build" | "run" | "test" | "bench" | "check")
}

pub(crate) fn route_command(command: Commands) -> Commands {
    let enabled = env_flag::flag("SOLDR_BLAZE");
    match command {
        Commands::Build { mut args } if env_routing(enabled, "build") => {
            args.insert(0, "build".to_owned());
            Commands::Blaze {
                args,
                allow_ci: false,
            }
        }
        Commands::Cargo { args } if args.first().is_some_and(|verb| env_routing(enabled, verb)) => {
            Commands::Blaze {
                args,
                allow_ci: false,
            }
        }
        other => other,
    }
}

fn scrubbed_variable(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    matches!(
        name.as_ref(),
        "RUSTC_WRAPPER"
            | "RUSTC_WORKSPACE_WRAPPER"
            | "CARGO_BUILD_RUSTC_WRAPPER"
            | "SOLDR_RUSTC_WRAPPER"
            | "SOLDR_LINKER"
    ) || name.starts_with("ZCCACHE_")
        || name.starts_with("SOLDR_CACHE_")
}

pub fn run(args: &[String], allow_ci: bool) -> Result<i32, SoldrError> {
    if let Some(message) = refusal(
        env_flag::foreign_flag("CI"),
        env_flag::foreign_flag("GITHUB_ACTIONS"),
        allow_ci,
    ) {
        return Err(SoldrError::Other(message.to_owned()));
    }

    let binary = std::env::var_os("SOLDR_BLAZE_BIN")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "blaze".into());
    let path = std::env::var_os("PATH").unwrap_or_default();
    let binary = crate::exec_cmd::find_on_path(&binary.to_string_lossy(), &path)
        .ok_or_else(|| SoldrError::Other(install_message()))?;
    eprintln!("soldr: note: blaze bypasses the pinned toolchain and its borrow checker is partial; cargo and CI remain the authority on whether this compiles.");
    let mut command = Command::new(binary);
    command.args(args);
    for (name, _) in std::env::vars_os() {
        if scrubbed_variable(&name) {
            command.env_remove(name);
        }
    }
    let status = command.status().map_err(|error| {
        SoldrError::Other(format!(
            "failed to execute blaze: {error}. {}",
            install_message()
        ))
    })?;
    Ok(status.code().unwrap_or(1))
}

fn install_message() -> String {
    "blaze executable not available: install with `curl -fsSL https://ix.dev/blaze | sh`; Windows installs from crates.io with no installer. Set SOLDR_BLAZE_BIN to override the executable.".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blaze_refusal_matrix() {
        for ci in [false, true] {
            for github_actions in [false, true] {
                for allow_ci in [false, true] {
                    let result = refusal(ci, github_actions, allow_ci);
                    assert_eq!(result.is_some(), (ci || github_actions) && !allow_ci);
                    if let Some(message) = result {
                        for reason in ["two thirds", "pinned toolchain", "--allow-ci"] {
                            assert!(message.contains(reason));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn blaze_env_routing_is_opt_in_and_dev_verbs_only() {
        for verb in ["build", "run", "test", "bench", "check"] {
            assert!(env_routing(true, verb));
            assert!(!env_routing(false, verb));
        }
        for verb in ["", "clippy", "nextest", "publish", "ci-test", "--version"] {
            assert!(!env_routing(true, verb));
        }
    }

    #[test]
    fn blaze_scrubs_interception_but_preserves_user_settings() {
        for name in [
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "CARGO_BUILD_RUSTC_WRAPPER",
            "SOLDR_RUSTC_WRAPPER",
            "SOLDR_LINKER",
            "ZCCACHE_DISABLE",
            "SOLDR_CACHE_DIR",
        ] {
            assert!(scrubbed_variable(OsStr::new(name)), "{name}");
        }
        for name in ["BLAZE_FLAGS", "PATH", "SOLDR_BLAZE_BIN", "CARGO_HOME"] {
            assert!(!scrubbed_variable(OsStr::new(name)), "{name}");
        }
    }
}
