/// The global flags every subcommand handler may need, resolved once by
/// [`run_cli`].
#[derive(Clone, Copy)]
struct DispatchFlags {
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
}

/// Exit with a handler's exit code, or propagate its error.
fn exit_with(result: Result<i32, SoldrError>) -> Result<(), SoldrError> {
    guarded_exit(result?)
}

async fn run_cli(cli: Cli) -> Result<(), SoldrError> {
    // #1364: a truthy `ZCCACHE_DISABLE` acts like `--no-cache` so the
    // standard zccache kill-switch actually bypasses the wrapper/daemon.
    let flags = DispatchFlags {
        cache_enabled: !cli.no_cache && !cargo_front_door::zccache_disable_requested(),
        trust_inherited_soldr_env: cli.trust_inherited_soldr_env,
    };
    // soldr#1766 / soldr#1761: global flags with an env-var spelling are
    // published together by `Cli::export_global_env`, so the whole process
    // tree -- including the daemon, which auto-forwards `SOLDR_*` -- agrees
    // without threading booleans through every prepare path.
    cli.export_global_env();

    // Each family handles its own commands and hands any other back.
    let Some(command) = dispatch_build_command(cli.command, flags).await? else {
        return Ok(());
    };
    let Some(command) = dispatch_toolchain_command(command, flags).await? else {
        return Ok(());
    };
    dispatch_state_command(command, flags).await
}

/// Build, compile and packaging commands. Returns any other command unhandled.
async fn dispatch_build_command(
    command: Commands,
    flags: DispatchFlags,
) -> Result<Option<Commands>, SoldrError> {
    let DispatchFlags {
        cache_enabled,
        trust_inherited_soldr_env,
    } = flags;
    let handled = match command {
        Commands::Build { args } => {
            run_blessed_build(args, cache_enabled, trust_inherited_soldr_env).await
        }
        Commands::Cc(args) => exit_with(crate::cc_cmd::run(args, crate::cc_cmd::Language::C).await),
        Commands::Cxx(args) => {
            exit_with(crate::cc_cmd::run(args, crate::cc_cmd::Language::Cxx).await)
        }
        Commands::Ld(args) => exit_with(crate::ld_cmd::run_ld(args).await),
        Commands::Link(args) => exit_with(crate::ld_cmd::run_link(args).await),
        Commands::Wheel(args) => run_wheel_command(&args, flags).await,
        Commands::Cargo { args } => run_cargo_command(args, flags).await,
        Commands::Dylint { args } => exit_with(
            Box::pin(run_dylint_command(
                args,
                cache_enabled,
                trust_inherited_soldr_env,
            ))
            .await,
        ),
        Commands::Lint { args } => {
            exit_with(lint_cmd::run_lint(&args, cache_enabled, trust_inherited_soldr_env).await)
        }
        Commands::CiTest { args } => {
            exit_with(crate::ci_test::run(&args, cache_enabled, trust_inherited_soldr_env).await)
        }
        Commands::Cook { args } => exit_with(cook::run_cook(&args, cache_enabled).await),
        Commands::Exec { args } => exit_with(exec_cmd::run_exec(&args)),
        Commands::Archive {
            target,
            stage_dir,
            input,
            extract_dir,
            output,
        } => archive_cmd::run(target, output, stage_dir, input, extract_dir),
        Commands::Prepare {
            target,
            github_env,
            save,
            restore,
        } => run_prepare_command(&target, github_env, save, restore).await,
        Commands::BuildFromSource {
            tool,
            target,
            version,
        } => build_from_source_cmd::run(&tool, target, version),
        Commands::Install(args) => crate::install::run(args).await,
        Commands::Env {
            target,
            shell_export,
            json,
            plan_only,
        } => exit_with(env_cmd::run_env_command(&target, shell_export, json, plan_only).await),
        other => return Ok(Some(other)),
    };
    handled.map(|()| None)
}

