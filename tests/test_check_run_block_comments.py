"""Regression locks for the folded-run-block guard.

soldr#3018 found the bug this guards against the expensive way: `ci.yml`'s
macOS queue watchdog hid `--grace-seconds 2700` behind a `#` inside a
`run: >-` scalar, so the shell dropped it and the watchdog ran at its 900s
default for as long as that was true. Every failure looked like the flaky
lane it was watching.
"""

from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"


def _module():
    return load_script_module(
        SCRIPTS / "check_run_block_comments.py", "check_run_block_comments"
    )


FOLDED_WITH_COMMENT = """\
jobs:
  a:
    steps:
      - run: >-
          tool --keep 1
          # explanation
          --dropped 2
"""

LITERAL_WITH_COMMENT = """\
jobs:
  a:
    steps:
      - run: |
          # this one is a real shell comment on its own line
          tool --kept 1
"""

FOLDED_CLEAN = """\
jobs:
  a:
    steps:
      # prose belongs here, where `#` is a YAML comment
      - run: >-
          tool --keep 1
          --also-keep 2
"""


def test_folded_block_with_a_comment_is_reported() -> None:
    module = _module()
    found = module.offending_blocks(FOLDED_WITH_COMMENT)
    assert len(found) == 1
    assert "explanation" in found[0].line


def test_literal_block_is_allowed() -> None:
    """`|` keeps newlines, so `#` really is a shell comment there."""
    module = _module()
    assert module.offending_blocks(LITERAL_WITH_COMMENT) == []


def test_folded_block_without_comments_is_allowed() -> None:
    module = _module()
    assert module.offending_blocks(FOLDED_CLEAN) == []


def test_the_repository_is_clean() -> None:
    module = _module()
    assert module.main(["--workflows", str(REPO_ROOT / ".github" / "workflows")]) == 0


def test_the_guard_would_have_caught_the_original_bug() -> None:
    """The exact shape from ci.yml before soldr#3018 fixed it."""
    module = _module()
    original = """\
jobs:
  e2e-macos-x64-queue-watchdog:
    steps:
      - run: >-
          python .github/scripts/target_run_queue_watchdog.py
          --runner "macos-15"
          # a 900s grace was calibrated when x86_64 had a pool to itself.
          --grace-seconds 2700
"""
    found = module.offending_blocks(original)
    assert len(found) == 1
