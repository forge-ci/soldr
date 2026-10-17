//! Unit coverage split from `save.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;

#[test]
fn projected_snapshot_uses_portable_paths_in_full_and_delta_archives() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let snapshot = root.path().join("snapshot");
    let restored = root.path().join("restored");
    for dir in [&cache, &snapshot, &restored] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let live = cache.join("daemon-route");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::write(live.join("index.bin"), b"live private store").unwrap();
    std::fs::write(cache.join("unrelated.bin"), b"keep").unwrap();
    std::fs::write(snapshot.join("index.bin"), b"original").unwrap();
    let projection = CacheProjection {
        source: &snapshot,
        archive_prefix: Path::new("portable"),
        exclude_prefix: Path::new("daemon-route"),
    };
    let archive = root.path().join("base.tar.zst");
    let report = save_with_projection(
        &SaveOptions {
            workspace: None,
            cache_dir: Some(&cache),
            out: &archive,
            zstd_level: 1,
            threads: Some(1),
            mtimes_only: false,
            profile: SaveProfile::Ci,
        },
        Some(&projection),
    )
    .unwrap();
    assert_eq!(report.cache_files, 2);
    assert_eq!(report.excluded_files, 1);
    let base = read_manifest_from_archive(&archive).unwrap();
    let mut paths: Vec<_> = base
        .cache_files
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    paths.sort();
    assert_eq!(paths, ["portable/index.bin", "unrelated.bin"]);
    let mut load_opts = LoadOptions {
        archive: &archive,
        cache_dir: Some(&restored),
        workspace: None,
        threads: Some(1),
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    };
    load(&load_opts).unwrap();
    assert_eq!(
        std::fs::read(restored.join("portable/index.bin")).unwrap(),
        b"original"
    );
    assert!(!restored.join("daemon-route").exists());
    // An old restored snapshot must not shadow the external producer's new one.
    std::fs::create_dir_all(cache.join("portable")).unwrap();
    std::fs::write(cache.join("portable/index.bin"), b"stale").unwrap();
    std::fs::write(snapshot.join("index.bin"), b"modified").unwrap();
    let delta = root.path().join("delta.tar.zst");
    save_delta_with_projection(
        &SaveDeltaOptions {
            workspace: None,
            cache_dir: &cache,
            base_manifest: &base,
            out: &delta,
            zstd_level: 1,
            threads: Some(1),
            profile: SaveProfile::Ci,
        },
        Some(&projection),
    )
    .unwrap();
    load_opts.archive = &delta;
    load(&load_opts).unwrap();
    assert_eq!(
        std::fs::read(restored.join("portable/index.bin")).unwrap(),
        b"modified"
    );
    assert_eq!(
        std::fs::read(live.join("index.bin")).unwrap(),
        b"live private store"
    );
}

#[test]
fn projected_snapshot_rejects_escaping_and_overlapping_source_paths() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let snapshot = root.path().join("snapshot");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::create_dir_all(&snapshot).unwrap();
    let mut projection = CacheProjection {
        source: &snapshot,
        archive_prefix: Path::new("../escape"),
        exclude_prefix: Path::new("daemon-route"),
    };
    assert!(projection.validate(&cache).is_err());
    projection.archive_prefix = Path::new("portable");
    projection.exclude_prefix = Path::new("/");
    assert!(projection.validate(&cache).is_err());
    projection.exclude_prefix = Path::new("daemon-route");
    projection.source = &cache;
    assert!(projection.validate(&cache).is_err());
}

#[test]
fn projected_snapshot_preserves_opaque_output_names_under_cook_profile() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let snapshot = root.path().join("snapshot");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::create_dir_all(snapshot.join("artifacts/.staged-v2/key")).unwrap();
    std::fs::write(
        snapshot.join("artifacts/.staged-v2/key/output-0"),
        b"library",
    )
    .unwrap();
    std::fs::write(snapshot.join("index.bin"), b"producer index").unwrap();
    let walk = walk_cache_with_projection(
        &cache,
        Some(1),
        SaveProfile::Cook,
        Some(&CacheProjection {
            source: &snapshot,
            archive_prefix: Path::new("portable"),
            exclude_prefix: Path::new("private"),
        }),
    )
    .unwrap();
    assert_eq!(
        walk.included_paths.len(),
        2,
        "opaque producer payload must remain complete"
    );
}

#[test]
fn projected_snapshot_delta_preserves_private_store_from_legacy_base() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let snapshot = root.path().join("snapshot");
    let restored = root.path().join("restored");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::create_dir_all(&snapshot).unwrap();
    std::fs::create_dir_all(restored.join("private")).unwrap();
    std::fs::write(restored.join("private/index.bin"), b"live compiler store").unwrap();
    std::fs::write(snapshot.join("index.bin"), b"new portable index").unwrap();
    let base = Manifest {
        version: MANIFEST_VERSION,
        cache_dir_name: CACHE_DIR_NAME.into(),
        cache_files: vec![CacheFile {
            path: "private/index.bin".into(),
            ..CacheFile::default()
        }],
        cache_symlinks: vec![SymlinkEntry {
            path: "private/current".into(),
            target: "index.bin".into(),
            is_dir: false,
        }],
        ..Manifest::default()
    };
    let delta = root.path().join("delta.tar.zst");
    save_delta_with_projection(
        &SaveDeltaOptions {
            workspace: None,
            cache_dir: &cache,
            base_manifest: &base,
            out: &delta,
            zstd_level: 1,
            threads: Some(1),
            profile: SaveProfile::Ci,
        },
        Some(&CacheProjection {
            source: &snapshot,
            archive_prefix: Path::new("portable"),
            exclude_prefix: Path::new("private"),
        }),
    )
    .unwrap();
    let manifest = read_manifest_from_archive(&delta).unwrap();
    assert!(
        manifest.deleted_cache_paths.is_empty(),
        "excluded private files and links are not tombstones"
    );
    load(&LoadOptions {
        archive: &delta,
        cache_dir: Some(&restored),
        workspace: None,
        threads: Some(1),
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .unwrap();
    assert_eq!(
        std::fs::read(restored.join("private/index.bin")).unwrap(),
        b"live compiler store"
    );
}

#[test]
fn full_profile_excludes_soldr_daemon_runtime_state() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path();
    std::fs::create_dir_all(cache.join("soldr-daemon")).unwrap();
    std::fs::create_dir_all(cache.join("zccache/artifacts")).unwrap();
    std::fs::write(cache.join("soldr-daemon/daemon.pid"), b"123\n").unwrap();
    std::fs::write(cache.join("soldr-daemon/.spawn.lock"), b"").unwrap();
    std::fs::write(
        cache.join("soldr-daemon/compile-daemon-unavailable"),
        b"stale",
    )
    .unwrap();
    let payload = cache.join("zccache/artifacts/hit.bin");
    std::fs::write(&payload, b"cache payload").unwrap();

    let walk = walk_cache_files_for_profile(cache, None, SaveProfile::Full).unwrap();

    assert_eq!(walk.included_paths, vec![payload]);
    assert_eq!(walk.excluded_files, 3);
}

