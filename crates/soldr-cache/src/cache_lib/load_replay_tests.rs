//! soldr#3289: the per-file SOURCE mtime replay decision now delegates to
//! `zccache::fingerprint::mtime_replay::replay_one` instead of
//! reimplementing size/hash/set-times checks in soldr. These tests pin the
//! adapter (`source_file_to_mtime_entry`, `replay_workspace_root`) and the
//! end-to-end delegation behavior, in particular that a same-size,
//! content-changed source is never stamped with a stale recorded mtime.

use super::*;

/// A deliberately old recorded mtime (2020-09-13T12:26:40Z), distinct from
/// any "fresh" mtime used below so a wrongly-preserved value is obvious.
const OLD_MTIME_MS: i64 = 1_600_000_000_000;

/// A deliberately fresh mtime (2023-11-14T22:13:20Z) standing in for
/// "whatever a checkout just stamped the file with".
const FRESH_UNIX_SECS: i64 = 1_700_000_000;

const MAIN_RS: &[u8] = b"fn main() {}\n";

fn set_mtime_secs(path: &Path, secs: i64) {
    let time = filetime::FileTime::from_unix_time(secs, 0);
    filetime::set_file_mtime(path, time).expect("set mtime");
}

fn mtime_of(path: &Path) -> filetime::FileTime {
    let meta = std::fs::metadata(path).expect("stat");
    filetime::FileTime::from_last_modification_time(&meta)
}

/// Write `content` to `<ws>/src/main.rs` stamped with the OLD mtime and
/// return the manifest entry soldr's save would have recorded for it.
fn recorded_main_rs(ws: &Path, content: &[u8]) -> (PathBuf, SourceFile) {
    let src_dir = ws.join("src");
    std::fs::create_dir_all(&src_dir).expect("mkdir src");
    let path = src_dir.join("main.rs");
    std::fs::write(&path, content).expect("write main.rs");
    set_mtime_secs(&path, OLD_MTIME_MS / 1000);
    let entry = SourceFile {
        path: "src/main.rs".to_string(),
        mtime_ms: OLD_MTIME_MS,
        mtime_ns: None,
        size: content.len() as u64,
        blake3: hash_file(&path).expect("hash").to_vec(),
    };
    (path, entry)
}

#[test]
fn changed_same_size_source_is_never_stamped_old() {
    let ws = tempfile::tempdir().expect("tempdir");
    let (path, entry) = recorded_main_rs(ws.path(), MAIN_RS);

    // Same byte count, different content: only the hash can catch this.
    let edited: &[u8] = b"fn xain() {}\n";
    assert_eq!(edited.len(), MAIN_RS.len());
    std::fs::write(&path, edited).expect("edit main.rs");
    set_mtime_secs(&path, FRESH_UNIX_SECS);

    let outcome = replay_one(ws.path(), &entry);
    assert_eq!(outcome, ReplayOutcome::Modified);
    assert_eq!(
        mtime_of(&path).unix_seconds(),
        FRESH_UNIX_SECS,
        "a content mismatch must never leave the stale recorded mtime on disk"
    );
}

#[test]
fn unchanged_source_gets_recorded_mtime() {
    let ws = tempfile::tempdir().expect("tempdir");
    let (path, entry) = recorded_main_rs(ws.path(), MAIN_RS);

    // Simulate a checkout stamping a fresh mtime over identical content.
    set_mtime_secs(&path, FRESH_UNIX_SECS);

    let outcome = replay_one(ws.path(), &entry);
    assert_eq!(outcome, ReplayOutcome::Applied);
    let restored = mtime_of(&path);
    assert_eq!(restored.unix_seconds(), OLD_MTIME_MS / 1000);
    assert_eq!(restored.nanoseconds(), 0);
}

#[test]
fn resized_source_is_size_mismatch_and_keeps_fresh_mtime() {
    let ws = tempfile::tempdir().expect("tempdir");
    let (path, entry) = recorded_main_rs(ws.path(), MAIN_RS);

    std::fs::write(&path, b"fn main() { /* grew */ }\n").expect("grow main.rs");
    set_mtime_secs(&path, FRESH_UNIX_SECS);

    let outcome = replay_one(ws.path(), &entry);
    assert_eq!(outcome, ReplayOutcome::SizeMismatch);
    assert_eq!(mtime_of(&path).unix_seconds(), FRESH_UNIX_SECS);
}

#[test]
fn missing_source_is_missing() {
    let ws = tempfile::tempdir().expect("tempdir");
    let entry = SourceFile {
        path: "src/absent.rs".to_string(),
        mtime_ms: OLD_MTIME_MS,
        mtime_ns: None,
        size: 0,
        blake3: Vec::new(),
    };

    assert_eq!(replay_one(ws.path(), &entry), ReplayOutcome::Missing);
}

#[test]
fn malformed_stored_hash_never_applies() {
    let ws = tempfile::tempdir().expect("tempdir");
    let (path, mut entry) = recorded_main_rs(ws.path(), MAIN_RS);
    set_mtime_secs(&path, FRESH_UNIX_SECS);
    // A malformed (too-short) stored hash: 16 bytes instead of 32.
    entry.blake3 = vec![0u8; 16];

    // Matching size and content, but the malformed hash can never match,
    // so it must fall through to Modified, never Applied.
    assert_eq!(replay_one(ws.path(), &entry), ReplayOutcome::Modified);
    assert_eq!(mtime_of(&path).unix_seconds(), FRESH_UNIX_SECS);

    // The missing-file check is still evaluated first.
    std::fs::remove_file(&path).expect("remove main.rs");
    assert_eq!(replay_one(ws.path(), &entry), ReplayOutcome::Missing);
}

