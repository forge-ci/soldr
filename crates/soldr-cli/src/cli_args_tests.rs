//! Tests for the global `Cli` flags that have an environment-variable
//! spelling (soldr#1802).
//!
//! Split out of `cli_args.rs` via `#[path]`: adding the
//! `--timestamp-lines` pair pushed that file past the 1,500-line ceiling,
//! and the ratchet correctly refused to let it grow. Same mechanism
//! `daemon/wire_tests.rs` uses.

use super::*;
use crate::TEST_PROCESS_ENV_LOCK as ENV_LOCK;

use clap::Parser;
// The barrier every env mutator in this crate takes (soldr#1663). These
// tests set process-global variables, so an unguarded run races any test
// that reads them.

fn restore(name: &str, previous: Option<String>) {
    match previous {
        Some(v) => std::env::set_var(name, v),
        None => std::env::remove_var(name),
    }
}

// soldr#1761. The flag is deliberately *not* a second resolution point:
// it publishes SOLDR_JOBS and lets `core::jobs` decide precedence, so a
// daemon spawned by this invocation reads the same tier a plain
// `SOLDR_JOBS=N` export would have populated.
#[test]
fn jobs_flag_publishes_the_env_var_the_resolver_reads() {
    let _lock = ENV_LOCK.lock();
    let name = soldr_core::core::jobs::SOLDR_JOBS_ENV_VAR;
    let previous = std::env::var(name).ok();
    std::env::remove_var(name);

    let cli = Cli::parse_from(["soldr", "--jobs", "3", "status"]);
    cli.export_global_env();
    let published = std::env::var(name).ok();

    restore(name, previous);
    assert_eq!(
        published.as_deref(),
        Some("3"),
        "--jobs must populate the resolver's top tier, not a parallel one"
    );
}

// soldr#1802. Same contract as --jobs: publish the variable that
// `should_timestamp` already reads rather than becoming a second
// decision point that could disagree with it.
#[test]
fn timestamp_lines_flag_publishes_the_env_var() {
    let _lock = ENV_LOCK.lock();
    let name = crate::cargo_front_door::timestamp_tee::TIMESTAMP_LINES_ENV_VAR;
    let previous = std::env::var(name).ok();

    std::env::remove_var(name);
    Cli::parse_from(["soldr", "--timestamp-lines", "status"]).export_global_env();
    let bare = std::env::var(name).ok();

    std::env::remove_var(name);
    Cli::parse_from(["soldr", "--no-timestamp-lines", "status"]).export_global_env();
    let off = std::env::var(name).ok();

    restore(name, previous);
    // The bare flag means "on" -- otherwise `--timestamp-lines` alone
    // would read as a request and do nothing.
    assert_eq!(bare.as_deref(), Some("1"));
    // And an explicit false must be publishable, since CI is the one
    // place the prefix is on by default and the one place a downstream
    // parser would need it off.
    assert_eq!(off.as_deref(), Some("0"));
}

// Whatever the flag publishes must be a spelling the resolver accepts;
// agreeing on the variable name but not its vocabulary would be a
// silent no-op.
#[test]
fn published_timestamp_values_round_trip_through_the_resolver() {
    use crate::cargo_front_door::timestamp_tee::should_timestamp;
    // is_terminal = true, where the default is off, so only an honoured
    // override can turn it on.
    assert!(
        should_timestamp(Some("1"), true),
        "published on-value ignored"
    );
    // is_terminal = false, where the default is on, so only an honoured
    // override can turn it off.
    assert!(
        !should_timestamp(Some("0"), false),
        "published off-value ignored"
    );
}

#[test]
fn no_timestamp_lines_flag_leaves_the_env_var_alone() {
    let _lock = ENV_LOCK.lock();
    let name = crate::cargo_front_door::timestamp_tee::TIMESTAMP_LINES_ENV_VAR;
    let previous = std::env::var(name).ok();
    std::env::set_var(name, "0");

    Cli::parse_from(["soldr", "status"]).export_global_env();
    let after = std::env::var(name).ok();

    restore(name, previous);
    assert_eq!(
        after.as_deref(),
        Some("0"),
        "an exported SOLDR_TIMESTAMP_LINES must survive an invocation without the flag"
    );
}

