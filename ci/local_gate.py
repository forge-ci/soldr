#!/usr/bin/env python3
"""soldr's local gate (zackees/ci.yml#166, GATE-001..004).

One command that is a superset of the remote quick gate, so a PR passes CI
on its first push:

    uv run --no-project --python 3.13 python ci/local_gate.py             # every lane
    uv run --no-project --python 3.13 python ci/local_gate.py --lane lint # = the remote Lint job
    uv run --no-project --python 3.13 python ci/local_gate.py --list

Do not call this directly before pushing; call it through the attesting
wrapper, which runs it on a clean tree and stamps HEAD with a tree-bound
`Local-Gate:` trailer that CI's `ci-mode` job verifies (see local-gate.toml):

    uvx --from git+https://github.com/zackees/ci.yml@<CI_LINT_REF> ci-lint local-gate run

Lanes:

- `lint` = `py-static` + `guards`: every check the remote `Lint` job runs.
  The Lint job runs exactly `ci/local_gate.py --lane lint` and nothing else
  (GATE-001), so this list is the single source of truth for it.
  `py-static` (ruff, flake8, isort, format, pylint, mypy) reads only Python;
  `guards` (repository-scanning guard scripts, the Python tests) read
  everything. They are separate lanes so the gate can cache them separately
  (GATE-007, zackees/ci.yml#177).
- `ci-lint`: `soldr lint ci` and dependency policy (deny bans, audit,
  machete) -- seconds, CI surfaces and lockfiles.
- `rust`: rustfmt, Clippy and Dylint -- ci-test's own commands, run by the
  host's soldr. Local only (the remote `build-linux-x64` job runs the same
  stages inside `soldr ci-test`).
- `cross`: Clippy and Dylint for `x86_64-pc-windows-msvc` and
  `aarch64-apple-darwin`, cross-checked from this Linux host (zackees/ci.yml
  #198 phase 2). They attest `rust/<target>/{clippy,dylint}`; no PR job
  covers those targets, so this is added coverage, not a skipped job.
  Per-target Dylint skips on proof (`ci/cross_dylint.py`, soldr#3787): a
  change outside `soldr-platform`, `Cargo.toml` and `build.rs` is covered by
  host Dylint; cross Clippy always runs.
- `wine`: Windows-MSVC unit tests of the crates that pass in full under
  Wine, cross-built here and executed in `docker/wine-test`, never on the
  host (`ci/wine_lane.py`; zackees/ci.yml#202 phase 3).
- `winvm`: the Windows-MSVC target-run partition (`_ci-target-run.yml`'s
  owned selection), built here and replayed natively in a warm local
  dockur/windows VM (`ci/winvm_lane.py`). Host-optional: with no VM it exits
  75 and local-gate records `winvm:n/a` -- not cached, not attested, and the
  gate passes (zackees/ci.yml#202, ci_lint 9687a0b).
- `tests`: soldr's own test suite, in bosn's isolated container
  (`bosn run --task test`), never on the host (GATE-005, soldr#3516).

Native *execution* (macOS, Windows, linux-arm64 target-runs) cannot run here
and stays remote; it is the residual first-push risk. Their *lints* run here
(`cross` lane).

Checks in a lane run in parallel; each one's output is shown only when it
fails -- as a bounded tail, with the complete output persisted under
`<git-dir>/local-gate/logs/<run-id>/` and its exact path printed (soldr#3564)
-- then a timing table. Exit 1 when any check fails.
"""

from __future__ import annotations

import argparse
import os
import re
import secrets
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def ci_lint_ref(ci_toml: Path = ROOT / "ci.toml") -> str:
    """Return the zackees/ci.yml SHA named by ci.toml's `linter` (soldr#3616).

    ci.toml is the ONE canonical ci-lint pin. Workflow `ref:` lines must stay
    literal YAML, so tests/test_local_gate_wiring.py keeps every copy equal.
    """
    linter = tomllib.loads(ci_toml.read_text(encoding="utf-8")).get("linter", "")
    match = re.fullmatch(r"zackees/ci\.yml@([0-9a-f]{40})", str(linter))
    if match is None:
        raise SystemExit(
            f"{ci_toml}: linter must be 'zackees/ci.yml@<40-hex sha>', got {linter!r}"
        )
    return match.group(1)


# The zackees/ci.yml commit whose ci_lint this repository uses.
CI_LINT_REF = ci_lint_ref()
# GATE-007 lanes (zackees/ci.yml#177), split along input boundaries so each
# can be cached on its own: Python linters read only Python; guards scan the
# whole repository; ci-lint is CI-surface and dependency policy (cheap);
# rust compiles the workspace; tests runs the suite in bosn.
LANES = ("py-static", "guards", "ci-lint", "rust", "cross", "wine", "winvm", "tests")
# zackees/ci.yml#198 phase 2: Clippy and Dylint for the non-Linux targets, run
# from this Linux host. No ordinary PR job lints these targets, so the
# attestation adds coverage rather than replacing a remote job (experiment X1
# found three Windows-only clippy warnings on main that way).
CROSS_TARGETS = ("x86_64-pc-windows-msvc", "aarch64-apple-darwin")
# `--lane lint` is exactly the remote Lint job (GATE-001 mirror).
LINT_ALIAS = ("py-static", "guards")
PY = ("uv", "run", "--no-project", "--python", "3.13")
PY_DIRS = (
    "src",
    "tests",
    ".github/scripts",
    ".github/actions",
    "ci",
    "tools",
    ".claude/hooks",
)
RUFF = (*PY, "--with", "ruff>=0.12,<0.13")
# Resolve imports against the source under review, ahead of an installed soldr.
PYLINT_SOURCE_ARGS = ("--init-hook", "import sys; sys.path.insert(0, 'src')")
PYLINT = (
    *PY,
    "--with",
    "pylint>=4.0,<5",
    "--with",
    "pytest>=8.0",
    "--with",
    "pyyaml>=6,<7",
)


