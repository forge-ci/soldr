pub(crate) async fn run_cargo_front_door(
    args: &[String],
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    let result = run_cargo_front_door_gated(args, cache_enabled, trust_inherited_soldr_env).await;
    emit_porcelain_catch_all(args, &result);
    result
}

/// Porcelain's last chance at the contract: exactly one JSON line must reach
/// the caller even when the run returned before the capture stage (a replayed
/// check, a front-door configuration error) or bailed out of the tail after
/// it. `porcelain::already_emitted` keeps this from doubling a report the run
/// already produced, so the invariant "exactly one line" holds on every path.
fn emit_porcelain_catch_all(args: &[String], result: &Result<i32, SoldrError>) {
    if !porcelain::enabled() || porcelain::already_emitted() {
        return;
    }
    let (exit_code, message) = match result {
        Ok(code) => (*code, None),
        Err(err) => (1, Some(err.to_string())),
    };
    let log = message.as_deref().and_then(|message| {
        let paths = crate::core::SoldrPaths::new().ok()?;
        porcelain::write_failure_log(&paths, &porcelain::command_string(args), message)
    });
    porcelain::emit(&porcelain::report_for_failure(
        &porcelain::command_string(args),
        exit_code,
        log,
        message.as_deref(),
    ));
}

async fn run_cargo_front_door_gated(
    args: &[String],
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    // soldr#3896: a pure check whose exact inputs already passed is replayed
    // before any setup; otherwise a pass is recorded for the next run.
    let armed = match crate::verdict::gate::open(args) {
        crate::verdict::gate::Gate::Replayed(code) => return Ok(code),
        crate::verdict::gate::Gate::Armed(armed) => Some(armed),
        crate::verdict::gate::Gate::Pass => None,
    };
    // soldr#3897: the gate may narrow a workspace run to changed members.
    let run_args = armed
        .as_ref()
        .and_then(|armed| armed.args_override())
        .unwrap_or(args)
        .to_vec();
    let result =
        run_cargo_front_door_ungated(&run_args, cache_enabled, trust_inherited_soldr_env).await;
    if let Some(armed) = armed {
        armed.finish(*result.as_ref().unwrap_or(&1));
    }
    result
}

async fn run_cargo_front_door_ungated(
    args: &[String],
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    // Time the front door (#1843); a warm no-op does not invoke a rustc
    // wrapper. Zero-cost unless SOLDR_PROFILE_STARTUP is set.
    let mut profile = crate::startup_profile::WrapperProfile::new();
    let setup = FrontDoorSetup::begin(args, cache_enabled, trust_inherited_soldr_env)?;
    let normalized = match normalize_front_door_args(args)? {
        FrontDoorArgs::Executed(code) => return Ok(code),
        FrontDoorArgs::Cargo(normalized) => normalized,
    };
    let mut prepared =
        prepare_child_cargo(&setup, &normalized, cache_enabled, &mut profile).await?;
    let mut session = FrontDoorBuildSession::begin(&mut prepared, &mut profile)?;
    // Everything above is pure soldr overhead the user pays before Cargo
    // starts. Emit the breakdown here so the total excludes Cargo itself.
    profile.finish_labeled("cargo front door", "pre_spawn_tail");
    crate::startup_trace::phase(crate::startup_trace::phase::CARGO_FRONT_DOOR_PRE_SPAWN);
    // soldr#3794: bracket the child so pre-cargo and post-cargo time can be split.
    let child_profile = crate::startup_profile::WrapperProfile::new();
    let cargo_run_result = run_child_cargo(&mut prepared, &setup);
    child_profile.finish_labeled("cargo child", "cargo_exit");
    // soldr#2302: cargo exited — drain + stop the per-unit tail before the tail
    // summary prints, on both the success and error paths.
    cache_states::stop_tail(session.cache_state_tail.take());
    // soldr#1843: BuildSessionStart must land before any BuildSessionEnd below.
    if let Some(publish) = session.publish.take() {
        let _ = crate::platform::wait::join_for(publish, crate::platform::wait::HELPER_JOIN);
    }
    match cargo_run_result {
        Ok(outcome) => complete_cargo_run(outcome, &prepared, session, &setup, normalized),
        Err(err) => handle_aborted_cargo_run(err, &prepared, session, &setup),
    }
}

