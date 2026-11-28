//! Registry versions are one release train with no reservation fallback.

#[test]
fn rust_and_python_versions_match_and_are_usable() {
    let rust_version = env!("CARGO_PKG_VERSION");
    assert_ne!(
        rust_version, "0.0.0",
        "the yanked/excluded namespace reservation must never be rebuilt"
    );

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let pyproject = std::fs::read_to_string(root.join("pyproject.toml")).expect("pyproject.toml");
    let python_package = std::fs::read_to_string(root.join("python/kernal_api/__init__.py"))
        .expect("Python package");

    let manifest_spelling = format!("version = \"{rust_version}\"");
    let python_spelling = format!("__version__ = \"{rust_version}\"");
    assert!(
        pyproject
            .lines()
            .any(|line| line.trim() == manifest_spelling),
        "pyproject.toml must carry the exact Rust version {rust_version}"
    );
    assert!(
        python_package
            .lines()
            .any(|line| line.trim() == python_spelling),
        "Python import metadata must carry the exact Rust version {rust_version}"
    );
}

/// Utility crates that are commonly already in a consumer's graph take caret
/// requirements: an exact pin forces the consumer's lockfile onto this one
/// release or fails resolution outright (#548). Additions need the same case.
const CARET_REQUIREMENTS: &[&str] = &["dirs", "globset", "jwalk", "tempfile"];

#[test]
fn every_direct_dependency_requirement_is_exact() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("Cargo.toml");
    let mut dependency_section = false;

    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            dependency_section = line
                .trim_matches(['[', ']'])
                .split('.')
                .next_back()
                .is_some_and(|section| {
                    matches!(
                        section,
                        "dependencies" | "dev-dependencies" | "build-dependencies"
                    )
                });
            continue;
        }
        if !dependency_section || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, declaration)) = line.split_once('=') else {
            continue;
        };
        let declaration = declaration.trim();
        if CARET_REQUIREMENTS.contains(&name.trim()) && declaration.starts_with('{') {
            assert!(
                !declaration.contains("version = \"="),
                "dependency {} is listed as caret but pinned exactly: {line}",
                name.trim()
            );
            continue;
        }
        if declaration.starts_with('"') {
            assert!(
                declaration.starts_with("\"="),
                "dependency {} must use an exact requirement: {line}",
                name.trim()
            );
        } else if declaration.starts_with('{') && declaration.contains("version") {
            assert!(
                declaration.contains("version = \"="),
                "dependency {} must use an exact requirement: {line}",
                name.trim()
            );
        }
    }
}
