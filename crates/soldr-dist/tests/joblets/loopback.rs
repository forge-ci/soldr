use running_process::AsyncProcessBuilder;
use soldr_dist::{
    cas::DigestMemo, compiler::Compiler, transport, trust::Trust, worker::Worker, DistClient,
    DistJob, DistOutcome, Result,
};
use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, sync::watch, task::JoinHandle};

struct Fixture {
    _temp: tempfile::TempDir,
    client: DistClient,
    endpoint: iroh::Endpoint,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<Result<()>>,
    uploaded: Arc<AtomicU64>,
}
impl Fixture {
    async fn new(compilers: Vec<Compiler>, trusted: bool) -> Self {
        Self::with_load(compilers, trusted, None).await
    }
    async fn with_load(
        compilers: Vec<Compiler>,
        trusted: bool,
        load: Option<serde_json::Value>,
    ) -> Self {
        Self::with_direction(compilers, trusted, load, false).await
    }
    async fn with_direction(
        compilers: Vec<Compiler>,
        trusted: bool,
        load: Option<serde_json::Value>,
        reverse: bool,
    ) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let worker_root = temp.path().join("worker");
        let client_root = temp.path().join("client");
        let endpoint = transport::endpoint(&worker_root, true).await.unwrap();
        let client_endpoint = transport::endpoint(&client_root, true).await.unwrap();
        Trust::add(&client_root.join("trusted_workers.toml"), endpoint.id()).unwrap();
        if trusted {
            Trust::add(
                &worker_root.join("trusted_clients.toml"),
                client_endpoint.id(),
            )
            .unwrap();
        }
        let client_ticket = client_endpoint.addr();
        let client = DistClient::new(client_endpoint, endpoint.addr(), &client_root)
            .unwrap()
            .with_reverse_only(reverse);
        let uploaded = Arc::new(AtomicU64::new(0));
        let host_load_file = load.map(|load| {
            let path = temp.path().join("host-load.json");
            fs::write(&path, serde_json::to_vec(&load).unwrap()).unwrap();
            path
        });
        let worker = Worker {
            endpoint: endpoint.clone(),
            root: worker_root.clone(),
            compilers,
            slots: 2,
            cas_budget: 1024 * 1024 * 32,
            host_load_file,
            signal_url: "https://signal.forge-ci.com".into(),
            total_cpus: 8,
            uploaded_blobs: uploaded.clone(),
            clients: if reverse { vec![client_ticket] } else { vec![] },
            outbound_only: reverse,
        };
        let (shutdown, rx) = watch::channel(false);
        let task = tokio::spawn(worker.serve(rx));
        Self {
            _temp: temp,
            client,
            endpoint,
            shutdown,
            task,
            uploaded,
        }
    }
    async fn stop(self) {
        self.shutdown.send(true).unwrap();
        self.task.await.unwrap().unwrap();
        self.client.endpoint.close().await;
    }
}
fn job(
    compiler: &Compiler,
    cwd: &Path,
    args: Vec<String>,
    source: &Path,
    output: &Path,
) -> DistJob {
    DistJob {
        compiler_kind: compiler.kind.clone(),
        compiler_identity: compiler.identity.clone(),
        argv: args,
        env: HashMap::new(),
        cwd: cwd.to_str().unwrap().into(),
        stdin: vec![],
        inputs: vec![DigestMemo::default().input(source).unwrap()],
        outputs: vec![output.to_str().unwrap().into()],
        rewrite_env: vec![],
    }
}
async fn compare(
    compiler: Compiler,
    source_name: &str,
    source: &str,
    args: &[&str],
    reverse: bool,
) {
    if !Path::new("/").is_absolute() {
        eprintln!("SKIP: dist job materialisation currently supports POSIX paths only");
        return;
    }
    let fixture = Fixture::with_direction(vec![compiler.clone()], true, None, reverse).await;
    let cwd = fixture._temp.path().join("project");
    fs::create_dir_all(&cwd).unwrap();
    let src = cwd.join(source_name);
    let out = cwd.join("hello.o");
    fs::write(&src, source).unwrap();
    let args: Vec<String> = args
        .iter()
        .map(|s| (*s).into())
        .chain([source_name.into(), "-o".into(), "hello.o".into()])
        .collect();
    let mut local = compiler
        .command()
        .args(&args)
        .current_dir(&cwd)
        .env("DIST_TAG", format!("{}/tag", cwd.display()));
    if let Some(toolchain) = &compiler.toolchain {
        local = local.env("RUSTUP_TOOLCHAIN", toolchain);
    }
    if compiler.kind == "rustc" {
        local = local.arg(format!("--remap-path-prefix={}=.", cwd.display()));
    }
    let local = local.capture().await.unwrap();
    assert!(
        local.status.success(),
        "{}",
        String::from_utf8_lossy(&local.stderr)
    );
    let expected = fs::read(&out).unwrap();
    fs::remove_file(&out).unwrap();
    let mut job = job(&compiler, &cwd, args, &src, &out);
    job.env
        .insert("DIST_TAG".into(), format!("{}/tag", cwd.display()));
    let outcome = fixture.client.execute(job.clone()).await;
    assert!(
        matches!(outcome, DistOutcome::Executed { exit_code: 0, .. }),
        "{outcome:?}"
    );
    assert_eq!(
        fs::read(&out).unwrap(),
        expected,
        "remote output differs from local"
    );
    let metrics = outcome.metrics();
    assert_eq!(metrics.worker_id, fixture.endpoint.id().to_string());
    assert_eq!(metrics.path, "direct");
    assert!(metrics.rtt_known);
    assert!(metrics.bytes_sent > 0 && metrics.bytes_received > 0);
    assert_eq!(metrics.uploaded_blobs, 1);
    assert!(metrics.cas_hits_known);
    assert_eq!(metrics.cas_hits, 0);
    let uploaded = fixture.client.uploaded_blobs.load(Ordering::Relaxed);
    assert_eq!(uploaded, 1);
    assert_eq!(fixture.uploaded.load(Ordering::Relaxed), 1);
    if reverse {
        assert_eq!(fixture.client.connections.outbound_connections(), 0);
        assert!(fixture.client.connections.incoming_connections() >= 1);
    }
    let second = fixture.client.execute(job).await;
    assert!(matches!(
        &second,
        DistOutcome::Executed { exit_code: 0, .. }
    ));
    assert_eq!(second.metrics().uploaded_blobs, 0);
    assert_eq!(second.metrics().need_digests, 0);
    assert!(second.metrics().cas_hits_known);
    assert_eq!(second.metrics().cas_hits, 1);
    let status = soldr_dist::status::load(&fixture._temp.path().join("client")).unwrap();
    assert_eq!(status.last_job.as_ref(), Some(second.metrics()));
    assert_eq!(
        fixture.client.uploaded_blobs.load(Ordering::Relaxed),
        uploaded,
        "second job uploaded inputs"
    );
    assert_eq!(fixture.uploaded.load(Ordering::Relaxed), 1);
    fixture.stop().await;
}
#[test]
fn rustc_byte_identical_and_zero_second_upload() {
    Runtime::new().unwrap().block_on(async {
        compare(
            Compiler::rustc().await.unwrap(),
            "hello.rs",
            "fn main() { println!(\"hello world {}\", env!(\"DIST_TAG\")); }\n",
            &["--emit=obj", "-g"],
            false,
        )
        .await;
    });
}
fn cc() -> Option<Compiler> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|p| p.join("cc"))
        .find(|p| p.is_file())
        .map(|p| Compiler::cc(&p).unwrap())
}
#[test]
fn c_byte_identical_and_zero_second_upload() {
    let Some(cc) = cc() else {
        eprintln!("SKIP: no host C compiler");
        return;
    };
    Runtime::new().unwrap().block_on(compare(
        cc,
        "hello.c",
        "int answer(void) { return 42; }\n",
        &["-c"],
        false,
    ));
}