@dataclass(frozen=True)
class Check:
    name: str
    argv: tuple[str, ...]
    lane: str
    # Diff ratchets compare against the PR base: they run locally and on
    # pull_request events, and are skipped on a push to main (nothing to diff).
    needs_base: bool = False
    # Run after every parallel check, alone: `soldr ci-test` saturates every
    # core by itself.
    exclusive: bool = False
    # Started first, so the longest checks overlap the short guards instead
    # of trailing them.
    slow: bool = False
    # Oldest version of argv[0] that runs this check correctly, read from
    # `<tool> --version`; an older tool fails fast with an upgrade hint.
    min_version: tuple[int, ...] | None = None
    # zackees/ci.yml#196 (candidate GATE-009): prove the isolated runner saw
    # THIS worktree. bosn can reuse a warm container still bound to another
    # checkout (zackees/bosn#314) -- once it ran a sibling session's tree and
    # this gate would have attested it. A fresh nonce is written to
    # NONCE_FILE; the runner must echo it back from its /repo.
    tree_nonce: bool = False
    # Host-optional (local-gate.toml `optional = true`): exit
    # NOT_APPLICABLE means "cannot run on this host" -- reported, not failed,
    # and a lane made only of such checks exits NOT_APPLICABLE itself so
    # ci-lint records it `n/a` and attests nothing for it.
    optional: bool = False


# EX_TEMPFAIL: ci-lint's "not applicable on this host" for an optional lane.
NOT_APPLICABLE = 75


def _base_ref() -> str:
    base = os.environ.get("GITHUB_BASE_REF") or "main"
    return f"origin/{base}"


def script(name: str, *extra: str) -> tuple[str, ...]:
    return (*PY, *extra, "python", f".github/scripts/{name}")


