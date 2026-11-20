use super::*;

fn fake_source(tmp: &tempfile::TempDir, bytes: &[u8]) -> PathBuf {
    let source = tmp.path().join("soldr");
    std::fs::write(&source, bytes).unwrap();
    source
}

// A minimal 64-bit LE Mach-O whose load-command region names
// @loader_path -- i.e. a binary that only runs from its own directory.
fn position_dependent_source(tmp: &tempfile::TempDir) -> PathBuf {
    let payload = b"\x0c\x00\x00\x00@loader_path/../soldr.dylibs/liblzma.dylib\x00";
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0xfeed_facfu32.to_le_bytes()); // magic
    bytes.extend_from_slice(&0u32.to_le_bytes()); // cputype
    bytes.extend_from_slice(&0u32.to_le_bytes()); // cpusubtype
    bytes.extend_from_slice(&2u32.to_le_bytes()); // filetype
    bytes.extend_from_slice(&1u32.to_le_bytes()); // ncmds
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // sizeofcmds
    bytes.extend_from_slice(&0u32.to_le_bytes()); // flags
    bytes.extend_from_slice(&0u32.to_le_bytes()); // reserved
    bytes.extend_from_slice(payload);
    let source = tmp.path().join("soldr");
    std::fs::write(&source, bytes).unwrap();
    source
}

// A minimal 64-bit LE ELF whose DT_RUNPATH is `$ORIGIN/../soldr.libs` --
// what maturin's repair writes for a bundled liblzma (soldr#3403).
fn origin_runpath_elf_source(tmp: &tempfile::TempDir) -> PathBuf {
    let rpath = b"$ORIGIN/../soldr.libs\0";
    let (dyn_offset, dyn_size) = (64u64 + 2 * 56, 3 * 16u64);
    let strtab_offset = dyn_offset + dyn_size;
    let total = strtab_offset + rpath.len() as u64;
    let base = 0x40_0000u64;
    let mut bytes = b"\x7fELF\x02\x01\x01".to_vec();
    bytes.resize(16, 0);
    for (value, width) in [(3u64, 2), (62, 2), (1, 4), (0, 8), (64, 8), (0, 8), (0, 4)] {
        bytes.extend_from_slice(&value.to_le_bytes()[..width]);
    }
    for (value, width) in [(64u64, 2), (56, 2), (2, 2), (0, 6)] {
        bytes.extend_from_slice(&value.to_le_bytes()[..width]);
    }
    for (kind, offset, size) in [(1u32, 0u64, total), (2, dyn_offset, dyn_size)] {
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&5u32.to_le_bytes());
        for field in [offset, base + offset, base + offset, size, size, 8] {
            bytes.extend_from_slice(&field.to_le_bytes());
        }
    }
    for (tag, value) in [(5u64, base + strtab_offset), (29, 0), (0, 0)] {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(rpath);
    let source = tmp.path().join("soldr");
    std::fs::write(&source, bytes).unwrap();
    source
}

#[test]
fn a_position_dependent_source_is_refused_for_the_broker_image() {
    let tmp = tempfile::tempdir().unwrap();
    let elf = origin_runpath_elf_source(&tmp);
    let error = require_relocatable(&elf, "the stable broker image")
        .expect_err("an $ORIGIN rpath cannot be copied to the broker path");
    assert!(
        error.contains("$ORIGIN") && error.contains("broker image"),
        "{error}"
    );

    let plain = tmp.path().join("plain");
    std::fs::write(&plain, b"#!/bin/sh\n").unwrap();
    assert!(require_relocatable(&plain, "the stable broker image").is_ok());
}

