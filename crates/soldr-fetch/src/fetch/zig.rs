//! Auto-bootstrap a `zig` binary for `cargo zigbuild` to consume.
//!
//! cargo-zigbuild shells out to a `zig` executable to do the cross-link
//! step. Until this module existed, `soldr cargo zigbuild ...` would
//! fetch `cargo-zigbuild` from `known_tools` but leave zig itself
//! missing — every cross-compile lane on a fresh runner exploded with
//! `Error: Failed to find zig / cannot find binary path` (observed in
//! `cross-compile-all-targets.yml` run 27893281530). The fix lives in
//! soldr, not the workflow, because soldr advertises itself as the
//! bootstrapper for cross builds (CLAUDE.md "Pre-built first") and
//! every consumer of `soldr cargo zigbuild` benefits.
//!
//! Resolution order, mirroring cargo-zigbuild's own logic:
//!   1. `ZIG` env var pointing at an existing file → use it.
//!   2. `zig` (or `zig.exe`) already on `PATH` → use it.
//!   3. Managed fetch from `https://ziglang.org/download/...`, cached
//!      at `~/.soldr/bin/zig-<MANAGED_ZIG_VERSION>/`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::core::{SoldrError, SoldrPaths};

use super::stream_download::{
    asset_http_client_with_protocol, get_request, send_asset_request, stream_response_to_temp_file,
    AssetProtocol, DownloadedAsset, ASSET_HEADER_TIMEOUT, ASSET_IDLE_TIMEOUT,
};
use super::trust;

/// Zig version that ships in soldr's managed bootstrap.
///
/// 0.14.1 (March 2025) is the floor that picks up cargo-zigbuild's
/// macOS SDKROOT fixes (v0.21.2 — rust-cross/cargo-zigbuild#387) and
/// the Xcode 15 / Apple SDK linker work that resolves the
/// `unable to find dynamic system library 'objc'` failure observed
/// on soldr's CI Apple cross-build lanes. cargo-zigbuild v0.21.4
/// added zig-0.15 compat and v0.23.0 handles zig-0.15 ZON output; we
/// stay on 0.14.1 because the API churn between zig 0.14 and 0.15 is
/// material and 0.14.1 is the most-tested combination with
/// cargo-zigbuild's current release. Bump in lockstep with cargo-
/// zigbuild whenever zig 0.15+ becomes the floor downstream needs.
pub const MANAGED_ZIG_VERSION: &str = "0.14.1";

const ZIG_ENV_VAR: &str = "ZIG";
const ZIG_DOWNLOAD_ATTEMPTS: u32 = 4;
const ZIG_DOWNLOAD_INITIAL_BACKOFF: Duration = Duration::from_secs(5);

/// Ensure a zig binary is available for cargo-zigbuild. Returns the
/// **directory** holding the binary so the caller can prepend it to
/// `PATH` (matching how `ensure_known_subcommand_tool` already wires
/// `cargo-zigbuild` into the child cargo's environment).
pub async fn ensure_zig(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    if let Some(dir) = zig_dir_from_env_var() {
        return Ok(dir);
    }
    if let Some(dir) = zig_dir_from_path() {
        return Ok(dir);
    }

    paths.ensure_dirs()?;

    let install_dir = paths.bin.join(format!("zig-{MANAGED_ZIG_VERSION}"));
    let stamp = install_dir.join(".complete");
    let cached_bin = managed_zig_binary_path(&install_dir);
    if stamp.is_file() && cached_bin.is_file() {
        if let Some(parent) = cached_bin.parent() {
            return Ok(parent.to_path_buf());
        }
    }

    let (asset, url) = zig_download_url(MANAGED_ZIG_VERSION)?;
    eprintln!("soldr: fetching zig v{MANAGED_ZIG_VERSION} ({asset})...");

    let downloaded = download_zig_asset(&url).await?;

    let digest = downloaded.sha256();
    let store = trust::PinnedChecksumStore::from_env()?;
    let mode = trust::TrustMode::from_env();
    match verify_zig_download(&asset, digest, &store, mode)? {
        trust::VerifyOutcome::Verified { sha256 } => {
            eprintln!("soldr: trust: verified zig v{MANAGED_ZIG_VERSION} {asset} sha256={sha256}");
        }
        trust::VerifyOutcome::Unverified { sha256 } => {
            eprintln!(
                "soldr: trust: unverified zig v{MANAGED_ZIG_VERSION} {asset} sha256={sha256} (set {} to pin; run with {}=strict to require pins)",
                trust::CHECKSUMS_FILE_ENV_VAR,
                trust::TRUST_MODE_ENV_VAR
            );
        }
    }

    let bin_dir = install_dir
        .parent()
        .ok_or_else(|| SoldrError::Other("zig install dir has no parent".into()))?;
    let dir = install_zig_archive(bin_dir, downloaded.path(), &asset)?;
    eprintln!("soldr: downloaded zig v{MANAGED_ZIG_VERSION}");
    Ok(dir)
}

