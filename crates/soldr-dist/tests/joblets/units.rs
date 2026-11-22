use prost::Message;
use soldr_dist::{
    cas::{Cas, DigestMemo},
    proto::*,
    sandbox,
    trust::{node_key, Trust},
    DistJob,
};
use std::{collections::HashMap, fs};

#[test]
fn proto_roundtrip() {
    let job = DistJob {
        compiler_kind: "rustc".into(),
        compiler_identity: "commit host".into(),
        argv: vec!["a.rs".into()],
        env: HashMap::from([("KEY".into(), "value".into())]),
        cwd: "/work".into(),
        stdin: b"stdin".to_vec(),
        inputs: vec![InputFile {
            abs_path: "/work/a.rs".into(),
            blake3: "abc".into(),
            size: 10,
            executable: true,
        }],
        outputs: vec!["/work/a.o".into()],
        rewrite_env: vec![],
    };
    let offer = Offer {
        job: Some(job),
        speculative: vec!["abc".into()],
        job_token: 7,
    };
    assert_eq!(
        offer,
        Offer::decode(offer.encode_to_vec().as_slice()).unwrap()
    );
    let reply = Reply {
        done: Some(Done {
            exit_code: 1,
            stdout: vec![0, 255],
            stderr: b"error".to_vec(),
            dependency_info: b"a.o: a.rs\n".to_vec(),
            outputs: vec![OutputFile {
                abs_path: "/work/a.o".into(),
                blake3: "def".into(),
                size: 5,
                mode: 0o755,
            }],
        }),
        ..Reply::default()
    };
    assert_eq!(
        reply,
        Reply::decode(reply.encode_to_vec().as_slice()).unwrap()
    );
    let hello = Hello {
        version: 999,
        toolchains: vec!["identity".into()],
    };
    assert_eq!(
        hello,
        Hello::decode(hello.encode_to_vec().as_slice()).unwrap()
    );
}
#[test]
fn cas_put_verify_restart_evict() {
    let temp = tempfile::tempdir().unwrap();
    let cas = Cas::new(temp.path(), 4).unwrap();
    let first = cas.put(b"1234").unwrap();
    assert_eq!(cas.get(&first).unwrap(), b"1234");
    assert_eq!(
        cas.path(&first).unwrap(),
        temp.path().join("cas").join(&first[..2]).join(&first)
    );
    let wrong = blake3::hash(b"abcd").to_hex().to_string();
    assert!(cas.receive(&wrong, 4, &b"abce"[..]).is_err());
    assert!(cas.receive(&wrong, 3, &b"abcd"[..]).is_err());
    assert!(!cas.contains(&wrong).unwrap());
    assert!(cas.path("../../bad").is_err());
    let second = cas.put(b"abcd").unwrap();
    // Set deterministic access order; no wall-clock sleeps.
    filetime::set_file_atime(
        cas.path(&first).unwrap(),
        filetime::FileTime::from_unix_time(0, 0),
    )
    .unwrap();
    let restarted = Cas::new(temp.path(), 4).unwrap();
    restarted.evict().unwrap();
    assert!(!restarted.contains(&first).unwrap());
    assert_eq!(restarted.get(&second).unwrap(), b"abcd");
    fs::write(cas.path(&second).unwrap(), b"evil").unwrap();
    assert!(cas.get(&second).is_err());
}
#[test]
fn materialisation_and_digest_memo() {
    if !posix_paths() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let cas = Cas::new(temp.path(), u64::MAX).unwrap();
    let source = temp.path().join("source.rs");
    fs::write(&source, b"source").unwrap();
    let memo = DigestMemo::default();
    let input = memo.input(&source).unwrap();
    assert_eq!(memo.input(&source).unwrap(), input);
    assert_eq!(cas.put(b"source").unwrap(), input.blake3);
    let sandbox = temp.path().join("sandbox");
    let job = DistJob {
        cwd: temp.path().to_str().unwrap().into(),
        inputs: vec![input.clone()],
        outputs: vec![temp.path().join("output.o").to_str().unwrap().into()],
        ..DistJob::default()
    };
    sandbox::materialise(&sandbox, &job, &cas).unwrap();
    assert_eq!(
        fs::read(sandbox::mirrored(&sandbox, &input.abs_path).unwrap()).unwrap(),
        b"source"
    );
    assert!(sandbox::mirrored(&sandbox, "/a/../../escape").is_err());
    assert!(sandbox::mirrored(&sandbox, "relative").is_err());
    fs::write(&source, b"changed").unwrap();
    assert_ne!(memo.input(&source).unwrap().blake3, input.blake3);
}
#[test]
fn trust_and_key_persistence() {
    let temp = tempfile::tempdir().unwrap();
    let key = node_key(temp.path()).unwrap();
    assert_eq!(key.public(), node_key(temp.path()).unwrap().public());
    let path = temp.path().join("trusted_clients.toml");
    assert!(!Trust::load(&path).unwrap().contains(key.public()));
    Trust::add(&path, key.public()).unwrap();
    Trust::add(&path, key.public()).unwrap();
    let trust = Trust::load(&path).unwrap();
    assert!(trust.contains(key.public()));
    assert_eq!(trust.peers.len(), 1);
    assert!(Trust::parse("peers = [\"invalid\"]").is_err());
    assert!(Trust::parse("unexpected = true").is_err());
    assert_eq!(
        soldr_platform::fs::permissions::mode(&temp.path().join("node.key")).unwrap_or(0o600)
            & 0o777,
        0o600
    );
}

