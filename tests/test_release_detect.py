"""Unit tests for extracted release detection (soldr#2469 step 2.2).

The bash this replaces decided whether a release happens at all, and no test
could reach it. These tests reproduce the 0.9.0 states — including the
immutable-and-incomplete release that made the incident unrecoverable — with
no network.
"""

from __future__ import annotations

import json
import re
import sys
from dataclasses import replace
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).parents[1]
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "auto-release.yml"
SCRIPTS = REPO_ROOT / ".github" / "scripts"

# release_detect imports its sibling release_completeness. Running the script
# normally puts .github/scripts first on sys.path; loading it by path does not.
sys.path.insert(0, str(SCRIPTS))

detect = load_script_module(
    REPO_ROOT / ".github" / "scripts" / "release_detect.py", "release_detect"
)


def make_state(**overrides: object) -> object:
    defaults = detect.ReleaseState(
        version="v0.9.2",
        cargo_version="0.9.2",
        npm_package_name="@zackees/soldr",
        npm_package_version="0.9.2",
        tag_exists=False,
        github=detect.GithubReleaseState(),
        pypi_latest="0.9.1",
        pypi_file_count=0,
        npm_has_version=False,
        force_pypi_publish=False,
    )
    return replace(defaults, **overrides)  # type: ignore[arg-type]


