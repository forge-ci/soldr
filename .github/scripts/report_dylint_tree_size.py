#!/usr/bin/env python3
"""Report the on-disk size of the Dylint target trees, and which cache
layers served this run vs. rebuilt from scratch.

soldr#2996 Phase 6 proposes persisting `target/dylint/` as a
`dylint-foundation`-tier cache entry, because it is the only mechanism that
can reach the 638s Dylint block: `Swatinem/rust-cache` deletes those trees at
save time, and `dylint_cook_acceptance.py` shows a per-unit object cache alone
still misses (`object_cache_only: miss`) while a restored tarball skips
(`warm_restored_target: skip`).

That proposal is gated on a number nobody has: how big the trees actually are.
The directive in soldr#2996 admits at most one cache family beyond `soldr
cook`, against a 5 GB budget, so a carve-out has to be costed before it is
granted -- not after.

This reports the three trees separately, because they are not
interchangeable. `soldr dylint cook` prewarms the analysis tree
(`--tree analysis`, the default) and, since soldr#3042, the third-party
dependency layer of the UI-test tree (`--tree tests`). It does not prewarm
`libraries/`, nor the linked UI-test products inside `tests/` -- those are
tier 3 and stay cold by design.

Reports, never fails: a missing tree is information (the stage did not run),
not an error, and this must never be the reason a lane goes red.

## soldr#2349 -- cache layer status

`_build-and-test.yml` now restores four separate Dylint-related cache layers
(the dylint nightly toolchain, the libraries+target trees this docstring was
originally about, the dylint driver + prepared-plan marker, plus the existing
rustup 1.95.0 cache). Whether each one actually served a hit or forced a
rebuild is the entire point of adding them -- a cache block nobody can see the
hit/miss status of cannot demonstrate "dylint is fast".

`crates/soldr-cli/src/ci_test/dylint_library_marker.rs` lands the per-library
"this Dylint library build was skipped, not just its target tree restored"
signal: `.soldr-dylint-library-marker-v1.json` next to the built libraries
(`<target>/dylint/libraries/<nightly>-<host>/`), rewritten by `record()` only
when the six `dylint-library-*` stages actually ran (a skip leaves it
untouched). Presence alone therefore never distinguishes skip from rebuilt --
cache-restore already put a marker on disk before `ci-test` runs at all, and a
rebuild rewrites the same filename. What distinguishes them is whether the
file's mtime *changed* across the `ci-test` step, so this script runs in two
modes:

* `--snapshot-library-marker PATH` (run once, before `ci-test`): prints and
  emits `state=absent` or `state=mtime:<st_mtime_ns>` and exits without
  producing the tree-size report. Its `$GITHUB_OUTPUT` value is threaded
  through to the second call.
* `--library-stage-skip-marker PATH --library-stage-skip-marker-before STATE`
  (run once, after `ci-test`, alongside the normal tree-size report): compares
  the marker's current state against the snapshotted `STATE` and reports
  skipped / rebuilt / did-not-run accordingly.

`--cache-hit LABEL=VALUE` (repeatable) prints one additional row per
`actions/cache`-restored Dylint layer (the libraries+target trees, the dylint driver + prepared-plan marker, plus the
existing rustup 1.95.0 cache), using each step's own `cache-hit` output. A
cache block nobody can see the hit/miss status of cannot demonstrate "dylint
is fast".

Usage:
    python3 .github/scripts/report_dylint_tree_size.py [--target-root target]
    python3 .github/scripts/report_dylint_tree_size.py \\
        --snapshot-library-marker target/dylint/libraries/<nightly>-<host>/.soldr-dylint-library-marker-v1.json
    python3 .github/scripts/report_dylint_tree_size.py \\
        --cache-hit "rustup 1.95.0=true" \\
        --cache-hit "dylint libraries+target trees=true" \\
        --cache-hit "dylint driver+prepared marker=true" \\
        --library-stage-skip-marker target/dylint/libraries/<nightly>-<host>/.soldr-dylint-library-marker-v1.json \\
        --library-stage-skip-marker-before "$SNAPSHOT_STATE"
"""

from __future__ import annotations

import argparse
import os
import pathlib
import sys
from dataclasses import dataclass

TREES = ("libraries", "target", "tests")


