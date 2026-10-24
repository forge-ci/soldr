//! soldr#3365 regression suite: retired-embedded-zccache-store expiry must be
//! **per file**, keyed on each file's own hard-link count and mtime, never on
//! the newest mtime anywhere in the tree.
//!
//! The fixture in [`host_shaped_stores`] mirrors the retired `v1.13.22` store
//! observed on a dev host on 2026-09-25: 499 GB, 1,463,494 entries, with the
//! newest entry (`.writer.lock`, touched by the last service shutdown) only
//! 27 h old. Under the current whole-tree newest-mtime gate
//! (`latest_tree_mtime` in `zccache_embedded_legacy.rs`) that one fresh
//! bookkeeping write pinned the entire store, however old the bulk of its
//! artifacts were.
//!
//! The issue's addendum measured that store's hard-link counts directly: of
//! 350,674 output files, 348,995 (99.7%, 526.3 GB) had `st_nlink == 1` — no
//! `target/` directory still references them, only the cache's own directory
//! entry does — while 1,679 (1.59 GB) were still hard-linked into a live
//! `target/` tree. That split drives the required behaviour:
//!
//! - `nlink == 1`: the cache is the only reference. Purge **eagerly, with no
//!   age gate at all** — an old binary that later asks for it just gets a
//!   miss and recompiles.
//! - `nlink > 1`: some `target/` directory still holds the inode. Removing
//!   the cache's own link frees no bytes (the inode survives), so it is
//!   age-gated on the file's own mtime like before, and `bytes_reclaimed`
//!   must not count it.
//!
//! Every test below is expected **RED** until per-file, link-count-aware
//! expiry lands: `A`, `B`, `C`, `D`, `D2`, `E`, `H` assert the required
//! per-file/per-link behaviour and currently fail because the whole store is
//! kept or removed as one unit, with no hard-link accounting at all. `F` is a
//! guard on already-correct behaviour (the current version store is never
//! touched) and is expected to pass today and after the fix.

use super::*;
use std::time::Duration;

pub(crate) struct HostShapedStores {
    pub retired: PathBuf,
    pub current: PathBuf,
    /// `nlink == 1`, ages 4-20 d: must be purged eagerly (age doesn't matter,
    /// but they're also past the pressure gate so an age-only implementation
    /// would remove them too — see `fresh_unlinked` for the case that tells
    /// the two apart).
    pub cold_unlinked: Vec<PathBuf>,
    /// `nlink == 1`, ages 1-60 h: must ALSO be purged eagerly, despite being
    /// younger than the pressure gate. This is the whole point of the eager
    /// rule — an age-only sweep keeps these.
    pub fresh_unlinked: Vec<PathBuf>,
    /// `nlink == 2` (cache + `target-sim`), ages 4-20 d: the cache's own link
    /// is age-gated and must be removed; the `target-sim` link must survive.
    pub cold_linked: Vec<PathBuf>,
    /// `nlink == 2` (cache + `target-sim`), ages 10-60 h: younger than the
    /// pressure gate, so both links survive.
    pub warm_linked: Vec<PathBuf>,
    /// `target-sim` counterparts of `cold_linked`, same indices.
    pub cold_linked_targets: Vec<PathBuf>,
    /// `target-sim` counterparts of `warm_linked`, same indices.
    pub warm_linked_targets: Vec<PathBuf>,
    pub current_files: Vec<PathBuf>,
    /// Total bytes across `cold_unlinked` + `fresh_unlinked` — the floor on
    /// `bytes_reclaimed` for a full sweep.
    pub unlinked_artifact_bytes: u64,
    /// Total bytes across `cold_linked` + `warm_linked` — must never be
    /// counted in `bytes_reclaimed`, since removing the cache's link frees no
    /// disk space while `target-sim` still holds the inode.
    pub linked_artifact_bytes: u64,
    /// Total bytes across the bookkeeping files (all `nlink == 1`, so all
    /// eligible for eager removal) — the ceiling on `bytes_reclaimed` above
    /// `unlinked_artifact_bytes`.
    pub bookkeeping_bytes: u64,
}

fn hours(count: f64) -> Duration {
    Duration::from_secs_f64(count * 3600.0)
}

fn set_mtime(path: &std::path::Path, time: SystemTime) {
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(time))
        .unwrap_or_else(|error| panic!("set mtime for {}: {error}", path.display()));
}

