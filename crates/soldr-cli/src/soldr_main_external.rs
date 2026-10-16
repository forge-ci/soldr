/// `soldr <tool>[@version] [args...]`: a cargo shorthand, the embedded
/// zccache surface, or a fetched tool.
async fn run_external_command(args: Vec<String>, flags: DispatchFlags) -> Result<(), SoldrError> {
    if args.is_empty() {
        eprintln!("usage: soldr <tool>[@version] [args...]");
        guarded_exit(1);
    }

    let (crate_name, version) = parse_tool_spec(&args[0]);
    let tool_args = &args[1..];

    // Issue #683 (parent #682, phase 1): bare cargo-subcommand
    // shorthand. When the typed verb (sans `@version`) is one
    // soldr already prebuilds as a cargo subcommand
    // (`KNOWN_TOOLS::lookup_by_cargo_subcommand`), route through
    // the cargo front door — `soldr nextest run` becomes
    // `soldr cargo nextest run`. This avoids the doomed
    // crates.io fetch for a literally-named `nextest` crate.
    // Version-pinned forms (`soldr nextest@0.9.x`) keep the
    // existing External path; cargo-subcommand pins are
    // managed in the soldr registry and the front door has no
    // per-invocation knob.
    if matches!(version, VersionSpec::Latest)
        && crate::fetch::lookup_by_cargo_subcommand(&crate_name).is_some()
    {
        return run_cargo_shorthand(&crate_name, tool_args, false, flags).await;
    }

    // Issue #685 (parent #682, phase 2): bare cargo built-in
    // shorthand. When the typed verb is one of cargo's own
    // first-party verbs (`build`, `test`, `check`, `clippy`,
    // `fmt`, ...), route through the cargo front door —
    // `soldr build --release` becomes `soldr cargo build
    // --release`. The collision verbs `clean` / `config` /
    // `version` are captured by clap before reaching this
    // arm; see `is_cargo_builtin_verb` for the explicit
    // exclusion list. Version-pinned forms keep the existing
    // External fetch path so `soldr build@1.0` parses
    // exactly like `soldr <unknown-tool>@1.0` does today.
    if matches!(version, VersionSpec::Latest) && is_cargo_builtin_verb(&crate_name) {
        return run_cargo_shorthand(&crate_name, tool_args, true, flags).await;
    }

    // soldr#2898: `soldr zccache <args>` is a reserved embedded surface.
    // No standalone binary is resolved, downloaded, or invoked.
    // The compatibility forms route directly
    // into Soldr-owned compatibility handlers.
    if crate_name == "zccache" {
        guarded_exit(crate::zccache_compat::run(tool_args, version).await?);
    }

    // Issue #412: when the user typed a verb that LOOKS like
    // a typo or a renamed built-in (for example,
    // `build-from-sorce`), emit a "did you mean?" hint before
    // we fire the network fetch. The fetch still runs — the
    // suggestion is advisory.
    // Cargo's bare built-ins are an equally real top-level shorthand.
    // Prefer them before Soldr-native verbs: in particular `tset`
    // must lead a user back to `soldr test` (cargo test), not the
    // unrelated orchestration surface `soldr ci-test`.
    if let Some(suggestion) = fuzzy_match::suggest_close_match(&crate_name, CARGO_BUILTIN_VERBS)
        .or_else(|| fuzzy_match::suggest_close_match(&crate_name, SOLDR_BUILTIN_VERBS))
    {
        eprintln!("soldr: '{crate_name}' is not a known built-in soldr verb.");
        eprintln!("soldr: did you mean: {suggestion}?");
    }

    run_fetched_tool(&crate_name, &version, tool_args, flags.cache_enabled).await
}

/// `soldr <verb> ...` as `soldr cargo <verb> ...` (issues #683 / #685).
async fn run_cargo_shorthand(
    verb: &str,
    tool_args: &[String],
    inject_msvc_host_env: bool,
    flags: DispatchFlags,
) -> Result<(), SoldrError> {
    let mut cargo_args = Vec::with_capacity(tool_args.len() + 1);
    cargo_args.push(verb.to_string());
    cargo_args.extend(tool_args.iter().cloned());
    let cargo_args = crate::target_lifecycle::prepare_cargo_invocation(cargo_args).await?;
    if inject_msvc_host_env {
        // soldr#1105: bare-verb dispatch must also pre-inject
        // the host MSVC env so `soldr check` / `soldr build` /
        // `soldr test` on Windows behave the same as the
        // explicit `soldr cargo ...` forms with respect to
        // rust-lld's `LIB` requirement.
        ensure_msvc_host_env_for_native(&cargo_args).await;
    }
    guarded_exit(
        cargo_front_door::run_cargo_front_door(
            &cargo_args,
            flags.cache_enabled,
            flags.trust_inherited_soldr_env,
        )
        .await?,
    );
}