@dataclass(frozen=True)
class TreeSize:
    total_bytes: int
    file_count: int


@dataclass(frozen=True)
class CacheHit:
    label: str
    value: str


def tree_bytes(path: pathlib.Path) -> TreeSize:
    """Return (total bytes, file count) below *path*, following no symlinks."""
    total = 0
    files = 0
    for root, _dirs, names in os.walk(path, followlinks=False):
        for name in names:
            entry = pathlib.Path(root) / name
            try:
                if entry.is_symlink():
                    continue
                total += entry.stat().st_size
                files += 1
            except OSError:
                # A file that vanished mid-walk is not worth failing over.
                continue
    return TreeSize(total_bytes=total, file_count=files)


def human(size: int) -> str:
    value = float(size)
    for unit in ("B", "KiB", "MiB", "GiB"):
        if value < 1024 or unit == "GiB":
            return f"{value:.1f} {unit}" if unit != "B" else f"{int(value)} B"
        value /= 1024
    return f"{value:.1f} GiB"


def report(target_root: pathlib.Path) -> list[str]:
    dylint_root = target_root / "dylint"
    lines = ["### Dylint tree sizes (soldr#2996 Phase 6)", ""]
    if not dylint_root.is_dir():
        lines.append(f"No `{dylint_root}` — the Dylint stages did not run.")
        return lines

    lines.append("| tree | size | files |")
    lines.append("|---|---:|---:|")
    grand_total = 0
    grand_files = 0
    for name in TREES:
        path = dylint_root / name
        if not path.is_dir():
            lines.append(f"| `{name}` | absent | — |")
            continue
        tree = tree_bytes(path)
        grand_total += tree.total_bytes
        grand_files += tree.file_count
        lines.append(f"| `{name}` | {human(tree.total_bytes)} | {tree.file_count} |")
    lines.append(f"| **total** | **{human(grand_total)}** | **{grand_files}** |")
    lines.append("")
    lines.append(
        "Uncompressed. A cache entry would be smaller; treat this as the "
        "ceiling when costing the carve-out against the 5 GB budget."
    )
    return lines


def parse_cache_hit_pairs(raw: list[str] | None) -> list[CacheHit]:
    """`["label=value", ...]` -> `[CacheHit(label, value), ...]`, skipping malformed entries.

    A malformed entry (no `=`, or an empty label) is dropped rather than
    raised: this script's one hard rule is that it never fails the lane, and a
    workflow typo in one `--cache-hit` argument must not take down the whole
    report.
    """
    pairs: list[CacheHit] = []
    for entry in raw or []:
        label, sep, value = entry.partition("=")
        label = label.strip()
        if not sep or not label:
            continue
        pairs.append(CacheHit(label, value.strip()))
    return pairs


def describe_cache_hit(value: str) -> str:
    """`actions/cache`'s `cache-hit` output is the literal string `true` on an
    exact hit and empty on anything else (miss, or a restore-keys partial
    restore); normalize both plus the boolean-ish spellings a hand-written
    `--cache-hit` argument might use."""
    normalized = value.strip().lower()
    if normalized in ("true", "hit", "1", "yes"):
        return "restored (cache hit)"
    if normalized in ("", "false", "miss", "0", "no"):
        return "rebuilt (cache miss)"
    return f"unknown ({value!r})"


def marker_state(path: pathlib.Path) -> str:
    """`absent`, or `mtime:<st_mtime_ns>` when *path* exists.

    Nanosecond mtime, not a hash: `dylint_library_marker.rs::record` writes
    via a temp-file-plus-atomic-rename (`replace_marker_file`), so an
    unrelated no-op rewrite would still change mtime — but `record` is only
    ever called on the non-skip path, so a changed mtime here always means a
    real rebuild happened, never a content-identical rewrite.
    """
    try:
        return f"mtime:{path.stat().st_mtime_ns}"
    except OSError:
        return "absent"


