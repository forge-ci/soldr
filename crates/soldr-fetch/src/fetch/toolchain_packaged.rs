//! Resolution for tools repackaged by soldr-toolchain across every target.

use super::{
    archive, check_cache, current_unix_ms, manifest_lookup, smoke_test_or_evict, FetchResult,
};
use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths, TargetTriple};

/// Exact filename for binaries that soldr-toolchain republishes across all
/// eight supported targets. Upstream Dylint 6.0.3 only publishes Linux GNU;
/// these explicitly opted-in packages keep the binary-only contract intact on
/// Windows, macOS, and musl without weakening catalogue SHA verification.
pub(super) fn asset_name(cache_name: &str, version: &str, target: &TargetTriple) -> Option<String> {
    // The catalogued asset prefix usually equals the cache name. maturin is
    // the exception: soldr caches and invokes the binary as `maturin`, but
    // the toolchain catalogues the forge-built blobs under the fork/package
    // identity `soldr-maturin` (soldr#2573), so the prefix is mapped rather
    // than assumed.
    //
    // `cargo-chef` and `crgx` join the same path from soldr-toolchain#181:
    // both are built by the forge-rust producer inside manylinux2014 with the
    // glibc floor measured by `readelf -V`, so the catalogued bundle honours
    // the 2.17 archive floor while upstream's own Linux binaries are 2.39.
    // `cargo-nextest` is deliberately absent: its republished bundle carries a
    // `-rust1.98.1` build label, and a consumer pin must not encode the
    // producer's compiler. It joins at the next nextest version bump, where an
    // unlabelled filename can be published without colliding with an existing
    // `(filename, sha256)` pin (soldr#3303).
    let asset_prefix = match cache_name {
        "cargo-dylint" | "dylint-link" | "dylint-driver" | "cargo-chef" | "crgx" => cache_name,
        "maturin" => "soldr-maturin",
        _ => return None,
    };
    Some(format!(
        "{}-{}-{}.tar.gz",
        asset_prefix,
        version.trim_start_matches('v'),
        target.triple()
    ))
}

/// Resolve an explicitly supported soldr-toolchain repackaged binary by its
/// exact versioned filename. The catalogue owner is intentionally not the
/// upstream repository: these rows are produced and hosted by
/// `zackees/soldr-toolchain`, and a unique exact filename plus its SHA-256 pin
/// is the complete identity needed by this path.
pub(super) async fn try_binary(
    paths: &SoldrPaths,
    cache_name: &str,
    binary_names: &[&str],
    version: &str,
    target: &TargetTriple,
) -> Result<Option<FetchResult>, SoldrError> {
    let Some(asset_name) = asset_name(cache_name, version, target) else {
        return Ok(None);
    };
    let manifest = manifest_lookup::get_or_fetch().await;
    let matches = manifest.lookup_asset(&asset_name);
    if matches.is_empty() {
        return Ok(None);
    }
    if matches.len() != 1 {
        return Err(SoldrError::Other(format!(
            "toolchain catalogue has {} rows for exact asset {asset_name}",
            matches.len()
        )));
    }
    let entry = matches[0];
    let bare_version = version.trim_start_matches('v');
    if let Some(result) = check_cache(paths, cache_name, bare_version, binary_names, target)? {
        return Ok(Some(result));
    }

    eprintln!(
        "soldr: toolchain catalogue hit for {} v{} {} -> {}",
        cache_name,
        bare_version,
        target.triple(),
        entry.asset
    );
    let download_started_at_ms = current_unix_ms();
    let download_started = std::time::Instant::now();
    let downloaded = manifest_lookup::materialize_catalogue_entry(paths, entry).await?;
    let binary_path = archive::extract_catalogue_asset_with_pin(
        paths,
        cache_name,
        bare_version,
        entry,
        downloaded.path(),
        target,
        binary_names,
    )
    .await?;
    if cache_name != "dylint-driver" {
        smoke_test_or_evict(&binary_path, cache_name, target)?;
    }
    soldr_core::build_log_meta::fetch_timing::record(
        soldr_core::build_log_meta::fetch_timing::FetchTiming {
            name: cache_name.to_string(),
            source: "catalogue".to_string(),
            started_at_ms: download_started_at_ms,
            duration_ms: download_started.elapsed().as_millis() as u64,
        },
    );

    Ok(Some(FetchResult {
        binary_path,
        version: bare_version.to_string(),
        cached: false,
    }))
}