/// soldr#3831: the verdict is remembered per image content, so a fresh
/// root does not reparse the whole binary.
#[test]
fn loader_path_verdict_is_memoized_by_content() {
    let tmp = tempfile::tempdir().unwrap();
    let elf = origin_runpath_elf_source(&tmp);
    // This test temporarily writes a false verdict to the content-keyed
    // global memo. Give its ELF distinct content so parallel trampoline
    // tests cannot observe that deliberately false value.
    let mut bytes = std::fs::read(&elf).unwrap();
    bytes.extend_from_slice(b"memo-verdict-test-fixture:");
    bytes.extend_from_slice(tmp.path().to_string_lossy().as_bytes());
    std::fs::write(&elf, &bytes).unwrap();
    let other_tmp = tempfile::tempdir().unwrap();
    let other = origin_runpath_elf_source(&other_tmp);
    assert!(
        bytes != std::fs::read(&other).unwrap(),
        "memo fixture must not share a digest with other ELF fixtures"
    );
    let dir = crate::daemon::image_hash::machine_scoped_cache_dir(&std::env::temp_dir());
    let digest = crate::daemon::image_hash::cached_blake3_hex(&dir, &elf).unwrap();
    let entry = dir.join(format!("{digest}.loader-path"));
    let _ = std::fs::remove_file(&entry);

    assert!(has_loader_path_reference(&elf));
    assert_eq!(std::fs::read(&entry).unwrap(), b"1");

    // A recorded verdict is served without reparsing: flip it and the
    // answer follows the memo, proving the read was skipped.
    std::fs::write(&entry, b"0").unwrap();
    assert!(!has_loader_path_reference(&elf));
    let _ = std::fs::remove_file(&entry);
}

#[test]
fn elf_with_an_origin_runpath_gets_a_trampoline() {
    let tmp = tempfile::tempdir().unwrap();
    let source = origin_runpath_elf_source(&tmp);
    let target = tmp.path().join("shims").join("rustc");

    let result = materialize_executable(&source, &target).unwrap();
    assert_eq!(result.link_mode, LINK_MODE_TRAMPOLINE);
    assert!(std::fs::read_to_string(&target)
        .unwrap()
        .starts_with("#!/bin/sh"));
}

// #1908: the guard belongs to materialize_executable, so every writer
// inherits it. Before this, rustc_wrapper_shim_binary and friends
// hardlinked the Mach-O straight into the shim dir and dyld aborted it
// -- and one of them overwrote a correct trampoline written moments
// earlier by `soldr shims`, so the same path could look right and then
// break.
#[test]
fn position_dependent_source_gets_a_trampoline() {
    let tmp = tempfile::tempdir().unwrap();
    let source = position_dependent_source(&tmp);
    let target = tmp.path().join("shims").join("rustc");

    let result = materialize_executable(&source, &target).unwrap();
    assert!(result.created);
    assert_eq!(result.link_mode, LINK_MODE_TRAMPOLINE);

    let body = std::fs::read_to_string(&target).unwrap();
    assert!(
        body.starts_with("#!/bin/sh"),
        "expected a trampoline: {body}"
    );
    // soldr#1934: this assertion used to demand the opposite -- the tool
    // name inserted ahead of `"$@"` -- and that is precisely what broke
    // every wheel install in 0.8.26. The wrapper contract is positional on
    // argv[1], so nothing may come between the shim and its arguments.
    assert!(
        !body.contains(" rustc \"$@\""),
        "the tool name must not be pushed into argv[1]: {body}"
    );
    assert!(
        body.trim_end().ends_with(" \"$@\""),
        "arguments must be forwarded verbatim: {body}"
    );
    // The identity travels in the environment instead, as the shim's $0.
    assert!(
        body.contains(crate::multicall::SHIM_ARGV0_ENV),
        "trampoline must carry its argv[0] identity: {body}"
    );
    assert!(
        body.contains(&source.to_string_lossy().to_string()),
        "trampoline must exec the source in place: {body}"
    );
}

// Idempotency matters here specifically: comparing a small script to a
// Mach-O always differs, so a naive byte comparison would republish the
// shim on every cargo invocation and undo the #1831 memo fast path.
#[test]
fn trampoline_materialization_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let source = position_dependent_source(&tmp);
    let target = tmp.path().join("shims").join("rustc");

    assert!(materialize_executable(&source, &target).unwrap().created);
    let second = materialize_executable(&source, &target).unwrap();
    assert!(
        !second.created,
        "an unchanged trampoline must not be rewritten"
    );
    assert_eq!(second.link_mode, LINK_MODE_TRAMPOLINE);
}

