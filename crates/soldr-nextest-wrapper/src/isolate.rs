//! Every test runs in a private soldr state root (soldr#3516).
//!
//! soldr's tests start soldr brokers and daemons. Those resolve their state
//! from the process environment, not from anything a test is obliged to set:
//!
//! * the state root is `$SOLDR_CACHE_DIR`, else `$HOME/.soldr`
//!   (`soldr-core` `SoldrPaths::new`);
//! * the pinned-tool directory is `$HOME/.soldr/bin` even when
//!   `SOLDR_CACHE_DIR` moves the root (`SoldrPaths` `pinned_bin`, issue #426);
//! * the per-user broker is `$HOME/.soldr/broker/soldr-broker`, with its
//!   socket fallback and lease database under `$XDG_RUNTIME_DIR`
//!   (`soldr-cli` `broker_identity.rs` `ResolvedBrokerEndpoint::resolve`);
//! * a broker spawn forwards exactly `HOME`, `XDG_CONFIG_HOME`,
//!   `XDG_RUNTIME_DIR` and `TMPDIR` (`broker_spawn.rs`
//!   `HOME_IDENTITY_ENV_VARS`).
//!
//! A test that forgot to override one of them reached the developer's real
//! `~/.soldr` and broker; a leaked fixture daemon once claimed that root and
//! wedged every soldr build on the machine. Isolation was a per-test
//! convention, so it could not be relied on. This wrapper is the one place
//! every Unix test process passes through, so it overrides all of them,
//! unconditionally, before the test starts: no test can reach the caller's
//! soldr state unless it names that path explicitly. The caller's toolchain
//! homes stay reachable through `~/.cargo` / `~/.rustup` symlinks in the
//! private home, because they default from `HOME` and are not soldr state.

use soldr_platform::process::test_child;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Exported to every isolated test: "this process runs in a wrapper-private
/// soldr root". `nextest_wrapper.sh` trusts an override of this wrapper only
/// under it (or `CI=true`), so a nested run cannot bypass the isolation.
pub(crate) const ISOLATED_ENV: &str = "SOLDR_TEST_ISOLATED";

/// What an outer `soldr cargo` front door exports to Cargo, which Nextest
/// hands on to every test: the outer build's route, session and wrapper
/// shim. Each names state in the *caller's* root (the shim lives under the
/// caller's `~/.soldr`, the route belongs to the caller's broker), so a test
/// inheriting them would dial the caller's daemon -- or, with its private
/// root, a route that does not exist there ("could not connect to the soldr
/// daemon (route ...)"). A test that wants a front door starts its own.
const OUTER_SESSION_ENVS: [&str; 5] = [
    "SOLDR_BROKER_SERVICE",
    "SOLDR_BUILD_SESSION_ID",
    "SOLDR_EFFECTIVE_RUSTC_WRAPPER",
    "SOLDR_EFFECTIVE_RUSTC_WRAPPER_ORIGIN",
    "RUSTC_WRAPPER",
];

/// The variables a test's soldr state is resolved from, and where each one
/// points inside the private root.
fn identity(root: &Path) -> [(&'static str, PathBuf); 7] {
    let home = root.join("h");
    [
        ("SOLDR_CACHE_DIR", home.join(".soldr")),
        ("XDG_CONFIG_HOME", home.join(".config")),
        ("XDG_CACHE_HOME", home.join(".cache")),
        ("XDG_DATA_HOME", home.join(".local").join("share")),
        ("XDG_STATE_HOME", home.join(".local").join("state")),
        ("XDG_RUNTIME_DIR", root.join("r")),
        ("HOME", home),
    ]
}

/// Point every soldr state input of `command` inside `root`, which the
/// caller has already created (mode 0700). Fails when the layout cannot be
/// created: a test never runs against the caller's state instead.
pub(crate) fn apply(command: &mut Command, root: &Path) -> std::io::Result<()> {
    let home = root.join("h");
    std::fs::create_dir(&home)?;
    // The toolchain homes default from HOME and are not soldr state. Resolve
    // them from the caller's environment before moving HOME, and reach them
    // through symlinks in the private home rather than by exporting
    // CARGO_HOME / RUSTUP_HOME: an explicit variable is itself an input the
    // code under test resolves differently from an unset one. A caller that
    // set them explicitly keeps them, unchanged, by inheritance.
    for (name, resolved, leaf) in [
        (
            "CARGO_HOME",
            soldr_core::core::resolve_cargo_home(),
            ".cargo",
        ),
        (
            "RUSTUP_HOME",
            soldr_core::core::resolve_rustup_home(),
            ".rustup",
        ),
    ] {
        if std::env::var_os(name).is_some_and(|value| !value.is_empty()) {
            continue;
        }
        if let Some(target) = resolved.filter(|path| path.is_dir()) {
            test_child::link_dir(&target, &home.join(leaf))?;
        }
    }
    for name in OUTER_SESSION_ENVS {
        command.env_remove(name);
    }
    // The broker's runtime directory must be owner-only, like a real one.
    test_child::create_private_dir(&root.join("r"))?;
    for (name, path) in identity(root) {
        std::fs::create_dir_all(&path)?;
        command.env(name, path);
    }
    command.env(ISOLATED_ENV, "1");
    Ok(())
}

#[cfg(test)]
#[path = "isolate_tests.rs"]
mod tests;
