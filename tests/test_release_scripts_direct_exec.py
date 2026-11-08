"""Release scripts that load `release_completeness.py` must run as files.

`auto-release.yml` runs these scripts directly (`uv run ... <script>.py`), not
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
import tempfile
from pathlib import Path

import pytest

SCRIPTS = Path(__file__).resolve().parents[1] / ".github" / "scripts"


@pytest.mark.parametrize(
    "script",
    ["verify_github_release_assets.py", "validate_npm_release_recovery.py"],
)
def test_script_imports_release_completeness_when_run_directly(script: str) -> None:
    # Temporary files, not pipes (PY-003).
    with tempfile.TemporaryFile("w+") as out, tempfile.TemporaryFile("w+") as err:
        result = subprocess.run(
            [sys.executable, str(SCRIPTS / script), "--help"],
            stdout=out,
            stderr=err,
            text=True,
            check=False,
            timeout=60,
        )
        out.seek(0)
        err.seek(0)
        stdout, stderr = out.read(), err.read()
    assert result.returncode == 0, stderr
    assert "usage:" in stdout
