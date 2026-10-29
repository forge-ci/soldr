#!/usr/bin/env python3
"""Terminal release-surface completeness gate (soldr#2469 step 1.1).

The 0.9.0 incident: run 31487675391 concluded green while the immutable
GitHub Release was missing 10 of its 17 assets, because the asset-verify
job was skipped (release already immutable) and skipped jobs do not fail
a run. This script is the terminal truth check: a normal release run must
hard-fail unless EVERY public surface is complete for the release ref —

  1. GitHub Release: all archives + wheels + SHA256SUMS derived from
     ci/canonical-targets.json (never a hand-maintained list),
  2. PyPI: every expected wheel filename present for the version,
  3. npm: the version exists for the package,
  4. crates.io: every publishable workspace crate's version exists, or an
     explicit skip reason when every crate is `publish = false` (soldr#3343).

Verification logic is pure (lists in, failure strings out) so tests
reproduce the 0.9.0 false-green without any network. Network fetchers use
stdlib urllib only.

Usage (CI):
    python3 .github/scripts/release_completeness.py --version v0.9.1
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import tomllib
import urllib.request
from dataclasses import asdict, dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
CONTRACT = REPO_ROOT / "ci" / "canonical-targets.json"
CI_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "ci.yml"
RELEASE_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "auto-release.yml"
NPM_PACKAGE = "@zackees/soldr"
PYPI_PROJECT = "soldr"
GITHUB_REPO = os.environ.get("GITHUB_REPOSITORY", "zackees/soldr")

# Wheel platform tag per canonical triple. Parity with the contract's
# included targets is unit-tested; a new target must extend this table
# explicitly (the same reviewed-decision property as soldr#2469 step 2.1).
WHEEL_TAGS: dict[str, str] = {
    "x86_64-pc-windows-msvc": "win_amd64",
    "aarch64-pc-windows-msvc": "win_arm64",
    "x86_64-apple-darwin": "macosx_10_12_x86_64",
    "aarch64-apple-darwin": "macosx_11_0_arm64",
    "x86_64-unknown-linux-gnu": "manylinux_2_17_x86_64.manylinux2014_x86_64",
    "aarch64-unknown-linux-gnu": "manylinux_2_17_aarch64.manylinux2014_aarch64",
    "x86_64-unknown-linux-musl": "musllinux_1_2_x86_64",
    "aarch64-unknown-linux-musl": "musllinux_1_2_aarch64",
}


def included_triples(contract_path: Path = CONTRACT) -> list[str]:
    data = json.loads(contract_path.read_text(encoding="utf-8"))
    return [
        entry["triple"]
        for entry in data["targets"]
        if entry["release"]["status"] == "included"
    ]


def _workflow_job(workflow: str, name: str) -> str | None:
    match = re.search(
        rf"^  {re.escape(name)}:\s*$(.*?)(?=^  [\w-]+:\s*$|\Z)",
        workflow,
        re.MULTILINE | re.DOTALL,
    )
    return match.group(1) if match else None


def _job_needs(job: str, dependency: str) -> bool:
    scalar_or_inline = re.search(
        r"^[ \t]+needs:[ \t]*(\S.*?)[ \t]*$", job, re.MULTILINE
    )
    if scalar_or_inline:
        value = scalar_or_inline.group(1).strip()
        if value.startswith("[") and value.endswith("]"):
            return dependency in {
                item.strip().strip("'\"") for item in value[1:-1].split(",")
            }
        return value.strip("'\"") == dependency
    block = re.search(
        r"^[ \t]+needs:[ \t]*$\n(?P<items>(?:[ \t]+-[ \t]+[^\n]+\n?)*)",
        job,
        re.MULTILINE,
    )
    if not block:
        return False
    return dependency in {
        line.split("-", 1)[1].strip().strip("'\"")
        for line in block.group("items").splitlines()
    }


def _job_requires_success(job: str, dependency: str) -> bool:
    """Whether a job's condition explicitly requires a dependency to succeed."""
    dotted = f"needs.{dependency}.result == 'success'"
    bracketed = f"needs['{dependency}'].result == 'success'"
    return dotted in job or bracketed in job


