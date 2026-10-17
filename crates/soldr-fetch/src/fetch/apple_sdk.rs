//! Auto-bootstrap the Apple macOS SDK for `*-apple-darwin`
//! cross-compiles.
//!
//! Resolution order:
//!   1. `SDKROOT` pointing at an existing directory.
//!   2. `xcrun --show-sdk-path` on macOS hosts with Xcode installed.
//!   3. A managed SDK row from the soldr-toolchain catalogue, selected
//!      by `(SDK version, SDK shape)` and sha256-verified before
//!      extraction.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::{SoldrError, SoldrPaths};

use super::manifest_lookup;
use super::tar_extract::unpack_tar_filtered;

/// Pinned macOS SDK version used when the caller does not set
/// `SOLDR_APPLE_SDK_VERSION`.
///
/// 14.5 is the first catalogue generation with per-arch thin SDK rows
/// (`darwin-x86_64`, `darwin-aarch64`) as well as `darwin-universal2`.
pub const MANAGED_APPLE_SDK_VERSION: &str = "14.5";

/// Directory basename expected inside the default SDK tarball.
pub const MANAGED_APPLE_SDK_DIRNAME: &str = "MacOSX14.5.sdk";

/// Compatibility constant for code/tests that check the managed SDK URL
/// shape. Runtime fetches are target-aware and call [`asset_url_for`]
/// instead of using this universal2 URL directly.
pub const MANAGED_APPLE_SDK_URL: &str =
    "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/apple-sdk/14.5/darwin-universal2/sdk.tar.zst";

/// Compatibility sha for [`MANAGED_APPLE_SDK_URL`]. Runtime fetches use
/// the matching sha from `catalogue.v1.json`.
pub const MANAGED_APPLE_SDK_SHA256: &str =
    "6f9dec0ac082309c4a2ee25733c2c40324f5d059e89b1e073db53462d62d2ff4";

const SDKROOT_ENV_VAR: &str = "SDKROOT";
const LEGACY_APPLE_SDK_VERSION: &str = "11.3";
const APPLE_SDK_EXTRACTION_FORMAT: &str = "apple-sdk-extract-v2";

/// Env var that pins the Apple SDK version soldr fetches.
pub const APPLE_SDK_VERSION_ENV_VAR: &str = "SOLDR_APPLE_SDK_VERSION";

/// Env var that pins the Apple SDK shape.
pub const APPLE_SDK_SHAPE_ENV_VAR: &str = "SOLDR_APPLE_SDK_SHAPE";

/// Versions soldr knows how to request from the toolchain catalogue.
pub const SUPPORTED_APPLE_SDK_VERSIONS: &[&str] = &["11.3", "13.3", "14.5", "15.2"];

/// Apple SDK packaging shapes a consumer can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppleSdkShape {
    /// `darwin-universal2` - fat artifact carrying both macOS slices.
    Universal2,
    /// `darwin-x86_64` - lipo-thinned to x86_64 only.
    ThinX86_64,
    /// `darwin-aarch64` - lipo-thinned to arm64 only.
    ThinAArch64,
}

impl AppleSdkShape {
    /// Catalogue-path slug matching the soldr-toolchain layout:
    /// `apple-sdk/<version>/<slug>/sdk.tar.zst`.
    pub fn catalogue_slug(&self) -> &'static str {
        match self {
            AppleSdkShape::Universal2 => "darwin-universal2",
            AppleSdkShape::ThinX86_64 => "darwin-x86_64",
            AppleSdkShape::ThinAArch64 => "darwin-aarch64",
        }
    }
}

/// Concrete managed SDK row soldr will fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppleSdkSelection {
    pub version: String,
    pub shape: AppleSdkShape,
}

enum ShapeEnv {
    Auto,
    Explicit(AppleSdkShape),
}

fn parse_shape_env() -> ShapeEnv {
    let Ok(value) = std::env::var(APPLE_SDK_SHAPE_ENV_VAR) else {
        return ShapeEnv::Auto;
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "universal2" => ShapeEnv::Explicit(AppleSdkShape::Universal2),
        "thin-x86_64" | "thin_x86_64" => ShapeEnv::Explicit(AppleSdkShape::ThinX86_64),
        "thin-aarch64" | "thin_aarch64" => ShapeEnv::Explicit(AppleSdkShape::ThinAArch64),
        "auto" | "" => ShapeEnv::Auto,
        other => {
            eprintln!(
                "soldr: warning: {APPLE_SDK_SHAPE_ENV_VAR}={other:?} not recognized; \
                 falling back to auto."
            );
            ShapeEnv::Auto
        }
    }
}

