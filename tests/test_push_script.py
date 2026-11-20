"""./push always attests before it pushes (ci.yml GATE-014)."""

from __future__ import annotations

import importlib.machinery
import importlib.util
from pathlib import Path
from types import ModuleType

import pytest

ROOT = Path(__file__).resolve().parents[1]


def _push() -> ModuleType:
    loader = importlib.machinery.SourceFileLoader("soldr_push", str(ROOT / "push"))
    spec = importlib.util.spec_from_loader("soldr_push", loader)
    assert spec is not None
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


def test_push_uses_the_ci_toml_ci_lint_pin() -> None:
    push = _push()
    assert any(push.CI_LINT_REF in part for part in push.CI_LINT)


@pytest.mark.parametrize("branch", ["main", "master", "HEAD"])
def test_push_refuses_protected_or_detached(branch: str) -> None:
    assert "feature branch" in (_push().refusal(branch) or "")


def test_gate_runs_before_any_push(monkeypatch: pytest.MonkeyPatch) -> None:
    """A failing gate exits before `git push` is ever attempted."""
    push = _push()
    calls: list[list[str]] = []
    monkeypatch.setattr(push, "git", lambda *a: "feat/x" if "--abbrev-ref" in a else "")

    def failing_step(argv: list[str]) -> int:
        calls.append(argv)
        return 1

    monkeypatch.setattr(push, "step", failing_step)
    assert push.main([]) == 1
    assert calls and calls[0][-2:] == ["local-gate", "run"]
    assert not any(argv[:2] == ["git", "push"] for argv in calls)


def test_push_has_no_attestation_bypass() -> None:
    text = (ROOT / "push").read_text(encoding="utf-8")
    assert "--no-proof" not in text and "--no-attest" not in text


def test_verify_names_the_stamped_head(monkeypatch: pytest.MonkeyPatch) -> None:
    """Outside Actions `local-gate verify` needs --sha; it must be HEAD after the gate."""
    push = _push()
    calls: list[list[str]] = []

    def fake_git(*args: str) -> str:
        if "--abbrev-ref" in args:
            return "feat/x"
        return "f" * 40 if args == ("rev-parse", "HEAD") else ""

    def ok_step(argv: list[str]) -> int:
        calls.append(argv)
        return 0

    monkeypatch.setattr(push, "git", fake_git)
    monkeypatch.setattr(push, "step", ok_step)
    assert push.main(["--prove-only"]) == 0
    verify = next(argv for argv in calls if "verify" in argv)
    assert verify[verify.index("--sha") + 1] == "f" * 40
    assert verify[verify.index("--event") + 1] == "pull_request"
    assert "--base-sha" in verify