// The reverse direction of the same bug: a stale hardlinked Mach-O
// sitting where a trampoline belongs must be replaced, not kept.
#[test]
fn trampoline_replaces_a_stale_hardlinked_shim() {
    let tmp = tempfile::tempdir().unwrap();
    let source = position_dependent_source(&tmp);
    let target = tmp.path().join("shims").join("rustc");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, std::fs::read(&source).unwrap()).unwrap();

    let result = materialize_executable(&source, &target).unwrap();
    assert!(result.created, "stale Mach-O shim must be replaced");
    assert!(std::fs::read_to_string(&target)
        .unwrap()
        .starts_with("#!/bin/sh"));
}

// Ordinary binaries must keep the fast path untouched -- the startup
// latency work in #1831/#1834 depends on it.
#[test]
fn ordinary_source_still_takes_the_hardlink_path() {
    let tmp = tempfile::tempdir().unwrap();
    let source = fake_source(&tmp, b"an ordinary binary mentioning nothing special");
    let target = tmp.path().join("shims").join("cargo");
    let result = materialize_executable(&source, &target).unwrap();
    assert!(result.created);
    assert_ne!(result.link_mode, LINK_MODE_TRAMPOLINE);
}

#[test]
fn materialize_executable_creates_matching_target() {
    let tmp = tempfile::tempdir().unwrap();
    let source = fake_source(&tmp, b"fake-soldr-v1");
    let target = tmp.path().join("cargo");
    let result = materialize_executable(&source, &target).unwrap();
    assert!(result.created);
    assert!([LINK_MODE_HARDLINK, LINK_MODE_COPY].contains(&result.link_mode));
    assert_eq!(std::fs::read(&target).unwrap(), b"fake-soldr-v1");
}

#[test]
fn materialize_executable_is_idempotent_for_matching_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let source = fake_source(&tmp, b"fake-soldr-v1");
    let target = tmp.path().join("rustc");
    assert!(materialize_executable(&source, &target).unwrap().created);
    let second = materialize_executable(&source, &target).unwrap();
    assert!(!second.created);
}

/// A `uv tool install` puts `~/.local/bin/soldr` on PATH as a symlink,
/// and on macOS `current_exe()` returns that unresolved path. Linking the
/// symlink itself made `soldr-daemon` a symlink to `.../bin/soldr`; the
/// broker canonicalizes it, stages the image as `soldr`, and the daemon
/// starts as the plain CLI (argv[0] `soldr`) and exits with usage.
fn symlinked_source(tmp: &tempfile::TempDir) -> Option<PathBuf> {
    let real = fake_source(tmp, b"fake-soldr-v1");
    let link_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&link_dir).unwrap();
    let link = link_dir.join("soldr");
    // Windows needs a privilege to create symlinks; the bug is Unix-only.
    crate::platform::fs::links::create(&real.to_string_lossy(), &link, false).ok()?;
    Some(link)
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| crate::platform::fs::links::is_link_or_reparse(&meta))
}

#[test]
fn symlinked_source_materializes_a_real_executable_not_a_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(source) = symlinked_source(&tmp) else {
        return;
    };
    let target = source.with_file_name("soldr-daemon");
    materialize_executable(&source, &target).unwrap();
    assert!(!is_symlink(&target), "{} is a symlink", target.display());
    assert_eq!(
        std::fs::canonicalize(&target).unwrap().file_name(),
        Some(std::ffi::OsStr::new("soldr-daemon")),
        "the alias must keep its own name after canonicalization"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"fake-soldr-v1");
}