/// Build a host-shaped pair of retired (`v1.13.22`) and current
/// (`versioned_subdir()`) embedded-zccache stores under `paths`, all ages
/// relative to `now`. A sibling directory outside the soldr cache root
/// (`target-sim`, alongside `paths.root`) stands in for a Cargo `target/`
/// tree: files hard-linked into it are what a real cache hit materializes.
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub(crate) fn host_shaped_stores(paths: &SoldrPaths, now: SystemTime) -> HostShapedStores {
    let embedded_root = embedded_cache_root(paths);
    let retired = embedded_root.join("v1.13.22");
    assert_ne!(
        "v1.13.22",
        zccache::core::config::versioned_subdir(),
        "fixture retired-store name must not collide with the current version"
    );
    let current = embedded_root.join(zccache::core::config::versioned_subdir());

    let target_sim = paths
        .root
        .parent()
        .expect("soldr root always has a parent")
        .join("target-sim");
    std::fs::create_dir_all(&target_sim).expect("target-sim root (stands in for Cargo's target/)");

    let shard_dir = |shard: u32| retired.join("artifacts").join(format!("{shard:02x}"));
    for shard in 0u32..64 {
        std::fs::create_dir_all(shard_dir(shard)).expect("shard dir");
    }

    let cold_payload = vec![0u8; 4096];

    // cold_unlinked: the bulk, 1,800 files, 4096 B, ages 4-20 d, nlink == 1.
    // An age-only sweep already removes these, so they don't by themselves
    // distinguish the old behaviour from the new one — see fresh_unlinked.
    let mut cold_unlinked = Vec::new();
    let mut unlinked_artifact_bytes: u64 = 0;
    for index in 0..1_800u32 {
        let shard = index % 64;
        let file = shard_dir(shard).join(format!("cold-unlinked-{index:05}.bin"));
        std::fs::write(&file, &cold_payload).expect("cold-unlinked artifact");
        let age_hours = 96.0 + ((index % 17) as f64) * 24.0; // 4 d .. 20 d
        set_mtime(&file, now - hours(age_hours));
        unlinked_artifact_bytes += cold_payload.len() as u64;
        cold_unlinked.push(file);
    }
    assert_eq!(cold_unlinked.len(), 1_800);

    // fresh_unlinked: 60 files aged 1-60 h, nlink == 1. This is the eager
    // rule's whole point: younger than the 72 h pressure gate, so an
    // age-only sweep keeps them, but nothing external references them, so
    // they must be purged anyway.
    let mut fresh_unlinked = Vec::new();
    for index in 0..60u32 {
        let shard = index % 64;
        let file = shard_dir(shard).join(format!("fresh-unlinked-{index:03}.bin"));
        std::fs::write(&file, &cold_payload).expect("fresh-unlinked artifact");
        let age_hours = 1.0 + index as f64; // 1 h .. 60 h
        set_mtime(&file, now - hours(age_hours));
        unlinked_artifact_bytes += cold_payload.len() as u64;
        fresh_unlinked.push(file);
    }
    assert_eq!(fresh_unlinked.len(), 60);

    // cold_linked: 60 files, ages 4-20 d, hard-linked into target-sim
    // (nlink == 2). The cache's own link is age-gated and must go; the
    // target-sim link must survive because the inode is still referenced.
    let mut cold_linked = Vec::new();
    let mut cold_linked_targets = Vec::new();
    let mut linked_artifact_bytes: u64 = 0;
    for index in 0..60u32 {
        let shard = index % 64;
        let file = shard_dir(shard).join(format!("cold-linked-{index:03}.bin"));
        let payload = format!("cold-linked-payload-{index}").into_bytes();
        std::fs::write(&file, &payload).expect("cold-linked artifact");
        let target = target_sim.join(format!("cold-linked-{index:03}.bin"));
        // Hard-link AFTER writing the cache file and BEFORE setting mtimes:
        // a hard link shares the inode, so the mtime set below applies to
        // both directory entries.
        std::fs::hard_link(&file, &target).expect("hard-link cold-linked into target-sim");
        let age_hours = 96.0 + ((index % 17) as f64) * 24.0; // 4 d .. 20 d
        set_mtime(&file, now - hours(age_hours));
        linked_artifact_bytes += payload.len() as u64;
        cold_linked.push(file);
        cold_linked_targets.push(target);
    }
    assert_eq!(cold_linked.len(), 60);

    // warm_linked: 12 files, ages 10-60 h, hard-linked into target-sim
    // (nlink == 2). Younger than the pressure gate, so both links survive.
    let mut warm_linked = Vec::new();
    let mut warm_linked_targets = Vec::new();
    let warm_ages_hours = [10.0, 25.0, 47.0, 60.0];
    for index in 0..12u32 {
        let shard = index % 64;
        let file = shard_dir(shard).join(format!("warm-linked-{index:03}.bin"));
        let payload = format!("warm-linked-payload-{index}").into_bytes();
        std::fs::write(&file, &payload).expect("warm-linked artifact");
        let target = target_sim.join(format!("warm-linked-{index:03}.bin"));
        std::fs::hard_link(&file, &target).expect("hard-link warm-linked into target-sim");
        let age_hours = warm_ages_hours[index as usize % warm_ages_hours.len()];
        set_mtime(&file, now - hours(age_hours));
        linked_artifact_bytes += payload.len() as u64;
        warm_linked.push(file);
        warm_linked_targets.push(target);
    }
    assert_eq!(warm_linked.len(), 12);

    // Bookkeeping mirroring the real store — all nlink == 1, so all eligible
    // for eager removal regardless of the individual ages below (which mirror
    // the real host-observed tree, including the fresh .writer.lock rewrite
    // that pinned the whole store under the old whole-tree gate, and the
    // orphaned .index.bin.tmp-1649967 at 69 h — younger than the 72 h
    // pressure gate, so only the eager nlink==1 rule removes it).
    let bookkeeping: &[(&str, f64)] = &[
        ("tmp/stale.tmp", 19.0 * 24.0),
        (".disk-maintenance-last-full-v1", 14.0 * 24.0),
        ("compiler_hash.bin", 8.0 * 24.0),
        ("metadata.bin", 8.0 * 24.0),
        (".index.bin.tmp-1649967", 69.0),
        ("system_includes.bin", 47.0),
        ("logs/zccache.log", 33.0),
        ("depgraph/graph.bin", 27.0),
        ("index.bin", 27.0),
        (ZCCACHE_WRITER_LOCK_FILE, 27.0),
    ];
    let mut bookkeeping_bytes: u64 = 0;
    for (rel, age_hours) in bookkeeping {
        let file = retired.join(rel);
        std::fs::create_dir_all(file.parent().expect("bookkeeping parent"))
            .expect("bookkeeping parent dir");
        std::fs::write(&file, b"bookkeeping").expect("bookkeeping file");
        set_mtime(&file, now - hours(*age_hours));
        bookkeeping_bytes += b"bookkeeping".len() as u64;
    }
    let staging = retired.join("staging");
    std::fs::create_dir_all(&staging).expect("staging dir (empty)");

    // Something wrote into the tree 27 h ago: set every directory in the
    // retired store fresh, last. This is what proves directory freshness and
    // fresh bookkeeping must never protect anything — under the old
    // whole-tree gate this alone pinned everything.
    let mut retired_dirs = vec![retired.clone(), retired.join("artifacts")];
    for shard in 0u32..64 {
        retired_dirs.push(shard_dir(shard));
    }
    for extra in ["logs", "depgraph", "staging", "tmp"] {
        retired_dirs.push(retired.join(extra));
    }
    for dir in &retired_dirs {
        set_mtime(dir, now - hours(27.0));
    }

    // Current store: never touched by the sweep, however old its files are.
    let mut current_files = Vec::new();
    let current_shard = current.join("artifacts").join("aa");
    std::fs::create_dir_all(&current_shard).expect("current shard dir");
    for i in 0..200u32 {
        let file = current_shard.join(format!("{i:04}.bin"));
        std::fs::write(&file, &cold_payload).expect("current artifact");
        set_mtime(&file, now - hours(40.0 * 24.0));
        current_files.push(file);
    }
    let current_index = current.join("index.bin");
    std::fs::write(&current_index, b"current index").expect("current index");
    set_mtime(&current_index, now - hours(40.0 * 24.0));
    current_files.push(current_index);

    HostShapedStores {
        retired,
        current,
        cold_unlinked,
        fresh_unlinked,
        cold_linked,
        warm_linked,
        cold_linked_targets,
        warm_linked_targets,
        current_files,
        unlinked_artifact_bytes,
        linked_artifact_bytes,
        bookkeeping_bytes,
    }
}