#[test]
fn remaps_keep_zccache_workspace_convention() {
    if !posix_paths() {
        return;
    }
    let job = DistJob {
        cwd: "/repo/crate".into(),
        ..DistJob::default()
    };
    let root = std::path::Path::new("/tmp/job");
    assert_eq!(
        sandbox::rewrite(root, &job, "--remap-path-prefix=/repo=.").unwrap(),
        "--remap-path-prefix=/tmp/job/root/repo=."
    );
    assert_eq!(
        sandbox::rewrite(root, &job, "/repo/crate/src/lib.rs").unwrap(),
        "/tmp/job/root/repo/crate/src/lib.rs"
    );
}

#[test]
fn equivalent_manifest_paths_cannot_corrupt_cas() {
    if !posix_paths() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let cas = Cas::new(temp.path(), u64::MAX).unwrap();
    let a = cas.put(b"first").unwrap();
    let b = cas.put(b"second").unwrap();
    let job = DistJob {
        cwd: "/repo".into(),
        inputs: vec![
            InputFile {
                abs_path: "/repo/a".into(),
                blake3: a.clone(),
                size: 5,
                executable: false,
            },
            InputFile {
                abs_path: "/repo//a".into(),
                blake3: b.clone(),
                size: 6,
                executable: false,
            },
        ],
        ..DistJob::default()
    };
    assert!(sandbox::materialise(&temp.path().join("job"), &job, &cas).is_err());
    assert_eq!(cas.get(&a).unwrap(), b"first");
    assert_eq!(cas.get(&b).unwrap(), b"second");
}

#[test]
fn lexical_output_paths_and_arg_component_boundaries() {
    if !posix_paths() {
        return;
    }
    let job = DistJob {
        cwd: "/work/proj".into(),
        ..DistJob::default()
    };
    let root = std::path::Path::new("/tmp/job");
    assert_eq!(
        sandbox::absolute(
            std::path::Path::new(&job.cwd),
            std::path::Path::new("../target/unit.o")
        )
        .unwrap(),
        std::path::Path::new("/work/target/unit.o")
    );
    assert_eq!(
        sandbox::rewrite(root, &job, "/work/project-config").unwrap(),
        "/work/project-config"
    );
    assert_eq!(
        sandbox::rewrite(root, &job, "--cfg=literal=\"/work/proj\"").unwrap(),
        "--cfg=literal=\"/work/proj\""
    );
}

#[test]
fn eviction_preserves_active_job_and_releases_on_drop() {
    let temp = tempfile::tempdir().unwrap();
    let cas = Cas::new(temp.path(), 0).unwrap();
    let digest = cas.put(b"active").unwrap();
    let lease = cas.lease([digest.clone()]);
    let unused = cas.put(b"unused").unwrap();
    cas.evict().unwrap();
    assert!(cas.contains(&digest).unwrap());
    assert!(!cas.contains(&unused).unwrap());
    drop(lease);
    cas.evict().unwrap();
    assert_eq!(cas.size().unwrap(), 0);
}

