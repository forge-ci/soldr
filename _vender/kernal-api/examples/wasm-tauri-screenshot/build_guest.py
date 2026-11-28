#!/usr/bin/env python3
"""Build and admit the screenshot Wasm guest. The caller owns build storage.

Usage: ``python examples/wasm-tauri-screenshot/build_guest.py
[--trap-after-capture | --block-after-capture] [--prepare-target-only]
[--target <triple>]``

``CARGO_TARGET_DIR`` must name absolute, caller-managed writable storage.
Prints the admitted artifact's path as the last stdout line.

The guest uses the same pinned threaded build lane, through Soldr and its
cache, as the real Rust threaded-smoke artifact; never substitute a
hand-authored Wasm fixture. Restored toolchain caches can keep rustup's
component bookkeeping while omitting the target's libraries, so the target is
verified, and repaired if needed, by ci/rust_target.py before any build.
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

EXAMPLE = Path(__file__).resolve().parent
REPO = EXAMPLE.parent.parent
GUEST = EXAMPLE / "guest"
DEFAULT_TARGET = "wasm32-wasip1-threads"
GUEST_NAME = "kernal-api-wasm-tauri-guest"


class BuildError(RuntimeError):
    """The guest cannot be built; the message says why."""


@dataclass(frozen=True)
class Variant:
    """One guest flavour: its target-dir suffix and optional proof feature."""

    suffix: str
    feature: str | None


VARIANTS = {
    None: Variant("", None),
    "trap": Variant("-trap", "proof-trap-after-capture"),
    "block": Variant("-block", "proof-block-after-capture"),
}


def run(arguments: list[str], *, cwd: Path | None = None, env: dict[str, str] | None = None) -> None:
    returncode = subprocess.run(arguments, cwd=cwd, env=env, check=False, stdout=sys.stderr).returncode
    if returncode != 0:
        raise BuildError(f"{' '.join(arguments)} exited {returncode}")


def guest_target_dir(variant: Variant) -> Path:
    root = os.environ.get("CARGO_TARGET_DIR", "")
    if not root or not Path(root).is_absolute():
        raise BuildError("CARGO_TARGET_DIR must name absolute caller-managed writable storage")
    return Path(root) / f"{GUEST_NAME}{variant.suffix}"


def prepare_target(target: str) -> None:
    # Run from the guest so its toolchain selection applies, as the build does.
    run([sys.executable, str(REPO / "ci" / "rust_target.py"), target], cwd=GUEST)


def build(variant: Variant, target: str) -> Path:
    target_dir = guest_target_dir(variant)
    command = [
        "soldr",
        "cargo",
        "build",
        "--locked",
        "--manifest-path",
        "Cargo.toml",
        "--target",
        target,
        "--release",
        "--target-dir",
        str(target_dir),
    ]
    if variant.feature:
        command += ["--features", variant.feature]
    run(command, cwd=GUEST, env=dict(os.environ, SOLDR_LINKER="default"))
    release = target_dir / target / "release"
    built = release / f"{GUEST_NAME}.wasm"
    admitted = release / f"{GUEST_NAME}.admitted.wasm"
    # Keep Cargo's output pristine; only the admitted copy carries metadata.
    shutil.copyfile(built, admitted)
    run(
        [
            "soldr",
            "cargo",
            "run",
            "--locked",
            "--manifest-path",
            str(REPO / "tools" / "wasm-abi-generator" / "Cargo.toml"),
            "--",
            "--embed-threaded-metadata",
            str(admitted),
        ]
    )
    return admitted


def parse(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    fault = parser.add_mutually_exclusive_group()
    fault.add_argument("--trap-after-capture", dest="fault", action="store_const", const="trap")
    fault.add_argument("--block-after-capture", dest="fault", action="store_const", const="block")
    parser.add_argument("--prepare-target-only", action="store_true")
    parser.add_argument("--target", default=DEFAULT_TARGET)
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse(argv)
    try:
        if not args.prepare_target_only and args.target != DEFAULT_TARGET:
            raise BuildError(f"The screenshot guest builds only for {DEFAULT_TARGET}")
        variant = VARIANTS[args.fault]
        guest_target_dir(variant)  # fail before touching the toolchain
        prepare_target(args.target)
        # Reuse the exact target repair for other guests without building.
        if args.prepare_target_only:
            return 0
        admitted = build(variant, args.target)
    except BuildError as error:
        print(error, file=sys.stderr)
        return 1
    print(admitted)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
