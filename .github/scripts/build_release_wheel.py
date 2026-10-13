#!/usr/bin/env python3
"""Build one release wheel with Soldr's source-built Maturin downstream.

setup-soldr prepares the compiler, linker, SDK, and sysroot and advertises a
generic ``python -m build --wheel`` hook. We validate that contract, then
source-build the pinned ``soldr-maturin`` sdist and invoke its ``maturin``
executable directly. This bootstraps the downstream even while the release
driver is an older Soldr that predates the managed-package integration.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import shutil
import subprocess
import sys
from collections.abc import Mapping, Sequence
from pathlib import Path
from types import MappingProxyType

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

RELEASE_TARGETS = frozenset(
    {
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
    }
)
CONTRACT_PATH = (
    Path(__file__).resolve().parents[2] / "contracts" / "zccache-runtime.v1.json"
)
MATURIN_CONTRACT = json.loads(CONTRACT_PATH.read_text(encoding="utf-8"))["maturin"]
SOLDR_MATURIN_PACKAGE = str(MATURIN_CONTRACT["pypi_package"])
SOLDR_MATURIN_VERSION = str(MATURIN_CONTRACT["managed_version"])
SOLDR_MATURIN_REQUIREMENT = f"{SOLDR_MATURIN_PACKAGE}=={SOLDR_MATURIN_VERSION}"
SOLDR_TOOLCHAIN = "1.98.1"
SOLDR_MATURIN_VENV = Path("target/soldr-maturin-release-env")
SOLDR_TOOLCHAIN_SHIMS = Path("target/soldr-maturin-release-shims")


def validate_target(target: str) -> None:
    """Reject targets outside the canonical release matrix."""

    if target not in RELEASE_TARGETS:
        raise ValueError(f"unsupported release wheel target: {target}")


def build_environment(
    target: str, base: Mapping[str, str]
) -> MappingProxyType[str, str]:
    """Copy *base* and enforce the release PEP 517 environment."""

    validate_target(target)
    env = dict(base)
    configured_profile = env.get("SOLDR_PEP517_PROFILE", "").strip()
    if configured_profile and configured_profile != "release":
        raise ValueError(
            f"release wheel requires SOLDR_PEP517_PROFILE=release, got {configured_profile!r}"
        )
    env["SOLDR_PEP517_PROFILE"] = "release"
    env["SOLDR_RELEASE_CI"] = "1"
    env.setdefault("CARGO_BUILD_JOBS", "2")
    env.setdefault("SOLDR_JOBS", "2")
    if target.endswith("-pc-windows-msvc"):
        env["MATURIN_USE_XWIN"] = "0"
        # The pinned setup-soldr re-exports `soldr env`'s blanket
        # `CARGO_TARGET_<T>_LINKER=clang` placeholder after `soldr prepare`
        # already exported the correct `lld-link`, and the later GITHUB_ENV
        # write wins. rustc then invokes `clang -flavor link <MSVC args>`,
        # which clang rejects (`unknown argument: '-flavor'`). Pin the
        # linker back to lld-link — it matches the exported
        # `-C linker-flavor=lld-link` RUSTFLAGS and resolves from the
        # managed LLVM bin dir `soldr prepare` put on PATH.
        triple_env = target.upper().replace("-", "_")
        env[f"CARGO_TARGET_{triple_env}_LINKER"] = "lld-link"
    return MappingProxyType(env)


def soldr_maturin_install_command(python: Path) -> list[str]:
    """Install the pinned downstream from its sdist without build caching."""

    return [
        "uv",
        "pip",
        "install",
        "--python",
        str(python),
        "--no-cache",
        "--no-binary",
        SOLDR_MATURIN_PACKAGE,
        SOLDR_MATURIN_REQUIREMENT,
        "patchelf; platform_system == 'Linux'",
    ]


def compatibility_for_target(target: str) -> str:
    """Return Maturin's release-wheel compatibility policy for *target*."""

    validate_target(target)
    if target.endswith("-unknown-linux-gnu"):
        return "manylinux_2_17"
    if target.endswith("-unknown-linux-musl"):
        return "musllinux_1_2"
    return "pypi"