fn auto_shape_for_target(target_triple: Option<&str>) -> AppleSdkShape {
    let Some(triple) = target_triple else {
        return AppleSdkShape::Universal2;
    };
    if triple.starts_with("x86_64-apple-darwin") {
        AppleSdkShape::ThinX86_64
    } else if triple.starts_with("aarch64-apple-darwin") {
        AppleSdkShape::ThinAArch64
    } else {
        AppleSdkShape::Universal2
    }
}

/// Resolve the Apple SDK shape soldr should fetch for `target`.
/// Precedence: `SOLDR_APPLE_SDK_SHAPE` > target-derived auto.
pub fn resolve_apple_sdk_shape(target_triple: Option<&str>) -> AppleSdkShape {
    match parse_shape_env() {
        ShapeEnv::Explicit(shape) => shape,
        ShapeEnv::Auto => auto_shape_for_target(target_triple),
    }
}

/// Resolve the Apple SDK version soldr should fetch.
pub fn resolve_apple_sdk_version() -> String {
    if let Ok(value) = std::env::var(APPLE_SDK_VERSION_ENV_VAR) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            if SUPPORTED_APPLE_SDK_VERSIONS.contains(&trimmed) {
                return trimmed.to_string();
            }
            eprintln!(
                "soldr: warning: {APPLE_SDK_VERSION_ENV_VAR}={trimmed:?} not in supported set \
                 {SUPPORTED_APPLE_SDK_VERSIONS:?}; falling back to {MANAGED_APPLE_SDK_VERSION}."
            );
        }
    }
    MANAGED_APPLE_SDK_VERSION.to_string()
}

/// Resolve the SDK row soldr should fetch for `target`.
///
/// The legacy 11.3 catalogue only has `darwin-universal2`, so auto mode
/// intentionally keeps that shape for 11.3. Explicit shape overrides are
/// still honored and will surface a catalogue miss if the row does not
/// exist.
pub fn resolve_apple_sdk_selection(target_triple: Option<&str>) -> AppleSdkSelection {
    let version = resolve_apple_sdk_version();
    let shape = match parse_shape_env() {
        ShapeEnv::Explicit(shape) => shape,
        ShapeEnv::Auto if version == LEGACY_APPLE_SDK_VERSION => AppleSdkShape::Universal2,
        ShapeEnv::Auto => auto_shape_for_target(target_triple),
    };
    AppleSdkSelection { version, shape }
}

/// URL substring identifying the catalogue row for a given
/// `(version, shape)`.
pub fn catalogue_url_substr(version: &str, shape: AppleSdkShape) -> String {
    format!(
        "/apple-sdk/{}/{}/",
        catalogue_version_segment(version),
        shape.catalogue_slug()
    )
}

/// Archive filename for the SDK row. The historical 11.3 row was
/// published as `.tar.zstd`; newer rows are `.tar.zst`.
pub fn sdk_archive_name(version: &str) -> &'static str {
    if version == LEGACY_APPLE_SDK_VERSION {
        "sdk.tar.zstd"
    } else {
        "sdk.tar.zst"
    }
}

/// Catalogue version path segment. The historical row lives under
/// `MacOSX11.3`; modern rows use the bare version string.
pub fn catalogue_version_segment(version: &str) -> String {
    if version == LEGACY_APPLE_SDK_VERSION {
        "MacOSX11.3".to_string()
    } else {
        version.to_string()
    }
}

pub fn sdk_dirname_for_version(version: &str) -> String {
    format!("MacOSX{version}.sdk")
}

pub fn asset_url_for(version: &str, shape: AppleSdkShape) -> String {
    format!(
        "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/apple-sdk/{}/{}/{}",
        catalogue_version_segment(version),
        shape.catalogue_slug(),
        sdk_archive_name(version)
    )
}

