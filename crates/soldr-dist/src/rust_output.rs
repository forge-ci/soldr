//! Restore client paths in rustc's textual sidecars and artifact notifications.
use crate::{proto::Done, sandbox, DistJob, Result};
use std::{collections::HashSet, path::Path};

pub(crate) fn restore(job: &DistJob, root: &Path, done: &mut Done) -> Result<()> {
    if job.compiler_kind != "rustc" {
        return Ok(());
    }
    for bytes in [&mut done.stdout, &mut done.stderr] {
        if let Ok(mut text) = String::from_utf8(bytes.clone()) {
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
