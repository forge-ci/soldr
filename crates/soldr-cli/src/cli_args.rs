//! clap derives for the `soldr` binary (top-level `Cli` + every
//! subcommand enum / args struct). Extracted from `main.rs` to keep
//! that file under the soldr-wide 1 000-LOC source budget enforced by
//! `ci/hooks/loc_guard.py` (warn >1K, block >1.5K). The split is
//! purely organisational — `main.rs` re-imports each type with
//! `use cli_args::*` so the dispatch path is unchanged.

use crate::broker_cmd::BrokerSubcommand;
use crate::{optimize, save_load};

/// soldr#2748 option C: parse an env-bound switch with the one owned-flag
/// rule (soldr#2740) -- `1/true/yes/on` is on, anything else off -- instead
/// of clap's own boolish set, so `--flag` and its `SOLDR_*` variable agree.
pub(crate) fn owned_flag(value: &str) -> Result<bool, std::convert::Infallible> {
    Ok(crate::core::flag_value(value))
}

const ROOT_LONG_ABOUT: &str = "Instant tools. Instant builds.\n\n\
soldr wraps cargo and the rustup toolchain so every Rust command goes through a\n\
managed, content-addressable cache and a repo-pinned toolchain.";

const ROOT_AFTER_HELP: &str = "\
Examples:\n  \
soldr cargo build --release   # cached build, pinned toolchain\n  \
soldr rustfmt --check src     # format check via pinned rustfmt\n  \
soldr toolchain               # install rust-toolchain.toml\n  \
soldr cook --release          # prebuild deps as a cache layer\n  \
soldr status                  # cache health + active toolchain\n\n\
Run `soldr help <command>` for detailed help on a subcommand,\n\
including environment variables, exit codes, and on-disk layout.";

const ROOT_BEFORE_HELP: &str = "\
Common commands:\n  \
cargo                  Run cargo through soldr (cached, pinned toolchain)\n  \
cc                     Compile C with a catalogue-backed toolchain\n  \
c++                    Compile C++ with a catalogue-backed toolchain\n  \
rustc                  Compile Rust source via the pinned toolchain\n  \
lint                   Run Soldr's unified Rust and dependency lint suites\n  \
ci-test                Run the prescribed host-validation DAG\n  \
replay-nextest         Replay an archived test suite with a packaged runner\n  \
rustfmt                Format Rust source via the pinned toolchain\n  \
clippy-driver          Run the clippy linter via the pinned toolchain\n  \
rustup                 Drop-in passthrough to the system rustup binary\n\n\
Less common toolchain commands:\n  \
rustdoc                Generate Rust documentation\n  \
rust-analyzer          Run the rust-analyzer language server\n  \
rust-gdb               Debug with gdb + Rust pretty-printers\n  \
rust-lldb              Debug with lldb + Rust pretty-printers\n  \
toolchain              Install the channel declared in rust-toolchain.toml\n  \
bootstrap              Install rustup itself into soldr's managed bin dir\n  \
doctor                 Report drift between rust-toolchain.toml and rustup\n  \
shims                  Install per-version PATH shims for Rust tools\n\n\
soldr cache & build:\n  \
cook                   Prebuild dependencies via bundled cargo-chef\n  \
status                 Show cache status and active toolchain\n  \
cache                  Inspect compilation cache entries\n  \
config                 Show or set soldr configuration\n  \
clean                  Clear the embedded zccache build cache\n  \
purge                  Purge all soldr-managed cache artifacts\n  \
gc                     Review reclaimable cargo target/ directories\n  \
save                   Bundle a build cache + source mtimes into a .tar.zst\n  \
load                   Restore a soldr-save archive on a fresh checkout\n\n\
soldr ops & infrastructure:\n  \
daemon                 Manage the long-lived soldr-daemon process\n  \
session-start          Start a zccache session and print its id\n  \
session-end            End a zccache session and emit its stats\n  \
optimize               Apply platform-specific hot-cache tuning\n  \
defender-exclusions    Manage Windows Defender exclusions for soldr caches\n\n  \
help                   Print this message or the help of the given subcommand\n\n";

#[derive(clap::Parser)]
#[command(
    name = "soldr",
    version,
    about = "Instant tools. Instant builds.",
    long_about = ROOT_LONG_ABOUT,
    before_help = ROOT_BEFORE_HELP,
    after_help = ROOT_AFTER_HELP,
    after_long_help = ROOT_AFTER_HELP,
    next_line_help = true,
    help_template = "{about-with-newline}\n{usage-heading} {usage}\n\n{before-help}Options:\n{options}{after-help}",
    max_term_width = 80
)]
pub(crate) struct Cli {
    #[arg(
        long,
        hide = true,
        help = "Deprecated: use ZCCACHE_DISABLE=1 instead",
        long_help = "Deprecated (soldr#2364) in favor of `ZCCACHE_DISABLE=1`, the \
supported kill-switch for soldr's compilation cache. Hidden; retained for compatibility."
    )]
    pub(crate) no_cache: bool,
    /// Trust inherited soldr/zccache workspace environment for cargo runs
    #[arg(
        long,
        env = crate::TRUST_INHERITED_SOLDR_ENV_VAR,
        value_parser = owned_flag,
        num_args = 0,
        default_missing_value = "1",
        hide_env = true,
        help = "Trust inherited soldr/zccache workspace env for cargo runs",
        long_help = "Trust inherited soldr/zccache workspace environment for cargo runs.\n\n\
By default, `soldr cargo ...` resolves a fresh soldr workspace context from the current cwd/manifest while preserving normal OS, Cargo, Rust, proxy, cert, and CI environment. This flag is an advanced escape hatch for CI/action workflows that intentionally inject soldr/zccache workspace state. Equivalent to SOLDR_TRUST_INHERITED_ENV=1."
    )]
    pub(crate) trust_inherited_soldr_env: bool,
    #[arg(
        long,
        global = true,
        help = "Allow a build with no rust-toolchain.toml (default, soldr#3887)",
        long_help = "Proceed even when no rust-toolchain.toml exists at or above the working directory.