def checks() -> list[Check]:
    base = _base_ref()
    lint = [
        # Must compare against the MERGE BASE, not the base tip. A branch
        # behind main differs from the tip in files it never touched, so a
        # tip comparison attributes main's own changes to this PR -- caught
        # while testing this step, which flagged a file the PR did not touch.
        #
        # merge-base needs real history, which the default shallow
        # pull_request checkout does not have (the first version of this step
        # skipped itself into uselessness for exactly that reason), hence
        # fetch-depth: 0 on the checkout above.
        Check(
            "Line-ceiling ratchet",
            (*script("loc_ratchet.py"), "--base-ref", base),
            "guards",
            needs_base=True,
        ),
        # soldr#3388 (prevention half of meta soldr#3389) — don't-grow ratchet
        # for swallowed child-process stdio in Python/shell/workflow YAML, plus
        # the check that every `ban_swallowed_child_stdio` allow/expect carries
        # a reason. Same merge-base-ratchet shape as the line-ceiling step
        # above, reusing its `origin/$GITHUB_BASE_REF` fetched by that step.
        Check(
            "Swallowed-stdio ratchet",
            (*script("swallowed_stdio_ratchet.py"), "--base-ref", base),
            "guards",
            needs_base=True,
        ),
        # soldr#2493 — the 1000-line absolute ceiling for production sources.
        # Unlike the 1500-line ratchet above, this is a hard threshold: every
        # production file under crates/*/src must be at or under 1000 lines.
        Check(
            "1000-line production ceiling",
            script("loc_ceiling.py"),
            "guards",
            needs_base=True,
        ),
        # soldr#2753 — the `.proto` files are the documented source of truth for
        # the wire format, but soldr runs no prost-build, so nothing read them
        # and they had drifted: wire.proto referenced a message it never defined
        # (invalid protobuf), and the rust_plan mirror understated its tag space
        # by two fields. Compares (message, field, tag) triples on both sides.
        Check("Protobuf schema drift", script("check_proto_drift.py"), "guards"),
        # soldr#2752 Rule A — the manifest half of the `soldr-deps` gateway,
        # landed first because it needs no facade and no source churn. Adding a
        # third-party dependency now means amending a checked-in inventory in
        # the same PR, so the edge shows up in review instead of as one line in
        # a manifest nobody diffs. Fails in both directions, like loc_ratchet:
        # an unlisted dependency, and a listed one that is gone.
        Check(
            "Third-party dependency inventory",
            script("check_dependency_inventory.py"),
            "guards",
        ),
        # soldr#2937 (phase 5 of soldr#2931) — linked test products are never
        # cacheable. Fails when a normal workflow puts one in a cross-run store,
        # or restores a broad target/ snapshot downstream of cook.
        Check(
            "Cache ownership policy",
            script("check_cache_ownership.py", "--with", "pyyaml"),
            "guards",
        ),
        # soldr#3318: ci.yml is the only PR entry point. Structural YAML parsing
        # rejects both PR event kinds outside this file before any Rust work.
        Check(
            "Single pull-request workflow entry point",
            script("check_pr_workflow_triggers.py", "--with", "pyyaml"),
            "guards",
        ),
        # zackees/ci.yml#6: a cache saved in PR context carries `pr-<N>` in its
        # key so the ci-pre janitor can attribute and delete it.
        Check(
            "PR-context cache keys carry pr-<N>",
            script("check_pr_cache_keys.py", "--with", "pyyaml"),
            "guards",
        ),
        # soldr#2360/#2363 — the broker-fronted daemon design deliberately
        # keeps the wire at protocol_v2/client_v2, broken in place; a
        # protocol_v3/client_v3 module would mean two wire majors coexisting,
        # which defeats the minimum-version floor the design relies on to force
        # upgrades. Cheap static grep, no build required.
        Check("No protocol_v3 policy", script("no_protocol_v3.py"), "guards"),
        # soldr#1981 — `--release` costs thin-LTO + single-CU codegen, and paying
        # that on a job whose output never ships is pure waste. soldr#1982 removed
        # the offenders; this keeps them gone. Runs before the toolchain setup so
        # a regression fails in seconds rather than after a full build.
        Check(
            "Release-profile policy",
            script("verify_release_profile_policy.py"),
            "guards",
        ),
        # soldr#2442 slice 4 — tree-level raw process-spawn guard: a new raw
        # `.spawn()` site in any production crate fails here by name until it
        # is routed through a sanctioned module or allowlisted with a
        # justification. Complements the soldr-daemon dylint boundary.
        Check("Spawn-path guard", script("spawn_path_guard.py"), "guards"),
        Check(
            "Nextest bare-Cargo guard", script("check_nextest_bare_cargo.py"), "guards"
        ),
        # soldr#2763 -- the v0.9.3 release died on macOS ARM64 because a release
        # script ran under the runner image's own python3, which predated the
        # 3.10 API it used. The interpreter floor was a per-file convention that
        # nothing enforced. A ratchet, like loc_ratchet: the 23 jobs already in
        # that state are baselined, and no new one may join them.
        #
        # Routed through `uv run` because the guard needs PyYAML, and because a
        # guard about pinning interpreters should not be exempt from its own
        # rule. That is also why it sits *after* the uv setup above rather than
        # with the other guards earlier in the job -- placed before it, the step died on
        # `uv: command not found`.
        Check(
            "Workflow Python interpreter pin",
            script("check_workflow_python_pin.py", "--with", "pyyaml"),
            "guards",
        ),
        # soldr#2945 — the lint libraries are now the authority on Dylint's
        # nightly, but nothing checked that the nightly they declare has drivers
        # published. Soldr fetches a prebuilt driver and refuses to build one
        # (binary-or-exit, #2432/#2484), so re-pinning `dylints/*/rust-
        # toolchain.toml` to an undriven nightly compiles, tests, lints and
        # merges — then breaks Dylint on every host until someone runs it. This
        # asserts the exact `<version>-<nightly>` driver asset exists for every
        # release-included canonical triple, not just the CI host's. Skipped, not
        # failed, when the catalogue is unreachable — same policy as above.
        Check(
            "Dylint driver published for every shipped triple",
            script("check_dylint_driver_assets.py"),
            "guards",
        ),
        # soldr#3284: Dylint remains authoritative but expensive; this cheap
        # source-level tripwire must run even when the ci-test lane is skipped.
        Check(
            "Enforce platform cfg boundary",
            script("platform_cfg_boundary_ratchet.py"),
            "guards",
            slow=True,
        ),
        # soldr#3836: vendored zccache is not a workspace member, so the
        # ban_raw_sync_primitive Dylint never sees it; this scan holds its
        # production code to the bounded-wait `zccache::sync` locks.
        Check(
            "zccache raw-lock boundary",
            script("check_zccache_raw_locks.py"),
            "guards",
        ),
        # Rust compiler validation is centralized in the native host lane.
        # Its single `soldr ci-test` invocation (soldr#2868) leaves this job
        # responsible for non-Rust policy, Python, and JavaScript checks.
        Check(
            "Check npm package metadata",
            ("node", "scripts/test-npm-package.js"),
            "guards",
        ),
        Check(
            "Verify direct CI job timeouts",
            script("verify_ci_job_timeouts.py"),
            "guards",
        ),
        # Three lanes assert the glibc floor of a binary soldr builds, and
        # they arrived in three separate PRs with nothing tying them
        # together. `auto-release.yml` is excluded on purpose — its ceiling
        # also covers crgx / cargo-chef, which soldr fetches prebuilt at
        # 2.39, so it legitimately differs (soldr#2170).
        Check(
            "Verify the per-PR glibc ceilings agree",
            script("check_glibc_ceilings.py"),
            "guards",
        ),
        # A `paths:` filter that matches nothing leaves its workflow silently
        # dark: it never runs, so it can never go red. `crates/soldr-cli/src/
        # cache_lib/**` moved to crates/soldr-cache in #1490 Phase 4, and
        # cook-size-gate kept watching the old location.
        # soldr#3018: a `#` inside a folded `run: >` block is not a comment --
        # YAML folds it onto one line and the shell then drops every argument
        # after it. That silently disabled `--grace-seconds 2700` on the macOS
        # queue watchdog, which ran at its 900s default for as long as nobody
        # noticed, and every failure looked like the flaky lane it was watching.
        Check(
            "Verify no folded run block hides arguments behind a comment",
            script("check_run_block_comments.py"),
            "guards",
        ),
        Check(
            "Verify workflow path filters still match",
            script("verify_workflow_paths.py"),
            "guards",
        ),
        Check(
            "Local gate mirrors the remote quick gate (GATE-001/002)",
            (
                "uvx",
                "--from",
                f"git+https://github.com/zackees/ci.yml@{CI_LINT_REF}",
                "--with",
                "pyyaml",
                "ci-lint",
                "local-gate",
                "lint",
                "--repo",
                ".",
            ),
            "guards",
        ),
        # zackees/ci.yml PY-002/PY-003 (owner directives 2026-10-01): records
        # are typed dataclasses, and no subprocess output is captured through a
        # pipe (a full pipe, or a soldr daemon inheriting one, hangs the
        # caller). A ratchet: counts per file may only fall
        # (ci/py-lint-baseline.json; regenerate with --write-baseline after
        # fixing sites), and new files start at zero.
        Check(
            "Python policy ratchet (PY-002 records, PY-003 no pipe capture)",
            (
                "uvx",
                "--from",
                f"git+https://github.com/zackees/ci.yml@{CI_LINT_REF}",
                "ci-lint",
                "py",
                "lint",
                "--root",
                ".",
                "--baseline",
                "ci/py-lint-baseline.json",
            ),
            "guards",
        ),
        # Check mode, not --fix (zackees/ci.yml#166): a gate that rewrote files
        # could not attest the tree it was given (`ci-lint local-gate run`
        # refuses that), and on CI the old `--fix`/format-in-place steps hid
        # unformatted code instead of failing it -- four test files on main
        # were not ruff-formatted when this gate was introduced.
        # soldr#2102. `./lint` runs six Python tools and CI ran none of them, so
        # lint drift accumulated unnoticed for months. flake8 is the one that can
        # be enforced today: with `.flake8` delegating line length to the formatter it
        # reports 0 violations across every Python directory, so this is a
        # regression guard rather than a new policy.
        #
        # isort and mypy still report on main and are NOT wired here --
        # each needs a decision (which width, whether to fix or scope mypy's 34
        # errors). soldr#2102 tracks them. Adding a gate that fails on main would
        # block every PR for a problem it did not cause.
        # ruff was not in CI at all -- it ran only from `./lint`, locally, so
        # nothing enforced it. #2102 widened its rule selection (bugbear and
        # friends) and it passes on `src tests`, so it is enforced here now.
        #
        # `.github/scripts` and `ci` were left out at first because they had 10
        # findings, and fixing them was owed a reviewed diff rather than a
        # drive-by. That diff happened, so the scope is now all four directories
        # and the two that were unenforced are the ones most worth enforcing:
        # they are the scripts CI itself runs.
        # Bounded, like the other four. `ruff>=0.4` was open-ended, so CI
        # resolved a newer ruff than `./install` gives contributors and
        # failed on RUF043 -- a rule that did not exist in the tested
        # version. An unbounded linter is a CI break waiting on someone
        # else's release date.
        Check(
            "Python lint (ruff)",
            (*RUFF, "ruff", "check", "--no-fix", *PY_DIRS),
            "py-static",
        ),
        # Pinned, not floating. An unpinned `--with flake8` means a future
        # release that adds a check turns this red on code nobody touched,
        # and the failure would look like the PR's fault. `>=7.3,<8` follows
        # the `maturin>=1.7,<2` style already in pyproject: patches flow,
        # a major cannot land unannounced. soldr#2102 covers the wider
        # problem -- isort, pylint and mypy are installed unpinned by
        # `./install` and declared nowhere, which is why contributors format
        # differently from one another.
        Check(
            "Python lint (flake8)",
            (*PY, "--with", "flake8>=7.3,<8", "python", "-m", "flake8", *PY_DIRS),
            "py-static",
        ),
        # Ruff's formatter uses Black-compatible style. Keep its width at the
        # previous formatter's 88 columns while Ruff lint retains its own
        # 100-column limit. CI and ./lint use the same bounded Ruff version.
        Check(
            "Python format (ruff)",
            (*RUFF, "ruff", "format", "--check", "--line-length", "88", *PY_DIRS),
            "py-static",
        ),
        Check(
            "Python import order (isort)",
            (
                *PY,
                "--with",
                "isort>=6.0,<7",
                "python",
                "-m",
                "isort",
                "--check-only",
                "--profile",
                "black",
                *PY_DIRS,
            ),
            "py-static",
        ),
        # mypy over both the shipped package and the tests. `src/` was cleaned
        # in #2107 (four annotation defects, none behavioural); `tests/` is
        # clean as of this change, so the whole tree is checked and this stays a
        # regression guard rather than a new policy.
        # The sixth and last tool from `./lint`, and the one the issue is named
        # for: pylint reported 319 findings, so `./lint` (which runs under
        # `set -e`) could never reach its final line.
        #
        # 315 of the 319 were in `tests/` and were pytest idiom, not defects --
        # fixtures shadowing their own names, tests reaching into the private
        # functions they exist to test. `src/` had four, all deliberate, all now
        # carrying an inline disable that states the reason at the site.
        #
        # So the two surfaces are checked with two rulesets. `src/` runs pylint's
        # DEFAULT configuration -- nothing disabled -- and scores 10.00/10. Only
        # `tests/` gets the relaxations, and they live in `tests/.pylintrc` with
        # a per-entry rationale. Passing that file explicitly rather than relying
        # on pylint's directory discovery means it applies where it is named and
        # nowhere else.
        # pytest in the ephemeral env for the same reason as the mypy step
        # below: 22 test files import it, and pylint reports import-error for
        # what it cannot resolve.
        #
        # pyyaml for the same reason, on the three invocations that reach a
        # file importing it (five under tests/, plus check_workflow_python_pin
        # under .github/scripts). mypy already carries types-PyYAML; pylint
        # needs the real package, because E0401 is about importability rather
        # than typing. This was masked for as long as it existed: the Python
        # tests step runs first, and it had been failing on a collection error
        # since #2823, so pylint never got to report.
        # pylint gets `.github/actions/setup-soldr` rather than the parent
        # `.github/actions`: ensure_soldr.py imports its sibling module, which
        # only resolves when pylint has that leaf directory on its path.
        # Pointed at the parent it reports E0401 for an import that is fine.
        # The other five tools resolve it either way and take the parent.
        # soldr#2120: the CI scripts themselves, on pylint's default rules.
        # Two surfaces here for the same reason src/ and tests/ are split:
        # `test_*.py` in this directory are pytest suites, so they take the
        # tests/ ruleset, and everything else takes the default one.
        Check(
            "Python lint (pylint src)",
            (
                *PYLINT,
                "python",
                "-m",
                "pylint",
                *PYLINT_SOURCE_ARGS,
                "src",
            ),
            "py-static",
        ),
        Check(
            "Python lint (pylint tests)",
            (
                *PYLINT,
                "python",
                "-m",
                "pylint",
                *PYLINT_SOURCE_ARGS,
                "--rcfile=tests/.pylintrc",
                "tests",
            ),
            "py-static",
            slow=True,
        ),
        Check(
            "Python lint (pylint CI scripts)",
            (
                *PYLINT,
                "python",
                "-m",
                "pylint",
                *PYLINT_SOURCE_ARGS,
                ".github/scripts",
                ".github/actions/setup-soldr",
                "ci",
                "tools",
                ".claude/hooks",
                "--ignore-patterns=test_.*\\.py",
            ),
            "py-static",
            slow=True,
        ),
        Check(
            "Python lint (pylint CI script tests)",
            (
                *PYLINT,
                "python",
                "-m",
                "pylint",
                *PYLINT_SOURCE_ARGS,
                "--rcfile=tests/.pylintrc",
                *_script_tests(),
            ),
            "py-static",
        ),
        # All four directories. `.github/scripts` and `ci` are the scripts CI
        # itself runs, so a type error in them breaks a lane rather than a
        # developer's editor. Every finding was fixed at the source rather than
        # suppressed (a `None` that could reach `.get`, a list indexed with a
        # str, an over-tight `dict[str, object]` on a `json.loads` payload).
        #
        # `ci/` was held back in #2119 because four of its findings were
        # `fcntl.flock` "has no attribute" -- which looked like a Windows-only
        # artifact that would make the check pass or fail depending on who ran
        # it. It was not: `perf_local.py` already branches correctly at runtime,
        # but on `os.name == "nt"`, and mypy narrows `sys.platform`, not
        # `os.name`. Switching to the idiom mypy understands prunes the dead
        # branch on each platform and the findings disappear on both, so no
        # platform-scoping is needed here after all.
        # pytest is in the ephemeral env because the tests import it, and
        # mypy cannot resolve an import it cannot see. This passed locally
        # without it only because `uv run` picked up the project .venv,
        # which CI does not have -- the same environment-parity trap that
        # bit the flake8 step in #2103.
        # types-PyYAML: tests/test_wheel_lane_policy.py imports yaml (already
        # importable in this env -- the Python tests step passes) but mypy
        # needs the stub package to type it.
        Check(
            "Python types (mypy)",
            (
                *PY,
                "--with",
                "mypy>=1.18,<1.20",
                "--with",
                "pytest>=8.0",
                "--with",
                "types-PyYAML",
                "python",
                "-m",
                "mypy",
                *PY_DIRS,
            ),
            "py-static",
            slow=True,
        ),
        # soldr#2013 — nothing ran `tests/` on CI, so it decayed silently: a test
        # asserting on a workflow #1982 deleted, two lints that scanned nothing
        # and reported clean (#2008), and a manifest-coupled assertion (#2004).
        # A deletion PR cannot learn it orphaned a test; a lint cannot learn it
        # stopped scanning. 281 tests, ~15s, and no Rust toolchain needed — so it
        # runs here, before the expensive setup below.
        # `.github/scripts/` is included deliberately: six test files live
        # beside the scripts they cover (target-run summary, the Windows
        # e2e/MSVC-cache policies, catalogued-asset download, nextest fetch,
        # toolchain asset query) and this step used to run `tests/` only, so
        # those 29 tests never executed in CI. Tests that exist but never run
        # are worse than none -- they rot silently while implying coverage.
        Check(
            "Python tests",
            (
                *PY,
                "--with",
                "pytest",
                "--with",
                "pyyaml",
                "python",
                "-m",
                "pytest",
                "tests/",
                ".github/scripts/",
                "-q",
            ),
            "guards",
            slow=True,
        ),
    ]
    # ci-test's lint and dependency-policy stages, command for command
    # (`soldr ci-test --explain-plan`), run by the host's own soldr. Using
    # soldr as a tool on the host is safe; running soldr's *tests* there is
    # not (the `tests` lane, below). Exactly CI's commands, no stricter: a
    # broader `soldr lint deps` (full `cargo deny check`) fails on main for
    # license/advisory policy CI does not enforce, and a gate main cannot
    # pass attests nothing.
    rust = [
        Check("rustfmt", ("soldr", "cargo", "fmt", "--all", "--", "--check"), "rust"),
        Check("soldr lint ci", ("soldr", "lint", "ci"), "ci-lint"),
        Check(
            "cargo deny (bans)", ("soldr", "cargo", "deny", "check", "bans"), "ci-lint"
        ),
        # soldr#3785: a third-party advisory must not turn unrelated PRs red.
        # The wrapper always reports; it blocks off pull requests (main,
        # release candidates, schedules) and on a PR that changes
        # Cargo.lock/Cargo.toml, and fails closed when it cannot diff.
        Check(
            "cargo audit",
            (*script("audit_policy.py"), "--base-ref", base),
            "ci-lint",
        ),
        Check("cargo machete", ("soldr", "cargo", "machete"), "ci-lint"),
        Check(
            "clippy",
            (
                "soldr",
                "cargo",
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ),
            "rust",
            exclusive=True,
        ),
        Check(
            "dylint",
            ("soldr", "cargo", "dylint", "--all", "--", "--workspace", "--all-targets"),
            "rust",
            exclusive=True,
        ),
    ]
    cross = [
        Check(
            "cross targets (rust-std)",
            ("soldr", "rustup", "target", "add", *CROSS_TARGETS),
            "cross",
        )
    ]
    for target in CROSS_TARGETS:
        cross += [
            Check(
                f"clippy ({target})",
                (
                    "soldr",
                    "cargo",
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--target",
                    target,
                    "--",
                    "-D",
                    "warnings",
                ),
                "cross",
                exclusive=True,
            ),
            Check(
                f"dylint ({target})",
                (*PY, "python", "ci/cross_dylint.py", target),
                "cross",
                exclusive=True,
            ),
        ]
    wine = [
        Check(
            "windows-msvc unit tests (wine, container)",
            (*PY, "python", "ci/wine_lane.py"),
            "wine",
            exclusive=True,
        )
    ]
    # The owned Windows MSVC target-run partition, replayed natively in a
    # local dockur/windows VM; never on this host (GATE-005).
    winvm = [
        Check(
            "windows-msvc target-run (local Windows VM)",
            (*PY, "python", "ci/winvm_lane.py"),
            "winvm",
            exclusive=True,
            optional=True,
        )
    ]
    # zackees/ci.yml#168 (GATE-005), soldr#3516: soldr's test suite starts
    # soldr daemons and touches soldr state roots, so it never runs on the
    # developer host -- the nextest run-wrapper refuses unless CI=true or
    # SOLDR_TEST_ISOLATED=1. Locally it runs in bosn's isolated container,
    # whose image sets the marker.
    tests = [
        Check(
            "soldr tests (isolated, bosn)",
            # A locally built Bosn can be selected without replacing a shared
            # installation. Its normal version and isolation checks still run.
            (os.environ.get("SOLDR_LOCAL_GATE_BOSN", "bosn"), "run", "--task", "test"),
            "tests",
            exclusive=True,
            # zackees/bosn#317 (0.1.5): older bosn reaps a task whose output
            # outruns its event queue -- `ci-test`'s warning burst died at
            # 390 s. 0.1.6 (zackees/bosn#322): a stale socket in the default
            # state dir is reclaimed instead of failing autostart, and a
            # client/daemon version mismatch is reported instead of a reset.
            min_version=(0, 1, 6),
            tree_nonce=True,
        )
    ]
    return lint + rust + cross + wine + winvm + tests