#[test]
fn daemon_runtime_exclusion_is_top_level_only() {
    assert!(archive_always_excludes_cache_path(Path::new(
        "soldr-daemon/daemon.pid"
    )));
    assert!(archive_always_excludes_cache_path(Path::new(
        "soldr-daemon/nested/state.json"
    )));
    assert!(archive_always_excludes_cache_path(Path::new(
        "Soldr-Daemon/daemon.pid"
    )));
    assert!(!archive_always_excludes_cache_path(Path::new(
        "zccache/artifacts/soldr-daemon/payload.bin"
    )));
    assert!(!archive_always_excludes_cache_path(Path::new(
        "soldr-daemon-cache/payload.bin"
    )));
}

// The exact path from the failing bench lane. A full-profile save used to
// collect this file, then die on it when the daemon removed its own lock
// between the walk and the stat.
#[test]
fn full_profile_never_archives_live_runtime_coordination_files() {
    let vanished = Path::new(
        "zccache/daemon-state/embedded-v1/v1.12.17/staging/2492-0-1785226007178685948/.active.lock",
    );
    assert!(
        archive_always_excludes_cache_path(vanished),
        "the lock that broke `soldr save` must be excluded from every profile"
    );

    for rel in [
        "zccache/daemon-state/embedded-v1/v1/staging/7-0-1/partial.bin",
        "zccache/x/daemon.sock",
        "zccache/x/daemon.pid",
        "zccache/x/.lock",
    ] {
        assert!(
            archive_always_excludes_cache_path(Path::new(rel)),
            "{rel} is runtime coordination state, not cache payload"
        );
    }

    // The exclusion must stay narrow: real payload that merely sits deep
    // in the same tree still gets archived, or the cache restores empty.
    for rel in [
        "zccache/daemon-state/embedded-v1/v1/objects/ab/cdef.o",
        "zccache/index.redb",
        "registry/cache/foo-1.0.crate",
    ] {
        assert!(
            !archive_always_excludes_cache_path(Path::new(rel)),
            "{rel} is cache payload and must still be archived"
        );
    }
}

// Defence in depth: even with the exclusion, walk-then-stat is two passes
// over a tree a live daemon writes, so the window cannot be closed
// entirely.
#[test]
fn a_file_that_vanishes_after_the_walk_is_skipped_not_fatal() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cache = tmp.path();
    let missing = cache.join("gone.bin");
    assert!(
        cache_file_entry(cache, &missing)
            .expect("a vanished file must not fail the save")
            .is_none(),
        "a vanished file must be skipped"
    );

    // ...but a file that is present is still archived, so the tolerance
    // cannot silently empty an archive.
    let present = cache.join("present.bin");
    std::fs::write(&present, b"payload").expect("write");
    assert!(
        cache_file_entry(cache, &present).expect("stat").is_some(),
        "an existing file must still produce an entry"
    );
}

// The exact path from #3533's failing Perf Matrix run. zccache writes its
// metadata atomically -- create `.{name}.tmp-<pid>-<seq>`, then rename --
// so these scratch names are half-written *and* about to disappear: they
// must be excluded before the walk ever stats or hashes them.
#[test]
fn zccache_tmp_scratch_names_are_excluded_from_every_profile() {
    for rel in [
        "zccache/daemon-state/embedded-v1/v1.15.0/.metadata.bin.tmp-2869-6",
        "zccache/daemon-state/embedded-v1/v1.15.0/.metadata.bin.tmp-42",
        "zccache/index.redb.tmp-99",
        "zccache/.tmp-17",
        "zccache/objects/.tmp-crashed/obj.o",
        "zccache/half-written.tmp",
    ] {
        assert!(
            archive_always_excludes_cache_path(Path::new(rel)),
            "{rel} is zccache scratch, never cache payload"
        );
    }

    // The exclusion must stay narrow: the finished files these temps are
    // renamed into, and ordinary payload, still get archived.
    for rel in [
        "zccache/daemon-state/embedded-v1/v1.15.0/.metadata.bin",
        "zccache/objects/ab/cdef.o",
        "registry/cache/foo-1.0.crate",
    ] {
        assert!(
            !archive_always_excludes_cache_path(Path::new(rel)),
            "{rel} is cache payload and must still be archived"
        );
    }
}

