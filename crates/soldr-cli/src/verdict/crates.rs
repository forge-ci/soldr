//! Per-crate verdicts (soldr#3897).
//!
//! A workspace-wide check whose workspace key misses can still skip the
//! members whose own inputs are unchanged. Each member gets a Merkle key:
//! the inputs shared by the whole workspace, the member's own files, and the
//! keys of the workspace members it depends on, so an edit re-checks the
//! edited crate and everything that depends on it and nothing else.
//!
//! Running a subset (`-p a -p b` instead of `--workspace`) is only sound for
//! a tool whose result for one crate does not depend on which other crates
//! are selected. Cargo unifies features across the selected packages, so a
//! crate can lint or doc-test differently under `-p` than under
//! `--workspace`; rustfmt reads syntax only. Adapters therefore opt in with
//! `per_package = true`, which the built-in table sets for `fmt` alone.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// One workspace member, repo-relative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    /// Directory of the member's `Cargo.toml`, repo-relative (`""` = root).
    pub dir: String,
    /// Names of the workspace members it depends on (any dependency kind).
    pub deps: Vec<String>,
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    manifest_path: String,
    #[serde(default)]
    dependencies: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    name: String,
    #[serde(default)]
    path: Option<String>,
}

/// Parse `cargo metadata --no-deps --format-version 1` into members with
/// repo-relative directories.
pub fn parse_metadata(json: &str, repo_root: &Path) -> Result<Vec<Member>, String> {
    let metadata: Metadata =
        serde_json::from_str(json).map_err(|err| format!("cargo metadata did not parse: {err}"))?;
    let root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let rel = |path: &Path| -> Result<String, String> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let stripped = canonical
            .strip_prefix(&root)
            .map_err(|_| format!("member `{}` is outside the repo", path.display()))?;
        Ok(stripped
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"))
    };
    let members: Vec<&Package> = metadata
        .packages
        .iter()
        .filter(|p| metadata.workspace_members.contains(&p.id))
        .collect();
    let mut dir_of = BTreeMap::new();
    for package in &members {
        let manifest = Path::new(&package.manifest_path);
        let dir = rel(manifest.parent().unwrap_or(manifest))?;
        dir_of.insert(dir, package.name.clone());
    }
    members
        .iter()
        .map(|package| {
            let manifest = Path::new(&package.manifest_path);
            let mut deps: Vec<String> = package
                .dependencies
                .iter()
                .filter_map(|dep| {
                    let path = dep.path.as_deref()?;
                    let dir = rel(Path::new(path)).ok()?;
                    dir_of.get(&dir).cloned().or_else(|| Some(dep.name.clone()))
                })
                .filter(|name| members.iter().any(|m| &m.name == name) && name != &package.name)
                .collect();
            deps.sort();
            deps.dedup();
            Ok(Member {
                name: package.name.clone(),
                dir: rel(manifest.parent().unwrap_or(manifest))?,
                deps,
            })
        })
        .collect()
}

/// The member owning `path`: the one with the longest directory prefix.
pub fn owner<'a>(members: &'a [Member], path: &str) -> Option<&'a Member> {
    members
        .iter()
        .filter(|m| {
            m.dir.is_empty()
                || path
                    .strip_prefix(m.dir.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
        .max_by_key(|m| m.dir.len())
}

/// Merkle keys: `own[m]` is the digest of member `m`'s own inputs; the key
/// folds in its workspace dependencies' keys. A dependency cycle (possible
/// through dev-dependencies) has no well-founded key, so it is an error and
/// the caller falls back to the workspace key.
pub fn merkle(
    members: &[Member],
    own: &BTreeMap<String, String>,
    mut seal: impl FnMut(&str, &str, &[String]) -> String,
) -> Result<BTreeMap<String, String>, String> {
    fn visit(
        name: &str,
        members: &[Member],
        own: &BTreeMap<String, String>,
        seal: &mut dyn FnMut(&str, &str, &[String]) -> String,
        done: &mut BTreeMap<String, String>,
        stack: &mut Vec<String>,
    ) -> Result<String, String> {
        if let Some(key) = done.get(name) {
            return Ok(key.clone());
        }
        if stack.iter().any(|s| s == name) {
            return Err(format!("workspace dependency cycle through `{name}`"));
        }
        let member = members
            .iter()
            .find(|m| m.name == name)
            .ok_or_else(|| format!("unknown member `{name}`"))?;
        stack.push(name.to_string());
        let mut dep_keys = Vec::new();
        for dep in &member.deps {
            dep_keys.push(visit(dep, members, own, seal, done, stack)?);
        }
        stack.pop();
        let key = seal(name, own.get(name).map_or("", String::as_str), &dep_keys);
        done.insert(name.to_string(), key.clone());
        Ok(key)
    }
    let mut done = BTreeMap::new();
    for member in members {
        visit(
            &member.name,
            members,
            own,
            &mut seal,
            &mut done,
            &mut Vec::new(),
        )?;
    }
    Ok(done)
}

/// Whether `args` select the whole workspace and nothing narrower, so a
/// subset can be substituted.
pub fn selects_workspace(args: &[String]) -> bool {
    let before_sep: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .take_while(|a| *a != "--")
        .collect();
    let whole = before_sep
        .iter()
        .any(|a| matches!(*a, "--workspace" | "--all"));
    let narrowed = before_sep.iter().any(|a| {
        matches!(*a, "-p" | "--package" | "--exclude")
            || a.starts_with("--package=")
            || a.starts_with("--exclude=")
    });
    whole && !narrowed
}

/// Replace `--workspace`/`--all` with `-p <name>` for each of `run`.
pub fn narrow(args: &[String], run: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + run.len() * 2);
    let mut done = false;
    let mut after_sep = false;
    for arg in args {
        if arg == "--" {
            after_sep = true;
        }
        if !after_sep && matches!(arg.as_str(), "--workspace" | "--all") {
            if !done {
                for name in run {
                    out.push("-p".into());
                    out.push(name.clone());
                }
                done = true;
            }
            continue;
        }
        out.push(arg.clone());
    }
    out
}
