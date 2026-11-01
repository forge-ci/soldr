//! Subcommand enums behind the top-level `soldr <verb>` dispatch surface
//! (issue #2493 split from `cli_args.rs`).
//!
//! All of these types are pure clap declarations: they depend on nothing
//! else in `cli_args.rs`, only on `clap` attribute macros. The parent
//! module re-exports every name below, so the `crate::cli_args::<Type>`
//! paths used across the crate are unchanged.

#[derive(clap::Subcommand)]
pub(crate) enum DaemonSubcommand {
    /// Ask the singleton broker to start this Soldr root's daemon route.
    Start {
        #[arg(long)]
        foreground: bool,
        /// Incompatible legacy option; broker-owned daemons own their lifetime.
        #[arg(long, value_name = "SECS", default_value_t = 0)]
        idle_timeout: u64,
    },
    /// Ask the running daemon to shut down gracefully.
    Stop,
    /// Print the daemon's status (uptime, pid, request count).
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Write soldr-daemon's running-process service definition.
    #[command(name = "install-servicedef")]
    InstallServiceDef {
        /// Override the soldr-daemon binary path. Defaults to a sibling
        /// of the current soldr executable.
        #[arg(long, value_name = "PATH")]
        daemon_binary: Option<std::path::PathBuf>,
        /// Emit the installed path and deferred broker-adoption items as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Query recorded build sessions.
    Builds {
        #[command(subcommand)]
        command: DaemonBuildsSubcommand,
    },
}

#[derive(clap::Subcommand)]
pub(crate) enum DaemonBuildsSubcommand {
    /// List recent build sessions, newest first.
    List {
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long, value_name = "UNIX_MS")]
        since_ms: Option<i64>,
        #[arg(long)]
        json: bool,
    },
    /// List the slowest finished build sessions whose `total_wall_ms`
    /// meets the threshold (default 60s).
    Slow {
        #[arg(long, default_value_t = 60_000, value_name = "MS")]
        threshold_ms: u64,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Subcommand)]
