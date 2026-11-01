use super::*;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn mkdir_with_file(path: &Path) {
    std::fs::create_dir_all(path.join("shims")).unwrap();
    std::fs::write(path.join("shims").join("rustc"), b"x").unwrap();
}

#[test]
fn parses_version_dir_names() {
    assert_eq!(
        parse_version_dir("v0.9.46"),
        Some(semver::Version::new(0, 9, 46))
    );
    let dev = format!("v0.9.46-{}", "a".repeat(64));
    assert_eq!(
        parse_version_dir(&dev),
        Some(semver::Version::new(0, 9, 46))
    );
    assert_eq!(parse_version_dir("v0.9.46-beta"), None);
    assert_eq!(parse_version_dir("verdicts"), None);
    assert_eq!(parse_version_dir("bin"), None);
}

#[test]
fn derives_tool_families() {
    assert_eq!(tool_family("llvm-21.1.5").as_deref(), Some("llvm"));
    assert_eq!(tool_family("llvm-21.1.5-r2").as_deref(), Some("llvm"));
    assert_eq!(
        tool_family("cargo-nextest-0.9.140").as_deref(),
        Some("cargo-nextest")
    );
    assert_eq!(tool_family("clang-shim"), None);
    assert_eq!(tool_family("rustup"), None);
    assert_eq!(tool_family("dylint-link-6.2.0"), None);
    assert_eq!(tool_family("cargo-dylint-6.0.3"), None);
}

#[test]
fn keeps_current_and_previous_and_reclaims_older_when_stale() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    for v in ["v0.9.40", "v0.9.46", "v0.9.47", "v0.9.48"] {
        mkdir_with_file(&tmp.path().join(v));
    }
    let dev = format!("v0.9.48-{}", "b".repeat(64));
    mkdir_with_file(&tmp.path().join(&dev));
    std::fs::create_dir_all(tmp.path().join("verdicts")).unwrap();

    // Fresh: nothing is old enough to reclaim.
    let report = sweep_stale_versions(&paths, "0.9.48", SystemTime::now(), 14 * DAY);
    assert!(report.version_dirs_removed.is_empty());

    // A year on, only the non-current, non-previous dirs go.
    let later = SystemTime::now() + 365 * DAY;
    let mut report = sweep_stale_versions(&paths, "0.9.48", later, 14 * DAY);
    report.version_dirs_removed.sort();
    assert_eq!(report.version_dirs_removed, vec!["v0.9.40", "v0.9.46"]);
    assert!(tmp.path().join("v0.9.47").is_dir());
    assert!(tmp.path().join("v0.9.48").is_dir());
    assert!(tmp.path().join(&dev).is_dir());
    assert!(tmp.path().join("verdicts").is_dir());
}

#[test]
fn reclaims_superseded_tool_revisions() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let bin = tmp.path().join("bin");
    let old = bin.join("llvm-21.1.4");
    let mid = bin.join("llvm-21.1.5");
    let new = bin.join("llvm-21.1.5-r2");
    for (i, dir) in [&old, &mid, &new].into_iter().enumerate() {
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let f = dir.join("bin").join("clang");
        std::fs::write(&f, b"x").unwrap();
        let t = filetime::FileTime::from_unix_time(1_000_000 + i as i64 * 1000, 0);
        for p in [f.clone(), dir.join("bin"), dir.to_path_buf()] {
            filetime::set_file_times(&p, t, t).unwrap();
        }
    }
    std::fs::write(bin.join("llvm-21.1.4.lock"), b"").unwrap();
    std::fs::create_dir_all(bin.join("dylint-link-6.0.3")).unwrap();
    std::fs::create_dir_all(bin.join("dylint-link-6.2.0")).unwrap();
    std::fs::create_dir_all(bin.join("dylint-link-6.3.0")).unwrap();

    let later = SystemTime::now() + 365 * DAY;
    let report = sweep_stale_versions(&paths, "0.9.48", later, 14 * DAY);
    // 21.1.5 is superseded by 21.1.5-r2; 21.1.4 survives as the second
    // live release in the family.
    assert_eq!(report.tool_revisions_removed, vec!["llvm-21.1.5"]);
    assert!(!mid.exists());
    assert!(old.is_dir() && new.is_dir());
    assert!(bin.join("llvm-21.1.4.lock").exists());
    assert!(bin.join("dylint-link-6.0.3").is_dir());
}

#[test]
fn splits_packaging_revisions() {
    assert_eq!(split_revision("llvm-21.1.5-r2"), ("llvm-21.1.5", 2));
    assert_eq!(split_revision("llvm-21.1.5"), ("llvm-21.1.5", 0));
}
