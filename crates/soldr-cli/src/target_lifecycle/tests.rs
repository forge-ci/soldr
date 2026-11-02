//! Unit tests for the target-lifecycle argv helpers.
//!
//! Split out of `target_lifecycle.rs` (soldr#2493). Converting the retired
//! watchdog-macro call sites to plain `#[test] fn` costs one line per test,
//! which took that
//! file from 992 to 1008 lines -- past the 1000-line production ceiling
//! soldr#2493 itself introduced. The two test modules are the natural seam,
//! and the layout matches the sibling `cargo_front_door/tests.rs`.

use super::*;

use super::*;

#[test]
fn every_canonical_target_has_a_stable_capability_plan() {
    for target in crate::core::CANONICAL_TARGETS {
        let plan = plan(target).unwrap_or_else(|error| panic!("{target}: {error}"));
        assert!(plan.canonical, "{target}");
        assert_eq!(plan.schema_version, 1);
        assert_eq!(plan.canonical_target, *target);
        assert!(plan.canonical_alias.is_some(), "{target}");
        let expected = if *target == "x86_64-pc-windows-gnu" {
            vec![
                "prepare",
                "build",
                "clippy",
                "test-no-run",
                "nextest-archive",
            ]
        } else {
            vec![
                "prepare",
                "build",
                "clippy",
                "test-no-run",
                "nextest-archive",
                "pep517-wheel",
                "pep517-sdist",
            ]
        };
        assert_eq!(plan.supported_operations, expected);
        assert!(
            plan.environment
                .keys
                .windows(2)
                .all(|pair| pair[0] < pair[1]),
            "environment keys must be sorted and unique: {:?}",
            plan.environment.keys
        );
    }
}

#[test]
fn linux_arm64_plan_uses_catalogue_gnu_toolchain_without_zig() {
    let plan = plan_for_host("aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu").unwrap();
    assert_eq!(plan.toolchain.family, "linux-gnu");
    assert_eq!(plan.toolchain.linker, "managed gcc");
    assert_eq!(plan.platform.kind, "gnu-linux-sysroot");
    assert_eq!(plan.platform.provider, "soldr-toolchain");
    assert!(plan.cache_identity.contains("gnu-linux-toolchain/"));
    let json = serde_json::to_string(&plan).unwrap();
    assert!(
        !json.contains("zig"),
        "normal GNU plans must not advertise Zig: {json}"
    );
    assert!(!json.contains("cargo-zigbuild"));
}

// soldr#2309: the linux-gnu plan advertises the C++ stdlib pin; every
// other family stays unchanged (musl/windows/darwin ship no such pin).
#[test]
fn only_linux_gnu_plans_advertise_the_cxx_stdlib_pin() {
    let host = "x86_64-unknown-linux-gnu";
    let gnu = plan_for_host("aarch64-unknown-linux-gnu", host).unwrap();
    assert!(gnu
        .environment
        .keys
        .contains(&"CXXSTDLIB_aarch64_unknown_linux_gnu".to_string()));
    assert!(gnu.environment.keys.contains(&"CXXSTDLIB".to_string()));
    for other in [
        "aarch64-unknown-linux-musl",
        "x86_64-pc-windows-msvc",
        "aarch64-apple-darwin",
    ] {
        let plan = plan_for_host(other, host).unwrap();
        assert!(
            !plan
                .environment
                .keys
                .iter()
                .any(|key| key.starts_with("CXXSTDLIB")),
            "{other} must not carry a C++ stdlib pin: {:?}",
            plan.environment.keys
        );
    }
}

