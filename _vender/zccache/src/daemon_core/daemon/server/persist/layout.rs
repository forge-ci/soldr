//! Central ownership boundary for the flat-v1 artifact filename.
//!
//! Runtime consumers receive resolved `CachedPayload` values and never infer
//! a cache location. Only persistence and compatibility code calls this
//! module, which gives strict validation one complete observation point.

use super::*;

pub(in crate::daemon_core::daemon::server) use crate::daemon_core::artifact::LegacyArtifactAccessPurpose as LegacyPathPurpose;

pub(in crate::daemon_core::daemon::server) fn legacy_artifact_path(
    artifact_dir: &Path,
    key_hex: &str,
    index: usize,
    purpose: LegacyPathPurpose,
    call_site: &'static str,
) -> NormalizedPath {
    let path: NormalizedPath = artifact_dir.join(format!("{key_hex}_{index}")).into();
    crate::daemon_core::artifact::record_legacy_artifact_access(&path, key_hex, index, purpose, call_site);
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_path_format_is_owned_here() {
        let root = NormalizedPath::new("/fixture/artifacts");
        assert_eq!(
            legacy_artifact_path(
                &root,
                "abcd",
                2,
                LegacyPathPurpose::CompatibilityRead,
                "test",
            ),
            root.join("abcd_2")
        );
    }
}
