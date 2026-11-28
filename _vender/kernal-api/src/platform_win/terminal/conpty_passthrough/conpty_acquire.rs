//! Transparent Win10 ConPTY sidecar self-acquisition (#445).
//!
//! On Windows 10 the system `kernel32!CreatePseudoConsole` silently
//! ignores `PSEUDOCONSOLE_PASSTHROUGH_MODE`. #443 added a
//! "sidecar `conpty.dll` next to the host exe" lookup, but that put
//! the bundling burden on every consumer. This module makes the
//! library self-acquiring instead: on first ConPTY use, the
//! Microsoft redistributable is fetched from a pinned GitHub
//! release asset, decompressed, and cached under the platform
//! cache directory. Subsequent runs find it in the cache and skip
//! the network entirely.
//!
//! Trust root: HTTPS to github.com plus GitHub's content-locked
//! release assets. No SHA pin in this revision — release assets
//! cannot be mutated after upload and an attacker who can replace
//! the asset can also replace the crate. We rely on HTTPS + the
//! maintainer's account integrity, the same trust root the crate's
//! own publication depends on.
//!
//! Failure mode: any error path (no network, cache write failure,
//! decompression error, asset missing for this `CARGO_PKG_VERSION`)
//! returns `io::Error`. Caller (`conpty_api::get`) logs and falls
//! back to `kernel32`. No crash. The error path is also exercised
//! by `KERNAL_API_CONPTY_OFFLINE=1` for air-gapped hosts.

#![cfg(windows)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Asset arch directory string. Compile-time constant per target.
pub(super) const fn arch_dir() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        "x64"
    }
    #[cfg(target_arch = "aarch64")]
    {
        "arm64"
    }
    #[cfg(target_arch = "x86")]
    {
        "x86"
    }
    #[cfg(target_arch = "arm")]
    {
        "arm"
    }
    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "x86",
        target_arch = "arm"
    )))]
    {
        compile_error!("Windows builds must target x86_64, aarch64, x86, or arm");
    }
}

static CACHED_SIDECAR_DIR: OnceLock<io::Result<PathBuf>> = OnceLock::new();

/// Returns the path to a directory containing a verified `conpty.dll`
/// (and `OpenConsole.exe` alongside it). Resolves via:
///
/// 1. Cache hit at `<cache_root>/<rp-version>/<arch>/`.
/// 2. HTTP fetch of the GitHub release asset for this
///    `CARGO_PKG_VERSION`, decompressed and atomically renamed into
///    the cache directory.
/// 3. `Err(io::Error)` on any failure — caller falls back to
///    `kernel32`.
///
/// The first-call result (success or failure) is cached for the
/// process lifetime via `OnceLock`. Repeated calls do not re-attempt
/// the fetch — preventing a hostile network from generating one
/// request per ConPTY open.
pub(super) fn ensure_cached_sidecar() -> io::Result<PathBuf> {
    let cached = CACHED_SIDECAR_DIR.get_or_init(|| {
        let cache_root = resolve_cache_root(std::env::var_os("KERNAL_API_CONPTY_CACHE"))?;
        let dir = cache_root
            .join("kernal-api")
            .join("conpty")
            .join(env!("CARGO_PKG_VERSION"))
            .join(arch_dir());
        ensure_in_dir(&dir)?;
        Ok(dir)
    });
    match cached {
        Ok(p) => Ok(p.clone()),
        Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
    }
}

/// Resolve the cache root, honoring the env-var override.
/// Defaults to the Local AppData known folder, the same path
/// `dirs::cache_dir()` returned before this module stopped using `dirs`.
fn resolve_cache_root(override_dir: Option<std::ffi::OsString>) -> io::Result<PathBuf> {
    if let Some(p) = override_dir {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    local_app_data_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no platform cache directory available",
        )
    })
}

