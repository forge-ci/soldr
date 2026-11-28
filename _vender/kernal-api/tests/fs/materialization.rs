#![cfg(feature = "fs")]
//! Cache-materialization primitives: replacement, links, permission bits,
//! path identity, change markers, volume facts, and native path spellings.

use std::fs;
use std::io;
use std::path::Path;

use kernal_api::platform::fs::{
    allocated_bytes, apply_metadata_mode, classify, file_change_marker, file_id_width,
    hard_link_count, make_executable, metadata_mode, native_call_path, path_file,
    path_from_raw_bytes, replacement, set_readonly, symlink_file, sync_directory_if_supported,
    volume_identity_u128, LinkKind,
};

fn write(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write fixture");
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).expect("read fixture")
}

// ---------------------------------------------------------------------------
// Replacement
// ---------------------------------------------------------------------------

#[test]
fn atomic_replace_swaps_the_destination_and_consumes_the_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("source");
    let destination = dir.path().join("destination");
    write(&source, "new");
    write(&destination, "old");

    replacement::atomic_replace(&source, &destination).expect("replace");

    assert_eq!(read(&destination), "new");
    assert!(!source.exists(), "success consumes the source path");
}

#[test]
fn a_failed_atomic_replace_leaves_the_destination_intact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let destination = dir.path().join("destination");
    write(&destination, "old");

    let error = replacement::atomic_replace(&dir.path().join("missing"), &destination)
        .expect_err("a missing source cannot replace");

    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert_eq!(read(&destination), "old");
}

#[test]
fn rename_generation_moves_into_an_unused_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("generation.tmp");
    let destination = dir.path().join("generation-1");
    write(&source, "generation");

    replacement::rename_generation(&source, &destination).expect("rename");

    assert_eq!(read(&destination), "generation");
    assert!(!source.exists());
}

#[test]
fn rename_generation_collision_behavior_is_host_native() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("source");
    let destination = dir.path().join("destination");
    write(&source, "new");
    write(&destination, "old");

    let result = replacement::rename_generation(&source, &destination);

    if kernal_api::platform::host::target_is_windows() {
        result.expect_err("Windows refuses an existing generation");
        assert_eq!(read(&destination), "old");
        assert_eq!(read(&source), "new");
    } else {
        result.expect("Unix rename replaces");
        assert_eq!(read(&destination), "new");
    }
}

#[test]
fn replace_with_delete_fallback_replaces_an_existing_destination() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("source");
    let destination = dir.path().join("destination");
    write(&source, "new");
    write(&destination, "old");

    replacement::replace_with_delete_fallback(&source, &destination).expect("replace");

    assert_eq!(read(&destination), "new");
    assert!(!source.exists());
}

#[test]
fn install_directory_creates_the_parent_for_a_fresh_destination() {
    let dir = tempfile::tempdir().expect("tempdir");
    let staged = dir.path().join("staged");
    let requested = dir.path().join("nested").join("installed");
    fs::create_dir(&staged).expect("staged dir");
    write(&staged.join("file"), "contents");

    replacement::install_directory(&staged, &requested).expect("install");

    assert_eq!(read(&requested.join("file")), "contents");
    assert!(!staged.exists(), "success consumes the staged tree");
}

#[test]
fn install_directory_replaces_an_existing_tree_and_removes_the_old_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let staged = dir.path().join("staged");
    let requested = dir.path().join("installed");
    fs::create_dir(&staged).expect("staged dir");
    fs::create_dir(&requested).expect("existing dir");
    write(&staged.join("new"), "new");
    write(&requested.join("old"), "old");

    replacement::install_directory(&staged, &requested).expect("install");

    assert_eq!(read(&requested.join("new")), "new");
    assert!(!requested.join("old").exists(), "the old tree is gone");
    assert!(!staged.exists(), "neither tree remains at the staged path");
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .expect("list parent")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(leftovers, ["installed"], "no backup or staged tree remains");
}

