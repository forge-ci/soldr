//! Make "this test performs no network I/O" an enforced invariant
//! rather than a comment (soldr#2159).
//!
//! `cli_build_fetch_overlap` is built on the premise that its target
//! needs no catalogue or network work. That premise held when the file
//! was written and then silently stopped holding four separate times:
//!
//! | hole | closed by |
//! |---|---|
//! | `*-sys` catalogue + cmake/ninja | `SOLDR_USE_LEGACY_VENDORED_SYS`, `SOLDR_USE_SYSTEM_CMAKE` |
//! | toolchain catalogue | `SOLDR_MANIFEST_DISABLE` (#2158) |
//! | managed zig, host-native gnu | `SOLDR_NATIVE_GNU_LINK=0` (#2161) |
//! | managed zig, cross gnu | stub `ZIG` (#2169) |
//!
//! Each fix was locally sufficient and globally incomplete, because the
//! invariant was enforced by a growing list of opt-outs that had to be
//! kept in step with production by hand. Nothing made a *new* fetch
//! path fail; it just made the suite slow, and then it aborted against
//! the 120s watchdog with no output naming the cause.
//!
//! This inverts that. `SOLDR_TEST_NO_NETWORK=1` makes constructing an
//! HTTP client a hard error, so a fifth hole fails immediately and says
//! which fetch opened it.
//!
//! The HTTP client is the chokepoint on purpose: it is the one thing
//! every fetch needs, and unlike [`super::retry::with_backoff`] it has
//! no bypass — `zig.rs` and `forge_dispatch.rs` keep their own retry
//! loops, so a guard in the retry helper would have missed exactly the
//! hole #2169 closed.
//!
//! # Which suites may arm this
//!
//! It propagates to child `soldr` processes: `isolated_soldr_command`
//! does not `env_clear`, and this var is not in its removal list, so
//! setting it on the parent reaches the children that actually fetch.
//!
//! Two boundaries make it wrong for most suites, both found by trying:
//!
//! **It cannot tell localhost from the internet.** It refuses *client
//! construction*, not a destination. `manifest_lookup.rs` and
//! `manifest_lookup_url_override.rs` spin up one-shot local HTTP
//! servers and point soldr at them deliberately — arming the guard
//! there would fail tests that are behaving exactly as intended.
//!
//! **A pass on a warm machine is not proof.** `cli_cargo_basic` passes
//! with the guard armed here, but that is warm-cache evidence: a suite
//! that would fetch a tool once on a cold runner turns into a hard CI
//! failure the moment it is armed, because this errors rather than
//! degrading. Arming a suite therefore needs an audit of what it can
//! fetch on a *cold* machine, not a green local run.
//!
//! `cli_build_fetch_overlap` qualifies because it stubs cargo, rustc,
//! rustup and zig, and every remaining fetch path it touches has an
//! explicit opt-out — the four that #2158/#2161/#2169 had to find one
//! at a time. That audit is what earns the guard, not the green run.

use crate::core::SoldrError;

/// Set to a truthy value to refuse all outbound HTTP. Test-only: the
/// production paths never set it, and an unset var is the normal case.
pub(crate) const NO_NETWORK_ENV_VAR: &str = "SOLDR_TEST_NO_NETWORK";

/// Error out when the caller is inside a no-network test.
///
/// `what` names the fetch, because the whole point is that the failure
/// says which code path reached for the network rather than leaving
/// someone to bisect a timeout.
pub(crate) fn ensure_network_allowed(what: &str) -> Result<(), SoldrError> {
    if !no_network_enabled() {
        return Ok(());
    }
    Err(SoldrError::Other(format!(
        "{NO_NETWORK_ENV_VAR} is set: refusing to build an HTTP client for {what}. \
         A test that sets this asserts it performs no network I/O, so this is a \
         code path reaching for the network that the test did not account for \
         -- not a flake. See soldr#2159."
    )))
}

fn no_network_enabled() -> bool {
    match std::env::var_os(NO_NETWORK_ENV_VAR) {
        None => false,
        Some(value) => {
            let raw = value.to_string_lossy();
            let trimmed = raw.trim();
            !(trimmed.is_empty()
                || trimmed.eq_ignore_ascii_case("0")
                || trimmed.eq_ignore_ascii_case("false")
                || trimmed.eq_ignore_ascii_case("no")
                || trimmed.eq_ignore_ascii_case("off"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialised because these mutate process env.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn unset_allows_the_network() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::remove_var(NO_NETWORK_ENV_VAR);
        assert!(ensure_network_allowed("the catalogue").is_ok());
    }

    #[test]
    fn a_truthy_value_refuses_and_names_the_fetch() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var(NO_NETWORK_ENV_VAR, "1");
        let err = ensure_network_allowed("managed zig").expect_err("must refuse");
        let rendered = err.to_string();
        // Naming the fetch is the feature: #2159 cost four rounds
        // precisely because the stall named nothing.
        assert!(rendered.contains("managed zig"), "{rendered}");
        assert!(rendered.contains("soldr#2159"), "{rendered}");
        std::env::remove_var(NO_NETWORK_ENV_VAR);
    }

    #[test]
    fn the_guard_is_wired_into_a_real_client_constructor() {
        // The unit tests above only prove the predicate. This proves the
        // guard is actually reached from a constructor -- otherwise the
        // module could be correct and connected to nothing, which is the
        // failure mode a hermeticity guard can least afford.
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var(NO_NETWORK_ENV_VAR, "1");
        let refused = super::super::stream_download::control_http_client("test");
        std::env::remove_var(NO_NETWORK_ENV_VAR);
        let err = refused.expect_err("http_client must refuse under the guard");
        assert!(err.to_string().contains(NO_NETWORK_ENV_VAR), "{err}");

        // ...and that it is off by default, or every real fetch breaks.
        assert!(super::super::stream_download::control_http_client("test").is_ok());
    }

    #[test]
    fn falsy_spellings_leave_the_network_alone() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for falsy in ["0", "false", "no", "off", ""] {
            std::env::set_var(NO_NETWORK_ENV_VAR, falsy);
            assert!(
                ensure_network_allowed("x").is_ok(),
                "{falsy:?} must not enable the guard"
            );
        }
        std::env::remove_var(NO_NETWORK_ENV_VAR);
    }
}
