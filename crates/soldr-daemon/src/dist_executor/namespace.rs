//! Verify compiler execution policy; file completeness is constructed before submission.
use soldr_dist::DistJob;

pub(super) fn check(job: &DistJob, _complete: bool) -> Result<(), String> {
    if job.inputs.is_empty() || !job.stdin.is_empty() {
        return Err("initial dist miss adapter requires declared file-based Rust sources".into());
    }
    if job.argv.iter().any(|arg| {
        arg.starts_with("-Z")
            || arg.starts_with("--sysroot")
            || arg.contains("codegen-backend")
            || arg.contains("llvm-args")
            || arg.contains("incremental")
            || arg.starts_with("--remap-path-scope")
    }) {
        return Err("custom compiler runtime options require namespace isolation".into());
    }
    Ok(())
}

#[test]
fn external_crates_and_cold_manifests_are_eligible_with_host_toolchain() {
    let job = DistJob {
        compiler_identity: "commit-hash: abc\nhost: x86_64-unknown-linux-gnu".into(),
        inputs: vec![soldr_dist::InputFile::default()],
        argv: vec!["--extern".into(), "dep=/build/libdep.rlib".into()],
        ..Default::default()
    };
    assert!(check(&job, false).is_ok());
}
