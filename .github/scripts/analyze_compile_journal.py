#!/usr/bin/env python3
"""Analyze zccache compile-journal records for soldr#3039 phase metrics.

soldr#3040 is the "measure before you optimize" deliverable for soldr#3039:
every later phase of #3039 (dedup, dylint tree routing, the third-party miss
cost table, the Fresh/Compiling four-bucket join, ...) is judged against the
numbers this script prints, so the derivations below are written down once,
here, instead of being re-guessed per phase.

WHAT THIS READS
    zccache's `JournalEntry` records (crates/zccache-daemon-core/src/daemon/
    compile_journal/mod.rs in zackees/zccache, schema documented in that
    repo's docs/journal-schema.md), one JSON object per line, written to
    `compile_journal.jsonl` and rotated siblings
    (`compile_journal.jsonl.<ISO8601-stamp>`) plus copies archived under
    `cache/zccache/history/<id>/`.

TWO METRIC DEFINITIONS LATER PHASES DEPEND ON
    * A third-party COMPILE is a third-party record with outcome `miss`
      (`link_miss` is a separate, cheaper event and is deliberately excluded
      from the cost figure -- see `cost.third_party_miss_wall_seconds`). A
      `hit` is not a compile: it is what caching this record is buying.
    * DUPLICATES are records whose `context_key` appears more than once --
      the concurrent flavour of duplicate, not a key colliding with an
      unrelated key across time. Records with no `context_key` (~17% of a
      real journal) are excluded from grouping and counted separately as
      `duplicates.records_without_context_key`.

SCHEMA FINDINGS THIS SCRIPT ENCODES
    * `ts` is documented as the timestamp of the record WRITE, and
      `JournalEntry::new` stamps `SystemTime::now()` when the record is
      built -- i.e. AFTER the compile finished. There is no start
      timestamp. Every interval this script computes is therefore
      `[ts_ns - latency_ns, ts_ns]`, derived, not read.
    * There is no CPU-time field anywhere in the schema. Every
      `*_cpu_seconds` key in the output is JSON `null`, and the text report
      prints "unavailable (the zccache journal carries no CPU-time field)"
      rather than a silently-wrong zero.
    * `crate_name` / `crate_type` / `output_ext` are optional extended
      `--profile` fields that are ABSENT on every real record sampled
      (104,870 of them). This script derives both from `args` instead,
      mirroring zccache's own `derive.rs`: both `--flag value` and
      `--flag=value` spellings are handled, because both occur.
    * `context_key` is optional and absent on ~17% of local records; those
      records are excluded from duplicate grouping (see above).

FIRST-PARTY VS THIRD-PARTY, AND WHY IT DEVIATES FROM THE ISSUE TEXT
    #3039 says "a crate name belongs to a workspace member". That rule
    cannot reproduce its own baseline: the pinned baseline is 241
    first-party records across 31 distinct crate names, but the costliest
    third-party unit is named `build_script_build` (every build script
    shares that literal name, first- or third-party), and soldr's eight
    nextest test-category binaries are not workspace member names either.
    This script classifies by `cwd`
    instead -- cargo sets a rustc unit's cwd to its package root,
    so `/registry/src/` or `/git/checkouts/` in the (forward-slash
    normalized) cwd is the reliable third-party signal. A non-empty cwd
    without either marker is first-party. Only when `cwd` is itself missing
    or empty does this fall back to name membership in the workspace set
    (root `Cargo.toml` `members = [...]` plus every `dylints/*/Cargo.toml`
    package name), and even then the fallback can only say "unclassified",
    never manufacture a party. `cargo` log lines carry package names, not a
    cwd, so log-derived classification (the four-bucket join, below) is
    name-only -- the one place the name-only rule is authoritative rather
    than a fallback.

FRESH IS NOT OBSERVABLE FROM THE HOST LANE'S CARGO_LOG LEVEL
    `CARGO_LOG=cargo::core::compiler::fingerprint=info` only logs *dirty*
    units; cargo's `Fresh` status line needs `-v`, which the host
    validation run does not pass. Note that the variable is exported by
    `.github/workflows/_build-and-test.yml` (through `$GITHUB_ENV`, so the
    whole lane inherits it) -- **not** by the `ci-test` verb, which sets no
    `CARGO_LOG` at all. Running `soldr ci-test` locally therefore produces
    no `fingerprint dirty for` lines unless you export the variable
    yourself; if this script reports `cargo logs: none supplied` or zero
    dirty units off CI, that is the first thing to check. The `fresh` and
    `compiling_no_record` buckets are therefore `null` unless the caller
    supplies `--lockfile` / `--lockfiles-from-repo`, in which case they are
    computed by diffing `Cargo.lock` package names against journal records
    and cargo dirty/status lines. Without a lockfile the text report prints
    "n/a" for both -- that is the log level's ceiling, not a bug to chase.

CARGO_LOG CONTAMINATION FROM NESTED FIXTURES
    Some nextest test bodies launch their own nested cargo/compiler
    fixtures (see CLAUDE.md's ci-test note). Because the lane exports
    `CARGO_LOG` through `$GITHUB_ENV`, those nested cargos inherit it too,
    and their dirty lines land in the same capture file as the workspace's
    own build. This script
    cannot and does not try to strip them out -- `cargo.dirty_units` is
    report-only and may include this contamination. It is named here so a
    reader of a future phase's numbers does not mistake a nested fixture's
    churn for the workspace's own build.

BASELINE COMPARISON TOLERANCE
    `--compare-baseline` reads a JSON file whose `tolerance` field is a
    PERCENTAGE: an actual value is "within tolerance" when
    `abs(actual - baseline) <= abs(baseline) * tolerance / 100`, except a
    zero baseline requires an exact match (a percentage of zero is always
    zero, so anything else would silently accept any delta).

PARTY-LEVEL HITS/MISSES VS THE COST DEFINITION
    `first_party.hits` / `misses` and `third_party.hits` / `misses` count
    the broad `outcome in (hit, link_hit)` / `(miss, link_miss)` sets, the
    same grouping the four-bucket join uses. `cost.third_party_miss_records`
    and `cost.third_party_miss_wall_seconds` are strictly `outcome ==
    "miss"` only, per the metric definition above -- the two counts are
    expected to differ, so both are emitted rather than leaving a later
    phase to guess which one a threshold was written against.
    `check_third_party_compiles.py` ratchets on the strict count.

USAGE
    python3 .github/scripts/analyze_compile_journal.py \\
        ~/.soldr/cache/zccache/history/ --cargo-logs build.log --json
    python3 .github/scripts/analyze_compile_journal.py <journal-dir> \\
        --compare-baseline \\
        .github/scripts/baselines/compile_journal_baseline_33536940076.json \\
        --fail-on-baseline-delta
"""

from __future__ import annotations

import argparse
import datetime
import json
import re
import sys
from collections import Counter
from dataclasses import asdict, dataclass, field
from pathlib import Path, PurePosixPath
from typing import Any

SCHEMA_VERSION = 1

# Colour can wrap a status verb on either side (check_warm_rebuild.py's
# ANSI-stripping idea, redefined here since these scripts are not an
# importable package).
ANSI = re.compile(r"\x1b\[[0-9;]*m")