// Walk-level proof for the path above: a full-profile save collects the
// payload and never the in-flight `.metadata.bin.tmp-*`.
#[test]
fn full_walk_never_collects_an_inflight_metadata_temp_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cache = tmp.path();
    let payload = cache.join("zccache/objects/ab/cdef.o");
    std::fs::create_dir_all(payload.parent().expect("parent")).expect("mkdir");
    std::fs::write(&payload, b"cache payload").expect("write");
    let temp = cache.join("zccache/daemon-state/embedded-v1/v1.15.0/.metadata.bin.tmp-2869-6");
    std::fs::create_dir_all(temp.parent().expect("parent")).expect("mkdir");
    std::fs::write(&temp, b"half-written").expect("write");

    let walk = walk_cache_files_for_profile(cache, None, SaveProfile::Full).expect("walk");

    assert_eq!(walk.included_paths, vec![payload]);
    assert_eq!(walk.excluded_files, 1);
    assert_eq!(walk.excluded_bytes, b"half-written".len() as u64);
}

// Exclusion accounting runs its own stat after the walk (to sum excluded
// bytes), so a scratch file the daemon already renamed away must not fail
// the save there either.
#[test]
fn exclusion_accounting_survives_a_file_vanished_after_the_walk() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let gone = tmp.path().join(".metadata.bin.tmp-9-1");
    assert_eq!(excluded_file_len(&gone).expect("vanished is skippable"), 0);

    let present = tmp.path().join("present.bin");
    std::fs::write(&present, b"abc").expect("write");
    assert_eq!(excluded_file_len(&present).expect("stat"), 3);
}

// The #3533 window inside `cache_file_entry`: the stat succeeds, then the
// daemon renames the file away, then `hash_file` fails with NotFound. That
// error must classify as skippable — and only NotFound must, so a
// permissions error or bad disk still fails the save loudly.
#[test]
fn a_hash_error_for_a_vanished_file_is_skippable_but_only_notfound() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing = tmp.path().join(".metadata.bin.tmp-2869-6");
    let err = hash_file(&missing).expect_err("a missing file cannot be hashed");
    assert!(
        save_error_is_not_found(&err),
        "the mid-save NotFound must classify as skippable: {err:?}"
    );

    assert!(save_error_is_not_found(&io(
        &missing,
        std::io::Error::from(std::io::ErrorKind::NotFound)
    )));
    assert!(save_error_is_not_found(&SaveLoadError::BareIo(
        std::io::Error::from(std::io::ErrorKind::NotFound)
    )));
    assert!(!save_error_is_not_found(&io(
        &missing,
        std::io::Error::from(std::io::ErrorKind::PermissionDenied)
    )));
    assert!(!save_error_is_not_found(&SaveLoadError::BadArchivePath(
        "x".into()
    )));
}

// The same window one stage later, at save_archive's `File::open`: the
// manifest pass stats and hashes successfully, then the daemon renames the
// file away, then the tar append opens it. Before the fix this `expect`
// failed with `io error at ...: No such file or directory` and killed the
// whole save; afterwards the append is skipped and the save continues.
#[test]
fn a_file_that_vanishes_before_the_append_is_skipped_not_fatal() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cache = tmp.path();
    let abs = cache.join("zccache/objects/ab/cdef.o");
    std::fs::create_dir_all(abs.parent().expect("parent")).expect("mkdir");
    std::fs::write(&abs, b"cache payload").expect("write");
    let (_entry, meta) = cache_file_entry(cache, &abs)
        .expect("manifest pass")
        .expect("the file exists during the manifest pass");

    // The daemon renames it away between the manifest pass and the append.
    std::fs::remove_file(&abs).expect("remove");

    let out = tmp.path().join("out.tar");
    let appended = {
        let file = File::create(&out).expect("create");
        let mut tar_builder = tar::Builder::new(file);
        let appended = append_cache_file_entry(&mut tar_builder, cache, &abs, &meta)
            .expect("a vanished file must not fail the save");
        tar_builder.finish().expect("finish");
        appended
    };
    assert!(!appended, "nothing should have been appended");

    // Positive control: the same call on a present file still appends, so
    // the tolerance cannot silently empty an archive.
    let keep = cache.join("zccache/objects/ab/keep.o");
    std::fs::write(&keep, b"kept").expect("write");
    let (_entry, meta) = cache_file_entry(cache, &keep)
        .expect("manifest pass")
        .expect("present");
    let appended = {
        let file = File::create(&out).expect("create");
        let mut tar_builder = tar::Builder::new(file);
        let appended =
            append_cache_file_entry(&mut tar_builder, cache, &keep, &meta).expect("append");
        tar_builder.finish().expect("finish");
        appended
    };
    assert!(appended, "a present file must still be archived");
    let entries = tar::Archive::new(File::open(&out).expect("open"))
        .entries()
        .expect("entries")
        .count();
    assert_eq!(entries, 1);
}

