//! Automatic codegen-unit selection for child Cargo (soldr#4014).
//!
//! The workspace manifest no longer pins `codegen-units = 1` in any profile.
//! A pin was a workaround for the amalgamated zccache crate fanning out into
//! many LLVM threads; that fan-out is now bounded by admission (zccache's
//! built-in exclusive classifier for its own amalgamated crate, plus the
//! codegen-thread term in `soldr-daemon`'s memory estimate), so ordinary
//! builds get Cargo's default.
//!
//! Single-unit codegen is selected per build intent instead:
//!
//! * `SOLDR_CODEGEN_UNITS=<n>` is the explicit opt-in. It sets
//!   `CARGO_PROFILE_<PROFILE>_CODEGEN_UNITS=<n>` for the profile this Cargo
//!   invocation selects.
//! * A shipping build -- `SOLDR_RELEASE_CI=1` (set only by the publishing
//!   lane) with the `release` profile -- gets one unit, so the published
//!   binary keeps its size without a manifest pin.
//! * Everything else gets Cargo's default.
//!
//! A caller-set `CARGO_PROFILE_<PROFILE>_CODEGEN_UNITS` always wins.

pub(super) const SOLDR_CODEGEN_UNITS_ENV: &str = "SOLDR_CODEGEN_UNITS";
pub(super) const SOLDR_RELEASE_CI_ENV: &str = "SOLDR_RELEASE_CI";

/// Why a codegen-unit count was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reason {
    /// `SOLDR_CODEGEN_UNITS` asked for it.
    Explicit,
    /// A publishing (`SOLDR_RELEASE_CI=1`) release build.
    Shipping,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Selection {
    pub(super) env_var: String,
    pub(super) units: u32,
    pub(super) reason: Reason,
}

/// Apply the selection to `command`. Returns what was injected, if anything.
pub(super) fn apply(command: &mut std::process::Command, args: &[String]) -> Option<Selection> {
    let selection = select(args, |key| std::env::var(key).ok());
    if let Some(selection) = &selection {
        command.env(&selection.env_var, selection.units.to_string());
    }
    selection
}

/// Pure selection over the Cargo args and an environment lookup.
pub(super) fn select(args: &[String], env: impl Fn(&str) -> Option<String>) -> Option<Selection> {
    let profile = cargo_profile(args)?;
    let env_var = profile_codegen_units_env(&profile);
    if env(&env_var).is_some_and(|value| !value.trim().is_empty()) {
        return None;
    }
    if let Some(raw) = env(SOLDR_CODEGEN_UNITS_ENV).filter(|value| !value.trim().is_empty()) {
        return match raw.trim().parse::<u32>() {
            Ok(units) if units >= 1 => Some(Selection {
                env_var,
                units,
                reason: Reason::Explicit,
            }),
            _ => {
                eprintln!(
                    "soldr: warning: ignoring {SOLDR_CODEGEN_UNITS_ENV}={raw:?}; expected a positive integer"
                );
                None
            }
        };
    }
    if profile == "release" && env(SOLDR_RELEASE_CI_ENV).as_deref() == Some("1") {
        return Some(Selection {
            env_var,
            units: 1,
            reason: Reason::Shipping,
        });
    }
    None
}

/// `CARGO_PROFILE_<NAME>_CODEGEN_UNITS`, using Cargo's env-config spelling.
pub(super) fn profile_codegen_units_env(profile: &str) -> String {
    format!(
        "CARGO_PROFILE_{}_CODEGEN_UNITS",
        profile.to_ascii_uppercase().replace('-', "_")
    )
}

