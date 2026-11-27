use crate::common::{
    install_fake_toolchain, install_logging_fake_cargo, isolated_soldr_command,
    read_logged_cargo_invocations, unique_temp_dir, BrokerHomeGuard,
};

#[test]
fn cargo_front_door_probes_pyo3_only_without_caller_configuration() {
    for configured in [false, true] {
        let workspace = unique_temp_dir("cargo-pyo3-metadata");
        let cache_root = workspace.join("soldr-root");
        let _broker = BrokerHomeGuard::new(&cache_root, &workspace);
        let log = workspace.join("cargo.log");
        let cargo = install_logging_fake_cargo(&log);
        let (_, rustc, _) = install_fake_toolchain(&workspace.join("toolchain.log"));

        let mut command = isolated_soldr_command();
        command
            .args([
                "--no-cache",
                "cargo",
                "test",
                "--no-run",
                "--target",
                "wasm32-unknown-unknown",
            ])
            .current_dir(&workspace)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("HOME", &workspace)
            .env("USERPROFILE", &workspace)
            .env("CARGO_TARGET_DIR", workspace.join("target"))
            .env("SOLDR_TEST_CARGO_BIN", &cargo)
            .env("SOLDR_TEST_RUSTC_BIN", &rustc)
            .env("SOLDR_LINKER", "default");
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("PYO3_") {
                command.env_remove(name);
            }
        }
        if configured {
            command.env("PYO3_PYTHON", "python3");
        }

        let output = command.output().expect("run the Cargo front door");
        assert!(
            output.status.success(),
            "cargo test failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let invocations = read_logged_cargo_invocations(&log);
        let called = |verb: &str| {
            invocations
                .iter()
                .any(|args| args.first().is_some_and(|arg| arg == verb))
        };
        assert!(called("test"), "missing cargo test: {invocations:?}");
        assert_eq!(
            called("metadata"),
            !configured,
            "unexpected metadata probe with configured={configured}: {invocations:?}"
        );
    }
}