NATIVE_COMPILER_BASENAMES = frozenset(
    {"cc", "gcc", "clang", "cc.exe", "clang.exe", "g++", "c++"}
)
NATIVE_SRC_EXTS = (".c", ".cc", ".cpp", ".cxx", ".c++", ".s", ".S")

TS_RE = re.compile(
    r"^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})"
    r"(?:\.(\d+))?(Z|[+-]\d{2}:?\d{2})?$"
)

MEMBERS_RE = re.compile(r"members\s*=\s*\[(?P<body>[^\]]*)\]", re.DOTALL)
QUOTED_RE = re.compile(r'"([^"]+)"')
PACKAGE_SECTION_NAME_RE = re.compile(
    r"\[package\].*?name\s*=\s*\"([^\"]+)\"", re.DOTALL
)
ANY_NAME_RE = re.compile(r'name\s*=\s*"([^"]+)"')

DIRTY_RE = re.compile(
    r"fingerprint dirty for\s+(?P<name>[A-Za-z0-9_.+-]+)\s+v(?P<version>\S+)"
)
REASON_LABEL_RE = re.compile(r"(?:cause|dirty):\s*(?P<payload>.+)")
LEADING_IDENT_RE = re.compile(r"^([A-Za-z][A-Za-z0-9_]*)")
CAMEL_RE = re.compile(r"\b[A-Z][a-z0-9]*(?:[A-Z][a-z0-9]*)+\b")
STATUS_RE = re.compile(
    r"^(?:\s*\d+\.\d+\s+)?\s*(?P<verb>Compiling|Checking|Fresh)\s+"
    r"(?P<crate>[A-Za-z0-9_.\-]+)\s+v(?P<version>\S+)"
)

LOCKFILE_NAME_RE = re.compile(r'^\s*name\s*=\s*"([^"]+)"', re.MULTILINE)


@dataclass(frozen=True)
class MetricPath:
    """Where a baseline metric lives in the summary: `summary[section][key]`."""

    section: str
    key: str


BASELINE_METRIC_PATHS: dict[str, MetricPath] = {
    "total_units": MetricPath("totals", "records"),
    "third_party": MetricPath("totals", "third_party"),
    "first_party": MetricPath("totals", "first_party"),
    "hits": MetricPath("outcomes", "hit"),
    "context_not_found": MetricPath("miss_reasons", "context_not_found"),
    "uncacheable_input": MetricPath("miss_reasons", "uncacheable_input"),
    "duplicate_records_concurrent": MetricPath("duplicates", "concurrent"),
    "duplicate_records_sequential": MetricPath("duplicates", "sequential"),
    "tree_stable": MetricPath("trees", "stable"),
    "tree_dylint_target": MetricPath("trees", "dylint/target"),
    "tree_dylint_tests": MetricPath("trees", "dylint/tests"),
}


@dataclass
class Record:
    """One parsed zccache `JournalEntry`, plus soldr-derived fields.

    `party` starts as "unclassified" and is filled in by `analyze()` once
    the workspace crate-name set is known -- classification needs context
    this constructor does not have (see the module docstring).
    """

    source: str
    ts: str | None
    ts_ns: int | None
    outcome: str
    miss_reason: str | None
    context_key: str | None
    daemon_generation: str | None
    latency_ns: int | None
    compiler: str | None
    args: list[str]
    cwd: str | None
    exit_code: int | None
    session_id: str | None
    crate_name: str
    crate_type: str
    test_harness: bool
    dylint_link: bool
    out_dir: str
    tree: str
    party: str = "unclassified"


@dataclass(frozen=True)
class NamedCount:
    """One `name -> count` entry; JSON renders a list of these as an object."""

    name: str
    count: int


def _counts_to_json(entries: list[NamedCount]) -> dict:
    return {entry.name: entry.count for entry in entries}


def _named_counts(mapping: Counter[str]) -> list[NamedCount]:
    return [NamedCount(name, count) for name, count in sorted(mapping.items())]


@dataclass(frozen=True)
class Interval:
    """A derived `[start, end]` compile interval in nanoseconds."""

    start: int
    end: int


@dataclass(frozen=True)
class CrateIdentity:
    """Crate name and crate type derived for one journal record."""

    name: str
    crate_type: str


@dataclass(frozen=True)
class DirtyEntry:
    """One `fingerprint dirty for` cargo log line."""

    crate: str
    version: str
    reason: str


@dataclass(frozen=True)
class StatusEntry:
    """One `Compiling` / `Checking` / `Fresh` cargo status line."""

    verb: str
    crate: str
    version: str


@dataclass(frozen=True)
class ParsedCargoLog:
    """Dirty and status lines parsed out of one cargo log."""

    dirty: list[DirtyEntry] = field(default_factory=list)
    status: list[StatusEntry] = field(default_factory=list)


@dataclass(frozen=True)
class JournalRead:
    """Result of reading journal files."""

    records: list["Record"]
    malformed: int
    dropped: int


@dataclass(frozen=True)
class DuplicateIdentity:
    """One duplicated `context_key` group, for the top-identities list."""

    context_key: str
    crate: str
    count: int


@dataclass(frozen=True)
class DuplicatesStats:
    """Duplicate-record accounting (`summary.duplicates`)."""

    groups: int = 0
    records_in_groups: int = 0
    excess_records: int = 0
    concurrent: int = 0
    sequential: int = 0
    cross_generation: int = 0
    records_without_context_key: int = 0
    top_identities: list[DuplicateIdentity] = field(default_factory=list)


@dataclass(frozen=True)
class CostKey:
    """Grouping key for third-party miss cost."""

    crate: str
    context_key: str | None


@dataclass(frozen=True)
class CostItem:
    """Third-party miss cost for one (crate, context_key) pair."""

    crate: str
    context_key: str | None
    count: int
    wall_seconds: float


@dataclass(frozen=True)
class CostStats:
    """Third-party cost block (`summary.cost`)."""

    third_party_miss_records: int = 0
    third_party_miss_wall_seconds: float = 0.0
    third_party_miss_cpu_seconds: float | None = None
    top_third_party: list[CostItem] = field(default_factory=list)


@dataclass(frozen=True)
class UncacheableKey:
    """Grouping key for `uncacheable_input` misses."""

    crate: str
    crate_type: str
    dylint_link: bool
    test_harness: bool
    first_party: bool


@dataclass(frozen=True)
class UncacheableBucket:
    """One `uncacheable_input` bucket and its record count."""

    crate: str
    crate_type: str
    dylint_link: bool
    test_harness: bool
    first_party: bool
    count: int


@dataclass(frozen=True)
class UncacheableStats:
    """`summary.uncacheable_input`."""

    total: int = 0
    buckets: list[UncacheableBucket] = field(default_factory=list)


@dataclass
class CrateCargoCounts:
    """Per-crate cargo dirty/compiling/fresh tallies."""

    name: str = ""
    dirty: int = 0
    compiling: int = 0
    fresh: int = 0
    reasons: list[str] = field(default_factory=list)


@dataclass(frozen=True)
class CargoSummary:
    """`summary.cargo`."""

    available: bool = False
    dirty_units: int = 0
    third_party_dirty_units: int = 0
    dirty_reasons: list[NamedCount] = field(default_factory=list)
    per_crate: list[CrateCargoCounts] = field(default_factory=list)


