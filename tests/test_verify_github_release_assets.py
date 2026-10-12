"""Tests for the extracted GitHub Release asset gate (soldr#2469)."""

from __future__ import annotations

from pathlib import Path
from typing import Any

from conftest import load_script_module

ROOT = Path(__file__).parents[1]
verify = load_script_module(
    ROOT / ".github" / "scripts" / "verify_github_release_assets.py",
    "verify_github_release_assets",
)


def complete_release(tag: str = "v0.9.2") -> dict[str, Any]:
    return {
        "isDraft": False,
        "assets": [
            {"name": name, "size": 1}
            for name in verify.RELEASE_COMPLETENESS.expected_github_assets(
                tag, verify.RELEASE_COMPLETENESS.included_triples()
            )
        ],
    }


def test_complete_published_release_passes() -> None:
    assert verify.verify_release_assets("v0.9.2", complete_release()) == []


def test_draft_and_missing_assets_are_named() -> None:
    failures = verify.verify_release_assets("v0.9.2", {"isDraft": True, "assets": []})
    assert failures[0] == "GitHub release v0.9.2 is still a draft"
    assert len(failures) == 19
    assert any("x86_64-unknown-linux-gnu.tar.zst" in item for item in failures)


def test_zero_sized_asset_is_rejected() -> None:
    release = complete_release()
    release["assets"][0]["size"] = 0
    failures = verify.verify_release_assets("v0.9.2", release)
    assert failures == [
        "GitHub release asset soldr-v0.9.2-x86_64-pc-windows-msvc.tar.zst "
        "has invalid size 0"
    ]


def test_main_reports_fixture_result(monkeypatch, capsys) -> None:
    monkeypatch.setattr(verify, "fetch_release", lambda _tag, _repo: complete_release())
    assert verify.main(["--version", "0.9.2", "--repo", "example/soldr"]) == 0
    assert "v0.9.2 has 18 expected assets" in capsys.readouterr().out
