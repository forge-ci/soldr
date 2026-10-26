"""Complete failed-check logs survive the bounded console tail (soldr#3564).

The gate captures a check's output in a temporary file, prints only the last
`LOG_TAIL_CHARS` characters when the check fails, and discards the capture.
The head of a long failure -- the failing assertions of a 3,500-test nextest
run, exactly the evidence needed to triage it -- was gone before anyone could
read it, and the outer `ci-lint` lane log, which receives only what this
prints, could not recover it either.

These tests pin the fix: the complete output lands in a durable file under
the resolved git directory, the exact path is echoed on failure, the console
stays bounded, and the exit code is unchanged. The log location is resolved
through `git rev-parse --git-dir`, so a linked worktree (where `.git` is a
file) gets its own correct, owned directory, and a tree with no git directory
at all degrades to a warning instead of a crash.
"""

from __future__ import annotations

import sys
from pathlib import Path
from typing import NoReturn

import pytest
from conftest import git, load_script_module

ROOT = Path(__file__).resolve().parent.parent
GATE = load_script_module(ROOT / "ci" / "local_gate.py", "soldr_local_gate")

# The marker sits at the HEAD of the fake child's output: a "durable" log
# that kept only the tail would drop it while the bounded-console assertions
# still passed -- which is exactly the loss soldr#3564 reported.
MARKER = "FAIL_MARKER: the assertion detail that used to be lost"
TAIL_CHARS = 20000
HEAD_CHARS = 40000

# The marker is assembled at runtime so it never appears in the child's
# command line: `main` echoes `$ <argv>`, and a literal marker there would
# make the "not in the console" assertion fail for the wrong reason.
_FAILING_CHILD = (
    "import sys\n"
    'm = "FAIL" + "_MARKER: the assertion detail that used to be lost"\n'
    "sys.stdout.write(m + chr(10))\n"
    f"sys.stdout.write('x' * {HEAD_CHARS} + chr(10))\n"
    "sys.exit(3)\n"
)
_PASSING_CHILD = (
    "import sys\nsys.stdout.write('quiet success detail' + chr(10))\nsys.exit(0)\n"
)


def _failing_check(name: str = "fake failing check", *, nonce: bool = False) -> object:
    return GATE.Check(
        name,
        (sys.executable, "-c", _FAILING_CHILD),
        "guards",
        tree_nonce=nonce,
    )


def _raise_oserror(*_args: object, **_kwargs: object) -> NoReturn:
    raise OSError("simulated unwritable temp directory")