pub(crate) enum DefenderExclusionsSubcommand {
    /// Report the soldr-owned paths that should be excluded, and which
    /// of them soldr believes it has already added (from the local
    /// managed-exclusions tracking file). Does not require admin.
    Check {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Add soldr's hot cache directories to Defender's exclusion list.
    /// Requires admin elevation; UAC self-relaunches on Windows when
    /// the parent shell is non-admin.
    Add {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
        /// Print what would change without invoking PowerShell.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove soldr-managed exclusions from Defender. Never touches
    /// user-added entries. Requires admin elevation.
    Remove {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
        /// Print what would change without invoking PowerShell.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(clap::Subcommand)]
pub(crate) enum ToolchainSubcommand {
    /// Install the channel declared in `rust-toolchain.toml`. No-op
    /// (exit 0 with a note) when the manifest is missing or omits
    /// `channel`.
    Install,
    /// Hardlink every file of the caller's and the managed rustup homes'
    /// toolchains into one content-addressed store, so the same toolchain
    /// stored twice takes the disk space of one (soldr#4027).
    Dedupe {
        /// Emit a JSON report.
        #[arg(long)]
        json: bool,
    },
    /// Install the channel and every declared component / target from
    /// `rust-toolchain.toml`. Stops at the first nonzero rustup exit.
    Prepare {
        /// Also provision what `soldr cc --target <TARGET>` needs (soldr#3806):
        /// the managed LLVM and the Apple SDK / xwin MSVC SDK / sysroot, and
        /// report them. Accepts the same spellings as `soldr cc --target`.
        #[arg(long, value_name = "TARGET")]
        target: Option<String>,
    },
    /// One-shot "make sure this host can build" verb (issue #407
    /// Phase 2): auto-bootstraps `rustup` if missing, runs the same
    /// install + component + target + plugin steps as `prepare`, then
    /// smoke-verifies the resolved toolchain by spawning
    /// `cargo --version` and `rustc --version`. Used by `setup-soldr`
    /// (`setup-soldr#133`) to delegate its TS toolchain logic to the
    /// soldr binary.
    Ensure {
        /// Emit the stable machine-facing JSON form
        /// (`schema_version: 1`) for consumption by setup-soldr and
        /// other tooling. See `docs/API.md` for the full schema.
        #[arg(long)]
        json: bool,
        /// Also provision what `soldr cc --target <TARGET>` needs and report
        /// it under the additive `build_tools.cross` key (soldr#3806).
        #[arg(long, value_name = "TARGET")]
        target: Option<String>,
    },
    /// Write PATH shim files into `--shim-dir` so a child process that
    /// resolves `cargo` / `rustfmt` / `clippy-driver` / `rustc` /
    /// `rustdoc` from PATH gets routed back through `soldr <tool>`
    /// (issue #407 Phase 3, ports setup-soldr's `ensure-shims.ts`).
    ///
    /// Idempotent by default: a shim file whose contents already match
    /// the expected body is left alone. `--force` overwrites regardless.
    /// `--json` emits the same `schema_version: 1` style payload
    /// `setup-soldr#133` consumes from `ensure`.
    Link {
        /// Destination directory for the shim files. Created if missing.
        #[arg(long, value_name = "PATH")]
        shim_dir: std::path::PathBuf,
        /// Emit the stable machine-facing JSON form
        /// (`schema_version: 1`) for consumption by setup-soldr and
        /// other tooling.
        #[arg(long)]
        json: bool,
        /// Overwrite existing shim files regardless of their current
        /// contents. Without `--force`, files whose contents differ are
        /// left untouched.
        #[arg(long)]
        force: bool,
    },
    /// Run env-detection probes (musl-cc availability, pre-populated
    /// `target/` warning, host triple summary) and emit either a
    /// human-readable summary or the stable `schema_version: 1` JSON
    /// payload consumed by `setup-soldr#133` (issue #407 Phase 4,
    /// ports the env-detection halves of setup-soldr's
    /// `detect-musl-cc.ts`, `detect-shared-target-warning.ts`, and
    /// `diagnostics.ts`).
    ///
    /// Namespaced under `toolchain` to avoid colliding with the
    /// top-level `soldr doctor` system check.
    Doctor {
        /// Emit the stable machine-facing JSON form
        /// (`schema_version: 1`) for consumption by setup-soldr and
        /// other tooling.
        #[arg(long)]
        json: bool,
    },
    /// soldr#988 Phase 2 — print the resolved soldr-toolchain
    /// catalogue origin and a one-line response summary (HTTP
    /// status, content-length, etag, last-modified). Honors
    /// `SOLDR_TOOLCHAIN_ORIGIN` (default
    /// `https://zackees.github.io/soldr-toolchain`).
    Catalogue {
        /// Emit the stable machine-facing JSON form
        /// (`schema_version: 1`) for tooling. Same shape conventions
        /// as the other `toolchain` JSON outputs.
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Subcommand)]
pub(crate) enum GcSubcommand {
    /// Delete eligible Cargo `target/` directories.
    Purge {
        /// Delete every eligible candidate without prompting.
        #[arg(long)]
        all: bool,
        /// Report what would be uninstalled and exit without deleting
        /// anything. Only supported with `--kind rustup_toolchain`
        /// (soldr#3507); every other kind rejects it rather than
        /// silently ignoring it.
        #[arg(long)]
        dry_run: bool,
        /// With `--kind rustup_toolchain`: only toolchains no pin has used
        /// within this many days are eligible (soldr#4027).
        #[arg(long, value_name = "DAYS")]
        unused_days: Option<u64>,
        /// Minimum age before a `target/` is considered stale
        /// (e.g. `10d`, `4w`).
        #[arg(long, default_value = "10d", value_name = "DURATION")]
        older_than: String,
        /// Minimum on-disk size before a `target/` is considered for
        /// reclamation (e.g. `256M`, `1GB`).
        #[arg(long, default_value = "256M", value_name = "SIZE")]
        larger_than: String,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
        /// Narrow the purge to a single taxonomy kind. Mutually exclusive
        /// with shorthand flags. The cargo-owned primary kinds
        /// (`cargo_registry_cache`, `cargo_git_db`,
        /// `cargo_installed_binaries`) are accepted for parse consistency
        /// but rejected before deletion; `rustup_toolchain` purges for
        /// real by delegating to `rustup toolchain uninstall`
        /// (soldr#3507).
        #[arg(
            long,
            value_enum,
            conflicts_with_all = [
                "registry_src",
                "git_checkouts",
                "target_incremental",
                "build_scripts",
                "doc",
                "subcommand_caches",
            ]
        )]
        kind: Option<GcListKind>,
        /// Shorthand for `--kind cargo_registry_src`. Walks
        /// `$CARGO_HOME/registry/src/<reg>/<crate>-<vers>/` and deletes
        /// the listed directories (#323 slice 2).
        #[arg(
            long,
            conflicts_with_all = [
                "kind",
                "git_checkouts",
                "target_incremental",
                "build_scripts",
                "doc",
                "subcommand_caches",
            ]
        )]
        registry_src: bool,
        /// Shorthand for `--kind cargo_git_checkouts`. Walks
        /// `$CARGO_HOME/git/checkouts/<repo>/<commit>/` and deletes
        /// the listed directories (#323 slice 3).
        #[arg(
            long,
            conflicts_with_all = [
                "kind",
                "registry_src",
                "target_incremental",
                "build_scripts",
                "doc",
                "subcommand_caches",
            ]
        )]
        git_checkouts: bool,
        /// Shorthand for `--kind cargo_target_incremental`.
        #[arg(
            long,
            conflicts_with_all = [
                "kind",
                "registry_src",
                "git_checkouts",
                "build_scripts",
                "doc",
                "subcommand_caches",
            ]
        )]
        target_incremental: bool,
        /// Shorthand for `--kind cargo_target_build_script_binaries`.
        #[arg(
            long,
            conflicts_with_all = [
                "kind",
                "registry_src",
                "git_checkouts",
                "target_incremental",
                "doc",
                "subcommand_caches",
            ]
        )]
        build_scripts: bool,
        /// Shorthand for `--kind cargo_target_doc`.
        #[arg(
            long,
            conflicts_with_all = [
                "kind",
                "registry_src",
                "git_checkouts",
                "target_incremental",
                "build_scripts",
                "subcommand_caches",
            ]
        )]
        doc: bool,
        /// Shorthand for `--kind cargo_target_subcommand_caches`.
        #[arg(
            long,
            conflicts_with_all = [
                "kind",
                "registry_src",
                "git_checkouts",
                "target_incremental",
                "build_scripts",
                "doc",
            ]
        )]
        subcommand_caches: bool,
    },
    /// List every `target/` directory currently tracked in the soldr
    /// registry, without applying any age or size thresholds.
    List {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
        /// Narrow the listing to a single taxonomy kind.
        #[arg(long, value_enum)]
        kind: Option<GcListKind>,
    },
    /// Run cargo's native `clean gc` against `$CARGO_HOME`. Requires
    /// a nightly toolchain because the command lives behind the
    /// unstable `-Zgc` flag.
    Cargo(Box<GcCargoArgs>),
    /// Read-only enumeration of every cache directory soldr knows
    /// about. Does not delete anything.
    Locations {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Orchestrator that runs `gc locations`, then conservative cargo
    /// `clean gc`, then the soldr target purge — and, with
    /// `--aggressive`, a second cargo GC pass with tighter ages.
    Sweep(Box<GcSweepArgs>),
    /// Walk a configurable root (default `~/dev`, override via `--root`
    /// or `SOLDR_GC_TARGET_ROOT`) for every workspace with a sibling
    /// `target/` directory, then either report (default) or purge them
    /// (issue #574). Designed for cross-repo `target/` reclamation —
    /// independent of the per-repo `target/` taxonomy walks above.
    Target(Box<GcTargetArgs>),
    /// Collapse byte-identical immutable artifacts (`deps/*.rlib|rmeta`,
    /// build-script `out/` files) across worktree `target/` dirs into one
    /// content-addressed store under `~/.soldr/target-store`, then report
    /// per-target shared vs. unique bytes (soldr#4033). With no paths, uses
    /// every registered or discovered target under the GC allowlist roots.
    Dedupe {
        /// `target/` directories to dedupe (default: all known targets).
        paths: Vec<std::path::PathBuf>,
        /// Report what would be shared without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Run one full maintenance pass against an explicitly supplied orphaned
    /// soldr root. Refuses live daemons, relative paths, and directory links;
    /// never discovers sibling product roots.
    Maintain {
        #[arg(long, value_name = "ABSOLUTE_PATH")]
        root: std::path::PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Issue #1286 (F5): run the auto-GC sweep synchronously in this
    /// process. Internal — the cargo front door spawns this detached at
    /// build end so the sweep survives the wrapper process exiting; the
    /// spawner owns the throttle, so this verb runs unconditionally.
    #[command(name = "auto-sweep", hide = true)]
    AutoSweep,
    /// Internal PEP517 delegate helper. Holds the root build lease until its
    /// stdin pipe closes; an abruptly terminated Python parent therefore
    /// releases the OS lock automatically.
    #[command(name = "hold-build-lease", hide = true)]
    HoldBuildLease,
}

#[derive(clap::Args)]
pub(crate) struct GcTargetArgs {
    /// Filesystem root to walk. Defaults to the value of
    /// `$SOLDR_GC_TARGET_ROOT`, falling back to `~/dev`.
    #[arg(long, value_name = "PATH")]
    pub(crate) root: Option<std::path::PathBuf>,
    /// Maximum walk depth. Default raised from 4 to 8 in #680 — clud's
    /// `/clud-pr` worktree layout puts targets at
    /// `~/dev/<repo>/.claude/worktrees/<branch>/target` (depth 6), and
    /// workspace-member projects routinely sit one or two levels deeper
    /// than that. `jwalk` scan time is roughly linear in depth, and a
    /// shallow `~/dev` adds <1s at depth 8.
    #[arg(long, default_value_t = 8, value_name = "N")]
    pub(crate) max_depth: usize,
    /// Report-only (the default).
    #[arg(long, conflicts_with = "purge")]
    pub(crate) dry_run: bool,
    /// Delete every reported `target/` directory after confirming a
    /// single y/n prompt (skipped when `--yes` is also passed).
    #[arg(long, conflicts_with = "dry_run")]
    pub(crate) purge: bool,
    /// Skip the interactive y/n prompt before purging. Without
    /// `--purge` this is a no-op.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Emit the stable machine-facing JSON form for this command.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Taxonomy kinds accepted by `gc list --kind` / `gc purge --kind`
/// (#323 slice 2). Unknown values are rejected at clap-parse time.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum GcListKind {
    /// Workspace `target/` dirs tracked by the soldr registry.
    #[value(name = "cargo_target")]
    CargoTarget,
    /// `target/<profile>/incremental/` directories under tracked targets.
    #[value(name = "cargo_target_incremental")]
    CargoTargetIncremental,
    /// `target/<profile>/build/*/build-script-build*` binaries.
    #[value(name = "cargo_target_build_script_binaries")]
    CargoTargetBuildScriptBinaries,
    /// `target/doc/` directories under tracked targets.
    #[value(name = "cargo_target_doc")]
    CargoTargetDoc,
    /// Tool-owned cache directories under tracked targets.
    #[value(name = "cargo_target_subcommand_caches")]
    CargoTargetSubcommandCaches,
    /// `$CARGO_HOME/registry/src/<reg>/<crate>-<vers>/` extracted
    /// crate sources.
    #[value(name = "cargo_registry_src")]
    CargoRegistrySrc,
    /// `$CARGO_HOME/registry/cache/<reg>/*.crate` package archives.
    #[value(name = "cargo_registry_cache")]
    CargoRegistryCache,
    /// `$CARGO_HOME/git/checkouts/<repo>/<commit>/` git-source crate
    /// checkouts (#323 slice 3).
    #[value(name = "cargo_git_checkouts")]
    CargoGitCheckouts,
    /// `$CARGO_HOME/git/db/<repo>/` primary bare git clones.
    #[value(name = "cargo_git_db")]
    CargoGitDb,
    /// `$CARGO_HOME/bin/<bin>` binaries installed by cargo.
    #[value(name = "cargo_installed_binaries")]
    CargoInstalledBinaries,
    /// `$RUSTUP_HOME/toolchains/<channel>` installed Rust toolchains.
    #[value(name = "rustup_toolchain")]
    RustupToolchain,
}

#[derive(clap::Args)]
pub(crate) struct GcCargoArgs {
    /// Report the plan and exit without invoking cargo.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Override the nightly toolchain. Defaults to
    /// `$SOLDR_GC_CARGO_TOOLCHAIN` if set, else `nightly`.
    #[arg(long, value_name = "TOOLCHAIN")]
    pub(crate) toolchain: Option<String>,
    /// Forwarded directly to cargo `--max-src-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_src_age: Option<String>,
    /// Forwarded directly to cargo `--max-crate-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_crate_age: Option<String>,
    /// Forwarded directly to cargo `--max-index-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_index_age: Option<String>,
    /// Forwarded directly to cargo `--max-git-co-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_git_co_age: Option<String>,
    /// Forwarded directly to cargo `--max-git-db-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_git_db_age: Option<String>,
    /// Forwarded directly to cargo `--max-download-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_download_age: Option<String>,
    /// Forwarded directly to cargo `--max-src-size`.
    #[arg(long, value_name = "SIZE")]
    pub(crate) max_src_size: Option<String>,
    /// Forwarded directly to cargo `--max-crate-size`.
    #[arg(long, value_name = "SIZE")]
    pub(crate) max_crate_size: Option<String>,
    /// Forwarded directly to cargo `--max-git-size`.
    #[arg(long, value_name = "SIZE")]
    pub(crate) max_git_size: Option<String>,
    /// Forwarded directly to cargo `--max-download-size`.
    #[arg(long, value_name = "SIZE")]
    pub(crate) max_download_size: Option<String>,
    /// Emit the stable machine-facing JSON form for this command.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(clap::Args)]
pub(crate) struct GcSweepArgs {
    /// Delete every eligible target/ candidate without prompting (used
    /// when the orchestrator runs the soldr target purge stage).
    #[arg(long)]
    pub(crate) all: bool,
    /// Plan and report without deleting anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Run cargo's `clean gc`. Default is on; pass `--no-cargo-gc` to
    /// skip cargo entirely. `--cargo-gc` is accepted but is the
    /// default.
    #[arg(long, conflicts_with = "no_cargo_gc")]
    pub(crate) cargo_gc: bool,
    /// Skip cargo's `clean gc` (e.g. on CI runners with no nightly).
    #[arg(long, conflicts_with = "cargo_gc")]
    pub(crate) no_cargo_gc: bool,
    /// After the standard pipeline, run cargo's `clean gc` again with
    /// tighter ages
    /// (`--max-src-age=7days --max-crate-age=14days --max-git-co-age=7days`).
    /// Floor: each value is clamped to `auto_gc.min_age_secs` before
    /// being forwarded.
    #[arg(long)]
    pub(crate) aggressive: bool,
    /// Emit the stable machine-facing JSON form for this command.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(clap::Subcommand)]
pub(crate) enum CacheSubcommand {
    /// Roll up the most recent compile-cache session into an
    /// AI-readable diagnosis document. Reads
    /// the latest per-session stats/history written by Soldr and, when an
    /// analyzer surface is available, produces per-tool/per-extension
    /// breakdowns over the per-session journal.
    Report {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Gracefully end the active session and stop soldr-daemon.
    ///
    /// Synchronous by default: checkpoints the embedded zccache service,
    /// requests Soldr-daemon shutdown, and does not return until the
    /// exact daemon generation that acknowledged the request exits. The
    /// caller can safely snapshot the cache directory after this succeeds.
    Shutdown {
        /// If set, copy the session log/journal/stats files into
        /// `<dir>/<session-id>/` after Soldr-daemon is quiescent. The
        /// directory (and any missing parents) is created on demand.
        #[arg(long, value_name = "DIR")]
        archive_logs: Option<std::path::PathBuf>,
        /// Skip the explicit pre-shutdown embedded-cache checkpoint
        /// (legacy flag name; debugging only). Graceful daemon shutdown
        /// still waits for its own cache flush to complete.
        #[arg(long)]
        no_depgraph_save: bool,
        /// Maximum time to wait for the acknowledged daemon generation to
        /// finish its graceful cache flush. Timing out never force-kills it.
        #[arg(long, value_name = "SECONDS", default_value_t = 300)]
        shutdown_timeout_seconds: u64,
        /// Skip the post-request poll that confirms the acknowledged daemon
        /// generation has actually exited. By default `shutdown` blocks
        /// until that generation exits (or the
        /// `--shutdown-timeout-seconds` deadline elapses); pass
        /// `--no-wait` only when you genuinely do not care
        /// (interactive shells). See soldr#383.
        #[arg(long)]
        no_wait: bool,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Show the embedded zccache store's on-disk size against its
    /// configured size cap (soldr#4028).
    ///
    /// The cap comes from `ZCCACHE_CACHE_SIZE_BYTES` /
    /// `ZCCACHE_CACHE_SIZE_PERCENT`, else `config.toml`
    /// `[zccache].max_size_gb`, else the 25 GiB default.
    Stats {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Enforce the embedded zccache size cap now, evicting least-recently
    /// used artifacts (soldr#4028).
    ///
    /// Runs a full maintenance pass when no daemon owns the root. A live
    /// daemon already enforces the cap every few minutes, so this reports
    /// that instead of racing it; stop it with `soldr cache shutdown` first
    /// to force an immediate pass.
    Gc {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Synchronously checkpoint the embedded zccache state without
    /// stopping Soldr-daemon.
    ///
    /// Returns 0 only when pending writes and the index writer drain and
    /// every persistence step reports completion. Pair with
    /// `cache shutdown` before archiving a live cache. See soldr#383.
    Flush {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Prune stale per-prefix build artifacts from a cargo `target/`
    /// directory, keeping only the newest entry per
    /// `(parent_dir, prefix)` bucket inside
    /// `target/<profile>/{deps, .fingerprint, incremental, build}/`.
    ///
    /// Defaults to a dry run for safety. Pass `--force` (or
    /// `--no-dry-run`) to actually delete entries.
    #[command(name = "prune-target")]
    PruneTarget {
        /// Path to the cargo `target/` directory to prune.
        path: std::path::PathBuf,
        /// Explicit dry-run mode (this is the default). Accepted for
        /// scriptability; mutually compatible with the default.
        #[arg(long, conflicts_with_all = ["force", "no_dry_run"])]
        dry_run: bool,
        /// Negate the dry-run default and actually delete entries.
        /// Equivalent to `--force`.
        #[arg(long = "no-dry-run", conflicts_with = "dry_run")]
        no_dry_run: bool,
        /// Actually delete entries. Equivalent to `--no-dry-run`.
        #[arg(long, conflicts_with = "dry_run")]
        force: bool,
        /// Switch from the legacy per-`(parent_dir, prefix)` orphan
        /// prune (issue #336) to the aggressive per-`prefix` strategy
        /// (issue #316): keep only the **newest hash family** per
        /// logical artifact name, deleting every other hash's files
        /// across `deps/`, `.fingerprint/`, `incremental/`, and
        /// `build/`. Recency is ranked by cargo's authoritative
        /// `.fingerprint/<prefix>-<hash>/invoked.timestamp` mtime when
        /// available, falling back to the entry's own filesystem
        /// mtime.
        #[arg(long = "keep-latest")]
        keep_latest: bool,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Trim cargo-recreatable noise from a `target/` directory before
    /// it is archived for CI cache transport. Composes:
    ///
    ///   * orphan hash-sibling pruning (same as `cache prune-target`),
    ///   * strip of build-script logspam, recreatable binaries, and
    ///     debug sidecars (CI profile only),
    ///   * removal of `target/<profile>/incremental/` (CI profile only).
    ///
    /// The CI profile is intended for `setup-soldr` to call before its
    /// `actions/cache@v4` save step so the rehydrate ships dramatically
    /// fewer bytes. Local profile only runs the hash-sibling prune.
    ///
    /// Dry-run by default. Pass `--force` to actually delete entries.
    /// Refuses to run when a `.cargo-lock` is present (active build).
    #[command(name = "trim-target")]
    TrimTarget {
        /// Path to the cargo `target/` directory to trim.
        path: std::path::PathBuf,
        /// Trim profile selector. `local` (default): only orphan
        /// hash-sibling prune. `ci`: also strip recreatable noise +
        /// remove incremental/.
        #[arg(long, value_enum, default_value_t = TrimProfileArg::Local)]
        profile: TrimProfileArg,
        /// Explicit dry-run mode (this is the default).
        #[arg(long, conflicts_with_all = ["force", "no_dry_run"])]
        dry_run: bool,
        /// Negate the dry-run default and actually delete entries.
        #[arg(long = "no-dry-run", conflicts_with = "dry_run")]
        no_dry_run: bool,
        /// Actually delete entries. Equivalent to `--no-dry-run`.
        #[arg(long, conflicts_with = "dry_run")]
        force: bool,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Remove a worktree-like directory robustly across Windows file-
    /// lock races with long-lived caching daemons (zccache,
    /// rust-analyzer). See soldr#710 for design.
    ///
    /// Tier 1: try inline `remove_dir_all`. POSIX always wins here
    /// (delete-on-close semantics); Windows wins in the no-handle case.
    /// Tier 2 (Windows fallback): on EACCES/EBUSY, atomically rename
    /// to a per-volume trash dir (`~/.soldr/trash-<volume>/<id>/`) and
    /// return immediately. Run `soldr cache sweep-trash` later (or
    /// from a periodic hook) to reclaim the bytes once the daemon
    /// idles.
    ///
    /// Intended consumers: the `clud-pr` skill's worktree teardown
    /// step; CI scripts that tear down per-PR build dirs.
    #[command(name = "release-worktree")]
    ReleaseWorktree {
        /// Path to remove (typically a `.claude/worktrees/<branch>/`
        /// directory).
        path: std::path::PathBuf,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Recursively delete every entry under `~/.soldr/trash-*/` that
    /// can currently be deleted. Per-entry failures are tolerated
    /// (daemon may still hold handles); re-run after the daemon idles
    /// to reclaim the rest. Pair with `release-worktree`.
    #[command(name = "sweep-trash")]
    SweepTrash {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
}

/// Trim profile presets for `cache trim-target`. Local keeps everything
/// a developer might want to inspect (incremental/, examples/, large
/// build-script stderr); CI strips it all in service of a smaller
/// archive.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum TrimProfileArg {
    /// Lightweight: only prune orphan hash siblings.
    Local,
    /// Aggressive: prune + strip recreatable noise + drop
    /// `incremental/`.
    Ci,
}

/// `soldr logs` subcommand surface — issue #820.
///
/// `list` / `show` / `paths` are implemented; `view` / `prune` stay
/// follow-up work tracked by the issue.
#[derive(clap::Subcommand)]
pub(crate) enum LogsSubcommand {
    /// List recent cargo launches recorded by the soldr daemon DB.
    List {
        /// Maximum number of launches to return. Defaults to the most
        /// recent 10.
        #[arg(long, default_value_t = 10)]
        limit: u32,
        /// Emit the stable machine-facing JSON form for this command
        /// (`schema_version: 1`). Stable enough for consumers; field
        /// additions are additive.
        #[arg(long)]
        json: bool,
    },
    /// Show one launch summary, cache hit/miss counts, slow compiles,
    /// miss reasons, and log paths.
    Show {
        /// Launch id from `soldr logs list`. Exact decimal ids are
        /// accepted, as are unique decimal or lower-hex prefixes.
        launch_id: String,
        /// Emit the stable machine-facing JSON form for this command
        /// (`schema_version: 1`). Stable enough for consumers; field
        /// additions are additive.
        #[arg(long)]
        json: bool,
    },
    /// Print every directory soldr writes logs into, annotated with
    /// what each directory contains. Self-documenting escape hatch
    /// so an agent or human triaging a slow build can locate the
    /// right journal without grepping source. JSON form for tooling.
    Paths {
        /// Emit the stable machine-facing JSON form for this command
        /// (`schema_version: 1`). Stable enough for consumers; field
        /// additions are additive.
        #[arg(long)]
        json: bool,
    },
    /// Stream a launch's archived JSONL journal(s) to stdout.
    View {
        /// Launch id from `soldr logs list` (exact or unique decimal prefix).
        launch_id: String,
    },
    /// Bounded retention: keep the newest N archived launches under the
    /// build-history directory reported by `soldr logs paths`.
    Prune {
        /// Number of most recent launches to keep.
        #[arg(long)]
        keep: usize,
        /// Report what would be removed without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Emit the stable machine-facing JSON form (`schema_version: 1`).
        #[arg(long)]
        json: bool,
    },
}
