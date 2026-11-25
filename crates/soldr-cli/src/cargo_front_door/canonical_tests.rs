use super::*;
use crate::cargo_metadata_canonical::{AlternateProfile, CanonicalEnforce};

fn profile() -> CanonicalProfile {
    CanonicalProfile {
        enforce: CanonicalEnforce::Warn,
        features: CanonicalFeatures::Explicit(vec![]),
        default_features: true,
        packages: CanonicalPackages::Workspace,
        exclude: vec![],
        dev_targets: false,
        target: CanonicalTarget::Implicit,
        clippy_args: vec!["-D".into(), "warnings".into()],
        unify: CanonicalUnify::Off,
        alternates: vec![],
    }
}

fn select(verb: CompilingVerb, args: &[&str]) -> Selection {
    Selection::parse(
        verb,
        &args.iter().map(|s| (*s).into()).collect::<Vec<_>>(),
        &["b".into(), "a".into()],
        false,
    )
}

#[test]
fn canonical_target_selection_table() {
    for (verb, flags, dev) in [
        (CompilingVerb::Check, vec!["--workspace"], false),
        (
            CompilingVerb::Build,
            vec!["--workspace", "--lib", "--bins", "--bin=a"],
            false,
        ),
        (CompilingVerb::Test, vec!["--workspace"], true),
        (CompilingVerb::NextestRun, vec!["--workspace"], true),
        // `nextest archive` builds every test binary the archive ships, so it
        // is a bare test verb for target breadth.
        (CompilingVerb::NextestArchive, vec!["--workspace"], true),
        (CompilingVerb::Test, vec!["--workspace", "--lib"], false),
        (
            CompilingVerb::NextestRun,
            vec!["--workspace", "--bins"],
            false,
        ),
    ] {
        let mut profile = profile();
        profile.dev_targets = dev;
        assert!(
            select(verb, &flags).drift(&profile).is_empty(),
            "{verb:?} {flags:?}"
        );
    }
    // Whole-kind selectors widen the run to the dev target kinds; naming one
    // target of a kind is still narrow (`cargo check --test foo` builds that
    // one target, not every test target).
    for flag in ["--all-targets", "--tests", "--benches", "--examples"] {
        let mut profile = profile();
        profile.dev_targets = true;
        assert!(
            select(CompilingVerb::Check, &["--workspace", flag])
                .drift(&profile)
                .is_empty(),
            "{flag}"
        );
    }
    // A single-target selector is narrow: `dev_targets` means "the dev target
    // kinds are selected", so only a whole-kind selector (or a bare test verb)
    // sets it.
    for flag in ["--test", "--bench", "--example"] {
        assert!(!select(CompilingVerb::Build, &["--workspace", flag, "foo"]).dev_targets);
        assert!(
            !select(
                CompilingVerb::Build,
                &["--workspace", &format!("{flag}=foo")]
            )
            .dev_targets
        );
    }
}

#[test]
fn canonical_features_are_sets_and_all_features_match() {
    let mut profile = profile();
    profile.features = CanonicalFeatures::Explicit(vec!["a".into(), "b".into(), "c".into()]);
    for flags in [
        vec!["--workspace", "--features", "c,b a", "-F", "a"],
        vec!["--workspace", "--features=a,b", "-Fc"],
    ] {
        assert!(select(CompilingVerb::Check, &flags)
            .drift(&profile)
            .is_empty());
    }
    profile.features = CanonicalFeatures::All;
    assert!(
        select(CompilingVerb::Check, &["--workspace", "--all-features"])
            .drift(&profile)
            .is_empty()
    );
    assert_eq!(
        select(CompilingVerb::Check, &["--workspace"]).drift(&profile)[0].field,
        "features"
    );
    profile = self::profile();
    profile.default_features = false;
    assert!(select(
        CompilingVerb::Check,
        &["--workspace", "--no-default-features"]
    )
    .drift(&profile)
    .is_empty());
}

#[test]
fn canonical_packages_compare_member_sets_and_exclusions() {
    for flags in [
        vec!["-p", "b", "--package=a", "-pa"],
        vec!["--all"],
        vec!["--workspace"],
    ] {
        assert!(select(CompilingVerb::Check, &flags)
            .drift(&profile())
            .is_empty());
    }
    let mut profile = profile();
    profile.exclude = vec!["b".into()];
    assert!(
        select(CompilingVerb::Check, &["--workspace", "--exclude=b"])
            .drift(&profile)
            .is_empty()
    );
    assert!(select(CompilingVerb::Check, &["-pa"])
        .drift(&profile)
        .is_empty());
    profile.unify = CanonicalUnify::Workspace;
    assert!(select(CompilingVerb::Check, &["-p", "b"])
        .drift(&profile)
        .is_empty());
}