#[test]
fn share_error_classifiers_recognize_only_windows_codes() {
    let access_denied = io::Error::from_raw_os_error(5);
    let sharing_violation = io::Error::from_raw_os_error(32);
    let lock_violation = io::Error::from_raw_os_error(33);
    let synthetic = io::Error::new(io::ErrorKind::PermissionDenied, "synthetic");
    let windows = kernal_api::platform::host::target_is_windows();

    assert_eq!(
        replacement::is_transient_share_error(&access_denied),
        windows
    );
    assert_eq!(
        replacement::is_transient_share_error(&sharing_violation),
        windows
    );
    assert!(!replacement::is_transient_share_error(&lock_violation));
    assert!(
        !replacement::is_transient_share_error(&synthetic),
        "a synthesized PermissionDenied has no native share code"
    );
    assert_eq!(replacement::is_lock_contention(&lock_violation), windows);
    assert!(!replacement::is_lock_contention(&sharing_violation));
    assert!(!replacement::is_lock_contention(&io::Error::from(
        io::ErrorKind::WouldBlock
    )));
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

#[test]
fn hard_link_count_tracks_each_new_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("source");
    let peer = dir.path().join("peer");
    write(&source, "linked");
    let before = hard_link_count(&source).expect("initial count");

    fs::hard_link(&source, &peer).expect("hard link");

    let after = hard_link_count(&source).expect("new count");
    assert_eq!(after, before + 1);
    assert_eq!(after, hard_link_count(&peer).expect("peer count"));
}

#[test]
fn hard_link_count_of_a_missing_path_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    hard_link_count(&dir.path().join("missing")).expect_err("missing path");
}

#[test]
fn ordinary_files_and_directories_classify_as_regular() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("file");
    write(&file, "regular");

    assert_eq!(classify(&file).expect("classify file"), LinkKind::Regular);
    assert_eq!(
        classify(dir.path()).expect("classify directory"),
        LinkKind::Regular
    );
    assert_eq!(
        classify(&dir.path().join("missing"))
            .expect_err("missing entry")
            .kind(),
        io::ErrorKind::NotFound
    );
}

/// Windows symlink creation needs Developer Mode or a privilege the test
/// host may not grant; skip rather than fail when it is refused there.
fn try_symlink(target: &Path, link: &Path) -> bool {
    match symlink_file(target, link) {
        Ok(()) => true,
        // ERROR_PRIVILEGE_NOT_HELD (1314) is how Windows refuses it.
        Err(error)
            if kernal_api::platform::host::target_is_windows()
                && (error.kind() == io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314)) =>
        {
            false
        }
        Err(error) => panic!("create symlink: {error}"),
    }
}

#[test]
fn symlinks_classify_without_being_followed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("target");
    let link = dir.path().join("link");
    write(&target, "target");
    if !try_symlink(&target, &link) {
        return;
    }

    assert_eq!(classify(&link).expect("classify link"), LinkKind::Symlink);
    assert_eq!(read(&link), "target", "the link resolves to its target");
}

#[test]
fn a_dangling_link_classifies_but_its_link_count_does_not_resolve() {
    let dir = tempfile::tempdir().expect("tempdir");
    let link = dir.path().join("link");
    if !try_symlink(&dir.path().join("missing"), &link) {
        return;
    }

    assert_eq!(classify(&link).expect("classify link"), LinkKind::Symlink);
    hard_link_count(&link).expect_err("the count follows the link");
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

#[test]
fn readonly_round_trip_restores_writability() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("file");
    write(&file, "data");

    set_readonly(&file, true).expect("set readonly");
    assert!(fs::metadata(&file)
        .expect("metadata")
        .permissions()
        .readonly());
    set_readonly(&file, true).expect("setting the current state again succeeds");

    set_readonly(&file, false).expect("clear readonly");
    assert!(!fs::metadata(&file)
        .expect("metadata")
        .permissions()
        .readonly());
    write(&file, "rewritten");
}

#[test]
fn set_readonly_on_a_missing_path_is_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        set_readonly(&dir.path().join("missing"), false)
            .expect_err("missing path")
            .kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn metadata_mode_round_trips_through_apply_metadata_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("file");
    write(&file, "data");
    let writable = metadata_mode(&fs::metadata(&file).expect("metadata"));

    set_readonly(&file, true).expect("set readonly");
    let readonly = metadata_mode(&fs::metadata(&file).expect("metadata"));
    assert_ne!(writable, readonly);

    apply_metadata_mode(&file, writable).expect("restore writable");
    assert_eq!(
        metadata_mode(&fs::metadata(&file).expect("metadata")),
        writable
    );
    apply_metadata_mode(&file, readonly).expect("restore readonly");
    assert!(fs::metadata(&file)
        .expect("metadata")
        .permissions()
        .readonly());
    set_readonly(&file, false).expect("let the tempdir clean up");
}