#[test]
fn linux_musl_plan_uses_catalogue_toolchain_without_zig() {
    let plan = plan_for_host("aarch64-unknown-linux-musl", "x86_64-unknown-linux-gnu").unwrap();
    assert_eq!(plan.toolchain.family, "linux-musl");
    assert_eq!(plan.toolchain.linker, "managed gcc");
    assert_eq!(plan.platform.kind, "musl-linux-sysroot");
    assert_eq!(plan.platform.provider, "soldr-toolchain");
    assert!(plan.cache_identity.contains("musl-linux-toolchain/"));
    let json = serde_json::to_string(&plan).unwrap();
    assert!(
        !json.contains("zig"),
        "normal musl plans must not advertise Zig: {json}"
    );
    assert!(!json.contains("cargo-zigbuild"));
}

// soldr#2519 deleted `host_native_gnu_uses_catalogue_toolchain` with the
// helper it called. Its point -- that a GNU target never inherits the
// runner's linker via the Zig fallback -- is now structural rather than
// conditional: there is no Zig fallback to reach. The catalogue assertions
// below cover what a GNU target actually gets.

#[test]
fn the_reported_linux_plans_match_the_catalogue_lifecycle() {
    let plan = plan_for_host("x86_64-unknown-linux-gnu", "x86_64-unknown-linux-gnu").unwrap();
    assert_eq!(plan.toolchain.linker, "managed gcc");
    assert_eq!(plan.platform.provider, "soldr-toolchain");
    assert_eq!(plan.platform.kind, "gnu-linux-sysroot");

    let musl = plan_for_host("x86_64-unknown-linux-musl", "x86_64-unknown-linux-musl").unwrap();
    assert_eq!(musl.toolchain.linker, "managed gcc");
    assert_eq!(musl.platform.provider, "soldr-toolchain");
    assert_eq!(musl.platform.kind, "musl-linux-sysroot");
}

// soldr#2519 deleted `only_explicit_legacy_musl_can_reach_zig_fallback` and
// `normal_musl_never_reaches_legacy_zig` with the helper they called. There
// is no longer an explicit-legacy branch for them to distinguish: the Zig
// musl fallback and its `SOLDR_USE_LEGACY_ZIGBUILD` gate are both gone, so
// every musl target takes the catalogue path asserted just above.

#[test]
fn unsupported_noncanonical_targets_have_no_capability_plan() {
    let error = plan("i686-unknown-linux-gnu").unwrap_err();
    assert!(error.to_string().contains("i686"));
}

#[test]
fn required_msvc_flags_merge_with_target_and_project_flags() {
    let required = "-C link-arg=/LIBPATH:/soldr/sdk";
    let merged = merge_flag_values(
        required,
        Some("-C target-cpu=x86-64-v3"),
        Some("-Dwarnings"),
    );
    assert!(merged.contains("/LIBPATH:/soldr/sdk"));
    assert!(merged.contains("target-cpu=x86-64-v3"));
    assert!(merged.contains("-Dwarnings"));
}

#[test]
fn required_c_flags_merge_with_project_flags() {
    let merged = merge_flag_values(
        "/imsvc/soldr/sdk/include",
        Some("-DPROJECT_TARGET=1"),
        Some("-DPROJECT_GLOBAL=1"),
    );
    assert!(merged.contains("/imsvc/soldr/sdk/include"));
    assert!(merged.contains("-DPROJECT_TARGET=1"));
    assert!(merged.contains("-DPROJECT_GLOBAL=1"));
}

#[test]
fn encoded_project_rustflags_keep_required_linker_search_path() {
    let merged = merge_encoded_rustflags(
        "-Dwarnings\u{1f}-C\u{1f}target-cpu=x86-64-v3",
        "-C link-arg=/LIBPATH:/soldr/sdk",
        None,
        Some("-C target-feature=+crt-static"),
    );
    let tokens: Vec<_> = merged.split('\u{1f}').collect();
    assert!(tokens.contains(&"-Dwarnings"));
    assert!(tokens.contains(&"target-cpu=x86-64-v3"));
    assert!(tokens.contains(&"target-feature=+crt-static"));
    assert!(tokens.contains(&"link-arg=/LIBPATH:/soldr/sdk"));
    assert_eq!(
        tokens.iter().filter(|token| **token == "-C").count(),
        3,
        "each encoded codegen option retains its -C prefix"
    );
}

