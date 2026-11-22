//! Conservative first-offload subset until compiler children have a real namespace.
use soldr_dist::DistJob;

pub(super) fn check(job: &DistJob) -> Result<(), String> {
    if job.inputs.is_empty() || !job.stdin.is_empty() {
        return Err("initial dist miss adapter requires declared file-based Rust sources".into());
    }
    if job.argv.iter().any(|arg| arg.starts_with("--extern")) {
        return Err(
            "external Rust crates require namespace isolation in the initial miss adapter".into(),
        );
    }
    if job.argv.iter().any(|arg| {
        arg.starts_with("-Z")
            || arg.starts_with("--sysroot")
            || arg.contains("codegen-backend")
            || arg.contains("llvm-args")
            || arg.contains("incremental")
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
    for input in &job.inputs {
        if !input.abs_path.ends_with(".rs") || input.size > 1024 * 1024 {
            return Err("initial dist miss adapter requires small Rust source inputs only".into());
        }
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(&input.abs_path)
            .and_then(|file| file.take(1024 * 1024 + 1).read_to_end(&mut bytes))
            .map_err(|e| format!("dist source inspection: {e}"))?;
        if zccache::hash::hash_bytes(&bytes).to_hex() != input.blake3 {
            return Err("dist source changed during namespace inspection".into());
        }
        // Deliberately includes comments/strings: false declines are safe. Includes,
        // module path attributes, assembly, external macros and identifier-pasting
        // can open absolute host paths that argv remapping cannot redirect.
        for token in [
            "include", "path", "asm", "feature", "extern", "concat", "env",
        ] {
            if bytes
                .windows(token.len())
                .any(|part| part == token.as_bytes())
            {
                return Err(format!(
                    "Rust source containing {token:?} requires namespace isolation"
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn namespace_dependent_sources_and_changed_inputs_decline() {
    use soldr_dist::InputFile;
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("lib.rs");
    let mut job = DistJob::default();
    let set = |job: &mut DistJob, bytes: &[u8]| {
        std::fs::write(&source, bytes).unwrap();
        job.inputs = vec![InputFile {
            abs_path: source.to_str().unwrap().into(),
            blake3: zccache::hash::hash_bytes(bytes).to_hex(),
            size: bytes.len() as u64,
            executable: false,
        }];
    };
    set(&mut job, b"pub fn answer() -> u32 { 42 }");
    assert!(check(&job).is_ok());
    for source in [
        "include!(\"/host/generated.rs\");",
        "#[ path = \"other.rs\"] mod x;",
        "use std::include as i;",
        "pub const HOME: Option<&str> = option_env!(\"HOME\");",
    ] {
        set(&mut job, source.as_bytes());
        assert!(check(&job).is_err());
    }
    set(&mut job, b"pub fn answer() -> u32 { 42 }");
    job.argv = vec!["--extern=macros=/host/libmacro.so".into()];
    assert!(check(&job).is_err());
    job.argv.clear();
    job.compiler_identity = "host: x86_64-unknown-linux-gnu".into();
    job.argv = vec!["--target=x86_64-unknown-linux-gnu".into()];
    assert!(check(&job).is_ok());
    job.argv = vec!["--target=/host/custom.json".into()];
    assert!(check(&job).is_err());
    job.argv = vec!["-Zcrate-attr=doc=include_str!(\"/host/file\")".into()];
    assert!(check(&job).is_err());
    job.argv.clear();
    std::fs::write(source, b"include!(\"/host/generated.rs\");").unwrap();
    assert!(check(&job).unwrap_err().contains("changed"));
}