@dataclass(frozen=True)
class BucketStats:
    """`summary.buckets` (the four-bucket join)."""

    third_party_total: int = 0
    fresh: int | None = None
    compiling_hit: int = 0
    compiling_miss: int = 0
    compiling_other: int = 0
    compiling_no_record: int | None = None


@dataclass(frozen=True)
class PartyStats:
    """Per-party hit/miss accounting (`summary.first_party` / `third_party`)."""

    records: int = 0
    hits: int = 0
    misses: int = 0
    crates: list[str] = field(default_factory=list)


@dataclass(frozen=True)
class ThirdPartyStats(PartyStats):
    """Third-party stats, which also carry the strict miss cost."""

    miss_wall_seconds: float | None = None
    miss_cpu_seconds: float | None = None


@dataclass(frozen=True)
class InputsInfo:
    """`summary.inputs`."""

    journal_files: list[str] = field(default_factory=list)
    records_read: int = 0
    malformed_lines: int = 0
    duplicate_lines_dropped: int = 0
    cargo_log_files: list[str] = field(default_factory=list)
    lockfiles: list[str] = field(default_factory=list)


@dataclass(frozen=True)
class Totals:
    """`summary.totals`."""

    records: int = 0
    first_party: int = 0
    third_party: int = 0
    unclassified: int = 0


@dataclass(frozen=True)
class Summary:
    """The full analysis result; `to_dict()` is the JSON payload."""

    schema_version: int = SCHEMA_VERSION
    inputs: InputsInfo = field(default_factory=InputsInfo)
    totals: Totals = field(default_factory=Totals)
    outcomes: list[NamedCount] = field(default_factory=list)
    miss_reasons: list[NamedCount] = field(default_factory=list)
    first_party: PartyStats = field(default_factory=PartyStats)
    third_party: ThirdPartyStats = field(default_factory=ThirdPartyStats)
    uncacheable_input: UncacheableStats = field(default_factory=UncacheableStats)
    duplicates: DuplicatesStats = field(default_factory=DuplicatesStats)
    trees: list[NamedCount] = field(default_factory=list)
    cost: CostStats = field(default_factory=CostStats)
    cargo: CargoSummary = field(default_factory=CargoSummary)
    buckets: BucketStats = field(default_factory=BucketStats)

    def to_dict(self) -> dict:
        """JSON-ready payload; key order matches the dataclass field order."""
        payload = asdict(self)
        payload["outcomes"] = _counts_to_json(self.outcomes)
        payload["miss_reasons"] = _counts_to_json(self.miss_reasons)
        payload["trees"] = _counts_to_json(self.trees)
        payload["cargo"]["dirty_reasons"] = _counts_to_json(self.cargo.dirty_reasons)
        payload["cargo"]["per_crate"] = {
            crate.name: {
                "dirty": crate.dirty,
                "compiling": crate.compiling,
                "fresh": crate.fresh,
                "reasons": crate.reasons,
            }
            for crate in self.cargo.per_crate
        }
        return payload


@dataclass(frozen=True)
class Baseline:
    """A pinned baseline file: a percentage tolerance plus metric values."""

    tolerance: float = 0
    metrics: list[NamedCount] = field(default_factory=list)

    @staticmethod
    def from_json(obj: dict) -> "Baseline":
        """Build from a decoded baseline JSON object."""
        return Baseline(
            tolerance=obj.get("tolerance", 0),
            metrics=[
                NamedCount(name, value)
                for name, value in obj.get("metrics", {}).items()
            ],
        )


@dataclass(frozen=True)
class BaselineRow:
    """One metric comparison against the baseline."""

    metric: str
    baseline: int
    actual: int
    delta: int
    within_tolerance: bool


def _parse_ts_ns(ts: str) -> int | None:
    """Parse an ISO8601 UTC timestamp into integer nanoseconds since epoch.

    Hand-rolled rather than `datetime.fromisoformat`: Python 3.10 does not
    accept a trailing `Z` (only 3.11+ does), and every `ts` in a real
    journal ends in `Z`. Epoch math uses `timedelta`'s integer day/second
    fields rather than `.timestamp()` to avoid float rounding at
    nanosecond precision.
    """
    match = TS_RE.match(ts)
    if not match:
        return None
    year, month, day, hour, minute, second, frac, tz = match.groups()
    try:
        dt = datetime.datetime(
            int(year),
            int(month),
            int(day),
            int(hour),
            int(minute),
            int(second),
            tzinfo=datetime.timezone.utc,
        )
    except ValueError:
        return None
    epoch = datetime.datetime(1970, 1, 1, tzinfo=datetime.timezone.utc)
    delta = dt - epoch
    base_ns = (delta.days * 86400 + delta.seconds) * 1_000_000_000
    frac_ns = int((frac + "0" * 9)[:9]) if frac else 0
    offset_ns = 0
    if tz and tz != "Z":
        sign = 1 if tz[0] == "+" else -1
        digits = tz[1:].replace(":", "")
        offset_hours = int(digits[0:2])
        offset_minutes = int(digits[2:4]) if len(digits) >= 4 else 0
        offset_ns = sign * (offset_hours * 3600 + offset_minutes * 60) * 1_000_000_000
    return base_ns + frac_ns - offset_ns


def _interval(record: Record) -> Interval | None:
    if record.ts_ns is None or record.latency_ns is None:
        return None
    end = record.ts_ns
    start = end - record.latency_ns
    return Interval(start, end)


def _get_flag(args: list[str], flag: str) -> str | None:
    """Return the value of `flag value` or `flag=value`, first match."""
    eq_prefix = flag + "="
    for index, arg in enumerate(args):
        if arg == flag:
            if index + 1 < len(args):
                return args[index + 1]
            return None
        if arg.startswith(eq_prefix):
            return arg[len(eq_prefix) :]
    return None


def _is_native_compiler(compiler: str | None) -> bool:
    if not compiler:
        return False
    basename = compiler.replace("\\", "/").rsplit("/", 1)[-1]
    return basename in NATIVE_COMPILER_BASENAMES


def _derive_crate_name_and_type(  # noqa: C901
    obj: dict, compiler: str | None, args: list[str]
) -> CrateIdentity:
    raw_name = obj.get("crate_name")
    crate_name = raw_name if isinstance(raw_name, str) and raw_name else None
    if crate_name is None:
        crate_name = _get_flag(args, "--crate-name")
    native = _is_native_compiler(compiler)
    if crate_name is None and native:
        for arg in args:
            if arg.startswith("-"):
                continue
            if arg.endswith(NATIVE_SRC_EXTS):
                crate_name = PurePosixPath(arg.replace("\\", "/")).stem
                break
    if crate_name is None:
        crate_name = "(unknown)"

    raw_type = obj.get("crate_type")
    crate_type = raw_type if isinstance(raw_type, str) and raw_type else None
    if crate_type is None:
        if crate_name == "build_script_build":
            crate_type = "build-script"
        else:
            flag_type = _get_flag(args, "--crate-type")
            if flag_type:
                crate_type = flag_type.split(",", 1)[0]
            elif native:
                crate_type = "native"
            else:
                crate_type = ""
    return CrateIdentity(crate_name, crate_type)