def _script_tests() -> tuple[str, ...]:
    return tuple(
        sorted(
            str(p.relative_to(ROOT))
            for p in (ROOT / ".github" / "scripts").glob("test_*.py")
        )
    )


@dataclass(frozen=True)
class Result:
    check: Check
    code: int
    seconds: float
    output: str


@dataclass(frozen=True)
class Captured:
    returncode: int
    output: str


def run_captured(argv: list[str]) -> Captured:
    """Run `argv` from the repository root with stdout and stderr captured
    through one temporary file, never a pipe (zackees/ci.yml PY-003): a full
    pipe blocks the child, and a soldr daemon or broker that inherits a pipe
    keeps the caller waiting for an EOF that never comes. This returns when
    the direct child exits, whatever it left running."""
    with tempfile.TemporaryFile() as out:
        proc = subprocess.run(
            argv,
            cwd=ROOT,
            stdin=subprocess.DEVNULL,
            stdout=out,
            stderr=subprocess.STDOUT,
            check=False,
        )
        out.seek(0)
        return Captured(proc.returncode, out.read().decode("utf-8", errors="replace"))


# The console excerpt on failure: long enough to diagnose a small check from
# the lane log alone, bounded so one runaway child cannot flood it. The
# complete output goes to a durable file instead (soldr#3564).
LOG_TAIL_CHARS = 20000


