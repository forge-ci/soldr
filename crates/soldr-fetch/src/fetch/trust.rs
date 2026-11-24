//! Integrity and trust policy for fetched binaries.
//!
//! `soldr` has historically treated every successful GitHub-Releases download
//! as authentic. Issue #42 requires that claim to be backed by actual integrity
//! enforcement. This module adds the primitives:
//!
//! - `sha256_of(bytes)` — compute the canonical integrity hash of an archive.
//! - `TrustMode` — `permissive` (default) or `strict`, read from
//!   `SOLDR_TRUST_MODE`. Unrecognised values fail closed to `strict`.
//! - `PinnedChecksumStore` — loaded from a TOML file referenced by
//!   `SOLDR_CHECKSUMS_FILE`, or returned empty if the env var is unset.
//! - `verify_download(asset, tool, version, sha256, store, mode)` — returns a
//!   `VerifyOutcome` that the caller logs and obeys.
//!
//! The fetch path then:
//! 1. Always prints the computed sha256 to stderr so humans can audit it.
//! 2. If a pin exists for the asset and mismatches, returns an error in any
//!    mode. This is the "no longer implicitly trusted" guarantee.
//! 3. If no pin exists and mode is `strict`, refuses to install. In
//!    `permissive` mode it installs and prints a `trust: unverified` warning.

use crate::core::SoldrError;
use kernal_api::hash::Sha256Hasher;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

pub const TRUST_MODE_ENV_VAR: &str = "SOLDR_TRUST_MODE";
pub const CHECKSUMS_FILE_ENV_VAR: &str = "SOLDR_CHECKSUMS_FILE";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustMode {
    /// Default. Unverified fetches install but emit a warning.
    Permissive,
    /// Every fetch must match a pinned checksum; unverified fetches fail.
    Strict,
}

impl TrustMode {
    pub fn from_env() -> Self {
        Self::parse(std::env::var(TRUST_MODE_ENV_VAR).ok().as_deref())
    }

    /// Parse a raw `SOLDR_TRUST_MODE` value. Pure so it is testable without
    /// mutating the process environment.
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("strict") => Self::Strict,
            Some("permissive") | Some("") | None => Self::Permissive,
            Some(other) => {
                eprintln!(
                    "soldr: unknown {TRUST_MODE_ENV_VAR}={other:?}; expected \"permissive\" or \"strict\"; failing closed to \"strict\""
                );
                // soldr#3681: fail closed. A typo in a hardening switch must
                // never silently disable it.
                Self::Strict
            }
        }
    }
}

/// A single pinned checksum entry. Keyed by (tool, version, asset name) so
/// asset matching stays platform-scoped — a Windows MSVC zip and a Linux musl
/// tar.gz for the same tool+version carry different checksums.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct RawPinnedChecksum {
    tool: String,
    version: String,
    asset: String,
    sha256: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawPinnedFile {
    #[serde(default, rename = "tool")]
    tools: Vec<RawPinnedChecksum>,
}

#[derive(Debug, Default, Clone)]
pub struct PinnedChecksumStore {
    // (tool, version, asset) → sha256 (lowercase hex)
    entries: HashMap<(String, String, String), String>,
}

impl PinnedChecksumStore {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_env() -> Result<Self, SoldrError> {
        match std::env::var(CHECKSUMS_FILE_ENV_VAR) {
            Ok(path) if !path.trim().is_empty() => Self::from_file(Path::new(path.trim())),
            _ => Ok(Self::empty()),
        }
    }

