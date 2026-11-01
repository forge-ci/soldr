/// `soldr wheel`: re-enter through `soldr maturin build ...`.
async fn run_wheel_command(
    args: &crate::wheel_cmd::WheelArgs,
    flags: DispatchFlags,
    inherited_provenance: &crate::replay_nextest::InheritedProvenance,
) -> Result<(), SoldrError> {
    // soldr#2139 gap 1. Re-enter through `soldr maturin build ...` so
    // provisioning, toolchain pinning, the build lease, target prep,
    // and the PyO3 plan stay in exactly one place. See `wheel_cmd`.
    let argv = crate::wheel_cmd::maturin_invocation(
        args,
        !flags.cache_enabled,
        flags.trust_inherited_soldr_env,
    )?;
    guarded_exit(Box::pin(run_with_args("soldr", &argv, inherited_provenance)).await?);
}

/// `soldr cargo ...`: the cargo front door.
async fn run_cargo_command(args: Vec<String>, flags: DispatchFlags) -> Result<(), SoldrError> {
    let args = crate::target_lifecycle::prepare_cargo_invocation(args).await?;
    // soldr#1079: same MSVC host env injection that
    // `Commands::Build` does, so `soldr cargo build` /
    // `soldr cargo test` on a native Windows MSVC target also
    // succeed from a plain PowerShell without `$env:LIB`.
    ensure_msvc_host_env_for_native(&args).await;
    guarded_exit(
        cargo_front_door::run_cargo_front_door(
            &args,
            flags.cache_enabled,
            flags.trust_inherited_soldr_env,
        )
        .await?,
    );
}

fn run_shims_command(json: bool) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    guarded_exit(install_shims::run_shims(&paths, json)?);
}

async fn run_toolchain_subcommand(subcommand: ToolchainSubcommand) -> Result<(), SoldrError> {
    match subcommand {
        ToolchainSubcommand::Install => exit_with(toolchain::run_toolchain_install()),
        ToolchainSubcommand::Dedupe { json } => {
            exit_with(crate::toolchain_store::run_toolchain_dedupe(json))
        }
        ToolchainSubcommand::Prepare { target } => exit_with(
            crate::toolchain_prepare::run_toolchain_prepare_for_target(target.as_deref()),
        ),
        ToolchainSubcommand::Ensure { json, target } => {
            exit_with(toolchain_ensure::run_toolchain_ensure(json, target).await)
        }
        ToolchainSubcommand::Link {
            shim_dir,
            json,
            force,
        } => exit_with(toolchain_link::run_toolchain_link(
            toolchain_link::LinkArgs {
                shim_dir,
                json,
                force,
            },
        )),
        ToolchainSubcommand::Doctor { json } => {
            exit_with(toolchain_doctor::run_toolchain_doctor(json))
        }
        ToolchainSubcommand::Catalogue { json } => {
            exit_with(crate::fetch::manifest_lookup::run_toolchain_catalogue(json).await)
        }
    }
}

