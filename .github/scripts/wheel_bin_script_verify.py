#!/usr/bin/env python3
"""Verify a release wheel ships its bin script, extracted from
release-auto.yml (soldr#2469 2.2).

Structural well-formedness of the single wheel in ``dist/``:

- exactly one entry at the PEP 491 ``<distname>-<version>.data/scripts/<bin>``
  path (anchored to the wheel root so a nested look-alike never passes);
- the primary-bin payload beats a 2 MiB floor (soldr#1140: v0.7.87 shipped
  ~118 KB stubs on two lanes — a real soldr binary is ~15 MB, so anything
  under the floor means the crate's ``main.rs`` never got compiled in). On
  macOS, maturin's delocate-style repair makes the ``.data/scripts/*``
  entry a ~450-byte launcher that execs the real binary under
  ``soldr.scripts/``; either location may carry the payload.

Deliberately NOT SHA-compared against the release tarball binary: from
maturin 1.13.2 the wheel binary is post-processed on macOS (dylib load-path
rewrites), so it is intentionally byte-different. The follow-up wheel smoke
(`<binary> --version`) is the functional check; this one is structural.
"""

from __future__ import annotations

import argparse
import sys
import zipfile
from pathlib import Path

MIN_PRIMARY_BIN_BYTES = 2 * 1024 * 1024


def single_wheel(dist: Path) -> Path:
    wheels = sorted(dist.glob("*.whl"))
    if len(wheels) != 1:
        sys.exit(f"expected exactly one wheel, found {len(wheels)}: {wheels}")
    return wheels[0]


def script_entry_for(wheel: Path, binary_name: str) -> str:
    # PEP 491: the wheel filename's first two `-`-separated tokens are
    # `<distname>-<version>`, and the data directory is
    # `<distname>-<version>.data/`.
    parts = wheel.stem.split("-")
    if len(parts) < 2:
        sys.exit(f"unexpected wheel filename: {wheel.name}")
    return f"{parts[0]}-{parts[1]}.data/scripts/{binary_name}"


def verify(dist: Path, binary_name: str) -> None:
    wheel_path = single_wheel(dist)
    script_entry = script_entry_for(wheel_path, binary_name)
    with zipfile.ZipFile(wheel_path) as wheel:
        names = wheel.namelist()
        if names.count(script_entry) != 1:
            matches = [name for name in names if name == script_entry]
            sys.exit(
                f"expected exactly one `{script_entry}` entry in "
                f"{wheel_path.name}, found {matches}"
            )
        info = wheel.getinfo(script_entry)
        if info.file_size == 0:
            sys.exit(f"`{script_entry}` in {wheel_path.name} is empty")

        soldr_scripts_entry = f"soldr.scripts/{binary_name}"
        candidates = [(script_entry, info.file_size)]
        if soldr_scripts_entry in names:
            candidates.append(
                (soldr_scripts_entry, wheel.getinfo(soldr_scripts_entry).file_size)
            )
        largest = max(size for _, size in candidates)
        if largest < MIN_PRIMARY_BIN_BYTES:
            layout = "\n".join(f"  {path}: {size} bytes" for path, size in candidates)
            sys.exit(
                f"wheel ships {binary_name!r} at {largest} bytes across "
                f"{len(candidates)} candidate path(s):\n{layout}\n"
                f"expected >= {MIN_PRIMARY_BIN_BYTES} bytes (2 MiB) — a "
                f"real soldr binary compiled from crates/soldr-cli is "
                f"~15 MB. Anything below this floor means the crate's "
                f"main.rs did not get compiled into the shipped bin "
                f"(soldr#1140 — v0.7.87 stub regression). Do NOT ship "
                f"this wheel; investigate the maturin cargo invocation."
            )
        # Dump all script entries so post-mortems can see per-bin sizes
        # even when the assertion above passes.
        print("=== wheel scripts/ layout ===")
        for name in sorted(names):
            if ".data/scripts/" in name or name.startswith("soldr.scripts/"):
                print(f"  {wheel.getinfo(name).file_size:>10}  {name}")

    print(
        f"{wheel_path.name} ships {binary_name} at `{script_entry}` "
        f"({info.file_size} bytes); primary-bin payload >="
        f"{MIN_PRIMARY_BIN_BYTES} bytes OK"
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--dist", default="dist", type=Path)
    args = parser.parse_args(argv)
    verify(args.dist, args.binary)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
