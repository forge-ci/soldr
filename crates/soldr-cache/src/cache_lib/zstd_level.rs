//! One zstd level knob for soldr's archive writers: `SOLDR_ZSTD_LEVEL`.
//! RED stub.

/// Environment variable read by `soldr save` and the cook packer.
pub const ZSTD_LEVEL_ENV: &str = "SOLDR_ZSTD_LEVEL";

/// Parse a level: an integer in zstd's range, or `none`/`0`.
pub fn parse_zstd_level(_raw: &str) -> Result<i32, String> {
    Err("unimplemented".into())
}

/// The level an environment value selects; `None` when unset or invalid.
pub fn level_from_env_value(_raw: Option<&str>) -> Option<i32> {
    None
}

/// `SOLDR_ZSTD_LEVEL` from the process environment.
pub fn env_zstd_level() -> Option<i32> {
    level_from_env_value(std::env::var(ZSTD_LEVEL_ENV).ok().as_deref())
}

/// An explicit flag wins, then the environment, then `default`.
pub fn resolve_zstd_level(_flag: Option<i32>, _env: Option<i32>, default: i32) -> i32 {
    default
}

#[cfg(test)]
#[path = "zstd_level_tests.rs"]
mod tests;
