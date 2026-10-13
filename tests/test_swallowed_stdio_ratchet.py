"""Tests for the swallowed-stdio ratchet (soldr#3388, meta soldr#3389).

Modelled on `tests/test_loc_ratchet.py`: real throwaway git repos rather than
mocked `git`, because the interesting behaviour is about what the *merge
base* held.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest
from conftest import commit_all, git, load_script_module

SCRIPT = (
    Path(__file__).resolve().parents[1]
    / ".github"
    / "scripts"
    / "swallowed_stdio_ratchet.py"
)


@pytest.fixture(scope="module")
def mod():
    return load_script_module(SCRIPT, "swallowed_stdio_ratchet")


def _write(repo: Path, rel: str, content: str) -> None:
    path = repo / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def _evaluate(mod, repo: Path):
    cwd = Path.cwd()
    os.chdir(repo)
    try:
        return mod.evaluate("main")
    finally:
        os.chdir(cwd)


PY_STDERR_DEVNULL = (
    "import subprocess\n"
    "\n"
    "def run():\n"
    "    subprocess.run(['true'], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)\n"
)

PY_STDIN_DEVNULL = (
    "import subprocess\n"
    "\n"
    "def run():\n"
    "    subprocess.run(['true'], stdin=subprocess.DEVNULL)\n"
)


def test_new_devnull_stderr_fails(mod, repo):
    _write(repo, "tool.py", "import subprocess\n")
    commit_all(repo, "base")
    git(repo, "checkout", "-q", "-b", "topic")
    _write(repo, "tool.py", PY_STDERR_DEVNULL)
    commit_all(repo, "swallow stderr")

    result = _evaluate(mod, repo)

    new = result.new

    checked = result.checked
    assert checked == 1
    assert len(new) == 1
    assert new[0].path == "tool.py"
    assert "DEVNULL" in new[0].text


def test_stdin_devnull_passes(mod, repo):
    _write(repo, "tool.py", "import subprocess\n")
    commit_all(repo, "base")
    git(repo, "checkout", "-q", "-b", "topic")
    _write(repo, "tool.py", PY_STDIN_DEVNULL)
    commit_all(repo, "close stdin only")

    result = _evaluate(mod, repo)

    new = result.new
    assert new == []


def test_preexisting_baseline_violation_passes(mod, repo):
    _write(repo, "tool.py", PY_STDERR_DEVNULL)
    commit_all(repo, "base already swallows stderr")
    git(repo, "checkout", "-q", "-b", "topic")
    # Touch the file for an unrelated reason; the violation itself is
    # untouched.
    _write(repo, "tool.py", PY_STDERR_DEVNULL + "\n# unrelated comment\n")
    commit_all(repo, "unrelated edit")

    result = _evaluate(mod, repo)

    new = result.new

    checked = result.checked
    assert checked == 1
    assert new == [], "a violation already on main must not be reported as new"


def test_removal_passes(mod, repo):
    _write(repo, "tool.py", PY_STDERR_DEVNULL)
    commit_all(repo, "base")
    git(repo, "checkout", "-q", "-b", "topic")
    _write(repo, "tool.py", "import subprocess\n")
    commit_all(repo, "fix it")

    result = _evaluate(mod, repo)

    new = result.new
    assert new == []


def test_line_shift_of_existing_violation_is_not_new(mod, repo):
    _write(repo, "tool.py", PY_STDERR_DEVNULL)
    commit_all(repo, "base")
    git(repo, "checkout", "-q", "-b", "topic")
    shifted = "# a new comment up top\n" * 5 + PY_STDERR_DEVNULL
    _write(repo, "tool.py", shifted)
    commit_all(repo, "push the violation down")

    result = _evaluate(mod, repo)

    new = result.new
    assert new == [], "an unrelated insertion above must not look like a new violation"


def test_stdio_ok_marker_exempts_python(mod, repo):
    _write(repo, "tool.py", "import subprocess\n")
    commit_all(repo, "base")
    git(repo, "checkout", "-q", "-b", "topic")
    content = (
        "import subprocess\n"
        "\n"
        "def run():\n"
        "    subprocess.run(\n"
        "        ['true'],\n"
        "        stderr=subprocess.DEVNULL,  # stdio-ok: best-effort cleanup probe\n"
        "    )\n"
    )
    _write(repo, "tool.py", content)
    commit_all(repo, "marked exception")

    result = _evaluate(mod, repo)

    new = result.new
    assert new == []


def test_stdio_ok_marker_with_empty_reason_does_not_exempt(mod, repo):
    _write(repo, "tool.py", "import subprocess\n")
    commit_all(repo, "base")
    git(repo, "checkout", "-q", "-b", "topic")
    content = (
        "import subprocess\n"
        "\n"
        "def run():\n"
        "    subprocess.run(\n"
        "        ['true'],\n"
        "        stderr=subprocess.DEVNULL,  # stdio-ok:\n"
        "    )\n"
    )
    _write(repo, "tool.py", content)
    commit_all(repo, "empty marker")

    result = _evaluate(mod, repo)

    new = result.new
    assert len(new) == 1


def test_shell_command_dash_v_presence_check_exempt(mod, repo):
    _write(repo, "script.sh", "#!/bin/sh\necho hi\n")
    commit_all(repo, "base")
    git(repo, "checkout", "-q", "-b", "topic")
    _write(
        repo,
        "script.sh",
        "#!/bin/sh\nif command -v jq >/dev/null 2>&1; then\n  echo have jq\nfi\n",
    )
    commit_all(repo, "presence check")

    result = _evaluate(mod, repo)

    new = result.new

    checked = result.checked
    assert checked == 1
    assert new == []


def test_shell_devnull_redirect_in_sh_file_fails_when_new(mod, repo):
    _write(repo, "script.sh", "#!/bin/sh\necho hi\n")
    commit_all(repo, "base")
    git(repo, "checkout", "-q", "-b", "topic")
    _write(repo, "script.sh", "#!/bin/sh\nrm -rf build 2>/dev/null\n")
    commit_all(repo, "swallow stderr")

    result = _evaluate(mod, repo)

    new = result.new

    checked = result.checked
    assert checked == 1
    assert len(new) == 1
    assert new[0].path == "script.sh"


def test_workflow_run_block_devnull_fails_when_new(mod, repo):
    base_workflow = (
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n"
        "    steps:\n      - name: build\n        run: echo hi\n"
    )
    _write(repo, ".github/workflows/ci.yml", base_workflow)
    commit_all(repo, "base")
    git(repo, "checkout", "-q", "-b", "topic")
    new_workflow = (
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n"
        "    steps:\n"
        "      - name: build\n"
        "        run: |\n"
        "          set -e\n"
        "          rm -rf build 2>/dev/null\n"
    )
    _write(repo, ".github/workflows/ci.yml", new_workflow)
    commit_all(repo, "swallow stderr in workflow")

    result = _evaluate(mod, repo)

    new = result.new

    checked = result.checked
    assert checked == 1
    assert len(new) == 1
    assert new[0].path == ".github/workflows/ci.yml"
    assert "/dev/null" in new[0].text


RUST_ALLOW_NO_REASON = (
    "use std::process::{Command, Stdio};\n"
    "\n"
    "#[allow(ban_swallowed_child_stdio)]\n"
    "fn quiet() {\n"
    '    let mut c = Command::new("x");\n'
    "    c.stdout(Stdio::null());\n"
    "}\n"
)

RUST_ALLOW_WITH_REASON = (
    "use std::process::{Command, Stdio};\n"
    "\n"
    "// reason: fixture proving the documented escape hatch.\n"
    "#[allow(ban_swallowed_child_stdio)]\n"
    "fn quiet() {\n"
    '    let mut c = Command::new("x");\n'
    "    c.stdout(Stdio::null());\n"
    "}\n"
)


def test_rust_allow_without_reason_fails_when_new(mod, repo):
    _write(repo, "crates/a/src/lib.rs", "fn noop() {}\n")
    commit_all(repo, "base")
    git(repo, "checkout", "-q", "-b", "topic")
    _write(repo, "crates/a/src/lib.rs", RUST_ALLOW_NO_REASON)
    commit_all(repo, "add unreasoned allow")

    result = _evaluate(mod, repo)

    new = result.new

    checked = result.checked
    assert checked == 1
    assert len(new) == 1
    assert new[0].lang == "rust"


def test_rust_allow_with_reason_passes(mod, repo):
    _write(repo, "crates/a/src/lib.rs", "fn noop() {}\n")
    commit_all(repo, "base")
    git(repo, "checkout", "-q", "-b", "topic")
    _write(repo, "crates/a/src/lib.rs", RUST_ALLOW_WITH_REASON)
    commit_all(repo, "add reasoned allow")

    result = _evaluate(mod, repo)

    new = result.new
    assert new == []


def test_main_exits_nonzero_on_new_violation(mod, repo, monkeypatch, capsys):
    _write(repo, "tool.py", "import subprocess\n")
    commit_all(repo, "base")
    git(repo, "checkout", "-q", "-b", "topic")
    _write(repo, "tool.py", PY_STDERR_DEVNULL)
    commit_all(repo, "swallow stderr")

    monkeypatch.chdir(repo)
    code = mod.main(["--base-ref", "main"])
    assert code == 1
    assert "swallowed_stdio_ratchet: FAIL" in capsys.readouterr().err


def test_an_unreachable_base_skips_rather_than_failing(mod, repo, monkeypatch, capsys):
    _write(repo, "tool.py", "import subprocess\n")
    commit_all(repo, "base")

    monkeypatch.chdir(repo)
    code = mod.main(["--base-ref", "origin/does-not-exist"])
    assert code == 0
    assert "skipped" in capsys.readouterr().err


def test_list_mode_reports_full_inventory_without_failing(
    mod, repo, monkeypatch, capsys
):
    _write(repo, "tool.py", PY_STDERR_DEVNULL)
    commit_all(repo, "base")

    monkeypatch.chdir(repo)
    code = mod.main(["--list"])
    assert code == 0
    out = capsys.readouterr().out
    assert "tool.py" in out
    assert "python: 1" in out
