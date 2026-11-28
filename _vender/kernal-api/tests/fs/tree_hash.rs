#![cfg(feature = "fs")]

use kernal_api::hash::{blake3_tree, TreeHashOptions};
use std::fs;
use std::path::Path;

fn hash(root: &Path, include: &[&str], exclude: &[&str]) -> String {
    blake3_tree(root, include, exclude, TreeHashOptions::default())
        .unwrap()
        .to_hex()
}

#[test]
fn same_size_edit_with_restored_mtime_changes_digest() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("source.cpp");
    fs::write(&file, "aaaa").unwrap();
    let mtime = file.metadata().unwrap().modified().unwrap();
    let first = hash(dir.path(), &[], &[]);
    fs::write(&file, "bbbb").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(file)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    assert_ne!(first, hash(dir.path(), &[], &[]));
}

#[test]
fn creation_deletion_and_rename_change_digest() {
    let dir = tempfile::tempdir().unwrap();
    let empty = hash(dir.path(), &[], &[]);
    fs::write(dir.path().join("a"), "content").unwrap();
    let first = hash(dir.path(), &[], &[]);
    assert_ne!(empty, first);
    fs::rename(dir.path().join("a"), dir.path().join("b")).unwrap();
    assert_ne!(first, hash(dir.path(), &[], &[]));
    fs::remove_file(dir.path().join("b")).unwrap();
    assert_eq!(empty, hash(dir.path(), &[], &[]));
}

#[test]
fn selection_exclusion_and_relocation_are_deterministic() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    for dir in [&first, &second] {
        fs::create_dir_all(dir.path().join("src/ignored")).unwrap();
        fs::write(dir.path().join("src/a.cpp"), "a").unwrap();
        fs::write(dir.path().join("src/z.cpp"), "z").unwrap();
    }
    fs::write(first.path().join("src/ignored/generated.cpp"), "noise").unwrap();
    fs::write(first.path().join("README"), "noise").unwrap();
    for exclude in ["src/ignored", "src/ignored/**"] {
        assert_eq!(
            hash(first.path(), &["src/**/*.cpp"], &[exclude]),
            hash(second.path(), &["src/**/*.cpp"], &[exclude])
        );
    }
    assert_eq!(
        hash(first.path(), &["src/a.cpp", "src/a.cpp"], &[]),
        hash(second.path(), &["src/a.cpp"], &[])
    );
}

#[test]
fn file_boundaries_are_unambiguous() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a"), b"bc\0d").unwrap();
    fs::write(dir.path().join("e"), b"f").unwrap();
    let first = hash(dir.path(), &[], &[]);
    fs::write(dir.path().join("a"), b"b").unwrap();
    fs::remove_file(dir.path().join("e")).unwrap();
    fs::write(dir.path().join("c"), b"de\0f").unwrap();
    assert_ne!(first, hash(dir.path(), &[], &[]));
}

#[test]
fn invalid_inputs_and_resource_limits_fail_loudly() {
    let dir = tempfile::tempdir().unwrap();
    assert!(blake3_tree(
        dir.path().join("missing"),
        &[],
        &[],
        TreeHashOptions::default()
    )
    .is_err());
    assert!(blake3_tree(dir.path(), &["["], &[], TreeHashOptions::default()).is_err());
    fs::write(dir.path().join("a"), "abcd").unwrap();
    assert!(blake3_tree(
        dir.path(),
        &[],
        &[],
        TreeHashOptions {
            maximum_file_bytes: 3,
            ..Default::default()
        }
    )
    .is_err());
    fs::write(dir.path().join("b"), "b").unwrap();
    assert!(blake3_tree(
        dir.path(),
        &[],
        &[],
        TreeHashOptions {
            maximum_files: 1,
            ..Default::default()
        }
    )
    .is_err());
}

#[test]
fn worker_count_and_creation_order_do_not_change_digest() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    for index in 0..32 {
        fs::write(first.path().join(format!("{index}.txt")), index.to_string()).unwrap();
    }
    for index in (0..32).rev() {
        fs::write(
            second.path().join(format!("{index}.txt")),
            index.to_string(),
        )
        .unwrap();
    }
    let sequential = blake3_tree(
        first.path(),
        &[],
        &[],
        TreeHashOptions {
            workers: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let parallel = blake3_tree(second.path(), &[], &[], TreeHashOptions::default()).unwrap();
    assert_eq!(sequential, parallel);
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_followed_or_hashed() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), "outside").unwrap();
    let empty = hash(dir.path(), &[], &[]);
    std::os::unix::fs::symlink(outside.path(), dir.path().join("directory-link")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("file-link"))
        .unwrap();
    assert_eq!(empty, hash(dir.path(), &[], &[]));
}

#[cfg(unix)]
#[test]
fn invalid_utf8_file_names_fail_instead_of_colliding() {
    use std::os::unix::ffi::OsStringExt;
    let dir = tempfile::tempdir().unwrap();
    let name = std::ffi::OsString::from_vec(vec![b'a', 0xff]);
    match fs::write(dir.path().join(name), "content") {
        Ok(()) => {
            // Linux and other byte-oriented filesystems accept the name, so the
            // hash is what must refuse to collide two distinct entries.
            let error = blake3_tree(dir.path(), &[], &[], TreeHashOptions::default()).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        }
        Err(error) => {
            // APFS enforces UTF-8 file names and rejects this one with EILSEQ,
            // so the precondition of this test -- a directory holding an
            // invalid-UTF-8 name -- cannot be constructed at all. The invariant
            // still holds, enforced a step earlier by the filesystem rather
            // than by the hash.
            //
            // Assert on the raw OS error rather than `ErrorKind`: Rust maps
            // EILSEQ to `Uncategorized`, which would also match a great many
            // unrelated failures.
            assert!(
                error.raw_os_error().is_some(),
                "expected the filesystem to refuse the name, got {error}"
            );
            // The rejection is about the NAME, not the directory. Without this
            // check the branch would pass on any unrelated failure, including
            // an unusable temporary directory.
            fs::write(dir.path().join("valid"), "content").unwrap();
        }
    }
}
