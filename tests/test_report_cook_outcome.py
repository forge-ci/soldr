"""Regression locks for the cook outcome reporter.

soldr#3008: cook crashed on every run of the only enabled lane for an
unknown period and nothing said so, because the action degrades to
"continuing without cooked deps". The lane stayed green while the cache
was never written. This reporter exists so that state is visible.
"""

import os
from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"


def _module():
    return load_script_module(SCRIPTS / "report_cook_outcome.py", "report_cook_outcome")


def test_neither_ran_nor_hit_is_a_warning() -> None:
    """The soldr#3008 state: cook produced nothing and nobody knew."""
    module = _module()
    outcome = module.classify(ran=False, hit=False, layer="none")
    level, message = outcome.level, outcome.message
    assert level == "warning"
    assert "no cook cache will be written" in message
    assert "soldr#3008" in message


def test_a_cache_hit_is_a_notice_not_a_warning() -> None:
    module = _module()
    level = module.classify(ran=False, hit=True, layer="none").level
    assert level == "notice"


def test_a_successful_cook_run_is_a_notice_and_names_the_layer() -> None:
    module = _module()
    outcome = module.classify(ran=True, hit=False, layer="base")
    level, message = outcome.level, outcome.message
    assert level == "notice"
    assert "base" in message


def test_truthy_only_accepts_the_actions_spelling() -> None:
    """The action emits the literal strings 'true'/'false'."""
    module = _module()
    assert module.truthy("true")
    assert module.truthy("TRUE")
    assert not module.truthy("false")
    assert not module.truthy("")
    assert not module.truthy(None)
    # A missing output must never be read as success.
    assert not module.truthy("1")


def test_reporter_never_fails_the_lane(tmp_path) -> None:
    """A reporter that can fail the build it reports on is worse than none."""
    module = _module()
    os.environ["COOK_RAN"] = "false"
    os.environ["COOK_HIT"] = "false"
    os.environ["GITHUB_STEP_SUMMARY"] = str(tmp_path / "missing" / "summary.md")
    try:
        assert module.main(["--target", "x86_64-pc-windows-gnu"]) == 0
    finally:
        for key in ("COOK_RAN", "COOK_HIT", "GITHUB_STEP_SUMMARY"):
            os.environ.pop(key, None)


def test_summary_records_the_target_and_outcome(tmp_path) -> None:
    module = _module()
    summary = tmp_path / "summary.md"
    os.environ["COOK_RAN"] = "true"
    os.environ["COOK_HIT"] = "false"
    os.environ["COOK_LAYER"] = "delta"
    os.environ["GITHUB_STEP_SUMMARY"] = str(summary)
    try:
        module.main(["--target", "x86_64-unknown-linux-gnu"])
    finally:
        for key in ("COOK_RAN", "COOK_HIT", "COOK_LAYER", "GITHUB_STEP_SUMMARY"):
            os.environ.pop(key, None)
    body = summary.read_text(encoding="utf-8")
    assert "x86_64-unknown-linux-gnu" in body
    assert "delta" in body