def _git_dir() -> Path | None:
    """The repository's real git directory, resolved by git itself.

    `ROOT / ".git"` is a directory only in a primary checkout: in a linked
    worktree it is a file pointing elsewhere, so path arithmetic cannot find
    the right place to keep gate state. None when git cannot answer or names
    no existing directory -- not a repository, or git is absent.
    """
    proc = run_captured(["git", "rev-parse", "--git-dir"])
    if proc.returncode != 0:
        return None
    first = next((ln.strip() for ln in proc.output.splitlines() if ln.strip()), "")
    if not first:
        return None
    path = Path(first)
    if not path.is_absolute():
        path = ROOT / path
    path = path.resolve()
    return path if path.is_dir() else None


def prepare_log_dir() -> Path | None:
    """This run's directory for complete failed-check logs (soldr#3564).

    The preferred location is `<git-dir>/local-gate/logs/<run-id>/`: durable,
    local-only (under the git dir, so it is never committed and never
    published), correct for worktrees, and unique per run so two gate
    invocations -- or two worktrees -- cannot overwrite each other's evidence.
    When no log location exists at all (no git directory, or nothing
    writable), degrade to a warning and None: losing the log is the bug being
    fixed here, so the gate reports where it cannot write one rather than
    crashing over its own diagnostics.
    """
    run_id = f"{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}-{secrets.token_hex(3)}"
    reason = "no git directory resolved"
    git_dir = _git_dir()
    if git_dir is not None:
        try:
            run_dir = git_dir / "local-gate" / "logs" / run_id
            run_dir.mkdir(parents=True, exist_ok=True)
            return run_dir
        except OSError as exc:
            reason = f"cannot write under {git_dir}/local-gate/logs ({exc})"
    try:
        fallback = Path(tempfile.mkdtemp(prefix="soldr-local-gate-logs-"))
    except OSError as exc:
        print(
            f"local gate: warning: {reason}; no temp dir either ({exc}); "
            "failed-check output will not be persisted",
            file=sys.stderr,
        )
        return None
    print(
        f"local gate: warning: {reason}; failed-check logs go to {fallback}",
        file=sys.stderr,
    )
    return fallback


