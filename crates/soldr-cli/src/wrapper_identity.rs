//! Soldr-owned effective-wrapper identity mirror (soldr#2545).
//!
//! Cargo fingerprints the `RUSTC_WRAPPER` executable path; changing it
//! between invocations silently invalidates every otherwise-warm artifact
//! and turns a warm build into a full-workspace recompile with no error.
//! The observed failure was two Soldr generations driving one shared
//! `target/`: each wrote a different versioned shim path and the "hang"
//! before tests was cargo rebuilding the world, twice.
//!
//! `SOLDR_RUSTC_WRAPPER` stays the public policy *input* and is untouched.
//! This module owns a private exact mirror of the *resolved* value: every
//! boundary where Soldr sets or clears `RUSTC_WRAPPER` on a child does it
//! through these helpers so the pair can never drift within Soldr-owned
//! lineage, and the wrapper re-entry asserts the inherited pair still
//! matches before any broker/daemon contact. Caller-owned wrappers are
//! deliberately not mirrored: Soldr asserts only what Soldr owns.
//!
//! The one sanctioned mid-build mutation is cargo-llvm-cov's documented
//! chained-wrapper protocol (soldr#3504): it points `RUSTC_WRAPPER` at
//! itself, stashes the wrapper it displaced in
//! `__CARGO_LLVM_COV_RUSTC_WRAPPER_PRE_EXISTING`, and execs that wrapper
//! as a chain. The guard recognizes that chain when it resolves back to
//! the mirror instead of calling it drift.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::core::SoldrError;

/// Private exact mirror of the effective `RUSTC_WRAPPER` Soldr resolved.
pub const EFFECTIVE_WRAPPER_ENV: &str = "SOLDR_EFFECTIVE_RUSTC_WRAPPER";

/// Which Soldr path produced the effective wrapper value.
pub const EFFECTIVE_WRAPPER_ORIGIN_ENV: &str = "SOLDR_EFFECTIVE_RUSTC_WRAPPER_ORIGIN";

/// cargo-llvm-cov's stash for the `RUSTC_WRAPPER` it displaced before
/// pointing the variable at itself (`src/wrapper.rs` `ENV_PRE_EXISTING`;
/// soldr#3504).
pub const LLVM_COV_PRE_EXISTING_ENV: &str = "__CARGO_LLVM_COV_RUSTC_WRAPPER_PRE_EXISTING";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapperOrigin {
    /// The managed compiler-named multicall shim (the normal cached path).
    SoldrManaged,
    /// A caller-supplied `SOLDR_RUSTC_WRAPPER` override Soldr applied.
    CustomOverride,
    /// The build-from-source cache shim.
    SourceBuild,
    /// Soldr explicitly cleared the wrapper (caching disabled).
    Disabled,
}

impl WrapperOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SoldrManaged => "soldr-managed",
            Self::CustomOverride => "custom-override",
            Self::SourceBuild => "source-build",
            Self::Disabled => "disabled",
        }
    }
}

/// Set `RUSTC_WRAPPER` and its private mirror from the same bytes.
///
/// The single `OsStr` source is the point: no `String` round trip, so
/// non-UTF-8 paths mirror byte-for-byte on Unix.
pub fn set_owned_rustc_wrapper(command: &mut Command, wrapper: &OsStr, origin: WrapperOrigin) {
    command.env("RUSTC_WRAPPER", wrapper);
    command.env(EFFECTIVE_WRAPPER_ENV, wrapper);
    command.env(EFFECTIVE_WRAPPER_ORIGIN_ENV, origin.as_str());
}

/// Remove `RUSTC_WRAPPER` and the mirror together, recording why.
pub fn remove_owned_rustc_wrapper(command: &mut Command) {
    command.env_remove("RUSTC_WRAPPER");
    command.env_remove(EFFECTIVE_WRAPPER_ENV);
    command.env(
        EFFECTIVE_WRAPPER_ORIGIN_ENV,
        WrapperOrigin::Disabled.as_str(),
    );
}

