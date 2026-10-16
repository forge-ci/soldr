"""Release scripts that load `release_completeness.py` must run as files.

`release-auto.yml` runs these scripts directly (`uv run ... <script>.py`), not
through `conftest.load_script_module`. Each one loads its sibling
`release_completeness.py` with importlib, and that helper defines dataclasses,
which resolve their own module through `sys.modules` while the class is being
created. A loader that skips the `sys.modules` registration imports fine under
pytest whenever another test registered `release_completeness` first, and
crashes in CI: the v0.9.33 release published its GitHub Release and then died
in `Verify GitHub Release assets` with `AttributeError: 'NoneType' object has
no attribute '__dict__'`, skipping PyPI and npm.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import pytest

SCRIPTS = Path(__file__).resolve().parents[1] / ".github" / "scripts"


@pytest.mark.parametrize(
    "script",
    ["verify_github_release_assets.py", "validate_npm_release_recovery.py"],
)
def test_script_imports_release_completeness_when_run_directly(script: str) -> None:
    result = subprocess.run(
        [sys.executable, str(SCRIPTS / script), "--help"],
        capture_output=True,
        text=True,
        check=False,
        timeout=60,
    )
    assert result.returncode == 0, result.stderr
    assert "usage:" in result.stdout
