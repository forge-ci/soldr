//! Enforce the one shipped executable (argv[0] multicall) policy repo-wide.

use crate::common;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

// Compilation inputs, not Soldr products. Keep this path-pattern list explicit:
// **/fixtures/**; examples/docker-*/**; bench/dylint_fixture/**.
// The first covers perf/fixtures, ci/fixtures, and tests/**/fixtures too.
const FIXTURE_PATTERNS: &[&str] = &[
    "**/fixtures/**",
    "examples/docker-*/**",
    "bench/dylint_fixture/**",
];
const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "_vender",
    "vendor",
    "vendored",
    "node_modules",
    ".venv",
];

// Every exception is documented with a reason in CLAUDE.md and AGENTS.md.
// Pin paths as well as names so another package cannot impersonate an exception.
const ALLOWED_BINS: &[(&str, &str, &str)] = &[
    ("crates/soldr-cli/Cargo.toml", "soldr-cli", "soldr"),
    (
        "crates/soldr-nextest-wrapper/Cargo.toml",
        "soldr-nextest-wrapper",
        "soldr-nextest-wrapper",
    ),
    (
        "ci/tokio-console-dump/Cargo.toml",
        "soldr-tokio-console-dump",
        "soldr-tokio-console-dump",
    ),
];

fn is_fixture(path: &Path) -> bool {
    let parts: Vec<_> = path.iter().filter_map(|part| part.to_str()).collect();
    FIXTURE_PATTERNS.iter().any(|pattern| match *pattern {
        "**/fixtures/**" => parts.contains(&"fixtures"),
        "examples/docker-*/**" => {
            parts.first() == Some(&"examples")
                && parts.get(1).is_some_and(|part| part.starts_with("docker-"))
        }
        "bench/dylint_fixture/**" => path.starts_with("bench/dylint_fixture"),
        _ => panic!("unimplemented fixture pattern: {pattern}"),
    })
}

fn collect_manifests(root: &Path, dir: &Path, manifests: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read repository directory") {
        let entry = entry.expect("read repository entry");
        let path = entry.path();
        let kind = entry.file_type().expect("read entry type");
        if kind.is_dir() {
            if !SKIP_DIRS.iter().any(|name| entry.file_name() == *name)
                && !is_fixture(path.strip_prefix(root).expect("repository path"))
            {
                collect_manifests(root, &path, manifests);
            }
        } else if kind.is_file() && entry.file_name() == "Cargo.toml" {
            manifests.push(path);
        }
    }
}

