"""The interpreter-pin guard must catch an unpinned job, and only that (soldr#2763).

The bug it guards: a workflow job runs `.github/scripts/*.py` under whatever
`python3` the runner image happens to ship. That killed the v0.9.3 release on
macOS ARM64 with `'PosixPath' object has no attribute 'hardlink_to'` -- a 3.10+
API on an image whose python3 was older.

Two directions matter here and they fail differently. Missing a genuinely
unpinned job lets the release bug back in. Flagging a pinned one -- or a job
that runs no repo Python at all -- makes the guard noise, and a noisy guard gets
baselined into uselessness, which is the same outcome more slowly.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

SCRIPT = (
    Path(__file__).resolve().parents[1]
    / ".github"
    / "scripts"
    / "check_workflow_python_pin.py"
)

SETUP_UV = "astral-sh/setup-uv@e58605a9b6da7c637471fab8847a5e5a6b8df081 # v5"
SETUP_PYTHON = "actions/setup-python@a309ff8b426b58ec0e2a45f0f869d46889d02405 # v6.2.0"


@pytest.fixture(scope="module")
def guard():
    return load_script_module(SCRIPT, "check_workflow_python_pin")


def write_workflow(directory: Path, name: str, body: str) -> Path:
    path = directory / name
    path.write_text(body, encoding="utf-8")
    return path


# --------------------------- catching the real thing ---------------------------


def test_unpinned_job_running_a_repo_script_is_flagged(guard, tmp_path):
    write_workflow(
        tmp_path,
        "release.yml",
        """
jobs:
  build:
    runs-on: macos-14
    steps:
      - run: python3 .github/scripts/stage_release_binaries.py --out dist
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {guard.JobRef("release.yml", "build")}