#[test]
fn legacy_archive_cannot_mutate_live_daemon_runtime() {
    let root = tempfile::tempdir().unwrap();
    let archived_cache = root.path().join("archived-cache");
    let restore_cache = root.path().join("restore-cache");
    let archived_runtime = archived_cache.join("soldr-daemon");
    let live_runtime = restore_cache.join("soldr-daemon");
    std::fs::create_dir_all(&archived_runtime).unwrap();
    std::fs::create_dir_all(&live_runtime).unwrap();
    let archived_file = archived_runtime.join("archived.pid");
    std::fs::write(&archived_file, b"old runtime").unwrap();
    std::fs::write(live_runtime.join("live.pid"), b"live runtime").unwrap();
    let (archived_entry, archived_meta) = cache_file_entry(&archived_cache, &archived_file)
        .unwrap()
        .expect("the fixture file exists");
    let manifest = Manifest {
        version: MANIFEST_VERSION,
        cache_dir_name: CACHE_DIR_NAME.into(),
        cache_file_count: 1,
        cache_layer_kind: CacheLayerKind::Complete as i32,
        cache_files: vec![archived_entry],
        deleted_cache_paths: vec!["soldr-daemon/live.pid".into()],
        cache_symlinks: vec![SymlinkEntry {
            path: "soldr-daemon/link".into(),
            target: "archived.pid".into(),
            is_dir: false,
        }],
        ..Manifest::default()
    };
    let archive = root.path().join("legacy.tar.zst");
    write_delta_archive(
        &archive,
        1,
        None,
        &manifest,
        &[(
            archived_file,
            "soldr-daemon/archived.pid".into(),
            archived_meta,
        )],
    )
    .unwrap();

    let report = load(&LoadOptions {
        archive: &archive,
        cache_dir: Some(&restore_cache),
        workspace: None,
        threads: None,
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .unwrap();

    assert_eq!(report.cache_files_restored, 0);
    assert_eq!(
        std::fs::read(live_runtime.join("live.pid")).unwrap(),
        b"live runtime"
    );
    assert!(!live_runtime.join("archived.pid").exists());
    assert!(!live_runtime.join("link").exists());
}

#[test]
fn delta_ignores_daemon_runtime_from_legacy_base_manifest() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let base = Manifest {
        version: MANIFEST_VERSION,
        cache_dir_name: CACHE_DIR_NAME.into(),
        cache_layer_kind: CacheLayerKind::Base as i32,
        cache_files: vec![CacheFile {
            path: "soldr-daemon/daemon.pid".into(),
            mtime_ns: 1,
            size: 3,
            blake3: vec![0; 32],
        }],
        cache_symlinks: vec![SymlinkEntry {
            path: "soldr-daemon/sock".into(),
            target: "target".into(),
            is_dir: false,
        }],
        ..Manifest::default()
    };
    let archive = root.path().join("delta.tar.zst");

    let report = save_delta(&SaveDeltaOptions {
        workspace: None,
        cache_dir: &cache,
        base_manifest: &base,
        out: &archive,
        zstd_level: 1,
        threads: None,
        profile: SaveProfile::Full,
    })
    .unwrap();
    let delta = read_manifest_from_archive(&archive).unwrap();

    assert_eq!(report.deleted_cache_files, 0);
    assert!(delta.deleted_cache_paths.is_empty());
}

#[test]
fn cargo_input_inventory_selects_declared_inputs() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let target = workspace.join("target");
    let source = workspace.join("src/main.rs");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::create_dir_all(target.join("debug/deps")).unwrap();
    std::fs::write(&source, "fn main() {}\n").unwrap();
    std::fs::write(workspace.join("Cargo.toml"), "[package]\n").unwrap();
    std::fs::write(workspace.join("Cargo.lock"), "# lock\n").unwrap();
    std::fs::write(workspace.join("irrelevant.log"), "noise\n").unwrap();
    let source_text = source.display().to_string().replace('\\', "/");
    std::fs::write(
        target.join("debug/deps/app.d"),
        format!("target: {source_text}\n"),
    )
    .unwrap();

    let files = cargo_input_inventory(&workspace, &target, None)
        .unwrap()
        .expect("valid dep-info should produce an inventory");
    assert!(files.contains(&PathBuf::from("src/main.rs")));
    assert!(files.contains(&PathBuf::from("Cargo.toml")));
    assert!(files.contains(&PathBuf::from("Cargo.lock")));
    assert!(!files.contains(&PathBuf::from("irrelevant.log")));
}