fn binary_names(manifest_path: &Path, manifest: &toml::Value) -> BTreeSet<String> {
    let Some(package) = manifest.get("package") else {
        return BTreeSet::new();
    };
    let package_name = package["name"].as_str().expect("package name");
    let dir = manifest_path.parent().expect("manifest directory");
    let mut names = BTreeSet::new();
    let mut explicit_paths = BTreeSet::new();
    if let Some(bins) = manifest.get("bin").and_then(toml::Value::as_array) {
        for bin in bins {
            if let Some(path) = bin.get("path").and_then(toml::Value::as_str) {
                explicit_paths.insert(dir.join(path));
            }
            names.insert(bin["name"].as_str().expect("binary name").to_owned());
        }
    }
    if package.get("autobins").and_then(toml::Value::as_bool) == Some(false) {
        return names;
    }
    if dir.join("src/main.rs").is_file() && !explicit_paths.contains(&dir.join("src/main.rs")) {
        names.insert(package_name.to_owned());
    }
    let bin_dir = dir.join("src/bin");
    if bin_dir.is_dir() {
        for entry in std::fs::read_dir(bin_dir).expect("read src/bin") {
            let path = entry.expect("read binary entry").path();
            if explicit_paths.contains(&path) || explicit_paths.contains(&path.join("main.rs")) {
                continue;
            }
            if path.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                names.insert(
                    path.file_stem()
                        .expect("binary stem")
                        .to_string_lossy()
                        .into_owned(),
                );
            } else if path.is_dir() && path.join("main.rs").is_file() {
                names.insert(
                    path.file_name()
                        .expect("binary directory")
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    names
}

fn scan_binaries(root: &Path) -> Vec<(String, String, String)> {
    let mut manifests = Vec::new();
    collect_manifests(root, root, &mut manifests);
    let mut binaries = Vec::new();
    for path in manifests {
        let manifest: toml::Value =
            toml::from_str(&std::fs::read_to_string(&path).expect("read Cargo.toml"))
                .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        for name in binary_names(&path, &manifest) {
            binaries.push((
                path.strip_prefix(root)
                    .expect("repository manifest")
                    .to_string_lossy()
                    .replace('\\', "/"),
                manifest["package"]["name"]
                    .as_str()
                    .expect("package name")
                    .to_owned(),
                name,
            ));
        }
    }
    binaries.sort();
    binaries
}

#[test]
fn soldr_is_the_only_shipped_compiled_binary_target() {
    let binaries = scan_binaries(&common::workspace_root());
    let violations: Vec<_> = binaries
        .iter()
        .filter(|(path, package, name)| {
            !ALLOWED_BINS.contains(&(path.as_str(), package.as_str(), name.as_str()))
        })
        .collect();
    assert!(
        violations.is_empty(),
        "One shipped executable (argv[0] multicall) policy violation: {violations:#?}. \
         Use a library crate + soldr subcommand + argv[0] alias of the soldr image. \
         New non-shipped exceptions need a named reason and an owner-approved PR; \
         see CLAUDE.md Key Design Rules."
    );
    assert!(
        binaries.iter().any(|(path, package, name)| {
            (path.as_str(), package.as_str(), name.as_str()) == ALLOWED_BINS[0]
        }),
        "soldr must remain a compiled binary target"
    );
}

fn fixture_file(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().expect("fixture parent")).expect("fixture directory");
    std::fs::write(path, contents).expect("write fixture");
}

#[test]
fn multicall_scanner_finds_explicit_and_implicit_standalone_binaries() {
    let temp = tempfile::tempdir().expect("fixture tempdir");
    let root = temp.path();
    fixture_file(root, "Cargo.toml", "[workspace]\nmembers = []\n");
    fixture_file(root, "standalone/Cargo.toml",
        "[package]\nname = 'standalone'\nversion = '0.0.0'\n[workspace]\n[[bin]]\nname = 'explicit'\npath = 'custom.rs'\n");
    for path in ["src/main.rs", "src/bin/helper.rs", "src/bin/nested/main.rs"] {
        fixture_file(root, &format!("standalone/{path}"), "fn main() {}\n");
    }
    fixture_file(root, "disabled/Cargo.toml",
        "[package]\nname = 'disabled'\nversion = '0.0.0'\nautobins = false\n[[bin]]\nname = 'still-explicit'\npath = 'custom.rs'\n");
    fixture_file(root, "disabled/src/main.rs", "fn main() {}\n");
    fixture_file(root, "disabled/src/bin/ignored.rs", "fn main() {}\n");
    let names: Vec<_> = scan_binaries(root)
        .into_iter()
        .map(|(_, _, name)| name)
        .collect();
    assert_eq!(
        names,
        [
            "still-explicit",
            "explicit",
            "helper",
            "nested",
            "standalone"
        ]
    );
}

#[test]
fn multicall_scanner_ignores_fixtures_and_build_or_vendored_trees() {
    let temp = tempfile::tempdir().expect("fixture tempdir");
    let root = temp.path();
    for dir in [
        "ci/fixtures/input",
        "perf/fixtures/input",
        "tests/deep/fixtures/input",
        "examples/docker-cross-win/crate",
        "bench/dylint_fixture/app",
        "target/generated",
        ".git/input",
        "_vender/input",
        "vendor/input",
        "vendored/input",
    ] {
        fixture_file(
            root,
            &format!("{dir}/Cargo.toml"),
            "[package]\nname = 'fixture'\nversion = '0.0.0'\n[[bin]]\nname = 'fixture-bin'\n",
        );
        fixture_file(root, &format!("{dir}/src/main.rs"), "fn main() {}\n");
    }
    assert!(scan_binaries(root).is_empty());
    fixture_file(
        root,
        "product/Cargo.toml",
        "[package]\nname = 'product'\nversion = '0.0.0'\n",
    );
    fixture_file(root, "product/src/main.rs", "fn main() {}\n");
    assert_eq!(
        scan_binaries(root),
        [(
            "product/Cargo.toml".into(),
            "product".into(),
            "product".into()
        )]
    );
}
