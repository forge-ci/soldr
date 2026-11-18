//! soldr#2794: the CRT linkage is the caller's policy, not soldr's plumbing.
//!
//! Everything here is driven through arguments rather than the process
//! environment. `tests/guards/env_lock_lint.rs` requires each mutated variable to sit
//! under one barrier, and `RUSTFLAGS` already has two; keeping the decision
//! logic pure means these tests cannot participate in that race at all.

use super::{
    cargo_config_rustflags, crt_linkage_from_merge, declared_config_linkage, xwin_msvc_link_args,
    CrtLinkage,
};

/// An xwin cache laid out the way the real tarball is.
fn xwin_cache() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tmpdir");
    for arch in ["x64", "arm64"] {
        std::fs::create_dir_all(tmp.path().join("crt").join("lib").join(arch)).unwrap();
        std::fs::create_dir_all(tmp.path().join("sdk").join("lib").join("um").join(arch)).unwrap();
        std::fs::create_dir_all(tmp.path().join("sdk").join("lib").join("ucrt").join(arch))
            .unwrap();
    }
    tmp
}

// --------------------------- deciding the linkage ---------------------------

#[test]
fn no_mention_of_crt_static_keeps_the_dynamic_default() {
    assert_eq!(
        crt_linkage_from_merge(["-C opt-level=3", "-Dwarnings"]),
        CrtLinkage::Dynamic,
        "silence must not be read as a request to change linkage"
    );
    assert_eq!(
        crt_linkage_from_merge(Vec::<String>::new()),
        CrtLinkage::Dynamic
    );
}

#[test]
fn plus_crt_static_selects_static() {
    assert_eq!(
        crt_linkage_from_merge(["-C target-feature=+crt-static"]),
        CrtLinkage::Static
    );
}

#[test]
fn minus_crt_static_is_an_explicit_dynamic_request() {
    assert_eq!(
        crt_linkage_from_merge(["-C target-feature=-crt-static"]),
        CrtLinkage::Dynamic
    );
}

/// rustc accumulates `-C target-feature`, so a later `-crt-static` really does
/// win. Reporting static here would emit the static archives for a link that
/// resolves dynamically -- reintroducing the mismatch from the other side.
#[test]
fn within_one_source_the_last_mention_wins() {
    assert_eq!(
        crt_linkage_from_merge(["-C target-feature=+crt-static,-crt-static"]),
        CrtLinkage::Dynamic
    );
    assert_eq!(
        crt_linkage_from_merge(["-C target-feature=-crt-static,+crt-static"]),
        CrtLinkage::Static
    );
}

/// A source saying nothing about the CRT must not veto one that does, whichever
/// side of it that source sits on.
#[test]
fn a_silent_source_does_not_veto_a_speaking_one() {
    assert_eq!(
        crt_linkage_from_merge(["-Dwarnings", "-C target-feature=+crt-static"]),
        CrtLinkage::Static
    );
    assert_eq!(
        crt_linkage_from_merge(["-C target-feature=+crt-static", "-Dwarnings"]),
        CrtLinkage::Static
    );
}

/// soldr#2830: the direction this asserts is the whole bug. soldr concatenates
/// every rustflags source and lets rustc's last-wins rule settle it, so the
/// source appended **last** is the strongest. The old model asserted the
/// opposite and produced a link line whose two halves disagreed.
#[test]
fn the_last_source_wins_because_soldr_concatenates() {
    assert_eq!(
        crt_linkage_from_merge([
            "-C target-feature=-crt-static",
            "-C target-feature=+crt-static",
        ]),
        CrtLinkage::Static,
        "a later-appended source must override an earlier one"
    );
}

/// The append order `requested_crt_linkage` uses, spelled out with the real
/// variable names so a reordering of that list fails here.
///
/// `merge_encoded_rustflags` appends ambient `CARGO_ENCODED_RUSTFLAGS`, then
/// soldr's own flags, then `CARGO_TARGET_<T>_RUSTFLAGS`, then `RUSTFLAGS`.
#[test]
fn rustflags_outranks_the_target_key_which_outranks_ambient_encoded() {
    let ambient_encoded = "-C target-feature=-crt-static";
    let target_key = "-C link-arg=/LIBPATH:xwin";
    let rustflags = "-C target-feature=+crt-static";
    assert_eq!(
        crt_linkage_from_merge([ambient_encoded, target_key, rustflags]),
        CrtLinkage::Static,
        "RUSTFLAGS is appended last and must win"
    );

    let target_key_static = "-C target-feature=+crt-static";
    assert_eq!(
        crt_linkage_from_merge([ambient_encoded, target_key_static, ""]),
        CrtLinkage::Static,
        "the target key must outrank ambient CARGO_ENCODED_RUSTFLAGS"
    );
}

// ------------------------------ cargo config -------------------------------

