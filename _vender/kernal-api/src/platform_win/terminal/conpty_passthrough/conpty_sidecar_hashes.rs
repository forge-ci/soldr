//! Compile-time SHA-256 verification table for the Win10 ConPTY sidecar (#447).
//!
//! `build.rs` parses `conpty-sidecar.sha256.toml` at the workspace root and
//! emits the const table this module `include!`s. The release workflow
//! rewrites the toml in-place before each cargo build, so a crate built in
//! the same workflow that publishes the release tarballs carries the
//! matching hashes.
//!
//! Pre-release dev checkouts ship with an empty manifest, so every const
//! below is `None` and the runtime falls back to "fetch only, no verify."

#![cfg(windows)]

include!(concat!(env!("OUT_DIR"), "/conpty_sidecar_hashes.rs"));

/// Returns the verification baseline for the current build's target arch,
/// if the manifest carried one. `None` means the runtime should fetch
/// without verifying (and log a diagnostic line on opt-in).
pub(super) fn expected_for_current_arch() -> Option<&'static ExpectedAsset> {
    #[cfg(target_arch = "x86_64")]
    {
        EXPECTED_X64.as_ref()
    }
    #[cfg(target_arch = "aarch64")]
    {
        EXPECTED_ARM64.as_ref()
    }
    #[cfg(target_arch = "x86")]
    {
        EXPECTED_X86.as_ref()
    }
    #[cfg(target_arch = "arm")]
    {
        EXPECTED_ARM.as_ref()
    }
    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "x86",
        target_arch = "arm"
    )))]
    {
        None
    }
}
