import importlib.util
import json
from pathlib import Path


def _load_release_completeness():
    """Load the contract helper by path; `.github/scripts` is not a package."""
    path = Path(__file__).parents[1] / ".github" / "scripts" / "release_completeness.py"
    spec = importlib.util.spec_from_file_location("release_completeness", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


release_completeness = _load_release_completeness()

REPO_ROOT = Path(__file__).resolve().parents[1]
RELEASE_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "auto-release.yml"
CONTRACT = REPO_ROOT / "ci" / "canonical-targets.json"


def test_release_workflow_builds_and_publishes_musllinux_wheels() -> None:
    workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

    # soldr#2469 step 2.2: expected wheel names are generated from
    # ci/canonical-targets.json via release_completeness.py (whose parity
    # test pins the musllinux_1_2 tags); the workflow gates must invoke
    # the generator instead of carrying inline name lists. The prepare-side
    # gate now imports the generator instead of shelling to it — its step
    # moved into release_detect.py — so only one CLI call remains in YAML.
    assert "verify_github_release_assets.py" in workflow
    asset_verifier = (
        REPO_ROOT / ".github" / "scripts" / "verify_github_release_assets.py"
    ).read_text(encoding="utf-8")
    assert "RELEASE_COMPLETENESS.expected_github_assets" in asset_verifier
    assert "release_completeness.py" in asset_verifier
    assert "expected_github_assets" in (
        REPO_ROOT / ".github" / "scripts" / "release_detect.py"
    ).read_text(encoding="utf-8")
    assert (
        "uses: zackees/setup-soldr@a07bab94f16124b5c6857b137a237a53a61e06d1" in workflow
    )
    assert "version: 0.9.27" not in workflow  # one pin: setup-soldr's default
    assert "cross-targets: ${{ matrix.setup_target }}" in workflow
    assert "target-wheel-hook" in workflow
    assert ".github/scripts/prepare_release_wheel.py" in workflow
    assert "build_release_wheel.py" in (
        REPO_ROOT / ".github" / "scripts" / "prepare_release_wheel.py"
    ).read_text(encoding="utf-8")
    # soldr#2469 step 2.1: the musl build lanes come from the
    # contract-generated matrix rather than inline workflow entries.
    contract = json.loads(CONTRACT.read_text(encoding="utf-8"))
    musl_triples = {
        entry["triple"]
        for entry in contract["targets"]
        if entry["triple"].endswith("-musl")
        and entry["release"]["status"] == "included"
    }
    assert musl_triples == {
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
    }
    assert "include: ${{ fromJSON(needs.prepare.outputs.build_matrix) }}" in workflow
    assert "name: pypi-soldr-${{ matrix.target }}" in workflow
    assert "Assert linux-musl wheels are tagged musllinux_1_2" in workflow
    assert "wheel_filename_lints.py musllinux" in workflow
    assert "Smoke test musllinux wheel on Alpine" in workflow
    assert ".github/scripts/smoke_musllinux_wheel.py" in workflow
    musllinux_smoke = (
        REPO_ROOT / ".github" / "scripts" / "smoke_musllinux_wheel.py"
    ).read_text(encoding="utf-8")
    assert "alpine:3.20" in musllinux_smoke
    assert "--only-binary=:all:" in musllinux_smoke
    assert (
        'pip install --no-index --only-binary=:all: --find-links /dist "soldr==${EXPECTED_VERSION}"'
        in musllinux_smoke
    )
    assert "uv pip install --python .venv dist/*.whl" not in musllinux_smoke
    # soldr#2469 step 2.2: the PyPI wheel gate moved to
    # `.github/scripts/verify_pypi_wheels.py`, and its expected count is now
    # derived from the contract instead of the literal `expected=8` that used
    # to live here. Assert the derived value, which is the property this line
    # always cared about -- and a stronger one, because the gate now checks
    # wheel *names* rather than just how many files PyPI reports.
    assert "verify_pypi_wheels.py" in workflow
    assert "expected=8" not in workflow
    wheel_gate = (
        REPO_ROOT / ".github" / "scripts" / "verify_pypi_wheels.py"
    ).read_text(encoding="utf-8")
    assert "RELEASE_COMPLETENESS.expected_pypi_files" in wheel_gate
    expected_wheels = release_completeness.expected_pypi_files(
        "v1.2.3", release_completeness.included_triples()
    )
    assert len(expected_wheels) == 8, expected_wheels
    assert any("musllinux_1_2_x86_64" in name for name in expected_wheels)
    assert any("musllinux_1_2_aarch64" in name for name in expected_wheels)

    stale_markers = [
        "PyPI wheel build is gnu-only for now",
        "musl rows skip wheel build by design",
        "expected=5",
        "6 platform wheels",
        "7 platform wheels",
    ]
    for marker in stale_markers:
        assert marker not in workflow
