//! Release-archive debug sidecar classification (issue #786).
//!
//! When `release-auto.yml` stages a soldr release archive, it bundles
//! every debug-symbol sidecar the platform build emits so post-mortem
//! symbolication works on every target — not just the Windows
//! `soldr.pdb` case PR #782 covered first. This module is the
//! platform-independent classifier the manifest writer consults to
//! tag each staged sidecar with the right `format` field.
//!
//! Centralizing the file-suffix → format mapping here lets unit tests
//! lock the manifest schema even though the actual staging logic
//! lives in a workflow `bash` step that isn't directly testable.
//! Anyone touching the YAML can use the same constant strings the
//! classifier returns, eliminating drift between the workflow's
//! `format` value and what installers/validators expect.

/// Format tag emitted in `manifest.json` under
/// `<component>.debug_info[].format`. Versioned implicitly via the
/// existing `schema_version` field — adding a new variant here is a
/// schema bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugSidecarFormat {
    /// Windows MSVC program database. Emitted by default on every
    /// `*-pc-windows-msvc` Rust release build.
    Pdb,
    /// macOS DWARF symbol bundle. Emitted only when
    /// `[profile.release] split-debuginfo = "packed"` (or `"unpacked"`
    /// for the per-object `.dwo` form). The bundle is a *directory*,
    /// not a file — workflow archiving must preserve its tree shape.
    Dsym,
    /// Linux split DWARF, packed form. Emitted when
    /// `[profile.release] split-debuginfo = "packed"`.
    Dwp,
    /// Linux split DWARF, unpacked per-object form. Emitted when
    /// `[profile.release] split-debuginfo = "unpacked"`. Most release
    /// profiles don't use this; included for completeness so the
    /// manifest tag is stable if a future profile change opts in.
    Dwo,
}

impl DebugSidecarFormat {
    /// The string written into `manifest.json` under
    /// `<component>.debug_info[].format`. Stable values consumed by
    /// `scripts/zccache-contract.js` and
    /// `.github/actions/setup-soldr/zccache_contract.py`.
    pub fn as_manifest_str(self) -> &'static str {
        match self {
            Self::Pdb => "pdb",
            Self::Dsym => "dsym",
            Self::Dwp => "dwp",
            Self::Dwo => "dwo",
        }
    }
}

/// Classify a sidecar by its filename. Accepts both leaf names
/// (`soldr.pdb`) and paths (`dist/package/soldr.pdb`). The match is
/// case-insensitive because:
///
/// * Windows preserves case in filenames but APIs return them in
///   whatever case the on-disk record holds — `SOLDR.PDB` is a valid
///   match;
/// * `.dSYM` is conventionally upper-S; matching it requires the
///   tolerant comparison;
/// * a filesystem with case-folding hardlinks (Windows + WSL2 NTFS)
///   surfaces both spellings depending on which API enumerated the
///   tree.
pub fn classify_sidecar(filename: &str) -> Option<DebugSidecarFormat> {
    let lower = filename.to_ascii_lowercase();
    // `.dSYM` is a directory; the workflow may pass either the bundle
    // root (`soldr.dSYM`) or a path that contains it.
    if lower.ends_with(".dsym") || lower.contains(".dsym/") || lower.contains(".dsym\\") {
        return Some(DebugSidecarFormat::Dsym);
    }
    if lower.ends_with(".pdb") {
        return Some(DebugSidecarFormat::Pdb);
    }
    if lower.ends_with(".dwp") {
        return Some(DebugSidecarFormat::Dwp);
    }
    if lower.ends_with(".dwo") {
        return Some(DebugSidecarFormat::Dwo);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdb_is_classified_as_pdb() {
        assert_eq!(classify_sidecar("soldr.pdb"), Some(DebugSidecarFormat::Pdb));
        assert_eq!(
            classify_sidecar("soldr_cli.pdb"),
            Some(DebugSidecarFormat::Pdb),
        );
    }

    #[test]
    fn dsym_bundle_is_classified_as_dsym() {
        // The macOS dSYM bundle is a directory; the workflow may pass
        // either form so both must classify cleanly.
        assert_eq!(
            classify_sidecar("soldr.dSYM"),
            Some(DebugSidecarFormat::Dsym),
        );
        assert_eq!(
            classify_sidecar("soldr.dSYM/Contents/Resources/DWARF/soldr"),
            Some(DebugSidecarFormat::Dsym),
        );
    }

    #[test]
    fn dwp_is_classified_as_dwp() {
        assert_eq!(classify_sidecar("soldr.dwp"), Some(DebugSidecarFormat::Dwp));
    }

    #[test]
    fn dwo_is_classified_as_dwo() {
        // Unpacked split-debuginfo emits per-object .dwo files. The
        // classifier supports them so a future profile flip is a
        // workflow change, not a code change.
        assert_eq!(
            classify_sidecar("soldr-abc123.dwo"),
            Some(DebugSidecarFormat::Dwo),
        );
    }

    #[test]
    fn classifier_is_case_insensitive() {
        // Windows PDB enumeration can return uppercase; the macOS dSYM
        // bundle is conventionally upper-S. The tolerant compare keeps
        // the workflow from having to canonicalize before calling.
        for name in ["SOLDR.PDB", "Soldr.Pdb", "soldr.DWP", "Soldr.DSYM"] {
            assert!(
                classify_sidecar(name).is_some(),
                "expected {name:?} to classify",
            );
        }
    }

    #[test]
    fn classifier_returns_none_for_release_binary() {
        // The release binary itself must NOT classify as a sidecar,
        // or the manifest writer would double-list it.
        assert_eq!(classify_sidecar("soldr"), None);
        assert_eq!(classify_sidecar("soldr.exe"), None);
        assert_eq!(classify_sidecar("zccache"), None);
        assert_eq!(classify_sidecar("zccache-daemon"), None);
    }

    #[test]
    fn classifier_returns_none_for_unrelated_files() {
        // Defensive: build artifacts in the same directory must not
        // be misclassified as sidecars. `dep-info`/`.d`/`.rlib` are the
        // common confounders.
        for name in [
            "soldr.d",
            "soldr.rlib",
            "libsoldr.so",
            "manifest.json",
            "README.md",
        ] {
            assert_eq!(
                classify_sidecar(name),
                None,
                "expected {name:?} to not classify as a sidecar",
            );
        }
    }

    #[test]
    fn manifest_strings_are_stable() {
        // Lock the JSON-facing strings. Changing any of these is a
        // manifest schema break that needs a coordinated bump in
        // `scripts/zccache-contract.js` and
        // `.github/actions/setup-soldr/zccache_contract.py`.
        assert_eq!(DebugSidecarFormat::Pdb.as_manifest_str(), "pdb");
        assert_eq!(DebugSidecarFormat::Dsym.as_manifest_str(), "dsym");
        assert_eq!(DebugSidecarFormat::Dwp.as_manifest_str(), "dwp");
        assert_eq!(DebugSidecarFormat::Dwo.as_manifest_str(), "dwo");
    }
}