#[test]
fn encoded_required_flags_preserve_paths_with_spaces() {
    let merged = merge_encoded_rustflags(
        "",
        "-C linker-flavor=lld-link -C link-arg=/LIBPATH:/soldr sdk/lib",
        None,
        None,
    );
    let tokens: Vec<_> = merged.split('\u{1f}').collect();
    assert!(tokens.contains(&"link-arg=/LIBPATH:/soldr sdk/lib"));
}

#[test]
fn applying_target_flags_consumes_higher_precedence_globals() {
    let _lock = crate::TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let target_key = "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS";
    let names = [target_key, "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"];
    let previous = names.map(std::env::var_os);
    std::env::remove_var(target_key);
    std::env::set_var("RUSTFLAGS", "-Dwarnings");
    std::env::set_var("CARGO_ENCODED_RUSTFLAGS", "-C\u{1f}target-cpu=x86-64-v3");
    let mut prep = BlessedPrep::default();
    prep.env.push((
        target_key.to_string(),
        "-C link-arg=/LIBPATH:/soldr/sdk".to_string(),
    ));

    apply_to_process(&prep);
    let rustflags = std::env::var_os("RUSTFLAGS");
    let encoded = std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap();
    for (name, value) in names.into_iter().zip(previous) {
        if let Some(value) = value {
            std::env::set_var(name, value);
        } else {
            std::env::remove_var(name);
        }
    }

    assert!(rustflags.is_none());
    let tokens: Vec<_> = encoded.split('\u{1f}').collect();
    assert!(tokens.contains(&"-Dwarnings"));
    assert!(tokens.contains(&"target-cpu=x86-64-v3"));
    assert!(tokens.contains(&"link-arg=/LIBPATH:/soldr/sdk"));
}

#[test]
fn all_compiling_cargo_surfaces_enter_the_lifecycle() {
    for command in [
        "build --target linux-arm64",
        "clippy --target linux-arm64",
        "test --no-run --target linux-arm64",
        "nextest archive --target linux-arm64",
    ] {
        let args = command
            .split_ascii_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(cargo_operation_requires_prep(&args), "{command}");
    }
    let clean = ["clean", "--target", "linux-arm64"].map(str::to_string);
    assert!(!cargo_operation_requires_prep(&clean));
}

/// soldr#2722: the soldr#2437 guard must not give GNU and musl the same
/// remedy. `soldr build` delegates GNU to Docker Linux (soldr#2319) before
/// the guard is reached, so for GNU there is a working path one verb away;
/// musl has no delegation at all. One shared message described neither.
#[test]
fn the_gnu_guard_message_names_the_delegated_build_that_works() {
    let message = windows_linux_guard_message("x86_64-unknown-linux-gnu", Some(TargetAbi::Gnu));

    assert!(
        message.contains("soldr build --target x86_64-unknown-linux-gnu"),
        "GNU remedy must name the verb that delegates: {message}"
    );
    assert!(
        message.contains("soldr#2319"),
        "GNU remedy must cite the delegation issue: {message}"
    );
    // `ci/perf_local.py` is a repo-internal CI harness, not something to
    // hand a user as the fix for their build.
    assert!(
        !message.contains("perf_local.py"),
        "no user-facing diagnostic should recommend the CI harness: {message}"
    );
}

#[test]
fn the_musl_guard_message_says_gnu_is_delegated_instead() {
    let message = windows_linux_guard_message("x86_64-unknown-linux-musl", Some(TargetAbi::Musl));

    // The whole point: a user stopped on musl cannot otherwise discover that
    // the sibling ABI is handled automatically.
    assert!(
        message.contains("x86_64-unknown-linux-gnu"),
        "musl remedy must point at the GNU triple that works: {message}"
    );
    assert!(
        message.contains("not delegated"),
        "musl remedy must say why it differs from GNU: {message}"
    );
    assert!(
        !message.contains("perf_local.py"),
        "no user-facing diagnostic should recommend the CI harness: {message}"
    );
}