/// Toolchain passthroughs and host setup. Returns any other command unhandled.
async fn dispatch_toolchain_command(
    command: Commands,
    flags: DispatchFlags,
) -> Result<Option<Commands>, SoldrError> {
    let cache_enabled = flags.cache_enabled;
    let handled = match command {
        Commands::Rustc { args } => {
            exit_with(toolchain::run_rustc_like("rustc", &args, cache_enabled))
        }
        Commands::Rustfmt { args } => exit_with(toolchain::run_rustfmt(&args, cache_enabled)),
        Commands::ClippyDriver { args } => exit_with(toolchain::run_rustc_like(
            "clippy-driver",
            &args,
            cache_enabled,
        )),
        Commands::Rustdoc { args } => exit_with(toolchain::run_rustdoc(&args)),
        Commands::RustGdb { args } => {
            exit_with(toolchain::run_toolchain_passthrough("rust-gdb", &args))
        }
        Commands::RustLldb { args } => {
            exit_with(toolchain::run_toolchain_passthrough("rust-lldb", &args))
        }
        Commands::RustAnalyzer { args } => {
            exit_with(toolchain::run_rust_analyzer(&args, cache_enabled))
        }
        Commands::Rustup { args } => exit_with(toolchain::run_rustup_passthrough(&args)),
        Commands::Toolchain { subcommand } => run_toolchain_subcommand(subcommand).await,
        Commands::Bootstrap { json } => exit_with(bootstrap::run_bootstrap(json).await),
        Commands::Doctor {
            json,
            refresh_defender_probe,
            remove_shadowing_shim: fix,
        } => exit_with(doctor::run_doctor(json, refresh_defender_probe, fix)),
        Commands::Optimize(args) => exit_with(optimize::run_optimize(args)),
        Commands::Shims { json } => run_shims_command(json),
        Commands::DefenderExclusions { subcommand } => {
            exit_with(optimize::run_defender_exclusions(subcommand))
        }
        other => return Ok(Some(other)),
    };
    handled.map(|()| None)
}

/// Cache, state, daemon and fetched-tool commands: every command the other
/// families hand back.
async fn dispatch_state_command(command: Commands, flags: DispatchFlags) -> Result<(), SoldrError> {
    match command {
        Commands::Save(args) => guarded_exit(save_load::run_save(args)),
        Commands::Hydrate(args) => guarded_exit(save_load::run_load(args)),
        Commands::Status { json } => run_status_command(json, flags.cache_enabled),
        Commands::Clean => cache::clear_zccache_cache(),
        Commands::Purge => cache::purge_soldr_cache(),
        Commands::Config { command } => crate::config_cmd::run_config_command(command),
        Commands::Logs { command } => run_logs_command(command),
        Commands::Cache { json, command } => run_cache_command(json, command).await,
        Commands::Version { json } => run_version_command(json),
        Commands::Gc {
            dry_run,
            all,
            older_than,
            larger_than,
            json,
            command,
        } => run_gc_cli(dry_run, all, older_than, larger_than, json, command).await,
        Commands::SessionStart {
            id,
            log,
            journal,
            json,
        } => cache::run_session_start_command(id, log, journal, json).await,
        Commands::SessionEnd { id, clear, json } => cache::run_session_end_command(id, clear, json),
        Commands::Daemon { command } => run_daemon_command(command).await,
        Commands::Broker { command } => crate::broker_cmd::run_broker_command(command),
        Commands::Verdict { args } => exit_with(crate::verdict::cli::run(&args)),
        Commands::External(args) => Box::pin(run_external_command(args, flags)).await,
        // Listed rather than `_` so a new command still fails to compile until
        // it is routed to a family.
        Commands::Build { .. }
        | Commands::Cc(_)
        | Commands::Cxx(_)
        | Commands::Ld(_)
        | Commands::Link(_)
        | Commands::Wheel(_)
        | Commands::Cargo { .. }
        | Commands::Dylint { .. }
        | Commands::Lint { .. }
        | Commands::CiTest { .. }
        | Commands::Cook { .. }
        | Commands::Exec { .. }
        | Commands::Archive { .. }
        | Commands::Prepare { .. }
        | Commands::BuildFromSource { .. }
        | Commands::Install(_)
        | Commands::Env { .. }
        | Commands::Rustc { .. }
        | Commands::Rustfmt { .. }
        | Commands::ClippyDriver { .. }
        | Commands::Rustdoc { .. }
        | Commands::RustGdb { .. }
        | Commands::RustLldb { .. }
        | Commands::RustAnalyzer { .. }
        | Commands::Rustup { .. }
        | Commands::Toolchain { .. }
        | Commands::Bootstrap { .. }
        | Commands::Doctor { .. }
        | Commands::Optimize(_)
        | Commands::Shims { .. }
        | Commands::DefenderExclusions { .. } => {
            unreachable!("dispatch_build_command / dispatch_toolchain_command handle this command")
        }
    }
}
