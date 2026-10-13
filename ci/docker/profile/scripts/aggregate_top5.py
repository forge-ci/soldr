#!/usr/bin/env python3
"""Aggregate on-CPU + off-CPU folded stacks into a top-5 markdown table.

Consumed by the entrypoint (`run_profile.sh`) after every scenario's
`oncpu.folded` and `offcpu.folded` have been produced by
stackcollapse-perf.pl / stackcollapse-bpftrace.pl.

Folded-stack line format (per Brendan Gregg's FlameGraph conventions):

    frame_root;frame_1;frame_2;...;frame_leaf COUNT

The leaf frame is the innermost function on the stack; that's what a
flame-graph reader would zoom into to identify a hot function. We
aggregate by leaf across every scenario and both sampler kinds, sum
sample counts, and emit the top 5.

Usage:
    aggregate_top5.py --out-dir <dir> --scenarios <name1> <name2> ...

Writes `<out-dir>/top5.md`.
"""

from __future__ import annotations

import argparse
import re
import sys
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

# Folded-stack lines end with a whitespace-separated integer sample count.
# Everything before the last space is the semicolon-joined stack.
_LINE_RE = re.compile(r"^(?P<stack>.+)\s+(?P<count>\d+)\s*$")


@dataclass(frozen=True)
class FoldedLeaf:
    """The leaf frame of one folded-stack line and its sample count."""

    leaf: str
    count: int