def persist_failed_log(log_dir: Path, result: Result) -> Path | None:
    """Write the check's COMPLETE output to `log_dir` and return the path.

    The console prints only a bounded tail of the same text; this file is the
    whole capture with a provenance header, so a failure diagnosable only
    from its first pages (a lost assertion above 20 KB of test output) stays
    diagnosable. None when the write fails -- a missing log is warned about,
    never allowed to change the gate's exit code.
    """
    slug = re.sub(r"[^A-Za-z0-9._-]+", "_", result.check.name).strip("._-") or "check"
    path = log_dir / f"{slug}.log"
    counter = 2
    while path.exists():  # two check names can sanitize to the same slug
        path = log_dir / f"{slug}-{counter}.log"
        counter += 1
    header = (
        f"# local gate failed check: {result.check.name}\n"
        f"# lane: {result.check.lane}  exit: {result.code}"
        f"  seconds: {result.seconds:.1f}\n"
        f"# command: {' '.join(result.check.argv)}\n"
        f"# complete output follows ({len(result.output)} chars)\n"
    )
    try:
        log_dir.mkdir(parents=True, exist_ok=True)
        path.write_text(header + result.output, encoding="utf-8")
    except OSError as exc:
        print(f"local gate: warning: cannot persist {path} ({exc})", file=sys.stderr)
        return None
    return path


