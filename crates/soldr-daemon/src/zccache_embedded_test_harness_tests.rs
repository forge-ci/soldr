//! Soldr admits rustc `--test` harnesses to the embedded cache
//! ([`super::soldr_compile_options`]). zccache's own default refuses them
//! (zccache#1525), which left every `cargo test` link and every Clippy/Dylint
//! check of a test target recompiling on each run.

use super::*;

fn test_daemon_identity() -> DaemonProcess {
    use running_process::broker::protocol::Endpoint;
    DaemonProcess::current_process(
        Endpoint {
            namespace_id: "soldr-zccache-harness-test".to_string(),
            path: "soldr-zccache-harness-test.sock".to_string(),
        },
        None,
    )
    .expect("current test process identity")
}

#[test]
fn soldr_admits_test_harnesses_explicitly() {
    // Explicit, so neither environment's `ZCCACHE_CACHE_TEST_BINS` can
    // restore zccache's refusal.
    assert_eq!(
        soldr_compile_options().test_harness_admission,
        Some(TestHarnessAdmission::All)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_test_harness_compiles_hit_the_cache() {
    let rustc = crate::test_support::rustc_from_env_or_path();
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("workspace");
    std::fs::create_dir_all(project.join("src")).expect("create source directory");
    std::fs::write(
        project.join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n#[test]\nfn answers() { assert_eq!(answer(), 42); }\n",
    )
    .expect("write source");

    // A client that asks zccache to refuse harnesses must not win.
    let mut compile_env: Vec<(String, String)> = std::env::vars().collect();
    compile_env.push(("ZCCACHE_CACHE_TEST_BINS".into(), "0".into()));

    // The two `--test` shapes cargo produces: the harness link (`cargo test`)
    // and the metadata-only check of a test target (`cargo clippy
    // --all-targets`, Dylint).
    let shapes: [(&str, &str); 2] = [
        ("link", "--emit=dep-info,link"),
        ("check", "--emit=dep-info,metadata"),
    ];
    let paths = SoldrPaths::with_root(temp.path().join("root"));
    let service = SoldrZccacheService::start(&paths, &test_daemon_identity())
        .await
        .expect("start embedded service");
    for (label, emit) in shapes {
        std::fs::create_dir_all(project.join(format!("target/{label}/deps")))
            .expect("create output directory");
        let request = || CompileRequest {
            args: vec![
                rustc.display().to_string(),
                "--edition".into(),
                "2021".into(),
                "--crate-name".into(),
                format!("harness_{label}"),
                emit.into(),
                "--test".into(),
                "-C".into(),
                format!("metadata=harness{label}"),
                "-C".into(),
                format!("extra-filename=-harness{label}"),
                "--out-dir".into(),
                format!("target/{label}/deps"),
                "src/lib.rs".into(),
            ],
            cwd: project.display().to_string(),
            env: compile_env.clone(),
            stdin: Vec::new(),
            lifecycle: None,
            ipc_busy_retries: 0,
        };
        let first = service.compile(request()).await.expect("cold compile");
        assert_eq!(
            first.exit_code,
            0,
            "{label}: cold rustc failed: {}",
            String::from_utf8_lossy(&first.stderr)
        );
        assert!(!first.cached, "{label}: first compile populates the cache");
        let second = service.compile(request()).await.expect("warm compile");
        assert_eq!(second.exit_code, 0, "{label}: warm compile failed");
        assert!(
            second.cached,
            "{label}: an identical `--test` compile must be served from the cache \
             (cache_outcome={})",
            second.cache_outcome
        );
    }
    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown service");
}
