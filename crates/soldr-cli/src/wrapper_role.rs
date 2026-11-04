//! Distinguish native compiler commands from Cargo wrapper callbacks.
//!
//! Absolute compiler paths are unambiguous callbacks. A bare tool name is
//! also a native CLI command, so ambient RUSTC_WRAPPER alone is insufficient:
//! a callback must prove a live Soldr ancestor and the executing wrapper image.

/// Native commands own route registration; true callbacks never enter here.
pub(crate) fn run_native_compiler(raw_args: &[String]) -> Result<i32, crate::core::SoldrError> {
    if crate::cache_lib::cache_enabled_in_current_process()
        && !crate::wrapper::is_non_cacheable_rustc(raw_args)
    {
        crate::soldr_main::start_daemon_route(crate::soldr_main::DAEMON_START_ROUTE_BUDGET)?;
    }
    crate::wrapper::run_rustc_wrapper(raw_args, crate::startup_profile::WrapperProfile::new())
}

pub(crate) fn explicit_callback(arg: &str) -> bool {
    crate::wrapper::is_wrapper_invocation(arg) && !matches!(arg, "rustc" | "clippy-driver")
}

/// Capture before the reentrancy guard replaces inherited lineage with self.
pub(crate) fn capture(raw_args: &[String]) -> Result<bool, String> {
    let Some(tool) = raw_args.get(1) else {
        return Ok(false);
    };
    if explicit_callback(tool) {
        return Ok(true);
    }
    if !crate::wrapper::is_wrapper_invocation(tool) {
        return Ok(false);
    }
    let inherited_pid = std::env::var(crate::reentrancy_guard::IN_SOLDR_PID_ENV).ok();
    let inherited_role = std::env::var(crate::reentrancy_guard::IN_SOLDR_ROLE_ENV).ok();
    if inherited_pid.is_none() {
        return Ok(false);
    }
    let invalid = || {
        "cannot verify bare compiler wrapper callback lineage; invoke the native command from a fresh shell or pass Cargo's absolute compiler path".to_string()
    };
    let pid = inherited_pid
        .as_deref()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(invalid)?;
    if !matches!(inherited_role.as_deref(), Some("front-door" | "wrapper"))
        || !ancestor_is_live(pid)
    {
        return Err(invalid());
    }
    crate::wrapper_identity::assert_inherited_wrapper_coherent("compiler callback role")
        .map_err(|error| error.to_string())?;
    let current = std::env::current_exe().map_err(|_| invalid())?;
    let parent = crate::platform::process::inspect::executable_path(pid).ok_or_else(invalid)?;
    let wrapper_matches = ["RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"]
        .iter()
        .any(|key| {
            let Some(value) = std::env::var_os(key) else {
                return false;
            };
            let Some(name) = value.to_str() else {
                return false;
            };
            let path = std::env::var_os("PATH").unwrap_or_default();
            crate::exec_cmd::find_on_path(name, &path).is_some_and(|wrapper| {
                crate::self_relocate::same_executable_image(&wrapper, &current)
            })
        });
    if wrapper_matches && crate::self_relocate::same_executable_image(&parent, &current) {
        Ok(true)
    } else {
        Err(invalid())
    }
}

fn ancestor_is_live(claimed: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, System};
    if claimed == std::process::id() || !crate::platform::process::inspect::is_alive(claimed) {
        return false;
    }
    let mut system = System::new();
    let mut pid = Pid::from_u32(std::process::id());
    for _ in 0..64 {
        if !system.refresh_process_specifics(pid, ProcessRefreshKind::new()) {
            return false;
        }
        let Some(process) = system.process(pid) else {
            return false;
        };
        let Some(parent) = process.parent() else {
            return false;
        };
        if parent.as_u32() == claimed {
            return true;
        }
        if parent == pid {
            return false;
        }
        pid = parent;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(tool: &str) -> Vec<String> {
        vec!["soldr".into(), tool.into()]
    }

    #[test]
    fn own_exported_wrapper_without_lineage_is_native() {
        let _lock = crate::TEST_PROCESS_ENV_LOCK.lock().unwrap();
        let _pid = crate::EnvVarGuard::remove("IN_SOLDR_PID");
        let _role = crate::EnvVarGuard::remove("IN_SOLDR_ROLE");
        let _wrapper = crate::EnvVarGuard::set("RUSTC_WRAPPER", std::env::current_exe().unwrap());
        assert_eq!(capture(&args("rustc")), Ok(false));
        assert_eq!(capture(&args("clippy-driver")), Ok(false));
    }

    #[test]
    fn claimed_unverifiable_lineage_never_becomes_native_bootstrap() {
        let _lock = crate::TEST_PROCESS_ENV_LOCK.lock().unwrap();
        let _role = crate::EnvVarGuard::set("IN_SOLDR_ROLE", "front-door");
        for value in [
            "garbage".to_string(),
            u32::MAX.to_string(),
            "1".to_string(),
            std::process::id().to_string(),
        ] {
            let _pid = crate::EnvVarGuard::set("IN_SOLDR_PID", value);
            assert!(capture(&args("rustc")).is_err());
        }
    }

    #[test]
    fn absolute_callback_never_requires_lineage_or_bootstraps() {
        let _lock = crate::TEST_PROCESS_ENV_LOCK.lock().unwrap();
        let _pid = crate::EnvVarGuard::set("IN_SOLDR_PID", "garbage");
        assert_eq!(capture(&args("/toolchain/bin/rustc")), Ok(true));
        assert_eq!(capture(&args("dylint-driver.exe")), Ok(true));
    }

    #[test]
    #[ignore = "subprocess helper"]
    fn callback_probe() {
        let expected = std::env::var("SOLDR_ROLE_TEST_EXPECTED").unwrap();
        let result = capture(&args("rustc"));
        assert_eq!(result == Ok(true), expected == "callback", "{result:?}");
        if expected == "error" {
            assert!(result.is_err());
        }
    }

    #[test]
    fn live_ancestor_and_copied_wrapper_image_are_verified() {
        let root = tempfile::tempdir().unwrap();
        let source = std::env::current_exe().unwrap();
        let copy = root
            .path()
            .join(crate::platform::executable::name::native("wrapper-probe"));
        std::fs::copy(&source, &copy).unwrap();
        for (wrapper, mirror, expected) in [
            (&copy, &copy, "callback"),
            (&source, &copy, "error"),
            (
                &root.path().join("unrelated-wrapper"),
                &root.path().join("unrelated-wrapper"),
                "error",
            ),
        ] {
            let output = std::process::Command::new(&source)
                .args([
                    "--exact",
                    "wrapper_role::tests::callback_probe",
                    "--ignored",
                    "--nocapture",
                ])
                .env("IN_SOLDR_PID", std::process::id().to_string())
                .env("IN_SOLDR_ROLE", "front-door")
                .env("RUSTC_WRAPPER", wrapper)
                .env(crate::wrapper_identity::EFFECTIVE_WRAPPER_ENV, mirror)
                .env(
                    crate::wrapper_identity::EFFECTIVE_WRAPPER_ORIGIN_ENV,
                    "source-build",
                )
                .env("SOLDR_ROLE_TEST_EXPECTED", expected)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }
}
