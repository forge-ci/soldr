"""Unit coverage for the PR-only jobs selected by canonical CI."""

import importlib.util
import sys
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / ".github/scripts/ci_path_policy.py"
SPEC = importlib.util.spec_from_file_location("ci_path_policy", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
# dataclasses resolve string annotations through sys.modules.
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


def test_docs_only_pr_skips_expensive_jobs() -> None:
    outputs = MODULE.select("pull_request", ["README.md", "docs/API.md"])
    assert outputs.output_lines() == ("docs_only=true",)


def test_smoke_paths_do_not_emit_obsolete_job_selectors() -> None:
    outputs = MODULE.select(
        "pull_request",
        [
            "crates/soldr-cache/src/cache_lib/strip_target.rs",
            "action.yml",
        ],
    )
    assert outputs.output_lines() == ("docs_only=false",)
