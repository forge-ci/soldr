#!/usr/bin/env python3
"""Measure how much of the Tier-2 zccache store one gate run uses (soldr#3120).

The `zccache-unit` store was once trimmed to its 2.00 GiB Actions budget; one saved store generation is
about 4 GiB, so the Repository Actions cache budget check stays red. Trimming
the store before the save is the proposed fix, and its cost depends on one
number: how many bytes of the store a single gate run actually touches. If that
working set fits under the trim cap, eviction only drops artifacts no later run
can hit. If it does not, every later gate starts partly cold.

zccache keeps artifact access times in its binary index, which is unreadable
from here, so this reads two things instead.

**Working set (read-only, on the real store).** Every file under the store is
stat'ed once per inode. A file counts as touched when its atime, ctime or mtime
is at or after `--since-file`, the epoch recorded right after the restore:

* a hit materialized by hardlink changes the inode's link count, which updates
  its ctime (ext4 on GitHub-hosted runners cannot reflink, so this is the
  normal path);
* a hit materialized by copy reads the file, which updates atime (under
  relatime, because restored files carry atime equal to mtime); and
* a miss publishes a new file, which sets mtime.

A reflink materialization touches none of these, so "nothing touched" is
reported as unmeasurable rather than as an empty working set.

**Trial trim (on a real copy).** This follows the method in the owner's
comment on soldr#3120. The store is copied, not hardlinked, because the index
is rewritten in place. `soldr gc maintain --root <copy> --json` then runs one
Full pass under `ZCCACHE_CACHE_SIZE_BYTES`. The copy is always removed, and
the saved store is never modified.

The copy trial costs about 70 s on the gate (soldr#3120 measured a 55 s copy and a
13.5 s pass), so the workflow runs it on `main` only. The walk takes about a second
and runs everywhere.

**Prune (opt-in, modifies the store).** `--prune-dead-version-dirs` removes
whole zccache version trees that this run touched zero files of. See
`prune_dead_version_dirs` for the guards. After soldr#3213 aligned the
bootstrap pin, the gate measured `v1.13.21` at 3.09 GiB with 0 bytes touched.

Always exits 0 once arguments parse.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import os
import pathlib
import shutil
import stat
import subprocess
import sys
import time
from pathlib import Path

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

GIB = 1024**3
FAMILY = "zccache-unit"
# soldr#3396 retired the `stable-cook-*` entry that shared the zccache-unit
# family (ci/cache-ownership.json), so the store owns the whole allocation.
# `--reserve-bytes` stays for a future co-tenant of the family.
DEFAULT_RESERVE_BYTES = 0
COPY_HEADROOM_BYTES = GIB // 2
TRIAL_TIMEOUT_SECONDS = 900


@dataclasses.dataclass
class VersionStats:
    """Per zccache version directory totals gathered by one store walk."""

    name: str
    total_bytes: int = 0
    touched_bytes: int = 0
    # files touched (a touched empty file still marks the version live)
    touched_files: int = 0
    # the directories carrying this version name
    paths: set[pathlib.Path] = dataclasses.field(default_factory=set)


@dataclasses.dataclass
class Walk:
    total_files: int = 0
    total_bytes: int = 0
    touched_files: int = 0
    touched_bytes: int = 0
    versions: list[VersionStats] = dataclasses.field(default_factory=list)

    def version(self, name: str) -> VersionStats:
        """The totals for version directory `name`, created on first use."""
        for stats in self.versions:
            if stats.name == name:
                return stats
        stats = VersionStats(name)
        self.versions.append(stats)
        return stats


@dataclasses.dataclass(frozen=True)
class VersionDir:
    """A store file's version directory name and path (None when unversioned)."""

    name: str
    directory: pathlib.Path | None


@dataclasses.dataclass(frozen=True)
class InodeKey:
    """Identity of a file on disk, used to count hardlinks once."""

    device: int
    inode: int


@dataclasses.dataclass(frozen=True)
class PrunedTree:
    """A removed version tree and the bytes it held."""

    name: str
    size: int


UNVERSIONED = "(unversioned)"


def version_dir(store: pathlib.Path, path: pathlib.Path) -> VersionDir:
    """The zccache version directory (`v1.13.22`) a store file lives under."""
    parts = path.relative_to(store).parts[:-1]
    for index, part in enumerate(parts):
        if part.startswith("v") and part[1:2].isdigit():
            return VersionDir(part, store.joinpath(*parts[: index + 1]))
    return VersionDir(UNVERSIONED, None)


