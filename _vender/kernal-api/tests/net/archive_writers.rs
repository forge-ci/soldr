#![cfg(feature = "archive")]
//! Writers, in-memory codecs, extra tar codecs and opt-in extraction policy
//! (kernal-api#562), exercised against real files.

use kernal_api::archive::{
    extract, extract_reader, zstd_decode, zstd_encode, ArchiveFormat, DanglingLinks,
    ExistingDestination, ExtractionLimits, TarCompression, TarEntryKind, TarReader, TarWriter,
    ZipCompression, ZipWriter,
};
use std::fs;
use std::io::{Cursor, Read};
use std::path::Path;

// Populate the same tree in the given file order, so directory iteration order
// and creation times differ between calls.
fn populate(root: &Path, reverse: bool) {
    let mut files: Vec<(&str, &[u8])> = vec![
        ("a/b.txt", b"hello"),
        ("a/nested/deep.bin", &[0, 1, 2, 3, 255]),
        ("c.txt", b"top level"),
        ("z/last.txt", b"zzz"),
    ];
    if reverse {
        files.reverse();
    }
    for (path, data) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }
    fs::create_dir_all(root.join("empty")).unwrap();
}

fn assert_tree(root: &Path) {
    assert_eq!(fs::read(root.join("a/b.txt")).unwrap(), b"hello");
    assert_eq!(
        fs::read(root.join("a/nested/deep.bin")).unwrap(),
        [0, 1, 2, 3, 255]
    );
    assert_eq!(fs::read(root.join("c.txt")).unwrap(), b"top level");
    assert_eq!(fs::read(root.join("z/last.txt")).unwrap(), b"zzz");
    assert!(root.join("empty").is_dir());
}

fn write_tar_zstd(source: &Path, output: &Path) {
    let mut writer = TarWriter::zstd(fs::File::create(output).unwrap(), 3).unwrap();
    writer.append_dir_all("", source).unwrap();
    writer
        .append_bytes("generated/info.txt", b"generated", 0o755)
        .unwrap();
    writer.finish().unwrap();
}

fn write_zip(source: &Path, output: &Path) {
    let mut writer = ZipWriter::new(fs::File::create(output).unwrap(), ZipCompression::Deflated);
    writer.append_dir_all("pkg", source).unwrap();
    writer
        .append_bytes("generated/info.txt", b"generated", 0o644)
        .unwrap();
    writer.finish().unwrap();
}

#[test]
fn tar_zstd_writer_is_deterministic_and_round_trips() {
    let temp = tempfile::tempdir().unwrap();
    let (first, second) = (temp.path().join("one"), temp.path().join("two"));
    populate(&first, false);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    populate(&second, true);
    write_tar_zstd(&first, &temp.path().join("one.tar.zst"));
    write_tar_zstd(&second, &temp.path().join("two.tar.zst"));
    let bytes = fs::read(temp.path().join("one.tar.zst")).unwrap();
    assert_eq!(bytes, fs::read(temp.path().join("two.tar.zst")).unwrap());

    let out = temp.path().join("out");
    extract(
        &temp.path().join("one.tar.zst"),
        &out,
        ArchiveFormat::TarZstd,
        ExtractionLimits::default(),
    )
    .unwrap();
    assert_tree(&out);
    assert_eq!(
        fs::read(out.join("generated/info.txt")).unwrap(),
        b"generated"
    );

    // Entries are sorted, and headers carry no timestamps or owners.
    let mut reader = TarReader::from_slice(&bytes, TarCompression::Zstd).unwrap();
    let mut names = Vec::new();
    for entry in reader.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().replace('\\', "/");
        let mut payload = Vec::new();
        entry.read_to_end(&mut payload).unwrap();
        assert_eq!(payload.len() as u64, entry.size());
        if name == "generated/info.txt" {
            assert_eq!(entry.kind(), TarEntryKind::File);
            assert_eq!(entry.mode().unwrap() & 0o777, 0o755);
        }
        names.push(name.trim_end_matches('/').to_owned());
    }
    assert_eq!(
        names,
        [
            "a",
            "a/b.txt",
            "a/nested",
            "a/nested/deep.bin",
            "c.txt",
            "empty",
            "z",
            "z/last.txt",
            "generated/info.txt"
        ]
    );
}