def maturin_build_command(maturin: Path, target: str) -> list[str]:
    """Return the locked direct build command for the release target.

    soldr#3038: `--strip` is load-bearing for wheel size, not a nicety.
    `[profile.release]` deliberately keeps `strip = "none"` at the Cargo
    level -- the release archive's packaging step
    (`stage_release_binaries.py::stage_debug_symbols`) needs the fully
    linked, undisturbed binary as the input to its own `objcopy`/`strip`
    symbol carve-out. Left at that Cargo-level default, the wheel step
    would bundle the SAME unstripped binary maturin just linked --
    measured at soldr#3038: 36.7 MiB compressed, versus 9.8 MiB before this
    change. `--strip` tells maturin to strip its own copy at packaging
    time, independent of the Cargo profile and without touching the
    archive's separately-built binary at all (different `--target-dir`
    output, a wholly separate compile) -- measured with it: 10.3 MiB
    compressed, matching the historical wheel size. See
    docs/DEBUG_SIDECARS.md for the measured before/after.
    """

    return [
        str(maturin),
        "build",
        "--release",
        "--locked",
        "--strip",
        "--target",
        target,
        "--target-dir",
        "target",
        "--out",
        "dist",
        "--compatibility",
        compatibility_for_target(target),
    ]


def venv_executable(venv: Path, name: str) -> Path:
    """Return an executable path inside a platform-native virtualenv."""

    scripts = "Scripts" if os.name == "nt" else "bin"
    suffix = ".exe" if os.name == "nt" else ""
    return venv / scripts / f"{name}{suffix}"


def resolve_soldr_driver(env: Mapping[str, str]) -> Path:
    """Resolve the pinned host Soldr that owns source-build toolchain shims."""

    configured = env.get("SOLDR_RELEASE_DRIVER", "").strip()
    candidate = configured or shutil.which("soldr", path=env.get("PATH"))
    if not candidate:
        raise RuntimeError("could not resolve the pinned Soldr release driver")
    return Path(candidate)


def source_build_environment(
    base: Mapping[str, str],
) -> MappingProxyType[str, str]:
    """Strip cross-target state before compiling the host Maturin executable."""

    env = dict(base)
    exact = {
        "AR",
        "CARGO_BUILD_TARGET",
        "CARGO_ENCODED_RUSTFLAGS",
        "CC",
        "CFLAGS",
        "CXX",
        "CXXFLAGS",
        "LDFLAGS",
        "MATURIN_USE_XWIN",
        "PYO3_CONFIG_FILE",
        "PYO3_CROSS",
        "PYO3_CROSS_LIB_DIR",
        "PYO3_CROSS_PYTHON_VERSION",
        "RANLIB",
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTFLAGS",
        "_PYTHON_SYSCONFIGDATA_NAME",
    }
    for key in list(env):
        if key in exact or key.startswith(("AR_", "CC_", "CXX_", "RANLIB_")):
            env.pop(key, None)
        elif key.startswith("CARGO_TARGET_"):
            env.pop(key, None)
    env.setdefault("CARGO_BUILD_JOBS", "2")
    env.setdefault("SOLDR_JOBS", "2")
    env["RUSTUP_TOOLCHAIN"] = SOLDR_TOOLCHAIN
    return MappingProxyType(env)


def resolve_toolchain_rustc(driver: Path, env: Mapping[str, str]) -> Path:
    """Resolve rustc through the pinned Soldr without exposing its shim to probes."""

    completed = run_captured(
        [str(driver), "rustup", "which", "rustc"],
        check=True,
        env=dict(env),
        capture_stdout=True,
        text=True,
    )
    rustc = Path(completed.stdout.strip())
    if not rustc.is_file():
        raise RuntimeError(f"Soldr resolved a missing rustc: {rustc}")
    return rustc


