#!/usr/bin/env python3
"""Run release builds that must use a native host toolchain.

The pinned Soldr remains the Rust front door, but ``soldr rustup run ...
cargo`` deliberately avoids repeating blessed target preparation. This is
required for native ARM64 musl (whose catalogue compiler is i386-hosted and
cannot execute on the native ARM64 runner it builds on).

soldr#3071: macOS release binaries (both `apple-darwin` triples) no longer
use this native path -- no GitHub Actions job may run on a macos-* runner, so
they cross-build through `build_matrix_binary` on the same ubuntu-24.04
runner as Windows, via soldr's blessed `soldr build` surface.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import stat
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

TOOLCHAIN = "1.98.1"
ARM64_MUSL = "aarch64-unknown-linux-musl"
MUSL_TARGETS = ("x86_64-unknown-linux-musl", ARM64_MUSL)
CONTRACT_PATH = (
    Path(__file__).resolve().parents[2] / "contracts" / "zccache-runtime.v1.json"
)
MATURIN_CONTRACT = json.loads(CONTRACT_PATH.read_text(encoding="utf-8"))["maturin"]
SOLDR_MATURIN_NO_BINARY = str(MATURIN_CONTRACT["pypi_package"])
SOLDR_MATURIN_REQUIREMENT = (
    f"{SOLDR_MATURIN_NO_BINARY}=={MATURIN_CONTRACT['managed_version']}"
)


def cargo_command(driver: Path, *args: str) -> list[str]:
    """Return a Cargo command routed through pinned Soldr and rustup."""

    return [str(driver), "rustup", "run", TOOLCHAIN, "cargo", *args]


def run(command: Sequence[str], *, env: Mapping[str, str] | None = None) -> None:
    print(f"release helper: $ {shlex.join(command)}", flush=True)
    subprocess.run(command, check=True, env=None if env is None else dict(env))


def soldr_cli_version(metadata: str) -> str:
    document = json.loads(metadata)
    versions = [
        package["version"]
        for package in document.get("packages", [])
        if package.get("name") == "soldr-cli"
    ]
    if len(versions) != 1:
        raise RuntimeError(f"expected one soldr-cli package, found {len(versions)}")
    return str(versions[0])


def release_build_environment(
    base: Mapping[str, str], *, target: str
) -> MappingProxyType[str, str]:
    env = dict(base)
    env.pop("RUSTC_WRAPPER", None)
    env.update(
        {
            "SOLDR_RELEASE_CI": "1",
            # soldr#786 linux+macOS follow-up: debug info generation is
            # controlled by `[profile.release] debug` in the workspace
            # Cargo.toml (`line-tables-only`). This used to force it back off
            # with CARGO_PROFILE_RELEASE_DEBUG=0 -- a profile env var
            # outranks the Cargo.toml setting, so that override would have
            # silently made the Cargo.toml change a no-op for the native
            # ARM64-musl and macOS release binaries built through this path.
            "CARGO_PROFILE_RELEASE_LTO": "thin",
            "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "1",
            "CARGO_BUILD_JOBS": "2",
            "SOLDR_JOBS": "2",
        }
    )
    if target.endswith("-apple-darwin"):
        # soldr#3038: [profile.release] defaults `split-debuginfo` to "off"
        # -- a post-link objcopy carve-out (stage_release_binaries.py::
        # stage_debug_symbols) replaced rustc's packed split on Linux,
        # because "packed" only sees the DWARF rustc itself compiles for
        # this build and left the precompiled std's and every `cc`-built C
        # dependency's DWARF embedded in the shipped binary AND duplicated
        # into the `.dwp`. macOS does not have that duplication problem:
        # `dsymutil` gathers debug info from the intermediate `.o` files via
        # N_OSO stabs, not from what ends up embedded in the linked binary,
        # so "packed" (which runs dsymutil) stays the native answer here.
        # See docs/DEBUG_SIDECARS.md.
        env["CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO"] = "packed"
    return MappingProxyType(env)


def build_binary(driver: Path, target: str) -> None:
    env = release_build_environment(os.environ, target=target)
    clean = cargo_command(
        driver, "clean", "-p", "soldr-cli", "--target", target, "--release"
    )
    print(f"release helper: $ {shlex.join(clean)}", flush=True)
    subprocess.run(
        clean,
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=env,
    )
    run(
        cargo_command(
            driver,
            "build",
            "--release",
            "--locked",
            "--target",
            target,
            "--package",
            "soldr-cli",
            "--bin",
            "soldr",
        ),
        env=env,
    )
    output = Path("target") / target / "release"
    if not (output / "soldr").is_file():
        raise RuntimeError(f"release binary was not produced under {output}")
    print(f"release helper: produced {output / 'soldr'}", flush=True)


def matrix_driver() -> Path:
    """The just-built release driver, by runner OS."""
    suffix = ".exe" if os.environ.get("RUNNER_OS") == "Windows" else ""
    return Path("target") / "release" / f"soldr{suffix}"


def restore_version_manifests() -> None:
    """Undo any manifest edits a previous step left behind.

    Best-effort exactly as the inline `|| true` was: on a fresh checkout
    there is nothing to restore and git exits non-zero, which is not a
    failure of this step.
    """
    subprocess.run(
        [
            "git",
            "restore",
            "--",
            "Cargo.toml",
            "Cargo.lock",
            "crates/soldr-cli/Cargo.toml",
        ],
        check=False,
    )


def build_matrix_binary(driver: Path, target: str) -> None:
    """Build the release binary for a matrix target through `soldr build`.

    Distinct from `build_binary` on purpose. That one drives
    `soldr rustup run <toolchain> cargo build --locked` and serves the one
    remaining native lane (ARM64 musl, whose catalogue compiler is
    i386-hosted and cannot execute on the native ARM64 runner it builds on)
    that must bypass target preparation. This one drives soldr's blessed
    `soldr build` surface, does *not* pass `--locked`, and runs
    `soldr prepare` for GNU Linux. Those differences are load-bearing for the
    targets this lane covers, so the two paths are extracted separately
    rather than merged -- unifying them would silently change what every
    release binary is built with. Whether they *should* converge is a
    question for soldr#2469 Phase 3, with the candidate workflow available to
    prove it.

    soldr#3071: both `apple-darwin` targets moved into this lane (Linux
    cross, same as Windows) when native macOS runners were removed from CI.
    Neither macOS SDK access nor Xcode CLT is available or needed here; the
    blessed build's bundled LLVM plus managed macOS SDK produce the binary
    entirely on the ubuntu-24.04 runner this lane already uses for every
    other target.

    The profile environment (`CARGO_PROFILE_RELEASE_*`, job bounds) stays in
    the workflow: those values are matrix expressions -- MSVC takes
    `lto=false`/`codegen-units=16` and everything else `thin`/`1` -- and
    moving them here would mean reimplementing the matrix in Python. The one
    exception is `CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO`, which only
    `apple-darwin` needs (soldr#3038 -- see `release_build_environment`'s
    docstring for why "packed" is macOS-native rather than a duplicate of the
    Linux post-link carve-out) and stays here rather than in the shared
    per-matrix-row `env:` block, which every non-darwin row would otherwise
    also have to spell out as a no-op.
    """
    restore_version_manifests()

    clean = [str(driver), "cargo", "clean", "-p", "soldr-cli", "--target", target]
    clean += ["--release"]
    print(f"release helper: $ {shlex.join(clean)}", flush=True)
    subprocess.run(
        clean, check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )

    env = dict(os.environ)
    if target.endswith("-apple-darwin"):
        env["CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO"] = "packed"

    # `--no-cache` avoids the resident embedded daemon behind the old
    # Linux-hosted cross-build memory collision.
    run(
        [
            str(driver),
            "--no-cache",
            "build",
            "--release",
            "--target",
            target,
            "--package",
            "soldr-cli",
            "--bin",
            "soldr",
        ],
        env=env,
    )

    if target.endswith("-unknown-linux-gnu"):
        github_env = os.environ.get("GITHUB_ENV")
        if github_env:
            run(
                [str(driver), "prepare", "--target", target, "--github-env", github_env]
            )

    output = Path("target") / target / "release"
    print(f"=== post-build diagnostic: {output}/ ===", flush=True)
    if output.is_dir():
        for entry in sorted(output.iterdir())[:20]:
            print(f"  {entry.name}", flush=True)
    else:
        print(f"  (missing: {output})", flush=True)
    print("===================================================", flush=True)


def wheel_environment(
    base: Mapping[str, str], *, driver: Path, cargo_bridge: Path
) -> MappingProxyType[str, str]:
    env = dict(base)
    env.pop("RUSTC_WRAPPER", None)
    env["SOLDR_RELEASE_DRIVER"] = str(driver)
    env["SOLDR_RELEASE_TOOLCHAIN"] = TOOLCHAIN
    env["CARGO"] = str(cargo_bridge)
    return MappingProxyType(env)


def resolve_toolchain_rustc(driver: Path, base: Mapping[str, str]) -> Path:
    """Resolve the pinned real rustc for host build-backend probes."""

    completed = run_captured(
        [str(driver), "rustup", "which", "rustc", "--toolchain", TOOLCHAIN],
        check=True,
        env=dict(base),
        capture_stdout=True,
        text=True,
    )
    rustc = Path(completed.stdout.strip())
    if not rustc.is_file():
        raise RuntimeError(f"Soldr resolved a missing rustc: {rustc}")
    return rustc


def host_tool_environment(
    base: Mapping[str, str], *, driver: Path, cargo_bridge: Path, rustc: Path
) -> MappingProxyType[str, str]:
    """Remove wheel-target state while source-building the host Maturin tool."""

    env = dict(wheel_environment(base, driver=driver, cargo_bridge=cargo_bridge))
    exact = {
        "AR",
        "CARGO_BUILD_TARGET",
        "CARGO_ENCODED_RUSTFLAGS",
        "CC",
        "CFLAGS",
        "CXX",
        "CXXFLAGS",
        "LDFLAGS",
        "RANLIB",
        "RUSTC",
        "RUSTFLAGS",
    }
    for key in list(env):
        if key in exact or key.startswith(("AR_", "CC_", "CXX_", "RANLIB_")):
            env.pop(key, None)
        elif key.startswith("CARGO_TARGET_"):
            env.pop(key, None)
    rustc = rustc.resolve()
    env["PATH"] = f"{rustc.parent}{os.pathsep}{env.get('PATH', '')}"
    env["RUSTC"] = str(rustc)
    env["RUSTUP_TOOLCHAIN"] = TOOLCHAIN
    # Preserve stricter limits supplied by constrained release lanes. The
    # defaults retain normal local/CI throughput when no caller limit exists.
    env.setdefault("CARGO_BUILD_JOBS", "2")
    env.setdefault("SOLDR_JOBS", "2")
    return MappingProxyType(env)


def build_musl_wheel(driver: Path, target: str, expected_version: str) -> None:
    if target not in MUSL_TARGETS:
        raise ValueError(f"not a release musl target: {target}")
    metadata = run_captured(
        cargo_command(
            driver,
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
            "crates/soldr-cli/Cargo.toml",
        ),
        check=True,
        capture_stdout=True,
        text=True,
    ).stdout
    actual_version = soldr_cli_version(metadata)
    if actual_version != expected_version:
        raise RuntimeError(
            f"soldr-cli version {actual_version!r} does not match release {expected_version!r}"
        )

    dist = Path("dist")
    dist.mkdir(exist_ok=True)
    for wheel in dist.glob("*.whl"):
        wheel.unlink()

    venv = Path(".venv-release-wheel")
    run(["uv", "python", "install", "3.13"])
    run(["uv", "venv", "--python", "3.13", "--clear", str(venv)])
    python = venv / "bin" / "python"

    cargo_bridge = Path(".github/scripts/cargo_via_soldr_rustup.sh").resolve()
    cargo_bridge.chmod(cargo_bridge.stat().st_mode | stat.S_IXUSR)
    rustc = resolve_toolchain_rustc(driver, os.environ)
    host_env = host_tool_environment(
        os.environ,
        driver=driver,
        cargo_bridge=cargo_bridge,
        rustc=rustc,
    )
    run(
        [
            "uv",
            "pip",
            "install",
            "--python",
            str(python),
            "--no-cache",
            "--no-binary",
            SOLDR_MATURIN_NO_BINARY,
            SOLDR_MATURIN_REQUIREMENT,
            "patchelf; platform_system == 'Linux'",
        ],
        env=host_env,
    )
    env = wheel_environment(os.environ, driver=driver, cargo_bridge=cargo_bridge)
    run(musl_wheel_maturin_command(venv / "bin" / "maturin", target), env=env)


def musl_wheel_maturin_command(maturin: Path, target: str) -> list[str]:
    """Return the locked direct build command for a musl release wheel.

    soldr#3038: `--strip` is load-bearing for wheel size here for the same
    reason as `build_release_wheel.py::maturin_build_command` (which this
    mirrors) -- `[profile.release]` deliberately keeps `strip = "none"` so
    the release archive's own packaging step gets an unstripped binary to
    carve debug symbols out of via `objcopy`. Left unstripped, this wheel
    would bundle that same unstripped binary; `--strip` tells maturin to
    strip its own copy at packaging time without touching the archive's
    separately-built one.
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
        "musllinux_1_2",
    ]


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    binary = subparsers.add_parser("binary")
    binary.add_argument("--driver", type=Path, required=True)
    binary.add_argument("--target", required=True)

    matrix_binary = subparsers.add_parser("matrix-binary")
    matrix_binary.add_argument("--target", required=True)

    wheel = subparsers.add_parser("musl-wheel")
    wheel.add_argument("--driver", type=Path, required=True)
    wheel.add_argument("--target", choices=MUSL_TARGETS, required=True)
    wheel.add_argument("--expected-version", required=True)

    args = parser.parse_args(argv)
    # `matrix-binary` derives its driver from RUNNER_OS rather than taking one:
    # the workflow previously spelled that `case "$RUNNER_OS" in Windows) ...`
    # at every call site, which is exactly the kind of duplication this
    # extraction exists to remove.
    driver = (
        matrix_driver() if args.command == "matrix-binary" else args.driver
    ).resolve()
    if not driver.is_file():
        parser.error(f"pinned Soldr driver does not exist: {driver}")
    if args.command == "matrix-binary":
        build_matrix_binary(driver, args.target)
    elif args.command == "binary":
        build_binary(driver, args.target)
    else:
        build_musl_wheel(driver, args.target, args.expected_version)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
