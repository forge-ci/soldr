#![expect(
    clippy::panic_in_result_fn,
    reason = "Native fixture assertions intentionally fail the test harness"
)]
use crate::fs_scan::test_support::{paths, scratch};
use crate::fs_scan::{scan_stream, ScanOptions};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
fn fixture() -> io::Result<PathBuf> {
    let root = scratch("accounting")?;
    fs::create_dir(root.join("wide"))?;
    fs::create_dir(root.join("nested"))?;
    fs::write(root.join("nested/data"), vec![7u8; 132_579])?;
    fs::hard_link(root.join("nested/data"), root.join("hardlink"))?;
    let mut sparse = File::create(root.join("sparse"))?;
    sparse.set_len(32 * 1024 * 1024)?;
    sparse.write_all(b"sparse")?;
    // Windows may allocate the sparse compression unit during delayed writeback.
    // Equivalence compares separate live scans, so finish fixture I/O first.
    sparse.sync_all()?;
    drop(sparse);
    fs::write(root.join("unreadable"), b"secret")?;
    for i in 0..3000 {
        fs::write(root.join("wide").join(i.to_string()), [i as u8; 13])?;
    }
    platform_fixture(&root)?;
    Ok(root)
}

fn platform_fixture(root: &Path) -> io::Result<()> {
    use std::os::unix::fs::{symlink, PermissionsExt};
    symlink("..", root.join("nested/loop"))?;
    symlink("missing", root.join("dangling"))?;
    fs::set_permissions(root.join("unreadable"), fs::Permissions::from_mode(0o0))?;
    // Running as root can bypass file modes; native CI uses an ordinary account.
    if std::env::var("USER").as_deref() != Ok("root") {
        assert!(File::open(root.join("unreadable")).is_err());
    }
    Ok(())
}

fn ground_truth(root: &Path) -> io::Result<(u64, u64, u64)> {
    use std::collections::HashSet;
    use std::os::unix::fs::MetadataExt;
    let mut seen = HashSet::new();
    let mut todo = vec![root.to_path_buf()];
    let mut allocated = 0;
    let mut apparent = 0;
    let mut entries = 0;
    while let Some(path) = todo.pop() {
        let m = fs::symlink_metadata(&path)?;
        entries += 1;
        if seen.insert((m.dev(), m.ino())) {
            allocated += m.blocks() * 512;
            apparent += m.len();
        }
        if m.is_dir() {
            for child in fs::read_dir(path)? {
                todo.push(child?.path());
            }
        }
    }
    Ok((allocated, apparent, entries))
}

#[test]
fn real_tree_parallel_accounting() -> Result<(), Box<dyn std::error::Error>> {
    let root = fixture()?;
    let expected = ground_truth(&root)?;
    let sparse = fs::metadata(root.join("sparse"))?;
    use std::os::unix::fs::MetadataExt;
    // Some guest filesystems fully allocate the sparse-write fixture. Always
    // check actual accounting; require a hole only when metadata proves one.
    let supports_sparse = sparse.blocks() * 512 < sparse.len();
    for threads in [1, 2, 8] {
        let mut saw_sparse = false;
        let summary = scan_stream(
            &root,
            &ScanOptions {
                threads: Some(threads),
                ..Default::default()
            },
            |entry| {
                if entry.relative_name == "sparse" {
                    saw_sparse = true;
                    assert_eq!(entry.allocated, sparse.blocks() * 512);
                    assert_eq!(entry.apparent, sparse.len());
                    if supports_sparse {
                        assert!(entry.allocated < entry.apparent);
                    }
                }
                true
            },
        )?;
        assert!(saw_sparse);
        assert_eq!(
            (summary.allocated, summary.apparent, summary.entries),
            expected
        );
        assert_eq!(summary.errors, 0);
    }
    Ok(())
}

#[test]
fn followed_loop_terminates_and_permission_denials_are_visible(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let root = scratch("links")?;
    fs::create_dir(root.join("child"))?;
    fs::write(root.join("child/file"), b"hello")?;
    symlink("..", root.join("child/loop"))?;
    let summary = scan_stream(
        &root,
        &ScanOptions {
            follow_symlinks: true,
            ..ScanOptions::default()
        },
        |_| true,
    )?;
    assert_eq!(summary.entries, 3);
    assert_eq!(summary.errors, 0);
    fs::create_dir(root.join("denied"))?;
    fs::write(root.join("denied/file"), b"hidden")?;
    fs::set_permissions(root.join("denied"), fs::Permissions::from_mode(0o0))?;
    let summary = scan_stream(&root, &ScanOptions::default(), |_| true)?;
    if std::env::var("USER").as_deref() != Ok("root") {
        assert_eq!(summary.errors, 1);
    }
    fs::set_permissions(root.join("denied"), fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[test]
fn arbitrary_filename_bytes_survive() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::ffi::OsStrExt;
    let root = scratch("bytes")?;
    let filesystem = native_macos::filesystem(&root)?;
    let invalid = root.join(std::ffi::OsStr::from_bytes(b"bad-\xff\n\""));
    // Probe the actual mount: guest filesystems may reject invalid UTF-8 even
    // when their reported filesystem type is neither APFS nor HFS+.
    let supports_arbitrary_bytes = match fs::write(&invalid, b"bytes") {
        Ok(()) => true,
        Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => false,
        Err(error) => return Err(error.into()),
    };
    for name in ["composed-\u{e9}\n\"", "decomposed-e\u{301}\n\""] {
        fs::write(root.join(name), b"bytes")?;
    }
    let mut expected = vec![
        root.join("composed-\u{e9}\n\""),
        root.join("decomposed-e\u{301}\n\""),
    ];
    if filesystem == b"hfs" {
        // HFS+ stores its decomposed form; APFS must preserve the supplied bytes.
        expected[0] = root.join("composed-e\u{301}\n\"");
    }
    if supports_arbitrary_bytes {
        expected.push(invalid);
    }
    expected.sort();
    let mut actual = paths(&root)?;
    actual.retain(|p| p != &root);
    actual.sort();
    assert_eq!(actual, expected);
    Ok(())
}

#[test]
fn followed_file_aliases_charge_an_inode_once() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::{symlink, MetadataExt};
    let root = scratch("file-aliases")?;
    fs::write(root.join("file"), b"one physical file")?;
    symlink("file", root.join("alias-one"))?;
    symlink("file", root.join("alias-two"))?;
    let file = fs::metadata(root.join("file"))?;
    assert_eq!(file.nlink(), 1);
    for threads in [1, 8] {
        let summary = scan_stream(
            &root,
            &ScanOptions {
                follow_symlinks: true,
                threads: Some(threads),
                ..ScanOptions::default()
            },
            |_| true,
        )?;
        let directory = fs::metadata(&root)?;
        assert_eq!(summary.errors, 0);
        assert_eq!(summary.entries, 4);
        assert_eq!(
            summary.allocated,
            (directory.blocks() + file.blocks()) * 512
        );
        assert_eq!(summary.apparent, directory.len() + file.len());
    }
    Ok(())
}

#[path = "fs_scan_test_filesystem.rs"]
mod native_macos;