def release_execution_failures(  # noqa: C901
    contract_path: Path = CONTRACT,
    ci_workflow_path: Path = CI_WORKFLOW,
    release_workflow_path: Path = RELEASE_WORKFLOW,
) -> list[str]:
    """Return release-blocking canonical targets that still lack execution."""

    data = json.loads(contract_path.read_text(encoding="utf-8"))
    failures = []
    for entry in data["targets"]:
        release = entry["release"]
        gate = release.get("execution_gate")
        if release["status"] != "included" or not isinstance(gate, dict):
            continue
        if gate.get("required") is not True:
            continue
        run_job = entry["ci"].get("run_job")
        provenance = release.get("artifact_provenance")
        execution = (
            provenance.get("execution") if isinstance(provenance, dict) else None
        )
        statuses = {
            artifact: (
                execution.get(artifact, {}).get("status")
                if isinstance(execution, dict)
                and isinstance(execution.get(artifact), dict)
                else None
            )
            for artifact in ("archive", "wheel")
        }
        prefix = (
            f"{entry['triple']}: release execution required by "
            f"soldr#{gate.get('issue')}"
        )
        if not isinstance(run_job, str) or not run_job.strip():
            failures.append(f"{prefix} but ci.run_job is missing")
            for artifact, status in statuses.items():
                if status != "not-executed":
                    failures.append(
                        f"{prefix} and has no run job, but manifest provenance "
                        f"claims {artifact} status {status!r} instead of 'not-executed'"
                    )
            continue
        workflow = ci_workflow_path.read_text(encoding="utf-8")
        job = _workflow_job(workflow, run_job)
        if job is None:
            failures.append(f"{prefix} but ci.run_job {run_job!r} does not exist")
            continue
        build_job = entry["ci"].get("build_job")
        if (
            entry["triple"] not in job
            or "./.github/workflows/_ci-target-run.yml" not in job
            or not isinstance(build_job, str)
            or not _job_needs(job, build_job)
        ):
            failures.append(
                f"{prefix} but ci.run_job {run_job!r} is not a target-matched "
                "replay attached to its build job"
            )
            continue
        release_workflow = release_workflow_path.read_text(encoding="utf-8")
        publishers = {
            name: _workflow_job(release_workflow, name)
            for name in ("publish", "publish-pypi", "publish-npm")
        }
        for artifact, status in statuses.items():
            if status == "not-executed":
                failures.append(
                    f"{prefix} has ci.run_job {run_job!r}, but manifest provenance "
                    f"still records {artifact} as 'not-executed'"
                )
                continue
            record = execution.get(artifact) if isinstance(execution, dict) else None
            release_gate = record.get("gate_job") if isinstance(record, dict) else None
            gate_job = (
                _workflow_job(release_workflow, release_gate)
                if isinstance(release_gate, str) and release_gate
                else None
            )
            if (
                status != "required-before-publication"
                or not isinstance(release_gate, str)
                or gate_job is None
            ):
                failures.append(
                    f"{prefix} has no real release gate for the shipped {artifact}; "
                    f"provenance status={status!r}, gate_job={release_gate!r}"
                )
                continue
            if entry["triple"] not in gate_job:
                failures.append(
                    f"{prefix} release gate {release_gate!r} is not target-matched "
                    f"to the shipped {artifact}"
                )
            artifact_name = (
                f"release-soldr-{entry['triple']}"
                if artifact == "archive"
                else f"pypi-soldr-{entry['triple']}"
            )
            if (
                "actions/download-artifact@" not in gate_job
                or artifact_name not in gate_job
            ):
                failures.append(
                    f"{prefix} release gate {release_gate!r} does not download "
                    f"the shipped {artifact} artifact {artifact_name!r}"
                )
            required_smoke_tokens = ["ci/smoke_release_artifacts.py"]
            if artifact == "wheel":
                required_smoke_tokens.append("--require-wheel-import")
            else:
                required_smoke_tokens.append("--require-daemon-cache-smoke")
            missing_tokens = [
                token for token in required_smoke_tokens if token not in gate_job
            ]
            if missing_tokens:
                failures.append(
                    f"{prefix} release gate {release_gate!r} does not prove the "
                    f"shipped {artifact}; missing {', '.join(missing_tokens)}"
                )
            for publisher_name, publisher in publishers.items():
                if publisher is None or not _job_needs(publisher, release_gate):
                    failures.append(
                        f"{prefix} release gate {release_gate!r} for the shipped "
                        f"{artifact} is not a {publisher_name} dependency"
                    )
                elif not _job_requires_success(publisher, release_gate):
                    failures.append(
                        f"{prefix} {publisher_name} does not require successful "
                        f"completion of release gate {release_gate!r} for the "
                        f"shipped {artifact}"
                    )
    return failures


