from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github" / "workflows"
SETUP_SOLDR_MANAGED_SHA = "35b5765c99cf392c259e684daeda10a950f72fff"
DIRECT_USE = re.compile(r"uses:\s*zackees/setup-soldr@([0-9a-f]{40})")


@dataclass(frozen=True)
class SetupPin:
    """A workflow's direct `zackees/setup-soldr@<sha>` pin."""

    workflow: str
    sha: str


def test_all_seventeen_setup_soldr_pins_use_managed_release() -> None:
    pins: list[SetupPin] = []
    for workflow in sorted(WORKFLOWS.glob("*.y*ml")):
        for match in DIRECT_USE.finditer(workflow.read_text(encoding="utf-8")):
            pins.append(SetupPin(workflow.name, match.group(1)))

    assert len(pins) == 17, pins
    assert {pin.sha for pin in pins} == {SETUP_SOLDR_MANAGED_SHA}, pins


def test_third_party_toolchain_uses_managed_rustup() -> None:
    workflow = (WORKFLOWS / "_bootstrap-e2e.yml").read_text(encoding="utf-8")
    assert "soldr rustup toolchain install $channel --profile minimal" in workflow
    assert (
        'soldr rustup target add --toolchain $channel "${{ inputs.target }}"'
        in workflow
    )
    assert "run: rustup " not in workflow
    assert workflow.count("if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }") >= 2


def test_managed_setup_does_not_reference_removed_rustup_cache() -> None:
    for name in ("_bootstrap-e2e.yml", "_build-and-test.yml", "ci.yml"):
        workflow = (WORKFLOWS / name).read_text(encoding="utf-8")
        assert "steps.rustup_cache.outputs" not in workflow
