"""soldr#2469 6.2: repository settings drift is reported, never silently passed."""

from __future__ import annotations

import json
from pathlib import Path

from conftest import load_script_module

ROOT = Path(__file__).resolve().parent.parent
AUDIT = load_script_module(
    ROOT / ".github" / "scripts" / "settings_audit.py", "settings_audit"
)
BASELINE = json.loads((ROOT / "ci" / "repo-settings.json").read_text(encoding="utf-8"))
REPO = "zackees/soldr"


def live(**overrides: object):
    defaults = {
        f"/repos/{REPO}": dict(BASELINE["repository"]),
        f"/repos/{REPO}/immutable-releases": {
            "enabled": True,
            "enforced_by_owner": False,
        },
        f"/repos/{REPO}/actions/permissions": {
            "enabled": True,
            "allowed_actions": "all",
            "sha_pinning_required": True,
        },
        f"/repos/{REPO}/actions/permissions/workflow": dict(
            BASELINE["workflow_permissions"]
        ),
        f"/repos/{REPO}/branches/main/protection": None,
    }
    answers = {**defaults, **overrides}

    def fetch(path: str) -> object:
        value = answers[path]
        if isinstance(value, Exception):
            raise value
        return value

    return fetch


def test_matching_settings_report_nothing() -> None:
    assert not AUDIT.audit(REPO, BASELINE, live())


def test_a_changed_setting_is_drift() -> None:
    changed = live(
        **{
            f"/repos/{REPO}/actions/permissions": {
                "enabled": True,
                "sha_pinning_required": False,
            }
        }
    )
    (finding,) = AUDIT.audit(REPO, BASELINE, changed)
    assert (
        finding.kind == "drift"
        and finding.setting == "actions_permissions.sha_pinning_required"
    )


def test_newly_protected_main_is_drift() -> None:
    protected = live(**{f"/repos/{REPO}/branches/main/protection": {"url": "x"}})
    (finding,) = AUDIT.audit(REPO, BASELINE, protected)
    assert finding.setting == "branch_protected" and finding.kind == "drift"


def test_an_unreadable_endpoint_is_unverified_not_passed() -> None:
    hidden = live(**{f"/repos/{REPO}/immutable-releases": AUDIT.Unreadable("HTTP 403")})
    (finding,) = AUDIT.audit(REPO, BASELINE, hidden)
    assert finding.kind == "unverified"


def test_only_drift_fails_the_audit(monkeypatch) -> None:
    hidden = live(**{f"/repos/{REPO}/immutable-releases": AUDIT.Unreadable("HTTP 403")})
    monkeypatch.setattr(AUDIT, "http_fetch", lambda _token: hidden)
    assert AUDIT.main(["--repo", REPO]) == 0
    drifted = live(
        **{f"/repos/{REPO}": {**BASELINE["repository"], "allow_auto_merge": True}}
    )
    monkeypatch.setattr(AUDIT, "http_fetch", lambda _token: drifted)
    assert AUDIT.main(["--repo", REPO]) == 1
