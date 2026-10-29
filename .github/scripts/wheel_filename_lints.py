#!/usr/bin/env python3
"""Release wheel-filename lints, extracted from auto-release.yml (soldr#2469 2.2).

Three independently-runnable gates over ``dist/*.whl``:

``version``
    soldr#1083 follow-up: catch wrong-versioned wheels (e.g. the
    0.0.1-tagged stubs from the v0.7.72 release that aborted PyPI upload)
    BEFORE they leave the build job and poison the publish-pypi twine step.
    PEP 491: the wheel filename is ``<distname>-<version>-...``, so the
    version is field 2.

``manylinux``
    soldr#1005: maturin quietly falling back to the runner glibc tags the
    wheel manylinux_2_39; it still builds and still smoke-tests on the
    runner, and only breaks downstream when pip on glibc<2.39 skips it and
    installs the 0.1.0 placeholder sdist. Every linux-gnu wheel must carry
    the manylinux_2_17 tag (the catalogue-backed Maturin target floor).

``musllinux``
    soldr#909: every linux-musl wheel must carry musllinux_1_2. A native
    Linux tag makes pip on Alpine skip the binary wheel and fall back to a
    source build.

Usage:
    wheel_filename_lints.py version --expected-version vX.Y.Z [--dist DIR]
    wheel_filename_lints.py manylinux [--dist DIR]
    wheel_filename_lints.py musllinux [--dist DIR]
"""

from __future__ import annotations

import argparse
import sys
from dataclasses import dataclass
from pathlib import Path


def collect_wheels(dist: Path) -> list[Path]:
    wheels = sorted(dist.glob("*.whl"))
    if not wheels:
        sys.exit(f"no wheels in {dist}/ — maturin produced nothing?")
    return wheels


@dataclass(frozen=True)
class VersionFailure:
    """A wheel whose filename does not carry the expected version."""

    name: str
    why: str


def version_failures(wheels: list[Path], expected: str) -> list[VersionFailure]:
    bad: list[VersionFailure] = []
    for wheel in wheels:
        parts = wheel.stem.split("-")
        if len(parts) < 2:
            bad.append(VersionFailure(wheel.name, "filename has no version field"))
            continue
        wheel_version = parts[1]
        if wheel_version != expected:
            bad.append(
                VersionFailure(
                    wheel.name, f"version {wheel_version!r} != expected {expected!r}"
                )
            )
    return bad


def lint_version(dist: Path, expected_version: str) -> None:
    # The prepare job emits `vX.Y.Z`; wheel filenames use `X.Y.Z`.
    expected = expected_version.lstrip("v")
    wheels = collect_wheels(dist)
    bad = version_failures(wheels, expected)
    if bad:
        msg = ["wheel version lint FAILED:"]
        msg.extend(f"  - {failure.name}: {failure.why}" for failure in bad)
        msg.append("")
        msg.append(
            f"Every wheel in dist/ must be tagged {expected!r} to match "
            f"[workspace.package].version in Cargo.toml. A wrong-versioned "
            f"wheel here would cause twine to abort the whole PyPI publish "
            f"on its first '400 File already exists' (see soldr#1083 + the "
            f"v0.7.72 release log)."
        )
        sys.exit("\n".join(msg))
    print(f"wheel version lint OK: {len(wheels)} wheel(s) all tagged {expected!r}:")
    for wheel in wheels:
        print(f"  - {wheel.name}")


def manylinux_failures(wheels: list[Path]) -> list[str]:
    return [wheel.name for wheel in wheels if "manylinux_2_17" not in wheel.name]


def lint_manylinux(dist: Path) -> None:
    wheels = collect_wheels(dist)
    bad = manylinux_failures(wheels)
    if bad:
        msg = ["manylinux_2_17 tag assertion FAILED:"]
        msg.extend(f"  - {name}" for name in bad)
        msg.append("")
        msg.append(
            "Every linux-gnu wheel must carry the manylinux_2_17 "
            "platform tag (glibc 2.17 floor via the catalogue-backed "
            "Maturin target environment). A higher tag "
            "(e.g. manylinux_2_39 from the runner glibc) makes "
            "`pip install soldr` on older glibc fall back to the "
            "0.1.0 placeholder sdist — the soldr#1005 trap. Check "
            "that `soldr prepare --github-env` exported the target "
            "environment before Maturin ran."
        )
        sys.exit("\n".join(msg))
    print(f"manylinux_2_17 tag assertion OK for {len(wheels)} wheel(s)")


def musllinux_failures(wheels: list[Path]) -> list[str]:
    return [wheel.name for wheel in wheels if "musllinux_1_2" not in wheel.name]


def lint_musllinux(dist: Path) -> None:
    wheels = collect_wheels(dist)
    bad = musllinux_failures(wheels)
    if bad:
        msg = ["musllinux_1_2 tag assertion FAILED:"]
        msg.extend(f"  - {name}" for name in bad)
        msg.append("")
        msg.append(
            "Every linux-musl wheel must carry the musllinux_1_2 "
            "platform tag (PEP 656). A native linux tag or missing "
            "musllinux tag makes `pip install soldr` on Alpine skip "
            "the binary wheel and fall back to source build, which is "
            "the soldr#909 regression."
        )
        sys.exit("\n".join(msg))
    print(f"musllinux_1_2 tag assertion OK for {len(wheels)} wheel(s)")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("gate", choices=["version", "manylinux", "musllinux"])
    parser.add_argument("--expected-version", default=None)
    parser.add_argument("--dist", default="dist", type=Path)
    args = parser.parse_args(argv)
    if args.gate == "version":
        if not args.expected_version:
            parser.error("version gate requires --expected-version")
        lint_version(args.dist, args.expected_version)
    elif args.gate == "manylinux":
        lint_manylinux(args.dist)
    else:
        lint_musllinux(args.dist)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
