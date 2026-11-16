//! One barrier for the soldr-toolchain catalogue environment variables.
//!
//! soldr#2951: `manifest_lookup_disable` and `manifest_lookup_url_override`
//! used to be their own cargo-generated test binaries, so their bare
//! `set_var` / `remove_var` calls were single-threaded by construction and the
//! comments saying so were true. soldr#2934 consolidated per-file test
//! binaries into category targets, and both modules now run in ONE process, in
//! parallel, mutating the same variables — with each file still reading as
//! correct in isolation. That is precisely the shape `guards::env_lock_lint`
//! exists to catch, and precisely why "CI is green" was not evidence: nextest
//! gives every test its own process, so the race is invisible to the runner
//! the project actually uses.
//!
//! Scoped deliberately to the catalogue variables instead of aliasing onto
//! `TEST_PROCESS_ENV_LOCK`. Per that lint's own header, collapsing barriers
//! over *disjoint* variables buys no correctness and starved a test with a
//! short deadline the one time it was tried. The rule it enforces is that one
//! variable has one barrier — which this satisfies, because nothing else in
//! this crate's tests writes these names.
//!
//! Restoring beats clearing on teardown. These tests can run under an outer
//! dogfooded soldr that has its own catalogue configuration, so a blanket
//! `remove_var` would silently hand every later test a different environment
//! than it started with.

#![allow(dead_code)]

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};

use soldr_cli::fetch::manifest_lookup::{
    MANIFEST_DISABLE_ENV_VAR, TOOLCHAIN_CATALOGUE_URL_ENV_VAR, TOOLCHAIN_CATALOGUE_V2_URL_ENV_VAR,
    TOOLCHAIN_ORIGIN_ENV_VAR, TOOLCHAIN_REDIRECT_ENV_VAR, TOOLCHAIN_REDIRECT_URL_ENV_VAR,
};
use soldr_cli::fetch::syslib_common::SYSLIB_ASSET_ORIGIN_ENV_VAR;

/// Every catalogue or syslib origin override the fixture tree mutates.
///
/// Completeness matters twice: a name missing here is a name whose prior value
/// is never restored, and also one a test can leave set for the next test in
/// the binary.
pub(crate) const CATALOGUE_ENV_VARS: &[&str] = &[
    MANIFEST_DISABLE_ENV_VAR,
    TOOLCHAIN_CATALOGUE_URL_ENV_VAR,
    TOOLCHAIN_CATALOGUE_V2_URL_ENV_VAR,
    TOOLCHAIN_ORIGIN_ENV_VAR,
    TOOLCHAIN_REDIRECT_ENV_VAR,
    TOOLCHAIN_REDIRECT_URL_ENV_VAR,
    SYSLIB_ASSET_ORIGIN_ENV_VAR,
];

static CATALOGUE_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Exclusive, restoring access to [`CATALOGUE_ENV_VARS`].
///
/// Acquiring clears all of them so a test starts from a known configuration
/// rather than inheriting whichever one the previous test happened to leave.
pub(crate) struct CatalogueEnvGuard {
    previous: Vec<(&'static str, Option<OsString>)>,
    /// Held for the guard's whole lifetime. Named with a leading underscore
    /// because it is never read — its only job is to exist.
    _barrier: MutexGuard<'static, ()>,
}

impl CatalogueEnvGuard {
    pub(crate) fn acquire() -> Self {
        // The guarded data is `()`, so a lock poisoned by an unrelated panic
        // carries no invariant worth propagating; recovering keeps one failure
        // from becoming every subsequent test's failure too.
        let barrier = CATALOGUE_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = CATALOGUE_ENV_VARS
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect();
        for name in CATALOGUE_ENV_VARS {
            std::env::remove_var(name);
        }
        Self {
            previous,
            _barrier: barrier,
        }
    }

    pub(crate) fn set(&self, name: &str, value: &str) {
        assert!(
            CATALOGUE_ENV_VARS.contains(&name),
            "{name} is not one of the variables this barrier restores; add it to \
             CATALOGUE_ENV_VARS or it will leak into the next test"
        );
        std::env::set_var(name, value);
    }

    pub(crate) fn unset(&self, name: &str) {
        assert!(
            CATALOGUE_ENV_VARS.contains(&name),
            "{name} is not one of the variables this barrier restores; add it to \
             CATALOGUE_ENV_VARS or it will leak into the next test"
        );
        std::env::remove_var(name);
    }
}

impl Drop for CatalogueEnvGuard {
    fn drop(&mut self) {
        // Runs before `_barrier` is released, so no waiter can observe the
        // half-restored state. "Was unset" is restored as unset, not as empty:
        // the production snapshot treats a blank value as unset, but other
        // readers need not, and a test must not rewrite the process it found.
        for (name, value) in &self.previous {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}
