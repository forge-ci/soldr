//! Construct Rust inputs before a cold cache miss; never require a prior local compile.
use crate::{cas::DigestMemo, sandbox, DistJob, Result};
mod env;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Digest memo shared by every construction in a process.
pub type SharedMemo = Arc<DigestMemo>;

/// Enlarge a hook manifest from package membership, compiler search paths and generated files.
///
/// Listing and hashing are blocking filesystem work, so they run on the blocking
/// pool: done inline they starve the async runtime of the daemon hosting the
/// client, which then cannot accept broker handoffs for the rest of the build.
pub async fn construct(job: DistJob, memo: &SharedMemo) -> Result<DistJob> {
    let memo = memo.clone();
    tokio::task::spawn_blocking(move || construct_blocking(job, &memo))
        .await
        .map_err(|error| format!("dist input construction task failed: {error}"))?
}

fn construct_blocking(mut job: DistJob, memo: &DigestMemo) -> Result<DistJob> {
    let original_inputs = job.inputs.clone();
    let cwd = Path::new(&job.cwd);
    let mut paths = job
        .inputs
        .iter()
        .map(|i| PathBuf::from(&i.abs_path))
        .collect::<BTreeSet<_>>();
    let package = package_root(&job)?;
    paths.extend(package_files(&package)?);
    search_files(&job.argv, cwd, &mut paths)?;
    if let Some(out) = job.env.get("OUT_DIR").filter(|p| !p.is_empty()) {
        tree(
            &sandbox::absolute(cwd, Path::new(out))?,
            &mut paths,
            &mut BTreeSet::new(),
        )?;
    }
    paths.retain(|path| !job.outputs.iter().any(|output| Path::new(output) == path));
    job.inputs = paths
        .into_iter()
        .map(|p| memo.input(&p))
        .collect::<Result<_>>()?;
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

/// Most files a package may contribute before construction declines the unit.
const PACKAGE_FILE_LIMIT: usize = 20_000;
/// Most bytes a package may contribute before construction declines the unit.
const PACKAGE_BYTE_LIMIT: u64 = 2 * 1024 * 1024 * 1024;

/// The package's source files, listed in-process: no `git` or `cargo` child, so
/// construction cannot depend on the daemon's PATH and costs no process spawn
/// per unit.
///
/// Registry sources carry Cargo's exact packaged list. Anything else is walked
/// the way Cargo lists a package outside version control: everything under the
/// package root except the root `target/`, dot-entries and nested packages.
/// `include`/`exclude` are not applied; shipping extra files is safe because
/// the worker's digest store deduplicates them and the dep-info proof checks
/// what the compiler actually read.
fn package_files(package: &Path) -> Result<BTreeSet<PathBuf>> {
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
    let mut paths = BTreeSet::new();
    let mut bytes = 0;
    walk_package(package, package, &mut paths, &mut bytes)?;
    if paths.is_empty() {
        return Err("package source listing is empty".into());
    }
    Ok(paths)
}

fn walk_package(
    package: &Path,
    dir: &Path,
    paths: &mut BTreeSet<PathBuf>,
    bytes: &mut u64,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        // file_type does not follow symlinks: a linked directory is not walked.
        let kind = entry.file_type()?;
        if kind.is_dir() {
            // `path` is never the package root itself, so its own manifest marks a nested package.
            let nested = path.join("Cargo.toml").is_file();
            if (dir == package && name == "target") || nested {
                continue;
            }
            walk_package(package, &path, paths, bytes)?;
        } else if path.is_file() {
            *bytes += entry.metadata()?.len();
            paths.insert(path);
            if paths.len() > PACKAGE_FILE_LIMIT || *bytes > PACKAGE_BYTE_LIMIT {
                return Err(format!(
                    "package {} exceeds the dist input limit ({PACKAGE_FILE_LIMIT} files / {} GiB)",
                    package.display(),
                    PACKAGE_BYTE_LIMIT >> 30
                )
                .into());
            }
        }
    }
    Ok(())
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
    fn package_walk_skips_target_dotfiles_and_nested_packages() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for (path, body) in [
            ("Cargo.toml", "[package]\nname=\"p\"\nversion=\"0.0.0\"\n"),
            ("src/lib.rs", "pub fn f() {}\n"),
            ("src/data/table.txt", "1\n"),
            ("build.rs", "fn main() {}\n"),
            ("target/debug/junk", "x"),
            (".git/HEAD", "ref"),
            ("src/.hidden.rs", "x"),
            (
                "nested/Cargo.toml",
                "[package]\nname=\"n\"\nversion=\"0.0.0\"\n",
            ),
            ("nested/src/lib.rs", "x"),
            (
                "sub/target/keep.rs",
                "kept: only the root target is build output",
            ),
        ] {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let listed: Vec<_> = package_files(root)
            .unwrap()
            .into_iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(
            listed,
            [
                "Cargo.toml",
                "build.rs",
                "src/data/table.txt",
                "src/lib.rs",
                "sub/target/keep.rs"
            ]
        );
    }
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
            let result = construct(job, &SharedMemo::default()).await;
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("keyed input changed"));
        });
    }
}
