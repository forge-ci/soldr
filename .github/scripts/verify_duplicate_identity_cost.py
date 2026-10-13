#!/usr/bin/env python3
"""Verify whether the 525x duplicate `cc` identity is real work (soldr#3045).

soldr#3045 is step 0 of Phase 1 of zccache in-flight coalescing (child of
soldr#3039, the "measure before you optimize" umbrella). soldr#3039's
baseline table reports 579 of 1,569 compiles as exact-`context_key`
duplicates, all concurrent, 525 of them one `cc` identity compiled under
`-C linker=dylint-link`. The checked-in baseline JSON
(`baselines/compile_journal_baseline_33536940076.json`) pins the 579/0
concurrent/sequential split and carries the `gh run download` line for the
artifact it came from; the 525x `cc` identity is in the issue's table, NOT
in that file -- which is exactly why it has to be re-derived here rather
than quoted. Before building coalescing, this tool answers, from a real
compile journal, the two questions that decide whether that number is worth
building anything for:

    1. Is the 525x duplicate `cc` identity real compilation, or is it cc-rs /
       autoconf-style capability probing (`--version`, `-dM -E`, throwaway
       `conftest.c` files, `/dev/null` targets)? A probe that runs 525 times
       concurrently is still cheap; a real compile that runs 525 times
       concurrently is a genuine coalescing opportunity.
    2. What is the CEILING on what in-flight coalescing could save? For any
       group of duplicate invocations that ran concurrently, at most all but
       one (the "leader") could ever be coalesced away -- so the ceiling per
       identity is `sum(latency) - max(latency)`, never the full sum.

This script does not decide soldr#3045's fate. It computes the numbers and
prints a mechanical verdict from a fixed threshold; closing or continuing a
phase of soldr#3039 is the owner's call (see CLAUDE.md's code-smell rule:
"a design question rather than a typo" needs an owner decision, not a tool).

TWO SCHEMA FACTS THAT BOUND THE ANSWER
    (a) NO CPU-TIME FIELD. `analyze_compile_journal.py` already documents
        that the zccache journal carries no CPU-time field and emits
        `*_cpu_seconds: None`. Every duration this tool prints is WALL
        latency derived from `latency_ns`, never CPU time -- every output
        key here says `wall`, and none says `cpu` next to a number.
    (b) RSS FIELDS ARE DROPPED BY `Record`. `analyze_compile_journal.Record`
        deliberately does not carry `child_peak_rss_bytes` or
        `tree_peak_rss_bytes` (see that module's docstring on optional
        extended fields). Those two fields are the best real-vs-probe memory
        evidence a journal has, so this tool reads the raw JSON object
        itself and keeps `(record, obj)` pairs throughout, rather than
        relying on `Record` alone.

READING INPUT
    Paths are resolved with `analyze_compile_journal.discover_journal_files`.
    Each resolved file is then read line by line, mirroring that module's
    own `_read_journals`: strip, skip blanks, dedupe byte-identical lines
    across ALL files by default, `json.loads`, then
    `record_from_json(obj, str(path))`. A line that fails to parse, or whose
    `record_from_json` returns None, is counted as malformed and skipped --
    never fatal.

    KEEP DEDUPE ON. `--no-dedupe` exists to inspect the raw files, NOT to
    measure duplicates. A `build-logs-*` artifact's
    `cache/zccache/history/<id>/compile_journal.jsonl` files are nested
    SNAPSHOTS of one growing journal -- measured on run 33536940076's
    artifact, the files form a strict-superset chain (424 lines subset-of 454
    subset-of 500), so 2,439 raw lines carry only 1,538 distinct records. With
    dedupe off, every re-snapshotted record becomes a fake "concurrent
    duplicate": this tool then reports 34% coalescable on a journal whose true
    concurrent-duplicate count is ZERO. A record's `ts` is millisecond-stamped,
    so two byte-identical lines are one compile written twice, never two
    compiles.

GROUPING
    Records are grouped by `record.context_key`. A record with a falsy
    `context_key` is excluded from grouping and counted separately as
    `records_without_context_key` (this matches
    `analyze_compile_journal._compute_duplicates`, where ~17% of a real
    journal has no key). Only groups with more than one member, AND with at
    least `--min-count` (default 2) members, are duplicate groups. Duplicate
    groups are sorted by member count descending, tied-break by the
    `context_key` string, and the first `--top N` (default 10) are reported
    in full detail. Every OTHER total in this tool's output (group counts,
    excess records, flavour totals, coalescable-seconds totals) is computed
    over ALL qualifying duplicate groups, not just the reported top N.

CLASSIFIER (`classify_invocation`) -- THE HEART OF THIS TOOL
    Returns "probe" | "real-compile" | "unknown" for one journal invocation.
    Every rule below is evidence-backed: the two argv shapes are literal
    cc-rs / openssl-build capability probes read out of real local journals
    under `~/.soldr/cache/zccache/history`, both recorded against
    `"compiler": "/run/current-system/sw/bin/cc"`:

        ["-dM", "-E", "-x", "c", "/dev/null"]
        ["-Wa,--help", "-c", "-o", "null.3117452.o", "-x", "assembler",
         "/dev/null"]

    Rules, in the order applied:
      * Non-native (a rustc record, `_is_native_compiler` is False) with
        `--crate-name` present -> "real-compile". rustc only sets
        `--crate-name` when it is actually compiling a crate.
      * Native + `/dev/null` in any arg -> "probe" (both examples above).
      * Native + an arg equal to `--version`, `-V`, `--help`, `-dumpmachine`,
        `-dumpversion`, or starting with `-Wa,--help` or `--print`
        -> "probe" (matches example 2's `-Wa,--help`).
      * Native + `-E` or `-dM` present -> "probe" (matches example 1).
      * Native + exactly one source-extension arg (see
        `analyze_compile_journal.NATIVE_SRC_EXTS`) whose basename matches
        `^(null|probe|test|conftest)[-_.0-9]*\\.[A-Za-z+]+$` -> "probe"
        (cc-rs writes throwaway probe files with names of that shape).
      * Native + `-c` present + at least one source-extension arg that is
        neither `/dev/null` nor a throwaway-named file -> "real-compile".
      * Anything else -> "unknown".

    NOT A RULE: latency. A slow probe on a contended runner is still a
    probe, and a fast compile is still a compile. Latency is reported next
    to the classification so a reader can judge it, but it never defines
    the class -- classifying by latency would make a busy CI runner turn
    real compiles into "probes" and vice versa, silently, per run.

OUTPUT SHAPE
    `main`'s summary is `{"totals": {...}, "identities": [...]}`.
    `totals` carries the aggregate counts described above plus three
    savings figures computed over duplicate groups that have at least one
    "concurrent" member (see `analyze_compile_journal._classify_duplicate`):
    `coalescable_wall_seconds_total`, `_probe`, `_real` (bucketed by the
    MAJORITY `classify_invocation` result across that group's members, ties
    broken toward "real-compile" -- see `_group_classification`; "unknown"
    groups land in `coalescable_wall_seconds_unknown`, neither probe nor
    real), and `coalescable_fraction_real` = `coalescable_wall_seconds_real`
    / `total_wall_seconds` (0.0 when the denominator is 0).
    Each entry in `identities` is one reported duplicate group's full detail
    (classification, wall-latency and RSS statistics, coalescable ceiling).

WHAT `total_wall_seconds` IS, AND IS NOT
    It is the SUM of every record's `latency_ns`, i.e. aggregate per-compile
    latency summed across compiles that ran in parallel. It is NOT the
    elapsed wall-clock of the build, and it is always much larger than it on
    an N-way runner. Every ratio in this tool -- including
    `coalescable_fraction_real` and the verdict's percentage -- is therefore
    a fraction of AGGREGATE COMPILE LATENCY. A coalesced second is a second
    of compiler work not done; how much of it reaches the build's elapsed
    time depends on whether that unit was on the critical path, which this
    journal cannot say. Read the percentage as "how much of the compiler
    work is redundant", never as "how much shorter the build gets".

VERDICT
    `verdict(totals)` is mechanical: `coalescable_fraction_real < 0.05` is
    "negligible" (recommend closing soldr#3045, but that call belongs to the
    owner), `>= 0.15` is "proceed", otherwise "need-better-data". The
    rendered sentence always carries the real-compile coalescable seconds,
    the total wall seconds, and the percentage it was computed from.

    ONE OVERRIDE, and it only ever weakens a close: a "negligible" that is
    computed while the UNKNOWN bucket alone is >= 5% of `total_wall_seconds`
    is downgraded to "need-better-data". "Negligible" is the verdict that
    argues for closing soldr#3045, and it is only honest when the work it
    excluded was actually classified. A big unclassified bucket means the
    classifier did not recognise this journal's shapes -- that is a reason
    to teach the classifier, not a reason to close the issue. The override
    can never turn a "proceed" into anything else, so it cannot hide payoff.

EXIT CODE
    Returns 1 when `discover_journal_files` finds no files at all (an
    operator path typo, which must not look like a clean result), and 2 when
    a requested `--json-out` / `--markdown-out` file could not be written.
    The second one matters because `--markdown-out` is the deliverable: the
    documented workflow pipes it straight into `gh issue comment
    --body-file`, so a silently-failed write would post a STALE file left
    over from an earlier run. Every other case -- including a journal with
    zero duplicate groups -- returns 0, because "no duplicates measured" is
    itself a real, reportable answer.

SCOPE: POINT THIS AT ONE RUN'S JOURNAL
    The concurrent/sequential/cross_generation flavour is decided pairwise
    inside each duplicate group (that is what
    `analyze_compile_journal._classify_duplicate` does), so the work is
    quadratic in the largest group. One `ci-test` run's journal has a
    largest group in the hundreds, which is instant. The whole
    `~/.soldr/cache/zccache/history/` tree is thousands of runs' journals
    merged into one grouping, which is both slow AND wrong for this
    question: records from two different runs are not each other's
    in-flight duplicates, and merging them inflates every group. Pass a
    single `history/<id>/` directory, or the journal(s) from one
    `build-logs-*` CI artifact.

USAGE
    python3 .github/scripts/verify_duplicate_identity_cost.py \\
        ~/.soldr/cache/zccache/history/<run-id>/ --top 10 \\
        --markdown-out summary.md

THE ANSWER THIS TOOL PRODUCED (run 33536940076, step 0's deliverable)
    Concurrent exact-`context_key` duplicates: 0. Coalescable real-compile
    wall time: 0.00s of 2442.21s. The 525x `cc` identity is REAL C
    compilation and NOT a duplicate:

      * The number comes from `soldr ci-test`'s compiler-unit report, not
        from this journal. That run's job log reads "1531 executions, 963
        identities, 568 duplicate executions", and its largest duplicate
        identity (`9308d1c2...`, `compiler=/usr/bin/cc`,
        `rustflags=-Clinker=dylint-link`) fired 412 times.
      * Those 412 are 206 distinct libgit2 C sources compiled twice each,
        204.3s aggregate, into two different build-script out-dirs
        (`libgit2-sys-6f2fbc7f71825ed9` vs `libgit2-sys-ac995ae7486d7a10`)
        with different flags (one carries `-mno-omit-leaf-frame-pointer`),
        ~3.5 minutes apart. All 412 have DISTINCT `context_key`s.
      * They collapse to one identity because
        `soldr-daemon/src/ci_test_report.rs::normalized_identity` is
        rustc-shaped: for a `cc` argv, `--crate-name`, `--crate-type`,
        `--target`, the `.rs` source, `--cfg` and `-C` are all empty, so the
        identity reduces to compiler + `CARGO_FEATURE_*` + RUSTFLAGS + cwd
        and ignores the C file and every cc argument.

    Coalescing keys on `context_key` and only fires for requests that
    overlap in time, so it would have coalesced ZERO of them. The 204.3s is
    real redundant work, but it is a fingerprint/out-dir problem, not an
    in-flight one.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import pathlib
import re
import sys
from collections import Counter
from dataclasses import asdict, dataclass
from typing import Any, TypeVar

# Loaded with importlib.util.spec_from_file_location rather than a package
# import: this script and analyze_compile_journal.py both live in
# `.github/scripts/`, which is executed as standalone files by CI, not
# imported as a package. A `sys.path.insert` + plain `import` would work too,
# but flake8's E402 (module-level import not at top of file) would then need
# a per-file exemption in `.flake8` -- a file this task does not own.
_ANALYZER_PATH = (
    pathlib.Path(__file__).resolve().with_name("analyze_compile_journal.py")
)
_SPEC = importlib.util.spec_from_file_location(
    "analyze_compile_journal", _ANALYZER_PATH
)
if _SPEC is None or _SPEC.loader is None:  # pragma: no cover - packaging accident
    raise ImportError(f"cannot load {_ANALYZER_PATH}")
analyze_compile_journal = importlib.util.module_from_spec(_SPEC)
sys.modules[_SPEC.name] = analyze_compile_journal
_SPEC.loader.exec_module(analyze_compile_journal)


T = TypeVar("T", int, float)

# Literal probe flags read out of real cc-rs / autoconf invocations. See the
# module docstring's CLASSIFIER section for the two argv shapes these were
# derived from.
_PROBE_EXACT_FLAGS = frozenset(
    {"--version", "-V", "--help", "-dumpmachine", "-dumpversion"}
)
# cc-rs writes throwaway probe source files named like `null.3117452.c`,
# `probe.c`, `conftest.c`, or plain `test.c`.
_THROWAWAY_SRC_RE = re.compile(r"^(null|probe|test|conftest)[-_.0-9]*\.[A-Za-z+]+$")


def _has_crate_name_flag(args: list[str]) -> bool:
    return any(arg == "--crate-name" or arg.startswith("--crate-name=") for arg in args)


def _has_dev_null(args: list[str]) -> bool:
    return any("/dev/null" in arg for arg in args)


def _has_probe_flag(args: list[str]) -> bool:
    for arg in args:
        if arg in _PROBE_EXACT_FLAGS:
            return True
        if arg.startswith("-Wa,--help") or arg.startswith("--print"):
            return True
    return False


def _has_preprocess_flag(args: list[str]) -> bool:
    return "-E" in args or "-dM" in args


def _only_source_arg_is_throwaway(args: list[str]) -> bool:
    src_args = [
        arg
        for arg in args
        if not arg.startswith("-")
        and arg.endswith(analyze_compile_journal.NATIVE_SRC_EXTS)
    ]
    if len(src_args) != 1:
        return False
    basename = pathlib.PurePosixPath(src_args[0].replace("\\", "/")).name
    return bool(_THROWAWAY_SRC_RE.match(basename))


def _has_real_native_source(args: list[str]) -> bool:
    for arg in args:
        if arg.startswith("-"):
            continue
        if not arg.endswith(analyze_compile_journal.NATIVE_SRC_EXTS):
            continue
        if "/dev/null" in arg:
            continue
        basename = pathlib.PurePosixPath(arg.replace("\\", "/")).name
        if _THROWAWAY_SRC_RE.match(basename):
            continue
        return True
    return False


def classify_invocation(args: list[str], compiler: str | None) -> str:
    """Classify one journal invocation as "probe" | "real-compile" | "unknown".

    See the module docstring's CLASSIFIER section for the full rule list and
    the evidence each rule is backed by. Deliberately NOT latency-based --
    see the NOT A RULE note there.
    """
    # Reuses the analyzer's own compiler-basename check rather than
    # duplicating it -- see the module docstring on why this drifts if
    # re-derived.
    # pylint: disable-next=protected-access
    native = analyze_compile_journal._is_native_compiler(compiler)

    if not native:
        if _has_crate_name_flag(args):
            return "real-compile"
        return "unknown"

    if (
        _has_dev_null(args)
        or _has_probe_flag(args)
        or _has_preprocess_flag(args)
        or _only_source_arg_is_throwaway(args)
    ):
        return "probe"

    if "-c" in args and _has_real_native_source(args):
        return "real-compile"

    return "unknown"


# Tie-break order when one duplicate group's members do not all classify the
# same way. Higher wins. "real-compile" is deliberately the top of the order:
# this tool's "negligible" verdict argues for CLOSING soldr#3045, so a tie must
# never be resolved in the direction that shrinks the measured payoff.
_CLASSIFICATION_PRIORITY = {"real-compile": 2, "unknown": 1, "probe": 0}


def _group_classification(counts: dict[str, int]) -> str:
    """Majority classification for one duplicate group, ties broken upward.

    A `context_key` group is by construction the same invocation identity, so
    a mixed group is unexpected -- but bucketing a 525-member group's
    coalescable seconds off `records[0]` alone would let one stray member flip
    the entire bucket, and a journal's record order is arrival order, not
    anything meaningful. Majority + a documented tie-break removes that
    dependence on ordering.
    """
    if not counts:
        return "unknown"
    return max(
        counts.items(),
        key=lambda item: (item[1], _CLASSIFICATION_PRIORITY.get(item[0], 0)),
    )[0]


def _percentile(values: list[T], quantile: float) -> T:
    """Nearest-rank percentile on an already-sorted list. `values` must be non-empty."""
    idx = min(int(len(values) * quantile), len(values) - 1)
    return values[idx]


@dataclass(frozen=True)
class WallStats:
    """min/p50/p90/max/mean/total wall milliseconds over `latency_ns`."""

    min: float | None
    p50: float | None
    p90: float | None
    max: float | None
    mean: float | None
    total: float | None
    excluded: int


@dataclass(frozen=True)
class RssStats:
    """min/p50/max/samples for one raw-JSON peak-RSS field."""

    min: int | None
    p50: int | None
    max: int | None
    samples: int


@dataclass(frozen=True)
class JournalEntry:
    """One accepted journal line: its `Record` plus the two raw RSS fields.

    `Record` drops `child_peak_rss_bytes` / `tree_peak_rss_bytes`, so they are
    read from the raw decoded object when the line is accepted; `None` when
    the field is absent or not a (non-bool) integer.
    """

    record: Any
    child_peak_rss_bytes: int | None
    tree_peak_rss_bytes: int | None


@dataclass(frozen=True)
class JournalRead:
    """Result of `_read_journals_with_raw`."""

    entries: list[JournalEntry]
    malformed: int
    deduped_lines: int


@dataclass(frozen=True)
class ContextGroup:
    """Every journal entry sharing one `context_key`."""

    context_key: str
    members: list[JournalEntry]


@dataclass(frozen=True)
class Grouping:
    """Entries grouped by `context_key`, plus keyed/unkeyed record counts."""

    groups: list[ContextGroup]
    with_key: int
    without_key: int


@dataclass(frozen=True)
class Flavours:
    """Concurrent / sequential / cross-generation duplicate counts."""

    concurrent: int = 0
    sequential: int = 0
    cross_generation: int = 0


@dataclass(frozen=True)
class ClassificationCount:
    """How many members of a group classified as one invocation class."""

    name: str
    count: int


@dataclass(frozen=True)
class Identity:
    """One duplicated `context_key` group."""

    context_key: str
    crate: str
    count: int
    classification: str
    classification_counts: list[ClassificationCount]
    native: bool
    dylint_link: bool
    tree: str
    compilers: list[str]
    distinct_argv: int
    distinct_out_dirs: int
    argv: list[str]
    cwd: str
    flavours: Flavours
    wall_ms: WallStats
    child_peak_rss_bytes: RssStats
    tree_peak_rss_bytes: RssStats
    coalescable_wall_seconds: float

    def to_dict(self) -> dict:
        """JSON-ready dict; `classification_counts` is keyed by class name."""
        wire = asdict(self)
        wire["classification_counts"] = {
            row.name: row.count for row in self.classification_counts
        }
        return wire


@dataclass(frozen=True)
class Totals:
    """Journal-wide totals."""

    journal_files: list[str]
    records: int
    malformed: int
    deduped_lines: int
    records_with_context_key: int
    records_without_context_key: int
    duplicate_groups: int
    duplicate_records: int
    excess_records: int
    concurrent: int
    sequential: int
    cross_generation: int
    total_wall_seconds: float
    coalescable_wall_seconds_total: float
    coalescable_wall_seconds_probe: float
    coalescable_wall_seconds_real: float
    coalescable_wall_seconds_unknown: float
    coalescable_fraction_real: float


@dataclass(frozen=True)
class Summary:
    """What `build_summary()` returns; `to_dict()` is the JSON schema."""

    totals: Totals
    identities: list[Identity]

    def to_dict(self) -> dict:
        """JSON-ready dict, identical to the pre-dataclass schema."""
        return {
            "totals": asdict(self.totals),
            "identities": [identity.to_dict() for identity in self.identities],
        }


@dataclass(frozen=True)
class Verdict:
    """Mechanical verdict: a code and the sentence carrying its numbers."""

    code: str
    sentence: str


def _wall_ms_stats(records: list[Any]) -> WallStats:
    """min/p50/p90/max/mean/total (milliseconds) over `latency_ns`, None-safe.

    `latency_ns` is WALL latency -- see schema fact (a) in the module
    docstring. Records with `latency_ns is None` are excluded and counted
    under `excluded`.
    """
    values = sorted(
        record.latency_ns / 1e6 for record in records if record.latency_ns is not None
    )
    excluded = len(records) - len(values)
    if not values:
        return WallStats(None, None, None, None, None, None, excluded)
    return WallStats(
        min=values[0],
        p50=_percentile(values, 0.5),
        p90=_percentile(values, 0.9),
        max=values[-1],
        mean=sum(values) / len(values),
        total=sum(values),
        excluded=excluded,
    )


def _rss_stats(readings: list[int | None]) -> RssStats:
    """min/p50/max/samples for one raw-JSON RSS field, per schema fact (b).

    `Record` drops `child_peak_rss_bytes` / `tree_peak_rss_bytes`, so these
    come from the raw decoded journal objects (see `JournalEntry`). All four
    values are None/0 when no member of the group carries the field at all.
    """
    values = sorted(value for value in readings if value is not None)
    if not values:
        return RssStats(None, None, None, 0)
    return RssStats(
        min=values[0],
        p50=_percentile(values, 0.5),
        max=values[-1],
        samples=len(values),
    )


def _int_field(obj: Any, field: str) -> int | None:
    """`obj[field]` when it is a real (non-bool) integer, else None."""
    value = obj.get(field)
    if isinstance(value, int) and not isinstance(value, bool):
        return value
    return None


def _truncate_argv(args: list[str]) -> list[str]:
    """Truncate each element to 200 chars and the list to 80 elements + marker."""
    truncated = [arg if len(arg) <= 200 else arg[:200] for arg in args]
    if len(truncated) > 80:
        remaining = len(truncated) - 80
        truncated = [*truncated[:80], f"... ({remaining} more)"]
    return truncated


def _read_journals_with_raw(
    journal_files: list[pathlib.Path], dedupe: bool
) -> JournalRead:
    """Read journals into `JournalEntry` rows, mirroring `_read_journals`.

    A raw JSON object is only kept once `record_from_json` accepts it (which
    requires `isinstance(obj, dict)`), so every `obj` here is a `dict`.
    """
    entries: list[JournalEntry] = []
    malformed = 0
    deduped_lines = 0
    seen: set[str] = set()
    for path in journal_files:
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        for raw_line in text.splitlines():
            line = raw_line.strip()
            if not line:
                continue
            if dedupe:
                if line in seen:
                    deduped_lines += 1
                    continue
                seen.add(line)
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                malformed += 1
                continue
            record = analyze_compile_journal.record_from_json(obj, str(path))
            if record is None:
                malformed += 1
                continue
            entries.append(
                JournalEntry(
                    record=record,
                    child_peak_rss_bytes=_int_field(obj, "child_peak_rss_bytes"),
                    tree_peak_rss_bytes=_int_field(obj, "tree_peak_rss_bytes"),
                )
            )
    return JournalRead(entries, malformed, deduped_lines)


def _group_by_context_key(entries: list[JournalEntry]) -> Grouping:
    by_key: dict[str, list[JournalEntry]] = {}
    with_key = 0
    without_key = 0
    for entry in entries:
        if entry.record.context_key:
            by_key.setdefault(entry.record.context_key, []).append(entry)
            with_key += 1
        else:
            without_key += 1
    groups = [ContextGroup(key, members) for key, members in by_key.items()]
    return Grouping(groups, with_key, without_key)


def _build_identity(
    context_key: str,
    members: list[JournalEntry],
    *,
    flavours: Flavours,
    classification: str,
    classification_counts: dict[str, int],
    coalescable_wall_seconds: float,
) -> Identity:
    records = [entry.record for entry in members]
    first_record = records[0]
    compilers = sorted(
        {
            record.compiler.replace("\\", "/").rsplit("/", 1)[-1]
            for record in records
            if record.compiler
        }
    )
    return Identity(
        context_key=context_key,
        crate=first_record.crate_name,
        count=len(members),
        classification=classification,
        classification_counts=[
            ClassificationCount(name, count)
            for name, count in classification_counts.items()
        ],
        # pylint: disable-next=protected-access
        native=analyze_compile_journal._is_native_compiler(first_record.compiler),
        dylint_link=any(record.dylint_link for record in records),
        tree=first_record.tree,
        compilers=compilers,
        distinct_argv=len({tuple(record.args) for record in records}),
        distinct_out_dirs=len({record.out_dir for record in records}),
        argv=_truncate_argv(first_record.args),
        cwd=first_record.cwd,
        flavours=flavours,
        wall_ms=_wall_ms_stats(records),
        child_peak_rss_bytes=_rss_stats([e.child_peak_rss_bytes for e in members]),
        tree_peak_rss_bytes=_rss_stats([e.tree_peak_rss_bytes for e in members]),
        coalescable_wall_seconds=coalescable_wall_seconds,
    )


def build_summary(
    journal_files: list[pathlib.Path],
    dedupe: bool = True,
    top: int = 10,
    min_count: int = 2,
) -> Summary:
    """Read `journal_files` and compute the full totals + identities summary."""
    journal = _read_journals_with_raw(journal_files, dedupe)
    grouping = _group_by_context_key(journal.entries)

    qualifying = [
        group
        for group in grouping.groups
        if len(group.members) > 1 and len(group.members) >= min_count
    ]
    sorted_groups = sorted(
        qualifying, key=lambda group: (-len(group.members), group.context_key)
    )

    duplicate_groups = len(sorted_groups)
    duplicate_records = sum(len(group.members) for group in sorted_groups)
    excess_records = sum(len(group.members) - 1 for group in sorted_groups)

    flavour_totals: Counter[str] = Counter()
    coalescable_total = 0.0
    coalescable_probe = 0.0
    coalescable_real = 0.0
    coalescable_unknown = 0.0
    identities: list[Identity] = []

    for rank, group in enumerate(sorted_groups):
        context_key, members = group.context_key, group.members
        records = [entry.record for entry in members]
        flavour_counts: Counter[str] = Counter()
        for record in records:
            others = [other for other in records if other is not record]
            # Reuses the analyzer's own concurrent/sequential/cross_generation
            # classifier rather than duplicating it.
            # pylint: disable-next=protected-access
            flavour = analyze_compile_journal._classify_duplicate(record, others)
            flavour_counts[flavour] += 1
            flavour_totals[flavour] += 1
        flavours = Flavours(
            concurrent=flavour_counts["concurrent"],
            sequential=flavour_counts["sequential"],
            cross_generation=flavour_counts["cross_generation"],
        )

        latencies = [
            record.latency_ns for record in records if record.latency_ns is not None
        ]
        coalescable_wall_seconds = (
            (sum(latencies) - max(latencies)) / 1e9 if latencies else 0.0
        )

        classification_counts: dict[str, int] = {}
        for record in records:
            cls = classify_invocation(record.args, record.compiler)
            classification_counts[cls] = classification_counts.get(cls, 0) + 1
        classification = _group_classification(classification_counts)

        if flavours.concurrent > 0:
            coalescable_total += coalescable_wall_seconds
            if classification == "probe":
                coalescable_probe += coalescable_wall_seconds
            elif classification == "real-compile":
                coalescable_real += coalescable_wall_seconds
            else:
                coalescable_unknown += coalescable_wall_seconds

        if rank < top:
            identities.append(
                _build_identity(
                    context_key,
                    members,
                    flavours=flavours,
                    classification=classification,
                    classification_counts=classification_counts,
                    coalescable_wall_seconds=coalescable_wall_seconds,
                )
            )

    total_wall_seconds = (
        sum((entry.record.latency_ns or 0) for entry in journal.entries) / 1e9
    )
    coalescable_fraction_real = (
        coalescable_real / total_wall_seconds if total_wall_seconds > 0 else 0.0
    )

    totals = Totals(
        journal_files=sorted(str(path) for path in journal_files),
        records=len(journal.entries),
        malformed=journal.malformed,
        deduped_lines=journal.deduped_lines,
        records_with_context_key=grouping.with_key,
        records_without_context_key=grouping.without_key,
        duplicate_groups=duplicate_groups,
        duplicate_records=duplicate_records,
        excess_records=excess_records,
        concurrent=flavour_totals["concurrent"],
        sequential=flavour_totals["sequential"],
        cross_generation=flavour_totals["cross_generation"],
        total_wall_seconds=total_wall_seconds,
        coalescable_wall_seconds_total=coalescable_total,
        coalescable_wall_seconds_probe=coalescable_probe,
        coalescable_wall_seconds_real=coalescable_real,
        coalescable_wall_seconds_unknown=coalescable_unknown,
        coalescable_fraction_real=coalescable_fraction_real,
    )

    return Summary(totals=totals, identities=identities)


def verdict(totals: Totals) -> Verdict:
    """Mechanical verdict from `coalescable_fraction_real`.

    < 0.05 -> "negligible" (recommend closing soldr#3045, but that call
    belongs to the owner -- a phase of soldr#3039 is closed by the owner,
    not by this tool). >= 0.15 -> "proceed". Otherwise -> "need-better-data".
    The sentence always carries the numbers it was derived from.

    One override, described in full in the module docstring's VERDICT
    section: a "negligible" reached while the UNKNOWN coalescable bucket is
    itself >= 5% of `total_wall_seconds` becomes "need-better-data", because
    that close would be resting on work the classifier never classified.
    """
    real_seconds = totals.coalescable_wall_seconds_real
    unknown_seconds = totals.coalescable_wall_seconds_unknown
    total_wall = totals.total_wall_seconds
    fraction = totals.coalescable_fraction_real
    percentage = fraction * 100.0
    unknown_fraction = unknown_seconds / total_wall if total_wall > 0 else 0.0
    numbers = (
        f"{real_seconds:.2f}s of real-compile coalescable wall time out of "
        f"{total_wall:.2f}s total measured wall time ({percentage:.2f}%)"
    )
    # Aggregate per-compile latency, not elapsed build time -- see the module
    # docstring. Stated in the sentence itself because this is the number that
    # gets quoted out of context into an issue comment.
    numbers += (
        ", where total measured wall time is the SUM of every record's "
        "latency across parallel compiles, not the build's elapsed time"
    )
    unknown_note = (
        f" Unclassified coalescable wall time: {unknown_seconds:.2f}s "
        f"({unknown_fraction * 100.0:.2f}% of total), excluded from the "
        "figure above."
    )

    if fraction < 0.05:
        if unknown_fraction >= 0.05:
            return Verdict(
                "need-better-data",
                "Cannot call this negligible: "
                + numbers
                + "."
                + unknown_note
                + " The unclassified bucket alone clears the 5% negligible "
                "threshold, so the close this would otherwise recommend "
                "would rest on work `classify_invocation` did not recognise. "
                "Teach the classifier the shapes in this journal, then "
                "re-run -- do not close soldr#3045 on this data.",
            )
        return Verdict(
            "negligible",
            "Negligible measured payoff: "
            + numbers
            + "."
            + unknown_note
            + " The mechanical recommendation is to close soldr#3045 as not "
            "worth building -- closing a phase of soldr#3039 is the owner's "
            "call, not this tool's.",
        )
    if fraction >= 0.15:
        return Verdict(
            "proceed",
            "Meaningful measured payoff: "
            + numbers
            + "."
            + unknown_note
            + " soldr#3045 is worth proceeding past step 0.",
        )
    return Verdict(
        "need-better-data",
        "Ambiguous measured payoff: "
        + numbers
        + "."
        + unknown_note
        + " This journal is neither clearly negligible nor clearly worth "
        "proceeding -- gather a larger or more representative journal before "
        "deciding soldr#3045's fate.",
    )


def _section(lines: list[str], title: str) -> None:
    lines.append("")
    lines.append(f"-- {title} --")


def render_text(summary: Summary, top: int = 10) -> str:
    """Render `summary` (the `Summary` `build_summary()` returns) as a console table."""
    lines: list[str] = []
    totals = summary.totals
    lines.append("== Duplicate identity cost verification (soldr#3045 step 0) ==")
    lines.append(
        f"journals: {len(totals.journal_files)} files, {totals.records} records, "
        f"{totals.malformed} malformed, {totals.deduped_lines} deduped lines"
    )

    _section(lines, "totals")
    lines.append(f"records_with_context_key: {totals.records_with_context_key}")
    lines.append(f"records_without_context_key: {totals.records_without_context_key}")
    lines.append(f"duplicate_groups: {totals.duplicate_groups}")
    lines.append(f"duplicate_records: {totals.duplicate_records}")
    lines.append(f"excess_records: {totals.excess_records}")
    lines.append(
        f"flavours: concurrent={totals.concurrent} sequential={totals.sequential} "
        f"cross_generation={totals.cross_generation}"
    )
    lines.append(f"total_wall_seconds: {totals.total_wall_seconds:.3f}")

    _section(lines, "coalescable wall seconds (ceiling on in-flight coalescing)")
    lines.append(f"total: {totals.coalescable_wall_seconds_total:.3f}")
    lines.append(f"probe: {totals.coalescable_wall_seconds_probe:.3f}")
    lines.append(f"real-compile: {totals.coalescable_wall_seconds_real:.3f}")
    lines.append(f"unknown: {totals.coalescable_wall_seconds_unknown:.3f}")
    lines.append(f"coalescable_fraction_real: {totals.coalescable_fraction_real:.4f}")

    _section(lines, f"top {top} duplicate identities")
    for rank, identity in enumerate(summary.identities[:top], start=1):
        p50 = identity.wall_ms.p50
        p50_display = "n/a" if p50 is None else f"{p50:.1f}"
        lines.append(
            f"  {rank:>2}. {identity.count:>5}x  {identity.crate}  "
            f"[{identity.classification}]  tree={identity.tree}  "
            f"dylint_link={identity.dylint_link}  p50_wall_ms={p50_display}  "
            f"coalescable_wall_seconds={identity.coalescable_wall_seconds:.3f}"
        )
        lines.append(f"      context_key={identity.context_key}")

    result = verdict(totals)
    _section(lines, "verdict")
    lines.append(f"[{result.code}] {result.sentence}")

    return "\n".join(lines)


def render_markdown(summary: Summary) -> str:
    """Render `summary` as a `gh issue comment 3045 --body-file`-ready markdown body."""
    totals = summary.totals
    identities = summary.identities
    lines: list[str] = []

    lines.append("### soldr#3045 step 0 -- duplicate identity cost verification")
    lines.append("")
    lines.append(
        f"Read {totals.records} records from {len(totals.journal_files)} "
        "journal file(s):"
    )
    for path in totals.journal_files:
        lines.append(f"- `{path}`")
    lines.append("")

    lines.append("#### Totals")
    lines.append("")
    lines.append("| metric | value |")
    lines.append("| --- | --- |")
    lines.append(f"| records | {totals.records} |")
    lines.append(f"| malformed | {totals.malformed} |")
    lines.append(f"| deduped_lines | {totals.deduped_lines} |")
    lines.append(f"| records_with_context_key | {totals.records_with_context_key} |")
    lines.append(
        f"| records_without_context_key | {totals.records_without_context_key} |"
    )
    lines.append(f"| duplicate_groups | {totals.duplicate_groups} |")
    lines.append(f"| duplicate_records | {totals.duplicate_records} |")
    lines.append(f"| excess_records | {totals.excess_records} |")
    lines.append(f"| concurrent | {totals.concurrent} |")
    lines.append(f"| sequential | {totals.sequential} |")
    lines.append(f"| cross_generation | {totals.cross_generation} |")
    lines.append(f"| total_wall_seconds | {totals.total_wall_seconds:.3f} |")
    lines.append(
        "| coalescable_wall_seconds_total | "
        f"{totals.coalescable_wall_seconds_total:.3f} |"
    )
    lines.append(
        "| coalescable_wall_seconds_probe | "
        f"{totals.coalescable_wall_seconds_probe:.3f} |"
    )
    lines.append(
        "| coalescable_wall_seconds_real | "
        f"{totals.coalescable_wall_seconds_real:.3f} |"
    )
    lines.append(
        "| coalescable_wall_seconds_unknown | "
        f"{totals.coalescable_wall_seconds_unknown:.3f} |"
    )
    lines.append(
        f"| coalescable_fraction_real | {totals.coalescable_fraction_real:.4f} |"
    )
    lines.append("")

    lines.append("#### Duplicate identities")
    lines.append("")
    lines.append(
        "| rank | crate | count | classification | dylint_link | tree | "
        "concurrent/sequential | p50 wall ms | total wall s | coalescable wall s |"
    )
    lines.append("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |")
    for rank, identity in enumerate(identities, start=1):
        wall = identity.wall_ms
        p50 = "n/a" if wall.p50 is None else f"{wall.p50:.1f}"
        total_s = "n/a" if wall.total is None else f"{wall.total / 1000.0:.3f}"
        flavours = identity.flavours
        lines.append(
            f"| {rank} | {identity.crate} | {identity.count} | "
            f"{identity.classification} | {identity.dylint_link} | "
            f"{identity.tree} | {flavours.concurrent}/{flavours.sequential} | "
            f"{p50} | {total_s} | {identity.coalescable_wall_seconds:.3f} |"
        )
    lines.append("")

    lines.append("#### Identity detail")
    lines.append("")
    for rank, identity in enumerate(identities, start=1):
        lines.append(f"`#{rank}` {identity.crate} (`{identity.context_key}`)")
        lines.append("")
        lines.append("```")
        # `compiler` and `native` are the decisive evidence for soldr#3045's
        # actual question -- a group whose crate is *named* `cc` is a rustc
        # compile of the cc-rs crate, while a group whose COMPILER is `cc` is
        # native work that may be a cc-rs probe. The table's crate column
        # cannot tell those apart, so the detail block names both.
        # `distinct_argv` is here for the same reason: it is how a reader
        # knows whether the single argv printed below represents the whole
        # group or just its first member.
        compilers = ", ".join(identity.compilers) or "(none recorded)"
        lines.append(f"compiler: {compilers}")
        lines.append(f"native compiler: {identity.native}")
        lines.append(f"distinct argv in group: {identity.distinct_argv}")
        lines.append(f"distinct out dirs in group: {identity.distinct_out_dirs}")
        # Rendered as `key=value` pairs rather than the dict itself: a dict
        # repr would put braces in the comment body, and the markdown is
        # pinned brace-free so no unrendered format placeholder can survive
        # into a posted comment.
        counts = " ".join(
            f"{row.name}={row.count}"
            for row in sorted(identity.classification_counts, key=lambda r: r.name)
        )
        lines.append(f"classification counts: {counts or '(none)'}")
        lines.append(f"cwd: {identity.cwd}")
        for arg in identity.argv:
            lines.append(arg)
        lines.append("```")
        lines.append("")

    lines.append("#### Schema limits")
    lines.append("")
    lines.append(
        "- No CPU-time field: the zccache journal carries no CPU-time field "
        "(`analyze_compile_journal.py` already documents this). Every "
        "duration above is WALL latency derived from `latency_ns`, never CPU "
        "time."
    )
    lines.append(
        "- `total_wall_seconds` is the SUM of every record's `latency_ns`, "
        "i.e. aggregate per-compile latency added up across compiles that ran "
        "in parallel. It is not the build's elapsed wall-clock and is much "
        "larger than it on an N-way runner. Read every percentage here as "
        '"share of compiler work that is redundant", not as "time the '
        'build would get shorter": whether a coalesced second reaches '
        "elapsed time depends on whether that unit sat on the critical path, "
        "which this journal cannot say."
    )
    lines.append(
        "- The `unknown` coalescable bucket is work `classify_invocation` "
        "could not call either way. It is excluded from "
        "`coalescable_wall_seconds_real`, so a large `unknown` makes the "
        "real-compile figure a floor rather than an estimate."
    )
    lines.append(
        "- RSS fields read from raw JSON: `analyze_compile_journal.Record` "
        "deliberately drops `child_peak_rss_bytes` and `tree_peak_rss_bytes`. "
        "This tool reads those two fields directly from the raw journal "
        "objects, since they are the best real-vs-probe memory evidence a "
        "journal has."
    )
    lines.append("")

    result = verdict(totals)
    lines.append("#### Recommendation")
    lines.append("")
    lines.append(f"**[{result.code}]** {result.sentence}")
    lines.append("")

    return "\n".join(lines)


def _write_output(path: str, content: str, label: str) -> bool:
    """Write `content` to `path`. Returns False (loudly) on any OSError.

    The caller turns a False into a non-zero exit deliberately: the
    documented workflow feeds `--markdown-out` straight to `gh issue comment
    --body-file`, and a failed write leaves whatever that path held before --
    so a swallowed error posts a STALE comment that looks like a fresh
    result. That is the one failure mode this whole script exists to avoid.
    """
    out_path = pathlib.Path(path)
    try:
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(content, encoding="utf-8")
    except OSError as error:
        print(
            f"verify_duplicate_identity_cost: could not write {label} {path}: "
            f"{error}. Anything already at that path is STALE -- do not post "
            "it.",
            file=sys.stderr,
        )
        return False
    return True


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "paths",
        nargs="*",
        help=(
            "compile-journal files or directories to scan -- ONE run's "
            "journal (a single history/<id>/ dir or one build-logs-* "
            "artifact), not the whole history tree; see the module docstring"
        ),
    )
    parser.add_argument(
        "--top", type=int, default=10, help="how many duplicate identities to report"
    )
    parser.add_argument(
        "--min-count",
        type=int,
        default=2,
        help="minimum member count for a group to count as a duplicate identity",
    )
    parser.add_argument(
        "--no-dedupe",
        action="store_true",
        help="do not drop byte-identical journal lines across files",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="print the summary as JSON instead of a table",
    )
    parser.add_argument(
        "--json-out", default=None, help="also write the summary JSON to this file"
    )
    parser.add_argument(
        "--markdown",
        action="store_true",
        help="print the markdown comment body instead of the text table",
    )
    parser.add_argument(
        "--markdown-out",
        default=None,
        help="also write the markdown comment body to this file",
    )
    args = parser.parse_args(argv)

    journal_files = analyze_compile_journal.discover_journal_files(args.paths)
    if not journal_files:
        print(
            "verify_duplicate_identity_cost: no compile journals found under "
            f"{args.paths!r} -- this is a wiring problem, not a result.",
            file=sys.stderr,
        )
        return 1

    summary = build_summary(
        journal_files,
        dedupe=not args.no_dedupe,
        top=args.top,
        min_count=args.min_count,
    )

    written = True
    if args.json_out:
        written &= _write_output(
            args.json_out, json.dumps(summary.to_dict(), indent=2) + "\n", "--json-out"
        )
    if args.markdown_out:
        written &= _write_output(
            args.markdown_out, render_markdown(summary), "--markdown-out"
        )

    # Printed even when a write failed: the numbers are still correct, and the
    # operator needs them on stdout to see what the unwritable file would have
    # said.
    if args.json:
        print(json.dumps(summary.to_dict(), indent=2))
    elif args.markdown:
        print(render_markdown(summary))
    else:
        print(render_text(summary, top=args.top))

    return 0 if written else 2


if __name__ == "__main__":
    raise SystemExit(main())
