"""Tests for the `--release` CI policy check (soldr#1981)."""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

SCRIPT = (
    Path(__file__).resolve().parents[1]
    / ".github"
    / "scripts"
    / "verify_release_profile_policy.py"
)


@pytest.fixture(scope="module")
def mod():
    return load_script_module(SCRIPT, "verify_release_profile_policy")


def _write(dir_: Path, name: str, body: str) -> None:
    (dir_ / name).write_text(body, encoding="utf-8")


def test_a_cheap_profile_passes(mod, tmp_path):
    _write(
        tmp_path,
        "cheap.yml",
        "jobs:\n  a:\n    run: cargo build --profile ci-release\n",
    )
    assert mod.scan(tmp_path) == []


def test_release_in_a_non_allowlisted_workflow_fails(mod, tmp_path):
    _write(
        tmp_path,
        "greedy.yml",
        "jobs:\n  a:\n    run: cargo build --release -p soldr-cli\n",
    )
    findings = mod.scan(tmp_path)
    assert len(findings) == 1
    finding = findings[0]
    assert finding.file == "greedy.yml"
    assert finding.line_number == 3
    assert "--release" in finding.line


def test_an_allowlisted_workflow_is_skipped(mod, tmp_path):
    name = next(iter(mod.ALLOWLIST))
    _write(tmp_path, name, "jobs:\n  a:\n    run: cargo build --release\n")
    assert mod.scan(tmp_path) == []


# `_ci-cross-build-linux.yml` carries a comment recording that Stage B moved
# *off* `--release`. A naive scan flags exactly that line, which would make the
# check punish the very cleanup it exists to protect.
def test_a_comment_about_release_is_not_a_violation(mod, tmp_path):
    _write(
        tmp_path,
        "documented.yml",
        "jobs:\n  a:\n    # after Stage B switched from --release to --profile ci-release:\n"
        "    run: cargo build --profile ci-release\n",
    )
    assert mod.scan(tmp_path) == []


# `--release-notes` and similar must not trip the word match.
def test_a_longer_flag_containing_release_is_not_a_violation(mod, tmp_path):
    _write(
        tmp_path,
        "other.yml",
        "jobs:\n  a:\n    run: gh release create --release-notes x\n",
    )
    assert mod.scan(tmp_path) == []


def test_release_before_a_line_continuation_is_caught(mod, tmp_path):
    _write(
        tmp_path,
        "wrapped.yml",
        "jobs:\n  a:\n    run: |\n      cargo build \\\n        --release \\\n       \n",
    )
    findings = mod.scan(tmp_path)
    assert len(findings) == 1, f"a wrapped invocation must still be caught: {findings}"


# An exemption that outlives its reason silently re-permits what the policy
# forbids. soldr#1982 deleted a workflow outright, which is how this happens.
def test_an_allowlist_entry_for_a_missing_workflow_is_reported(mod, tmp_path):
    stale = mod.unused_allowlist_entries(tmp_path)
    assert stale, "every allowlisted name is absent here, so all should be stale"
    assert all("no such workflow" in entry for entry in stale)


def test_an_allowlist_entry_that_no_longer_uses_release_is_reported(mod, tmp_path):
    name = next(iter(mod.ALLOWLIST))
    _write(tmp_path, name, "jobs:\n  a:\n    run: cargo build --profile ci-release\n")
    stale = mod.unused_allowlist_entries(tmp_path)
    assert f"{name} (no longer uses --release)" in stale


def test_every_allowlist_entry_carries_a_reason(mod):
    """The allowlist is reason-bearing by design; a bare name teaches nothing."""
    for name, reason in mod.ALLOWLIST.items():
        assert reason.strip(), f"{name} needs a reason"
        assert len(reason) > 20, (
            f"{name}'s reason is too terse to be useful: {reason!r}"
        )


# soldr#2139 lane: a per-line, reason-bearing opt-out, so permitting one
# `--release` in a large multi-purpose workflow does not exempt the other
# ~1,000 lines the way an ALLOWLIST entry would.
def test_a_trailing_allow_marker_excuses_the_line(mod, tmp_path):
    _write(
        tmp_path,
        "marked.yml",
        "jobs:\n  a:\n    run: cargo build --release  "
        "# allow-release: the release profile is the thing under test here\n",
    )
    assert mod.scan(tmp_path) == []


def test_an_allow_marker_on_the_preceding_line_excuses_it(mod, tmp_path):
    # A wrapped shell invocation has nowhere to put a trailing comment.
    _write(
        tmp_path,
        "wrapped-marked.yml",
        "jobs:\n  a:\n    run: |\n"
        "      # allow-release: the release profile is the thing under test here\n"
        "      cargo build \\\n        --release \\\n       \n",
    )
    assert mod.scan(tmp_path) == []


def test_a_reasonless_allow_marker_is_not_an_exemption(mod, tmp_path):
    _write(
        tmp_path,
        "bare.yml",
        "jobs:\n  a:\n    run: cargo build --release  # allow-release: why\n",
    )
    assert len(mod.scan(tmp_path)) == 1, "a one-word reason teaches nothing"


def test_an_allow_marker_two_lines_up_does_not_reach(mod, tmp_path):
    _write(
        tmp_path,
        "distant.yml",
        "jobs:\n  a:\n    run: |\n"
        "      # allow-release: the release profile is the thing under test here\n"
        "      echo unrelated\n"
        "      cargo build --release\n",
    )
    assert len(mod.scan(tmp_path)) == 1, "the marker must sit on or above the line"


# soldr#2303: `--profile release` is the same expensive profile spelled the long
# way, and it used to slip past the `--release`-only regex.
def test_profile_release_is_flagged(mod, tmp_path):
    _write(
        tmp_path,
        "long.yml",
        "jobs:\n  a:\n    run: cargo build --profile release -p soldr-cli\n",
    )
    findings = mod.scan(tmp_path)
    assert len(findings) == 1, f"--profile release must be caught: {findings}"
    assert findings[0].line_number == 3


def test_profile_equals_release_is_flagged(mod, tmp_path):
    _write(
        tmp_path,
        "eq.yml",
        "jobs:\n  a:\n    run: cargo build --profile=release\n",
    )
    assert len(mod.scan(tmp_path)) == 1


@pytest.mark.parametrize("profile", ["ci-release", "ci-bootstrap", "ci-nextest"])
def test_cheap_named_profiles_are_not_flagged(mod, tmp_path, profile):
    # The `release` profile *name* is the target; profiles whose names merely
    # end in `-release` (or start with `ci-`) must not false-positive.
    _write(
        tmp_path,
        "cheap.yml",
        f"jobs:\n  a:\n    run: cargo build --profile {profile}\n",
    )
    assert mod.scan(tmp_path) == [], f"--profile {profile} must pass"


def test_a_marker_excuses_a_profile_release_line(mod, tmp_path):
    _write(
        tmp_path,
        "marked.yml",
        "jobs:\n  a:\n    run: cargo build --profile release  "
        "# allow-release: dylint loads these cdylibs from the release-profile path\n",
    )
    assert mod.scan(tmp_path) == []


def test_the_real_repository_is_clean(mod):
    """The policy must hold on the tree that ships it."""
    workflows = Path(__file__).resolve().parents[1] / ".github" / "workflows"
    if not workflows.is_dir():
        pytest.skip("workflows directory absent")
    assert mod.scan(workflows) == []
    assert mod.unused_allowlist_entries(workflows) == []
