//! Distinguishes genuine CI-released `soldr` binaries from dev/manual
//! builds (soldr#1597 Phase 2).
//!
//! `build.rs` turns a CI-only `SOLDR_RELEASE_CI=1` env var (set solely by
//! `release-auto.yml`'s publish step) into a compile-time
//! `SOLDR_OFFICIAL_BUILD` env constant, so this check is free at runtime
//! and can't be spoofed by setting an env var against an already-built
//! dev binary.

/// `true` for binaries built by `release-auto.yml`'s publish step;
/// `false` for any locally-built (`cargo build` / `soldr cargo build`)
/// dev binary.
pub fn is_official_build() -> bool {
    parse_official_marker(option_env!("SOLDR_OFFICIAL_BUILD"))
}

fn parse_official_marker(marker: Option<&str>) -> bool {
    marker == Some("1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_official_marker_only_accepts_stamped_one() {
        assert!(parse_official_marker(Some("1")));
        assert!(!parse_official_marker(None));
        assert!(!parse_official_marker(Some("0")));
        assert!(!parse_official_marker(Some("true")));
        assert!(!parse_official_marker(Some("")));
    }

    #[test]
    fn dev_build_under_test_reports_unofficial() {
        // The test binary itself is always a local/dev build (SOLDR_RELEASE_CI
        // is never set for `cargo test` runs), so this should always hold.
        assert!(!is_official_build());
    }
}
