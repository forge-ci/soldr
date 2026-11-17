use super::*;

#[test]
fn parses_positive_negative_and_none() {
    assert_eq!(parse_zstd_level("3"), Ok(3));
    assert_eq!(parse_zstd_level(" 19 "), Ok(19));
    assert_eq!(parse_zstd_level("22"), Ok(22));
    assert_eq!(parse_zstd_level("-1"), Ok(-1));
    assert_eq!(parse_zstd_level("-7"), Ok(-7));
    // Archives are always zstd frames: `none` and 0 select the fastest
    // standard level rather than libzstd's 0 (= default level 3).
    assert_eq!(parse_zstd_level("none"), Ok(1));
    assert_eq!(parse_zstd_level("NONE"), Ok(1));
    assert_eq!(parse_zstd_level("0"), Ok(1));
}

#[test]
fn rejects_out_of_range_and_junk() {
    for raw in ["23", "-200000", "fast", "", "3.5", "1e2"] {
        assert!(parse_zstd_level(raw).is_err(), "{raw:?} must be rejected");
    }
}

#[test]
fn env_values_fall_back_safely() {
    assert_eq!(level_from_env_value(None), None);
    assert_eq!(level_from_env_value(Some("")), None);
    assert_eq!(level_from_env_value(Some("  ")), None);
    assert_eq!(level_from_env_value(Some("junk")), None);
    assert_eq!(level_from_env_value(Some("99")), None);
    assert_eq!(level_from_env_value(Some("-3")), Some(-3));
    assert_eq!(level_from_env_value(Some("1")), Some(1));
}

#[test]
fn flag_beats_env_beats_default() {
    assert_eq!(resolve_zstd_level(Some(9), Some(1), 3), 9);
    assert_eq!(resolve_zstd_level(None, Some(1), 3), 1);
    assert_eq!(resolve_zstd_level(None, None, 3), 3);
    assert_eq!(resolve_zstd_level(Some(-2), None, 19), -2);
}