def _derive_out_dir(args: list[str]) -> str:
    out_dir = _get_flag(args, "--out-dir")
    if out_dir:
        return out_dir
    o_value = _get_flag(args, "-o")
    if o_value:
        return str(PurePosixPath(o_value.replace("\\", "/")).parent)
    return ""


def classify_tree(out_dir: str) -> str:
    """Bucket an `out_dir` (or `-o` parent) into a target-tree family.

    Normalizes slashes and appends a trailing `/` before matching so a path
    ending exactly at `.../target/debug` (no trailing slash) still matches.
    First match wins, in the order below.
    """
    normalized = out_dir.replace("\\", "/")
    if not normalized.endswith("/"):
        normalized += "/"
    if "/target/dylint/libraries/" in normalized:
        return "dylint/libraries"
    if "/target/dylint/tests/" in normalized:
        return "dylint/tests"
    if "/target/dylint/target/" in normalized:
        return "dylint/target"
    triple = re.search(r"/target/([^/]+)/", normalized)
    if triple and triple.group(1).count("-") >= 2:
        return "stable"
    if "/target/debug/" in normalized or "/target/release/" in normalized:
        return "stable"
    return "other"


def record_from_json(obj: Any, source: str) -> Record | None:
    """Build a `Record` from one decoded journal JSON object.

    Returns None when `obj` is not a usable JournalEntry (not an object, or
    missing/empty `outcome`) -- the caller counts that as a malformed line.
    """
    if not isinstance(obj, dict):
        return None
    outcome_raw = obj.get("outcome")
    if not isinstance(outcome_raw, str) or not outcome_raw:
        return None

    ts_raw = obj.get("ts")
    ts = ts_raw if isinstance(ts_raw, str) else None
    ts_ns = _parse_ts_ns(ts) if ts else None

    latency_raw = obj.get("latency_ns")
    latency_ns = (
        latency_raw
        if isinstance(latency_raw, int) and not isinstance(latency_raw, bool)
        else None
    )

    miss_reason_raw = obj.get("miss_reason")
    miss_reason = miss_reason_raw if isinstance(miss_reason_raw, str) else None

    context_key_raw = obj.get("context_key")
    context_key = (
        context_key_raw
        if isinstance(context_key_raw, str) and context_key_raw
        else None
    )

    generation_raw = obj.get("daemon_generation")
    daemon_generation = generation_raw if isinstance(generation_raw, str) else None

    compiler_raw = obj.get("compiler")
    compiler = compiler_raw if isinstance(compiler_raw, str) else None

    args_raw = obj.get("args")
    args = (
        [item for item in args_raw if isinstance(item, str)]
        if isinstance(args_raw, list)
        else []
    )

    cwd_raw = obj.get("cwd")
    cwd = cwd_raw if isinstance(cwd_raw, str) else None

    exit_code_raw = obj.get("exit_code")
    exit_code = (
        exit_code_raw
        if isinstance(exit_code_raw, int) and not isinstance(exit_code_raw, bool)
        else None
    )

    session_id_raw = obj.get("session_id")
    session_id = session_id_raw if isinstance(session_id_raw, str) else None

    identity = _derive_crate_name_and_type(obj, compiler, args)
    crate_name, crate_type = identity.name, identity.crate_type
    test_harness = "--test" in args
    dylint_link = any("dylint-link" in arg for arg in args)
    out_dir = _derive_out_dir(args)
    tree = classify_tree(out_dir)

    return Record(
        source=source,
        ts=ts,
        ts_ns=ts_ns,
        outcome=outcome_raw,
        miss_reason=miss_reason,
        context_key=context_key,
        daemon_generation=daemon_generation,
        latency_ns=latency_ns,
        compiler=compiler,
        args=args,
        cwd=cwd,
        exit_code=exit_code,
        session_id=session_id,
        crate_name=crate_name,
        crate_type=crate_type,
        test_harness=test_harness,
        dylint_link=dylint_link,
        out_dir=out_dir,
        tree=tree,
    )


def discover_journal_files(paths: list[str | Path]) -> list[Path]:
    """Resolve journal inputs to a sorted, deduplicated list of files.

    A file argument is read as-is. A directory argument is walked (never
    following symlinked subdirectories) for any file whose basename starts
    with `compile_journal` and contains `.jsonl` -- this matches both the
    live `compile_journal.jsonl` and rotated
    `compile_journal.jsonl.<ISO8601-stamp>` siblings. Unreadable inputs are
    skipped, never fatal.
    """
    found: set[Path] = set()
    for raw in paths:
        path = Path(raw)
        try:
            if path.is_file():
                found.add(path)
            elif path.is_dir():
                found.update(_walk_for_journals(path))
        except OSError:
            continue
    return sorted(found)


def _walk_for_journals(root: Path) -> list[Path]:
    found: list[Path] = []
    stack = [root]
    while stack:
        current = stack.pop()
        try:
            entries = list(current.iterdir())
        except OSError:
            continue
        for entry in entries:
            try:
                if entry.is_dir() and not entry.is_symlink():
                    stack.append(entry)
                elif (
                    entry.is_file()
                    and entry.name.startswith("compile_journal")
                    and ".jsonl" in entry.name
                ):
                    found.append(entry)
            except OSError:
                continue
    return found


def _discover_cargo_logs(paths: list[str | Path]) -> list[Path]:
    found: set[Path] = set()
    for raw in paths:
        path = Path(raw)
        try:
            if path.is_file():
                found.add(path)
            elif path.is_dir():
                found.update(_walk_for_suffixed(path, (".log", ".txt")))
        except OSError:
            continue
    return sorted(found)


def _walk_for_suffixed(root: Path, suffixes: tuple[str, ...]) -> list[Path]:
    found: list[Path] = []
    stack = [root]
    while stack:
        current = stack.pop()
        try:
            entries = list(current.iterdir())
        except OSError:
            continue
        for entry in entries:
            try:
                if entry.is_dir() and not entry.is_symlink():
                    stack.append(entry)
                elif entry.is_file() and entry.suffix in suffixes:
                    found.append(entry)
            except OSError:
                continue
    return found


def workspace_crate_names(repo_root: Path) -> set[str]:
    """Crate names for every workspace member plus every `dylints/*` lint.

    Parsed with regexes rather than `tomllib` -- the project floor is
    Python 3.10 and `tomllib` is 3.11+. This mirrors the one Cargo.toml
    convention soldr relies on (`[package] name = "..."` following a
    workspace-relative `members = [...]` list); it is not a general TOML
    parser.
    """
    names: set[str] = set()
    root_toml = repo_root / "Cargo.toml"
    try:
        root_text = root_toml.read_text(encoding="utf-8")
    except OSError:
        root_text = ""
    for member in _parse_members(root_text):
        member_toml = repo_root / member / "Cargo.toml"
        try:
            member_text = member_toml.read_text(encoding="utf-8")
        except OSError:
            names.add(PurePosixPath(member.replace("\\", "/")).name)
            continue
        name = _parse_package_name(member_text)
        names.add(name or PurePosixPath(member.replace("\\", "/")).name)

    dylints_dir = repo_root / "dylints"
    try:
        children = sorted(dylints_dir.iterdir()) if dylints_dir.is_dir() else []
    except OSError:
        children = []
    for child in children:
        member_toml = child / "Cargo.toml"
        if not member_toml.is_file():
            continue
        try:
            member_text = member_toml.read_text(encoding="utf-8")
        except OSError:
            continue
        name = _parse_package_name(member_text)
        if name:
            names.add(name)
    return names


