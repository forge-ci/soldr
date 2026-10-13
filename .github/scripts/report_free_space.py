#!/usr/bin/env python3
"""Report free disk space on the volumes a target-run shard consumes.

soldr#2734: a windows-gnu shard exhausted the runner disk and three tests
died on a raw ``Os { code: 112, kind: StorageFull }`` from inside
``isolated_daemon.rs``. Nothing in the job output said how much disk was
left at any point, so the cause could only be inferred backwards from an
OS error three layers from the shard that caused it.

soldr#2699 is why nothing else reports it. That issue set
``SOLDR_TARGET_WARN_FREE_GB`` and ``SOLDR_TARGET_BLOCK_FREE_GB`` equal at
1 GiB on this lane, deliberately, so soldr's watchdog never enters its
warn-tier auto-prune path -- pruning mid-run could delete a concurrently
executing test's target tree. The trade-off is that soldr stays silent
until 1 GiB, by which point the runner is effectively out.

This closes that observability gap without touching the thresholds:
print a reading, before and after the archive run, and let a future
exhaustion be diagnosed forwards.

**This is a diagnostic and must never fail the lane.** An unreadable
path is reported and skipped; the exit code is always 0. A disk report
that can turn a green run red is worse than no disk report.
"""

from __future__ import annotations

import argparse
import shutil
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

GIB = 1024**3


@dataclass(frozen=True)
class DiskReading:
    """Free and total capacity of one volume, in GiB."""

    free_gib: float
    total_gib: float


@dataclass(frozen=True)
class Volume:
    """A named path whose volume is reported."""

    name: str
    path: Path


def measure(path: Path) -> DiskReading | None:
    """Free and total GiB for ``path``'s volume, or None if unreadable.

    Returns None rather than raising: on a runner mid-exhaustion the
    interesting paths are exactly the ones most likely to misbehave.
    """
    try:
        usage = shutil.disk_usage(path)
    except OSError:
        return None
    return DiskReading(usage.free / GIB, usage.total / GIB)


def render(label: str, path: Path) -> str:
    measured = measure(path)
    if measured is None:
        return f"target-run disk: {label}={path} unreadable"
    free, total = measured.free_gib, measured.total_gib
    used_pct = 100.0 * (total - free) / total if total else 0.0
    return (
        f"target-run disk: {label}={path} "
        f"free={free:.2f}GiB total={total:.2f}GiB used={used_pct:.1f}%"
    )


def volumes(workspace: Path) -> list[Volume]:
    """The volumes that actually fill up during a shard.

    ``workspace`` holds the extracted archive and ``target/``; the temp
    dir is where every isolated-home test builds its sandbox, and on
    Windows runners the two are frequently different volumes -- which is
    why reporting only one of them would have missed soldr#2734.
    """
    return [
        Volume("workspace", workspace),
        Volume("temp", Path(tempfile.gettempdir())),
    ]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--label",
        default="",
        help="Marks when the reading was taken, e.g. 'before' or 'after'.",
    )
    parser.add_argument(
        "--workspace",
        type=Path,
        default=Path.cwd(),
        help="Checkout root; defaults to the current directory.",
    )
    args = parser.parse_args(argv)

    when = f" [{args.label}]" if args.label else ""
    for volume in volumes(args.workspace):
        print(f"{render(volume.name + when, volume.path)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
