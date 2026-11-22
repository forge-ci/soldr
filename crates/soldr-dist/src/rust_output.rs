//! Restore client paths in rustc's textual sidecars and artifact notifications.
use crate::{proto::Done, sandbox, DistJob, Result};
use std::{collections::HashSet, path::Path};

pub(crate) fn restore(job: &DistJob, root: &Path, done: &mut Done) -> Result<()> {
    if job.compiler_kind != "rustc" {
        return Ok(());
    }
    for bytes in [&mut done.stdout, &mut done.stderr] {
        if let Ok(mut text) = String::from_utf8(bytes.clone()) {
            // A private proof sidecar is transport metadata, never a Cargo artifact.
            let proof = root.join("rust-input-proof.d");
            text = text
                .split_inclusive('\n')
                .filter(|line| {
                    serde_json::from_str::<serde_json::Value>(line)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("artifact")
                                .and_then(|v| v.as_str())
                                .map(str::to_owned)
                        })
                        .is_none_or(|artifact| Path::new(&artifact) != proof)
                })
                .collect();
            for output in &job.outputs {
                let mirrored = sandbox::mirrored(root, output)?;
                let before = serde_json::to_string(&mirrored)?;
                let after = serde_json::to_string(output)?;
                text = text.replace(&before, &after);
            }
            *bytes = text.into_bytes();
        }
    }
    if done.exit_code != 0 {
        return Ok(());
    }
    let prefix = root.join("root");
    let prefix = prefix.to_str().ok_or("non-UTF8 sandbox root")?;
    if prefix.chars().any(|c| c.is_whitespace() || c == '\\') {
        return Err("Rust dep-info restoration requires an unescaped sandbox root".into());
    }
    for output in depfiles(job)? {
        let path = sandbox::mirrored(root, &output)?;
        let text = std::fs::read_to_string(&path)?;
        std::fs::write(path, text.replace(prefix, ""))?;
    }
    Ok(())
}

/// Reuse Cargo's depfile where present; otherwise add a private proof sidecar.
pub(crate) fn proof_args(
    job: &DistJob,
    root: &Path,
) -> Result<(Vec<String>, Option<std::path::PathBuf>)> {
    let mut args = sandbox::argv(root, job)?;
    if job.compiler_kind != "rustc" {
        return Ok((args, None));
    }
    if args.iter().any(|arg| arg.starts_with("--remap-path-scope")) {
        return Err("custom remap scope requires separate input proof support".into());
    }
    // Keep rustc's default all scope: narrower scopes retain physical paths in rmeta.
    // Dep-info paths are validated physically before any textual restoration.
    if let Some(path) = depfiles(job)?.into_iter().next() {
        return Ok((args, Some(sandbox::mirrored(root, &path)?)));
    }
    // rustc hashes the set of emitted output types into the crate hash (SVH), so
    // adding dep-info to a unit that emits metadata changes its rlib/rmeta bytes.
    // Cargo always emits dep-info; only hand-built jobs reach this.
    if emitted_kinds(&args)
        .iter()
        .any(|kind| kind == "link" || kind == "metadata")
    {
        return Err("rustc job emits metadata without dep-info: adding the input proof \
                    would change its crate hash, so it cannot be built remotely"
            .into());
    }
    let proof = root.join("rust-input-proof.d");
    let mode = format!("dep-info={}", proof.display());
    if let Some(index) = args
        .iter()
        .position(|a| a == "--emit" || a.starts_with("--emit="))
    {
        let index = if args[index] == "--emit" {
            index + 1
        } else {
            index
        };
        let value = args.get_mut(index).ok_or("missing emit value")?;
        value.push(',');
        value.push_str(&mode);
    } else {
        args.push(format!("--emit=link,{mode}"));
    }
    Ok((args, Some(proof)))
}

pub(crate) fn proof(job: &DistJob, root: &Path, path: Option<&Path>) -> Result<Vec<u8>> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    // On a compiler error a proof may not exist: the client then declines before publication.
    if !path.is_file() {
        return Ok(Vec::new());
    }
    if std::fs::metadata(path)?.len() > 4 * 1024 * 1024 {
        return Err("Rust dependency proof exceeds limit".into());
    }
    let text = std::fs::read_to_string(path)?;
    let prefix = root.join("root");
    let cwd = sandbox::mirrored(root, &job.cwd)?;
    for actual in crate::dep_info::paths(&text, &cwd)? {
        let relative = actual
            .strip_prefix(&prefix)
            .map_err(|_| format!("dependency read outside sandbox: {}", actual.display()))?;
        let client = Path::new("/").join(relative);
        let expected = job
            .inputs
            .iter()
            .find(|input| Path::new(&input.abs_path) == client)
            .ok_or_else(|| format!("dependency outside shipped inputs: {}", client.display()))?;
        let mut hash = blake3::Hasher::new();
        hash.update_reader(std::fs::File::open(&actual)?)?;
        if hash.finalize().to_hex().as_str() != expected.blake3 {
            return Err(format!(
                "dependency bytes differ from shipped input: {}",
                client.display()
            )
            .into());
        }
    }
    Ok(text
        .replace(prefix.to_str().ok_or("non-UTF8 sandbox root")?, "")
        .into_bytes())
}

