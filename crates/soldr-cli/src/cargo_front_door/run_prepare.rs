// The pre-spawn phases of `run_cargo_front_door` (run.rs): toolchain and
// Dylint resolution, the child cargo command and its environment, the cache
// plan, and the build-like preflight checks.

/// Everything the child cargo run and its completion need.
struct PreparedCargo {
    /// The final cargo arguments, subcommand-tool global args included.
    args: Vec<String>,
    explicit_toolchain: Option<String>,
    paths: SoldrPaths,
    cargo: PathBuf,
    command: std::process::Command,
    cache_plan: CargoCachePlan,
    build_like_cargo: bool,
    cache_enabled_for_cargo: bool,
    dylint_entrypoint: bool,
    dylint_plan: Option<crate::dylint_toolchain::DylintToolchainPlan>,
    cargo_job_budget: Option<job_budget::AppliedCargoJobBudget>,
    capture_cargo_artifacts: bool,
    capture_for_diagnostics: bool,
    unrecorded_debug_warning: Option<profile_debug::UnrecordedDebugWarning>,
    // Declared rustfmt-first so they drop in the original order.
    _rustfmt_shim_guard: Option<crate::shim_dir::ShimDirGuard>,
    _dylint_shim_guard: Option<crate::shim_dir::ShimDirGuard>,
}

/// The Dylint scope of this invocation.
struct DylintScope {
    requested: bool,
    /// Only the process that introduces the Dylint scope owns the
    /// setup-soldr success signal.
    entrypoint: bool,
    plan: Option<crate::dylint_toolchain::DylintToolchainPlan>,
    /// The subcommand-tool bootstrap already resolved for an entrypoint.
    early_bootstrap: Option<SubcommandToolBootstrap>,
}

/// The toolchain the child cargo runs under.
struct ResolvedCargoToolchain {
    build_like_cargo: bool,
    paths: SoldrPaths,
    dylint: DylintScope,
    cargo: PathBuf,
    rustc: PathBuf,
    cargo_bin_dir: PathBuf,
}

