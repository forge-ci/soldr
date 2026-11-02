//! Built-in verb lists, split out of `cli_args.rs` (soldr#2361 Phase 2 --
//! `cli_args.rs` is over the repo's loc_ratchet ceiling and adding
//! `Commands::Broker` there needed room; this pair of consts plus their
//! predicate is self-contained enough to relocate without touching the
//! `Commands` enum itself).

/// Flat list of every built-in soldr verb that clap recognizes,
/// PLUS aliases (for example, `purge-targets` for `gc`). Used by the
/// fuzzy-match suggestion
/// path in `Commands::External` (issue #412) to detect typos /
/// pre-rename verbs that fell through to the external-tool fetch.
///
/// Must stay in sync with the `Commands` enum + `#[command(alias = ...)]`
/// attributes below. A unit test in `main_tests.rs` walks the const
/// against clap's discovered subcommands and fails when they drift,
/// so adding a new verb here without updating the enum (or vice
/// versa) trips the build.
/// Cargo's own first-party verbs that — when typed bare as
/// `soldr <verb>` — should be routed through soldr's cargo front
/// door (`soldr cargo <verb> ...`) instead of falling through
/// `Commands::External` and attempting a doomed crates.io fetch
/// for a literally-named crate (`build`, `test`, etc. are not
/// real crates). Issue #685, phase 2 of #682.
///
/// The collision verbs `clean`, `config`, and `version` are
/// deliberately EXCLUDED — those map to soldr-native built-ins
/// that clap captures before the External arm runs. `soldr cargo
/// clean` / `soldr cargo config` continue to work as the explicit
/// escape hatch. A unit test in `main_tests.rs` asserts the
/// exclusion stays in place.
///
/// The list is intentionally a superset of cargo's own `--list`
/// output of first-party commands; new cargo verbs (rare) need an
/// explicit add here.
pub(crate) const CARGO_BUILTIN_VERBS: &[&str] = &[
    // `build` is intentionally NOT here. As of soldr#1012 PR 1, `build`
    // is a soldr-native verb (`Commands::Build`) — clap captures it
    // before the External arm runs. It joins `clean`, `config`, and
    // `version` as a collision verb whose meaning is owned by soldr.
    // `soldr cargo build` is the explicit legacy-passthrough escape
    // hatch; `soldr build` is the blessed default.
    "test",
    "check",
    "run",
    "bench",
    "doc",
    "fmt",
    "clippy",
    "tree",
    "update",
    "fix",
    "add",
    "remove",
    "metadata",
    "pkgid",
    "search",
    "vendor",
    "yank",
    "owner",
    "login",
    "logout",
    "init",
    "new",
    "generate-lockfile",
    "verify-project",
    "locate-project",
    "report",
    // `install` is NOT here: soldr#2310 promoted it to `Commands::Install`.
    "uninstall",
    "publish",
    // soldr#3637: without these, `soldr package` / `soldr info` fell
    // through to a crates.io tool fetch instead of `cargo <verb>`.
    "package",
    "info",
];

/// Predicate form of [`CARGO_BUILTIN_VERBS`]. Lives next to the const
/// so callers (the External arm dispatcher and the tests) share one
/// source of truth.
pub(crate) fn is_cargo_builtin_verb(verb: &str) -> bool {
    CARGO_BUILTIN_VERBS.contains(&verb)
}

pub(crate) const SOLDR_BUILTIN_VERBS: &[&str] = &[
    // soldr#1012: `build` is a soldr-native verb (the blessed-default
    // surface). It layers catalogue-driven sysroot prep on top of the
    // cargo front door and stays paired with `Commands::Build` in the
    // enum.
    "build",
    "cc",
    "c++",
    // soldr#3806 -- standalone target linkers (GNU ld / MSVC link.exe syntax).
    "ld",
    "link",
    "install", // soldr#2310 — soldr-native verb (Commands::Install)
    // soldr#2139 gap 1 — the blessed abi3 Python wheel surface.
    "wheel",
    "cargo",
    "dylint",
    "blaze", // C002 — local-dev JIT passthrough (Commands::Blaze)
    "cook",
    "lint",
    // soldr#3894 — verdict ledger inspection.
    "verdict",
    "ci-test",
    // soldr#1059 — PATH-prepending escape hatch for cargo extensions.
    "exec",
    "replay-nextest",
    "rustc",
    "rustfmt",
    "clippy-driver",
    "rustdoc",
    "rust-gdb",
    "rust-lldb",
    "rust-analyzer",
    "status",
    "clean",
    "purge",
    "config",
    "cache",
    // soldr#820 phase 1 — `soldr logs` discoverable runtime-log API
    "logs",
    "version",
    "gc",
    "purge-targets", // alias of `gc`
    "rustup",
    "toolchain",
    "bootstrap",
    "doctor",
    "shims",
    "optimize",
    "defender-exclusions",
    // pre-existing drift caught by the SOLDR_BUILTIN_VERBS gate while
    // landing soldr#1012 PR 1 — `Commands::Env` was added but never
    // registered in this const. Belongs here next to other verbs.
    "env",
    "session-start",
    "session-end",
    "save",
    // `Commands::Hydrate` is the primary spelling; `load` is its
    // `visible_alias`. Both are live surfaces, so both belong here — the
    // fuzzy-match path only knows what this const lists.
    "hydrate",
    "load",
    "archive",
    "prepare",
    "build-from-source",
    "daemon",
    "broker", // soldr#2361 Phase 2 (Commands::Broker)
    "shims",
];
