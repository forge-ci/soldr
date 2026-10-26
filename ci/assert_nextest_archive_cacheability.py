#!/usr/bin/env python3
"""Assert that DEPENDENCY compilation stays warm across a nextest archive build.

## What changed, and why (soldr#1391 -> soldr#2931 / soldr#2937)

soldr#1391 built this harness around the invariant "the full linked nextest
archive must be warm-cacheable: positive hits and **zero** misses". soldr#2931
inverted that policy. Cache admission follows the stability of an artifact's
identity key relative to its size, and by that measure a **linked test product
is never cacheable**: a test binary is one of the largest things the build
produces and its key moves with every source edit in the workspace. Requiring
it to be a cache hit was requiring the store to carry exactly what the policy
now forbids -- and it made the lane red for weeks over units that were never
supposed to be reused.

The valuable half of the old check survives unchanged: **dependency
compilation must still be warm**. That is the property a compilation cache
exists to deliver, it is Tier 1 (`cook`) plus Tier 2 (`zccache-unit`) in the
soldr#2931 tiering, and it regresses silently.

So the assertion is now:

* the warm run must reuse the compiler cache at all (`warm_hits > 0`); and
* **no dependency unit may miss** on the warm run.

First-party units -- everything named `soldr*`, which is where every
test-harness LINK product lives -- are reported with their miss counts and are
never a failure. They are not expected or required to be cache hits.

## Why it is still a Docker harness

The source tree is bind-mounted, but Cargo's target dir, CARGO_HOME, and the
soldr home live on Linux Docker volumes so Cargo mtimes and zccache state are
not distorted by Windows bind-mount behavior.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from collections import deque
from dataclasses import asdict, dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
IMAGE = "soldr-cook-dev"
DOCKERFILE = REPO_ROOT / "docker" / "cook-shared-cache" / "Dockerfile"

BASH_SCRIPT = r"""
set -euo pipefail

export CARGO_HOME=/root/.cargo
export CARGO_TERM_COLOR=always
export RUST_BACKTRACE=1
export SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS="${SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS:-120000}"

report_memory_pressure() {
  # soldr#2781/#2817: this lane keeps dying to "compiler process was
  # terminated by a Unix signal ... can indicate an OOM/resource-limit kill",
  # and every triage has stopped at "can indicate". The container's own cgroup
  # counts OOM kills, so the question is answerable right here -- in both
  # directions, which is the point: a zero rules memory out rather than
  # merely failing to confirm it.
  python3 /work/ci/assert_nextest_archive_cacheability.py \
    --memory-pressure "$1" >&2 || true
}

echo "## environment"
rustc --version
cargo --version
report_memory_pressure startup

echo "## bootstrap soldr"
export CARGO_TARGET_DIR=/root/.soldr/bootstrap-target
cargo build -p soldr-cli --bin soldr
SOLDR_BIN=/root/.soldr/bootstrap-target/debug/soldr
"$SOLDR_BIN" --version

DIAGNOSTICS_DIR=/tmp/soldr-cacheability
CACHE="$DIAGNOSTICS_DIR/root"
ARCHIVE_DIR="$DIAGNOSTICS_DIR/archives"
export SOLDR_CACHE_DIR="$CACHE"
rm -rf "$CACHE" "$ARCHIVE_DIR" /tmp/cold-report.json /tmp/warm-report.json
mkdir -p "$CACHE" "$ARCHIVE_DIR"

export CARGO_TARGET_DIR=/work/target

collect_warm_miss_units() {
  # soldr#2937: the pass/fail decision moved out of this shell and into
  # `evaluate_warm_result` in the Python driver, because it now depends on
  # WHICH units missed rather than on how many. A dependency miss is a
  # regression; a first-party miss is a test-harness link product and is
  # expected. Emit the distinct unit names as JSON and let the driver classify
  # them -- that logic is unit-tested, and a 40-minute Docker run is the worst
  # possible place to discover a classification bug.
  if [[ ! -f /tmp/warm-build.log ]]; then
    echo '[]'
    return 0
  fi
  grep -oE 'soldr\[cache\] [A-Za-z0-9_]+ .*MISS' /tmp/warm-build.log \
    | sed -E 's/soldr\[cache\] ([A-Za-z0-9_]+) .*/\1/' \
    | sort -u \
    | jq -Rcs 'split("\n") | map(select(length > 0))'
}

report_warm_misses() {
  # soldr#2824: "107 misses" is a number, not a diagnosis. The build log names
  # every unit it missed, so print the distinct set -- that is what turns
  # "cacheability regressed" into "these units regressed".
  if [[ ! -f /tmp/warm-build.log ]]; then
    echo "## warm miss detail unavailable: /tmp/warm-build.log missing" >&2
    return 0
  fi
  # NOT sort -u: the counts below need the duplicates. Collapsing here made
  # `uniq -c` report 1 for every unit -- a "most-missed" list that was all
  # ones. The distinct count is taken separately, just below.
  local units
  units="$(grep -oE 'soldr\[cache\] [A-Za-z0-9_]+ .*MISS' /tmp/warm-build.log \
    | sed -E 's/soldr\[cache\] ([A-Za-z0-9_]+) .*/\1/' | sort)"
  local count
  count="$(printf '%s\n' "$units" | sort -u | grep -c . || true)"
  echo "## warm-run misses by unit ($count distinct, most-missed first)" >&2
  # Counts, not just names: a unit that misses once is a keying question, and
  # one that misses repeatedly in a single run is a different question.
  printf '%s\n' "$units" | sort | uniq -c | sort -rn | sed 's/^/  /' >&2
}

