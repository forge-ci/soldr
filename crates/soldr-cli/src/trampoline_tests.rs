//! Unit tests for [`crate::trampoline`]: the cargo-run arg parser, the
//! sidecar (de)serializer, layout resolution, and the dep-info parser.
//! Lives in a sibling file referenced via `#[path]` so `trampoline.rs`
//! stays comfortably under the 1000-LOC ceiling (post-#339 convention).

use super::*;

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn strips_no_trampoline_flag() {
    let (cleaned, saw) = strip_no_trampoline_flag(&argv(&[
        "run",
        "--no-trampoline",
        "--release",
        "--",
        "--no-trampoline",
    ]));
    assert!(saw);
    assert_eq!(
        cleaned,
        argv(&["run", "--release", "--", "--no-trampoline"])
    );
}

#[test]
fn resolves_manifest_default_run_with_multiple_binaries() {
    let directory = tempfile::tempdir().expect("tempdir");
    let manifest = directory.path().join("Cargo.toml");
    std::fs::write(
        &manifest,
        r#"[package]
name = "project"
version = "0.1.0"
default-run = "app"

[[bin]]
name = "tool"

[[bin]]
name = "app"
"#,
    )
    .expect("write manifest");
    let mut parsed = parse_run_args(&argv(&["run"])).expect("parsed");
    parsed.manifest_path = Some(manifest);
    assert_eq!(resolve_bin_name(&parsed).unwrap().as_deref(), Some("app"));

    parsed.bin = Some("tool".into());
    assert_eq!(resolve_bin_name(&parsed).unwrap().as_deref(), Some("tool"));
}

#[test]
fn parses_basic_run() {
    let parsed = parse_run_args(&argv(&["run", "--bin", "demo"])).expect("parsed");
    assert_eq!(parsed.bin.as_deref(), Some("demo"));
    assert!(!parsed.release);
    assert!(parsed.trailing.is_empty());
}

#[test]
fn parses_release_and_features() {
    let parsed = parse_run_args(&argv(&[
        "run",
        "--release",
        "--bin=demo",
        "--features",
        "a,b",
        "-F",
        "c",
        "--",
        "arg1",
        "arg2",
    ]))
    .expect("parsed");
    assert!(parsed.release);
    assert_eq!(parsed.bin.as_deref(), Some("demo"));
    assert_eq!(parsed.features, vec!["a", "b", "c"]);
    assert_eq!(
        parsed.trailing,
        vec!["arg1".to_string(), "arg2".to_string()]
    );
}

#[test]
fn parses_toolchain_prefix() {
    let parsed = parse_run_args(&argv(&["+nightly", "run", "--bin", "demo"])).expect("parsed");
    assert_eq!(parsed.toolchain.as_deref(), Some("nightly"));
    assert_eq!(parsed.bin.as_deref(), Some("demo"));
}

#[test]
fn example_falls_through() {
    assert!(parse_run_args(&argv(&["run", "--example", "demo"])).is_none());
}

#[test]
fn unknown_flag_falls_through() {
    assert!(parse_run_args(&argv(&["run", "--frobnicate"])).is_none());
}

#[test]
fn non_run_subcommand_falls_through() {
    assert!(parse_run_args(&argv(&["build"])).is_none());
    assert!(parse_run_args(&argv(&["test"])).is_none());
}

#[test]
fn run_short_alias_is_recognized() {
    let parsed = parse_run_args(&argv(&["r", "--bin", "demo"])).expect("parsed");
    assert_eq!(parsed.bin.as_deref(), Some("demo"));
}

#[test]
fn split_features_handles_commas_and_spaces() {
    assert_eq!(split_features("a,b c"), vec!["a", "b", "c"]);
    assert_eq!(split_features("  "), Vec::<String>::new());
}

#[test]
fn dep_info_parses_simple_stanza() {
    let text = "/tmp/target/debug/foo: /tmp/src/main.rs /tmp/src/lib.rs\n";
    let sources =
        parse_dep_info_for_output(text, Path::new("/tmp/target/debug/foo")).expect("parsed");
    let strs: Vec<String> = sources
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    assert!(strs.contains(&"/tmp/src/main.rs".to_string()));
    assert!(strs.contains(&"/tmp/src/lib.rs".to_string()));
}

