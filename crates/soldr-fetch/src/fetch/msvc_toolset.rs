//! Managed MSVC toolset bundle fetcher — the download-fallback half of
//! soldr#1079 / soldr#2292.
//!
//! `crate::msvc_host` (soldr-cli) probes the host for an installed
//! Visual Studio C++ toolset via `vswhere.exe` first, and only reaches
//! for this module when that probe comes back empty or the discovered
//! toolset fails the ABI-compatibility check (see
//! `msvc_host::is_compatible_vc_tools_version`). This module supplies
//! the "or download it" half: a soldr-managed MSVC toolset bundle,
//! repackaged the same way the `cmake`/`ninja` bundles in
//! [`super::cmake_tools`] are — pinned version, sha256-verified,
//! extracted under `~/.soldr/bin/syslib/msvc/<version>/<slug>/package/`.
//!
//! Bundle layout (forge recipe `msvc-<shape>` on
//! zackees/soldr-toolchain), mirroring a real MSVC toolset root:
//!
//! ```text
//! package/bin/Hostx64/x64/cl.exe
//! package/include/...
//! package/lib/x64/...
//! ```
//!
//! `msvc_host::MsvcHostLayout::synthesize_env_x64` treats this
//! `package/` directory the same way it treats a `VC\Tools\MSVC\<ver>\`
//! directory under a real VS install — same relative layout, same
//! synthesis logic, just a different root.
//!
//! Only the `windows-x64` slug is published as of this writing; an
//! `aarch64-pc-windows-msvc` host falls through to the catalogue-miss
//! error naturally (no special-casing needed here — see
//! [`host_slug_for`]).
//!
//! Stub-until-ingested, same as every other catalogue consumer in this
//! module tree: when the soldr-toolchain catalogue has no row yet for
//! `(msvc, MANAGED_MSVC_VERSION, slug)`, [`ensure_msvc_bundle`] surfaces
//! the "not yet ingested" error from [`super::syslib_common`] and the
//! caller decides how to report that (see `msvc_host`'s combined
//! host-probe + download-failure error).

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

/// MSVC toolset version pinned by the soldr-toolchain `msvc-<shape>`
/// recipes. Matches the `VCToolsVersion` this bundle reports, so a
/// bundle-sourced layout passes the same compatibility check
/// (`msvc_host::is_compatible_vc_tools_version`) as a host-probed one.
/// See soldr#2292.
pub const MANAGED_MSVC_VERSION: &str = "14.44.35207";

/// Host triple → catalogue shape slug. MSVC is a HOST tool (it
/// compiles for `*-pc-windows-msvc` targets but runs on the build
/// machine), so the table keys on the host, not `--target` — same
/// convention as [`super::cmake_tools::CMAKE_TOOL_HOSTS`].
///
/// Only `windows-x64` is published today. `aarch64-pc-windows-msvc` is
/// deliberately absent: publishing that shape is tracked separately,
/// and an absent table row is a clean "unsupported host" error instead
/// of a table entry pointing at a bundle that doesn't exist yet.
pub const MSVC_TOOL_HOSTS: &[(&str, &str)] = &[("x86_64-pc-windows-msvc", "windows-x64")];

/// Catalogue slug for a host triple, if the host is supported.
pub fn host_slug_for(host_triple: &str) -> Option<&'static str> {
    MSVC_TOOL_HOSTS
        .iter()
        .find(|(rust, _)| *rust == host_triple)
        .map(|(_, slug)| *slug)
}

/// Assets-branch URL for the MSVC toolset bundle. Catalogue layout:
/// `msvc/<version>/<slug>/bundle.tar.zst`.
pub fn msvc_asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for("msvc", version, slug)
}

/// Materialize the managed MSVC toolset bundle for `host_triple`.
/// Returns the bundle root (contains `bin/Hostx64/x64/`, `include/`,
/// `lib/x64/` — the same relative layout as a real
/// `VC\Tools\MSVC\<version>\` directory).
pub async fn ensure_msvc_bundle(
    paths: &SoldrPaths,
    host_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = host_slug_for(host_triple).ok_or_else(|| unsupported_host(host_triple))?;
    super::syslib_common::ensure_syslib_bundle(paths, "msvc", MANAGED_MSVC_VERSION, slug).await
}

fn unsupported_host(host_triple: &str) -> SoldrError {
    SoldrError::UnsupportedPlatform(format!(
        "no managed msvc bundle for host {host_triple}; supported: {:?}",
        MSVC_TOOL_HOSTS.iter().map(|(t, _)| *t).collect::<Vec<_>>()
    ))
}

/// Path to `cl.exe` inside a materialized bundle root, following the
/// same `bin\Hostx64\x64\cl.exe` relative layout used by a real VS
/// install (and probed by `msvc_host::hostx64_cl_exe_path`).
pub fn cl_exe(bundle_root: &Path) -> PathBuf {
    bundle_root
        .join("bin")
        .join("Hostx64")
        .join("x64")
        .join("cl.exe")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_slug_for_windows_x64_only() {
        assert_eq!(host_slug_for("x86_64-pc-windows-msvc"), Some("windows-x64"));
        // aarch64 host: not yet published, must miss cleanly rather
        // than pointing at a nonexistent bundle.
        assert_eq!(host_slug_for("aarch64-pc-windows-msvc"), None);
        assert_eq!(host_slug_for("x86_64-unknown-linux-gnu"), None);
    }

    #[test]
    fn asset_url_matches_catalogue_layout() {
        let u = msvc_asset_url_for(MANAGED_MSVC_VERSION, "windows-x64");
        assert!(u.starts_with("https://media.githubusercontent.com/media/"));
        assert!(u.contains("/msvc/14.44.35207/windows-x64/"));
        assert!(u.ends_with("/bundle.tar.zst"));
    }

    #[test]
    fn version_constant_well_formed() {
        let parts: Vec<&str> = MANAGED_MSVC_VERSION.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "expected MAJOR.MINOR.PATCH, got {MANAGED_MSVC_VERSION}"
        );
        for p in parts {
            assert!(
                p.chars().all(|c| c.is_ascii_digit()),
                "non-digit in {MANAGED_MSVC_VERSION}"
            );
        }
    }

    #[test]
    fn cl_exe_path_layout() {
        let root = Path::new("root");
        let cl = cl_exe(root);
        assert!(
            cl.ends_with("bin/Hostx64/x64/cl.exe") || cl.ends_with("bin\\Hostx64\\x64\\cl.exe"),
            "{}",
            cl.display()
        );
    }

    #[test]
    fn ensure_bundle_rejects_unsupported_host() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(ensure_msvc_bundle(&paths, "aarch64-pc-windows-msvc"))
            .expect_err("aarch64 host must error until that shape is published");
        assert!(matches!(err, SoldrError::UnsupportedPlatform(_)));
    }
}
