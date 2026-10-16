use super::*;
use std::ffi::OsStr;

#[test]
fn rustc_wrapper_override_defaults_to_managed_zccache() {
    assert_eq!(
        rustc_wrapper_mode_from_env_var(None),
        RustcWrapperMode::ManagedZccache
    );
}

#[test]
fn rustc_wrapper_override_disables_wrapper_for_empty_or_none() {
    for value in ["", " ", "none", "NONE"] {
        assert_eq!(
            rustc_wrapper_mode_from_env_var(Some(OsStr::new(value))),
            RustcWrapperMode::Disabled,
            "expected {value:?} to disable wrapper injection"
        );
    }
}

#[test]
fn rustc_wrapper_override_uses_custom_wrapper_name() {
    assert_eq!(
        rustc_wrapper_mode_from_env_var(Some(OsStr::new("sccache"))),
        RustcWrapperMode::Custom("sccache".into())
    );
}

#[test]
fn current_soldr_override_is_recognized_as_the_embedded_cache_front_door() {
    let current = std::env::current_exe().expect("current test executable");
    assert!(custom_wrapper_is_current_soldr(current.as_os_str()));
}

#[test]
fn unrelated_custom_wrapper_is_not_recognized_as_soldr() {
    let wrapper = unique_test_dir("custom-wrapper").join("soldr");
    std::fs::write(&wrapper, b"not the running executable").expect("fake custom wrapper");
    assert!(!custom_wrapper_is_current_soldr(wrapper.as_os_str()));
}

#[test]
fn sccache_wrapper_detection_accepts_binary_names_and_paths() {
    assert!(is_sccache_wrapper(OsStr::new("sccache")));
    assert!(is_sccache_wrapper(OsStr::new("sccache.exe")));
    assert!(is_sccache_wrapper(OsStr::new("/tmp/tools/sccache")));
    assert!(!is_sccache_wrapper(OsStr::new("zccache")));
    assert!(!is_sccache_wrapper(OsStr::new("sccache-proxy")));
}

// Parent-cache L1.x env injection (issue #352). The decision function
// takes the inherited values of `ZCCACHE_PATH_REMAP` (set by the user)
// and `SOLDR_PATH_REMAP` (soldr-side escape hatch) and decides whether
// soldr should inject `ZCCACHE_PATH_REMAP=auto` onto the spawned cargo
// child. None means do not inject; Some(value) means inject that value.
//
// Rules:
//   1. If the user already set ZCCACHE_PATH_REMAP, do not override.
//   2. Otherwise read SOLDR_PATH_REMAP (default `auto`). `off`
//      (case-insensitive) suppresses the injection. Anything else, or
//      unset, injects `auto`.

#[test]
fn path_remap_injects_auto_when_nothing_set() {
    assert_eq!(resolve_path_remap_env(None, None), Some("auto"));
}

#[test]
fn path_remap_skips_when_soldr_override_is_off() {
    assert_eq!(resolve_path_remap_env(None, Some("off")), None);
}

#[test]
fn path_remap_skips_when_soldr_override_is_off_case_insensitive() {
    assert_eq!(resolve_path_remap_env(None, Some("OFF")), None);
    assert_eq!(resolve_path_remap_env(None, Some("Off")), None);
    assert_eq!(resolve_path_remap_env(None, Some(" off ")), None);
}

#[test]
fn path_remap_injects_auto_when_soldr_override_is_auto() {
    assert_eq!(resolve_path_remap_env(None, Some("auto")), Some("auto"));
    assert_eq!(resolve_path_remap_env(None, Some("AUTO")), Some("auto"));
}

#[test]
fn path_remap_preserves_user_value_when_zccache_already_set_to_non_auto() {
    assert_eq!(resolve_path_remap_env(Some("disabled"), None), None);
    assert_eq!(resolve_path_remap_env(Some("disabled"), Some("auto")), None);
}

#[test]
fn path_remap_treats_empty_user_value_as_unset() {
    assert_eq!(resolve_path_remap_env(Some(""), None), Some("auto"));
    assert_eq!(resolve_path_remap_env(Some("   "), None), Some("auto"));
    assert_eq!(resolve_path_remap_env(Some(""), Some("off")), None);
}

