#!/usr/bin/env python3
"""Write a stable summary for a cross-target nextest archive run.

The summary exists before toolchain provisioning starts and is enriched after
nextest lists and runs the archive. This leaves an actionable artifact even
when the target worker fails before the test runner can start.
"""

from __future__ import annotations

import argparse
import json
import xml.etree.ElementTree as ET
from dataclasses import asdict, dataclass
from pathlib import Path

SCHEMA_VERSION = 1


@dataclass(frozen=True)
class ListCounts:
    """Selected and ignored testcase counts from a nextest list document."""

    discovered: int
    ignored: int


@dataclass(frozen=True)
class JunitCounts:
    """Totals reconciled from a JUnit report."""

    executed: int
    passed: int
    failed: int
    skipped: int


@dataclass(frozen=True)
class RunSummary:
    """The stable per-target summary; `asdict` is the emitted JSON schema."""

    schema_version: int
    target: str
    partition: str | None
    phase: str
    discovered: int | None
    ignored: int | None
    executed: int | None
    passed: int | None
    failed: int | None
    skipped: int | None


def _int_attr(element: ET.Element, name: str) -> int:
    value = element.attrib.get(name, "0")
    try:
        count = int(value)
    except ValueError as error:
        raise ValueError(f"invalid JUnit {name} count: {value!r}") from error
    if count < 0:
        raise ValueError(f"JUnit {name} count must be nonnegative: {count}")
    return count


def read_test_list(path: Path | None) -> ListCounts | None:  # noqa: C901
    if path is None:
        return None
    if not path.is_file():
        raise ValueError(f"required nextest list JSON is missing: {path}")
    payload = json.loads(path.read_text(encoding="utf-8"))
    reported = payload.get("test-count")
    if isinstance(reported, bool) or not isinstance(reported, int):
        raise ValueError("nextest list JSON is missing integer test-count")
    if reported < 0:
        raise ValueError("nextest list test-count must be nonnegative")
    rust_suites = payload.get("rust-suites")
    if not isinstance(rust_suites, dict):
        raise ValueError("nextest list JSON is missing rust-suites object")
    selected_ignored: list[bool] = []
    for suite in rust_suites.values():
        for testcase in suite.get("testcases", {}).values():
            filter_match = testcase.get("filter-match")
            if filter_match is None:
                # Unfiltered `nextest list` documents no match decision; every
                # testcase is selected in that representation.
                selected_ignored.append(testcase.get("ignored") is True)
                continue
            if not isinstance(filter_match, dict) or filter_match.get("status") not in {
                "matches",
                "mismatch",
            }:
                raise ValueError("nextest testcase has invalid filter-match status")
            if filter_match["status"] == "matches":
                selected_ignored.append(testcase.get("ignored") is True)
    discovered = len(selected_ignored)
    if discovered > reported:
        raise ValueError(
            "nextest selected testcase count exceeds reported test-count: "
            f"selected={discovered}, reported={reported}"
        )
    ignored = sum(selected_ignored)
    if ignored > discovered:
        raise ValueError("nextest ignored count exceeds discovered count")
    return ListCounts(discovered, ignored)


def read_junit(path: Path | None) -> JunitCounts | None:
    if path is None or not path.is_file():
        return None
    root = ET.parse(path).getroot()
    root_tag = root.tag.rsplit("}", 1)[-1]
    if root_tag == "testsuite":
        suites = [root]
    elif root_tag == "testsuites":
        suites = [
            child for child in root if child.tag.rsplit("}", 1)[-1] == "testsuite"
        ]
        if not suites:
            if "tests" not in root.attrib:
                raise ValueError(
                    "JUnit testsuites root contains no test suites or totals"
                )
            suites = [root]
    else:
        raise ValueError(f"unexpected JUnit root element: {root_tag!r}")
    executed = sum(_int_attr(suite, "tests") for suite in suites)
    failures = sum(_int_attr(suite, "failures") for suite in suites)
    errors = sum(_int_attr(suite, "errors") for suite in suites)
    skipped = sum(_int_attr(suite, "skipped") for suite in suites)
    failed = failures + errors
    passed = executed - failed - skipped
    if failed + skipped > executed:
        raise ValueError("JUnit totals are internally inconsistent")
    return JunitCounts(executed, passed, failed, skipped)


def build_summary(
    target: str,
    list_json: Path | None = None,
    junit: Path | None = None,
    *,
    require_junit: bool = False,
    partition: str | None = None,
) -> RunSummary:
    listed = read_test_list(list_json)
    discovered = listed.discovered if listed else None
    ignored = listed.ignored if listed else None
    run = read_junit(junit)
    if require_junit and run is None:
        raise ValueError(f"required JUnit report is missing: {junit}")
    if discovered == 0:
        raise ValueError("nextest selected zero tests")
    if run is not None and run.executed == 0:
        raise ValueError("nextest executed zero tests")
    if run is not None and discovered is not None and ignored is not None:
        accounted = run.executed + ignored
        is_complete_partition = partition in (None, "hash:1/1")
        counts_disagree = (
            accounted != discovered
            if is_complete_partition
            else run.executed > discovered
        )
        # soldr#2724: the lane runs `--max-fail 3:immediate`, so a run that
        # hits its third failure stops with tests still unexecuted -- by
        # design, and reported as such by nextest. Under-execution *with*
        # failures is that intentional early stop; under-execution with zero
        # failures is the coverage hole this check exists to catch (a
        # partition quietly skipping tests). Discriminating on the observed
        # failure count keeps the guard without threading `--max-fail`'s
        # value from the workflow into this script.
        #
        # Over-execution is never explained by an early stop, so it still
        # raises regardless: more tests ran than were discovered.
        stopped_early = run.failed > 0 and accounted < discovered
        if counts_disagree and not stopped_early:
            raise ValueError(
                "nextest coverage counts disagree: "
                f"discovered={discovered}, executed={run.executed}, "
                f"ignored={ignored}"
            )
    if run is not None:
        phase = "completed"
    elif discovered is not None:
        phase = "listed"
    else:
        phase = "setup"
    return RunSummary(
        schema_version=SCHEMA_VERSION,
        target=target,
        partition=partition,
        phase=phase,
        discovered=discovered,
        ignored=ignored,
        executed=run.executed if run else None,
        passed=run.passed if run else None,
        failed=run.failed if run else None,
        skipped=run.skipped if run else None,
    )


def append_markdown(path: Path, summary: RunSummary) -> None:
    values = [
        summary.target,
        summary.phase,
        summary.discovered,
        summary.executed,
        summary.passed,
        summary.failed,
        summary.ignored,
        summary.skipped,
    ]
    rendered = ["n/a" if value is None else str(value) for value in values]
    with path.open("a", encoding="utf-8") as stream:
        stream.write("\n### Target-run coverage\n\n")
        stream.write(
            "| Target | Phase | Discovered | Executed | Passed | Failed | Ignored | Skipped |\n"
        )
        stream.write("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |\n")
        stream.write("| " + " | ".join(rendered) + " |\n")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--list-json", type=Path)
    parser.add_argument("--junit", type=Path)
    parser.add_argument("--require-junit", action="store_true")
    parser.add_argument("--partition")
    parser.add_argument("--github-summary", type=Path)
    args = parser.parse_args()

    summary = build_summary(
        args.target,
        args.list_json,
        args.junit,
        require_junit=args.require_junit,
        partition=args.partition,
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(asdict(summary), indent=2) + "\n", encoding="utf-8"
    )
    if args.github_summary is not None:
        append_markdown(args.github_summary, summary)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