def test_ci_directory_scripts_count_too(guard, tmp_path):
    """`ci/*.py` runs on the same runners and has the same floor."""
    write_workflow(
        tmp_path,
        "perf.yml",
        """
jobs:
  gate:
    runs-on: ubuntu-24.04
    steps:
      - run: python3 ci/perf_local.py cargo build
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {guard.JobRef("perf.yml", "gate")}


def test_every_unpinned_job_in_a_file_is_reported(guard, tmp_path):
    write_workflow(
        tmp_path,
        "many.yml",
        """
jobs:
  first:
    steps:
      - run: python3 ci/one.py
  second:
    steps:
      - run: python3 .github/scripts/two.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {
        guard.JobRef("many.yml", "first"),
        guard.JobRef("many.yml", "second"),
    }


# ------------------------------ not over-firing --------------------------------


def test_setup_python_pins_a_bare_python3_call(guard, tmp_path):
    """setup-python prepends its interpreter to PATH, so `python3` resolves to it."""
    write_workflow(
        tmp_path,
        "release.yml",
        f"""
jobs:
  build:
    steps:
      - uses: {SETUP_PYTHON}
      - run: python3 .github/scripts/stage_release_binaries.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == set()


def test_setup_uv_pins_an_invocation_routed_through_uv_run(guard, tmp_path):
    write_workflow(
        tmp_path,
        "release.yml",
        f"""
jobs:
  build:
    steps:
      - uses: {SETUP_UV}
      - run: uv run --python 3.13 .github/scripts/stage_release_binaries.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == set()


def test_setup_uv_requires_an_explicit_python_313_for_a_repo_script(guard, tmp_path):
    write_workflow(
        tmp_path,
        "cook.yml",
        f"""
jobs:
  cook:
    steps:
      - uses: {SETUP_UV}
      - run: uv run --no-project python .github/scripts/prepare_cook_fixture.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {guard.JobRef("cook.yml", "cook")}


def test_release_scripts_require_setup_uv_and_no_project(guard, tmp_path):
    """The release-specific soldr#2763 contract belongs in this guard.

    `setup-python` correctly pins ordinary workflow jobs, but the release
    surface must use its isolated uv front door for every repository script.
    """
    write_workflow(
        tmp_path,
        "auto-release.yml",
        f"""
jobs:
  release:
    steps:
      - uses: {SETUP_PYTHON}
      - run: python3 .github/scripts/stage_release_binaries.py
  missing-no-project:
    steps:
      - uses: {SETUP_UV}
      - run: uv run --python 3.13 python .github/scripts/publish.py
  compliant:
    steps:
      - uses: {SETUP_UV}
      - run: uv run --no-project --python 3.13 python .github/scripts/verify.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {
        guard.JobRef("auto-release.yml", "release"),
        guard.JobRef("auto-release.yml", "missing-no-project"),
    }


def test_setup_uv_does_not_pin_a_bare_python3_call(guard, tmp_path):
    """The distinction the loose version of this guard got wrong.

    Installing uv does not change what `python3` means. A job that sets uv up
    and then calls `python3 script.py` is exactly as exposed as one that set up
    nothing -- but it *looks* pinned, which is worse, because a reader checking
    for a setup step finds one. Two jobs in this repo were in that state.
    """
    write_workflow(
        tmp_path,
        "release.yml",
        f"""
jobs:
  build:
    steps:
      - uses: {SETUP_UV}
      - run: python3 .github/scripts/stage_release_binaries.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {guard.JobRef("release.yml", "build")}


def test_one_uv_run_does_not_cover_a_sibling_bare_call(guard, tmp_path):
    """Partial adoption is the state a half-finished migration leaves behind."""
    write_workflow(
        tmp_path,
        "release.yml",
        f"""
jobs:
  build:
    steps:
      - uses: {SETUP_UV}
      - run: uv run --python 3.13 .github/scripts/one.py
      - run: python3 .github/scripts/two.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {guard.JobRef("release.yml", "build")}


@pytest.mark.parametrize("separator", ["&&", ";", "|", "&"])
def test_shell_command_boundaries_bind_each_script_to_its_own_uv_run(
    guard, tmp_path, separator
):
    """A routed command cannot certify a later bare command on the same line."""
    write_workflow(
        tmp_path,
        "release.yml",
        f"""
jobs:
  routed:
    steps:
      - uses: {SETUP_UV}
      - run: uv run --python 3.13 ci/one.py {separator} uv run --python 3.13 ci/two.py
  bare:
    steps:
      - uses: {SETUP_UV}
      - run: uv run --python 3.13 ci/one.py {separator} python3 ci/two.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {guard.JobRef("release.yml", "bare")}


def test_redirection_is_not_a_shell_command_boundary(guard, tmp_path):
    write_workflow(
        tmp_path,
        "release.yml",
        f"""
jobs:
  redirected:
    steps:
      - uses: {SETUP_UV}
      - run: 2>&1 uv run --python 3.13 ci/one.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == set()


@pytest.mark.parametrize("separator", ["&&", ";"])
def test_release_flags_do_not_cross_shell_command_boundaries(
    guard, tmp_path, separator
):
    """A release script must carry its own Python and isolation flags."""
    write_workflow(
        tmp_path,
        "auto-release.yml",
        f"""
jobs:
  compliant:
    steps:
      - uses: {SETUP_UV}
      - run: uv run --no-project --python 3.13 ci/one.py {separator} uv run --no-project --python 3.13 ci/two.py
  inherited-flags:
    steps:
      - uses: {SETUP_UV}
      - run: uv run --no-project --python 3.13 ci/one.py {separator} uv run python ci/two.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {
        guard.JobRef("auto-release.yml", "inherited-flags")
    }


def test_a_container_job_is_not_flagged(guard, tmp_path):
    """The image is pinned in the workflow, so the interpreter cannot drift."""
    write_workflow(
        tmp_path,
        "cross.yml",
        """
jobs:
  build:
    container:
      image: ghcr.io/zackees/soldr-cross:pinned
    steps:
      - run: python3 ci/build.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == set()


def test_a_job_running_no_repo_python_is_ignored(guard, tmp_path):
    """Including one that runs Python -- the floor only binds on repo scripts."""
    write_workflow(
        tmp_path,
        "build.yml",
        """
jobs:
  build:
    steps:
      - run: cargo build --workspace
      - run: python3 -c "print('inline is not a repo script')"
      - run: python3 /usr/share/tool/vendored.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == set()


def test_pinning_one_job_does_not_excuse_its_neighbour(guard, tmp_path):
    """A setup step is per job; a sibling job on another runner gets nothing."""
    write_workflow(
        tmp_path,
        "two.yml",
        f"""
jobs:
  pinned:
    steps:
      - uses: {SETUP_PYTHON}
      - run: python3 ci/a.py
  unpinned:
    steps:
      - run: python3 ci/b.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {guard.JobRef("two.yml", "unpinned")}


# ---------------------------- the baseline is honest ---------------------------


def test_baseline_matches_the_repository_exactly(guard):
    """Both directions, or the list rots.

    An unlisted unpinned job is the release bug walking back in. A listed job
    that has since been pinned makes the baseline a record of solved problems,
    and the next reader cannot tell which entries still mean anything.
    """
    found = guard.unpinned_jobs(guard.WORKFLOW_DIR)
    assert found - guard.BASELINE == set(), "unpinned job missing from BASELINE"
    assert guard.BASELINE - found == set(), "BASELINE entry is no longer unpinned"


def test_baseline_entries_name_workflows_that_exist(guard):
    """A renamed or deleted workflow must not leave a silent entry behind."""
    for workflow in {entry.workflow for entry in guard.BASELINE}:
        assert (guard.WORKFLOW_DIR / workflow).is_file(), (
            f"BASELINE names {workflow}, which does not exist"
        )


# --------------------------------- exit codes ----------------------------------


def test_main_passes_when_only_baselined_jobs_are_unpinned(guard, monkeypatch):
    # main() parses the real sys.argv, which under pytest is pytest's own.
    monkeypatch.setattr("sys.argv", ["check"])
    assert guard.main() == 0


def test_main_fails_on_a_new_unpinned_job(guard, tmp_path, monkeypatch, capsys):
    write_workflow(
        tmp_path,
        "brand-new.yml",
        """
jobs:
  fresh:
    steps:
      - run: python3 .github/scripts/whatever.py
""",
    )
    monkeypatch.setattr("sys.argv", ["check", "--workflow-dir", str(tmp_path)])
    assert guard.main() == 1
    out = capsys.readouterr().out
    assert "brand-new.yml" in out and "fresh" in out
    # The message has to say what to do; a guard that only says "no" gets
    # worked around rather than satisfied.
    assert "setup-uv" in out


def test_uv_must_be_installed_before_it_is_used(guard, tmp_path):
    """A job that sets up uv *after* using it is not pinned, it is broken.

    soldr#2763: the guard passed a Lint job whose first `uv run` was step 1
    while `setup-uv` was step 8 -- every one of those steps would have died on
    `uv: command not found`. Reporting that job as pinned is worse than not
    checking, because it certifies a job that cannot run.
    """
    write_workflow(
        tmp_path,
        "late.yml",
        f"""
jobs:
  build:
    steps:
      - run: uv run --python 3.13 python ci/first.py
      - uses: {SETUP_UV}
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {guard.JobRef("late.yml", "build")}


def test_uv_installed_before_use_is_accepted(guard, tmp_path):
    write_workflow(
        tmp_path,
        "early.yml",
        f"""
jobs:
  build:
    steps:
      - uses: {SETUP_UV}
      - run: uv run --python 3.13 python ci/first.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == set()


# ------------------- per-invocation routing, not counting -------------------
#
# soldr#2763: the predicate compared the NUMBER of routed calls against the
# number of script paths. Both directions of that were wrong, and both are
# reachable from ordinary workflow edits.


def test_a_wrapped_command_is_still_pinned(guard):
    r"""A trailing backslash is a formatting choice, not a pinning one.

    Asserted against the helper with an explicit string rather than through a
    YAML fixture: PyYAML normalizes a continuation inside a block scalar, so a
    fixture-shaped version of this test passes whether or not the folding
    exists and proves nothing. The real `setup-soldr-action.yml` reaches the
    helper with the script on its own line, which is the case that matters.
    """
    # Built by joining rather than written as one literal: the payload ends a
    # line with a backslash, which is exactly the character most likely to be
    # mangled by whatever edits this file next, and a mangled version passes
    # vacuously.
    backslash = chr(92)
    wrapped = "\n".join(
        [
            "uv run --no-project --python 3.13 python " + backslash,
            "  .github/scripts/one.py",
        ]
    )
    assert wrapped.endswith("py")
    assert backslash + "\n" in wrapped, "fixture lost its line continuation"
    assert guard.unrouted_script_invocations(wrapped) == []


def test_an_unrelated_routed_call_does_not_cover_a_bare_one(guard, tmp_path):
    """The false negative: one routed call, one bare call, counts agree.

    A job that runs pytest through the pinned interpreter and a repo script
    through whatever `python3` means had one of each. The counts compared
    equal and the job was reported pinned -- while the script it actually
    cares about ran unpinned.
    """
    write_workflow(
        tmp_path,
        "release.yml",
        f"""
jobs:
  build:
    steps:
      - uses: {SETUP_UV}
      - run: uv run --no-project --with pytest python -m pytest tests/ -q
      - run: python3 .github/scripts/one.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {guard.JobRef("release.yml", "build")}


def test_the_routing_must_precede_the_script_on_its_own_line(guard, tmp_path):
    """`uv run` later in the block does not retroactively pin an earlier call."""
    write_workflow(
        tmp_path,
        "release.yml",
        f"""
jobs:
  build:
    steps:
      - uses: {SETUP_UV}
      - run: |
          python3 .github/scripts/one.py
          uv run --python 3.13 python .github/scripts/two.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {guard.JobRef("release.yml", "build")}


def test_the_unrouted_list_names_the_offending_line(guard):
    """The helper is the diagnosis, so it has to say which call is bare."""
    unrouted = guard.unrouted_script_invocations(
        "uv run --python 3.13 python .github/scripts/ok.py\n"
        "python3 .github/scripts/bad.py\n"
    )
    assert len(unrouted) == 1
    assert "bad.py" in unrouted[0]


def test_two_scripts_on_one_routed_line_are_not_double_counted(guard):
    """One line, one verdict -- an argument that happens to be a path is not
    a second invocation."""
    assert (
        guard.unrouted_script_invocations(
            "uv run --python 3.13 python .github/scripts/a.py --plan ci/b.py\n"
        )
        == []
    )


# ---------------- stdlib-only scripts on a versioned image (ci-pre) ----------------


def test_stdlib_script_on_a_versioned_ubuntu_image_counts_as_pinned(guard, tmp_path):
    # ci-pre.yml may install nothing (zackees/ci.yml#6); check_cache_budget.py
    # is stdlib-only and enforces its own floor, and ubuntu-24.04 fixes python3.
    write_workflow(
        tmp_path,
        "ci-pre.yml",
        """
jobs:
  cache-budget:
    runs-on: ubuntu-24.04
    steps:
      - run: python3 .github/scripts/check_cache_budget.py --event-name push
""",
    )
    assert guard.unpinned_jobs(tmp_path) == set()


def test_stdlib_script_on_a_floating_image_is_still_unpinned(guard, tmp_path):
    write_workflow(
        tmp_path,
        "ci-pre.yml",
        """
jobs:
  cache-budget:
    runs-on: ubuntu-latest
    steps:
      - run: python3 .github/scripts/check_cache_budget.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {guard.JobRef("ci-pre.yml", "cache-budget")}


def test_a_non_allowlisted_script_beside_a_stdlib_one_is_unpinned(guard, tmp_path):
    write_workflow(
        tmp_path,
        "ci-pre.yml",
        """
jobs:
  cache-budget:
    runs-on: ubuntu-24.04
    steps:
      - run: python3 .github/scripts/check_cache_budget.py
      - run: python3 .github/scripts/stage_release_binaries.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {guard.JobRef("ci-pre.yml", "cache-budget")}