pub fn install_dir_for_selection(paths: &SoldrPaths, selection: &AppleSdkSelection) -> PathBuf {
    paths
        .bin
        .join("apple-sdk")
        .join(&selection.version)
        .join(selection.shape.catalogue_slug())
}

pub fn sdk_dir_for_selection(paths: &SoldrPaths, selection: &AppleSdkSelection) -> PathBuf {
    install_dir_for_selection(paths, selection).join(sdk_dirname_for_version(&selection.version))
}

pub fn sdk_dir_for_target(paths: &SoldrPaths, target_triple: Option<&str>) -> PathBuf {
    let selection = resolve_apple_sdk_selection(target_triple);
    sdk_dir_for_selection(paths, &selection)
}

fn expected_cache_stamp(selection: &AppleSdkSelection) -> String {
    format!(
        "{APPLE_SDK_EXTRACTION_FORMAT} {} {}",
        selection.version,
        selection.shape.catalogue_slug()
    )
}

fn cache_stamp_is_current(stamp: &Path, selection: &AppleSdkSelection) -> bool {
    std::fs::read_to_string(stamp)
        .map(|contents| contents.trim() == expected_cache_stamp(selection))
        .unwrap_or(false)
}

/// Ensure an Apple macOS SDK is available. Returns the path of the
/// `*.sdk` directory so callers can set `SDKROOT`.
pub async fn ensure_apple_sdk(
    paths: &SoldrPaths,
    target_triple: Option<&str>,
) -> Result<PathBuf, SoldrError> {
    if let Some(sdk) = sdk_from_env_var() {
        return Ok(sdk);
    }
    if let Some(sdk) = sdk_from_xcrun() {
        return Ok(sdk);
    }
    fetch_managed_sdk(paths, target_triple).await
}

