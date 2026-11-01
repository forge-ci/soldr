use super::*;

fn args(out_dir: &Path, crate_type: &str) -> Vec<String> {
    vec![
        "--crate-name".into(),
        "ban_x".into(),
        "src/lib.rs".into(),
        "--crate-type".into(),
        crate_type.into(),
        "--out-dir".into(),
        out_dir.to_string_lossy().into_owned(),
    ]
}

fn lib(name: &str) -> String {
    format!(
        "{}{name}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )
}

#[test]
fn profile_dir_is_derived_only_from_the_build_dir_layout() {
    let profile = Path::new("t").join("release");
    let out = profile
        .join("build")
        .join("ban_x")
        .join("71a8baebcfb0f93d")
        .join("out");
    assert_eq!(profile_dir_of_build_out(&out), Some(profile.clone()));
    assert_eq!(profile_dir_of_build_out(&profile.join("deps")), None);
    let bad_hash = profile.join("build").join("ban_x").join("xyz").join("out");
    assert_eq!(profile_dir_of_build_out(&bad_hash), None);
}

#[test]
fn only_cdylib_invocations_get_a_plan() {
    let out = Path::new("r/build/ban_x/0123456789abcdef/out");
    assert!(UpliftPlan::from_rustc_args(&args(out, "lib")).is_none());
    let plan = UpliftPlan::from_rustc_args(&args(out, "cdylib")).expect("plan");
    assert_eq!(plan.profile_dir, PathBuf::from("r"));
    assert_eq!(plan.crate_name, "ban_x");
}

/// soldr#3989: the suffixed copy materialized beside the plain library in the
/// build-dir out-dir must reach the profile dir cargo-dylint reads.
#[test]
fn suffixed_copy_is_uplifted_to_the_profile_dir() {
    let temp = tempfile::tempdir().expect("tempdir");
    let profile = temp.path().join("release");
    let out = profile
        .join("build")
        .join("ban_x")
        .join("0123456789abcdef")
        .join("out");
    std::fs::create_dir_all(&out).expect("mkdir");
    let suffixed = lib("ban_x@nightly-2026-08-15-x86_64-unknown-linux-gnu");
    std::fs::write(out.join(&suffixed), b"lint").expect("write");
    std::fs::write(out.join(lib("ban_x")), b"lint").expect("write");
    std::fs::write(out.join(lib("other@nightly")), b"x").expect("write");

    uplift_suffixed_library(&args(&out, "cdylib"));

    assert_eq!(
        std::fs::read(profile.join(&suffixed)).expect("uplifted"),
        b"lint"
    );
    assert!(!profile.join(lib("other@nightly")).exists());
}
