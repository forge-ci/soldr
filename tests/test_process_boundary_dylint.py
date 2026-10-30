from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def test_root_workspace_loads_process_boundary_dylint() -> None:
    manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    assert "[workspace.metadata.dylint]" in manifest
    assert 'libraries = [{ path = "dylints/*" }]' in manifest
    assert (ROOT / "dylints" / "ban_raw_process_creation" / "src" / "lib.rs").is_file()


def test_required_ci_runs_root_dylint_policy() -> None:
    lint_workflow = (ROOT / ".github" / "workflows" / "ci.yml").read_text(
        encoding="utf-8"
    )
    host_workflow = (ROOT / ".github" / "workflows" / "_build-and-test.yml").read_text(
        encoding="utf-8"
    )
    plan = (ROOT / "crates" / "soldr-cli" / "src" / "ci_test" / "plan.rs").read_text(
        encoding="utf-8"
    )

    assert host_workflow.count("- name: Run prescribed host validation") == 1
    assert 'ci-test --target "${{ inputs.target }}"' in host_workflow
    assert "Enforce daemon process-creation boundary" not in lint_workflow
    assert "Build Soldr Dylint front door" not in lint_workflow
    assert "DYLINTS" in plan
    for name in (
        "ban_raw_process_creation",
        "ban_raw_network_access",
        "ban_raw_local_socket_name",
        "ban_raw_ipc_transport",
        "ban_platform_cfg_outside_boundary",
        "ban_raw_env_flag",
        "ban_swallowed_child_stdio",
        "ban_raw_sync_primitive",
    ):
        assert f'"{name}"' in plan

    for target in ('join("libraries")', 'join("target")', 'join("tests")'):
        assert target in plan
    assert '"--no-build"' in plan
    assert '"--workspace"' in plan
    assert '"--all-targets"' in plan

    dylint_config = (
        ROOT / "dylints" / "ban_raw_process_creation" / ".cargo" / "config.toml"
    ).read_text(encoding="utf-8")
    assert 'rustflags = ["-C", "linker=dylint-link"]' in dylint_config
    dylint_manifest = (
        ROOT / "dylints" / "ban_raw_process_creation" / "Cargo.toml"
    ).read_text(encoding="utf-8")
    assert "[profile.release]" in dylint_manifest
    assert "opt-level = 0" in dylint_manifest
    assert "lto = false" in dylint_manifest

    for manifest_path in (ROOT / "dylints").glob("*/Cargo.toml"):
        manifest_text = manifest_path.read_text(encoding="utf-8")
        assert 'dylint_testing = "=6.2.0"' in manifest_text


def test_process_boundary_has_required_ui_fixtures() -> None:
    ui = ROOT / "dylints" / "ban_raw_process_creation" / "ui"
    expected = {
        "disallowed_spawn.rs",
        "disallowed_spawn_ufcs.rs",
        "disallowed_spawn_function_item.rs",
        "disallowed_output.rs",
        "disallowed_status.rs",
        "disallowed_tokio_status.rs",
        "disallowed_creation_flags.rs",
        "disallowed_create_process.rs",
    }
    assert expected <= {path.name for path in ui.glob("*.rs")}


def test_dylint_runtime_environment_uses_one_platform_facade() -> None:
    # soldr#2493 replaced the four cfg-duplicated impls with a single
    # runtime match on the host facts facade; no host cfg remains in
    # the cli source. soldr#2945 moved that facade call, with the rest
    # of the driver half of `dylint_toolchain.rs`, into
    # `dylint_driver.rs` to stay under the hard 1,000-line ceiling.
    source = (ROOT / "crates" / "soldr-cli" / "src" / "dylint_driver.rs").read_text(
        encoding="utf-8"
    )
    assert source.count("fn apply_driver_runtime_environment(") == 1
    assert source.count("fn apply_driver_runtime_environment_impl(") == 1
    assert source.count("#[cfg(") == source.count("#[cfg(test)]")
    for host in ("HostOs::Windows", "HostOs::Linux", "HostOs::MacOs"):
        assert host in source
    assert "match crate::platform::host::facts::os()" in source