#[test]
fn windows_metadata_mode_is_the_readonly_attribute() {
    if !kernal_api::platform::host::target_is_windows() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("file");
    write(&file, "data");
    assert_eq!(metadata_mode(&fs::metadata(&file).expect("metadata")), 0);
    apply_metadata_mode(&file, 7).expect("any nonzero value is readonly");
    assert_eq!(metadata_mode(&fs::metadata(&file).expect("metadata")), 1);
    apply_metadata_mode(&file, 0).expect("zero is writable");
}

#[cfg(unix)]
#[test]
fn unix_permission_toggles_retain_unrelated_mode_bits() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("file");
    write(&file, "data");
    let mode = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o7777;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o664)).expect("chmod");

    set_readonly(&file, true).expect("set readonly");
    assert_eq!(mode(&file), 0o444, "readonly clears every write bit");
    set_readonly(&file, false).expect("clear readonly");
    assert_eq!(mode(&file), 0o644, "writable adds only owner-write");
    make_executable(&file).expect("make executable");
    assert_eq!(mode(&file), 0o755, "execute adds every execute bit");

    apply_metadata_mode(&file, 0o100_600).expect("apply exact mode");
    assert_eq!(mode(&file), 0o600);
    assert_eq!(
        metadata_mode(&fs::metadata(&file).expect("metadata")) & 0o170_000,
        0o100_000,
        "metadata_mode keeps the full mode, including the file-type bits"
    );
}

#[test]
fn make_executable_leaves_contents_readable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("tool");
    write(&file, "data");
    make_executable(&file).expect("make executable");
    assert_eq!(read(&file), "data");
}

// ---------------------------------------------------------------------------
// Identity and change markers
// ---------------------------------------------------------------------------

#[test]
fn hard_links_share_a_path_identity_and_copies_do_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    let original = dir.path().join("original");
    let link = dir.path().join("link");
    let copy = dir.path().join("copy");
    write(&original, "data");
    fs::hard_link(&original, &link).expect("hard link");
    fs::copy(&original, &copy).expect("copy");

    let identity = path_file::file_identity(&original).expect("identity");
    assert_eq!(identity, path_file::file_identity(&link).expect("link"));
    assert_ne!(identity, path_file::file_identity(&copy).expect("copy"));
    assert!(path_file::same_file(&original, &link).expect("same file"));
    assert!(!path_file::same_file(&original, &copy).expect("distinct file"));
}

#[test]
fn path_identity_admits_directories() {
    let dir = tempfile::tempdir().expect("tempdir");
    let child = dir.path().join("child");
    fs::create_dir(&child).expect("child dir");
    assert_eq!(
        path_file::file_identity(&child).expect("directory identity"),
        path_file::file_identity(&child.join("..").join("child")).expect("alias identity")
    );
}

#[test]
fn missing_paths_have_no_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let present = dir.path().join("present");
    let missing = dir.path().join("missing");
    write(&present, "data");

    assert_eq!(
        path_file::file_identity(&missing)
            .expect_err("no identity")
            .kind(),
        io::ErrorKind::NotFound
    );
    let comparison = path_file::same_file(&present, &missing);
    if kernal_api::platform::host::target_is_windows() {
        assert!(!comparison.expect("Windows compares an unavailable identity as false"));
    } else {
        comparison.expect_err("Unix propagates the metadata error");
    }
}

#[test]
fn change_markers_are_host_journal_observations() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("file");
    write(&file, "v1");

    let first = file_change_marker(&file);
    assert_eq!(
        first,
        file_change_marker(&file),
        "an untouched file is stable"
    );
    if !kernal_api::platform::host::target_is_windows() {
        assert_eq!(first, None, "Linux and macOS keep no change journal");
    }
    assert_eq!(file_change_marker(&dir.path().join("missing")), None);
}

// ---------------------------------------------------------------------------
// Volume facts
// ---------------------------------------------------------------------------

#[test]
fn siblings_share_a_raw_volume_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    write(&first, "first");
    write(&second, "second");

    let volume = volume_identity_u128(&first).expect("volume identity");
    assert_eq!(Some(volume), volume_identity_u128(&second));
    assert_eq!(Some(volume), volume_identity_u128(dir.path()));
    assert_eq!(volume_identity_u128(&dir.path().join("missing")), None);
}

