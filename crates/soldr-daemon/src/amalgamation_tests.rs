//! Unit coverage split from `amalgamation.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn write(dir: &Path, name: &str, bytes: usize) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, vec![b'x'; bytes]).expect("write fixture");
    path
}

#[test]
fn a_large_translation_unit_is_detected_by_size_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "private-amalgamation.c", 2_000_000);
    let args = vec!["-O3".into(), "-c".into(), "private-amalgamation.c".into()];

    let found = detect(&args, dir.path()).expect("size alone must be enough");
    assert_eq!(found.bytes, 2_000_000);
    assert!(found
        .describe()
        .starts_with("private-amalgamation.c (2.0 MB)"));
}

#[test]
fn an_ordinary_source_is_not_an_amalgamation() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "util.c", 4_096);
    let args = vec!["-O2".into(), "-c".into(), "util.c".into()];

    assert_eq!(detect(&args, dir.path()), None);
}

// The allowlist supplements the threshold; it does not replace it. A
// known name under the size bar still counts, which is what makes the
// table useful for a source that grows across versions.
#[test]
fn a_known_name_counts_even_when_small() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "sqlite3.c", 1_024);
    let args = vec!["-c".into(), "sqlite3.c".into()];

    assert!(detect(&args, dir.path()).is_some());
}

#[test]
fn absolute_source_paths_are_measured_where_they_are() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write(dir.path(), "big.c", 1_500_000);
    let elsewhere = tempfile::tempdir().expect("second tempdir");
    let args = vec!["-c".into(), path.display().to_string()];

    assert!(detect(&args, elsewhere.path()).is_some());
}

// Runs on a failure path: a missing or unreadable source must produce
// "no amalgamation", never an error or a panic.
#[test]
fn an_unmeasurable_source_is_simply_not_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let args = vec!["-c".into(), "absent.c".into()];

    assert_eq!(detect(&args, dir.path()), None);
}

// `-o sqlite3.o` and friends must not be mistaken for the input, and a
// flag that merely ends in a source-looking extension is still a flag.
#[test]
fn flags_are_not_translation_units() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "sqlite3.c", 4_000_000);
    let args = vec!["--include=x.c".into(), "-Wp,-MD,dep.c".into()];

    assert_eq!(detect(&args, dir.path()), None);
}

#[test]
fn non_source_arguments_are_ignored() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "libbig.a", 5_000_000);
    let args = vec!["-c".into(), "libbig.a".into()];

    assert_eq!(detect(&args, dir.path()), None);
}

// ---- the pre-compile notice (soldr#2781) ----------------------------
//
// These matter more than the post-mortem's: this is the line a user sees
// *while* a 255,000-line translation unit is in the compiler, and it is
// the only warning they get before an OOM kill takes the build with no
// indication of which file was responsible.

#[test]
fn the_notice_names_the_file_and_its_size() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "sqlite3.c", 8_400_000);
    let args = vec!["-O3".into(), "-c".into(), "sqlite3.c".into()];

    let notice = compile_notice(&args, dir.path()).expect("an amalgamation must announce");

    assert!(notice.contains("sqlite3.c"), "{notice}");
    assert!(notice.contains("8.4 MB"), "{notice}");
}

#[test]
fn the_notice_gives_the_cause_and_the_remedy() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "sqlite3.c", 8_400_000);
    let args = vec!["-c".to_string(), "sqlite3.c".into()];

    let notice = compile_notice(&args, dir.path()).expect("notice");

    // Reading this mid-build, the two questions are "why is this slow /
    // why did it die" and "what do I do".
    assert!(notice.contains("memory"), "{notice}");
    assert!(notice.contains("CARGO_BUILD_JOBS"), "{notice}");
    assert!(notice.contains("SOLDR_JOBS"), "{notice}");
    assert!(notice.contains("INFO"), "{notice}");
}

#[test]
fn an_ordinary_compile_says_nothing() {
    // A notice on every `cc` invocation would be noise, and noise is how
    // the one that matters gets skipped.
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "util.c", 3_000);
    let args = vec!["-c".to_string(), "util.c".into()];

    assert_eq!(compile_notice(&args, dir.path()), None);
}

#[test]
fn a_rustc_invocation_says_nothing() {
    // The daemon compiles rustc units through the same path. rustc splits
    // work across codegen units inside one invocation, so a large .rs is
    // not the single-process spike a large .c is -- and a notice here
    // would fire on ordinary Rust builds.
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "lib.rs", 4_000_000);
    let args = vec!["--edition=2021".to_string(), "lib.rs".into()];

    assert_eq!(compile_notice(&args, dir.path()), None);
}