async fn prepare_child_cargo(
    setup: &FrontDoorSetup,
    normalized: &NormalizedArgs,
    cache_enabled: bool,
    profile: &mut crate::startup_profile::WrapperProfile,
) -> Result<PreparedCargo, SoldrError> {
    let explicit_toolchain = normalized.explicit_toolchain.as_deref();
    let toolchain = resolve_cargo_toolchain(&normalized.args, explicit_toolchain, profile).await?;
    let ResolvedCargoToolchain {
        build_like_cargo,
        paths,
        dylint,
        cargo,
        rustc,
        cargo_bin_dir,
    } = toolchain;
    let existing_path = std::env::var_os("PATH");
    // Build the embedded-cache session plan on a background Tokio task while
    // the rest of the front-door pipeline performs known-subcommand fetch,
    // environment scrubbing, session-id stamping, target-registry
    // memoization, pre-GC, low-disk probing, profile_debug detection, and
    // linker injection. Since soldr#1368 this no longer downloads or extracts
    // a zccache binary; it prepares cache-root, rust-plan, and session state
    // for the service embedded in soldr-daemon. On warm builds the background
    // future resolves effectively immediately so the join at
    // `CargoCachePlan::finalize` is free.
    //
    // We intentionally spawn after the run-trampoline branch above because
    // that path exits without spawning cargo, and we don't
    // want to start a fetch we'll just drop. `cache_enabled` here is
    // the same flag the original synchronous `CargoCachePlan::prepare`
    // gated on; passing `false` produces a no-op `Disabled` prefetch.
    let cache_plan_prefetch = cache_plan::CargoCachePlanPrefetch::start(cache_enabled, &paths);

    // If the user invoked a known ecosystem subcommand (e.g. `cargo nextest`),
    // fetch the corresponding `cargo-<sub>` binary and prepend its directory to
    // PATH so cargo's subcommand dispatch finds it. Also collect transitive
    // bootstrap env (e.g. SDKROOT for explicit legacy
    // `cargo zigbuild --target *-apple-darwin`).
    let mut subcommand_tool_bootstrap = match dylint.early_bootstrap {
        Some(bootstrap) => bootstrap,
        None => ensure_known_subcommand_tool(&normalized.args, &paths).await?,
    };
    host_tooling::inject(&normalized.args, &paths, &mut subcommand_tool_bootstrap).await;
    let args: Vec<String> = if subcommand_tool_bootstrap.cargo_args.is_empty() {
        normalized.args.clone()
    } else {
        insert_cargo_global_args(&normalized.args, &subcommand_tool_bootstrap.cargo_args)
    };
    let (mut command, cargo_job_budget) = build_child_cargo_command(&ChildCargoSpec {
        args: &args,
        cargo: &cargo,
        rustc: &rustc,
        trust_inherited_soldr_env: setup.trust_inherited_soldr_env,
        build_like_cargo,
        explicit_toolchain,
        dylint_plan: dylint.plan.as_ref(),
        dylint_dependency_cook: normalized.dylint_dependency_cook,
        transitive_env_overrides: &subcommand_tool_bootstrap.env,
    })?;

    // Issue #824 follow-up: always engage RUSTC_WRAPPER + the zccache
    // session when caching is enabled, regardless of whether the cargo
    // subcommand is in our known-compiling set. The previous policy
    // (`cache_enabled && build_like_cargo`) silently dropped rustc
    // observations whenever soldr's classifier said "this subcommand
    // doesn't compile" — but build scripts, third-party cargo subcommand
    // plugins not yet in `known_tools`, and even some normally-non-
    // compiling verbs *can* re-shell to rustc through paths we don't
    // model. We always want zccache to see the call, then have zccache
    // itself decide whether to cache or pass through (its "non-cacheable"
    // classifier already handles read-only / non-hashable inputs).
    //
    // The trade-off is a small session-start/stop overhead (~hundreds of
    // ms) for cargo subcommands that don't end up spawning rustc — but
    // the observability win is "no rustc call goes unrecorded". The other
    // hooks (cook hydrate, disk watchdog, target-registry memo) still
    // gate on `build_like_cargo` because those have nothing to do with
    // rustc wrapping — they care about whether `target/` will be touched.
    let cache_enabled_for_cargo = cache_enabled;
    run_pre_path_hooks(&args, &paths, &rustc, build_like_cargo, profile);
    let child_path = apply_child_path_and_target(
        &mut command,
        ChildPathSpec {
            args: &args,
            paths: &paths,
            cargo: &cargo,
            cargo_bin_dir,
            extra_bin_dirs: subcommand_tool_bootstrap.bin_dirs,
            existing_path,
            dylint_active: dylint.plan.is_some(),
            dylint_requested: dylint.requested,
            cache_enabled_for_cargo,
            build_like_cargo,
        },
    )
    .await?;
    let cache_plan = finalize_child_cache_plan(
        &mut command,
        cache_plan_prefetch,
        cache_enabled_for_cargo,
        child_path.native_cache_target.as_deref(),
        dylint.plan.is_some(),
        &paths,
        profile,
    )
    .await?;
    let capture_cargo_artifacts = run_pre_spawn_checks(
        &mut command,
        &cache_plan,
        &args,
        &cargo,
        build_like_cargo,
        profile,
    )?;

    // Capture build diagnostics and non-TTY #422/`-Zthreads` output.
    use std::io::IsTerminal;
    // `-Zthreads` also requires a diagnostic capture under a TTY.
    let capture_for_diagnostics = strip_diagnostics::should_capture(
        build_like_cargo,
        std::io::stderr().is_terminal(),
        zthreads_fallback::environment_mentions_zthreads(),
    );
    Ok(PreparedCargo {
        args,
        explicit_toolchain: normalized.explicit_toolchain.clone(),
        paths,
        cargo,
        command,
        cache_plan,
        build_like_cargo,
        cache_enabled_for_cargo,
        dylint_entrypoint: dylint.entrypoint,
        dylint_plan: dylint.plan,
        cargo_job_budget,
        capture_cargo_artifacts,
        capture_for_diagnostics,
        unrecorded_debug_warning: child_path.unrecorded_debug_warning,
        _rustfmt_shim_guard: child_path.rustfmt_shim_guard,
        _dylint_shim_guard: child_path.dylint_shim_guard,
    })
}

