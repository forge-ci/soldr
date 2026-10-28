"""Archived target tests must enter the target-local Soldr replay command."""

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def test_target_and_recovery_replay_enter_soldr() -> None:
    workflow = (ROOT / ".github/workflows/_ci-target-run.yml").read_text()
    recovery = (ROOT / "ci/macos_recovery_run.py").read_text()
    assert '"$SOLDR_BIN" replay-nextest --runner "$NEXTEST_BIN"' in workflow
    assert '"$SOLDR_BIN" replay-nextest --runner "$NEXTEST_BIN"' in recovery
    assert '"$NEXTEST_BIN" nextest' not in workflow
    assert '"$NEXTEST_BIN" nextest' not in recovery