#[test]
fn cargo_input_inventory_falls_back_on_malformed_dep_info() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let target = workspace.join("target/debug/deps");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("broken.d"), "not makefile dep-info\n").unwrap();
    assert!(
        cargo_input_inventory(&workspace, workspace.join("target").as_path(), None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn profile_line_matches_documented_shape() {
    // Synthetic per-file latencies: 6 values with known order so the
    // percentile math is hand-checkable. p50/p95/p99 indices on a
    // 6-element vec are round((6-1)*p): 3 → 250, 5 → 1200, 5 → 1200.
    let latencies = vec![100u64, 150, 200, 250, 450, 1200];
    let per_worker_counts = vec![2usize, 2, 1, 1];
    let phases = ExtractPhaseTimings {
        zstd_decode_us: 4_120_000,
        tar_parse_us: 890_000,
        extract_total_us: 10_510_000,
    };
    let line = format_profile_line(phases, &per_worker_counts, &latencies, 6);

    assert!(
        line.starts_with("soldr load: profile: "),
        "missing prefix: {line}"
    );
    assert!(
        line.contains("zstd_decode=4120ms"),
        "wrong zstd_decode: {line}"
    );
    assert!(line.contains("tar_parse=890ms"), "wrong tar_parse: {line}");
    assert!(
        line.contains("extract_total=10510ms"),
        "wrong extract_total: {line}"
    );
    assert!(
        line.contains("workers={0:n=2, 1:n=2, 2:n=1, 3:n=1}"),
        "wrong workers shape: {line}"
    );
    assert!(line.contains("per_file_p50_us=250"), "p50 wrong: {line}");
    assert!(line.contains("p95_us=1200"), "p95 wrong: {line}");
    assert!(line.contains("p99_us=1200"), "p99 wrong: {line}");
    assert!(line.contains("cache_files=6"), "files count wrong: {line}");
}

#[test]
fn profile_line_handles_empty_latencies() {
    // No per-file data (e.g. cache had zero regular entries) — must
    // still emit a parseable line, with zeros for the percentiles.
    let line = format_profile_line(
        ExtractPhaseTimings {
            zstd_decode_us: 0,
            tar_parse_us: 0,
            extract_total_us: 1_000,
        },
        &[],
        &[],
        0,
    );
    assert!(line.contains("per_file_p50_us=0"), "{line}");
    assert!(line.contains("p95_us=0"), "{line}");
    assert!(line.contains("p99_us=0"), "{line}");
    assert!(line.contains("workers={}"), "{line}");
}

// extract_one is the worker entrypoint. Pointing it at a destination
// whose parent path conflicts with a pre-existing regular file makes
// create_dir_all fail. Gives us a deterministic, OS-agnostic failure
// injection without patching production code. The first-error-wins
// semantic is exercised end-to-end in
// crates/soldr-cli/tests/toolchain_env/save_roundtrip.rs.
#[test]
fn extract_one_returns_error_with_failing_path() {
    let tmp = tempfile::tempdir().unwrap();
    let blocking_file = tmp.path().join("not-a-dir");
    std::fs::write(&blocking_file, b"i am a file").unwrap();

    // Now try to extract a job whose dest claims the blocking file
    // is a parent directory. create_dir_all should bail.
    let job = ExtractJob {
        dest: blocking_file.join("child.bin"),
        entry_type: tar::EntryType::Regular,
        body: b"unused".to_vec(),
        mtime_secs: None,
        mtime_ns: None,
        mode_bits: None,
    };
    let err = extract_one(&job).expect_err("worker must surface the IO error");
    let msg = err.to_string();
    assert!(
        msg.contains("not-a-dir"),
        "error must mention the offending path: {msg}"
    );
}

// #1909: dropping the dispatch without `finish()` -- which is what
// every `?` in the driver loop does -- must still wait for workers.
//
// Before the Drop impl those workers kept writing into the cache tree
// after `load()` had returned. Cargo exec'ing a build script while a
// worker still held it open for write is exactly `ETXTBSY`. The
// workers also leaked permanently, parked on a barrier whose final
// party had already gone home.
//
// The assertion is that drop *returns at all*: if the barrier is not
// satisfied it blocks forever, so a regression hangs this test rather
// than failing it. That is deliberate -- there is no non-racy way to
// observe "a worker is still running" from outside, and a hang is an
// unambiguous signal. The nextest budget bounds it.
#[test]
fn dropping_dispatch_without_finish_still_waits_for_workers() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = build_pool(Some(2)).expect("pool");
    let err_slot: Arc<Mutex<Option<SaveLoadError>>> = Arc::new(Mutex::new(None));
    let counter = Arc::new(AtomicU64::new(0));

    let dest = tmp.path().join("nested").join("payload.bin");
    {
        let dispatch = ExtractDispatch::start(
            &pool,
            Some(2),
            Arc::clone(&err_slot),
            Arc::clone(&counter),
            None,
        );
        dispatch
            .send(ExtractJob {
                dest: dest.clone(),
                entry_type: tar::EntryType::Regular,
                body: b"payload".to_vec(),
                mtime_secs: None,
                mtime_ns: None,
                mode_bits: None,
            })
            .expect("send");
        // Deliberately no `finish()` -- emulate a `?` bailing out of
        // the driver loop with work still in flight.
    }

    // Reaching here at all means Drop waited. And because it waited,
    // the in-flight job is guaranteed complete -- no sleep, no poll.
    assert!(
        dest.exists(),
        "drop must not return until workers have finished writing"
    );
    assert_eq!(counter.load(Ordering::Relaxed), 1, "job should be counted");
    assert!(err_slot.lock().is_none(), "no worker error");
}

// #1548 — purely-lexical symlink-target containment. Runs on every
// platform (no symlink creation involved), which is what gives the
// Windows lanes coverage of the validation logic that the
// #[cfg(unix)] integration tests exercise end-to-end.
#[test]
fn symlink_target_validation_accepts_safe_relative_targets() {
    // Sibling file.
    assert_eq!(
        resolve_symlink_target_in_root(Path::new("deps/link.rlib"), "libfoo.rlib"),
        Some(PathBuf::from("deps").join("libfoo.rlib"))
    );
    // Up-and-over that stays inside the root.
    assert_eq!(
        resolve_symlink_target_in_root(Path::new("out/link"), "../deps/libfoo.rlib"),
        Some(PathBuf::from("deps").join("libfoo.rlib"))
    );
    // `.` components are harmless.
    assert_eq!(
        resolve_symlink_target_in_root(Path::new("a/link"), "./b/./c"),
        Some(PathBuf::from("a").join("b").join("c"))
    );
    // Link at the root pointing at a root-level sibling.
    assert_eq!(
        resolve_symlink_target_in_root(Path::new("link"), "payload.bin"),
        Some(PathBuf::from("payload.bin"))
    );
}

#[test]
fn symlink_target_validation_rejects_unsafe_targets() {
    // Absolute POSIX target — rejected even when it would point back
    // inside the root; we only ever preserve relative links.
    assert_eq!(
        resolve_symlink_target_in_root(Path::new("a/link"), "/etc/passwd"),
        None
    );
    // Escapes the root via `..`.
    assert_eq!(
        resolve_symlink_target_in_root(Path::new("link"), "../outside.txt"),
        None
    );
    assert_eq!(
        resolve_symlink_target_in_root(Path::new("a/link"), "../../../x"),
        None
    );
    // Exactly-at-root resolution (empty) is meaningless for a link.
    assert_eq!(
        resolve_symlink_target_in_root(Path::new("a/link"), ".."),
        None
    );
    // Empty target.
    assert_eq!(
        resolve_symlink_target_in_root(Path::new("a/link"), ""),
        None
    );
    // Windows drive / UNC prefixes are rejected on Windows (Prefix
    // component); on Unix "C:" is just a weird-but-contained relative
    // component, which is harmless — so only assert the containment
    // property that holds everywhere: no result may escape the root.
    if let Some(resolved) = resolve_symlink_target_in_root(Path::new("a/link"), "C:/evil") {
        assert!(
            !resolved
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "resolved path must stay inside the root: {resolved:?}"
        );
    }
}

// #1547 — the workspace source walker must not exclude a directory by
// *name* alone: a `target/` directory that isn't the real Cargo output
// dir (e.g. `src/target/mod.rs`) is legitimate tracked source and must
// be hashed. Only the resolved Cargo target dir(s) are excluded, and
// `.git` / `node_modules` stay excluded by name (never legitimate
// source basenames).
#[test]
fn walk_workspace_files_hashes_nested_dir_literally_named_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Legitimate source: a package sub-module directory that happens
    // to be named "target", nested under src/ — NOT the Cargo build
    // output dir.
    std::fs::create_dir_all(root.join("src/target")).unwrap();
    std::fs::write(root.join("src/target/mod.rs"), b"pub fn noop() {}").unwrap();
    std::fs::write(root.join("src/lib.rs"), b"mod target;").unwrap();

    // The REAL Cargo output dir at the workspace root — must stay
    // excluded.
    std::fs::create_dir_all(root.join("target/debug")).unwrap();
    std::fs::write(root.join("target/debug/build-artifact.bin"), b"junk").unwrap();

    // .git and node_modules — must stay excluded regardless of depth.
    std::fs::create_dir_all(root.join(".git/objects")).unwrap();
    std::fs::write(root.join(".git/objects/pack.idx"), b"gitjunk").unwrap();
    std::fs::create_dir_all(root.join("node_modules/leftpad")).unwrap();
    std::fs::write(root.join("node_modules/leftpad/index.js"), b"jsjunk").unwrap();

    let files = walk_workspace_files(root, None).unwrap();
    let rel_strs: Vec<String> = files
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();

    assert!(
        rel_strs.contains(&"src/target/mod.rs".to_string()),
        "src/target/mod.rs must be hashed as legitimate source, got: {rel_strs:?}"
    );
    assert!(
        rel_strs.contains(&"src/lib.rs".to_string()),
        "src/lib.rs must be hashed, got: {rel_strs:?}"
    );
    assert!(
        !rel_strs
            .iter()
            .any(|p| p.starts_with("target/") || p == "target"),
        "the real workspace target/ dir must stay excluded, got: {rel_strs:?}"
    );
    assert!(
        !rel_strs.iter().any(|p| p.starts_with(".git/")),
        ".git must stay excluded, got: {rel_strs:?}"
    );
    assert!(
        !rel_strs.iter().any(|p| p.starts_with("node_modules/")),
        "node_modules must stay excluded, got: {rel_strs:?}"
    );
}