/// A: the core acceptance test from the addendum. Every `nlink == 1` file —
/// cold or fresh — is purged on one sweep; `nlink > 1` files are age-gated on
/// their own mtime and only the cache's own link is removed; `bytes_reclaimed`
/// never counts a still-linked file.
#[test]
fn host_shaped_retired_store_purges_unlinked_files_and_ages_out_linked_ones() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join(".soldr"));
    let now = SystemTime::now();
    let fixture = host_shaped_stores(&paths, now);

    let report = sweep_legacy_cache_roots(&paths, now, crate::cache_lib::gc_policy::STALENESS_GATE);

    let unlinked_survivors: Vec<&PathBuf> = fixture
        .cold_unlinked
        .iter()
        .chain(fixture.fresh_unlinked.iter())
        .filter(|path| path.exists())
        .collect();
    assert!(
        unlinked_survivors.is_empty(),
        "{} unlinked (nlink==1) artifacts survived the sweep, expected eager purge regardless \
         of age; first few: {:?}; report={report:?}",
        unlinked_survivors.len(),
        unlinked_survivors.iter().take(5).collect::<Vec<_>>(),
    );

    let tmp_orphan = fixture.retired.join(".index.bin.tmp-1649967");
    assert!(
        !tmp_orphan.exists(),
        "orphaned .tmp bookkeeping file must be purged eagerly (nlink==1, 69 h old — younger \
         than the pressure gate): {}; report={report:?}",
        tmp_orphan.display(),
    );

    let cold_linked_cache_survivors: Vec<&PathBuf> = fixture
        .cold_linked
        .iter()
        .filter(|path| path.exists())
        .collect();
    assert!(
        cold_linked_cache_survivors.is_empty(),
        "{} cold linked artifacts survived in the cache (expected the cache's own link removed \
         once older than the pressure gate): {:?}; report={report:?}",
        cold_linked_cache_survivors.len(),
        cold_linked_cache_survivors
            .iter()
            .take(5)
            .collect::<Vec<_>>(),
    );
    for (index, target) in fixture.cold_linked_targets.iter().enumerate() {
        assert!(
            target.exists(),
            "target-sim copy of cold_linked[{index}] must survive removal of the cache's own \
             link: {}; report={report:?}",
            target.display(),
        );
        let expected = format!("cold-linked-payload-{index}").into_bytes();
        let actual = std::fs::read(target).unwrap_or_else(|error| {
            panic!(
                "read surviving target-sim copy {}: {error}",
                target.display()
            )
        });
        assert_eq!(
            actual,
            expected,
            "target-sim copy of cold_linked[{index}] must keep its original content: {}",
            target.display(),
        );
    }

    let missing_warm: Vec<&PathBuf> = fixture
        .warm_linked
        .iter()
        .chain(fixture.warm_linked_targets.iter())
        .filter(|path| !path.exists())
        .collect();
    assert!(
        missing_warm.is_empty(),
        "warm linked artifacts must survive in both the cache and target-sim (younger than the \
         pressure gate): {missing_warm:?}; report={report:?}"
    );

    let missing_current: Vec<&PathBuf> = fixture
        .current_files
        .iter()
        .filter(|path| !path.exists())
        .collect();
    assert!(
        missing_current.is_empty(),
        "current-store files must never be touched: {missing_current:?}; report={report:?}"
    );

    assert_eq!(report.failed, 0, "report={report:?}");
    assert_eq!(report.live_retained, 0, "report={report:?}");
    assert!(
        report.bytes_reclaimed >= fixture.unlinked_artifact_bytes
            && report.bytes_reclaimed
                <= fixture.unlinked_artifact_bytes + fixture.bookkeeping_bytes,
        "expected bytes_reclaimed in [{}, {}] (unlinked artifact bytes, plus at most the \
         nlink==1 bookkeeping bytes; never the linked artifacts' {} bytes), got {}: report={report:?}",
        fixture.unlinked_artifact_bytes,
        fixture.unlinked_artifact_bytes + fixture.bookkeeping_bytes,
        fixture.linked_artifact_bytes,
        report.bytes_reclaimed,
    );
    assert!(
        fixture.retired.is_dir(),
        "the store directory must remain because warm linked artifacts survive: report={report:?}"
    );
}

