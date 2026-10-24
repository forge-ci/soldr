#!/usr/bin/env python3
"""Every workflow job that runs a repo Python script must pin its interpreter.

soldr#2763. The v0.9.3 release died on macOS ARM64 with::

    AttributeError: 'PosixPath' object has no attribute 'hardlink_to'

`Path.hardlink_to` is 3.10+. The job ran `.github/scripts/*.py` under whatever
`python3` the runner image happened to ship, and that image's was older. The
call site had been extracted from inline YAML one PR earlier, so this was its
first execution on that lane -- the interpreter floor had been a per-file
convention all along, and nothing was checking it.

An audit at the time found a second landmine already in place
(`validate_npm_release_recovery.py` importing `tomllib`, 3.11+, unguarded), which
is what makes this a class rather than an incident: fixing the one API that blew
up means re-dispatching the release and dying at the next one a cycle later.

## What is checked

For each job in `.github/workflows/*.yml`: if any step's `run:` invokes a script
under `.github/scripts/` or `ci/`, the job must pin the interpreter that runs
it. See `job_pins_interpreter` for what counts -- in particular, setting up uv
and then calling bare `python3` does *not*, which is a distinction two jobs in
this repo were already on the wrong side of.

## Why a ratchet, and not a threshold

Eighteen jobs are unpinned today. soldr#2763 converts them in slices; the
    four acceptance lanes left this list together because they share a
    shape: one stdlib-only repo script, invoked once, with no other Python
    in the job. Failing all
of them at once would block every PR on a sweep through lanes that cannot be
validated locally, so those jobs are baselined in `BASELINE` below: they may
stay as they are, but no *new* unpinned job may appear. Same bargain
`loc_ratchet.py` strikes, and for the same reason -- "don't make it worse" beats
"fix it before you may proceed" when the fix is a wide, hard-to-test sweep.

The baseline fails in **both** directions, like `check_dependency_inventory.py`:
a job that gets pinned must be removed from the list. Otherwise the list rots
into a permanent record of a problem that was quietly solved, and the next
reader cannot tell which entries are real.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys
from dataclasses import dataclass

import yaml


@dataclass(frozen=True, order=True)
class JobRef:
    """One workflow job, named by its file and job id."""

    workflow: str
    job: str


REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
WORKFLOW_DIR = REPO_ROOT / ".github" / "workflows"

# A `run:` mentioning a path under these directories is running repo Python.
SCRIPT_PATTERN = re.compile(r"(?:\.github/scripts|ci)/[\w./-]+\.py")

# `actions/setup-python` prepends its chosen interpreter to PATH, so a bare
# `python3` in a later step resolves to it. `astral-sh/setup-uv` does not: it
# installs uv and leaves `python3` meaning whatever the image ships. So uv pins
# only the invocations actually routed through it -- decided per invocation by
# `unrouted_script_invocations` below, not by counting.
SETUP_PYTHON_PATTERN = re.compile(r"actions/setup-python")
SETUP_UV_PATTERN = re.compile(r"astral-sh/setup-uv")
UV_RUN_PATTERN = re.compile(r"\buv\s+run\b")
UV_PYTHON_313_PATTERN = re.compile(r"--python(?:=|\s+)3\.13(?=\s|$)")
UV_NO_PROJECT_PATTERN = re.compile(r"--no-project(?=\s|$)")
RELEASE_WORKFLOW = "auto-release.yml"

# Repo scripts that run under a runner image's own `python3` on purpose: the
# fast pre-check (ci-pre.yml, zackees/ci.yml#6) may install nothing. Each is
# standard-library only and refuses to run below its declared
# `STDLIB_PYTHON_FLOOR` (tests/test_ci_pre_workflow.py checks both), so the
# interpreter floor is enforced by the script instead of by an install step.
# That counts as pinned only on a versioned runner label, whose image ships a
# known python3 (ubuntu-24.04: 3.12), never on a floating `-latest` label.
STDLIB_SCRIPTS: frozenset[str] = frozenset({".github/scripts/check_cache_budget.py"})
VERSIONED_UBUNTU = re.compile(r"^ubuntu-[0-9]{2}\.[0-9]{2}(?:-arm)?$")

# Jobs running repo Python under an unpinned interpreter as of soldr#2763.
# Entries are `JobRef(workflow file, job id)`. Shrink this list; never grow it.
BASELINE: frozenset[JobRef] = frozenset(
    {
        JobRef("_bootstrap-e2e.yml", "bootstrap-e2e"),
        JobRef("_build-and-test.yml", "build-and-test"),
        JobRef("_ci-cross-build-linux.yml", "cross-build"),
        JobRef("baseline-zero-deps.yml", "build-soldr"),
        JobRef("baseline-zero-deps.yml", "docker-baseline"),
        JobRef("benchmark-stats.yml", "gate"),
        JobRef("cross-compile-all-targets.yml", "bootstrap-and-linux-x86"),
        JobRef("cross-compile-all-targets.yml", "cross-compile"),
        JobRef("docker-linux-cross-smoke.yml", "smoke"),
        JobRef("perf-matrix.yml", "gate"),
    }
)


# Shell line continuations, so a `run:` block that wraps a long command reads
# the same to this guard as one that does not.
CONTINUATION_PATTERN = re.compile(r"\\s*\n\s*")


def shell_command_prefix(line: str, script_start: int) -> str:  # noqa: C901
    """Return the shell command segment immediately before a script path.

    A prior ``uv run`` on a compound shell line cannot route a later command:
    ``uv run ... ci/one.py && python3 ci/two.py`` still executes the second
    script under the runner's Python. Keep quote-aware handling here so a
    separator in an argument is not mistaken for shell syntax.
    """
    command_start = 0
    quote: str | None = None
    escaped = False
    index = 0
    while index < script_start:
        char = line[index]
        if escaped:
            escaped = False
        elif char == "\\":
            escaped = True
        elif quote:
            if char == quote:
                quote = None
        elif char in {"'", '"'}:
            quote = char
        elif char == ";":
            command_start = index + 1
        elif char == "|":
            # `|`, `||`, and `|&` all start a fresh shell command. The latter
            # pipes stderr too; it is still a pipeline, not a redirection.
            command_start = index + 1
            if line[index + 1 : index + 2] in {"|", "&"}:
                command_start = index + 2
                index += 1
        elif char == "&":
            next_char = line[index + 1 : index + 2]
            previous_char = line[index - 1 : index]
            # `2>&1`, `>&1`, and `&> log` are redirections, not a command
            # boundary. Every other ampersand is either `&&` or a background
            # command separator and must not let its left command certify the
            # next script.
            if previous_char not in {">", "<"} and next_char != ">":
                command_start = index + 1
                if next_char == "&":
                    command_start = index + 2
                    index += 1
        index += 1
    return line[command_start:script_start]


def unrouted_script_invocations(runs: str) -> list[str]:
    """Repo-script invocations in `runs` that are not routed through `uv run`.

    Per invocation, not by counting. The count comparison this replaces was
    wrong in both directions:

    * **False negative.** A job with one `uv run` for something else (a pytest
      call, say) and one bare `python3 .github/scripts/x.py` had one of each,
      compared equal, and was reported pinned -- which is precisely the state
      the module docstring says the guard exists to catch.
    * **False positive.** A routed call split across lines with a trailing
      backslash did not match a single-line pattern, so wrapping a long
      command for readability made a correctly pinned job report as unpinned.

    Continuations are folded first, then each shell command carrying a script
    path must have `uv run` ahead of it in that same command segment.
    """
    joined = CONTINUATION_PATTERN.sub(" ", runs)
    unrouted = []
    for line in joined.splitlines():
        for match in SCRIPT_PATTERN.finditer(line):
            if "uv run" not in shell_command_prefix(line, match.start()):
                unrouted.append(line.strip())
                break
    return unrouted


def uv_script_invocations_missing_option(
    runs: str, option: re.Pattern[str]
) -> list[str]:
    """Return routed repo-script lines missing an option on their own ``uv run``.

    The option must occur after the last ``uv run`` before a script path. This
    keeps an earlier, unrelated ``uv run --python 3.13`` from certifying a
    later invocation, just as ``unrouted_script_invocations`` keeps it from
    certifying a bare Python call.
    """
    joined = CONTINUATION_PATTERN.sub(" ", runs)
    missing = []
    for line in joined.splitlines():
        for match in SCRIPT_PATTERN.finditer(line):
            prefix = shell_command_prefix(line, match.start())
            invocations = list(UV_RUN_PATTERN.finditer(prefix))
            if not invocations:
                continue
            options = prefix[invocations[-1].end() :]
            if not option.search(options):
                missing.append(line.strip())
                break
    return missing


def job_run_text(job: dict) -> str:
    """Every `run:` body in the job, joined."""
    return "\n".join(
        step.get("run") or ""
        for step in job.get("steps") or []
        if isinstance(step, dict)
    )


def job_uses_text(job: dict) -> str:
    """Every `uses:` reference in the job, joined."""
    return "\n".join(
        step.get("uses") or ""
        for step in job.get("steps") or []
        if isinstance(step, dict)
    )


def job_runs_repo_python(job: dict) -> bool:
    """Does any step in `job` invoke a script under `.github/scripts/` or `ci/`?"""
    return bool(SCRIPT_PATTERN.search(job_run_text(job)))


def stdlib_only_job(job: dict) -> bool:
    """Every repo script is in `STDLIB_SCRIPTS`, on a versioned ubuntu image."""
    runs_on = job.get("runs-on")
    if not isinstance(runs_on, str) or not VERSIONED_UBUNTU.fullmatch(runs_on):
        return False
    scripts = set(SCRIPT_PATTERN.findall(job_run_text(job)))
    return bool(scripts) and scripts <= STDLIB_SCRIPTS


def job_pins_interpreter(job: dict, *, release_job: bool = False) -> bool:
    """Does `job` fix which Python runs its repo scripts?

    Three ways, and the third is the one that is easy to get wrong:

    * `container:` -- the image is pinned in the workflow, so the interpreter
      cannot vary with the runner image.
    * `actions/setup-python` -- prepends its interpreter to PATH, so a later
      bare `python3` resolves to the pinned one.
    * `astral-sh/setup-uv` -- **only for invocations routed through
      `uv run --python 3.13`**.
      Installing uv does not change what `python3` means, so a job that sets up
      uv and then runs `python3 script.py` is exactly as exposed as one that set
    up nothing at all. Two jobs in this repo were in that state when the guard
    was written, and a check keyed on the setup step alone called them safe.
    Release jobs must additionally use `--no-project`; their wheel/release
    environment must not be influenced by a checked-out project.
    """
    if job.get("container") and not release_job:
        return True
    if not release_job and stdlib_only_job(job):
        return True
    uses = job_uses_text(job)
    if SETUP_PYTHON_PATTERN.search(uses) and not release_job:
        return True
    if not SETUP_UV_PATTERN.search(uses):
        return False
    runs = job_run_text(job)
    # Every repo-script invocation must go through `uv run`, not merely one.
    if unrouted_script_invocations(runs):
        return False
    # Pin each uv-managed interpreter explicitly. `uv run` otherwise chooses
    # an available interpreter, which is deliberately not a release contract.
    if uv_script_invocations_missing_option(runs, UV_PYTHON_313_PATTERN):
        return False
    # Release auto is the source of soldr#2763: it must be self-contained and
    # never inherit a repository project/environment by accident. Keep this
    # policy here, beside the general workflow guard, rather than in a second
    # release-only test with a divergent parser and job list.
    if release_job and uv_script_invocations_missing_option(
        runs, UV_NO_PROJECT_PATTERN
    ):
        return False
    # ...and uv must exist before the first step that uses it.
    #
    # soldr#2763: this check was missing, and the guard passed a Lint job whose
    # first `uv run` was step 1 while `setup-uv` was step 8 -- every one of
    # those steps would have died on `uv: command not found`. A guard that
    # reports a job pinned when it cannot run at all is worse than no guard.
    return uv_is_installed_before_use(job)


def uv_is_installed_before_use(job: dict) -> bool:
    """Does `astral-sh/setup-uv` run before the first step invoking `uv`?

    Ordering is invisible to a "does this job set up uv" check and fatal at
    runtime: `uv run` in a step before the setup fails with
    `uv: command not found`, which is how soldr#2800's first attempt broke.
    """
    setup_at: int | None = None
    first_use: int | None = None
    for index, step in enumerate(job.get("steps") or []):
        if not isinstance(step, dict):
            continue
        if setup_at is None and SETUP_UV_PATTERN.search(step.get("uses") or ""):
            setup_at = index
        if first_use is None and "uv run" in (step.get("run") or ""):
            first_use = index
    if first_use is None:
        return True
    return setup_at is not None and setup_at < first_use


def unpinned_jobs(workflow_dir: pathlib.Path) -> set[JobRef]:
    """Every `(file, job)` that runs repo Python without pinning an interpreter."""
    found: set[JobRef] = set()
    paths = sorted(workflow_dir.glob("*.yml")) + sorted(workflow_dir.glob("*.yaml"))
    for path in paths:
        document = yaml.safe_load(path.read_text(encoding="utf-8"))
        if not isinstance(document, dict):
            continue
        for job_id, job in (document.get("jobs") or {}).items():
            if not isinstance(job, dict):
                continue
            if job_runs_repo_python(job) and not job_pins_interpreter(
                job, release_job=path.name == RELEASE_WORKFLOW
            ):
                found.add(JobRef(path.name, str(job_id)))
    return found


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--workflow-dir",
        type=pathlib.Path,
        default=WORKFLOW_DIR,
        help="directory of workflow YAML to check (default: .github/workflows)",
    )
    args = parser.parse_args()

    found = unpinned_jobs(args.workflow_dir)
    added = sorted(found - BASELINE)
    fixed = sorted(BASELINE - found)

    if added:
        print("error: workflow job runs repo Python under an unpinned interpreter:")
        for ref in added:
            print(f"  {ref.workflow}: job '{ref.job}'")
        print()
        print(
            "Pin it: add `actions/setup-python`, or add `astral-sh/setup-uv` and\n"
            "invoke the script as `uv run --python 3.13 <script>`. Setting up\n"
            "uv without routing the call through `uv run` does not count -- it\n"
            "leaves `python3` meaning whatever the runner image ships, which\n"
            "differs per platform and drifts without notice (soldr#2763)."
        )
    if fixed:
        if added:
            print()
        print("error: these jobs now pin an interpreter and must leave the baseline:")
        for ref in fixed:
            print(f"  {ref.workflow}: job '{ref.job}'")
        print()
        print(
            "Remove them from BASELINE in this script. The list is a record of\n"
            "work still to do; entries that are done make it unreadable."
        )
    if added or fixed:
        return 1

    print(
        f"check_workflow_python_pin: no unpinned jobs beyond the "
        f"{len(BASELINE)} baselined."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
