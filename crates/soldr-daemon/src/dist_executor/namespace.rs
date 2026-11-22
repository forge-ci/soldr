//! Conservative first-offload subset until compiler children have a real namespace.
use soldr_dist::DistJob;

pub(super) fn check(job: &DistJob, complete: bool) -> Result<(), String> {
    if job.inputs.is_empty() || !job.stdin.is_empty() {
        return Err("initial dist miss adapter requires declared file-based Rust sources".into());
    }
    if !complete {
        independent_source(job)?;
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
        if !complete && bytes.windows(3).any(|part| part == b"mod") {
            return Err("incomplete module graph requires local dependency discovery".into());
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

fn independent_source(job: &DistJob) -> Result<(), String> {
    if job.inputs.len() != 1 {
        return Err("incomplete manifest is not a single-source Rust job".into());
    }
    let source = std::path::Path::new(&job.inputs[0].abs_path);
    let cwd = std::path::Path::new(&job.cwd);
    let source_argument = job.argv.iter().any(|arg| {
        !arg.starts_with('-')
            && soldr_dist::sandbox::absolute(cwd, std::path::Path::new(arg))
                .ok()
                .as_deref()
                == Some(source)
    });
    if !source_argument {
        return Err("incomplete manifest does not identify rustc's source argument".into());
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
    assert!(check(&job, true).is_ok());
    for source in [
        "include!(\"/host/generated.rs\");",
        "#[ path = \"other.rs\"] mod x;",
        "use std::include as i;",
        "pub const HOME: Option<&str> = option_env!(\"HOME\");",
    ] {
        set(&mut job, source.as_bytes());
        assert!(check(&job, true).is_err());
    }
    set(&mut job, b"pub fn answer() -> u32 { 42 }");
    job.argv = vec!["--extern=macros=/host/libmacro.so".into()];
    assert!(check(&job, true).is_err());
    job.argv.clear();
    job.compiler_identity = "host: x86_64-unknown-linux-gnu".into();
    job.argv = vec!["--target=x86_64-unknown-linux-gnu".into()];
    assert!(check(&job, true).is_ok());
    job.argv = vec!["--target=/host/custom.json".into()];
    assert!(check(&job, true).is_err());
    job.argv = vec!["-Zcrate-attr=doc=include_str!(\"/host/file\")".into()];
    assert!(check(&job, true).is_err());
    job.argv.clear();
    std::fs::write(source, b"include!(\"/host/generated.rs\");").unwrap();
    assert!(check(&job, true).unwrap_err().contains("changed"));
}

#[test]
fn cold_single_file_manifest_can_be_proven_without_a_local_object_compile() {
    use soldr_dist::InputFile;
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("lib.rs");
    let mut job = DistJob {
        cwd: root.path().to_str().unwrap().into(),
        argv: vec!["lib.rs".into()],
        ..Default::default()
    };
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
    assert!(check(&job, false).is_ok());
    job.argv.clear();
    assert!(check(&job, false).is_err());
    job.argv.push("lib.rs".into());
    set(&mut job, b"mod undiscovered;");
    assert!(check(&job, false).unwrap_err().contains("module graph"));
    set(
        &mut job,
        br#"pub const DATA: &str = include_str!("/host/data");"#,
    );
    assert!(check(&job, false).is_err());
}
