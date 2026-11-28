//! Pinned on-demand bootstrap for the `reld` linker (soldr#3276).
//!
//! `reld` is a host executable used by Cargo's linker invocation. Explicit
//! selection must therefore resolve one verified binary before Cargo starts,
//! rather than relying on ambient `PATH` state.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths, TargetTriple};
use crate::platform::host::facts::{HostArch, HostOs};

/// The release soldr installs when a project explicitly selects `reld`.
pub const MANAGED_RELD_VERSION: &str = "0.2.3";

/// Explicit local development override. It must name an absolute executable.
pub const RELD_BIN_ENV_VAR: &str = "SOLDR_RELD_BIN";

/// Overrides the `https://github.com/zackees/reld/releases/download` prefix
/// used to build the release download URL. Test-only seam (soldr#3276 §2):
/// lets an integration test point `ensure_reld` at a local HTTP fixture
/// instead of live GitHub, while going through the exact same
/// download -> verify -> extract -> stamp -> reuse path production uses.
/// This does not weaken trust: the sha256 pin is still enforced by
/// `download_and_extract_with_pin`, it is just supplied by the caller
/// instead of the built-in release table (see `ensure_reld_from`).
pub const RELD_BASE_URL_ENV_VAR: &str = "SOLDR_RELD_BASE_URL_OVERRIDE";

pub(crate) struct ReleaseAsset {
    pub(crate) triple: &'static str,
    pub(crate) extension: &'static str,
    pub(crate) sha256: &'static str,
    /// Install directory stem under `bin/` (`<cache_name>-<version>`).
    pub(crate) cache_name: &'static str,
    /// Files extracted flat beside `reld`; the first is the executable.
    pub(crate) files: &'static [&'static str],
}

/// The static build ships only the executable.
const STATIC_FILES: &[&str] = &["reld"];

/// Resolve a verified host executable for an explicit `reld` selection.
pub async fn ensure_reld(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    if let Some(path) = reld_bin_from_env()? {
        return Ok(path);
    }

    let asset = release_asset_for(
        crate::platform::host::facts::os(),
        crate::platform::host::facts::arch(),
        crate::platform::host::facts::glibc_version().as_deref(),
    )?;
    let base_url = std::env::var(RELD_BASE_URL_ENV_VAR)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "https://github.com/zackees/reld/releases/download".to_string());
    ensure_reld_from(paths, &base_url, &asset).await
}

/// Core of [`ensure_reld`] with the release base URL and asset (name +
/// pinned sha256) injected, so both production and tests share the exact
/// download/verify/extract/stamp/reuse logic.
pub(crate) async fn ensure_reld_from(
    paths: &SoldrPaths,
    base_url: &str,
    asset: &ReleaseAsset,
) -> Result<PathBuf, SoldrError> {
    let host = TargetTriple::host()?;
    let binary = managed_binary_path(paths, asset);
    if binary.is_file() {
        return Ok(binary);
    }

    let asset_name = format!(
        "reld-v{MANAGED_RELD_VERSION}-{}.{}",
        asset.triple, asset.extension
    );
    let url = format!("{base_url}/v{MANAGED_RELD_VERSION}/{asset_name}");
    eprintln!("soldr: fetching reld v{MANAGED_RELD_VERSION} ({asset_name})...");
    // This bootstrap bypasses the repository-fetch retry loop.
    let resolved = super::retry::with_backoff("reld", || {
        super::archive::download_and_extract_with_pin(
            paths,
            asset.cache_name,
            MANAGED_RELD_VERSION,
            &url,
            &host,
            asset.files,
            Some((&asset_name, asset.sha256)),
        )
    })
    .await?;
    if !resolved.is_file() {
        return Err(SoldrError::Archive(format!(
            "reld archive did not produce an executable at {}",
            resolved.display()
        )));
    }
    eprintln!("soldr: downloaded reld v{MANAGED_RELD_VERSION}");
    Ok(resolved)
}

