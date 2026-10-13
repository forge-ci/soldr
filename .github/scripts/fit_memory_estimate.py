#!/usr/bin/env python3
"""Join soldr's shadow memory estimates to measured compiler peaks (soldr#3152).

WHY
    soldr#3152 replaces admission name lists with a per-unit peak-memory
    estimate spent against live headroom. An estimate is only safe to spend
    once it has been checked against reality: an under-estimate is an OOM.
    This script is that check. It is the offline half of shadow mode.

WHAT IT READS
    * `admission-estimate.jsonl`: one row per Rust compiler child soldr's
      daemon classified, written by
      `crates/soldr-daemon/src/memory_estimate.rs::record`. Each row carries
      `estimate_bytes`, the features it was derived from, the admission
      decision (`exclusive`), and `args_digest`.
    * zccache `compile_journal.jsonl` (and rotated siblings / history
      copies): each compile's full `args` and, from zccache 1.13.23,
      `child_peak_rss_bytes`, the largest resident high-water mark sampled
      from the compiler children that request spawned.

JOIN KEY
    `unit_key` = `<crate name>/<cargo -C metadata>`. zccache hands admission
    the argument vector it executes, which it may have rewritten, but journals
    the client's original arguments, so a hash of the whole command line does
    not survive: the first CI data joined 92 of 1,235 estimates that way.
    Cargo's per-unit metadata hash survives in both. Schema-1 rows, which have
    no `unit_key`, fall back to `args_digest` (lower-case hex SHA-256 of every
    argument followed by one NUL byte). Both keys pin the same test vectors in
    Rust and Python, so neither side can drift silently.

WHAT IT REPORTS
    * `joined`: estimates matched to a journal row that carries a measured
      peak. When several journal rows share a digest, the largest peak wins,
      because that is the one admission must not under-predict.
    * `unmeasured`: matched, but no journal row for the digest has a peak (a
      cache hit, an older zccache, or a platform that could not measure).
    * `unjoined`: no journal row at all for the digest.
    * `under_predicted` and `worst_under_predictions`: rows whose measured
      peak exceeds the estimate, ranked by measured/estimate.
    * `measured_source`: `tree` when any joined measurement used zccache
      1.13.24's `tree_peak_rss_bytes` (compiler plus live descendants, so a
      linker grandchild counts), otherwise `child` (the compiler alone).
    * `history`: schema-3 rows score the per-unit history estimator
      (soldr#3152 step 4) the same way: `joined` units had a remembered
      `history_tree_peak_bytes`, `without_history` were first sightings, and
      `under_predicted` / `max_measured_over_history` measure how far the
      remembered peak fell short of the compile's measured peak.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Iterable, Iterator

JOURNAL_NAME = "compile_journal.jsonl"


def args_digest(args: Iterable[str]) -> str:
    """Hex SHA-256 of each argument followed by a NUL byte."""
    hasher = hashlib.sha256()
    for arg in args:
        hasher.update(arg.encode("utf-8"))
        hasher.update(b"\0")
    return hasher.hexdigest()


@dataclass(frozen=True)
class JournalRow:
    """The fields of one zccache journal row that the join needs."""

    args: tuple[str, ...]
    child_peak_rss_bytes: object
    tree_peak_rss_bytes: object


@dataclass(frozen=True)
class EstimateRow:
    """One admission-estimate row that carries a usable estimate and join key."""

    key: str
    estimate_bytes: int
    schema_version: int
    history_tree_peak_bytes: object
    crate_name: object
    is_test: object
    lto: object
    exclusive: object


@dataclass(frozen=True)
class PeakRecord:
    """Largest measured peaks for one join key; None when never measured."""

    key: str
    child_peak: int | None
    tree_peak: int | None


@dataclass(frozen=True)
class PeakPair:
    crate_name: object
    is_test: object
    lto: object
    exclusive: object
    estimate_bytes: int
    measured_bytes: int
    measured_over_estimate: float | None


@dataclass(frozen=True)
class HistoryPair:
    history: int
    measured: int


@dataclass(frozen=True)
class HistoryReport:
    joined: int
    without_history: int
    under_predicted: int
    max_measured_over_history: float | None


@dataclass(frozen=True)
class Report:
    estimates: int
    joined: int
    unmeasured: int
    unjoined: int
    measured_source: str
    under_predicted: int
    max_measured_over_estimate: float | None
    worst_under_predictions: tuple[PeakPair, ...]
    history: HistoryReport


def _read_jsonl(path: Path) -> Iterator[object]:
    """Every JSON value on a line of `path`; unparseable lines are skipped."""
    with path.open(encoding="utf-8", errors="replace") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue


def _read_journal(path: Path) -> Iterator[JournalRow]:
    for row in _read_jsonl(path):
        if not isinstance(row, dict):
            continue
        args = row.get("args")
        if not isinstance(args, list) or not all(isinstance(a, str) for a in args):
            continue
        yield JournalRow(
            args=tuple(args),
            child_peak_rss_bytes=row.get("child_peak_rss_bytes"),
            tree_peak_rss_bytes=row.get("tree_peak_rss_bytes"),
        )


def _read_estimates(path: Path) -> Iterator[EstimateRow]:
    for row in _read_jsonl(path):
        if not isinstance(row, dict):
            continue
        estimate = row.get("estimate_bytes")
        key = row.get("unit_key") or row.get("args_digest")
        if not isinstance(estimate, int) or not isinstance(key, str):
            continue
        schema_version = row.get("schema_version", 1)
        yield EstimateRow(
            key=key,
            estimate_bytes=estimate,
            schema_version=schema_version if isinstance(schema_version, int) else 1,
            history_tree_peak_bytes=row.get("history_tree_peak_bytes"),
            crate_name=row.get("crate_name"),
            is_test=row.get("is_test"),
            lto=row.get("lto"),
            exclusive=row.get("exclusive"),
        )


def unit_key(args: list[str]) -> str | None:
    """`<crate name>/<cargo -C metadata>`, or None without both parts."""
    name = None
    metadata = None
    index = 0
    while index < len(args):
        arg = args[index]
        value = None
        if arg == "--crate-name" and index + 1 < len(args):
            name = args[index + 1]
            index += 1
        elif arg.startswith("--crate-name="):
            name = arg.split("=", 1)[1]
        elif arg == "-C" and index + 1 < len(args):
            value = args[index + 1]
            index += 1
        elif arg.startswith("-C") and len(arg) > 2:
            value = arg[2:]
        if value is not None and value.startswith("metadata="):
            metadata = value.split("=", 1)[1]
        index += 1
    if name is None or metadata is None:
        return None
    return f"{name}/{metadata}"


def discover_journals(paths: Iterable[Path]) -> list[Path]:
    """Journal files named directly, or found under directories."""
    found: set[Path] = set()
    for path in paths:
        if path.is_file():
            found.add(path)
        elif path.is_dir():
            for candidate in path.rglob(f"{JOURNAL_NAME}*"):
                if candidate.is_file():
                    found.add(candidate)
    return sorted(found)


def _merge_peak(peaks: dict[str, int | None], key: str, peak: object) -> None:
    current = peaks.get(key)
    if isinstance(peak, int) and peak > 0:
        peaks[key] = peak if current is None else max(current, peak)
    else:
        peaks.setdefault(key, None)


def _journal_peaks(journals: Iterable[Path]) -> tuple[PeakRecord, ...]:
    """Child and tree peaks per join key; None when never measured.

    Each journal row is indexed under both its `args_digest` and, when cargo
    supplied one, its `unit_key`, so every estimate schema joins from the same
    records.
    """
    child: dict[str, int | None] = {}
    tree: dict[str, int | None] = {}
    for journal in journals:
        for row in _read_journal(journal):
            args = list(row.args)
            keys = [args_digest(args)]
            key = unit_key(args)
            if key is not None:
                keys.append(key)
            for join_key in keys:
                _merge_peak(child, join_key, row.child_peak_rss_bytes)
                _merge_peak(tree, join_key, row.tree_peak_rss_bytes)
    return tuple(PeakRecord(key, peak, tree[key]) for key, peak in child.items())


def measured_peaks(journals: Iterable[Path]) -> tuple[PeakRecord, ...]:
    """Largest measured peaks per join key."""
    return _journal_peaks(journals)


def analyze(
    estimate_logs: Iterable[Path], journal_paths: Iterable[Path], top: int = 10
) -> Report:
    """Join estimates to measured peaks and summarise prediction error."""
    peaks = {
        record.key: record
        for record in _journal_peaks(discover_journals(journal_paths))
    }
    estimates = 0
    joined = 0
    unmeasured = 0
    unjoined = 0
    used_tree = False
    pairs: list[PeakPair] = []
    history_pairs: list[HistoryPair] = []
    without_history = 0
    for log in estimate_logs:
        for row in _read_estimates(log):
            estimate = row.estimate_bytes
            estimates += 1
            record = peaks.get(row.key)
            if record is None:
                unjoined += 1
                continue
            tree = record.tree_peak
            measured = tree if tree is not None else record.child_peak
            if measured is None:
                unmeasured += 1
                continue
            used_tree = used_tree or tree is not None
            joined += 1
            if row.schema_version >= 3:
                history = row.history_tree_peak_bytes
                if isinstance(history, int) and history > 0:
                    history_pairs.append(HistoryPair(history, measured))
                else:
                    without_history += 1
            pairs.append(
                PeakPair(
                    crate_name=row.crate_name,
                    is_test=row.is_test,
                    lto=row.lto,
                    exclusive=row.exclusive,
                    estimate_bytes=estimate,
                    measured_bytes=measured,
                    measured_over_estimate=measured / estimate if estimate else None,
                )
            )

    under = [p for p in pairs if p.measured_bytes > p.estimate_bytes]
    under.sort(key=lambda p: p.measured_over_estimate or 0.0, reverse=True)
    ratios = [p.measured_over_estimate for p in pairs if p.measured_over_estimate]
    history_ratios = [pair.measured / pair.history for pair in history_pairs]
    return Report(
        estimates=estimates,
        joined=joined,
        unmeasured=unmeasured,
        unjoined=unjoined,
        measured_source="tree" if used_tree else "child",
        under_predicted=len(under),
        max_measured_over_estimate=max(ratios) if ratios else None,
        worst_under_predictions=tuple(under[:top]),
        history=HistoryReport(
            joined=len(history_pairs),
            without_history=without_history,
            under_predicted=sum(
                1 for pair in history_pairs if pair.measured > pair.history
            ),
            max_measured_over_history=max(history_ratios) if history_ratios else None,
        ),
    )


def _format_text(report: Report) -> str:
    lines = [
        f"estimates: {report.estimates}",
        f"joined (measured peak): {report.joined}",
        f"unmeasured (no peak on any matching journal row): {report.unmeasured}",
        f"unjoined (no matching journal row): {report.unjoined}",
        f"measured source: {report.measured_source}",
        f"under-predicted: {report.under_predicted}",
        f"max measured/estimate: {report.max_measured_over_estimate}",
    ]
    history = report.history
    lines.append(
        f"history: joined {history.joined}, first sightings "
        f"{history.without_history}, under-predicted "
        f"{history.under_predicted}, max measured/history "
        f"{history.max_measured_over_history}"
    )
    for pair in report.worst_under_predictions:
        lines.append(
            f"  {pair.crate_name}: measured {pair.measured_bytes} > "
            f"estimate {pair.estimate_bytes} "
            f"(x{pair.measured_over_estimate:.2f})"
        )
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--estimates",
        nargs="+",
        required=True,
        type=Path,
        help="admission-estimate.jsonl file(s) written by soldr-daemon",
    )
    parser.add_argument(
        "--journal",
        nargs="+",
        required=True,
        type=Path,
        help="compile_journal.jsonl file(s) or directories to search",
    )
    parser.add_argument("--top", type=int, default=10)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    report = analyze(args.estimates, args.journal, top=args.top)
    if args.json:
        print(json.dumps(asdict(report), indent=2))
    else:
        print(_format_text(report))
    return 0


if __name__ == "__main__":
    sys.exit(main())
