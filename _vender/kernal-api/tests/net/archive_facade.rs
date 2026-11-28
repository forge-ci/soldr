#![cfg(feature = "archive")]

use kernal_api::archive::{extract, ArchiveFormat, DanglingLinks, ExtractionLimits};
use std::io::{Read, Write};

#[test]
#[ignore = "requires KERNAL_ARCHIVE_ZSTD_FIXTURE pointing to a real toolchain archive"]
fn real_zstd_toolchain_extracts_into_fresh_staging() {
    let source = std::env::var_os("KERNAL_ARCHIVE_ZSTD_FIXTURE").expect("fixture path");
    let temp = tempfile::tempdir().unwrap();
    extract(
        std::path::Path::new(&source),
        temp.path(),
        ArchiveFormat::TarZstd,
        ExtractionLimits {
            dangling_links: DanglingLinks::PreserveMissingLeaf,
            ..ExtractionLimits::default()
        },
    )
    .unwrap();
    assert!(std::fs::read_dir(temp.path()).unwrap().next().is_some());
    let decoder = zstd::stream::read::Decoder::new(std::fs::File::open(&source).unwrap()).unwrap();
    let mut archive = tar::Archive::new(decoder);
    let mut files = 0;
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let output = temp.path().join(entry.path().unwrap());
        let kind = entry.header().entry_type();
        if kind.is_file() {
            let expected = kernal_api::hash::blake3_reader(&mut entry, Default::default()).unwrap();
            let actual = kernal_api::hash::blake3_file(&output, Default::default()).unwrap();
            assert_eq!(actual, expected, "{}", output.display());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
                    entry.header().mode().unwrap() & 0o777
                );
            }
            files += 1;
        } else if kind.is_symlink() {
            assert_eq!(
                std::fs::read_link(&output).unwrap(),
                entry.link_name().unwrap().unwrap()
            );
        } else if kind.is_dir() {
            assert!(output.is_dir());
        }
    }
    assert!(files > 0);
}

#[test]
fn tar_zstd_preserves_long_names_pax_and_links() {
    let mut builder = tar::Builder::new(Vec::new());
    builder
        .append_pax_extensions([("comment", b"bounded metadata".as_slice())])
        .unwrap();
    let long_name = format!("root/{}/tool", "nested".repeat(24));
    let mut header = tar::Header::new_gnu();
    header.set_size(4);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(&mut header, &long_name, &b"tool"[..])
        .unwrap();
    {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        builder
            .append_link(&mut header, "alias", &long_name)
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Link);
        header.set_size(0);
        header.set_mode(0o755);
        builder
            .append_link(&mut header, "hard", &long_name)
            .unwrap();
    }
    let bytes = zstd::stream::encode_all(builder.into_inner().unwrap().as_slice(), 1).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("input.tar.zst");
    std::fs::write(&archive, bytes).unwrap();
    let out = dir.path().join("out");
    extract(
        &archive,
        &out,
        ArchiveFormat::TarZstd,
        ExtractionLimits::default(),
    )
    .unwrap();
    assert_eq!(std::fs::read(out.join(&long_name)).unwrap(), b"tool");
    assert_eq!(std::fs::read(out.join("alias")).unwrap(), b"tool");
    assert_eq!(std::fs::read(out.join("hard")).unwrap(), b"tool");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(out.join("hard")).unwrap().ino(),
            std::fs::metadata(out.join(&long_name)).unwrap().ino()
        );
    }
}

#[test]
fn tar_rejects_oversized_extension_before_reading_its_body_and_corrupt_gzip_footer() {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::GNULongName);
    header.set_size(65537);
    header.set_cksum();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(header.as_bytes()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("input.tgz");
    std::fs::write(&archive, gzip.finish().unwrap()).unwrap();
    let error = extract(
        &archive,
        &dir.path().join("out"),
        ArchiveFormat::TarGzip,
        ExtractionLimits::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("64 KiB"), "{error}");
    let mut bytes = tgz_fixture();
    let crc = bytes.len() - 8;
    bytes[crc] ^= 1;
    std::fs::write(&archive, bytes).unwrap();
    assert!(kernal_api::archive::extract_member(
        &archive,
        "package/bin/tool",
        &dir.path().join("selected"),
        ArchiveFormat::TarGzip,
        ExtractionLimits::default()
    )
    .is_err());
}