/// Both branches keep the root cause, which is what soldr#2437 documents.
#[test]
fn both_guard_messages_keep_the_elf_compiler_root_cause() {
    for (target, abi) in [
        ("x86_64-unknown-linux-gnu", TargetAbi::Gnu),
        ("aarch64-unknown-linux-musl", TargetAbi::Musl),
    ] {
        let message = windows_linux_guard_message(target, Some(abi));
        assert!(
            message.contains("soldr#2437") && message.contains("Linux ELF"),
            "{target} lost the root-cause explanation: {message}"
        );
        assert!(
            message.contains(target),
            "{target} must be named in its own diagnostic: {message}"
        );
    }
}

// ── soldr#2874: host shape vs target shape ───────────────────────────────
//
// Every bundle in the catalogue is x86_64-hosted; the slug names the target
// shape. These pin the decision on hosts soldr's own CI does not run on,
// which is the point -- the bug only appears on a native ARM64 Linux runner,
// and a `cfg!`-driven answer could only be checked by running there.

use crate::fetch::catalogue_linux_host::{
    bundle_host_fitness, BundleHostFitness, GNU_BUNDLE_HOST, MUSL_BUNDLE_HOST,
};
use crate::platform::host::facts::{HostArch, HostLibc, HostOs};

const X64: &str = "x86_64-unknown-linux-gnu";
const ARM64: &str = "aarch64-unknown-linux-gnu";

fn decide(
    arch: HostArch,
    host: &str,
    target: &str,
    base: &str,
    floor: Option<&str>,
) -> GnuBundleDecision {
    let fitness = bundle_host_fitness(GNU_BUNDLE_HOST, HostOs::Linux, arch, HostLibc::Gnu);
    decide_gnu_bundle(fitness, host, target, base, floor, false)
}

fn macos_gnu_fitness() -> BundleHostFitness {
    bundle_host_fitness(
        GNU_BUNDLE_HOST,
        HostOs::MacOs,
        HostArch::Aarch64,
        HostLibc::None,
    )
}

#[test]
fn an_x86_64_host_still_uses_the_catalogue_bundle_for_both_targets() {
    // The regression guard for the fix itself: cross-building ARM64 from
    // x86_64 is the case the bundle exists for and must stay unchanged.
    assert_eq!(
        decide(HostArch::X86_64, X64, ARM64, ARM64, None),
        GnuBundleDecision::UseCatalogue
    );
    assert_eq!(
        decide(HostArch::X86_64, X64, X64, X64, None),
        GnuBundleDecision::UseCatalogue
    );
    assert_eq!(
        decide(
            HostArch::X86_64,
            X64,
            "x86_64-unknown-linux-gnu.2.17",
            X64,
            Some("2.17")
        ),
        GnuBundleDecision::UseCatalogue
    );
}

#[test]
fn a_native_arm64_host_building_arm64_uses_its_own_compiler() {
    // soldr#2874's reproduction: `soldr cargo build --target
    // aarch64-unknown-linux-gnu` on `ubuntu-24.04-arm`. The bundle was
    // selected by target shape and then executed, giving `Exec format error
    // (os error 8)`. Natively, the host toolchain already targets this
    // triple, so the bundle was never load-bearing here.
    assert_eq!(
        decide(HostArch::Aarch64, ARM64, ARM64, ARM64, None),
        GnuBundleDecision::UseHostCompiler
    );
}

#[test]
fn a_native_arm64_host_cross_building_x86_64_is_refused_precisely() {
    // Nothing stands in for the bundle here: the host compiler targets
    // ARM64, not x86_64. Refusing early beats failing inside the first
    // `-sys` crate after a several-hundred-megabyte download.
    let decision = decide(HostArch::Aarch64, ARM64, X64, X64, None);
    let GnuBundleDecision::Reject(message) = decision else {
        panic!("expected a rejection, got {decision:?}");
    };
    assert!(message.contains(X64), "{message}");
    assert!(message.contains(ARM64), "{message}");
    assert!(message.contains("soldr#2874"), "{message}");
}

