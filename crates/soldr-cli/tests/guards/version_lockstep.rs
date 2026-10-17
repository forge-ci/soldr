//! Regression test for soldr#1026 — the v0.7.65 release trap.
//!
//! A release PR bumps `Cargo.toml`'s `[workspace.package].version`,
//! `package.json`'s top-level `"version"`, and `Cargo.lock`'s
//! `soldr-cli` `version` in lockstep. If any of the three drifts —
//! historically `Cargo.lock` was the one a human contributor forgot —
//! the CI lanes that pass `--locked` fail immediately with
//!
//!     error: cannot update the lock file because --locked was passed
//!     to prevent this
//!
//! That kills the whole release matrix before any code compiles. The
//! v0.7.65 run (#28338586417) burned for ~10 minutes diagnosing this
//! on a Friday afternoon — exactly the failure shape this test
//! prevents.
//!
//! The test is a single integration binary (lives under `tests/` so it
//! gets its own process, no race with other lib tests touching env)
//! and runs on every `cargo test` invocation.

use std::fs;
use std::path::{Path, PathBuf};

use crate::common;

/// Locate the soldr repo root from the CARGO_MANIFEST_DIR convention.
/// `CARGO_MANIFEST_DIR` points at `<repo>/crates/soldr-cli`; we want
/// `<repo>/`.
fn repo_root() -> PathBuf {
    common::workspace_root()
}

/// Extract `[workspace.package] version = "..."` from the repo's
/// root `Cargo.toml`. Hand-parsed (no `toml` crate dep here) because
/// the file shape is stable and we only need one field.
fn read_cargo_toml_version(root: &Path) -> String {
    let s = fs::read_to_string(root.join("Cargo.toml")).expect("read Cargo.toml");
    let mut in_section = false;
    for raw in s.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_section = line == "[workspace.package]";
            continue;
        }
        if in_section {
            if let Some(rest) = line.strip_prefix("version") {
                let rest = rest.trim_start_matches([' ', '=']);
                let rest = rest.trim();
                if let Some(stripped) = rest.strip_prefix('"') {
                    if let Some(end) = stripped.find('"') {
                        return stripped[..end].to_string();
                    }
                }
            }
        }
    }
    panic!("could not find [workspace.package].version in Cargo.toml");
}

/// Extract `name = "soldr-cli"` package's `version` from Cargo.lock.
/// Walks each `[[package]]` block until name matches, then reads the
/// adjacent `version = "..."` line. The lockfile shape is set by
/// cargo and stable across Rust 1.x.
fn read_cargo_lock_version(root: &Path) -> String {
    let s = fs::read_to_string(root.join("Cargo.lock")).expect("read Cargo.lock");
    let mut current_name: Option<String> = None;
    for raw in s.lines() {
        let line = raw.trim();
        if line == "[[package]]" {
            current_name = None;
            continue;
        }
        if let Some(rest) = line.strip_prefix("name") {
            let rest = rest.trim_start_matches([' ', '=']).trim();
            if let Some(stripped) = rest.strip_prefix('"') {
                if let Some(end) = stripped.find('"') {
                    current_name = Some(stripped[..end].to_string());
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("version") {
            if current_name.as_deref() == Some("soldr-cli") {
                let rest = rest.trim_start_matches([' ', '=']).trim();
                if let Some(stripped) = rest.strip_prefix('"') {
                    if let Some(end) = stripped.find('"') {
                        return stripped[..end].to_string();
                    }
                }
            }
        }
    }
    panic!("could not find soldr-cli's version entry in Cargo.lock");
}

/// Extract the top-level `"version"` field from package.json. Same
/// rationale as the Cargo.toml parser: shape is stable, no need to
/// pull in a JSON crate.
fn read_package_json_version(root: &Path) -> String {
    let s = fs::read_to_string(root.join("package.json")).expect("read package.json");
    for raw in s.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("\"version\"") {
            let rest = rest.trim_start_matches([' ', ':']).trim();
            if let Some(stripped) = rest.strip_prefix('"') {
                if let Some(end) = stripped.find('"') {
                    return stripped[..end].to_string();
                }
            }
        }
    }
    panic!("could not find top-level \"version\" in package.json");
}

fn read_section_lines(manifest: &Path, section: &str) -> Vec<String> {
    let s = fs::read_to_string(manifest).unwrap_or_else(|e| {
        panic!("read {}: {e}", manifest.display());
    });
    let header = format!("[{section}]");
    let mut in_section = false;
    let mut lines = Vec::new();
    for raw in s.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_section = line == header;
            continue;
        }
        if in_section && !line.is_empty() {
            lines.push(line.to_string());
        }
    }
    lines
}