def test_failed_check_persists_complete_output_and_prints_a_bounded_tail(
    tmp_path: Path, capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    git_dir = tmp_path / ".git"
    git_dir.mkdir()
    monkeypatch.setattr(GATE, "_git_dir", lambda: git_dir)
    monkeypatch.setattr(GATE, "checks", lambda: [_failing_check()])

    code = GATE.main([])

    captured = capsys.readouterr()
    out = captured.out
    assert code == 1  # exit behavior is unchanged
    assert GATE.LOG_TAIL_CHARS == TAIL_CHARS
    logs = sorted((git_dir / "local-gate" / "logs").rglob("*.log"))
    assert len(logs) == 1  # one durable file for the one failed check
    assert str(logs[0]) in out  # the exact path is echoed
    body = logs[0].read_text(encoding="utf-8")
    assert MARKER in body  # the head of the output survives...
    assert "x" * HEAD_CHARS in body  # ...completely
    assert MARKER not in out  # the console shows a tail, not the head
    assert "x" * TAIL_CHARS in out
    assert "x" * (TAIL_CHARS + 1) not in out  # ...and it stays bounded
    assert "last 20000 of" in out  # the truncation is labeled, not silent


def test_each_run_gets_its_own_log_directory(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    git_dir = tmp_path / ".git"
    git_dir.mkdir()
    monkeypatch.setattr(GATE, "_git_dir", lambda: git_dir)

    first = GATE.prepare_log_dir()
    second = GATE.prepare_log_dir()

    assert first is not None and second is not None
    assert first != second  # parallel gate runs cannot share a file path
    assert first.parent == second.parent == git_dir / "local-gate" / "logs"
    assert first.is_dir() and second.is_dir()


def test_git_dir_resolves_in_a_clone_and_a_linked_worktree(
    repo: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    git(repo, "commit", "-q", "--allow-empty", "-m", "base")
    monkeypatch.setattr(GATE, "ROOT", repo)
    assert GATE._git_dir() == (repo / ".git").resolve()

    linked = tmp_path / "linked"
    git(repo, "worktree", "add", "-q", "-b", "gate-log-wt", str(linked))
    assert (linked / ".git").is_file()  # the shape path arithmetic gets wrong
    monkeypatch.setattr(GATE, "ROOT", linked)

    linked_git_dir = GATE._git_dir()
    assert linked_git_dir is not None and linked_git_dir.is_dir()
    assert linked_git_dir != (repo / ".git").resolve()
    assert linked.name in linked_git_dir.parts  # .git/worktrees/<name>: per-worktree

    log_dir = GATE.prepare_log_dir()
    assert log_dir is not None
    assert log_dir.is_relative_to(linked_git_dir)
    probe = log_dir / "owned.log"
    probe.write_text("durable\n", encoding="utf-8")
    assert probe.read_text(encoding="utf-8") == "durable\n"


def test_a_missing_git_dir_degrades_to_a_temp_log_dir(
    tmp_path: Path, capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    plain = tmp_path / "plain"
    plain.mkdir()
    monkeypatch.setattr(GATE, "ROOT", plain)
    # GIT_DIR points nowhere, so the answer does not depend on whether some
    # ancestor of the temp directory happens to be a repository.
    monkeypatch.setenv("GIT_DIR", str(tmp_path / "no-such-git-dir"))

    log_dir = GATE.prepare_log_dir()

    err = capsys.readouterr().err
    assert log_dir is not None and log_dir.is_dir()
    assert "warning" in err
    assert log_dir.name.startswith("soldr-local-gate-logs-")
    # still durable: a failed check can write its complete output there
    probe = log_dir / "probe.log"
    probe.write_text("kept\n", encoding="utf-8")
    assert probe.read_text(encoding="utf-8") == "kept\n"


def test_an_unwritable_log_location_does_not_crash_the_gate(
    tmp_path: Path, capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(GATE, "_git_dir", lambda: None)
    monkeypatch.setattr(GATE.tempfile, "mkdtemp", _raise_oserror)
    monkeypatch.setattr(GATE, "checks", lambda: [_failing_check()])

    code = GATE.main([])

    captured = capsys.readouterr()
    assert code == 1  # the failure still fails, with or without a log
    assert "FAIL: fake failing check" in captured.out
    assert "full log: unavailable" in captured.out
    assert "x" * TAIL_CHARS in captured.out  # the bounded tail still prints
    assert "warning" in captured.err


def test_a_nonce_proof_failure_persists_its_provenance(
    tmp_path: Path, capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    git_dir = tmp_path / ".git"
    git_dir.mkdir()
    monkeypatch.setattr(GATE, "_git_dir", lambda: git_dir)
    monkeypatch.setattr(
        GATE, "ROOT", tmp_path
    )  # the nonce file lands in the throwaway tree
    # Runs, exits clean, never echoes the nonce: `_run` rewrites this into a
    # failure whose added provenance must reach the durable log as well.
    # zackees/ci.yml#414: a missing echo is NOT another worktree's blame --
    # the verdict names the exit code, suggests a rerun, and must never carry
    # the destructive "stop the other session's container" advice.
    child = "import sys\nsys.exit(0)\n"
    check = GATE.Check(
        "fake nonce check",
        (sys.executable, "-c", child),
        "tests",
        tree_nonce=True,
    )
    monkeypatch.setattr(GATE, "checks", lambda: [check])

    code = GATE.main([])

    out = capsys.readouterr().out
    assert code == 1  # the proof failure is a failure, as before
    logs = sorted((git_dir / "local-gate" / "logs").rglob("*.log"))
    assert len(logs) == 1
    assert str(logs[0]) in out
    body = logs[0].read_text(encoding="utf-8")
    assert (
        "exited 0 without echoing a nonce" in body
    )  # the #414 rewrite reaches the log...
    assert GATE.NONCE_MARKER in body  # ...with its expected/seen provenance
    assert "zackees/ci.yml#414" in body
    assert "Stop or remove" not in body, (
        "no-echo must never blame another worktree's container"
    )
    assert not (tmp_path / GATE.NONCE_FILE).exists()  # nonce cleanup unchanged


def test_a_different_nonce_keeps_the_other_worktree_blame(
    tmp_path: Path, capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    """zackees/ci.yml#414: ONLY a runner that echoed a different nonce gets
    the wrong-worktree verdict with its (destructive, and here correct)
    container advice."""

    git_dir = tmp_path / ".git"
    git_dir.mkdir()
    monkeypatch.setattr(GATE, "_git_dir", lambda: git_dir)
    monkeypatch.setattr(GATE, "ROOT", tmp_path)
    child = f"print({GATE.NONCE_MARKER!r} + '{'0' * 32}')\n"
    check = GATE.Check(
        "fake other-tree check", (sys.executable, "-c", child), "tests", tree_nonce=True
    )
    monkeypatch.setattr(GATE, "checks", lambda: [check])

    assert GATE.main([]) == 1
    body = sorted((git_dir / "local-gate" / "logs").rglob("*.log"))[0].read_text(
        encoding="utf-8"
    )
    assert "proved a DIFFERENT worktree" in body
    assert "Stop or remove" in body  # the advice is correct for THIS case


def test_a_failed_runner_reports_its_exit_code_and_suggests_a_rerun(
    tmp_path: Path, capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    """zackees/ci.yml#414: the soldr 2026-10-22 case -- bosn setup died
    (reaped Docker output) before any test ran, no nonce was ever echoed,
    and the old verdict told the operator to destroy a peer's container.
    Now: exit code, rerun, explicitly not another worktree."""

    git_dir = tmp_path / ".git"
    git_dir.mkdir()
    monkeypatch.setattr(GATE, "_git_dir", lambda: git_dir)
    monkeypatch.setattr(GATE, "ROOT", tmp_path)
    child = "import sys\nprint('bosn run: ensure of stack cook failed (job 602)')\nsys.exit(2)\n"
    check = GATE.Check(
        "fake dying runner", (sys.executable, "-c", child), "tests", tree_nonce=True
    )
    monkeypatch.setattr(GATE, "checks", lambda: [check])

    assert GATE.main([]) == 1
    capsys.readouterr()
    body = sorted((git_dir / "local-gate" / "logs").rglob("*.log"))[0].read_text(
        encoding="utf-8"
    )
    assert "failed with exit 2" in body
    assert "Rerun" in body
    assert "not evidence of another worktree" in body
    assert "Stop or remove" not in body
    assert (
        "bosn run: ensure of stack cook failed" in body
    )  # the runner's own error stays first


def test_successful_checks_write_no_logs_and_stay_concise(
    tmp_path: Path, capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    git_dir = tmp_path / ".git"
    git_dir.mkdir()
    monkeypatch.setattr(GATE, "_git_dir", lambda: git_dir)
    check = GATE.Check(
        "fake passing check",
        (sys.executable, "-c", _PASSING_CHILD),
        "guards",
    )
    monkeypatch.setattr(GATE, "checks", lambda: [check])

    code = GATE.main([])

    out = capsys.readouterr().out
    assert code == 0
    assert "ok" in out
    assert "quiet success detail" not in out  # successful output stays off the console
    assert "full log" not in out
    assert not (git_dir / "local-gate").exists()  # no log machinery runs at all
