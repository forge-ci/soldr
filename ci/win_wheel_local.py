#!/usr/bin/env python3
"""Build an installable win-x64 soldr wheel in Docker. Run it with no arguments.

    uv run --no-project python ci/win_wheel_local.py

Why this exists: a host whose soldr is wedged cannot build a replacement with
itself. This builds one on Linux, cross-compiled for Windows x64, driven by a
*released* soldr from PyPI, and leaves the wheel in `dist/win-wheel/` ready for
`uv pip install`.

Everything is baked in -- the Dockerfile is embedded below and the image is
built from stdin, so this one file is the whole harness. No context directory,
no second script, no long `docker run -v ... -v ...` line to retype.

Speed comes from two layers:

* **Bake time** -- the Rust toolchain, the Windows std, the blessed MSVC
  SDK/sysroot and maturin land in image layers, so a cold run downloads none of
  them. Measured at ~110s including the SDK.
* **Run time** -- `/root/.soldr` (which holds soldr's managed CARGO_HOME,
  RUSTUP_HOME, the SDK and the compiler cache) and `/work/target` are named
  volumes, so a warm rebuild compiles only what changed.

Dev profile on purpose: this is a recovery/test harness, so build time matters
more than binary size. `--release` is there for the rare case where the shipped
shape is what needs reproducing. Release wheels come from `auto-release.yml`.

Options, all optional::

    --release        build the release profile instead of dev
    --rebuild-image  bake the image from scratch (ignores the layer cache)
    --status         show the image and volumes, then exit
    --wipe           remove the warm volumes (the only destructive action)
"""

from __future__ import annotations

import argparse
import hashlib
import os
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

# --- baked-in configuration -------------------------------------------------

REPO_ROOT = Path(__file__).resolve().parents[1]
OUT_DIR = REPO_ROOT / "dist" / "win-wheel"
IMAGE = "soldr-win-wheel"
TARGET = "x86_64-pc-windows-msvc"
RUST_TOOLCHAIN = "1.98.1"

# The soldr that DRIVES the build is the latest published release. Only
# setup-soldr's default pins a soldr version; a pin here drifts stale and an
# old driver is exactly what wedged hosts before (0.9.0's broker could not dial
# its daemon route).

# `soldr wheel` refuses this workspace ("could not read Cargo metadata, so
# soldr cannot prove this workspace is abi3-safe") even though `soldr cargo
# metadata` succeeds. Its own error names the supported alternative, which is
# what we use.
BUILD_COMMAND = "soldr maturin build --target {target} --out /out {profile}"

DOCKERFILE = f"""
FROM python:3.13-slim-bookworm

# clang/lld back the MSVC link path; git is needed for the vendored submodules
# and cargo's git dependencies.
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \\
    --mount=type=cache,target=/var/lib/apt/lists,sharing=locked \\
    apt-get -o Acquire::Retries=5 update \\
    && apt-get -o Acquire::Retries=5 install -y --no-install-recommends \\
        ca-certificates clang curl git lld llvm pkg-config xz-utils

COPY --from=ghcr.io/astral-sh/uv:0.9.18 /uv /uvx /usr/local/bin/

RUN --mount=type=cache,target=/root/.cache/uv \\
    uv pip install --system --break-system-packages soldr

# Bake the toolchain and the target lifecycle so a cold run downloads nothing.
# `--allow-unpinned` is required and honest here: there is no
# `rust-toolchain.toml` in the image, so soldr is told explicitly that
# resolving rustc from PATH is intended at bake time. At run time the repo is
# mounted and its pin applies again. soldr's own `prepare --help` calls out
# this docker-bake shape.
RUN soldr rustup toolchain install {RUST_TOOLCHAIN} --profile minimal --no-self-update \\
    && soldr rustup default {RUST_TOOLCHAIN} \\
    && soldr rustup target add {TARGET} --toolchain {RUST_TOOLCHAIN} \\
    && soldr prepare --target {TARGET} --allow-unpinned

WORKDIR /work
"""

# --- plumbing ---------------------------------------------------------------


@dataclass(frozen=True)
class VolumeNames:
    """The two named Docker volumes that carry a checkout's warm state."""

    soldr: str
    target: str

    def all(self) -> list[str]:
        return [self.soldr, self.target]