/// Output types a rustc command line emits (`link` when `--emit` is absent).
fn emitted_kinds(args: &[String]) -> Vec<String> {
    let mut kinds = Vec::new();
    let mut seen_emit = false;
    for (index, arg) in args.iter().enumerate() {
        let spec = if arg == "--emit" {
            args.get(index + 1).map(String::as_str)
        } else {
            arg.strip_prefix("--emit=")
        };
        if let Some(spec) = spec {
            seen_emit = true;
            for mode in spec.split(',') {
                kinds.push(mode.split_once('=').map_or(mode, |(kind, _)| kind).to_owned());
            }
        }
    }
    if !seen_emit {
        kinds.push("link".into());
    }
    kinds
}

fn depfiles(job: &DistJob) -> Result<HashSet<String>> {
    let mut paths = HashSet::new();
    for (index, arg) in job.argv.iter().enumerate() {
        let spec = if arg == "--emit" {
            job.argv.get(index + 1).map(String::as_str)
        } else {
            arg.strip_prefix("--emit=")
        };
        for mode in spec.unwrap_or("").split(',') {
            if let Some(path) = mode.strip_prefix("dep-info=") {
                let path = sandbox::absolute(Path::new(&job.cwd), Path::new(path))?;
                let path = path.to_str().ok_or("non-UTF8 dep-info path")?;
                if !job.outputs.iter().any(|output| output == path) {
                    return Err("undeclared dep-info output".into());
                }
                paths.insert(path.to_owned());
            } else if mode == "dep-info" {
                paths.extend(
                    job.outputs
                        .iter()
                        .filter(|path| {
                            job.outputs.len() == 1
                                || Path::new(path).extension().is_some_and(|ext| ext == "d")
                        })
                        .cloned(),
                );
            }
        }
    }
    Ok(paths)
}

#[test]
fn dep_info_and_artifact_json_use_client_paths_without_changing_binary_outputs() {
    let root = tempfile::tempdir().unwrap();
    let job = DistJob {
        compiler_kind: "rustc".into(),
        cwd: "/repo".into(),
        argv: vec!["--emit=link=/stage/a.rlib,dep-info=/stage/a.d".into()],
        outputs: vec!["/stage/a.rlib".into(), "/stage/a.d".into()],
        ..Default::default()
    };
    let object = sandbox::mirrored(root.path(), "/stage/a.rlib").unwrap();
    let dep = sandbox::mirrored(root.path(), "/stage/a.d").unwrap();
    std::fs::create_dir_all(object.parent().unwrap()).unwrap();
    std::fs::write(&object, b"binary untouched").unwrap();
    let prefix = root.path().join("root");
    std::fs::write(
        &dep,
        format!(
            "{}/stage/a.rlib: {}/repo/lib.rs\n",
            prefix.display(),
            prefix.display()
        ),
    )
    .unwrap();
    let mut done = Done {
        stdout: serde_json::to_vec(&serde_json::json!({"artifact":object})).unwrap(),
        ..Default::default()
    };
    restore(&job, root.path(), &mut done).unwrap();
    assert_eq!(
        std::fs::read_to_string(dep).unwrap(),
        "/stage/a.rlib: /repo/lib.rs\n"
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&done.stdout).unwrap()["artifact"],
        "/stage/a.rlib"
    );
    assert_eq!(std::fs::read(object).unwrap(), b"binary untouched");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rustc(argv: &[&str], outputs: &[&str]) -> DistJob {
        DistJob {
            compiler_kind: "rustc".into(),
            cwd: "/w".into(),
            argv: argv.iter().map(|s| (*s).into()).collect(),
            outputs: outputs.iter().map(|s| (*s).into()).collect(),
            ..DistJob::default()
        }
    }

    #[test]
    fn metadata_units_without_dep_info_are_refused_not_altered() {
        let root = Path::new("/tmp/job");
        for argv in [
            &["--crate-type=rlib", "lib.rs"][..],
            &["--emit=metadata,link", "lib.rs"][..],
            &["--emit", "link=/w/out.rlib", "lib.rs"][..],
        ] {
            let error = proof_args(&rustc(argv, &["/w/out.rlib"]), root).unwrap_err();
            assert!(error.to_string().contains("crate hash"), "{argv:?}: {error}");
        }
        // An object-only unit carries no metadata hash, so the proof may be added.
        let (args, proof) = proof_args(&rustc(&["--emit=obj", "a.rs"], &["/w/a.o"]), root).unwrap();
        assert!(args.iter().any(|a| a.contains("dep-info=")), "{args:?}");
        assert!(proof.is_some());
        // Cargo's own dep-info is reused untouched.
        let (args, _) = proof_args(
            &rustc(&["--emit=dep-info=/w/a.d,metadata,link", "lib.rs"], &["/w/a.d", "/w/a.rlib"]),
            root,
        )
        .unwrap();
        assert_eq!(args.iter().filter(|a| a.contains("dep-info=")).count(), 1);
    }
}
