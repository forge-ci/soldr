from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest
from conftest import maturin_release_build_command

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "native_release_build.py"


def load_module():
    spec = importlib.util.spec_from_file_location("native_release_build", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


MODULE = load_module()


def test_musl_release_source_builds_the_pinned_soldr_maturin() -> None:
    assert MODULE.SOLDR_MATURIN_REQUIREMENT == "soldr-maturin==1.14.1.post1"
    assert MODULE.SOLDR_MATURIN_NO_BINARY == "soldr-maturin"


def test_musl_wheel_maturin_command_strips_for_wheel_size(
    tmp_path: Path,
) -> None:
    """soldr#3038: without --strip this wheel bundles the same unstripped
    binary [profile.release] deliberately leaves for the archive's own
    objcopy carve-out -- see the function's own docstring for the measured
    size cost (36.7 MiB vs 10.3 MiB compressed, x86_64-gnu).
    """
    maturin = tmp_path / "maturin"
    command = MODULE.musl_wheel_maturin_command(maturin, "x86_64-unknown-linux-musl")
    assert command == maturin_release_build_command(
        str(maturin), "x86_64-unknown-linux-musl", "musllinux_1_2"
    )


def test_cargo_command_routes_through_pinned_soldr_rustup() -> None:
    driver = Path("release-tools") / "soldr"
    command = MODULE.cargo_command(driver, "build", "--release")
    assert command == [
        str(driver),
        "rustup",
        "run",
        "1.98.1",
        "cargo",
        "build",
        "--release",
    ]


def test_soldr_cli_version_requires_exactly_one_package() -> None:
    assert (
        MODULE.soldr_cli_version(
            '{"packages":[{"name":"soldr-cli","version":"0.9.0"}]}'
        )
        == "0.9.0"
    )
    with pytest.raises(RuntimeError, match="expected one soldr-cli package"):
        MODULE.soldr_cli_version('{"packages":[]}')


def test_release_build_environment_is_bounded_and_reproducible() -> None:
    env = MODULE.release_build_environment(
        {"KEEP": "yes", "SOLDR_JOBS": "99", "RUSTC_WRAPPER": "stale-wrapper"},
        target="aarch64-unknown-linux-musl",
    )
    assert env["KEEP"] == "yes"
    assert env["SOLDR_RELEASE_CI"] == "1"
    assert env["CARGO_PROFILE_RELEASE_LTO"] == "thin"
    assert env["CARGO_PROFILE_RELEASE_CODEGEN_UNITS"] == "1"
    assert env["CARGO_BUILD_JOBS"] == "2"
    assert env["SOLDR_JOBS"] == "2"
    assert "RUSTC_WRAPPER" not in env
    # soldr#3038: ARM64 musl is ELF -- the post-link objcopy carve-out
    # handles it, so it must NOT get the macOS-only packed override.
    assert "CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO" not in env


def test_release_build_environment_keeps_packed_split_debuginfo_for_darwin() -> None:
    """soldr#3038: macOS keeps `split-debuginfo = "packed"` (dsymutil's
    `.dSYM` model has no duplication problem, unlike Linux's packed split
    which left std/C-dependency DWARF embedded in the shipped binary AND
    duplicated into the `.dwp`). This is injected here, per-target, because
    Cargo.toml profiles cannot themselves vary by target triple.
    """
    for target in ("x86_64-apple-darwin", "aarch64-apple-darwin"):
        env = MODULE.release_build_environment({}, target=target)
        assert env["CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO"] == "packed", target


def test_wheel_environment_points_maturin_at_soldr_cargo_bridge() -> None:
    driver = Path("release-tools") / "soldr"
    cargo_bridge = Path("release-tools") / "cargo-via-soldr"
    env = MODULE.wheel_environment(
        {"KEEP": "yes", "RUSTC_WRAPPER": "stale-wrapper"},
        driver=driver,
        cargo_bridge=cargo_bridge,
    )
    assert env == {
        "KEEP": "yes",
        "SOLDR_RELEASE_DRIVER": str(driver),
        "SOLDR_RELEASE_TOOLCHAIN": "1.98.1",
        "CARGO": str(cargo_bridge),
    }


def test_wheel_environment_preserves_concurrency_limits() -> None:
    env = MODULE.wheel_environment(
        {"CARGO_BUILD_JOBS": "1", "SOLDR_JOBS": "1"},
        driver=Path("release-tools") / "soldr",
        cargo_bridge=Path("release-tools") / "cargo-via-soldr",
    )
    assert env["CARGO_BUILD_JOBS"] == "1"
    assert env["SOLDR_JOBS"] == "1"


def test_host_tool_environment_strips_musl_cross_state() -> None:
    driver = Path("release-tools") / "soldr"
    cargo_bridge = Path("release-tools") / "cargo-via-soldr"
    rustc = Path("toolchains") / "1.98.1" / "bin" / "rustc"
    env = MODULE.host_tool_environment(
        {
            "KEEP": "yes",
            "PATH": "caller-bin",
            "CARGO_BUILD_TARGET": "aarch64-unknown-linux-musl",
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER": "musl-gcc",
            "CC_aarch64_unknown_linux_musl": "musl-gcc",
            "RUSTC_WRAPPER": "stale-wrapper",
        },
        driver=driver,
        cargo_bridge=cargo_bridge,
        rustc=rustc,
    )
    resolved_rustc = rustc.resolve()
    assert env == {
        "KEEP": "yes",
        "PATH": f"{resolved_rustc.parent}{MODULE.os.pathsep}caller-bin",
        "SOLDR_RELEASE_DRIVER": str(driver),
        "SOLDR_RELEASE_TOOLCHAIN": "1.98.1",
        "CARGO": str(cargo_bridge),
        "RUSTC": str(resolved_rustc),
        "RUSTUP_TOOLCHAIN": "1.98.1",
        "CARGO_BUILD_JOBS": "2",
        "SOLDR_JOBS": "2",
    }


def test_host_tool_environment_preserves_stricter_concurrency_limits() -> None:
    env = MODULE.host_tool_environment(
        {"CARGO_BUILD_JOBS": "1", "SOLDR_JOBS": "1"},
        driver=Path("release-tools") / "soldr",
        cargo_bridge=Path("release-tools") / "cargo-via-soldr",
        rustc=Path("toolchains") / "1.98.1" / "bin" / "rustc",
    )
    assert env["CARGO_BUILD_JOBS"] == "1"
    assert env["SOLDR_JOBS"] == "1"


def test_matrix_driver_is_exe_suffixed_on_windows(monkeypatch) -> None:
    monkeypatch.setenv("RUNNER_OS", "Windows")
    assert MODULE.matrix_driver().name == "soldr.exe"
    monkeypatch.setenv("RUNNER_OS", "Linux")
    assert MODULE.matrix_driver().name == "soldr"
    monkeypatch.delenv("RUNNER_OS", raising=False)
    assert MODULE.matrix_driver().name == "soldr"


def _record_matrix_build(monkeypatch, target: str, *, github_env: str | None):
    """Run `build_matrix_binary` with every subprocess captured."""
    commands: list[list[str]] = []

    def fake_run(command, env=None):  # `run` helper: check=True calls
        commands.append(list(command))

    def fake_subprocess_run(command, **_kwargs):  # best-effort calls
        commands.append(list(command))

    monkeypatch.setattr(MODULE, "run", fake_run)
    monkeypatch.setattr(MODULE.subprocess, "run", fake_subprocess_run)
    if github_env is None:
        monkeypatch.delenv("GITHUB_ENV", raising=False)
    else:
        monkeypatch.setenv("GITHUB_ENV", github_env)
    MODULE.build_matrix_binary(Path("target/release/soldr"), target)
    return commands


def test_matrix_build_uses_the_blessed_build_surface(monkeypatch) -> None:
    """Not `rustup run cargo build`: this lane drives `soldr build`.

    The distinction is why this path is extracted separately from
    `build_binary` rather than merged with it.
    """
    commands = _record_matrix_build(
        monkeypatch, "x86_64-pc-windows-msvc", github_env=None
    )
    build = [c for c in commands if "build" in c and "clean" not in c]
    assert len(build) == 1
    assert build[0][1:4] == ["--no-cache", "build", "--release"]
    assert "rustup" not in build[0]
    # `--locked` is deliberately absent here; adding it is a behavior change.
    assert "--locked" not in build[0]


def test_matrix_build_injects_packed_split_debuginfo_for_darwin(monkeypatch) -> None:
    """soldr#3071: both apple-darwin triples now build through this lane
    (Linux cross, same as Windows) rather than the native `build_binary`
    path, so the macOS-only split-debuginfo override (soldr#3038) must move
    here with them instead of silently disappearing.
    """
    captured_envs: list[dict[str, str] | None] = []

    def fake_run(command, env=None):
        captured_envs.append(env)

    monkeypatch.setattr(MODULE, "run", fake_run)
    monkeypatch.setattr(MODULE.subprocess, "run", lambda *_a, **_k: None)
    monkeypatch.delenv("CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO", raising=False)

    MODULE.build_matrix_binary(Path("target/release/soldr"), "aarch64-apple-darwin")
    assert captured_envs[0] is not None
    assert captured_envs[0]["CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO"] == "packed"

    captured_envs.clear()
    MODULE.build_matrix_binary(Path("target/release/soldr"), "x86_64-pc-windows-msvc")
    assert captured_envs[0] is not None
    assert "CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO" not in captured_envs[0]


def test_matrix_build_restores_manifests_before_building(monkeypatch) -> None:
    commands = _record_matrix_build(
        monkeypatch, "x86_64-unknown-linux-musl", github_env=None
    )
    assert commands[0][:2] == ["git", "restore"]
    assert "Cargo.lock" in commands[0]


def test_gnu_linux_runs_prepare_and_other_targets_do_not(monkeypatch) -> None:
    gnu = _record_matrix_build(
        monkeypatch, "x86_64-unknown-linux-gnu", github_env="/tmp/env"
    )
    assert any("prepare" in c for c in gnu)

    msvc = _record_matrix_build(
        monkeypatch, "x86_64-pc-windows-msvc", github_env="/tmp/env"
    )
    assert not any("prepare" in c for c in msvc)


def test_prepare_is_skipped_when_github_env_is_absent(monkeypatch) -> None:
    """Outside Actions there is no file to write; the build must still run."""
    commands = _record_matrix_build(
        monkeypatch, "x86_64-unknown-linux-gnu", github_env=None
    )
    assert not any("prepare" in c for c in commands)
    assert any("build" in c and "clean" not in c for c in commands)


def test_the_workflow_invokes_matrix_binary_and_keeps_the_profile_env() -> None:
    workflow = (
        Path(__file__).parents[1] / ".github" / "workflows" / "release-auto.yml"
    ).read_text(encoding="utf-8")
    assert "native_release_build.py matrix-binary" in workflow
    # The profile values are matrix expressions and must stay in YAML --
    # moving them into Python would mean reimplementing the matrix there.
    assert "CARGO_PROFILE_RELEASE_LTO: ${{ contains(matrix.target," in workflow
    # The inline implementation must be gone, not merely bypassed.
    assert (
        'case "$RUNNER_OS" in'
        not in workflow.split("Build ARM64 musl")[0].split(
            "Build release binary (soldr-driven)"
        )[-1]
    )
