//! Regression for #4184: sanitized library builds retain the managed route.
use super::*;

#[test]
fn library_timeout_never_switches_to_an_uncached_retry() {
    let _lock = crate::TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _retry = crate::EnvVarGuard::remove(CARGO_TIMEOUT_RETRY_DISABLE_ENV_VAR);
    let _library = crate::EnvVarGuard::set(crate::dylint_library_cache::BUILDING_LIBRARIES, "1");
    let args = ["build".to_string(), "--release".to_string()];
    assert!(!cargo_timeout_retry_allowed(true, &args));
    let _ordinary = crate::EnvVarGuard::set(crate::dylint_library_cache::BUILDING_LIBRARIES, "0");
    assert!(cargo_timeout_retry_allowed(true, &args));
}

#[test]
fn dylint_library_child_keeps_wrapper_route_and_store_remap_after_sanitizing() {
    let _lock = crate::TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _native = crate::EnvVarGuard::set(crate::native_cc::NATIVE_CACHE_ENV_VAR, "0");
    let _library = crate::EnvVarGuard::set(crate::dylint_library_cache::BUILDING_LIBRARIES, "1");
    let tmp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(tmp.path().join("soldr"));
    let target = tmp.path().join("store/toolchain");
    std::fs::create_dir_all(&target).unwrap();
    let mut command = std::process::Command::new("fixture");
    command
        .args(["build", "--release", "--target-dir"])
        .arg(&target);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let wrapper = crate::zccache::RustcWrapperPlan::ManagedZccache(Box::new(
            crate::zccache::ManagedZccacheWrapperPlan {
                session: crate::build_cache_session::BuildCacheSession {
                    cache_dir: tmp.path().join("cache"),
                    session_id: "fixture".into(),
                    session_log_path: tmp.path().join("log"),
                    journal_path: tmp.path().join("journal"),
                    session_stats_path: tmp.path().join("stats"),
                },
                child_env: crate::zccache::ZccacheChildEnv {
                    path_remap: Some("auto"),
                    worktree_root: Some(tmp.path().join("checkout")),
                    materialization_mode: None,
                },
                wrapper_path: tmp.path().join("shims/rustc"),
                daemon_path: tmp.path().join("soldr-daemon"),
                broker_service_name: "soldr-daemon-library-fixture".into(),
            },
        ));
        let prefetch =
            cache_plan::CargoCachePlanPrefetch::Pending(tokio::spawn(async { Ok(wrapper) }));
        finalize_child_cache_plan(
            &mut command,
            prefetch,
            true,
            None,
            true,
            &paths,
            &mut crate::startup_profile::WrapperProfile::new(),
        )
        .await
        .unwrap();
    });
    // These are exactly the variables dylint_internal 6.2 sanitizes, plus
    // RUSTFLAGS removed by cargo-dylint before building metadata entries.
    for name in ["CARGO", "RUSTC", "RUSTUP_TOOLCHAIN", "RUSTFLAGS"] {
        command.env_remove(name);
    }
    let get = |key: &str| {
        command
            .get_envs()
            .find_map(|(name, value)| (name == key).then_some(value))
            .flatten()
    };
    let expected = paths
        .versioned_shims_dir()
        .join(format!("soldr-dylint{}", std::env::consts::EXE_SUFFIX));
    assert_eq!(get("RUSTC_WRAPPER"), Some(expected.as_os_str()));
    assert_eq!(
        get(crate::wrapper_identity::EFFECTIVE_WRAPPER_ENV),
        Some(expected.as_os_str())
    );
    assert_eq!(
        get("SOLDR_BROKER_SERVICE"),
        Some(std::ffi::OsStr::new("soldr-daemon-library-fixture"))
    );
    assert_eq!(get("SOLDR_CACHE_ENABLED"), Some(std::ffi::OsStr::new("1")));
    assert_eq!(get("ZCCACHE_WORKTREE_ROOT"), Some(target.as_os_str()));
}