fn dependency_line(manifest: &Path, section: &str, dep: &str) -> Option<String> {
    read_section_lines(manifest, section)
        .into_iter()
        .find(|line| {
            line.split_once('=')
                .is_some_and(|(name, _)| name.trim() == dep)
        })
}

fn extract_dependency_version(line: &str) -> Option<String> {
    let (_, rhs) = line.split_once('=')?;
    let rhs = rhs.trim();
    if let Some(stripped) = rhs.strip_prefix('"') {
        let end = stripped.find('"')?;
        return Some(stripped[..end].to_string());
    }
    let version_idx = rhs.find("version")?;
    let version_rhs = rhs[version_idx + "version".len()..]
        .trim_start_matches([' ', '='])
        .trim();
    let stripped = version_rhs.strip_prefix('"')?;
    let end = stripped.find('"')?;
    Some(stripped[..end].to_string())
}

/// A single `[[package]]` block from `Cargo.lock`.
struct LockPackage {
    name: String,
    version: String,
    /// Raw `dependencies = [...]` entries: either a bare crate name (only
    /// one version of it is locked) or `"<name> <version>"` when Cargo
    /// needed to disambiguate.
    dependencies: Vec<String>,
}

/// Hand-parse every `[[package]]` block in `Cargo.lock`. Same rationale as
/// the other parsers in this file: the lockfile shape is stable, no need
/// for a TOML crate dependency here.
fn parse_cargo_lock(lock: &str) -> Vec<LockPackage> {
    let mut packages = Vec::new();
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;
    let mut dependencies: Vec<String> = Vec::new();
    let mut in_deps = false;

    let flush = |name: &mut Option<String>,
                 version: &mut Option<String>,
                 dependencies: &mut Vec<String>,
                 packages: &mut Vec<LockPackage>| {
        if let (Some(n), Some(v)) = (name.take(), version.take()) {
            packages.push(LockPackage {
                name: n,
                version: v,
                dependencies: std::mem::take(dependencies),
            });
        }
        dependencies.clear();
    };

    for raw in lock.lines() {
        let line = raw.trim();
        if line == "[[package]]" {
            flush(&mut name, &mut version, &mut dependencies, &mut packages);
            in_deps = false;
            continue;
        }
        if in_deps {
            if line == "]" {
                in_deps = false;
                continue;
            }
            if let Some(stripped) = line.strip_prefix('"') {
                if let Some(end) = stripped.find('"') {
                    dependencies.push(stripped[..end].to_string());
                }
            }
            continue;
        }
        if line == "dependencies = [" {
            in_deps = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("name") {
            let rest = rest.trim_start_matches([' ', '=']).trim();
            if let Some(stripped) = rest.strip_prefix('"') {
                if let Some(end) = stripped.find('"') {
                    name = Some(stripped[..end].to_string());
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("version") {
            let rest = rest.trim_start_matches([' ', '=']).trim();
            if let Some(stripped) = rest.strip_prefix('"') {
                if let Some(end) = stripped.find('"') {
                    version = Some(stripped[..end].to_string());
                }
            }
        }
    }
    flush(&mut name, &mut version, &mut dependencies, &mut packages);
    packages
}

/// A `Cargo.lock` `dependencies = [...]` entry is either a bare crate name
/// (exactly one version of it is locked, so the caller's own single locked
/// version applies) or `"<name> <version>"` (Cargo had to disambiguate).
/// Returns the dependency's resolved version given the single locked
/// version of that crate, if there is exactly one.
fn resolved_dependency_version<'a>(entry: &'a str, sole_locked_version: &'a str) -> &'a str {
    match entry.split_once(' ') {
        Some((_, version)) => version,
        None => sole_locked_version,
    }
}

/// soldr#3297: soldr and the locked `zccache` must resolve one shared
/// `kernal-api` version, and soldr's `running-process` pin must equal the
/// version that `kernal-api` itself requires.
#[test]
fn kernal_api_version_matches_across_the_fleet() {
    let root = repo_root();
    let lock = fs::read_to_string(root.join("Cargo.lock")).expect("read Cargo.lock");
    let packages = parse_cargo_lock(&lock);

    // (i) exactly one kernal-api version is locked.
    let kernal_versions: Vec<&str> = packages
        .iter()
        .filter(|p| p.name == "kernal-api")
        .map(|p| p.version.as_str())
        .collect();
    assert_eq!(
        kernal_versions.len(),
        1,
        "soldr#3297: Cargo.lock must resolve exactly one kernal-api version; found {kernal_versions:?}. \
         Two kernal-api versions in one graph defeats the point of the shared systems facade."
    );
    let locked_kernal = kernal_versions[0];

    // (ii) soldr's manifest pin equals the locked version.
    let manifest = root.join("crates/soldr-platform/Cargo.toml");
    let line = dependency_line(&manifest, "dependencies", "kernal-api")
        .expect("crates/soldr-platform/Cargo.toml must depend on kernal-api");
    let manifest_kernal = extract_dependency_version(&line).expect(
        "crates/soldr-platform/Cargo.toml's kernal-api dependency must pin an exact version",
    );
    let expected_manifest_kernal = format!("={locked_kernal}");
    assert_eq!(
        manifest_kernal, expected_manifest_kernal,
        "soldr#3297: crates/soldr-platform/Cargo.toml pins kernal-api {manifest_kernal} but \
         Cargo.lock resolved kernal-api {locked_kernal}. Update the manifest pin to \
         \"{expected_manifest_kernal}\" (or refresh Cargo.lock) so both agree."
    );

    // (iii) the locked zccache and soldr use the same kernal-api version.
    if let Some(zccache) = packages.iter().find(|p| p.name == "zccache") {
        if let Some(dep_entry) = zccache
            .dependencies
            .iter()
            .find(|d| d.split(' ').next() == Some("kernal-api"))
        {
            let zccache_kernal = resolved_dependency_version(dep_entry, locked_kernal);
            assert_eq!(
                zccache_kernal, locked_kernal,
                "soldr#3297: the locked zccache package depends on kernal-api {zccache_kernal} but \
                 soldr pins kernal-api {locked_kernal} in crates/soldr-platform/Cargo.toml. Move \
                 soldr's kernal-api pin to {zccache_kernal} (soldr#3297 Step 2 — the zccache bump)."
            );
        }
    }

    // (iv) exactly one running-process version is locked, and it equals
    // the version kernal-api itself requires.
    let running_process_versions: Vec<&str> = packages
        .iter()
        .filter(|p| p.name == "running-process")
        .map(|p| p.version.as_str())
        .collect();
    assert_eq!(
        running_process_versions.len(),
        1,
        "soldr#3297: Cargo.lock must resolve exactly one running-process version; found \
         {running_process_versions:?}."
    );
    let locked_running_process = running_process_versions[0];

    let kernal_package = packages
        .iter()
        .find(|p| p.name == "kernal-api" && p.version == locked_kernal)
        .expect("the locked kernal-api package block must exist in Cargo.lock");
    let kernal_running_process_entry = kernal_package
        .dependencies
        .iter()
        .find(|d| d.split(' ').next() == Some("running-process"))
        .expect("kernal-api's Cargo.lock entry must depend on running-process");
    let kernal_running_process =
        resolved_dependency_version(kernal_running_process_entry, locked_running_process);
    assert_eq!(
        kernal_running_process, locked_running_process,
        "soldr#3297: kernal-api {locked_kernal} depends on running-process {kernal_running_process} \
         but Cargo.lock resolved running-process {locked_running_process} for soldr. Align soldr's \
         running-process pin (crates/soldr-cli, crates/soldr-daemon, crates/soldr-platform \
         Cargo.toml) to \"={kernal_running_process}\"."
    );
}

#[test]
fn cargo_toml_cargo_lock_and_package_json_share_one_version() {
    let root = repo_root();
    let cargo_toml = read_cargo_toml_version(&root);
    let cargo_lock = read_cargo_lock_version(&root);
    let package_json = read_package_json_version(&root);

    assert_eq!(
        cargo_toml, cargo_lock,
        "soldr#1026 v0.7.65 trap: Cargo.toml says {cargo_toml}, Cargo.lock says \
             {cargo_lock}. CI runs `cargo build --locked` and will fail with `cannot update \
             the lock file` until you refresh Cargo.lock (run `cargo build -p soldr-cli` \
             after bumping Cargo.toml and `git add Cargo.lock`). See CLAUDE.md \
             'Bumping soldr's own version (release PRs)'."
    );
    assert_eq!(
        cargo_toml, package_json,
        "soldr#1026 lockstep drift: Cargo.toml says {cargo_toml}, package.json says \
             {package_json}. The release pipeline reads BOTH — the npm-side wrapper bumps off \
             package.json and the crate bumps off Cargo.toml. Update package.json's top-level \
             \"version\" to match."
    );
}

#[test]
fn externalized_dependencies_are_exact_and_consistent() {
    let root = repo_root();
    let lock = fs::read_to_string(root.join("Cargo.lock"))
        .expect("read Cargo.lock")
        .replace("\r\n", "\n");

    // zccache has exactly one pin: the root `[workspace.dependencies]` entry.
    // Members inherit it with `workspace = true` and must not carry a version.
    let workspace_manifest = root.join("Cargo.toml");
    let workspace_line = dependency_line(&workspace_manifest, "workspace.dependencies", "zccache")
        .expect("root [workspace.dependencies] must pin zccache");
    assert_eq!(
        extract_dependency_version(&workspace_line).as_deref(),
        Some("=1.15.5"),
        "the workspace must pin the exact released zccache version"
    );
    assert!(
        !workspace_line.contains("path") && !workspace_line.contains("git"),
        "workspace zccache must resolve from the registry: {workspace_line}"
    );
    for relative in [
        "crates/soldr-cli/Cargo.toml",
        "crates/soldr-cache/Cargo.toml",
        "crates/soldr-daemon/Cargo.toml",
    ] {
        let line = dependency_line(&root.join(relative), "dependencies", "zccache")
            .unwrap_or_else(|| panic!("{relative} must depend on zccache"));
        assert!(
            line.contains("workspace = true") && !line.contains("version"),
            "{relative} must inherit the one workspace zccache pin: {line}"
        );
    }

    for (dependency, version, manifests) in [
        ("zccache", "1.15.5", &[][..]),
        (
            "running-process",
            "4.10.16",
            &[
                "crates/soldr-cli/Cargo.toml",
                "crates/soldr-daemon/Cargo.toml",
                "crates/soldr-platform/Cargo.toml",
            ][..],
        ),
        (
            "kernal-api",
            "0.1.26",
            &["crates/soldr-platform/Cargo.toml"][..],
        ),
    ] {
        let locked = format!("name = \"{dependency}\"\nversion = \"{version}\"");
        assert!(
            lock.contains(&locked),
            "Cargo.lock must resolve {dependency} {version}"
        );
        for relative in manifests {
            let manifest = root.join(relative);
            let line = dependency_line(&manifest, "dependencies", dependency)
                .unwrap_or_else(|| panic!("{relative} must depend on {dependency}"));
            let expected = format!("={version}");
            assert_eq!(
                extract_dependency_version(&line).as_deref(),
                Some(expected.as_str()),
                "{relative} must pin the exact released {dependency} version"
            );
            assert!(
                !line.contains("path") && !line.contains("git"),
                "{relative} must resolve {dependency} from the registry: {line}"
            );
        }
    }
}

#[test]
fn external_zccache_profiles_bound_internal_codegen_parallelism() {
    let root = repo_root();
    let manifest = fs::read_to_string(root.join("Cargo.toml")).expect("read workspace manifest");

    for profile in [
        "dev",
        "test",
        "release",
        "ci-bootstrap",
        "ci-release",
        "ci-nextest",
    ] {
        let section = format!("profile.{profile}.package.zccache");
        let lines = read_section_lines(&root.join("Cargo.toml"), &section);
        assert!(
            lines.iter().any(|line| line == "codegen-units = 1"),
            "[{section}] must keep the amalgamated zccache unit single-codegen"
        );
    }
    assert!(manifest.contains("[profile.dev.package.zccache]"));
}
