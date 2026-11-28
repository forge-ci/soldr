//! The `file:line` opt-in, shared by both symbol backends (#803).
//!
//! This lives outside `object_symbols` and `pdb_symbols` because those two are
//! mutually exclusive by target: `object_symbols` is compiled off Windows and
//! `pdb_symbols` on it. The opt-in belongs to neither, and a constant that only
//! one platform can see is a constant the other platform will re-spell — which
//! is exactly the drift the repo's env-literal lint exists to prevent. One
//! spelling, visible to both, means a rename cannot leave half the workers
//! reading a name nobody sets.

/// Opt-in for `file:line` resolution.
///
/// Off by default because it is not free: names come from a symbol-table walk,
/// while line numbers parse a line program for every module in the capture. A
/// caller that only wants "which function" should not pay for it.
pub const LINE_NUMBERS_ENV: &str = "KERNAL_API_SYMBOL_LINE_NUMBERS";

/// Whether the caller asked for line numbers.
pub fn line_numbers_requested() -> bool {
    requested_from(std::env::var_os(LINE_NUMBERS_ENV))
}

/// The decision, separated from reading the environment.
///
/// Split so it can be tested without `set_var`. Env-mutating tests race under
/// a parallel runner — one test's variable leaks into another's read — and
/// this repo has already been bitten by exactly that. A pure function takes
/// the value as an argument and cannot.
pub fn requested_from(value: Option<std::ffi::OsString>) -> bool {
    // Present-and-not-"0": `=1`, `=true`, or a bare set all mean yes, and an
    // explicit `=0` means no. Treating any set value as yes would make
    // `LINE_NUMBERS=0` enable the thing it names.
    value.is_some_and(|value| value != "0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_opt_in_is_off_by_default_and_respects_an_explicit_zero() {
        use std::ffi::OsString;
        // Unset: off. Line resolution costs a line-program parse per module,
        // so a caller that never asked must not pay for it.
        assert!(!requested_from(None));
        // Set to anything meaningful: on.
        assert!(requested_from(Some(OsString::from("1"))));
        assert!(requested_from(Some(OsString::from("true"))));
        // Bare set, no value: on. Matches how the other probe env vars behave.
        assert!(requested_from(Some(OsString::from(""))));
        // Explicitly zero: off. Without this branch, `LINE_NUMBERS=0` would
        // enable the feature it names — the sort of thing nobody reports
        // because they assume they set it wrong.
        assert!(!requested_from(Some(OsString::from("0"))));
    }

    #[test]
    fn the_variable_is_namespaced_to_the_probe() {
        // It is read by every worker the daemon spawns; a generic name would
        // be a collision waiting to happen.
        assert!(LINE_NUMBERS_ENV.starts_with("KERNAL_API_"));
    }
}