fn sdk_from_env_var() -> Option<PathBuf> {
    let value = std::env::var_os(SDKROOT_ENV_VAR)?;
    let p = PathBuf::from(value);
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

/// On macOS hosts with Xcode installed, `xcrun --show-sdk-path` returns
/// the SDK path Apple's tooling expects. Use it when present so we do
/// not redundantly fetch our own copy on developer macs.
fn sdk_from_xcrun() -> Option<PathBuf> {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::MacOs {
        return None;
    }
    let out = Command::new("xcrun")
        .args(["--show-sdk-path"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path_str = String::from_utf8(out.stdout).ok()?;
    let p = PathBuf::from(path_str.trim());
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

async fn fetch_managed_sdk(
    paths: &SoldrPaths,
    target_triple: Option<&str>,
) -> Result<PathBuf, SoldrError> {
    paths.ensure_dirs()?;
    let selection = resolve_apple_sdk_selection(target_triple);
    let install_dir = install_dir_for_selection(paths, &selection);
    let stamp = install_dir.join(".complete");
    let expected_sdk_dir = sdk_dir_for_selection(paths, &selection);

    if cache_stamp_is_current(&stamp, &selection) {
        if let Ok(found) = find_extracted_sdk_dir(&install_dir, &expected_sdk_dir) {
            if validate_extracted_sdk(&found).is_ok() {
                return Ok(found);
            }
        }
    }

    let lock_key = format!("{}-{}", selection.version, selection.shape.catalogue_slug());
    let _install_lock =
        super::syslib_common::acquire_install_lock(&paths.bin.join("apple-sdk"), &lock_key)?;
    if cache_stamp_is_current(&stamp, &selection) {
        if let Ok(found) = find_extracted_sdk_dir(&install_dir, &expected_sdk_dir) {
            if validate_extracted_sdk(&found).is_ok() {
                return Ok(found);
            }
        }
    }

    let url = asset_url_for(&selection.version, selection.shape);
    let source_path = format!(
        "apple-sdk/{}/{}/{}",
        catalogue_version_segment(&selection.version),
        selection.shape.catalogue_slug(),
        sdk_archive_name(&selection.version)
    );
    let entry = catalogue_entry_for_source_path(&source_path)
        .await
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "Apple SDK {}/{} not found in the soldr-toolchain catalogue. Expected URL: {url}",
                selection.version,
                selection.shape.catalogue_slug()
            ))
        })?;
    let resolved_url = manifest_lookup::resolved_download_label(&entry);

    eprintln!(
        "soldr: fetching Apple SDK {}/{} from {resolved_url}...",
        selection.version,
        selection.shape.catalogue_slug()
    );

    let downloaded = manifest_lookup::materialize_catalogue_entry(paths, &entry).await?;
    let digest = downloaded.sha256();
    eprintln!(
        "soldr: trust: verified Apple SDK {}/{} sha256={digest}",
        selection.version,
        selection.shape.catalogue_slug()
    );

    // Extract into a unique sibling first.  In particular, never delete a
    // previously-good SDK before the new archive has been completely
    // validated: a failed download/extraction must not leave a partial
    // canonical directory behind (Windows retries otherwise become fragile).
    let parent = install_dir
        .parent()
        .ok_or_else(|| SoldrError::Archive("Apple SDK install path has no parent".into()))?;
    std::fs::create_dir_all(parent)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let staging = parent.join(format!(".staging-{}-{}", std::process::id(), nonce));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;
    let result = (|| -> Result<PathBuf, SoldrError> {
        extract_tar_zst_tree(std::fs::File::open(downloaded.path())?, &staging)?;
        let sdk_dir = find_extracted_sdk_dir(
            &staging,
            &staging.join(expected_sdk_dir.file_name().unwrap_or_default()),
        )?;
        validate_extracted_sdk(&sdk_dir)?;
        let sdk_relative = sdk_dir
            .strip_prefix(&staging)
            .map_err(|_| SoldrError::Archive("Apple SDK path escaped staging directory".into()))?
            .to_path_buf();
        std::fs::write(staging.join(".complete"), expected_cache_stamp(&selection))?;

        // Promote only after extraction and validation have succeeded.  A
        // pre-existing install is moved aside rather than synchronously
        // deleted, which keeps retries safe on Windows where files may still
        // be held open by a compiler process.
        if install_dir.exists() {
            let backup = parent.join(format!(".previous-{}-{}", std::process::id(), nonce));
            std::fs::rename(&install_dir, &backup)?;
            if let Err(err) = std::fs::rename(&staging, &install_dir) {
                let _ = std::fs::rename(&backup, &install_dir);
                return Err(err.into());
            }
            let _ = std::fs::remove_dir_all(backup);
        } else {
            std::fs::rename(&staging, &install_dir)?;
        }
        Ok(install_dir.join(sdk_relative))
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    let sdk_dir = result?;
    eprintln!("soldr: extracted Apple SDK to {}", sdk_dir.display());
    Ok(sdk_dir)
}

async fn catalogue_entry_for_source_path(
    source_path: &str,
) -> Option<manifest_lookup::ManifestEntry> {
    let index = manifest_lookup::get_or_fetch().await;
    index
        .entries
        .iter()
        .find(|e| e.source_path.as_deref() == Some(source_path))
        .cloned()
}

fn find_extracted_sdk_dir(install_dir: &Path, expected: &Path) -> Result<PathBuf, SoldrError> {
    if expected.is_dir() {
        return Ok(expected.to_path_buf());
    }
    let mut candidates = Vec::new();
    if install_dir.is_dir() {
        collect_sdk_dirs(install_dir, &mut candidates)?;
    }
    if let Some(expected_name) = expected.file_name() {
        let mut matching_name: Vec<PathBuf> = candidates
            .iter()
            .filter(|path| path.file_name() == Some(expected_name))
            .cloned()
            .collect();
        if matching_name.len() == 1 {
            return Ok(matching_name.remove(0));
        }
    }
    if candidates.len() == 1 {
        return Ok(candidates.remove(0));
    }
    Err(SoldrError::Archive(format!(
        "Apple SDK extract did not produce expected directory {}",
        expected.display()
    )))
}

fn collect_sdk_dirs(root: &Path, out: &mut Vec<PathBuf>) -> Result<(), SoldrError> {
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        if is_apple_sdk_dir(&path) {
            out.push(path);
        } else {
            collect_sdk_dirs(&path, out)?;
        }
    }
    Ok(())
}

fn is_apple_sdk_dir(path: &Path) -> bool {
    if path
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|name| name.ends_with(".sdk"))
    {
        return true;
    }

    // The thin 14.5 catalogue archives are Conan-style packages:
    // `package/sdk/{SDKSettings.plist,SDKSettings.json,System,usr}`.
    // Accept that SDK root even though the directory name is not
    // `MacOSX*.sdk`.
    (path.join("SDKSettings.plist").is_file() || path.join("SDKSettings.json").is_file())
        && path.join("System").is_dir()
        && path.join("usr").is_dir()
}