#[test]
fn escaping_manifest_path_is_never_stamped() {
    let root = tempfile::tempdir().expect("tempdir");
    let ws = root.path().join("ws");
    std::fs::create_dir_all(&ws).expect("mkdir ws");
    let outside = root.path().join("outside.rs");
    std::fs::write(&outside, MAIN_RS).expect("write outside.rs");
    set_mtime_secs(&outside, FRESH_UNIX_SECS);
    let entry = SourceFile {
        path: "../outside.rs".to_string(),
        mtime_ms: OLD_MTIME_MS,
        mtime_ns: None,
        size: MAIN_RS.len() as u64,
        blake3: hash_file(&outside).expect("hash").to_vec(),
    };

    assert_eq!(replay_one(&ws, &entry), ReplayOutcome::Missing);
    assert_eq!(mtime_of(&outside).unix_seconds(), FRESH_UNIX_SECS);
}

#[test]
fn adapter_clamps_negative_mtime_and_hex_encodes_hash() {
    let ws = tempfile::tempdir().expect("tempdir");
    let path = ws.path().join("file.txt");
    std::fs::write(&path, b"hello\n").expect("write file.txt");
    let blake3 = hash_file(&path).expect("hash").to_vec();

    let negative = SourceFile {
        path: "file.txt".to_string(),
        mtime_ms: -5,
        mtime_ns: None,
        size: 6,
        blake3: blake3.clone(),
    };
    assert_eq!(source_file_to_mtime_entry(&negative).mtime_ns, 0);

    let positive = SourceFile {
        path: "file.txt".to_string(),
        mtime_ms: 1234,
        mtime_ns: None,
        size: 6,
        blake3,
    };
    let adapted = source_file_to_mtime_entry(&positive);
    assert_eq!(adapted.mtime_ns, 1_234_000_000);
    assert_eq!(adapted.path, "file.txt");
    assert_eq!(adapted.size, 6);
    let expected_hex = zccache::hash::hash_file(&path).expect("hash").to_hex();
    assert_eq!(adapted.blake3, expected_hex);
}

#[test]
fn verbatim_drive_root_is_stripped_for_replay() {
    assert_eq!(
        replay_workspace_root(Path::new(r"\\?\C:\ws")),
        PathBuf::from(r"C:\ws")
    );
    assert_eq!(
        replay_workspace_root(Path::new(r"\\?\UNC\server\share\ws")),
        PathBuf::from(r"\\server\share\ws")
    );

    let plain = PathBuf::from("/ws/plain-path");
    assert_eq!(replay_workspace_root(&plain), plain);
}

#[test]
fn nanosecond_field_takes_precedence_including_epoch() {
    let entry = SourceFile {
        mtime_ms: 1234,
        mtime_ns: Some(1_234_567_800),
        ..SourceFile::default()
    };
    assert_eq!(source_file_to_mtime_entry(&entry).mtime_ns, 1_234_567_800);
    let epoch = SourceFile {
        mtime_ns: Some(0),
        ..entry.clone()
    };
    assert_eq!(source_file_to_mtime_entry(&epoch).mtime_ns, 0);
    let negative = SourceFile {
        mtime_ns: Some(-1),
        ..entry
    };
    assert_eq!(source_file_to_mtime_entry(&negative).mtime_ns, 0);
}

#[test]
fn legacy_wire_timestamp_still_decodes_and_replays() {
    // Field 2 (varint 1234), emitted by pre-#3851 SourceFile messages.
    let legacy = SourceFile::decode(&[0x10, 0xd2, 0x09][..]).unwrap();
    assert_eq!(legacy.mtime_ns, None);
    assert_eq!(source_file_to_mtime_entry(&legacy).mtime_ns, 1_234_000_000);
    let excessive = SourceFile {
        mtime_ms: i64::MAX,
        ..legacy
    };
    assert_eq!(source_file_to_mtime_entry(&excessive).mtime_ns, i64::MAX);
}

#[test]
fn changed_source_with_same_size_and_exact_mtime_is_never_replayed() {
    let ws = tempfile::tempdir().unwrap();
    let (path, mut entry) = recorded_main_rs(ws.path(), MAIN_RS);
    entry.mtime_ns = Some(OLD_MTIME_MS * 1_000_000 + 251_336_800);
    std::fs::write(&path, b"fn xain() {}\n").unwrap();
    filetime::set_file_mtime(
        &path,
        filetime::FileTime::from_system_time(ns_to_systime(entry.mtime_ns.unwrap())),
    )
    .unwrap();
    let before = mtime_of(&path);
    assert_eq!(replay_one(ws.path(), &entry), ReplayOutcome::Modified);
    assert_eq!(mtime_of(&path), before);
}

#[test]
fn filesystem_precision_diagnostic_names_requested_and_actual_timestamps() {
    assert!(source_mtime_precision_warning("src/main.rs", 123, 123).is_none());
    let warning =
        source_mtime_precision_warning("src/main.rs", 1_234_567_800, 1_234_000_000).unwrap();
    for detail in [
        "filesystem timestamp precision limitation",
        "src/main.rs",
        "1234567800",
        "1234000000",
        "Cargo may rebuild",
    ] {
        assert!(warning.contains(detail), "{warning}");
    }
}