#[test]
fn dep_info_picks_correct_stanza_among_many() {
    let text = "\
/tmp/target/debug/foo.rlib: /tmp/src/lib.rs
/tmp/target/debug/foo: /tmp/src/main.rs
";
    let sources =
        parse_dep_info_for_output(text, Path::new("/tmp/target/debug/foo")).expect("parsed");
    let strs: Vec<String> = sources
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    assert_eq!(strs, vec!["/tmp/src/main.rs".to_string()]);
}

#[test]
fn dep_info_honors_escaped_spaces() {
    let text = "/tmp/target/debug/foo: /tmp/path\\ with\\ spaces/main.rs\n";
    let sources =
        parse_dep_info_for_output(text, Path::new("/tmp/target/debug/foo")).expect("parsed");
    let strs: Vec<String> = sources
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    assert_eq!(strs, vec!["/tmp/path with spaces/main.rs".to_string()]);
}

#[test]
fn dep_info_joins_line_continuations() {
    let text = "/tmp/target/debug/foo: /tmp/a.rs \\\n /tmp/b.rs \\\n /tmp/c.rs\n";
    let sources =
        parse_dep_info_for_output(text, Path::new("/tmp/target/debug/foo")).expect("parsed");
    let strs: Vec<String> = sources
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    assert!(strs.contains(&"/tmp/a.rs".to_string()));
    assert!(strs.contains(&"/tmp/b.rs".to_string()));
    assert!(strs.contains(&"/tmp/c.rs".to_string()));
}

#[test]
fn dep_info_handles_windows_drive_letters() {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let text = "C:\\target\\debug\\foo.exe: C:\\src\\main.rs C:\\src\\lib.rs\n";
    let sources =
        parse_dep_info_for_output(text, Path::new("C:\\target\\debug\\foo.exe")).expect("parsed");
    let strs: Vec<String> = sources
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    assert!(strs.iter().any(|s| s.ends_with("main.rs")));
    assert!(strs.iter().any(|s| s.ends_with("lib.rs")));
}

#[test]
fn sidecar_roundtrips_through_toml() {
    let original = Sidecar {
        binary_path: "target/debug/foo".to_string(),
        binary_mtime_nanos: 12345,
        binary_size_bytes: 9999,
        binary_hash: "blake3:cafef00d".to_string(),
        cargo_args_fingerprint: "blake3:deadbeef".to_string(),
        source_files: vec![SidecarSource {
            path: "src/main.rs".to_string(),
            mtime_nanos: 678,
            size_bytes: 4096,
            content_hash: "blake3:facefeed".to_string(),
        }],
    };
    let text = toml::to_string(&original).expect("serialize");
    let round: Sidecar = toml::from_str(&text).expect("deserialize");
    assert_eq!(original, round);
}

#[test]
fn write_sidecar_writes_atomically() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sub").join("foo.toml");
    let data = Sidecar {
        binary_path: "x".into(),
        binary_mtime_nanos: 1,
        binary_size_bytes: 2,
        binary_hash: "blake3:0".into(),
        cargo_args_fingerprint: "blake3:0".into(),
        source_files: vec![],
    };
    write_sidecar_atomic(&path, &data).expect("write");
    assert!(path.is_file());
    let text = fs::read_to_string(&path).expect("read");
    assert!(text.contains("blake3:0"));
    // Ensure the temp file is not lingering.
    let tmp = path.with_file_name("foo.toml.tmp");
    assert!(!tmp.exists());
}

