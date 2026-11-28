//! Immutable Clang resources for Linux-hosted MSVC (Refs #4167).
//!
//! Compose in a private staging tree, then rename under sdk/. Never overlay a
//! running LLVM installation: other slots may be using its Linux/Darwin files.
use std::path::{Path, PathBuf};

use kernal_api::hash::Sha256Hasher;

use super::manifest_lookup::{self, ManifestEntry, ManifestIndex};
use crate::core::{SoldrError, SoldrPaths};

const FORMAT: &str = "msvc-resources-v1";
const BUILTINS: &str = "lib/windows/clang_rt.builtins-x86_64.lib";
/// The verified catalogue unit a published tree was composed from, so a
/// restored tree can be re-verified without the catalogue (soldr#4174).
const UNIT_RECORD: &str = "catalogue-unit";

fn select_unit(index: &ManifestIndex) -> Result<ManifestEntry, SoldrError> {
    let asset = format!(
        "compiler-rt/{}/windows-x64/bundle.tar.zst",
        super::MANAGED_LLVM_VERSION
    );
    index
        .lookup("zackees", "soldr-toolchain", "assets", &asset)
        .filter(|entry| entry.source_path.as_deref() == Some(asset.as_str()))
        .cloned()
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "MSVC requires verified catalogue unit {asset}; missing or mismatched identity (Refs #4167)"
            ))
        })
}

/// The returned directory is passed explicitly to both clang driver modes.
/// ARM64 keeps the existing resources until a matching builtins unit is published.
pub async fn ensure(
    paths: &SoldrPaths,
    llvm_bin: &Path,
    target: &str,
) -> Result<PathBuf, SoldrError> {
    let major = super::MANAGED_LLVM_VERSION.split('.').next().unwrap();
    let resource = llvm_bin
        .join(format!("../lib/clang/{major}"))
        .canonicalize()?;
    if target != "x86_64-pc-windows-msvc" {
        return Ok(resource);
    }
    let index = manifest_lookup::get_or_fetch().await;
    ensure_from_index(paths, &resource, &index).await
}

async fn ensure_from_index(
    paths: &SoldrPaths,
    resource: &Path,
    index: &ManifestIndex,
) -> Result<PathBuf, SoldrError> {
    let parent = paths.root.join("sdk/windows-msvc/clang-resources");
    let relative = "package/lib/clang/21";
    // soldr#4174: with no catalogue at all (offline, air-gapped, restored
    // prepared state), reuse a tree an earlier online run verified. A
    // reachable catalogue keeps the strict #4167 identity check.
    let unit = match select_unit(index) {
        Ok(unit) => unit,
        Err(error) if index.entries.is_empty() => {
            return recorded_tree(&parent, resource, relative).ok_or(error);
        }
        Err(error) => return Err(error),
    };
    let record = unit_record(&unit.asset, &unit.sha256, unit.size_bytes);
    let identity = tree_identity(resource, &record);
    let _lock = super::syslib_common::acquire_install_lock(&parent, &identity)?;
    let parent = parent.canonicalize()?;
    let root = parent.join(&identity);
    if root.exists() {
        validate(&root, relative, &identity)?;
        return Ok(root.join(relative));
    }
    let downloaded = manifest_lookup::materialize_catalogue_entry(paths, &unit).await?;
    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(&parent)?;
    copy_resources(resource, &staging.path().join(relative))?;
    // Keep the producer's license and manifest, and allow only its Windows
    // runtime subtree to join the composed resources. Headers remain LLVM's.
    let input = staging.path().join("compiler-rt-input");
    std::fs::create_dir(&input)?;
    let decoder = zstd::stream::read::Decoder::new(std::fs::File::open(downloaded.path())?)?;
    super::tar_extract::unpack_tar(&mut tar::Archive::new(decoder), &input)?;
    let overlay = input.join(relative).join("lib/windows");
    copy_resources(&overlay, &staging.path().join(relative).join("lib/windows"))?;
    std::fs::write(staging.path().join(UNIT_RECORD), &record)?;
    std::fs::write(staging.path().join(".complete"), &identity)?;
    validate(staging.path(), relative, &identity)?;
    crate::platform::fs::permissions::restore_mode(staging.path(), Some(0o755))?;
    std::fs::rename(staging.path(), &root)?;
    eprintln!(
        "soldr: trust: verified {} sha256={}",
        unit.asset,
        downloaded.sha256()
    );
    Ok(root.join(relative))
}

/// Line-oriented so the identity below hashes exactly the pre-#4174 input.
fn unit_record(asset: &str, sha256: &str, size_bytes: u64) -> String {
    format!("{asset}\n{sha256}\n{size_bytes}")
}

fn tree_identity(resource: &Path, record: &str) -> String {
    Sha256Hasher::digest(format!("{FORMAT}\n{}\n{record}", resource.display())).to_hex()
}

/// A published tree for this LLVM resource dir whose recorded unit is the
/// expected asset and hashes to the tree's own name. The tree's content was
/// verified against that unit's sha256 when it was published.
fn recorded_tree(parent: &Path, resource: &Path, relative: &str) -> Option<PathBuf> {
    let asset = format!(
        "compiler-rt/{}/windows-x64/bundle.tar.zst",
        super::MANAGED_LLVM_VERSION
    );
    // Canonicalize like the online path does, so a reused tree resolves to
    // the same path it was verified at (on Windows the `\\?\` verbatim form).
    let parent = parent.canonicalize().ok()?;
    for entry in std::fs::read_dir(&parent).ok()?.flatten() {
        let root = entry.path();
        let Ok(record) = std::fs::read_to_string(root.join(UNIT_RECORD)) else {
            continue;
        };
        let identity = tree_identity(resource, &record);
        if record.lines().next() == Some(asset.as_str())
            && entry.file_name().to_str() == Some(identity.as_str())
            && validate(&root, relative, &identity).is_ok()
        {
            eprintln!(
                "soldr: trust: reused {asset} verified at publish (catalogue unreachable; soldr#4174)"
            );
            return Some(root.join(relative));
        }
    }
    None
}

fn validate(root: &Path, relative: &str, identity: &str) -> Result<(), SoldrError> {
    let resource = root.join(relative);
    if std::fs::read_to_string(root.join(".complete"))
        .ok()
        .as_deref()
        != Some(identity)
        || !resource.join("include/intrin.h").is_file()
        || std::fs::metadata(resource.join(BUILTINS)).map_or(true, |m| m.len() == 0)
    {
        return Err(SoldrError::Other(format!(
            "incomplete MSVC Clang resources {}; move it aside and retry",
            root.display()
        )));
    }
    Ok(())
}

fn copy_resources(source: &Path, dest: &Path) -> Result<(), SoldrError> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        // Materialize links too: an absolute link in the input must never let
        // the overlay write back into the shared original resource directory.
        if from.is_dir() {
            copy_resources(&from, &to)?;
        } else {
            std::fs::copy(from, to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "msvc_resources_tests.rs"]
mod tests;
