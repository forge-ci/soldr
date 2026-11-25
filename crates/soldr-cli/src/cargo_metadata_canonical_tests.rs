use super::*;

fn parse(body: &str) -> Result<CanonicalProfile, SoldrError> {
    CanonicalProfile::parse(toml::from_str(body).unwrap())
}

#[test]
fn reads_canonical_profile_and_unify_defaults_to_auto() {
    let profile = parse("").unwrap();
    assert_eq!(profile.enforce, CanonicalEnforce::Warn);
    assert_eq!(profile.features, CanonicalFeatures::Explicit(vec![]));
    assert!(profile.default_features && profile.dev_targets);
    assert_eq!(profile.packages, CanonicalPackages::Workspace);
    assert_eq!(profile.target, CanonicalTarget::ExplicitHost);
    assert_eq!(profile.clippy_args, ["-D", "warnings"]);
    assert_eq!(profile.unify, CanonicalUnify::Auto);
    let profile = parse(
        r#"features = ["b", "a", "a"]
unify = "workspace"
[alternates.e2e]
features = ["e2e"]
packages = ["a"]
dev-targets = false
clippy-args = []"#,
    )
    .unwrap();
    assert_eq!(
        profile.features,
        CanonicalFeatures::Explicit(vec!["a".into(), "b".into()])
    );
    assert_eq!(profile.unify, CanonicalUnify::Workspace);
    assert_eq!(profile.alternates[0].name, "e2e");
    assert_eq!(profile.alternates[0].dev_targets, Some(false));
}

#[test]
fn canonical_profile_present_absent_fallback_and_workspace_precedence() {
    for (body, expected) in [
        ("[package]\nname = 'a'", None),
        ("[workspace]\nmembers = []", None),
        ("[package.metadata.soldr.cargo]\nenforce = 'refuse'", Some(CanonicalEnforce::Refuse)),
        ("[package.metadata.soldr.cargo]\nenforce = 'refuse'\n[workspace.metadata.soldr.cargo]\nenforce = 'off'", Some(CanonicalEnforce::Off)),
        ("[workspace.metadata.soldr]\ntargets = []\n[package.metadata.soldr.cargo]\nenforce = 'refuse'", None),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), body).unwrap();
        assert_eq!(profile_from(dir.path()).unwrap().map(|(p, _)| p.enforce), expected, "{body}");
    }
    let dir = tempfile::tempdir().unwrap();
    assert!(profile_from(dir.path()).unwrap().is_none());
}

#[test]
fn canonical_profile_finds_workspace_from_member_and_explicit_pointer() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    let member = root.join("a");
    std::fs::create_dir_all(&member).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace.metadata.soldr.cargo]\nfeatures = 'all'",
    )
    .unwrap();
    std::fs::write(member.join("Cargo.toml"), "[package]\nname = 'a'").unwrap();
    assert_eq!(
        profile_from(&member).unwrap().unwrap().1,
        root.join("Cargo.toml")
    );
    let detached = dir.path().join("detached");
    std::fs::create_dir(&detached).unwrap();
    std::fs::write(
        detached.join("Cargo.toml"),
        "[package]\nworkspace = '../workspace'",
    )
    .unwrap();
    assert_eq!(
        profile_from(&detached).unwrap().unwrap().0.features,
        CanonicalFeatures::All
    );
}

#[test]
fn canonical_profile_rejects_invalid_combinations_and_unknown_keys() {
    for (body, fragment) in [
        (
            "features = 'all'\ndefault-features = false",
            "default-features",
        ),
        ("packages = ['a']\nexclude = ['b']", "exclude"),
        ("packages = ['a']\nexclude = []", "exclude"),
        ("typo = true", "typo"),
        ("[alternates.e2e]\ntypo = true", "typo"),
        (
            "default-features = false\n[alternates.e2e]\nfeatures = 'all'",
            "default-features",
        ),
    ] {
        assert!(
            parse(body).unwrap_err().to_string().contains(fragment),
            "{body}"
        );
    }
}

#[test]
fn canonical_profile_enum_errors_name_the_key_and_allowed_values() {
    for (key, allowed) in [
        ("enforce", vec!["off", "warn", "refuse"]),
        ("features", vec!["all", "list"]),
        ("packages", vec!["workspace", "list"]),
        ("target", vec!["explicit-host", "implicit"]),
        ("unify", vec!["auto", "workspace", "off"]),
    ] {
        let error = parse(&format!("{key} = 'typo'")).unwrap_err().to_string();
        assert!(error.contains(key), "{error}");
        for value in allowed {
            assert!(error.contains(value), "{error}");
        }
    }
}

#[test]
fn canonical_profile_reports_malformed_manifest() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "[broken").unwrap();
    assert!(profile_from(dir.path())
        .unwrap_err()
        .to_string()
        .contains("Cargo.toml"));
}
