#![cfg(feature = "fs-scan")]

use std::collections::HashMap;
use std::path::Path;

use kernal_api::fs_scan::{self, EntryKind, ScanOptions};

#[test]
fn stream_preserves_names_parents_and_deduplicates_hardlinks(
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path();
    std::fs::create_dir(root.join("sub"))?;
    std::fs::write(root.join("sub/name.txt"), vec![42; 8192])?;
    std::fs::hard_link(root.join("sub/name.txt"), root.join("alias"))?;
    let mut serial = HashMap::new();
    let summary = fs_scan::scan_stream(
        root,
        &ScanOptions {
            threads: Some(1),
            ..Default::default()
        },
        |entry| {
            serial.insert(
                entry.relative_name.to_owned(),
                (
                    entry.identity,
                    entry.parent,
                    entry.kind,
                    entry.allocated,
                    entry.apparent,
                ),
            );
            true
        },
    )?;
    assert_eq!(summary.entries, 4);
    assert_eq!(summary.errors, 0);
    assert!(!summary.cancelled);
    let root_entry = serial
        .get(Path::new("").as_os_str())
        .ok_or("missing root")?;
    let sub = serial
        .get(Path::new("sub").as_os_str())
        .ok_or("missing sub")?;
    let file = serial
        .get(Path::new("sub").join("name.txt").as_os_str())
        .ok_or("missing file")?;
    let alias = serial
        .get(Path::new("alias").as_os_str())
        .ok_or("missing alias")?;
    assert_eq!(root_entry.2, EntryKind::Directory);
    assert_eq!(sub.1, root_entry.0.entry_id());
    assert_eq!(file.1, sub.0.entry_id());
    assert_eq!(file.0, alias.0);
    assert_eq!(file.4 + alias.4, 8192);
    assert_eq!(summary.allocated, serial.values().map(|e| e.3).sum::<u64>());
    assert_eq!(summary.apparent, serial.values().map(|e| e.4).sum::<u64>());
    let parallel = fs_scan::scan_stream(
        root,
        &ScanOptions {
            threads: Some(8),
            ..Default::default()
        },
        |_| true,
    )?;
    assert_eq!(
        (
            summary.allocated,
            summary.apparent,
            summary.entries,
            summary.errors
        ),
        (
            parallel.allocated,
            parallel.apparent,
            parallel.entries,
            parallel.errors
        )
    );
    Ok(())
}

#[test]
fn cancellation_root_errors_and_thread_validation() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    std::fs::write(fixture.path().join("file"), b"hello")?;
    let mut calls = 0;
    let summary = fs_scan::scan_stream(fixture.path(), &ScanOptions::default(), |_| {
        calls += 1;
        false
    })?;
    assert!(summary.cancelled);
    assert_eq!(summary.entries, 1);
    assert_eq!(calls, 1);
    assert!(fs_scan::scan_stream(
        &fixture.path().join("absent"),
        &ScanOptions::default(),
        |_| true
    )
    .is_err());
    for threads in [0, 257] {
        assert!(fs_scan::scan_stream(
            fixture.path(),
            &ScanOptions {
                threads: Some(threads),
                ..Default::default()
            },
            |_| true
        )
        .is_err());
    }
    Ok(())
}