// The notice is worthless if it arrives after the compile it describes.
// `compile()` must call it before handing work to zccache -- the whole
// point is forewarning, and a post-mortem already exists in
// `compiler_exit`. Checked against the source because the emission is an
// `eprintln!` that no stable in-process API can capture.
#[test]
fn the_compile_path_announces_before_dispatching() {
    let src = include_str!("zccache_embedded.rs");
    let announce = src
        .find("compile_notice(")
        .expect("compile() must ask for the notice");
    let dispatch = src
        .find(".compile_with_options(zreq")
        .expect("compile() must dispatch to zccache");
    assert!(
        announce < dispatch,
        "the notice must be emitted before the compile it describes, \
         not after it returns"
    );
}

// The shape the daemon actually receives: `CompileRequest::args` carries
// the compiler at [0] and the compiler's own arguments after it, and the
// cwd arrives as a String. Every other test here passes bare flags, so
// this is the one that would catch the detector being fed the wrong slice
// -- `rustc_args` (args[1..]) instead of `args`, say, or a compiler path
// being mistaken for an input.
#[test]
fn a_request_shaped_argv_finds_the_input_not_the_compiler() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "sqlite3.c", 8_400_000);
    let args: Vec<String> = [
        "/usr/bin/cc",
        "-O3",
        "-DSQLITE_CORE",
        "-c",
        "sqlite3.c",
        "-o",
        "sqlite3.o",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();

    let found = detect(&args, dir.path()).expect("the input must be found");
    assert_eq!(found.path.file_name().unwrap(), "sqlite3.c");
    assert!(compile_notice(&args, dir.path()).is_some());
}

// A compiler whose own path ends in a source extension must not be
// mistaken for the translation unit. Contrived, but the detector scans
// args[0] too and the failure would be silent: the notice would name the
// compiler and the real amalgamation would go unannounced.
#[test]
fn a_compiler_path_is_not_the_translation_unit() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "cc.c", 9_000_000);
    write(dir.path(), "real.c", 2_000_000);
    let compiler = dir.path().join("cc.c").display().to_string();
    let args = vec![compiler, "-c".to_string(), "real.c".to_string()];

    let found = detect(&args, dir.path()).expect("the real input is found");
    assert_eq!(
        found.path.file_name().unwrap(),
        "real.c",
        "args[0] is the compiler and must never be reported as the unit"
    );
}

#[test]
fn soldr_policy_marks_kernal_api_for_exclusive_access() {
    let args = vec![
        "--crate-name".to_string(),
        "kernal_api".to_string(),
        "--crate-type=lib".to_string(),
        "/registry/kernal-api/src/lib.rs".to_string(),
    ];

    assert!(soldr_rust_crate_requires_exclusive_access(&args));
}

#[test]
fn upstream_zccache_names_are_not_reclassified_by_soldr() {
    for crate_name in ["zccache", "zccache_cli_core", "zccache_daemon_core"] {
        let args = vec![
            format!("--crate-name={crate_name}"),
            "--crate-type=lib".to_string(),
        ];

        assert!(
            !soldr_rust_crate_requires_exclusive_access(&args),
            "{crate_name} belongs to zccache's built-in predicate"
        );
    }
}

#[test]
fn linking_and_test_forms_of_kernal_api_keep_shared_access() {
    for suffix in [
        vec!["--crate-type=bin".to_string()],
        vec!["--crate-type=lib".to_string(), "--test".to_string()],
        vec!["--crate-type=lib,cdylib".to_string()],
    ] {
        let mut args = vec!["--crate-name=kernal_api".to_string()];
        args.extend(suffix);
        assert!(!soldr_rust_crate_requires_exclusive_access(&args));
    }
}

#[test]
fn soldr_daemon_test_link_has_exclusive_access() {
    let args = vec![
        "--crate-name=soldr_daemon".to_string(),
        "--test".to_string(),
        "crates/soldr-daemon/src/lib.rs".to_string(),
    ];

    assert!(soldr_rust_crate_requires_exclusive_access(&args));
}

#[test]
fn soldr_cli_test_link_has_exclusive_access() {
    let args = vec![
        "--crate-name=soldr_cli".to_string(),
        "--test".to_string(),
        "crates/soldr-cli/src/lib.rs".to_string(),
    ];

    assert!(soldr_rust_crate_requires_exclusive_access(&args));
}