#[test]
fn digest_manifest_retains_symlink_alias() {
    if !posix_paths() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("actual.rs"), b"source").unwrap();
    let alias = temp.path().join("alias.rs");
    if let Err(e) = soldr_platform::fs::links::create("actual.rs", &alias, false) {
        eprintln!("SKIP: host cannot create symlinks: {e}");
        return;
    }
    let input = DigestMemo::default().input(&alias).unwrap();
    assert_eq!(input.abs_path, alias.to_str().unwrap());
    let cas = Cas::new(temp.path(), u64::MAX).unwrap();
    cas.put(b"source").unwrap();
    let job = DistJob {
        cwd: temp.path().to_str().unwrap().into(),
        inputs: vec![input.clone()],
        ..DistJob::default()
    };
    let root = temp.path().join("sandbox");
    sandbox::materialise(&root, &job, &cas).unwrap();
    assert_eq!(
        fs::read(sandbox::mirrored(&root, &input.abs_path).unwrap()).unwrap(),
        b"source"
    );
}

#[test]
fn host_load_reservation_staleness_and_running_slots() {
    use soldr_dist::host_load::{free_slots, HostLoad};
    let mut load: HostLoad = serde_json::from_str(
        r#"{"gh_jobs_running":3,"gh_cpus_reserved":6,"cap":8,"updated_at":1000.25}"#,
    )
    .unwrap();
    assert_eq!(load.limit(4, 8, 1010.0), 2);
    assert_eq!(free_slots(load.limit(4, 8, 1010.0), 2), 0);
    assert_eq!(load.limit(4, 8, 1030.25), 2);
    assert_eq!(load.limit(4, 8, 1030.26), 1);
    load.gh_cpus_reserved = 100;
    assert_eq!(load.limit(4, 8, 1010.0), 0);
    assert_eq!(free_slots(load.limit(4, 8, 1040.0), 1), 0);
    assert_eq!(load.limit(0, 8, 1040.0), 0);
    assert_eq!(load.limit(4, 8, 900.0), 1);
    let temp = tempfile::tempdir().unwrap();
    assert_eq!(
        soldr_dist::host_load::limit(Some(&temp.path().join("missing.json")), 8, 8),
        1
    );
}

#[test]
fn c_search_paths_and_existing_remaps_are_relocated() {
    if !posix_paths() {
        return;
    }
    let job = DistJob {
        cwd: "/repo".into(),
        inputs: vec![InputFile {
            abs_path: "/outside/headers/nested/a.h".into(),
            ..InputFile::default()
        }],
        ..DistJob::default()
    };
    let root = std::path::Path::new("/tmp/job");
    assert_eq!(
        sandbox::rewrite(root, &job, "-iquote/outside/headers").unwrap(),
        "-iquote/tmp/job/root/outside/headers"
    );
    assert_eq!(
        sandbox::rewrite(root, &job, "-ffile-prefix-map=/repo=.").unwrap(),
        "-ffile-prefix-map=/tmp/job/root/repo=."
    );
}

#[test]
fn absolute_output_parent_segments_are_normalised_before_matching() {
    if !posix_paths() {
        return;
    }
    let job = DistJob {
        cwd: "/repo".into(),
        outputs: vec!["/outputs/unit.o".into()],
        ..DistJob::default()
    };
    assert_eq!(
        sandbox::rewrite(
            std::path::Path::new("/tmp/job"),
            &job,
            "/outputs/subdir/../unit.o"
        )
        .unwrap(),
        "/tmp/job/root/outputs/unit.o"
    );
}

fn posix_paths() -> bool {
    let supported = std::path::Path::new("/").is_absolute();
    if !supported {
        eprintln!("SKIP: dist job materialisation currently supports POSIX paths only");
    }
    supported
}

