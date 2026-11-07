// Build scripts are allowed to panic on setup failure: cargo surfaces
// the panic message and fails the build cleanly.
#![allow(clippy::expect_used)]

/// soldr#1597 Phase 2: `release-auto.yml` sets `SOLDR_RELEASE_CI=1` only
/// for the build step that produces published release artifacts. Turn
/// that into a compile-time `SOLDR_OFFICIAL_BUILD` env constant so the
/// official-vs-dev check (`build_provenance::is_official_build`) has
/// zero runtime cost and can't be toggled post-build by setting an env
/// var against an already-compiled dev binary.
fn main() {
    println!("cargo:rerun-if-env-changed=SOLDR_RELEASE_CI");
    if std::env::var("SOLDR_RELEASE_CI").as_deref() == Ok("1") {
        println!("cargo:rustc-env=SOLDR_OFFICIAL_BUILD=1");
    }
}