/// Fetch `crate_name` (or reuse the cached copy) and run it as a child,
/// exiting with its exit code.
async fn run_fetched_tool(
    crate_name: &str,
    version: &VersionSpec,
    tool_args: &[String],
    cache_enabled: bool,
) -> Result<(), SoldrError> {
    // Progress chatter for a human at a terminal. Into a pipe (CI,
    // an orchestrator's nested call) a cached tool says nothing; a
    // real download still reports itself below.
    let chatty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    if chatty {
        eprintln!("soldr: fetching {crate_name}...");
    }
    // soldr#1264 follow-on: maturin gets a provisioning ladder
    // instead of the bare fetch — prebuilt binary from GitHub
    // Releases first, manual uv-provisioned isolated env as
    // the fallback (SOLDR_MATURIN_PROVISIONER=auto|binary|uv).
    // Everything else keeps the plain fetch_tool path.
    let result = if crate_name == "maturin" {
        fetch_maturin_with_provisioner(version).await?
    } else {
        crate::fetch::fetch_tool(crate_name, version).await?
    };

    if result.cached {
        if chatty {
            eprintln!("soldr: using cached {crate_name} v{}", result.version);
        }
    } else {
        eprintln!("soldr: downloaded {crate_name} v{}", result.version);
    }

    let normalized_tool_args;
    let tool_args = if crate_name == "maturin" {
        normalized_tool_args = crate::pyo3_detect::normalize_explicit_target_args(tool_args);
        normalized_tool_args.as_slice()
    } else {
        tool_args
    };
    let mut final_tool_args = tool_args.to_vec();
    let mut command = std::process::Command::new(&result.binary_path);
    let maturin = if crate_name == "maturin" {
        Some(
            prepare_maturin_child(&mut command, tool_args, &mut final_tool_args, cache_enabled)
                .await?,
        )
    } else {
        None
    };
    command.args(&final_tool_args);

    // Issue #493: when the user runs `soldr <external-tool>`,
    // install a transient PATH shim so any nested `cargo` /
    // `rustc` / `rustdoc` / `rustfmt` / `clippy-driver` spawned
    // by the tool routes back through soldr (and therefore
    // zccache and the managed toolchain home). The guard's
    // Drop removes the shim dir after the child exits.
    let _shim_guard = install_child_shim_dir(&mut command);

    suppress_windows_console_window(&mut command);
    let build_started = std::time::SystemTime::now(); // soldr#3433
                                                      // soldr#2024: the child's output explains this exit, inherited
                                                      // or teed back via `emit_child_output`.
    exit_guard::mark_spoke();
    let pep517 = maturin.as_ref().and_then(|child| child.pep517.as_ref());
    let status = wait_for_tool_child(&mut command, pep517)?;

    let stamp_dir = maturin
        .as_ref()
        .and_then(|child| child.stamp_dir.as_deref());
    let code = maybe_stage_bundle_bins(status.code().unwrap_or(1), stamp_dir, build_started);
    maybe_stamp_wheel(code, stamp_dir, build_started);
    if code != 0 {
        // soldr#1878: cargo surfaces a bare `Caused by:` with nothing
        // in it when the wrapped rustc dies without diagnostics. Say
        // which tool actually failed and where the full output went,
        // so the failure is never attributable to soldr by omission.
        eprintln!("soldr: {crate_name} exited {code}.");
    }
    // `process::exit` skips destructors, so anything still sitting in
    // a buffered stdout/stderr would be dropped here (soldr#1878).
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    guarded_exit(code);
}

/// What a maturin child keeps alive while it runs, and what its exit is
/// reported against.
struct MaturinChild {
    /// Held across the complete direct/PEP517 maturin child. This is
    /// separate from the short-lived stats session request: the
    /// OS-held lease is what prevents daemon GC from deleting a reused
    /// PEP517 target or wheel namespace while maturin is using it.
    _build_lease: Option<crate::cache_lib::build_active::BuildActivityLease>,
    /// soldr#3433: where the built wheel lands, for stamping.
    stamp_dir: Option<std::path::PathBuf>,
    /// Set for a `maturin build`: the PEP 517 linker policy applied.
    pep517: Option<Pep517Build>,
}