/// Materialize the exact catalogued Dylint driver in cargo-dylint's cache.
///
/// Catalogue archives use the native Windows `.exe` filename, but
/// cargo-dylint's cross-platform cache contract is deliberately extensionless:
/// `$DYLINT_DRIVER_PATH/<nightly>-<host>/dylint-driver`.
pub async fn ensure_dylint_driver(
    paths: &SoldrPaths,
    dylint_version: &str,
    channel: &str,
    driver_root: &Path,
) -> Result<Option<PathBuf>, SoldrError> {
    let target = TargetTriple::host()?;
    let Some(dated_channel) = dated_nightly_prefix(channel) else {
        return Err(SoldrError::Other(format!(
            "Dylint driver catalogue lookup requires a dated nightly, got `{channel}`"
        )));
    };
    let asset_version = format!("{}-{dated_channel}", dylint_version.trim_start_matches('v'));
    let Some(result) = try_binary(
        paths,
        "dylint-driver",
        &["dylint-driver"],
        &asset_version,
        &target,
    )
    .await?
    else {
        return Ok(None);
    };

    let qualified_channel = format!("{dated_channel}-{}", target.triple());
    let destination =
        install_extensionless_driver(&result.binary_path, driver_root, &qualified_channel)?;
    eprintln!(
        "soldr: installed catalogued Dylint driver {} at {}",
        asset_version,
        destination.display()
    );
    Ok(Some(destination))
}

fn install_extensionless_driver(
    source: &Path,
    driver_root: &Path,
    qualified_channel: &str,
) -> Result<PathBuf, SoldrError> {
    let driver_dir = driver_root.join(qualified_channel);
    std::fs::create_dir_all(&driver_dir)?;
    let destination = driver_dir.join("dylint-driver");
    // Unix hosts get a loader-env wrapper in the `dylint-driver` slot and
    // the real binary beside it (soldr#2634 finding 4): the catalogued
    // driver's baked rpath names the *builder's* rustup layout, so a host
    // with any other `RUSTUP_HOME` fails
    // `error while loading shared libraries: librustc_driver-…` the
    // moment cargo-dylint execs the driver directly — soldr's own probe
    // survives only because it injects the toolchain `lib/` itself. The
    // wrapper derives that directory from the invoking environment at run
    // time, so the staged driver is layout-portable. Windows resolves
    // rustc DLLs through PATH, which cargo-dylint already provides.
    let payload_destination =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            destination.clone()
        } else {
            driver_dir.join("dylint-driver-real")
        };
    install_driver_file_atomically(source, &driver_dir, &payload_destination, |src, tmp| {
        std::fs::copy(src, tmp).map(|_| ())
    })?;
    if payload_destination != destination {
        let script = driver_loader_wrapper_script(qualified_channel);
        install_driver_file_atomically(source, &driver_dir, &destination, |_, tmp| {
            std::fs::write(tmp, &script)
        })?;
    }
    Ok(destination)
}

/// Write one staged driver file through a part-file + rename so a
/// concurrent reader never observes a torn executable — and, since
/// soldr#3538, never observes a missing one either.
///
/// The rename goes straight over the destination: POSIX `rename(2)`
/// atomically replaces it, so readers see either the old file or the new
/// one. The previous implementation deleted the destination first, which
/// left a window where a concurrent `soldr cargo dylint` in another
/// worktree got `No such file or directory` execing or probing the driver
/// (and two racing installers could even collide on the delete itself).
/// Removal happens only as the fallback for platforms/filesystems whose
/// `rename` refuses to replace an existing destination (Windows), matching
/// the `replace_marker_file` pattern in `dylint_cook.rs`,
/// `dylint_driver/local_build.rs`, and `ci_test/dylint_library_marker.rs`.
fn install_driver_file_atomically(
    source: &Path,
    driver_dir: &Path,
    destination: &Path,
    materialize: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), SoldrError> {
    let file_name = destination
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("dylint-driver");
    let temporary = driver_dir.join(format!(".{file_name}.part-{}", std::process::id()));
    materialize(source, &temporary)?;
    crate::platform::fs::permissions::make_executable(&temporary)?;
    replace_installed_file(&temporary, destination, |from, to| {
        std::fs::rename(from, to)
    })
    .map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        SoldrError::Io(error)
    })
}