def walk_store(store: pathlib.Path, since: float) -> Walk:
    walk = Walk()
    seen: set[InodeKey] = set()
    for dirpath, _dirnames, filenames in os.walk(store):
        for name in filenames:
            path = pathlib.Path(dirpath) / name
            try:
                info = path.lstat()
            except FileNotFoundError:
                continue
            if not stat.S_ISREG(info.st_mode):
                continue
            inode = InodeKey(info.st_dev, info.st_ino)
            if inode in seen:
                continue
            seen.add(inode)
            touched = max(info.st_atime, info.st_ctime, info.st_mtime) >= since
            located = version_dir(store, path)
            bucket = walk.version(located.name)
            if located.directory is not None:
                bucket.paths.add(located.directory)
            walk.total_files += 1
            walk.total_bytes += info.st_size
            bucket.total_bytes += info.st_size
            if touched:
                walk.touched_files += 1
                walk.touched_bytes += info.st_size
                bucket.touched_bytes += info.st_size
                bucket.touched_files += 1
    return walk


def measurement_is_valid(walk: Walk) -> bool:
    """Some, but not all, files were touched after the restore marker."""
    return 0 < walk.touched_files < walk.total_files


def prune_dead_version_dirs(walk: Walk) -> list[PrunedTree]:
    """Remove whole zccache version trees this run never touched (soldr#3120).

    A version tree nobody read, linked or wrote during a full gate run is dead
    weight that every later generation would restore and save again. zccache's
    own maintenance only walks its current version, and soldr's legacy sweep
    runs only inside a maintenance pass, which CI never gets to run.

    Deliberately conservative. Nothing is removed unless the walk is a valid
    measurement and some other version tree was touched, so a run that used
    nothing never empties the store. A name that maps to more than one
    directory is left alone. Unversioned files are never removed.
    """
    if not measurement_is_valid(walk):
        return []
    live = {
        stats.name
        for stats in walk.versions
        if stats.touched_files and stats.name != UNVERSIONED
    }
    if not live:
        return []
    pruned: list[PrunedTree] = []
    for stats in sorted(walk.versions, key=lambda item: item.name):
        name, paths = stats.name, stats.paths
        if name in live or name == UNVERSIONED or len(paths) != 1:
            continue
        (directory,) = paths
        try:
            shutil.rmtree(directory)
        except OSError as error:
            print(f"could not prune {directory}: {error}", file=sys.stderr)
            continue
        pruned.append(PrunedTree(name, stats.total_bytes))
    return pruned


def working_set_verdict(walk: Walk, cap_bytes: int) -> str:
    if walk.total_files == 0:
        return "store is empty: nothing to measure"
    if walk.touched_files == 0:
        return (
            "no file touched since the restore marker: unmeasurable "
            "(reflink materialization, noatime, or no compile ran)"
        )
    if walk.touched_files == walk.total_files:
        return (
            "every file touched since the marker: the marker predates the "
            "restore, so this is not a working set"
        )
    if walk.touched_bytes <= cap_bytes:
        return "working set fits under the trim cap"
    return (
        "working set exceeds the trim cap: trimming would start later gates partly cold"
    )


def default_cap_bytes(manifest_path: pathlib.Path, reserve_bytes: int) -> int:
    """The on-disk trim cap: the family's `store_cap_bytes` minus the reserve.

    The family's Actions budget (its ci.toml footprint) counts the COMPRESSED
    saved entry (the store compresses about 4x). Trimming the uncompressed
    store to it evicted the driver build's units every run (soldr#3458).
    `store_cap_bytes` is the one per-group number ci/cache-ownership.json
    keeps, because ci.toml has no field for an on-disk cap (soldr#3618).
    """
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    family = manifest["family_groups"][FAMILY]
    return int(family["store_cap_bytes"]) - reserve_bytes


def read_since(path: pathlib.Path) -> float | None:
    try:
        return float(path.read_text(encoding="utf-8").strip())
    except (OSError, ValueError):
        return None


def parse_status(stdout: str) -> dict | None:
    """The `gc maintain --json` status object, tolerating surrounding lines."""
    start, end = stdout.find("{"), stdout.rfind("}")
    if start < 0 or end < start:
        return None
    try:
        return json.loads(stdout[start : end + 1])
    except json.JSONDecodeError:
        return None


