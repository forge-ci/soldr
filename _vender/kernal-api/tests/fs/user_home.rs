#![cfg(feature = "fs")]

#[test]
fn user_home_matches_native_account_resolution() {
    assert_eq!(kernal_api::platform::fs::user_home_dir(), dirs::home_dir());
}

#[cfg(unix)]
#[test]
fn user_home_environment_probe() {
    let Ok(mode) = std::env::var("KERNAL_USER_HOME_PROBE") else {
        return;
    };
    let home = kernal_api::platform::fs::user_home_dir().expect("test account has a home");
    if mode == "override" {
        assert_eq!(
            home,
            std::path::PathBuf::from("/kernel-home-probe/not-created")
        );
    } else {
        assert!(
            !home.as_os_str().is_empty(),
            "empty HOME must use account fallback"
        );
        assert!(home.is_absolute());
    }
}

#[cfg(unix)]
#[test]
fn user_home_handles_override_empty_and_missing_environment() {
    for mode in ["override", "empty", "missing"] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "user_home::user_home_environment_probe"]);
        command.env("KERNAL_USER_HOME_PROBE", mode);
        match mode {
            "override" => command.env("HOME", "/kernel-home-probe/not-created"),
            "empty" => command.env("HOME", ""),
            _ => command.env_remove("HOME"),
        };
        assert!(command.status().unwrap().success(), "home mode {mode}");
    }
}
