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
    let host = job
        .compiler_identity
        .lines()
        .find_map(|line| line.strip_prefix("host: "));
    for (index, arg) in job.argv.iter().enumerate() {
        let target = if arg == "--target" {
            Some(
                job.argv
                    .get(index + 1)
                    .ok_or("missing target value")?
                    .as_str(),
            )
        } else {
            arg.strip_prefix("--target=")
        };
        if target.is_some() && target != host {
            return Err("non-host targets require independently verified target resources".into());
        }
    }
    Ok(())
}

#[test]
fn external_crates_and_cold_manifests_are_eligible_with_host_toolchain() {
    let mut job = DistJob {
        compiler_identity: "commit-hash: abc\nhost: x86_64-unknown-linux-gnu".into(),
        inputs: vec![soldr_dist::InputFile::default()],
        argv: vec!["--extern".into(), "dep=/build/libdep.rlib".into()],
        ..Default::default()
    };
    assert!(check(&job, false).is_ok());
    job.argv
        .extend(["--target".into(), "aarch64-unknown-linux-gnu".into()]);
    assert!(check(&job, true).unwrap_err().contains("non-host"));
}