def describe_library_stage(before: str | None, path: pathlib.Path) -> str:
    """Skip vs. rebuilt vs. did-not-run for the Dylint library marker.

    `before` is falsy (`None`, or `""`) when `--library-stage-skip-marker-before`
    was not passed, or was passed an empty value -- which is the live case in
    `_build-and-test.yml`: `${{ steps.dylint_library_marker_before.outputs.state
    }}` renders as `""` if the snapshot step never ran (e.g. the job died
    before it, and this report step still runs via `if: always()`). Treated
    distinctly from a snapshot that ran and saw `absent`, since the two mean
    different things: "we don't know" vs. "there really was no marker before
    ci-test ran".
    """
    if not before:
        return "unknown (no --library-stage-skip-marker-before snapshot)"
    after = marker_state(path)
    if before == "absent" and after == "absent":
        return "did not run (no marker before or after ci-test)"
    if before == "absent":
        return "rebuilt (marker created this run)"
    if after == "absent":
        return "marker disappeared during this run (tree wiped?) — treat as rebuilt"
    if before == after:
        return "skipped (marker unchanged — library build cache hit)"
    return "rebuilt (marker rewritten this run)"


def cache_layer_lines(
    pairs: list[CacheHit],
    stage_skip_marker: pathlib.Path | None,
    stage_skip_marker_before: str | None,
) -> list[str]:
    """Report which Dylint-related cache layers served this run.

    Empty input (no `--cache-hit` and no `--library-stage-skip-marker`)
    reports nothing -- callers that never pass these flags (existing
    `report_dylint_tree_size.py` invocations, and every test in
    `tests/test_report_dylint_tree_size.py`) get the tree-size section only,
    unchanged.
    """
    if not pairs and stage_skip_marker is None:
        return []

    lines = ["### Dylint cache layer status (soldr#2349)", ""]
    lines.append("| layer | status |")
    lines.append("|---|---|")
    for hit in pairs:
        lines.append(f"| {hit.label} | {describe_cache_hit(hit.value)} |")

    if stage_skip_marker is not None:
        status = describe_library_stage(stage_skip_marker_before, stage_skip_marker)
        lines.append(f"| dylint libraries stage | {status} |")
    lines.append("")
    return lines


def emit_github_output(name: str, value: str) -> None:
    """Append `name=value` to `$GITHUB_OUTPUT` when it is set (for a step
    with an `id:` to read back as `steps.<id>.outputs.<name>`)."""
    target = os.environ.get("GITHUB_OUTPUT")
    if not target:
        return
    with pathlib.Path(target).open("a", encoding="utf-8") as handle:
        handle.write(f"{name}={value}\n")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target-root", default="target")
    parser.add_argument(
        "--cache-hit",
        action="append",
        metavar="LABEL=VALUE",
        help=(
            "One Dylint-related cache layer's restore status, e.g. "
            "'dylint libraries+target trees=true'. Repeatable."
        ),
    )
    parser.add_argument(
        "--library-stage-skip-marker",
        type=pathlib.Path,
        default=None,
        help=(
            "Path to dylint_library_marker.rs's per-library stage-skip "
            "marker file. Combine with --library-stage-skip-marker-before."
        ),
    )
    parser.add_argument(
        "--library-stage-skip-marker-before",
        default=None,
        help=(
            "The `state` value a prior --snapshot-library-marker call "
            "emitted, e.g. from steps.<id>.outputs.state."
        ),
    )
    parser.add_argument(
        "--snapshot-library-marker",
        type=pathlib.Path,
        default=None,
        help=(
            "Snapshot-only mode: print and emit this marker path's current "
            "state (absent, or mtime:<ns>) as $GITHUB_OUTPUT `state`, then "
            "exit without producing the tree-size report. Run this once "
            "before ci-test; feed its `state` output back in as "
            "--library-stage-skip-marker-before after ci-test."
        ),
    )
    args = parser.parse_args(argv)

    if args.snapshot_library_marker is not None:
        state = marker_state(args.snapshot_library_marker)
        print(f"state={state}")
        emit_github_output("state", state)
        return 0

    lines = report(pathlib.Path(args.target_root))
    lines.extend(
        cache_layer_lines(
            parse_cache_hit_pairs(args.cache_hit),
            args.library_stage_skip_marker,
            args.library_stage_skip_marker_before,
        )
    )
    body = "\n".join(lines)
    print(body)

    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        try:
            with pathlib.Path(summary).open("a", encoding="utf-8") as handle:
                handle.write(body + "\n")
        except OSError as error:
            print(
                f"report_dylint_tree_size: summary unwritable: {error}", file=sys.stderr
            )
    return 0


if __name__ == "__main__":
    sys.exit(main())
