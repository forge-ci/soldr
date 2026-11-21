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
        r#"{"gh_jobs_running":3,"gh_cpus_reserved":6,"cap":8,"updated_at":1000}"#,
    )
    .unwrap();
    assert_eq!(load.limit(4, 8, 1010.0), 2);
    assert_eq!(free_slots(load.limit(4, 8, 1010.0), 2), 0);
    assert_eq!(load.limit(4, 8, 1030.0), 2);
    assert_eq!(load.limit(4, 8, 1030.1), 1);
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
    let job = DistJob { cwd: "/repo".into(), outputs: vec!["/outputs/unit.o".into()], ..DistJob::default() };
    assert_eq!(sandbox::rewrite(std::path::Path::new("/tmp/job"), &job, "/outputs/subdir/../unit.o").unwrap(), "/tmp/job/root/outputs/unit.o");
}
