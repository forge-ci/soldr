#!/usr/bin/env python3
"""Benchmark local PEP 517 editable and wheel installs.

The harness uses temporary virtual environments and temporary project copies,
but deliberately inherits the caller's Cargo, rustup, soldr, and zccache state.
It never deletes or redirects those caches. Each sample records its complete
install log so packaging, Cargo, cache, and linker phases can be inspected
after the wall-clock summary.

Run with ``uv run --no-project --script ci/bench_pep517.py --project PATH``.
Use ``--repetitions 1`` for a quick smoke run; the default is three samples
per scenario so reported medians are meaningful.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from collections.abc import Iterable
from dataclasses import asdict, dataclass
from pathlib import Path

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

PHASE_PATTERNS = {
    "packaging": re.compile(r"wheel|editable|dist-info|setuptools|maturin", re.I),
    "cargo": re.compile(r"cargo|compiling|fresh|finished|rustc", re.I),
    "cache": re.compile(r"zccache|cache hit|cache miss|cached|reuse", re.I),
    "linking": re.compile(r"linker|linking|rust-lld|lld|mold|link\.exe", re.I),
}


@dataclass(frozen=True)
class PhaseCounts:
    """Install-log lines matching each build-phase pattern."""

    packaging: int
    cargo: int
    cache: int
    linking: int


@dataclass(frozen=True)
class Sample:
    """One timed install and where its complete log was written."""

    scenario: str
    repetition: int
    elapsed_s: float
    returncode: int
    command: list[str]
    log: str
    phase_lines: PhaseCounts


@dataclass(frozen=True)
class ScenarioResult:
    """Median wall-clock seconds over the successful samples, plus the samples."""

    median_s: float | None
    samples: list[Sample]


@dataclass(frozen=True)
class KindResults:
    """The three measured scenarios for one install kind."""

    cold: ScenarioResult
    warm_noop: ScenarioResult
    source_change: ScenarioResult


@dataclass(frozen=True)
class Scenarios:
    """Results for the editable and wheel install kinds."""

    editable: KindResults
    wheel: KindResults


@dataclass(frozen=True)
class Versions:
    """Tool versions reported by `<tool> --version`."""

    uv: str
    soldr: str


@dataclass(frozen=True)
class CacheEnvironment:
    """The inherited cache-related environment variables (None when unset)."""

    cargo_home: str | None
    rustup_home: str | None
    cargo_target_dir: str | None
    rustc_wrapper: str | None
    zccache_cache_dir: str | None
    zccache_path_remap: str | None


@dataclass(frozen=True)
class Metadata:
    """Environment and option record for one benchmark run."""

    platform: str
    python: str
    project: str
    versions: Versions
    caches_preserved: bool
    cache_environment: CacheEnvironment
    frontend: str
    no_build_isolation: bool
    touch_staged_artifacts: bool
    force_reinstall: bool
    backend_source: str | None


@dataclass(frozen=True)
class BenchmarkResults:
    """The full benchmark document written to the output JSON file."""

    metadata: Metadata
    scenarios: Scenarios

    def to_json(self) -> str:
        """Serialize, omitting unset `backend_source` and cache variables."""
        document = asdict(self)
        metadata = document["metadata"]
        if self.metadata.backend_source is None:
            del metadata["backend_source"]
        metadata["cache_environment"] = {
            name.upper(): value
            for name, value in metadata["cache_environment"].items()
            if value
        }
        return json.dumps(document, indent=2)


def _python_path(venv: Path) -> Path:
    return venv / ("Scripts/python.exe" if os.name == "nt" else "bin/python")


def _run(
    cmd: list[str], *, cwd: Path | None = None, env: dict[str, str] | None = None
) -> subprocess.CompletedProcess[str]:
    return run_captured(
        cmd,
        cwd=str(cwd) if cwd else None,
        env=env,
        check=False,
        text=True,
        capture_stdout=True,
        stderr=subprocess.STDOUT,
        encoding="utf-8",
        errors="replace",
    )


def _phase_counts(output: str) -> PhaseCounts:
    counts = {
        phase: sum(1 for line in output.splitlines() if pattern.search(line))
        for phase, pattern in PHASE_PATTERNS.items()
    }
    return PhaseCounts(
        packaging=counts["packaging"],
        cargo=counts["cargo"],
        cache=counts["cache"],
        linking=counts["linking"],
    )


def _copy_project(source: Path, destination: Path) -> None:
    ignored = shutil.ignore_patterns(
        ".cache",
        ".cargo",
        ".git",
        ".venv",
        ".claude",
        ".clud",
        ".extern-repos",
        ".pytest_cache",
        ".ruff_cache",
        ".zap",
        "build",
        "fbuild.egg-info",
        "target",
        ".zccache",
        "__pycache__",
    )
    shutil.copytree(source, destination, ignore=ignored)


def _touch_staged_artifacts(project: Path) -> int:
    """Make copied native artifacts newer than their copied source inputs.

    This models a downstream project's normal warm state: Cargo already built
    the command binaries and extension, so this benchmark measures only PEP
    517 packaging. It never touches the caller's checkout.
    """
    candidates = [project / "ci" / "bin"]
    candidates.extend((project / "python").glob("**/_native.*"))
    refreshed = 0
    for candidate in candidates:
        paths: Iterable[Path]
        if candidate.is_dir():
            paths = candidate.rglob("*")
        else:
            paths = (candidate,)
        for path in paths:
            if path.is_file():
                path.touch()
                refreshed += 1
    return refreshed


def _source_change_file(project: Path, requested: str | None) -> Path | None:
    if requested:
        candidate = project / requested
        return candidate if candidate.is_file() else None
    candidates = sorted(project.glob("crates/**/*.rs"))
    return candidates[0] if candidates else None


def _sample(
    *,
    scenario: str,
    repetition: int,
    project: Path,
    venv: Path,
    output_dir: Path,
    reinstall: bool,
    editable: bool,
    frontend: str,
    no_build_isolation: bool,
    backend_source: Path | None,
) -> Sample:
    if frontend == "pip":
        install_args = [
            str(_python_path(venv)),
            "-m",
            "pip",
            "install",
            "--disable-pip-version-check",
            "--no-deps",
        ]
    else:
        install_args = [
            "uv",
            "pip",
            "install",
            "--python",
            str(_python_path(venv)),
            "--no-deps",
        ]
    if no_build_isolation:
        install_args.append("--no-build-isolation")
    if reinstall:
        install_args.append("--force-reinstall" if frontend == "pip" else "--reinstall")
    if editable:
        install_args.append("-e")
    install_args.append(str(project))

    started = time.perf_counter()
    env = os.environ.copy()
    if backend_source:
        source_root = str(backend_source / "src")
        env["PYTHONPATH"] = os.pathsep.join(
            part for part in (source_root, env.get("PYTHONPATH")) if part
        )
    result = _run(install_args, cwd=project, env=env)
    elapsed = time.perf_counter() - started
    log_path = output_dir / "logs" / f"{scenario}-r{repetition}.log"
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log_path.write_text(result.stdout, encoding="utf-8")
    return Sample(
        scenario=scenario,
        repetition=repetition,
        elapsed_s=round(elapsed, 3),
        returncode=result.returncode,
        command=install_args,
        log=str(log_path),
        phase_lines=_phase_counts(result.stdout),
    )


def _prepare_venv(
    root: Path, name: str, *, seed: bool, install_setuptools: bool
) -> Path:
    venv = root / f"venv-{name}"
    args = ["uv", "venv"]
    if seed:
        args.append("--seed")
    args.append(str(venv))
    result = _run(args)
    if result.returncode:
        raise RuntimeError(result.stdout)
    python = _python_path(venv)
    if not python.is_file():
        raise RuntimeError(f"uv did not create the expected interpreter: {python}")
    if install_setuptools:
        result = _run(
            ["uv", "pip", "install", "--python", str(python), "setuptools>=64"]
        )
        if result.returncode:
            raise RuntimeError(
                f"could not install setuptools in benchmark venv:\n{result.stdout}"
            )
    return venv


def _metadata(
    project: Path,
    *,
    frontend: str,
    no_build_isolation: bool,
    touch_staged_artifacts: bool,
    force_reinstall: bool,
    backend_source: Path | None,
) -> Metadata:
    tool_versions = [
        _run(command).stdout.strip()
        for command in (["uv", "--version"], ["soldr", "--version"])
    ]
    return Metadata(
        platform=sys.platform,
        python=sys.version.split()[0],
        project=str(project),
        versions=Versions(uv=tool_versions[0], soldr=tool_versions[1]),
        caches_preserved=True,
        cache_environment=CacheEnvironment(
            cargo_home=os.environ.get("CARGO_HOME"),
            rustup_home=os.environ.get("RUSTUP_HOME"),
            cargo_target_dir=os.environ.get("CARGO_TARGET_DIR"),
            rustc_wrapper=os.environ.get("RUSTC_WRAPPER"),
            zccache_cache_dir=os.environ.get("ZCCACHE_CACHE_DIR"),
            zccache_path_remap=os.environ.get("ZCCACHE_PATH_REMAP"),
        ),
        frontend=frontend,
        no_build_isolation=no_build_isolation,
        touch_staged_artifacts=touch_staged_artifacts,
        force_reinstall=force_reinstall,
        backend_source=str(backend_source) if backend_source else None,
    )


def _median(samples: list[Sample]) -> float | None:
    successful = [
        float(sample.elapsed_s) for sample in samples if sample.returncode == 0
    ]
    return round(statistics.median(successful), 3) if successful else None


def benchmark(
    project: Path,
    output_dir: Path,
    repetitions: int,
    *,
    source_file: str | None,
    frontend: str,
    no_build_isolation: bool,
    backend_source: Path | None,
    touch_staged_artifacts: bool,
    force_reinstall: bool,
) -> BenchmarkResults:
    metadata = _metadata(
        project,
        frontend=frontend,
        no_build_isolation=no_build_isolation,
        touch_staged_artifacts=touch_staged_artifacts,
        force_reinstall=force_reinstall,
        backend_source=backend_source,
    )
    with tempfile.TemporaryDirectory(prefix="soldr-pep517-bench-") as raw:
        temp_root = Path(raw)
        kind_results: dict[str, KindResults] = {}
        for editable in (True, False):
            kind = "editable" if editable else "wheel"
            cold: list[Sample] = []
            warm_noop: list[Sample] = []
            source_change: list[Sample] = []
            for repetition in range(1, repetitions + 1):
                project_copy = temp_root / f"{kind}-project-{repetition}"
                _copy_project(project, project_copy)
                if touch_staged_artifacts:
                    _touch_staged_artifacts(project_copy)
                venv = _prepare_venv(
                    temp_root,
                    f"{kind}-{repetition}",
                    seed=frontend == "pip" or no_build_isolation,
                    install_setuptools=no_build_isolation,
                )
                cold.append(
                    _sample(
                        scenario=f"{kind}-cold",
                        repetition=repetition,
                        project=project_copy,
                        venv=venv,
                        output_dir=output_dir,
                        reinstall=False,
                        editable=editable,
                        frontend=frontend,
                        no_build_isolation=no_build_isolation,
                        backend_source=backend_source,
                    )
                )
                warm_noop.append(
                    _sample(
                        scenario=f"{kind}-warm-noop",
                        repetition=repetition,
                        project=project_copy,
                        venv=venv,
                        output_dir=output_dir,
                        reinstall=force_reinstall,
                        editable=editable,
                        frontend=frontend,
                        no_build_isolation=no_build_isolation,
                        backend_source=backend_source,
                    )
                )
                changed = _source_change_file(project_copy, source_file)
                if changed:
                    with changed.open("a", encoding="utf-8") as stream:
                        stream.write("\n")
                    source_change.append(
                        _sample(
                            scenario=f"{kind}-source-change",
                            repetition=repetition,
                            project=project_copy,
                            venv=venv,
                            output_dir=output_dir,
                            reinstall=force_reinstall,
                            editable=editable,
                            frontend=frontend,
                            no_build_isolation=no_build_isolation,
                            backend_source=backend_source,
                        )
                    )
            kind_results[kind] = KindResults(
                cold=ScenarioResult(_median(cold), cold),
                warm_noop=ScenarioResult(_median(warm_noop), warm_noop),
                source_change=ScenarioResult(_median(source_change), source_change),
            )
    return BenchmarkResults(
        metadata=metadata,
        scenarios=Scenarios(
            editable=kind_results["editable"], wheel=kind_results["wheel"]
        ),
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", type=Path, required=True)
    parser.add_argument(
        "--output", type=Path, default=Path("ci/bench-results/pep517.json")
    )
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--source-file", help="Rust source path relative to --project")
    parser.add_argument(
        "--frontend",
        choices=("uv", "pip"),
        default="uv",
        help="installer to measure (default: uv)",
    )
    parser.add_argument(
        "--no-build-isolation",
        action="store_true",
        help="pass --no-build-isolation to the installer",
    )
    parser.add_argument(
        "--backend-source",
        type=Path,
        help="soldr checkout whose src/ directory is injected for PEP 517 imports",
    )
    parser.add_argument(
        "--touch-staged-artifacts",
        action="store_true",
        help="refresh copied ci/bin and python/**/_native.* mtimes before timing",
    )
    parser.add_argument(
        "--force-reinstall",
        action="store_true",
        help="measure forced replacement instead of the literal pip install . command",
    )
    args = parser.parse_args()
    project = args.project.resolve()
    if not project.is_dir():
        parser.error(f"project directory does not exist: {project}")
    if args.repetitions < 1:
        parser.error("--repetitions must be positive")
    backend_source = args.backend_source.resolve() if args.backend_source else None
    if backend_source and not (backend_source / "src" / "__init__.py").is_file():
        parser.error(
            "--backend-source must be a soldr checkout containing src/soldr/__init__.py"
        )
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    result = benchmark(
        project,
        output.parent,
        args.repetitions,
        source_file=args.source_file,
        frontend=args.frontend,
        no_build_isolation=args.no_build_isolation,
        backend_source=backend_source,
        touch_staged_artifacts=args.touch_staged_artifacts,
        force_reinstall=args.force_reinstall,
    )
    output.write_text(result.to_json() + "\n", encoding="utf-8")
    print(
        json.dumps(
            {
                name: {
                    "cold": cases.cold.median_s,
                    "warm_noop": cases.warm_noop.median_s,
                    "source_change": cases.source_change.median_s,
                }
                for name, cases in (
                    ("editable", result.scenarios.editable),
                    ("wheel", result.scenarios.wheel),
                )
            },
            indent=2,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
