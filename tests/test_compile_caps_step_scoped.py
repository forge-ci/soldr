"""Compile caps are step-scoped, and every cap states its reason (soldr#3148).

CLAUDE.md, "Concurrency caps are a last resort": a cap that has to stay should
sit at *step* scope, never job scope. A job-level `env:` block applies to every
subsequent step and silently serializes unrelated work.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

import yaml

WORKFLOWS = Path(__file__).resolve().parents[1] / ".github" / "workflows"
CAPS = {"CARGO_BUILD_JOBS", "SOLDR_JOBS"}

# Job-level caps that remain, each with the reason its workflow states.
JOB_SCOPE_EXCEPTIONS = {
    # Cache-disabled release+LTO cross build; the policy matrix carries a
    # measured per-cell memory bound (soldr#2453/#2469).
    ("ci.yml", "wheel-cross-verify"),
    # workflow_dispatch-only probe; its job comment records the soldr-daemon
    # codegen-units=1 kill it caps against (soldr#2781).
    ("macos-universal2-probe.yml", "build-slice"),
}


def _load(name: str) -> dict:
    return yaml.safe_load((WORKFLOWS / name).read_text(encoding="utf-8")) or {}


@dataclass(frozen=True)
class EnvVar:
    """One `env:` entry of a workflow step."""

    name: str
    value: str


@dataclass(frozen=True)
class Step:
    """The parts of a workflow step these tests inspect."""

    env: tuple[EnvVar, ...]
    run: str

    def env_names(self) -> set[str]:
        return {var.name for var in self.env}

    def env_value(self, name: str) -> str | None:
        return next((var.value for var in self.env if var.name == name), None)

    def has_cap(self) -> bool:
        return bool(CAPS & self.env_names())


def _step(name: str, job: str, step_name: str) -> Step:
    for step in _load(name)["jobs"][job]["steps"]:
        if step.get("name") == step_name:
            return Step(
                env=tuple(
                    EnvVar(str(key), str(value))
                    for key, value in (step.get("env") or {}).items()
                ),
                run=step.get("run") or "",
            )
    raise KeyError(step_name)


def test_no_workflow_or_job_scope_caps_outside_the_named_exceptions() -> None:
    offenders = []
    for path in sorted(WORKFLOWS.glob("*.y*ml")):
        document = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
        if CAPS & set(document.get("env") or {}):
            offenders.append((path.name, "<workflow>"))
        for job_id, job in (document.get("jobs") or {}).items():
            if CAPS & set((job or {}).get("env") or {}):
                if (path.name, job_id) not in JOB_SCOPE_EXCEPTIONS:
                    offenders.append((path.name, job_id))
    assert not offenders, f"job/workflow-scoped compile caps: {offenders}"


def test_bootstrap_e2e_builds_are_uncapped() -> None:
    # Both builds are wrapped: "Build soldr-cli" runs the released soldr pin,
    # which carries soldr#3211 since 0.9.16, and the third-party build runs
    # the freshly built source soldr.
    for name in ("Build soldr-cli", "Build third-party app through soldr"):
        step = _step("_bootstrap-e2e.yml", "bootstrap-e2e", name)
        assert not step.has_cap(), name
    text = (WORKFLOWS / "_bootstrap-e2e.yml").read_text(encoding="utf-8")
    assert "soldr#3211" in text


def test_setup_soldr_action_caps_only_the_no_cache_test() -> None:
    build = _step("setup-soldr-action.yml", "smoke", "Build through soldr")
    assert not build.has_cap()
    test = _step("setup-soldr-action.yml", "smoke", "Test through soldr")
    assert test.env_value("CARGO_BUILD_JOBS") == "1"
    assert test.env_value("SOLDR_JOBS") == "1"
    # The one honest rung-4 reason: `--no-cache` has no admission gate.
    assert "--no-cache cargo nextest run" in test.run


def test_cook_size_gate_caps_only_the_cache_disabled_cook() -> None:
    build = _step(
        "cook-size-gate.yml", "cook-size-gate", "Build soldr CLI (ci-release)"
    )
    assert not build.has_cap()
    cook = _step(
        "cook-size-gate.yml",
        "cook-size-gate",
        "Run soldr cook against zccache (release profile)",
    )
    assert cook.env_value("ZCCACHE_DISABLE") == "1"
    assert cook.env_value("CARGO_BUILD_JOBS") == "1"
    assert cook.env_value("SOLDR_JOBS") == "1"
