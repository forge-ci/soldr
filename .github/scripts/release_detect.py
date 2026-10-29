#!/usr/bin/env python3
"""Release detection: what, if anything, this ref still needs published.

soldr#2469 step 2.2, extracted from the 136-line ``Compare candidate version
against PyPI`` block in ``auto-release.yml``. This is the decision the whole
workflow hangs off — every downstream job is gated on ``should_release`` and
the three per-surface flags — and until now it lived in inline bash that no
test could exercise. The 0.9.0 incident ran straight through it.

The rules it encodes, unchanged from the bash it replaces:

* the candidate version is ``[workspace.package].version`` in ``Cargo.toml``,
  and ``package.json`` must already agree with it (the #1024/#1025 lockstep
  trap fails here rather than in a later lane);
* a GitHub release is *complete* only when it is published (not a draft) and
  carries every asset the target contract expects — so the 0.9.0 shape, an
  immutable release missing 10 of 17 assets, is **not** complete;
* an immutable release is never republished, complete or not: GitHub refuses
  asset mutation on it (``HTTP 422``), so attempting it is the terminal wound,
  not a recovery;
* PyPI publishes when the version has no files, or when explicitly forced;
* npm publishes when the version is absent;
* the run releases at all when any one surface still needs work.

Pure functions take fetched data and return decisions, so the tests reproduce
the incident states — including the immutable-and-incomplete one — with no
network. Fetchers use stdlib urllib and one ``git ls-remote``.

Usage (CI):
    python3 .github/scripts/release_detect.py

The sibling ``release_completeness`` import resolves because Python puts a
script's own directory first on ``sys.path``; the tests do the same insert
explicitly, since loading this file by path gets no such entry.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import urllib.error
from dataclasses import dataclass, field
from pathlib import Path

from release_completeness import (
    PYPI_PROJECT,
    expected_github_assets,
    fetch_json,
    included_triples,
)

# Dependency-free capture also works when this script is invoked by absolute path.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

REPO_ROOT = Path(__file__).resolve().parents[2]
GITHUB_REPO = os.environ.get("GITHUB_REPOSITORY", "zackees/soldr")

VERSION_PATTERN = re.compile(r"^v[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$")

# Sentinel kept from the bash: distinguishes "no release object at all" from
# "a release exists and these named assets are missing". Downstream summary
# readers have been reading this exact string since the incident writeup.
NO_RELEASE = "release-not-found"


class DetectionError(RuntimeError):
    """A precondition the release cannot proceed without."""


@dataclass(frozen=True)
class GithubReleaseState:
    complete: bool = False
    immutable: bool = False
    missing_assets: str = NO_RELEASE


@dataclass(frozen=True)
class ReleaseState:
    version: str
    cargo_version: str
    npm_package_name: str
    npm_package_version: str
    tag_exists: bool
    github: GithubReleaseState
    pypi_latest: str
    pypi_file_count: int
    npm_has_version: bool
    force_pypi_publish: bool


@dataclass(frozen=True)
class Decisions:
    should_publish_github_release: bool
    should_publish_pypi: bool
    should_publish_npm: bool
    # soldr#2469 3.1: build, smoke and verify the full production matrix on
    # the exact candidate, publishing nothing.
    dry_run: bool = False
    should_release: bool = field(init=False)

    def __post_init__(self) -> None:
        object.__setattr__(
            self,
            "should_release",
            self.dry_run
            or self.should_publish_github_release
            or self.should_publish_pypi
            or self.should_publish_npm,
        )


def derive_workspace_version(cargo_toml: str) -> str:
    """First ``version = "..."`` inside ``[workspace.package]``.

    Section-scoped on purpose: ``Cargo.toml`` has many ``version`` keys, and
    the bash this replaces used a sed range for the same reason.
    """
    in_section = False
    for line in cargo_toml.splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_section = stripped == "[workspace.package]"
            continue
        if not in_section:
            continue
        match = re.match(r'^version\s*=\s*"([^"]*)"', stripped)
        if match:
            return match.group(1)
    raise DetectionError("failed to derive workspace version from Cargo.toml")


@dataclass(frozen=True)
class NpmMetadata:
    name: str
    version: str


def npm_metadata(package_json: str) -> NpmMetadata:
    data = json.loads(package_json)
    name = data.get("name") or ""
    version = data.get("version") or ""
    if not name or not version:
        raise DetectionError("failed to derive npm package metadata from package.json")
    return NpmMetadata(name=name, version=version)


def validate_candidate(cargo_version: str, npm_package_version: str) -> str:
    version = f"v{cargo_version}"
    if not VERSION_PATTERN.match(version):
        raise DetectionError("derived version must look like vX.Y.Z")
    if npm_package_version != cargo_version:
        raise DetectionError(
            f"package.json version ({npm_package_version}) must match Cargo.toml ({cargo_version})"
        )
    return version


def github_release_state(
    release_json: dict | None, expected_assets: list[str]
) -> GithubReleaseState:
    """Complete means published AND carrying every contracted asset.

    The 0.9.0 release was immutable with 10 of 17 assets; reporting that as
    complete is exactly the false-green soldr#2469 step 1.1 exists to kill.
    """
    if not release_json:
        return GithubReleaseState()
    immutable = bool(release_json.get("immutable", False))
    is_draft = bool(release_json.get("draft", False))
    present = {asset.get("name", "") for asset in release_json.get("assets", [])}
    missing = [asset for asset in expected_assets if asset not in present]
    if not is_draft and not missing:
        return GithubReleaseState(complete=True, immutable=immutable, missing_assets="")
    reasons = (["draft-release"] if is_draft else []) + missing
    return GithubReleaseState(
        complete=False, immutable=immutable, missing_assets=",".join(reasons)
    )


def decide(state: ReleaseState, *, dry_run: bool = False) -> Decisions:
    if dry_run:
        return Decisions(
            should_publish_github_release=False,
            should_publish_pypi=False,
            should_publish_npm=False,
            dry_run=True,
        )
    return Decisions(
        # Never touch an immutable release: GitHub answers asset mutation on
        # one with HTTP 422, which is how 0.9.0 became unrecoverable.
        should_publish_github_release=(
            not state.github.complete and not state.github.immutable
        ),
        should_publish_pypi=(state.pypi_file_count == 0 or state.force_pypi_publish),
        should_publish_npm=not state.npm_has_version,
    )


def render_outputs(state: ReleaseState, decisions: Decisions, commit_sha: str) -> str:
    pairs = [
        ("should_release", decisions.should_release),
        ("should_publish_github_release", decisions.should_publish_github_release),
        ("should_publish_pypi", decisions.should_publish_pypi),
        ("should_publish_npm", decisions.should_publish_npm),
        ("dry_run", decisions.dry_run),
        ("tag_exists", state.tag_exists),
        ("github_release_complete", state.github.complete),
        ("github_release_immutable", state.github.immutable),
        ("pypi_has_version", state.pypi_file_count > 0),
        ("pypi_file_count", state.pypi_file_count),
        ("npm_has_version", state.npm_has_version),
        ("version", state.version),
        ("commit_sha", commit_sha),
    ]
    lines = [f"{key}={_render(value)}" for key, value in pairs]
    return "\n".join(lines) + "\n"


def render_summary(state: ReleaseState, decisions: Decisions) -> str:
    rows = [
        ("candidate version", state.version),
        ("current PyPI latest", f"v{state.pypi_latest}"),
        ("PyPI files for candidate", state.pypi_file_count),
        (
            "npm package",
            f"{state.npm_package_name}@{state.npm_package_version}",
        ),
        ("npm has candidate", state.npm_has_version),
        ("GitHub tag exists", state.tag_exists),
        ("GitHub release complete", state.github.complete),
        ("GitHub release immutable", state.github.immutable),
        ("GitHub release missing assets", state.github.missing_assets or "none"),
        ("force PyPI publish", state.force_pypi_publish),
        ("publish GitHub release assets", decisions.should_publish_github_release),
        ("publish PyPI wheels", decisions.should_publish_pypi),
        ("publish npm package", decisions.should_publish_npm),
        ("dry run (build and smoke only)", decisions.dry_run),
    ]
    body = "\n".join(f"- {label}: `{_render(value)}`" for label, value in rows)
    return f"### Release detection\n\n{body}\n"


def _render(value: object) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


def tag_exists(version: str) -> bool:
    result = run_captured(
        ["git", "ls-remote", "--exit-code", "--tags", "origin", version],
        cwd=REPO_ROOT,
        capture_output=True,
        check=False,
    )
    return result.returncode == 0


def fetch_release_json(version: str) -> dict | None:
    headers = {"Accept": "application/vnd.github+json"}
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    try:
        return fetch_json(
            f"https://api.github.com/repos/{GITHUB_REPO}/releases/tags/{version}",
            headers,
        )
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise


@dataclass(frozen=True)
class PypiState:
    latest: str
    file_count: int


def fetch_pypi_state(cargo_version: str) -> PypiState:
    data = fetch_json(f"https://pypi.org/pypi/{PYPI_PROJECT}/json")
    latest = (data.get("info") or {}).get("version") or ""
    if not latest:
        raise DetectionError("failed to fetch latest PyPI version for soldr")
    files = (data.get("releases") or {}).get(cargo_version) or []
    return PypiState(latest=latest, file_count=len(files))


def fetch_npm_has_version(package_name: str, cargo_version: str) -> bool:
    quoted = package_name.replace("/", "%2F")
    data = fetch_json(f"https://registry.npmjs.org/{quoted}")
    return cargo_version in (data.get("versions") or {})


def truthy(value: str | None) -> bool:
    return (value or "").strip().lower() == "true"


def collect_state(force_pypi_publish: bool) -> ReleaseState:
    cargo_version = derive_workspace_version(
        (REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8")
    )
    npm = npm_metadata((REPO_ROOT / "package.json").read_text(encoding="utf-8"))
    version = validate_candidate(cargo_version, npm.version)
    expected = expected_github_assets(version, included_triples())
    pypi = fetch_pypi_state(cargo_version)
    return ReleaseState(
        version=version,
        cargo_version=cargo_version,
        npm_package_name=npm.name,
        npm_package_version=npm.version,
        tag_exists=tag_exists(version),
        github=github_release_state(fetch_release_json(version), expected),
        pypi_latest=pypi.latest,
        pypi_file_count=pypi.file_count,
        npm_has_version=fetch_npm_has_version(npm.name, cargo_version),
        force_pypi_publish=force_pypi_publish,
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--commit-sha",
        default=os.environ.get("GITHUB_SHA", ""),
        help="commit the release describes (echoed to outputs)",
    )
    args = parser.parse_args(argv)

    try:
        state = collect_state(truthy(os.environ.get("FORCE_PYPI_PUBLISH")))
    except DetectionError as error:
        print(str(error), file=sys.stderr)
        return 1

    decisions = decide(state, dry_run=truthy(os.environ.get("RELEASE_DRY_RUN")))
    outputs = render_outputs(state, decisions, args.commit_sha)
    summary = render_summary(state, decisions)

    output_path = os.environ.get("GITHUB_OUTPUT")
    if output_path:
        with open(output_path, "a", encoding="utf-8") as handle:
            handle.write(outputs)
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary_path:
        with open(summary_path, "a", encoding="utf-8") as handle:
            handle.write(summary)
    print(summary)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