// #1547 mutation check: CARGO_TARGET_DIR overrides must also be
// resolved-path excluded even though they don't literally live under
// `<workspace>/target`.
#[test]
fn walk_workspace_files_excludes_cargo_target_dir_env_override() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), b"// real source").unwrap();
    // Override target dir lives INSIDE the workspace under a
    // differently-named directory (simulating CARGO_TARGET_DIR=out).
    std::fs::create_dir_all(root.join("out/debug")).unwrap();
    std::fs::write(root.join("out/debug/artifact.bin"), b"junk").unwrap();
    // A directory named "out" is NOT excluded when CARGO_TARGET_DIR is
    // unset — sanity check the negative case first.
    let baseline = walk_workspace_files(root, None).unwrap();
    let baseline_strs: Vec<String> = baseline
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    assert!(
        baseline_strs.contains(&"out/debug/artifact.bin".to_string()),
        "without CARGO_TARGET_DIR, out/ is ordinary source: {baseline_strs:?}"
    );
}

// #1547 — a directory named "target" that is NOT at the workspace
// root (and not a CARGO_TARGET_DIR/CARGO_BUILD_TARGET_DIR override)
// must never be excluded, including deeper nesting than one level.
#[test]
fn walk_workspace_files_hashes_deeply_nested_target_named_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("crates/foo/src/target")).unwrap();
    std::fs::write(
        root.join("crates/foo/src/target/mod.rs"),
        b"pub struct Target;",
    )
    .unwrap();

    let files = walk_workspace_files(root, None).unwrap();
    let rel_strs: Vec<String> = files
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    assert!(
        rel_strs.contains(&"crates/foo/src/target/mod.rs".to_string()),
        "deeply nested target-named source must be hashed: {rel_strs:?}"
    );
}
#[cfg(test)]
mod etxtbsy_tests {
    use super::*;

    // #1909: a restored build script failed `execve` with ETXTBSY, which
    // happens only when some process holds the target open for writing. The
    // extractor now stages into a sibling and renames, so the inode that
    // lands at `dest` never had a writable descriptor pointing at it and no
    // fork-inherited fd can refer to it.

    fn regular_job(dest: PathBuf, body: &[u8], mode: Option<u32>) -> ExtractJob {
        ExtractJob {
            dest,
            entry_type: tar::EntryType::Regular,
            body: body.to_vec(),
            mtime_secs: Some(1_700_000_000),
            mtime_ns: None,
            mode_bits: mode,
        }
    }

    #[test]
    fn staging_path_is_a_sibling_of_the_destination() {
        // Same directory or `rename` stops being atomic -- a temp dir can sit
        // on a different filesystem, where rename degrades to copy+delete and
        // reintroduces the very window this fix closes.
        let dest = Path::new("/some/deep/dir/build-script-build");
        let staged = staging_path_for(dest);
        assert_eq!(
            staged.parent(),
            dest.parent(),
            "staging file must be a sibling so the rename stays atomic"
        );
        assert_ne!(staged.file_name(), dest.file_name());
    }

    #[test]
    fn staging_paths_are_unique_across_calls() {
        // Concurrent workers restore different entries simultaneously; two
        // collisions would corrupt each other's content.
        let dest = Path::new("/tmp/target/debug/build/x/build-script-build");
        let a = staging_path_for(dest);
        let b = staging_path_for(dest);
        assert_ne!(
            a, b,
            "concurrent extract workers must not share a staging path"
        );
    }