explain_report() {
  # soldr#2824: the miss *list* was added in soldr#2825 and closed with the
  # line "the per-unit reason is in the compile journal named above". No
  # journal was named anywhere in this harness, so that sentence pointed at
  # nothing and the group under it printed only its own header.
  #
  # The reason was never missing -- it was being discarded. `soldr cache
  # report --json` already carries the journal's path, zccache's own analysis
  # of it, the staged counters (plan_unsupported, publication_failure,
  # publication_conflict, salvage_failure, materialize_failure ...) and any
  # diagnoses. This harness captured the whole report and read four integers
  # out of it.
  #
  # The cold report matters as much as the warm one: publication happens on
  # the cold run, so a unit that missed warm most often failed to *publish*
  # cold, and only the cold report can say that.
  local label="$1"
  local report="$2"
  python3 /work/ci/assert_nextest_archive_cacheability.py \
    --explain-report "$label" "$report" >&2 || true
}

print_daemon_diagnostics() {
  # The sentinel PhaseTracker watches for. Everything below announces itself
  # with a `##` marker too, so without it the last marker before the stream
  # ends is a diagnostic section rather than the phase that actually failed.
  echo "## post-failure diagnostics" >&2
  report_memory_pressure failure
  echo "## soldr daemon diagnostics" >&2
  cat "$DIAGNOSTICS_DIR/soldr-daemon-status.json" >&2 || true
  cat "$DIAGNOSTICS_DIR/soldr-daemon-status.err" >&2 || true
  if [ -f "$CACHE/daemon-spawn.log" ]; then
    echo "## daemon-spawn.log tail" >&2
    tail -n 200 "$CACHE/daemon-spawn.log" >&2 || true
  fi
  echo "## soldr processes" >&2
  ps -ef | grep -E '[s]oldr|[z]ccache' >&2 || true
  echo "## retained diagnostic files" >&2
  find "$DIAGNOSTICS_DIR" -maxdepth 4 -type f | wc -l | \
    xargs printf 'file count: %s\n' >&2 || true
  du -sh "$DIAGNOSTICS_DIR" >&2 || true
  find "$DIAGNOSTICS_DIR" -maxdepth 4 -type f -printf '%p %s bytes\n' | \
    sort | head -n 200 >&2 || true
}

on_exit() {
  status=$?
  trap - EXIT
  if [ "$status" -ne 0 ]; then
    set +e
    "$SOLDR_BIN" daemon status --json \
      > "$DIAGNOSTICS_DIR/soldr-daemon-status.json" \
      2> "$DIAGNOSTICS_DIR/soldr-daemon-status.err"
    print_daemon_diagnostics
  fi
  exit "$status"
}
trap on_exit EXIT

# Resolve the cargo-nextest front-door tool before starting the daemon.  The
# first-use fetch/bootstrap path can restart the managed process while Cargo
# is already compiling; that obscures the cacheability check with a daemon
# lifecycle failure.  Subsequent archive builds exercise only compilation and
# cache traffic. Install the failure trap first so bootstrap failures retain
# the same diagnostics as archive failures.
echo "## prefetch cargo-nextest"
"$SOLDR_BIN" cargo nextest --version

ensure_soldr_daemon() {
  echo "## ensure soldr daemon"
  "$SOLDR_BIN" daemon start || true
  for _ in $(seq 1 120); do
    "$SOLDR_BIN" daemon status --json \
      > "$DIAGNOSTICS_DIR/soldr-daemon-status.json" \
      2> "$DIAGNOSTICS_DIR/soldr-daemon-status.err" || true
    if jq -e '.running == true' "$DIAGNOSTICS_DIR/soldr-daemon-status.json" > /dev/null 2>&1; then
      cat "$DIAGNOSTICS_DIR/soldr-daemon-status.json"
      return 0
    fi
    sleep 1
  done
  echo "soldr daemon did not report running" >&2
  print_daemon_diagnostics
  return 1
}

stop_soldr_daemon() {
  echo "## stop soldr daemon"
  "$SOLDR_BIN" daemon stop || true
  for _ in $(seq 1 60); do
    "$SOLDR_BIN" daemon status --json \
      > "$DIAGNOSTICS_DIR/soldr-daemon-status.json" \
      2> "$DIAGNOSTICS_DIR/soldr-daemon-status.err" || true
    if jq -e '.running == false' "$DIAGNOSTICS_DIR/soldr-daemon-status.json" > /dev/null 2>&1; then
      cat "$DIAGNOSTICS_DIR/soldr-daemon-status.json"
      return 0
    fi
    sleep 1
  done
  echo "soldr daemon did not stop" >&2
  print_daemon_diagnostics
  return 1
}

clean_target() {
  # Cargo tries to remove the target-dir root. In this Docker harness that
  # root is a volume mount point, so Docker Desktop can report EBUSY after
  # Cargo has removed the contents. Keep the cleanup deterministic by
  # emptying the mount point without deleting the mount point itself.
  cargo clean || true
  find "$CARGO_TARGET_DIR" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
}

