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
    };
    let offer = Offer {
        job: Some(job),
        speculative: vec!["abc".into()],
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
    let mut memo = DigestMemo::default();
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
fn rust_runtime_environment_dependencies_decline_before_execution() {
    let compiler = soldr_dist::compiler::Compiler {
        kind: "rustc".into(),
        identity: "test".into(),
        program: "soldr".into(),
        prefix: vec!["rustc".into()],
        toolchain: None,
    };
    let job = DistJob {
        env: HashMap::from([("RUSTUP_TOOLCHAIN".into(), "1.98.1".into())]),
        ..DistJob::default()
    };
    assert!(compiler
        .environment_decline(&job)
        .unwrap()
        .contains("RUSTUP_TOOLCHAIN"));
}

#[test]
fn custom_input_path_environment_declines_instead_of_reading_host_files() {
    let compiler = soldr_dist::compiler::Compiler {
        kind: "rustc".into(),
        identity: "test".into(),
        program: "soldr".into(),
        prefix: vec!["rustc".into()],
        toolchain: None,
    };
    let job = DistJob {
        env: HashMap::from([("DATA_FILE".into(), "/repo/build/../data.bin".into())]),
        inputs: vec![InputFile {
            abs_path: "/repo/data.bin".into(),
            ..InputFile::default()
        }],
        ..DistJob::default()
    };
    assert!(compiler
        .environment_decline(&job)
        .unwrap()
        .contains("DATA_FILE"));
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
    let relay = "https://relay.example.com";
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