/// `soldr prepare --target ...`: every requested target, concurrently.
async fn run_prepare_command(
    target: &str,
    github_env: Option<std::path::PathBuf>,
    save: Option<std::path::PathBuf>,
    restore: Option<std::path::PathBuf>,
) -> Result<(), SoldrError> {
    // `--target` accepts three shapes — see
    // `prepare_cmd::parse_target_arg` for the parser:
    //   - `all`         → every triple under
    //                     `[workspace.metadata.soldr].targets`
    //                     (needs a workspace context — #914).
    //   - `<a>,<b>,<c>` → an explicit comma-separated list.
    //                     Useful for docker-image bake steps
    //                     where no Cargo.toml is mounted yet.
    //   - `<triple>`    → a single triple (legacy default).
    let targets = crate::target_lifecycle::resolve_prepare_targets(target, github_env.is_some())?;
    // soldr#940 — run per-target preparations concurrently with
    // a bounded worker pool. `--target all` previously serialized
    // 8 cold downloads on top of each other; now they overlap.
    // Per-target dispatch (zig + Apple SDK, LLVM + xwin, …) is
    // also internally parallelized — see `prepare_cmd::run`.
    //
    // Concurrency cap: min(num_cpus, num_targets, 4). 4 is the
    // GitHub-runner-friendly ceiling — beyond that the
    // contention on the NIC dominates the parallelism win.
    let cpu_cap = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let concurrency = cpu_cap.min(targets.len()).clamp(1, 4);
    if targets.len() > 1 {
        eprintln!(
            "soldr prepare: parallelizing {} targets with {} workers (soldr#940)",
            targets.len(),
            concurrency,
        );
    }
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut handles = Vec::with_capacity(targets.len());
    for triple in &targets {
        let triple_owned = triple.clone();
        let github_env_clone = github_env.clone();
        let save_clone = save.clone();
        let restore_clone = restore.clone();
        let sem_clone = std::sync::Arc::clone(&sem);
        handles.push(tokio::spawn(async move {
            let _permit = sem_clone
                .acquire_owned()
                .await
                .expect("semaphore not closed");
            eprintln!("soldr prepare: ===== target {triple_owned} =====");
            let result = prepare_cmd::run(
                triple_owned.clone(),
                github_env_clone,
                save_clone,
                restore_clone,
            )
            .await;
            (triple_owned, result)
        }));
    }
    let mut failures: Vec<(String, String)> = Vec::new();
    for handle in handles {
        let (triple, result) = handle
            .await
            .map_err(|e| SoldrError::Other(format!("prepare worker join: {e}")))?;
        if let Err(e) = result {
            eprintln!("soldr prepare: target {triple} failed: {e}");
            failures.push((triple, e.to_string()));
        }
    }
    if !failures.is_empty() {
        let summary = failures
            .iter()
            .map(|(t, e)| format!("  {t}: {e}"))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(SoldrError::Other(format!(
            "soldr prepare: {} of {} target(s) failed:\n{summary}",
            failures.len(),
            targets.len()
        )));
    }
    Ok(())
}

fn run_status_command(json: bool, cache_enabled: bool) -> Result<(), SoldrError> {
    let output = cache::collect_status_output(cache_enabled)?;
    if json {
        cache::print_json(&output)?;
    } else {
        cache::print_status_output(&output);
    }
    Ok(())
}

fn run_version_command(json: bool) -> Result<(), SoldrError> {
    let output = cache::version_output();
    if json {
        cache::print_json(&output)?;
    } else {
        println!("soldr {}", output.soldr_version);
    }
    Ok(())
}

fn run_logs_command(command: Option<LogsSubcommand>) -> Result<(), SoldrError> {
    match command {
        // soldr#820 / soldr#3698: list, show, paths, view, prune.
        Some(LogsSubcommand::List { limit, json }) => {
            exit_with(logs_cmd::run_logs_list(limit, json))
        }
        Some(LogsSubcommand::Show { launch_id, json }) => {
            exit_with(logs_cmd::run_logs_show(&launch_id, json))
        }
        Some(LogsSubcommand::Paths { json }) => exit_with(logs_cmd::run_logs_paths(json)),
        Some(LogsSubcommand::View { launch_id }) => {
            exit_with(crate::logs_retention::run_logs_view(&launch_id))
        }
        Some(LogsSubcommand::Prune {
            keep,
            dry_run,
            json,
        }) => exit_with(crate::logs_retention::run_logs_prune(keep, dry_run, json)),
        None => {
            // Bare `soldr logs` with no subcommand: print the help-shaped
            // overview from the issue's design plus follow-up hints.
            eprintln!("soldr logs — inspect soldr's runtime activity (issue #820)");
            eprintln!();
            eprintln!("Subcommands:");
            eprintln!("  soldr logs list                List recent launches");
            eprintln!("  soldr logs show <launch-id>    Session summary + log paths");
            eprintln!(
                "  soldr logs paths               Print every directory soldr writes logs into"
            );
            eprintln!("  soldr logs view <launch-id>    Stream a launch's JSONL journal");
            eprintln!("  soldr logs prune --keep N      Keep the newest N archived launches");
            eprintln!();
            eprintln!("Run `soldr logs list --json` for a machine-readable form.");
            guarded_exit(0);
        }
    }
}