/// Verify a Zig download: a user pin (`SOLDR_CHECKSUMS_FILE`) wins, then the
/// built-in pin, then the trust mode decides.
fn verify_zig_download(
    asset: &str,
    digest: &str,
    store: &trust::PinnedChecksumStore,
    mode: trust::TrustMode,
) -> Result<trust::VerifyOutcome, SoldrError> {
    if store.lookup("zig", MANAGED_ZIG_VERSION, asset).is_some() {
        return trust::verify_download("zig", MANAGED_ZIG_VERSION, asset, digest, store, mode);
    }
    let Some(expected) = builtin_zig_sha256(MANAGED_ZIG_VERSION, asset) else {
        return trust::verify_download("zig", MANAGED_ZIG_VERSION, asset, digest, store, mode);
    };
    let actual = digest.to_ascii_lowercase();
    if actual == expected {
        Ok(trust::VerifyOutcome::Verified { sha256: actual })
    } else {
        Err(SoldrError::Other(format!(
            "trust: built-in sha256 mismatch for zig v{MANAGED_ZIG_VERSION} asset {asset}\n  expected: {expected}\n  actual:   {actual}"
        )))
    }
}

/// Built-in pins for every `MANAGED_ZIG_VERSION` asset, from
/// `https://ziglang.org/download/index.json` (`shasum`), cross-checked by
/// hashing the downloaded archives. Bump with `MANAGED_ZIG_VERSION`.
const BUILTIN_ZIG_PINS: &[(&str, &str, &str)] = &[
    (
        "0.14.1",
        "zig-x86_64-linux-0.14.1.tar.xz",
        "24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c",
    ),
    (
        "0.14.1",
        "zig-aarch64-linux-0.14.1.tar.xz",
        "f7a654acc967864f7a050ddacfaa778c7504a0eca8d2b678839c21eea47c992b",
    ),
    (
        "0.14.1",
        "zig-x86_64-macos-0.14.1.tar.xz",
        "b0f8bdfb9035783db58dd6c19d7dea89892acc3814421853e5752fe4573e5f43",
    ),
    (
        "0.14.1",
        "zig-aarch64-macos-0.14.1.tar.xz",
        "39f3dc5e79c22088ce878edc821dedb4ca5a1cd9f5ef915e9b3cc3053e8faefa",
    ),
    (
        "0.14.1",
        "zig-x86_64-windows-0.14.1.zip",
        "554f5378228923ffd558eac35e21af020c73789d87afeabf4bfd16f2e6feed2c",
    ),
    (
        "0.14.1",
        "zig-aarch64-windows-0.14.1.zip",
        "b5aac0ccc40dd91e8311b1f257717d8e3903b5fefb8f659de6d65a840ad1d0e7",
    ),
];

/// Built-in SHA-256 pin for a managed Zig asset (soldr#3682).
fn builtin_zig_sha256(version: &str, asset: &str) -> Option<&'static str> {
    BUILTIN_ZIG_PINS
        .iter()
        .find(|(v, a, _)| *v == version && *a == asset)
        .map(|(_, _, sha)| *sha)
}