/// The PEP 517 linker policy a maturin build child runs under.
struct Pep517Build {
    linker: crate::linker::Pep517LinkerState,
    paths: SoldrPaths,
}

/// soldr#1264: `soldr maturin ...` is the engine behind the PEP
/// 517 build backend (src/soldr/__init__.py). maturin spawns
/// `cargo` itself, and on Windows the #493 `.cmd` PATH shims
/// below are invisible to Rust-spawned children (CreateProcess
/// resolves only `cargo.exe`, never `.cmd`), so on a
/// PATH-poisoned machine (e.g. a chocolatey GNU cargo ahead of
/// rustup's proxies) maturin silently builds the wrong toolchain
/// and cmake-based *-sys deps explode in "MSYS Makefiles" flag
/// mangling. Pin the child's toolchain + build tools before exec:
///   * `CARGO` -> soldr's resolved rustup cargo (honors
///     rust-toolchain.toml + MSVC-on-Windows); maturin reads it
///     before falling back to bare PATH lookup (caller-provided
///     CARGO always wins).
///   * `RUSTC_WRAPPER` -> soldr's current binary when caching is
///     enabled and unset, so `soldr maturin build` gets the same
///     embedded-zccache route as the PEP 517 backend (caller wins).
///   * managed cmake/ninja env (`CMAKE`, `CMAKE_GENERATOR=Ninja`,
///     PATH prepends) via the same `inject_cmake_tooling` the
///     blessed `soldr build` surface uses (#1257); same opt-outs.
async fn prepare_maturin_child(
    command: &mut std::process::Command,
    tool_args: &[String],
    final_tool_args: &mut Vec<String>,
    cache_enabled: bool,
) -> Result<MaturinChild, SoldrError> {
    pin_maturin_toolchain(command);
    // CARGO alone is not enough: resolve_toolchain_binary's
    // last-resort probe is a PATH lookup, and on the
    // poisoned-fixture machine a GNU-host rustup resolves
    // the pinned channel to its GNU variant. Force the
    // TARGET too — same runtime MSVC-default policy the
    // cargo front door applies via CARGO_BUILD_TARGET
    // (Windows-only; explicit user env always wins). Both
    // cargo and maturin honor CARGO_BUILD_TARGET, so even
    // a wrong-host cargo emits the right-target wheel.
    let paths = SoldrPaths::new()?;
    // The Python PEP 517 backend must select the same effective
    // product root as this binary (#1763): an explicit SOLDR_CACHE_DIR
    // here must not be dropped by the child's own default.
    command.env(crate::core::SOLDR_CACHE_DIR_ENV_VAR, &paths.root);
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let maturin_build = crate::pyo3_detect::maturin_args_are_build(tool_args);
    let stamp_dir = crate::wheel_stamp::wheel_dir_for_stamp(tool_args, &workspace_root);
    let build_lease = acquire_maturin_build_lease(&paths, tool_args)?;
    let maturin_target = crate::pyo3_detect::resolve_build_target(tool_args, &workspace_root);
    if maturin_build {
        apply_maturin_build_target(command, &paths, &maturin_target, final_tool_args).await?;
    }
    command.env(
        crate::cache_lib::CACHE_ENABLED_ENV_VAR,
        crate::cache_lib::cache_enabled_env_value(cache_enabled),
    );
    apply_maturin_wrapper_env(command, &paths, cache_enabled).await?;
    let mut prep = crate::blessed_build::BlessedPrep::default();
    crate::blessed_build::inject_cmake_tooling(&paths, &mut prep).await;
    // Mutate our own env (inherited by the child) so the
    // shim-dir PATH prepend below composes on top.
    for (k, v) in &prep.env {
        std::env::set_var(k, v);
    }
    for dir in &prep.path_dirs {
        prepend_to_path_env(dir);
    }

    let pep517 = if maturin_build {
        let paths = paths.clone();
        let linker = crate::linker::apply_pep517_override(command, &maturin_target, &paths).await?;
        if linker.cached_fallback {
            eprintln!(
                "soldr warning: fast linker `{}` was unavailable on the previous PEP 517 build; using the working standard linker",
                linker.candidate.as_deref().unwrap_or("unknown")
            );
        }
        let pep517 = Pep517Build { linker, paths };
        let mut pyo3_plan = crate::pyo3_detect::resolve_for_invocation(
            &workspace_root,
            tool_args,
            Some(&maturin_target),
        );
        pyo3_plan.materialize_compatibility(&pep517.paths).await?;
        pyo3_plan.emit_diagnostic();
        pyo3_plan.apply_to_command(command);
        Some(pep517)
    } else {
        None
    };
    Ok(MaturinChild {
        _build_lease: build_lease,
        stamp_dir,
        pep517,
    })
}