def trial_trim(
    store: pathlib.Path,
    store_bytes: int,
    soldr: pathlib.Path,
    cap_bytes: int,
    scratch_root: pathlib.Path,
) -> dict:
    """Trim a real copy of `store` and return what happened; never raises."""
    try:
        free = shutil.disk_usage(scratch_root).free
    except OSError as error:
        return {"skipped": f"scratch root unavailable: {error}"}
    if free < store_bytes + COPY_HEADROOM_BYTES:
        return {
            "skipped": f"{free} bytes free under {scratch_root}, need {store_bytes + COPY_HEADROOM_BYTES}"
        }
    root = scratch_root / f"zccache-trial-{os.getpid()}"
    copy = root / "cache" / "zccache" / "daemon-state"
    try:
        started = time.monotonic()
        shutil.copytree(store, copy, symlinks=True)
        copy_seconds = time.monotonic() - started
        env = dict(os.environ)
        env.pop("ZCCACHE_CACHE_SIZE_PERCENT", None)
        env.pop("ZCCACHE_CACHE_DIR", None)
        env["ZCCACHE_CACHE_SIZE_BYTES"] = str(cap_bytes)
        env["SOLDR_CACHE_DIR"] = str(root)
        started = time.monotonic()
        proc = run_captured(
            [str(soldr), "gc", "maintain", "--root", str(root), "--json"],
            capture_output=True,
            text=True,
            env=env,
            timeout=TRIAL_TIMEOUT_SECONDS,
            check=False,
        )
        status = parse_status(proc.stdout)
        return {
            "copy_seconds": round(copy_seconds, 1),
            "maintain_seconds": round(time.monotonic() - started, 1),
            "exit_code": proc.returncode,
            "report": (status or {}).get("zccache"),
            "deferred_reason": (status or {}).get("deferred_reason"),
            "stderr_tail": proc.stderr[-800:],
        }
    except (OSError, subprocess.SubprocessError) as error:
        return {"skipped": f"trial failed: {error}"}
    finally:
        shutil.rmtree(root, ignore_errors=True)


def trim_store(store: pathlib.Path, soldr: pathlib.Path, cap_bytes: int) -> dict:
    """Evict units from the real store until it fits `cap_bytes` (soldr#3252).

    Unlike `trial_trim`, this modifies the store the Save step archives, so the
    saved generation fits the `zccache-unit` allocation instead of exceeding it
    — the one lever that un-reds the budget gate. The store lives at
    `<root>/cache/zccache/daemon-state`, so the root is `store.parents[2]` and
    the maintain pass is run in place, not on a copy. Never raises.
    """
    root = store.parents[2]
    try:
        env = dict(os.environ)
        env.pop("ZCCACHE_CACHE_SIZE_PERCENT", None)
        env.pop("ZCCACHE_CACHE_DIR", None)
        env["ZCCACHE_CACHE_SIZE_BYTES"] = str(cap_bytes)
        env["SOLDR_CACHE_DIR"] = str(root)
        started = time.monotonic()
        proc = run_captured(
            [str(soldr), "gc", "maintain", "--root", str(root), "--json"],
            capture_output=True,
            text=True,
            env=env,
            timeout=TRIAL_TIMEOUT_SECONDS,
            check=False,
        )
        status = parse_status(proc.stdout)
        return {
            "maintain_seconds": round(time.monotonic() - started, 1),
            "exit_code": proc.returncode,
            "report": (status or {}).get("zccache"),
            "deferred_reason": (status or {}).get("deferred_reason"),
            "stderr_tail": proc.stderr[-800:],
        }
    except (OSError, subprocess.SubprocessError) as error:
        return {"skipped": f"trim failed: {error}"}


def gib(value: int) -> str:
    return f"{value / GIB:.2f} GiB"


