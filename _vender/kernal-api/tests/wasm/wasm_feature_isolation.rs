//! Build-script-backed manifest guard: ordinary users must not inherit the
//! sketch host. The build script uses its existing TOML dependency to parse
//! this contract structurally, so this test does not depend on formatting.

#[test]
fn wasm_engine_is_opt_in_and_not_part_of_full() {
    assert!(
        option_env!("KERNAL_API_WASM_FEATURE_ISOLATION") == Some("verified"),
        "build.rs must structurally verify the opt-in Wasm feature contract"
    );
}