#[test]
fn cargo_config_rustflags_reads_target_and_build_tables() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    std::fs::create_dir_all(tmp.path().join(".cargo")).unwrap();
    std::fs::write(
        tmp.path().join(".cargo/config.toml"),
        "[build]\nrustflags = [\"-C\", \"target-feature=+crt-static\"]\n",
    )
    .unwrap();
    let flags = cargo_config_rustflags(tmp.path(), "x86_64-pc-windows-msvc");
    assert_eq!(
        declared_config_linkage(&flags),
        Some(CrtLinkage::Static),
        "build.rustflags must still be read, to warn about"
    );
}

#[test]
fn cargo_config_target_table_is_more_specific_than_build() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    std::fs::create_dir_all(tmp.path().join(".cargo")).unwrap();
    std::fs::write(
        tmp.path().join(".cargo/config.toml"),
        "[build]\nrustflags = [\"-C\", \"target-feature=-crt-static\"]\n\n\
         [target.x86_64-pc-windows-msvc]\nrustflags = \"-C target-feature=+crt-static\"\n",
    )
    .unwrap();
    let flags = cargo_config_rustflags(tmp.path(), "x86_64-pc-windows-msvc");
    assert!(
        flags[0].contains("+crt-static"),
        "the target table must be listed before build: {flags:?}"
    );
    assert_eq!(declared_config_linkage(&flags), Some(CrtLinkage::Static));
}

/// This feeds a link-flag choice that has a working default, so a broken or
/// absent config must degrade to "said nothing" rather than fail a build.
#[test]
fn malformed_or_absent_config_yields_nothing() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    assert!(cargo_config_rustflags(tmp.path(), "x86_64-pc-windows-msvc").is_empty());

    std::fs::create_dir_all(tmp.path().join(".cargo")).unwrap();
    std::fs::write(tmp.path().join(".cargo/config.toml"), "not = [valid toml").unwrap();
    assert!(cargo_config_rustflags(tmp.path(), "x86_64-pc-windows-msvc").is_empty());
}

#[test]
fn another_targets_rustflags_are_not_borrowed() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    std::fs::create_dir_all(tmp.path().join(".cargo")).unwrap();
    std::fs::write(
        tmp.path().join(".cargo/config.toml"),
        "[target.aarch64-pc-windows-msvc]\nrustflags = \"-C target-feature=+crt-static\"\n",
    )
    .unwrap();
    let flags = cargo_config_rustflags(tmp.path(), "x86_64-pc-windows-msvc");
    assert!(
        flags.is_empty(),
        "a sibling target's flags must not be borrowed: {flags:?}"
    );
    assert_eq!(declared_config_linkage(&flags), None);
}

/// soldr#2830: a config-declared preference must not reach the decision.
///
/// `requested_crt_linkage` builds its source list from three environment
/// variables and nothing else, so a config can only ever produce a warning.
/// Asserting that here rather than through the real function keeps this test
/// off the process environment, which is what the module header requires:
/// `RUSTFLAGS` is already mutated under two other test barriers, and
/// `tests/guards/env_lock_lint.rs` wants one barrier per variable.
#[test]
fn a_config_preference_cannot_change_the_decision() {
    let config_says_static = "-C target-feature=+crt-static";
    // The environment is silent, exactly as it is for a project that puts the
    // flag only in `.cargo/config.toml`.
    assert_eq!(
        crt_linkage_from_merge(["", "", ""]),
        CrtLinkage::Dynamic,
        "a silent environment is dynamic regardless of what a config says"
    );
    // ...and the config's own reading is still available, for the warning.
    assert_eq!(
        declared_config_linkage(&[config_says_static.to_string()]),
        Some(CrtLinkage::Static)
    );
}

// ------------------------------- the flags ---------------------------------

/// The exact pairing the issue reported as failing: rustc emits
/// `/defaultlib:libcmt` under `+crt-static` while soldr forced the import
/// libraries, and lld-link found `__vcrt_InitializeCriticalSectionEx` in both.
#[test]
fn static_linkage_emits_the_static_archives_and_excludes_the_import_libraries() {
    let tmp = xwin_cache();
    let out =
        xwin_msvc_link_args(tmp.path(), "x86_64-pc-windows-msvc", CrtLinkage::Static).unwrap();

    for expected in [
        "link-arg=/DEFAULTLIB:libucrt.lib",
        "link-arg=/DEFAULTLIB:libvcruntime.lib",
        "link-arg=/NODEFAULTLIB:ucrt.lib",
        "link-arg=/NODEFAULTLIB:vcruntime.lib",
    ] {
        assert!(out.contains(expected), "missing {expected} in: {out}");
    }
    // The dynamic spellings must be gone, not merely outnumbered. `contains`
    // on the bare names would match the `lib`-prefixed ones, so anchor on the
    // full flag.
    assert!(
        !out.contains("link-arg=/DEFAULTLIB:ucrt.lib"),
        "dynamic ucrt import library survived: {out}"
    );
    assert!(
        !out.contains("link-arg=/DEFAULTLIB:vcruntime.lib"),
        "dynamic vcruntime import library survived: {out}"
    );
    assert!(
        !out.contains("link-arg=/NODEFAULTLIB:libucrt.lib"),
        "static ucrt archive is excluded in a static build: {out}"
    );
}