fn validate_extracted_sdk(sdk_dir: &Path) -> Result<(), SoldrError> {
    let pthread = sdk_dir.join("usr/include/pthread.h");
    let pthread_meta = std::fs::metadata(&pthread).map_err(|err| {
        SoldrError::Archive(format!(
            "Apple SDK validation failed: {} is unreadable: {err}",
            pthread.display()
        ))
    })?;
    if pthread_meta.is_dir() {
        return Err(SoldrError::Archive(format!(
            "Apple SDK validation failed: {} resolves to a directory, expected a header file",
            pthread.display()
        )));
    }
    std::fs::File::open(&pthread).map_err(|err| {
        SoldrError::Archive(format!(
            "Apple SDK validation failed: {} cannot be opened: {err}",
            pthread.display()
        ))
    })?;

    // Framework Headers is a chained directory-link path in Apple's SDKs:
    // Headers -> Versions/Current/Headers and Current -> A. Traversal proves
    // both target separator normalization and NTFS directory-link flavor.
    let framework_headers =
        sdk_dir.join("System/Library/Frameworks/CoreFoundation.framework/Headers");
    if !framework_headers.is_dir() {
        return Err(SoldrError::Archive(format!(
            "Apple SDK validation failed: {} is not a traversable directory",
            framework_headers.display()
        )));
    }
    std::fs::read_dir(&framework_headers).map_err(|err| {
        SoldrError::Archive(format!(
            "Apple SDK validation failed: {} cannot be enumerated: {err}",
            framework_headers.display()
        ))
    })?;
    Ok(())
}

fn extract_tar_zst_tree<R: std::io::Read>(reader: R, dest: &Path) -> Result<(), SoldrError> {
    let zst = zstd::stream::read::Decoder::new(reader)
        .map_err(|e| SoldrError::Archive(format!("zstd decoder init: {e}")))?;
    let mut archive = tar::Archive::new(zst);
    unpack_tar_filtered(&mut archive, dest, |path| {
        if is_optional_man_entry(path) {
            return Ok(true);
        }
        validate_archive_path(path)?;
        Ok(false)
    })
}

fn is_optional_man_entry(path: &Path) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    normalized == "package/sdk/usr/share/man"
        || normalized.starts_with("package/sdk/usr/share/man/")
}

