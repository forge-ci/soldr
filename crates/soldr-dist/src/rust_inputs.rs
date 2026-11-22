//! Construct Rust inputs before a cold cache miss; never require a prior local compile.
use crate::{cas::DigestMemo, sandbox, DistJob, Result};
use running_process::AsyncProcessBuilder;
mod env;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

/// Enlarge a hook manifest from package membership, compiler search paths and generated files.
pub async fn construct(mut job: DistJob, memo: &tokio::sync::Mutex<DigestMemo>) -> Result<DistJob> {
    let original_inputs = job.inputs.clone();
    let cwd = Path::new(&job.cwd);
    let mut paths = job
        .inputs
        .iter()
        .map(|i| PathBuf::from(&i.abs_path))
        .collect::<BTreeSet<_>>();
    let package = package_root(&job)?;
    paths.extend(package_files(&package).await?);
    search_files(&job.argv, cwd, &mut paths)?;
    if let Some(out) = job.env.get("OUT_DIR").filter(|p| !p.is_empty()) {
        tree(
            &sandbox::absolute(cwd, Path::new(out))?,
            &mut paths,
            &mut BTreeSet::new(),
        )?;
    }
    paths.retain(|path| !job.outputs.iter().any(|output| Path::new(output) == path));
    let mut memo = memo.lock().await;
    job.inputs = paths
        .into_iter()
        .map(|p| memo.input(&p))
        .collect::<Result<_>>()?;
    drop(memo);
    for known in &original_inputs {
        if !job.inputs.iter().any(|input| input == known) {
            return Err(format!(
                "keyed input changed during dist construction: {}",
                known.abs_path
            )
            .into());
        }
    }
    env::select(&mut job)?;
    Ok(job)
}

fn package_root(job: &DistJob) -> Result<PathBuf> {
    if let Some(root) = job.env.get("CARGO_MANIFEST_DIR") {
        return sandbox::absolute(Path::new(&job.cwd), Path::new(root));
    }
    // Standalone debug invocations have no Cargo environment; prefer their source's package.
    for input in &job.inputs {
        if !input.abs_path.ends_with(".rs") {
            continue;
        }
        for root in Path::new(&input.abs_path).ancestors().skip(1) {
            if root.join("Cargo.toml").is_file() {
                return Ok(root.into());
            }
        }
    }
    Err("Rust unit has no CARGO_MANIFEST_DIR or discoverable package".into())
}

async fn package_files(package: &Path) -> Result<BTreeSet<PathBuf>> {
    let git = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        AsyncProcessBuilder::new("git")
            .kill_on_drop(true)
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(package)
            .capture(),
    )
    .await
    .map_err(|_| "git package root deadline")??;
    if git.status.success() {
        let root = PathBuf::from(std::str::from_utf8(&git.stdout)?.trim());
        let files = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            AsyncProcessBuilder::new("git")
                .kill_on_drop(true)
                .args(["ls-files", "--cached", "--full-name", "-z", "--", "."])
                .current_dir(package)
                .capture(),
        )
        .await
        .map_err(|_| "git package files deadline")??;
        if !files.status.success() {
            return Err("git package source listing failed".into());
        }
        return file_list(&root, package, &files.stdout, 0);
    }
    // Cargo stores the packaged file list for registry sources; avoid nested Cargo locks.
    let checksum = package.join(".cargo-checksum.json");
    if checksum.is_file() {
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(checksum)?)?;
        let files = value
            .get("files")
            .and_then(serde_json::Value::as_object)
            .ok_or("invalid Cargo package file list")?;
        let listed = files.keys().cloned().collect::<Vec<_>>().join("\n");
        return file_list(package, package, listed.as_bytes(), b'\n');
    }
    // Unpacked source archives use Cargo's package rules, offline and with a deadline.
    let files = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        AsyncProcessBuilder::new("soldr")
            .env(soldr_core::self_relocate::SELF_SPAWN_EDGE_ENV_VAR, "1")
            .kill_on_drop(true)
            .args([
                "cargo",
                "package",
                "--list",
                "--allow-dirty",
                "--no-verify",
                "--offline",
            ])
            .current_dir(package)
            .capture(),
    )
    .await
    .map_err(|_| "Cargo package listing deadline")??;
    if !files.status.success() {
        return Err("Cargo package source listing failed".into());
    }
    file_list(package, package, &files.stdout, b'\n')
}