#[test]
fn an_explicit_glibc_floor_on_an_unsupported_host_is_refused_not_downgraded() {
    // The failure mode this exists to prevent: silently building against the
    // host's own newer glibc would produce an artifact that CLAIMS a 2.17
    // floor it does not have. A wrong artifact is worse than a refusal.
    let decision = decide(
        HostArch::Aarch64,
        ARM64,
        "aarch64-unknown-linux-gnu.2.17",
        ARM64,
        Some("2.17"),
    );
    let GnuBundleDecision::Reject(message) = decision else {
        panic!("expected a rejection, got {decision:?}");
    };
    assert!(message.contains("2.17"), "{message}");
    // It has to say what to do instead, in both directions.
    assert!(message.contains("Drop the `.2.17` suffix"), "{message}");
    assert!(message.contains("cross-build from an"), "{message}");
}

/// soldr#3082: an accepted glibc floor must reach the lifecycle path that
/// enforces it. Classifying the raw `…gnu.2.17` string sent it to the
/// custom-spec passthrough instead, so the build silently linked against the
/// host glibc and exited 0.
#[test]
fn an_accepted_glibc_floor_routes_to_the_lifecycle_that_enforces_it() {
    for target in [
        "x86_64-unknown-linux-gnu.2.17",
        "aarch64-unknown-linux-gnu.2.17",
    ] {
        assert_eq!(prep_route(target), PrepRoute::Lifecycle, "{target}");
    }
}

/// The floor must not change the route: a suffixed target goes wherever its
/// base triple goes, so the suffix can only ever add enforcement.
#[test]
fn a_glibc_floor_takes_the_same_route_as_its_base_triple() {
    for (floored, base) in [
        ("x86_64-unknown-linux-gnu.2.17", "x86_64-unknown-linux-gnu"),
        (
            "aarch64-unknown-linux-gnu.2.17",
            "aarch64-unknown-linux-gnu",
        ),
    ] {
        assert_eq!(prep_route(floored), prep_route(base), "{floored}");
    }
}

/// Cargo's custom target-spec passthrough stays intact: a spec path is not a
/// floor and not a classifiable triple.
#[test]
fn custom_target_specs_keep_the_passthrough_route() {
    for spec in ["my-target.json", "./specs/thumbv7em-custom.json"] {
        assert_eq!(prep_route(spec), PrepRoute::Passthrough, "{spec}");
    }
}

#[test]
fn the_diagnostic_names_the_host_shape_and_the_bundle_host_shape() {
    // The defect was those two being conflated, so a message that mentions
    // only one of them would not have prevented it.
    let message = gnu_bundle_host_message(BundleHostFitness::WrongArch, ARM64, ARM64, None);
    assert!(message.contains(GNU_BUNDLE_HOST.host), "{message}");
    assert!(message.contains("Exec format error"), "{message}");
    assert!(
        message.contains("names the target shape, not the host shape"),
        "{message}"
    );
}

#[test]
fn the_existing_cross_guard_seam_still_forces_the_catalogue_path() {
    // `cli_build_fetch_overlap` seeds a fake `linux-x64-gnu` bundle and runs
    // with `SOLDR_WINDOWS_LINUX_CROSS_GUARD=off` so the catalogue machinery
    // is exercised from macOS and Windows lanes. Those hosts fail the
    // fitness check for real, so without honouring the same seam this fix
    // would refuse and take three of that file's tests down with it -- which
    // is exactly what the darwin lanes reported before this arm existed.
    const MAC: &str = "aarch64-apple-darwin";
    assert_eq!(
        decide_gnu_bundle(macos_gnu_fitness(), MAC, X64, X64, None, true),
        GnuBundleDecision::UseCatalogue
    );
    // And it must be the seam doing it, not the host: with the seam off, a
    // macOS host cannot execute a Linux ELF and there is no host compiler
    // targeting a Linux triple to fall back to.
    assert!(matches!(
        decide_gnu_bundle(macos_gnu_fitness(), MAC, X64, X64, None, false),
        GnuBundleDecision::Reject(_)
    ));
}