#[test]
fn compute_layout_uses_release_directory() {
    let parsed = ParsedRunArgs {
        toolchain: None,
        bin: Some("foo".to_string()),
        release: true,
        profile: None,
        manifest_path: Some(PathBuf::from("/tmp/proj/Cargo.toml")),
        target: None,
        features: vec![],
        all_features: false,
        no_default_features: false,
        target_dir: None,
        trailing: vec![],
    };
    let layout = compute_layout(&parsed, "foo");
    assert!(layout.binary_path.to_string_lossy().contains("release"));
    assert!(layout
        .sidecar_path
        .to_string_lossy()
        .contains(".soldr-trampoline"));
}

#[test]
fn compute_layout_with_target_triple() {
    let parsed = ParsedRunArgs {
        toolchain: None,
        bin: Some("foo".to_string()),
        release: false,
        profile: None,
        manifest_path: Some(PathBuf::from("/tmp/proj/Cargo.toml")),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        features: vec![],
        all_features: false,
        no_default_features: false,
        target_dir: None,
        trailing: vec![],
    };
    let layout = compute_layout(&parsed, "foo");
    let bin_str = layout.binary_path.to_string_lossy();
    assert!(bin_str.contains("x86_64-unknown-linux-gnu"));
    assert!(bin_str.contains("debug"));
}

#[test]
fn compute_layout_custom_profile_maps_dev_to_debug() {
    let parsed = ParsedRunArgs {
        toolchain: None,
        bin: Some("foo".to_string()),
        release: false,
        profile: Some("dev".to_string()),
        manifest_path: Some(PathBuf::from("/tmp/proj/Cargo.toml")),
        target: None,
        features: vec![],
        all_features: false,
        no_default_features: false,
        target_dir: None,
        trailing: vec![],
    };
    let layout = compute_layout(&parsed, "foo");
    assert!(layout.binary_path.to_string_lossy().contains("debug"));
}

#[test]
fn trailing_user_args_extracts_after_separator() {
    assert_eq!(
        trailing_user_args(&argv(&["run", "--bin", "demo", "--", "foo", "bar"])),
        vec!["foo".to_string(), "bar".to_string()]
    );
    assert!(trailing_user_args(&argv(&["run", "--bin", "demo"])).is_empty());
}

// -------------------------------------------------------------------------
// Content-hash oracle (issue #342). The fast-skip path uses mtime+size, the
// slow check uses blake3 content hashes for binary AND every source file.
// Mtime spoofing, tar-with-mtime-epoch restore, and same-second edits all
// stay correct because content hash is never load-bearing-on-mtime.
// -------------------------------------------------------------------------

#[test]
fn compute_file_hash_stable_for_same_content() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = tmp.path().join("a.txt");
    let b = tmp.path().join("b.txt");
    fs::write(&a, b"identical content").unwrap();
    fs::write(&b, b"identical content").unwrap();
    let ha = compute_file_hash(&a).unwrap();
    let hb = compute_file_hash(&b).unwrap();
    assert_eq!(ha, hb);
    assert!(ha.starts_with("blake3:"));
}

#[test]
fn compute_file_hash_changes_when_content_changes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let f = tmp.path().join("f.txt");
    fs::write(&f, b"before").unwrap();
    let h1 = compute_file_hash(&f).unwrap();
    fs::write(&f, b"after").unwrap();
    let h2 = compute_file_hash(&f).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn sidecar_legacy_empty_hash_round_trip_deserialises() {
    // A pre-#342 sidecar (no binary_hash / content_hash fields) must
    // deserialize cleanly via `#[serde(default)]`.
    let legacy = r#"binary_path = "target/debug/foo"
binary_mtime_nanos = 12345
binary_size_bytes = 9999
cargo_args_fingerprint = "blake3:abc"

[[source_files]]
path = "src/main.rs"
mtime_nanos = 678
size_bytes = 4096
"#;
    let parsed: Sidecar = toml::from_str(legacy).expect("legacy sidecar must parse");
    assert!(
        parsed.binary_hash.is_empty(),
        "missing field defaults to empty string"
    );
    assert_eq!(parsed.source_files.len(), 1);
    assert!(parsed.source_files[0].content_hash.is_empty());
}

