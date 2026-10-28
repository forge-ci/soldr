"""Release CI builds its source drivers through an uncached Soldr."""

from pathlib import Path

import yaml

WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/ci.yml"


def test_source_bootstrap_drivers_use_soldr() -> None:
    workflow = yaml.safe_load(WORKFLOW.read_text())
    found = set()
    for job in workflow["jobs"].values():
        steps = job.get("steps", [])
        for index, step in enumerate(steps):
            name = step.get("name")
            if name not in {"Build bootstrap soldr", "Build local soldr driver"}:
                continue
            found.add(name)
            setup = next(
                previous
                for previous in steps[:index]
                if "setup-soldr@" in str(previous.get("uses", ""))
            )
            config = setup["with"]
            assert str(config["toolchain"]) == "1.98.1"
            for key in (
                "cache",
                "build-cache",
                "target-cache",
                "cargo-registry-cache",
                "soldr-mini-cache",
                "solo-toolchain-cache",
            ):
                assert config[key] is False
            assert config["prebuild-deps"] == "none"
            assert step["env"]["SOLDR_RUSTC_WRAPPER"] == "none"
            assert "soldr cargo build" in step["run"]
            if name == "Build bootstrap soldr":
                assert (
                    setup["if"]
                    == step["if"]
                    == "steps.bootstrap_cache.outputs.cache-hit != 'true'"
                )
                assert "--target x86_64-unknown-linux-gnu" in step["run"]
                assert (
                    'target/x86_64-unknown-linux-gnu/debug/soldr "$RUNNER_TEMP/soldr-bin/soldr"'
                    in step["run"]
                )
                checkout = next(
                    previous
                    for previous in steps
                    if "actions/checkout@" in str(previous.get("uses", ""))
                )
                assert (
                    checkout["with"]["ref"]
                    == "${{ needs.ci-mode.outputs.checkout_sha }}"
                )
    assert found == {"Build bootstrap soldr", "Build local soldr driver"}


def test_release_action_smoke_routes_tool_probes_through_soldr() -> None:
    workflow = WORKFLOW.with_name("setup-soldr-action.yml").read_text()
    assert "soldr cargo --version" in workflow
    assert "soldr rustc --version" in workflow
    assert "\n          cargo --version" not in workflow
    assert "\n          rustc --version" not in workflow
