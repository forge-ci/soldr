"""Tests for the protocol_v3/client_v3 guard (soldr#2360/#2363).

The design deliberately keeps the broker wire at v2, broken in place, rather
than introducing a v3 module -- the minimum-version floor is what forces
integrators to upgrade, not a coexisting new major. These tests pin the two
ways this check could pass for the wrong reason: a file that isn't scanned
at all, and a near-miss identifier (`protocol_v30`) that should not trip it.
"""

from __future__ import annotations

from pathlib import Path

from _script_loader import load_script_module

SCRIPT = Path(__file__).resolve().parent / "no_protocol_v3.py"
no_protocol_v3 = load_script_module(SCRIPT, name="no_protocol_v3")


def _write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def test_clean_tree_reports_no_findings(tmp_path: Path) -> None:
    _write(
        tmp_path / "crates" / "soldr-daemon" / "src" / "broker.rs",
        "use running_process::broker::protocol_v2::CacheManifest;\n",
    )
    findings = no_protocol_v3.scan(("crates",), tmp_path)
    assert findings == []


def test_detects_protocol_v3_module_path(tmp_path: Path) -> None:
    _write(
        tmp_path / "crates" / "soldr-daemon" / "src" / "broker.rs",
        "pub mod protocol_v3;\n",
    )
    findings = no_protocol_v3.scan(("crates",), tmp_path)
    assert len(findings) == 1
    assert findings[0].lineno == 1


def test_does_not_false_positive_on_near_miss_identifiers(tmp_path: Path) -> None:
    _write(
        tmp_path / "crates" / "soldr-daemon" / "src" / "broker.rs",
        "// protocol_v30 and my_client_v3x are not the banned names\n"
        "let client_v33 = 1;\n",
    )
    findings = no_protocol_v3.scan(("crates",), tmp_path)
    assert findings == []


def test_ignores_roots_that_do_not_exist(tmp_path: Path) -> None:
    findings = no_protocol_v3.scan(("nonexistent-root",), tmp_path)
    assert findings == []


def test_main_returns_nonzero_on_violation(tmp_path: Path, capsys) -> None:
    _write(
        tmp_path / "crates" / "soldr-daemon" / "src" / "broker.rs",
        "pub mod protocol_v3;\n",
    )
    code = no_protocol_v3.main(["--roots", "crates", "--repo-root", str(tmp_path)])
    assert code == 1
    err = capsys.readouterr().err
    assert "protocol_v3" in err


def test_main_returns_zero_on_clean_tree(tmp_path: Path, capsys) -> None:
    _write(
        tmp_path / "crates" / "soldr-daemon" / "src" / "broker.rs",
        "use running_process::broker::protocol_v2::CacheManifest;\n",
    )
    code = no_protocol_v3.main(["--roots", "crates", "--repo-root", str(tmp_path)])
    assert code == 0