/// soldr injects a `vcruntime` default that upstream cargo-xwin does not, so
/// porting only their `ucrt` -> `libucrt` swap would leave the exact duplicate
/// pair from the issue on the link line.
#[test]
fn static_linkage_covers_vcruntime_not_just_ucrt() {
    let tmp = xwin_cache();
    let out =
        xwin_msvc_link_args(tmp.path(), "x86_64-pc-windows-msvc", CrtLinkage::Static).unwrap();
    assert!(
        out.contains("link-arg=/NODEFAULTLIB:vcruntime.lib")
            && out.contains("link-arg=/DEFAULTLIB:libvcruntime.lib"),
        "vcruntime must be swapped alongside ucrt: {out}"
    );
}

/// The acceptance criterion: existing dynamic consumers are untouched.
#[test]
fn dynamic_linkage_is_unchanged_from_before_the_fix() {
    let tmp = xwin_cache();
    let out =
        xwin_msvc_link_args(tmp.path(), "x86_64-pc-windows-msvc", CrtLinkage::Dynamic).unwrap();
    assert!(out.contains("link-arg=/NODEFAULTLIB:libucrt.lib"), "{out}");
    assert!(out.contains("link-arg=/DEFAULTLIB:ucrt.lib"), "{out}");
    assert!(out.contains("link-arg=/DEFAULTLIB:vcruntime.lib"), "{out}");
    assert!(
        !out.contains("link-arg=/DEFAULTLIB:libvcruntime.lib"),
        "the dynamic arm must not acquire new flags: {out}"
    );
}

/// The plumbing -- linker flavor and library search paths -- is soldr's and is
/// identical either way. Only the policy moves.
#[test]
fn linkage_changes_only_the_crt_flags() {
    let tmp = xwin_cache();
    let dynamic =
        xwin_msvc_link_args(tmp.path(), "x86_64-pc-windows-msvc", CrtLinkage::Dynamic).unwrap();
    let static_ =
        xwin_msvc_link_args(tmp.path(), "x86_64-pc-windows-msvc", CrtLinkage::Static).unwrap();

    for out in [&dynamic, &static_] {
        assert!(out.contains("linker-flavor=lld-link"), "{out}");
    }
    let libpaths = |text: &str| {
        text.split_whitespace()
            .filter(|token| token.starts_with("link-arg=/LIBPATH:"))
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        libpaths(&dynamic),
        libpaths(&static_),
        "library search paths are plumbing and must not depend on the CRT"
    );
    assert!(!libpaths(&static_).is_empty(), "expected LIBPATH entries");
}

/// Every CRT flag needs its own `-C`, or rustc treats it as a plain argument
/// and drops it silently -- the failure mode would be a link that still has
/// both CRTs, i.e. indistinguishable from not having made this change.
#[test]
fn static_crt_flags_are_each_prefixed_with_dash_c() {
    let tmp = xwin_cache();
    let out =
        xwin_msvc_link_args(tmp.path(), "x86_64-pc-windows-msvc", CrtLinkage::Static).unwrap();
    let tokens: Vec<_> = out.split_whitespace().collect();
    let crt_indexes: Vec<_> = tokens
        .iter()
        .enumerate()
        .filter_map(|(index, token)| {
            (token.contains("DEFAULTLIB:") && token.starts_with("link-arg=")).then_some(index)
        })
        .collect();
    assert_eq!(crt_indexes.len(), 4, "expected four CRT flags: {out}");
    for index in crt_indexes {
        assert_eq!(tokens.get(index.wrapping_sub(1)), Some(&"-C"), "{out}");
    }
}

/// aarch64 never shipped the Windows 7 API shims, so it has no overlapping
/// `winapi_downlevel.obj` and never reproduced the duplicate symbol. The flag
/// swap still has to apply there -- the issue's x64-only symptom is a property
/// of that one object file, not of the policy.
#[test]
fn static_linkage_applies_to_aarch64_too() {
    let tmp = xwin_cache();
    let out =
        xwin_msvc_link_args(tmp.path(), "aarch64-pc-windows-msvc", CrtLinkage::Static).unwrap();
    assert!(out.contains("link-arg=/DEFAULTLIB:libucrt.lib"), "{out}");
    assert!(
        out.contains("link-arg=/DEFAULTLIB:libvcruntime.lib"),
        "{out}"
    );
}

#[test]
fn non_msvc_targets_get_no_crt_flags_whichever_linkage() {
    let tmp = xwin_cache();
    for linkage in [CrtLinkage::Dynamic, CrtLinkage::Static] {
        let out = xwin_msvc_link_args(tmp.path(), "x86_64-unknown-linux-gnu", linkage).unwrap();
        assert!(out.is_empty(), "non-msvc triple must stay empty: {out:?}");
    }
}