/// Move `temporary` onto `destination`, replacing any existing file
/// without ever leaving the destination absent on the happy path.
///
/// Extracted so tests can assert the rename-over semantics directly
/// (soldr#3538): a plain `rename` first — atomic replace on POSIX — and
/// the remove-then-rename fallback only when the rename failed *and* a
/// destination is still there to get in the way.
fn replace_installed_file(
    temporary: &Path,
    destination: &Path,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if let Err(error) = rename(temporary, destination) {
        // Windows rename does not replace an existing destination. The
        // delete is confined to this fallback so a POSIX reader never
        // sees the destination disappear mid-install (soldr#3538).
        if destination.exists() {
            std::fs::remove_file(destination)?;
            rename(temporary, destination)?;
        } else {
            return Err(error);
        }
    }
    Ok(())
}

/// The Unix `dylint-driver` slot: export the toolchain's shared-library
/// directory for the loader, then exec the real catalogued binary.
///
/// The toolchain directory is derived from `RUSTUP_HOME` (or its default)
/// at *run* time, so one staged driver works across hosts whose rustup
/// layouts differ from the catalogue builder's. A missing directory adds
/// nothing and lets the exec proceed — an rpath-compatible layout still
/// works, and an incompatible one fails with the loader's own message.
fn driver_loader_wrapper_script(qualified_channel: &str) -> String {
    format!(
        r#"#!/bin/sh
# soldr: loader-env wrapper for the catalogued Dylint driver (soldr#2634).
dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
lib="${{RUSTUP_HOME:-$HOME/.rustup}}/toolchains/{qualified_channel}/lib"
if [ -d "$lib" ]; then
  if [ "$(uname)" = "Darwin" ]; then
    DYLD_FALLBACK_LIBRARY_PATH="$lib${{DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}}"
    export DYLD_FALLBACK_LIBRARY_PATH
  else
    # Nix keeps runtime closure libraries (such as libz) in a separate
    # variable; retain it after rustc's libs, ahead of the caller's entries.
    if [ -n "${{NIX_LD_LIBRARY_PATH:-}}" ]; then
      LD_LIBRARY_PATH="$lib:$NIX_LD_LIBRARY_PATH${{LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}}"
    else
      LD_LIBRARY_PATH="$lib${{LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}}"
    fi
    export LD_LIBRARY_PATH
  fi
fi
exec "$dir/dylint-driver-real" "$@"
"#
    )
}

