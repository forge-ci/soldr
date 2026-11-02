"""Reclaim disk on a GitHub-hosted Linux runner before a soldr dogfood lane.

The setup-soldr smoke lane builds and nextests ``soldr-cli`` on a stock
``ubuntu-24.04`` runner. soldr's fail-closed disk guard
(``crates/soldr-cli/src/gc/disk.rs``) blocks a build below 5 GiB free, and the
runner image's preinstalled SDKs leave too little room for that build plus the
nested fixture compiles its tests launch (main run 41501629294:
``build volume has only 3.55 GiB free, below the 5 GiB block threshold``).
soldr was right to refuse; the lane never made room.

This removes a fixed, bounded list of preinstalled toolsets the lane never
uses, then asserts the workspace volume has at least ``--require-gib`` free and
exits 1 with a named diagnostic if not. It only acts on a GitHub-hosted Linux
runner (``GITHUB_ACTIONS=true``, ``RUNNER_ENVIRONMENT=github-hosted``); any
other host is refused rather than having system directories deleted.
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

# Preinstalled runner-image toolsets this lane never touches. Rust, uv and
# PowerShell (the lane's pwsh steps) are deliberately absent.
RECLAIM_DIRS: tuple[str, ...] = (
    "/usr/share/dotnet",
    "/usr/local/lib/android",
    "/opt/ghc",
    "/usr/local/.ghcup",
    "/opt/hostedtoolcache/CodeQL",
    "/usr/share/swift",
    "/usr/local/share/chromium",
    "/usr/local/share/boost",
)

GIB = 1024**3


def free_gib(path: Path) -> float:
    return shutil.disk_usage(path).free / GIB


def is_github_hosted_linux(env: dict[str, str]) -> bool:
    return (
        sys.platform.startswith("linux")
        and env.get("GITHUB_ACTIONS") == "true"
        and env.get("RUNNER_ENVIRONMENT", "github-hosted") == "github-hosted"
    )


def remove_dir(path: str) -> None:
    if not os.path.isdir(path):
        print(f"reclaim: skip {path} (absent)")
        return
    # Root-owned image directories: sudo is passwordless on hosted runners.
    # Child output streams straight to the job log (no pipe capture).
    result = subprocess.run(
        ["sudo", "rm", "-rf", "--one-file-system", path], check=False
    )
    if result.returncode != 0:
        sys.stderr.write(f"reclaim: rm {path} failed ({result.returncode})\n")
    else:
        print(f"reclaim: removed {path}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--volume", default=os.environ.get("GITHUB_WORKSPACE", "."))
    parser.add_argument("--require-gib", type=float, default=20.0)
    args = parser.parse_args(argv)

    if not is_github_hosted_linux(dict(os.environ)):
        sys.stderr.write("reclaim: refusing: not a GitHub-hosted Linux runner\n")
        return 2

    volume = Path(args.volume)
    before = free_gib(volume)
    print(f"reclaim: {volume} free before = {before:.2f} GiB")
    for path in RECLAIM_DIRS:
        remove_dir(path)
    after = free_gib(volume)
    print(f"reclaim: {volume} free after = {after:.2f} GiB (+{after - before:.2f})")
    if after < args.require_gib:
        sys.stderr.write(
            f"\x1b[31mreclaim: FATAL: only {after:.2f} GiB free on {volume} after "
            f"reclaim, below the lane's {args.require_gib:.0f} GiB floor; soldr's "
            "5 GiB disk guard would block the dogfood build/tests.\x1b[0m\n"
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