#[test]
fn rust_runtime_environment_reads_are_checked_before_execution() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("lib.rs");
    std::fs::write(
        &source,
        "pub const HOME: &str = env!(\"RUSTUP_TOOLCHAIN\");",
    )
    .unwrap();
    let mut job = DistJob {
        env: HashMap::from([("RUSTUP_TOOLCHAIN".into(), "1.98.1".into())]),
        inputs: vec![soldr_dist::cas::DigestMemo::default()
            .input(&source)
            .unwrap()],
        ..Default::default()
    };
    assert!(soldr_dist::rust_inputs::validate_environment(&mut job)
        .unwrap_err()
        .to_string()
        .contains("RUSTUP_TOOLCHAIN"));
}

#[test]
fn aliased_option_env_keeps_the_original_execution_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("lib.rs");
    std::fs::write(
        &source,
        "use std::option_env as opt; pub const V: Option<&str> = opt!(\"CUSTOM\");",
    )
    .unwrap();
    let mut job = DistJob {
        cwd: temp.path().to_str().unwrap().into(),
        env: HashMap::from([("CUSTOM".into(), "present".into())]),
        inputs: vec![soldr_dist::cas::DigestMemo::default()
            .input(&source)
            .unwrap()],
        ..Default::default()
    };
    soldr_dist::rust_inputs::validate_environment(&mut job).unwrap();
    assert_eq!(job.env.get("CUSTOM").map(String::as_str), Some("present"));
    assert!(soldr_dist::dep_info::validate(&job, "a.o: lib.rs\n# env-dep:CUSTOM\n").is_err());
    assert!(
        soldr_dist::dep_info::validate(&job, "a.o: lib.rs\n# env-dep:CUSTOM=present\n").is_ok()
    );
}

#[test]
fn endpoint_identity_has_one_process_owner_until_lease_drops() {
    let temp = tempfile::tempdir().unwrap();
    let lease = soldr_dist::transport::EndpointLease::acquire(temp.path()).unwrap();
    assert!(soldr_dist::transport::EndpointLease::acquire(temp.path()).is_err());
    drop(lease);
    assert!(soldr_dist::transport::EndpointLease::acquire(temp.path()).is_ok());
}

#[test]
fn service_cli_accepts_signal_and_status_root() {
    use clap::Parser;
    use soldr_dist::cli::{DistArgs, DistCommand};
    #[derive(Parser)]
    struct Command {
        #[command(flatten)]
        args: DistArgs,
    }
    let serve = Command::try_parse_from([
        "dist",
        "serve",
        "--root",
        "/var/lib/soldr-dist/state",
        "--slots",
        "8",
        "--cas-budget",
        "10000",
        "--relay-url",
        "https://relay.example.com",
        "--key",
        "/var/lib/soldr/host.key",
        "--signal-url",
        "https://signal.example.com",
        "--host-load-file",
        "/run/forge/host-load.json",
    ])
    .unwrap()
    .args;
    assert_eq!(
        serve.root.unwrap(),
        std::path::Path::new("/var/lib/soldr-dist/state")
    );
    let DistCommand::Serve {
        signal_url,
        slots,
        key,
        ..
    } = serve.command
    else {
        panic!("expected serve");
    };
    assert_eq!(signal_url, "https://signal.example.com");
    assert_eq!(
        key.unwrap(),
        std::path::Path::new("/var/lib/soldr/host.key")
    );
    assert_eq!(slots, 8);
    let status = Command::try_parse_from([
        "dist",
        "status",
        "--json",
        "--root",
        "/var/lib/soldr-dist/state",
    ])
    .unwrap()
    .args;
    assert_eq!(
        status.root.unwrap(),
        std::path::Path::new("/var/lib/soldr-dist/state")
    );
    assert!(matches!(status.command, DistCommand::Status { json: true }));
}