/// The eight consolidated integration-test binaries from soldr#2934. None
/// is named `soldr_cli` or `soldr_daemon`, so none matched the old name
/// list -- and per soldr#2931 each links the full soldr graph, making them
/// the heaviest links in the workspace.
const CONSOLIDATED_TEST_BINARIES: &[&str] = &[
    "broker",
    "cache_gc",
    "cargo_front_door",
    "cook_dylint",
    "daemon",
    "fetch_tools",
    "guards",
    "toolchain_env",
];

/// A `--test` link of `crate`, linking `externs` rlibs of `each` bytes.
fn test_link(crate_name: &str, externs: usize, each: u64) -> (Vec<String>, u64) {
    let mut args = vec![
        format!("--crate-name={crate_name}"),
        "--test".to_string(),
        format!("crates/soldr-cli/tests/{crate_name}/main.rs"),
    ];
    for i in 0..externs {
        args.push("--extern".to_string());
        args.push(format!("dep{i}=/target/debug/deps/libdep{i}.rlib"));
    }
    (args, each)
}

fn sized(each: u64) -> impl Fn(&Path) -> Option<u64> {
    move |_| Some(each)
}

#[test]
fn consolidated_integration_test_links_get_exclusive_access_by_size() {
    // 43 externs x 5 MB = 215 MB, matching the 147-231 MB measured for a
    // real full-graph test link.
    for name in CONSOLIDATED_TEST_BINARIES {
        let (args, each) = test_link(name, 43, 5_000_000);
        assert!(
            soldr_rust_crate_requires_exclusive_access_with(&args, sized(each)),
            "{name} links the full graph and must not share a compiler slot",
        );
    }
}

#[test]
fn the_old_name_list_would_have_missed_every_one_of_them() {
    // The regression soldr#3150 records: these are exactly the binaries the
    // name-based rule could not see.
    for name in CONSOLIDATED_TEST_BINARIES {
        assert!(
            !SOLDR_HEAVY_TEST_LINKS.contains(name),
            "{name} is not in the name list -- that is the bug being fixed",
        );
    }
}

#[test]
fn small_test_links_keep_shared_access() {
    // Trivial test binaries carry 0-4 externs in a real journal. They must
    // keep packing in parallel: serializing them would cost more build time
    // than the OOMs exclusivity prevents.
    for externs in [0_usize, 1, 2, 4] {
        let (args, each) = test_link("tiny_probe", externs, 1_000_000);
        assert!(
            !soldr_rust_crate_requires_exclusive_access_with(&args, sized(each)),
            "a {externs}-extern test link must not reserve the machine",
        );
    }
}

#[test]
fn a_heavy_non_test_compile_keeps_shared_access() {
    // Only `--test` links are measured. An ordinary rlib compile lists its
    // externs without linking them and has a much flatter memory profile.
    let mut args = vec![
        "--crate-name=some_lib".to_string(),
        "--crate-type=lib".to_string(),
        "crates/some-lib/src/lib.rs".to_string(),
    ];
    for i in 0..43 {
        args.push("--extern".to_string());
        args.push(format!("dep{i}=/target/debug/deps/libdep{i}.rlib"));
    }
    assert!(!soldr_rust_crate_requires_exclusive_access_with(
        &args,
        sized(5_000_000)
    ));
}

#[test]
fn unmeasurable_externs_do_not_fail_admission() {
    // An input that cannot be stat'd must not turn admission into an error
    // or a false positive -- the rule `measure` already follows.
    let (args, _) = test_link("broker", 43, 5_000_000);
    assert!(!soldr_rust_crate_requires_exclusive_access_with(
        &args,
        |_| None
    ));
}

#[test]
fn both_extern_spellings_are_measured() {
    // `--extern k=v` and `--extern=k=v` both occur in real command lines.
    let split = vec![
        "--extern".to_string(),
        "a=/x/liba.rlib".to_string(),
        "--extern=b=/x/libb.rlib".to_string(),
    ];
    assert_eq!(extern_rlib_bytes(&split, |_| Some(10)), 20);
}

#[test]
fn bare_sysroot_externs_are_skipped() {
    // `--extern proc_macro` names a sysroot crate with no path to measure.
    let args = vec![
        "--extern".to_string(),
        "proc_macro".to_string(),
        "--extern".to_string(),
        "a=/x/liba.rlib".to_string(),
    ];
    assert_eq!(extern_rlib_bytes(&args, |_| Some(7)), 7);
}

#[test]
fn non_test_soldr_daemon_build_keeps_shared_access() {
    let args = vec![
        "--crate-name=soldr_daemon".to_string(),
        "--crate-type=lib".to_string(),
        "crates/soldr-daemon/src/lib.rs".to_string(),
    ];

    assert!(!soldr_rust_crate_requires_exclusive_access(&args));
}

