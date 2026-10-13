#!/usr/bin/env python3
"""Source-install the canonical Soldr Maturin downstream for CI."""

from __future__ import annotations

import json
import os
import shlex
import shutil
import subprocess
import sys
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

REPO_ROOT = Path(__file__).resolve().parents[2]
CONTRACT_PATH = REPO_ROOT / "contracts" / "zccache-runtime.v1.json"
SOLDR_TOOLCHAIN = "1.98.1"
SOLDR_TOOLCHAIN_SHIMS = Path("target/soldr-maturin-ci-shims")


@dataclass(frozen=True)
class Distribution:
    """The canonical PyPI distribution and its managed version."""

    package: str
    version: str


def distribution() -> Distribution:
    """Return the canonical PyPI distribution and managed version."""

    contract = json.loads(CONTRACT_PATH.read_text(encoding="utf-8"))["maturin"]
    return Distribution(str(contract["pypi_package"]), str(contract["managed_version"]))


def install_command(python: Path) -> list[str]:
    """Return the uncached sdist-only install command."""

    dist = distribution()
    package, version = dist.package, dist.version
    return [
        str(python),
        "-m",
        "pip",
        "install",
        "--no-cache-dir",
        "--no-binary",
        package,
        f"{package}=={version}",
    ]


def source_build_environment(
    base: Mapping[str, str], *, rustc: Path, cargo: Path
) -> dict:
    """Expose real rustc to probes while routing Cargo through Soldr.

    soldr#3123: the CI smoke installer runs the sdist compile at the
    runner's full width; the release scripts keep their own cap.
    """

    env = dict(base)
    for key in list(env):
        if key in {
            "CARGO_BUILD_TARGET",
            "CARGO_ENCODED_RUSTFLAGS",
            "MATURIN_USE_XWIN",
            "RUSTC",
            "RUSTC_WRAPPER",
            "RUSTFLAGS",
        } or key.startswith("CARGO_TARGET_"):
            env.pop(key, None)
    rustc = rustc.resolve()
    cargo = cargo.resolve()
    env.update(
        {
            "CARGO": str(cargo),
            "PATH": f"{rustc.parent}{os.pathsep}{cargo.parent}{os.pathsep}{env.get('PATH', '')}",
            "RUSTC": str(rustc),
            "RUSTUP_TOOLCHAIN": SOLDR_TOOLCHAIN,
        }
    )
    return env


def run(command: Sequence[str], *, env: Mapping[str, str]) -> None:
    print(f"soldr-maturin installer: $ {shlex.join(command)}", flush=True)
    subprocess.run(command, check=True, env=dict(env))


def main() -> int:
    base_env = dict(os.environ)
    configured = base_env.get("SOLDR_RELEASE_DRIVER", "").strip()
    candidate = configured or shutil.which("soldr", path=base_env.get("PATH"))
    if not candidate:
        raise RuntimeError("could not resolve Soldr for Maturin source installation")
    driver = Path(candidate)
    run(
        [
            str(driver),
            "toolchain",
            "link",
            "--shim-dir",
            str(SOLDR_TOOLCHAIN_SHIMS),
            "--force",
        ],
        env=base_env,
    )
    completed = run_captured(
        [str(driver), "rustup", "which", "rustc"],
        check=True,
        env=base_env,
        capture_stdout=True,
        text=True,
    )
    rustc = Path(completed.stdout.strip())
    if not rustc.is_file():
        raise RuntimeError(f"Soldr resolved a missing rustc: {rustc}")
    suffix = ".exe" if os.name == "nt" else ""
    cargo = SOLDR_TOOLCHAIN_SHIMS / f"cargo{suffix}"
    env = source_build_environment(base_env, rustc=rustc, cargo=cargo)
    python = Path(sys.executable)
    run(install_command(python), env=env)
    dist = distribution()
    package, version = dist.package, dist.version
    run(
        [
            str(python),
            "-c",
            (
                f"from importlib.metadata import version; assert version('{package}') == '{version}'"
            ),
        ],
        env=env,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