#[test]
fn explicit_root_status_preserves_daemon_build_identity() {
    let root = tempfile::tempdir().unwrap();
    let expected = Status {
        node_id: "serving-node".into(),
        role: "dist".into(),
        pid: u64::from(std::process::id()),
        build_id: "a".repeat(64),
        signal_url: "https://signal.example.com".into(),
        ..Status::default()
    };
    fs::write(root.path().join("status.pb"), expected.encode_to_vec()).unwrap();
    let loaded = soldr_dist::status::load(root.path()).unwrap();
    let json = soldr_dist::status::json(&loaded);
    assert_eq!(json["build_id"], expected.build_id);
    assert_eq!(json["signal_url"], expected.signal_url);
    assert_eq!(json["pid"], std::process::id());
    // A privileged status query must not replace the daemon's identity/key.
    assert!(!root.path().join("node.key").exists());
    let actual = soldr_dist::status::build_id().unwrap();
    assert_eq!(actual.len(), 64);
    assert!(actual
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    assert_ne!(loaded.build_id, actual);
    use sha2::Digest;
    let executable = fs::read(std::env::current_exe().unwrap()).unwrap();
    assert_eq!(actual, format!("{:x}", sha2::Sha256::digest(executable)));
}

#[test]
fn inactive_status_cannot_confirm_a_deployed_node() {
    let root = tempfile::tempdir().unwrap();
    let missing = soldr_dist::status::load(root.path()).unwrap();
    assert!(missing.build_id.is_empty());
    for pid in [0, u64::from(u32::MAX)] {
        let stale = Status {
            pid,
            role: "dist".into(),
            build_id: "a".repeat(64),
            running_joblets: 1,
            ..Status::default()
        };
        fs::write(root.path().join("status.pb"), stale.encode_to_vec()).unwrap();
        let loaded = soldr_dist::status::load(root.path()).unwrap();
        assert_eq!(loaded.pid, 0);
        assert_eq!(loaded.running_joblets, 0);
        assert!(loaded.build_id.is_empty());
    }
}

#[test]
fn explicit_host_key_preserves_read_only_seed_and_endpoint_identity() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("host.key");
    let expected = iroh::SecretKey::generate();
    fs::write(&path, expected.to_bytes()).unwrap();
    soldr_platform::fs::permissions::restore_mode(&path, Some(0o400)).unwrap();
    let mode = soldr_platform::fs::permissions::mode(&path);
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let actual = soldr_dist::trust::load_or_create_key(&path).unwrap();
    assert_eq!(actual.public(), expected.public());
    assert_eq!(fs::read(&path).unwrap(), expected.to_bytes());
    assert_eq!(soldr_platform::fs::permissions::mode(&path), mode);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let endpoint = soldr_dist::transport::endpoint_with_identity(actual, true, None)
            .await
            .unwrap();
        assert_eq!(endpoint.id(), expected.public());
        endpoint.close().await;
    });
    assert!(!root.path().join("node.key").exists());
}

#[test]
fn explicit_key_creation_is_atomic_private_and_invalid_keys_are_not_replaced() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("shared/host.key");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                soldr_dist::trust::load_or_create_key(&path)
                    .unwrap()
                    .public()
            })
        })
        .collect();
    let keys: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(keys.iter().all(|id| *id == keys[0]));
    assert_eq!(fs::read(&path).unwrap().len(), 32);
    if let Some(mode) = soldr_platform::fs::permissions::mode(&path) {
        assert_eq!(mode & 0o777, 0o600);
    }
    let invalid = root.path().join("invalid.key");
    fs::write(&invalid, b"invalid seed").unwrap();
    let error = soldr_dist::trust::load_or_create_key(&invalid)
        .unwrap_err()
        .to_string();
    assert!(error.contains("32-byte"));
    assert!(error.contains("invalid.key"));
    assert_eq!(fs::read(invalid).unwrap(), b"invalid seed");
}

#[test]
fn shared_key_lease_excludes_endpoints_with_different_roots() {
    use soldr_dist::transport::EndpointLease;
    let parent = tempfile::tempdir().unwrap();
    let key = parent.path().join("host.key");
    soldr_dist::trust::load_or_create_key(&key).unwrap();
    soldr_platform::fs::permissions::restore_mode(&key, Some(0o400)).unwrap();
    let mode = soldr_platform::fs::permissions::mode(&key);
    let root_a = parent.path().join("a");
    let root_b = parent.path().join("b");
    let first = EndpointLease::acquire_with_key(&root_a, &key).unwrap();
    assert!(EndpointLease::acquire_with_key(&root_b, &key).is_err());
    // Signing/read access for the phone-home agent stays available.
    soldr_dist::trust::load_or_create_key(&key).unwrap();
    drop(first);
    let second = EndpointLease::acquire_with_key(&root_b, &key).unwrap();
    assert_eq!(soldr_platform::fs::permissions::mode(&key), mode);
    drop(second);
}

