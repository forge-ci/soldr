"""The all-targets workflow and its banner use the managed toolchain."""

import os
import subprocess
import tempfile
from pathlib import Path

import pytest
import yaml

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/cross-compile-all-targets.yml"
BANNER = ROOT / ".github/scripts/print_build_banner.sh"


def run_banner(env):
    with tempfile.TemporaryFile(mode="w+") as output:
        subprocess.run(
            ["bash", str(BANNER), "demo", "debug", "target", "soldr cargo"],
            env=env,
            stdout=output,
            text=True,
            check=True,
        )
        output.seek(0)
        return output.read()


def test_source_bootstrap_installs_soldr_before_build():
    workflow = yaml.safe_load(WORKFLOW.read_text())
    assert workflow["env"]["SOLDR_NO_GLOBAL_DELEGATION"] == "1"
    steps = workflow["jobs"]["bootstrap-and-linux-x86"]["steps"]
    build = next(
        i
        for i, step in enumerate(steps)
        if "Build soldr — debug" in step.get("name", "")
    )
    setup = next(
        step for step in steps[:build] if "setup-soldr@" in str(step.get("uses", ""))
    )
    assert str(setup["with"]["toolchain"]) == "1.98.1"
    assert setup["with"]["cache"] is False
    assert setup["with"]["prebuild-deps"] == "none"
    assert "soldr cargo build" in steps[build]["run"]
    assert "--target x86_64-unknown-linux-gnu" in steps[build]["run"]
    assert steps[build]["env"]["SOLDR_RUSTC_WRAPPER"] == "none"


@pytest.mark.skipif(
    os.name == "nt", reason="the workflow and its shell banner run on Linux"
)
def test_banner_queries_soldr_and_handles_probe_failure(tmp_path, capfd):
    fake = tmp_path / "soldr"
    fake.write_text(
        '#!/bin/sh\nif [ "$1" = rustc ]; then echo probe-failed >&2; exit 7; fi\necho soldr-test\n'
    )
    fake.chmod(0o755)
    forbidden = tmp_path / "rustc"
    forbidden.write_text("#!/bin/sh\necho WRONG-RUSTC\n")
    forbidden.chmod(0o755)
    env = dict(os.environ, PATH=f"{tmp_path}:/usr/bin:/bin")
    output = run_banner(env)
    assert "WRONG-RUSTC" not in output
    assert "soldr rustc unavailable" in output
    assert "soldr-test" in output
    assert "probe-failed" in capfd.readouterr().err


@pytest.mark.skipif(
    os.name == "nt", reason="the workflow and its shell banner run on Linux"
)
def test_banner_reports_missing_soldr_without_using_rustc():
    env = dict(os.environ, PATH="/usr/bin:/bin")
    output = run_banner(env)
    assert "soldr rustc unavailable" in output
    assert "soldr not on PATH yet" in output


@pytest.mark.skipif(
    os.name == "nt", reason="the workflow and its shell banner run on Linux"
)
def test_banner_uses_soldr_compiler_probe(tmp_path):
    fake = tmp_path / "soldr"
    fake.write_text(
        '#!/bin/sh\nif [ "$1" = rustc ]; then echo "rustc 1.98.1"; '
        'else echo "soldr 0.9"; fi\n'
    )
    fake.chmod(0o755)
    env = dict(os.environ, PATH=f"{tmp_path}:/usr/bin:/bin")
    output = run_banner(env)
    assert "rustc      : rustc 1.98.1" in output
    assert "soldr      : soldr 0.9" in output
