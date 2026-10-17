//! Source timestamp fidelity is independent of cache artifact timestamps (#3851).
use super::*;

#[test]
fn full_and_delta_preserve_source_submillisecond_mtime() {
    let root = tempfile::tempdir().unwrap();
    let ws = root.path().join("ws");
    let cache = root.path().join("cache");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    let source = ws.join("source.rs");
    std::fs::write(&source, b"unchanged source\n").unwrap();
    let requested = filetime::FileTime::from_unix_time(1_792_262_706, 251_336_800);
    filetime::set_file_mtime(&source, requested).unwrap();
    let original =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&source).unwrap());
    if original.nanoseconds().is_multiple_of(1_000_000) {
        eprintln!("filesystem cannot represent submillisecond source mtimes: {original:?}");
        return;
    }
    let base_archive = root.path().join("base.tar.zst");
    save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: Some(&cache),
        out: &base_archive,
        zstd_level: 1,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .unwrap();
    let base = read_manifest_from_archive(&base_archive).unwrap();
    let delta_archive = root.path().join("delta.tar.zst");
    save_delta(&SaveDeltaOptions {
        workspace: Some(&ws),
        cache_dir: &cache,
        base_manifest: &base,
        out: &delta_archive,
        zstd_level: 1,
        threads: None,
        profile: SaveProfile::Full,
    })
    .unwrap();
    for archive in [&base_archive, &delta_archive] {
        filetime::set_file_mtime(
            &source,
            filetime::FileTime::from_unix_time(1_800_000_000, 0),
        )
        .unwrap();
        let report = load(&LoadOptions {
            archive,
            cache_dir: Some(&cache),
            workspace: Some(&ws),
            threads: None,
            mtimes_only: false,
            profile_extract: false,
            auto_defender_exclude: false,
        })
        .unwrap();
        assert_eq!(report.mtimes_applied, 1);
        let restored =
            filetime::FileTime::from_last_modification_time(&std::fs::metadata(&source).unwrap());
        assert_eq!(
            restored,
            original,
            "source mtime after {}",
            archive.display()
        );
    }
}