#[test]
fn static_peer_accepts_ticket_or_node_id_with_explicit_relay() {
    use soldr_dist::transport::{parse_peer, ticket};
    let id = iroh::SecretKey::generate().public();
    let relay = "https://signal.forge-ci.com";
    let expected = iroh::EndpointAddr::new(id).with_relay_url(relay.parse().unwrap());
    assert_eq!(parse_peer(&id.to_string(), Some(relay)).unwrap(), expected);
    assert_eq!(
        parse_peer(&ticket(&expected).unwrap(), None).unwrap(),
        expected
    );
    assert!(parse_peer(&id.to_string(), None)
        .unwrap_err()
        .to_string()
        .contains("relay"));
    assert!(parse_peer(&id.to_string(), Some("http://relay.example.com")).is_err());
    assert!(parse_peer("not-a-peer", Some(relay)).is_err());
}

#[test]
fn worker_alias_is_the_same_native_serve_command() {
    use clap::{CommandFactory, Parser};
    use soldr_dist::cli::{DistArgs, DistCommand};
    #[derive(Parser)]
    struct Command {
        #[command(flatten)]
        args: DistArgs,
    }
    let parsed = Command::try_parse_from(["dist", "worker", "--slots", "4"]).unwrap();
    assert!(matches!(
        parsed.args.command,
        DistCommand::Serve { slots: 4, .. }
    ));
    // Exercise the exact production env binding without changing process-global env.
    let mut command = Command::command();
    let exec = command.find_subcommand_mut("exec").unwrap();
    let worker = exec
        .get_arguments()
        .find(|arg| arg.get_id() == "worker")
        .unwrap();
    assert_eq!(
        worker.get_env().unwrap(),
        std::ffi::OsStr::new("SOLDR_DIST_PEERS")
    );
}

#[test]
fn direct_lan_cli_and_ticket_need_no_relay() {
    use clap::Parser;
    #[derive(Parser)]
    struct Command {
        #[command(flatten)]
        args: soldr_dist::cli::DistArgs,
    }
    let args = Command::try_parse_from(["dist", "serve", "--relay", "none"])
        .unwrap()
        .args;
    assert_eq!(args.relay.as_deref(), Some("none"));
    let root = tempfile::tempdir().unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let endpoint = soldr_dist::transport::endpoint(root.path(), false)
            .await
            .unwrap();
        let addr = endpoint.addr();
        assert!(addr.relay_urls().next().is_none());
        assert!(addr.ip_addrs().next().is_some());
        assert!(addr.ip_addrs().all(|a| !a.ip().is_unspecified()));
        let ticket = soldr_dist::transport::ticket(&addr).unwrap();
        assert_eq!(
            soldr_dist::transport::parse_peer(&ticket, None).unwrap(),
            addr
        );
        endpoint.close().await;
    });
}

#[test]
fn assigned_rust_emit_outputs_are_mirrored_in_both_argument_forms() {
    use std::path::Path;
    let mut job = soldr_dist::DistJob {
        compiler_kind: "rustc".into(),
        cwd: "/repo".into(),
        outputs: vec!["/staging/lib.rlib".into(), "/repo/lib.d".into()],
        argv: vec!["--emit=link=/staging/lib.rlib,dep-info=lib.d".into()],
        ..Default::default()
    };
    let expected = "link=/job/root/staging/lib.rlib,dep-info=/job/root/repo/lib.d";
    assert_eq!(
        soldr_dist::sandbox::argv(Path::new("/job"), &job).unwrap(),
        [format!("--emit={expected}")]
    );
    job.argv = vec![
        "--emit".into(),
        "link=/staging/lib.rlib,dep-info=lib.d".into(),
    ];
    assert_eq!(
        soldr_dist::sandbox::argv(Path::new("/job"), &job).unwrap(),
        ["--emit", expected]
    );
    job.argv = vec!["--emit=link=/unlisted/file".into()];
    assert!(soldr_dist::sandbox::argv(Path::new("/job"), &job).is_err());
    job.argv = vec!["--emit=link=-".into()];
    assert!(soldr_dist::sandbox::argv(Path::new("/job"), &job).is_err());
}