#[test]
fn outbound_only_worker_compiles_without_client_dialling_it() {
    Runtime::new().unwrap().block_on(async {
        compare(
            Compiler::rustc().await.unwrap(),
            "hello.rs",
            "pub fn answer() -> u32 { 42 }\n",
            &["--crate-type=lib", "--emit=obj"],
            true,
        )
        .await;
    });
}
#[test]
fn toolchain_mismatch_declined_and_untrusted_rejected() {
    Runtime::new().unwrap().block_on(async {
        let fixture = Fixture::new(vec![], true).await;
        let job = DistJob {
            compiler_kind: "rustc".into(),
            compiler_identity: "wrong".into(),
            ..DistJob::default()
        };
        assert!(matches!(fixture.client.execute(job.clone()).await, DistOutcome::Declined { reason, .. } if reason == "toolchain mismatch"));
        fs::write(
            fixture._temp.path().join("client/trusted_workers.toml"),
            "peers = []\n",
        )
        .unwrap();
        assert!(matches!(fixture.client.execute(job.clone()).await, DistOutcome::Declined { reason, .. } if reason == "untrusted worker"));
        fixture.stop().await;
        let fixture = Fixture::new(vec![], false).await;
        assert!(matches!(fixture.client.execute(job).await, DistOutcome::Declined { reason, .. } if reason == "worker lost"));
        fixture.stop().await;
    });
}
#[test]
fn worker_loss_during_job_is_bounded() {
    let Some(shell) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join("sh"))
            .find(|p| p.is_file())
    }) else {
        eprintln!("SKIP: test requires a POSIX shell");
        return;
    };
    Runtime::new().unwrap().block_on(async {
        let marker_dir = tempfile::tempdir().unwrap();
        let marker = marker_dir.path().join("compiler-started");
        let compiler = Compiler {
            kind: "cc".into(),
            identity: "test-slow-compiler".into(),
            program: shell,
            prefix: vec![
                "-c".into(),
                "printf started > \"$1\"; sleep 60".into(),
                "dist-fixture".into(),
                marker.to_str().unwrap().into(),
            ],
            toolchain: None,
        };
        let fixture = Fixture::new(vec![compiler.clone()], true).await;
        let job = DistJob {
            compiler_kind: compiler.kind,
            compiler_identity: compiler.identity,
            cwd: "/dist-test".into(),
            ..DistJob::default()
        };
        let execute = fixture.client.execute(job);
        tokio::pin!(execute);
        let ready = async {
            let mut ticks = tokio::time::interval(Duration::from_millis(20));
            loop {
                ticks.tick().await;
                if marker.exists() {
                    break;
                }
            }
            fixture.endpoint.close().await;
        };
        let start = Instant::now();
        let (outcome, ()) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(execute, ready)
        })
        .await
        .unwrap();
        assert!(matches!(outcome, DistOutcome::Declined { reason, .. } if reason == "worker lost"));
        assert!(start.elapsed() < Duration::from_secs(10));
        fixture.task.abort();
        fixture.client.endpoint.close().await;
    });
}