// soldr#2302. Same contract: the flag publishes the env spelling the front
// door's `cache_states::enabled()` reads, and its absence leaves it untouched.
#[test]
fn no_cache_states_flag_publishes_the_env_var() {
    let _lock = ENV_LOCK.lock();
    let name = crate::cargo_front_door::cache_states::NO_CACHE_STATES_ENV_VAR;
    let previous = std::env::var(name).ok();

    std::env::remove_var(name);
    Cli::parse_from(["soldr", "--no-cache-states", "cargo", "build"]).export_global_env();
    let set = std::env::var(name).ok();

    std::env::remove_var(name);
    Cli::parse_from(["soldr", "cargo", "build"]).export_global_env();
    let unset = std::env::var(name).ok();

    restore(name, previous);
    assert_eq!(
        set.as_deref(),
        Some("1"),
        "--no-cache-states must publish the opt-out"
    );
    assert_eq!(unset, None, "without the flag the opt-out must stay unset");
}

// Non-global by deliberate choice, so it must PRECEDE the verb --
// same placement rule as `--no-cache`. It is not `global = true`
// because a global arg is cloned into every subcommand, and building
// soldr's clap Command that way came within one argument of
// exhausting the 1 MiB Windows main-thread stack (see
// `main.rs::CLI_STACK_BYTES`).
#[test]
fn timestamp_lines_precedes_the_verb() {
    let cli = Cli::parse_from(["soldr", "--timestamp-lines", "status"]);
    assert!(cli.timestamp_lines);

    // After a clap-captured verb it is not accepted at all.
    assert!(Cli::try_parse_from(["soldr", "status", "--timestamp-lines"]).is_err());
}

// After `cargo`, args are raw passthrough and belong to cargo; soldr
// must not steal this one. Getting the placement wrong is the mistake
// a user actually makes, so it is pinned.
#[test]
fn timestamp_lines_must_precede_the_cargo_passthrough() {
    let after = Cli::parse_from(["soldr", "cargo", "build", "--timestamp-lines"]);
    assert!(!after.timestamp_lines);

    let before = Cli::parse_from(["soldr", "--timestamp-lines", "cargo", "build"]);
    assert!(before.timestamp_lines);
}

// Both at once is a parse error rather than a silent precedence rule.
#[test]
fn the_two_timestamp_flags_conflict() {
    assert!(Cli::try_parse_from([
        "soldr",
        "--timestamp-lines",
        "--no-timestamp-lines",
        "status"
    ])
    .is_err());
}

// Absent flag must leave the variable untouched rather than writing a
// default: an exported SOLDR_JOBS, or a config.toml value, has to keep
// winning over "the user did not pass --jobs".
#[test]
fn no_jobs_flag_leaves_the_env_var_alone() {
    let _lock = ENV_LOCK.lock();
    let name = soldr_core::core::jobs::SOLDR_JOBS_ENV_VAR;
    let previous = std::env::var(name).ok();
    std::env::set_var(name, "9");

    let cli = Cli::parse_from(["soldr", "status"]);
    cli.export_global_env();
    let after = std::env::var(name).ok();

    restore(name, previous);
    assert_eq!(
        after.as_deref(),
        Some("9"),
        "an absent flag must not overwrite an existing SOLDR_JOBS"
    );
}

// `global = true` is what makes `soldr cargo build --jobs 4` parse; a
// non-global flag would only be accepted before the subcommand.
#[test]
fn jobs_flag_is_accepted_after_the_subcommand() {
    let cli = Cli::parse_from(["soldr", "status", "--jobs", "4"]);
    assert_eq!(cli.jobs, Some(4));
}

