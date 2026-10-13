//! libuuid sysroot overlay for the catalogue GNU/Linux toolchain (soldr#3771).
//!
//! The conda-built glibc 2.17 sysroot in `gnu-linux-toolchain` ships no
//! libuuid, a library every distro sysroot carries. C code that includes
//! `<uuid/uuid.h>` and build scripts that emit `cargo:rustc-link-lib=uuid`
//! therefore fail for `--target *-unknown-linux-gnu` even though the host
//! build works off `/usr/include`.
//!
//! Consumes the soldr-toolchain `recipes/libuuid-<platform>/` catalogue rows
//! (util-linux libuuid, static, built inside manylinux2014 so it links
//! against glibc 2.17). Each row ships:
//!
//! ```text
//! include/uuid/uuid.h
//! lib/libuuid.a
//! lib/pkgconfig/uuid.pc
//! ```
//!
//! Unlike the `*-sys` substitutions, there is no crate to gate on and no
//! pkg-config probe to answer: the consumer is plain `cc` plus
//! `rustc-link-lib`. The bundle is therefore layered onto the managed
//! sysroot itself, through the same target-scoped variables that already
//! carry `--sysroot`.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

pub const MANAGED_LIBUUID_VERSION: &str = "2.41.6";

pub const LIBUUID_TARGETS: &[(&str, &str)] = &[
    ("x86_64-unknown-linux-gnu", "linux-x64-gnu"),
    ("aarch64-unknown-linux-gnu", "linux-arm64-gnu"),
];

pub fn catalogue_slug_for(triple: &str) -> Option<&'static str> {
    LIBUUID_TARGETS
        .iter()
        .find(|(rust, _)| *rust == triple)
        .map(|(_, slug)| *slug)
}

pub fn asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for("libuuid", version, slug)
}

pub async fn ensure_libuuid_sysroot(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = catalogue_slug_for(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "no libuuid sysroot recipe for target {target_triple}; \
             supported: {:?}",
            LIBUUID_TARGETS.iter().map(|(t, _)| *t).collect::<Vec<_>>()
        ))
    })?;
    super::syslib_common::ensure_syslib_bundle(paths, "libuuid", MANAGED_LIBUUID_VERSION, slug)
        .await
}

/// Layer a materialized libuuid bundle at `root` onto the prepared GNU
/// target environment `env`.
pub fn overlay_env(_env: &mut Vec<(String, String)>, _target_triple: &str, _root: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn slug_for_supported_triples() {
        assert_eq!(
            catalogue_slug_for("x86_64-unknown-linux-gnu"),
            Some("linux-x64-gnu")
        );
        assert_eq!(
            catalogue_slug_for("aarch64-unknown-linux-gnu"),
            Some("linux-arm64-gnu")
        );
        assert_eq!(catalogue_slug_for("x86_64-unknown-linux-musl"), None);
        assert_eq!(catalogue_slug_for("x86_64-pc-windows-msvc"), None);
    }

    #[test]
    fn asset_url_layout_matches_catalogue() {
        let u = asset_url_for(MANAGED_LIBUUID_VERSION, "linux-x64-gnu");
        assert!(u.contains("/libuuid/2.41.6/linux-x64-gnu/"));
        assert!(u.ends_with("/bundle.tar.zst"));
    }

    #[test]
    fn ensure_rejects_unknown_target() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ensure_libuuid_sysroot(&paths, "x86_64-unknown-linux-musl"));
        assert!(matches!(
            result.expect_err("unsupported target must error"),
            SoldrError::UnsupportedPlatform(_)
        ));
    }

    #[test]
    fn overlay_appends_include_and_link_search_to_existing_target_flags() {
        let root = Path::new("/opt/uuid/package");
        let mut env = vec![
            (
                "CFLAGS_x86_64_unknown_linux_gnu".to_string(),
                "--sysroot=/s".to_string(),
            ),
            (
                "CXXFLAGS_x86_64_unknown_linux_gnu".to_string(),
                "--sysroot=/s".to_string(),
            ),
            (
                "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS".to_string(),
                "-C link-arg=--sysroot=/s".to_string(),
            ),
        ];
        overlay_env(&mut env, "x86_64-unknown-linux-gnu", root);
        assert_eq!(
            value(&env, "CFLAGS_x86_64_unknown_linux_gnu"),
            Some("--sysroot=/s -isystem /opt/uuid/package/include")
        );
        assert_eq!(
            value(&env, "CXXFLAGS_x86_64_unknown_linux_gnu"),
            Some("--sysroot=/s -isystem /opt/uuid/package/include")
        );
        assert_eq!(
            value(&env, "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS"),
            Some("-C link-arg=--sysroot=/s -L native=/opt/uuid/package/lib")
        );
        assert_eq!(env.len(), 3, "overlay must not add new keys: {env:?}");
    }

    #[test]
    fn overlay_creates_missing_target_flags() {
        let mut env = Vec::new();
        overlay_env(
            &mut env,
            "aarch64-unknown-linux-gnu",
            Path::new("/u/package"),
        );
        assert_eq!(
            value(&env, "CFLAGS_aarch64_unknown_linux_gnu"),
            Some("-isystem /u/package/include")
        );
        assert_eq!(
            value(&env, "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS"),
            Some("-L native=/u/package/lib")
        );
    }
}