/// The cargo/rustc pair (and the Dylint scope that may pick their channel).
async fn resolve_cargo_toolchain(
    args: &[String],
    explicit_toolchain: Option<&str>,
    profile: &mut crate::startup_profile::WrapperProfile,
) -> Result<ResolvedCargoToolchain, SoldrError> {
    let build_like_cargo = cargo_args_are_cacheable(args);
    if build_like_cargo {
        let repo_root = profile_debug::cargo_invocation_repo_path(args);
        line_endings::maybe_emit_crlf_warning(&repo_root);
    }
    profile.mark("crlf_warning");

    // soldr#2334: the bare `soldr cargo build --target <foreign>`
    // passthrough is contractually verbatim (CLAUDE.md two-build-paths),
    // so it does NOT route C dependencies through the managed target
    // toolchain — cc-built deps compile as host objects and the final
    // link fails with a wall of undefined references. When that shape is
    // detected with no routed/caller toolchain in scope, say so once and
    // name the blessed route instead of letting the link failure explain
    // itself badly.
    maybe_hint_foreign_target_passthrough(args);

    crate::toolchain::ensure_cargo_toolchain(explicit_toolchain)?;
    profile.mark("ensure_cargo_toolchain");
    let paths = SoldrPaths::new()?;
    paths.ensure_dirs()?;
    let dylint = prepare_dylint_scope(args, explicit_toolchain, &paths).await?;
    let effective_toolchain = dylint
        .plan
        .as_ref()
        .map(|plan| plan.channel.as_str())
        .or(explicit_toolchain);
    let cargo = resolve_toolchain_binary_for_channel("cargo", effective_toolchain)?;
    let rustc = resolve_toolchain_binary_for_channel("rustc", effective_toolchain)?;
    crate::startup_trace::phase(crate::startup_trace::phase::CARGO_FRONT_DOOR_TOOLCHAIN_RESOLVED);
    // Deliberately uncached for the ambient default (`binaries.rs`), so this
    // is up to two `rustup which` subprocesses on every invocation.
    profile.mark("resolve_toolchain_binaries");
    let cargo_bin_dir = cargo
        .parent()
        .ok_or_else(|| SoldrError::Other("failed to resolve cargo bin directory".into()))?
        .to_path_buf();
    Ok(ResolvedCargoToolchain {
        build_like_cargo,
        paths,
        dylint,
        cargo,
        rustc,
        cargo_bin_dir,
    })
}

async fn prepare_dylint_scope(
    args: &[String],
    explicit_toolchain: Option<&str>,
    paths: &SoldrPaths,
) -> Result<DylintScope, SoldrError> {
    let dylint_requested = first_cargo_subcommand(args) == Some("dylint");
    let dylint_targets = crate::dylint_target::requested_targets_for_cargo(args)?;
    let dylint_scope_already_active =
        std::env::var_os(crate::dylint_toolchain::TOOLCHAIN_ENV_VAR).is_some();
    // Only the process that introduces the Dylint scope owns the setup-soldr
    // success signal. Recursive cargo-dylint invocations inherit the scope but
    // must never publish completion for their parent.
    let dylint_entrypoint = dylint_requested && !dylint_scope_already_active;
    let dylint_scoped = dylint_requested || dylint_scope_already_active;
    if dylint_entrypoint {
        crate::dylint_toolchain::clear_success_marker()?;
    }
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // Resolve release-packaged Dylint binaries before preparing nightly.
    let early_dylint = if dylint_entrypoint {
        let bootstrap = ensure_known_subcommand_tool(args, paths).await?;
        let plan =
            crate::dylint_toolchain::resolve_plan(explicit_toolchain, &workspace_root).await?;
        crate::dylint_driver::ensure_prebuilt_driver(&plan, paths).await?;
        Some((bootstrap, plan))
    } else {
        None
    };
    let dylint_plan = if let Some((_, plan)) = early_dylint.as_ref() {
        Some(crate::dylint_toolchain::prepare_resolved(plan.clone())?)
    } else if dylint_scoped {
        Some(crate::dylint_toolchain::prepare(explicit_toolchain, &workspace_root).await?)
    } else {
        None
    };
    if let Some(plan) = dylint_plan.as_ref() {
        crate::dylint_target::ensure_targets(&plan.channel, &dylint_targets)?;
    }
    Ok(DylintScope {
        requested: dylint_requested,
        entrypoint: dylint_entrypoint,
        plan: dylint_plan,
        early_bootstrap: early_dylint.map(|(bootstrap, _)| bootstrap),
    })
}