def _parse_members(text: str) -> list[str]:
    match = MEMBERS_RE.search(text)
    if not match:
        return []
    return QUOTED_RE.findall(match.group("body"))


def _parse_package_name(text: str) -> str | None:
    match = PACKAGE_SECTION_NAME_RE.search(text)
    if match:
        return match.group(1)
    match = ANY_NAME_RE.search(text)
    return match.group(1) if match else None


def _normalize_crate(name: str) -> str:
    return name.replace("-", "_")


def _classify_party(record: Record, workspace_names: set[str]) -> str:
    cwd = (record.cwd or "").replace("\\", "/")
    if cwd:
        if "/registry/src/" in cwd or "/git/checkouts/" in cwd:
            return "third"
        return "first"
    normalized_names = {_normalize_crate(name) for name in workspace_names}
    if _normalize_crate(record.crate_name) in normalized_names:
        return "first"
    return "unclassified"


def parse_cargo_logs(text: str) -> ParsedCargoLog:
    """Parse cargo/nextest build output for dirty-fingerprint and status lines.

    Returns a `ParsedCargoLog` with `dirty` and `status` lists. `dirty` entries carry
    crate, version, and a best-effort `reason` (see `_extract_dirty_
    reason`). `status` entries carry crate, version, and `verb` -- the
    caller folds `Checking` together with `Compiling` because soldr's
    `ci-test` stable-host pass is Clippy, whose status verb is `Checking`,
    not `Compiling`.
    """
    lines = [ANSI.sub("", line) for line in text.splitlines()]
    dirty: list[DirtyEntry] = []
    status: list[StatusEntry] = []
    for index, line in enumerate(lines):
        dirty_match = DIRTY_RE.search(line)
        if dirty_match:
            dirty.append(
                DirtyEntry(
                    crate=dirty_match.group("name"),
                    version=dirty_match.group("version"),
                    reason=_extract_dirty_reason(line, lines, index, dirty_match.end()),
                )
            )
            continue
        status_match = STATUS_RE.match(line)
        if status_match:
            status.append(
                StatusEntry(
                    verb=status_match.group("verb"),
                    crate=status_match.group("crate"),
                    version=status_match.group("version"),
                )
            )
    return ParsedCargoLog(dirty=dirty, status=status)


def _extract_dirty_reason(
    header: str, lines: list[str], index: int, header_end: int
) -> str:
    detail = header[header_end:]
    candidates = [detail]
    for offset in (1, 2):
        if index + offset < len(lines):
            candidates.append(lines[index + offset])
    for candidate in candidates:
        label = REASON_LABEL_RE.search(candidate)
        if not label:
            continue
        ident = LEADING_IDENT_RE.match(label.group("payload").strip())
        if ident:
            return ident.group(1)
    for candidate in candidates:
        camel = CAMEL_RE.search(candidate)
        if camel:
            return camel.group(0)
    return "unknown"


def _parse_lockfile(text: str) -> set[str]:
    return set(LOCKFILE_NAME_RE.findall(text))


def _classify_duplicate(record: Record, others: list[Record]) -> str:
    """cross_generation > concurrent > sequential, first match wins."""
    if (
        others
        and record.daemon_generation is not None
        and all(other.daemon_generation != record.daemon_generation for other in others)
    ):
        return "cross_generation"
    interval = _interval(record)
    if interval is not None:
        same_generation = [
            other
            for other in others
            if other.daemon_generation == record.daemon_generation
        ]
        for other in same_generation:
            other_interval = _interval(other)
            if other_interval is None:
                continue
            if (
                interval.start < other_interval.end
                and other_interval.start < interval.end
            ):
                return "concurrent"
    return "sequential"


def _compute_duplicates(records: list[Record]) -> DuplicatesStats:
    groups: dict[str, list[Record]] = {}
    without_key = 0
    for record in records:
        if record.context_key:
            groups.setdefault(record.context_key, []).append(record)
        else:
            without_key += 1

    dup_groups = 0
    records_in_groups = 0
    excess = 0
    concurrent = 0
    sequential = 0
    cross_generation = 0
    identities: list[DuplicateIdentity] = []

    for key, members in groups.items():
        if len(members) <= 1:
            continue
        dup_groups += 1
        records_in_groups += len(members)
        excess += len(members) - 1
        identities.append(DuplicateIdentity(key, members[0].crate_name, len(members)))
        for record in members:
            others = [member for member in members if member is not record]
            flavour = _classify_duplicate(record, others)
            if flavour == "cross_generation":
                cross_generation += 1
            elif flavour == "concurrent":
                concurrent += 1
            else:
                sequential += 1

    identities.sort(key=lambda item: (-item.count, item.context_key))
    return DuplicatesStats(
        groups=dup_groups,
        records_in_groups=records_in_groups,
        excess_records=excess,
        concurrent=concurrent,
        sequential=sequential,
        cross_generation=cross_generation,
        records_without_context_key=without_key,
        top_identities=identities[:10],
    )


def _compute_cost(records: list[Record]) -> CostStats:
    """Cost of third-party work, on the STRICT `outcome == "miss"` definition.

    `third_party_miss_records` is the count that matches the metric
    definition in the module docstring, and it is the number
    `check_third_party_compiles.py` ratchets on. It is deliberately not the
    same as `third_party.misses`, which is the broad `miss | link_miss`
    grouping the four-bucket join uses; both are emitted so a later phase
    never has to guess which one a threshold was written against.
    """
    misses = [r for r in records if r.party == "third" and r.outcome == "miss"]
    total_wall = sum((r.latency_ns or 0) for r in misses) / 1e9

    grouped: dict[CostKey, list[Record]] = {}
    for record in misses:
        key = CostKey(record.crate_name, record.context_key)
        grouped.setdefault(key, []).append(record)

    items: list[CostItem] = []
    for group_key, members in grouped.items():
        wall_seconds = sum((member.latency_ns or 0) for member in members) / 1e9
        items.append(
            CostItem(
                crate=group_key.crate,
                context_key=group_key.context_key,
                count=len(members),
                wall_seconds=wall_seconds,
            )
        )
    items.sort(
        key=lambda item: (-item.wall_seconds, item.crate, item.context_key or "")
    )

    return CostStats(
        third_party_miss_records=len(misses),
        third_party_miss_wall_seconds=total_wall,
        third_party_miss_cpu_seconds=None,
        top_third_party=items[:10],
    )


