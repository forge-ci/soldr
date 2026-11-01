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