/// The inputs that shape the child cargo command and its environment.
struct ChildCargoSpec<'a> {
    args: &'a [String],
    cargo: &'a Path,
    rustc: &'a Path,
    trust_inherited_soldr_env: bool,
    build_like_cargo: bool,
    explicit_toolchain: Option<&'a str>,
    dylint_plan: Option<&'a crate::dylint_toolchain::DylintToolchainPlan>,
    dylint_dependency_cook: bool,
    transitive_env_overrides: &'a [(String, String)],
}

fn build_child_cargo_command(
    spec: &ChildCargoSpec<'_>,
) -> Result<
    (
        std::process::Command,
        Option<job_budget::AppliedCargoJobBudget>,
    ),
    SoldrError,
> {
    let args = spec.args;
    // Compute env-var overrides keyed off the subcommand + its
    // --target argument. Today this fixes ring's build.rs on
    // `cargo xwin build --target *-pc-windows-msvc` by routing cc-rs
    // to `clang-cl` instead of the GNU-flavoured `clang`. See
    // `compute_subcommand_env_overrides` for the full rule set.
    let subcommand_env_overrides = compute_subcommand_env_overrides(args);

    let mut command = std::process::Command::new(spec.cargo);
    command.args(crate::target_alias::args_without_glibc_floor(args).iter());
    crate::binaries::apply_resolved_toolchain_homes(&mut command, spec.cargo);
    suppress_windows_console_window(&mut command);
    // These Soldr control variables are consumed by this front-door
    // process. Letting Cargo inherit them leaks daemon lifecycle or retry
    // policy into build scripts and test binaries that may spawn nested Soldr.
    scrub_soldr_cache_lifecycle_env_for_child_cargo(&mut command);
    command.env_remove(zthreads_fallback::ATTEMPTED_ENV);
    if !spec.trust_inherited_soldr_env {
        scrub_inherited_soldr_workspace_env_for_child_cargo(&mut command);
    }
    // soldr cargo is the top of the invocation tree, so any inherited
    // MAKEFLAGS/CARGO_MAKEFLAGS points at jobserver fds that aren't open in
    // our process. Stripping them lets cargo start a fresh jobserver instead
    // of printing the "failed to connect to jobserver" warning (see #283).
    command.env_remove("MAKEFLAGS");
    command.env_remove("CARGO_MAKEFLAGS");
    quiet_library_backtraces_for_child_cargo(&mut command);
    command.env("RUSTC", spec.rustc);
    // soldr#2878: Cargo performs fingerprinting and directory scans before a
    // rustc request can reach the daemon's compiler admission gate. Capture
    // cgroup/memory observations for a precise failure diagnostic, but leave
    // Cargo's default jobserver policy and every explicit override untouched.
    let cargo_job_budget = spec
        .build_like_cargo
        .then(|| job_budget::apply(args, &mut command));

    apply_child_cargo_toolchain_env(&mut command, spec);

    // Apply subcommand-derived env overrides (e.g. CC_<triple>=clang-cl
    // for `cargo xwin build --target *-pc-windows-msvc`). Honor a
    // caller-set value — don't clobber if the user already exported
    // their own CC / CXX / AR.
    for (key, value) in &subcommand_env_overrides {
        if std::env::var_os(key).is_none() {
            command.env(key, value);
        }
    }
    // Apply transitive-bootstrap env overrides (e.g. SDKROOT for explicit
    // legacy `cargo zigbuild --target *-apple-darwin`). These come from
    // `ensure_known_subcommand_tool` which calls into ensure_apple_sdk
    // / ensure_zig / etc. The functions themselves already gate on
    // `var_os` being unset before pushing, so just apply them.
    for (key, value) in spec.transitive_env_overrides {
        command.env(key, value);
    }

    emit_zig_cross_linker_preflight(&command, args)?;
    Ok((command, cargo_job_budget))
}