/// B: directory mtimes must never protect a file the age gate would
/// otherwise expire. Exercised on linked files specifically, since an
/// unlinked file bypasses the age gate entirely (see test A / the eager
/// rule) and so can't tell "directory freshness ignored" apart from "eager
/// purge fired anyway".
#[test]
fn directory_freshness_does_not_pin_older_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join(".soldr"));
    let now = SystemTime::now();
    let retired = embedded_cache_root(&paths).join("v0.0.1");
    let shard = retired.join("artifacts").join("ab");
    std::fs::create_dir_all(&shard).expect("shard dir");
    let target_sim = paths
        .root
        .parent()
        .expect("soldr root always has a parent")
        .join("target-sim");
    std::fs::create_dir_all(&target_sim).expect("target-sim root");

    let old_linked = shard.join("old-linked.bin");
    let old_linked_target = target_sim.join("old-linked.bin");
    std::fs::write(&old_linked, b"old-linked-payload").expect("old linked artifact");
    std::fs::hard_link(&old_linked, &old_linked_target).expect("hard-link old-linked artifact");
    set_mtime(&old_linked, now - hours(10.0 * 24.0));

    let fresh_linked = shard.join("fresh-linked.bin");
    let fresh_linked_target = target_sim.join("fresh-linked.bin");
    std::fs::write(&fresh_linked, b"fresh-linked-payload").expect("fresh linked artifact");
    std::fs::hard_link(&fresh_linked, &fresh_linked_target)
        .expect("hard-link fresh-linked artifact");
    set_mtime(&fresh_linked, now - hours(1.0));

    for dir in [&shard, &retired.join("artifacts"), &retired] {
        set_mtime(dir, now);
    }

    let report = sweep_legacy_cache_roots(&paths, now, crate::cache_lib::gc_policy::STALENESS_GATE);

    assert!(
        !old_linked.exists(),
        "old-linked.bin's cache copy must expire despite fresh parent dirs: report={report:?}"
    );
    assert!(
        old_linked_target.exists(),
        "old-linked.bin's target-sim copy must survive (still linked elsewhere): report={report:?}"
    );
    assert!(
        fresh_linked.exists(),
        "fresh-linked.bin must survive in the cache: report={report:?}"
    );
    assert!(
        fresh_linked_target.exists(),
        "fresh-linked.bin must survive in target-sim: report={report:?}"
    );
}

