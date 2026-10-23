#!/usr/bin/env python3
"""Copy an integration fixture's private daemon/broker logs into the uploaded diagnostics.

soldr#3128. The RSS-ceiling breach fixture
(`crates/soldr-cli/tests/daemon/daemon_rss_ceiling.rs`) drives a real broker
and daemon through *private* roots it creates under the OS temp directory:

    <temp>/soldr-rss-breach-home-<nanos>/       HOME / USERPROFILE
    <temp>/soldr-rss-breach-cache-a-<nanos>/    SOLDR_CACHE_DIR (priming route)
    <temp>/soldr-rss-breach-cache-b-<nanos>/    SOLDR_CACHE_DIR (breach route)
    <temp>/soldr-rss-ceiling-{cache,home}-<nanos>/
    <temp>/soldr-rss-rate-{cache,home}-<nanos>/

Every daemon lifecycle record, broker spawn log, watchdog status file and
memory-breach dump the run produced is inside those roots and nowhere else.
The target-run diagnostics artifact carries toolchain JSON, nextest
inventories and JUnit -- none of which can say why a build inside the fixture
stalled -- so when the Windows lanes timed out at 300 s there was nothing left
to read afterwards.

Deliberately narrow. Only files named in `COLLECTED_NAMES` are copied, so the
compiler caches, staged executables and cargo target trees that share those
roots (gigabytes) stay where they are. Each file is capped, and the whole
collection is capped; a file over its cap is copied *tail first*, because the
end of a log is where a stall is described. `index.json` records everything
copied, truncated or skipped, so an empty artifact is distinguishable from a
collector that silently did nothing.

Never fails the job it runs in: the exit code is 0 for every outcome except an
explicit `--strict` request. Diagnostics collection is not a gate.

Usage:
    python3 .github/scripts/collect_fixture_diagnostics.py --output DIR
        [--root DIR ...] [--prefix NAME ...] [--max-file-bytes N]
        [--max-total-bytes N] [--max-depth N] [--strict]
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import tempfile
from dataclasses import asdict, dataclass
from pathlib import Path

# Fixture root name prefixes, matched against the *directory name* directly
# under a scanned root. `unique_temp_dir("rss-breach-home")` in the fixture
# produces `soldr-rss-breach-home-<nanos>`.
#
# soldr#3831: the cold-start fixtures (`cold_first_compile.rs`
# `cold-{home,root}-*`, `cli_exec.rs` `exec-cargo-zccache-shims`) stop their
# processes but keep their roots, so their bring-up timings are collectable.
DEFAULT_PREFIXES = (
    "soldr-rss-",
    "soldr-cold-home-",
    "soldr-cold-root-",
    "soldr-exec-cargo-zccache-shims-",
)

# The complete set of files worth uploading, by exact base name. Everything
# else under a fixture root is cache, source or a build artifact.
#
# `heap.pprof` is deliberately absent: it is a binary heap profile, not a log,
# and the summary/mimalloc counters beside it carry the numbers a triage pass
# needs. Ask for it by hand from a rerun if a profile is genuinely required.
COLLECTED_NAMES = frozenset(
    {
        # Daemon route state (SOLDR_CACHE_DIR/cache/soldr-daemon/).
        "lifecycle.jsonl",
        "rss-ceiling-v1.json",
        "daemon.log",
        "compile-delivery.jsonl",
        "embedded-zccache.warn.log",
        # Client-written logs under <root>/ and <root>/logs/.
        "daemon-spawn.log",
        "auto-gc.log",
        "cargo-abort.jsonl",
        "compile-daemon-fallbacks.jsonl",
        # Broker, under the HOME-anchored running-process directory.
        "broker-spawn.log",
        "broker-bringup.jsonl",
        # Per-phase daemon cold-start timings (soldr#3163, soldr#3831).
        "daemon-bringup.jsonl",
        # memory-breach-<ms>-<pid>/ dump contents (soldr#3057).
        "summary.json",
        "mimalloc-stats.json",
        "proc-self-status.txt",
        # soldr#3053 — the cgroup-wide counters plus the compiles that were
        # in flight at breach time. Collected because the per-process
        # `proc-self-status.txt` cannot say which unit caused the growth, and
        # CLAUDE.md's "Diagnosing before capping" is explicit that cgroup OOM
        # counters alone are not sufficient evidence either.
        "cgroup.json",
        # Embedded zccache session logs.
        "last-session.log",
        "last-session.jsonl",
        "last-session-stats.json",
    }
)

DEFAULT_MAX_FILE_BYTES = 2 * 1024 * 1024
DEFAULT_MAX_TOTAL_BYTES = 32 * 1024 * 1024
DEFAULT_MAX_DEPTH = 10


def default_roots() -> list[Path]:
    """Directories that can hold a fixture root.

    The OS temp directory is where `std::env::temp_dir()` points. `RUNNER_TEMP`
    is added because a workflow may point the job's `TMP`/`TEMP` at it, and
    scanning a directory twice costs one `scandir` and is deduplicated below.
    """

    roots = [Path(tempfile.gettempdir())]
    runner_temp = os.environ.get("RUNNER_TEMP")
    if runner_temp:
        roots.append(Path(runner_temp))
    return roots


def fixture_roots(root: Path, prefixes: tuple[str, ...]) -> list[Path]:
    """Directories directly under `root` whose name starts with a prefix."""

    try:
        entries = sorted(root.iterdir())
    except OSError:
        return []
    return [
        entry for entry in entries if entry.is_dir() and entry.name.startswith(prefixes)
    ]


def collect_files(fixture: Path, max_depth: int) -> list[Path]:
    """Whitelisted files under `fixture`, breadth-bounded by `max_depth`."""

    found: list[Path] = []
    pending = [(fixture, 0)]
    while pending:
        directory, depth = pending.pop()
        try:
            entries = sorted(directory.iterdir())
        except OSError:
            continue
        for entry in entries:
            try:
                is_dir = entry.is_dir()
            except OSError:
                continue
            if is_dir:
                if depth < max_depth:
                    pending.append((entry, depth + 1))
            elif entry.name in COLLECTED_NAMES:
                found.append(entry)
    return sorted(found)


@dataclass(frozen=True)
class CopyResult:
    """What `copy_capped` wrote for one source file."""

    bytes: int
    original_bytes: int
    truncated: bool


@dataclass(frozen=True)
class CopiedFile:
    """An `index.json` `copied` row: where the file landed and how much."""

    path: str
    bytes: int
    original_bytes: int
    truncated: bool


@dataclass(frozen=True)
class SkippedFile:
    """An `index.json` `skipped` row: the source and why it was left behind."""

    path: str
    reason: str


@dataclass(frozen=True)
class CollectionIndex:
    """Everything one collection run copied, truncated or skipped."""

    schema_version: int
    issue: str
    roots_scanned: list[str]
    fixture_roots: list[str]
    collected_names: list[str]
    copied: list[CopiedFile]
    skipped: list[SkippedFile]
    total_bytes: int

    def to_dict(self) -> dict:
        """The `index.json` payload."""
        return asdict(self)


def copy_capped(source: Path, destination: Path, max_bytes: int) -> CopyResult:
    """Copy `source` to `destination`, keeping at most the last `max_bytes`.

    The tail is kept rather than the head: a log that grew past the cap did so
    because the run kept going, and the interesting part of a stall is the last
    thing written before it.
    """

    size = source.stat().st_size
    truncated = size > max_bytes
    destination.parent.mkdir(parents=True, exist_ok=True)
    with source.open("rb") as handle:
        if truncated:
            handle.seek(size - max_bytes)
        payload = handle.read()
    destination.write_bytes(payload)
    return CopyResult(len(payload), size, truncated)


def collect(
    *,
    output: Path,
    roots: list[Path],
    prefixes: tuple[str, ...],
    max_file_bytes: int = DEFAULT_MAX_FILE_BYTES,
    max_total_bytes: int = DEFAULT_MAX_TOTAL_BYTES,
    max_depth: int = DEFAULT_MAX_DEPTH,
) -> CollectionIndex:
    """Copy every whitelisted fixture file into `output`. Returns the index."""

    output.mkdir(parents=True, exist_ok=True)
    copied: list[CopiedFile] = []
    skipped: list[SkippedFile] = []
    scanned: list[str] = []
    total = 0
    seen_roots: set[Path] = set()

    for root in roots:
        try:
            resolved = root.resolve()
        except OSError:
            continue
        if resolved in seen_roots:
            continue
        seen_roots.add(resolved)
        for fixture in fixture_roots(resolved, prefixes):
            scanned.append(str(fixture))
            for source in collect_files(fixture, max_depth):
                relative = Path(fixture.name) / source.relative_to(fixture)
                if total >= max_total_bytes:
                    skipped.append(SkippedFile(str(source), "total-cap"))
                    continue
                try:
                    record = copy_capped(source, output / relative, max_file_bytes)
                except OSError as error:
                    skipped.append(SkippedFile(str(source), str(error)))
                    continue
                total += record.bytes
                copied.append(
                    CopiedFile(
                        path=str(relative),
                        bytes=record.bytes,
                        original_bytes=record.original_bytes,
                        truncated=record.truncated,
                    )
                )

    index = CollectionIndex(
        schema_version=1,
        issue="soldr#3128",
        roots_scanned=[str(root) for root in sorted(seen_roots)],
        fixture_roots=scanned,
        collected_names=sorted(COLLECTED_NAMES),
        copied=copied,
        skipped=skipped,
        total_bytes=total,
    )
    (output / "index.json").write_text(
        json.dumps(index.to_dict(), indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    return index


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--root", action="append", type=Path, default=None)
    parser.add_argument("--prefix", action="append", default=None)
    parser.add_argument("--max-file-bytes", type=int, default=DEFAULT_MAX_FILE_BYTES)
    parser.add_argument("--max-total-bytes", type=int, default=DEFAULT_MAX_TOTAL_BYTES)
    parser.add_argument("--max-depth", type=int, default=DEFAULT_MAX_DEPTH)
    parser.add_argument(
        "--strict",
        action="store_true",
        help="Exit non-zero on a collection error (off by default: diagnostics never gate a job).",
    )
    args = parser.parse_args(argv)

    roots = args.root if args.root else default_roots()
    prefixes = tuple(args.prefix) if args.prefix else DEFAULT_PREFIXES
    try:
        index = collect(
            output=args.output,
            roots=list(roots),
            prefixes=prefixes,
            max_file_bytes=args.max_file_bytes,
            max_total_bytes=args.max_total_bytes,
            max_depth=args.max_depth,
        )
    except OSError as error:
        print(f"fixture diagnostics collection failed: {error}", file=sys.stderr)
        return 1 if args.strict else 0

    fixture_root_count = len(index.fixture_roots)
    copied_count = len(index.copied)
    print(
        "fixture diagnostics: "
        f"{fixture_root_count} fixture roots, "
        f"{copied_count} files, {index.total_bytes} bytes -> {args.output}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