def _print_failures(failed: list[Result]) -> None:
    """Report every failed check: its exact durable log path, then the
    bounded tail of its output (soldr#3564). The tail is labeled as a tail
    rather than presented as the full output, and a log that could not be
    written is named instead of silently omitted."""
    log_dir = prepare_log_dir() if failed else None
    for result in failed:
        log_path = persist_failed_log(log_dir, result) if log_dir is not None else None
        print(f"\n===== FAIL: {result.check.name} (exit {result.code}) =====")
        print(f"$ {' '.join(result.check.argv)}")
        if log_path is not None:
            print(f"full log: {log_path}")
        else:
            print("full log: unavailable (see warning on stderr)")
        tail = result.output.rstrip()
        if len(tail) > LOG_TAIL_CHARS:
            print(
                f"(console tail: last {LOG_TAIL_CHARS} of {len(tail)} chars;"
                " the complete output is in the full log above)"
            )
        print(tail[-LOG_TAIL_CHARS:])


def tool_version(tool: str) -> tuple[int, ...] | None:
    proc = run_captured([tool, "--version"])
    match = re.search(r"(\d+)\.(\d+)\.(\d+)", proc.output)
    return tuple(int(part) for part in match.groups()) if match else None


NONCE_FILE = ".gate-nonce"
NONCE_MARKER = "gate-nonce: "