#[test]
fn path_remap_preserves_user_value_when_zccache_already_auto() {
    // User explicitly set `auto` — soldr must not double-inject. The
    // decision function returns None because the env is already correct
    // in the inherited environment.
    assert_eq!(resolve_path_remap_env(Some("auto"), None), None);
    assert_eq!(resolve_path_remap_env(Some("auto"), Some("off")), None);
}

#[test]
fn path_remap_auto_active_tracks_child_state() {
    assert!(path_remap_auto_active(None, None));
    assert!(path_remap_auto_active(Some(""), None));
    assert!(path_remap_auto_active(Some("auto"), Some("off")));
    assert!(!path_remap_auto_active(None, Some("off")));
    assert!(!path_remap_auto_active(Some("disabled"), None));
}

#[test]
fn worktree_root_env_uses_git_root_by_default() {
    let temp = unique_test_dir("worktree-root-git");
    let root = temp.join("repo");
    let nested = root.join("crates").join("demo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(&nested).unwrap();

    assert_eq!(find_git_worktree_root(&nested), Some(root.clone()));
    assert_eq!(resolve_worktree_root_env(None, &nested), Some(root));
}

#[test]
fn worktree_root_env_falls_back_to_cwd_without_git_root() {
    let temp = unique_test_dir("worktree-root-cwd");
    let cwd = temp.join("repo");
    std::fs::create_dir_all(&cwd).unwrap();

    assert_eq!(find_git_worktree_root(&cwd), None);
    assert_eq!(resolve_worktree_root_env(None, &cwd), Some(cwd));
}

// ---------------------------------------------------------------
// zccache cache-hit delivery mode (soldr#3407, zccache#1683).
// ---------------------------------------------------------------

fn child_env_with_mode(
    mode: Option<crate::core::materialization_mode::ResolvedZccacheMode>,
    probed: crate::core::materialization_mode::ZccacheMode,
) -> ZccacheChildEnv {
    ZccacheChildEnv::from_inputs(
        Some("off"),
        None,
        None,
        std::path::Path::new("/repo"),
        mode,
        move || probed,
    )
}

fn injected_mode(env: &ZccacheChildEnv) -> Option<Option<String>> {
    let mut command = std::process::Command::new("cargo");
    env.apply_to_command(&mut command);
    command
        .get_envs()
        .find(|(key, _)| *key == OsStr::new("ZCCACHE_MODE"))
        .map(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()))
}

#[test]
fn soldr_owned_non_auto_mode_is_injected_in_canonical_spelling() {
    use crate::core::materialization_mode::{ResolvedZccacheMode, ZccacheMode, ZccacheModeSource};
    for (mode, source) in [
        (ZccacheMode::Copy, ZccacheModeSource::SoldrEnv),
        (ZccacheMode::Reflink, ZccacheModeSource::Config),
        (
            ZccacheMode::ReflinkOrLinkOrCopy,
            ZccacheModeSource::SoldrEnv,
        ),
    ] {
        // Probe result is irrelevant (and must not be consulted) for an
        // explicit non-AUTO mode; pin it to something else to prove it.
        let env = child_env_with_mode(
            Some(ResolvedZccacheMode { mode, source }),
            ZccacheMode::Copy,
        );
        assert_eq!(env.materialization_mode, Some(mode.as_str()));
        assert_eq!(
            injected_mode(&env),
            Some(Some(mode.as_str().to_string())),
            "{source:?} must reach the cargo child as ZCCACHE_MODE"
        );
    }
}

#[test]
fn the_users_own_non_auto_zccache_mode_is_left_alone() {
    use crate::core::materialization_mode::{ResolvedZccacheMode, ZccacheMode, ZccacheModeSource};
    for mode in [
        ZccacheMode::Link,
        ZccacheMode::Copy,
        ZccacheMode::Reflink,
        ZccacheMode::ReflinkOrLinkOrCopy,
    ] {
        let env = child_env_with_mode(
            Some(ResolvedZccacheMode {
                mode,
                source: ZccacheModeSource::ZccacheEnv,
            }),
            ZccacheMode::Copy,
        );
        assert_eq!(env.materialization_mode, None, "{mode:?}");
        assert_eq!(
            injected_mode(&env),
            None,
            "{mode:?}: never rewrite the user's value"
        );
    }
}