def volume_names() -> VolumeNames:
    """Per-checkout volume names, so sibling clones keep separate warm trees.

    Note neither is `~/.cargo` or `~/.rustup`: soldr keeps its managed
    CARGO_HOME and RUSTUP_HOME *inside* its own home, so one volume at
    `/root/.soldr` carries the registry, toolchain, SDK and compiler cache.
    """
    suffix = hashlib.sha256(str(REPO_ROOT).encode("utf-8")).hexdigest()[:10]
    return VolumeNames(f"{IMAGE}-soldr-{suffix}", f"{IMAGE}-target-{suffix}")


def run(command: list[str], *, stdin: str | None = None) -> int:
    print(f"+ {' '.join(command)}", flush=True)
    return subprocess.run(
        command,
        check=False,
        input=stdin.encode("utf-8") if stdin is not None else None,
    ).returncode


def docker_available() -> bool:
    try:
        return run(["docker", "version", "--format", "{{.Server.Version}}"]) == 0
    except FileNotFoundError:
        return False


def build_image(*, rebuild: bool) -> int:
    """Build from stdin so the harness needs no context directory."""
    command = ["docker", "build", "-t", IMAGE, "-"]
    if rebuild:
        command.insert(2, "--no-cache")
    return run(command, stdin=DOCKERFILE)


def build_wheel(*, release: bool) -> int:
    volumes = volume_names()
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    for name in volumes.all():
        run(["docker", "volume", "create", name])

    profile = "--release" if release else ""
    inner = BUILD_COMMAND.format(target=TARGET, profile=profile).strip()
    return run(
        [
            "docker",
            "run",
            "--rm",
            # Bind-mounted, never COPYied, so an edit is visible immediately
            # and no image layer is invalidated by a source change.
            "-v",
            f"{REPO_ROOT}:/work",
            "-v",
            f"{OUT_DIR}:/out",
            "-v",
            f"{volumes.target}:/work/target",
            # Seeded from the image on first mount, so the baked toolchain and
            # SDK survive into the volume and stay warm from then on.
            "-v",
            f"{volumes.soldr}:/root/.soldr",
            # Bounded on purpose: Docker Desktop's memory cap OOM-kills an
            # unbounded parallel rustc fleet mid-build (observed at 7 jobs;
            # soldr#2453 names the signature). 2 matches the release wheel
            # lane's budget. Override via the same env vars if your Docker
            # has more memory.
            "-e",
            f"CARGO_BUILD_JOBS={os.environ.get('CARGO_BUILD_JOBS', '2')}",
            "-e",
            f"SOLDR_JOBS={os.environ.get('SOLDR_JOBS', '2')}",
            "-w",
            "/work",
            IMAGE,
            "bash",
            "-lc",
            inner,
        ]
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--rebuild-image", action="store_true")
    parser.add_argument("--status", action="store_true")
    parser.add_argument("--wipe", action="store_true")
    args = parser.parse_args(argv)

    if not docker_available():
        print(
            "win_wheel_local: docker is not available. Start Docker Desktop (or the "
            "daemon) and retry.",
            file=sys.stderr,
        )
        return 2

    if args.status:
        run(["docker", "images", IMAGE])
        run(["docker", "volume", "ls", "--filter", f"name={IMAGE}-"])
        return 0

    if args.wipe:
        return run(["docker", "volume", "rm", "--force", *volume_names().all()])

    before = (
        {path.name for path in OUT_DIR.glob("*.whl")} if OUT_DIR.is_dir() else set()
    )

    code = build_image(rebuild=args.rebuild_image)
    if code != 0:
        return code
    code = build_wheel(release=args.release)
    if code != 0:
        return code

    produced = sorted(path for path in OUT_DIR.glob("*.whl") if path.name not in before)
    if not produced:
        # A zero exit with no new wheel means the build silently produced
        # nothing; say so rather than reporting success.
        print(
            f"win_wheel_local: build succeeded but no new wheel appeared in {OUT_DIR}",
            file=sys.stderr,
        )
        return 1

    print()
    for wheel in produced:
        print(f"win_wheel_local: {wheel} ({wheel.stat().st_size} bytes)")
    print("\ninstall it with:")
    print(f'  uv pip install --python <interpreter> "{produced[0]}"')
    return 0


if __name__ == "__main__":
    sys.exit(main())
