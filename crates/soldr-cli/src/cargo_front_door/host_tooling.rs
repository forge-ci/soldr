//! Managed host tools inherited by native compile-capable invocations.
//! Regression coverage for zackees/clud#500.

use crate::core::SoldrPaths;

pub(super) fn should_inject_native_cmake(args: &[String]) -> bool {
    super::subcommand::cargo_args_are_cacheable(args)
        && !super::subcommand::cargo_args_specify_target(args)
}

pub(super) async fn inject(
    args: &[String],
    paths: &SoldrPaths,
    bootstrap: &mut super::SubcommandToolBootstrap,
) {
    if !should_inject_native_cmake(args) {
        return;
    }
    // soldr#3783: skip the provisioning cost when the lockfile provably has no cmake.
    let cwd = std::env::current_dir().unwrap_or_default();
    if !super::cmake_need::managed_cmake_needed(args, &cwd) {
        return;
    }

    let mut prep = crate::blessed_build::BlessedPrep::default();
    crate::blessed_build::inject_cmake_tooling(paths, &mut prep).await;
    bootstrap.bin_dirs.extend(prep.path_prefix());
    bootstrap.env.extend(prep.env);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn native_compile_cmake_policy() {
        assert!(should_inject_native_cmake(&args(&["test", "-p", "soldr"])));
        assert!(should_inject_native_cmake(&args(&["build", "--release"])));
        assert!(!should_inject_native_cmake(&args(&["metadata"])));
        assert!(!should_inject_native_cmake(&args(&[
            "test",
            "--target",
            "aarch64-unknown-linux-gnu",
        ])));
    }

    #[test]
    fn native_compile_inherits_managed_cmake_and_ninja() {
        let _env = crate::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _cmake = crate::EnvVarGuard::remove("CMAKE");
        let _generator = crate::EnvVarGuard::remove("CMAKE_GENERATOR");
        let _system = crate::EnvVarGuard::remove(crate::blessed_build::USE_SYSTEM_CMAKE_ENV_VAR);

        let host = crate::fetch::cmake_tools::current_host_triple();
        let Some(slug) = crate::fetch::cmake_tools::host_slug_for(host) else {
            return;
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());

        for (tool, version, executable) in [
            (
                "cmake",
                crate::fetch::cmake_tools::MANAGED_CMAKE_VERSION,
                "cmake",
            ),
            (
                "ninja",
                crate::fetch::cmake_tools::MANAGED_NINJA_VERSION,
                "ninja",
            ),
        ] {
            let install = paths.bin.join("syslib").join(tool).join(version).join(slug);
            let package = install.join("package");
            let exe = if executable == "cmake" {
                crate::fetch::cmake_tools::cmake_exe(&package)
            } else {
                crate::fetch::cmake_tools::ninja_exe(&package)
            };
            std::fs::create_dir_all(exe.parent().expect("bin dir")).expect("create bin");
            std::fs::write(exe, b"fixture").expect("write executable fixture");
            std::fs::write(install.join(".complete"), b"").expect("write stamp");
        }

        // soldr#3783: the project graph must contain cmake for provisioning to run.
        let project = tempfile::tempdir().expect("project");
        std::fs::write(
            project.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n",
        )
        .expect("manifest");
        std::fs::write(
            project.path().join("Cargo.lock"),
            "[[package]]\nname = \"cmake\"\n",
        )
        .expect("lock");
        let manifest = project.path().join("Cargo.toml").display().to_string();

        let mut bootstrap = super::super::SubcommandToolBootstrap {
            bin_dirs: Vec::new(),
            env: Vec::new(),
            cargo_args: Vec::new(),
        };
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(inject(
                &args(&["test", "--manifest-path", &manifest]),
                &paths,
                &mut bootstrap,
            ));

        assert!(bootstrap.env.iter().any(|(key, _)| key == "CMAKE"));
        assert!(bootstrap
            .env
            .iter()
            .any(|(key, value)| key == "CMAKE_GENERATOR" && value == "Ninja"));
        assert_eq!(
            bootstrap.bin_dirs.len(),
            2,
            "both managed tools must reach PATH"
        );
    }
}
