"""No lane pins soldr; setup-soldr's default is the one pin (soldr#1799).

soldr#1799 began as a stale *default*: setup-soldr used to resolve to 0.8.23,
which garbage-collected live artifacts out of `target/` after every build, so
every lane recompiled the world and stayed green while running 10-50x slower.
The first fix pinned `version:` at every call site and kept the pins agreeing.

Pins at call sites then became the problem themselves: they lagged releases,
and an old soldr installed on a shared host (a pinned `uv tool install`, a
PEP 723 `soldr==` dependency) downgraded every other session or started an old
daemon that took over `~/.soldr` and locked newer daemons out. So the pin now
lives in exactly one place -- setup-soldr's own default, which its ingest
workflow bumps to the current release on every soldr release -- and every call
site and bootstrap here takes that default or the latest published release.

This file guards the policy: no `version:` at a setup-soldr call site (except
a named, frozen experiment), and no `soldr==X.Y.Z` in the non-action
bootstraps.
"""

from __future__ import annotations

import re
from pathlib import Path

WORKFLOWS = sorted(
    (Path(__file__).resolve().parents[1] / ".github" / "workflows").glob("*.y*ml")
)

# A `uses:` KEY. A comment merely mentioning the action is not a call site --
# getting this wrong is how the first version of this audit invented call sites
# at line numbers that held comments.
USES = re.compile(r"^(\s*)uses:\s*zackees/setup-soldr@")
VERSION = re.compile(r"\s*version:\s*(\S+)")

# Call sites that may pin, and why. Each entry is load-bearing.
EXEMPT = {
    # A measurement baseline, frozen at 0.7.28 on purpose so the cache-delta
    # numbers stay comparable across runs. It is an experiment, not a lane.
    "cache-delta-experiment.yml": "0.7.28 experiment baseline",
}


def _call_sites() -> "list[tuple[str, int, str | None]]":
    """[(workflow, line, pinned_version_or_None)] for real `uses:` keys."""
    sites = []
    for workflow in WORKFLOWS:
        lines = workflow.read_text(encoding="utf-8").splitlines()
        for index, line in enumerate(lines):
            match = USES.match(line)
            if not match:
                continue
            indent = len(match.group(1))
            version = None
            for candidate in lines[index + 1 :]:
                if not candidate.strip() or candidate.lstrip().startswith("#"):
                    continue
                current = len(candidate) - len(candidate.lstrip())
                if current <= indent and candidate.lstrip().startswith("-"):
                    break  # next step; this one had no `version:`
                found = VERSION.match(candidate)
                if found:
                    version = found.group(1).strip("\"'")
                    break
            sites.append((workflow.name, index + 1, version))
    return sites


def test_no_call_site_pins_a_version() -> None:
    pinned = [
        f"{name}:{line} ({version})"
        for name, line, version in _call_sites()
        if version is not None and name not in EXEMPT
    ]
    assert not pinned, (
        "these setup-soldr call sites pin `version:`; take the action's "
        "default instead -- setup-soldr's default is the one soldr pin: "
        + ", ".join(pinned)
    )


def test_non_action_bootstraps_install_the_latest_release() -> None:
    root = Path(__file__).resolve().parents[1]
    for relative in (
        "ci/win_wheel_local.py",
        ".github/workflows/docker-linux-cross-smoke.yml",
        ".github/workflows/build-all-from-linux.yml",
    ):
        text = (root / relative).read_text(encoding="utf-8")
        assert not re.search(r"soldr==\d", text), f"{relative} pins soldr"
    build_all = (root / ".github/workflows/build-all-from-linux.yml").read_text(
        encoding="utf-8"
    )
    assert 'default: ""' in build_all, "build-all-from-linux must default to latest"


def test_every_setup_soldr_call_disables_cook_delta() -> None:
    """Keep expensive compile output out of the archive unless an experiment owns it."""
    missing_or_enabled: list[str] = []
    use_line = re.compile(r"^(\s*)(?:-\s*)?uses:\s*zackees/setup-soldr@")
    for workflow in WORKFLOWS:
        lines = workflow.read_text(encoding="utf-8").splitlines()
        for index, line in enumerate(lines):
            match = use_line.match(line)
            if not match:
                continue
            step_indent = len(match.group(1))
            block: list[str] = []
            for candidate in lines[index + 1 :]:
                stripped = candidate.lstrip()
                if stripped and not stripped.startswith("#"):
                    indent = len(candidate) - len(stripped)
                    if indent <= step_indent and stripped.startswith("-"):
                        break
                block.append(candidate)
            if not re.search(
                r"^\s*cook-delta:\s*false\s*(?:#.*)?$", "\n".join(block), re.M
            ):
                missing_or_enabled.append(f"{workflow.name}:{index + 1}")

    assert not missing_or_enabled, (
        "setup-soldr cook delta must be explicitly disabled to keep per-commit "
        "archives out of the shared cook-layer budget (soldr#3347 / "
        "setup-soldr#528): " + ", ".join(missing_or_enabled)
    )


def test_pep517_backend_requirement_is_a_floor_not_a_pin() -> None:
    # The backend delegates to any strictly newer global soldr, so an exact
    # pin that lags the global install splits one build across two versions:
    # the pinned binary becomes the per-crate RUSTC_WRAPPER and the broker
    # refuses it ("wanted_version is below min_version"). Float to latest.
    text = (Path(__file__).resolve().parents[1] / "pyproject.toml").read_text(
        encoding="utf-8"
    )
    requires = text.split("[build-system]", 1)[1].split("]", 1)[0]
    assert '"soldr==' not in requires, "pin the PEP 517 backend with >=, not =="
    match = re.search(r'"soldr>=(\d+)\.(\d+)\.(\d+)"', requires)
    assert match, "the PEP 517 backend needs a catalogue-v2-capable soldr floor"
    assert tuple(int(p) for p in match.groups()) >= (0, 9, 6)


def test_the_scan_finds_the_call_sites() -> None:
    # Without this, a regex that stopped matching would make every assertion
    # above vacuously true -- the failure mode these checks exist to catch.
    sites = _call_sites()
    assert len(sites) >= 8, f"expected many setup-soldr call sites, found {len(sites)}"
    assert any(name == "ci.yml" for name, _, _ in sites)
    # and the exemptions must still refer to workflows that exist
    present = {workflow.name for workflow in WORKFLOWS}
    stale = sorted(set(EXEMPT) - present)
    assert not stale, f"EXEMPT names workflows that no longer exist: {stale}"
