"""Recovery guest and explicitly opted-in hosted macOS lane contracts.

This replaces tests/test_macos_dockur_lane_contract.py, whose contract (a
hand-baked dockur/macos image pulled from GHCR over ssh, soldr#3071) never
worked: the image was never published and the ssh secret was never set, so
`e2e-macos-x64` and `smoke_macos_x64` failed at preflight on every run.
soldr#3076 replaces that whole plan with zackees/docker-mac-x64, which needs
neither.
"""

import json
import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = REPO_ROOT / ".github" / "workflows"

# A `runs-on:`/`runs_on:`/`runner:` YAML value (bare or quoted, matrix-row
# style or JSON-style `"runner": "..."`) naming a macos-* runner label.
# Comment-only lines are excluded below.
RUNNER_LABEL_PATTERN = re.compile(
    r'(?:runs[-_]on|"?runner"?)\s*:\s*"?macos-[a-z0-9.]+', re.IGNORECASE
)

DOCKER_MAC_X64_PIN = "0769c0bcc9367c6516da39c0a3aa4cb55988ec79"


def _non_comment_lines(text: str) -> list[str]:
    return [line for line in text.splitlines() if not line.strip().startswith("#")]


def test_hosted_macos_runner_labels_are_confined_to_opt_in_and_release() -> None:
    offenders = []
    for workflow in sorted(WORKFLOWS.glob("*.y*ml")):
        for line in _non_comment_lines(workflow.read_text(encoding="utf-8")):
            if RUNNER_LABEL_PATTERN.search(line):
                offenders.append(f"{workflow.name}: {line.strip()}")
    assert offenders == [
        "auto-release.yml: runs-on: macos-15-intel",
        "auto-release.yml: runs-on: macos-15",
        "ci.yml: runs_on: macos-15-intel",
        "ci.yml: runs_on: macos-15",
        # soldr#3276 §6: reld live-fetch evidence job. Uses the same hosted
        # macos-15 runner the existing full-mode macOS e2e lanes already use
        # (this job depends on e2e-macos-arm64-build and only runs when that
        # succeeded), so it adds no new runner exposure beyond what full CI
        # already provisions.
        "ci.yml: runs-on: macos-15",
    ]


SCAN_ROOTS = (".github", "ci", "docs", "tests")
SCAN_TOP_LEVEL_FILES = ("README.md",)


def test_no_ghcr_baked_guest_image_or_ssh_secret_anywhere() -> None:
    """soldr#3071's hand-baked guest was never published; soldr#3076 dropped it.

    Scoped to the directories that could plausibly reference it (workflow
    config, ci scripts, docs, and this test suite) rather than the whole
    checkout -- `target/` alone is tens of gigabytes of unrelated build
    output.
    """
    this_file = Path(__file__).resolve()
    offenders = []
    paths = [
        p
        for root in SCAN_ROOTS
        for p in sorted((REPO_ROOT / root).rglob("*"))
        if p.is_file() and p.resolve() != this_file
    ]
    paths += [REPO_ROOT / name for name in SCAN_TOP_LEVEL_FILES]
    for path in paths:
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, PermissionError):
            continue
        if "ghcr.io/zackees/soldr/macos-x64-guest" in text:
            offenders.append(f"{path.relative_to(REPO_ROOT)}: baked guest image string")
        if "SOLDR_MACOS_GUEST_SSH_KEY" in text:
            offenders.append(f"{path.relative_to(REPO_ROOT)}: guest ssh secret name")
    assert not offenders, offenders


def test_x64_lane_uses_the_recovery_guest_on_an_ubuntu_runner() -> None:
    replay = (WORKFLOWS / "macos-recovery-replay.yml").read_text(encoding="utf-8")
    start = replay.index("  replay:\n")
    run_job = replay[start:]
    assert "runs_on: ubuntu-24.04" in run_job
    assert "target_execution: x86_64-recovery" in run_job
    assert "uses: ./.github/workflows/_ci-target-run.yml" in run_job


def _assert_single_full_recovery_partition(job: str) -> None:
    match = re.search(r"(?m)^\s+replay_partitions: '([^']+)'$", job)
    assert match is not None, "all entry points must declare the same full partition"
    partitions = json.loads(match.group(1))
    assert len(partitions) == 1
    assert partitions[0]["label"] == "all"
    assert partitions[0]["value"] == "hash:1/1"
    assert partitions[0]["run_followup"] is True
    assert "target_execution: x86_64-recovery" in job
    assert "runs_on: ubuntu-24.04" in job