#[test]
fn reserved_host_capacity_declines_joblets() {
    Runtime::new().unwrap().block_on(async {
        let compiler = Compiler {
            kind: "cc".into(),
            identity: "admission-test".into(),
            program: "/unused".into(),
            prefix: vec![],
            toolchain: None,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let fixture = Fixture::with_load(
            vec![compiler.clone()],
            true,
            Some(serde_json::json!({
                "gh_jobs_running": 2, "gh_cpus_reserved": 8, "cap": 8, "updated_at": now
            })),
        )
        .await;
        let job = DistJob {
            compiler_kind: compiler.kind,
            compiler_identity: compiler.identity,
            cwd: "/dist-test".into(),
            ..DistJob::default()
        };
        assert!(matches!(fixture.client.execute(job).await, DistOutcome::Declined { reason, .. } if reason == "worker busy"));
        fixture.stop().await;
    });
}

#[test]
fn joined_c_search_path_uses_cas_instead_of_host_headers() {
    if !Path::new("/").is_absolute() {
        eprintln!("SKIP: dist job materialisation currently supports POSIX paths only");
        return;
    }
    let Some(compiler) = cc() else {
        eprintln!("SKIP: no host C compiler");
        return;
    };
    Runtime::new().unwrap().block_on(async {
        let fixture = Fixture::new(vec![compiler.clone()], true).await;
        let cwd = fixture._temp.path().join("project");
        let headers = fixture._temp.path().join("headers/nested");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&headers).unwrap();
        let source = cwd.join("answer.c");
        let header = headers.join("answer.h");
        let output = cwd.join("answer.o");
        fs::write(&header, b"#define ANSWER 42\n").unwrap();
        fs::write(
            &source,
            format!(
                "#include \"nested/answer.h\"\nint answer(void) {{ return ANSWER; }}\n/*{}*/\n",
                "padding".repeat(10000)
            ),
        )
        .unwrap();
        let search = headers.parent().unwrap();
        let args = vec![
            "-c".into(),
            format!("-iquote{}", search.display()),
            "answer.c".into(),
            "-o".into(),
            "answer.o".into(),
        ];
        let local = AsyncProcessBuilder::new(&compiler.program)
            .args(&args)
            .current_dir(&cwd)
            .capture()
            .await
            .unwrap();
        assert!(
            local.status.success(),
            "{}",
            String::from_utf8_lossy(&local.stderr)
        );
        let expected = fs::read(&output).unwrap();
        let mut job = job(&compiler, &cwd, args, &source, &output);
        job.inputs
            .push(DigestMemo::default().input(&header).unwrap());
        let node_cas =
            soldr_dist::cas::Cas::new(&fixture._temp.path().join("worker"), u64::MAX).unwrap();
        node_cas.put(b"#define ANSWER 42\n").unwrap();
        // Same-process host has deliberately different bytes. Only the mirrored CAS
        // header may be read. Large source disables speculative header re-upload.
        fs::write(&header, b"#define ANSWER 7\n").unwrap();
        let outcome = fixture.client.execute(job).await;
        assert!(
            matches!(outcome, DistOutcome::Executed { exit_code: 0, .. }),
            "{outcome:?}"
        );
        assert_eq!(fs::read(&output).unwrap(), expected);
        fixture.stop().await;
    });
}