/// C: the core pinning-loop regression: a writer that keeps touching the
/// store (a stray old binary, a pinned CI job, a mounted Docker harness) must
/// never be able to hold the whole tree young forever. Seeds are hard-linked
/// into target-sim (age-gated); each pass's `written-<d>.bin` is nlink == 1
/// (eagerly purged, however fresh). After each pass, for every artifact ever
/// created: `exists_in_cache` iff (linked AND mtime newer than
/// `now_d - 72 h`).
#[test]
fn a_writer_that_keeps_writing_cannot_pin_the_store() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join(".soldr"));
    let t0 = SystemTime::now();
    let retired = embedded_cache_root(&paths).join("v0.0.1");
    let artifacts = retired.join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts dir");
    let target_sim = paths
        .root
        .parent()
        .expect("soldr root always has a parent")
        .join("target-sim");
    std::fs::create_dir_all(&target_sim).expect("target-sim root");

    let mut planted: Vec<(PathBuf, SystemTime, bool)> = Vec::new();
    for k in 1..=20u64 {
        let file = artifacts.join(format!("seed-{k}.bin"));
        let target = target_sim.join(format!("seed-{k}.bin"));
        let mtime = t0 - (hours(k as f64 * 12.0) + Duration::from_secs(30 * 60));
        std::fs::write(&file, b"payload").expect("seed artifact");
        std::fs::hard_link(&file, &target).expect("hard-link seed artifact into target-sim");
        set_mtime(&file, mtime);
        planted.push((file, mtime, true));
    }

    for d in 0..6u64 {
        let now_d = t0 + Duration::from_secs(d * 24 * 60 * 60);

        // written-<d>.bin: nlink == 1 — the eager rule means it must never
        // survive a sweep, however fresh it is.
        // A writer recreates its layout: a pass that expired every artifact
        // legitimately removes the whole store (see the empty-store test).
        std::fs::create_dir_all(&artifacts).expect("writer recreates artifacts dir");
        let written = artifacts.join(format!("written-{d}.bin"));
        let written_mtime = now_d - Duration::from_secs(60);
        std::fs::write(&written, b"payload").expect("written artifact");
        set_mtime(&written, written_mtime);
        planted.push((written, written_mtime, false));

        for name in ["index.bin", ZCCACHE_WRITER_LOCK_FILE] {
            let file = retired.join(name);
            std::fs::write(&file, b"payload").expect("bookkeeping rewrite");
            set_mtime(&file, now_d - Duration::from_secs(60));
        }
        set_mtime(&retired, now_d - Duration::from_secs(60));
        set_mtime(&artifacts, now_d - Duration::from_secs(60));

        let report =
            sweep_legacy_cache_roots(&paths, now_d, crate::cache_lib::gc_policy::STALENESS_GATE);

        for (file, mtime, linked) in &planted {
            let expected_in_cache = *linked
                && now_d
                    .duration_since(*mtime)
                    .map(|age| age < crate::cache_lib::gc_policy::STALENESS_GATE)
                    .unwrap_or(true);
            assert_eq!(
                file.exists(),
                expected_in_cache,
                "pass d={d}: {} expected exists_in_cache={expected_in_cache} (linked={linked}, \
                 mtime={mtime:?}, now_d={now_d:?}); report={report:?}",
                file.display(),
            );
        }

        for k in 1..=20u64 {
            let target = target_sim.join(format!("seed-{k}.bin"));
            assert!(
                target.exists(),
                "pass d={d}: target-sim copy of seed-{k}.bin must always survive (it is never \
                 the last link): report={report:?}"
            );
        }
    }
}