/// Pin the maturin child's `CARGO` (and the sibling `RUSTC`) to soldr's
/// resolved toolchain, unless the caller already set `CARGO`.
fn pin_maturin_toolchain(command: &mut std::process::Command) {
    if std::env::var_os("CARGO").is_some() {
        return;
    }
    match resolve_toolchain_binary("cargo") {
        Ok(cargo) => {
            // A direct (non-rustup-proxy) toolchain cargo spawns `rustc` from
            // PATH — on the poisoned-fixture machine that's the GNU standalone,
            // which lacks the msvc std and dies with E0463. Pin RUSTC to the
            // sibling rustc of the resolved cargo so cargo and rustc always come
            // from the same toolchain; fall back to the resolver when there is
            // no sibling.
            if std::env::var_os("RUSTC").is_none() {
                let sibling = cargo
                    .parent()
                    .map(|dir| dir.join(crate::platform::executable::name::native("rustc")));
                match sibling.filter(|p| p.is_file()) {
                    Some(rustc) => {
                        command.env("RUSTC", rustc);
                    }
                    None => match resolve_toolchain_binary("rustc") {
                        Ok(rustc) => {
                            command.env("RUSTC", rustc);
                        }
                        Err(err) => eprintln!(
                            "soldr warning: could not resolve \
                             toolchain rustc for maturin: {err}"
                        ),
                    },
                }
            }
            command.env("CARGO", &cargo);
        }
        Err(err) => eprintln!(
            "soldr warning: could not resolve toolchain cargo for \
             maturin; child falls back to PATH lookup: {err}"
        ),
    }
}

/// A `maturin build`'s target: the xwin policy, `CARGO_BUILD_TARGET`, and
/// the blessed target preparation.
async fn apply_maturin_build_target(
    command: &mut std::process::Command,
    paths: &SoldrPaths,
    maturin_target: &str,
    final_tool_args: &mut Vec<String>,
) -> Result<(), SoldrError> {
    let explicit_maturin = std::env::var_os(MATURIN_USE_XWIN_ENV_VAR).map(|_| "set");
    if let Some(policy) = maturin_xwin_policy(maturin_target, explicit_maturin) {
        command.env(MATURIN_USE_XWIN_ENV_VAR, policy);
    }
    let is_windows_host =
        crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows;
    if std::env::var_os("CARGO_BUILD_TARGET").is_none()
        && (is_windows_host || maturin_target != crate::pyo3_detect::host_triple())
    {
        command.env("CARGO_BUILD_TARGET", maturin_target);
    }

    // Target OS/SDK preparation is orthogonal to Python ABI
    // policy. Direct maturin and PEP 517 builds receive the same
    // blessed target preparation as `soldr build` before the
    // PyO3 plan decides whether any Python variables are valid.
    if crate::wheel_cmd::maturin_target_needs_prep(
        maturin_target,
        crate::pyo3_detect::host_triple(),
    ) {
        let target_prep =
            crate::target_lifecycle::prepare_for_invocation(paths, maturin_target, &[]).await?;
        crate::target_lifecycle::apply_to_process(&target_prep);
        // Maturin forwards Cargo's unstable --config option.
        // Preserve target-scoped build-script overrides just as
        // the direct cargo lifecycle does.
        crate::target_lifecycle::insert_args_before_separator(
            final_tool_args,
            target_prep.cargo_args,
        );
    }
    Ok(())
}

