use super::*;
use ZccacheModeSource::{Config, SoldrEnv, ZccacheEnv};

fn resolved(mode: ZccacheMode, source: ZccacheModeSource) -> Option<ResolvedZccacheMode> {
    Some(ResolvedZccacheMode { mode, source })
}

#[test]
fn parse_accepts_each_mode_case_and_whitespace_insensitively() {
    for mode in ZccacheMode::ALL {
        assert_eq!(ZccacheMode::parse(mode.as_str()), Ok(Some(mode)));
        let lower = format!("  {}\n", mode.as_str().to_ascii_lowercase());
        assert_eq!(ZccacheMode::parse(&lower), Ok(Some(mode)), "{lower:?}");
    }
}

#[test]
fn parse_treats_empty_as_unset_and_rejects_unknown_words() {
    assert_eq!(ZccacheMode::parse(""), Ok(None));
    assert_eq!(ZccacheMode::parse("   "), Ok(None));
    for raw in ["hardlink", "cow", "1", "on"] {
        assert_eq!(ZccacheMode::parse(raw), Err(UnknownZccacheMode), "{raw}");
    }
}

/// (SOLDR_ZCCACHE_MODE, config.toml, ZCCACHE_MODE, expected)
type PrecedenceRow = (
    Option<&'static str>,
    Option<&'static str>,
    Option<&'static str>,
    Option<ResolvedZccacheMode>,
);

/// Each tier wins only when every higher tier is absent.
#[test]
fn precedence_table() {
    let rows: [PrecedenceRow; 6] = [
        (
            Some("copy"),
            Some("link"),
            Some("reflink"),
            resolved(ZccacheMode::Copy, SoldrEnv),
        ),
        (
            None,
            Some("link"),
            Some("reflink"),
            resolved(ZccacheMode::Link, Config),
        ),
        (
            None,
            None,
            Some("reflink"),
            resolved(ZccacheMode::Reflink, ZccacheEnv),
        ),
        (None, None, None, None),
        (
            Some("auto"),
            None,
            None,
            resolved(ZccacheMode::Auto, SoldrEnv),
        ),
        (
            None,
            Some("REFLINK"),
            None,
            resolved(ZccacheMode::Reflink, Config),
        ),
    ];
    for (soldr, config, zccache, expected) in rows {
        assert_eq!(
            resolve_zccache_mode_from(soldr, config, zccache),
            Ok(expected),
            "soldr={soldr:?} config={config:?} zccache={zccache:?}"
        );
    }
}

#[test]
fn empty_values_fall_through_to_the_next_tier() {
    assert_eq!(
        resolve_zccache_mode_from(Some(""), Some(" "), Some("copy")),
        Ok(resolved(ZccacheMode::Copy, ZccacheEnv))
    );
}

#[test]
fn an_invalid_value_is_an_error_naming_its_source() {
    for (soldr, config, zccache, source) in [
        (Some("bogus"), None, None, SoldrEnv),
        (None, Some("bogus"), None, Config),
        (None, None, Some("bogus"), ZccacheEnv),
    ] {
        let error = resolve_zccache_mode_from(soldr, config, zccache).unwrap_err();
        assert_eq!(error.source, source);
        let message = error.to_string();
        assert!(message.contains(source.describe()), "{message}");
        assert!(message.contains("\"bogus\""), "{message}");
        assert!(
            message.contains("AUTO, LINK, COPY, REFLINK, REFLINK_OR_LINK_OR_COPY"),
            "{message}"
        );
    }
}

/// A higher tier that is valid shadows an invalid lower tier: only the
/// value that would actually be used can fail the build.
#[test]
fn a_winning_tier_shadows_an_invalid_lower_one() {
    assert_eq!(
        resolve_zccache_mode_from(Some("copy"), Some("bogus"), Some("bogus")),
        Ok(resolved(ZccacheMode::Copy, SoldrEnv))
    );
}

#[test]
fn only_soldr_owned_sources_are_injected_on_the_child() {
    let soldr = ResolvedZccacheMode {
        mode: ZccacheMode::Copy,
        source: SoldrEnv,
    };
    let config = ResolvedZccacheMode {
        mode: ZccacheMode::Reflink,
        source: Config,
    };
    let user = ResolvedZccacheMode {
        mode: ZccacheMode::Link,
        source: ZccacheEnv,
    };
    // No AUTO in this set, so the probe must never fire.
    assert_eq!(soldr.child_env_value(unreachable_probe), Some("COPY"));
    assert_eq!(config.child_env_value(unreachable_probe), Some("REFLINK"));
    assert_eq!(user.child_env_value(unreachable_probe), None);
}

#[test]
fn an_explicit_reflink_or_link_or_copy_behaves_like_any_other_explicit_mode() {
    let soldr_owned = ResolvedZccacheMode {
        mode: ZccacheMode::ReflinkOrLinkOrCopy,
        source: SoldrEnv,
    };
    assert_eq!(
        soldr_owned.child_env_value(unreachable_probe),
        Some("REFLINK_OR_LINK_OR_COPY")
    );

    let user_owned = ResolvedZccacheMode {
        mode: ZccacheMode::ReflinkOrLinkOrCopy,
        source: ZccacheEnv,
    };
    assert_eq!(
        user_owned.child_env_value(unreachable_probe),
        None,
        "the user's own explicit REFLINK_OR_LINK_OR_COPY must not be rewritten"
    );
}