/// `FOLDERID_LocalAppData` via `SHGetKnownFolderPath` with no flags and the
/// current user's token: exactly what `dirs::cache_dir()` (dirs-sys) asks
/// for on Windows, so the cache location is unchanged.
fn local_app_data_dir() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath};

    let mut raw: *mut u16 = std::ptr::null_mut();
    // SAFETY: the GUID is a live static, flags 0 and a null token select the
    // calling user's default path, and `raw` receives a CoTaskMem string.
    let hr = unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, std::ptr::null_mut(), &mut raw) };
    let path = if hr == 0 && !raw.is_null() {
        // SAFETY: on success `raw` is a NUL-terminated UTF-16 string owned by
        // us until `CoTaskMemFree` below; the scan stops at its terminator.
        let wide = unsafe {
            let mut len = 0;
            while *raw.add(len) != 0 {
                len += 1;
            }
            std::slice::from_raw_parts(raw, len)
        };
        Some(PathBuf::from(std::ffi::OsString::from_wide(wide)))
    } else {
        None
    };
    // SAFETY: the API requires freeing `raw` on success and failure alike;
    // `CoTaskMemFree` accepts null.
    unsafe { CoTaskMemFree(raw.cast()) };
    path
}

/// Stage the sidecar binaries into `cache_dir` (no-op if already
/// present). Public helper for unit tests; production code reaches it
/// through `ensure_cached_sidecar`.
pub(super) fn ensure_in_dir(cache_dir: &Path) -> io::Result<()> {
    let dll = cache_dir.join("conpty.dll");
    let exe = cache_dir.join("OpenConsole.exe");
    if dll.is_file() && exe.is_file() {
        diag(|| format!("ConPTY sidecar cache hit at {}", cache_dir.display()));
        return Ok(());
    }

    if std::env::var_os("KERNAL_API_CONPTY_OFFLINE").is_some() {
        diag(|| "ConPTY sidecar fetch suppressed (KERNAL_API_CONPTY_OFFLINE)".to_string());
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "offline mode: no cached sidecar and fetch disabled",
        ));
    }

    fetch_and_extract(cache_dir)
}

fn fetch_and_extract(cache_dir: &Path) -> io::Result<()> {
    let Some(expected) = super::conpty_sidecar_hashes::expected_for_current_arch() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "kernal-api was built without a verified ConPTY sidecar for {}",
                arch_dir()
            ),
        ));
    };
    fetch_and_extract_from(&asset_url(), expected, cache_dir)
}