/// Fail closed when Soldr-owned wrapper state drifted (soldr#2545).
///
/// Reads the *process* environment: called at re-entry boundaries where the
/// current process inherited a Soldr-owned lineage. A mirror with no (or a
/// different) `RUSTC_WRAPPER` means something between the owning Soldr and
/// this process mutated exactly the pair this invariant exists to protect —
/// continuing would hand cargo a different wrapper identity and silently
/// recompile the world. Caller-owned or unmirrored environments pass.
/// cargo-llvm-cov's sanctioned chained-wrapper protocol passes too when the
/// chain resolves back to the mirror (soldr#3504).
pub fn assert_inherited_wrapper_coherent(boundary: &str) -> Result<(), SoldrError> {
    let Some(mirror) = std::env::var_os(EFFECTIVE_WRAPPER_ENV) else {
        return Ok(());
    };
    let origin = std::env::var(EFFECTIVE_WRAPPER_ORIGIN_ENV)
        .unwrap_or_else(|_| "<origin missing>".to_string());
    if origin == WrapperOrigin::Disabled.as_str() {
        // A disabled origin leaves no mirror; seeing one anyway is drift.
        return Err(drift_error(boundary, &origin, Some(&mirror), None));
    }
    match std::env::var_os("RUSTC_WRAPPER") {
        Some(actual) if actual == mirror => Ok(()),
        actual => {
            // soldr#3504: cargo-llvm-cov is the one sanctioned mid-build
            // mutation of this pair. When its chain resolves back to the
            // mirror it is not drift; anything else still fails closed.
            let pre_existing = std::env::var_os(LLVM_COV_PRE_EXISTING_ENV);
            if llvm_cov_chain_is_coherent(&mirror, actual.as_deref(), pre_existing.as_deref()) {
                return Ok(());
            }
            Err(drift_error(
                boundary,
                &origin,
                Some(&mirror),
                actual.as_deref(),
            ))
        }
    }
}

/// Wrapper sources after validating the owned mirror and sanctioned chains.
/// The mirror remains the effective wrapper when cargo-llvm-cov temporarily
/// installs its own outer wrapper and chains back to the displaced image.
pub(crate) fn coherent_wrapper_sources(boundary: &str) -> Result<Vec<OsString>, SoldrError> {
    assert_inherited_wrapper_coherent(boundary)?;
    Ok(std::env::var_os(EFFECTIVE_WRAPPER_ENV)
        .or_else(|| std::env::var_os("RUSTC_WRAPPER"))
        .into_iter()
        .chain(std::env::var_os("RUSTC_WORKSPACE_WRAPPER"))
        .collect())
}

/// Whether a mismatched `RUSTC_WRAPPER`/mirror pair is cargo-llvm-cov's
/// sanctioned chained-wrapper protocol rather than drift (soldr#3504).
///
/// cargo-llvm-cov (`src/wrapper.rs`) points `RUSTC_WRAPPER` at its own
/// image, stashes the wrapper it displaced in
/// [`LLVM_COV_PRE_EXISTING_ENV`], and later execs that stashed wrapper as
/// a chain — both children inherit the Soldr mirror untouched, so every
/// guard boundary legitimately sees mirror = shim while `RUSTC_WRAPPER` =
/// cargo-llvm-cov. The chain counts as coherent only when it resolves back
/// to Soldr: the stashed pre-existing wrapper **is** the mirror (that is
/// how cargo-llvm-cov reached this shim in the first place), or, with no
/// stash to validate, `RUSTC_WRAPPER` is the cargo-llvm-cov image itself.
/// A stash naming some other wrapper — or a mismatched pair with neither
/// marker — is the drift this guard exists to catch (soldr#2545).
fn llvm_cov_chain_is_coherent(
    mirror: &OsStr,
    actual: Option<&OsStr>,
    pre_existing: Option<&OsStr>,
) -> bool {
    let Some(actual) = actual else {
        // A mirror with no `RUSTC_WRAPPER` at all is the original
        // soldr#2545 drift; a lingering stash does not rescue it.
        return false;
    };
    match pre_existing {
        Some(pre_existing) => pre_existing == mirror,
        None => is_cargo_llvm_cov(actual),
    }
}

/// Whether `wrapper` names the cargo-llvm-cov binary: its file stem,
/// with any `.exe`/`.bat` suffix dropped, matched case-insensitively
/// because Windows installs are.
fn is_cargo_llvm_cov(wrapper: &OsStr) -> bool {
    Path::new(wrapper)
        .file_stem()
        .and_then(OsStr::to_str)
        .is_some_and(|stem| stem.eq_ignore_ascii_case("cargo-llvm-cov"))
}