clean_target
ensure_soldr_daemon

echo "## cold nextest archive build"
cold_start=$(date +%s%3N)
CARGO_PROFILE_TEST_DEBUG=line-tables-only \
  "$SOLDR_BIN" cargo nextest archive --workspace \
  --cargo-profile ci-nextest \
  --archive-file "$ARCHIVE_DIR/cold-tests.tar.zst" \
  --archive-format tar-zst
cold_end=$(date +%s%3N)
"$SOLDR_BIN" cache flush --json
"$SOLDR_BIN" cache report --json > /tmp/cold-report.json
ls -lh "$ARCHIVE_DIR/cold-tests.tar.zst"
"$SOLDR_BIN" cache shutdown --no-wait --json || true
stop_soldr_daemon

echo "## warm nextest archive build after cargo clean and daemon restart"
clean_target
ensure_soldr_daemon
warm_start=$(date +%s%3N)
# Tee'd so a failure can say WHICH units missed. soldr#2824: this reported
# "warm run had misses; expected zero" and nothing else, so three weeks of red
# runs carried no evidence about what was uncacheable.
CARGO_PROFILE_TEST_DEBUG=line-tables-only \
  "$SOLDR_BIN" cargo nextest archive --workspace \
  --cargo-profile ci-nextest \
  --archive-file "$ARCHIVE_DIR/warm-tests.tar.zst" \
  --archive-format tar-zst 2>&1 | tee /tmp/warm-build.log
warm_end=$(date +%s%3N)
"$SOLDR_BIN" cache flush --json
"$SOLDR_BIN" cache report --json > /tmp/warm-report.json
ls -lh "$ARCHIVE_DIR/warm-tests.tar.zst"
"$SOLDR_BIN" cache shutdown --no-wait --json || true
stop_soldr_daemon

stat_json() {
  local report="$1"
  local key="$2"
  jq -r --arg k "$key" '.last_session.stats[$k] // .last_session[$k] // 0' "$report"
}

cold_hits="$(stat_json /tmp/cold-report.json hits)"
cold_misses="$(stat_json /tmp/cold-report.json misses)"
cold_non_cacheable="$(stat_json /tmp/cold-report.json non_cacheable)"
cold_hit_rate="$(stat_json /tmp/cold-report.json hit_rate)"
warm_hits="$(stat_json /tmp/warm-report.json hits)"
warm_misses="$(stat_json /tmp/warm-report.json misses)"
warm_non_cacheable="$(stat_json /tmp/warm-report.json non_cacheable)"
warm_hit_rate="$(stat_json /tmp/warm-report.json hit_rate)"

result="$(
  jq -cn \
    --argjson cold_hits "$cold_hits" \
    --argjson cold_misses "$cold_misses" \
    --argjson cold_non_cacheable "$cold_non_cacheable" \
    --argjson cold_hit_rate "$cold_hit_rate" \
    --argjson warm_hits "$warm_hits" \
    --argjson warm_misses "$warm_misses" \
    --argjson warm_non_cacheable "$warm_non_cacheable" \
    --argjson warm_hit_rate "$warm_hit_rate" \
    '{
      cold_hits: $cold_hits,
      cold_misses: $cold_misses,
      cold_non_cacheable: $cold_non_cacheable,
      cold_hit_rate: $cold_hit_rate,
      warm_hits: $warm_hits,
      warm_misses: $warm_misses,
      warm_non_cacheable: $warm_non_cacheable,
      warm_hit_rate: $warm_hit_rate
    }'
)"
echo "CACHEABILITY_RESULT $result"
echo "CACHEABILITY_WARM_MISSES $(collect_warm_miss_units)"
# soldr#3606: the first-party unit set comes from Cargo, not from a name rule.
# Every workspace target name (lib, bin, test, bench, example) as one JSON
# array; `null` when metadata is unavailable so the driver degrades instead of
# the harness aborting.
echo "CACHEABILITY_WORKSPACE_METADATA $("$SOLDR_BIN" cargo metadata --no-deps --format-version 1 --offline | jq -c '[.packages[].targets[].name]' || echo null)"

# soldr#2937: no verdict here. Whether a miss is a regression depends on
# whether the unit is a dependency or a first-party test-harness link product,
# and that classification lives in `evaluate_warm_result` where it is testable
# without paying 40 minutes to find out it was wrong. This shell's job is now
# to run the two builds and hand over the evidence.
#
# The evidence is printed whenever ANY unit missed, not only on failure: a run
# that passes with first-party misses is the normal, expected shape under the
# soldr#2931 policy, and the reader should be able to see which link products
# were rebuilt.
if (( warm_misses != 0 )) || (( warm_hits <= 0 )); then
  report_warm_misses
  explain_report cold /tmp/cold-report.json
  explain_report warm /tmp/warm-report.json
fi