// ── soldr#3296: musl bundle host shape ───────────────────────────────────

#[test]
fn an_x86_64_host_can_use_the_catalogue_musl_bundle() {
    assert_eq!(
        decide_musl_bundle(
            bundle_host_fitness(
                MUSL_BUNDLE_HOST,
                HostOs::Linux,
                HostArch::X86_64,
                HostLibc::Gnu
            ),
            X64,
            "aarch64-unknown-linux-musl",
            false,
        ),
        MuslBundleDecision::UseCatalogue
    );
}

#[test]
fn an_arm64_host_refuses_the_x86_64_hosted_musl_bundle_before_download() {
    let decision = decide_musl_bundle(
        bundle_host_fitness(
            MUSL_BUNDLE_HOST,
            HostOs::Linux,
            HostArch::Aarch64,
            HostLibc::Gnu,
        ),
        ARM64,
        "aarch64-unknown-linux-musl",
        false,
    );
    let MuslBundleDecision::Reject(message) = decision else {
        panic!("expected a rejection, got {decision:?}");
    };
    assert!(message.contains("soldr#3296"), "{message}");
    assert!(message.contains(ARM64), "{message}");
    assert!(message.contains("aarch64-unknown-linux-musl"), "{message}");
    assert!(message.contains("Exec format error"), "{message}");
}

#[test]
fn the_existing_cross_guard_seam_can_exercise_the_musl_fixture_path() {
    assert_eq!(
        decide_musl_bundle(
            bundle_host_fitness(
                MUSL_BUNDLE_HOST,
                HostOs::MacOs,
                HostArch::Aarch64,
                HostLibc::None
            ),
            "aarch64-apple-darwin",
            "x86_64-unknown-linux-musl",
            true,
        ),
        MuslBundleDecision::UseCatalogue
    );
}

// ── soldr#3435: host libc ────────────────────────────────────────────────

const MUSL_HOST: &str = "x86_64-unknown-linux-musl";

fn alpine(req: crate::fetch::catalogue_linux_host::BundleHostRequirement) -> BundleHostFitness {
    bundle_host_fitness(req, HostOs::Linux, HostArch::X86_64, HostLibc::Musl)
}

#[test]
fn a_musl_host_refuses_the_glibc_dynamic_gnu_bundle_and_says_why() {
    for floor in [None, Some("2.17")] {
        let decision =
            decide_gnu_bundle(alpine(GNU_BUNDLE_HOST), MUSL_HOST, X64, X64, floor, false);
        let GnuBundleDecision::Reject(message) = decision else {
            panic!("expected a rejection, got {decision:?}");
        };
        assert!(message.contains(MUSL_HOST), "{message}");
        assert!(message.contains("musl libc"), "{message}");
        assert!(message.contains("glibc's loader"), "{message}");
        assert!(message.contains(GNU_BUNDLE_HOST.host), "{message}");
        assert!(message.contains("soldr#3435"), "{message}");
        assert!(!message.contains("Exec format error"), "{message}");
    }
}

#[test]
fn a_musl_host_still_uses_the_static_musl_bundle() {
    assert_eq!(
        decide_musl_bundle(
            alpine(MUSL_BUNDLE_HOST),
            MUSL_HOST,
            "x86_64-unknown-linux-musl",
            false
        ),
        MuslBundleDecision::UseCatalogue
    );
}