def with_toolchain_shims(
    base: Mapping[str, str], shim_dir: Path, rustc: Path
) -> MappingProxyType[str, str]:
    """Route Cargo through Soldr while exposing its pinned real rustc to probes."""

    env = dict(base)
    shim_dir = shim_dir.resolve()
    rustc = rustc.resolve()
    env["PATH"] = (
        f"{rustc.parent}{os.pathsep}{shim_dir}{os.pathsep}{env.get('PATH', '')}"
    )
    suffix = ".exe" if os.name == "nt" else ""
    env["CARGO"] = str(shim_dir / f"cargo{suffix}")
    env["RUSTC"] = str(rustc)
    return MappingProxyType(env)


def run(command: Sequence[str], *, env: Mapping[str, str]) -> None:
    """Run one visible release helper command."""

    print(f"release wheel helper: $ {shlex.join(command)}", flush=True)
    subprocess.run(command, check=True, env=dict(env))


def read_github_env(path: Path) -> MappingProxyType[str, str]:
    """Read the simple ``NAME=value`` records emitted by ``soldr prepare``."""

    result: dict[str, str] = {}
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line:
            continue
        key, separator, value = line.partition("=")
        if not separator or not key:
            raise ValueError(f"invalid GitHub environment record at {path}:{number}")
        result[key] = value
    return MappingProxyType(result)


def run_hook(*, target: str, hook: str, base_env: Mapping[str, str]) -> None:
    command = shlex.split(hook)
    if command != ["python", "-m", "build", "--wheel"]:
        raise ValueError(f"unexpected setup-soldr target wheel hook: {hook!r}")
    target_env = build_environment(target, base_env)
    driver = resolve_soldr_driver(target_env)
    host_env = source_build_environment(target_env)
    run(
        [
            str(driver),
            "toolchain",
            "link",
            "--shim-dir",
            str(SOLDR_TOOLCHAIN_SHIMS),
            "--force",
        ],
        env=host_env,
    )
    rustc = resolve_toolchain_rustc(driver, host_env)
    host_env = with_toolchain_shims(host_env, SOLDR_TOOLCHAIN_SHIMS, rustc)
    run(
        [
            "uv",
            "venv",
            "--python",
            sys.executable,
            "--clear",
            str(SOLDR_MATURIN_VENV),
        ],
        env=host_env,
    )
    python = venv_executable(SOLDR_MATURIN_VENV, "python")
    run(soldr_maturin_install_command(python), env=host_env)
    run(
        [
            str(python),
            "-c",
            (
                "from importlib.metadata import version; "
                f"assert version('{SOLDR_MATURIN_PACKAGE}') == "
                f"'{SOLDR_MATURIN_VERSION}'"
            ),
        ],
        env=host_env,
    )
    maturin = venv_executable(SOLDR_MATURIN_VENV, "maturin")
    target_env = with_toolchain_shims(target_env, SOLDR_TOOLCHAIN_SHIMS, rustc)
    print(f"setup-soldr wheel target: {target}", flush=True)
    print("Soldr PEP 517 profile: release", flush=True)
    print(
        f"Maturin distribution: {SOLDR_MATURIN_REQUIREMENT} (source-built)", flush=True
    )
    run(maturin_build_command(maturin, target), env=target_env)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=sorted(RELEASE_TARGETS))
    parser.add_argument("--hook", required=True)
    parser.add_argument(
        "--github-env",
        type=Path,
        help="Optional soldr prepare --github-env file for local/Docker reproduction.",
    )
    args = parser.parse_args(argv)
    base_env = dict(os.environ)
    if args.github_env:
        base_env.update(read_github_env(args.github_env))
    run_hook(target=args.target, hook=args.hook, base_env=base_env)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