fn dated_nightly_prefix(channel: &str) -> Option<&str> {
    let prefix = channel.get(..18)?;
    (prefix.starts_with("nightly-")
        && prefix.as_bytes()[8..]
            .iter()
            .enumerate()
            .all(|(index, byte)| {
                matches!(index, 4 | 7)
                    .then_some(*byte == b'-')
                    .unwrap_or(byte.is_ascii_digit())
            }))
    .then_some(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// soldr#3303: the toolchain publishes forge-built `cargo-chef` and
    /// `crgx` bundles under exactly these names, and soldr reached them only
    /// once `asset_name` stopped returning `None` for them. The names are
    /// asserted verbatim because the catalogue row is identified by exact
    /// filename plus its SHA-256 pin — a rename on either side is a miss that
    /// silently falls back to upstream's glibc-2.39 binaries.
    #[test]
    fn forge_built_cook_tools_resolve_by_exact_published_filename() {
        let target = TargetTriple::from_triple("x86_64-unknown-linux-gnu").unwrap();

        assert_eq!(
            asset_name("cargo-chef", "v0.1.73", &target).as_deref(),
            Some("cargo-chef-0.1.73-x86_64-unknown-linux-gnu.tar.gz")
        );
        assert_eq!(
            asset_name("crgx", "0.1.0", &target).as_deref(),
            Some("crgx-0.1.0-x86_64-unknown-linux-gnu.tar.gz")
        );
    }

    /// cargo-nextest stays off this path on purpose: soldr-toolchain#182
    /// republished it as `cargo-nextest-0.9.140-rust1.98.1-<triple>.tar.gz`,
    /// and `asset_name` must not learn that build label — it would encode the
    /// producer's compiler version into a consumer-side pin.
    #[test]
    fn nextest_is_not_resolved_by_a_build_labelled_filename() {
        let target = TargetTriple::from_triple("x86_64-unknown-linux-gnu").unwrap();

        assert_eq!(asset_name("cargo-nextest", "0.9.140", &target), None);
    }

    /// soldr#2634 finding 4: the Unix `dylint-driver` slot must be the
    /// loader-env wrapper with the real catalogued binary beside it, so
    /// cargo-dylint's direct exec survives rustup layouts that differ
    /// from the catalogue builder's baked rpath. Windows keeps the plain
    /// copy (DLL resolution goes through PATH there).
    #[test]
    fn staged_driver_layout_is_loader_portable() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let source = temp.path().join("fetched-driver");
        std::fs::write(&source, b"driver-payload").expect("source");
        let root = temp.path().join("drivers");
        let channel = "nightly-2026-05-28-x86_64-unknown-linux-gnu";

        let destination =
            install_extensionless_driver(&source, &root, channel).expect("stage driver");
        assert_eq!(destination, root.join(channel).join("dylint-driver"));
        let staged = std::fs::read(&destination).expect("read staged slot");

        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            assert_eq!(staged, b"driver-payload", "Windows stages the plain copy");
            return;
        }
        let script = String::from_utf8(staged).expect("wrapper script is text");
        assert!(script.starts_with("#!/bin/sh"), "wrapper must be a script");
        assert!(
            script.contains(&format!("toolchains/{channel}/lib")),
            "wrapper must derive the staged channel's lib dir: {script}"
        );
        assert!(
            script.contains("[ -n \"${NIX_LD_LIBRARY_PATH:-}\" ]")
                && script
                    .contains("$lib:$NIX_LD_LIBRARY_PATH${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"),
            "Linux wrapper must retain non-empty Nix loader paths after toolchain libs: {script}"
        );
        assert!(
            script.contains("exec \"$dir/dylint-driver-real\""),
            "wrapper must exec the real driver: {script}"
        );
        let payload = std::fs::read(root.join(channel).join("dylint-driver-real"))
            .expect("real driver beside the wrapper");
        assert_eq!(payload, b"driver-payload");
    }

    /// Restaging over an existing installation must replace both files.
    #[test]
    fn restaging_replaces_an_existing_driver() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let source_one = temp.path().join("driver-one");
        let source_two = temp.path().join("driver-two");
        std::fs::write(&source_one, b"one").expect("source one");
        std::fs::write(&source_two, b"two").expect("source two");
        let root = temp.path().join("drivers");
        let channel = "nightly-2026-05-28-x86_64-unknown-linux-gnu";

        install_extensionless_driver(&source_one, &root, channel).expect("first stage");
        install_extensionless_driver(&source_two, &root, channel).expect("second stage");

        let payload_path = if crate::platform::host::facts::os()
            == crate::platform::host::facts::HostOs::Windows
        {
            root.join(channel).join("dylint-driver")
        } else {
            root.join(channel).join("dylint-driver-real")
        };
        assert_eq!(std::fs::read(payload_path).expect("payload"), b"two");
    }

    /// soldr#3538: the destination must still be present — with its old
    /// bytes — at the moment the rename runs. The pre-fix install deleted
    /// the destination first, so a concurrent `soldr cargo dylint` in
    /// another worktree saw `No such file or directory` execing or probing
    /// the driver. This asserts the rename-over ordering directly: if the
    /// helper ever deletes before renaming again, the bytes observed at
    /// rename time drop to empty and the test fails.
    #[test]
    fn install_renames_over_the_destination_without_deleting_it_first() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let destination = temp.path().join("dylint-driver-real");
        let temporary = temp.path().join(".dylint-driver-real.part-test");
        std::fs::write(&destination, b"old-driver").expect("old driver");
        std::fs::write(&temporary, b"new-driver").expect("staged driver");

        let mut observed_at_rename_time = Vec::new();
        replace_installed_file(&temporary, &destination, |from, to| {
            observed_at_rename_time = std::fs::read(to).unwrap_or_default();
            std::fs::rename(from, to)
        })
        .expect("rename over an existing destination must succeed on this platform");

        assert_eq!(
            observed_at_rename_time, b"old-driver",
            "the destination must survive intact until the rename replaces it — never be \
             deleted first (soldr#3538)"
        );
        assert_eq!(
            std::fs::read(&destination).expect("installed bytes"),
            b"new-driver"
        );
        assert!(!temporary.exists(), "the staged part-file must be consumed");
    }

    /// Windows-style platforms: `rename` refuses to replace an existing
    /// destination, so the helper falls back to remove-then-rename — but
    /// only after the rename has actually failed, never preemptively.
    #[test]
    fn replace_installed_file_falls_back_to_remove_then_rename_when_rename_refuses() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let destination = temp.path().join("dylint-driver");
        let temporary = temp.path().join(".dylint-driver.part-test");
        std::fs::write(&destination, b"old-driver").expect("old driver");
        std::fs::write(&temporary, b"new-driver").expect("staged driver");

        let mut attempts = 0;
        replace_installed_file(&temporary, &destination, |from, to| {
            attempts += 1;
            if attempts == 1 {
                assert!(
                    to.exists(),
                    "the fallback may only fire while a destination is still in the way"
                );
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "simulated Windows destination-exists failure",
                ));
            }
            std::fs::rename(from, to)
        })
        .expect("fallback must complete the install");

        assert_eq!(attempts, 2, "exactly one failed rename then one retry");
        assert_eq!(
            std::fs::read(&destination).expect("installed bytes"),
            b"new-driver"
        );
        assert!(!temporary.exists(), "the staged part-file must be consumed");
    }

    /// A rename failure with nothing to fall back to is an error, not a
    /// silent success — and the caller cleans up the part-file.
    #[test]
    fn replace_installed_file_propagates_the_error_when_no_destination_blocks_the_rename() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let destination = temp.path().join("dylint-driver");
        let temporary = temp.path().join(".dylint-driver.part-test");
        std::fs::write(&temporary, b"new-driver").expect("staged driver");

        let error = replace_installed_file(&temporary, &destination, |_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "simulated failure",
            ))
        })
        .expect_err("a failed rename with no destination to clear must be an error");

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(!destination.exists());
        assert!(temporary.exists(), "the caller owns part-file cleanup");
    }

    /// The issue's actual failure mode, exercised end-to-end: a reader
    /// hammering the installed driver path while installs restage it over
    /// and over. On POSIX, `rename(2)` replaces the destination atomically,
    /// so this can never observe absence — while the pre-fix
    /// delete-then-rename opened a window on every restage (soldr#3538).
    /// POSIX-only because the Windows fallback path legitimately removes.
    /// Gated by a **runtime** host check, not `#[cfg(unix)]`: host `cfg`
    /// outside `soldr-platform` is denied by the #2493 boundary
    /// (`dylints/ban_platform_cfg_outside_boundary` and
    /// `.github/scripts/platform_cfg_boundary_ratchet.py`) — the same
    /// convention as `dylint_link_validation_tests.rs` (soldr#3284).
    #[test]
    fn a_reader_never_sees_the_destination_absent_during_repeated_installs() {
        use std::sync::atomic::{AtomicBool, Ordering};

        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            return;
        }
        let temp = tempfile::TempDir::new().expect("tempdir");
        let source_one = temp.path().join("driver-one");
        let source_two = temp.path().join("driver-two");
        std::fs::write(&source_one, b"one").expect("source one");
        std::fs::write(&source_two, b"two").expect("source two");
        let root = temp.path().join("drivers");
        let channel = "nightly-2026-05-28-x86_64-unknown-linux-gnu";
        install_extensionless_driver(&source_one, &root, channel).expect("first stage");
        let wrapper = root.join(channel).join("dylint-driver");

        let done = std::sync::Arc::new(AtomicBool::new(false));
        let reader = {
            let wrapper = wrapper.clone();
            let done = std::sync::Arc::clone(&done);
            std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    if !wrapper.is_file() {
                        return false;
                    }
                }
                true
            })
        };

        for iteration in 0..50 {
            let source = if iteration % 2 == 0 {
                &source_two
            } else {
                &source_one
            };
            install_extensionless_driver(source, &root, channel)
                .expect("restage while the reader is running");
        }
        done.store(true, Ordering::Relaxed);
        assert!(
            reader.join().expect("reader thread"),
            "the installed driver path must never be absent while a concurrent reader watches \
             it (soldr#3538)"
        );
        assert!(wrapper.is_file(), "wrapper must be installed at the end");
    }

    #[test]
    fn dylint_packages_cover_all_supported_targets() {
        let targets = [
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
        ];
        for triple in targets {
            let target = TargetTriple::from_triple(triple).unwrap();
            assert_eq!(
                asset_name("cargo-dylint", "v6.2.0", &target),
                Some(format!("cargo-dylint-6.2.0-{triple}.tar.gz"))
            );
            assert_eq!(
                asset_name("dylint-link", "6.2.0", &target),
                Some(format!("dylint-link-6.2.0-{triple}.tar.gz"))
            );
            assert_eq!(
                asset_name("dylint-driver", "6.2.0-nightly-2026-08-15", &target),
                Some(format!(
                    "dylint-driver-6.2.0-nightly-2026-08-15-{triple}.tar.gz"
                ))
            );
        }
        let host = TargetTriple::from_triple("x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(asset_name("cargo-nextest", "1", &host), None);
    }

    // The forge-built maturin blobs are catalogued under the fork/package
    // identity `soldr-maturin`, while soldr's cache name for the tool is
    // plain `maturin` (soldr#2573). The mapping must produce the catalogued
    // filename exactly, for every supported target, or the sha-pinned CDN
    // rung silently never fires and the fetch falls through to GitHub.
    #[test]
    fn maturin_maps_to_the_soldr_maturin_catalogue_prefix() {
        let targets = [
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
        ];
        for triple in targets {
            let target = TargetTriple::from_triple(triple).unwrap();
            assert_eq!(
                asset_name("maturin", "1.14.1.post1", &target),
                Some(format!("soldr-maturin-1.14.1.post1-{triple}.tar.gz"))
            );
        }
        // The mapped prefix must not leak to lookalike cache names.
        let host = TargetTriple::from_triple("x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(asset_name("soldr-maturin", "1.14.1.post1", &host), None);
    }

    #[test]
    fn dated_nightly_accepts_qualified_channel() {
        assert_eq!(
            dated_nightly_prefix("nightly-2026-05-28-x86_64-pc-windows-msvc"),
            Some("nightly-2026-05-28")
        );
        assert_eq!(dated_nightly_prefix("1.94.1"), None);
    }

    #[test]
    fn driver_installation_is_extensionless_on_every_host() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("dylint-driver.exe");
        std::fs::write(&source, b"driver").unwrap();
        let destination = install_extensionless_driver(
            &source,
            &dir.path().join("drivers"),
            "nightly-2026-05-28-x86_64-pc-windows-msvc",
        )
        .unwrap();
        assert_eq!(destination.file_name().unwrap(), "dylint-driver");
        // The extensionless slot holds the payload directly on Windows
        // and the loader-env wrapper on Unix (soldr#2634 finding 4); the
        // payload then lives beside it. Either way the payload bytes are
        // staged under the extensionless contract.
        let payload = if crate::platform::host::facts::os()
            == crate::platform::host::facts::HostOs::Windows
        {
            destination
        } else {
            destination.with_file_name("dylint-driver-real")
        };
        assert_eq!(std::fs::read(payload).unwrap(), b"driver");
    }

    #[test]
    fn catalogued_dylint_binary_is_smoked_and_evicted() {
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("cargo-dylint");
        std::fs::write(&bogus, b"not an executable").unwrap();
        let target = TargetTriple::host().unwrap();

        let error = smoke_test_or_evict(&bogus, "cargo-dylint", &target).unwrap_err();

        assert!(error.to_string().contains("smoke test failed"));
        assert!(!bogus.exists(), "failed catalogue binary must be evicted");
    }
}
