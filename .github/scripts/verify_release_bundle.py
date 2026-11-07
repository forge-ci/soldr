#!/usr/bin/env python3
"""Run a portability ratchet over every executable in a release bundle.

A Soldr release archive ships four executables, not just ``soldr``: the daemon,
crgx, and cargo-chef are staged beside it.  Each target-specific portability
gate must inspect all four, or a prebuilt support binary can drift beyond its
published compatibility promise unnoticed.

This collects the staged files and delegates to the established per-format
verifiers.  Extracted from release-auto.yml for soldr#2469 step 2.2.

Usage (CI):
    python3 .github/scripts/verify_release_bundle.py \
        --target x86_64-unknown-linux-musl --check static
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

from release_artifacts import binary_suffix

BINARY_STEMS = ("soldr", "soldr-daemon", "crgx", "cargo-chef")
CHECKS = ("windows-imports", "macos-min-version", "static", "glibc-baseline")
SCRIPT_DIR = Path(__file__).parent


class BundleVerificationError(RuntimeError):
    """The staged package has no binaries for a portability verifier to inspect."""


def bundled_binaries(package_dir: Path, target: str) -> list[Path]:
    """Return existing top-level release executables in stable bundle order."""
    suffix = binary_suffix(target)
    binaries = [package_dir / f"{stem}{suffix}" for stem in BINARY_STEMS]
    present = [path for path in binaries if path.is_file()]
    if present:
        return present
    if package_dir.is_dir():
        listing = "\n".join(f"  {path.name}" for path in sorted(package_dir.iterdir()))
    else:
        listing = f"  {package_dir} does not exist"
    raise BundleVerificationError(
        f"no bundled binaries found in {package_dir} to verify; contents:\n{listing or '  (empty)'}"
    )


def checker_command(
    check: str, target: str, binaries: list[Path], max_glibc: str
) -> list[str]:
    """Build the exact established verifier command for one portability gate."""
    scripts = {
        "windows-imports": "verify_windows_imports.py",
        "macos-min-version": "verify_macos_min_version.py",
        "static": "verify_static_link.py",
        "glibc-baseline": "verify_glibc_baseline.py",
    }
    try:
        command = [sys.executable, str(SCRIPT_DIR / scripts[check])]
    except KeyError as error:
        raise BundleVerificationError(f"unknown bundle check: {check}") from error
    if check == "macos-min-version":
        command.extend(["--target", target])
    elif check == "glibc-baseline":
        command.extend(["--max-glibc", max_glibc])
    return [*command, *(str(binary) for binary in binaries)]


def verify_bundle(check: str, target: str, package_dir: Path, max_glibc: str) -> None:
    binaries = bundled_binaries(package_dir, target)
    print(f"verifying {check} for: {', '.join(path.name for path in binaries)}")
    subprocess.run(checker_command(check, target, binaries, max_glibc), check=True)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--check", required=True, choices=CHECKS)
    parser.add_argument("--package-dir", type=Path, default=Path("dist/package"))
    parser.add_argument("--max-glibc", default="2.39")
    args = parser.parse_args(argv)
    try:
        verify_bundle(args.check, args.target, args.package_dir, args.max_glibc)
    except (OSError, subprocess.CalledProcessError, BundleVerificationError) as error:
        print(str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
