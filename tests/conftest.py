"""Pytest collection hooks shared across the soldr test suite."""

from __future__ import annotations

import importlib.util
import os
import re
import shlex
import shutil
import subprocess
import sys
from pathlib import Path
from types import ModuleType

import pytest

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

NEXTEST_WRAPPER_UNDER_TEST_ENV = "SOLDR_NEXTEST_WRAPPER_UNDER_TEST"
NEXTEST_WRAPPER_NAME = "soldr-nextest-wrapper"


def nextest_wrapper_argv() -> list[str]:
    """The Nextest run-wrapper the black-box suites drive (soldr#3454).

    The native ``soldr-nextest-wrapper`` is the only implementation. It is the
    binary named in ``SOLDR_NEXTEST_WRAPPER_UNDER_TEST`` (the bosn plan always
    sets it, so the suites really run there), else a debug build under
    ``CARGO_TARGET_DIR`` (default ``<repo>/target``). With neither, the calling
    test skips and says how to build one; a named binary that does not exist
    is a failure, not a skip.
    """

    named = os.environ.get(NEXTEST_WRAPPER_UNDER_TEST_ENV, "").strip()
    if named:
        if not Path(named).is_file():
            pytest.fail(
                f"{NEXTEST_WRAPPER_UNDER_TEST_ENV}={named} names no file", pytrace=False
            )
        return [named]
    target = os.environ.get("CARGO_TARGET_DIR", "").strip() or str(
        Path(__file__).resolve().parents[1] / "target"
    )
    built = Path(target) / "debug" / NEXTEST_WRAPPER_NAME
    if built.is_file():
        return [str(built)]
    # pytest.skip raises; spelled as a raise so every path visibly exits.
    raise pytest.skip.Exception(
        f"no {NEXTEST_WRAPPER_NAME} binary at {built}: build it "
        f"(soldr cargo build -p {NEXTEST_WRAPPER_NAME}) or name it in "
        f"{NEXTEST_WRAPPER_UNDER_TEST_ENV}"
    )


