#!/usr/bin/env python3
"""Keep the per-PR glibc ceilings in agreement (soldr#2145).

Three workflow steps assert the glibc floor of a binary **soldr itself
builds**, and they must agree — they are three measurements of one fact:

  _ci-cross-build-linux.yml        aarch64 cross-build, debug profile
  cross-compile-all-targets.yml    x86_64 host-native, release profile
  cross-compile-all-targets.yml    aarch64 cross-build, release profile

They arrived in three separate PRs (#2145's lane, then soldr#2163, then
soldr#2165) and nothing tied them together, so one could be tightened or
loosened alone and the others would keep reporting the old number.

`release-auto.yml` has **two** ceilings, and excluding the whole file
(as this script used to) silently dropped one of them:

  :902  the *bundled* archive — `soldr`, `soldr-daemon`, `crgx`,
        `cargo-chef`. `crgx` and `cargo-chef` are fetched prebuilt from
        the soldr-toolchain catalogue, and the published v0.8.30 bundle
        measures both at 2.39, so this ceiling is pinned by third-party
        build choices. soldr#2170 tried to lower it and was closed for
        exactly this. Genuinely out of scope.

  :499  `target/<triple>/release/<binary>` — **soldr's own build**, the
        same fact the three per-PR ceilings measure. It is not out of
        scope, and as of soldr#1060's glibc-2.17 pass it agrees with them:
        both -gnu legs build through the blessed `soldr build` surface
        (`native_release_build.py::build_matrix_binary`), the same
        managed gcc-13.3.0-glibc-2.17-sysroot path the per-PR lanes
        exercise, so there is no longer a reason for this one to be
        looser.

That agreement was reached deliberately: soldr#2145/#2609 established
2.17 is achievable for soldr's own binaries via the managed sysroot, and
this release-lane step was the one remaining place still pinned to the
pre-fix 2.39 measurement. So it is asserted here against an explicit
expected value, the same way the other three are pooled.

If a future change regresses the release lane's own-binary floor above
2.17, raise `RELEASE_OWN_BINARY_CEILING` deliberately, with a note
explaining why, rather than by reflex.

`verify_wheel_glibc.py` is a different script against different
artifacts (manylinux wheels, 2.17) and is out of scope.

Usage:
  python3 .github/scripts/check_glibc_ceilings.py [--repo-root PATH]

Exit codes:
  0 — every own-binary ceiling agrees
  1 — they disagree, or an expected invocation vanished
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

SCRIPT = "verify_glibc_baseline.py"

# Workflows whose `verify_glibc_baseline.py` invocations gate a binary soldr
# builds and must all carry the same ceiling. `release-auto.yml` is handled
# separately by `check_release_ceilings` rather than skipped — its own-binary
# ceiling is knowingly looser, so it is pinned instead of pooled.
OWN_BINARY_WORKFLOWS = (
    ".github/workflows/_ci-cross-build-linux.yml",
    ".github/workflows/cross-compile-all-targets.yml",
)

# Losing these silently would be worse than a mismatch: the lanes would go
# green because nothing is checked any more.
MIN_EXPECTED_INVOCATIONS = 3

RELEASE_WORKFLOW = ".github/workflows/release-auto.yml"

# `release-auto.yml`'s own-binary ceiling. Now matches the per-PR ceilings
# (see module docs, soldr#1060) — still pinned explicitly, rather than pooled
# with the other three, so a future regression is named against this file's
# rationale instead of silently drifting.
RELEASE_OWN_BINARY_CEILING = "2.17"

# Tells the two release invocations apart. The own-binary step invokes the
# ELF verifier directly for one built artifact. The bundled step dispatches to
# the shared bundle verifier because it also includes fetched third-party
# binaries.
RELEASE_OWN_BINARY_MARKER = "${{ matrix.binary }}"
RELEASE_BUNDLED_SCRIPT = "verify_release_bundle.py"
RELEASE_BUNDLED_MARKER = "--check glibc-baseline"

CEILING_RE = re.compile(r"--max-glibc\s+([0-9][0-9.]*)")


def ceilings_in(text: str) -> list[str]:
    """Every `--max-glibc` passed to SCRIPT in `text`.

    An invocation is usually split across continuation lines, so the flag
    is looked for in a small window after the script name rather than on
    the same line.
    """
    found: list[str] = []
    lines = text.splitlines()
    for index, line in enumerate(lines):
        if SCRIPT not in line:
            continue
        window = "\n".join(lines[index : index + 4])
        match = CEILING_RE.search(window)
        if match:
            found.append(match.group(1))
    return found


def ceilings_by_marker(text: str) -> "dict[str, str]":
    """Map each release ratchet marker to its glibc ceiling.

    The own-binary step calls the ELF verifier directly. The bundled step goes
    through the shared dispatcher so it can find all staged executables first.
    Both shapes must be recognized, or extraction would make this guard pass
    vacuously after a release ratchet disappeared.
    """
    found: dict[str, str] = {}
    lines = text.splitlines()
    for index in range(len(lines)):
        window = "\n".join(lines[index : index + 6])
        match = CEILING_RE.search(window)
        if not match:
            continue
        if SCRIPT in window and RELEASE_OWN_BINARY_MARKER in window:
            found[RELEASE_OWN_BINARY_MARKER] = match.group(1)
        if RELEASE_BUNDLED_SCRIPT in window and RELEASE_BUNDLED_MARKER in window:
            found[RELEASE_BUNDLED_MARKER] = match.group(1)
    return found


def check_release_ceilings(repo_root: Path) -> int:
    """Assert release-auto's own-binary ceiling is the declared value."""
    path = repo_root / RELEASE_WORKFLOW
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        print(
            f"check_glibc_ceilings: cannot read {RELEASE_WORKFLOW}: {error}",
            file=sys.stderr,
        )
        return 1

    found = ceilings_by_marker(text)
    missing = [
        m for m in (RELEASE_OWN_BINARY_MARKER, RELEASE_BUNDLED_MARKER) if m not in found
    ]
    if missing:
        print(
            f"check_glibc_ceilings: {RELEASE_WORKFLOW} no longer has a "
            f"glibc release check for: {', '.join(missing)}. A release ratchet was "
            "removed or reshaped — if deliberate, update this script and say why.",
            file=sys.stderr,
        )
        return 1

    actual = found[RELEASE_OWN_BINARY_MARKER]
    if actual != RELEASE_OWN_BINARY_CEILING:
        print(
            f"check_glibc_ceilings: {RELEASE_WORKFLOW}'s own-binary ceiling is "
            f"{actual}, expected {RELEASE_OWN_BINARY_CEILING}. This gates the same "
            "fact as the per-PR ceilings and is knowingly looser (soldr#2145); if "
            "you are tightening it because a release confirmed the number, update "
            "RELEASE_OWN_BINARY_CEILING to match.",
            file=sys.stderr,
        )
        return 1
    return 0


