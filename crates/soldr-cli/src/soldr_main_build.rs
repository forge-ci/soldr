async fn run_blessed_build(
    args: Vec<String>,
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Result<(), SoldrError> {
    if let Some(code) = crate::remote_build::auto::try_build(
        &args,
        cache_enabled,
        crate::remote_build::auto::BuildMode::Blessed,
    )
    .await? {
        guarded_exit(code);
    }

    // soldr#1012: prepare a recognized `--target X` / `--target=X`
    // through the blessed catalogue before entering the cargo front
    // door. This materializes the required SDK/sysroot and compiler
    // shims, then applies their target-scoped environment.
    let mut full_args = Vec::with_capacity(args.len() + 1);
    full_args.push("build".to_string());
    full_args.extend(args);
    target_alias::normalize_target_aliases_in_args(&mut full_args);

    // soldr#2319 (Approach C): the catalogue Linux GNU compiler is an ELF
    // binary, so a Windows host cannot execute it. Delegate this supported
    // Windows-host × Linux-GNU surface to Linux Docker before any native
    // toolchain preparation attempts to run that compiler.
    if let Some(target_triple) = extract_target_from_args(&full_args) {
        if crate::docker_cross::should_delegate_to_docker(
            crate::docker_cross::Host::current(),
            &target_triple,
            &crate::docker_cross::EnvSnapshot::from_process(),
        ) {
            guarded_exit(crate::docker_cross::run(&target_triple, &full_args).await?);
        }
    }

    // Try to recognize a target from argv so we can prep
    // before invoking cargo. If the user didn't pass `--target`,
    // only the host-side prep (managed cmake/ninja) runs and we
    // forward otherwise unchanged.
    if let Some(target_triple) = extract_target_from_args(&full_args) {
        let paths = crate::core::SoldrPaths::new()?;
        // soldr#2618: settle all rustup work (pinned toolchain install +
        // target add) BEFORE the prefetch spawns. The prefetch's cargo is
        // a rustup proxy on a fresh root, and letting it auto-install the
        // toolchain concurrently with prep's `rustup target add` produced
        // the `Missing manifest in toolchain` race — worse, the resulting
        // prep error reaped the prefetch mid-extraction (kill_on_drop),
        // leaving a permanently wedged partial toolchain.
        crate::target_lifecycle::provision_target_toolchain(&target_triple)?;
        // soldr#1543: start a bounded `cargo fetch
        // --target <T>` NOW so dependency acquisition overlaps
        // the catalogue/SDK materialization below. Joined
        // right after prep (before the front door spawns
        // cargo), so the two cargos never race; on a prep
        // error the child is reaped via kill_on_drop. Fetch
        // failures are logged + ignored — the main build owns
        // real dependency errors.
        let dep_prefetch =
            crate::fetch_overlap::spawn_for_blessed_build(&full_args, &target_triple);
        let feature_args = crate::target_lifecycle::cargo_feature_flags(&full_args);
        let prep = crate::target_lifecycle::prepare_for_invocation(
            &paths,
            &target_triple,
            &feature_args,
        )
        .await?;
        let cargo_args = prep.cargo_args.clone();
        crate::target_lifecycle::apply_to_process(&prep);

        // soldr#2519: every cross target now builds with plain `cargo build`
        // against the blessed sysroot. The old soldr#882 hop that rewrote argv
        // into `cargo xwin build` / `cargo zigbuild` only ever fired when
        // `SOLDR_USE_LEGACY_{XWIN,ZIGBUILD}` was set, and those toggles are
        // gone -- they routed around soldr's pinned, sha256-verified SDK to an
        // unpinned third-party download. `target_lifecycle::prepare_for_invocation`
        // above already failed loudly if the blessed SDK could not be
        // materialized, so there is nothing left to fall back to.
        full_args = insert_cargo_config_args(full_args, &cargo_args);

        // Join the overlapped dependency prefetch before the
        // main cargo build spawns (soldr#1543).
        if let Some(dep_prefetch) = dep_prefetch {
            dep_prefetch.join().await;
        }
    } else {
        // Native host build (no --target): the cross-compile
        // sysroot prep doesn't apply, but the managed cmake +
        // ninja injection does — cmake-based *-sys build
        // scripts run on the host regardless of target, and
        // "use whatever cmake/make PATH serves" is exactly the
        // failure mode soldr exists to remove (a pip-installed
        // MSYS make + "MSYS Makefiles" generator broke native
        // libz-ng-sys builds — see fetch::cmake_tools).
        let paths = crate::core::SoldrPaths::new()?;
        let mut prep = crate::blessed_build::BlessedPrep::default();
        crate::blessed_build::inject_cmake_tooling(&paths, &mut prep).await;
        for (k, v) in &prep.env {
            std::env::set_var(k, v);
        }
        for dir in &prep.path_dirs {
            prepend_to_path_env(dir);
        }
    }

    // soldr#1079: ensure native Windows MSVC builds get LIB /
    // INCLUDE / PATH (link.exe) injected from the host VS
    // install, so users invoking `soldr build` from a plain
    // PowerShell don't have to set `$env:LIB` themselves.
    // No-op when not on Windows, when the user opted out via
    // `SOLDR_MSVC_DISCOVERY=off`, when LIB is already set, or
    // when the resolved target is non-MSVC.
    ensure_msvc_host_env_for_native(&full_args).await;

    guarded_exit(
        cargo_front_door::run_cargo_front_door(
            &full_args,
            cache_enabled,
            trust_inherited_soldr_env,
        )
        .await?,
    );
}