#[test]
fn soldr_cli_dylint_workspace_analysis_has_exclusive_access() {
    let args = vec![
        "--crate-name=soldr_cli".to_string(),
        "--crate-type=lib".to_string(),
        "--emit=dep-info,metadata".to_string(),
        "crates/soldr-cli/src/lib.rs".to_string(),
    ];

    assert!(soldr_rust_crate_requires_exclusive_access(&args));
}

#[test]
fn an_ordinary_rust_crate_keeps_shared_access() {
    let args = vec![
        "--crate-name=small_crate".to_string(),
        "--crate-type=lib".to_string(),
        "/registry/small-crate/src/lib.rs".to_string(),
    ];

    assert!(!soldr_rust_crate_requires_exclusive_access(&args));
}

#[test]
fn compile_dispatch_uses_only_zccaches_post_hit_resource_gate() {
    let src = include_str!("zccache_embedded.rs");
    assert!(
        !src.contains("compile_resource_gate"),
        "Soldr must not acquire a general compiler resource gate before \
         zccache knows whether the request is a cache hit"
    );
    assert!(
        src.contains("start_with_options_and_host_admission_classifier"),
        "Soldr's product-specific predicate must feed zccache's canonical \
         post-hit compiler admission"
    );
}

struct CountingSoldrPolicy {
    calls: Arc<AtomicUsize>,
}

impl HostAdmissionClassifier for CountingSoldrPolicy {
    fn requires_exclusive(
        &self,
        request: &HostCompilerRequest<'_>,
    ) -> Result<bool, HostAdmissionError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        SoldrHostAdmissionClassifier.requires_exclusive(request)
    }
}

type RecordedAdmission = (CompilerFamily, Vec<String>, bool);
type RecordedAdmissions = Arc<Mutex<Vec<RecordedAdmission>>>;

struct RecordingSoldrPolicy {
    requests: RecordedAdmissions,
}

impl HostAdmissionClassifier for RecordingSoldrPolicy {
    fn requires_exclusive(
        &self,
        request: &HostCompilerRequest<'_>,
    ) -> Result<bool, HostAdmissionError> {
        let exclusive = SoldrHostAdmissionClassifier.requires_exclusive(request)?;
        self.requests.lock().expect("recording policy lock").push((
            request.family(),
            request.args().to_vec(),
            exclusive,
        ));
        Ok(exclusive)
    }
}

// This is the behavior the removed Soldr-side gate could not provide: a
// hit never reaches the product classifier or takes compiler admission.
// Keep the test here, against the exact zccache release Soldr embeds, so a
// future pin cannot silently move the callback ahead of hit detection.
#[tokio::test]
async fn pinned_embedded_hook_runs_after_cache_hit_classification() {
    use zccache::audit::{AuditId, AuditMode};
    use zccache::embedded::{
        AuditConfig, AuditContext, CompileRequest, HostIdentity, RuntimeHooks, ServiceLimits,
        ShutdownMode, ZccacheConfig, ZccacheService, ZccacheStartOptions,
    };

    let Some(compiler) = crate::test_support::find_on_path("cc") else {
        return;
    };
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("host-policy.c");
    let output = temp.path().join("host-policy.o");
    std::fs::write(&source, "int soldr_host_policy(void) { return 1; }\n").expect("source fixture");
    let calls = Arc::new(AtomicUsize::new(0));
    let service = ZccacheService::start_with_options_and_host_admission_classifier(
        ZccacheConfig {
            host: HostIdentity {
                product: "soldr-host-policy-test".into(),
                instance_id: temp.path().display().to_string(),
                workspace_id: "soldr-host-policy-workspace".into(),
            },
            cache_root: temp.path().join("cache").into(),
            audit: AuditConfig {
                mode: AuditMode::Off,
                ..AuditConfig::default()
            },
            limits: ServiceLimits::default(),
            runtime: RuntimeHooks::default(),
            cancellation: None,
        },
        ZccacheStartOptions::default(),
        Arc::new(CountingSoldrPolicy {
            calls: Arc::clone(&calls),
        }),
    )
    .await
    .expect("embedded service starts");
    let request = CompileRequest {
        audit: AuditContext::new(
            AuditId::new("soldr-host-policy-run").expect("run id"),
            AuditId::new("soldr-host-policy-trace").expect("trace id"),
        ),
        compiler: compiler.into(),
        args: vec![
            "-c".into(),
            source.display().to_string(),
            "-o".into(),
            output.display().to_string(),
        ],
        cwd: temp.path().into(),
        env: Vec::new(),
        stdin: Vec::new(),
    };

    let miss = service.compile(request.clone()).await.expect("cache miss");
    assert!(!miss.cached, "first compile must execute the compiler");
    assert_eq!(calls.load(Ordering::Relaxed), 1, "miss invokes policy");

    std::fs::remove_file(&output).expect("remove cold output");
    let hit = service.compile(request).await.expect("cache hit");
    assert!(hit.cached, "second compile must replay cached output");
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "cache hit must bypass host policy and compiler admission"
    );
    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown");
}