async fn run_cache_command(json: bool, command: Option<CacheSubcommand>) -> Result<(), SoldrError> {
    match command {
        Some(CacheSubcommand::Report { json: report_json }) => {
            cache::run_cache_report_command(report_json || json)?;
        }
        Some(CacheSubcommand::Shutdown {
            archive_logs,
            no_depgraph_save,
            shutdown_timeout_seconds,
            no_wait,
            json: shutdown_json,
        }) => {
            cache::run_cache_shutdown_command(
                archive_logs,
                no_depgraph_save,
                shutdown_timeout_seconds,
                !no_wait,
                shutdown_json || json,
            )
            .await?;
        }
        Some(CacheSubcommand::Stats { json: stats_json }) => {
            cache::run_cache_stats_command(stats_json || json)?;
        }
        Some(CacheSubcommand::Gc { json: gc_json }) => {
            cache::run_cache_gc_command(gc_json || json).await?;
        }
        Some(CacheSubcommand::Flush { json: flush_json }) => {
            cache::run_cache_flush_command(flush_json || json).await?;
        }
        Some(CacheSubcommand::PruneTarget {
            path,
            dry_run,
            no_dry_run,
            force,
            keep_latest,
            json: prune_json,
        }) => {
            let effective_dry_run = !(force || no_dry_run);
            // Either flag pair maps onto the same boolean; `dry_run`
            // is the documented default so we accept it explicitly.
            let _ = dry_run;
            cache::run_cache_prune_target_command(
                path,
                effective_dry_run,
                keep_latest,
                prune_json || json,
            )?;
        }
        Some(CacheSubcommand::TrimTarget {
            path,
            profile,
            dry_run,
            no_dry_run,
            force,
            json: trim_json,
        }) => {
            let effective_dry_run = !(force || no_dry_run);
            let _ = dry_run;
            let trim_profile = match profile {
                TrimProfileArg::Local => cache::TrimProfile::Local,
                TrimProfileArg::Ci => cache::TrimProfile::Ci,
            };
            cache::run_cache_trim_target_command(
                path,
                trim_profile,
                effective_dry_run,
                trim_json || json,
            )?;
        }
        Some(CacheSubcommand::ReleaseWorktree {
            path,
            json: rw_json,
        }) => {
            cache::run_cache_release_worktree_command(path, rw_json || json)?;
        }
        Some(CacheSubcommand::SweepTrash { json: st_json }) => {
            cache::run_cache_sweep_trash_command(st_json || json)?;
        }
        None => {
            let output = cache::collect_cache_output()?;
            if json {
                cache::print_json(&output)?;
            } else {
                cache::print_cache_output(&output);
            }
        }
    }
    Ok(())
}