// ---------------------------------------------------------------
// AUTO / unset: soldr probes once and injects the explicit answer
// (soldr#3440, zccache#1792) instead of the chain zccache's own AUTO no
// longer runs.
// ---------------------------------------------------------------

#[test]
fn auto_from_any_tier_is_injected_as_the_probed_mode() {
    use crate::core::materialization_mode::{ResolvedZccacheMode, ZccacheMode, ZccacheModeSource};
    for source in [
        ZccacheModeSource::SoldrEnv,
        ZccacheModeSource::Config,
        ZccacheModeSource::ZccacheEnv,
    ] {
        let auto = Some(ResolvedZccacheMode {
            mode: ZccacheMode::Auto,
            source,
        });
        for probed in [ZccacheMode::Reflink, ZccacheMode::Link, ZccacheMode::Copy] {
            let env = child_env_with_mode(auto, probed);
            assert_eq!(
                injected_mode(&env),
                Some(Some(probed.as_str().to_string())),
                "{source:?} probed={probed:?}"
            );
        }
    }
}

#[test]
fn no_mode_configured_is_also_injected_as_the_probed_mode() {
    use crate::core::materialization_mode::ZccacheMode;
    for probed in [ZccacheMode::Reflink, ZccacheMode::Link, ZccacheMode::Copy] {
        let env = child_env_with_mode(None, probed);
        assert_eq!(env.materialization_mode, Some(probed.as_str()));
        assert_eq!(injected_mode(&env), Some(Some(probed.as_str().to_string())));
    }
}

#[test]
fn probe_is_not_invoked_when_an_explicit_non_auto_mode_wins() {
    use crate::core::materialization_mode::{ResolvedZccacheMode, ZccacheMode, ZccacheModeSource};
    for (mode, source) in [
        (ZccacheMode::Link, ZccacheModeSource::SoldrEnv),
        (ZccacheMode::Copy, ZccacheModeSource::ZccacheEnv),
        (ZccacheMode::Reflink, ZccacheModeSource::Config),
        (
            ZccacheMode::ReflinkOrLinkOrCopy,
            ZccacheModeSource::SoldrEnv,
        ),
    ] {
        let mut probed = false;
        let env = ZccacheChildEnv::from_inputs(
            Some("off"),
            None,
            None,
            std::path::Path::new("/repo"),
            Some(ResolvedZccacheMode { mode, source }),
            || {
                probed = true;
                ZccacheMode::Reflink
            },
        );
        let _ = env.materialization_mode;
        assert!(
            !probed,
            "probe must not run for explicit {mode:?} ({source:?})"
        );
    }
}

#[test]
fn delivery_capability_conversion_matches_zccache_mode() {
    use crate::core::materialization_mode::ZccacheMode;
    use crate::platform::fs::delivery_probe::DeliveryCapability;
    assert_eq!(
        delivery_capability_to_mode(DeliveryCapability::Reflink),
        ZccacheMode::Reflink
    );
    assert_eq!(
        delivery_capability_to_mode(DeliveryCapability::Link),
        ZccacheMode::Link
    );
    assert_eq!(
        delivery_capability_to_mode(DeliveryCapability::Copy),
        ZccacheMode::Copy
    );
}

#[test]
fn worktree_root_env_preserves_user_value() {
    let cwd = std::path::Path::new("/repo");
    assert_eq!(
        resolve_worktree_root_env(Some(OsStr::new("/custom/root")), cwd),
        None
    );
}

// ---------------------------------------------------------------
// Private-session opt-in (`SOLDR_ZCCACHE_PRIVATE`). Routes session-local
// auxiliary state to `<cwd>/.zccache`; it does not relocate the embedded
// daemon's Rust artifact store.
// ---------------------------------------------------------------

#[test]
fn private_session_flag_truthy_values() {
    for v in [
        "1", "true", "yes", "on", "TRUE", "Yes", "ON", " 1 ", " true ",
    ] {
        assert!(
            parse_private_session_flag(Some(v)),
            "expected {v:?} to parse truthy",
        );
    }
}