Without a pin soldr resolves rustc from PATH, which can select a mismatched-host toolchain and makes cache keys depend on ambient PATH state. Since soldr#3887 this is the default; the flag overrides SOLDR_ALLOW_UNPINNED=0, which turns the strict soldr#1766 pin guard on. Equivalent to SOLDR_ALLOW_UNPINNED=1."
    )]
    pub(crate) allow_unpinned: bool,
    /// soldr#1802 — force the elapsed-seconds line prefix on.
    ///
    /// A pair of plain boolean flags rather than one `--timestamp-lines=BOOL`.
    /// The default is *conditional* (on for non-TTY, off for a terminal), so
    /// "not passed" must stay distinguishable from "passed false" — but the
    /// `Option<bool>` spelling that expresses that (`num_args = 0..=1` +
    /// `default_missing_value`) sends clap into unbounded recursion when the
    /// arg is also `global = true`, overflowing the stack on *every*
    /// invocation including `--version`. Two flags express the same tri-state
    /// with no exotic clap features.
    #[arg(
        long,
        conflicts_with = "no_timestamp_lines",
        help = "Prefix relayed output lines with elapsed seconds (soldr#1802)",
        long_help = "Prefix every relayed output line with seconds elapsed since soldr started.

Equivalent to SOLDR_TIMESTAMP_LINES=1. Left unset, the prefix is on when stderr is not a terminal (CI, Docker, `2>file`) and off on an interactive terminal, where it would fight cargo's progress redraw.

Place it BEFORE `cargo`, as in `soldr --timestamp-lines cargo build`: everything after `cargo` is passed through to cargo untouched."
    )]
    pub(crate) timestamp_lines: bool,
    /// soldr#1802 — force the elapsed-seconds line prefix off.
    #[arg(
        long,
        help = "Suppress the elapsed-seconds line prefix (soldr#1802)",
        long_help = "Suppress the elapsed-seconds prefix on relayed output lines.

Equivalent to SOLDR_TIMESTAMP_LINES=0. Useful in CI, where the prefix is on by default, when a downstream tool parses cargo's exact bytes. Note the capture channel soldr's own diagnostic scanner reads is always raw, so this only affects what reaches your terminal.

Place it BEFORE `cargo`, as with --timestamp-lines."
    )]
    pub(crate) no_timestamp_lines: bool,
    /// soldr#2302 — suppress per-unit cache HIT/MISS lines + stats summary (SOLDR_NO_CACHE_STATES=1); place before `cargo`, as with --no-cache.
    #[arg(long, help = "Suppress cache HIT/MISS lines + the stats summary")]
    pub(crate) no_cache_states: bool,
    /// machine-readable front-door output: one JSON line on stdout (SOLDR_PORCELAIN=1); place before `cargo`, as with --no-cache.
    #[arg(
        long,
        help = "Emit one machine-readable JSON line instead of relaying cargo output",
        long_help = "Replace the relayed cargo/nextest output with exactly ONE line of JSON on stdout \
(agent-harness mode).\n\nThe child's complete stdout+stderr is preserved in a durable log under \
`<soldr root>/logs/porcelain/`, readable afterwards from the JSON's `log_path`; the JSON also \
carries `schema_version`, `command`, `exit_code`, `duration_ms`, `expires_at` and a `summary` \
digest of the run's failures.\n\nEquivalent to SOLDR_PORCELAIN=1, which is what an agent harness \
sets. Place it BEFORE `cargo`, as in `soldr --porcelain cargo build`: everything after `cargo` is \
passed through to cargo untouched. Without the flag or the variable, output streams exactly as it \
does today."
    )]
    pub(crate) porcelain: bool,
    /// soldr#2546 — opt-in build process-tree tracing.
    #[arg(
        long,
        help = "Trace build child processes to stderr + a JSONL timeline (soldr#2546)",
        long_help = "Trace the processes soldr spawns for this build: each spawn and exit is \
announced on stderr with elapsed time and PID, and a structured JSONL timeline is written \
beside the build logs.

Equivalent to SOLDR_DEBUG_TRACE=1. Place it BEFORE `cargo`, as in `soldr --debug cargo build`: \
everything after `cargo` is passed through untouched, so `cargo install --debug` keeps its \
own meaning. With the flag absent no tracing work is performed."
    )]
    pub(crate) debug: bool,
    #[arg(
        long,
        value_enum,
        default_value_t = ZccacheSourceArg::Managed,
        value_name = "SOURCE",
        hide_possible_values = true,
        help = "Embedded zccache compatibility selector",
        long_help = "Select the zccache integration backing the compilation cache.\n\n\
`managed` (default) uses the zccache service compiled into soldr-daemon.\n\
`system` is retained as a compatibility spelling and currently selects the\n\
same embedded service. To use an external cache wrapper, set\n\
`SOLDR_RUSTC_WRAPPER=/path/to/zccache`."
    )]
    pub(crate) zccache: ZccacheSourceArg,
    #[arg(
        long,
        global = true,
        value_name = "N",
        help = "Cap concurrent daemon compiles (soldr#1761)",
        long_help = "Cap the number of compiles the daemon runs concurrently.

Equivalent to SOLDR_JOBS=N, and takes the same top precedence: above `[jobs].max_parallel_compiles` in config.toml, and above the legacy ZCCACHE_MAX_PARALLEL_COMPILES. Left unset, the default leaves one logical CPU free, and on an SMT machine additionally caps at (physical cores + 2) so a build cannot saturate every hardware thread — 10 on an 8-core/16-thread host. A machine without SMT keeps all its slots, and if the CPU topology cannot be read the limit falls back to one less than the logical CPU count.