#[test]
fn tar_global_pax_resets_local_size_before_next_header() {
    let mut builder = tar::Builder::new(Vec::new());
    builder
        .append_pax_extensions([("size", b"100000".as_slice())])
        .unwrap();
    let mut global = tar::Header::new_ustar();
    global.set_entry_type(tar::EntryType::XGlobalHeader);
    global.set_size(0);
    global.set_mode(0o644);
    global.set_cksum();
    builder
        .append_data(&mut global, "global", std::io::empty())
        .unwrap();
    let mut regular = tar::Header::new_gnu();
    regular.set_size(0);
    regular.set_mode(0o644);
    regular.set_cksum();
    builder
        .append_data(&mut regular, "file", std::io::empty())
        .unwrap();
    let mut bytes = builder.into_inner().unwrap();
    bytes.truncate(bytes.len() - 1024);
    let mut long = tar::Header::new_gnu();
    long.set_entry_type(tar::EntryType::GNULongName);
    long.set_size(65537);
    long.set_cksum();
    bytes.extend_from_slice(long.as_bytes());
    let compressed = zstd::stream::encode_all(bytes.as_slice(), 1).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("input.tar.zst");
    std::fs::write(&archive, compressed).unwrap();
    let error = extract(
        &archive,
        &dir.path().join("out"),
        ArchiveFormat::TarZstd,
        ExtractionLimits::default(),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("64 KiB"),
        "must reject oversized header before trying to read body: {error}"
    );
}

#[test]
#[ignore = "requires KERNAL_ARCHIVE_TGZ_FIXTURE pointing to a real archive"]
fn real_tgz_member_matches_original_stream() {
    let source = std::env::var_os("KERNAL_ARCHIVE_TGZ_FIXTURE").expect("fixture path");
    let member = std::env::var("KERNAL_ARCHIVE_TGZ_MEMBER").expect("member name");
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("tool");
    kernal_api::archive::extract_member(
        std::path::Path::new(&source),
        &member,
        &output,
        ArchiveFormat::TarGzip,
        ExtractionLimits::default(),
    )
    .unwrap();
    let decoder = flate2::read::GzDecoder::new(std::fs::File::open(&source).unwrap());
    let mut archive = tar::Archive::new(decoder);
    let mut expected = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        if entry.path().unwrap() == std::path::Path::new(&member) {
            entry.read_to_end(&mut expected).unwrap();
            break;
        }
    }
    assert!(!expected.is_empty());
    assert_eq!(std::fs::read(output).unwrap(), expected);
}

fn tgz_fixture() -> Vec<u8> {
    let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(gzip);
    let mut header = tar::Header::new_gnu();
    header.set_size(4);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(&mut header, "package/bin/tool", &b"tool"[..])
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}

#[test]
fn tgz_extracts_files_and_selected_member() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("input.tgz");
    std::fs::write(&archive, tgz_fixture()).unwrap();
    let out = dir.path().join("out");
    extract(
        &archive,
        &out,
        ArchiveFormat::TarGzip,
        ExtractionLimits::default(),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(out.join("package/bin/tool")).unwrap(),
        b"tool"
    );
    let member = dir.path().join("single/tool");
    kernal_api::archive::extract_member(
        &archive,
        "package/bin/tool",
        &member,
        ArchiveFormat::TarGzip,
        ExtractionLimits::default(),
    )
    .unwrap();
    assert_eq!(std::fs::read(&member).unwrap(), b"tool");
}

