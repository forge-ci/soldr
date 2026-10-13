from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github" / "workflows"
SETUP_SOLDR_V0_9_82_SHA = "a07bab94f16124b5c6857b137a237a53a61e06d1"
DIRECT_USE = re.compile(r"uses:\s*zackees/setup-soldr@([0-9a-f]{40})")


@dataclass(frozen=True)
class SetupPin:
    """A workflow's direct `zackees/setup-soldr@<sha>` pin."""

    workflow: str
    sha: str


def test_all_fourteen_setup_soldr_pins_consume_v0_9_80_release() -> None:
    pins: list[SetupPin] = []
    for workflow in sorted(WORKFLOWS.glob("*.y*ml")):
        for match in DIRECT_USE.finditer(workflow.read_text(encoding="utf-8")):
            pins.append(SetupPin(workflow.name, match.group(1)))

    assert len(pins) == 14, pins
    assert {pin.sha for pin in pins} == {SETUP_SOLDR_V0_9_82_SHA}, pins
