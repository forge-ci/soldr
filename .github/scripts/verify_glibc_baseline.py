#!/usr/bin/env python3
"""Verify a glibc release binary's symbol-version floor (soldr#1060).

The `-unknown-linux-gnu` artifacts exist for one reason: to be the fallback
for users whose distro is too old for the musl build to be wanted, or who hit
a musl edge case. soldr#1060 says so directly, and names glibc 2.17 as the
target baseline -- Rust's Tier 1 minimum and the `manylinux_2_17` floor.

The RFC also warns, in as many words: *"do not build on modern Ubuntu and call
it portable."* That is exactly what happens today. `release-auto.yml` builds
these binaries natively on `ubuntu-24.04`, so they carry a `GLIBC_2.39`
requirement and refuse to start on RHEL 9 (2.34), Debian 12 (2.36) or
Ubuntu 22.04 (2.35) -- measured on the published v0.8.29 artifacts, both
x86_64 and aarch64.

Nothing detected that, because a too-new floor is invisible everywhere except
the old distro the artifact exists to serve. This script makes the floor a
number in the build log and a ratchet: it cannot silently get worse while the
real fix (linking against a 2.17 baseline via zigbuild or a container) is
outstanding.

Usage:
    python3 .github/scripts/verify_glibc_baseline.py --max-glibc 2.17 <binary>...

Exit codes:
  0 - every binary's floor is at or below --max-glibc
  1 - a binary requires a newer glibc, or could not be inspected
"""

from __future__ import annotations

import argparse
import re
import shutil
import sys
from pathlib import Path

# Dependency-free capture also works when this script is invoked by absolute path.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

# `readelf -V` lists each requirement as `Name: GLIBC_2.14` inside the
# version-needs section. Anchoring on `Name:` keeps version *definitions*
# (which appear when inspecting libc itself) out of the requirement set.
_NEEDED_RE = re.compile(r"Name:\s*GLIBC_([0-9][0-9.]*)")
# Fallback for binutils wording drift. Deliberately kept as a fallback rather
# than the primary: if the precise parse ever stops matching, we must not
# silently conclude "no requirement", which would read as a pass.
_ANY_RE = re.compile(r"GLIBC_([0-9][0-9.]*)")


def parse_version(text: str) -> "tuple[int, ...]":
    """`"2.17"` -> `(2, 17)`. Trailing dots and empty parts are tolerated so a
    malformed token cannot raise mid-scan."""
    parts = [p for p in text.strip().split(".") if p != ""]
    return tuple(int(p) for p in parts if p.isdigit())


def format_version(version: "tuple[int, ...]") -> str:
    return ".".join(str(part) for part in version)


def glibc_requirements(readelf_output: str) -> "list[tuple[int, ...]]":
    """Every GLIBC symbol version the binary requires.

    Pure, so the decision is unit-tested without binutils or a real ELF.
    """
    matches = _NEEDED_RE.findall(readelf_output)
    if not matches:
        matches = _ANY_RE.findall(readelf_output)
    versions = [parse_version(m) for m in matches]
    return sorted({v for v in versions if v})


def max_glibc_requirement(readelf_output: str) -> "tuple[int, ...] | None":
    """The highest required GLIBC version, or None when the binary requires
    no glibc symbols at all (a static or non-glibc binary)."""
    versions = glibc_requirements(readelf_output)
    return versions[-1] if versions else None


def _readelf_versions(binary: str) -> "tuple[int, str]":
    """Run `readelf -V`, returning `(exit_code, combined_output)`.

    The exit code is load-bearing: readelf exits non-zero when it cannot read
    the file at all. Treating that as ordinary output would let a missing path
    parse as "no GLIBC requirements found" and report a pass -- the same
    wrong-reason pass that `verify_static_link` was fixed for.
    """
    tool = shutil.which("readelf") or shutil.which("llvm-readelf")
    if tool is None:
        raise FileNotFoundError("readelf not found on PATH")
    completed = run_captured(
        [tool, "-V", binary],
        capture_output=True,
        text=True,
        check=False,
    )
    return completed.returncode, completed.stdout + completed.stderr


def main(argv: "list[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", nargs="+", help="ELF binaries to inspect")
    parser.add_argument(
        "--max-glibc",
        default="2.17",
        help=(
            "Highest glibc symbol version the binary may require. Defaults to "
            "the soldr#1060 target of 2.17; CI passes the current measured "
            "floor so the value ratchets rather than blocking a release."
        ),
    )
    args = parser.parse_args(argv)

    ceiling = parse_version(args.max_glibc)
    if not ceiling:
        print(
            f"verify_glibc_baseline: --max-glibc {args.max_glibc!r} is not a version",
            file=sys.stderr,
        )
        return 1

    failures = 0
    for binary in args.binaries:
        try:
            code, output = _readelf_versions(binary)
        except (OSError, FileNotFoundError) as error:
            print(
                f"verify_glibc_baseline: cannot inspect {binary}: {error}",
                file=sys.stderr,
            )
            failures += 1
            continue

        if code != 0:
            last = output.strip().splitlines()[-1] if output.strip() else "no output"
            print(
                f"verify_glibc_baseline: cannot inspect {binary}: readelf exited {code}: {last}",
                file=sys.stderr,
            )
            failures += 1
            continue

        required = max_glibc_requirement(output)
        if required is None:
            print(
                f"verify_glibc_baseline: {binary} requires no glibc symbols "
                f"(static or non-glibc) - OK"
            )
            continue

        if required <= ceiling:
            print(
                f"verify_glibc_baseline: {binary} needs at most "
                f"GLIBC_{format_version(required)} (ceiling "
                f"{format_version(ceiling)}) - OK"
            )
            continue

        failures += 1
        print(
            f"verify_glibc_baseline: {binary} requires "
            f"GLIBC_{format_version(required)}, above the "
            f"{format_version(ceiling)} ceiling (soldr#1060).",
            file=sys.stderr,
        )
        print(
            "  This binary is the fallback for users whose glibc is OLD. A "
            "floor above the ceiling means it cannot start on the distros it "
            "exists to serve.",
            file=sys.stderr,
        )
        print(
            "  Fix by linking against the baseline (cargo zigbuild --target "
            "<triple>.2.17, or an old-glibc container) rather than raising "
            "the ceiling.",
            file=sys.stderr,
        )

    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