def render(  # noqa: C901
    walk: Walk | None,
    verdict: str,
    cap_bytes: int,
    trial: dict | None,
    pruned: list[PrunedTree] | None = None,
    *,
    trim: dict | None = None,
) -> str:
    lines = [
        "## Tier-2 zccache working set (soldr#3120)",
        "",
        f"Trim cap: {gib(cap_bytes)}",
        "",
    ]
    if walk is not None:
        lines += [
            f"Working set: **{gib(walk.touched_bytes)}** of {gib(walk.total_bytes)} "
            f"({walk.touched_files} of {walk.total_files} files touched since the restore)",
            "",
            f"Verdict: {verdict}",
            "",
            "| version dir | bytes | touched |",
            "| --- | ---: | ---: |",
        ]
        for stats in sorted(walk.versions, key=lambda item: item.name):
            name = stats.name
            lines.append(
                f"| `{name}` | {gib(stats.total_bytes)} | {gib(stats.touched_bytes)} |"
            )
        lines.append("")
    else:
        lines += [f"Working set: {verdict}", ""]
    if pruned is not None:
        if pruned:
            names = ", ".join(f"`{tree.name}` ({gib(tree.size)})" for tree in pruned)
            lines += [f"Pruned dead version trees before the save: {names}", ""]
        else:
            lines += ["Pruned dead version trees before the save: none", ""]
    if trial is not None:
        if "skipped" in trial:
            lines.append(f"Trial trim: skipped ({trial['skipped']})")
        else:
            report = trial.get("report") or {}
            lines += [
                f"Trial trim on a copy (copy {trial['copy_seconds']} s, "
                f"maintain {trial['maintain_seconds']} s, exit {trial['exit_code']}):",
                "",
                "| field | value |",
                "| --- | ---: |",
            ]
            for field in (
                "pressure",
                "budget_bytes",
                "usage_before_bytes",
                "usage_after_bytes",
                "bytes_reclaimed",
                "artifacts_removed",
                "expired_artifacts_removed",
            ):
                lines.append(f"| {field} | {report.get(field, 'n/a')} |")
            if trial.get("deferred_reason"):
                lines.append(f"\ndeferred: {trial['deferred_reason']}")
    if trim is not None:
        if "skipped" in trim:
            lines.append(f"Trim before save: skipped ({trim['skipped']})")
        else:
            report = trim.get("report") or {}
            lines += [
                f"Trim before save (maintain {trim['maintain_seconds']} s, "
                f"exit {trim['exit_code']}):",
                "",
                "| field | value |",
                "| --- | ---: |",
            ]
            for field in (
                "pressure",
                "budget_bytes",
                "usage_before_bytes",
                "usage_after_bytes",
                "bytes_reclaimed",
                "artifacts_removed",
                "expired_artifacts_removed",
            ):
                lines.append(f"| {field} | {report.get(field, 'n/a')} |")
            if trim.get("deferred_reason"):
                lines.append(f"\ndeferred: {trim['deferred_reason']}")
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--store", type=pathlib.Path, required=True)
    parser.add_argument("--since-file", type=pathlib.Path, required=True)
    parser.add_argument(
        "--soldr", type=pathlib.Path, help="soldr binary for the trial trim"
    )
    parser.add_argument(
        "--trial",
        choices=("true", "false"),
        default="true",
        help="run the copy trial (about 70 s on the gate); the walk always runs",
    )
    parser.add_argument("--scratch-root", type=pathlib.Path)
    parser.add_argument(
        "--prune-dead-version-dirs",
        action="store_true",
        help="remove version trees this run never touched (modifies the store)",
    )
    parser.add_argument(
        "--trim",
        action="store_true",
        help="run `soldr gc maintain` on the real store to fit --cap-bytes "
        "before the save (modifies the store, soldr#3252)",
    )
    parser.add_argument("--cap-bytes", type=int)
    parser.add_argument("--reserve-bytes", type=int, default=DEFAULT_RESERVE_BYTES)
    parser.add_argument(
        "--manifest",
        type=pathlib.Path,
        default=pathlib.Path(__file__).resolve().parents[2]
        / "ci"
        / "cache-ownership.json",
    )
    args = parser.parse_args(argv)

    cap_bytes = args.cap_bytes or default_cap_bytes(args.manifest, args.reserve_bytes)
    if not args.store.is_dir():
        print(f"no object store at {args.store} -- nothing to measure")
        return 0
    since = read_since(args.since_file)
    walk = None
    if since is None:
        verdict = f"no restore marker at {args.since_file}: working set not measured"
    else:
        walk = walk_store(args.store, since)
        verdict = working_set_verdict(walk, cap_bytes)
    pruned = None
    if walk is not None and args.prune_dead_version_dirs:
        pruned = prune_dead_version_dirs(walk)
    trial = None
    if (
        args.trial == "true"
        and args.soldr is not None
        and args.scratch_root is not None
    ):
        store_bytes = (
            walk.total_bytes - sum(tree.size for tree in pruned or [])
            if walk is not None
            else walk_store(args.store, float("inf")).total_bytes
        )
        trial = trial_trim(
            args.store, store_bytes, args.soldr, cap_bytes, args.scratch_root
        )
        if trial.get("stderr_tail") and trial.get("exit_code"):
            print(trial["stderr_tail"], file=sys.stderr)

    trim = None
    if args.trim:
        if args.soldr is None:
            trim = {"skipped": "--trim needs --soldr"}
        else:
            trim = trim_store(args.store, args.soldr, cap_bytes)
            if trim.get("stderr_tail") and trim.get("exit_code"):
                print(trim["stderr_tail"], file=sys.stderr)

    text = render(walk, verdict, cap_bytes, trial, pruned, trim=trim)
    print(text)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as handle:
            handle.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