def main(argv: "list[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", default=".", type=Path)
    args = parser.parse_args(argv)

    seen: dict[str, list[str]] = {}
    for relative in OWN_BINARY_WORKFLOWS:
        path = args.repo_root / relative
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as error:
            print(
                f"check_glibc_ceilings: cannot read {relative}: {error}",
                file=sys.stderr,
            )
            return 1
        found = ceilings_in(text)
        if found:
            seen[relative] = found

    total = sum(len(v) for v in seen.values())
    if total < MIN_EXPECTED_INVOCATIONS:
        print(
            f"check_glibc_ceilings: expected at least {MIN_EXPECTED_INVOCATIONS} "
            f"{SCRIPT} invocations across {', '.join(OWN_BINARY_WORKFLOWS)}, found {total}. "
            "A ratchet was removed or renamed — if that was deliberate, update "
            "MIN_EXPECTED_INVOCATIONS with a note saying why.",
            file=sys.stderr,
        )
        for relative, found in seen.items():
            print(f"  {relative}: {found}", file=sys.stderr)
        return 1

    distinct = sorted({value for values in seen.values() for value in values})
    if len(distinct) != 1:
        print(
            f"check_glibc_ceilings: the per-PR ceilings disagree: {', '.join(distinct)}. "
            "These gate binaries soldr builds, so they are three measurements of one "
            "fact and must move together.",
            file=sys.stderr,
        )
        for relative, found in seen.items():
            print(f"  {relative}: {found}", file=sys.stderr)
        return 1

    if (status := check_release_ceilings(args.repo_root)) != 0:
        return status

    print(
        f"check_glibc_ceilings: {total} per-PR own-binary ceilings agree at "
        f"{distinct[0]}; {RELEASE_WORKFLOW} own-binary ceiling pinned at "
        f"{RELEASE_OWN_BINARY_CEILING} (soldr#2145) - OK"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