#[test]
fn canonical_explicit_target_and_build_target_environment() {
    let mut profile = profile();
    profile.target = CanonicalTarget::ExplicitHost;
    for flags in [
        vec!["--workspace", "--target", "host"],
        vec!["--workspace", "--target=host"],
    ] {
        assert!(select(CompilingVerb::Check, &flags)
            .drift(&profile)
            .is_empty());
    }
    assert_eq!(
        select(CompilingVerb::Check, &["--workspace"]).drift(&profile)[0].field,
        "target"
    );
    let selection = Selection::parse(
        CompilingVerb::Check,
        &["--workspace".into()],
        &["a".into()],
        true,
    );
    assert!(selection.drift(&profile).is_empty());
}

#[test]
fn canonical_tail_only_affects_clippy() {
    let mut profile = profile();
    profile.dev_targets = true;
    assert!(select(
        CompilingVerb::Test,
        &["--workspace", "--", "--features", "not-a-feature"]
    )
    .drift(&profile)
    .is_empty());
    profile.dev_targets = false;
    assert!(select(
        CompilingVerb::Clippy,
        &["--workspace", "--", "-D", "warnings"]
    )
    .drift(&profile)
    .is_empty());
    let drift = select(
        CompilingVerb::Clippy,
        &["--workspace", "--", "-A", "clippy::all"],
    )
    .drift(&profile);
    assert_eq!(drift[0].field, "clippy args");
    assert_eq!(drift[0].found, "-A clippy::all");
}

#[test]
fn canonical_alternates_inherit_omitted_fields() {
    let mut profile = profile();
    profile.alternates.push(AlternateProfile {
        name: "e2e".into(),
        features: Some(CanonicalFeatures::Explicit(vec!["e2e".into()])),
        packages: Some(CanonicalPackages::Explicit(vec!["a".into()])),
        ..AlternateProfile::default()
    });
    assert!(select(CompilingVerb::Check, &["-pa", "--features=e2e"])
        .drift(&profile)
        .is_empty());
    assert!(!select(CompilingVerb::Check, &["-pa", "--features=other"])
        .drift(&profile)
        .is_empty());
}

#[test]
fn canonical_only_compiling_verbs_are_checked() {
    for verb in [
        "check",
        "clippy",
        "test",
        "build",
        "nextest run",
        "nextest archive",
    ] {
        let args = verb
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert!(CompilingVerb::from_args(&args).is_some());
    }
    for verb in [
        "metadata",
        "tree",
        "fmt",
        "doc",
        "nextest list",
        "nextest show-config",
    ] {
        let args = verb
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert!(CompilingVerb::from_args(&args).is_none());
    }
}

#[test]
fn canonical_empty_package_list_renders_the_default_selection() {
    let mut profile = profile();
    profile.packages = CanonicalPackages::Explicit(vec![]);
    assert_eq!(
        command(&profile, CompilingVerb::Check, "host"),
        "soldr cargo check"
    );
    assert!(select(CompilingVerb::Check, &[]).drift(&profile).is_empty());
}

#[test]
fn canonical_rendered_commands_match_the_same_profile() {
    for verb in [
        CompilingVerb::Check,
        CompilingVerb::Clippy,
        CompilingVerb::Test,
        CompilingVerb::Build,
        CompilingVerb::NextestRun,
        CompilingVerb::NextestArchive,
    ] {
        for dev_targets in [false, true] {
            let mut profile = profile();
            profile.dev_targets = dev_targets;
            profile.target = CanonicalTarget::ExplicitHost;
            profile.features = CanonicalFeatures::All;
            let rendered = command(&profile, verb, "host");
            let args = rendered
                .split_whitespace()
                .skip(2)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let selection = Selection::parse(verb, &args, &["a".into(), "b".into()], false);
            assert!(
                selection.drift(&profile).is_empty(),
                "{rendered}: {:?}",
                selection.drift(&profile)
            );
        }
    }
}

#[test]
fn canonical_dev_targets_tracks_target_breadth() {
    // `dev-targets` is the `--all-targets` axis. A bare test verb builds every
    // test target and therefore matches it; any narrowing does not.
    assert!(select(CompilingVerb::Test, &["--workspace"]).dev_targets);
    assert!(select(CompilingVerb::NextestRun, &["--workspace"]).dev_targets);
    assert!(select(CompilingVerb::NextestArchive, &["--workspace"]).dev_targets);
    assert!(select(CompilingVerb::Build, &["--workspace", "--all-targets"]).dev_targets);
    for args in [
        vec!["--workspace", "--lib"],
        vec!["--workspace", "--bins"],
        vec!["--workspace", "--lib", "--bins"],
    ] {
        assert!(!select(CompilingVerb::Test, &args).dev_targets, "{args:?}");
        assert!(!select(CompilingVerb::Build, &args).dev_targets, "{args:?}");
    }
}
