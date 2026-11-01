use super::*;

fn env_of(command: &Command, name: &str) -> Option<PathBuf> {
    command
        .get_envs()
        .find(|(key, _)| *key == name)
        .and_then(|(_, value)| value.map(PathBuf::from))
}

#[test]
fn every_soldr_state_input_points_inside_the_private_root() {
    let base = tempfile::tempdir().expect("tmp");
    let root = base.path().join("root");
    std::fs::create_dir(&root).expect("root");
    let mut command = Command::new("true");
    apply(&mut command, &root).expect("apply");
    for name in [
        "HOME",
        "SOLDR_CACHE_DIR",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        let value = env_of(&command, name).unwrap_or_else(|| panic!("{name} unset"));
        assert!(value.starts_with(&root), "{name}={}", value.display());
        assert!(value.is_dir(), "{name} directory exists");
    }
    assert_eq!(env_of(&command, ISOLATED_ENV), Some(PathBuf::from("1")));
}

#[test]
fn toolchain_homes_are_reached_through_the_private_home_not_exported() {
    let base = tempfile::tempdir().expect("tmp");
    let mut command = Command::new("true");
    apply(&mut command, base.path()).expect("apply");
    assert_eq!(env_of(&command, "CARGO_HOME"), None);
    assert_eq!(env_of(&command, "RUSTUP_HOME"), None);
    let home = base.path().join("h");
    for (leaf, resolved) in [
        (".cargo", soldr_core::core::resolve_cargo_home()),
        (".rustup", soldr_core::core::resolve_rustup_home()),
    ] {
        let explicit = std::env::var_os(if leaf == ".cargo" {
            "CARGO_HOME"
        } else {
            "RUSTUP_HOME"
        })
        .is_some_and(|value| !value.is_empty());
        if let Some(resolved) = resolved.filter(|path| path.is_dir() && !explicit) {
            assert_eq!(std::fs::read_link(home.join(leaf)).ok(), Some(resolved));
        }
    }
}

#[test]
fn a_missing_root_fails_instead_of_running_unisolated() {
    let base = tempfile::tempdir().expect("tmp");
    let mut command = Command::new("true");
    assert!(apply(&mut command, &base.path().join("absent")).is_err());
}

#[test]
fn the_outer_front_doors_route_and_shim_are_not_inherited() {
    let base = tempfile::tempdir().expect("tmp");
    let mut command = Command::new("true");
    command.env("SOLDR_BROKER_SERVICE", "soldr-daemon-outer");
    command.env("RUSTC_WRAPPER", "/caller/.soldr/shims/rustc");
    apply(&mut command, base.path()).expect("apply");
    for name in OUTER_SESSION_ENVS {
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == name && value.is_none()),
            "{name} is removed"
        );
    }
}

#[test]
fn path_entries_inside_the_callers_soldr_root_are_dropped() {
    let base = tempfile::tempdir().expect("tmp");
    let root = base.path().join(".soldr");
    let shims = root.join("v0.9.49").join("shims");
    std::fs::create_dir_all(&shims).expect("shims");
    let keep = base.path().join("bin");
    std::fs::create_dir_all(&keep).expect("bin");
    let path = std::env::join_paths([shims.as_path(), keep.as_path(), root.join("bin").as_path()])
        .expect("join");
    let stripped = strip_entries_under(&path, &[root]).expect("strip");
    assert_eq!(
        std::env::split_paths(&stripped).collect::<Vec<_>>(),
        vec![keep]
    );
}

#[test]
fn a_symlinked_path_entry_is_judged_by_its_target() {
    let base = tempfile::tempdir().expect("tmp");
    let root = base.path().join(".soldr");
    let shims = root.join("shims");
    std::fs::create_dir_all(&shims).expect("shims");
    let alias = base.path().join("alias");
    test_child::link_dir(&shims, &alias).expect("link");
    let stripped = strip_entries_under(alias.as_os_str(), &[root]).expect("strip");
    assert!(stripped.is_empty(), "{stripped:?}");
}

#[test]
fn variables_naming_a_program_in_the_callers_root_are_recognized() {
    let base = tempfile::tempdir().expect("tmp");
    let root = base.path().join(".soldr");
    let shims = root.join("bin").join("linker-shims").join("v1");
    std::fs::create_dir_all(&shims).expect("shims");
    let linker = shims.join("clang-afe5f3c59276da8fcce9");
    std::fs::write(&linker, b"").expect("linker shim");
    let elsewhere = base.path().join("usr-bin");
    std::fs::create_dir_all(&elsewhere).expect("bin");
    std::fs::write(elsewhere.join("clang"), b"").expect("clang");
    let path = std::env::join_paths([shims.as_path(), elsewhere.as_path()]).expect("join");
    let roots = [root.clone()];
    let names = |value: &str| names_program_under(OsStr::new(value), &path, &roots);
    // A bare linker-shim name found through PATH, and an absolute shim path
    // followed by its real compiler (the `CC` shape).
    assert!(names("clang-afe5f3c59276da8fcce9"));
    let cc = format!(
        "{} /usr/bin/cc",
        root.join("v0.9.49/shims/zccache-soldr").display()
    );
    assert!(names(&cc));
    // A user's own settings and plain values are not touched.
    assert!(!names("clang"));
    assert!(!names("/usr/bin/cc"));
    assert!(!names("1"));
    assert!(!names("soldr-managed"));
}