This governs the daemon, not cargo. `--jobs` is soldr's own flag at every position, so `soldr build --jobs 1` caps the daemon and leaves cargo's parallelism at its default (soldr#2786).

To set cargo's job count explicitly, any of these reach it: `soldr build -j N` (the short form is not soldr's), `soldr build -- --jobs N`, or `CARGO_BUILD_JOBS=N`. Soldr preserves that explicit value. An OOM is a scheduling/admission defect to diagnose from compiler and cgroup telemetry, not a signal for Soldr to silently lower Cargo's global parallelism.

Applies to a daemon this invocation starts. A daemon already running keeps the limit it started with, so run `soldr daemon stop` first to change it."
    )]
    pub(crate) jobs: Option<usize>,
    #[arg(
        long,
        global = true,
        value_enum,
        value_name = "MODE",
        hide_possible_values = true,
        help = "How cache hits are delivered (see --help for modes)",
        long_help = "How the embedded zccache delivers a cache hit to its output path (ZCCACHE_MODE, zccache#1683, zccache#1792).

`link` hardlinks eligible outputs. `copy` always writes an independent, writable copy. `reflink` writes an independent copy-on-write clone, falling back to a copy where the volume cannot clone. `reflink-or-link-or-copy` clones, else hardlinks, else copies -- the old meaning of `auto`, added in zccache 1.15.0. `auto` -- and leaving this unset entirely -- means soldr decides: zccache's own AUTO no longer hardlinks Rust outputs (zccache#1792), so soldr probes the pair of directories a cache hit actually moves between once and injects the single explicit answer (REFLINK, LINK, or COPY) instead, to keep the fast hardlink path where the pair supports it (soldr#3440). This also applies when your own ZCCACHE_MODE names AUTO; an explicit LINK, COPY, REFLINK, or REFLINK_OR_LINK_OR_COPY in your own ZCCACHE_MODE is passed through untouched.

Equivalent to SOLDR_ZCCACHE_MODE=MODE and takes the same top precedence: above `[zccache] mode` in config.toml, and above your own ZCCACHE_MODE. Soldr sets ZCCACHE_MODE on the cargo child; the embedded service reads it per compile, so it applies without restarting the daemon. An unknown value in any tier fails the build."
    )]
    pub(crate) zccache_mode: Option<ZccacheModeArg>,
    #[command(subcommand)]
    pub(crate) command: Commands,
}

impl Cli {
    /// Publish the global flags that have an environment-variable spelling.
    ///
    /// Both of these must reach the daemon, which resolves them in its own
    /// process, and `SOLDR_*` is what survives the spawn scrub (soldr#1931).
    /// Setting the variable rather than threading a boolean keeps a single
    /// resolution point for each: `crate::toolchain` for the pin, and
    /// `core::jobs` for the compile limit — the flag simply populates that
    /// resolver's top precedence tier rather than becoming a second one.
    ///
    /// Lives here rather than in `run_cli` so the flag and its effect stay in
    /// one file; the dispatch path just calls this once.
    pub(crate) fn export_global_env(&self) {
        // soldr#1766.
        if self.allow_unpinned {
            std::env::set_var(crate::toolchain::ALLOW_UNPINNED_ENV_VAR, "1");
        }
        // soldr#1761.
        if let Some(jobs) = self.jobs {
            std::env::set_var(soldr_core::core::jobs::SOLDR_JOBS_ENV_VAR, jobs.to_string());
        }
        // soldr#3407. Populate the top tier `core::materialization_mode`
        // resolves, like --jobs, rather than a second decision point.
        if let Some(mode) = self.zccache_mode {
            std::env::set_var(
                soldr_core::core::materialization_mode::SOLDR_ZCCACHE_MODE_ENV_VAR,
                mode.into_mode().as_str(),
            );
        }
        // soldr#1802. Publishing the variable rather than threading a bool
        // keeps `should_timestamp` the single decision point, so the flag
        // populates its top precedence tier instead of becoming a second
        // one that could disagree with it.
        // `conflicts_with` makes both-at-once a parse error, so at most one
        // arm runs and neither needs to defer to the other.
        if self.timestamp_lines || self.no_timestamp_lines {
            std::env::set_var(
                crate::cargo_front_door::timestamp_tee::TIMESTAMP_LINES_ENV_VAR,
                if self.timestamp_lines { "1" } else { "0" },
            );
        }
        // soldr#2302. Publish the env spelling `cache_states::enabled()` reads.
        if self.no_cache_states {
            std::env::set_var(
                crate::cargo_front_door::cache_states::NO_CACHE_STATES_ENV_VAR,
                "1",
            );
        }
        // Porcelain. Publish the env spelling `porcelain::enabled()` reads, so
        // nested soldr invocations (the zthreads/no-cache retries) inherit the
        // mode and emit the one JSON line themselves.
        if self.porcelain {
            std::env::set_var(crate::cargo_front_door::porcelain::PORCELAIN_ENV_VAR, "1");
        }
        // soldr#2546. Publish the env spelling `debug_trace::enabled()` reads,
        // so nested soldr invocations (shims, lint children) inherit tracing.
        if self.debug {
            std::env::set_var(
                crate::cargo_front_door::debug_trace::DEBUG_TRACE_ENV_VAR,
                "1",
            );
        }
    }
}

/// `--zccache-mode` values (soldr#3407, zccache#1792).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ZccacheModeArg {
    Auto,
    Link,
    Copy,
    Reflink,
    ReflinkOrLinkOrCopy,
}