class TestVersionDerivation:
    def test_reads_the_workspace_package_section_only(self) -> None:
        cargo = (
            '[package]\nversion = "9.9.9"\n\n'
            '[workspace.package]\nrust-version = "1.95.0"\nversion = "0.9.2"\n\n'
            '[dependencies]\nserde = { version = "1" }\n'
        )
        assert detect.derive_workspace_version(cargo) == "0.9.2"

    def test_a_missing_section_is_an_error_not_a_guess(self) -> None:
        with pytest.raises(detect.DetectionError, match="workspace version"):
            detect.derive_workspace_version('[package]\nversion = "1.0.0"\n')

    def test_the_real_manifest_parses(self) -> None:
        version = detect.derive_workspace_version(
            (REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8")
        )
        assert re.match(r"^\d+\.\d+\.\d+", version), version


class TestCandidateValidation:
    def test_lockstep_mismatch_fails_here_not_in_a_later_lane(self) -> None:
        # The v0.7.65 trap (#1024/#1025): a bumped Cargo.toml with a stale
        # package.json used to surface as an unrelated lockfile error.
        with pytest.raises(detect.DetectionError, match="must match"):
            detect.validate_candidate("0.9.2", "0.9.1")

    def test_malformed_version_is_rejected(self) -> None:
        with pytest.raises(detect.DetectionError, match="vX.Y.Z"):
            detect.validate_candidate("not-a-version", "not-a-version")

    def test_prerelease_suffix_is_accepted(self) -> None:
        assert detect.validate_candidate("0.9.2-rc.1", "0.9.2-rc.1") == "v0.9.2-rc.1"

    def test_npm_metadata_requires_both_fields(self) -> None:
        with pytest.raises(detect.DetectionError, match="npm package metadata"):
            detect.npm_metadata(json.dumps({"name": "@zackees/soldr"}))


class TestGithubReleaseState:
    def test_absent_release_reports_the_sentinel(self) -> None:
        state = detect.github_release_state(None, ["a.tar.zst"])
        assert not state.complete
        assert not state.immutable
        assert state.missing_assets == detect.NO_RELEASE

    def test_published_release_with_every_asset_is_complete(self) -> None:
        release = {"draft": False, "assets": [{"name": "a"}, {"name": "b"}]}
        state = detect.github_release_state(release, ["a", "b"])
        assert state.complete
        assert state.missing_assets == ""

    def test_the_0_9_0_shape_is_not_complete(self) -> None:
        # Immutable, published, and missing most of its contracted assets.
        # Reporting this complete is the false-green the incident produced.
        release = {"draft": False, "immutable": True, "assets": [{"name": "a"}]}
        state = detect.github_release_state(release, ["a", "b", "c"])
        assert not state.complete
        assert state.immutable
        assert state.missing_assets == "b,c"

    def test_a_draft_is_incomplete_even_with_every_asset(self) -> None:
        release = {"draft": True, "assets": [{"name": "a"}]}
        state = detect.github_release_state(release, ["a"])
        assert not state.complete
        assert state.missing_assets == "draft-release"

    def test_draft_and_missing_assets_report_both(self) -> None:
        release = {"draft": True, "assets": []}
        state = detect.github_release_state(release, ["a"])
        assert state.missing_assets == "draft-release,a"


class TestDecisions:
    def test_an_immutable_incomplete_release_is_never_republished(self) -> None:
        # GitHub answers asset mutation on an immutable release with HTTP 422
        # (run 31484706106) — retrying is what made 0.9.0 terminal.
        state = make_state(
            github=detect.GithubReleaseState(
                complete=False, immutable=True, missing_assets="b,c"
            ),
            pypi_file_count=8,
            npm_has_version=True,
        )
        decisions = detect.decide(state)
        assert not decisions.should_publish_github_release
        assert not decisions.should_release

    def test_an_incomplete_mutable_release_is_republished(self) -> None:
        state = make_state(
            github=detect.GithubReleaseState(complete=False, immutable=False),
            pypi_file_count=8,
            npm_has_version=True,
        )
        decisions = detect.decide(state)
        assert decisions.should_publish_github_release
        assert decisions.should_release

    def test_a_fully_published_version_releases_nothing(self) -> None:
        state = make_state(
            github=detect.GithubReleaseState(complete=True, missing_assets=""),
            pypi_file_count=8,
            npm_has_version=True,
        )
        decisions = detect.decide(state)
        assert not decisions.should_release

    def test_forcing_pypi_republishes_an_existing_version(self) -> None:
        state = make_state(
            github=detect.GithubReleaseState(complete=True, missing_assets=""),
            pypi_file_count=8,
            npm_has_version=True,
            force_pypi_publish=True,
        )
        decisions = detect.decide(state)
        assert decisions.should_publish_pypi
        assert decisions.should_release

    def test_a_missing_npm_version_alone_still_releases(self) -> None:
        state = make_state(
            github=detect.GithubReleaseState(complete=True, missing_assets=""),
            pypi_file_count=8,
            npm_has_version=False,
        )
        decisions = detect.decide(state)
        assert decisions.should_publish_npm
        assert decisions.should_release


class TestRendering:
    def test_outputs_are_github_flavoured_booleans(self) -> None:
        state = make_state(tag_exists=True, pypi_file_count=3)
        rendered = detect.render_outputs(state, detect.decide(state), "deadbeef")
        assert "tag_exists=true" in rendered
        assert "github_release_complete=false" in rendered
        assert "pypi_has_version=true" in rendered
        assert "pypi_file_count=3" in rendered
        assert "version=v0.9.2" in rendered
        assert "commit_sha=deadbeef" in rendered

    def test_summary_names_the_missing_assets(self) -> None:
        state = make_state(
            github=detect.GithubReleaseState(
                complete=False, immutable=True, missing_assets="b,c"
            )
        )
        summary = detect.render_summary(state, detect.decide(state))
        assert "### Release detection" in summary
        assert "GitHub release missing assets: `b,c`" in summary
        assert "GitHub release immutable: `true`" in summary

    def test_summary_says_none_rather_than_empty(self) -> None:
        state = make_state(
            github=detect.GithubReleaseState(complete=True, missing_assets="")
        )
        summary = detect.render_summary(state, detect.decide(state))
        assert "GitHub release missing assets: `none`" in summary


def test_every_output_the_workflow_consumes_is_produced() -> None:
    """Drift guard: the extraction must not silently drop a consumer.

    A missing output is not a loud failure in Actions — ``steps.validate.
    outputs.foo`` simply evaluates to the empty string, so an ``if:`` gate
    turns falsy and a publication job silently stops running. That is the
    incident's own failure mode (skipped jobs do not fail a run), so it is
    worth a test rather than a careful reading.
    """
    workflow = WORKFLOW.read_text(encoding="utf-8")
    consumed = set(re.findall(r"steps\.validate\.outputs\.([A-Za-z0-9_]+)", workflow))
    consumed |= set(
        re.findall(r"needs\.prepare\.outputs\.([A-Za-z0-9_]+)", workflow)
    ) & set(re.findall(r"steps\.validate\.outputs\.([A-Za-z0-9_]+)", workflow))
    state = make_state()
    produced = {
        line.split("=", 1)[0]
        for line in detect.render_outputs(
            state, detect.decide(state), "sha"
        ).splitlines()
        if line
    }
    missing = sorted(consumed - produced)
    assert not missing, (
        f"auto-release.yml reads outputs the detector never writes: {missing}. "
        "An unwritten output is the empty string in Actions, which silently "
        "disables the job that gates on it."
    )


def test_a_dry_run_builds_everything_and_publishes_nothing() -> None:
    """soldr#2469 3.1: the full production matrix runs; no surface is written."""
    decisions = detect.decide(make_state(), dry_run=True)
    assert decisions.should_release and decisions.dry_run
    assert not decisions.should_publish_github_release
    assert not decisions.should_publish_pypi
    assert not decisions.should_publish_npm
    outputs = detect.render_outputs(make_state(), decisions, "0" * 40)
    assert "dry_run=true" in outputs and "should_publish_pypi=false" in outputs


def test_a_dry_run_skips_every_post_publish_verifier() -> None:
    workflow = (
        Path(__file__).parents[1] / ".github" / "workflows" / "auto-release.yml"
    ).read_text(encoding="utf-8")
    for job in (
        "verify_github_release",
        "smoke-published-dylint",
        "release-completeness",
    ):
        start = workflow.index(f"\n  {job}:")
        condition = workflow[start : workflow.index("\n    runs-on:", start)]
        assert "dry_run != 'true'" in condition, job
    contract = workflow.index("\n  release_execution_contract:")
    assert (
        "dry_run == 'true'" in workflow[contract : workflow.index("runs-on:", contract)]
    )