// ---------------------------------------------------------------------
// AUTO / unset: soldr probes once and injects the explicit answer
// (zccache#1792, soldr#3440).
// ---------------------------------------------------------------------

fn unreachable_probe() -> ZccacheMode {
    panic!("probe must not run for an explicit non-AUTO mode")
}

#[test]
fn child_env_value_never_probes_for_an_explicit_non_auto_mode() {
    for mode in [
        ZccacheMode::Link,
        ZccacheMode::Copy,
        ZccacheMode::Reflink,
        ZccacheMode::ReflinkOrLinkOrCopy,
    ] {
        for source in [SoldrEnv, Config, ZccacheEnv] {
            let resolved = ResolvedZccacheMode { mode, source };
            // Panics (via unreachable_probe) if the probe fires.
            let value = resolved.child_env_value(unreachable_probe);
            assert_eq!(
                value,
                source.soldr_owned().then(|| mode.as_str()),
                "{mode:?}/{source:?}"
            );
        }
    }
}

#[test]
fn child_env_value_injects_the_probed_mode_for_auto_regardless_of_source() {
    for source in [SoldrEnv, Config, ZccacheEnv] {
        let resolved = ResolvedZccacheMode {
            mode: ZccacheMode::Auto,
            source,
        };
        for probed in [ZccacheMode::Reflink, ZccacheMode::Link, ZccacheMode::Copy] {
            assert_eq!(
                resolved.child_env_value(|| probed),
                Some(probed.as_str()),
                "{source:?} probed={probed:?}"
            );
        }
    }
}

#[test]
fn child_env_value_for_defaults_unset_to_the_probed_mode() {
    assert_eq!(
        child_env_value_for(None, || ZccacheMode::Reflink),
        Some("REFLINK")
    );
    assert_eq!(
        child_env_value_for(None, || ZccacheMode::Link),
        Some("LINK")
    );
    assert_eq!(
        child_env_value_for(None, || ZccacheMode::Copy),
        Some("COPY")
    );
}

/// Full truth table: (resolved mode, probed mode) -> injected env value.
/// The probed mode is irrelevant (and never consulted) outside the
/// unset/AUTO rows.
#[test]
fn child_env_value_for_truth_table() {
    let cases: [(Option<ResolvedZccacheMode>, ZccacheMode, Option<&str>); 11] = [
        (None, ZccacheMode::Reflink, Some("REFLINK")),
        (None, ZccacheMode::Link, Some("LINK")),
        (None, ZccacheMode::Copy, Some("COPY")),
        (
            resolved(ZccacheMode::Auto, SoldrEnv),
            ZccacheMode::Reflink,
            Some("REFLINK"),
        ),
        (
            resolved(ZccacheMode::Auto, Config),
            ZccacheMode::Link,
            Some("LINK"),
        ),
        (
            resolved(ZccacheMode::Auto, ZccacheEnv),
            ZccacheMode::Copy,
            Some("COPY"),
        ),
        (
            resolved(ZccacheMode::Link, SoldrEnv),
            ZccacheMode::Reflink,
            Some("LINK"),
        ),
        (
            resolved(ZccacheMode::Link, ZccacheEnv),
            ZccacheMode::Reflink,
            None,
        ),
        (
            resolved(ZccacheMode::Copy, ZccacheEnv),
            ZccacheMode::Reflink,
            None,
        ),
        (
            resolved(ZccacheMode::Reflink, Config),
            ZccacheMode::Copy,
            Some("REFLINK"),
        ),
        (
            resolved(ZccacheMode::ReflinkOrLinkOrCopy, SoldrEnv),
            ZccacheMode::Copy,
            Some("REFLINK_OR_LINK_OR_COPY"),
        ),
    ];
    for (resolved_mode, probed, expected) in cases {
        assert_eq!(
            child_env_value_for(resolved_mode, || probed),
            expected,
            "{resolved_mode:?} probed={probed:?}"
        );
    }
}

#[test]
fn config_toml_zccache_mode_parses() {
    let parsed: crate::core::SoldrConfig =
        toml::from_str("[zccache]\nmode = \"reflink\"\n").unwrap();
    assert_eq!(parsed.zccache.mode.as_deref(), Some("reflink"));
    let empty: crate::core::SoldrConfig = toml::from_str("").unwrap();
    assert_eq!(empty.zccache, ZccacheModeConfig::default());
}

#[test]
fn zccache_max_size_defaults_to_bounded_cap_and_zero_defers_upstream() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let default: ZccacheModeConfig = toml::from_str("").unwrap();
    assert_eq!(
        default.max_size_bytes(),
        Some(DEFAULT_ZCCACHE_MAX_SIZE_GB * GIB)
    );
    let set: ZccacheModeConfig = toml::from_str("max_size_gb = 10").unwrap();
    assert_eq!(set.max_size_bytes(), Some(10 * GIB));
    let dynamic: ZccacheModeConfig = toml::from_str("max_size_gb = 0").unwrap();
    assert_eq!(dynamic.max_size_bytes(), None);
}
