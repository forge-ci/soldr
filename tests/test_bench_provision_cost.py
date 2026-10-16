"""soldr#3802: the README comparison pre-provisions soldr's tools and reports
the provisioning cost separately instead of charging it to the cold build."""

from __future__ import annotations

import importlib.util
import json
import shutil
import subprocess
import sys
from pathlib import Path
from types import ModuleType

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
BENCH = REPO_ROOT / "bench"
RUN_COMPARISON = BENCH / "run_comparison.sh"


def _script() -> str:
    return RUN_COMPARISON.read_text(encoding="utf-8")


def _function_body(text: str, name: str) -> str:
    start = text.index(f"{name}() {{")
    end = text.index("\n}\n", start) + 3
    return text[start:end]


def test_cold_key_is_kept_and_only_the_label_changes() -> None:
    text = _script()
    label = "Cold build (tools pre-provisioned)"

    # Both label sites change; the key stays "cold" so history rows and the
    # renderer overlays keep matching.
    assert f'"cold" "{label}" "cold"' in text
    assert f'{{key: "cold", label: "{label}", samples: 1}}' in text
    assert 'scenario_key: "cold-preprovisioned"' not in text
    assert text.count('"Cold build"') == 0


def test_soldr_cold_cell_prepares_under_the_same_env_before_the_timed_build() -> None:
    text = _script()
    cold = _function_body(text, "measure_cold")
    build = _function_body(text, "run_build")
    prepare = _function_body(text, "run_prepare")

    assert '[[ "${tool}" == "soldr" ]]' in cold
    assert cold.index("run_prepare") < cold.index('run_build "${tool}"')
    assert "provision_ms=" in cold and "provision_bytes=" in cold
    # Identical env: same cache root, same target dir, same soldr on PATH.
    for fragment in (
        'SOLDR_CACHE_DIR="${cache_dir}"',
        'CARGO_TARGET_DIR="${target_dir}"',
    ):
        assert fragment in prepare and fragment in build
    assert "toolchain prepare" in prepare
    assert "--allow-unpinned" in prepare
    # Only soldr pre-provisions; warm and worktree-share cells are untouched.
    assert "run_prepare" not in _function_body(text, "measure_warm")
    assert "run_prepare" not in _function_body(text, "measure_worktree_share")
    # cache_bytes is reported net of the provisioned tool bytes.
    assert "cache_bytes - provision_bytes" in cold


def test_history_rows_carry_the_provision_fields() -> None:
    assemble = (BENCH / "assemble_benchmark_stats.sh").read_text(encoding="utf-8")
    assert "provision_ms," in assemble
    assert "provision_bytes" in assemble


@pytest.mark.skipif(
    shutil.which("bash") is None or shutil.which("jq") is None,
    reason="needs bash and jq",
)
def test_append_result_emits_provision_fields_null_for_other_tools(
    tmp_path: Path,
) -> None:
    text = _script()
    helpers = _function_body(text, "json_escape") + _function_body(
        text, "append_result"
    )
    out = tmp_path / "results.jsonl"
    program = (
        f'set -euo pipefail\nRESULTS_JSONL="{out}"\n{helpers}\n'
        'append_result b f cold "Cold build (tools pre-provisioned)" cold soldr '
        "soldr cmd 100 5 project 7 3\n"
        'append_result b f cold "Cold build (tools pre-provisioned)" cold bare '
        "bare cmd 100 5 project\n"
        'append_result b f cold "Cold build (tools pre-provisioned)" cold sccache '
        "sccache cmd 100 5 project null null\n"
    )
    subprocess.run(["bash", "-c", program], check=True)
    rows = [json.loads(line) for line in out.read_text().splitlines()]

    assert rows[0]["provision_ms"] == 7 and rows[0]["provision_bytes"] == 3
    assert rows[0]["scenario_key"] == "cold"
    assert rows[1]["provision_ms"] is None and rows[1]["provision_bytes"] is None
    assert rows[2]["provision_ms"] is None
    assert rows[0]["wall_ms"] == 100 and rows[0]["cache_bytes"] == 5


@pytest.fixture
def bench_modules():
    """Load the bench modules by file path (no sys.path import, so mypy and
    pylint resolve nothing at import time), keyed by module name.

    The renderer exits at import without Pillow, which the Lint lane does not
    install, so it is loaded only when Pillow is present; the test that needs
    it skips otherwise."""
    names = ["benchmark_models"]
    if importlib.util.find_spec("PIL") is not None:
        names.append("render_comparison_bars")
    sys.path.insert(0, str(BENCH))
    try:
        loaded: dict[str, ModuleType] = {}
        for name in names:
            spec = importlib.util.spec_from_file_location(name, BENCH / f"{name}.py")
            assert spec is not None and spec.loader is not None
            module = importlib.util.module_from_spec(spec)
            sys.modules[name] = module
            spec.loader.exec_module(module)
            loaded[name] = module
        yield loaded
    finally:
        sys.path.remove(str(BENCH))
        for name in names:
            sys.modules.pop(name, None)


def _row(tool: str, scenario: str, wall: int, **extra: object) -> dict:
    return {
        "benchmark": "rust-only",
        "scenario_key": scenario,
        "tool": tool,
        "wall_ms": wall,
        "cache_bytes": 1024,
        **extra,
    }


def _document(with_provision: bool) -> dict:
    extra = {"provision_ms": 4200, "provision_bytes": 2048} if with_provision else {}
    rows = []
    for tool in ("bare", "sccache", "soldr"):
        rows.append(_row(tool, "cold", 9000, **(extra if tool == "soldr" else {})))
        rows.append(_row(tool, "warm", 3000))
        rows.append(_row(tool, "worktree-share", 2000))
    return {
        "ran_at": "2026-10-16T00:00:00Z",
        "tools": [{"key": "soldr", "label": "soldr"}],
        "results": rows,
    }


def test_provision_ms_round_trips_through_the_models(bench_modules) -> None:
    models = bench_modules["benchmark_models"]

    doc = models.ComparisonDocument.from_json(_document(True))
    soldr_cold = [
        r for r in doc.results if r.tool == "soldr" and r.scenario_key == "cold"
    ]
    assert soldr_cold[0].provision_ms == 4200
    assert soldr_cold[0].provision_bytes == 2048
    assert all(r.provision_ms is None for r in doc.results if r.tool != "soldr")

    # Rows written before the field existed (and explicit nulls) still parse.
    legacy = models.ComparisonDocument.from_json(_document(False))
    assert all(r.provision_ms is None for r in legacy.results)
    # History rows with and without the field both load.
    for comparison in (_document(True)["results"], _document(False)["results"]):
        models.HistoryRow.from_json({"canaries": {}, "comparison": comparison})


def test_both_chart_renders_succeed_with_and_without_provision_ms(
    bench_modules, tmp_path: Path
) -> None:
    pytest.importorskip("PIL")
    renderer = bench_modules["render_comparison_bars"]
    models = bench_modules["benchmark_models"]

    assert renderer.provision_annotation(None) is None
    legacy = models.ComparisonDocument.from_json(_document(False))
    assert all(renderer.provision_annotation(r) is None for r in legacy.results)
    current = models.ComparisonDocument.from_json(_document(True))
    notes = [renderer.provision_annotation(r) for r in current.results]
    assert [n for n in notes if n] == ["+provision 4.200s"]

    renderer.OUT_DIR = tmp_path
    for doc in (legacy, current):
        for benchmark in renderer.BENCHMARKS:
            output = renderer.render_benchmark(doc, benchmark)
            assert output.is_file() and output.stat().st_size > 0
