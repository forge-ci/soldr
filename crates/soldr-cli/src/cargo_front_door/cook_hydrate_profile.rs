//! Which profile a cook's `cargo chef prepare` hydrates (soldr#4109).
//!
//! `prepare` takes no profile flag, so with only the argv to go on the hydrate
//! pre-flight resolves `dev` and restores a dev archive into a `--release` or
//! `--profile <name>` cook, which reads none of it. `soldr cook` publishes its
//! profile here first, the same way `SOLDR_COOK_HYDRATE_TARGET` carries its
//! target (soldr#3043).
//!
//! Only `cargo chef prepare` reads the value. A later cargo run keeps resolving
//! its own profile, so a dev build after a release cook still hydrates the dev
//! archive for itself.

use super::cook_hydrate::resolve_profile_name;
use super::subcommand::first_cargo_subcommand_index;

/// Never set by a user; `soldr cook` sets it for itself.
const SOLDR_COOK_HYDRATE_PROFILE_ENV: &str = "SOLDR_COOK_HYDRATE_PROFILE";

/// Called by `soldr cook` before any phase.
pub(crate) fn publish_cook_profile(profile: &str) {
    std::env::set_var(SOLDR_COOK_HYDRATE_PROFILE_ENV, profile);
}

/// The profile whose archive this front-door run should hydrate: the enclosing
/// cook's for its `cargo chef prepare`, otherwise the one the argv names.
pub(super) fn hydrate_profile_name(args: &[String]) -> String {
    let is_chef_prepare = first_cargo_subcommand_index(args).is_some_and(|idx| {
        args[idx] == "chef" && args.get(idx + 1).is_some_and(|arg| arg == "prepare")
    });
    if is_chef_prepare {
        if let Ok(profile) = std::env::var(SOLDR_COOK_HYDRATE_PROFILE_ENV) {
            if !profile.trim().is_empty() {
                return profile.trim().to_string();
            }
        }
    }
    resolve_profile_name(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    /// `soldr cook --release` must not make its `cargo chef prepare` look up,
    /// and restore, the dev archive. All one test: it owns this variable.
    #[test]
    fn chef_prepare_hydrates_the_profile_of_the_enclosing_cook() {
        let prepare = argv(&["chef", "prepare", "--recipe-path", "/tmp/recipe.json"]);

        // Outside a cook: dev, as before.
        std::env::remove_var(SOLDR_COOK_HYDRATE_PROFILE_ENV);
        assert_eq!(hydrate_profile_name(&prepare), "dev");

        publish_cook_profile("dev");
        assert_eq!(hydrate_profile_name(&prepare), "dev");
        publish_cook_profile("release");
        assert_eq!(hydrate_profile_name(&prepare), "release");
        publish_cook_profile("ci");
        assert_eq!(hydrate_profile_name(&prepare), "ci");

        // Every other invocation keeps resolving its own profile.
        publish_cook_profile("release");
        assert_eq!(hydrate_profile_name(&argv(&["build"])), "dev");
        assert_eq!(
            hydrate_profile_name(&argv(&["chef", "cook", "--profile", "ci"])),
            "ci"
        );

        publish_cook_profile("  ");
        assert_eq!(hydrate_profile_name(&prepare), "dev");
        std::env::remove_var(SOLDR_COOK_HYDRATE_PROFILE_ENV);
    }
}