#[test]
fn cargo_feature_flags_extracts_only_feature_selection() {
    use crate::target_lifecycle::cargo_feature_flags;
    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    assert_eq!(
        cargo_feature_flags(&argv(&["build", "--all-features"])),
        ["--all-features"]
    );
    assert_eq!(
        cargo_feature_flags(&argv(&[
            "build",
            "--no-default-features",
            "--features",
            "a,b"
        ])),
        ["--no-default-features", "--features", "a,b"]
    );
    assert_eq!(
        cargo_feature_flags(&argv(&[
            "build",
            "--features=a,b",
            "--target",
            "x86_64-pc-windows-msvc"
        ])),
        ["--features=a,b"]
    );
    // Package/workspace selection and unrelated flags are NOT forwarded.
    assert!(cargo_feature_flags(&argv(&[
        "build",
        "--release",
        "--workspace",
        "--target",
        "x"
    ]))
    .is_empty());
    // A `--` separator ends the scan.
    assert_eq!(
        cargo_feature_flags(&argv(&["build", "--", "--all-features"])),
        Vec::<String>::new()
    );
}

// soldr#3390: `resolve_prepare_targets` used to fall through to
// `AliasError`'s unrendered `Display`, which named `soldr build` -- a command
// `soldr prepare` never ran. RED on `main`.
#[test]
fn resolve_prepare_targets_names_the_prepare_surface() {
    let error = resolve_prepare_targets("win-x86", false).unwrap_err();
    let message = error.to_string();
    assert!(
        message.starts_with("soldr prepare --target `win-x86`"),
        "{message}"
    );
}

#[test]
fn resolve_prepare_targets_all_error_does_not_name_build() {
    // `all` is only accepted as the whole value; mixing it into a
    // comma-separated list must reject without ever saying `soldr build`.
    let error = resolve_prepare_targets("linux-x64,all", false).unwrap_err();
    assert!(!error.to_string().contains("soldr build"), "{error}");
}

// soldr#4016: `soldr lint --cross-dylint` runs `cargo dylint ... -- --target
// <triple>`; that step needs the same per-target C/asm prep as cross Clippy,
// or cc-rs build scripts fall back to the host `cc`.
#[test]
fn cross_dylint_requires_the_same_target_prep_as_clippy() {
    let args = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
    assert!(cargo_operation_requires_prep(&args(
        "dylint --all -- --workspace --target x86_64-pc-windows-msvc"
    )));
    assert!(cargo_operation_requires_prep(&args(
        "clippy --workspace --target x86_64-pc-windows-msvc"
    )));
}

// soldr#4056: cargo-dylint builds its lint library for the host with no
// `--target`; a Darwin prep promoted into `CARGO_ENCODED_RUSTFLAGS` handed
// `-C link-arg=--target=arm64-apple-darwin` to host build-script links. The
// dylint path keeps target rustflags in the target-scoped key only.
#[test]
fn target_scoped_apply_keeps_target_rustflags_out_of_host_builds() {
    let _lock = crate::TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let target_key = "CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS";
    let names = [target_key, "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"];
    let previous = names.map(std::env::var_os);
    for name in names {
        std::env::remove_var(name);
    }
    let mut prep = BlessedPrep::default();
    prep.env.push((
        target_key.to_string(),
        "-C link-arg=--target=arm64-apple-darwin".to_string(),
    ));

    apply_to_process_target_scoped(&prep);
    let target = std::env::var(target_key).ok();
    let encoded = std::env::var_os("CARGO_ENCODED_RUSTFLAGS");
    let rustflags = std::env::var_os("RUSTFLAGS");
    for (name, value) in names.into_iter().zip(previous) {
        if let Some(value) = value {
            std::env::set_var(name, value);
        } else {
            std::env::remove_var(name);
        }
    }

    assert_eq!(
        target.as_deref(),
        Some("-C link-arg=--target=arm64-apple-darwin")
    );
    assert!(encoded.is_none(), "{encoded:?}");
    assert!(rustflags.is_none(), "{rustflags:?}");
}