def _compute_uncacheable(records: list[Record]) -> UncacheableStats:
    matches = [r for r in records if r.miss_reason == "uncacheable_input"]
    counts: dict[UncacheableKey, int] = {}
    order: list[UncacheableKey] = []
    for record in matches:
        key = UncacheableKey(
            record.crate_name,
            record.crate_type,
            record.dylint_link,
            record.test_harness,
            record.party == "first",
        )
        if key not in counts:
            counts[key] = 0
            order.append(key)
        counts[key] += 1
    order.sort(key=lambda key: (-counts[key], key.crate))
    buckets = [
        UncacheableBucket(
            crate=key.crate,
            crate_type=key.crate_type,
            dylint_link=key.dylint_link,
            test_harness=key.test_harness,
            first_party=key.first_party,
            count=counts[key],
        )
        for key in order
    ]
    return UncacheableStats(total=len(matches), buckets=buckets)


def _compute_cargo_summary(
    parsed_logs: list[ParsedCargoLog],
    workspace_names: set[str],
    available: bool,
) -> CargoSummary:
    ws_norm = {_normalize_crate(name) for name in workspace_names}
    dirty_reasons: Counter[str] = Counter()
    per_crate: dict[str, CrateCargoCounts] = {}
    dirty_units = 0
    third_party_dirty_units = 0
    for parsed in parsed_logs:
        for dirty_entry in parsed.dirty:
            dirty_units += 1
            crate = dirty_entry.crate
            reason = dirty_entry.reason
            dirty_reasons[reason] += 1
            bucket = per_crate.setdefault(crate, CrateCargoCounts(crate))
            bucket.dirty += 1
            bucket.reasons.append(reason)
            if _normalize_crate(crate) not in ws_norm:
                third_party_dirty_units += 1
        for status_entry in parsed.status:
            crate = status_entry.crate
            verb = status_entry.verb
            bucket = per_crate.setdefault(crate, CrateCargoCounts(crate))
            if verb in ("Compiling", "Checking"):
                bucket.compiling += 1
            elif verb == "Fresh":
                bucket.fresh += 1
    for bucket in per_crate.values():
        bucket.reasons = sorted(bucket.reasons)
    return CargoSummary(
        available=available,
        dirty_units=dirty_units,
        third_party_dirty_units=third_party_dirty_units,
        dirty_reasons=_named_counts(dirty_reasons),
        per_crate=[per_crate[name] for name in sorted(per_crate)],
    )


def _compute_buckets(
    records: list[Record],
    parsed_logs: list[ParsedCargoLog],
    lockfile_names: set[str],
    workspace_names: set[str],
) -> BucketStats:
    third_party = [r for r in records if r.party == "third"]
    third_party_total = len(third_party)
    compiling_hit = sum(1 for r in third_party if r.outcome in ("hit", "link_hit"))
    compiling_miss = sum(1 for r in third_party if r.outcome in ("miss", "link_miss"))
    compiling_other = third_party_total - compiling_hit - compiling_miss

    fresh: int | None = None
    compiling_no_record: int | None = None
    if lockfile_names:
        ws_norm = {_normalize_crate(name) for name in workspace_names}
        lock_norm = {_normalize_crate(name) for name in lockfile_names} - ws_norm
        journal_names = {_normalize_crate(r.crate_name) for r in records}
        cargo_names: set[str] = set()
        for parsed in parsed_logs:
            cargo_names.update(
                _normalize_crate(e.crate)
                for e in parsed.status
                if e.verb in ("Compiling", "Checking")
            )
            cargo_names.update(_normalize_crate(e.crate) for e in parsed.dirty)
        fresh = sum(
            1
            for name in lock_norm
            if name not in journal_names and name not in cargo_names
        )
        compiling_no_record = sum(
            1 for name in lock_norm if name in cargo_names and name not in journal_names
        )

    return BucketStats(
        third_party_total=third_party_total,
        fresh=fresh,
        compiling_hit=compiling_hit,
        compiling_miss=compiling_miss,
        compiling_other=compiling_other,
        compiling_no_record=compiling_no_record,
    )


def _party_stats(records: list[Record], party: str) -> PartyStats:
    subset = [r for r in records if r.party == party]
    hits = sum(1 for r in subset if r.outcome in ("hit", "link_hit"))
    misses = sum(1 for r in subset if r.outcome in ("miss", "link_miss"))
    crates = sorted({r.crate_name for r in subset})
    return PartyStats(records=len(subset), hits=hits, misses=misses, crates=crates)


def _default_repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def repo_lockfiles(repo_root: Path | None = None) -> list[Path]:
    """Every `Cargo.lock` that `--lockfiles-from-repo` means.

    The workspace root lockfile plus each `dylints/*/Cargo.lock`: the lint
    libraries are separate cargo workspaces resolved against a different
    (nightly) toolchain, so the third-party crates that appear only there
    are exactly the ones the `dylint/*` target trees pay for. Missing files
    are skipped rather than reported, because both callers treat lockfiles
    as an optional refinement of the four-bucket join.

    Public, and shared with `check_third_party_compiles.py`, so the two
    entry points resolve the same flag through one implementation --
    CLAUDE.md's soldr#2945 rule: when a behaviour has more than one entry
    point, either they resolve their inputs through one implementation or
    they drift.
    """
    resolved = Path(repo_root) if repo_root is not None else _default_repo_root()
    found: list[Path] = []
    root_lock = resolved / "Cargo.lock"
    if root_lock.is_file():
        found.append(root_lock)
    dylints_dir = resolved / "dylints"
    try:
        children = sorted(dylints_dir.iterdir()) if dylints_dir.is_dir() else []
    except OSError:
        children = []
    for child in children:
        candidate = child / "Cargo.lock"
        if candidate.is_file():
            found.append(candidate)
    return found


def _read_journals(journal_files: list[Path], dedupe: bool) -> JournalRead:
    records: list[Record] = []
    malformed = 0
    dropped = 0
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
                    dropped += 1
                    continue
                seen.add(line)
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                malformed += 1
                continue
            record = record_from_json(obj, str(path))
            if record is None:
                malformed += 1
                continue
            records.append(record)
    return JournalRead(records, malformed, dropped)


def _read_cargo_logs(paths: list[Path]) -> list[ParsedCargoLog]:
    parsed: list[ParsedCargoLog] = []
    for path in paths:
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        parsed.append(parse_cargo_logs(text))
    return parsed


def _read_lockfiles(paths: list[Path]) -> set[str]:
    names: set[str] = set()
    for path in paths:
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        names |= _parse_lockfile(text)
    return names