#[test]
fn zip_writer_is_deterministic_and_round_trips() {
    let temp = tempfile::tempdir().unwrap();
    let (first, second) = (temp.path().join("one"), temp.path().join("two"));
    populate(&first, false);
    std::thread::sleep(std::time::Duration::from_millis(2100));
    populate(&second, true);
    write_zip(&first, &temp.path().join("one.zip"));
    write_zip(&second, &temp.path().join("two.zip"));
    assert_eq!(
        fs::read(temp.path().join("one.zip")).unwrap(),
        fs::read(temp.path().join("two.zip")).unwrap()
    );
    let out = temp.path().join("out");
    extract(
        &temp.path().join("one.zip"),
        &out,
        ArchiveFormat::Zip,
        ExtractionLimits::default(),
    )
    .unwrap();
    assert_tree(&out.join("pkg"));
    assert_eq!(
        fs::read(out.join("generated/info.txt")).unwrap(),
        b"generated"
    );
}

#[test]
fn writer_symlinks_round_trip_through_extraction() {
    let temp = tempfile::tempdir().unwrap();
    let mut tar = TarWriter::plain(Vec::new());
    tar.append_bytes("bin/tool", b"#!/bin/sh\n", 0o755).unwrap();
    tar.append_symlink("tool", "bin/tool").unwrap();
    let tar = tar.finish().unwrap();
    extract_reader(
        &tar[..],
        &temp.path().join("tar"),
        ArchiveFormat::TarZstd,
        ExtractionLimits::default(),
    )
    .expect_err("plain tar bytes are not a zstd stream");
    let zstd_tar = zstd_encode(&tar, 3).unwrap();
    extract_reader(
        &zstd_tar[..],
        &temp.path().join("tar"),
        ArchiveFormat::TarZstd,
        ExtractionLimits::default(),
    )
    .unwrap();

    let mut zip = ZipWriter::new(Cursor::new(Vec::new()), ZipCompression::Stored);
    zip.append_bytes("bin/tool", b"#!/bin/sh\n", 0o755).unwrap();
    zip.append_symlink("tool", "bin/tool").unwrap();
    let zip = zip.finish().unwrap().into_inner();
    fs::write(temp.path().join("links.zip"), zip).unwrap();
    extract(
        &temp.path().join("links.zip"),
        &temp.path().join("zip"),
        ArchiveFormat::Zip,
        ExtractionLimits::default(),
    )
    .unwrap();
    for root in ["tar", "zip"] {
        let link = temp.path().join(root).join("tool");
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(link).unwrap(), b"#!/bin/sh\n");
    }
}

#[test]
fn zstd_buffers_round_trip_deterministically_and_respect_the_ceiling() {
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let encoded = zstd_encode(&data, 19).unwrap();
    assert_eq!(encoded, zstd_encode(&data, 19).unwrap());
    assert!(encoded.len() < data.len());
    assert_eq!(zstd_decode(&encoded, data.len() as u64).unwrap(), data);
    assert_eq!(zstd_decode(&encoded, u64::MAX).unwrap(), data);
    assert!(zstd_decode(&encoded, data.len() as u64 - 1).is_err());
    assert!(zstd_decode(b"not zstd", u64::MAX).is_err());
}

fn plain_tar() -> Vec<u8> {
    let mut tar = TarWriter::plain(Vec::new());
    tar.append_dir("pkg").unwrap();
    tar.append_bytes("pkg/data.txt", b"payload", 0o644).unwrap();
    tar.finish().unwrap()
}

#[test]
fn tar_reader_and_extract_reader_accept_any_reader() {
    let tar = plain_tar();
    let mut reader = TarReader::new(Cursor::new(tar.clone()), TarCompression::None).unwrap();
    let kinds: Vec<_> = reader
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().kind())
        .collect();
    assert_eq!(kinds, [TarEntryKind::Directory, TarEntryKind::File]);

    let temp = tempfile::tempdir().unwrap();
    let gzip = {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    };
    extract_reader(
        &gzip[..],
        &temp.path().join("out"),
        ArchiveFormat::TarGzip,
        ExtractionLimits::default(),
    )
    .unwrap();
    assert_eq!(
        fs::read(temp.path().join("out/pkg/data.txt")).unwrap(),
        b"payload"
    );
    let too_small = ExtractionLimits {
        max_input_bytes: gzip.len() as u64 - 1,
        ..ExtractionLimits::default()
    };
    assert!(extract_reader(
        &gzip[..],
        &temp.path().join("small"),
        ArchiveFormat::TarGzip,
        too_small
    )
    .is_err());
    assert!(extract_reader(
        &gzip[..],
        &temp.path().join("zip"),
        ArchiveFormat::Zip,
        ExtractionLimits::default()
    )
    .is_err());
}