fn drift_error(
    boundary: &str,
    origin: &str,
    mirror: Option<&OsStr>,
    actual: Option<&OsStr>,
) -> SoldrError {
    let show = |value: Option<&OsStr>| {
        value
            .map(|v| v.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<unset>".to_string())
    };
    SoldrError::Other(format!(
        "soldr-owned RUSTC_WRAPPER identity drifted (observed at {boundary}): \
         {EFFECTIVE_WRAPPER_ENV}={} ({EFFECTIVE_WRAPPER_ORIGIN_ENV}={origin}) but \
         RUSTC_WRAPPER={}. Cargo fingerprints the wrapper path, so continuing \
         would silently invalidate every warm artifact and recompile the \
         workspace (soldr#2545). Fix whatever mutated one of the pair without \
         the other; Soldr never does.",
        show(mirror),
        show(actual),
    ))
}

/// Resolve a caller-owned `RUSTC_WRAPPER` spelled as the bare name `soldr`
/// to this process's own image.
///
/// The PEP 517 backend presets `RUSTC_WRAPPER=soldr`, meaning "soldr
/// itself". Left bare, cargo resolves it through `PATH`, which in a pip
/// build env finds the backend's pinned wheel binary — not necessarily the
/// newer global soldr that the backend delegated to. That process registers
/// the broker route under its own version (`min_version`) and exports it via
/// `SOLDR_BROKER_SERVICE`, so an older wrapper asking for its own lower
/// `wanted_version` is refused on every compile ("wanted_version is below
/// min_version"). Pinning the wrapper to the registering image keeps route
/// owner and route client the same binary. Any other wrapper is left alone.
pub fn pin_bare_soldr_wrapper(inherited: &OsStr, current_exe: &Path) -> Option<PathBuf> {
    let bare = inherited == OsStr::new("soldr")
        || (crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows
            && inherited.eq_ignore_ascii_case("soldr.exe"));
    bare.then(|| current_exe.to_path_buf())
}

/// Owned-state view for callers that only need to report identity.
pub fn inherited_identity() -> Option<(OsString, String)> {
    let mirror = std::env::var_os(EFFECTIVE_WRAPPER_ENV)?;
    let origin = std::env::var(EFFECTIVE_WRAPPER_ORIGIN_ENV).ok()?;
    Some((mirror, origin))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_pairs(command: &Command) -> Vec<(String, Option<OsString>)> {
        command
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(OsString::from)))
            .collect()
    }

    #[test]
    fn set_updates_wrapper_and_mirror_byte_for_byte() {
        let mut command = Command::new("true");
        let path = OsString::from("/root/.soldr/v9.9.9/shims/rustc");
        set_owned_rustc_wrapper(&mut command, &path, WrapperOrigin::SoldrManaged);
        let envs = env_pairs(&command);
        let get = |name: &str| {
            envs.iter()
                .find(|(k, _)| k == name)
                .and_then(|(_, v)| v.clone())
        };
        assert_eq!(get("RUSTC_WRAPPER"), Some(path.clone()));
        assert_eq!(get(EFFECTIVE_WRAPPER_ENV), Some(path));
        assert_eq!(
            get(EFFECTIVE_WRAPPER_ORIGIN_ENV),
            Some(OsString::from("soldr-managed"))
        );
    }

    #[test]
    fn remove_clears_both_and_records_disabled() {
        let mut command = Command::new("true");
        set_owned_rustc_wrapper(
            &mut command,
            OsStr::new("/x/rustc"),
            WrapperOrigin::SoldrManaged,
        );
        remove_owned_rustc_wrapper(&mut command);
        let envs = env_pairs(&command);
        let get = |name: &str| {
            envs.iter()
                .rev()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("RUSTC_WRAPPER"), Some(None), "removed, not just unset");
        assert_eq!(get(EFFECTIVE_WRAPPER_ENV), Some(None));
        assert_eq!(
            get(EFFECTIVE_WRAPPER_ORIGIN_ENV).flatten(),
            Some(OsString::from("disabled"))
        );
    }

    #[test]
    fn bare_soldr_wrapper_pins_to_current_image() {
        let exe = Path::new("/home/u/.venv/bin/soldr");
        assert_eq!(
            pin_bare_soldr_wrapper(OsStr::new("soldr"), exe),
            Some(exe.to_path_buf())
        );
    }

    #[test]
    fn other_wrappers_are_left_to_the_caller() {
        let exe = Path::new("/home/u/.venv/bin/soldr");
        for wrapper in ["/opt/build-env/bin/soldr", "sccache", "soldr-ci", ""] {
            assert_eq!(
                pin_bare_soldr_wrapper(OsStr::new(wrapper), exe),
                None,
                "{wrapper:?} must not be rewritten"
            );
        }
    }

    // soldr#3504: cargo-llvm-cov's chained-wrapper protocol is accepted,
    // and everything short of a chain that resolves back to Soldr still
    // fails closed.
    const MIRROR: &str = "/home/u/.soldr/0.9.30/shims/rustc";
    const LLVM_COV: &str = "/home/u/.cargo/bin/cargo-llvm-cov";
    const OTHER: &str = "/some/other/versioned/shims/rustc";

    #[test]
    fn llvm_cov_chain_resolving_back_to_the_mirror_is_accepted() {
        // The exact #3504 failure shape: the chain head is cargo-llvm-cov
        // and the stash it displaced is the Soldr shim.
        assert!(llvm_cov_chain_is_coherent(
            OsStr::new(MIRROR),
            Some(OsStr::new(LLVM_COV)),
            Some(OsStr::new(MIRROR)),
        ));
        // The stash alone vouches for the chain: whatever names the head,
        // it execs the stashed wrapper, and the stashed wrapper is ours.
        assert!(llvm_cov_chain_is_coherent(
            OsStr::new(MIRROR),
            Some(OsStr::new(OTHER)),
            Some(OsStr::new(MIRROR)),
        ));
        // No stash to validate: only cargo-llvm-cov's own image excuses
        // the disagreement.
        assert!(llvm_cov_chain_is_coherent(
            OsStr::new(MIRROR),
            Some(OsStr::new(LLVM_COV)),
            None,
        ));
    }

    #[test]
    fn wrapper_drift_without_a_coherent_chain_is_rejected() {
        // Chain resolves away from Soldr: the stash is a different wrapper.
        assert!(!llvm_cov_chain_is_coherent(
            OsStr::new(MIRROR),
            Some(OsStr::new(LLVM_COV)),
            Some(OsStr::new(OTHER)),
        ));
        assert!(!llvm_cov_chain_is_coherent(
            OsStr::new(MIRROR),
            Some(OsStr::new(OTHER)),
            Some(OsStr::new(OTHER)),
        ));
        // Plain soldr#2545 drift: no llvm-cov involvement at all.
        assert!(!llvm_cov_chain_is_coherent(
            OsStr::new(MIRROR),
            Some(OsStr::new(OTHER)),
            None,
        ));
        // A mirror with RUSTC_WRAPPER unset is drift even with a stash.
        assert!(!llvm_cov_chain_is_coherent(
            OsStr::new(MIRROR),
            None,
            Some(OsStr::new(MIRROR)),
        ));
        assert!(!llvm_cov_chain_is_coherent(OsStr::new(MIRROR), None, None));
    }

    #[test]
    fn chain_head_recognizes_cargo_llvm_cov_binary_name() {
        for path in [
            LLVM_COV,
            "cargo-llvm-cov",
            "cargo-llvm-cov.exe",
            "/c/Users/u/.cargo/bin/cargo-llvm-cov.exe",
        ] {
            assert!(
                is_cargo_llvm_cov(OsStr::new(path)),
                "{path} must be recognized as cargo-llvm-cov"
            );
        }
        for path in [
            OTHER,
            "/w/sccache",
            "/shims/rustc",
            "cargo-llvm-cov-nextest",
        ] {
            assert!(
                !is_cargo_llvm_cov(OsStr::new(path)),
                "{path} must not be recognized as cargo-llvm-cov"
            );
        }
    }

    // The process-env assertion is covered by the integration test
    // (cli_wrapper_identity.rs), which owns real child environments;
    // mutating this process's env here would race sibling tests.
}