def _build_summary(
    *,
    journal_files: list[Path],
    records: list[Record],
    malformed_lines: int,
    duplicate_lines_dropped: int,
    cargo_log_files: list[Path],
    parsed_logs: list[ParsedCargoLog],
    lockfile_paths: list[Path],
    lockfile_names: set[str],
    workspace_names: set[str],
) -> Summary:
    outcomes: Counter[str] = Counter(r.outcome for r in records if r.outcome)
    miss_reasons: Counter[str] = Counter(
        r.miss_reason for r in records if r.miss_reason
    )
    trees: Counter[str] = Counter(r.tree for r in records)

    first_party_stats = _party_stats(records, "first")
    third_base = _party_stats(records, "third")
    unclassified_count = sum(1 for r in records if r.party == "unclassified")

    cost = _compute_cost(records)
    third_party_stats = ThirdPartyStats(
        **asdict(third_base),
        miss_wall_seconds=cost.third_party_miss_wall_seconds,
        miss_cpu_seconds=None,
    )

    uncacheable = _compute_uncacheable(records)
    duplicates = _compute_duplicates(records)
    cargo_summary = _compute_cargo_summary(
        parsed_logs, workspace_names, bool(cargo_log_files)
    )
    buckets = _compute_buckets(records, parsed_logs, lockfile_names, workspace_names)

    return Summary(
        schema_version=SCHEMA_VERSION,
        inputs=InputsInfo(
            journal_files=sorted(str(p) for p in journal_files),
            records_read=len(records),
            malformed_lines=malformed_lines,
            duplicate_lines_dropped=duplicate_lines_dropped,
            cargo_log_files=sorted(str(p) for p in cargo_log_files),
            lockfiles=sorted(str(p) for p in lockfile_paths),
        ),
        totals=Totals(
            records=len(records),
            first_party=first_party_stats.records,
            third_party=third_party_stats.records,
            unclassified=unclassified_count,
        ),
        outcomes=_named_counts(outcomes),
        miss_reasons=_named_counts(miss_reasons),
        first_party=first_party_stats,
        third_party=third_party_stats,
        uncacheable_input=uncacheable,
        duplicates=duplicates,
        trees=[
            NamedCount(name, trees.get(name, 0))
            for name in (
                "stable",
                "dylint/target",
                "dylint/tests",
                "dylint/libraries",
                "other",
            )
        ],
        cost=cost,
        cargo=cargo_summary,
        buckets=buckets,
    )


def analyze(
    journal_paths: list[str | Path],
    cargo_log_paths: list[str | Path] | None = None,
    repo_root: Path | None = None,
    lockfiles: list[str | Path] | None = None,
    dedupe: bool = True,
) -> dict:
    """`analyze_summary(...)` as the JSON-ready payload (`Summary.to_dict()`).

    The dict is the on-disk / cross-script JSON boundary; in-process callers
    should prefer the typed `analyze_summary()`.
    """
    return analyze_summary(
        journal_paths, cargo_log_paths, repo_root, lockfiles, dedupe
    ).to_dict()


def analyze_summary(
    journal_paths: list[str | Path],
    cargo_log_paths: list[str | Path] | None = None,
    repo_root: Path | None = None,
    lockfiles: list[str | Path] | None = None,
    dedupe: bool = True,
) -> Summary:
    """Read journals (+ optional cargo logs / lockfiles) and summarize them.

    See the module docstring for the schema this returns and the
    derivations it applies. `repo_root` defaults to this checkout's root
    (three parents up from `.github/scripts/`).
    """
    resolved_root = Path(repo_root) if repo_root is not None else _default_repo_root()
    cargo_log_paths = list(cargo_log_paths or [])
    lockfile_paths = [Path(p) for p in (lockfiles or [])]

    journal_files = discover_journal_files(journal_paths)
    journal = _read_journals(journal_files, dedupe)
    records = journal.records
    malformed_lines = journal.malformed
    duplicate_lines_dropped = journal.dropped

    workspace_names = workspace_crate_names(resolved_root)
    for record in records:
        record.party = _classify_party(record, workspace_names)

    cargo_log_files = _discover_cargo_logs(cargo_log_paths)
    parsed_logs = _read_cargo_logs(cargo_log_files)

    lockfile_names = _read_lockfiles(lockfile_paths)

    return _build_summary(
        journal_files=journal_files,
        records=records,
        malformed_lines=malformed_lines,
        duplicate_lines_dropped=duplicate_lines_dropped,
        cargo_log_files=cargo_log_files,
        parsed_logs=parsed_logs,
        lockfile_paths=lockfile_paths,
        lockfile_names=lockfile_names,
        workspace_names=workspace_names,
    )


def _section(lines: list[str], title: str) -> None:
    lines.append("")
    lines.append(f"-- {title} --")


def render_text(summary: Summary, top: int = 10, verbose: bool = False) -> str:  # noqa: C901
    """Render `summary` (the `Summary` `analyze_summary()` returns) as a console table."""
    lines: list[str] = []
    inputs = summary.inputs
    lines.append("== Compile journal analysis ==")
    lines.append(
        f"journals: {len(inputs.journal_files)} files, "
        f"{inputs.records_read} records read, "
        f"{inputs.malformed_lines} malformed, "
        f"{inputs.duplicate_lines_dropped} duplicate lines dropped"
    )
    if inputs.cargo_log_files:
        lines.append(f"cargo logs: {len(inputs.cargo_log_files)} files")
    if inputs.lockfiles:
        lines.append(f"lockfiles: {len(inputs.lockfiles)} files")

    totals = summary.totals
    _section(lines, "totals")
    lines.append(f"records: {totals.records}")
    lines.append(f"first-party: {totals.first_party}")
    lines.append(f"third-party: {totals.third_party}")
    lines.append(f"unclassified: {totals.unclassified}")

    _section(lines, "outcomes")
    for entry in summary.outcomes:
        lines.append(f"  {entry.name}: {entry.count}")

    _section(lines, "miss reasons")
    for entry in summary.miss_reasons:
        lines.append(f"  {entry.name}: {entry.count}")

    uncacheable = summary.uncacheable_input
    _section(lines, "uncacheable_input")
    lines.append(
        "  baseline (run 33536940076): 120 uncacheable_input records; the "
        "per-crate breakdown is in zackees/soldr#3039. Deliberately not a "
        "hand-written sum of buckets here -- the buckets below ARE the "
        "breakdown, and a second copy in prose can only drift from them."
    )
    lines.append(f"  total: {uncacheable.total}")
    for bucket in uncacheable.buckets[:top]:
        lines.append(
            f"  {bucket.count:>5}  {bucket.crate} "
            f"(type={bucket.crate_type!r} dylint_link={bucket.dylint_link} "
            f"test_harness={bucket.test_harness} first_party={bucket.first_party})"
        )

    duplicates = summary.duplicates
    _section(lines, "duplicates")
    lines.append(
        f"  groups: {duplicates.groups}  records_in_groups: "
        f"{duplicates.records_in_groups}  excess: {duplicates.excess_records}"
    )
    lines.append(
        f"  concurrent: {duplicates.concurrent}  sequential: "
        f"{duplicates.sequential}  cross_generation: {duplicates.cross_generation}"
    )
    lines.append(
        f"  records_without_context_key: {duplicates.records_without_context_key}"
    )
    for identity in duplicates.top_identities[:top]:
        lines.append(
            f"    {identity.count:>5}  {identity.crate}  {identity.context_key}"
        )

    trees = summary.trees
    _section(lines, "trees")
    for entry in trees:
        lines.append(f"  {entry.name}: {entry.count}")

    cost = summary.cost
    _section(lines, "cost")
    lines.append(
        f"  third-party miss records (strict outcome==miss): "
        f"{cost.third_party_miss_records}"
    )
    lines.append(
        f"  third-party miss wall seconds: {cost.third_party_miss_wall_seconds:.3f}"
    )
    lines.append(
        "  third-party miss cpu seconds: unavailable (the zccache journal "
        "carries no CPU-time field)"
    )
    for item in cost.top_third_party[:top]:
        lines.append(
            f"    {item.wall_seconds:>10.3f}s  {item.crate}  "
            f"{item.context_key}  (x{item.count})"
        )

    cargo = summary.cargo
    if not cargo.available:
        lines.append("")
        lines.append("cargo logs: none supplied")
    else:
        _section(lines, "cargo")
        lines.append(
            f"  dirty units: {cargo.dirty_units}  third-party dirty: "
            f"{cargo.third_party_dirty_units}"
        )
        for entry in cargo.dirty_reasons:
            lines.append(f"    {entry.name}: {entry.count}")
        if verbose:
            for info in cargo.per_crate:
                lines.append(
                    f"    {info.name}: dirty={info.dirty} compiling={info.compiling} "
                    f"fresh={info.fresh}"
                )

    buckets = summary.buckets
    _section(lines, "four-bucket join (#3039)")
    fresh_display = "n/a" if buckets.fresh is None else buckets.fresh
    no_record_display = (
        "n/a" if buckets.compiling_no_record is None else buckets.compiling_no_record
    )
    lines.append(f"  third_party_total: {buckets.third_party_total}")
    lines.append(f"  fresh: {fresh_display}")
    lines.append(f"  compiling_hit: {buckets.compiling_hit}")
    lines.append(f"  compiling_miss: {buckets.compiling_miss}")
    lines.append(f"  compiling_other: {buckets.compiling_other}")
    lines.append(f"  compiling_no_record: {no_record_display}")
    bucket_sum = (
        buckets.compiling_hit + buckets.compiling_miss + buckets.compiling_other
    )
    lines.append(
        f"  sum(compiling_hit+compiling_miss+compiling_other) = {bucket_sum} "
        f"(third_party_total = {buckets.third_party_total})"
    )

    if verbose:
        _section(lines, "first-party crates")
        for crate in summary.first_party.crates:
            lines.append(f"  {crate}")
        _section(lines, "third-party crates")
        for crate in summary.third_party.crates:
            lines.append(f"  {crate}")

    return "\n".join(lines)


