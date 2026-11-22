//! Mirrored absolute paths and workspace-relative debug/source paths.
use crate::{cas::Cas, DistJob, Result};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

pub fn mirrored(root: &Path, original: &str) -> Result<PathBuf> {
    let path = Path::new(original);
    if !path.is_absolute() {
        return Err("Dist requires absolute manifest paths".into());
    }
    let mut result = root.join("root");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(part) => result.push(part),
            _ => return Err("Dist path has parent traversal or unsupported prefix".into()),
        }
    }
    Ok(result)
}
pub fn materialise(root: &Path, job: &DistJob, cas: &Cas) -> Result<()> {
    for input in &job.inputs {
        let dest = mirrored(root, &input.abs_path)?;
        if job.outputs.contains(&input.abs_path) {
            return Err("input/output overlap".into());
        }
        fs::create_dir_all(dest.parent().ok_or("input parent missing")?)?;
        // Verify old CAS entries too: disk corruption must never become compiler input.
        cas.verify(&input.blake3)?;
        let source = cas.path(&input.blake3)?;
        if fs::hard_link(&source, &dest).is_err() {
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&dest)?;
            std::io::copy(&mut fs::File::open(&source)?, &mut output)?;
        }
        // Never chmod a CAS hardlink: executable mode belongs to the manifest.
        if input.executable {
            let bytes = fs::read(&dest)?;
            fs::remove_file(&dest)?;
            fs::write(&dest, bytes)?;
            soldr_platform::fs::permissions::make_executable(&dest)?;
        }
    }
    fs::create_dir_all(mirrored(root, &job.cwd)?)?;
    for output in &job.outputs {
        let output = mirrored(root, output)?;
        fs::create_dir_all(output.parent().ok_or("output parent missing")?)?;
    }
    Ok(())
}
/// Normalise lexical absolute paths without requiring outputs to exist.
pub fn absolute(cwd: &Path, path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut result = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                if !result.pop() {
                    return Err("path escapes filesystem root".into());
                }
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    Ok(result)
}
/// Rewrite path-bearing compiler argv, preserving unrelated string arguments.
pub fn rewrite(root: &Path, job: &DistJob, value: &str) -> Result<String> {
    if job.compiler_kind == "rustc" {
        if let Some(value) = value.strip_prefix("--emit=") {
            return Ok(format!("--emit={}", emit(root, job, value)?));
        }
    }
    let remap_prefix = [
        "--remap-path-prefix=",
        "-ffile-prefix-map=",
        "-fdebug-prefix-map=",
        "-fmacro-prefix-map=",
    ]
    .into_iter()
    .find(|prefix| value.starts_with(prefix));
    if let Some(prefix) = remap_prefix {
        let remap = value.strip_prefix(prefix).ok_or("invalid path remap")?;
        let (from, to) = remap.split_once('=').ok_or("invalid rustc path remap")?;
        return Ok(format!("{prefix}{}={to}", mirrored(root, from)?.display()));
    }
    // Recognise a standalone absolute path or one following a compiler option.
    let start = if value.starts_with('/') {
        0
    } else if value.starts_with("--extern=")
        || value.starts_with("-L")
        || value.starts_with("-I")
        || [
            "-iquote",
            "-isystem",
            "-idirafter",
            "-include",
            "-imacros",
            "-isysroot",
        ]
        .iter()
        .any(|prefix| value.starts_with(prefix))
        || value.starts_with("--sysroot=")
        || value.starts_with("--out-dir=")
    {
        match value.find('/') {
            Some(start) => start,
            None => return Ok(value.into()),
        }
    } else if value
        .split_once('=')
        .is_some_and(|(name, path)| !name.starts_with('-') && path.starts_with('/'))
    {
        value.find('/').ok_or("missing extern path")?
    } else {
        return Ok(value.into());
    };
    let normal = absolute(Path::new(&job.cwd), Path::new(&value[start..]))?;
    let path = normal.to_str().ok_or("non-UTF8 path")?;
    // Only declared file/directory trees belong to the mirror. Sysroots stay worker-owned.
    let owned = Path::new(path).starts_with(&job.cwd)
        || job.inputs.iter().any(|i| {
            Path::new(path) == Path::new(&i.abs_path)
                || (Path::new(path).parent().is_some() && Path::new(&i.abs_path).starts_with(path))
        })
        || job.outputs.iter().any(|p| {
            Path::new(path) == Path::new(p)
                || Path::new(p).parent().is_some_and(|p| p == Path::new(path))
        });
    if !owned {
        return Ok(value.into());
    }
    Ok(format!(
        "{}{}",
        &value[..start],
        mirrored(root, path)?.display()
    ))
}

/// Handle rustc's paired --emit form as well as ordinary path-bearing arguments.
pub fn argv(root: &Path, job: &DistJob) -> Result<Vec<String>> {
    job.argv
        .iter()
        .enumerate()
        .map(|(index, arg)| {
            if job.compiler_kind == "rustc" && index > 0 && job.argv[index - 1] == "--emit" {
                emit(root, job, arg)
            } else {
                rewrite(root, job, arg)
            }
        })
        .collect()
}

fn emit(root: &Path, job: &DistJob, value: &str) -> Result<String> {
    value
        .split(',')
        .map(|part| {
            let Some((kind, path)) = part.split_once('=') else {
                return Ok(part.to_owned());
            };
            if path == "-" {
                return Err("stdout emit assignments cannot be materialised".into());
            }
            let path = absolute(Path::new(&job.cwd), Path::new(path))?;
            if !job.outputs.iter().any(|output| Path::new(output) == path) {
                return Err("rustc emit path is not a declared output".into());
            }
            Ok(format!(
                "{kind}={}",
                mirrored(root, path.to_str().ok_or("non-UTF8 emit path")?)?.display()
            ))
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join(","))
}
