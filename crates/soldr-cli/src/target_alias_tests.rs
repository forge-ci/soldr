use super::*;

#[test]
fn canonical_alias_resolves_to_rust_triple() {
    let r = resolve_soldr_target("win-x64").unwrap();
    assert_eq!(r.rust_triple, "x86_64-pc-windows-msvc");
    assert!(r.via_alias);
    let r = resolve_soldr_target("mac-arm64").unwrap();
    assert_eq!(r.rust_triple, "aarch64-apple-darwin");
    assert!(r.via_alias);
}

#[test]
fn synonym_resolves_to_canonical_triple() {
    let r = resolve_soldr_target("apple-silicon").unwrap();
    assert_eq!(r.rust_triple, "aarch64-apple-darwin");
    let r = resolve_soldr_target("musl-x64").unwrap();
    assert_eq!(r.rust_triple, "x86_64-unknown-linux-musl");
    let r = resolve_soldr_target("linux-amd64-musl").unwrap();
    assert_eq!(r.rust_triple, "x86_64-unknown-linux-musl");
}

#[test]
fn rust_triple_passthrough() {
    let r = resolve_soldr_target("x86_64-pc-windows-msvc").unwrap();
    assert_eq!(r.rust_triple, "x86_64-pc-windows-msvc");
    assert!(!r.via_alias);
    let r = resolve_soldr_target("wasm32-unknown-unknown").unwrap();
    assert_eq!(r.rust_triple, "wasm32-unknown-unknown");
    assert!(!r.via_alias);
}

#[test]
fn case_insensitivity() {
    let r = resolve_soldr_target("WIN-X64").unwrap();
    assert_eq!(r.rust_triple, "x86_64-pc-windows-msvc");
    let r = resolve_soldr_target("Apple-Silicon").unwrap();
    assert_eq!(r.rust_triple, "aarch64-apple-darwin");
}