def load_script_module(path: str | Path, name: str | None = None) -> ModuleType:
    """Import a standalone script as a module (soldr#2113).

    The scripts these guards cover live in `.github/scripts/` and similar
    non-package directories, so there is no `import` statement that reaches
    them. Every guard hand-rolled the same importlib dance; this is that
    dance, once.

    `name` defaults to the file stem. The module is registered in
    `sys.modules` *before* `exec_module` runs, which several of the guarded
    scripts require: a dataclass resolves its own `__module__` through
    `sys.modules` while the class is being created, and raises `KeyError` if
    the entry is not there yet. Registering unconditionally is the superset of
    what the call sites did, so no caller loses behaviour by moving here.
    """

    script = Path(path)
    module_name = name or script.stem
    spec = importlib.util.spec_from_file_location(module_name, script)
    if spec is None or spec.loader is None:
        raise ImportError(f"cannot load {script} as a module")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def lint_lane_text() -> str:
    """What the remote Lint job runs, as text (zackees/ci.yml#166).

    The Lint job runs exactly `ci/local_gate.py --lane lint` (GATE-001), so
    "CI runs check X" now means "X is a lint-lane check of the local gate".
    Returns each lint-lane check's name and shell-joined argv, one per line,
    after asserting that ci.yml's Lint job still invokes that lane.
    """

    root = Path(__file__).resolve().parent.parent
    workflow = (root / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    assert "python ci/local_gate.py --lane lint" in workflow
    gate = load_script_module(root / "ci" / "local_gate.py", "soldr_local_gate")
    return "\n".join(
        f"{check.name}: {shlex.join(check.argv)}"
        for check in gate.checks()
        if check.lane in gate.LINT_ALIAS
    )


def write_collected_recovery_summary(base: Path, lines: list[str]) -> Path:
    """Build a `<base>/summary.txt` fixture for a Recovery guest smoke test.

    `ci/smoke_release_artifacts.py` and `ci/macos_recovery_run.py` each parse
    this exact flat `key=value` shape out of a zackees/docker-mac-x64
    `collect` tarball (soldr#3076); their tests share the fixture-builder
    here rather than each hand-rolling the same `mkdir` + `write_text`.
    """

    base.mkdir(parents=True, exist_ok=True)
    (base / "summary.txt").write_text("\n".join(lines), encoding="utf-8")
    return base


def assert_recovery_verify_collected_contract(
    module: ModuleType, tmp_path: Path, *, passing_lines: list[str]
) -> None:
    """Shared soldr#3076 `verify_collected` behavior contract.

    `ci/smoke_release_artifacts.py` and `ci/macos_recovery_run.py` each
    implement the same four-case contract (pass, one named per-check
    failure, one check that never ran, a missing results file, and a
    nonzero guest exit code) against different check sets. Both modules'
    tests call this once rather than each repeating the four cases, which is
    also what keeps pylint's `duplicate-code` check quiet across the pair
    (see `load_script_module`'s docstring above for the same rationale
    applied to the importlib dance, soldr#2113).
    """

    first_check = passing_lines[0].split("=", 1)[0]

    passing = write_collected_recovery_summary(tmp_path / "pass", passing_lines)
    assert module.verify_collected(passing, guest_exit_code="0") == 0

    failing = write_collected_recovery_summary(
        tmp_path / "fail", [f"{first_check}=fail:boom"]
    )
    with pytest.raises(SystemExit, match=f"{first_check}: boom"):
        module.verify_collected(failing, guest_exit_code="0")

    incomplete = write_collected_recovery_summary(
        tmp_path / "incomplete", passing_lines[1:]
    )
    with pytest.raises(SystemExit, match=f"{first_check}: no result recorded"):
        module.verify_collected(incomplete, guest_exit_code="0")

    missing = write_collected_recovery_summary(tmp_path / "missing", [])
    (missing / "summary.txt").unlink()
    with pytest.raises(SystemExit, match="never wrote results"):
        module.verify_collected(missing, guest_exit_code="0")

    nonzero = write_collected_recovery_summary(tmp_path / "nonzero", passing_lines)
    with pytest.raises(SystemExit, match="guest script exit code"):
        module.verify_collected(nonzero, guest_exit_code="1")


def write_fake_soldr_console(venv: Path, *, windows: bool) -> Path:
    """Materialize the platform's virtualenv console-script location."""

    console = (
        venv / ("Scripts" if windows else "bin") / ("soldr.exe" if windows else "soldr")
    )
    console.parent.mkdir(parents=True, exist_ok=True)
    console.write_bytes(b"")
    return console


def uv_pip_install_command(venv: Path, *packages: str) -> list[str]:
    """Build the exact pip-install argv used by isolated wheel smoke doubles."""

    return ["uv", "pip", "install", "--python", str(venv), *packages]


def maturin_release_build_command(
    maturin: str, target: str, compatibility: str
) -> list[str]:
    """The exact release-locked maturin argv two release scripts must agree on.

    `build_release_wheel.maturin_build_command` and
    `native_release_build.musl_wheel_maturin_command` both produce this shape;
    their tests share the expectation here so the two cannot drift apart (and
    so pylint's `duplicate-code` check does not report the copy).
    """

    return [
        maturin,
        "build",
        "--release",
        "--locked",
        "--strip",
        "--target",
        target,
        "--target-dir",
        "target",
        "--out",
        "dist",
        "--compatibility",
        compatibility,
    ]


# Release scripts are standalone modules, but several share this module. Load it
# once during test collection so their direct sibling import resolves under the
# importlib-based test loader too.
RELEASE_ARTIFACTS = load_script_module(
    Path(__file__).parents[1] / ".github" / "scripts" / "release_artifacts.py",
    "release_artifacts",
)


# The workspace's first-party crates. Two guards assert over this same list,
# and a crate added to the workspace must reach both of them.
WORKSPACE_CRATES = [
    "soldr-cli",
    "soldr-core",
    "soldr-fetch",
    "soldr-cache",
    "soldr-daemon",
]

# The triples soldr ships, spelled out rather than derived from
# `ci/canonical-targets.json`. Two guards assert over this same list -- the
# target-removal contract and the Dylint driver-asset check -- so a contract
# edit that drops one has to answer to both, instead of quietly shrinking the
# set each of them checks. Dropping a target is a reviewed decision
# (soldr#2469 step 2.1): it needs a `compatibility_decisions` entry and this
# list updated in the same PR.
RELEASE_INCLUDED_TRIPLES = [
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
]

# Canonical Dylint CI policy shared by the nightly-agreement and process-
# boundary guards. The full rustup toolchain key is also the target-directory
# key, keeping nightly artifacts separate from the project's Rust 1.95 tree.
DYLINT_NIGHTLY = "nightly-2026-08-15-x86_64-unknown-linux-gnu"
DYLINT_BUILD_STEPS = (
    "Build daemon process-creation boundary lint",
    "Build fetch network boundary lint",
    "Build local-socket name boundary lint",
    "Build raw IPC transport boundary lint",
    "Build platform-cfg directory boundary lint",
    "Build env-flag boundary lint",
)
DYLINT_TEST_STEPS = (
    "Test env-flag boundary lint",
    "Test daemon process-creation boundary lint",
    "Test fetch network boundary lint",
    "Test local-socket name boundary lint",
    "Test raw IPC transport boundary lint",
    "Test platform-cfg directory boundary lint",
)

# soldr#2996: the cook `cache:` input is gated by an explicit target
# allowlist, not by falling through an exclusion list. soldr#3121 added
# aarch64-pc-windows-msvc, x86_64-pc-windows-msvc and
# aarch64-unknown-linux-gnu. Two guards -- _ci-cross-build-linux.yml's own
# step and setup-soldr/cook's flags check -- pin this exact literal, so a
# lane silently gaining or losing cook has to answer to both.
COOK_CACHE_ALLOWLIST_INPUT = (
    "cache: ${{ (inputs.target == 'x86_64-pc-windows-gnu' "
    "|| inputs.target == 'x86_64-unknown-linux-gnu' "
    "|| inputs.target == 'aarch64-pc-windows-msvc' "
    "|| inputs.target == 'x86_64-pc-windows-msvc' "
    "|| inputs.target == 'aarch64-unknown-linux-gnu') "
    "&& 'true' || 'false' }}"
)


def workflow_step(workflow: str, name: str) -> str:
    """Return one named GitHub Actions step body from a workflow string."""

    match = re.search(
        rf"^      - name: {re.escape(name)}\n(?P<body>.*?)(?=^      - name: |\Z)",
        workflow,
        re.MULTILINE | re.DOTALL,
    )
    if match is None:
        raise AssertionError(f"workflow has no {name!r} step")
    return match.group("body")


def docker_available() -> bool:
    """Whether a reachable docker daemon exists.

    Both docker-backed guards are opt-in and skip without one. `docker info`
    is the probe rather than `which docker`, because a present client with a
    dead daemon is the common local state (see the Docker Desktop notes in
    CLAUDE.md).

    The two call sites had different timeouts (10s and 20s); this takes the
    larger. A daemon that answers in 15s would otherwise be reported absent,
    silently downgrading a real integration run to a skip.
    """

    if shutil.which("docker") is None:
        return False
    try:
        result = subprocess.run(
            ["docker", "info"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=20,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0


def git(repo: Path, *args: str) -> str:
    """Run git in `repo` and return stdout, raising on failure."""

    return run_captured(
        ["git", *args], cwd=repo, check=True, capture_output=True, text=True
    ).stdout


def commit_all(repo: Path, message: str) -> None:
    """Stage everything and commit -- the ratchet tests' fixture shape.

    Shared so `test_loc_ratchet.py`-style ratchet tests (soldr#1966,
    soldr#3388) do not each hand-roll the same two-line `git add -A` +
    `git commit` (soldr#2113's `load_script_module` rationale applies the same
    way here: one implementation instead of pylint's `duplicate-code` finding
    the copy).
    """

    git(repo, "add", "-A")
    git(repo, "commit", "-q", "-m", message)


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    """A throwaway git repo with an identity configured.

    Commits fail outright without `user.email` / `user.name`, and CI runners
    have no global git identity, so configuring it is part of the fixture
    rather than something each test remembers.
    """

    root = tmp_path / "repo"
    root.mkdir()
    git(root, "init", "-q", "-b", "main")
    git(root, "config", "user.email", "t@example.com")
    git(root, "config", "user.name", "t")
    return root


def pytest_addoption(parser: pytest.Parser) -> None:
    parser.addoption(
        "--act-integration",
        action="store_true",
        default=False,
        help=(
            "Run docker-based act-image integration tests (marked "
            "@pytest.mark.act_integration). Skipped by default because they "
            "require docker + ~1 GB image pulls."
        ),
    )
    parser.addoption(
        "--cacheability-integration",
        action="store_true",
        default=False,
        help=(
            "Run Docker-based cacheability integration tests (marked "
            "@pytest.mark.cacheability_integration). Skipped by default "
            "because they build the full nextest archive twice."
        ),
    )


def pytest_collection_modifyitems(
    config: pytest.Config, items: list[pytest.Item]
) -> None:
    marker_options = {
        "act_integration": "--act-integration",
        "cacheability_integration": "--cacheability-integration",
    }
    marker_expression = config.getoption("-m") or ""
    for item in items:
        for marker, option in marker_options.items():
            if marker not in item.keywords:
                continue
            if config.getoption(option) or marker in marker_expression:
                continue
            item.add_marker(
                pytest.mark.skip(
                    reason=(
                        f"{marker} tests are opt-in. Re-run with {option} "
                        f"or `-m {marker}` to execute them."
                    )
                )
            )