fn file_list(
    root: &Path,
    package: &Path,
    bytes: &[u8],
    separator: u8,
) -> Result<BTreeSet<PathBuf>> {
    let mut paths = BTreeSet::new();
    for value in bytes.split(|b| *b == separator).filter(|p| !p.is_empty()) {
        let path = sandbox::absolute(root, Path::new(std::str::from_utf8(value)?))?;
        if !path.starts_with(package) {
            return Err("package listing escaped its source root".into());
        }
        // Cargo lists generated Cargo.toml.orig and VCS metadata that need not exist yet.
        if path.is_file() {
            paths.insert(path);
        }
    }
    if paths.is_empty() {
        return Err("package source listing is empty".into());
    }
    Ok(paths)
}

fn search_files(args: &[String], cwd: &Path, paths: &mut BTreeSet<PathBuf>) -> Result<()> {
    for (i, arg) in args.iter().enumerate() {
        let external = if arg == "--extern" {
            args.get(i + 1).map(String::as_str)
        } else {
            arg.strip_prefix("--extern=")
        };
        if let Some(value) = external {
            let (_, path) = value
                .split_once('=')
                .ok_or("extern requires an explicit artifact")?;
            paths.insert(sandbox::absolute(cwd, Path::new(path))?);
        }
        let search = if arg == "-L" {
            args.get(i + 1).map(String::as_str)
        } else {
            arg.strip_prefix("-L")
        };
        if let Some(value) = search {
            let path = value.split_once('=').map_or(value, |(_, p)| p);
            let path = sandbox::absolute(cwd, Path::new(path))?;
            // Include transitive metadata/native archives as well as explicit externs.
            // Over-approximation is safe; digest CAS prevents repeated uploads.
            for entry in std::fs::read_dir(path)? {
                let path = entry?.path();
                if path.is_file() {
                    paths.insert(path);
                }
            }
        }
    }
    Ok(())
}

fn tree(root: &Path, paths: &mut BTreeSet<PathBuf>, visited: &mut BTreeSet<PathBuf>) -> Result<()> {
    let canonical = std::fs::canonicalize(root)?;
    if !visited.insert(canonical.clone()) {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            tree(&path, paths, visited)?;
        } else if path.is_file() {
            paths.insert(path);
        }
    }
    visited.remove(&canonical);
    Ok(())
}

/// Check caller-supplied Rust jobs as well as manifests built by the cache adapter.
pub fn validate_environment(job: &mut DistJob) -> Result<()> {
    env::inspect(job).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn construction_rejects_changes_to_inputs_already_in_the_cache_key() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("lib.rs");
            std::fs::write(&source, "pub fn value()->u32 { 42 }\n").unwrap();
            std::fs::write(
                temp.path().join("Cargo.toml"),
                "[package]\nname=\"key_fixture\"\nversion=\"0.0.0\"\n",
            )
            .unwrap();
            std::fs::write(
                temp.path().join(".cargo-checksum.json"),
                r#"{"files":{"lib.rs":"unused","Cargo.toml":"unused"},"package":null}"#,
            )
            .unwrap();
            let known = DigestMemo::default().input(&source).unwrap();
            std::fs::write(&source, "pub fn value()->u32 { 7 }\n").unwrap();
            let job = DistJob {
                cwd: temp.path().to_str().unwrap().into(),
                env: std::collections::HashMap::from([(
                    "CARGO_MANIFEST_DIR".into(),
                    temp.path().to_str().unwrap().into(),
                )]),
                inputs: vec![known],
                ..Default::default()
            };
            let result = construct(job, &tokio::sync::Mutex::new(DigestMemo::default())).await;
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("keyed input changed"));
        });
    }
}
