#!/usr/bin/env python3
"""Fail a full CI run when any canonical target or smoke job was skipped."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
EXTRA_REQUIRED = {
    "setup-soldr-action",
    "cook-size-gate",
    "lint",
    "build-linux-x64",
    "pep517-daemon-smoke",
    "windows-e2e-policy",
    "e2e-cross-bootstrap-soldr",
    "e2e-linux-x64-gnu-build",
    "wheel-cross-policy",
    "wheel-cross-verify",
}


@dataclass(frozen=True)
class JsonDocument:
    """A parsed JSON object whose schema the consumers validate field by field."""

    fields: dict  # raw JSON object at the parse boundary

    def get(self, key: str, default: Any = None) -> Any:
        return self.fields.get(key, default)


def required_jobs(contract: JsonDocument) -> set[str]:  # noqa: C901
    declared = contract.get("required_jobs", list(EXTRA_REQUIRED))
    if "required_jobs" in contract.fields and (
        not isinstance(contract.get("schema_version"), int)
        or isinstance(contract.get("schema_version"), bool)
        or contract.get("schema_version") != 1
    ):
        raise ValueError("portable coverage contract requires schema_version 1")
    if (
        not isinstance(declared, list)
        or not declared
        or any(
            not isinstance(job, str)
            or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_-]*", job)
            for job in declared
        )
        or len(set(declared)) != len(declared)
    ):
        raise ValueError("required_jobs must be a nonempty list of unique job names")
    jobs = set(declared)
    targets = contract.get("targets")
    if not isinstance(targets, list):
        raise ValueError("targets must be a list")
    identities = set()
    for target in targets:
        if not isinstance(target, dict) or not isinstance(target.get("ci"), dict):
            raise ValueError("target must declare a ci object")
        if "required_jobs" in contract.fields:
            identity = target.get("triple")
            if (
                not isinstance(identity, str)
                or not identity.strip()
                or identity in identities
            ):
                raise ValueError(
                    "portable target identities must be nonempty and unique"
                )
            identities.add(identity)
        ci = target["ci"]
        for field in ("build_job", "run_job"):
            job = ci.get(field)
            if field == "run_job" and job is None:
                continue
            if not isinstance(job, str) or not re.fullmatch(
                r"[A-Za-z_][A-Za-z0-9_-]*", job
            ):
                raise ValueError(f"target {field} must be a valid job ID")
        jobs.add(ci["build_job"])
        if ci.get("run_job"):
            jobs.add(ci["run_job"])
    return jobs


def coverage_failures(
    contract: JsonDocument,
    needs: JsonDocument,
    *,
    expected_sha: str | None = None,
    selected_sha: str | None = None,
) -> list[str]:
    failures = []
    if expected_sha is not None or selected_sha is not None:
        if (
            not isinstance(expected_sha, str)
            or not re.fullmatch(r"[0-9a-fA-F]{40}", expected_sha)
            or not isinstance(selected_sha, str)
            or selected_sha.lower() != expected_sha.lower()
        ):
            failures.append(
                "candidate SHA is missing, invalid, or does not match selection"
            )
    for job in sorted(required_jobs(contract)):
        result = needs.get(job)
        state = result.get("result") if isinstance(result, dict) else "missing"
        if state != "success":
            failures.append(f"{job}: {state or 'missing'}")
    for target in contract.fields["targets"]:
        ci = target["ci"]
        if ci.get("run_job"):
            continue
        issue = ci.get("execution_exception", {}).get("issue")
        reference = f" (soldr#{issue})" if issue else ""
        failures.append(f"{target['triple']}: no full-CI execution job{reference}")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--contract", type=Path)
    parser.add_argument("--expected-sha")
    parser.add_argument("--selected-sha")
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    contract_path = args.contract or ROOT / "ci" / "canonical-targets.json"
    raw = contract_path.read_bytes()
    contract = JsonDocument(json.loads(raw))
    needs = JsonDocument(json.loads(os.environ["CI_NEEDS_JSON"]))
    if args.contract and "required_jobs" not in contract.fields:
        raise ValueError("portable contract requires explicit required_jobs")
    if (args.contract or args.report) and (
        not args.expected_sha or not args.selected_sha
    ):
        raise ValueError("portable coverage and reports require both candidate SHAs")
    failures = coverage_failures(
        contract, needs, expected_sha=args.expected_sha, selected_sha=args.selected_sha
    )
    if args.report:
        jobs = sorted(required_jobs(contract))
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(
            json.dumps(
                {
                    "schema": "fleet-ci-coverage/v1",
                    "manifest_sha256": hashlib.sha256(raw).hexdigest(),
                    "candidate_sha": args.expected_sha.lower(),
                    "selected_sha": args.selected_sha.lower(),
                    "required_jobs": jobs,
                    "outcomes": {
                        job: (
                            needs.get(job, {}).get("result", "missing")
                            if isinstance(needs.get(job, {}), dict)
                            else "missing"
                        )
                        for job in jobs
                    },
                    "failures": failures,
                    "success": not failures,
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
    if failures:
        for failure in failures:
            print(f"::error::full CI coverage incomplete: {failure}")
        return 1
    print("Full CI coverage complete for all declared targets and smoke jobs")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