/// `soldr gc [--older-than ...] [<subcommand>]`.
async fn run_gc_cli(
    dry_run: bool,
    all: bool,
    older_than: String,
    larger_than: String,
    json: bool,
    command: Option<GcSubcommand>,
) -> Result<(), SoldrError> {
    if all {
        return Err(SoldrError::Other(
            "`soldr gc --all` no longer deletes targets; use `soldr gc purge --all`".into(),
        ));
    }
    if dry_run && command.is_some() {
        return Err(SoldrError::Other(
            "`soldr gc --dry-run` is a summary alias; use `soldr gc` or `soldr gc purge`".into(),
        ));
    }
    let invocation = match command {
        Some(GcSubcommand::Purge {
            all,
            older_than,
            larger_than,
            json,
            kind,
            registry_src,
            git_checkouts,
            target_incremental,
            build_scripts,
            doc,
            subcommand_caches,
            dry_run,
            unused_days,
        }) => {
            let effective_kind = gc_purge_kind(
                kind,
                registry_src,
                git_checkouts,
                target_incremental,
                build_scripts,
                doc,
                subcommand_caches,
            );
            match gc_purge_invocation(
                effective_kind,
                all,
                older_than,
                larger_than,
                json,
                dry_run,
                unused_days,
            )? {
                Some(invocation) => invocation,
                None => return Ok(()),
            }
        }
        Some(subcommand) => return run_gc_subcommand(subcommand).await,
        None => gc::GcInvocation {
            mode: gc::GcMode::Summary,
            older_than,
            larger_than,
            json,
        },
    };
    gc::run_gc_command(invocation)?;
    Ok(())
}

/// The taxonomy kind a `soldr gc purge` invocation selects.
fn gc_purge_kind(
    kind: Option<GcListKind>,
    registry_src: bool,
    git_checkouts: bool,
    target_incremental: bool,
    build_scripts: bool,
    doc: bool,
    subcommand_caches: bool,
) -> Option<GcListKind> {
    // #323 slice 2: --registry-src is a shorthand for
    // --kind cargo_registry_src; clap already enforces
    // mutual exclusion.
    // #323 slice 3: --git-checkouts is a shorthand for
    // --kind cargo_git_checkouts.
    // #323 slice 4: in-target subtree shorthands map to
    // their explicit taxonomy kinds.
    if registry_src {
        Some(GcListKind::CargoRegistrySrc)
    } else if git_checkouts {
        Some(GcListKind::CargoGitCheckouts)
    } else if target_incremental {
        Some(GcListKind::CargoTargetIncremental)
    } else if build_scripts {
        Some(GcListKind::CargoTargetBuildScriptBinaries)
    } else if doc {
        Some(GcListKind::CargoTargetDoc)
    } else if subcommand_caches {
        Some(GcListKind::CargoTargetSubcommandCaches)
    } else {
        kind
    }
}

/// Runs a kind-specific `soldr gc purge` directly (`None`), or returns the
/// target-cache purge for `gc::run_gc_command`.
fn gc_purge_invocation(
    effective_kind: Option<GcListKind>,
    all: bool,
    older_than: String,
    larger_than: String,
    json: bool,
    dry_run: bool,
    unused_days: Option<u64>,
) -> Result<Option<gc::GcInvocation>, SoldrError> {
    if unused_days.is_some() && !matches!(effective_kind, Some(GcListKind::RustupToolchain)) {
        return Err(SoldrError::Other(
            "gc purge --unused-days is only supported with `--kind rustup_toolchain`".to_string(),
        ));
    }
    // `--dry-run` exists for the rustup toolchain purge (soldr#3507).
    // Every other kind rejects it loudly instead of pretending to honor
    // it; the three cargo-owned report-only kinds below keep their
    // original message even when `--dry-run` is passed alongside it.
    if dry_run
        && !matches!(
            effective_kind,
            Some(
                GcListKind::RustupToolchain
                    | GcListKind::CargoRegistryCache
                    | GcListKind::CargoGitDb
                    | GcListKind::CargoInstalledBinaries
            )
        )
    {
        let got = match effective_kind {
            Some(kind) => format!("--kind {}", gc_kind_display_name(kind)),
            None => "the default cargo_target purge".to_string(),
        };
        return Err(SoldrError::Other(format!(
            "gc purge --dry-run is only supported with `--kind rustup_toolchain` (got {got}); use `soldr gc` for a report-only summary of tracked targets"
        )));
    }
    match effective_kind {
        Some(GcListKind::RustupToolchain) => {
            // Real deletion, delegated: rustup owns the bytes (soldr#3507).
            gc::run_gc_purge_rustup_toolchain_command(all, json, dry_run, unused_days)?;
            Ok(None)
        }
        Some(GcListKind::CargoRegistrySrc) => {
            gc::run_gc_purge_registry_src_command(all, json)?;
            Ok(None)
        }
        Some(GcListKind::CargoGitCheckouts) => {
            gc::run_gc_purge_git_checkouts_command(all, json)?;
            Ok(None)
        }
        Some(
            GcListKind::CargoTargetIncremental
            | GcListKind::CargoTargetBuildScriptBinaries
            | GcListKind::CargoTargetDoc
            | GcListKind::CargoTargetSubcommandCaches,
        ) => {
            gc::run_gc_purge_target_subtree_command(
                effective_kind.expect("matched Some").into(),
                all,
                json,
            )?;
            Ok(None)
        }
        Some(
            GcListKind::CargoRegistryCache
            | GcListKind::CargoGitDb
            | GcListKind::CargoInstalledBinaries,
        ) => {
            let kind_name = gc_kind_display_name(effective_kind.expect("matched Some"));
            Err(SoldrError::Other(format!(
                "gc purge --kind {kind_name} is report-only; cargo/rustup own deletion for this primary cache"
            )))
        }
        Some(GcListKind::CargoTarget) | None => Ok(Some(gc::GcInvocation {
            mode: gc::GcMode::Purge { all },
            older_than,
            larger_than,
            json,
        })),
    }
}