/// What the front door resolves before it touches the arguments, and keeps
/// for the rest of the run.
struct FrontDoorSetup {
    cargo_wait_timeout: Option<Duration>,
    nested_cargo_mode: nested_cargo_guard::GuardMode,
    trust_inherited_soldr_env: bool,
    /// The stable-rustc fallback re-enters this front door with the
    /// caller-facing arguments captured here.
    zthreads_retry_context: ZthreadsRetryContext,
    _fresh_workspace_env: FreshSoldrWorkspaceEnvGuard,
    command_lifetime_shutdown_timeout: Option<Duration>,
}

impl FrontDoorSetup {
    fn begin(
        args: &[String],
        cache_enabled: bool,
        trust_inherited_soldr_env: bool,
    ) -> Result<Self, SoldrError> {
        if cargo_args_use_reserved_no_cache(args) {
            return Err(SoldrError::Other(
                "`--no-cache` must appear before `cargo`, as in `soldr --no-cache cargo build`"
                    .into(),
            ));
        }

        // Parse the opt-in watchdog before starting daemons, spawning Cargo, or
        // mutating build-session state. Malformed configuration is a user-facing
        // error, not a reason to launch a child that would need cleanup.
        let cargo_wait_timeout = cargo_wait_timeout()?;
        // soldr#2924: consumed before any Cargo spawns, so the permit spans one run.
        let nested_cargo_mode = nested_cargo_guard::consume_mode_env();
        crate::startup_trace::phase(crate::startup_trace::phase::CARGO_FRONT_DOOR_ENTERED);

        // soldr#2545 pre-spawn sweep: a front door nested inside a Soldr-owned
        // lineage (build scripts, tools re-invoking `soldr cargo`) must fail
        // here — before daemons start or cargo spawns — if the inherited
        // wrapper pair drifted, because cargo would fingerprint the changed
        // wrapper and silently recompile the world.
        crate::wrapper_identity::assert_inherited_wrapper_coherent("cargo front door")?;

        // soldr#3518: restore a killed cook's journaled sources before Cargo reads them.
        crate::cook_source_journal::recover_stale_cook_journals(&std::env::current_dir()?)?;

        // The stable-rustc fallback re-enters this front door. Snapshot the caller-facing
        // contract before toolchain directives and Soldr-private Cargo flags are
        // normalized so the retry performs the same processing exactly once.
        let zthreads_retry_context =
            ZthreadsRetryContext::new(args, cache_enabled, trust_inherited_soldr_env);
        let fresh_workspace_env =
            FreshSoldrWorkspaceEnvGuard::apply_unless_trusted(trust_inherited_soldr_env);

        let cache_lifecycle = cache_lifecycle_from_env()?;
        let command_lifetime_shutdown_timeout = if cache_lifecycle == CacheLifecycle::Command {
            Some(command_lifetime_shutdown_timeout()?)
        } else {
            None
        };
        Ok(Self {
            cargo_wait_timeout,
            nested_cargo_mode,
            trust_inherited_soldr_env,
            zthreads_retry_context,
            _fresh_workspace_env: fresh_workspace_env,
            command_lifetime_shutdown_timeout,
        })
    }
}

/// The cargo arguments after Soldr's private flags and toolchain directive
/// are stripped.
struct NormalizedArgs {
    args: Vec<String>,
    explicit_toolchain: Option<String>,
    dylint_dependency_cook: bool,
    trampoline_plan: Option<Box<FellThroughPlan>>,
}

enum FrontDoorArgs {
    /// The `cargo run` trampoline ran the binary; exit with its code.
    Executed(i32),
    Cargo(NormalizedArgs),
}