/// D: a store whose remaining files are all nlink == 1 and old, with fresh
/// bookkeeping (also nlink == 1, so eagerly eligible too), must be removed
/// entirely.
#[test]
fn a_store_with_no_artifacts_left_is_removed_entirely() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join(".soldr"));
    let now = SystemTime::now();
    let retired = embedded_cache_root(&paths).join("v0.0.1");
    let artifacts = retired.join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts dir");

    for i in 0..50u32 {
        let file = artifacts.join(format!("old-{i:04}.bin"));
        std::fs::write(&file, b"payload").expect("old artifact");
        set_mtime(&file, now - hours(10.0 * 24.0));
    }

    for rel in [
        "index.bin",
        ZCCACHE_WRITER_LOCK_FILE,
        "depgraph/graph.bin",
        "logs/zccache.log",
    ] {
        let file = retired.join(rel);
        std::fs::create_dir_all(file.parent().expect("bookkeeping parent"))
            .expect("bookkeeping parent dir");
        std::fs::write(&file, b"payload").expect("bookkeeping file");
        set_mtime(&file, now - hours(2.0));
    }
    let staging = retired.join("staging");
    std::fs::create_dir_all(&staging).expect("staging dir");

    for dir in [
        &retired,
        &artifacts,
        &retired.join("depgraph"),
        &retired.join("logs"),
        &staging,
    ] {
        set_mtime(dir, now - hours(2.0));
    }

    let report = sweep_legacy_cache_roots(&paths, now, crate::cache_lib::gc_policy::STALENESS_GATE);

    assert!(
        !retired.exists(),
        "a store with only bookkeeping left must be removed entirely: report={report:?}"
    );
    assert_eq!(report.failed, 0, "report={report:?}");
}

/// D2: 200 nlink==1 artifacts, all 1 h old, plus fresh bookkeeping — every
/// one of them is eagerly eligible, so the whole store must be gone after a
/// single sweep pass, with no need to wait out any age gate.
#[test]
fn a_store_of_fresh_unlinked_files_is_removed_on_the_first_pass() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join(".soldr"));
    let now = SystemTime::now();
    let retired = embedded_cache_root(&paths).join("v0.0.1");
    let artifacts = retired.join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts dir");

    let mut fresh = Vec::new();
    for i in 0..200u32 {
        let file = artifacts.join(format!("fresh-{i:04}.bin"));
        std::fs::write(&file, b"payload").expect("fresh artifact");
        set_mtime(&file, now - hours(1.0));
        fresh.push(file);
    }

    for rel in [
        "index.bin",
        ZCCACHE_WRITER_LOCK_FILE,
        "depgraph/graph.bin",
        "logs/zccache.log",
    ] {
        let file = retired.join(rel);
        std::fs::create_dir_all(file.parent().expect("bookkeeping parent"))
            .expect("bookkeeping parent dir");
        std::fs::write(&file, b"payload").expect("bookkeeping file");
        set_mtime(&file, now - hours(1.0));
    }

    for dir in [
        &retired,
        &artifacts,
        &retired.join("depgraph"),
        &retired.join("logs"),
    ] {
        set_mtime(dir, now - hours(1.0));
    }

    let report = sweep_legacy_cache_roots(&paths, now, crate::cache_lib::gc_policy::STALENESS_GATE);

    let survivors: Vec<&PathBuf> = fresh.iter().filter(|path| path.exists()).collect();
    assert!(
        survivors.is_empty(),
        "{} of {} fresh (1 h old) unlinked artifacts survived the FIRST sweep pass, expected \
         eager purge regardless of age: {:?}; report={report:?}",
        survivors.len(),
        fresh.len(),
        survivors.iter().take(5).collect::<Vec<_>>(),
    );
    assert!(
        !retired.exists(),
        "a store of only fresh unlinked artifacts plus fresh bookkeeping must be removed \
         entirely on the first pass: report={report:?}"
    );
    assert_eq!(report.failed, 0, "report={report:?}");
}