/// Download `url`, verify it against `expected`, and extract it into
/// `cache_dir`. Production passes [`asset_url`] and the compile-time pin;
/// the live test passes a published release and that release's manifest.
fn fetch_and_extract_from(
    url: &str,
    expected: &super::conpty_sidecar_hashes::ExpectedAsset,
    cache_dir: &Path,
) -> io::Result<()> {
    diag(|| {
        format!(
            "ConPTY sidecar cache miss; fetching {} → {}",
            url,
            cache_dir.display()
        )
    });

    let bytes = http_get(url)
        .map_err(|e| io::Error::other(format!("conpty sidecar fetch from {url} failed: {e}")))?;

    // #447: SHA-256 verify the downloaded bytes against the compile-time
    // baseline before we touch them further. A mismatch discards the
    // fetch and falls through to the kernel32 path with a one-line
    // diagnostic; no decompression on bad bytes.
    {
        let actual_hex = sha256_hex(&bytes);
        if !ct_eq(&actual_hex, expected.sha256_hex) {
            diag(|| {
                format!(
                    "ConPTY sidecar SHA-256 mismatch — discarding fetch; \
                     expected {} (size {}), got {} (size {}); see #447",
                    expected.sha256_hex,
                    expected.size_bytes,
                    actual_hex,
                    bytes.len(),
                )
            });
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "conpty sidecar hash mismatch: expected {}, got {}",
                    expected.sha256_hex, actual_hex
                ),
            ));
        }
        if expected.size_bytes != 0 && expected.size_bytes != bytes.len() as u64 {
            // Collision-resistance means this branch is unreachable in
            // practice when the SHA matched, but log defensively if the
            // manifest carried a stale size_bytes field.
            diag(|| {
                format!(
                    "ConPTY sidecar size mismatch (sha matched): expected {} got {}",
                    expected.size_bytes,
                    bytes.len()
                )
            });
        }
    }

    let parent = cache_dir
        .parent()
        .ok_or_else(|| io::Error::other("cache dir has no parent"))?;
    fs::create_dir_all(parent)?;

    let tmp_dir = parent.join(format!(
        ".tmp-conpty-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir)?;

    extract_tar_zst(&bytes, &tmp_dir).map_err(|e| {
        let _ = fs::remove_dir_all(&tmp_dir);
        io::Error::other(format!("conpty sidecar archive extraction failed: {e}"))
    })?;

    if !tmp_dir.join("conpty.dll").is_file() || !tmp_dir.join("OpenConsole.exe").is_file() {
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "conpty sidecar archive missing conpty.dll or OpenConsole.exe",
        ));
    }

    match fs::rename(&tmp_dir, cache_dir) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            // Another process may have won the race and the final
            // dir now exists. Accept that outcome silently.
            if cache_dir.join("conpty.dll").is_file() && cache_dir.join("OpenConsole.exe").is_file()
            {
                let _ = fs::remove_dir_all(&tmp_dir);
                Ok(())
            } else {
                let _ = fs::remove_dir_all(&tmp_dir);
                Err(rename_err)
            }
        }
    }
}

fn asset_url() -> String {
    format!(
        "https://github.com/zackees/kernal-api/releases/download/v{ver}/conpty-sidecar-{arch}.tar.zst",
        ver = env!("CARGO_PKG_VERSION"),
        arch = arch_dir(),
    )
}

/// Default wall-clock bound on the ConPTY sidecar download. The fetch runs
/// on the first Win10 PTY spawn while holding the `API` `OnceLock`, so every
/// concurrent first-spawner stalls behind it until it completes or times out
/// (issue #590, cluster J). Kept bounded — and made overridable via
/// `KERNAL_API_CONPTY_SIDECAR_FETCH_TIMEOUT_MS` so operators can tighten
/// the worst-case first-spawn stall — so a slow/hostile mirror can never
/// wedge PTY creation.
const DEFAULT_SIDECAR_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const SIDECAR_FETCH_TIMEOUT_ENV: &str = "KERNAL_API_CONPTY_SIDECAR_FETCH_TIMEOUT_MS";

fn parse_sidecar_fetch_timeout(raw: Option<&str>) -> std::time::Duration {
    raw.and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map(std::time::Duration::from_millis)
        .unwrap_or(DEFAULT_SIDECAR_FETCH_TIMEOUT)
}

fn sidecar_fetch_timeout() -> std::time::Duration {
    parse_sidecar_fetch_timeout(std::env::var(SIDECAR_FETCH_TIMEOUT_ENV).ok().as_deref())
}

fn http_get(url: &str) -> Result<Vec<u8>, String> {
    // One deadline bounds the whole call, body read included, so a trickling
    // mirror cannot stall past it; see `winhttp_get` for the parity contract.
    super::winhttp_get::https_get(url, sidecar_fetch_timeout())
}