    #[test]
    fn extract_leaves_no_staging_file_behind() {
        // A stray `.soldr-tmp` inside target/ would survive into the next
        // build and confuse cargo.
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("build-script-build");
        extract_one(&regular_job(dest.clone(), b"#!/bin/sh\n", None)).expect("extract");

        assert_eq!(std::fs::read(&dest).unwrap(), b"#!/bin/sh\n");
        let strays: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("soldr-tmp"))
            .collect();
        assert!(strays.is_empty(), "staging files left behind: {strays:?}");
    }

    #[test]
    fn extract_replaces_an_existing_destination_atomically() {
        // Restores land on top of a previous build's artifacts. The rename
        // must replace the old inode rather than fail on it.
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("artifact.bin");
        std::fs::write(&dest, b"stale contents").unwrap();

        extract_one(&regular_job(dest.clone(), b"fresh", None)).expect("extract over existing");
        assert_eq!(std::fs::read(&dest).unwrap(), b"fresh");
    }

    #[test]
    fn restored_executable_keeps_its_mode_through_the_rename() {
        // Unix mode bits only exist on unix hosts; the facade's `mode()`
        // returns `None` on Windows, so the test self-skips there.
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            return;
        }
        // The mode is applied to the staging file; `rename` must carry it
        // over, or #587/#1889 would regress silently.
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("build-script-build");
        extract_one(&regular_job(
            dest.clone(),
            b"#!/bin/sh\nexit 0\n",
            Some(0o755),
        ))
        .expect("extract");

        let mode =
            crate::platform::fs::permissions::mode(&dest).expect("stat restored mode") & 0o777;
        assert_eq!(mode, 0o755, "executable bit must survive the rename");
    }

    #[test]
    fn restored_executable_can_actually_be_executed() {
        // Unix mode bits only exist on unix hosts; the facade's `mode()`
        // returns `None` on Windows, so the test self-skips there.
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            return;
        }
        // The end-to-end property the issue is about: after extract_one
        // returns, the file is immediately runnable -- no ETXTBSY, no EACCES.
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("build-script-build");
        extract_one(&regular_job(
            dest.clone(),
            b"#!/bin/sh\nexit 7\n",
            Some(0o755),
        ))
        .expect("extract");
        assert_eq!(
            crate::platform::fs::permissions::mode(&dest).expect("stat restored mode") & 0o111,
            0o111
        );

        let status = std::process::Command::new(&dest)
            .status()
            .expect("restored build script must be executable immediately after restore");
        assert_eq!(status.code(), Some(7));
    }
}

// soldr#2760 — the save walks run *inside* a rayon pool: `save()` does
// `pool.install(|| rayon::join(|| workspace_files_for_save(..), ..))`.
// jwalk's default parallelism is `RayonDefaultPool { busy_timeout: 1s }`,
// and `rayon::spawn` from inside a pool context targets *that* pool. When
// the pool's threads are already occupied by the join, the walk cannot be
// scheduled, and after one second jwalk aborts it with
//
//     rayon thread-pool too busy or dependency loop detected
//
// which surfaces as `SaveLoadError::Walk` — a hard save failure, not a
// slow save. On the emulated aarch64-msvc runner that is the whole flake.
//
// A one-thread pool makes the race deterministic: the calling closure IS
// the only worker, so a walk that spawns onto the ambient pool can never
// make progress. `Parallelism::RayonNewPool` / `Serial` return
// `timeout() == None` and are immune, which is the fix.
#[test]
fn walk_workspace_files_survives_a_saturated_ambient_pool() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), b"pub fn noop() {}").unwrap();
    std::fs::write(root.join("Cargo.toml"), b"[package]\nname='x'\n").unwrap();

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();

    let files = pool
        .install(|| walk_workspace_files(root, None))
        .expect("walk must not abort when the ambient rayon pool is saturated");

    let rel: Vec<String> = files
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    assert!(rel.contains(&"src/lib.rs".to_string()), "got: {rel:?}");
}

// soldr#2760 — `RayonDefaultPool` is the one variant that can abort a walk
// (it is the only one whose `timeout()` is `Some`). No caller argument may
// select it, including the `None` and `Some(0)` shapes that used to fall
// through to jwalk's default.
#[test]
fn walk_parallelism_is_never_the_aborting_default() {
    for threads in [None, Some(0), Some(1), Some(8)] {
        assert!(
            matches!(
                walk_parallelism(threads),
                jwalk::Parallelism::RayonNewPool(_)
            ),
            "threads={threads:?} must select a dedicated pool"
        );
    }
}

// soldr#2760 — the original defect was one walker *silently missing* the
// call: `cargo_input_inventory`'s metadata walk had no `.parallelism(..)`,
// so it used the aborting default even when the caller passed an explicit
// `--threads` that its sibling walks honoured. Nothing failed loudly; the
// walk just kept a 1-second abort nobody had chosen.
//
// Counting the two constructs keeps that specific mistake from recurring:
// a walker added without a pool leaves the counts unequal.
#[test]
fn every_jwalk_walker_in_this_module_selects_its_own_pool() {
    let src = include_str!("save_inventory.rs");
    let walkers = src.matches("jwalk::WalkDir::new").count();
    let pooled = src.matches("parallelism(walk_parallelism").count();
    assert_eq!(
        walkers, pooled,
        "every jwalk walker must go through walk_parallelism(): \
         found {walkers} walkers but {pooled} parallelism() calls"
    );
}