/// The child cargo's toolchain pin and the Dylint plan's environment.
fn apply_child_cargo_toolchain_env(command: &mut std::process::Command, spec: &ChildCargoSpec<'_>) {
    // Issue #836 (sub of #835): pin the rust toolchain explicitly via
    // RUSTUP_TOOLCHAIN so rustup does NOT consult `rust-toolchain.toml`
    // on the cargo side and try to install the manifest's declared
    // `components = [...]` automatically.
    //
    // Why this matters in CI: many runner images (the GitHub-hosted
    // ubuntu-* lineage especially) ship a pre-existing `bin/cargo-fmt`
    // that conflicts with rustup's `rustfmt-preview` component install,
    // producing the well-known
    //
    //     error: failed to install component:
    //       'rustfmt-preview-x86_64-unknown-linux-gnu',
    //       detected conflict: 'bin/cargo-fmt'
    //
    // which kills the build before cargo even starts compiling. The
    // soldr bootstrap is supposed to short-circuit this — soldr itself
    // already knows the manifest channel (via
    // `read_rust_toolchain_manifest`), so passing it explicitly to
    // rustup with `RUSTUP_TOOLCHAIN` skips the manifest read on the
    // child cargo, and with it the entire auto-component-install path.
    //
    // Honor an explicit caller-set RUSTUP_TOOLCHAIN (don't clobber).
    // For users who genuinely need rustfmt / clippy at build time,
    // `soldr cargo fmt` / `clippy` still self-install via
    // `component_install::maybe_install_component_for_subcommand`.
    if let Some(toolchain) = spec.explicit_toolchain {
        command.env("RUSTUP_TOOLCHAIN", toolchain);
    } else if std::env::var_os("RUSTUP_TOOLCHAIN").is_none() {
        crate::toolchain_dir_name::export_manifest_channel(command);
    }
    if let Some(plan) = spec.dylint_plan {
        plan.apply_to_command(command);
    }
    // soldr#3394: tools that read RUSTUP_TOOLCHAIN literally (the Dylint
    // driver's sysroot) need the installed directory name under a managed home.
    crate::toolchain_dir_name::apply_installed_toolchain_dir_name(command);
    if spec.dylint_dependency_cook {
        command.env_remove("RUSTC_WORKSPACE_WRAPPER");
        for (name, _) in std::env::vars_os() {
            let text = name.to_string_lossy();
            if text.starts_with("DYLINT_") || text == "ZCCACHE_DYLINT_CACHE_INPUT_HASH" {
                command.env_remove(name);
            }
        }
    }
}

/// Component auto-install, the cook-index hydrate and the startup GC
/// warning: best-effort hooks that run before the child PATH is built.
fn run_pre_path_hooks(
    args: &[String],
    paths: &SoldrPaths,
    rustc: &Path,
    build_like_cargo: bool,
    profile: &mut crate::startup_profile::WrapperProfile,
) {
    // Issue #597: auto-install rustup components for `soldr cargo {fmt,
    // clippy,miri}` when they're missing. Best-effort and silent on
    // failure — cargo's own error surfaces if the auto-install fails.
    // Honors SOLDR_NO_AUTO_COMPONENT=1.
    component_install::maybe_install_component_for_subcommand(args, paths);

    // PR 3 (#578, meta #579): cross-repo cook-index pre-flight hydrate.
    // Best-effort — every failure path is silent so a missing daemon,
    // missing Cargo.lock, mismatched sha, or extract error never
    // breaks the cargo build. Only fires for build-like cargo
    // commands; `cargo metadata` / `cargo search` / etc. don't need
    // target/ to be populated.
    if build_like_cargo {
        cook_hydrate::maybe_hydrate(args, paths, rustc);
    }
    // Nothing here is memoized: a recursive walk hashing every Cargo.toml,
    // plus `rustc -V`, `git config --get remote.origin.url` and
    // `git branch --show-current` subprocesses, plus a daemon CookLookup.
    profile.mark("cook_hydrate");

    if build_like_cargo {
        // Cargo front door only: keep startup/low-disk warnings off unrelated
        // commands and out of the rustc-wrapper hot path.
        gc::emit_startup_target_warning_if_due();
    }
    profile.mark("gc_startup_warning");
}

/// Whether the cargo subcommand builds through a target-aware PyO3 plan.
fn cargo_subcommand_builds_pyo3(cargo_subcommand: Option<&str>) -> bool {
    matches!(
        cargo_subcommand,
        Some(
            "b" | "build"
                | "c"
                | "check"
                | "t"
                | "test"
                | "bench"
                | "d"
                | "doc"
                | "r"
                | "run"
                | "clippy"
                | "fix"
        )
    ) || cargo_subcommand == Some(concat!("rust", "c"))
}