@dataclass(frozen=True)
class BuildMatrixRow:
    """One `strategy.matrix.include` row; field order is the JSON key order."""

    name: str
    runner: str
    target: str
    setup_target: str
    binary: str


def build_matrix(contract_path: Path = CONTRACT) -> list[BuildMatrixRow]:
    """Contract-generated release build matrix (soldr#2469 step 2.1).

    Returns the `strategy.matrix.include` list for auto-release.yml's build
    job, derived from each included target's `release.build` block — the
    workflow's hand-inlined matrix this replaces is exactly what let PR
    #2455 shrink the matrix and the contract together with nothing failing.
    """
    data = json.loads(contract_path.read_text(encoding="utf-8"))
    matrix: list[BuildMatrixRow] = []
    for entry in data["targets"]:
        if entry["release"]["status"] != "included":
            continue
        build = entry["release"]["build"]
        matrix.append(
            BuildMatrixRow(
                name=build["name"],
                runner=build["runner"],
                target=entry["triple"],
                setup_target=build["setup_target"],
                binary=build["binary"],
            )
        )
    return matrix


def expected_github_assets(tag: str, triples: list[str]) -> list[str]:
    version = tag.lstrip("v")
    assets = [f"soldr-{tag}-{triple}.tar.zst" for triple in triples]
    assets += [
        f"soldr-{version}-py3-none-{WHEEL_TAGS[triple]}.whl" for triple in triples
    ]
    assets.append(f"soldr-{tag}-SHA256SUMS.txt")
    # soldr#3343: the release-level record of the artifact set.
    assets.append(f"soldr-{tag}-info.json")
    return assets


def expected_pypi_files(tag: str, triples: list[str]) -> list[str]:
    version = tag.lstrip("v")
    return [f"soldr-{version}-py3-none-{WHEEL_TAGS[triple]}.whl" for triple in triples]


def verify_surfaces(
    tag: str,
    triples: list[str],
    github_assets: list[str],
    pypi_files: list[str],
    npm_versions: list[str],
) -> list[str]:
    """Pure completeness check. Returns one failure line per gap."""
    failures = []
    github_present = set(github_assets)
    for asset in expected_github_assets(tag, triples):
        if asset not in github_present:
            failures.append(f"github-release missing asset: {asset}")
    pypi_present = set(pypi_files)
    for wheel in expected_pypi_files(tag, triples):
        if wheel not in pypi_present:
            failures.append(f"pypi missing file: {wheel}")
    version = tag.lstrip("v")
    if version not in npm_versions:
        failures.append(f"npm {NPM_PACKAGE} missing version: {version}")
    return failures


def publishable_crates(root: Path = REPO_ROOT) -> list[str]:
    """Workspace member crates that crates.io would accept (soldr#3343).

    A member is publishable unless its manifest says `publish = false` (or an
    empty registry list). Detected from the manifests, never hand-listed, so
    a crate that drops `publish = false` immediately joins the release gate.
    """
    workspace = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    names = []
    for member in workspace.get("workspace", {}).get("members", []):
        manifest = tomllib.loads(
            (root / member / "Cargo.toml").read_text(encoding="utf-8")
        )
        package = manifest.get("package", {})
        if package.get("publish", True) in (False, []):
            continue
        names.append(package["name"])
    return sorted(names)