/// Read-only source fixture; extracted data is isolated and never executed.
#[test]
#[ignore = "requires KERNAL_ARCHIVE_ZIP_FIXTURE pointing to a real archive"]
fn real_zip_fixture_extracts_into_fresh_staging() {
    let archive = std::env::var_os("KERNAL_ARCHIVE_ZIP_FIXTURE").expect("fixture path");
    let temp = tempfile::tempdir().unwrap();
    extract(
        std::path::Path::new(&archive),
        temp.path(),
        ArchiveFormat::Zip,
        ExtractionLimits::default(),
    )
    .unwrap();
    assert!(std::fs::read_dir(temp.path()).unwrap().next().is_some());
}

fn zip_links(links: &[(&str, &str)]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    writer.start_file("lib/tool", options).unwrap();
    writer.write_all(b"executable").unwrap();
    for (path, target) in links {
        writer.add_symlink(*path, *target, options).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[test]
fn zip_rejects_link_targets_with_non_directory_prefixes() {
    for target in [
        "lib/tool/../tool",
        "missing/../lib/tool",
        "lib/tool/child",
        "lib/tool/",
        "lib/tool/.",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("input.zip");
        std::fs::write(&archive, zip_links(&[("bad", target)])).unwrap();
        let out = dir.path().join("out");
        assert!(
            extract(
                &archive,
                &out,
                ArchiveFormat::Zip,
                ExtractionLimits::default()
            )
            .is_err(),
            "accepted {target}"
        );
        assert!(std::fs::symlink_metadata(out.join("bad")).is_err());
    }
}

#[cfg(unix)]
#[test]
fn zip_preserves_missing_leaf_only_when_explicitly_enabled() {
    for target in [r"..\acorn\bin\acorn", "lib/missing"] {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("input.zip");
        std::fs::write(&archive, zip_links(&[("alias", target)])).unwrap();
        assert!(extract(
            &archive,
            &dir.path().join("strict"),
            ArchiveFormat::Zip,
            ExtractionLimits::default()
        )
        .is_err());
        let out = dir.path().join("out");
        extract(
            &archive,
            &out,
            ArchiveFormat::Zip,
            ExtractionLimits {
                dangling_links: DanglingLinks::PreserveMissingLeaf,
                ..ExtractionLimits::default()
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::read_link(out.join("alias")).unwrap(),
            std::path::Path::new(target)
        );
        assert!(!out.join("alias").exists());
    }
    for target in [
        "../outside",
        "missing/../lib/tool",
        "lib/tool/../tool",
        "missing/child",
        "lib/missing/",
        "lib/tool/.",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("input.zip");
        std::fs::write(&archive, zip_links(&[("bad", target)])).unwrap();
        assert!(
            extract(
                &archive,
                &dir.path().join("out"),
                ArchiveFormat::Zip,
                ExtractionLimits {
                    dangling_links: DanglingLinks::PreserveMissingLeaf,
                    ..ExtractionLimits::default()
                }
            )
            .is_err(),
            "accepted {target}"
        );
    }
}

#[test]
fn zip_preserves_internal_link_chains_and_relative_parent_targets() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("input.zip");
    std::fs::write(
        &archive,
        zip_links(&[("bin/tool", "../alias/tool"), ("alias", "lib")]),
    )
    .unwrap();
    let out = dir.path().join("out");
    extract(
        &archive,
        &out,
        ArchiveFormat::Zip,
        ExtractionLimits::default(),
    )
    .unwrap();
    assert_eq!(std::fs::read(out.join("bin/tool")).unwrap(), b"executable");
    assert_eq!(
        std::fs::read_link(out.join("bin/tool")).unwrap(),
        std::path::PathBuf::from("../alias/tool")
    );
}

#[test]
fn zip_rejects_escaping_cyclic_and_dangling_links_before_creating_them() {
    for links in [
        vec![("bad", "../outside")],
        vec![("bad", "/outside")],
        vec![("bad", "other"), ("other", "bad")],
        vec![("bad", "missing")],
        vec![("alias", "."), ("bad", "alias/../outside")],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("input.zip");
        std::fs::write(&archive, zip_links(&links)).unwrap();
        let out = dir.path().join("out");
        assert!(extract(
            &archive,
            &out,
            ArchiveFormat::Zip,
            ExtractionLimits::default()
        )
        .is_err());
        assert!(std::fs::symlink_metadata(out.join("bad")).is_err());
    }
}

fn zip_fixture(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (path, content) in files {
        writer
            .start_file(*path, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(content).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[test]
fn zip_extracts_nested_files_and_creates_destination() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("input.zip");
    std::fs::write(
        &archive,
        zip_fixture(&[("hello.txt", b"hello"), ("sub/world.txt", b"world")]),
    )
    .unwrap();
    let out = dir.path().join("nested/out");
    extract(
        &archive,
        &out,
        ArchiveFormat::Zip,
        ExtractionLimits::default(),
    )
    .unwrap();
    assert_eq!(std::fs::read(out.join("hello.txt")).unwrap(), b"hello");
    assert_eq!(std::fs::read(out.join("sub/world.txt")).unwrap(), b"world");
}

#[test]
fn archive_entry_limit_is_independent_of_total_output() {
    let mut tar = tar::Builder::new(Vec::new());
    for name in ["first", "second"] {
        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, name, &b"12345"[..]).unwrap();
    }
    let tar = tar.into_inner().unwrap();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&tar).unwrap();
    let fixtures = [
        (
            ArchiveFormat::Zip,
            zip_fixture(&[("first", b"12345"), ("second", b"12345")]),
        ),
        (ArchiveFormat::TarGzip, gzip.finish().unwrap()),
        (
            ArchiveFormat::TarZstd,
            zstd::stream::encode_all(&tar[..], 1).unwrap(),
        ),
    ];
    for (format, bytes) in fixtures {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("archive");
        std::fs::write(&input, bytes).unwrap();
        let rejected = dir.path().join("rejected");
        let limits = ExtractionLimits {
            max_entry_bytes: 4,
            max_output_bytes: 10,
            ..ExtractionLimits::default()
        };
        let error = extract(&input, &rejected, format, limits).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(!rejected.join("first").exists());
        if !matches!(format, ArchiveFormat::Zip) {
            // Selecting a later member must not bypass validation of an
            // oversized member that the tar backend would otherwise skip.
            let selected = dir.path().join("selected");
            let error =
                kernal_api::archive::extract_member(&input, "second", &selected, format, limits)
                    .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert!(!selected.exists());
        }
        let accepted = dir.path().join("accepted");
        extract(
            &input,
            &accepted,
            format,
            ExtractionLimits {
                max_entry_bytes: 5,
                ..limits
            },
        )
        .unwrap();
        for name in ["first", "second"] {
            assert_eq!(std::fs::read(accepted.join(name)).unwrap(), b"12345");
        }
    }
}

#[test]
fn zip_rejects_traversal_and_resource_overflow() {
    for path in ["../escape", "/absolute", "C:/drive", "sub\\escape"] {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("input.zip");
        std::fs::write(&archive, zip_fixture(&[(path, b"data")])).unwrap();
        assert!(extract(
            &archive,
            &dir.path().join("out"),
            ArchiveFormat::Zip,
            ExtractionLimits::default()
        )
        .is_err());
        assert!(!dir.path().join("escape").exists());
    }
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("input.zip");
    std::fs::write(&archive, zip_fixture(&[("large", b"12345")])).unwrap();
    let limits = ExtractionLimits {
        max_output_bytes: 4,
        ..ExtractionLimits::default()
    };
    assert!(extract(
        &archive,
        &dir.path().join("out"),
        ArchiveFormat::Zip,
        limits
    )
    .is_err());
}

#[test]
fn zip_rejects_input_metadata_entry_limits_and_nonempty_destination() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("input.zip");
    std::fs::write(&archive, zip_fixture(&[("file", b"data")])).unwrap();
    for (index, limits) in [
        ExtractionLimits {
            max_input_bytes: 1,
            ..ExtractionLimits::default()
        },
        ExtractionLimits {
            max_entries: 0,
            ..ExtractionLimits::default()
        },
        ExtractionLimits {
            max_metadata_bytes: 1,
            ..ExtractionLimits::default()
        },
        ExtractionLimits {
            max_path_bytes: 1,
            ..ExtractionLimits::default()
        },
    ]
    .into_iter()
    .enumerate()
    {
        assert!(extract(
            &archive,
            &dir.path().join(format!("out{index}")),
            ArchiveFormat::Zip,
            limits
        )
        .is_err());
    }
    let out = dir.path().join("existing");
    std::fs::create_dir(&out).unwrap();
    std::fs::write(out.join("file"), b"preserve").unwrap();
    assert!(extract(
        &archive,
        &out,
        ArchiveFormat::Zip,
        ExtractionLimits::default()
    )
    .is_err());
    assert_eq!(std::fs::read(out.join("file")).unwrap(), b"preserve");
}

#[test]
fn zip_rejects_inconsistent_directory_entry_counts() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("input.zip");
    let mut bytes = zip_fixture(&[("file", b"data")]);
    let end = bytes.len() - 22;
    bytes[end + 10..end + 12].copy_from_slice(&0_u16.to_le_bytes());
    std::fs::write(&archive, bytes).unwrap();
    assert!(extract(
        &archive,
        &dir.path().join("out"),
        ArchiveFormat::Zip,
        ExtractionLimits::default()
    )
    .is_err());
}

