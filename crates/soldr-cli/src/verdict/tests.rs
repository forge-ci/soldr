use std::collections::BTreeMap;
use std::path::Path;

use super::context::Context;
use super::gate::strip_cargo_status;
use super::key::{
    digest_components, is_exact_channel, normalize_args, parse_ls_files, parse_status, Dirty,
};
use super::profiles::{self, match_invocation, Match};
use super::record::{Record, MAX_OUTPUT_BYTES, SCHEMA};
use super::store::{Seen, Store};
use super::Mode;

fn argv(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

fn matched(text: &str) -> Option<String> {
    let profiles = profiles::builtin();
    match match_invocation(&profiles, &argv(text)) {
        Match::Check(profile) => Some(profile.name.clone()),
        _ => None,
    }
}

#[test]
fn builtin_adapters_claim_their_pure_invocations() {
    assert_eq!(
        matched("clippy --workspace -- -D warnings").as_deref(),
        Some("clippy")
    );
    assert_eq!(matched("+nightly clippy").as_deref(), Some("clippy"));
    assert_eq!(matched("fmt --all -- --check").as_deref(), Some("fmt"));
    assert_eq!(matched("dylint --all").as_deref(), Some("dylint"));
    assert_eq!(
        matched("test --doc --workspace").as_deref(),
        Some("doctest")
    );
    assert_eq!(matched("deny check bans licenses").as_deref(), Some("deny"));
    assert_eq!(matched("machete").as_deref(), Some("machete"));
}

#[test]
fn rewriting_or_networked_invocations_are_never_checks() {
    for text in [
        "clippy --fix",
        "fmt --all",
        "dylint --fix",
        "test --workspace",
        "deny check",
        "deny check bans advisories",
        "machete --fix",
        "audit",
        "build",
    ] {
        assert_eq!(matched(text), None, "{text}");
    }
}

#[test]
fn impure_adapter_is_recognised_with_a_reason() {
    let profiles = profiles::builtin();
    match match_invocation(&profiles, &argv("audit")) {
        Match::NotPure(profile, reason) => {
            assert_eq!(profile.name, "audit");
            assert!(reason.contains("impure"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn workspace_rows_add_and_override_adapters() {
    let config = profiles::parse_workspace_config(
        r#"
[workspace]
members = []
[workspace.metadata.soldr.verdict]
exclude = ["docs/**"]
[workspace.metadata.soldr.verdict.tools.hack]
subcommand = "hack"
requires = ["check"]
identity = ["cargo hack --version"]
[workspace.metadata.soldr.verdict.tools.clippy]
inputs = ["lints.toml"]
"#,
    );
    assert_eq!(config.exclude, vec!["docs/**".to_string()]);
    let effective = profiles::effective(&config);
    let hack = effective
        .iter()
        .find(|p| p.name == "hack")
        .expect("hack row");
    assert_eq!(hack.requires, vec!["check".to_string()]);
    let clippy = effective
        .iter()
        .find(|p| p.name == "clippy")
        .expect("clippy row");
    assert_eq!(clippy.inputs, vec!["lints.toml".to_string()]);
    assert!(
        clippy.forbids.contains(&"--fix".to_string()),
        "unset fields keep the built-in"
    );
    assert_eq!(effective.iter().filter(|p| p.name == "clippy").count(), 1);
}

#[test]
fn normalization_drops_presentation_flags_only() {
    assert_eq!(
        normalize_args(&argv(
            "clippy --color always -j8 --jobs=4 -q --workspace --target-dir t -- -D warnings -v"
        )),
        argv("clippy --workspace -- -D warnings -v"),
    );
    assert_eq!(
        normalize_args(&argv("clippy --color=never --workspace")),
        normalize_args(&argv("clippy --workspace")),
    );
}

#[test]
fn git_index_and_status_parse() {
    let index = b"100644 aaaa 0\tsrc/lib.rs\x00100644 bbbb 0\tCargo.toml\x00";
    let tracked = parse_ls_files(index);
    assert_eq!(tracked.get("src/lib.rs").map(String::as_str), Some("aaaa"));
    assert_eq!(tracked.len(), 2);
    let status = parse_status(b" M src/lib.rs\0?? new.rs\0 D gone.rs\0");
    assert_eq!(status.get("src/lib.rs"), Some(&Dirty::Modified));
    assert_eq!(status.get("new.rs"), Some(&Dirty::Untracked));
    assert_eq!(status.get("gone.rs"), Some(&Dirty::Deleted));
}

#[test]
fn only_exact_channels_count_as_pins() {
    for exact in [
        "1.89.0",
        "nightly-2026-05-28",
        "beta-2026-01-02",
        "1.89.0-x86_64-unknown-linux-gnu",
    ] {
        assert!(is_exact_channel(exact), "{exact}");
    }
    for floating in ["stable", "nightly", "beta", "1.89", ""] {
        assert!(!is_exact_channel(floating), "{floating}");
    }
}

#[test]
fn digest_is_deterministic_and_sensitive() {
    let mut components = BTreeMap::new();
    components.insert("files".to_string(), "abc".to_string());
    let first = digest_components("clippy", "ws", &components);
    assert_eq!(first.len(), 32);
    assert_eq!(first, digest_components("clippy", "ws", &components));
    assert_ne!(first, digest_components("fmt", "ws", &components));
    assert_ne!(first, digest_components("clippy", "crate:x", &components));
    components.insert("files".to_string(), "abd".to_string());
    assert_ne!(first, digest_components("clippy", "ws", &components));
}

#[test]
fn record_validation_rejects_mismatches() {
    let record = Record {
        v: SCHEMA,
        key: "k".into(),
        tool: "clippy".into(),
        argv: vec![],
        cwd: ".".into(),
        toolchain: "pin:1.89.0".into(),
        soldr: "0".into(),
        exit: 0,
        created: 0,
        output: None,
        output_captured: false,
    };
    let text = serde_json::to_string(&record).unwrap();
    assert!(Record::parse_for(&text, "k").is_some());
    assert!(Record::parse_for(&text, "other").is_none());
    let failed = serde_json::to_string(&Record {
        exit: 1,
        ..record.clone()
    })
    .unwrap();
    assert!(Record::parse_for(&failed, "k").is_none());
    let big = "x".repeat(MAX_OUTPUT_BYTES + 1);
    assert!(!record.clone().with_output(Some(&big)).output_captured);
    let kept = record.with_output(Some("warning: x\n"));
    assert!(kept.output_captured);
    assert_eq!(kept.output.as_deref(), Some("warning: x\n"));
}

#[test]
fn replay_output_drops_cargo_progress() {
    let raw = "    Checking foo v0.1.0\nwarning: unused\n    Finished `dev` profile\n";
    assert_eq!(strip_cargo_status(raw), "warning: unused\n");
}

#[test]
fn modes_parse() {
    assert_eq!(Mode::parse(None), Mode::On);
    assert_eq!(Mode::parse(Some("off")), Mode::Off);
    assert_eq!(Mode::parse(Some("READ")), Mode::Read);
}

#[test]
fn store_round_trips_and_gc_keeps_newest() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::at(dir.path());
    for i in 0..3 {
        let record = Record {
            v: SCHEMA,
            key: format!("k{i}"),
            tool: "clippy".into(),
            argv: vec![],
            cwd: ".".into(),
            toolchain: String::new(),
            soldr: String::new(),
            exit: 0,
            created: i,
            output: None,
            output_captured: false,
        };
        store.put(&record).unwrap();
        let file = store.records_dir().join(format!("k{i}.json"));
        let at = filetime::FileTime::from_unix_time(1_000 + i as i64, 0);
        filetime::set_file_mtime(file, at).unwrap();
    }
    assert!(store.lookup("k1").is_some());
    assert!(store.spool_dir().join("k1.json").is_file());
    assert_eq!(store.gc(2), 1);
    assert!(store.lookup("k0").is_none(), "oldest dropped");
    assert!(store.lookup("k2").is_some());
    store
        .note_seen(&Seen {
            tool: "clippy".into(),
            args: argv("clippy"),
            cwd: ".".into(),
            key: "k2".into(),
            hit: false,
        })
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(store.seen_path())
            .unwrap()
            .lines()
            .count(),
        1
    );
}

fn git(dir: &Path, args: &[&str]) {
    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t");
    let status = crate::core::command_output_with_timeout_duration(
        &mut command,
        "test git",
        std::time::Duration::from_secs(60),
    )
    .expect("git");
    assert!(status.status.success(), "git {args:?}");
}

fn fixture_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.89.0\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("docs/notes.md"), "notes\n").unwrap();
    git(root, &["init", "-q"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "init"]);
    dir
}

fn key_in(root: &Path, args: &str) -> Result<String, String> {
    Context::discover(root)
        .and_then(|ctx| ctx.derive(&argv(args), root))
        .map(|d| d.key)
}

#[test]
fn key_tracks_sources_and_ignores_the_unrelated() {
    let repo = fixture_repo();
    let root = repo.path();
    let clean = key_in(root, "clippy --workspace").expect("key");
    assert!(clean.starts_with("soldr-verdict-v1-clippy-ws-"), "{clean}");
    assert_eq!(
        clean,
        key_in(root, "clippy --workspace --color always").unwrap()
    );
    assert_ne!(
        clean,
        key_in(root, "clippy --workspace -- -D warnings").unwrap()
    );

    // An uncommitted edit is hashed by content and changes the key...
    std::fs::write(root.join("src/lib.rs"), "pub fn g() {}\n").unwrap();
    let edited = key_in(root, "clippy --workspace").unwrap();
    assert_ne!(clean, edited);
    // ...and committing the same content keeps the same verdict-relevant
    // bytes but moves them from content hash to blob id.
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    assert_eq!(clean, key_in(root, "clippy --workspace").unwrap());

    // A file outside every package root and input glob does not matter
    // unless the repo root itself is a package (it is here).
    std::fs::write(root.join("docs/notes.md"), "changed\n").unwrap();
    assert_ne!(
        clean,
        key_in(root, "clippy --workspace").unwrap(),
        "root package: docs are inputs"
    );
}

#[test]
fn virtual_workspace_ignores_files_outside_packages() {
    let repo = fixture_repo();
    let root = repo.path();
    std::fs::create_dir_all(root.join("crates/fx")).unwrap();
    std::fs::rename(root.join("src"), root.join("crates/fx/src")).unwrap();
    std::fs::write(
        root.join("crates/fx/Cargo.toml"),
        "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/fx\"]\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "virtual"]);
    let before = key_in(root, "fmt --all -- --check").unwrap();
    std::fs::write(root.join("docs/notes.md"), "changed\n").unwrap();
    assert_eq!(before, key_in(root, "fmt --all -- --check").unwrap());
    std::fs::write(root.join("crates/fx/README.md"), "doc include\n").unwrap();
    assert_ne!(
        before,
        key_in(root, "fmt --all -- --check").unwrap(),
        "package files count"
    );
}

#[test]
fn unaccountable_inputs_disable_the_skip() {
    let repo = fixture_repo();
    let root = repo.path();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fx\"\nversion = \"0.1.0\"\n[dependencies]\nother = { path = \"../other\" }\n",
    )
    .unwrap();
    let reason = key_in(root, "clippy").unwrap_err();
    assert!(reason.contains("outside the repo"), "{reason}");

    let plain = tempfile::tempdir().unwrap();
    assert!(
        key_in(plain.path(), "clippy").is_err(),
        "not a git checkout"
    );
}

#[test]
fn disabled_workspace_never_skips() {
    let repo = fixture_repo();
    let root = repo.path();
    let mut manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    manifest.push_str("[package.metadata.soldr.verdict]\nenabled = false\n");
    std::fs::write(root.join("Cargo.toml"), manifest).unwrap();
    let reason = key_in(root, "clippy").unwrap_err();
    assert!(reason.contains("disabled"), "{reason}");
}

mod per_crate {
    use std::collections::BTreeMap;

    use super::super::crates::{merkle, narrow, owner, parse_metadata, selects_workspace, Member};
    use super::super::key::{derive_crates, KeyRequest, Snapshot};
    use super::super::profiles;
    use super::{argv, fixture_repo, git};

    fn member(name: &str, dir: &str, deps: &[&str]) -> Member {
        Member {
            name: name.into(),
            dir: dir.into(),
            deps: deps.iter().map(|d| (*d).to_string()).collect(),
        }
    }

    #[test]
    fn workspace_selection_is_detected_and_narrowed() {
        assert!(selects_workspace(&argv("fmt --all -- --check")));
        assert!(selects_workspace(&argv("clippy --workspace")));
        assert!(!selects_workspace(&argv("fmt -p a -- --check")));
        assert!(!selects_workspace(&argv("clippy --workspace --exclude b")));
        assert!(
            !selects_workspace(&argv("fmt -- --check --all")),
            "after -- is the tool's argv"
        );
        assert_eq!(
            narrow(&argv("fmt --all -- --check"), &["a".into(), "c".into()]),
            argv("fmt -p a -p c -- --check"),
        );
    }

    #[test]
    fn files_belong_to_the_deepest_member() {
        let members = [member("root", "", &[]), member("a", "crates/a", &[])];
        assert_eq!(owner(&members, "crates/a/src/lib.rs").unwrap().name, "a");
        assert_eq!(owner(&members, "src/main.rs").unwrap().name, "root");
        assert_eq!(owner(&[member("a", "crates/a", &[])], "docs/x.md"), None);
        assert_eq!(
            owner(&[member("a", "crates/a", &[])], "crates/ab/x.rs"),
            None
        );
    }

    #[test]
    fn merkle_keys_move_with_dependencies_only() {
        let members = [
            member("app", "app", &["lib"]),
            member("lib", "lib", &[]),
            member("other", "other", &[]),
        ];
        let seal =
            |name: &str, own: &str, deps: &[String]| format!("{name}:{own}:{}", deps.join("+"));
        let mut own: BTreeMap<String, String> = [("app", "1"), ("lib", "1"), ("other", "1")]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .into();
        let before = merkle(&members, &own, seal).unwrap();
        own.insert("lib".into(), "2".into());
        let after = merkle(&members, &own, seal).unwrap();
        assert_ne!(before["lib"], after["lib"]);
        assert_ne!(before["app"], after["app"], "dependent re-keys");
        assert_eq!(
            before["other"], after["other"],
            "unrelated member keeps its key"
        );
        let cyclic = [member("a", "a", &["b"]), member("b", "b", &["a"])];
        assert!(merkle(&cyclic, &BTreeMap::new(), seal).is_err());
    }

    #[test]
    fn metadata_parses_members_and_path_edges() {
        let repo = fixture_repo();
        let root = repo.path().canonicalize().unwrap();
        let json = format!(
            r#"{{"workspace_members":["app-id","lib-id"],"packages":[
              {{"id":"app-id","name":"app","manifest_path":"{r}/app/Cargo.toml",
                "dependencies":[{{"name":"lib","path":"{r}/lib"}},{{"name":"serde"}}]}},
              {{"id":"lib-id","name":"lib","manifest_path":"{r}/lib/Cargo.toml","dependencies":[]}},
              {{"id":"ext","name":"serde","manifest_path":"/elsewhere/Cargo.toml","dependencies":[]}}]}}"#,
            r = root.display()
        );
        let members = parse_metadata(&json, &root).unwrap();
        assert_eq!(
            members,
            vec![member("app", "app", &["lib"]), member("lib", "lib", &[])]
        );
    }

    #[test]
    fn crate_keys_isolate_edits_to_the_edited_member_and_its_dependents() {
        let repo = fixture_repo();
        let root = repo.path();
        for (name, deps) in [
            ("app", "lib = { path = \"../lib\" }\n"),
            ("lib", ""),
            ("other", ""),
        ] {
            std::fs::create_dir_all(root.join(name).join("src")).unwrap();
            std::fs::write(
                root.join(name).join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n[dependencies]\n{deps}"
                ),
            )
            .unwrap();
            std::fs::write(root.join(name).join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        }
        std::fs::remove_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\", \"lib\", \"other\"]\n",
        )
        .unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", "ws"]);
        let members = vec![
            member("app", "app", &["lib"]),
            member("lib", "lib", &[]),
            member("other", "other", &[]),
        ];
        let profile = profiles::builtin()
            .into_iter()
            .find(|p| p.name == "fmt")
            .unwrap();
        let config = profiles::WorkspaceConfig::default();
        let keys = |args: &str| {
            let args = argv(args);
            derive_crates(
                &KeyRequest {
                    profile: &profile,
                    config: &config,
                    args: &args,
                    cwd: root,
                },
                &Snapshot::discover(root).unwrap(),
                &members,
            )
            .unwrap()
        };
        let before = keys("fmt --all -- --check");
        assert_eq!(
            before,
            keys("fmt -- --check"),
            "selection flags do not enter crate keys"
        );
        std::fs::write(root.join("lib/src/lib.rs"), "pub fn g() {}\n").unwrap();
        let after = keys("fmt --all -- --check");
        assert_ne!(before["lib"], after["lib"]);
        assert_ne!(before["app"], after["app"]);
        assert_eq!(before["other"], after["other"]);
        assert!(
            after["other"].starts_with("soldr-verdict-v1-fmt-c-other-"),
            "{}",
            after["other"]
        );
    }

    #[test]
    fn only_fmt_narrows_by_default() {
        let narrowing: Vec<String> = profiles::builtin()
            .into_iter()
            .filter(|p| p.per_package)
            .map(|p| p.name)
            .collect();
        assert_eq!(narrowing, vec!["fmt".to_string()]);
    }
}