/// The profile a Cargo invocation compiles with, or `None` when the
/// subcommand does not compile (or cannot be told from its args).
fn cargo_profile(args: &[String]) -> Option<String> {
    let cargo_args: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .take_while(|arg| *arg != "--")
        .collect();
    let subcommand = cargo_args
        .iter()
        .copied()
        .find(|arg| !arg.starts_with('-') && !arg.starts_with('+'))?;
    let mut iter = cargo_args.iter().copied();
    while let Some(arg) = iter.next() {
        if arg == "--profile" {
            return iter.next().map(str::to_string);
        }
        if let Some(value) = arg.strip_prefix("--profile=") {
            return Some(value.to_string());
        }
    }
    let release = cargo_args.contains(&"--release");
    let profile = match subcommand {
        _ if release => "release",
        "install" if !cargo_args.contains(&"--debug") => "release",
        "bench" => "bench",
        "t" | "test" | "nextest" => "test",
        "b" | "build" | "c" | "check" | "r" | "run" | "rustc" | "clippy" | "fix" | "d" | "doc"
        | "install" => "dev",
        _ => return None,
    };
    Some(profile.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    fn select_with(list: &[&str], env: &[(&str, &str)]) -> Option<Selection> {
        let env: HashMap<String, String> = env
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        select(&args(list), |key| env.get(key).cloned())
    }

    #[test]
    fn ordinary_builds_keep_cargos_default() {
        assert_eq!(select_with(&["build"], &[]), None);
        assert_eq!(select_with(&["build", "--release"], &[]), None);
        assert_eq!(select_with(&["test", "--workspace"], &[]), None);
    }

    #[test]
    fn publishing_release_build_gets_one_unit() {
        let selection = select_with(&["build", "--release"], &[("SOLDR_RELEASE_CI", "1")])
            .expect("shipping build selects one unit");
        assert_eq!(selection.env_var, "CARGO_PROFILE_RELEASE_CODEGEN_UNITS");
        assert_eq!(selection.units, 1);
        assert_eq!(selection.reason, Reason::Shipping);
        let profile = select_with(
            &["build", "--profile=release", "-p", "soldr-cli"],
            &[("SOLDR_RELEASE_CI", "1")],
        );
        assert_eq!(profile.map(|selection| selection.units), Some(1));
    }

    #[test]
    fn publishing_lane_leaves_non_shipping_profiles_alone() {
        for list in [
            &["build"][..],
            &["build", "--profile", "ci-release"][..],
            &["test"][..],
        ] {
            assert_eq!(select_with(list, &[("SOLDR_RELEASE_CI", "1")]), None);
        }
    }

    #[test]
    fn explicit_opt_in_targets_the_selected_profile() {
        let cases = [
            (&["build"][..], "CARGO_PROFILE_DEV_CODEGEN_UNITS"),
            (&["test"][..], "CARGO_PROFILE_TEST_CODEGEN_UNITS"),
            (&["nextest", "run"][..], "CARGO_PROFILE_TEST_CODEGEN_UNITS"),
            (
                &["build", "--release"][..],
                "CARGO_PROFILE_RELEASE_CODEGEN_UNITS",
            ),
            (
                &["build", "--profile", "ci-nextest"][..],
                "CARGO_PROFILE_CI_NEXTEST_CODEGEN_UNITS",
            ),
        ];
        for (list, env_var) in cases {
            let selection = select_with(list, &[("SOLDR_CODEGEN_UNITS", "4")])
                .unwrap_or_else(|| panic!("{list:?} selects explicit units"));
            assert_eq!(selection.env_var, env_var);
            assert_eq!(selection.units, 4);
            assert_eq!(selection.reason, Reason::Explicit);
        }
    }

    #[test]
    fn caller_set_cargo_profile_env_wins() {
        assert_eq!(
            select_with(
                &["build", "--release"],
                &[
                    ("SOLDR_RELEASE_CI", "1"),
                    ("SOLDR_CODEGEN_UNITS", "1"),
                    ("CARGO_PROFILE_RELEASE_CODEGEN_UNITS", "16"),
                ],
            ),
            None
        );
    }

    #[test]
    fn invalid_opt_in_is_ignored() {
        assert_eq!(
            select_with(&["build"], &[("SOLDR_CODEGEN_UNITS", "0")]),
            None
        );
        assert_eq!(
            select_with(&["build"], &[("SOLDR_CODEGEN_UNITS", "many")]),
            None
        );
    }

    #[test]
    fn non_compiling_subcommands_are_untouched() {
        assert_eq!(
            select_with(&["metadata"], &[("SOLDR_CODEGEN_UNITS", "1")]),
            None
        );
        assert_eq!(select_with(&["fmt"], &[("SOLDR_RELEASE_CI", "1")]), None);
    }
}