// soldr#2546. Same contract as --timestamp-lines: publish the variable
// `debug_trace::enabled()` reads so nested soldr invocations inherit it.
#[test]
fn debug_flag_publishes_the_env_var() {
    let _lock = ENV_LOCK.lock();
    let name = crate::cargo_front_door::debug_trace::DEBUG_TRACE_ENV_VAR;
    let previous = std::env::var(name).ok();

    std::env::remove_var(name);
    Cli::parse_from(["soldr", "--debug", "status"]).export_global_env();
    let on = std::env::var(name).ok();

    std::env::remove_var(name);
    Cli::parse_from(["soldr", "status"]).export_global_env();
    let absent = std::env::var(name).ok();

    restore(name, previous);
    assert_eq!(on.as_deref(), Some("1"));
    assert_eq!(absent, None, "without --debug nothing may be published");
}

// soldr#2546: `--debug` must not collide with cargo's own `--debug`
// meaning (`cargo install --debug`). After `cargo`, args are raw
// passthrough and belong to cargo byte-for-byte.
#[test]
fn debug_flag_must_precede_the_cargo_passthrough() {
    let after = Cli::parse_from(["soldr", "cargo", "install", "--debug", "cargo-audit"]);
    assert!(!after.debug, "--debug after `cargo` belongs to cargo");
    match &after.command {
        Commands::Cargo { args } => {
            assert_eq!(
                args,
                &[
                    "install".to_string(),
                    "--debug".into(),
                    "cargo-audit".into()
                ],
                "cargo's own --debug must pass through untouched"
            );
        }
        _ => panic!("expected the cargo passthrough"),
    }

    let before = Cli::parse_from(["soldr", "--debug", "cargo", "build"]);
    assert!(before.debug);
    match &before.command {
        Commands::Cargo { args } => assert_eq!(args, &["build".to_string()]),
        _ => panic!("expected the cargo passthrough"),
    }
}

// soldr#3407. Same contract as --jobs: publish SOLDR_ZCCACHE_MODE, the top
// tier `core::materialization_mode` reads, in the canonical spelling
// zccache documents, rather than becoming a second decision point.
#[test]
fn zccache_mode_flag_publishes_the_env_var_the_resolver_reads() {
    let _lock = ENV_LOCK.lock();
    let name = soldr_core::core::materialization_mode::SOLDR_ZCCACHE_MODE_ENV_VAR;
    let previous = std::env::var(name).ok();

    let mut published = Vec::new();
    for (flag, canonical) in [
        ("auto", "AUTO"),
        ("link", "LINK"),
        ("copy", "COPY"),
        ("reflink", "REFLINK"),
        ("reflink-or-link-or-copy", "REFLINK_OR_LINK_OR_COPY"),
    ] {
        std::env::remove_var(name);
        Cli::parse_from(["soldr", "--zccache-mode", flag, "status"]).export_global_env();
        published.push((canonical, std::env::var(name).ok()));
    }

    std::env::set_var(name, "COPY");
    Cli::parse_from(["soldr", "status"]).export_global_env();
    let untouched = std::env::var(name).ok();

    restore(name, previous);
    for (canonical, value) in published {
        assert_eq!(value.as_deref(), Some(canonical));
    }
    assert_eq!(
        untouched.as_deref(),
        Some("COPY"),
        "an absent flag must not overwrite an existing SOLDR_ZCCACHE_MODE"
    );
}

#[test]
fn zccache_mode_flag_is_global_and_rejects_unknown_modes() {
    let cli = Cli::parse_from(["soldr", "status", "--zccache-mode", "reflink"]);
    assert_eq!(cli.zccache_mode, Some(ZccacheModeArg::Reflink));
    let cli = Cli::parse_from([
        "soldr",
        "status",
        "--zccache-mode",
        "reflink-or-link-or-copy",
    ]);
    assert_eq!(cli.zccache_mode, Some(ZccacheModeArg::ReflinkOrLinkOrCopy));
    assert!(
        Cli::try_parse_from(["soldr", "--zccache-mode", "hardlink", "status"]).is_err(),
        "an unknown mode is a usage error"
    );
}
