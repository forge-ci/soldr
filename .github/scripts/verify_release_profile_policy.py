#!/usr/bin/env python3
"""Fail when a workflow uses `--release` without being allowed to (soldr#1981).

`--release` means `lto = "thin"` + `codegen-units = 1` + `strip = "symbols"`.
Those settings exist to make the *shipped* binary small and fast, and they are
the most expensive things you can ask of the compiler: single-CU codegen is
effectively single-threaded LLVM, and thin-LTO adds a whole-program pass on
top. Paying that on a job whose output never ships is pure waste.

soldr#1982 removed the offenders. This exists so they cannot come back — the
issue asked for exactly that:

    Add a CI policy check ... that fails when a workflow uses `--release`
    without being on an allowlist. Same shape as the timeout gate — this class
    of regression should be impossible, not fixed once.

The allowlist is deliberately per-file and reason-bearing rather than a bare
set of names. A reader hitting a failure needs to know what would justify an
exemption, and "it is in the list" does not tell them.

Usage:
    verify_release_profile_policy.py [--workflows .github/workflows]
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path

# Workflows permitted to use `--release`, each with the reason it survives.
#
# The test is not "does this build something" but **does the artifact ship, or
# is `--release` itself the thing under measurement**. A cheaper profile that
# still exercises the code path is always preferred; see `ci-release`,
# `ci-nextest`, and `ci-bootstrap` in the workspace Cargo.toml.
ALLOWLIST: dict[str, str] = {
    "auto-release.yml": "builds the binaries that are actually published",
    # cross-compile-all-targets.yml no longer uses --release: it is now an
    # opt-in (workflow_dispatch) quick/debug cross-compile validation sweep,
    # so it needs no exemption and must not be a stale allowlist entry.
    "build-all-from-linux.yml": (
        "exists to prove `soldr build --release --target X` works for all 9 targets"
    ),
    "perf-matrix.yml": "measures the shipped profile; a cheaper one measures nothing",
    "perf-cold-warm.yml": "same — --release is the profile under measurement",
    "benchmark-stats.yml": "same — benchmark numbers are only meaningful at ship profile",
    "parent-cache-bench.yml": "same — benchmark baseline",
    "baseline-zero-deps.yml": (
        "benchmarks a zero-dependency baseline; soldr#1981 flagged that a cheaper "
        "profile may invalidate the comparison, so this needs re-baselining before "
        "it can move"
    ),
    "cook-size-gate.yml": (
        "the CLI build here is already `--profile ci-release` (soldr#1982); the "
        "remaining `--release` is `soldr cook --release`, a cook flag rather than a "
        "cargo profile, and it is load-bearing for the size baseline"
    ),
}

# `--release` as a standalone argument. Avoids matching `--release-notes` or a
# word inside prose, while still catching it at end-of-line or before a `\`
# continuation.
RELEASE_FLAG = re.compile(r"(?<![\w-])--release(?![\w-])")

# `--profile release` / `--profile=release` — the same expensive profile spelled
# the long way. soldr#2303: this evaded the check, so `cargo build --profile
# release` for the Dylint driver cdylibs slipped past a policy whose whole point
# is that this regression is impossible. The `release` profile *name* only — the
# cheap `--profile ci-release` / `ci-bootstrap` must NOT match (the `(?![\w-])`
# tail is what keeps `ci-release` and `release-foo` out).
PROFILE_RELEASE = re.compile(r"--profile[=\s]+release(?![\w-])")


SCRIPT_REFERENCE = re.compile(r"\.github/scripts/([A-Za-z0-9_.-]+\.py)")


def referenced_scripts(text: str, scripts_dir: Path) -> list[Path]:
    """Existing `.github/scripts/*.py` files a workflow invokes."""
    found = []
    for name in sorted(set(SCRIPT_REFERENCE.findall(text))):
        candidate = scripts_dir / name
        if candidate.is_file():
            found.append(candidate)
    return found


def uses_release_profile(line: str) -> bool:
    """True when *line* asks cargo for the `release` profile, either spelling."""
    return bool(RELEASE_FLAG.search(line) or PROFILE_RELEASE.search(line))


# Line-level, reason-bearing opt-out, in the same shape as the repo's
# `// allow-bare-test: <reason>` escape from the timed_test lint.
#
# The file-level ALLOWLIST is too blunt for a large multi-purpose workflow:
# putting `ci.yml` in it would exempt ~1,000 lines and twenty jobs in order to
# permit one. A per-line marker keeps the rest of the file governed and still
# forces the author to write down why.
#
# The comment may sit at the end of the line or on the line immediately above
# it, because a wrapped shell invocation has nowhere to put a trailing comment.
ALLOW_MARKER = re.compile(r"#\s*allow-release:\s*(?P<reason>\S.*)$")
# A marker with no reason (or a one-word one) is not an exemption.
MIN_REASON_CHARS = 20


def is_comment(line: str) -> bool:
    """True for a YAML comment line.

    Comments explaining a past `--release` removal are exactly what a naive
    scan would flag — `_ci-cross-build-linux.yml` carries one recording that
    Stage B moved off it.
    """
    return line.lstrip().startswith("#")


def allow_reason(line: str) -> str | None:
    """The `allow-release:` reason on *line*, if it carries a usable one."""
    match = ALLOW_MARKER.search(line)
    if not match:
        return None
    reason = match.group("reason").strip()
    return reason if len(reason) >= MIN_REASON_CHARS else None


def is_exempt(lines: list[str], index: int) -> bool:
    """Whether the `--release` on `lines[index]` is explicitly excused.

    The marker may sit at the end of the offending line, or on the comment
    line immediately above the *shell command* it belongs to. The second form
    exists because a wrapped invocation has nowhere to put a trailing comment
    and its `--release` usually lands several continuation lines down, so the
    walk skips back over `\\`-continued lines first.
    """
    if allow_reason(lines[index]):
        return True
    start = index
    while start > 0 and lines[start - 1].rstrip().endswith("\\"):
        start -= 1
    if start > 0 and is_comment(lines[start - 1]):
        return allow_reason(lines[start - 1]) is not None
    return False


@dataclass(frozen=True)
class ReleaseFinding:
    """One disallowed `--release` line in a workflow file."""

    file: str
    line_number: int
    line: str


def scan(workflows_dir: Path) -> list[ReleaseFinding]:
    """Every disallowed `--release` in the workflows."""
    findings: list[ReleaseFinding] = []
    for path in sorted(workflows_dir.glob("*.yml")):
        if path.name in ALLOWLIST:
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except OSError:
            continue
        lines = text.splitlines()
        for index, line in enumerate(lines):
            if is_comment(line):
                continue
            if uses_release_profile(line) and not is_exempt(lines, index):
                findings.append(ReleaseFinding(path.name, index + 1, line.strip()))
    return findings


def unused_allowlist_entries(workflows_dir: Path) -> list[str]:
    """Allowlisted names that no longer need to be.

    An exemption that has outlived its reason is worse than no exemption: it
    silently re-permits the thing the policy forbids. soldr#1982 deleted
    `windows-gnu-mingw-validation.yml` outright, which is exactly how an entry
    goes stale.
    """
    stale = []
    for name in sorted(ALLOWLIST):
        path = workflows_dir / name
        if not path.exists():
            stale.append(f"{name} (no such workflow)")
            continue
        text = path.read_text(encoding="utf-8")
        # Follow delegation. soldr#2469 step 2.2 is moving release logic out of
        # YAML into `.github/scripts/*.py`, so an allowlisted workflow can stop
        # containing `--release` while still being the thing that builds the
        # shipped binaries -- auto-release.yml did exactly that when its matrix
        # build moved to `native_release_build.py`. Reading only the YAML would
        # report the entry stale and invite someone to delete a live exemption.
        sources = [text]
        sources.extend(
            script.read_text(encoding="utf-8")
            for script in referenced_scripts(text, path.parent.parent / "scripts")
        )
        if not any(
            uses_release_profile(line)
            for source in sources
            for line in source.splitlines()
            if not is_comment(line)
        ):
            stale.append(f"{name} (no longer uses --release)")
    return stale


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workflows", default=".github/workflows")
    args = parser.parse_args(argv)

    workflows_dir = Path(args.workflows)
    if not workflows_dir.is_dir():
        print(
            f"release-profile policy: no such directory {workflows_dir}",
            file=sys.stderr,
        )
        return 1

    findings = scan(workflows_dir)
    stale = unused_allowlist_entries(workflows_dir)

    if not findings and not stale:
        print(
            f"release-profile policy: clean "
            f"({len(ALLOWLIST)} allowlisted workflow(s) checked)."
        )
        return 0

    if findings:
        print("release-profile policy: FAIL", file=sys.stderr)
        for finding in findings:
            print(
                f"  - {finding.file}:{finding.line_number}: {finding.line}",
                file=sys.stderr,
            )
        print(
            "\n`--release` costs thin-LTO + single-CU codegen. Use the cheapest profile "
            "that still exercises the path -- `ci-release`, `ci-nextest`, or "
            "`ci-bootstrap`. If the artifact genuinely ships, or `--release` is what "
            "the job measures, add the workflow to ALLOWLIST with the reason -- or, "
            "for a single line in an otherwise-governed workflow, append "
            "`# allow-release: <reason>` (20+ chars) to that line or the one above it.",
            file=sys.stderr,
        )
    if stale:
        print("\nrelease-profile policy: stale ALLOWLIST entries", file=sys.stderr)
        for entry in stale:
            print(f"  - {entry}", file=sys.stderr)
        print(
            "\nRemove them. An exemption that outlives its reason silently re-permits "
            "what the policy forbids.",
            file=sys.stderr,
        )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