#[test]
fn forge_relay_privacy_and_reverse_peers_are_enforced() {
    use soldr_dist::transport::{parse_worker, relay_config, relay_mode, DEFAULT_RELAY_URL};
    assert!(matches!(
        relay_mode(None).unwrap(),
        iroh::RelayMode::Disabled
    ));
    assert_eq!(
        relay_config(None, None).unwrap().as_deref(),
        Some(DEFAULT_RELAY_URL)
    );
    for rejected in [
        "default",
        "staging",
        "https://use1-1.relay.n0.iroh-canary.iroh.link",
        "https://iroh.link",
        "https://iroh.network",
        "https://n0.computer",
        "http://signal.forge-ci.com",
    ] {
        assert!(relay_mode(Some(rejected)).is_err(), "{rejected}");
    }
    let source = include_str!("../../src/transport.rs");
    for variant in ["Default", "Staging"] {
        assert!(!source.contains(&format!("RelayMode::{variant}")));
    }
    let id = iroh::SecretKey::generate().public();
    assert_eq!(parse_worker(&id.to_string(), None, true).unwrap().id, id);
    assert!(parse_worker(&id.to_string(), None, false).is_err());
    let forbidden =
        iroh::EndpointAddr::new(id).with_relay_url("https://iroh.link".parse().unwrap());
    assert!(soldr_dist::transport::parse_ticket(
        &soldr_dist::transport::ticket(&forbidden).unwrap()
    )
    .is_err());
    #[derive(clap::Parser)]
    struct Command {
        #[command(flatten)]
        args: soldr_dist::cli::DistArgs,
    }
    use clap::Parser;
    assert!(Command::try_parse_from(["dist", "serve", "--relay", "default"]).is_err());
    let args = Command::try_parse_from(["dist", "serve", "--relay", DEFAULT_RELAY_URL])
        .unwrap()
        .args;
    assert_eq!(args.relay.as_deref(), Some(DEFAULT_RELAY_URL));
    assert!(Command::try_parse_from(["dist", "serve", "--outbound-only"]).is_err());
}

