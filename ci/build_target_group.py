#!/usr/bin/env python3
"""Build a named group of canonical soldr targets in one invocation (soldr#2460).

Owner request: ``win-mac-musl`` — win-x64, mac-arm64, linux-x64-musl,
linux-arm64-musl — as a locally runnable Python script rather than a GHA
matrix. Each target is built with ``soldr build --release --target <alias>``
(dogfooding policy: never bare cargo), sequentially and fail-fast by
default; ``--keep-going`` runs every target and reports all failures at
the end. Built executables are collected under ``<out-dir>/<group>/<alias>/``.

Alias→triple resolution reads ``ci/canonical-targets.json`` (the
parity-enforced source of truth, soldr#1695) instead of duplicating the
table. Intended primary environment is a Linux host/container: a Windows
host cannot build the Linux legs today (soldr#2315), and the script says
so up front instead of surfacing a raw toolchain error.

Usage:
    uv run --no-project python ci/build_target_group.py --dry-run
    uv run --no-project python ci/build_target_group.py [--group win-mac-musl]
        [--out-dir dist] [--keep-going] [-- <args forwarded to soldr build>]
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
CANONICAL_TARGETS = REPO_ROOT / "ci" / "canonical-targets.json"

# Group name -> ordered canonical aliases. Aliases must exist in
# ci/canonical-targets.json (unit-tested parity guard).
GROUPS: dict[str, list[str]] = {
    "win-mac-musl": ["win-x64", "mac-arm64", "linux-x64-musl", "linux-arm64-musl"],
}

# Workspace executables collected per target. The full release bundle
# (crgx, cargo-chef staging) is release-packaging concern, out of scope.
COLLECTED_BINARIES = ("soldr", "soldr-daemon")


@dataclass(frozen=True)
class CanonicalTarget:
    """A friendly target alias and the triple it resolves to."""

    alias: str
    triple: str


@dataclass(frozen=True)
class ArtifactMove:
    """Where one built executable is collected from and to."""

    source: Path
    destination: Path


def load_canonical_aliases(
    path: Path = CANONICAL_TARGETS,
) -> tuple[CanonicalTarget, ...]:
    data = json.loads(path.read_text(encoding="utf-8"))
    return tuple(
        CanonicalTarget(entry["alias"], entry["triple"]) for entry in data["targets"]
    )


def resolve_group(
    group: str, aliases: tuple[CanonicalTarget, ...]
) -> tuple[CanonicalTarget, ...]:
    """Expand a group name into ordered canonical targets."""
    if group not in GROUPS:
        known = ", ".join(sorted(GROUPS))
        raise KeyError(f"unknown group {group!r}; known groups: {known}")
    triples = {target.alias: target.triple for target in aliases}
    plan = []
    for alias in GROUPS[group]:
        if alias not in triples:
            raise KeyError(
                f"group {group!r} names alias {alias!r} not present in "
                f"{CANONICAL_TARGETS.name}"
            )
        plan.append(CanonicalTarget(alias, triples[alias]))
    return tuple(plan)


def build_command(alias: str, passthrough: list[str]) -> list[str]:
    return ["soldr", "build", "--release", "--target", alias, *passthrough]


def artifact_moves(
    group: str, alias: str, triple: str, out_dir: Path
) -> list[ArtifactMove]:
    """Source/destination moves for a target's built executables."""
    suffix = ".exe" if "windows" in triple else ""
    release_dir = Path("target") / triple / "release"
    dest_dir = out_dir / group / alias
    return [
        ArtifactMove(release_dir / f"{name}{suffix}", dest_dir / f"{name}{suffix}")
        for name in COLLECTED_BINARIES
    ]


def windows_host_warning(plan: tuple[CanonicalTarget, ...]) -> str | None:
    if sys.platform != "win32":
        return None
    blocked = [target.alias for target in plan if "linux" in target.triple]
    if not blocked:
        return None
    return (
        "warning: Windows host -> Linux target has no blessed toolchain "
        f"today (soldr#2315); expect {', '.join(blocked)} to fail. "
        "Run this group from a Linux host/container."
    )


def print_plan(
    group: str,
    plan: tuple[CanonicalTarget, ...],
    out_dir: Path,
    passthrough: list[str],
) -> None:
    print(f"group {group}: {len(plan)} targets")
    for target in plan:
        print(f"  {target.alias} -> {target.triple}")
        print(f"    $ {' '.join(build_command(target.alias, passthrough))}")
        for move in artifact_moves(group, target.alias, target.triple, out_dir):
            print(f"    {move.source} -> {move.destination}")


def run_target(
    group: str,
    alias: str,
    triple: str,
    out_dir: Path,
    passthrough: list[str],
) -> bool:
    cmd = build_command(alias, passthrough)
    print(f"==> {' '.join(cmd)}", flush=True)
    result = subprocess.run(cmd, cwd=REPO_ROOT, check=False)
    if result.returncode != 0:
        print(f"error: {alias} build exited {result.returncode}", file=sys.stderr)
        return False
    missing = []
    for move in artifact_moves(group, alias, triple, out_dir):
        src, dest = move.source, move.destination
        src_abs = REPO_ROOT / src
        if not src_abs.is_file():
            missing.append(str(src))
            continue
        dest_abs = REPO_ROOT / dest
        dest_abs.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src_abs, dest_abs)
        print(f"    collected {dest}")
    if missing:
        print(
            f"error: {alias} built but expected artifacts are missing: "
            f"{', '.join(missing)}",
            file=sys.stderr,
        )
        return False
    return True


def main(argv: list[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    passthrough: list[str] = []
    if "--" in args:
        split = args.index("--")
        args, passthrough = args[:split], args[split + 1 :]

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--group", default="win-mac-musl")
    parser.add_argument("--out-dir", default="dist", type=Path)
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the resolved plan (alias -> triple -> commands -> "
        "artifacts) and run nothing",
    )
    parser.add_argument(
        "--keep-going",
        action="store_true",
        help="build every target even after a failure; exit non-zero "
        "listing the failures",
    )
    opts = parser.parse_args(args)

    try:
        plan = resolve_group(opts.group, load_canonical_aliases())
    except KeyError as err:
        parser.exit(2, f"error: {err.args[0]}\n")

    if opts.dry_run:
        print_plan(opts.group, plan, opts.out_dir, passthrough)
        return 0

    warning = windows_host_warning(plan)
    if warning:
        print(warning, file=sys.stderr)

    failures = []
    for target in plan:
        if run_target(
            opts.group, target.alias, target.triple, opts.out_dir, passthrough
        ):
            continue
        failures.append(target.alias)
        if not opts.keep_going:
            break

    if failures:
        print(f"failed targets: {', '.join(failures)}", file=sys.stderr)
        return 1
    print(f"group {opts.group}: all {len(plan)} targets built")
    return 0


if __name__ == "__main__":
    sys.exit(main())