/// The maturin child's `RUSTC_WRAPPER` route: soldr's managed wrapper, or
/// the caller's wrapper plus the broker route it needs.
async fn apply_maturin_wrapper_env(
    command: &mut std::process::Command,
    paths: &SoldrPaths,
    cache_enabled: bool,
) -> Result<(), SoldrError> {
    // soldr#2545 pre-spawn sweep: the wrapper policy below keys
    // on the inherited `RUSTC_WRAPPER`; if a Soldr-owned pair
    // drifted upstream, propagating it into a maturin/tool child
    // would bake the drift into that build. Fail first.
    crate::wrapper_identity::assert_inherited_wrapper_coherent("tool dispatch")?;
    if std::env::var_os("RUSTC_WRAPPER").is_none() {
        if cache_enabled {
            let wrapper_plan = crate::zccache::prepare_rustc_wrapper_plan(paths).await?;
            wrapper_plan.apply_to_command(command)?;
        } else {
            command.env_remove("RUSTC_WRAPPER");
        }
        return Ok(());
    }
    if !cache_enabled {
        return Ok(());
    }
    crate::zccache::ZccacheChildEnv::from_current_process()?.apply_to_command(command);
    // soldr#2451: the caller (e.g. the PEP 517 backend, which
    // presets RUSTC_WRAPPER=soldr) owns the wrapper, so we must
    // not override it — but the cargo children it spawns still
    // re-enter soldr as that wrapper and resolve the broker
    // daemon route by SOLDR_BROKER_SERVICE. The managed-plan
    // branch above sets it; this caller-wrapper branch used to
    // skip it, leaving a wheel-consumer build with no way to
    // name the route (no sibling soldr-daemon beside the
    // wrapper) — the "cannot resolve the broker daemon route
    // (os error 2)" pep517-daemon-smoke failure. Register the
    // daemon image and pass the service name down explicitly.
    // The route is registered for *this* image's version, so
    // a bare `soldr` wrapper must also resolve to this image
    // rather than whichever soldr PATH finds first.
    if let (Some(inherited), Ok(exe)) = (std::env::var_os("RUSTC_WRAPPER"), std::env::current_exe())
    {
        if let Some(pinned) = crate::wrapper_identity::pin_bare_soldr_wrapper(&inherited, &exe) {
            command.env("RUSTC_WRAPPER", pinned);
        }
    }
    match crate::zccache::register_broker_daemon_service() {
        Ok((_daemon, service_name)) => {
            command.env(
                crate::daemon::backend_handle_adoption::SOLDR_BROKER_SERVICE_ENV_VAR,
                service_name,
            );
        }
        Err(err) => eprintln!(
            "soldr warning: could not register the broker daemon route for the \
             caller-provided RUSTC_WRAPPER; cacheable compiles may fail: {err}"
        ),
    }
    Ok(())
}

/// Issue #493: a transient PATH shim dir for the child, or `None` when
/// shims are disabled or could not be built.
fn install_child_shim_dir(command: &mut std::process::Command) -> Option<shim_dir::ShimDirGuard> {
    if !shim_dir::should_install_shims() {
        return None;
    }
    match shim_dir::build_shim_dir() {
        Ok(guard) => {
            shim_dir::apply_to_command(command, &guard.path);
            Some(guard)
        }
        Err(err) => {
            eprintln!(
                "soldr warning: failed to build child shim dir; \
                 nested cargo/rustc calls will bypass soldr: {err}"
            );
            None
        }
    }
}

/// Run the tool child to completion. A PEP 517 build under an automatic
/// fast linker retries once with the standard linker on a link failure.
fn wait_for_tool_child(
    command: &mut std::process::Command,
    pep517: Option<&Pep517Build>,
) -> Result<std::process::ExitStatus, SoldrError> {
    let Some(Pep517Build {
        linker: state,
        paths,
    }) = pep517
    else {
        return Ok(command.status()?);
    };
    if !(state.should_retry() || state.explicit_fast) {
        return Ok(command.status()?);
    }
    let first = command.output()?;
    emit_child_output(&first);
    if state.should_retry() && crate::linker::looks_like_linker_failure(&first) {
        eprintln!(
            "soldr warning: automatic fast linker `{}` failed; retrying once with the standard linker",
            state.candidate.as_deref().unwrap_or("unknown")
        );
        state.clear_injected_env(command);
        let fallback = command.output()?;
        emit_child_output(&fallback);
        crate::linker::report_fallback_outcome(
            &fallback,
            Some(paths),
            state.cache_key.as_deref(),
            state.candidate.as_deref().unwrap_or("unknown"),
        );
        return Ok(fallback.status);
    }
    if state.explicit_fast && crate::linker::looks_like_linker_failure(&first) {
        eprintln!(
            "soldr warning: explicitly requested SOLDR_LINKER=fast failed; no linker fallback was attempted"
        );
    }
    Ok(first.status)
}