fn extract_tar_zst(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let decoder = zstd::Decoder::new(io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let mut archive = tar::Archive::new(decoder);
    archive.set_overwrite(true);
    archive.set_preserve_permissions(false);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        // Defend against path traversal in the tarball.
        if path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        }) {
            return Err(format!(
                "rejecting unsafe path in archive: {}",
                path.display()
            ));
        }
        let out_path = dest.join(&path);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        entry.unpack(&out_path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn diag(f: impl FnOnce() -> String) {
    if std::env::var_os("KERNAL_API_CONPTY_DIAGNOSTICS").is_some() {
        eprintln!("kernal-api: {}", f());
    }
}

/// Lowercase-hex SHA-256 of `bytes`. Kept as a helper so the runtime
/// fetch path and `verify_asset` produce the same string format.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

/// Constant-time string equality. Both inputs are expected to be
/// lowercase ASCII hex strings of the same length; the length check
/// itself short-circuits, but the byte comparison does not branch on
/// match status. Avoids pulling in `subtle` for a 7-line primitive.
fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Test-only verification helper exercised by the unit tests below.
/// Wraps the same SHA-256 comparison the runtime fetch path performs
/// inside `fetch_and_extract`, so a regression in either side is caught
/// here without needing a network fixture.
#[cfg(test)]
pub(super) fn verify_asset(
    bytes: &[u8],
    expected: &super::conpty_sidecar_hashes::ExpectedAsset,
) -> Result<(), String> {
    let actual = sha256_hex(bytes);
    if !ct_eq(&actual, expected.sha256_hex) {
        return Err(format!(
            "sha mismatch: expected {}, got {}",
            expected.sha256_hex, actual
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn sidecar_fetch_timeout_defaults_when_unset_or_invalid() {
        assert_eq!(
            parse_sidecar_fetch_timeout(None),
            DEFAULT_SIDECAR_FETCH_TIMEOUT
        );
        assert_eq!(
            parse_sidecar_fetch_timeout(Some("bogus")),
            DEFAULT_SIDECAR_FETCH_TIMEOUT
        );
        assert_eq!(
            parse_sidecar_fetch_timeout(Some("0")),
            DEFAULT_SIDECAR_FETCH_TIMEOUT
        );
    }

    #[test]
    fn sidecar_fetch_timeout_honors_valid_override() {
        assert_eq!(
            parse_sidecar_fetch_timeout(Some("5000")),
            std::time::Duration::from_millis(5000)
        );
    }

    fn stage_fake_sidecar(dir: &Path) {
        fs::create_dir_all(dir).expect("mkdir");
        for name in ["conpty.dll", "OpenConsole.exe"] {
            let mut f = fs::File::create(dir.join(name)).expect("create");
            f.write_all(b"fake content for test").expect("write");
        }
    }

    /// `ensure_in_dir` returns Ok without touching the network when
    /// the cache dir is already populated.
    #[test]
    fn cache_hit_returns_ok_without_network() {
        let tmp = tempfile::tempdir().unwrap();
        stage_fake_sidecar(tmp.path());
        ensure_in_dir(tmp.path()).expect("cache hit must succeed");
        // Files unchanged.
        assert!(tmp.path().join("conpty.dll").is_file());
        assert!(tmp.path().join("OpenConsole.exe").is_file());
    }

    /// In offline mode with no cache, `ensure_in_dir` returns an
    /// io::Error of kind NotFound — the caller (conpty_api) will use
    /// the kernel32 fallback path.
    #[test]
    fn offline_mode_returns_not_found_without_network() {
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: env var mutation is process-global, but this test
        // sets+removes synchronously and we don't depend on parallel
        // ordering with other tests for this env var.
        std::env::set_var("KERNAL_API_CONPTY_OFFLINE", "1");
        let result = ensure_in_dir(tmp.path());
        std::env::remove_var("KERNAL_API_CONPTY_OFFLINE");
        let err = result.expect_err("offline + empty cache must error");
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "got {err}");
    }

    /// `resolve_cache_root` honors the env-var override when non-empty.
    #[test]
    fn cache_root_override_used_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_cache_root(Some(tmp.path().as_os_str().to_owned())).unwrap();
        assert_eq!(resolved, tmp.path());
    }

    /// Empty override falls through to the platform default.
    #[test]
    fn empty_override_falls_through_to_platform() {
        let resolved = resolve_cache_root(Some(std::ffi::OsString::new())).unwrap();
        assert_eq!(resolved, local_app_data_dir().expect("platform cache dir"));
        // No comparison with %LOCALAPPDATA%: hosts may redirect the variable
        // independently of the known folder, and the known folder is the
        // contract (it is what `dirs::cache_dir()` returned).
        assert!(resolved.is_absolute(), "got {}", resolved.display());
    }

    /// Last published release whose sidecar assets the live test fetches.
    /// The crate's own `v{CARGO_PKG_VERSION}` assets are uploaded only after
    /// full CI passes on the release candidate, so the test cannot fetch
    /// them. Its hashes are copied from that release's published
    /// `conpty-sidecar.sha256.toml`, not from the source tree: the asset
    /// build is not byte-reproducible, so the tree's compiled-in pin matches
    /// no published asset (see the conpty-sidecar asset-reproducibility
    /// issue). Bump both after a release that changes
    /// `WINDOWS_CONPTY_VERSION.txt`.
    const PINNED_PUBLISHED: &str = "0.1.40";

    fn pinned_published_asset() -> Option<super::super::conpty_sidecar_hashes::ExpectedAsset> {
        use super::super::conpty_sidecar_hashes::ExpectedAsset;
        let (sha256_hex, size_bytes) = match arch_dir() {
            "x64" => ("606fbdd35edfa76df3221e5d2df25c6a150fcdae4fa681d8310213d7f4871594", 505_888),
            "arm64" => ("6045eb23021848e845b7aee4f98e08e457300ae43c051cd68a7e9cc15829a8f8", 470_883),
            "x86" => ("19c8aab5829715261fb8106792f796d10a7077e82a38da6a6bb1655e5bf24e13", 457_236),
            _ => return None,
        };
        Some(ExpectedAsset { sha256_hex, size_bytes })
    }

    fn pinned_published_url() -> String {
        format!(
            "https://github.com/zackees/kernal-api/releases/download/v{PINNED_PUBLISHED}/conpty-sidecar-{}.tar.zst",
            arch_dir()
        )
    }

    /// A transient upstream failure worth retrying: a GitHub 502/503/504 or
    /// a timeout while sending the request (which includes the connect).
    /// A 404 or a hash mismatch is never retried.
    fn is_transient_upstream(message: &str) -> bool {
        ["status code 502", "status code 503", "status code 504"]
            .iter()
            .any(|s| message.contains(s))
            || message.contains("WinHttpSendRequest: timed out")
    }

    #[test]
    fn transient_upstream_classification() {
        for msg in [
            "conpty sidecar fetch from u failed: status code 502",
            "conpty sidecar fetch from u failed: status code 503",
            "conpty sidecar fetch from u failed: status code 504",
            "conpty sidecar fetch from u failed: WinHttpSendRequest: timed out",
        ] {
            assert!(is_transient_upstream(msg), "{msg}");
        }
        for msg in [
            "conpty sidecar fetch from u failed: status code 404",
            "conpty sidecar fetch from u failed: status code 500",
            "conpty sidecar hash mismatch: expected a, got b",
            "conpty sidecar fetch from u failed: WinHttpReadData: timed out",
        ] {
            assert!(!is_transient_upstream(msg), "{msg}");
        }
    }

    /// Live end-to-end fetch of a published, pinned release asset: WinHTTP
    /// download, redirect to the CDN, SHA-256 check, extraction.
    #[test]
    fn fetch_and_extract_downloads_pinned_asset() {
        if std::env::var_os("KERNAL_API_CONPTY_OFFLINE").is_some() {
            eprintln!(
                "SKIPPED (offline): KERNAL_API_CONPTY_OFFLINE is set; \
                 live sidecar fetch of v{PINNED_PUBLISHED} not run"
            );
            return;
        }
        let expected = pinned_published_asset().unwrap_or_else(|| {
            panic!("no published sidecar pin for arch {} in v{PINNED_PUBLISHED}", arch_dir())
        });
        let url = pinned_published_url();
        let tmp = tempfile::tempdir().unwrap();
        const ATTEMPTS: u32 = 3;
        let mut last = String::new();
        for attempt in 1..=ATTEMPTS {
            let cache_dir = tmp.path().join(format!("attempt-{attempt}")).join(arch_dir());
            match fetch_and_extract_from(&url, &expected, &cache_dir) {
                Ok(()) => {
                    for name in ["conpty.dll", "OpenConsole.exe"] {
                        let file = cache_dir.join(name);
                        let len = fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
                        assert!(len > 0, "missing or empty {}", file.display());
                    }
                    return;
                }
                Err(e) if is_transient_upstream(&e.to_string()) => {
                    last = e.to_string();
                    if attempt < ATTEMPTS {
                        std::thread::sleep(std::time::Duration::from_secs(2u64.pow(attempt)));
                    }
                }
                Err(e) => panic!(
                    "live fetch of pinned sidecar v{PINNED_PUBLISHED} ({url}) failed: {e}. \
                     If this is a 404 or a hash mismatch, PINNED_PUBLISHED and its \
                     hashes need bumping to the latest release after \
                     WINDOWS_CONPTY_VERSION.txt changed."
                ),
            }
        }
        panic!("upstream GitHub 5xx after {ATTEMPTS} attempts fetching {url}: {last}");
    }

    /// `arch_dir` is a compile-time constant matching the host arch.
    #[test]
    fn arch_dir_matches_target() {
        let s = arch_dir();
        #[cfg(target_arch = "x86_64")]
        assert_eq!(s, "x64");
        #[cfg(target_arch = "aarch64")]
        assert_eq!(s, "arm64");
        #[cfg(target_arch = "x86")]
        assert_eq!(s, "x86");
        #[cfg(target_arch = "arm")]
        assert_eq!(s, "arm");
    }

    /// `asset_url` includes the kernal-api version and arch.
    #[test]
    fn asset_url_contains_version_and_arch() {
        let url = asset_url();
        assert!(url.contains(env!("CARGO_PKG_VERSION")), "got {url}");
        assert!(url.contains(arch_dir()), "got {url}");
        assert!(url.starts_with("https://github.com/zackees/kernal-api/releases/download/"));
        assert!(url.ends_with(".tar.zst"));
    }

    /// #447: matching bytes verify cleanly against a known SHA-256.
    /// SHA-256 of "hello world" is the canonical hex literal below;
    /// `python -c "import hashlib;print(hashlib.sha256(b'hello world').hexdigest())"`.
    #[test]
    fn verify_asset_accepts_matching_sha() {
        let bytes = b"hello world".to_vec();
        let expected = super::super::conpty_sidecar_hashes::ExpectedAsset {
            sha256_hex: "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
            size_bytes: 11,
        };
        verify_asset(&bytes, &expected).expect("matching sha should verify");
    }

    /// #447: a single flipped byte must be rejected with a mismatch
    /// error, not silently accepted.
    #[test]
    fn verify_asset_rejects_flipped_byte() {
        let bytes = b"hello world".to_vec();
        let expected = super::super::conpty_sidecar_hashes::ExpectedAsset {
            sha256_hex: "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
            size_bytes: 11,
        };
        let mut tampered = bytes.clone();
        tampered[0] ^= 0x01;
        let err = verify_asset(&tampered, &expected).expect_err("flipped byte should reject");
        assert!(err.contains("sha mismatch"), "got: {err}");
    }

    /// `ct_eq` accepts equal-length matching strings and rejects on
    /// any length or byte difference.
    #[test]
    fn ct_eq_matches_and_rejects() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "ab"));
        assert!(!ct_eq("", "x"));
        assert!(ct_eq("", ""));
    }
}
