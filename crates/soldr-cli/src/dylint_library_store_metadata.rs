//! Resolve only lint-library dependency subgraphs, including standalone lint workspaces.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    resolve: Resolve,
}
#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    version: String,
    source: Option<String>,
    manifest_path: PathBuf,
}
#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}
#[derive(Deserialize)]
struct Node {
    id: String,
    dependencies: Vec<String>,
}

fn load(root: &Path) -> Result<Metadata, Box<dyn std::error::Error>> {
    let cargo = crate::binaries::resolve_toolchain_binary("cargo")?;
    let mut command = std::process::Command::new(&cargo);
    command
        .current_dir(root)
        .args(["metadata", "--format-version", "1"]);
    crate::toolchain_dir_name::carry_toolchain_into_probe(&mut command, root);
    command
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER");
    if let Some(rustc) = cargo
        .parent()
        .map(|p| p.join(crate::platform::executable::name::native("rustc")))
        .filter(|p| p.is_file())
    {
        command.env("RUSTC", rustc);
    }
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!(
            "Dylint store metadata for {}: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

pub(super) fn dependency_closure(
    root: &Path,
    libraries: &[PathBuf],
) -> Result<String, Box<dyn std::error::Error>> {
    let root = root.canonicalize()?;
    // A virtual workspace may contain only excluded, standalone lint libraries.
    // Cargo cannot describe an empty workspace; each library still has a graph.
    let workspace = load(&root).ok();
    let mut records = BTreeSet::new();
    for library in libraries {
        let manifest = library.join("Cargo.toml").canonicalize()?;
        // Excluded Dylint libraries commonly declare their own [workspace].
        let standalone;
        let metadata = if let Some(workspace) = workspace.as_ref().filter(|workspace| {
            workspace.packages.iter().any(|p| {
                p.manifest_path
                    .canonicalize()
                    .is_ok_and(|path| path == manifest)
            })
        }) {
            workspace
        } else {
            standalone = load(library)?;
            &standalone
        };
        let package = metadata
            .packages
            .iter()
            .find(|p| {
                p.manifest_path
                    .canonicalize()
                    .is_ok_and(|path| path == manifest)
            })
            .ok_or("declared Dylint library missing from cargo metadata")?;
        collect(metadata, &package.id, &root, &mut records)?;
    }
    Ok(records.into_iter().collect::<Vec<_>>().join("\n"))
}

fn collect(
    metadata: &Metadata,
    start: &str,
    root: &Path,
    records: &mut BTreeSet<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let nodes: BTreeMap<_, _> = metadata
        .resolve
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n))
        .collect();
    let packages: BTreeMap<_, _> = metadata
        .packages
        .iter()
        .map(|p| (p.id.as_str(), p))
        .collect();
    let mut pending = vec![start];
    let mut visited = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id) {
            continue;
        }
        let package = packages.get(id).ok_or("missing resolved package")?;
        // Cargo's path IDs are file URLs (escaped spaces, platform-dependent
        // drive syntax). Construct their equivalent identity from canonical
        // manifests so different worktrees and Windows path spellings agree.
        let id = if package.source.is_none() {
            let manifest = package.manifest_path.canonicalize()?;
            let relative = manifest.strip_prefix(root).unwrap_or(&manifest);
            format!(
                "path+{}#{}@{}",
                relative.to_string_lossy().replace('\\', "/"),
                package.name,
                package.version
            )
        } else {
            package.id.clone()
        };
        records.insert(serde_json::to_string(&(
            id,
            &package.name,
            &package.version,
            &package.source,
        ))?);
        let node = nodes
            .get(package.id.as_str())
            .ok_or("missing resolve node")?;
        pending.extend(node.dependencies.iter().map(String::as_str));
    }
    Ok(())
}