#[test]
fn file_id_width_matches_the_host_identifier() {
    const WIDTH: u32 = file_id_width();
    let expected = if kernal_api::platform::host::target_is_windows() {
        128
    } else {
        64
    };
    assert_eq!(WIDTH, expected);
}

#[test]
fn allocated_bytes_reports_storage_not_a_failure() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("file");
    fs::write(&file, vec![7_u8; 64 * 1024]).expect("write");
    let metadata = fs::metadata(&file).expect("metadata");

    let allocated = allocated_bytes(&file, &metadata);

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        assert_eq!(allocated, metadata.blocks() * 512);
    }
    #[cfg(windows)]
    assert_eq!(allocated, metadata.len(), "an uncompressed file");
    let _ = allocated;
}

// ---------------------------------------------------------------------------
// Paths and durability
// ---------------------------------------------------------------------------

#[test]
fn raw_path_bytes_keep_utf8_and_follow_the_host_rule_otherwise() {
    assert_eq!(
        path_from_raw_bytes(b"dir/header.h"),
        Some("dir/header.h".into())
    );
    let first = path_from_raw_bytes(b"header-\xff.h");
    let second = path_from_raw_bytes(b"header-\xfe.h");
    if kernal_api::platform::host::target_is_windows() {
        assert_eq!(first, None, "Windows refuses a lossy conversion");
        assert_eq!(second, None);
    } else {
        let first = first.expect("Unix preserves arbitrary bytes");
        assert_ne!(Some(first), second, "distinct bytes stay distinct");
    }
}

#[test]
fn native_call_path_names_the_same_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("file");
    write(&file, "data");
    let missing = dir.path().join("not-yet-created");

    let native = native_call_path(&file).expect("native path");
    assert_eq!(read(&native), "data");
    let pending = native_call_path(&missing).expect("the final file may be missing");
    assert_eq!(pending.file_name(), missing.file_name());

    if kernal_api::platform::host::target_is_windows() {
        assert!(native.is_absolute());
        assert!(native_call_path(Path::new("..")).is_err(), "no file name");
    } else {
        assert_eq!(native, file, "Unix returns the path unchanged");
    }
}

#[test]
fn directory_sync_is_best_effort_only_where_unsupported() {
    let dir = tempfile::tempdir().expect("tempdir");
    sync_directory_if_supported(dir.path()).expect("sync existing directory");

    let missing = sync_directory_if_supported(&dir.path().join("missing"));
    if kernal_api::platform::host::target_is_windows() {
        missing.expect("Windows does not resolve the directory");
    } else {
        missing.expect_err("Unix opens the directory to flush it");
    }
}

// ---------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------

#[test]
fn await_no_writers_never_reports_clear_while_a_writer_is_open() {
    use kernal_api::platform::fs::{await_no_writers, WriterWait};
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("artifact");
    let writer = fs::File::create(&path).expect("create");
    let open = await_no_writers(&path, Duration::from_millis(20)).expect("observe");
    assert!(
        matches!(open, WriterWait::TimedOut | WriterWait::Unobservable),
        "{open:?}"
    );
    drop(writer);
    let closed = await_no_writers(&path, Duration::from_secs(5)).expect("observe");
    assert!(
        matches!(closed, WriterWait::Clear { .. } | WriterWait::Unobservable),
        "{closed:?}"
    );
    assert_eq!(
        await_no_writers(&dir.path().join("missing"), Duration::ZERO)
            .expect_err("missing file")
            .kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn extent_sharing_never_calls_a_reflinked_copy_exclusive() {
    use kernal_api::platform::fs::{extent_sharing, reflink_file, ExtentSharing};

    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("source");
    fs::write(&source, vec![7_u8; 256 * 1024]).expect("write");
    let fresh = extent_sharing(&source).expect("observe fresh file");
    assert_ne!(
        fresh,
        ExtentSharing::Shared,
        "a fresh file shares no blocks"
    );

    let clone = dir.path().join("clone");
    if reflink_file(&source, &clone).is_ok() {
        // The volume can clone, so both names now share every extent.
        for path in [&source, &clone] {
            let observed = extent_sharing(path).expect("observe clone");
            assert_ne!(observed, ExtentSharing::Exclusive, "{path:?}");
        }
    }

    assert_eq!(
        extent_sharing(&dir.path().join("missing"))
            .expect_err("missing file")
            .kind(),
        io::ErrorKind::NotFound
    );
}