def test_scheduled_recovery_replay_uses_one_full_owned_partition() -> None:
    replay = (WORKFLOWS / "macos-recovery-replay.yml").read_text(encoding="utf-8")
    assert "schedule:" in replay
    assert "workflow_dispatch:" in replay
    assert "workflow_call:" in replay
    job = replay.split("\n  replay:\n", 1)[1]
    _assert_single_full_recovery_partition(job)
    assert "source_ref: ${{ github.sha }}" in job


def test_release_recovery_replay_uses_one_full_owned_partition() -> None:
    release = (WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8")
    job = release.split("\n  e2e_macos_x64_replay:\n", 1)[1].split(
        "\n  release_execution_contract:\n", 1
    )[0]
    _assert_single_full_recovery_partition(job)
    assert "source_ref: ${{ needs.prepare.outputs.commit_sha }}" in job


def test_x64_replay_lane_is_off_the_pull_request_critical_path() -> None:
    """soldr#3116: the replay lane produced 0 green results in 25 CI runs and
    was the last job to finish in most of them (34-40 min of a wedged guest).
    Nothing in ci.yml consumed its output until the PR-entry consolidation:
    ci.yml now calls the reusable replay only when `macos-replay` is applied,
    while the standalone workflow retains nightly and manual operation."""
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    assert re.search(r"(?m)^  e2e-macos-x64:\s*$", ci) is not None
    # The cross-build stays a per-PR, build-only lane (soldr#1978 item 3
    # shape, like aarch64-apple-darwin): a darwin cross-compile break must
    # still fail the PR even though nothing in ci.yml replays the archive.
    assert re.search(r"(?m)^  e2e-macos-x64-build:\s*$", ci) is not None
    replay = (WORKFLOWS / "macos-recovery-replay.yml").read_text(encoding="utf-8")
    assert "schedule:" in replay
    assert "workflow_dispatch:" in replay
    assert "workflow_call:" in replay
    assert "pull_request:" not in replay
    assert "macos-recovery-replay:" in ci
    assert "github.event.label.name == 'macos-replay'" in ci
    assert "github.event.label.name == 'macos-replay'" in replay
    assert "uses: ./.github/workflows/_ci-cross-build-linux.yml" in replay
    assert "target: x86_64-apple-darwin" in replay
    # The build-only aarch64 lane is untouched by the move.
    assert re.search(r"(?m)^  e2e-macos-arm64-build:\s*$", ci) is not None


def test_macos_arm64_run_job_exists_for_opt_in_ci() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    assert re.search(r"(?m)^  e2e-macos-arm64:\s*$", ci) is not None
    assert re.search(r"(?m)^  e2e-macos-arm64-build:\s*$", ci) is not None


def test_docker_mac_x64_action_is_pinned_to_a_full_sha_with_a_main_comment() -> None:
    target_run = (WORKFLOWS / "_ci-target-run.yml").read_text(encoding="utf-8")
    for workflow_name, text in (("_ci-target-run.yml", target_run),):
        pattern = re.compile(
            rf"uses:\s*zackees/docker-mac-x64@{DOCKER_MAC_X64_PIN}\s*#\s*main"
        )
        assert pattern.search(text), (
            f"{workflow_name} must pin zackees/docker-mac-x64 to a full commit "
            f"SHA ({DOCKER_MAC_X64_PIN}) with a '# main' comment"
        )


def test_no_dockur_ssh_machinery_referenced_in_workflows() -> None:
    for workflow in sorted(WORKFLOWS.glob("*.y*ml")):
        text = workflow.read_text(encoding="utf-8")
        assert "macos_x64_guest.py" not in text, workflow.name
        assert "x86_64-dockur" not in text, workflow.name
        # soldr#3076: GHCR auth existed only to pull the retired baked guest
        # image; docker-mac-x64 needs no registry auth at all.
        assert "docker/login-action" not in text, workflow.name


def test_recovery_lane_ships_the_tests_archive_to_the_guest() -> None:
    """soldr#3078: the Recovery guest replays the real nextest archive now,
    not just `soldr --version`/`--help`, so the share-dir prep must actually
    ship it in."""
    target_run = (WORKFLOWS / "_ci-target-run.yml").read_text(encoding="utf-8")
    assert "share/tests.tar.zst" in target_run
    assert "-tests.tar.zst" in target_run
    assert "nextest_list_all" in target_run or "nextest list" in target_run


def test_recovery_replay_keeps_partition_routing_and_guest_diagnostics() -> None:
    target_run = (WORKFLOWS / "_ci-target-run.yml").read_text(encoding="utf-8")
    assert '--partition "$REPLAY_PARTITION"' in target_run
    assert '--partition "$REPLAY_PARTITION" \\' in target_run
    assert (
        "name: target-run-${{ inputs.target }}-${{ matrix.replay.label }}-recovery-diagnostics"
        in target_run
    )


def test_recovery_lane_records_the_executor_contract_as_diagnostics() -> None:
    target_run = (WORKFLOWS / "_ci-target-run.yml").read_text(encoding="utf-8")
    assert "macos_recovery_run.py describe-executor" in target_run
    assert '"$RUNNER_TEMP/recovery-executor-contract.json"' in target_run
    assert "${{ runner.temp }}/recovery-executor-contract.json" in target_run


def test_release_workflow_has_the_macos_x64_replay_jobs() -> None:
    """soldr#3078: the release runs the same archive replay
    macos-recovery-replay.yml runs nightly (soldr#3116), pinned to the release
    commit.

    The jobs must keep existing and running. soldr#3088 removed only their
    hold over `publish` -- see the test below.
    """
    release = (WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8")
    assert re.search(r"(?m)^  e2e_macos_x64_build:\s*$", release) is not None
    assert re.search(r"(?m)^  e2e_macos_x64_replay:\s*$", release) is not None
    assert "uses: ./.github/workflows/_ci-cross-build-linux.yml" in release
    assert "target_execution: x86_64-recovery" in release


def test_release_macos_x64_smoke_executes_the_shipped_wheel() -> None:
    release = (WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8")
    smoke = release.split("\n  smoke_macos_x64:\n", 1)[1].split(
        "\n  smoke_macos_arm64:\n", 1
    )[0]
    assert "pypi-soldr-x86_64-apple-darwin" in smoke
    assert "--require-wheel-import" in smoke
    assert "runs-on: macos-15-intel" in smoke
    assert "--require-daemon-cache-smoke" in smoke
    contract = json.loads((REPO_ROOT / "ci" / "canonical-targets.json").read_text())
    x64 = next(
        row for row in contract["targets"] if row["triple"] == "x86_64-apple-darwin"
    )
    assert x64["release"]["artifact_provenance"]["execution"]["wheel"] == {
        "status": "required-before-publication",
        "gate_job": "smoke_macos_x64",
    }


def _publish_job() -> str:
    release = (WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8")
    start = release.index("\n  publish:\n")
    end = release.index("\n  verify_github_release:\n", start)
    return release[start:end]


def test_release_publish_is_not_gated_on_the_macos_x64_replay() -> None:
    """soldr#3088: inverted from soldr#3078's gate. The replay lane has never
    been green, and both v0.9.12 release attempts (runs 33820395040 and
    33824207338) were blocked by harness bugs -- a missing `mkdir -p` for
    nextest's `--extract-to`, in a guest where that same binary reported
    `arch=pass:x86_64` / `version=pass:soldr 0.9.12` / `help=pass`. A gate
    that has produced only false positives must not make releases
    unpublishable. Restore it under the criteria in soldr#3088.
    """
    publish_job = _publish_job()
    assert "e2e_macos_x64_replay" not in _needs_and_if(publish_job), (
        "publish must not depend on the Recovery replay lane (soldr#3088); a "
        "failed `needs:` skips publish even when the `if:` would allow it"
    )


def test_release_publish_keeps_the_native_macos_and_windows_smokes() -> None:
    """soldr#3088 dropped exactly one gate. The two native release smokes --
    the real per-OS signal on the shipped binaries -- stay required."""
    publish_job = _publish_job()
    for job in ("smoke_macos_x64", "smoke_windows"):
        assert f"      - {job}\n" in publish_job, job
        assert f"needs.{job}.result == 'success'" in publish_job, job


def _needs_and_if(publish_job: str) -> str:
    """The dependency-bearing part of the job: `needs:` list plus the `if:`
    expression, with comment lines stripped so soldr#3088's rationale (which
    names the lane) is not mistaken for a live dependency."""
    return "\n".join(
        line
        for line in publish_job.splitlines()
        if not line.strip().startswith("#") and "steps:" not in line
    ).split("runs-on:", maxsplit=1)[0]