#[test]
fn a_symlinked_alias_left_by_an_older_soldr_is_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(source) = symlinked_source(&tmp) else {
        return;
    };
    let target = source.with_file_name("soldr-daemon");
    // What older releases left behind: a link to the `soldr` symlink.
    crate::platform::fs::links::create(
        &std::fs::canonicalize(&source).unwrap().to_string_lossy(),
        &target,
        false,
    )
    .unwrap();
    let result = materialize_executable(&source, &target).unwrap();
    assert!(result.created);
    assert!(!is_symlink(&target), "{} is a symlink", target.display());
    // And it stays repaired: the next call is a no-op.
    assert!(!materialize_executable(&source, &target).unwrap().created);
}

#[test]
fn unchanged_copy_uses_validated_materialization_memo() {
    let tmp = tempfile::tempdir().unwrap();
    let source = fake_source(&tmp, b"fake-soldr-v1");
    let target = tmp.path().join("rustc");
    materialize_executable(&source, &target).unwrap();
    // Force the cross-volume fallback shape: the normal tempdir path
    // creates a hardlink, whose contents naturally change with source.
    std::fs::remove_file(&target).unwrap();
    std::fs::copy(&source, &target).unwrap();
    let copied = materialization_memo(&source, &target).unwrap();
    assert!(write_materialization_memo_if_unchanged(
        &source,
        &target,
        &copied.source,
        Some(&copied.target),
    ));

    assert!(materialization_memo_matches(&source, &target));
    let second = materialize_executable(&source, &target).unwrap();
    assert!(!second.created);

    let original_modified = std::fs::metadata(&source).unwrap().modified().unwrap();
    std::fs::remove_file(&source).unwrap();
    // A different length makes the FileStamp invalidation deterministic
    // even when two writes share one coarse filesystem timestamp.
    std::fs::write(&source, b"fake-soldr-v2-longer").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_modified(original_modified)
        .unwrap();
    assert!(!materialization_memo_matches(&source, &target));
    let replaced = materialize_executable(&source, &target).unwrap();
    assert!(replaced.created);
    assert_eq!(std::fs::read(target).unwrap(), b"fake-soldr-v2-longer");
}

#[test]
fn memo_rejects_source_change_after_content_verification() {
    let tmp = tempfile::tempdir().unwrap();
    let source = fake_source(&tmp, b"fake-soldr-v1");
    let target = tmp.path().join("rustc");
    std::fs::copy(&source, &target).unwrap();
    let verified = materialization_memo(&source, &target).unwrap();
    assert!(executable_matches(&target, &source).unwrap());

    std::fs::remove_file(&source).unwrap();
    // Make the replacement a different length so the test proves a real
    // FileStamp change even on mounts whose timestamp granularity cannot
    // distinguish two immediate writes. Same-size content replacement is
    // covered by `unchanged_copy_uses_validated_materialization_memo`,
    // which restores the original mtime before the content comparison.
    std::fs::write(&source, b"fake-soldr-v2-longer").unwrap();
    assert!(!write_materialization_memo_if_unchanged(
        &source,
        &target,
        &verified.source,
        Some(&verified.target),
    ));
    assert!(!materialization_memo_path(&target).exists());
}

#[test]
fn materialize_executable_accepts_an_identical_concurrent_winner() {
    let tmp = tempfile::tempdir().unwrap();
    let source = fake_source(&tmp, b"fake-soldr-v1");
    let target = tmp.path().join("rustc");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
    let writers = (0..16)
        .map(|_| {
            let source = source.clone();
            let target = target.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                materialize_executable(&source, &target)
            })
        })
        .collect::<Vec<_>>();

    for writer in writers {
        writer
            .join()
            .expect("materialization writer panicked")
            .expect("concurrent materialization failed");
    }
    assert_eq!(std::fs::read(target).unwrap(), b"fake-soldr-v1");
}

#[test]
fn materialize_executable_replaces_different_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let source = fake_source(&tmp, b"fake-soldr-v1");
    let target = tmp.path().join("rustfmt");
    materialize_executable(&source, &target).unwrap();
    std::fs::remove_file(&source).unwrap();
    std::fs::write(&source, b"fake-soldr-v2").unwrap();
    let replaced = materialize_executable(&source, &target).unwrap();
    assert!(replaced.created);
    assert_eq!(std::fs::read(&target).unwrap(), b"fake-soldr-v2");
}