fn validate_archive_path(path: &Path) -> Result<(), SoldrError> {
    use std::path::Component;
    if path.is_absolute()
        || path.components().any(|component| match component {
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => true,
            Component::Normal(value) => value.to_string_lossy().contains(':') || value == ".",
            Component::CurDir => false,
        })
    {
        return Err(SoldrError::Archive(format!(
            "Apple SDK archive contains unsafe path {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{OsStr, OsString};

    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn env_var_overrides_when_pointing_at_real_dir() {
        let _lock = ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().expect("tmpdir");
        let fake_sdk = tmp.path().join("FakeMacOSX.sdk");
        std::fs::create_dir_all(&fake_sdk).expect("mk");
        let _guard = EnvVarGuard::set(SDKROOT_ENV_VAR, &fake_sdk);
        let resolved = sdk_from_env_var();
        assert_eq!(resolved.as_deref(), Some(fake_sdk.as_path()));
    }

    #[test]
    fn env_var_ignored_when_path_is_missing() {
        let _lock = ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _guard = EnvVarGuard::set(SDKROOT_ENV_VAR, "/definitely/not/a/real/path/3819237");
        let resolved = sdk_from_env_var();
        assert!(
            resolved.is_none(),
            "missing dir should be ignored: {resolved:?}"
        );
    }

    #[test]
    fn constants_are_well_formed() {
        assert_eq!(MANAGED_APPLE_SDK_VERSION, "14.5");
        assert_eq!(MANAGED_APPLE_SDK_DIRNAME, "MacOSX14.5.sdk");
        assert!(MANAGED_APPLE_SDK_URL.starts_with("https://"));
        assert!(MANAGED_APPLE_SDK_URL.ends_with(".tar.zst"));
        assert_eq!(MANAGED_APPLE_SDK_SHA256.len(), 64);
        assert!(MANAGED_APPLE_SDK_SHA256
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn shape_slugs_match_catalogue_layout() {
        assert_eq!(
            AppleSdkShape::Universal2.catalogue_slug(),
            "darwin-universal2"
        );
        assert_eq!(AppleSdkShape::ThinX86_64.catalogue_slug(), "darwin-x86_64");
        assert_eq!(
            AppleSdkShape::ThinAArch64.catalogue_slug(),
            "darwin-aarch64"
        );
    }

    #[test]
    fn catalogue_url_substr_format() {
        assert_eq!(
            catalogue_url_substr("14.5", AppleSdkShape::Universal2),
            "/apple-sdk/14.5/darwin-universal2/"
        );
        assert_eq!(
            catalogue_url_substr("14.5", AppleSdkShape::ThinX86_64),
            "/apple-sdk/14.5/darwin-x86_64/"
        );
        assert_eq!(
            catalogue_url_substr("11.3", AppleSdkShape::Universal2),
            "/apple-sdk/MacOSX11.3/darwin-universal2/"
        );
    }

    #[test]
    fn asset_url_layout_matches_live_catalogue_rows() {
        assert_eq!(
            asset_url_for("14.5", AppleSdkShape::ThinX86_64),
            "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/apple-sdk/14.5/darwin-x86_64/sdk.tar.zst"
        );
        assert_eq!(
            asset_url_for("14.5", AppleSdkShape::ThinAArch64),
            "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/apple-sdk/14.5/darwin-aarch64/sdk.tar.zst"
        );
        assert_eq!(
            asset_url_for("11.3", AppleSdkShape::Universal2),
            "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/apple-sdk/MacOSX11.3/darwin-universal2/sdk.tar.zstd"
        );
    }

    #[test]
    fn selection_defaults_to_target_specific_14_5_rows() {
        let _lock = ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _shape = EnvVarGuard::remove(APPLE_SDK_SHAPE_ENV_VAR);
        let _version = EnvVarGuard::remove(APPLE_SDK_VERSION_ENV_VAR);

        assert_eq!(
            resolve_apple_sdk_selection(Some("x86_64-apple-darwin")),
            AppleSdkSelection {
                version: "14.5".to_string(),
                shape: AppleSdkShape::ThinX86_64,
            }
        );
        assert_eq!(
            resolve_apple_sdk_selection(Some("aarch64-apple-darwin")),
            AppleSdkSelection {
                version: "14.5".to_string(),
                shape: AppleSdkShape::ThinAArch64,
            }
        );
    }

    #[test]
    fn sdk_cache_path_includes_version_and_shape() {
        let _lock = ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _shape = EnvVarGuard::remove(APPLE_SDK_SHAPE_ENV_VAR);
        let _version = EnvVarGuard::remove(APPLE_SDK_VERSION_ENV_VAR);
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().join("soldr"));

        let x86 = sdk_dir_for_target(&paths, Some("x86_64-apple-darwin"));
        assert!(
            x86.ends_with("apple-sdk/14.5/darwin-x86_64/MacOSX14.5.sdk")
                || x86.ends_with("apple-sdk\\14.5\\darwin-x86_64\\MacOSX14.5.sdk")
        );

        let _version_113 = EnvVarGuard::set(APPLE_SDK_VERSION_ENV_VAR, "11.3");
        let legacy = sdk_dir_for_target(&paths, Some("x86_64-apple-darwin"));
        assert!(
            legacy.ends_with("apple-sdk/11.3/darwin-universal2/MacOSX11.3.sdk")
                || legacy.ends_with("apple-sdk\\11.3\\darwin-universal2\\MacOSX11.3.sdk")
        );
        assert_ne!(x86, legacy);
    }

    #[test]
    fn picker_env_var_behaviour_serial() {
        let _lock = ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _shape = EnvVarGuard::remove(APPLE_SDK_SHAPE_ENV_VAR);
        let _version = EnvVarGuard::remove(APPLE_SDK_VERSION_ENV_VAR);

        assert_eq!(
            resolve_apple_sdk_shape(Some("x86_64-apple-darwin")),
            AppleSdkShape::ThinX86_64
        );
        assert_eq!(
            resolve_apple_sdk_shape(Some("aarch64-apple-darwin")),
            AppleSdkShape::ThinAArch64
        );
        assert_eq!(
            resolve_apple_sdk_shape(Some("x86_64-unknown-linux-gnu")),
            AppleSdkShape::Universal2,
            "non-darwin target falls back to universal2"
        );
        assert_eq!(
            resolve_apple_sdk_shape(None),
            AppleSdkShape::Universal2,
            "missing target falls back to universal2"
        );

        for (env_value, expected) in [
            ("universal2", AppleSdkShape::Universal2),
            ("thin-x86_64", AppleSdkShape::ThinX86_64),
            ("thin-aarch64", AppleSdkShape::ThinAArch64),
            ("Universal2", AppleSdkShape::Universal2),
            ("thin_x86_64", AppleSdkShape::ThinX86_64),
        ] {
            let _shape_override = EnvVarGuard::set(APPLE_SDK_SHAPE_ENV_VAR, env_value);
            assert_eq!(
                resolve_apple_sdk_shape(Some("aarch64-apple-darwin")),
                expected,
                "{env_value} should override target-arch detection"
            );
        }

        let _auto = EnvVarGuard::set(APPLE_SDK_SHAPE_ENV_VAR, "auto");
        assert_eq!(
            resolve_apple_sdk_shape(Some("x86_64-apple-darwin")),
            AppleSdkShape::ThinX86_64
        );

        let _version_145 = EnvVarGuard::set(APPLE_SDK_VERSION_ENV_VAR, "14.5");
        assert_eq!(resolve_apple_sdk_version(), "14.5");
        drop(_version_145);

        let _version_bad = EnvVarGuard::set(APPLE_SDK_VERSION_ENV_VAR, "99.99");
        assert_eq!(
            resolve_apple_sdk_version(),
            MANAGED_APPLE_SDK_VERSION,
            "unsupported version falls back to default"
        );
        drop(_version_bad);

        assert_eq!(resolve_apple_sdk_version(), MANAGED_APPLE_SDK_VERSION);
    }

    #[test]
    fn legacy_11_3_auto_keeps_universal2_shape() {
        let _lock = ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _shape = EnvVarGuard::remove(APPLE_SDK_SHAPE_ENV_VAR);
        let _version = EnvVarGuard::set(APPLE_SDK_VERSION_ENV_VAR, "11.3");

        assert_eq!(
            resolve_apple_sdk_selection(Some("x86_64-apple-darwin")),
            AppleSdkSelection {
                version: "11.3".to_string(),
                shape: AppleSdkShape::Universal2,
            }
        );
    }

    #[test]
    fn supported_versions_include_default() {
        assert!(SUPPORTED_APPLE_SDK_VERSIONS.contains(&MANAGED_APPLE_SDK_VERSION));
    }

    #[test]
    fn cache_stamp_requires_current_extraction_format() {
        let selection = AppleSdkSelection {
            version: "14.5".to_string(),
            shape: AppleSdkShape::ThinX86_64,
        };
        let temp = tempfile::tempdir().expect("tempdir");
        let stamp = temp.path().join(".complete");
        std::fs::write(&stamp, "14.5 darwin-x86_64").expect("write legacy stamp");
        assert!(
            !cache_stamp_is_current(&stamp, &selection),
            "legacy extraction stamps must trigger self-healing"
        );
        std::fs::write(&stamp, expected_cache_stamp(&selection)).expect("write current stamp");
        assert!(cache_stamp_is_current(&stamp, &selection));
    }

    #[test]
    fn validate_extracted_sdk_requires_readable_header_and_framework_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sdk = temp.path().join("MacOSX14.5.sdk");
        let pthread = sdk.join("usr/include/pthread.h");
        let headers = sdk.join("System/Library/Frameworks/CoreFoundation.framework/Headers");
        std::fs::create_dir_all(headers).expect("create framework headers");
        std::fs::create_dir_all(pthread.parent().expect("header parent"))
            .expect("create include dir");
        std::fs::write(&pthread, "header").expect("write pthread header");
        assert!(validate_extracted_sdk(&sdk).is_ok());

        std::fs::remove_file(&pthread).expect("remove pthread header");
        let err = validate_extracted_sdk(&sdk).expect_err("missing header must fail validation");
        assert!(err.to_string().contains("pthread.h"));
    }

    #[test]
    fn find_extracted_sdk_dir_accepts_single_sdk_dir() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let expected = tmp.path().join("MacOSX14.5.sdk");
        let fallback = tmp.path().join("SomeOtherName.sdk");
        std::fs::create_dir_all(&fallback).expect("sdk dir");
        assert_eq!(
            find_extracted_sdk_dir(tmp.path(), &expected).expect("single sdk fallback"),
            fallback
        );
    }

    #[test]
    fn find_extracted_sdk_dir_accepts_nested_sdk_dir() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let expected = tmp.path().join("MacOSX14.5.sdk");
        let nested = tmp
            .path()
            .join("package")
            .join("payload")
            .join("MacOSX14.5.sdk");
        std::fs::create_dir_all(&nested).expect("sdk dir");
        assert_eq!(
            find_extracted_sdk_dir(tmp.path(), &expected).expect("nested sdk fallback"),
            nested
        );
    }

    #[test]
    fn find_extracted_sdk_dir_accepts_conan_package_sdk_root() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let expected = tmp.path().join("MacOSX14.5.sdk");
        let sdk = tmp.path().join("package").join("sdk");
        std::fs::create_dir_all(sdk.join("System")).expect("System dir");
        std::fs::create_dir_all(sdk.join("usr")).expect("usr dir");
        std::fs::write(sdk.join("SDKSettings.plist"), "<plist/>").expect("SDKSettings");
        assert_eq!(
            find_extracted_sdk_dir(tmp.path(), &expected).expect("package sdk fallback"),
            sdk
        );
    }

    #[test]
    fn find_extracted_sdk_dir_prefers_expected_name_when_nested() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let expected = tmp.path().join("MacOSX14.5.sdk");
        let wanted = tmp.path().join("thin").join("MacOSX14.5.sdk");
        let other = tmp.path().join("other").join("MacOSX15.2.sdk");
        std::fs::create_dir_all(&wanted).expect("wanted sdk dir");
        std::fs::create_dir_all(&other).expect("other sdk dir");
        assert_eq!(
            find_extracted_sdk_dir(tmp.path(), &expected).expect("expected name wins"),
            wanted
        );
    }

    #[test]
    fn extract_skips_windows_invalid_manpage_names() {
        let mut raw = Vec::new();
        {
            let encoder = zstd::stream::write::Encoder::new(&mut raw, 1).expect("zstd");
            let mut builder = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(
                    &mut header,
                    "package/sdk/usr/share/man/foo:bar",
                    &b"man"[..],
                )
                .expect("manpage");
            for dir in [
                "package/",
                "package/sdk/",
                "package/sdk/usr/",
                "package/sdk/usr/lib/",
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_mode(0o755);
                header.set_cksum();
                builder
                    .append_data(&mut header, dir, std::io::empty())
                    .expect("directory");
            }
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(
                    &mut header,
                    "package/sdk/usr/lib/libSystem.tbd",
                    &b"lib"[..],
                )
                .expect("library");
            let encoder = builder.into_inner().expect("finish tar");
            encoder.finish().expect("finish zstd");
        }
        let dest = tempfile::tempdir().expect("dest");
        extract_tar_zst_tree(std::io::Cursor::new(raw), dest.path()).expect("extract");
        assert!(!dest
            .path()
            .join("package/sdk/usr/share/man/foo:bar")
            .exists());
        assert!(dest
            .path()
            .join("package/sdk/usr/lib/libSystem.tbd")
            .is_file());
    }

    #[test]
    fn extract_rejects_invalid_paths_outside_optional_man_tree() {
        let mut raw = Vec::new();
        {
            let encoder = zstd::stream::write::Encoder::new(&mut raw, 1).expect("zstd");
            let mut builder = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "package/sdk/usr/lib/bad:name", &b"bad"[..])
                .expect("entry");
            let encoder = builder.into_inner().expect("finish tar");
            encoder.finish().expect("finish zstd");
        }
        let dest = tempfile::tempdir().expect("dest");
        let err =
            extract_tar_zst_tree(std::io::Cursor::new(raw), dest.path()).expect_err("must reject");
        assert!(err.to_string().contains("unsafe path"));
    }
}