#[test]
fn self_heal_updates_only_drifted_mtime_size_not_hashes() {
    // Construct a sidecar with a known hash and drifted mtime+size.
    // self_heal_sidecar should rewrite mtime+size but leave hash
    // unchanged so the next invocation's fast-skip path matches.
    let tmp = tempfile::tempdir().expect("tempdir");
    let sidecar_path = tmp.path().join("foo.toml");
    let original = Sidecar {
        binary_path: "target/debug/foo".into(),
        binary_mtime_nanos: 100,
        binary_size_bytes: 200,
        binary_hash: "blake3:aaaaaaaa".into(),
        cargo_args_fingerprint: "blake3:bb".into(),
        source_files: vec![SidecarSource {
            path: "src/main.rs".into(),
            mtime_nanos: 300,
            size_bytes: 400,
            content_hash: "blake3:cccccccc".into(),
        }],
    };
    write_sidecar_atomic(&sidecar_path, &original).unwrap();

    let refresh = &[RefreshedSource {
        idx: 0,
        mtime_nanos: 999,
        size_bytes: 888,
    }];
    self_heal_sidecar(&sidecar_path, &original, Some((777, 666)), refresh).unwrap();

    let reread: Sidecar = toml::from_str(&fs::read_to_string(&sidecar_path).unwrap()).unwrap();
    assert_eq!(reread.binary_mtime_nanos, 777);
    assert_eq!(reread.binary_size_bytes, 666);
    assert_eq!(
        reread.binary_hash, "blake3:aaaaaaaa",
        "binary_hash must NOT change during self-heal"
    );
    assert_eq!(reread.source_files[0].mtime_nanos, 999);
    assert_eq!(reread.source_files[0].size_bytes, 888);
    assert_eq!(
        reread.source_files[0].content_hash, "blake3:cccccccc",
        "content_hash must NOT change during self-heal"
    );
}

fn probe_rustc_identity(
    stdout: &str,
    stderr: &str,
    code: i32,
) -> (Result<String, SoldrError>, String, String) {
    let temp = tempfile::tempdir().unwrap();
    let rustc =
        crate::core::tool_output::write_fake_tool(temp.path(), "rustc", stdout, stderr, code);
    let log = temp.path().join("small-tools.jsonl");
    let mut forwarded = Vec::new();
    let result = rustc_identity_with_sinks(
        &rustc,
        crate::core::tool_output::ToolSinks {
            stderr: &mut forwarded,
            log_path: Some(log.clone()),
        },
    );
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    (result, String::from_utf8(forwarded).unwrap(), logged)
}

/// soldr#3383: a failing `rustc -vV` names rustc's own stderr.
#[test]
fn rustc_identity_failure_carries_stderr() {
    let (result, _, logged) = probe_rustc_identity("", "MARKER_VV_3383", 1);
    let err = result.unwrap_err().to_string();
    assert!(err.contains("rustc -vV"), "{err}");
    assert!(err.contains("MARKER_VV_3383"), "{err}");
    assert!(logged.contains("MARKER_VV_3383"), "{logged}");
}

/// soldr#3389 contract: success still forwards and logs stderr.
#[test]
fn rustc_identity_success_forwards_and_logs_stderr() {
    let (result, forwarded, logged) = probe_rustc_identity("rustc 9.9.9", "MARKER_VV_OK", 0);
    assert_eq!(result.unwrap(), "rustc 9.9.9");
    assert!(forwarded.contains("rustc -vV: MARKER_VV_OK"), "{forwarded}");
    assert!(logged.contains("MARKER_VV_OK"), "{logged}");
}

#[test]
fn trampoline_env_flags_use_shared_parser() {
    for v in ["ON", " true ", "TRUE", "on", "Yes", "1"] {
        assert!(crate::core::flag_value(v), "{v:?}");
        assert!(trampoline_flag_value(Some(v)), "{v:?}");
    }
    for v in ["0", "false", "off", "", "nope"] {
        assert!(!crate::core::flag_value(v), "{v:?}");
        assert!(!trampoline_flag_value(Some(v)), "{v:?}");
    }
    assert!(!trampoline_flag_value(None));
}