/// Extract `archive` into `<bin_dir>/zig-<MANAGED_ZIG_VERSION>` and return
/// the directory holding the zig binary.
fn install_zig_archive(bin_dir: &Path, archive: &Path, asset: &str) -> Result<PathBuf, SoldrError> {
    let dir_name = format!("zig-{MANAGED_ZIG_VERSION}");
    let install_dir = bin_dir.join(&dir_name);
    // Serialize installers; a waiter usually finds the winner finished.
    let _lock = super::syslib_common::acquire_install_lock(bin_dir, &dir_name)?;
    if let Some(dir) = completed_zig_dir(&install_dir) {
        return Ok(dir);
    }

    // Extract into a sibling staging dir and promote it whole, so no
    // partial tree ever sits under the canonical name.
    let staging = tempfile::Builder::new()
        .prefix(&format!(".{dir_name}-staging-"))
        .tempdir_in(bin_dir)?;
    if asset.ends_with(".zip") {
        extract_zip_tree(std::fs::File::open(archive)?, staging.path())?;
    } else {
        extract_tar_xz_tree(std::fs::File::open(archive)?, staging.path())?;
    }

    let resolved = managed_zig_binary_path(staging.path());
    if !resolved.is_file() {
        return Err(SoldrError::Archive(format!(
            "zig binary not found after extract at {}",
            resolved.display()
        )));
    }

    // Publish the extracted binary with a fixed 0o755 (no-op on
    // Windows, where Unix mode bits are meaningless).
    let source = std::fs::metadata(&resolved)?.permissions();
    crate::platform::fs::permissions::make_executable_from(&resolved, &source)?;
    std::fs::write(staging.path().join(".complete"), MANAGED_ZIG_VERSION)?;

    let staging_path = staging.keep();
    if let Err(error) = super::archive::promote_staged_tool_dir(&staging_path, &install_dir) {
        let _ = std::fs::remove_dir_all(&staging_path);
        return Err(error);
    }
    completed_zig_dir(&install_dir).ok_or_else(|| {
        SoldrError::Archive(format!(
            "zig install incomplete at {}",
            install_dir.display()
        ))
    })
}