#[test]
fn zip64_metadata_is_bounded_before_backend_allocation() {
    let original = zip_fixture(&[("file", b"data")]);
    let footer = &original[original.len() - 22..];
    let offset = (original.len() - 22) as u64;
    let directory_size = u32::from_le_bytes(footer[12..16].try_into().unwrap()) as u64;
    let directory_offset = u32::from_le_bytes(footer[16..20].try_into().unwrap()) as u64;
    for count in [1, u64::MAX] {
        let mut bytes = original[..original.len() - 22].to_vec();
        bytes.extend_from_slice(b"PK\x06\x06");
        bytes.extend_from_slice(&44_u64.to_le_bytes());
        bytes.extend_from_slice(&45_u16.to_le_bytes());
        bytes.extend_from_slice(&45_u16.to_le_bytes());
        bytes.extend_from_slice(&[0; 8]);
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&directory_size.to_le_bytes());
        bytes.extend_from_slice(&directory_offset.to_le_bytes());
        bytes.extend_from_slice(b"PK\x06\x07");
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&offset.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        let mut footer = footer.to_vec();
        footer[8..20].fill(0xff);
        bytes.extend_from_slice(&footer);
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("input.zip");
        std::fs::write(&archive, bytes).unwrap();
        let out = dir.path().join("out");
        let result = extract(
            &archive,
            &out,
            ArchiveFormat::Zip,
            ExtractionLimits::default(),
        );
        if count == 1 {
            result.unwrap();
            assert_eq!(std::fs::read(out.join("file")).unwrap(), b"data");
        } else {
            assert!(result.is_err());
            assert!(!out.exists());
        }
    }
}

#[cfg(unix)]
#[test]
fn zip_preserves_executable_bits_without_special_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("input.zip");
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file(
            "tool",
            zip::write::SimpleFileOptions::default().unix_permissions(0o755),
        )
        .unwrap();
    writer.write_all(b"tool").unwrap();
    std::fs::write(&archive, writer.finish().unwrap().into_inner()).unwrap();
    let out = dir.path().join("out");
    extract(
        &archive,
        &out,
        ArchiveFormat::Zip,
        ExtractionLimits::default(),
    )
    .unwrap();
    assert_eq!(
        std::fs::metadata(out.join("tool"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
}