/// The inputs to the child's PATH, target and linker setup.
struct ChildPathSpec<'a> {
    args: &'a [String],
    paths: &'a SoldrPaths,
    cargo: &'a Path,
    cargo_bin_dir: PathBuf,
    extra_bin_dirs: Vec<PathBuf>,
    existing_path: Option<OsString>,
    dylint_active: bool,
    dylint_requested: bool,
    cache_enabled_for_cargo: bool,
    build_like_cargo: bool,
}

/// The shim dirs the child's PATH points into (kept alive for the run) and
/// the target the native cache keys on.
struct ChildPathOutcome {
    dylint_shim_guard: Option<crate::shim_dir::ShimDirGuard>,
    rustfmt_shim_guard: Option<crate::shim_dir::ShimDirGuard>,
    native_cache_target: Option<String>,
    unrecorded_debug_warning: Option<profile_debug::UnrecordedDebugWarning>,
}

async fn apply_child_path_and_target(
    command: &mut std::process::Command,
    spec: ChildPathSpec<'_>,
) -> Result<ChildPathOutcome, SoldrError> {
    let args = spec.args;
    let pyo3_build = cargo_subcommand_builds_pyo3(first_cargo_subcommand(args));
    let dylint_shim_guard = if spec.dylint_active && crate::shim_dir::should_install_shims() {
        Some(crate::shim_dir::build_dylint_shim_dir()?)
    } else {
        None
    };
    let mut path_dirs: Vec<std::path::PathBuf> = Vec::with_capacity(2 + spec.extra_bin_dirs.len());
    if let Some(guard) = &dylint_shim_guard {
        path_dirs.push(guard.path.clone());
        command.env(crate::shim_dir::SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR, "1");
    }
    // soldr#3452: a nested `cargo` must still see RUSTUP_TOOLCHAIN.
    path_dirs.extend(crate::cargo_toolchain_shim::shim_dir_for(
        command, spec.paths, spec.cargo,
    )?);
    path_dirs.push(spec.cargo_bin_dir);
    path_dirs.extend(spec.extra_bin_dirs);
    command.env(
        "PATH",
        disk::prepend_paths(&path_dirs, spec.existing_path.as_deref())?,
    );
    let rustfmt_shim_guard =
        maybe_apply_rustfmt_zccache_shim(command, args, spec.cache_enabled_for_cargo);
    let explicit_target = target::default_cargo_build_target(args, spec.dylint_requested)?;
    if let Some(target) = explicit_target.as_deref() {
        command.env("CARGO_BUILD_TARGET", target);
    }
    let known_cargo_target = target::known_cargo_build_target(args, explicit_target.as_deref());
    // Applies the profile-debug default to `command`, and returns a warning
    // the daemon could not record yet (soldr#3740).
    let unrecorded_debug_warning = if spec.build_like_cargo {
        profile_debug::maybe_apply_cargo_profile_debug_default(
            command,
            args,
            spec.paths,
            known_cargo_target.as_deref(),
        )?
    } else {
        None
    };
    // soldr#1610/#1614: every cargo-backed build surface consumes the
    // same target-aware PyO3 plan. The resolver is conservative: it only
    // injects PYO3_NO_PYTHON for a proven cross ABI3 extension, never for
    // embedding/non-ABI3 builds, and never downloads Python assets merely
    // because PyO3 appears in metadata.
    if pyo3_build {
        let workspace_root =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        // soldr#3794: an up-to-date lockfile without `pyo3` makes the
        // `cargo metadata` probe pointless; the resolved cargo is the one
        // for the effective channel, not the ambient default.
        let hints = crate::pyo3_detect::CargoProbeHints {
            cargo: Some(spec.cargo),
            lock_proves_no_pyo3: lockfile_proof::lockfile_lacks_package(
                args,
                &workspace_root,
                "pyo3",
            ),
        };
        let mut pyo3_plan = crate::pyo3_detect::resolve_for_cargo_invocation(
            &workspace_root,
            args,
            known_cargo_target.as_deref(),
            hints,
        );
        pyo3_plan.materialize_compatibility(spec.paths).await?;
        pyo3_plan.emit_diagnostic();
        pyo3_plan.apply_to_command(command);
    }
    let native_cache_target = known_cargo_target.filter(|target| target.ends_with("-apple-darwin"));

    target::apply_linker_override(
        command,
        args,
        explicit_target.as_deref(),
        spec.paths,
        spec.dylint_active,
    )
    .await?;
    Ok(ChildPathOutcome {
        dylint_shim_guard,
        rustfmt_shim_guard,
        native_cache_target,
        unrecorded_debug_warning,
    })
}