def _run(check: Check) -> Result:
    if not check.tree_nonce:
        return _run_plain(check)
    nonce = secrets.token_hex(16)
    path = ROOT / NONCE_FILE
    path.write_text(nonce + "\n", encoding="utf-8")
    try:
        result = _run_plain(check)
    finally:
        path.unlink(missing_ok=True)
    if f"{NONCE_MARKER}{nonce}" in result.output:
        return result
    seen = [ln for ln in result.output.splitlines() if ln.startswith(NONCE_MARKER)]
    return Result(
        check,
        result.code or 1,
        result.seconds,
        result.output + f"\nlocal gate: the isolated runner did not see this worktree "
        f"(expected {NONCE_MARKER}{nonce}, saw {seen[-1] if seen else 'no nonce'}). "
        "It ran another checkout's tree -- zackees/bosn#314, zackees/ci.yml#196. "
        "Stop or remove the bosn setup container bound to the other worktree, "
        "or wait for it, then rerun.",
    )


def _run_plain(check: Check) -> Result:
    start = time.monotonic()
    tool = shutil.which(check.argv[0])
    if tool is None:
        return Result(check, 127, 0.0, f"{check.argv[0]}: not found on PATH")
    if check.min_version is not None:
        found = tool_version(tool)
        if found is None or found < check.min_version:
            want = ".".join(map(str, check.min_version))
            return Result(
                check,
                1,
                0.0,
                f"{check.argv[0]} {found or 'unknown'} is older than {want}; "
                f"upgrade it: uv tool upgrade {check.argv[0]}",
            )
    proc = run_captured(list(check.argv))
    return Result(check, proc.returncode, time.monotonic() - start, proc.output)


def _not_applicable(result: Result) -> bool:
    return result.check.optional and result.code == NOT_APPLICABLE


def _fetch_base() -> None:
    """The diff ratchets need the base branch; fetch it, never fatally. Its
    output is captured to a file and forwarded only on failure, not
    swallowed (soldr#3389) and not piped (zackees/ci.yml PY-003)."""
    ref = _base_ref().removeprefix("origin/")
    proc = run_captured(["git", "fetch", "--no-tags", "--quiet", "origin", ref])
    if proc.returncode != 0:
        print(
            f"note: git fetch origin {ref} failed (exit {proc.returncode}):",
            file=sys.stderr,
        )
        print(proc.output.rstrip(), file=sys.stderr)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "--lane",
        choices=("all", "lint", *LANES),
        default="all",
        help="one GATE-007 lane, or `lint` (= the remote Lint job: py-static + guards)",
    )
    parser.add_argument("--list", action="store_true", help="print the checks and exit")
    parser.add_argument("--jobs", type=int, default=min(8, os.cpu_count() or 2))
    args = parser.parse_args(argv)

    event = os.environ.get("GITHUB_EVENT_NAME", "")
    wanted = LINT_ALIAS if args.lane == "lint" else (args.lane,)
    selected = [c for c in checks() if args.lane == "all" or c.lane in wanted]
    if event and event != "pull_request":
        selected = [c for c in selected if not c.needs_base]
    if args.list:
        for check in selected:
            print(f"[{check.lane}] {check.name}: {' '.join(check.argv)}")
        return 0
    if any(c.needs_base for c in selected):
        _fetch_base()

    start = time.monotonic()
    parallel = sorted(
        (c for c in selected if not c.exclusive), key=lambda c: not c.slow
    )
    results: list[Result] = []
    with ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        for future in as_completed([pool.submit(_run, c) for c in parallel]):
            result = future.result()
            results.append(result)
            print(
                f"{'ok  ' if result.code == 0 else 'FAIL'} {result.seconds:6.1f}s  {result.check.name}",
                flush=True,
            )
    for check in (c for c in selected if c.exclusive):
        result = _run(check)
        results.append(result)
        status = (
            "n/a "
            if _not_applicable(result)
            else "ok  "
            if result.code == 0
            else "FAIL"
        )
        print(f"{status} {result.seconds:6.1f}s  {check.name}", flush=True)
        if _not_applicable(result):
            print(
                f"      {result.output.strip().splitlines()[-1] if result.output.strip() else ''}"
            )

    skipped = [r for r in results if _not_applicable(r)]
    failed = [r for r in results if r.code != 0 and not _not_applicable(r)]
    _print_failures(failed)
    total = time.monotonic() - start
    print(
        f"\nlocal gate ({args.lane}): {len(results) - len(failed) - len(skipped)}/{len(results)} passed"
        f"{f', {len(skipped)} not applicable here' if skipped else ''} in {total:.0f}s"
    )
    if failed:
        return 1
    return NOT_APPLICABLE if results and len(skipped) == len(results) else 0


if __name__ == "__main__":
    sys.exit(main())
