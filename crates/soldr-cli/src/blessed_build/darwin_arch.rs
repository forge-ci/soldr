//! The per-architecture parameters of a blessed Darwin cross-build:
//! the clang target triple, and the macOS deployment target.
//!
//! They belong together because the second one being *absent* from this
//! set is the whole bug below — the clang triple branched on arch and
//! the deployment target did not.
//!
//! This value used to be the literal `11.0`, written five times in the
//! Darwin arm of `blessed_build` with no branch on architecture. That
//! is how `x86_64-apple-darwin` ended up demanding macOS 11.0 — which
//! excludes every Intel Mac on 10.13–10.15 — while the `crgx` and
//! `cargo-chef` binaries shipped in the same bundle sat at 10.12, so
//! the x86_64 bundle was not even internally consistent (soldr#2146,
//! item 2 of soldr#1060).
//!
//! It was never an SDK default that nobody pinned. `MACOSX_DEPLOYMENT_TARGET`
//! appears throughout the tree only as a passthrough or a comment; the
//! effective value came from these explicit `-mmacosx-version-min` flags,
//! which is also why setting that env var had no effect — the flag won.
//! Setting it now does what the ratchet script has always claimed it does.

/// Apple Silicon shipped with Big Sur, so there is nothing below 11.0
/// to support. Not negotiable.
const AARCH64_MIN_OS: &str = "11.0";

/// Rust's own default for `x86_64-apple-darwin`, and the floor the
/// prebuilt tools in the same bundle already use.
const X86_64_MIN_OS: &str = "10.12";

/// The lane-level override. Honored so the ratchet script's advice
/// ("set `MACOSX_DEPLOYMENT_TARGET` for the lane rather than raising
/// the ceiling") is finally true.
pub(super) const DEPLOYMENT_TARGET_ENV_VAR: &str = "MACOSX_DEPLOYMENT_TARGET";

/// The `--target=` value to hand clang for `target_triple`.
pub(crate) fn clang_target(target_triple: &str) -> &'static str {
    if target_triple.starts_with("aarch64") {
        "arm64-apple-darwin"
    } else {
        "x86_64-apple-darwin"
    }
}

/// The `-mmacosx-version-min` value for `target_triple`.
pub(crate) fn deployment_target(target_triple: &str) -> String {
    let default = default_for(target_triple);
    match std::env::var(DEPLOYMENT_TARGET_ENV_VAR) {
        Ok(value) => resolve_override(&value, default),
        Err(_) => default.to_string(),
    }
}

fn default_for(target_triple: &str) -> &'static str {
    if target_triple.starts_with("aarch64") {
        AARCH64_MIN_OS
    } else {
        X86_64_MIN_OS
    }
}

/// Apply an override, rejecting anything that is not a version number.
///
/// A malformed value is worth a warning rather than a hard failure: it
/// would otherwise reach clang as `-mmacosx-version-min=<garbage>` and
/// fail deep inside a `*-sys` crate's configure step, where the cause is
/// far less obvious than it is here.
fn resolve_override(value: &str, default: &'static str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return default.to_string();
    }
    if !is_version_like(trimmed) {
        eprintln!(
            "soldr build: ignoring {DEPLOYMENT_TARGET_ENV_VAR}={value:?}: \
             not a version number; using {default}"
        );
        return default.to_string();
    }
    trimmed.to_string()
}

/// Digits and dots, at least one digit, no leading/trailing/doubled dot.
fn is_version_like(value: &str) -> bool {
    let mut parts = value.split('.');
    parts.all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_arch_gets_its_own_floor() {
        assert_eq!(default_for("aarch64-apple-darwin"), "11.0");
        assert_eq!(default_for("x86_64-apple-darwin"), "10.12");
    }

    #[test]
    fn both_per_arch_values_branch_on_the_same_input() {
        // The bug was that only one of these two branched. Asserting
        // them side by side is what makes a future divergence obvious.
        assert_eq!(clang_target("aarch64-apple-darwin"), "arm64-apple-darwin");
        assert_eq!(clang_target("x86_64-apple-darwin"), "x86_64-apple-darwin");
        assert_ne!(
            default_for("aarch64-apple-darwin"),
            default_for("x86_64-apple-darwin"),
            "the deployment target must differ per arch, as the clang triple does"
        );
    }

    #[test]
    fn an_override_wins_over_the_arch_default() {
        assert_eq!(resolve_override("10.15", "10.12"), "10.15");
        assert_eq!(resolve_override("  11.0  ", "10.12"), "11.0");
        assert_eq!(resolve_override("13", "10.12"), "13");
    }

    #[test]
    fn a_malformed_override_falls_back_instead_of_reaching_clang() {
        // The failure mode this guards against is a `*-sys` configure
        // step dying on `-mmacosx-version-min=latest`, where nothing
        // points back at the env var.
        assert_eq!(resolve_override("latest", "10.12"), "10.12");
        assert_eq!(resolve_override("10.15.x", "10.12"), "10.12");
        assert_eq!(resolve_override("10..15", "10.12"), "10.12");
        assert_eq!(resolve_override(".10", "10.12"), "10.12");
        assert_eq!(resolve_override("10.", "10.12"), "10.12");
    }

    #[test]
    fn an_empty_override_is_treated_as_unset() {
        assert_eq!(resolve_override("", "11.0"), "11.0");
        assert_eq!(resolve_override("   ", "11.0"), "11.0");
    }
}
