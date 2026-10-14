#!/usr/bin/env python3
"""Report drift between the live repository settings and ci/repo-settings.json.

soldr#2469 6.2: the settings the release and merge model depend on --
immutable releases, SHA-pinned actions, read-only default workflow token,
merge methods, whether ``main`` is protected -- live outside the git tree, so
nothing noticed when one changed. This audit reads each one and compares it to
the reviewed baseline:

* a value that differs from the baseline is **drift** and fails the audit;
* a value the token cannot read (several endpoints need admin read; the
  workflow's ``GITHUB_TOKEN`` has none) is **unverified**: reported, never
  silently passed. Configure the ``SOLDR_SETTINGS_AUDIT_TOKEN`` secret with
  admin read to verify those too.

    python3 .github/scripts/settings_audit.py [--repo owner/name]
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BASELINE = ROOT / "ci" / "repo-settings.json"

Fetch = Callable[[str], object]


class Unreadable(Exception):
    """The token may not read this endpoint."""


@dataclass(frozen=True)
class Finding:
    setting: str
    expected: str
    actual: str
    kind: str  # "drift" | "unverified"


def http_fetch(token: str) -> Fetch:
    def fetch(path: str) -> object:
        request = urllib.request.Request(
            f"https://api.github.com{path}",
            headers={
                "Accept": "application/vnd.github+json",
                **({"Authorization": f"Bearer {token}"} if token else {}),
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                return json.loads(response.read().decode("utf-8"))
        except urllib.error.HTTPError as error:
            if error.code == 404 and path.endswith("/protection"):
                return None  # "Branch not protected" is a readable answer
            if error.code in (401, 403, 404):
                raise Unreadable(f"{path}: HTTP {error.code}") from error
            raise

    return fetch


def _compare(prefix: str, expected: object, actual: object, out: list[Finding]) -> None:
    if not isinstance(expected, dict) or not isinstance(actual, dict):
        raise ValueError(f"{prefix}: baseline and live value must both be objects")
    for key, want in expected.items():
        if key not in actual:
            # GitHub omits some fields (e.g. the allow_* merge methods) unless
            # the token has admin read: absent means unreadable, not changed.
            out.append(
                Finding(
                    f"{prefix}.{key}",
                    json.dumps(want),
                    "field not returned",
                    "unverified",
                )
            )
            continue
        got = actual[key]
        if got != want:
            out.append(
                Finding(f"{prefix}.{key}", json.dumps(want), json.dumps(got), "drift")
            )


def audit(repo: str, baseline: dict, fetch: Fetch) -> list[Finding]:
    findings: list[Finding] = []
    sections = {
        "repository": f"/repos/{repo}",
        "immutable_releases": f"/repos/{repo}/immutable-releases",
        "actions_permissions": f"/repos/{repo}/actions/permissions",
        "workflow_permissions": f"/repos/{repo}/actions/permissions/workflow",
    }
    for name, path in sections.items():
        if name not in baseline:
            continue
        try:
            live = fetch(path)
        except Unreadable as error:
            findings.append(
                Finding(name, json.dumps(baseline[name]), str(error), "unverified")
            )
            continue
        _compare(name, baseline[name], live, findings)
    if "branch_protected" in baseline:
        branch = baseline.get("repository", {}).get("default_branch", "main")
        try:
            protected = fetch(f"/repos/{repo}/branches/{branch}/protection") is not None
        except Unreadable as error:
            findings.append(Finding("branch_protected", "?", str(error), "unverified"))
        else:
            if protected != baseline["branch_protected"]:
                findings.append(
                    Finding(
                        "branch_protected",
                        json.dumps(baseline["branch_protected"]),
                        json.dumps(protected),
                        "drift",
                    )
                )
    return findings


def render(findings: list[Finding]) -> str:
    if not findings:
        return "repository settings match ci/repo-settings.json"
    lines = []
    for finding in findings:
        lines.append(
            f"{finding.kind}: {finding.setting}: expected {finding.expected}, "
            f"live {finding.actual}"
        )
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo", default=os.environ.get("GITHUB_REPOSITORY", "zackees/soldr")
    )
    args = parser.parse_args(argv)
    baseline = json.loads(BASELINE.read_text(encoding="utf-8"))
    token = os.environ.get("SOLDR_SETTINGS_AUDIT_TOKEN") or os.environ.get(
        "GH_TOKEN", ""
    )
    findings = audit(args.repo, baseline, http_fetch(token))
    report = render(findings)
    print(report)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as handle:
            handle.write(f"### Repository settings audit\n\n```\n{report}\n```\n")
    return 1 if any(f.kind == "drift" for f in findings) else 0


if __name__ == "__main__":
    sys.exit(main())
