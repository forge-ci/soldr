//! Unit tests for [`crate::cook_target_prep`].

use super::*;

fn parsed(parts: &[&str]) -> crate::cook::CookArgs {
    let argv: Vec<String> = parts.iter().map(|s| (*s).to_string()).collect();
    crate::cook::parse_cook_args(&argv).expect("parse cook args")
}

#[test]
fn a_canonical_target_cook_gets_the_same_prep_as_a_build() {
    assert_eq!(
        cook_prep_triple(&parsed(&["--target", "x86_64-unknown-linux-gnu"])).as_deref(),
        Some("x86_64-unknown-linux-gnu")
    );
    assert_eq!(
        cook_prep_triple(&parsed(&["--target=aarch64-apple-darwin", "--release"])).as_deref(),
        Some("aarch64-apple-darwin")
    );
}

#[test]
fn a_host_cook_needs_no_target_prep() {
    assert_eq!(cook_prep_triple(&parsed(&["--workspace"])), None);
}

#[test]
fn a_custom_target_spec_is_left_to_cargo() {
    assert_eq!(
        cook_prep_triple(&parsed(&["--target", "my-board.json"])),
        None
    );
}