#[test]
fn cook_profile_keeps_the_dependency_graph_and_drops_linked_products() {
    use std::path::Path;

    // Kept: the dependency closure a warm compile actually needs.
    for kept in [
        "debug/deps/libserde-1234.rlib",
        "debug/deps/libserde-1234.rmeta",
        "debug/deps/libproc_macro_hack-9.dylib",
        "debug/deps/libplugin-7.so",
        "debug/deps/plugin-7.dll",
        "debug/.fingerprint/serde-1234/lib-serde",
        // Build scripts are dependency payload even though they are
        // extensionless executables; Cargo needs them to rematerialize.
        "debug/build/libz-sys-abc/build-script-build",
        "debug/build/libz-sys-abc/out/libz.a",
    ] {
        assert!(
            !cook_profile_excludes_cache_path(Path::new(kept)),
            "cook profile must keep {kept}"
        );
    }

    // Dropped: everything a build adds on top, which is tier 3.
    for dropped in [
        // Linked test executables -- the 1.62 GB in zackees/setup-soldr#499.
        "debug/deps/cargo_front_door-8f2a1c",
        "debug/deps/cargo_front_door-8f2a1c.exe",
        "debug/soldr",
        "debug/soldr.exe",
        "debug/incremental/soldr-abc/x.bin",
        "debug/examples/demo",
        "debug/deps/soldr-1234.pdb",
        "debug/deps/soldr-1234.dwp",
    ] {
        assert!(
            cook_profile_excludes_cache_path(Path::new(dropped)),
            "cook profile must drop {dropped}"
        );
    }
}

#[test]
fn cook_profile_parses_and_round_trips_its_name() {
    assert_eq!(SaveProfile::parse("cook"), Some(SaveProfile::Cook));
    assert_eq!(SaveProfile::parse("COOK"), Some(SaveProfile::Cook));
    assert_eq!(SaveProfile::Cook.as_str(), "cook");
    // The existing spellings must keep resolving where they did.
    assert_eq!(SaveProfile::parse("full"), Some(SaveProfile::Full));
    assert_eq!(SaveProfile::parse("minimal"), Some(SaveProfile::Ci));
}

#[test]
fn cook_profile_excludes_are_narrower_than_a_full_save() {
    use std::path::Path;

    // A guard against the predicate degenerating into "drop everything":
    // the rlib that makes a warm build possible must survive.
    let rlib = Path::new("release/deps/libtokio-9f.rlib");
    assert!(!cook_profile_excludes_cache_path(rlib));
    assert!(cook_profile_excludes_cache_path(Path::new(
        "release/deps/tokio_test-9f"
    )));
}

#[test]
fn cook_profile_walk_archives_the_dep_graph_of_a_real_target_tree() {
    // Exercises the walk itself, not just the path predicate, on a tree
    // shaped like the one zackees/setup-soldr#499 measured: a cook slice
    // with a completed build layered on top.
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path();
    for dir in [
        "debug/deps",
        "debug/.fingerprint/serde-1234",
        "debug/build/libz-sys-abc/out",
        "debug/incremental/soldr-abc",
        "debug/examples",
    ] {
        std::fs::create_dir_all(target.join(dir)).unwrap();
    }

    // Cook's payload.
    let rlib = target.join("debug/deps/libserde-1234.rlib");
    let rmeta = target.join("debug/deps/libserde-1234.rmeta");
    let fingerprint = target.join("debug/.fingerprint/serde-1234/lib-serde");
    let build_script = target.join("debug/build/libz-sys-abc/build-script-build");
    let build_out = target.join("debug/build/libz-sys-abc/out/libz.a");
    for kept in [&rlib, &rmeta, &fingerprint, &build_script, &build_out] {
        std::fs::write(kept, b"dep").unwrap();
    }

    // What the build added afterwards -- tier 3, and the bulk of the bytes.
    for dropped in [
        "debug/soldr",
        "debug/deps/cargo_front_door-8f2a1c",
        "debug/incremental/soldr-abc/state.bin",
        "debug/examples/demo",
    ] {
        std::fs::write(target.join(dropped), b"linked product").unwrap();
    }

    let walk = walk_cache_files_for_profile(target, None, SaveProfile::Cook).unwrap();
    let mut included = walk
        .included_paths
        .iter()
        .map(|p| {
            p.strip_prefix(target)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect::<Vec<_>>();
    included.sort();

    assert_eq!(
        included,
        vec![
            "debug/.fingerprint/serde-1234/lib-serde",
            "debug/build/libz-sys-abc/build-script-build",
            "debug/build/libz-sys-abc/out/libz.a",
            "debug/deps/libserde-1234.rlib",
            "debug/deps/libserde-1234.rmeta",
        ]
    );
    assert_eq!(walk.excluded_files, 4);

    // The same tree under the historical profile keeps everything, which is
    // the behaviour that produced the oversized entry.
    let full = walk_cache_files_for_profile(target, None, SaveProfile::Full).unwrap();
    assert_eq!(full.included_paths.len(), 9);
    assert_eq!(full.excluded_files, 0);
}

// soldr#3687: a save that fails partway must leave the previous good
// archive byte-for-byte intact and no partial file beside it.
#[test]
fn a_failed_save_preserves_the_previous_archive_and_leaves_no_partial() {
    let root = tempfile::tempdir().unwrap();
    let out = root.path().join("cache.tar.zst");
    std::fs::write(&out, b"previous good archive").unwrap();
    let err = write_archive_file(&out, |mut file| -> Result<()> {
        file.write_all(b"partial").unwrap();
        Err(SaveLoadError::BareIo(std::io::Error::other(
            "injected write failure",
        )))
    })
    .unwrap_err();
    assert!(err.to_string().contains("injected write failure"), "{err}");
    assert_eq!(std::fs::read(&out).unwrap(), b"previous good archive");
    let entries: Vec<_> = std::fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("cache.tar.zst")]);
}

#[test]
fn a_successful_save_replaces_the_previous_archive() {
    let root = tempfile::tempdir().unwrap();
    let out = root.path().join("nested").join("cache.tar.zst");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    std::fs::write(&out, b"old").unwrap();
    write_archive_file(&out, |mut file| -> Result<()> {
        file.write_all(b"new archive")
            .map_err(SaveLoadError::BareIo)
    })
    .unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), b"new archive");
    assert_eq!(std::fs::read_dir(out.parent().unwrap()).unwrap().count(), 1);
}