fn normalize_front_door_args(args: &[String]) -> Result<FrontDoorArgs, SoldrError> {
    // Retain the old target-GC flags as stripped compatibility no-ops.
    let (args_without_dylint_cook_flag, dylint_dependency_cook) =
        strip_dylint_dependency_cook_flag(args);
    let args_owned = strip_no_gc_target_flags(&args_without_dylint_cook_flag);
    let (args_owned, explicit_toolchain) = subcommand::strip_cargo_toolchain_directive(&args_owned);

    // `cargo run` trampoline (issue #344). When the binary is already
    // up-to-date with the recorded sources, this exec's the binary
    // directly and never spawns cargo. Otherwise we get back a plan that
    // strips the soldr-private `--no-trampoline` flag from the arg list
    // and lets us refresh the sidecar after cargo succeeds.
    let trampoline_plan = if subcommand::is_cargo_run_invocation(&args_owned) {
        match try_run_trampoline(&args_owned)? {
            TrampolineDecision::Executed(code) => return Ok(FrontDoorArgs::Executed(code)),
            TrampolineDecision::FellThrough(plan) => Some(plan),
        }
    } else {
        None
    };

    // Workspace build/check/clippy freshness belongs to Cargo. The retired
    // sidecar path did not model Cargo's complete semantic identity and could
    // return false Fresh results (#1528). Keep accepting the historical
    // soldr-only opt-out flag as argument-cleanup compatibility, but always
    // invoke Cargo for these verbs.
    let workspace_args = matches!(
        first_cargo_subcommand(&args_owned),
        Some("build" | "b" | "check" | "c" | "clippy")
    )
    .then(|| strip_no_trampoline_flag(&args_owned).0);

    // Use the cleaned arg vector from here on so `--no-trampoline` is
    // not forwarded to cargo.
    let args = match (trampoline_plan.as_ref(), workspace_args) {
        (Some(plan), _) => plan.cleaned_args.clone(),
        (None, Some(cleaned)) => cleaned,
        (None, None) => args_owned,
    };
    Ok(FrontDoorArgs::Cargo(NormalizedArgs {
        args,
        explicit_toolchain,
        dylint_dependency_cook,
        trampoline_plan,
    }))
}

/// soldr#2334: hint (never fail) when a foreign `--target` goes through
/// the verbatim cargo passthrough with no routed target C toolchain.
///
/// Fires only when every condition holds:
/// - the subcommand compiles (`build`/`b`/`test`/`t`/`bench`/`run`/`r`),
/// - an explicit `--target` names a triple that is not the host,
/// - no target-scoped C compiler is in scope: neither the blessed prep
///   (which exports `CC_<triple>` into the process before the front door
///   runs) nor a caller-managed override.
fn maybe_hint_foreign_target_passthrough(args: &[String]) {
    let compiles = matches!(
        first_cargo_subcommand(args),
        Some("build" | "b" | "test" | "t" | "bench" | "run" | "r")
    );
    if !compiles {
        return;
    }
    let Some(triple) = subcommand::cargo_args_target(args) else {
        return;
    };
    if !foreign_target_passthrough_needs_hint(triple, crate::pyo3_detect::host_triple(), |key| {
        std::env::var_os(key).is_some()
    }) {
        return;
    }
    eprintln!(
        "soldr: note: `--target {triple}` through the bare cargo passthrough uses \
         whatever C toolchain cargo finds on this host, so cc-built dependencies \
         may compile as host objects and fail the final link (soldr#2334). The \
         blessed cross route is `soldr build --target {triple}`, which manages \
         the target C toolchain and sysroot."
    );
}

/// Pure decision core for the soldr#2334 hint, so the discrimination
/// matrix (host-native, blessed-prep, caller-managed, bare passthrough)
/// is unit-testable without process env.
fn foreign_target_passthrough_needs_hint(
    triple: &str,
    host_triple: &str,
    env_present: impl Fn(&str) -> bool,
) -> bool {
    if triple == host_triple {
        return false;
    }
    // Only hint for target families soldr actually manages a C toolchain
    // for — an exotic triple gets whatever cargo does today, unhinted.
    let managed_family = triple.ends_with("-unknown-linux-gnu")
        || triple.ends_with("-unknown-linux-musl")
        || triple.ends_with("-pc-windows-gnu")
        || triple.ends_with("-apple-darwin");
    if !managed_family {
        return false;
    }
    let suffix = triple.replace('-', "_");
    // Blessed prep exports CC_<triple>; callers doing it by hand set the
    // same var. Either way a routed toolchain is in scope: stay quiet.
    !env_present(&format!("CC_{suffix}"))
}

#[cfg(test)]
mod foreign_target_hint_tests {
    use super::foreign_target_passthrough_needs_hint as needs_hint;

    #[test]
    fn hint_matrix() {
        let host = "x86_64-unknown-linux-gnu";
        let none = |_: &str| false;
        // Foreign managed family, nothing routed: hint.
        assert!(needs_hint("x86_64-pc-windows-gnu", host, none));
        assert!(needs_hint("aarch64-unknown-linux-gnu", host, none));
        // Host-native: never.
        assert!(!needs_hint(host, host, none));
        // Unmanaged family: never.
        assert!(!needs_hint("wasm32-unknown-unknown", host, none));
        assert!(!needs_hint("x86_64-pc-windows-msvc", host, none));
        // Routed toolchain in scope (blessed prep or caller): quiet.
        let routed = |key: &str| key == "CC_x86_64_pc_windows_gnu";
        assert!(!needs_hint("x86_64-pc-windows-gnu", host, routed));
    }
}
