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
//! `PATH` entries inside the caller's soldr root (the front door's
//! `~/.soldr/<version>/shims`, its linker shims, the pinned `~/.soldr/bin`)
//! are dropped, and so is every variable naming a program there (`CC`,
//! `CARGO_TARGET_<triple>_LINKER`, ...): a test running one would re-enter
//! the caller's soldr and dial the caller's broker.

use soldr_platform::process::test_child;
use std::ffi::{OsStr, OsString};
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

/// The caller's soldr state roots, as the code under test would resolve them
/// from the wrapper's own environment: `$SOLDR_CACHE_DIR` and `$HOME/.soldr`
/// (the pinned-tool and shim directories stay under `$HOME/.soldr` even when
/// `SOLDR_CACHE_DIR` moves the root).
fn caller_soldr_roots() -> Vec<PathBuf> {
    let non_empty = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    let mut roots: Vec<PathBuf> = non_empty("SOLDR_CACHE_DIR")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    if let Some(home) = non_empty("HOME") {
        roots.push(PathBuf::from(home).join(".soldr"));
    }
    roots
}

/// Is `path` inside one of `roots`? Judged both as written and canonicalized
/// on each side, so a symlinked (or, on Windows, `\\?\`-prefixed) spelling
/// of either is judged by what it resolves to.
fn inside_any(path: &Path, roots: &[PathBuf]) -> bool {
    let canonical_roots: Vec<PathBuf> = roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect();
    let under = |candidate: &Path| {
        roots
            .iter()
            .chain(&canonical_roots)
            .any(|root| candidate.starts_with(root))
    };
    under(path) || std::fs::canonicalize(path).is_ok_and(|resolved| under(&resolved))
}

/// `path` without the entries that lie inside any of `roots`.
fn strip_entries_under(
    path: &OsStr,
    roots: &[PathBuf],
) -> Result<OsString, std::env::JoinPathsError> {
    std::env::join_paths(
        std::env::split_paths(path)
            .filter(|entry| entry.as_os_str().is_empty() || !inside_any(entry, roots)),
    )
}

/// Does the variable value `value` name a program inside `roots`? Its first
/// whitespace-separated word is the program (`CC` may read
/// `<shim> <real compiler>`): an absolute path is judged directly, a bare
/// name by where `path` resolves it -- `clang-<digest>` is a linker shim the
/// outer front door put on PATH.
fn names_program_under(value: &OsStr, path: &OsStr, roots: &[PathBuf]) -> bool {
    let Some(program) = value
        .to_str()
        .and_then(|text| text.split_whitespace().next())
    else {
        return false;
    };
    let program = Path::new(program);
    if program.is_absolute() {
        return inside_any(program, roots);
    }
    if program.components().count() != 1 {
        return false;
    }
    std::env::split_paths(path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .is_some_and(|found| inside_any(&found, roots))
}

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
    // Anything else the outer front door exported that names a program in
    // the caller's root -- `CC`/`CXX` (its native-compiler shim),
    // `CARGO_TARGET_<triple>_LINKER` (a `clang-<digest>` linker shim found
    // through PATH) -- would run the caller's soldr. Drop the variable; a
    // user's own setting, which names nothing in a soldr root, is kept.
    let roots = caller_soldr_roots();
    let path = std::env::var_os("PATH").unwrap_or_default();
    for (name, value) in std::env::vars_os() {
        if name != "PATH" && names_program_under(&value, &path, &roots) {
            command.env_remove(name);
        }
    }
    let stripped = strip_entries_under(&path, &roots)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    command.env("PATH", stripped);
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