/// E: a held `.writer.lock` defers expiry for everything, unlinked or linked,
/// however old. Once released, the eager and age-gated rules both apply
/// immediately.
#[test]
fn a_held_writer_lock_defers_expiry_until_the_service_exits() {
    use fs2::FileExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join(".soldr"));
    let now = SystemTime::now();
    let retired = embedded_cache_root(&paths).join("v0.0.1");
    let artifacts = retired.join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts dir");
    let target_sim = paths
        .root
        .parent()
        .expect("soldr root always has a parent")
        .join("target-sim");
    std::fs::create_dir_all(&target_sim).expect("target-sim root");

    let mut cold_unlinked = Vec::new();
    for i in 0..10u32 {
        let file = artifacts.join(format!("cold-unlinked-{i:02}.bin"));
        std::fs::write(&file, b"payload").expect("cold unlinked artifact");
        set_mtime(&file, now - hours(10.0 * 24.0));
        cold_unlinked.push(file);
    }
    let mut fresh_unlinked = Vec::new();
    for i in 0..5u32 {
        let file = artifacts.join(format!("fresh-unlinked-{i:02}.bin"));
        std::fs::write(&file, b"payload").expect("fresh unlinked artifact");
        set_mtime(&file, now - hours(1.0));
        fresh_unlinked.push(file);
    }

    let cold_linked = artifacts.join("cold-linked.bin");
    let cold_linked_target = target_sim.join("cold-linked.bin");
    std::fs::write(&cold_linked, b"cold-linked-payload").expect("cold linked artifact");
    std::fs::hard_link(&cold_linked, &cold_linked_target).expect("hard-link cold-linked");
    set_mtime(&cold_linked, now - hours(10.0 * 24.0));

    let warm_linked = artifacts.join("warm-linked.bin");
    let warm_linked_target = target_sim.join("warm-linked.bin");
    std::fs::write(&warm_linked, b"warm-linked-payload").expect("warm linked artifact");
    std::fs::hard_link(&warm_linked, &warm_linked_target).expect("hard-link warm-linked");
    set_mtime(&warm_linked, now - hours(1.0));

    let lock_path = retired.join(ZCCACHE_WRITER_LOCK_FILE);
    let lock = std::fs::File::create(&lock_path).expect("writer lock file");
    lock.try_lock_exclusive().expect("acquire writer lock");

    let held = sweep_legacy_cache_roots(&paths, now, crate::cache_lib::gc_policy::STALENESS_GATE);
    assert_eq!(held.live_retained, 1, "{held:?}");
    assert_eq!(held.failed, 0, "{held:?}");
    for file in cold_unlinked.iter().chain(fresh_unlinked.iter()) {
        assert!(
            file.exists(),
            "unlinked artifact must be untouched while the writer lock is held: {}; held={held:?}",
            file.display()
        );
    }
    assert!(
        cold_linked.exists(),
        "cold_linked must be untouched while the writer lock is held: held={held:?}"
    );
    assert!(
        warm_linked.exists(),
        "warm_linked must be untouched while the writer lock is held: held={held:?}"
    );

    drop(lock);
    let released =
        sweep_legacy_cache_roots(&paths, now, crate::cache_lib::gc_policy::STALENESS_GATE);
    assert_eq!(released.live_retained, 0, "{released:?}");
    for file in cold_unlinked.iter().chain(fresh_unlinked.iter()) {
        assert!(
            !file.exists(),
            "unlinked artifact must be purged eagerly once the lock is released: {}; \
             released={released:?}",
            file.display()
        );
    }
    assert!(
        !cold_linked.exists(),
        "cold_linked's cache copy must be removed once the lock is released (older than the \
         pressure gate): released={released:?}"
    );
    assert!(
        cold_linked_target.exists(),
        "cold_linked's target-sim copy must survive the release: released={released:?}"
    );
    assert!(
        warm_linked.exists(),
        "warm_linked must survive after lock release (younger than the pressure gate): \
         released={released:?}"
    );
    assert!(
        warm_linked_target.exists(),
        "warm_linked's target-sim copy must survive after lock release: released={released:?}"
    );
}

/// F: guard — the current version's store must never be touched, however old
/// its files are. Expected GREEN today and after the fix.
#[test]
fn the_current_store_is_never_touched_however_old() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join(".soldr"));
    let now = SystemTime::now();
    let fixture = host_shaped_stores(&paths, now);

    let _report = sweep_legacy_cache_roots(&paths, now, Duration::ZERO);

    for file in &fixture.current_files {
        assert!(
            file.exists(),
            "current-store file must never be swept: {}",
            file.display()
        );
    }
}