fn reld_bin_from_env() -> Result<Option<PathBuf>, SoldrError> {
    let Some(value) = std::env::var_os(RELD_BIN_ENV_VAR) else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(SoldrError::Other(format!(
            "{RELD_BIN_ENV_VAR} must be an absolute path, got {}",
            path.display()
        )));
    }
    if !path.is_file() {
        return Err(SoldrError::Other(format!(
            "{RELD_BIN_ENV_VAR} does not name a file: {}",
            path.display()
        )));
    }
    Ok(Some(path))
}

fn managed_binary_path(paths: &SoldrPaths, asset: &ReleaseAsset) -> PathBuf {
    paths
        .bin
        .join(format!("{}-{MANAGED_RELD_VERSION}", asset.cache_name))
        .join(format!("reld{}", std::env::consts::EXE_SUFFIX))
}

fn release_asset_for(
    os: HostOs,
    arch: HostArch,
    _glibc: Option<&str>,
) -> Result<ReleaseAsset, SoldrError> {
    let asset = match (os, arch) {
        // Prefer the static musl release on Linux x86_64 so the bootstrap does
        // not depend on a runner's glibc version.
        (HostOs::Linux, HostArch::X86_64) => ReleaseAsset {
            triple: "x86_64-unknown-linux-musl",
            extension: "tar.gz",
            cache_name: "reld",
            files: STATIC_FILES,
            sha256: "f65044bdb26a10b273948277c1cd960511f69a0eba9ba9c0c0dfa4e61aed5eb1",
        },
        (HostOs::Linux, HostArch::Aarch64) => ReleaseAsset {
            triple: "aarch64-unknown-linux-gnu",
            extension: "tar.gz",
            cache_name: "reld",
            files: STATIC_FILES,
            sha256: "45e89ee11e838f66f37cf6cd5fe35178d296219cd3a0d92172e1740cb75df3f1",
        },
        (HostOs::MacOs, HostArch::X86_64) => ReleaseAsset {
            triple: "x86_64-apple-darwin",
            extension: "tar.gz",
            cache_name: "reld",
            files: STATIC_FILES,
            sha256: "073565e1fff66671ab27bbc83f4fe31b9dd6bcdc2163ce1e06b6eb5f7b185e8f",
        },
        (HostOs::MacOs, HostArch::Aarch64) => ReleaseAsset {
            triple: "aarch64-apple-darwin",
            extension: "tar.gz",
            cache_name: "reld",
            files: STATIC_FILES,
            sha256: "d688c529eda43a18bed4477d0aa5fb7bd1982caf84b2689e850e5f88317e20d8",
        },
        (HostOs::Windows, HostArch::X86_64) => ReleaseAsset {
            triple: "x86_64-pc-windows-msvc",
            extension: "zip",
            cache_name: "reld",
            files: STATIC_FILES,
            sha256: "be053e50da77dfecd559d764bd4f3659da3e84a278b425849ee95c5de50f699f",
        },
        (HostOs::Windows, HostArch::Aarch64) => ReleaseAsset {
            triple: "aarch64-pc-windows-msvc",
            extension: "zip",
            cache_name: "reld",
            files: STATIC_FILES,
            sha256: "eba79aa6c769fd825f3f842838c88bcbb0bbf5b715c19a16b05c4bd377261a4d",
        },
        (os, arch) => {
            return Err(SoldrError::UnsupportedPlatform(format!(
                "reld v{MANAGED_RELD_VERSION} has no managed asset for host {arch:?} on {os:?}"
            )));
        }
    };
    Ok(asset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SoldrPaths;
    use crate::fetch::trust;
    use kernal_api::hash::Sha256Hasher;
    use std::io::Write as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serialised: these tests mutate process env (`SOLDR_TRUST_MODE`,
    /// `SOLDR_CHECKSUMS_FILE`).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime")
    }

    /// Build a `tar.gz` archive containing one executable named `reld`
    /// (plus the platform's exe suffix) with the given body, mirroring the
    /// on-disk shape a real release asset extracts into.
    fn build_fixture_archive(body: &[u8]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        let name = format!("reld{}", std::env::consts::EXE_SUFFIX);
        builder
            .append_data(&mut header, &name, body)
            .expect("append fixture reld binary");
        let tar_bytes = builder.into_inner().expect("finish tar");

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).expect("write tar into gzip");
        gz.finish().expect("finish gzip")
    }

    /// Serve `body` for GET requests. Returns the local HTTP fixture base.
    async fn serve_once(body: Vec<u8>) -> (String, std::sync::Arc<std::sync::atomic::AtomicU32>) {
        serve_with_initial_status(body, "200 OK").await
    }

    async fn serve_with_initial_status(
        body: Vec<u8>,
        initial_status: &'static str,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicU32>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let hits_clone = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let attempt = hits_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let status = if attempt == 0 {
                    initial_status
                } else {
                    "200 OK"
                };
                let mut buf = [0_u8; 2048];
                let _ = socket.read(&mut buf).await;
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&body).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{address}"), hits)
    }

    fn fetch_after_initial_status(status: &'static str) -> (Result<PathBuf, SoldrError>, u32) {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let body = build_fixture_archive(b"fixture reld");
        let sha256 = Sha256Hasher::digest(&body).to_hex();
        let asset = ReleaseAsset {
            triple: "test-fixture",
            extension: "tar.gz",
            cache_name: "reld",
            files: STATIC_FILES,
            sha256: Box::leak(sha256.into_boxed_str()),
        };
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let rt = runtime();
        let (base_url, hits) = rt.block_on(serve_with_initial_status(body, status));
        let result = rt.block_on(ensure_reld_from(&paths, &base_url, &asset));
        if let Ok(path) = &result {
            assert_eq!(
                std::fs::read(path).expect("installed reld"),
                b"fixture reld"
            );
        }
        (result, hits.load(std::sync::atomic::Ordering::SeqCst))
    }

    #[test]
    fn ensure_reld_from_retries_504_and_installs_verified_asset() {
        let (result, hits) = fetch_after_initial_status("504 Gateway Timeout");
        assert!(
            result.is_ok(),
            "transient asset failure must recover: {result:?}"
        );
        assert_eq!(
            hits, 2,
            "one failed download followed by one successful download"
        );
    }

    #[test]
    fn ensure_reld_from_does_not_retry_404() {
        let (result, hits) = fetch_after_initial_status("404 Not Found");
        let error = result.expect_err("missing asset must fail");
        assert!(matches!(&error, SoldrError::ReleaseNotFound(_)), "{error}");
        assert!(error.to_string().contains("404"), "{error}");
        assert_eq!(hits, 1, "a missing asset must not be retried");
    }

    /// soldr#3276 §2/§3: `ensure_reld_from` against a local HTTP fixture
    /// (no live GitHub) proves the full download -> verify -> extract ->
    /// stamp -> reuse path, that a second call performs no network access,
    /// and that `SOLDR_TRUST_MODE=strict` passes on the built-in-style pin
    /// supplied here (the pin -- not the trust-mode env var -- is what
    /// `download_and_extract_with_pin` enforces; strict mode must not
    /// additionally require a `SOLDR_CHECKSUMS_FILE` entry for it).
    #[test]
    fn ensure_reld_from_downloads_verifies_and_reuses_without_a_second_fetch() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous_trust_mode = std::env::var_os(trust::TRUST_MODE_ENV_VAR);
        std::env::set_var(trust::TRUST_MODE_ENV_VAR, "strict");

        let body = b"#!/bin/sh\necho fake-reld\n".to_vec();
        let sha256 = Sha256Hasher::digest(build_fixture_archive(&body)).to_hex();
        let asset = ReleaseAsset {
            triple: "test-fixture",
            extension: "tar.gz",
            cache_name: "reld",
            files: STATIC_FILES,
            sha256: Box::leak(sha256.into_boxed_str()),
        };

        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());

        let rt = runtime();
        let (base_url, hits) = rt.block_on(serve_once(build_fixture_archive(&body)));

        let first = rt
            .block_on(ensure_reld_from(&paths, &base_url, &asset))
            .expect("first fetch succeeds under strict trust mode with a matching pin");
        assert!(first.is_file(), "resolved reld path must exist: {first:?}");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "first call must hit the network exactly once"
        );

        // Second call reuses the stamped managed install; the server is
        // still listening, so a network hit here would show up in `hits`.
        let second = rt
            .block_on(ensure_reld_from(&paths, &base_url, &asset))
            .expect("second fetch reuses the managed install");
        assert_eq!(second, first);
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "second call must perform no network access"
        );

        match previous_trust_mode {
            Some(value) => std::env::set_var(trust::TRUST_MODE_ENV_VAR, value),
            None => std::env::remove_var(trust::TRUST_MODE_ENV_VAR),
        }
    }

    /// A pin mismatch is a hard error even under strict mode's default
    /// (permissive) counterpart -- the manifest-style pin passed to
    /// `download_and_extract_with_pin` is always enforced, matching the
    /// "explicit reld never falls back silently" acceptance criterion.
    #[test]
    fn ensure_reld_from_rejects_a_body_that_does_not_match_the_pin() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let body = b"unexpected content".to_vec();
        let asset = ReleaseAsset {
            triple: "test-fixture",
            extension: "tar.gz",
            cache_name: "reld",
            files: STATIC_FILES,
            sha256: "0000000000000000000000000000000000000000000000000000000000000",
        };

        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());

        let rt = runtime();
        let (base_url, _hits) = rt.block_on(serve_once(build_fixture_archive(&body)));

        let result = rt.block_on(ensure_reld_from(&paths, &base_url, &asset));
        assert!(result.is_err(), "sha256 mismatch must be a hard error");
    }

    #[test]
    fn release_assets_cover_supported_hosts_and_prefer_static_linux_x64() {
        let linux = release_asset_for(HostOs::Linux, HostArch::X86_64, None).expect("linux x64");
        assert_eq!(linux.triple, "x86_64-unknown-linux-musl");
        assert_eq!(linux.extension, "tar.gz");
        assert_eq!(linux.sha256.len(), 64);

        let windows =
            release_asset_for(HostOs::Windows, HostArch::Aarch64, None).expect("windows arm");
        assert_eq!(windows.triple, "aarch64-pc-windows-msvc");
        assert_eq!(windows.extension, "zip");

        let mac = release_asset_for(HostOs::MacOs, HostArch::Aarch64, None).expect("mac arm");
        assert_eq!(mac.triple, "aarch64-apple-darwin");
    }

    #[test]
    fn unknown_host_is_rejected() {
        assert!(release_asset_for(HostOs::Linux, HostArch::Unknown("riscv64"), None).is_err());
    }

    /// soldr#4281: only the glibc archives ship `libllvm_ld.so`, reld's
    /// in-process COFF engine. Hosts with glibc >= 2.39 get that archive
    /// and its library; older hosts keep the static build.
    #[test]
    fn linux_x64_with_new_glibc_gets_the_in_process_coff_engine() {
        for glibc in ["2.39", "2.40", "3.0"] {
            let asset = release_asset_for(HostOs::Linux, HostArch::X86_64, Some(glibc)).unwrap();
            assert_eq!(asset.triple, "x86_64-unknown-linux-gnu", "glibc {glibc}");
            assert_eq!(
                asset.sha256,
                "33927b4b243c418ce81094ccb0741ddd4d06cf77fc2b56d29d9cc8a6b8ff297d"
            );
            assert_eq!(asset.files[0], "reld");
            assert!(asset.files.contains(&"libllvm_ld.so"));
            assert!(asset.files.contains(&"LICENSE-LLVM.txt"));
            assert!(asset.files.contains(&"reld-link"));
            assert_ne!(asset.cache_name, "reld", "must not reuse a static install");
        }
    }

    #[test]
    fn linux_x64_with_old_or_unknown_glibc_keeps_the_static_build() {
        for glibc in [None, Some("2.35"), Some("2.38"), Some("garbage")] {
            let asset = release_asset_for(HostOs::Linux, HostArch::X86_64, glibc).unwrap();
            assert_eq!(asset.triple, "x86_64-unknown-linux-musl", "glibc {glibc:?}");
            assert_eq!(asset.files, STATIC_FILES);
            assert!(
                asset.files.contains(&"reld-link"),
                "MSVC links need reld-link"
            );
            assert_eq!(asset.cache_name, "reld");
        }
    }

    #[test]
    fn linux_arm64_glibc_archive_keeps_its_coff_engine() {
        let asset = release_asset_for(HostOs::Linux, HostArch::Aarch64, Some("2.39")).unwrap();
        assert_eq!(asset.triple, "aarch64-unknown-linux-gnu");
        assert!(asset.files.contains(&"libllvm_ld.so"));
        assert_ne!(asset.cache_name, "reld");
    }

    /// The whole file list is extracted flat beside `reld`.
    #[test]
    fn ensure_reld_from_installs_the_coff_engine_beside_reld() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut builder = tar::Builder::new(Vec::new());
        for (name, body) in [
            ("reld-v0.2.3-x/reld", b"reld".as_slice()),
            ("reld-v0.2.3-x/libllvm_ld.so", b"engine".as_slice()),
            ("reld-v0.2.3-x/LICENSE-LLVM.txt", b"llvm".as_slice()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, name, body).unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&builder.into_inner().unwrap()).unwrap();
        let body = gz.finish().unwrap();
        let asset = ReleaseAsset {
            triple: "test-fixture",
            extension: "tar.gz",
            sha256: Box::leak(Sha256Hasher::digest(&body).to_hex().into_boxed_str()),
            cache_name: "reld-glibc",
            files: &["reld", "libllvm_ld.so", "LICENSE-LLVM.txt"],
        };
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let rt = runtime();
        let (base_url, _hits) = rt.block_on(serve_once(body));
        let reld = rt
            .block_on(ensure_reld_from(&paths, &base_url, &asset))
            .unwrap();
        assert_eq!(
            reld.file_name().unwrap(),
            format!("reld{}", std::env::consts::EXE_SUFFIX).as_str()
        );
        assert_eq!(
            std::fs::read(reld.with_file_name("libllvm_ld.so")).unwrap(),
            b"engine"
        );
        assert!(reld.with_file_name("LICENSE-LLVM.txt").is_file());
    }

    /// An install from an older soldr that kept only `reld` is completed,
    /// not reused, so `reld-link` and the COFF engine appear.
    #[test]
    fn an_install_missing_listed_files_is_refreshed() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut builder = tar::Builder::new(Vec::new());
        for name in ["d/reld", "d/reld-link"] {
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, name, b"new!".as_slice())
                .unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&builder.into_inner().unwrap()).unwrap();
        let body = gz.finish().unwrap();
        let asset = ReleaseAsset {
            triple: "test-fixture",
            extension: "tar.gz",
            sha256: Box::leak(Sha256Hasher::digest(&body).to_hex().into_boxed_str()),
            cache_name: "reld",
            files: &["reld", "reld-link"],
        };
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let old = managed_binary_path(&paths, &asset);
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, b"old").unwrap();
        let rt = runtime();
        let (base_url, hits) = rt.block_on(serve_once(body));
        let reld = rt
            .block_on(ensure_reld_from(&paths, &base_url, &asset))
            .unwrap();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(reld
            .with_file_name(format!("reld-link{}", std::env::consts::EXE_SUFFIX))
            .is_file());
    }
}
