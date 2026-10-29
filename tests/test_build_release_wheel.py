from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest
from conftest import maturin_release_build_command

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "build_release_wheel.py"


def _load_script():
    spec = importlib.util.spec_from_file_location("build_release_wheel", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


wheel = _load_script()


@pytest.mark.parametrize(
    "target",
    sorted(wheel.RELEASE_TARGETS),
)
def test_canonical_release_targets_are_accepted(target: str) -> None:
    wheel.validate_target(target)


def test_environment_preserves_unrelated_caller_arguments() -> None:
    env = wheel.build_environment(
        "aarch64-unknown-linux-gnu",
        {"MATURIN_PEP517_ARGS": "--features tokio-console", "KEEP": "yes"},
    )
    assert env["KEEP"] == "yes"
    assert env["MATURIN_PEP517_ARGS"] == "--features tokio-console"
    assert env["SOLDR_PEP517_PROFILE"] == "release"
    assert env["SOLDR_RELEASE_CI"] == "1"
    assert env["CARGO_BUILD_JOBS"] == "2"
    assert env["SOLDR_JOBS"] == "2"


def test_environment_preserves_stricter_concurrency_limits() -> None:
    env = wheel.build_environment(
        "aarch64-unknown-linux-gnu",
        {"CARGO_BUILD_JOBS": "1", "SOLDR_JOBS": "1"},
    )
    assert env["CARGO_BUILD_JOBS"] == "1"
    assert env["SOLDR_JOBS"] == "1"


def test_release_environment_forces_soldr_maturin_off_xwin() -> None:
    env = wheel.build_environment("x86_64-pc-windows-msvc", {"MATURIN_USE_XWIN": "1"})
    assert env["MATURIN_USE_XWIN"] == "0"


@pytest.mark.parametrize(
    "target,key",
    [
        ("x86_64-pc-windows-msvc", "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"),
        ("aarch64-pc-windows-msvc", "CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER"),
    ],
)
def test_release_environment_pins_msvc_linker_to_lld_link(
    target: str, key: str
) -> None:
    # The pinned setup-soldr's last GITHUB_ENV write is `soldr env`'s
    # blanket `LINKER=clang` placeholder, which clobbers `soldr prepare`'s
    # lld-link and makes rustc drive `clang -flavor link` — a hard clang
    # error that killed both Linux-hosted MSVC wheel lanes of every
    # Auto Release attempt. The wheel build must always relink
    # through lld-link, even when the inherited env says otherwise.
    env = wheel.build_environment(target, {key: "clang"})
    assert env[key] == "lld-link"


def test_release_environment_leaves_non_msvc_linker_alone() -> None:
    key = "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER"
    env = wheel.build_environment("aarch64-apple-darwin", {key: "clang"})
    assert env[key] == "clang"


def test_source_install_is_pinned_and_forces_a_local_build() -> None:
    command = wheel.soldr_maturin_install_command(Path("venv-python"))
    assert command == [
        "uv",
        "pip",
        "install",
        "--python",
        "venv-python",
        "--no-cache",
        "--no-binary",
        "soldr-maturin",
        "soldr-maturin==1.14.1.post1",
        "patchelf; platform_system == 'Linux'",
    ]


def test_source_build_environment_removes_cross_target_state() -> None:
    env = wheel.source_build_environment(
        {
            "KEEP": "yes",
            "CARGO_BUILD_TARGET": "x86_64-pc-windows-msvc",
            "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER": "lld-link",
            "CC_x86_64_pc_windows_msvc": "clang-cl",
            "RUSTC_WRAPPER": "soldr",
            "MATURIN_USE_XWIN": "0",
        }
    )
    assert env == {
        "KEEP": "yes",
        "CARGO_BUILD_JOBS": "2",
        "RUSTUP_TOOLCHAIN": "1.98.1",
        "SOLDR_JOBS": "2",
    }


def test_source_build_environment_preserves_stricter_concurrency_limits() -> None:
    env = wheel.source_build_environment({"CARGO_BUILD_JOBS": "1", "SOLDR_JOBS": "1"})
    assert env["CARGO_BUILD_JOBS"] == "1"
    assert env["SOLDR_JOBS"] == "1"


@pytest.mark.parametrize(
    ("target", "compatibility"),
    [
        ("x86_64-pc-windows-msvc", "pypi"),
        ("aarch64-apple-darwin", "pypi"),
        ("x86_64-unknown-linux-gnu", "manylinux_2_17"),
    ],
)
def test_direct_maturin_command_is_release_locked(
    target: str, compatibility: str
) -> None:
    command = wheel.maturin_build_command(Path("maturin"), target)
    assert command == maturin_release_build_command("maturin", target, compatibility)


def test_environment_rejects_non_release_pep517_profile() -> None:
    with pytest.raises(ValueError, match="requires SOLDR_PEP517_PROFILE=release"):
        wheel.build_environment(
            "x86_64-unknown-linux-gnu", {"SOLDR_PEP517_PROFILE": "dev"}
        )


def test_unknown_target_is_rejected() -> None:
    with pytest.raises(ValueError, match="unsupported"):
        wheel.validate_target("riscv64gc-unknown-linux-gnu")


def test_hook_source_builds_downstream_then_runs_it_with_target_env(
    monkeypatch,
) -> None:
    observed = []

    def fake_run(command, *, env):
        observed.append((command, env))

    monkeypatch.setattr(wheel, "run", fake_run)
    monkeypatch.setattr(
        wheel, "resolve_soldr_driver", lambda _env: Path("release-driver")
    )
    monkeypatch.setattr(
        wheel,
        "resolve_toolchain_rustc",
        lambda _driver, _env: Path("toolchain/bin/rustc"),
    )
    wheel.run_hook(
        target="x86_64-pc-windows-msvc",
        hook="python -m build --wheel",
        base_env={"PATH": "/managed"},
    )

    assert observed[0][0] == [
        "release-driver",
        "toolchain",
        "link",
        "--shim-dir",
        str(wheel.SOLDR_TOOLCHAIN_SHIMS),
        "--force",
    ]
    assert observed[2][0] == wheel.soldr_maturin_install_command(
        wheel.venv_executable(wheel.SOLDR_MATURIN_VENV, "python")
    )
    assert observed[-1][0] == wheel.maturin_build_command(
        wheel.venv_executable(wheel.SOLDR_MATURIN_VENV, "maturin"),
        "x86_64-pc-windows-msvc",
    )
    assert observed[-1][1]["MATURIN_USE_XWIN"] == "0"
    assert Path(observed[-1][1]["CARGO"]).name in {"cargo", "cargo.exe"}


def test_github_env_loader_preserves_non_shell_identifier(tmp_path: Path) -> None:
    env_file = tmp_path / "github-env"
    env_file.write_text(
        "PKG_CONFIG_PATH_x86_64-unknown-linux-gnu=/managed/pkgconfig\n"
        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS=-C link-self-contained=no\n",
        encoding="utf-8",
    )
    assert wheel.read_github_env(env_file) == {
        "PKG_CONFIG_PATH_x86_64-unknown-linux-gnu": "/managed/pkgconfig",
        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS": "-C link-self-contained=no",
    }


def test_github_env_loader_rejects_malformed_record(tmp_path: Path) -> None:
    env_file = tmp_path / "github-env"
    env_file.write_text("missing-separator\n", encoding="utf-8")
    with pytest.raises(ValueError, match="invalid GitHub environment record"):
        wheel.read_github_env(env_file)