def crates_io_plan(crates: list[str]) -> str:
    """The crates.io line of the release summary: what is gated, or why not."""
    if not crates:
        return "crates.io: skipped -- every workspace crate is `publish = false`"
    return f"crates.io: gated on {', '.join(crates)}"


def crates_io_failures(version: str, published: dict[str, list[str]]) -> list[str]:
    """One failure per publishable crate whose `version` is not on crates.io."""
    return [
        f"crates.io {crate} missing version: {version}"
        for crate, versions in sorted(published.items())
        if version not in versions
    ]


def fetch_crate_versions(crate: str) -> list[str]:
    data = fetch_json(
        f"https://crates.io/api/v1/crates/{crate}",
        {"User-Agent": f"{GITHUB_REPO} release-completeness"},
    )
    return [entry["num"] for entry in data.get("versions", [])]


def fetch_json(url: str, headers: dict[str, str] | None = None) -> dict:
    request = urllib.request.Request(url, headers=headers or {})
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.loads(response.read().decode("utf-8"))


def fetch_github_assets(tag: str) -> list[str]:
    headers = {"Accept": "application/vnd.github+json"}
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    data = fetch_json(
        f"https://api.github.com/repos/{GITHUB_REPO}/releases/tags/{tag}",
        headers,
    )
    return [asset["name"] for asset in data.get("assets", [])]


def fetch_pypi_files(version: str) -> list[str]:
    data = fetch_json(f"https://pypi.org/pypi/{PYPI_PROJECT}/{version}/json")
    return [entry["filename"] for entry in data.get("urls", [])]


def fetch_npm_versions() -> list[str]:
    quoted = NPM_PACKAGE.replace("/", "%2F")
    data = fetch_json(f"https://registry.npmjs.org/{quoted}")
    return list(data.get("versions", {}).keys())


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", help="release tag, e.g. v0.9.1")
    parser.add_argument(
        "--list-expected-github-assets",
        action="store_true",
        help="print the contract-derived GitHub asset names (one per line) "
        "and exit without any network access — the single source the "
        "workflow's inline asset lists were replaced by (soldr#2469 "
        "step 2.2)",
    )
    parser.add_argument(
        "--build-matrix",
        action="store_true",
        help="print the contract-derived release build matrix as a JSON "
        "array for `strategy.matrix.include`, no network access "
        "(soldr#2469 step 2.1)",
    )
    parser.add_argument(
        "--verify-execution-contract",
        action="store_true",
        help="fail when a release-blocking target still has no execution job",
    )
    opts = parser.parse_args(argv)
    if opts.build_matrix:
        print(
            json.dumps([asdict(row) for row in build_matrix()], separators=(",", ":"))
        )
        return 0
    if opts.verify_execution_contract:
        failures = release_execution_failures()
        if failures:
            print("release execution contract is BLOCKED:", file=sys.stderr)
            for failure in failures:
                print(f"  - {failure}", file=sys.stderr)
            return 1
        print("release execution contract complete")
        return 0
    if not opts.version:
        parser.error("--version is required except with --build-matrix")
    tag = opts.version if opts.version.startswith("v") else f"v{opts.version}"

    triples = included_triples()
    if opts.list_expected_github_assets:
        for asset in expected_github_assets(tag, triples):
            print(asset)
        return 0
    failures = verify_surfaces(
        tag,
        triples,
        fetch_github_assets(tag),
        fetch_pypi_files(tag.lstrip("v")),
        fetch_npm_versions(),
    )
    crates = publishable_crates()
    print(crates_io_plan(crates))
    failures += crates_io_failures(
        tag.lstrip("v"), {crate: fetch_crate_versions(crate) for crate in crates}
    )
    if failures:
        print(f"release {tag} is INCOMPLETE ({len(failures)} gaps):", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        print(
            "A green run must mean a complete public release surface "
            "(soldr#2469 step 1.1). Recovery-only dispatches must be named "
            "and reported as recovery, never as a normal release result.",
            file=sys.stderr,
        )
        return 1
    total = len(expected_github_assets(tag, triples))
    print(
        f"release {tag} complete: {total} GitHub assets, "
        f"{len(expected_pypi_files(tag, triples))} PyPI wheels, npm published"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