/// Taxonomy kind → the `--kind` spelling users type.
fn gc_kind_display_name(kind: GcListKind) -> &'static str {
    match kind {
        GcListKind::CargoTarget => "cargo_target",
        GcListKind::CargoTargetIncremental => "cargo_target_incremental",
        GcListKind::CargoTargetBuildScriptBinaries => "cargo_target_build_script_binaries",
        GcListKind::CargoTargetDoc => "cargo_target_doc",
        GcListKind::CargoTargetSubcommandCaches => "cargo_target_subcommand_caches",
        GcListKind::CargoRegistrySrc => "cargo_registry_src",
        GcListKind::CargoRegistryCache => "cargo_registry_cache",
        GcListKind::CargoGitCheckouts => "cargo_git_checkouts",
        GcListKind::CargoGitDb => "cargo_git_db",
        GcListKind::CargoInstalledBinaries => "cargo_installed_binaries",
        GcListKind::RustupToolchain => "rustup_toolchain",
    }
}

/// Every `soldr gc` subcommand except `purge`, which `run_gc_cli` resolves.
async fn run_gc_subcommand(subcommand: GcSubcommand) -> Result<(), SoldrError> {
    match subcommand {
        GcSubcommand::Purge { .. } => unreachable!("run_gc_cli handles `soldr gc purge`"),
        GcSubcommand::List { json, kind } => gc::run_gc_list_command(json, kind.map(Into::into)),
        GcSubcommand::Cargo(args) => gc::run_gc_cargo_command(*args),
        GcSubcommand::Locations { json } => gc::run_gc_locations_command(json),
        GcSubcommand::Sweep(args) => gc::run_gc_sweep_command(*args),
        GcSubcommand::Target(args) => gc::run_gc_target_command(*args),
        GcSubcommand::Maintain { root, json } => {
            let status = crate::daemon::maintenance::run_manual_root(root)
                .await
                .map_err(SoldrError::Other)?;
            if json {
                cache::print_json(&status)?;
            } else {
                cache::print_maintenance_status(Some(&status));
            }
            if status.successful_at_ms.is_none() {
                return Err(SoldrError::Other(format!(
                    "cache maintenance did not complete: {}",
                    status
                        .deferred_reason
                        .as_deref()
                        .unwrap_or("component failure")
                )));
            }
            Ok(())
        }
        GcSubcommand::AutoSweep => gc::run_gc_auto_sweep_command(),
        GcSubcommand::HoldBuildLease => run_build_lease_helper(),
    }
}
