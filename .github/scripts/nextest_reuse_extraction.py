#!/usr/bin/env python3
"""Emit the flags that let ``nextest run`` reuse an already-extracted archive.

soldr#2933 (phase 1 of soldr#2931). The target-run replay makes two nextest
passes over the same archive -- an inventory pass (``nextest list``, whose
JSON feeds ``target_run_summary.py``'s coverage reconciliation) and an
execution pass (``nextest run``). Both were spelled ``--archive-file``, and
``--archive-file`` decompresses the *entire* archive into a fresh directory
on every invocation. A 3,302,138,143-byte archive was therefore inflated
twice per shard, three shards deep, onto whichever volume the OS handed out.
Run 33065541438 died of it: ``Os { code: 112, kind: StorageFull }``.

The fix is to extract once and run from the extraction. The inventory pass
does the extraction (``--archive-file <a> --extract-to <dir>``); this script
then tells the execution pass how to consume that directory instead of
decompressing it again.

    reuse_args=()
    while IFS= read -r arg; do reuse_args+=("$arg"); done \
      < <(python .github/scripts/nextest_reuse_extraction.py \
            --extract-dir "$DIR" --archive "$ARCHIVE")
    "$NEXTEST_BIN" nextest run "${reuse_args[@]}" ...

One argument per line, so values containing spaces survive the round trip
without any quoting convention.

FLAG VERIFICATION NOTE
----------------------
The reuse path emits nextest's documented "reuse builds" flags --
``--binaries-metadata``, ``--cargo-metadata``, ``--target-dir-remap`` -- and
is taken only when the two metadata JSON files are actually found inside the
extraction. What is *not* pinned down from inside this repo is the exact
layout nextest writes them at inside an archive, so the search is a bounded
walk rather than a hardcoded path, and anything unrecognised falls back to
``--archive-file`` + ``--extract-to`` + ``--extract-overwrite``, i.e. the
pre-soldr#2933 behaviour aimed at the chosen volume. The fallback still fixes
the volume half of the bug even if the reuse half needs another look, and
``SOLDR_TARGET_RUN_EXTRACT_REUSE=off`` forces it without a code change.
"""

from __future__ import annotations

import argparse
import os
import sys
from dataclasses import dataclass
from pathlib import Path

BINARIES_METADATA = "binaries-metadata.json"
CARGO_METADATA = "cargo-metadata.json"

REUSE_ENV = "SOLDR_TARGET_RUN_EXTRACT_REUSE"
OFF_VALUES = frozenset({"0", "off", "false", "no"})

# The metadata lives near the top of the extraction. Walking the whole tree
# would mean stat-ing every one of the tens of thousands of extracted files
# to find two of them; a shallow bound finds them without paying for that.
MAX_SEARCH_DEPTH = 4


def reuse_enabled(environ: dict[str, str] | None = None) -> bool:
    """Whether the reuse path is permitted. Default on; env can force off."""

    env = os.environ if environ is None else environ
    return env.get(REUSE_ENV, "").strip().lower() not in OFF_VALUES


def find_shallow(
    root: Path, name: str, max_depth: int = MAX_SEARCH_DEPTH
) -> Path | None:
    """First file called ``name`` within ``max_depth`` levels of ``root``.

    Breadth-first so the shallowest match wins: if an archive ever carried
    both a top-level metadata file and a stale nested copy, the top-level one
    is the authoritative pair with ``target-dir-remap``.
    """

    if not root.is_dir():
        return None
    frontier = [(root, 0)]
    while frontier:
        directory, depth = frontier.pop(0)
        candidate = directory / name
        try:
            if candidate.is_file():
                return candidate
        except OSError:
            pass
        if depth >= max_depth:
            continue
        try:
            children = sorted(c for c in directory.iterdir() if c.is_dir())
        except OSError:
            continue
        frontier.extend((child, depth + 1) for child in children)
    return None


def find_target_dir(root: Path) -> Path | None:
    """The extracted cargo target directory, if the archive carries one."""

    candidate = root / "target"
    if candidate.is_dir():
        return candidate
    return None


def reuse_args(extract_dir: Path) -> list[str] | None:
    """Flags that run from ``extract_dir``, or None when it is not usable."""

    binaries = find_shallow(extract_dir, BINARIES_METADATA)
    cargo = find_shallow(extract_dir, CARGO_METADATA)
    target_dir = find_target_dir(extract_dir)
    if binaries is None or cargo is None or target_dir is None:
        return None
    return [
        "--binaries-metadata",
        binaries.as_posix(),
        "--cargo-metadata",
        cargo.as_posix(),
        "--target-dir-remap",
        target_dir.as_posix(),
    ]


def fallback_args(archive: Path, extract_dir: Path) -> list[str]:
    """Re-extract, but into the volume that was chosen on purpose.

    This is the pre-soldr#2933 shape with one difference that matters: the
    destination is named rather than inherited from ``GetTempPath``, so even
    the degraded path cannot land on the small OS volume again.
    """

    return [
        "--archive-file",
        archive.as_posix(),
        "--extract-to",
        extract_dir.as_posix(),
        "--extract-overwrite",
    ]


@dataclass(frozen=True)
class Resolution:
    """The extraction flags to use plus a one-line explanation for the log."""

    flags: list[str]
    reason: str


def resolve(
    extract_dir: Path,
    archive: Path,
    *,
    allow_reuse: bool = True,
) -> Resolution:
    """The flags to use plus a one-line explanation for the log."""

    if not allow_reuse:
        return Resolution(
            fallback_args(archive, extract_dir),
            f"reuse disabled by {REUSE_ENV}; re-extracting into "
            f"{extract_dir.as_posix()}",
        )
    args = reuse_args(extract_dir)
    if args is None:
        return Resolution(
            fallback_args(archive, extract_dir),
            "no reuse metadata found under "
            f"{extract_dir.as_posix()} ({BINARIES_METADATA} / "
            f"{CARGO_METADATA} / target/); re-extracting into the same "
            "directory instead",
        )
    return Resolution(args, f"reusing the extraction at {extract_dir.as_posix()}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--extract-dir", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    args = parser.parse_args(argv)

    resolution = resolve(args.extract_dir, args.archive, allow_reuse=reuse_enabled())
    flags, reason = resolution.flags, resolution.reason
    print(f"nextest extraction: {reason}", file=sys.stderr)
    # LF, never CRLF -- and `reconfigure` rather than writing "\n", because a
    # text-mode stream on Windows translates the \n on the way out no matter
    # how it was produced.
    #
    # These lines are read one-per-argument by a bash `while IFS= read -r`
    # loop in the workflow. `read` strips the \n and leaves the \r, so under
    # the default translation nextest was handed `--binaries-metadata\r`:
    #   error: unexpected argument '--binaries-metadata\n' found
    #   tip: a similar argument exists: '--binaries-metadata'
    # That killed every Windows target-run lane, and the value lines would
    # have carried a trailing \r into a path even if the flag had parsed.
    try:
        sys.stdout.reconfigure(newline="\n")  # type: ignore[union-attr]
    except (AttributeError, ValueError):  # pragma: no cover - non-TextIO stdout
        pass
    for flag in flags:
        sys.stdout.write(f"{flag}\n")
    sys.stdout.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