/// H: `bytes_reclaimed` counts bytes actually freed — only files whose last
/// link was removed. Ten nlink==1 files and ten nlink==2 files, all the same
/// size, no bookkeeping: only the ten nlink==1 files' bytes count.
#[test]
fn bytes_reclaimed_counts_only_last_links() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join(".soldr"));
    let now = SystemTime::now();
    let retired = embedded_cache_root(&paths).join("v0.0.1");
    let artifacts = retired.join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts dir");
    let target_sim = paths
        .root
        .parent()
        .expect("soldr root always has a parent")
        .join("target-sim");
    std::fs::create_dir_all(&target_sim).expect("target-sim root");

    let payload = vec![0u8; 4096];

    let mut unlinked = Vec::new();
    for i in 0..10u32 {
        let file = artifacts.join(format!("unlinked-{i:02}.bin"));
        std::fs::write(&file, &payload).expect("unlinked artifact");
        set_mtime(&file, now - hours(10.0 * 24.0));
        unlinked.push(file);
    }

    let mut linked = Vec::new();
    let mut linked_targets = Vec::new();
    for i in 0..10u32 {
        let file = artifacts.join(format!("linked-{i:02}.bin"));
        let target = target_sim.join(format!("linked-{i:02}.bin"));
        std::fs::write(&file, &payload).expect("linked artifact");
        std::fs::hard_link(&file, &target).expect("hard-link linked artifact");
        set_mtime(&file, now - hours(10.0 * 24.0));
        linked.push(file);
        linked_targets.push(target);
    }

    set_mtime(&artifacts, now - hours(10.0 * 24.0));
    set_mtime(&retired, now - hours(10.0 * 24.0));

    let report = sweep_legacy_cache_roots(&paths, now, crate::cache_lib::gc_policy::STALENESS_GATE);

    assert_eq!(
        report.bytes_reclaimed, 40_960,
        "expected exactly the 10 last-link (nlink==1) files' bytes (10 * 4096), none of the 10 \
         still-linked files' bytes: report={report:?}"
    );
    for file in &unlinked {
        assert!(
            !file.exists(),
            "unlinked artifact must be purged: {}; report={report:?}",
            file.display()
        );
    }
    for file in &linked {
        assert!(
            !file.exists(),
            "linked artifact's cache copy must be removed once older than the pressure gate: \
             {}; report={report:?}",
            file.display()
        );
    }
    for target in &linked_targets {
        assert!(
            target.exists(),
            "linked artifact's target-sim copy must survive: {}; report={report:?}",
            target.display()
        );
        let bytes = std::fs::metadata(target)
            .unwrap_or_else(|error| {
                panic!(
                    "stat surviving target-sim copy {}: {error}",
                    target.display()
                )
            })
            .len();
        assert_eq!(
            bytes,
            4096,
            "target-sim copy must keep its original size: {}",
            target.display()
        );
    }
    assert!(
        !retired.exists(),
        "store dir must be removed once no artifact remains in the cache: report={report:?}"
    );
    assert_eq!(report.failed, 0, "report={report:?}");
}

/// soldr#3882: a sibling route's store restored from CI's Tier-2 archive is
/// all single-link files. A pressure sweep by another daemon (the bootstrap
/// soldr) must not purge it while its `.last-active` marker is recent: that
/// store is the previous run's, waiting for its own lineage to adopt it.
#[test]
fn a_recently_active_sibling_route_store_survives_a_pressure_sweep() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join(".soldr"));
    let now = SystemTime::now();
    let version = zccache::core::config::versioned_subdir();
    let store = paths
        .cache
        .join("zccache")
        .join("daemon-state")
        .join("embedded-v1-soldr-daemon-previous-run")
        .join(&version);
    let artifacts = store.join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts dir");
    let restored = artifacts.join("restored.bin");
    std::fs::write(&restored, b"payload").expect("restored artifact");
    set_mtime(&restored, now - hours(5.0 * 24.0));
    let index = store.join("index.bin");
    std::fs::write(&index, b"index").expect("index");
    set_mtime(&index, now - hours(5.0 * 24.0));
    let marker = store.join(zccache::core::config::LAST_ACTIVE_MARKER_FILE);
    std::fs::write(&marker, b"").expect("last-active marker");
    set_mtime(&marker, now - hours(2.0));

    let report = sweep_legacy_cache_roots(&paths, now, crate::cache_lib::gc_policy::STALENESS_GATE);
    assert!(restored.exists(), "{report:?}");
    assert!(index.exists(), "{report:?}");
}

/// The other half of the same contract: a sibling route nobody has used
/// within the gate is still reclaimed, so old generations do not pile up.
#[test]
fn a_stale_sibling_route_store_is_still_reclaimed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join(".soldr"));
    let now = SystemTime::now();
    let version = zccache::core::config::versioned_subdir();
    let store = paths
        .cache
        .join("zccache")
        .join("daemon-state")
        .join("embedded-v1-soldr-daemon-long-gone")
        .join(&version);
    let artifacts = store.join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts dir");
    let stale = artifacts.join("stale.bin");
    std::fs::write(&stale, b"payload").expect("stale artifact");
    set_mtime(&stale, now - hours(10.0 * 24.0));
    let marker = store.join(zccache::core::config::LAST_ACTIVE_MARKER_FILE);
    std::fs::write(&marker, b"").expect("last-active marker");
    set_mtime(&marker, now - hours(10.0 * 24.0));

    let report = sweep_legacy_cache_roots(&paths, now, crate::cache_lib::gc_policy::STALENESS_GATE);
    assert!(!stale.exists(), "{report:?}");
}