#[cfg(feature = "archive-xz")]
#[test]
fn tar_xz_extracts_from_file_and_reader() {
    use std::io::Write;
    let mut encoder =
        lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(6)).unwrap();
    encoder.write_all(&plain_tar()).unwrap();
    let xz = encoder.finish().unwrap();
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("a.tar.xz"), &xz).unwrap();
    extract(
        &temp.path().join("a.tar.xz"),
        &temp.path().join("file"),
        ArchiveFormat::TarXz,
        ExtractionLimits::default(),
    )
    .unwrap();
    extract_reader(
        &xz[..],
        &temp.path().join("reader"),
        ArchiveFormat::TarXz,
        ExtractionLimits::default(),
    )
    .unwrap();
    for root in ["file", "reader"] {
        assert_eq!(
            fs::read(temp.path().join(root).join("pkg/data.txt")).unwrap(),
            b"payload"
        );
    }
    let mut reader = TarReader::from_slice(&xz, TarCompression::Xz).unwrap();
    assert_eq!(reader.entries().unwrap().count(), 2);
}

#[cfg(feature = "archive-bzip2")]
#[test]
fn tar_bzip2_extracts_from_file_and_reader() {
    use std::io::Write;
    let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    encoder.write_all(&plain_tar()).unwrap();
    let bz2 = encoder.finish().unwrap();
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("a.tar.bz2"), &bz2).unwrap();
    extract(
        &temp.path().join("a.tar.bz2"),
        &temp.path().join("file"),
        ArchiveFormat::TarBzip2,
        ExtractionLimits::default(),
    )
    .unwrap();
    extract_reader(
        &bz2[..],
        &temp.path().join("reader"),
        ArchiveFormat::TarBzip2,
        ExtractionLimits::default(),
    )
    .unwrap();
    for root in ["file", "reader"] {
        assert_eq!(
            fs::read(temp.path().join(root).join("pkg/data.txt")).unwrap(),
            b"payload"
        );
    }
    let mut reader = TarReader::from_slice(&bz2, TarCompression::Bzip2).unwrap();
    assert_eq!(reader.entries().unwrap().count(), 2);
}

#[test]
fn merge_into_nonempty_destination_is_explicit_opt_in() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("a.tar.zst");
    fs::write(&archive, zstd_encode(&plain_tar(), 3).unwrap()).unwrap();
    let dest = temp.path().join("dest");
    fs::create_dir_all(dest.join("pkg")).unwrap();
    fs::write(dest.join("keep.txt"), b"mine").unwrap();
    fs::write(dest.join("pkg/data.txt"), b"stale").unwrap();
    let error = extract(
        &archive,
        &dest,
        ArchiveFormat::TarZstd,
        ExtractionLimits::default(),
    )
    .expect_err("default still requires an empty destination");
    assert!(error.to_string().contains("not empty"), "{error}");
    assert_eq!(fs::read(dest.join("pkg/data.txt")).unwrap(), b"stale");
    let merge = ExtractionLimits {
        existing_destination: ExistingDestination::Merge,
        ..ExtractionLimits::default()
    };
    extract(&archive, &dest, ArchiveFormat::TarZstd, merge).unwrap();
    assert_eq!(fs::read(dest.join("pkg/data.txt")).unwrap(), b"payload");
    assert_eq!(fs::read(dest.join("keep.txt")).unwrap(), b"mine");
}