def compare_baseline(summary: Summary, baseline: Baseline) -> list[BaselineRow]:
    """Compare `summary` against a pinned baseline's `metrics` block.

    `tolerance` is a PERCENTAGE (see the module docstring): within-
    tolerance means `abs(delta) <= abs(baseline) * tolerance / 100`, except
    a zero baseline requires an exact match.
    """
    tolerance = baseline.tolerance
    payload = summary.to_dict()
    metrics = _counts_to_json(baseline.metrics)
    rows: list[BaselineRow] = []
    for name, path in BASELINE_METRIC_PATHS.items():
        if name not in metrics:
            continue
        expected = metrics[name]
        actual = payload.get(path.section, {}).get(path.key, 0)
        delta = actual - expected
        if expected == 0:
            within = delta == 0
        else:
            within = abs(delta) <= abs(expected) * (tolerance / 100.0)
        rows.append(
            BaselineRow(
                metric=name,
                baseline=expected,
                actual=actual,
                delta=delta,
                within_tolerance=within,
            )
        )
    return rows


def _print_baseline_table(rows: list[BaselineRow], tolerance: float) -> None:
    print("")
    print(f"-- baseline comparison (+/-{tolerance}%) --")
    print(f"{'metric':<32}{'baseline':>12}{'actual':>12}{'delta':>12}  within")
    for row in rows:
        print(
            f"{row.metric:<32}{row.baseline:>12}{row.actual:>12}"
            f"{row.delta:>12}  {row.within_tolerance}"
        )


def main(argv: list[str] | None = None) -> int:  # noqa: C901
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "paths", nargs="*", help="compile-journal files or directories to scan"
    )
    parser.add_argument(
        "--cargo-logs",
        action="append",
        default=[],
        help="cargo/nextest build-output file or directory (repeatable)",
    )
    parser.add_argument(
        "--lockfile",
        action="append",
        default=[],
        help="Cargo.lock file to join against (repeatable)",
    )
    parser.add_argument(
        "--lockfiles-from-repo",
        action="store_true",
        help="add <repo-root>/Cargo.lock and every dylints/*/Cargo.lock that exists",
    )
    parser.add_argument(
        "--repo-root",
        default=None,
        help="repository root (default: this script's repo)",
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
        "--verbose",
        action="store_true",
        help="print crate lists and per-tree/per-bucket detail",
    )
    parser.add_argument(
        "--no-dedupe",
        action="store_true",
        help="do not drop byte-identical journal lines across files",
    )
    parser.add_argument(
        "--top", type=int, default=10, help="how many entries to show per top-N list"
    )
    parser.add_argument(
        "--compare-baseline", default=None, help="baseline JSON file to compare against"
    )
    parser.add_argument(
        "--fail-on-baseline-delta",
        action="store_true",
        help="exit 1 when a compared metric is outside its tolerance",
    )
    args = parser.parse_args(argv)

    repo_root = Path(args.repo_root) if args.repo_root else None
    resolved_root = repo_root or _default_repo_root()

    lockfiles: list[str | Path] = list(args.lockfile)
    if args.lockfiles_from_repo:
        lockfiles.extend(repo_lockfiles(resolved_root))

    journal_files = discover_journal_files(args.paths)
    if not journal_files:
        notice = (
            f"analyze_compile_journal: no compile journals found under {args.paths!r}"
        )
        if args.json:
            print(notice, file=sys.stderr)
        else:
            print(notice)

    summary = analyze_summary(
        args.paths,
        cargo_log_paths=args.cargo_logs,
        repo_root=repo_root,
        lockfiles=lockfiles,
        dedupe=not args.no_dedupe,
    )

    if args.json_out:
        # Report-only: a missing parent directory (the ci-test step failed
        # before it created `logs/`, say) must not turn the analysis into a
        # traceback that hides the numbers already computed above.
        out_path = Path(args.json_out)
        try:
            out_path.parent.mkdir(parents=True, exist_ok=True)
            out_path.write_text(
                json.dumps(summary.to_dict(), indent=2) + "\n", encoding="utf-8"
            )
        except OSError as error:
            print(
                f"analyze_compile_journal: could not write --json-out {args.json_out}: "
                f"{error}",
                file=sys.stderr,
            )

    if args.json:
        print(json.dumps(summary.to_dict(), indent=2))
    else:
        print(render_text(summary, top=args.top, verbose=args.verbose))

    exit_code = 0
    if args.compare_baseline:
        try:
            baseline = json.loads(
                Path(args.compare_baseline).read_text(encoding="utf-8")
            )
        except (OSError, json.JSONDecodeError) as error:
            print(
                f"analyze_compile_journal: could not read baseline "
                f"{args.compare_baseline}: {error}",
                file=sys.stderr,
            )
            baseline = None
        if baseline is not None and not isinstance(baseline, dict):
            print(
                f"analyze_compile_journal: baseline {args.compare_baseline} is not a "
                "JSON object; skipping the comparison",
                file=sys.stderr,
            )
            baseline = None
        if baseline is not None:
            parsed_baseline = Baseline.from_json(baseline)
            rows = compare_baseline(summary, parsed_baseline)
            _print_baseline_table(rows, parsed_baseline.tolerance)
            if args.fail_on_baseline_delta and not all(
                row.within_tolerance for row in rows
            ):
                exit_code = 1
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