echo "TIMING_MS cold=$((cold_end - cold_start)) warm=$((warm_end - warm_start))"
"""


def _counter_lines(report: object) -> list[str]:
    """Non-zero staged counters, largest first.

    These are the closest thing the pipeline has to a per-unit reason:
    `plan_unsupported` says a unit was never eligible, `publication_failure`
    and `publication_conflict` say it compiled but never became durable, and
    `materialize_failure` says it was found and could not be placed.

    Read defensively at every level. The `last_session` value is passed
    through verbatim from zccache and its shape moves across protocol
    versions -- `cache/report.rs` says so explicitly, and this is a diagnostic
    printed while something is already failing. It must degrade to a note
    rather than raise a second error on top of the first.
    """
    session = report.get("last_session") if isinstance(report, dict) else None
    if not isinstance(session, dict):
        return ["  (no last_session in the report)"]
    profile = session.get("phase_profile")
    staged = profile.get("staged") if isinstance(profile, dict) else None
    counters = staged.get("counters") if isinstance(staged, dict) else None
    if not isinstance(counters, dict):
        return ["  (no phase_profile.staged.counters -- the shape may have moved)"]
    nonzero = [
        (name, value)
        for name, value in counters.items()
        if isinstance(value, (int, float)) and value
    ]
    if not nonzero:
        return ["  (every counter is zero)"]
    nonzero.sort(key=lambda entry: (-entry[1], entry[0]))
    return [f"  {name} = {value}" for name, value in nonzero]


def _diagnosis_lines(report: object) -> list[str]:
    entries = report.get("diagnoses") if isinstance(report, dict) else None
    if not isinstance(entries, list) or not entries:
        return ["  (none)"]
    lines = []
    for entry in entries:
        if not isinstance(entry, dict):
            lines.append(f"  {entry}")
            continue
        severity = entry.get("severity", "?")
        kind = entry.get("kind", "?")
        message = entry.get("message", "")
        lines.append(f"  [{severity}] {kind}: {message}")
    return lines


def explain_report(label: str, report: object) -> list[str]:
    """The reasons a cacheability failure has, rendered for a CI log.

    soldr#2824/#2825: the harness printed which units missed and then said the
    reason was "in the compile journal named above" -- naming no journal, and
    printing nothing. Every field below was already in the report the harness
    captured; none of it was being read.
    """
    lines = [f"## {label} report evidence"]
    fields = report if isinstance(report, dict) else {}

    journal = fields.get("journal_path") or "(absent)"
    present = fields.get("journal_present")
    lines.append(f"  journal_path: {journal}")
    lines.append(f"  journal_present: {present}")

    lines.append(f"## {label} staged counters (non-zero)")
    lines.extend(_counter_lines(report))

    lines.append(f"## {label} diagnoses")
    lines.extend(_diagnosis_lines(report))

    notes = fields.get("notes")
    lines.append(f"## {label} notes")
    if isinstance(notes, list) and notes:
        lines.extend(f"  {note}" for note in notes)
    else:
        lines.append("  (none)")

    rollups = fields.get("rollups")
    lines.append(f"## {label} rollups (zccache analyze over that journal)")
    if rollups is None:
        # Distinguished from "absent" on purpose: a null rollups with a note
        # explaining why is a different failure from a missing key.
        lines.append("  null -- see notes above")
    else:
        rendered = json.dumps(rollups, indent=2, sort_keys=True, default=str)
        # Capped: this runs inside an already-failing job and an unbounded
        # dump of an evolving structure would bury the counters above it.
        capped = rendered.splitlines()[:40]
        lines.extend(f"  {line}" for line in capped)
        if len(rendered.splitlines()) > len(capped):
            lines.append("  ... (truncated)")

    return lines


def emit_report_explanation(label: str, path: str) -> int:
    """Load a `cache report --json` file and print what it says about misses."""
    try:
        with open(path, encoding="utf-8") as handle:
            report = json.load(handle)
    except (OSError, json.JSONDecodeError) as error:
        print(f"## {label} evidence unavailable: {path}: {error}")
        return 0
    if not isinstance(report, dict):
        print(f"## {label} evidence unavailable: {path} is not an object")
        return 0
    for line in explain_report(label, report):
        print(line)
    return 0


# --------------------------------------------------------------------------
# The soldr#2931 warm-run verdict (replaces the soldr#1391 zero-miss rule)
# --------------------------------------------------------------------------

# First-party units are read from `cargo metadata` (every workspace target
# name, see `first_party_units`), never from a name rule. A `soldr*` prefix
# silently drifted when soldr#2934 added category test targets (`broker`,
# `daemon`, `guards`, ...) that carry no such prefix, and those first-party
# link products were then reported as dependency misses (soldr#3606).

# `build_script_build` is the unit name cargo gives EVERY crate's build script,
# first-party and dependency alike, so the name does not identify its crate.
# Reported, never fatal: failing on an ambiguous name would make the verdict
# unattributable, which is the exact defect soldr#2824 spent three weeks on.
AMBIGUOUS_UNITS = frozenset({"build_script_build", "build_script_main"})


def normalize_unit(name: str) -> str:
    """zccache unit name -> comparable form (`soldr-cli` and `soldr_cli` agree)."""
    return name.strip().replace("-", "_").lower()


def first_party_units(target_names: "list[str]") -> "frozenset[str]":
    """Workspace cargo target names -> the unit names zccache reports for them."""
    return frozenset(
        unit for unit in (normalize_unit(str(name)) for name in target_names) if unit
    )


def classify_warm_misses(
    units: "list[str]", first_party: "frozenset[str]"
) -> "tuple[list[str], list[str]]":
    """Split warm-run miss units into `(dependency, expected)`.

    `dependency` is the half that must not exist: an external crate that
    recompiled on a warm run means dependency compilation stopped being warm,
    which is the property the compilation cache exists to deliver.

    `expected` is first-party plus ambiguous units. A first-party test harness
    is cached, but it re-links whenever anything it links changed, so a
    first-party unit missing is not by itself a defect.
    """
    dependency: list[str] = []
    expected: list[str] = []
    for raw in units:
        unit = normalize_unit(str(raw))
        if not unit:
            continue
        if unit in first_party or unit in AMBIGUOUS_UNITS:
            expected.append(unit)
        else:
            dependency.append(unit)
    return sorted(set(dependency)), sorted(set(expected))


@dataclass(frozen=True)
class WarmResult:
    """The ``CACHEABILITY_RESULT`` counters the harness emits as JSON."""

    cold_hits: int = 0
    cold_misses: int = 0
    cold_non_cacheable: int = 0
    cold_hit_rate: float = 0.0
    warm_hits: int = 0
    warm_misses: int = 0
    warm_non_cacheable: int = 0
    warm_hit_rate: float = 0.0

    @classmethod
    def from_json(cls, raw: object) -> "WarmResult":
        fields = raw if isinstance(raw, dict) else {}
        return cls(
            cold_hits=int(fields.get("cold_hits", 0)),
            cold_misses=int(fields.get("cold_misses", 0)),
            cold_non_cacheable=int(fields.get("cold_non_cacheable", 0)),
            cold_hit_rate=float(fields.get("cold_hit_rate", 0.0)),
            warm_hits=int(fields.get("warm_hits", 0)),
            warm_misses=int(fields.get("warm_misses", 0)),
            warm_non_cacheable=int(fields.get("warm_non_cacheable", 0)),
            warm_hit_rate=float(fields.get("warm_hit_rate", 0.0)),
        )


def evaluate_warm_result(
    result: WarmResult,
    warm_miss_units: "list[str] | None",
    first_party: "frozenset[str] | None",
) -> "list[str]":
    """Failure lines for a warm run. Empty means the lane passes.

    Two conditions, and only two:

    1. The warm run reused the compiler cache at all. Zero hits means nothing
       was warm, which no amount of tiering explains away.
    2. No *dependency* unit missed.

    Deliberately absent: any requirement on the linked test archive. That was
    the soldr#1391 invariant and soldr#2931 inverted it.

    `warm_miss_units` is `None` when the harness could not produce a per-unit
    list (a missing build log). The check then degrades to condition 1 rather
    than failing: an absent diagnostic is not evidence of a regression, and a
    guard that fails on its own missing input teaches people to ignore it.

    `first_party` is `None` when workspace metadata was unavailable (soldr#3606).
    Misses then cannot be attributed to first-party or dependency, so the check
    degrades exactly as it does for a missing `warm_miss_units`.
    """
    failures: list[str] = []

    if result.warm_hits <= 0:
        failures.append(
            "warm run reported zero compiler-cache hits: dependency "
            "compilation is not warm at all"
        )

    if warm_miss_units is None:
        return failures
    if first_party is None:
        print(
            "note: per-unit miss classification skipped because workspace "
            "metadata was unavailable (soldr#3606)"
        )
        return failures

    dependency, expected = classify_warm_misses(warm_miss_units, first_party)
    if dependency:
        failures.append(
            "dependency units recompiled on the warm run (they must hit the "
            f"compiler cache): {', '.join(dependency)}"
        )
    if expected and not failures:
        # Not a failure -- said out loud so a green run does not read as if
        # nothing missed. Under soldr#2931 these are the linked test products.
        print(
            "note: "
            f"{len(expected)} first-party unit(s) rebuilt warm, which the "
            "soldr#2931 policy expects (linked test products are never "
            f"cacheable): {', '.join(expected)}"
        )
    return failures


CGROUP_ROOT = Path("/sys/fs/cgroup")
MEMINFO = Path("/proc/meminfo")

# Env vars that decide how many of these oversized units run at once. Printed
# beside the limits because the pair is what a reader needs: a 7.8 GiB ceiling
# is fine at one job and marginal at two.
CONCURRENCY_VARS = ("SOLDR_JOBS", "CARGO_BUILD_JOBS", "ZCCACHE_MAX_PARALLEL_COMPILES")


def _read_text(path: Path) -> str | None:
    """Contents of ``path``, or None if it cannot be read.

    Every caller here is a diagnostic running while something else is already
    failing, so an unreadable file is a fact to report, never an exception to
    raise on top of the original failure.
    """
    try:
        return path.read_text(encoding="utf-8").strip()
    except OSError:
        return None


def _human_bytes(value: int) -> str:
    step = 1024.0
    size = float(value)
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if size < step or unit == "TiB":
            return f"{size:.1f} {unit}" if unit != "B" else f"{int(size)} B"
        size /= step
    return f"{size:.1f} TiB"


def _bytes_line(label: str, raw: str | None) -> str:
    if raw is None:
        return f"  {label}: (unreadable)"
    if raw == "max":
        # cgroup v2 spells "no limit" as the literal string, not a number.
        return f"  {label}: max (no cgroup limit)"
    try:
        value = int(raw)
    except ValueError:
        return f"  {label}: {raw}"
    return f"  {label}: {_human_bytes(value)} ({value})"


@dataclass(frozen=True)
class MemoryEvent:
    """One ``<key> <count>`` line of ``memory.events``."""

    name: str
    count: int


def event_count(events: tuple[MemoryEvent, ...], name: str) -> int:
    """The count recorded for ``name`` (the last such line), or 0."""
    matches = [event.count for event in events if event.name == name]
    return matches[-1] if matches else 0


def parse_memory_events(raw: str | None) -> tuple[MemoryEvent, ...]:
    """``memory.events`` as records. Unparsable lines are skipped.

    The file is ``<key> <count>`` per line. ``oom_kill`` is the one that
    settles the question this harness keeps asking: cgroup v2 counts every
    process in the cgroup killed by *any* OOM killer, so a zero there rules
    memory out rather than merely failing to confirm it.
    """
    events: list[MemoryEvent] = []
    if not raw:
        return ()
    for line in raw.splitlines():
        parts = line.split()
        if len(parts) != 2:
            continue
        try:
            events.append(MemoryEvent(parts[0], int(parts[1])))
        except ValueError:
            continue
    return tuple(events)


def _meminfo_line(label: str, raw: str | None) -> str:
    if raw is None:
        return f"  {label}: (unreadable)"
    for line in raw.splitlines():
        if line.startswith(f"{label}:"):
            return f"  {line.split(':', 1)[1].strip()}  <- {label}"
    return f"  {label}: (absent)"


def oom_verdict(events: tuple[MemoryEvent, ...], events_readable: bool) -> str:
    """The one line worth reading, stated as a determination.

    soldr#2781/#2817: soldr's own message says a signal kill "can indicate an
    OOM/resource-limit kill", and every triage of this lane has stopped there
    -- the owner's #2781 report notes `dmesg` had no OOM record, which is not
    the same question as whether the *cgroup* recorded one. This answers the
    question that is actually answerable inside the container.
    """
    if not events_readable:
        return (
            "  VERDICT: unknown -- memory.events is unreadable, so this run "
            "cannot say whether an OOM kill happened"
        )
    killed = event_count(events, "oom_kill") + event_count(events, "oom_group_kill")
    if killed:
        return (
            f"  VERDICT: the kernel OOM-killed {killed} process(es) in this "
            "cgroup -- a signal-killed compile here IS a memory kill"
        )
    return (
        "  VERDICT: no OOM kill recorded in this cgroup -- a signal-killed "
        "compile here is NOT the memory limit; look elsewhere"
    )


def memory_pressure_lines(
    label: str,
    cgroup_root: Path = CGROUP_ROOT,
    meminfo: Path = MEMINFO,
    environ: dict[str, str] | None = None,
) -> list[str]:
    """Host/container memory facts, plus whether the kernel OOM-killed here."""
    env = os.environ if environ is None else environ
    lines = [f"## {label} memory pressure"]

    memory_max = cgroup_root / "memory.max"
    if not memory_max.exists():
        lines.append(f"  cgroup: no memory.max under {cgroup_root} (not cgroup v2?)")
    else:
        lines.append(f"  cgroup: v2 at {cgroup_root}")
    lines.append(_bytes_line("memory.max", _read_text(memory_max)))
    lines.append(_bytes_line("memory.high", _read_text(cgroup_root / "memory.high")))
    # memory.peak is the high-water mark for the whole run, so it survives the
    # death of whatever process reached it -- which is the only reason this is
    # readable at all after the compile that spiked is gone.
    lines.append(_bytes_line("memory.peak", _read_text(cgroup_root / "memory.peak")))

    raw_events = _read_text(cgroup_root / "memory.events")
    events = parse_memory_events(raw_events)
    if events:
        latest = {event.name: event.count for event in events}
        rendered = " ".join(f"{k}={v}" for k, v in sorted(latest.items()))
    else:
        rendered = "(unreadable)" if raw_events is None else "(empty)"
    lines.append(f"  memory.events: {rendered}")

    meminfo_raw = _read_text(meminfo)
    lines.append(_meminfo_line("MemTotal", meminfo_raw))
    lines.append(_meminfo_line("MemAvailable", meminfo_raw))
    lines.append(_meminfo_line("SwapTotal", meminfo_raw))
    lines.append(f"  nproc: {os.cpu_count()}")
    for name in CONCURRENCY_VARS:
        lines.append(f"  {name}: {env.get(name, '(unset)')}")

    lines.append(oom_verdict(events, raw_events is not None))
    return lines


def emit_memory_pressure(label: str) -> int:
    for line in memory_pressure_lines(label):
        print(line)
    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Run a Docker/Linux cold-clean-warm nextest archive build and fail "
            "if any DEPENDENCY unit recompiled on the warm run. Linked "
            "test-harness products are not required to be cache hits "
            "(soldr#2931 inverted the soldr#1391 zero-miss invariant)."
        )
    )
    parser.add_argument(
        "--image",
        default=IMAGE,
        help=f"Docker image tag to build/use (default: {IMAGE})",
    )
    parser.add_argument(
        "--keep-volumes",
        action="store_true",
        help="keep the temporary Docker volumes for post-failure inspection",
    )
    parser.add_argument(
        "--suffix",
        default=None,
        help="override the Docker volume suffix; default is timestamp + pid",
    )
    parser.add_argument(
        "--explain-report",
        nargs=2,
        metavar=("LABEL", "PATH"),
        default=None,
        help=(
            "print why a `soldr cache report --json` shows misses, and exit. "
            "Used by the harness from inside the container; runs no Docker."
        ),
    )
    parser.add_argument(
        "--memory-pressure",
        metavar="LABEL",
        default=None,
        help=(
            "print the cgroup memory limits, peak, and OOM-kill count, and "
            "exit. Used by the harness from inside the container."
        ),
    )
    return parser.parse_args(argv)


@dataclass(frozen=True)
class PhaseRecord:
    """One timed harness phase."""

    name: str
    seconds: float


class PhaseTracker:
    """Turn the harness's ``## <name>`` markers into observable phases.

    soldr#1978 item 4. The acceptance is ~39-47 minutes and >99% of it lands
    in a single Actions step, so a failure says only "the 40-minute step
    failed" -- you re-run 40 minutes to find out where. The harness already
    announces every stage on stdout as ``## cold nextest archive build`` and
    friends; nothing was reading them.

    This folds each stage into a collapsible Actions group and records how
    long it took, so the log has navigable sections and the job summary ends
    with a timing table. Crucially it also remembers the phase that was open
    when output stopped: on failure that name is the single most useful fact,
    and it is exactly what the opaque step could never report.

    Grouping markers are emitted only under Actions -- locally they would be
    noise, since a terminal already shows the ``##`` lines in context.
    """

    MARKER = "## "

    # soldr#2817: everything the failure trap prints is itself announced with
    # a `##` marker, so the last marker before the stream ended was always the
    # last *diagnostic* section -- run 32893551296 died in `cold nextest
    # archive build` and reported `failed during phase: retained diagnostic
    # files`. The docstring above calls the open phase "the single most useful
    # fact"; it was being overwritten by the code printed to explain it.
    # The trap announces itself with this sentinel first, and everything after
    # it is still grouped for readability but can no longer claim the blame.
    DIAGNOSTICS_MARKER = "post-failure diagnostics"

    def __init__(self, clock=time.monotonic, emit_groups: bool = False) -> None:
        self._clock = clock
        self._emit_groups = emit_groups
        self._started_at: float | None = None
        self.current: str | None = None
        self.phases: list[PhaseRecord] = []
        self.failed: str | None = None

    def feed(self, line: str) -> str | None:
        """Consume one harness line; return a control line to print, if any."""
        if not line.startswith(self.MARKER):
            return None
        name = line[len(self.MARKER) :].strip()
        if not name:
            return None
        if name.lower() == self.DIAGNOSTICS_MARKER and self.failed is None:
            # Captured before _close() clears it.
            self.failed = self.current
        closing = self._close()
        self.current = name
        self._started_at = self._clock()
        if not self._emit_groups:
            return None
        # The close has to precede the open or Actions nests the groups.
        return f"{closing or ''}::group::{name}"

    def finish(self) -> None:
        """Close the open phase, if any. Safe to call more than once."""
        self._close()
        self.current = None

    def _close(self) -> str | None:
        if self.current is None or self._started_at is None:
            return None
        self.phases.append(PhaseRecord(self.current, self._clock() - self._started_at))
        self.current = None
        self._started_at = None
        return "::endgroup::\n" if self._emit_groups else None

    def record(self, name: str, seconds: float) -> None:
        """Record a phase measured outside the harness stream."""
        self.phases.append(PhaseRecord(name, seconds))

    def summary_markdown(self, failed_phase: str | None = None) -> str:
        """A phase-timing table for the job summary."""
        lines = ["### Cacheability phases", "", "| phase | duration |", "|---|---:|"]
        for phase in self.phases:
            lines.append(f"| {phase.name} | {format_duration(phase.seconds)} |")
        total = sum(phase.seconds for phase in self.phases)
        lines.append(f"| **total** | **{format_duration(total)}** |")
        if failed_phase:
            lines += ["", f"**Failed during:** `{failed_phase}`"]
        return "\n".join(lines) + "\n"


def format_duration(seconds: float) -> str:
    minutes, secs = divmod(int(seconds), 60)
    return f"{minutes}m {secs:02d}s" if minutes else f"{secs}s"


def write_step_summary(markdown: str) -> None:
    """Append to the Actions job summary; a no-op off Actions."""
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not path:
        return
    try:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(markdown)
    except OSError as err:  # a summary must never fail the acceptance
        print(f"warning: could not write job summary: {err}", file=sys.stderr)


def on_github_actions() -> bool:
    return os.environ.get("GITHUB_ACTIONS") == "true"


def docker_available() -> bool:
    try:
        result = subprocess.run(
            ["docker", "info"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
            timeout=20,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0


def build_image(image: str) -> int:
    return subprocess.run(
        ["docker", "build", "-f", str(DOCKERFILE), "-t", image, str(REPO_ROOT)],
        check=False,
    ).returncode


def _diagnostic_list(line: str, prefix: str) -> "list[str] | None":
    """JSON list carried on a harness diagnostic line, or None.

    A diagnostic that cannot be parsed must not become the failure:
    `evaluate_warm_result` degrades on None.
    """
    try:
        decoded = json.loads(line.removeprefix(prefix).strip())
    except json.JSONDecodeError:
        return None
    return decoded if isinstance(decoded, list) else None


def run_harness(  # noqa: C901
    image: str, volumes: list[str], tracker: "PhaseTracker | None" = None
) -> "tuple[int, WarmResult | None, list[str] | None, frozenset[str] | None]":
    cmd = [
        "docker",
        "run",
        "--rm",
        "--init",
        "-i",
        "-v",
        f"{REPO_ROOT}:/work",
        "-v",
        f"{volumes[0]}:/work/target",
        "-v",
        f"{volumes[1]}:/root/.cargo",
        "-v",
        f"{volumes[2]}:/root/.soldr",
        "-v",
        f"{volumes[3]}:/tmp/soldr-cacheability",
        "-w",
        "/work",
        image,
        "bash",
        "-s",
    ]
    print("+ " + " ".join(cmd), flush=True)
    # Long-lived by design: the recipe is written to stdin and output is
    # streamed back for phase tracking over a ~40-minute run.
    # pylint: disable-next=consider-using-with
    process = subprocess.Popen(
        cmd,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    assert process.stdin is not None
    assert process.stdout is not None

    try:
        payload = BASH_SCRIPT.replace("\r\n", "\n").replace("\r", "\n")
        process.stdin.write(payload.encode("utf-8"))
        process.stdin.close()
    except BrokenPipeError:
        pass

    result: WarmResult | None = None
    warm_miss_units: list[str] | None = None
    first_party: frozenset[str] | None = None
    tail: deque[str] = deque(maxlen=80)
    for raw_line in process.stdout:
        line = raw_line.decode("utf-8", errors="replace")
        if tracker is not None:
            control = tracker.feed(line)
            if control:
                print(control, flush=True)
        print(line, end="", flush=True)
        tail.append(line)
        if line.startswith("CACHEABILITY_RESULT "):
            payload = line.removeprefix("CACHEABILITY_RESULT ").strip()
            result = WarmResult.from_json(json.loads(payload))
        elif line.startswith("CACHEABILITY_WARM_MISSES "):
            warm_miss_units = _diagnostic_list(line, "CACHEABILITY_WARM_MISSES ")
        elif line.startswith("CACHEABILITY_WORKSPACE_METADATA "):
            targets = _diagnostic_list(line, "CACHEABILITY_WORKSPACE_METADATA ")
            first_party = None if targets is None else first_party_units(targets)

    code = process.wait()
    if tracker is not None and code == 0:
        # Leave the phase open on failure so the summary can name it.
        tracker.finish()
    if code != 0:
        print("\nlast harness output:", file=sys.stderr)
        for line in tail:
            print(line, end="", file=sys.stderr)
    return code, result, warm_miss_units, first_party


def remove_volumes(volumes: list[str]) -> None:
    subprocess.run(["docker", "volume", "rm", "--force", *volumes], check=False)


def main(argv: list[str]) -> int:  # noqa: C901
    args = parse_args(argv)
    if args.explain_report is not None:
        # Before the Docker check: this runs *inside* the container, where
        # there is no Docker daemon to reach.
        label, path = args.explain_report
        return emit_report_explanation(label, path)
    if args.memory_pressure is not None:
        # Same reason as above: inside the container, no Docker to reach.
        return emit_memory_pressure(args.memory_pressure)
    if not docker_available():
        print(
            "error: docker is not available or the daemon is not reachable",
            file=sys.stderr,
        )
        return 2

    suffix = args.suffix or f"{int(time.time())}-{os.getpid()}"
    volumes = [
        f"soldr-nextest-cacheability-target-{suffix}",
        f"soldr-nextest-cacheability-cargo-{suffix}",
        f"soldr-nextest-cacheability-home-{suffix}",
        f"soldr-nextest-cacheability-diagnostics-{suffix}",
    ]
    print("Docker volumes:")
    for volume in volumes:
        print(f"  {volume}")

    tracker = PhaseTracker(emit_groups=on_github_actions())
    try:
        # soldr#1978 item 4: the image build is a phase in its own right --
        # `--pull` with no layer cache means it can dominate a run, and until
        # now it was indistinguishable from the acceptance it precedes.
        build_started = time.monotonic()
        image_code = build_image(args.image)
        tracker.record("docker build", time.monotonic() - build_started)
        if image_code != 0:
            return image_code
        code, result, warm_miss_units, first_party = run_harness(
            args.image, volumes, tracker
        )
        if code != 0:
            return code
        if result is None:
            print("error: harness did not emit CACHEABILITY_RESULT", file=sys.stderr)
            return 4
        failures = evaluate_warm_result(result, warm_miss_units, first_party)
        if failures:
            print("CACHEABILITY_FAILURE dependency compilation is not warm")
            for failure in failures:
                print(f"error: {failure}", file=sys.stderr)
            print(f"error: report was {asdict(result)}", file=sys.stderr)
            return 6
        print(
            "CACHEABILITY_OK dependency units reused the compiler cache; "
            "test-harness link products are not required to be hits (soldr#2931)"
        )
        return 0
    finally:
        # The summary matters most when something failed, so it is written
        # here rather than on the success path: `failed_phase` names the
        # stage that was still open, which is the fact a single opaque step
        # could never give you.
        failed_phase = tracker.failed or tracker.current
        tracker.finish()
        write_step_summary(tracker.summary_markdown(failed_phase))
        if failed_phase:
            print(f"failed during phase: {failed_phase}", file=sys.stderr)
        if args.keep_volumes:
            print("Keeping Docker volumes for inspection.")
        else:
            remove_volumes(volumes)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