    pub fn from_file(path: &Path) -> Result<Self, SoldrError> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            SoldrError::Other(format!(
                "failed to read {CHECKSUMS_FILE_ENV_VAR} at {}: {e}",
                path.display()
            ))
        })?;
        Self::from_toml(&text)
    }

    pub fn from_toml(text: &str) -> Result<Self, SoldrError> {
        let raw: RawPinnedFile = toml::from_str(text).map_err(|e| {
            SoldrError::Other(format!("failed to parse pinned checksums TOML: {e}"))
        })?;
        let mut entries = HashMap::new();
        for entry in raw.tools {
            let sha = entry.sha256.trim().to_ascii_lowercase();
            if !is_hex64(&sha) {
                return Err(SoldrError::Other(format!(
                    "pinned sha256 for {}@{} asset {} is not a 64-char hex string",
                    entry.tool, entry.version, entry.asset
                )));
            }
            entries.insert((entry.tool, entry.version, entry.asset), sha);
        }
        Ok(Self { entries })
    }

    pub fn lookup(&self, tool: &str, version: &str, asset: &str) -> Option<&str> {
        self.entries
            .get(&(tool.to_string(), version.to_string(), asset.to_string()))
            .map(|s| s.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Compute the canonical SHA-256 hex digest of a downloaded archive.
pub fn sha256_of(bytes: &[u8]) -> String {
    let mut hasher = Sha256Hasher::new();
    hasher.update(bytes);
    hasher.finalize().to_hex()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// A pinned checksum existed and matched.
    Verified { sha256: String },
    /// No pinned checksum exists for this (tool, version, asset); permissive
    /// mode installed it anyway.
    Unverified { sha256: String },
}

/// Verify a downloaded archive against a pinned checksum store and the active
/// trust mode.
///
/// Returns `Err` in two cases:
/// 1. A pin exists for this (tool, version, asset) and does not match. Always
///    an error regardless of mode.
/// 2. No pin exists and mode is `Strict`.
pub fn verify_download(
    tool: &str,
    version: &str,
    asset_name: &str,
    sha256: &str,
    store: &PinnedChecksumStore,
    mode: TrustMode,
) -> Result<VerifyOutcome, SoldrError> {
    let actual = sha256.to_ascii_lowercase();
    match store.lookup(tool, version, asset_name) {
        Some(expected) => {
            if expected == actual {
                Ok(VerifyOutcome::Verified { sha256: actual })
            } else {
                Err(SoldrError::Other(format!(
                    "trust: pinned sha256 mismatch for {tool} v{version} asset {asset_name}\n  expected: {expected}\n  actual:   {actual}"
                )))
            }
        }
        None => match mode {
            TrustMode::Strict => Err(SoldrError::Other(format!(
                "trust: no pinned checksum for {tool} v{version} asset {asset_name}; refusing fetch under {TRUST_MODE_ENV_VAR}=strict"
            ))),
            TrustMode::Permissive => Ok(VerifyOutcome::Unverified { sha256: actual }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_of_matches_expected_digest() {
        // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            sha256_of(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(
            sha256_of(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn trust_mode_defaults_to_permissive() {
        assert_eq!(TrustMode::parse(None), TrustMode::Permissive);
        assert_eq!(TrustMode::parse(Some("")), TrustMode::Permissive);
        assert_eq!(TrustMode::parse(Some("  ")), TrustMode::Permissive);
        assert_eq!(TrustMode::parse(Some("permissive")), TrustMode::Permissive);
        assert_eq!(TrustMode::parse(Some("Permissive")), TrustMode::Permissive);
        assert_eq!(TrustMode::parse(Some("strict")), TrustMode::Strict);
        assert_eq!(TrustMode::parse(Some(" STRICT ")), TrustMode::Strict);
    }

    #[test]
    fn trust_mode_unknown_value_fails_closed() {
        // soldr#3681: a typo in a hardening switch must not disable it.
        for typo in ["strcit", "1", "true", "on", "yes"] {
            assert_eq!(
                TrustMode::parse(Some(typo)),
                TrustMode::Strict,
                "SOLDR_TRUST_MODE={typo:?} must fail closed"
            );
        }
    }

    #[test]
    fn pinned_store_parses_toml_entries() {
        let toml_text = r#"
            [[tool]]
            tool = "cargo-nextest"
            version = "0.9.100"
            asset = "cargo-nextest-0.9.100-x86_64-pc-windows-msvc.zip"
            sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
        "#;
        let store = PinnedChecksumStore::from_toml(toml_text).unwrap();
        assert_eq!(
            store.lookup(
                "cargo-nextest",
                "0.9.100",
                "cargo-nextest-0.9.100-x86_64-pc-windows-msvc.zip"
            ),
            Some("0000000000000000000000000000000000000000000000000000000000000000")
        );
        assert!(store
            .lookup("cargo-nextest", "0.9.100", "other.zip")
            .is_none());
    }

    #[test]
    fn pinned_store_rejects_malformed_sha256() {
        let toml_text = r#"
            [[tool]]
            tool = "x"
            version = "1"
            asset = "x.zip"
            sha256 = "not-a-hex-string"
        "#;
        let err = PinnedChecksumStore::from_toml(toml_text).unwrap_err();
        assert!(err.to_string().contains("64-char hex"));
    }

    #[test]
    fn verify_download_accepts_matching_pin() {
        let toml_text = r#"
            [[tool]]
            tool = "cargo-nextest"
            version = "0.9.100"
            asset = "cargo-nextest.zip"
            sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        "#;
        let store = PinnedChecksumStore::from_toml(toml_text).unwrap();
        let outcome = verify_download(
            "cargo-nextest",
            "0.9.100",
            "cargo-nextest.zip",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            &store,
            TrustMode::Strict,
        )
        .unwrap();
        assert!(matches!(outcome, VerifyOutcome::Verified { .. }));
    }

    #[test]
    fn verify_download_rejects_pin_mismatch_in_either_mode() {
        let toml_text = r#"
            [[tool]]
            tool = "t"
            version = "1"
            asset = "a.zip"
            sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        "#;
        let store = PinnedChecksumStore::from_toml(toml_text).unwrap();
        for mode in [TrustMode::Permissive, TrustMode::Strict] {
            let err = verify_download(
                "t",
                "1",
                "a.zip",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                &store,
                mode,
            )
            .unwrap_err();
            assert!(err.to_string().contains("pinned sha256 mismatch"));
        }
    }

    #[test]
    fn verify_download_strict_mode_rejects_missing_pin() {
        let store = PinnedChecksumStore::empty();
        let err = verify_download(
            "cargo-nextest",
            "0.9.100",
            "cargo-nextest.zip",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            &store,
            TrustMode::Strict,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no pinned checksum"));
    }

    #[test]
    fn verify_download_permissive_mode_allows_missing_pin() {
        let store = PinnedChecksumStore::empty();
        let outcome = verify_download(
            "cargo-nextest",
            "0.9.100",
            "cargo-nextest.zip",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            &store,
            TrustMode::Permissive,
        )
        .unwrap();
        assert!(matches!(outcome, VerifyOutcome::Unverified { .. }));
    }

    #[test]
    fn verify_download_is_case_insensitive_on_the_digest() {
        let toml_text = r#"
            [[tool]]
            tool = "t"
            version = "1"
            asset = "a.zip"
            sha256 = "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855"
        "#;
        let store = PinnedChecksumStore::from_toml(toml_text).unwrap();
        let outcome = verify_download(
            "t",
            "1",
            "a.zip",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            &store,
            TrustMode::Strict,
        )
        .unwrap();
        assert!(matches!(outcome, VerifyOutcome::Verified { .. }));
    }
}
