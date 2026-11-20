//! One zstd level knob for soldr's archive writers: `SOLDR_ZSTD_LEVEL`.
//!
//! soldr compresses in-process with libzstd (`zstdmt`, all cores), so the
//! zstd CLI's `ZSTD_CLEVEL` never reaches it. `SOLDR_ZSTD_LEVEL` is the one
//! level knob for `soldr save` (default 3) and the cook packer (default 19):
//! a self-hosted runner with fast local disks sets a fast level, a hosted
//! runner leaves it unset and keeps the defaults. An explicit
//! `--zstd-level` still wins.
//!
//! Accepted: an integer in libzstd's range, negative "fast" levels included,
//! or `none`/`0`. Every archive soldr writes is a zstd frame, so `none`
//! and `0` select level 1, the fastest standard level (libzstd itself would
//! read 0 as its default, 3). An invalid environment value is ignored with a
//! warning: a typo must not fail a cache save.

/// Environment variable read by `soldr save` and the cook packer.
pub const ZSTD_LEVEL_ENV: &str = "SOLDR_ZSTD_LEVEL";

/// Level used for `none` and `0`.
pub const NONE_LEVEL: i32 = 1;

/// Parse a level: an integer in libzstd's range, or `none`/`0`.
pub fn parse_zstd_level(raw: &str) -> Result<i32, String> {
    let value = raw.trim();
    if value.eq_ignore_ascii_case("none") {
        return Ok(NONE_LEVEL);
    }
    let level: i32 = value
        .parse()
        .map_err(|_| format!("zstd level {raw:?} is not an integer or \"none\""))?;
    if level == 0 {
        return Ok(NONE_LEVEL);
    }
    let range = zstd::compression_level_range();
    if !range.contains(&level) {
        return Err(format!(
            "zstd level {level} is outside {}..={}",
            range.start(),
            range.end()
        ));
    }
    Ok(level)
}

/// The level an environment value selects; `None` when unset, empty or
/// invalid (invalid values are reported on stderr).
pub fn level_from_env_value(raw: Option<&str>) -> Option<i32> {
    let raw = raw?;
    if raw.trim().is_empty() {
        return None;
    }
    match parse_zstd_level(raw) {
        Ok(level) => Some(level),
        Err(err) => {
            eprintln!("soldr: ignoring {ZSTD_LEVEL_ENV}: {err}");
            None
        }
    }
}

/// `SOLDR_ZSTD_LEVEL` from the process environment.
pub fn env_zstd_level() -> Option<i32> {
    level_from_env_value(std::env::var(ZSTD_LEVEL_ENV).ok().as_deref())
}

/// An explicit flag wins, then the environment, then `default`.
pub fn resolve_zstd_level(flag: Option<i32>, env: Option<i32>, default: i32) -> i32 {
    flag.or(env).unwrap_or(default)
}

#[cfg(test)]
#[path = "zstd_level_tests.rs"]
mod tests;