#[test]
fn unresolved_symlinks_are_explicit_opt_in_and_still_confined() {
    let temp = tempfile::tempdir().unwrap();
    let archive_with = |target: &str| {
        let mut tar = TarWriter::plain(Vec::new());
        tar.append_bytes("file.txt", b"x", 0o644).unwrap();
        tar.append_symlink("link", target).unwrap();
        let path = temp
            .path()
            .join(format!("{}.tar.zst", target.replace(['/', '.'], "_")));
        fs::write(&path, zstd_encode(&tar.finish().unwrap(), 3).unwrap()).unwrap();
        path
    };
    let missing = archive_with("missing/dir/tool");
    let limits = |dangling_links| ExtractionLimits {
        dangling_links,
        ..ExtractionLimits::default()
    };
    for policy in [DanglingLinks::Reject, DanglingLinks::PreserveMissingLeaf] {
        let out = temp.path().join(format!("{policy:?}"));
        assert!(extract(&missing, &out, ArchiveFormat::TarZstd, limits(policy)).is_err());
    }
    let out = temp.path().join("unresolved");
    extract(
        &missing,
        &out,
        ArchiveFormat::TarZstd,
        limits(DanglingLinks::PreserveUnresolved),
    )
    .unwrap();
    assert!(fs::symlink_metadata(out.join("link"))
        .unwrap()
        .file_type()
        .is_symlink());

    let escaping = archive_with("missing/../../outside");
    let out = temp.path().join("escape");
    assert!(extract(
        &escaping,
        &out,
        ArchiveFormat::TarZstd,
        limits(DanglingLinks::PreserveUnresolved)
    )
    .is_err());
    assert!(fs::symlink_metadata(out.join("link")).is_err());
}

#[test]
fn size_caps_are_removed_only_on_request() {
    let caps = ExtractionLimits::without_size_caps();
    assert_eq!(caps.max_input_bytes, u64::MAX);
    assert_eq!(caps.max_output_bytes, u64::MAX);
    assert_eq!(caps.max_entry_bytes, u64::MAX);
    assert_eq!(caps.max_entries, u64::MAX);
    assert_eq!(caps.dangling_links, DanglingLinks::Reject);
    assert_eq!(caps.existing_destination, ExistingDestination::RequireEmpty);
    let defaults = ExtractionLimits::default();
    assert_eq!(caps.max_path_bytes, defaults.max_path_bytes);
    assert!(defaults.max_output_bytes < u64::MAX);

    let temp = tempfile::tempdir().unwrap();
    let archive = zstd_encode(&plain_tar(), 3).unwrap();
    let tiny = ExtractionLimits {
        max_output_bytes: 3,
        ..ExtractionLimits::default()
    };
    assert!(extract_reader(
        &archive[..],
        &temp.path().join("tiny"),
        ArchiveFormat::TarZstd,
        tiny
    )
    .is_err());
    extract_reader(
        &archive[..],
        &temp.path().join("open"),
        ArchiveFormat::TarZstd,
        caps,
    )
    .unwrap();
}

#[cfg(not(all(feature = "archive-xz", feature = "archive-bzip2")))]
#[test]
fn disabled_codecs_report_unsupported() {
    let temp = tempfile::tempdir().unwrap();
    let format = if cfg!(feature = "archive-xz") {
        ArchiveFormat::TarBzip2
    } else {
        ArchiveFormat::TarXz
    };
    let error = extract_reader(&b""[..], temp.path(), format, ExtractionLimits::default())
        .expect_err("codec feature is disabled");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

// Rewrite every local and central header's method field. Only Stored and
// Deflated are readable, so a bzip2-labelled entry must fail cleanly.
fn relabel_zip_method(archive: &mut [u8], method: u16) {
    for index in 0..archive.len().saturating_sub(12) {
        let field = match &archive[index..index + 4] {
            b"PK\x03\x04" => index + 8,
            b"PK\x01\x02" => index + 10,
            _ => continue,
        };
        archive[field..field + 2].copy_from_slice(&method.to_le_bytes());
    }
}

#[test]
fn zip_entry_with_unsupported_method_fails_with_a_facade_error() {
    let temp = tempfile::tempdir().unwrap();
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()), ZipCompression::Stored);
    writer
        .append_bytes("payload.txt", b"payload", 0o644)
        .unwrap();
    let mut archive = writer.finish().unwrap().into_inner();
    relabel_zip_method(&mut archive, 12);
    let path = temp.path().join("bzip2.zip");
    fs::write(&path, &archive).unwrap();
    let error = extract(
        &path,
        &temp.path().join("out"),
        ArchiveFormat::Zip,
        ExtractionLimits::default(),
    )
    .expect_err("bzip2 ZIP entries are unsupported");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    let debug = format!("{error:?}");
    assert!(
        !debug.contains("ZipError") && !debug.contains("CompressionMethodNotSupported"),
        "backend error leaked: {debug}"
    );
    let inner = error.into_inner().expect("facade error carries a message");
    assert!(inner.source().is_none(), "backend error leaked as a source");
}