impl ZccacheModeArg {
    pub(crate) fn into_mode(self) -> soldr_core::core::materialization_mode::ZccacheMode {
        use soldr_core::core::materialization_mode::ZccacheMode;
        match self {
            Self::Auto => ZccacheMode::Auto,
            Self::Link => ZccacheMode::Link,
            Self::Copy => ZccacheMode::Copy,
            Self::Reflink => ZccacheMode::Reflink,
            Self::ReflinkOrLinkOrCopy => ZccacheMode::ReflinkOrLinkOrCopy,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ZccacheSourceArg {
    /// Use the zccache service embedded in soldr-daemon (default).
    #[default]
    Managed,
    /// Compatibility spelling; currently equivalent to `managed`.
    System,
}

pub(crate) use crate::builtin_verbs::{
    is_cargo_builtin_verb, CARGO_BUILTIN_VERBS, SOLDR_BUILTIN_VERBS,
};

#[derive(clap::Subcommand)]
pub(crate) enum Commands {
    /// Build the workspace via soldr's blessed cross-compile path
    ///
    /// `soldr build --target X` is the **blessed-default surface** for
    /// builds. It forwards to the same `cargo_front_door` pipeline as
    /// `soldr cargo build`, but first prepares any supported target
    /// sysroot and compiler/linker environment. For example,
    /// `*-pc-windows-msvc` can use the managed xwin-cache with clang/lld
    /// directly, without routing the default path through `cargo xwin`.
    ///
    /// The surface contract is the load-bearing part: callers asking
    /// for `soldr build` get the soldr-blessed toolchain story, while
    /// callers asking for `soldr cargo build` get the explicit legacy
    /// passthrough.
    ///
    /// Internal sharing of dispatch with `Commands::Cargo` is
    /// intentional and expected; what matters is the user-facing
    /// contract that `soldr build` evolves into the blessed-default
    /// without breaking the alias surface.
    Build {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Compile C source with a catalogue-backed target compiler (soldr#2335)
    Cc(crate::cc_cmd::CcArgs),
    /// Compile C++ source with a catalogue-backed target compiler (soldr#2335)
    #[command(name = "c++")]
    Cxx(crate::cc_cmd::CcArgs),
    /// Link with a target linker in GNU ld syntax (ELF, Mach-O, win-x64-gnu) (soldr#3806)
    Ld(crate::ld_cmd::LdArgs),
    /// Link with a target linker in MSVC link.exe syntax (win-* MSVC) (soldr#3806)
    Link(crate::ld_cmd::LinkArgs),
    /// Build an abi3 Python wheel through soldr's blessed toolchain (soldr#2139)
    ///
    /// `soldr wheel [--release] [--host-glibc] [--target <triple>]` resolves
    /// the target (friendly aliases included, host by default), prepares the
    /// sysroot, and provisions maturin. `--release` is opt-in; the default is
    /// a quick dev wheel. A release `*-linux-gnu` wheel is always built
    /// against the catalogue glibc 2.17 sysroot and tagged `manylinux_2_17`,
    /// host target included (soldr#3432), and soldr prints one green
    /// `soldr: info:` line on stderr saying so. `--host-glibc` opts a
    /// host-target build out: it links this host's glibc, prints its own info
    /// line, and lets maturin derive the tag from the bytes (`pypi`). A
    /// release cross musl wheel is tagged `musllinux_1_2`; dev wheels and
    /// everything else are tagged from the bytes. A native aarch64 Linux host
    /// cannot run the catalogue bundle, so a host-target release gnu wheel is
    /// refused there unless `--host-glibc` is passed.
    /// Arguments after the flags are forwarded to `maturin build` verbatim.
    /// abi3 only in this first cut: a non-abi3 extension needs a CPython
    /// built for the target, which is refused rather than silently degraded.
    Wheel(crate::wheel_cmd::WheelArgs),
    /// Run cargo through soldr (cached, pinned toolchain)
    Cargo {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Prepare Dylint dependencies or run the Dylint extension
    Dylint {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Soldr's cache-aware validation suites
    ///
    /// `soldr lint` runs formatting validation, Clippy, and configured
    /// Dylint libraries with one canonical workspace scope. `lint deps`
    /// runs dependency/policy checks concurrently without starting the
    /// compiler cache. `lint ci` statically validates CI/build surfaces
    /// (workflows, composite actions, helper scripts) — it needs no Cargo
    /// manifest and starts no compiler cache; use `--format json` for a
    /// machine-readable report. `lint all` extends every suite (including
    /// `ci`) with udeps and semver-checks.
    ///
    /// soldr#3378: the `rust`/`all` suites also run Clippy once per target
    /// declared in `[workspace.metadata.soldr].targets` (package-metadata
    /// fallback) in addition to the host — a declared target is treated as a
    /// claim of support, so `#[cfg(windows)]`/`#[cfg(target_os = "macos")]`
    /// code gets type-checked on every `soldr lint`, not only in a platform CI
    /// lane. A workspace with no declared targets keeps host-only behavior.
    /// `--target <triple>` (repeatable, friendly aliases like `win-x64`
    /// accepted) overrides the declared list; `--host-only` skips cross
    /// targets entirely. `--cross-dylint` opts in to a custom-lint pass for
    /// each selected cross target using the lint libraries' exact nightly.
    /// These flags are valid only for `rust`/`all`.
    ///
    /// soldr#3958: `[workspace.metadata.soldr.lint]` declares the step list
    /// (`steps = ["fmt", "clippy", "dylint"]`, built-ins in order) and repo
    /// commands (`extra = [["ruff", "check", "."]]`) for the `rust`/`all`
    /// suites. With `SOLDR_PORCELAIN=1`, `lint rust` prints one JSON summary
    /// line on success; a failing step's output is printed in full.
    ///
    /// soldr#4006: `rust`/`all` first reformat in place (`cargo fmt -- -l`) and
    /// report the rewritten files (`autofixed` in porcelain), so formatting
    /// alone never fails a lint. `--check`, or `CI` set, only verifies.
    Lint {
        /// Suite selector (`rust`, `deps`, `ci`, or `all`). Rust/deps/all
        /// accept cargo scope flags; `ci` accepts only `--format json|human`.
        /// `rust`/`all` additionally accept `--target <triple>` (repeatable)
        /// `--host-only`, `--cross-dylint`, and `--check`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Inspect the verdict ledger (soldr#3894)
    ///
    /// `soldr cargo clippy|fmt --check|dylint|test --doc|deny check ...` skip
    /// themselves when a passing run with identical inputs is recorded.
    /// `key`/`explain` show the key for an invocation, `rekey` re-derives a
    /// prior job's keys, `ls`/`gc`/`paths`/`profiles` manage the store.
    /// `SOLDR_VERDICTS=off` disables skipping.
    Verdict {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Execute Soldr's frozen validation DAG
    ///
    /// `soldr ci-test` is the CI-oriented orchestration exception to Soldr's
    /// normal per-rustc boundary: it freezes the compiler domains and
    /// schedules fmt, policy checks, clippy, Dylint, nextest, doctests, and
    /// dependency policy stages. Use `--explain-plan --format json` to inspect
    /// its versioned plan without invoking compiler work.
    CiTest {
        /// `--explain-plan [--format human|json]`, `--target`, `--no-run`, or scope flags
        /// (`--package`, `--features`, `--all-features`,
        /// `--no-default-features`). A target without an executor requires
        /// `--no-run`; toolchain/profile/target-dir overrides are rejected.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run an arbitrary command with rustup's toolchain bin dir prepended to PATH (#1059)
    ///
    /// Workaround for Windows hosts where a Chocolatey-installed
    /// standalone Rust shadows rustup's `cargo` proxy. Cargo extensions
    /// like `cargo-dylint` / `cargo-binstall` invoke `"cargo"` directly
    /// (not through soldr), find the Chocolatey shim on PATH, and fail
    /// to honor per-crate `rust-toolchain.toml` overrides.
    ///
    /// `soldr exec <cmd> [args...]` resolves rustup's cargo via
    /// `rustup which cargo`, prepends its containing directory to PATH
    /// for the child process, and execs `<cmd>` unchanged. Any
    /// subprocess `<cmd>` spawns will then see rustup's proxy first.
    ///
    /// Example: `soldr exec cargo-dylint dylint --all`
    Exec {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Replay prebuilt nextest archives with the specified native runner.
    ///
    /// This path does not install a toolchain or fetch a runner. Only version
    /// probes and archive-backed list/run operations are accepted.
    ReplayNextest {
        #[arg(long)]
        runner: std::path::PathBuf,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Compile Rust source via the pinned toolchain
    Rustc {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Format Rust source via the pinned toolchain
    Rustfmt {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run the clippy linter via the pinned toolchain
    #[command(name = "clippy-driver")]
    ClippyDriver {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Drop-in passthrough to the system rustup binary
    ///
    /// When the first non-flag positional argument is `target` or
    /// `component` and `rust-toolchain.toml` declares a `channel`,
    /// soldr automatically inserts `--toolchain <channel>` after the
    /// subcommand (unless the user already passed `--toolchain`).
    /// Every other invocation is forwarded verbatim.
    Rustup {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Generate Rust documentation
    Rustdoc {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run the rust-analyzer language server
    #[command(name = "rust-analyzer")]
    RustAnalyzer {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Debug with gdb + Rust pretty-printers
    #[command(name = "rust-gdb")]
    RustGdb {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Debug with lldb + Rust pretty-printers
    #[command(name = "rust-lldb")]
    RustLldb {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Install the channel declared in rust-toolchain.toml
    Toolchain {
        #[command(subcommand)]
        subcommand: ToolchainSubcommand,
    },
    /// Install rustup itself into soldr's managed bin dir
    #[command(
        long_about = "Install `rustup` itself into the soldr-managed bin dir when the host has no system-managed toolchain manager. Idempotent: a re-run with rustup already present prints the resolved path and exits 0.\n\nFetches `rustup-init` from `https://static.rust-lang.org/rustup/dist/<host-triple>/` under the same `SOLDR_TRUST_MODE` / `SOLDR_CHECKSUMS_FILE` policy as every other soldr-fetched binary. Set `SOLDR_NO_BOOTSTRAP=1` to disable the implicit auto-install that runs from `soldr cargo` / `soldr rustup ...` when rustup is missing."
    )]
    Bootstrap {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Report drift between rust-toolchain.toml and rustup
    #[command(
        long_about = "Diagnose drift between `rust-toolchain.toml` and the currently installed rustup state. Read-only: never mutates rustup. Exit code is `1` when drift is detected, `0` otherwise."
    )]
    Doctor {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
        /// Force a fresh Defender real-time-scan probe of the soldr
        /// cache directory, ignoring the cached result. No-op outside
        /// Windows. Issue #357.
        #[arg(long)]
        refresh_defender_probe: bool,
        /// Delete an unmanaged extensionless `soldr` that is shadowing the
        /// installed `soldr.exe` on an MSYS PATH (issue #1979).
        ///
        /// Opt-in on purpose: a plain `soldr doctor` only reports the
        /// shadowing. Removing a binary from PATH is not something a
        /// diagnostic should do as a side effect, but no installer owns
        /// this file, so without an explicit switch an affected machine
        /// cannot be repaired by any soldr command.
        #[arg(long)]
        remove_shadowing_shim: bool,
    },
    /// Install per-version PATH shims for Rust tools
    ///
    /// Writes `cargo`, `rustc`, `rustfmt`, `clippy-driver`, and
    /// `rustdoc` shims under `~/.soldr/v<MANAGED_SHIM_VERSION>/shims/`
    /// and emits stable JSON describing where they live.
    Shims {
        /// Emit the stable machine-facing JSON form
        /// (`schema_version: 1`).
        #[arg(long)]
        json: bool,
    },

    /// Prebuild dependencies via bundled cargo-chef
    #[command(
        long_about = "Content-addressable dependency prebuild via the bundled `cargo-chef`. Splits a project build into a recipe phase (`cargo chef prepare`) and a stub-project compile phase (`cargo chef cook`) so the dep set can be cached as an output layer (Docker), a tarball (CI), or just a warm `target/` (local dev) that survives source-code commits.\n\nRoutes both phases through the cargo front door so zccache, `ZCCACHE_PATH_REMAP=auto`, and the soldr-managed toolchain homes all apply.\n\nRecognised flags (everything else: pass after `--`): `--release`, `--target <triple>`, `--workspace`, `--profile <name>`, `-p`/`--package <name>` (repeatable), `--recipe-path <path>`, `--keep-recipe`, `--prepare-only`, `--cook-only`, `--no-trim`, `--no-index` (skip the cross-repo cook-index pack, also `SOLDR_COOK_INDEX=0`; use on ephemeral CI runners where the index is never reused)."
    )]
    Cook {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show cache status and active toolchain
    Status {
        /// Emit the stable machine-facing JSON form for this command
        #[arg(long)]
        json: bool,
    },
    /// Inspect compilation cache entries
    Cache {
        /// Emit the stable machine-facing JSON form for this command
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: Option<CacheSubcommand>,
    },
    /// Inspect soldr's runtime logs (issue #820)
    ///
    /// `list` shows recent build launches, `show` expands one launch
    /// into cache stats / slow compiles / log paths, and `paths`
    /// prints every directory soldr writes runtime logs into.
    Logs {
        #[command(subcommand)]
        command: Option<LogsSubcommand>,
    },
    /// Show or set soldr configuration (`~/.soldr/config.toml`)
    ///
    /// `list` (default) prints every set key, `get <key>` / `set <key>
    /// <value>` read and write one dotted key, `path` prints the file.
    Config {
        #[command(subcommand)]
        command: Option<crate::config_cmd::ConfigSubcommand>,
    },
    /// Clear the embedded zccache build cache
    Clean,
    /// Purge all soldr-managed cache artifacts
    Purge,
    /// Review reclaimable cargo target/ directories
    ///
    /// Aliases: `purge-targets` (matches issue #234's `soldr --purge`
    /// wording). Uses the soldr registry (`~/.soldr/state.sqlite3`) to
    /// discover tracked Cargo `target/` directories.
    #[command(alias = "purge-targets")]
    Gc {
        /// Deprecated: `soldr gc` is already a non-destructive summary.
        #[arg(long, hide = true)]
        dry_run: bool,
        /// Deprecated: use `soldr gc purge --all`.
        #[arg(long, hide = true)]
        all: bool,
        /// Minimum age before a `target/` is included in the summary
        /// (e.g. `10d`, `4w`).
        #[arg(long, default_value = "10d", value_name = "DURATION")]
        older_than: String,
        /// Minimum on-disk size before a `target/` is included in the
        /// summary (e.g. `256M`, `1GB`).
        #[arg(long, default_value = "256M", value_name = "SIZE")]
        larger_than: String,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: Option<GcSubcommand>,
    },
    /// Bundle a build cache + source mtimes into a .tar.zst
    #[command(
        long_about = "Bundle a build-cache directory plus a content-verified snapshot of source-file mtimes into a single `.tar.zst` archive. The output is consumed by `soldr hydrate` (historical alias: `soldr load`) to restore both the cache and Cargo-friendly source mtimes on a fresh checkout."
    )]
    Save(save_load::SaveArgs),
    /// Hydrate a soldr-save archive on a fresh checkout
    #[command(
        visible_alias = "load",
        long_about = "Hydrate an archive produced by `soldr save`: rematerialize the cache in the destination directory and replay each source-file mtime, but only when the current file's size and BLAKE3 hash still match the snapshot so soldr cannot underbuild after a real source change. The historical `soldr load` spelling remains an alias."
    )]
    Hydrate(save_load::LoadArgs),

    /// Bundle or extract a soldr release .tar.zst archive
    #[command(
        long_about = "Bundle a previously-built `soldr` release binary together with `soldr-daemon` and bundled tools into a single `.tar.zst` archive, or extract one for validation.\n\nBy default, `soldr archive` resolves the built soldr binary and daemon from `target/<triple>/release/` and optional managed tools from the soldr cache. In release CI, pass `--stage-dir <DIR>` to compress an already-validated staging directory with soldr's in-process zstd encoder instead of shelling out to `tar | zstd`. For smoke tests, pass `--input <FILE> --extract-dir <DIR>` to extract through soldr's in-process zstd decoder.\n\nThe output is a flat `.tar.zst` (zstd level 19) — every entry sits at the archive root unless `--stage-dir` contains a nested debug-info sidecar directory such as a dSYM bundle."
    )]
    Archive {
        /// Target triple to bundle for. Defaults to the auto-detected
        /// host triple.
        #[arg(long, value_name = "TRIPLE")]
        target: Option<String>,
        /// Existing staging directory to archive as-is. Used by release
        /// CI after it has staged and validated every required binary.
        #[arg(long, value_name = "DIR")]
        stage_dir: Option<std::path::PathBuf>,
        /// Existing soldr release archive to extract.
        #[arg(long, value_name = "FILE")]
        input: Option<std::path::PathBuf>,
        /// Destination directory for `--input` extraction.
        #[arg(long, value_name = "DIR")]
        extract_dir: Option<std::path::PathBuf>,
        /// Destination archive path. Suggest the `.tar.zst` suffix.
        #[arg(long, value_name = "FILE")]
        output: Option<std::path::PathBuf>,
    },

    /// Prepare the cross-compile toolchain for a target triple
    #[cfg_attr(
        any(),
        command(
            name = "prepare",
            long_about = "Uniform cross-compile toolchain bootstrap. Same invocation shape for every target — only `--target` varies. Internally dispatches based on the triple:\n\n  *-pc-windows-msvc:  ensure LLVM toolchain + extract the vendored xwin MSVC CRT cache from soldr-toolchain assets, then export target-scoped Cargo/cc-rs/linker env for the blessed `soldr build` path and explicit cargo-xwin fallback consumers.\n  x86_64-pc-windows-gnu: on Windows x64, ensure managed MinGW-w64 GCC plus GNU syslibs and export target-scoped Cargo/cc-rs env; on other hosts, fail visibly instead of falling back to cargo-zigbuild.\n  *-apple-darwin:     ensure the target-shaped Apple SDK and print `SDKROOT=<path>` (and append to $GITHUB_ENV when --github-env is set). `soldr build --target` is the blessed Darwin cross-build path; prepare is for env export and legacy/external tooling.\n  *-unknown-linux-*:  ensure cargo-zigbuild + zig (when triple != host).\n  All targets:        `rustup target add <triple>`.\n\nCollapses the per-step ad-hoc downloads in `cross-compile-all-targets.yml` into a single 'Preparing Cross Compile Toolchain' step. Designed to be wrapped by `.github/actions/prepare-cross-toolchain/action.yml`."
        )
    )]
    #[command(
        long_about = "Prepare soldr's complete target lifecycle. The invocation shape is identical for every canonical target: select an alias or Rust triple with `--target`; soldr installs the Rust standard library, selects and materializes the blessed compiler/linker plus SDK or sysroot, and exports the target-scoped environment. The same preparation is consumed by build, clippy, test compilation, nextest archives, and PEP 517 operations. Legacy backend wrappers are diagnostic-only overrides and are never selected by this command."
    )]
    Prepare {
        /// Target alias or triple to prepare the toolchain for. Three shapes
        /// are accepted:
        ///
        ///   * a single triple, e.g. `x86_64-pc-windows-msvc`;
        ///   * a comma-separated list, e.g.
        ///     `x86_64-pc-windows-msvc,aarch64-apple-darwin` —
        ///     useful for docker-image bake steps where no workspace
        ///     `Cargo.toml` is mounted yet;
        ///   * the literal `all` — expands to every triple declared
        ///     under `[workspace.metadata.soldr].targets` (or
        ///     `[package.metadata.soldr].targets`) in the nearest
        ///     `Cargo.toml`.
        ///
        /// See zackees/soldr#914.
        #[arg(long, value_name = "TARGET[,TARGET...]|all")]
        target: String,
        /// Optional path to append `KEY=VALUE` env-var lines (e.g.
        /// `SDKROOT=<path>` for darwin lanes). When running under
        /// GitHub Actions, point at `$GITHUB_ENV`. Export is limited
        /// to one target because global PATH/flag keys cannot safely
        /// represent several target lifecycles in one job.
        #[arg(long, value_name = "FILE")]
        github_env: Option<std::path::PathBuf>,
        /// Capture the prepared state (zig + LLVM + Apple SDK + xwin
        /// cache) into a single `tar.zst` archive at this path after
        /// preparation completes. Designed for `actions/cache@v4`'s
        /// save step on GitHub Actions so subsequent runs can
        /// `--restore` instead of re-downloading.
        #[arg(long, value_name = "FILE")]
        save: Option<std::path::PathBuf>,
        /// Extract a previously-saved archive BEFORE running the
        /// normal prepare flow. Anything still missing after restore
        /// is downloaded normally — partial restores are non-fatal.
        #[arg(long, value_name = "FILE")]
        restore: Option<std::path::PathBuf>,
    },

    /// soldr#938 — print the cross-compile env block (shell-eval).
    #[command(
        name = "env",
        long_about = "Print the cross-compile env block soldr would set internally for the given target, in shell-eval form. Use to bridge env into shells/IDE integrations that bypass `soldr cargo`:\n\n  eval \"$(soldr env --target mac-arm64)\"\n  soldr env --target win-x64 --shell-export   # `export KEY=VALUE` for sh/bash/zsh\n  soldr env --target linux-x64-musl --json    # stable JSON for tooling\n\nResolves the target via the same alias table soldr build uses (`win-x64`, `mac-arm64`, etc.; or Rust triple). JSON output includes the shared target-aware PyO3 plan. PYO3_NO_PYTHON is emitted only for a workspace-metadata-proven ABI3 cross extension; target Python compatibility assets are opt-in and separate from OS SDK preparation. See soldr#1610 + soldr#1614."
    )]
    Env {
        /// Target triple OR soldr alias (e.g. `win-x64`, `mac-arm64`,
        /// `linux-x64-musl`, `apple-silicon`, `x86_64-pc-windows-msvc`).
        /// See `crate::target_alias` for the full alias table.
        #[arg(long, value_name = "TRIPLE-OR-ALIAS")]
        target: String,
        /// Emit `export KEY=VALUE` lines (shell-export form) suitable
        /// for `eval`. Default is bare `KEY=VALUE` lines which `set
        /// -a` users can also source.
        #[arg(long)]
        shell_export: bool,
        /// Emit the same env block in stable JSON form. Mutually
        /// exclusive with --shell-export.
        #[arg(long, conflicts_with = "shell_export")]
        json: bool,
        /// Resolution/introspection only: emit the JSON payload without
        /// materializing the toolchain (env is null). Requires --json.
        /// The default (soldr#2304) prepares the target exactly like
        /// `soldr prepare` and emits the complete blessed environment.
        #[arg(long, requires = "json")]
        plan_only: bool,
    },

    /// Cross-compile a soldr-bundled tool (crgx, cargo-chef) for a target triple
    #[command(
        name = "build-from-source",
        long_about = "Source-build a whitelisted soldr-managed tool (today: `crgx`, `cargo-chef`) for an arbitrary Rust target triple and deposit the resulting binary into `~/.soldr/bin/<tool>-from-source/<version>/<triple>/<tool>[.exe]` with a sha256 sidecar.\n\nMotivation (sub-issue #859 of meta #853): the release pipeline already source-builds these tools in some lanes because upstream does not always ship prebuilt binaries for every target — notably `aarch64-apple-darwin` for cargo-chef. This verb lifts that ad-hoc shell into a first-class soldr command:\n\n  soldr build-from-source crgx --target aarch64-apple-darwin\n  soldr build-from-source cargo-chef --target aarch64-apple-darwin\n\nResolution:\n  * `--target` defaults to the auto-detected host triple.\n  * `--version` defaults to the registry pin in `known_tools::lookup_by_crate(<tool>).pinned_version`.\n  * The build invokes `cargo install <tool>@<version> --target <triple> --root <staging> --force` via the directly-resolved cargo binary, clearing inherited `RUSTC_WRAPPER` / `RUSTC_WORKSPACE_WRAPPER`. This is the same direct-cargo pattern `soldr toolchain prepare` uses for plugin installs.\n\nOutputs:\n  * The installed binary lands at `~/.soldr/bin/<tool>-from-source/<version>/<triple>/<tool>[.exe]`.\n  * A sibling `<tool>.sha256` sidecar records the binary's sha256 in `<hash>  <name>` format compatible with `sha256sum -c`."
    )]
    BuildFromSource {
        /// Whitelisted tool name. Supported today: `crgx`, `cargo-chef`.
        #[arg(value_name = "TOOL")]
        tool: String,
        /// Target triple to build for. Defaults to the auto-detected
        /// host triple.
        #[arg(long, value_name = "TRIPLE")]
        target: Option<String>,
        /// Crate version to install (without the leading `v`). Defaults
        /// to the registry pin (`known_tools::lookup_by_crate(<tool>).
        /// pinned_version`).
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
    },

    /// Install a Rust tool from a GitHub URL or local path, prebuilt-first
    /// (soldr#2310). Args flattened from [`crate::install::InstallArgs`].
    Install(#[command(flatten)] crate::install::InstallArgs),

    /// Manage the long-lived soldr-daemon process
    Daemon {
        #[command(subcommand)]
        command: DaemonSubcommand,
    },
    /// Manage the v2 broker (soldr#2361 Phase 2; the front door spawns this
    /// unconditionally — soldr#2388)
    Broker {
        #[command(subcommand)]
        command: BrokerSubcommand,
    },
    /// Start a zccache session and print its id
    ///
    /// Idempotent: when `ZCCACHE_SESSION_ID` is already set in the
    /// environment (and `--id` is not), emits the existing session
    /// metadata without contacting the daemon. Otherwise boots the
    /// daemon if necessary and runs `zccache session-start`.
    #[command(name = "session-start")]
    SessionStart {
        /// Explicit session id. Without this flag soldr lets zccache
        /// assign one.
        #[arg(long, value_name = "UUID")]
        id: Option<String>,
        /// Override the session log path. Defaults to the soldr-managed
        /// `<cache>/zccache/logs/last-session.log`.
        #[arg(long, value_name = "PATH")]
        log: Option<std::path::PathBuf>,
        /// Override the per-session JSONL journal path. Defaults to the
        /// soldr-managed `<cache>/zccache/logs/last-session.jsonl`.
        #[arg(long, value_name = "PATH")]
        journal: Option<std::path::PathBuf>,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// End a zccache session and emit its stats
    ///
    /// Idempotent: a second call against an already-finalized session
    /// reports the prior stats (or notes that the session is gone)
    /// without erroring.
    #[command(name = "session-end")]
    SessionEnd {
        /// Session id to end. Defaults to `$ZCCACHE_SESSION_ID`.
        #[arg(long, value_name = "UUID")]
        id: Option<String>,
        /// After ending the session, drop its journal/log files from
        /// disk so the next session-start begins from a clean slate.
        #[arg(long)]
        clear: bool,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Apply platform-specific hot-cache tuning
    #[command(
        long_about = "Apply platform-specific hot-cache optimizations (Windows Defender exclusions today; future platforms TBD). Auto-skips on CI. See `docs/API.md` for the full matrix."
    )]
    Optimize(optimize::OptimizeArgs),
    /// Manage Windows Defender exclusions for soldr caches
    ///
    /// Self-documenting verbs (`check` / `add` / `remove`) over the same
    /// Defender machinery `soldr optimize` already exposes. Windows-only;
    /// no-op with a clear message on macOS / Linux.
    #[command(name = "defender-exclusions")]
    DefenderExclusions {
        #[command(subcommand)]
        subcommand: DefenderExclusionsSubcommand,
    },
    /// Show version
    #[command(hide = true)]
    Version {
        /// Emit the stable machine-facing JSON form for this command
        #[arg(long)]
        json: bool,
    },
    /// Anything else is a tool to fetch and run
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[path = "cli_args_subcommands.rs"]
mod subcommands;
pub use subcommands::TrimProfileArg;
pub(crate) use subcommands::{
    CacheSubcommand, DaemonBuildsSubcommand, DaemonSubcommand, DefenderExclusionsSubcommand,
    GcCargoArgs, GcListKind, GcSubcommand, GcSweepArgs, GcTargetArgs, LogsSubcommand,
    ToolchainSubcommand,
};

#[cfg(test)]
#[path = "cli_args_tests.rs"]
mod global_flag_tests;