#[test]
fn rustc_stdin_job_has_no_file_inputs() {
    if !Path::new("/").is_absolute() {
        eprintln!("SKIP: dist job materialisation currently supports POSIX paths only");
        return;
    }
    Runtime::new().unwrap().block_on(async {
        let compiler = Compiler::rustc().await.unwrap();
        let fixture = Fixture::new(vec![compiler.clone()], true).await;
        let output = fixture._temp.path().join("stdin.o");
        let job = DistJob {
            compiler_kind: compiler.kind,
            compiler_identity: compiler.identity,
            cwd: "/dist-stdin".into(),
            argv: vec![
                "--emit=obj".into(),
                "--crate-name=stdin_unit".into(),
                "-".into(),
                "-o".into(),
                output.to_str().unwrap().into(),
            ],
            stdin: b"fn main() {}\n".to_vec(),
            outputs: vec![output.to_str().unwrap().into()],
            ..DistJob::default()
        };
        let outcome = fixture.client.execute(job).await;
        assert!(
            matches!(outcome, DistOutcome::Executed { exit_code: 0, .. }),
            "{outcome:?}"
        );
        assert!(!fs::read(output).unwrap().is_empty());
        assert_eq!(fixture.client.uploaded_blobs.load(Ordering::Relaxed), 0);
        fixture.stop().await;
    });
}

#[path = "rust_dependencies.rs"]
mod rust_dependencies;