#[test]
fn job_metrics_round_trip_and_status_report_the_recorded_outcome() {
    use prost::Message;
    let root = tempfile::tempdir().unwrap();
    let job = soldr_dist::DistJob {
        inputs: ["a", "a", "b"]
            .into_iter()
            .map(|digest| soldr_dist::InputFile {
                blake3: digest.into(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let mut metrics = soldr_dist::JobMetrics::for_job("worker".into(), &job);
    assert_eq!(
        (
            metrics.input_files,
            metrics.input_digests,
            metrics.dedup_inputs
        ),
        (3, 2, 1)
    );
    metrics.outcome = "executed".into();
    metrics.path = "relay".into();
    metrics.rtt_known = true;
    metrics.rtt_micros = 500;
    metrics.cas_hits_known = true;
    metrics.cas_hits = 2;
    metrics.completed_unix_millis = 10;
    let decoded = soldr_dist::JobMetrics::decode(metrics.encode_to_vec().as_slice()).unwrap();
    assert_eq!(decoded, metrics);
    soldr_dist::status::record_job(root.path(), &metrics).unwrap();
    let mut older = metrics.clone();
    older.completed_unix_millis = 9;
    older.path = "direct".into();
    soldr_dist::status::record_job(root.path(), &older).unwrap();
    let status = soldr_dist::status::load(root.path()).unwrap();
    assert_eq!(status.last_job, Some(metrics.clone()));
    let json = soldr_dist::status::json(&status);
    assert_eq!(json["last_job"]["path"], "relay");
    assert_eq!(json["last_job"]["cas_hits"], 2);
    let outcome = soldr_dist::DistOutcome::Executed {
        exit_code: 0,
        stdout: vec![0, 255],
        stderr: vec![],
        metrics,
    };
    let json = serde_json::to_value(&outcome).unwrap();
    assert_eq!(json["outcome"], "executed");
    assert_eq!(json["stdout"], serde_json::json!([0, 255]));
}

#[test]
fn contended_metrics_writer_fails_without_waiting_for_the_lock() {
    use fs2::FileExt;
    let root = tempfile::tempdir().unwrap();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(root.path().join("metrics.lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    let metrics = soldr_dist::JobMetrics::default();
    assert!(soldr_dist::status::record_job(root.path(), &metrics).is_err());
    drop(lock);
    assert!(soldr_dist::status::record_job(root.path(), &metrics).is_ok());
}

#[test]
fn original_and_sandbox_remaps_both_cover_imported_dependency_paths() {
    let root = tempfile::tempdir().unwrap();
    let job = DistJob {
        compiler_kind: "rustc".into(),
        argv: vec![
            "--remap-path-prefix=/client=.".into(),
            "--remap-path-prefix".into(),
            "/dependency=/virtual".into(),
        ],
        ..Default::default()
    };
    let args = soldr_dist::sandbox::argv(root.path(), &job).unwrap();
    assert_eq!(args.len(), 4);
    assert_eq!(args[0], "--remap-path-prefix=/client=.");
    assert_eq!(args[2], "--remap-path-prefix=/dependency=/virtual");
    assert_eq!(
        args[1],
        format!(
            "--remap-path-prefix={}/root/client=.",
            root.path().display()
        )
    );
    assert_eq!(
        args[3],
        format!(
            "--remap-path-prefix={}/root/dependency=/virtual",
            root.path().display()
        )
    );
}

#[test]
fn path_mode_parses_auto_and_relay_and_rejects_the_rest() {
    use soldr_dist::transport::{path_mode, PathMode};
    assert_eq!(path_mode(None).unwrap(), PathMode::Auto);
    assert_eq!(path_mode(Some("")).unwrap(), PathMode::Auto);
    assert_eq!(path_mode(Some("auto")).unwrap(), PathMode::Auto);
    assert_eq!(path_mode(Some("relay")).unwrap(), PathMode::Relay);
    let error = path_mode(Some("direct")).unwrap_err().to_string();
    assert!(
        error.contains("SOLDR_DIST_PATH must be auto or relay"),
        "{error}"
    );
}

#[test]
fn relay_only_endpoint_binds_no_ip_socket_and_refuses_third_party_relays() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let key = node_key(temp.path()).unwrap();
        let endpoint =
            soldr_dist::transport::endpoint_relay_only(key.clone(), "https://signal.forge-ci.com")
                .await
                .unwrap();
        // No IP transport: the relay is the only path a peer can use.
        assert!(
            endpoint.bound_sockets().is_empty(),
            "{:?}",
            endpoint.bound_sockets()
        );
        endpoint.close().await;
        let refused =
            soldr_dist::transport::endpoint_relay_only(key, "https://relay.example.com").await;
        assert!(refused.is_err(), "a third-party relay must be refused");
    });
}

#[test]
fn job_log_appends_one_labelled_line_per_joblet() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("jobs.jsonl");
    let metrics = JobMetrics {
        path: "relay".into(),
        bytes_sent: 10,
        bytes_received: 20,
        outcome: "executed".into(),
        ..JobMetrics::default()
    };
    let argv: Vec<String> = ["rustc", "--crate-name", "syn", "--edition=2021"]
        .map(String::from)
        .to_vec();
    let unit = soldr_dist::status::unit_name(&argv);
    soldr_dist::status::append_job_line(&log, unit, &metrics).unwrap();
    soldr_dist::status::append_job_line(&log, "unknown", &JobMetrics::default()).unwrap();
    let text = fs::read_to_string(&log).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["unit"], "syn");
    assert_eq!(lines[0]["metrics"]["path"], "relay");
    assert_eq!(lines[0]["metrics"]["bytes_sent"], 10);
    assert_eq!(lines[0]["metrics"]["bytes_received"], 20);
    assert_eq!(lines[1]["unit"], "unknown");
}

#[test]
fn unit_name_reads_both_crate_name_spellings() {
    // Build-script probes pass `--crate-name=X`; they were logged as "unknown".
    let joined: Vec<String> = ["rustc", "--crate-name=anyhow_build_probe"]
        .map(String::from)
        .to_vec();
    assert_eq!(soldr_dist::status::unit_name(&joined), "anyhow_build_probe");
    let none: Vec<String> = ["rustc", "--crate-type", "lib"].map(String::from).to_vec();
    assert_eq!(soldr_dist::status::unit_name(&none), "unknown");
}