#[test]
fn private_session_flag_falsy_values() {
    for v in [
        "0", "false", "no", "off", "FALSE", "No", "OFF", "", "   ", "maybe", "2",
    ] {
        assert!(
            !parse_private_session_flag(Some(v)),
            "expected {v:?} to parse falsy",
        );
    }
    assert!(
        !parse_private_session_flag(None),
        "unset env should be falsy",
    );
}

#[test]
fn private_session_cache_dir_is_dot_zccache_under_cwd() {
    let cwd = std::env::current_dir().expect("cwd");
    let resolved = private_session_cache_dir().expect("private dir");
    assert_eq!(resolved, cwd.join(PRIVATE_SESSION_CACHE_DIR_NAME));
    assert!(
        resolved.is_absolute(),
        "private session cache dir must be absolute: {}",
        resolved.display(),
    );
    assert_eq!(
        resolved.file_name().and_then(|s| s.to_str()),
        Some(".zccache"),
        "private session cache dir tail must be `.zccache`",
    );
}

fn unique_test_dir(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
#[test]
fn cache_lifecycle_defaults_to_job_long_cache() {
    assert_eq!(
        cache_lifecycle_from_env_value(None).unwrap(),
        CacheLifecycle::Job
    );
    assert_eq!(
        cache_lifecycle_from_env_value(Some(OsStr::new(""))).unwrap(),
        CacheLifecycle::Job
    );
    assert_eq!(
        cache_lifecycle_from_env_value(Some(OsStr::new("job"))).unwrap(),
        CacheLifecycle::Job
    );
}

#[test]
fn cache_lifecycle_accepts_command_lifetime_aliases() {
    for value in ["command", "COMMAND", "command-lifetime", "self-build"] {
        assert_eq!(
            cache_lifecycle_from_env_value(Some(OsStr::new(value))).unwrap(),
            CacheLifecycle::Command,
            "expected {value:?} to enable command-lifetime cache shutdown"
        );
    }
}

#[test]
fn cache_lifecycle_rejects_unknown_values() {
    let err = cache_lifecycle_from_env_value(Some(OsStr::new("forever"))).unwrap_err();
    assert!(
        err.to_string().contains(SOLDR_CACHE_LIFECYCLE_ENV_VAR),
        "expected env var name in error: {err}"
    );
}

#[test]
fn command_lifetime_shutdown_timeout_parser_defaults_and_validates() {
    assert_eq!(parse_shutdown_timeout_seconds("").unwrap(), 300);
    assert_eq!(parse_shutdown_timeout_seconds(" 5 ").unwrap(), 5);
    // #3647: malformed/zero overrides fall back to the 300 s default (docs/DAEMON_TIMEOUTS.md), matching SOLDR_COMPILE_REPLY_TIMEOUT_SECS.
    assert_eq!(parse_shutdown_timeout_seconds("0").unwrap(), 300);
    assert_eq!(parse_shutdown_timeout_seconds("abc").unwrap(), 300);
    assert_eq!(parse_shutdown_timeout_seconds("5m").unwrap(), 300);
    assert_eq!(parse_shutdown_timeout_seconds("-1").unwrap(), 300);
}

#[test]
fn prewarm_gating_table() {
    use PrewarmPlanKind::*;
    for (kind, optout, want) in [
        (Managed, None, true),
        (Managed, Some("1"), true),
        (Managed, Some("garbage"), true),
        (Managed, Some("0"), false),
        (Managed, Some("false"), false),
        (Managed, Some("off"), false),
        (Custom, None, false),
        (Disabled, None, false),
        (NoCache, None, false),
    ] {
        assert_eq!(should_prewarm(kind, optout), want, "{kind:?} {optout:?}");
    }
}

#[test]
fn prewarm_error_is_swallowed_and_hook_runs_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    fn failing(_: &str) -> std::io::Result<()> {
        CALLS.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::other("boom"))
    }
    let handle = spawn_route_prewarm("svc", failing).expect("spawn");
    handle
        .join()
        .expect("a failing pre-warm must not panic the thread");
    assert_eq!(CALLS.load(Ordering::SeqCst), 1);
}