// Regression guard for the concurrent-install failure: reinstalling soldr
// while a build was running could not replace a shim whose image was
// mapped by a live `rustc` wrapper. Windows refuses to delete or overwrite
// a running executable, so the old publish path (`remove_file` + retry)
// burned all 20 attempts and returned an error, while a cargo process that
// spawned `<shims>/rustc` in the meantime saw a bare "cannot find the path
// specified" with no compiler diagnostics. Renaming the running image
// aside succeeds where deleting it cannot.
//
// This test fails against the pre-fix implementation.
#[test]
fn replaces_a_shim_whose_image_is_currently_running() {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
        // Only Windows refuses to delete or overwrite a running
        // image; the test fixtures are Windows-only.
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("running-shim.exe");
    // A real executable is required: the point is a mapped image, which no
    // amount of fake bytes reproduces.
    let system_exe = std::path::Path::new(r"C:\Windows\System32\ping.exe");
    std::fs::copy(system_exe, &target).unwrap();

    let mut child = std::process::Command::new(&target)
        .args(["-n", "30", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn shim under test");

    // Sanity: the image really is locked against deletion right now.
    assert!(
        std::fs::remove_file(&target).is_err(),
        "precondition failed: a running image should not be deletable"
    );

    let source = fake_source(&tmp, b"fake-soldr-next-version");
    let result = materialize_executable(&source, &target);

    let _ = child.kill();
    let _ = child.wait();

    let replaced = result.expect("replacing a running shim must succeed");
    assert!(replaced.created);
    assert_eq!(std::fs::read(&target).unwrap(), b"fake-soldr-next-version");
}

#[test]
fn replacing_a_shim_keeps_the_path_readable_for_spawners() {
    let tmp = tempfile::tempdir().unwrap();
    let source = fake_source(&tmp, b"fake-soldr-v1");
    let target = tmp.path().join("rustc");
    materialize_executable(&source, &target).unwrap();

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let missing = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let observer = {
        let target = target.clone();
        let stop = std::sync::Arc::clone(&stop);
        let missing = std::sync::Arc::clone(&missing);
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                // A spawner only ever needs *some* shim at this path.
                if std::fs::metadata(&target).is_err() {
                    missing.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        })
    };

    for generation in 2..40u8 {
        let bytes = format!("fake-soldr-v{generation}");
        std::fs::remove_file(&source).unwrap();
        std::fs::write(&source, bytes.as_bytes()).unwrap();
        materialize_executable(&source, &target).unwrap();
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    observer.join().unwrap();

    assert_eq!(
        missing.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "shim path disappeared while it was being replaced"
    );
}

#[test]
fn swap_stale_target_restores_the_shim_when_publish_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("rustc");
    std::fs::write(&target, b"stale-shim").unwrap();
    // A tmp path that does not exist makes the publishing rename fail.
    let missing_tmp = tmp.path().join("never-created");

    let err = swap_stale_target(&missing_tmp, &target).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"stale-shim",
        "a failed publish must leave the previous shim in place"
    );
}

#[test]
fn stale_paths_are_unique_and_swept() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("rustc");
    let mut paths = std::collections::HashSet::new();
    for _ in 0..256 {
        assert!(paths.insert(stale_path_for(&target)));
    }
    for path in &paths {
        std::fs::write(path, b"leftover").unwrap();
    }
    std::fs::write(&target, b"current").unwrap();
    sweep_stale_siblings(&target);
    for path in &paths {
        assert!(!path.exists(), "stale sibling was not swept: {path:?}");
    }
    assert!(target.exists(), "sweep must not remove the live shim");
}

#[test]
fn temporary_paths_are_unique_within_one_process() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("rustc");
    let mut paths = std::collections::HashSet::new();
    for _ in 0..1_024 {
        assert!(paths.insert(tmp_path_for(&target)));
    }
}
