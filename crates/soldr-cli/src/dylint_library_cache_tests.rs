use super::*;

#[test]
fn compile_requests_choose_source_for_lints_and_target_for_registry_dependencies() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("checkout");
    let target = tmp.path().join("store/channel");
    for (manifest, expected, building, remap) in [
        (source.join("dylints/one"), source.clone(), "1", "auto"),
        (tmp.path().join("registry/dep"), target.clone(), "1", "auto"),
        (
            tmp.path().join("checkout-other/lint"),
            target.clone(),
            "1",
            "auto",
        ),
        (source.join("dylints/one"), target.clone(), "0", "auto"),
        (source.join("dylints/one"), target.clone(), "1", "off"),
    ] {
        let mut env = vec![
            SessionEnvVar {
                key: BUILDING_LIBRARIES.into(),
                value: building.into(),
            },
            SessionEnvVar {
                key: SOURCE_ROOT.into(),
                value: source.to_string_lossy().into_owned(),
            },
            SessionEnvVar {
                key: "CARGO_MANIFEST_DIR".into(),
                value: manifest.to_string_lossy().into_owned(),
            },
            SessionEnvVar {
                key: "ZCCACHE_PATH_REMAP".into(),
                value: remap.into(),
            },
            SessionEnvVar {
                key: "ZCCACHE_WORKTREE_ROOT".into(),
                value: target.to_string_lossy().into_owned(),
            },
        ];
        apply_compile_root(&mut env);
        assert_eq!(Path::new(&env[4].value), expected);
    }
}

#[test]
fn disabled_path_remapping_keeps_the_callers_root() {
    let mut command = Command::new("fixture");
    command
        .env("ZCCACHE_PATH_REMAP", "off")
        .env("ZCCACHE_WORKTREE_ROOT", "caller-root");
    apply_target_remap(
        &mut command,
        &["build".into(), "--target-dir=other-root".into()],
    );
    assert!(command
        .get_envs()
        .any(|(name, value)| name == "ZCCACHE_WORKTREE_ROOT"
            && value == Some(std::ffi::OsStr::new("caller-root"))));
}

#[test]
fn library_target_args_and_remap_use_the_physical_store() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store/nightly-host-key");
    let target = tmp.path().join("checkout/target");
    std::fs::create_dir_all(&store).unwrap();
    crate::dylint_library_store::link_libraries_dir(&target, &store).unwrap();
    let linked = target.join("dylint/libraries/nightly-host");
    let store = store.join("nightly-host");
    let args = vec![
        "build".into(),
        "--release".into(),
        "--target-dir".into(),
        linked.to_string_lossy().into_owned(),
    ];
    let args = canonical_target_args(&args);
    let store = store.canonicalize().unwrap();
    assert_eq!(Path::new(&args[3]), store);
    let mut command = Command::new("fixture");
    command
        .current_dir(tmp.path())
        .env("ZCCACHE_PATH_REMAP", "auto");
    apply_target_remap(&mut command, &args);
    assert!(command
        .get_envs()
        .any(|(name, value)| name == "ZCCACHE_WORKTREE_ROOT" && value == Some(store.as_os_str())));
    let equals =
        canonical_target_args(&["build".into(), format!("--target-dir={}", linked.display())]);
    assert_eq!(equals[1], format!("--target-dir={}", store.display()));
}

#[test]
fn managed_dylint_binary_uses_one_registry_home_and_host_binary_keeps_caller_home() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(tmp.path().join("soldr"));
    let mut command = Command::new("fixture");
    command.env("CARGO_HOME", tmp.path().join("caller"));
    apply_homes(
        &mut command,
        &crate::fetch::managed_rustup_home(&paths).join("toolchains/nightly/bin/cargo"),
        &paths,
        None,
    );
    assert!(command.get_envs().any(|(name, value)| name == "CARGO_HOME"
        && value == Some(crate::fetch::managed_cargo_home(&paths).as_os_str())));
    let mut host = Command::new("fixture");
    host.env("CARGO_HOME", tmp.path().join("caller"));
    apply_homes(&mut host, &tmp.path().join("host/bin/cargo"), &paths, None);
    assert!(host.get_envs().any(|(name, value)| name == "CARGO_HOME"
        && value == Some(tmp.path().join("caller").as_os_str())));
}

#[test]
fn explicitly_selected_registry_home_is_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(tmp.path().join("soldr"));
    let caller = tmp.path().join("private-registry");
    let binary = crate::fetch::managed_rustup_home(&paths).join("toolchains/nightly/bin/cargo");
    let mut command = Command::new("fixture");
    command.env("CARGO_HOME", &caller);
    apply_homes(&mut command, &binary, &paths, Some(caller.as_os_str()));
    assert!(command
        .get_envs()
        .any(|(name, value)| name == "CARGO_HOME" && value == Some(caller.as_os_str())));
}