async fn finalize_child_cache_plan(
    command: &mut std::process::Command,
    cache_plan_prefetch: cache_plan::CargoCachePlanPrefetch,
    cache_enabled_for_cargo: bool,
    native_cache_target: Option<&str>,
    dylint_active: bool,
    paths: &SoldrPaths,
    profile: &mut crate::startup_profile::WrapperProfile,
) -> Result<CargoCachePlan, SoldrError> {
    // L3 (soldr#980): await the background zccache prefetch we kicked
    // off near the top of this function. Up until this point the cargo
    // command has been built without any wrapper env, so the prefetch
    // has been overlapping the entire setup pipeline. On a cold build
    // (binary not yet on disk) this is where the ~1-2 s saving falls
    // out — on a warm build the await is a near-no-op.
    //
    // Note: `cache_enabled_for_cargo` is currently `cache_enabled` (see
    // the comment above its assignment for the #824 follow-up
    // rationale). We thread it through `finalize` for symmetry with the
    // old synchronous API so that future divergence between the two
    // flags doesn't silently rewire the prefetch decision.
    let cache_plan = CargoCachePlan::finalize(cache_enabled_for_cargo, cache_plan_prefetch).await?;
    profile.mark("cache_plan_finalize");
    cache_plan.apply_to_command(command, native_cache_target)?;
    if dylint_active && cache_plan.uses_managed_zccache() {
        // Re-point the pair, not just RUSTC_WRAPPER: `apply_to_command`
        // above already stamped the rustc-shim identity into the
        // effective-wrapper mirror, and cargo-dylint re-enters the front
        // door (its nested `cargo metadata`), where a mismatched pair
        // fails the soldr#2545 drift guard. That failure is silent at
        // this level — dylint reports "No libraries were found" and
        // exits 0 having linted nothing (soldr#2634).
        crate::wrapper_identity::set_owned_rustc_wrapper(
            command,
            crate::binaries::dylint_wrapper_shim_binary(paths)?.as_os_str(),
            crate::wrapper_identity::WrapperOrigin::SoldrManaged,
        );
    }
    Ok(cache_plan)
}

/// The checks between the finalized cache plan and the build session:
/// artifact capture, the test-suite guard, the disk watchdog, the no-cache
/// detach and the stale fallback-notice scrub. Returns whether cargo's JSON
/// artifact stream is captured.
fn run_pre_spawn_checks(
    command: &mut std::process::Command,
    cache_plan: &CargoCachePlan,
    args: &[String],
    cargo: &Path,
    build_like_cargo: bool,
    profile: &mut crate::startup_profile::WrapperProfile,
) -> Result<bool, SoldrError> {
    // soldr#2996/#2997: packed-DWARF embedding (soldr#1775) used to be reachable
    // only when a target-cache plan existed, because this capture -- which
    // produces the artifact closure the embed consumes -- required one. That
    // coupling was accidental, so removing the target cache would have silently
    // deleted the feature.
    //
    // It gets its own explicit gate instead. Enabling the capture unconditionally
    // is not an option: it appends `--message-format=json` to every build-like
    // invocation, clippy and dylint included, which changes the command line the
    // whole toolchain sees. Default-off preserves exactly the behaviour every
    // caller has today; opting in is now a decision rather than a side effect of
    // a cache setting.
    let capture_cargo_artifacts = build_like_cargo
        && !cargo_args_have_message_format(args)
        && std::env::var_os(crate::EMBED_PACKED_DWARF_ENV_VAR)
            .map(|value| crate::core::flag_value(&value.to_string_lossy()))
            .unwrap_or(false);
    if capture_cargo_artifacts {
        // Cargo's JSON stream is line-oriented and preserves rendered
        // diagnostics in the message payload. It lets us build an exact
        // package-aware closure while teeing the bytes unchanged below.
        command.arg("--message-format=json");
    }
    if build_like_cargo || cargo_args_may_compile_unmediated(args) {
        forbid_test_suite_target(args)?;
    }
    if build_like_cargo {
        check_build_disk_space(cache_plan, args)?;
    }
    detach_target_for_unmediated_build(cache_plan, args, cargo, command)?;
    // Two full recursive walks of target/ plus a `cargo metadata`
    // subprocess. Only runs without a zccache session (i.e. --no-cache).
    profile.mark("no_cache_detach");
    if build_like_cargo {
        scrub_stale_fallback_notices(cache_plan, args);
    }
    Ok(capture_cargo_artifacts)
}