def parse_folded(path: Path) -> list[FoldedLeaf]:
    """Return the leaf and count of each folded-stack line. Empty if missing."""
    if not path.is_file() or path.stat().st_size == 0:
        return []
    out: list[FoldedLeaf] = []
    with path.open("r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            match = _LINE_RE.match(line)
            if not match:
                continue
            stack = match.group("stack")
            count = int(match.group("count"))
            # Last semicolon-separated frame is the leaf. If the stack is a
            # single frame, that frame IS the leaf.
            leaf = stack.rsplit(";", 1)[-1].strip()
            if not leaf:
                continue
            out.append(FoldedLeaf(leaf, count))
    return out


def _normalize_leaf(leaf: str) -> str:
    """Drop offsets and address suffixes so `foo+0x12` and `foo+0x40` bucket together."""
    # Strip trailing "+0x..." offset annotations.
    leaf = re.sub(r"\+0x[0-9a-fA-F]+$", "", leaf)
    # Some perf-script outputs append "[unknown]" or address hex in parens.
    leaf = re.sub(r"\s*\(?[0-9a-fA-F]{6,}\)?\s*$", "", leaf).strip()
    return leaf or "<unknown>"


def main() -> int:  # noqa: C901
    parser = argparse.ArgumentParser()
    parser.add_argument("--out-dir", required=True, type=Path)
    parser.add_argument("--scenarios", nargs="+", required=True)
    parser.add_argument("--top-n", type=int, default=5)
    args = parser.parse_args()

    out_dir: Path = args.out_dir
    scenarios: list[str] = args.scenarios

    # totals[leaf] = { "on_cpu": int, "off_cpu": int,
    #                  "scenario_on_cpu": {scen: samples},
    #                  "scenario_off_cpu": {scen: samples} }
    totals: dict[str, dict] = defaultdict(
        lambda: {
            "on_cpu": 0,
            "off_cpu": 0,
            "scenario_on_cpu": defaultdict(int),
            "scenario_off_cpu": defaultdict(int),
        }
    )

    seen_any = False
    for scen in scenarios:
        for kind, filename in (
            ("on_cpu", "oncpu.folded"),
            ("off_cpu", "offcpu.folded"),
        ):
            path = out_dir / scen / filename
            entries = parse_folded(path)
            if entries:
                seen_any = True
            for entry in entries:
                leaf = _normalize_leaf(entry.leaf)
                totals[leaf][kind] += entry.count
                totals[leaf][f"scenario_{kind}"][scen] += entry.count

    top5_path = out_dir / "top5.md"

    if not seen_any:
        top5_path.write_text(
            "# Top 5 slowest items (perf-matrix cycle, on-CPU + off-CPU)\n\n"
            "_No folded-stack data captured. Check `run.log` and each "
            "scenario's `perf.log` / `offcpu.bpftrace.log`._\n",
            encoding="utf-8",
        )
        print(f"[aggregate_top5] no data — wrote empty {top5_path}")
        return 0

    # On-CPU and off-CPU counts live on different scales:
    #   - on-CPU  = perf-script periods × Hz (10^7 - 10^10 range)
    #   - off-CPU = raw blocked µs (bpftrace) or sched_switch counts
    #     (10^0 - 10^4 range)
    # Naively summing hides off-CPU signal. Emit TWO tables so both
    # dimensions get a fair look.
    top_on_cpu = sorted(
        [(leaf, row) for leaf, row in totals.items() if row["on_cpu"] > 0],
        key=lambda kv: kv[1]["on_cpu"],
        reverse=True,
    )[: args.top_n]
    top_off_cpu = sorted(
        [(leaf, row) for leaf, row in totals.items() if row["off_cpu"] > 0],
        key=lambda kv: kv[1]["off_cpu"],
        reverse=True,
    )[: args.top_n]

    def _dominant_scenario(row: dict, kind: str) -> str:
        source = row[f"scenario_{kind}"]
        if not source:
            return "n/a"
        return max(source.items(), key=lambda kv: kv[1])[0]

    lines: list[str] = []
    lines.append("# Top 5 slowest items (perf-matrix cycle)")
    lines.append("")
    lines.append(f"Aggregated across scenarios: `{', '.join(scenarios)}`.")
    lines.append("")
    lines.append("## On-CPU (perf record -F 99)")
    lines.append("")
    lines.append("| Rank | Function | On-CPU samples | Dominant scenario |")
    lines.append("|------|----------|----------------|-------------------|")
    for rank, (leaf, row) in enumerate(top_on_cpu, start=1):
        leaf_md = leaf.replace("|", "\\|")
        lines.append(
            f"| {rank} | `{leaf_md}` | {row['on_cpu']:,} | {_dominant_scenario(row, 'on_cpu')} |"
        )
    if not top_on_cpu:
        lines.append("| _no on-CPU samples captured_ | | | |")
    lines.append("")
    lines.append("## Off-CPU (bpftrace sched_switch, perf-sched fallback)")
    lines.append("")
    lines.append("| Rank | Function | Off-CPU samples | Dominant scenario |")
    lines.append("|------|----------|-----------------|-------------------|")
    for rank, (leaf, row) in enumerate(top_off_cpu, start=1):
        leaf_md = leaf.replace("|", "\\|")
        lines.append(
            f"| {rank} | `{leaf_md}` | {row['off_cpu']:,} | {_dominant_scenario(row, 'off_cpu')} |"
        )
    if not top_off_cpu:
        lines.append("| _no off-CPU samples captured_ | | | |")
    lines.append("")
    lines.append("Notes:")
    lines.append(
        "- On-CPU units: perf-script sample periods (Hz-scaled by "
        "`SOLDR_PROFILE_HZ`, default 99)."
    )
    lines.append(
        "- Off-CPU units: bpftrace blocked microseconds when bpftrace succeeds; "
        "sched_switch stack counts on the perf-sched fallback path."
    )
    lines.append(
        "- Off-CPU stacks almost always leaf at kernel `__schedule_[k]` "
        "(where the task is actually descheduled). Look one frame up for the "
        "syscall or userspace waiter."
    )
    lines.append(
        "- Leaf frames are normalized: address offsets stripped so `foo+0x12` "
        "and `foo+0x40` bucket together."
    )
    lines.append(
        "- Frame-pointer unwinds miss symbols for release-built Rust code; "
        "leaves like `[unknown]` or `[soldr]` indicate the sample landed in "
        "user-space Rust. Rerun with `SOLDR_PROFILE_HZ` higher or switch "
        "`perf record` to `--call-graph dwarf` for finer-grained user-space "
        "resolution."
    )
    lines.append(
        "- Open per-scenario `.svg` files for full stack context; this table "
        "is a triage summary."
    )
    lines.append("")

    top5_path.write_text("\n".join(lines), encoding="utf-8")
    print(
        f"[aggregate_top5] wrote {top5_path} with "
        f"{len(top_on_cpu)} on-CPU rows + {len(top_off_cpu)} off-CPU rows"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