// This reaches the exact production route: zccache identifies a real
// rustc request, normalizes its compiler arguments, and only then calls
// Soldr's embedded admission policy.  A pure argv test above cannot catch
// a future zccache change that drops `--test` or calls the host policy
// with a non-Rust family on this pipeline.
#[tokio::test]
async fn pinned_embedded_hook_marks_real_soldr_daemon_test_rustc_exclusive() {
    use zccache::audit::{AuditId, AuditMode};
    use zccache::embedded::{
        AuditConfig, AuditContext, CompileRequest, HostIdentity, RuntimeHooks, ServiceLimits,
        ShutdownMode, ZccacheConfig, ZccacheService, ZccacheStartOptions,
    };

    let Some(compiler) = crate::test_support::find_on_path("rustc") else {
        return;
    };
    let current_dir = std::env::current_dir().expect("resolve current directory");
    let repo = current_dir
        .ancestors()
        .find(|candidate| candidate.join("rust-toolchain.toml").is_file())
        .expect("find repository rust-toolchain.toml");
    let pinned_toolchain = crate::core::read_rust_toolchain_manifest(repo)
        .expect("read repository rust-toolchain.toml")
        .channel
        .expect("repository rust-toolchain.toml declares a channel");
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("workspace");
    std::fs::create_dir_all(project.join("src")).expect("create source directory");
    std::fs::write(
        project.join("src/lib.rs"),
        "#[test]\nfn admission_fixture() { assert_eq!(2 + 2, 4); }\n",
    )
    .expect("write source");
    let requests: RecordedAdmissions = Arc::new(Mutex::new(Vec::new()));
    let service = ZccacheService::start_with_options_and_host_admission_classifier(
        ZccacheConfig {
            host: HostIdentity {
                product: "soldr-rustc-host-policy-test".into(),
                instance_id: temp.path().display().to_string(),
                workspace_id: "soldr-rustc-host-policy-workspace".into(),
            },
            cache_root: temp.path().join("cache").into(),
            audit: AuditConfig {
                mode: AuditMode::Off,
                ..AuditConfig::default()
            },
            limits: ServiceLimits::default(),
            runtime: RuntimeHooks::default(),
            cancellation: None,
        },
        ZccacheStartOptions::default(),
        Arc::new(RecordingSoldrPolicy {
            requests: Arc::clone(&requests),
        }),
    )
    .await
    .expect("embedded service starts");
    let request = CompileRequest {
        audit: AuditContext::new(
            AuditId::new("soldr-rustc-host-policy-run").expect("run id"),
            AuditId::new("soldr-rustc-host-policy-trace").expect("trace id"),
        ),
        compiler: compiler.into(),
        args: vec![
            "--edition=2021".into(),
            "--crate-name=soldr_daemon".into(),
            "--test".into(),
            "--emit=metadata".into(),
            "--out-dir".into(),
            "target/debug/deps".into(),
            "src/lib.rs".into(),
        ],
        cwd: project.clone().into(),
        env: std::env::vars()
            .filter(|(key, _)| key != "RUSTUP_TOOLCHAIN")
            .chain(std::iter::once((
                "RUSTUP_TOOLCHAIN".into(),
                pinned_toolchain,
            )))
            .collect(),
        stdin: Vec::new(),
    };

    let response = service.compile(request).await.expect("rustc compile");
    assert_eq!(
        response.exit_code,
        0,
        "real rustc failed: {}",
        String::from_utf8_lossy(&response.stderr)
    );
    let recorded = {
        let mut guard = requests.lock().expect("recorded rustc request");
        std::mem::take(&mut *guard)
    };
    assert_eq!(recorded.len(), 1, "one cold rustc miss reaches the policy");
    let (family, args, exclusive) = &recorded[0];
    assert_eq!(*family, CompilerFamily::Rustc);
    assert!(args.iter().any(|arg| arg == "--crate-name=soldr_daemon"));
    assert!(args.iter().any(|arg| arg == "--test"));
    assert!(
        *exclusive,
        "real soldr_daemon test rustc needs exclusive admission"
    );
    service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown");
}