/// The low-disk advisory and the host-volume disk watchdog.
fn check_build_disk_space(cache_plan: &CargoCachePlan, args: &[String]) -> Result<(), SoldrError> {
    let probe_path = cache_plan
        .target_dir_for_hooks(args)
        .unwrap_or_else(|| disk::cargo_disk_space_probe_path(args));
    disk::maybe_emit_low_disk_warning(&probe_path);
    // Issue #574: host-volume disk watchdog. Distinct from the
    // legacy 2 GiB advisory above — this layer warns at 10 GiB and
    // aborts at 5 GiB so cross-repo target/ bloat surfaces before
    // the build sets the disk on fire. Returning Err here lets the
    // top-level dispatch print the error and exit with a non-zero
    // code (same path as any other SoldrError from the front door).
    let watchdog_path = cache_plan
        .target_dir_for_hooks(args)
        .unwrap_or_else(|| disk::cargo_disk_space_probe_path(args));
    match gc::disk::check_disk_or_warn_or_block(&watchdog_path) {
        gc::disk::DiskCheckOutcome::Disabled | gc::disk::DiskCheckOutcome::Ok { .. } => {}
        gc::disk::DiskCheckOutcome::Warn {
            free_bytes,
            threshold_gib,
        } => {
            gc::disk::warn_and_reclaim(&watchdog_path, free_bytes, threshold_gib);
        }
        // soldr#2134: reclaim first, block only if that was not enough.
        gc::disk::DiskCheckOutcome::Block {
            free_bytes,
            threshold_gib,
        } => gc::disk::reclaim_then_block(&watchdog_path, free_bytes, threshold_gib)?,
    }
    Ok(())
}

fn detach_target_for_unmediated_build(
    cache_plan: &CargoCachePlan,
    args: &[String],
    cargo: &Path,
    command: &std::process::Command,
) -> Result<(), SoldrError> {
    // A preceding cached build may have materialized immutable outputs as
    // protected hardlinks to cache blobs. Whenever the finalized wrapper plan
    // has no embedded-cache session, detach shared target files locally
    // before the unmediated compiler can overwrite them. This must not depend
    // on the daemon being responsive. Conservatively include `install`:
    // configuration can select a persistent target root without a visible
    // command-line or environment override.
    if cargo_args_may_compile_unmediated(args) && cache_plan.zccache_session().is_none() {
        let report = no_cache_detach::prepare_target_for_unmediated_build(cargo, args, command)?;
        if report.detached_shared > 0 || report.made_writable > 0 {
            eprintln!(
                "soldr: no-cache preflight prepared {}: detached {} shared file(s), made {} private file(s) writable",
                report.target_dir.display(),
                report.detached_shared,
                report.made_writable,
            );
        }
    }
    Ok(())
}

fn scrub_stale_fallback_notices(cache_plan: &CargoCachePlan, args: &[String]) {
    // Target-registry memoization for the wrapper hot path (#440).
    // Without this, every rustc invocation re-opens redb and writes
    // the same target row (~14 ms p50 on Windows in the issue #440
    // profile). The cargo front door runs once per build session and
    // already knows the target dir, so do the upsert here and
    // propagate a recorded-marker env var that lets the wrapper skip
    // its own redb work + daemon target-touch IPC.
    let target_dir_for_memo: Option<std::path::PathBuf> = cache_plan.target_dir_for_hooks(args);
    if let Some(dir) = target_dir_for_memo.as_deref() {
        match scrub_cached_fallback_diagnostics_once(dir) {
            Ok(FallbackOutputScrub::AlreadyDone | FallbackOutputScrub::Complete(0)) => {}
            Ok(FallbackOutputScrub::DeferredForActiveBuild(_)) => {}
            Ok(FallbackOutputScrub::Complete(count)) => eprintln!(
                "soldr: removed {count} stale compiler-cache fallback notice file(s) from {}",
                dir.display()
            ),
            Err(error) => eprintln!(
                "soldr warning: failed to remove stale compiler-cache fallback notices from {}: {error}",
                dir.display()
            ),
        }
    }
}