#[test]
fn ambiguous_arm_rejected_with_suggestion() {
    let err = resolve_soldr_target("linux-arm").unwrap_err();
    match err {
        AliasError::Ambiguous { disambiguated, .. } => {
            assert_eq!(disambiguated, "linux-arm64");
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
}

#[test]
fn short_arm_spellings_resolve_where_no_arm32_target_exists() {
    // soldr#3806: Windows and macOS have no supported ARM32 target.
    for (input, triple) in [
        ("win-arm", "aarch64-pc-windows-msvc"),
        ("WIN-ARM", "aarch64-pc-windows-msvc"),
        ("windows-arm", "aarch64-pc-windows-msvc"),
        ("mac-arm", "aarch64-apple-darwin"),
        ("macos-arm", "aarch64-apple-darwin"),
        ("darwin-arm", "aarch64-apple-darwin"),
    ] {
        let resolved = resolve_soldr_target(input).unwrap();
        assert_eq!(resolved.rust_triple, triple, "{input}");
        assert!(resolved.via_alias, "{input}");
    }
}

#[test]
fn glibc_suffixed_aliases_resolve_like_the_versioned_triple() {
    // soldr#3806: every accepted spelling lands on the soldr#2139 result.
    for input in [
        "linux-x64-glibc_2_17",
        "linux-x64-glibc2.17",
        "linux-x64-glibc_2.17",
        "linux-x64-glibc2_17",
        "LINUX-X64-GLIBC_2_17",
        "linux-amd64-glibc_2_17",
    ] {
        let resolved = resolve_soldr_target(input).unwrap();
        assert_eq!(resolved.rust_triple, "x86_64-unknown-linux-gnu", "{input}");
        assert_eq!(resolved.glibc_floor.as_deref(), Some("2.17"), "{input}");
        assert!(resolved.via_alias, "{input}");
        assert_eq!(resolved.input, input);
    }
    let arm = resolve_soldr_target("linux-arm64-glibc_2_17").unwrap();
    assert_eq!(arm.rust_triple, "aarch64-unknown-linux-gnu");
    assert_eq!(arm.glibc_floor.as_deref(), Some("2.17"));
    let triple = resolve_soldr_target("x86_64-unknown-linux-gnu.2.17").unwrap();
    let alias = resolve_soldr_target("linux-x64-glibc_2_17").unwrap();
    assert_eq!(
        (triple.rust_triple, triple.glibc_floor),
        (alias.rust_triple, alias.glibc_floor)
    );
}

#[test]
fn glibc_suffix_on_a_non_gnu_alias_is_rejected() {
    for (input, base) in [
        ("mac-x64-glibc_2_17", "x86_64-apple-darwin"),
        ("win-x64-glibc_2_17", "x86_64-pc-windows-msvc"),
        ("linux-x64-musl-glibc_2_17", "x86_64-unknown-linux-musl"),
    ] {
        match resolve_soldr_target(input).unwrap_err() {
            AliasError::GlibcVersioned {
                base: got, version, ..
            } => {
                assert_eq!(got, base, "{input}");
                assert_eq!(version, "2.17", "{input}");
            }
            other => panic!("{input}: expected GlibcVersioned, got {other:?}"),
        }
    }
}

#[test]
fn glibc_suffix_with_an_unbacked_floor_is_rejected() {
    match resolve_soldr_target("linux-x64-glibc_2_28").unwrap_err() {
        AliasError::GlibcVersioned { base, version, .. } => {
            assert_eq!(base, "x86_64-unknown-linux-gnu");
            assert_eq!(version, "2.28");
        }
        other => panic!("expected GlibcVersioned, got {other:?}"),
    }
}

#[test]
fn glibc_suffixed_alias_normalizes_in_cargo_arguments_and_then_strips() {
    let mut args = vec![
        "build".to_string(),
        "--target".to_string(),
        "linux-x64-glibc_2_17".to_string(),
        "--target=win-arm".to_string(),
    ];
    normalize_target_aliases_in_args(&mut args);
    assert_eq!(args[2], "x86_64-unknown-linux-gnu.2.17");
    assert_eq!(args[3], "--target=aarch64-pc-windows-msvc");
    strip_glibc_floor_in_args(&mut args);
    assert_eq!(args[2], "x86_64-unknown-linux-gnu");
}

#[test]
fn thirty_two_bit_rejected() {
    let err = resolve_soldr_target("win-x86").unwrap_err();
    match err {
        AliasError::Thirty2Bit { suggestion, .. } => {
            assert_eq!(suggestion, "win-x64");
        }
        other => panic!("expected Thirty2Bit, got {other:?}"),
    }
}

#[test]
fn the_floor_suffix_is_stripped_from_both_argv_spellings() {
    // The suffix must never reach a cargo child. Both spellings are
    // covered because argv carries whichever the user typed.
    let mut split = vec![
        "build".to_string(),
        "--target".to_string(),
        "x86_64-unknown-linux-gnu.2.17".to_string(),
        "--release".to_string(),
    ];
    strip_glibc_floor_in_args(&mut split);
    // The borrowing form must agree, since it is what the spawn uses.
    assert_eq!(args_without_glibc_floor(&split).as_ref(), split.as_slice());
    assert_eq!(split[2], "x86_64-unknown-linux-gnu");
    assert_eq!(split[3], "--release", "unrelated args must be untouched");

    let mut joined = vec![
        "build".to_string(),
        "--target=aarch64-unknown-linux-gnu.2.28".to_string(),
    ];
    strip_glibc_floor_in_args(&mut joined);
    assert_eq!(joined[1], "--target=aarch64-unknown-linux-gnu");
}

#[test]
fn stripping_leaves_ordinary_targets_alone() {
    // Including musl, which has no glibc to floor, and a bare `--target`
    // at the end of argv, which must not index past the end.
    let mut args = vec![
        "build".to_string(),
        "--target".to_string(),
        "x86_64-unknown-linux-musl".to_string(),
    ];
    strip_glibc_floor_in_args(&mut args);
    assert_eq!(args[2], "x86_64-unknown-linux-musl");

    let mut trailing = vec!["build".to_string(), "--target".to_string()];
    strip_glibc_floor_in_args(&mut trailing);
    assert_eq!(trailing, vec!["build".to_string(), "--target".to_string()]);
}

#[test]
fn the_catalogue_glibc_baseline_resolves_to_the_bare_triple() {
    // The GNU catalogue currently provides a glibc 2.17 sysroot. A higher
    // requested floor must not be accepted and silently mapped to 2.17.
    // `rust_triple` stays bare because rustc does not understand the
    // suffixed soldr spelling.
    for (input, base) in [
        ("x86_64-unknown-linux-gnu.2.17", "x86_64-unknown-linux-gnu"),
        (
            "aarch64-unknown-linux-gnu.2.17",
            "aarch64-unknown-linux-gnu",
        ),
    ] {
        let resolved = resolve_soldr_target(input).unwrap();
        assert_eq!(resolved.rust_triple, base, "{input}");
        assert_eq!(resolved.glibc_floor.as_deref(), Some("2.17"), "{input}");
        assert_eq!(resolved.input, input);
    }
    assert!(resolve_soldr_target("aarch64-unknown-linux-gnu.2.28").is_err());
}

#[test]
fn a_floor_on_an_unenforceable_target_is_still_rejected() {
    // The floor is only meaningful where soldr links through managed zig.
    // Anywhere else soldr would have to drop the suffix and ship a binary
    // whose floor is not the one that was asked for -- silently wrong, and
    // strictly worse than refusing.
    let err = resolve_soldr_target("i686-unknown-linux-gnu.2.17").unwrap_err();
    match &err {
        AliasError::GlibcVersioned { base, version, .. } => {
            assert_eq!(base, "i686-unknown-linux-gnu");
            assert_eq!(version, "2.17");
        }
        other => panic!("expected GlibcVersioned, got {other:?}"),
    }

    // The message has to name the way forward, or the user is stuck.
    let rendered = err.to_string();
    assert!(
        rendered.contains("zigbuild"),
        "the error must point at a tool that can do it: {rendered}"
    );
    assert!(
        rendered.contains("x86_64-unknown-linux-gnu")
            && rendered.contains("aarch64-unknown-linux-gnu"),
        "the error must name the triples where a floor *is* honoured: {rendered}"
    );
}

#[test]
fn only_glibc_triples_take_the_versioned_path() {
    // The gate keys on `-linux-gnu`, so nothing else is diverted into an
    // error about a libc it does not have.
    for input in [
        "x86_64-unknown-linux-musl.2.17",
        "x86_64-apple-darwin.11.0",
        "x86_64-unknown-linux-gnu.foo",
    ] {
        let err = resolve_soldr_target(input).unwrap_err();
        assert!(
            !matches!(err, AliasError::GlibcVersioned { .. }),
            "{input} must not be treated as glibc-versioned, got {err:?}"
        );
    }

    // And the plain triple still resolves, i.e. the gate did not widen.
    let ok = resolve_soldr_target("x86_64-unknown-linux-gnu").unwrap();
    assert_eq!(ok.rust_triple, "x86_64-unknown-linux-gnu");
}

// soldr#2139: `soldr build` never calls `resolve_soldr_target` -- it only
// runs `normalize_target_aliases_in_args`, which passes an unrecognised
// target straight through. So the guard is also exposed on its own and
// called from every blessed-prep entry. These pin that the standalone
// form agrees with the resolver in both directions.
#[test]
fn the_standalone_guard_rejects_what_the_resolver_rejects() {
    // soldr#2139: the guard now splits on whether the floor can be
    // *enforced*, so it must agree with the resolver in both directions.
    let err = reject_glibc_versioned("i686-unknown-linux-gnu.2.17").unwrap_err();
    match err {
        AliasError::GlibcVersioned {
            input,
            base,
            version,
        } => {
            assert_eq!(input, "i686-unknown-linux-gnu.2.17");
            assert_eq!(base, "i686-unknown-linux-gnu");
            assert_eq!(version, "2.17");
        }
        other => panic!("expected GlibcVersioned, got {other:?}"),
    }
    // Case and surrounding whitespace must not smuggle an unenforceable
    // floor past the guard.
    assert!(reject_glibc_versioned("  I686-Unknown-Linux-GNU.2.17 ").is_err());
    assert!(resolve_soldr_target("i686-unknown-linux-gnu.2.17").is_err());

    // ...and the enforceable ones pass both, however they are spelled.
    assert!(reject_glibc_versioned("  X86_64-Unknown-Linux-GNU.2.17 ").is_ok());
    assert!(reject_glibc_versioned("aarch64-unknown-linux-gnu.2.17").is_ok());
    assert!(resolve_soldr_target("aarch64-unknown-linux-gnu.2.17").is_ok());
    assert!(reject_glibc_versioned("aarch64-unknown-linux-gnu.2.28").is_err());
}

#[test]
fn the_standalone_guard_does_not_over_reject() {
    // This one runs on *every* prepared target, so a false positive would
    // break builds that work today -- including custom triples that the
    // alias table has never heard of and must not be judged on.
    for input in [
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl.2.17",
        "x86_64-apple-darwin.11.0",
        "x86_64-unknown-linux-gnu.foo",
        "thumbv7em-none-eabihf",
        "some-vendor-custom-linux-gnueabi",
    ] {
        assert!(
            reject_glibc_versioned(input).is_ok(),
            "{input} must pass the guard untouched",
        );
    }
}

#[test]
fn all_alias_rejected_for_build() {
    let err = resolve_soldr_target("all").unwrap_err();
    assert!(matches!(err, AliasError::AllNotBuildable));
}

#[test]
fn unknown_input_carries_jaro_winkler_suggestion() {
    let err = resolve_soldr_target("win-arm6").unwrap_err();
    match err {
        AliasError::Unknown { suggestion, .. } => {
            assert_eq!(suggestion, "win-arm64");
        }
        other => panic!("expected Unknown with suggestion, got {other:?}"),
    }
}

#[test]
fn native_resolves_to_a_real_triple() {
    let r = resolve_soldr_target("native").unwrap();
    assert!(r.via_alias);
    // Resolved triple varies by host; just confirm it's non-empty
    // and looks like a triple.
    assert!(looks_like_rust_triple(&r.rust_triple));
}

#[test]
fn synonyms_table_targets_are_all_canonical() {
    // Static invariant: every synonym value must appear as a key
    // in CANONICAL_ALIASES. If this fails, the resolver's
    // synonym → canonical → triple chain panics in production.
    let canonical_keys: std::collections::HashSet<&str> =
        CANONICAL_ALIASES.iter().map(|(k, _)| *k).collect();
    for (syn, canonical_target) in SYNONYMS {
        assert!(
            canonical_keys.contains(canonical_target),
            "synonym `{syn}` → `{canonical_target}` is not a canonical alias key"
        );
    }
}

#[test]
fn canonical_aliases_match_canonical_targets_const() {
    // Soldr ships TWO related canonical-list tables:
    //   - crate::core::CANONICAL_TARGETS (Rust triples)
    //   - target_alias::CANONICAL_ALIASES (alias → triple)
    // Their VALUES (Rust triples) must agree as a set.
    let triples_const: std::collections::HashSet<&str> =
        crate::core::CANONICAL_TARGETS.iter().copied().collect();
    let triples_alias: std::collections::HashSet<&str> =
        CANONICAL_ALIASES.iter().map(|(_, t)| *t).collect();
    assert_eq!(triples_const, triples_alias);
}

#[test]
fn canonical_aliases_normalize_in_cargo_arguments() {
    for (alias, triple) in CANONICAL_ALIASES {
        let mut split = vec![
            "build".to_string(),
            "--target".to_string(),
            alias.to_string(),
        ];
        normalize_target_aliases_in_args(&mut split);
        assert_eq!(split[2], *triple, "split --target form drifted for {alias}");

        let mut equals = vec!["build".to_string(), format!("--target={alias}")];
        normalize_target_aliases_in_args(&mut equals);
        assert_eq!(
            equals[1],
            format!("--target={triple}"),
            "equals --target form drifted for {alias}"
        );
    }
}

// soldr#3390: every AliasError variant, rendered against every surface.
// RED on `main` for `AllNotBuildable` (its body named `soldr build`
// unconditionally) and for any call site that kept its own prefix.
#[test]
fn every_alias_error_renders_cleanly_for_every_surface() {
    let errors: Vec<(&str, AliasError)> = vec![
        ("Unknown", resolve_soldr_target("win-armm").unwrap_err()),
        ("Ambiguous", resolve_soldr_target("linux-arm").unwrap_err()),
        ("Thirty2Bit", resolve_soldr_target("win-x86").unwrap_err()),
        (
            "GlibcVersioned",
            resolve_soldr_target("i686-unknown-linux-gnu.2.17").unwrap_err(),
        ),
        ("AllNotBuildable", resolve_soldr_target("all").unwrap_err()),
    ];

    for (name, error) in &errors {
        // The bare, unrendered Display never names the wrong command --
        // an incomplete message beats a false one.
        let bare = error.to_string();
        assert!(
            !bare.contains("soldr build"),
            "{name}: bare Display must not name a command: {bare:?}"
        );

        for &surface in TargetSurface::ALL {
            let verb = surface.verb();
            let rendered = error.render(surface);
            let expected_prefix = format!("soldr {verb} --target");
            assert!(
                rendered.starts_with(&expected_prefix),
                "{name}/{verb}: expected prefix {expected_prefix:?}, got {rendered:?}"
            );
            // `AllNotBuildable`'s own body legitimately names
            // `soldr prepare --target all` -- that is the one command
            // `all` IS valid for, not an accidental doubling -- so
            // rendering it for the `Prepare` surface is the one case
            // where the prefix and the body both contain the same
            // substring. Every other combination must see it exactly
            // once; a second occurrence anywhere else is exactly the
            // wheel/lint verb-doubling this test exists to catch.
            let allowed_occurrences =
                if *name == "AllNotBuildable" && surface == TargetSurface::Prepare {
                    2
                } else {
                    1
                };
            let occurrences = rendered.matches(&expected_prefix).count();
            assert_eq!(
                occurrences, allowed_occurrences,
                "{name}/{verb}: expected {allowed_occurrences} occurrence(s) of \
                 {expected_prefix:?} in {rendered:?}"
            );
            if surface != TargetSurface::Build {
                assert!(
                    !rendered.contains("soldr build"),
                    "{name}/{verb}: a non-build surface must never name build: {rendered:?}"
                );
            }
        }
    }
}

#[test]
fn target_surface_all_has_nine_distinct_verbs() {
    assert_eq!(TargetSurface::ALL.len(), 9);
    let verbs: std::collections::HashSet<&str> =
        TargetSurface::ALL.iter().map(|s| s.verb()).collect();
    assert_eq!(verbs.len(), 9, "verbs must be pairwise distinct");
}