fn completed_zig_dir(install_dir: &Path) -> Option<PathBuf> {
    let bin = managed_zig_binary_path(install_dir);
    if install_dir.join(".complete").is_file() && bin.is_file() {
        bin.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

async fn download_zig_asset(url: &str) -> Result<DownloadedAsset, SoldrError> {
    let client = asset_http_client_with_protocol("managed zig", AssetProtocol::Http1Only)?;
    super::retry::with_asset_backoff_params(
        "managed zig",
        ZIG_DOWNLOAD_ATTEMPTS,
        ZIG_DOWNLOAD_INITIAL_BACKOFF,
        || download_zig_asset_once(&client, url),
    )
    .await
}

async fn download_zig_asset_once(
    client: &reqwest::Client,
    url: &str,
) -> Result<DownloadedAsset, SoldrError> {
    let resp = send_asset_request(
        get_request(client, url).header(reqwest::header::ACCEPT_ENCODING, "identity"),
        url,
        ASSET_HEADER_TIMEOUT,
    )
    .await?;
    stream_response_to_temp_file(resp, url, ASSET_IDLE_TIMEOUT).await
}

fn zig_dir_from_env_var() -> Option<PathBuf> {
    let value = std::env::var_os(ZIG_ENV_VAR)?;
    let p = PathBuf::from(value);
    if p.is_file() {
        p.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

fn zig_dir_from_path() -> Option<PathBuf> {
    let exe = zig_binary_filename();
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(exe);
        if candidate.is_file() {
            return Some(dir);
        }
    }
    None
}

fn zig_binary_filename() -> &'static str {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        "zig.exe"
    } else {
        "zig"
    }
}

fn managed_zig_binary_path(install_dir: &Path) -> PathBuf {
    // Try the constructed name first (no I/O). Fall back to a single-
    // depth scan if the constructed name doesn't exist. The scan is
    // the robustness layer for future upstream naming changes
    // (see soldr#1032 — the zig-0.14.0 asset-naming swap also flipped
    // the tarball's internal top-level dir name, and the previous
    // hardcoded `zig-{os}-{arch}-{ver}` formula in `managed_zig_archive_root`
    // pointed at a directory that doesn't exist for 0.14+).
    let constructed = install_dir
        .join(managed_zig_archive_root())
        .join(zig_binary_filename());
    if constructed.is_file() {
        return constructed;
    }
    if let Some(scanned) = scan_for_zig_binary(install_dir) {
        return scanned;
    }
    constructed
}

/// The official zig tarballs unpack to a single top-level directory
/// `zig-<arch>-<os>-<version>` (the 0.14+ naming) or
/// `zig-<os>-<arch>-<version>` (the 0.13.x naming). Match the same
/// branching used by `zig_download_url` so the extracted directory
/// name is found on the first try without scanning.
fn managed_zig_archive_root() -> String {
    let (os, arch) = host_zig_os_arch().unwrap_or(("linux", "x86_64"));
    let pre_0_14 = matches!(
        MANAGED_ZIG_VERSION,
        "0.13.0" | "0.12.0" | "0.11.0" | "0.10.1" | "0.10.0"
    );
    if pre_0_14 {
        format!("zig-{os}-{arch}-{MANAGED_ZIG_VERSION}")
    } else {
        format!("zig-{arch}-{os}-{MANAGED_ZIG_VERSION}")
    }
}

/// Fallback: look one level down from `install_dir` for a directory
/// that contains the zig binary. Robustness layer if upstream changes
/// the top-level directory naming again in a future release without
/// soldr getting a coordinated bump.
fn scan_for_zig_binary(install_dir: &Path) -> Option<PathBuf> {
    let exe = zig_binary_filename();
    let read_dir = std::fs::read_dir(install_dir).ok()?;
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let candidate = path.join(exe);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn host_zig_os_arch() -> Option<(&'static str, &'static str)> {
    use crate::platform::host::facts::{arch, os, HostArch, HostOs};

    match (os(), arch()) {
        (HostOs::Linux, HostArch::X86_64) => Some(("linux", "x86_64")),
        (HostOs::Linux, HostArch::Aarch64) => Some(("linux", "aarch64")),
        (HostOs::MacOs, HostArch::X86_64) => Some(("macos", "x86_64")),
        (HostOs::MacOs, HostArch::Aarch64) => Some(("macos", "aarch64")),
        (HostOs::Windows, HostArch::X86_64) => Some(("windows", "x86_64")),
        (HostOs::Windows, HostArch::Aarch64) => Some(("windows", "aarch64")),
        _ => None,
    }
}

fn zig_download_url(version: &str) -> Result<(String, String), SoldrError> {
    let Some((os, arch)) = host_zig_os_arch() else {
        return Err(SoldrError::Other(format!(
            "unsupported host for zig bootstrap: arch={} os={}",
            std::env::consts::ARCH,
            std::env::consts::OS,
        )));
    };
    let ext = if os == "windows" { "zip" } else { "tar.xz" };
    // Asset-naming swap landed in zig 0.14.0: pre-0.14 used
    // `zig-{os}-{arch}-{ver}.tar.xz`; 0.14+ uses
    // `zig-{arch}-{os}-{ver}.tar.xz`. Branch on the major/minor here
    // rather than threading a per-version table — the swap is the
    // only naming change in the version range soldr cares about.
    let pre_0_14 = matches!(
        version,
        "0.13.0" | "0.12.0" | "0.11.0" | "0.10.1" | "0.10.0"
    );
    let asset = if pre_0_14 {
        format!("zig-{os}-{arch}-{version}.{ext}")
    } else {
        format!("zig-{arch}-{os}-{version}.{ext}")
    };
    let url = format!("https://ziglang.org/download/{version}/{asset}");
    Ok((asset, url))
}

fn extract_tar_xz_tree<R: std::io::Read>(reader: R, dest: &Path) -> Result<(), SoldrError> {
    let xz = xz2::read::XzDecoder::new(reader);
    let mut archive = tar::Archive::new(xz);
    archive
        .unpack(dest)
        .map_err(|e| SoldrError::Archive(e.to_string()))?;
    Ok(())
}

fn extract_zip_tree<R: std::io::Read + std::io::Seek>(
    reader: R,
    dest: &Path,
) -> Result<(), SoldrError> {
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| SoldrError::Archive(e.to_string()))?;
    archive
        .extract(dest)
        .map_err(|e| SoldrError::Archive(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zip_with(build: impl FnOnce(&mut zip::ZipWriter<std::io::Cursor<Vec<u8>>>)) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        build(&mut zw);
        zw.finish().unwrap().into_inner()
    }

    /// The zip 2.x -> 9.x migration must keep `ZipArchive::extract`'s
    /// containment guarantees: `..` entries and symlinks that point
    /// outside the destination are rejected, never written.
    #[test]
    fn extract_zip_tree_rejects_traversal_and_escaping_symlink() {
        use std::io::Write;
        let opts = zip::write::SimpleFileOptions::default();

        let outer = tempfile::tempdir().unwrap();
        let dest = outer.path().join("dest");
        std::fs::create_dir(&dest).unwrap();

        let slip = zip_with(|zw| {
            zw.start_file("../escape.txt", opts).unwrap();
            zw.write_all(b"pwned").unwrap();
        });
        let _ = extract_zip_tree(std::io::Cursor::new(slip), &dest);
        assert!(!outer.path().join("escape.txt").exists());

        let link = zip_with(|zw| {
            zw.add_symlink("link", "../outside", opts).unwrap();
            zw.start_file("link/payload", opts).unwrap();
            zw.write_all(b"pwned").unwrap();
        });
        let _ = extract_zip_tree(std::io::Cursor::new(link), &dest);
        assert!(!outer.path().join("outside").exists());
        assert!(!outer.path().join("payload").exists());
    }

    #[test]
    fn zig_download_url_known_targets() {
        // Smoke: we don't hit the network in unit tests, just verify the
        // URL builder formats the asset name correctly.
        let (asset, url) =
            zig_download_url("0.13.0").expect("host should resolve in unit-test env");
        assert!(
            asset.starts_with("zig-") && asset.contains("-0.13.0."),
            "asset name should embed version: {asset}",
        );
        assert!(
            url.starts_with("https://ziglang.org/download/0.13.0/"),
            "url should target ziglang.org/download/<ver>/: {url}",
        );
        assert!(
            asset.ends_with(".tar.xz") || asset.ends_with(".zip"),
            "asset extension should match the OS family: {asset}",
        );
    }

    #[test]
    fn zig_download_url_naming_swap_at_0_14() {
        // Zig 0.13.0: pre-swap naming → `zig-{os}-{arch}-{ver}.tar.xz`.
        // Zig 0.14.0+: post-swap naming → `zig-{arch}-{os}-{ver}.tar.xz`.
        let (asset_13, _) = zig_download_url("0.13.0").unwrap();
        let (asset_14, _) = zig_download_url("0.14.1").unwrap();
        // Whatever host the unit tests run on, the arch + os tokens
        // must appear in swapped order between the two versions.
        let Some((os, arch)) = host_zig_os_arch() else {
            return;
        };
        let pre_13 = format!("zig-{os}-{arch}-0.13.0");
        let post_14 = format!("zig-{arch}-{os}-0.14.1");
        assert!(asset_13.starts_with(&pre_13), "0.13.0: {asset_13}");
        assert!(asset_14.starts_with(&post_14), "0.14.1: {asset_14}");
    }

    #[test]
    fn managed_zig_archive_root_swaps_with_version() {
        // The directory name inside the tarball mirrors the asset name.
        // 0.13.x uses `zig-{os}-{arch}-{ver}/`, 0.14+ uses
        // `zig-{arch}-{os}-{ver}/`. This test confirms managed_zig_archive_root
        // follows the same pre/post-0.14 branching as zig_download_url —
        // they MUST agree or extracts land in a directory the lookup misses
        // (soldr#1032).
        let Some((os, arch)) = host_zig_os_arch() else {
            return;
        };
        let root = managed_zig_archive_root();
        // MANAGED_ZIG_VERSION is currently 0.14.1 (post-swap).
        let expected = format!("zig-{arch}-{os}-{MANAGED_ZIG_VERSION}");
        assert_eq!(
            root, expected,
            "managed_zig_archive_root should match the 0.14+ post-swap layout"
        );
    }

    #[test]
    fn managed_zig_binary_path_falls_back_to_scan() {
        // Even if upstream changes the directory naming AGAIN in a future
        // release without soldr getting a coordinated bump, the scan
        // fallback should locate the binary. soldr#1032 hardening.
        let tmp = tempfile::tempdir().expect("tmpdir");
        // Create a directory whose name does NOT match the constructed
        // one, containing the zig binary.
        let surprise = tmp.path().join("zig-some-weird-future-naming-0.99.0");
        std::fs::create_dir_all(&surprise).unwrap();
        let zig = surprise.join(zig_binary_filename());
        std::fs::write(&zig, b"#!/bin/sh\necho fake\n").unwrap();
        let resolved = managed_zig_binary_path(tmp.path());
        assert_eq!(
            resolved, zig,
            "scan fallback should find the zig binary in any subdirectory"
        );
    }

    #[test]
    fn env_var_overrides_path_and_managed_install() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let exe = tmp.path().join(zig_binary_filename());
        std::fs::write(&exe, b"#!/bin/sh\necho fake-zig\n").expect("write");
        let prev = std::env::var_os(ZIG_ENV_VAR);
        std::env::set_var(ZIG_ENV_VAR, &exe);
        let resolved = zig_dir_from_env_var();
        match prev {
            Some(v) => std::env::set_var(ZIG_ENV_VAR, v),
            None => std::env::remove_var(ZIG_ENV_VAR),
        }
        assert_eq!(resolved.as_deref(), exe.parent());
    }

    #[test]
    fn builtin_pin_exists_for_every_managed_zig_asset() {
        for (os, arch) in [
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("macos", "x86_64"),
            ("macos", "aarch64"),
            ("windows", "x86_64"),
            ("windows", "aarch64"),
        ] {
            let ext = if os == "windows" { "zip" } else { "tar.xz" };
            let asset = format!("zig-{arch}-{os}-{MANAGED_ZIG_VERSION}.{ext}");
            let pin = builtin_zig_sha256(MANAGED_ZIG_VERSION, &asset)
                .unwrap_or_else(|| panic!("no built-in sha256 pin for {asset}"));
            assert_eq!(pin.len(), 64, "{asset}: {pin}");
            assert!(pin
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        }
    }

    #[test]
    fn builtin_pin_verifies_under_strict_without_user_pin_file() {
        let (asset, _) = zig_download_url(MANAGED_ZIG_VERSION).unwrap();
        let pin = builtin_zig_sha256(MANAGED_ZIG_VERSION, &asset).expect("host asset pinned");
        let outcome = verify_zig_download(
            &asset,
            pin,
            &trust::PinnedChecksumStore::empty(),
            trust::TrustMode::Strict,
        )
        .expect("built-in pin satisfies strict mode");
        assert!(matches!(outcome, trust::VerifyOutcome::Verified { .. }));
        let bad = "0".repeat(64);
        assert!(verify_zig_download(
            &asset,
            &bad,
            &trust::PinnedChecksumStore::empty(),
            trust::TrustMode::Permissive,
        )
        .is_err());
    }

    fn fixture_zig_tar_xz(dir: &Path) -> PathBuf {
        // Large-ish payload so concurrent extractions overlap in time.
        let root = managed_zig_archive_root();
        let archive = dir.join("zig-fixture.tar.xz");
        let file = std::fs::File::create(&archive).unwrap();
        let xz = xz2::write::XzEncoder::new(file, 0);
        let mut builder = tar::Builder::new(xz);
        let mut add = |name: String, data: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, name, data).unwrap();
        };
        add(
            format!("{root}/{}", zig_binary_filename()),
            b"#!/bin/sh\necho zig\n",
        );
        let blob = vec![7u8; 256 * 1024];
        for i in 0..200 {
            add(format!("{root}/lib/file{i}.zig"), &blob);
        }
        builder.into_inner().unwrap().finish().unwrap();
        archive
    }

    #[test]
    fn concurrent_installs_do_not_corrupt_the_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = fixture_zig_tar_xz(tmp.path());
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        // Four installers released together: unlocked in-place extraction
        // deletes a peer's half-extracted tree (soldr#3682).
        let barrier = std::sync::Barrier::new(4);
        let results: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        install_zig_archive(&bin_dir, &archive, "zig-fixture.tar.xz")
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for result in &results {
            let dir = result.as_ref().expect("install succeeds");
            assert!(dir.join(zig_binary_filename()).is_file());
        }
        let install_dir = bin_dir.join(format!("zig-{MANAGED_ZIG_VERSION}"));
        assert!(install_dir.join(".complete").is_file());
        let lib = install_dir.join(managed_zig_archive_root()).join("lib");
        assert_eq!(std::fs::read_dir(&lib).unwrap().count(), 200, "tree intact");
        // No staging/backup debris left beside the install.
        let leftovers: Vec<_> = std::fs::read_dir(&bin_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with(".zig-") || !n.ends_with(".lock"))
            .filter(|n| n != &format!("zig-{MANAGED_ZIG_VERSION}"))
            .collect();
        assert!(leftovers.is_empty(), "debris: {leftovers:?}");
    }
}
