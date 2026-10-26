import json
import re
from pathlib import Path

from conftest import COOK_CACHE_ALLOWLIST_INPUT, WORKSPACE_CRATES

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = REPO_ROOT / ".github" / "workflows"


def _job_block(workflow: str, job: str, next_job: str | None = None) -> str:
    start = workflow.index(f"  {job}:\n")
    if next_job:
        end = workflow.index(f"  {next_job}:\n", start)
    else:
        match = re.search(r"(?m)^  [a-zA-Z0-9_-]+:\n", workflow[start + 1 :])
        end = start + 1 + match.start() if match else len(workflow)
    return workflow[start:end]


def _step_block(workflow: str, step_name: str) -> str:
    """Return one workflow step and fail if its name is not unique."""
    marker = f"      - name: {step_name}\n"
    assert workflow.count(marker) == 1, f"expected exactly one {step_name!r} step"
    start = workflow.index(marker)
    end_match = re.search(
        r"(?m)^      - (?:name: |uses: |run: )", workflow[start + len(marker) :]
    )
    end = start + len(marker) + end_match.start() if end_match else len(workflow)
    return workflow[start:end]


def _job_input(job: str, name: str) -> str:
    matches = re.findall(rf"^      {re.escape(name)}: ([^\n]+)$", job, re.MULTILINE)
    assert len(matches) == 1, f"expected one {name!r} input"
    return matches[0]


def _assert_no_narrowing(command: str) -> None:
    assert not re.search(r"(?:^|\s)(?:-p|--package|--exclude)(?:[=\s]|$)", command)
    assert not re.search(r"(?:^|\s)(?:-E|--filter|--filter-expr)(?:[=\s]|$)", command)


def test_windows_behavior_contract_reaches_native_target_runners() -> None:
    behavioral_test = (
        REPO_ROOT
        / "crates"
        / "soldr-cli"
        / "tests"
        / "cache_gc"
        / "windows_delete_semantics.rs"
    ).read_text(encoding="utf-8")
    expected_tests = [
        "read_only_files_do_not_block_a_recursive_delete",
        "read_only_files_do_not_block_a_single_file_delete",
        "a_read_only_directory_does_not_block_its_parents_delete",
    ]
    assert not behavioral_test.startswith("#![cfg(windows)]\n")
    assert behavioral_test.count("HostOs::Windows") == len(expected_tests)
    # soldr#2493: plain `#[test]` is the declaration form now; nextest owns
    # the budgets. What still matters is that every expected test is declared
    # here and that the file is not `#![cfg(windows)]`-gated away.
    for test_name in expected_tests:
        declaration = rf"(?m)^#\[test\]\s*\nfn {re.escape(test_name)}\("
        assert re.search(declaration, behavioral_test), test_name

    cross = (WORKFLOWS / "_ci-cross-build-linux.yml").read_text(encoding="utf-8")
    archive = _step_block(cross, "Build nextest archive")
    for required in [
        "soldr cargo nextest archive",
        '--target "$target"',
        "--workspace",
        '--archive-file "$archive"',
        "--archive-format tar-zst",
        'archive="dist/${{ inputs.artifact_name }}-tests.tar.zst"',
        'ls -la "$archive"',
    ]:
        assert required in archive
    command_marker = "\n          soldr cargo nextest archive \\\n"
    archive_command = archive[archive.index(command_marker) + 1 :]
    archive_command = archive_command[
        : archive_command.index('\n          ls -la "$archive"')
    ]
    _assert_no_narrowing(archive_command)

    upload = _step_block(cross, "Upload artifact")
    assert "name: ${{ inputs.artifact_name }}" in upload
    assert "dist/${{ inputs.artifact_name }}-tests.tar.zst" in upload
    assert "if-no-files-found: error" in upload
    assert "if-no-files-found: warn" not in upload
    assert "if-no-files-found: ignore" not in upload

    target_run = (WORKFLOWS / "_ci-target-run.yml").read_text(encoding="utf-8")
    replay = _step_block(target_run, "Run owned pre-built native tests")
    archive_assignment = 'archive="artifact/${{ inputs.artifact_name }}-tests.tar.zst"'
    archive_check = 'test -f "$archive"'
    list_command = '"$NEXTEST_BIN" nextest list'
    run_command = '"$NEXTEST_BIN" nextest run'
    for required in [
        archive_assignment,
        archive_check,
        list_command,
        run_command,
        '--archive-file "$archive"',
        "--max-fail 3:immediate",
    ]:
        assert required in replay
    assert replay.index(archive_assignment) < replay.index(archive_check)
    assert replay.index(archive_check) < replay.index(list_command)
    assert replay.index(list_command) < replay.index(run_command)
    # soldr#2933: exactly ONE decompression per shard. This assertion used to
    # require 2 -- it encoded the bug. `nextest list` and `nextest run` each
    # re-inflated the whole 3.3 GiB archive into a fresh temp dir, so a lane
    # paid six decompressions across three shards and exhausted the runner's
    # C: volume (`os error 112`) while D: sat at 143 GiB free. The list pass is
    # now the single extraction, aimed at an explicitly chosen volume, and the
    # run pass consumes it.
    assert replay.count('--archive-file "$archive"') == 1
    assert '--extract-to "$NEXTEST_EXTRACT_DIR"' in replay
    assert "--no-fail-fast" not in replay
    assert replay.count(list_command) == 2
    first_list = replay.index(list_command)
    ownership_check = replay.index("target_run_ownership.py", first_list)
    second_list = replay.index(list_command, ownership_check)
    run_start = replay.index(run_command)
    complete_inventory_invocation = replay[first_list:ownership_check]
    owned_list_invocation = replay[second_list:run_start]
    run_invocation = replay[replay.index(run_command) :]
    _assert_no_narrowing(complete_inventory_invocation)
    assert '-E "$TARGET_RUN_FILTER"' in owned_list_invocation
    assert '-E "$TARGET_RUN_FILTER"' in run_invocation


def test_windows_target_runner_pairs_share_their_producer_artifacts() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    pairs = [
        (
            "e2e-windows-x64-build",
            "e2e-windows-x64",
            "x86_64-pc-windows-msvc",
            "windows-2025",
            "soldr-ci-e2e-windows-x64",
        ),
        (
            "e2e-windows-arm64-build",
            "e2e-windows-arm64",
            "aarch64-pc-windows-msvc",
            "windows-11-arm",
            "soldr-ci-e2e-windows-arm64",
        ),
    ]
    for build_name, run_name, target, runner, artifact in pairs:
        build = _job_block(ci, build_name, run_name)
        run = _job_block(ci, run_name)
        assert "uses: ./.github/workflows/_ci-cross-build-linux.yml" in build
        assert "uses: ./.github/workflows/_ci-target-run.yml" in run
        assert re.search(rf"(?m)^    needs: \[ci-mode, {re.escape(build_name)}\]$", run)
        assert _job_input(build, "artifact_name") == artifact
        assert _job_input(run, "artifact_name") == artifact
        assert (
            _job_input(build, "source_ref")
            == "${{ needs.ci-mode.outputs.checkout_sha }}"
        )
        assert _job_input(build, "target") == target
        assert _job_input(run, "target") == target
        assert _job_input(run, "runs_on") == runner


def test_linux_x64_musl_replay_is_sharded_and_consumes_its_archive() -> None:
    """soldr#3316: the restored runtime proof must stay non-vacuous and bounded."""

    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    build = _job_block(ci, "e2e-linux-x64-musl-build", "e2e-linux-x64-musl")
    run = _job_block(ci, "e2e-linux-x64-musl", "e2e-linux-arm64-musl-build")

    assert _job_input(build, "artifact_name") == "soldr-ci-e2e-linux-x64-musl"
    assert _job_input(run, "artifact_name") == "soldr-ci-e2e-linux-x64-musl"
    assert _job_input(run, "target") == "x86_64-unknown-linux-musl"
    assert _job_input(run, "runs_on") == "ubuntu-24.04"
    partitions = json.loads(_job_input(run, "replay_partitions").strip("'"))
    assert partitions == [
        {"label": f"{index}-of-6", "value": f"hash:{index}/6", "run_followup": False}
        for index in range(1, 7)
    ]


def test_windows_gnu_target_run_is_bounded_and_disk_safe() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    target_run = (WORKFLOWS / "_ci-target-run.yml").read_text(encoding="utf-8")
    gnu_run = _job_block(ci, "e2e-windows-x64-gnu", "e2e-windows-arm64-build")

    assert "extended_replay:" in target_run
    assert "default: false" in target_run
    # soldr#3078: x86_64-recovery now replays a real nextest partition
    # (toolchain provisioning included) inside the guest, so it gets a
    # 90-minute budget instead of the smoke-only 30; the pre-existing
    # run_pep517_smoke / extended_replay / default chain is unchanged.
    assert "inputs.target_execution == 'x86_64-recovery' && 90" in target_run
    assert "inputs.run_pep517_smoke && 65" in target_run
    assert "inputs.extended_replay && 55" in target_run
    assert "|| 30 }}" in target_run
    assert _job_input(gnu_run, "extended_replay") == "true"
    partitions = json.loads(_job_input(gnu_run, "replay_partitions").strip("'"))
    assert partitions == [
        {"label": "1-of-3", "value": "hash:1/3", "run_followup": False},
        {"label": "2-of-3", "value": "hash:2/3", "run_followup": False},
        {"label": "3-of-3", "value": "hash:3/3", "run_followup": False},
    ]
    assert "fromJSON(inputs.replay_partitions)" in target_run
    # The Recovery guest forwards this partition separately when generating
    # and verifying its script; this count guards the native replay calls.
    native_replay = target_run.split(
        "      - name: Prepare the Recovery guest share directory", 1
    )[0]
    assert native_replay.count('--partition "$REPLAY_PARTITION"') == 4
    assert "partition_args" not in target_run
    assert "nextest list" in target_run
    assert "nextest run" in target_run
    assert "matrix.replay.label }}-diagnostics" in target_run
    # soldr#3047 retired the pep517 Swatinem/rust-cache restore from this
    # lane, so the guard now appears on exactly three steps -- wheel build,
    # daemon smoke, and the always()-guarded log upload -- not four.
    assert (
        target_run.count("inputs.run_pep517_smoke && matrix.replay.run_followup") == 3
    )
    assert "uses: Swatinem/rust-cache" not in target_run


def test_windows_msvc_ci_builds_and_archives_real_tests() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    cross = (WORKFLOWS / "_ci-cross-build-linux.yml").read_text(encoding="utf-8")
    target_run = (WORKFLOWS / "_ci-target-run.yml").read_text(encoding="utf-8")
    cache_roundtrip = (
        REPO_ROOT / ".github" / "scripts" / "windows_msvc_cache_roundtrip.py"
    ).read_text(encoding="utf-8")
    nextest_config = (REPO_ROOT / ".config" / "nextest.toml").read_text(
        encoding="utf-8"
    )
    baseline = (WORKFLOWS / "baseline-zero-deps.yml").read_text(encoding="utf-8")
    arm_build = _job_block(ci, "e2e-windows-arm64-build", "e2e-windows-arm64")
    arm_run = _job_block(ci, "e2e-windows-arm64")
    assert "if: false" not in arm_build
    assert "if: false" not in arm_run

    # soldr#3122: both MSVC target-run lanes shard into three hash partitions
    # instead of a single serial `hash:1/1` replay.
    x64_run = _job_block(ci, "e2e-windows-x64", "e2e-windows-x64-gnu-build")
    assert "hash:3/3" in x64_run
    assert "hash:3/3" in arm_run

    assert "if: (!contains(inputs.target, 'pc-windows-msvc'))" not in cross
    assert "soldr --no-cache build --profile" not in cross
    assert 'soldr build --profile "$ci_profile" --target "$target"' in cross
    assert "soldr cargo nextest archive" in cross
    assert "soldr_args+=(--no-cache)" not in cross
    assert "Validate warm Windows cache restoration" in cross
    assert "windows_msvc_cache_roundtrip.py" in cross
    assert "--phase build" in cross
    assert "--phase archive" in cross
    assert (
        "--no-cache"
        not in cross[cross.index("- name: Cross-build soldr (ci-nextest profile)") :]
    )
    # soldr#2996: the cook gate is an allowlist now. The old exclusion list
    # meant x86_64-pc-windows-gnu had cook only by not matching any clause.
    # soldr#3121 added aarch64-pc-windows-msvc, x86_64-pc-windows-msvc and
    # aarch64-unknown-linux-gnu to the allowlist.
    assert COOK_CACHE_ALLOWLIST_INPUT in cross
    assert "inputs.target == 'x86_64-unknown-linux-gnu'" in cross
    assert "expected binary missing: $binary; searching target tree" in cross
    assert 'find target -type f \\( -name "soldr" -o -name "soldr.exe" \\)' in cross
    assert "normalized binary layout:" in cross
    assert '-path "*/$ci_profile/deps/soldr"' in cross
    assert 'dd if="$binary" bs=1 count=2' in cross
    assert (
        "              soldr --no-cache cargo xwin build "
        "--target x86_64-pc-windows-msvc\n"
        "              ls -l target/x86_64-pc-windows-msvc/debug/hellowin.exe"
        in baseline
    )
    for first_party_package in WORKSPACE_CRATES:
        assert f"-p {first_party_package}" in cross

    assert "test archive missing" not in target_run
    assert '"$SOLDR_BIN" --version' not in target_run
    assert '"$NEXTEST_BIN" nextest run' in target_run
    assert 'echo "SOLDR_BIN=$soldr_bin"' in target_run
    assert (
        "case '${{ inputs.target }}' in *-pc-windows-*) suffix=\".exe\"" in target_run
    )
    assert "artifact/package/soldr$suffix" in target_run
    assert "artifact/package/soldr-daemon$suffix" in target_run
    assert 'echo "SOLDR_INTERNAL_DAEMON_EXE=$daemon_bin"' in target_run
    # soldr#3539: soldr-daemon is derived on the target-run side, never
    # uploaded as a second copy of the same bytes.
    assert "dist/package/soldr-daemon" not in cross
    assert 'cp "$soldr_bin" "$daemon_bin"' in target_run
    assert "cargo_bin=$(python .github/scripts/run_target_command.py" in target_run
    assert "rustc_bin=$(python .github/scripts/run_target_command.py" in target_run
    assert 'echo "CARGO=$cargo_bin"' in target_run
    assert 'echo "RUSTC=$rustc_bin"' in target_run
    assert "SOLDR_SESSION_ATTEMPT_BUDGET_MS" not in target_run
    assert "RUNNING_PROCESS_BROKER_CLIENT_TIMEOUT_MS" not in target_run
    assert 'echo "$TOOLCHAIN_SHIM_DIR" >> "$GITHUB_PATH"' not in target_run
    assert "artifact/package/tools/cargo-nextest$suffix" in target_run
    assert 'echo "SOLDR_TEST_WORKSPACE_ROOT=$GITHUB_WORKSPACE"' in target_run
    assert "SOLDR_GITHUB_TOKEN: ${{ github.token }}" in target_run
    assert 'SOLDR_TARGET_WARN_FREE_GB: "1"' in target_run
    assert 'SOLDR_TARGET_BLOCK_FREE_GB: "1"' in target_run
    assert 'SOLDR_USE_SYSTEM_CMAKE: "1"' in target_run
    assert "SOLDR_TARGET_AUTO_PRUNE_ENABLED" not in target_run
    assert "actions/setup-python@" in target_run
    assert 'python-version: "3.13"' in target_run
    assert "python3 .github/scripts/target_run_summary.py" not in target_run
    assert '"$SOLDR_BIN" toolchain ensure --json' in target_run
    assert '"$SOLDR_BIN" toolchain link' in target_run
    assert '--shim-dir "$TOOLCHAIN_SHIM_DIR"' in target_run
    assert '"$SOLDR_BIN" rustc -vV' in target_run
    assert '"$SOLDR_BIN" cargo -V' in target_run
    assert "skip_filter:" not in target_run
    assert "inputs.skip_filter" not in target_run
    assert "SOLDR_TEST_SKIP_SOURCE_TREE" not in target_run
    assert "submodules: recursive" in target_run
    # The archive producer remains complete. Native execution is narrowed by
    # a positive ownership declaration whose selectors are checked against the
    # complete inventory before either list or run can consume them (#2999).
    assert "--filter-expr" not in cross
    assert "\n            -E " not in cross
    assert "\n            --filter " not in cross
    assert "target_run_ownership.py" in target_run
    assert "ci/target-run-ownership.json" in target_run
    assert target_run.count('-E "$TARGET_RUN_FILTER"') == 2
    assert 'test -n "$TARGET_RUN_FILTER"' in target_run
    assert "NEXTEST_ALL_LIST_JSON" in target_run
    assert "ARCHIVE_FILTER" not in cache_roundtrip
    assert '"-E"' not in cache_roundtrip
    # soldr#2931/#2999: the producer keeps a complete inventory inside the
    # archive budget. Native replay partitions cover every positively selected
    # host-sensitive test rather than re-executing the portable inventory.
    assert "archive must preserve the complete test" in cross
    assert "replay partitions must cover every" in cross
    assert "selected test" in cross
    assert "--profile target-run" in target_run
    assert "target/nextest/target-run/junit.xml" in target_run
    assert "target_run_summary.py" in target_run
    assert "--require-junit" in target_run
    assert target_run.index('if [ "$run_status" -ne 0 ]') < target_run.index(
        'exit "$summary_status"'
    )
    assert "if: always()" in target_run
    assert "[profile.target-run.junit]" in nextest_config
    assert 'path = "junit.xml"' in nextest_config
    assert "default-filter" not in nextest_config
    assert "fetch_catalogued_nextest.py" in cross
    assert "cargo-nextest.json" in target_run
    assert "taiki-e/install-action" not in target_run


def test_native_linux_runs_the_complete_workspace_suite() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    build_and_test = (WORKFLOWS / "_build-and-test.yml").read_text(encoding="utf-8")
    assert "x86_64 GNU is the canonical native exception" in ci
    assert "other seven" in ci
    # soldr#2868: the pinned PATH soldr bootstraps the source revision's CLI,
    # then that exact binary owns the prescribed host-validation plan. The
    # source wrapper remains explicit so compiler identity cannot flip inside
    # the validation DAG or leak an old daemon route into nested tests.
    flattened = " ".join(build_and_test.split())
    assert build_and_test.count("name: Run prescribed host validation") == 1
    # soldr#3024: four Linux test processes can overlap while every nested
    # compiler remains governed by Soldr's shared/exclusive admission. The
    # Windows-target cold-build override still reserves the complete pool.
    assert 'NEXTEST_TEST_THREADS: "4"' in build_and_test
    # The driver is built into its own target dir under the runner temp:
    # `soldr cook`'s cargo-chef skeleton build overwrites every bin in the
    # workspace target dir with a `fn main() {}` stub.
    assert 'source_soldr="${RUNNER_TEMP}/soldr-source-driver/' in flattened
    assert 'source_soldr="${GITHUB_WORKSPACE}/target/' not in flattened
    assert 'SOLDR_RUSTC_WRAPPER="$source_soldr" "$source_soldr"' in flattened
    assert "bootstrap_wrapper" not in flattened
    handoff = build_and_test.index("name: Hand off bootstrap broker to source revision")
    validation = build_and_test.index("name: Run prescribed host validation")
    assert handoff < validation
    assert "soldr broker remove" in build_and_test[handoff:validation]
    assert '"$source_soldr" daemon start' in build_and_test[handoff:validation]
    assert '"$source_soldr" broker remove || true' in build_and_test
    assert 'soldr-source-driver/${{ inputs.target }}/debug/soldr"' in flattened
    assert 'ci-test --target "${{ inputs.target }}"' in flattened
    assert "nextest run --no-run" not in build_and_test
    assert "- name: Run CLI smoke tests" not in build_and_test
    assert "soldr cargo test" not in build_and_test
    # The lane must stay on bash; PowerShell steps were removed so the logic
    # lives in `.github/scripts/*.py` where it is testable.
    assert "shell: pwsh" not in build_and_test


def test_archived_source_tests_use_only_runtime_workspace_resolution() -> None:
    crates_root = REPO_ROOT / "crates"
    allowed = {
        crates_root / "soldr-cli" / "tests" / "common" / "mod.rs",
        crates_root / "soldr-cli" / "src" / "prepare_cmd.rs",
        crates_root / "soldr-fetch" / "build.rs",
    }
    forbidden = (
        'env!("CARGO_MANIFEST_DIR")',
        'option_env!("CARGO_MANIFEST_DIR")',
        'var("CARGO_MANIFEST_DIR")',
        'var_os("CARGO_MANIFEST_DIR")',
    )
    offenders = []
    for path in crates_root.rglob("*.rs"):
        if path in allowed:
            continue
        body = path.read_text(encoding="utf-8")
        if any(pattern in body for pattern in forbidden):
            offenders.append(path.relative_to(REPO_ROOT).as_posix())
    assert not offenders, "workflows must not hardcode these patterns: " + ", ".join(
        offenders
    )


def test_fast_build_only_skips_windows_e2e_for_low_risk_changes() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    policy = _job_block(ci, "windows-e2e-policy", "e2e-cross-bootstrap-soldr")
    x64_build = _job_block(ci, "e2e-windows-x64-build", "e2e-windows-x64")
    arm64_build = _job_block(ci, "e2e-windows-arm64-build", "e2e-windows-arm64")

    assert "windows_e2e_policy.py" in policy
    assert "fetch-depth: 0" in policy
    assert "run_windows_e2e" in policy
    for block in [x64_build, arm64_build]:
        assert "windows-e2e-policy" in block
        assert "needs.windows-e2e-policy.outputs.run_windows_e2e == 'true'" in block
        assert "fast-build" not in block

    windows_section = ci[ci.index("# ---------- Windows x64") :]
    assert "github.event.pull_request.labels" not in windows_section

    # soldr#3018 widened the same classification past Windows. The two
    # properties that must survive that are pinned here rather than the old
    # prose, which no longer describes the policy.
    #
    # 1. The proof-of-life lane is never gated. A proof of life a heuristic
    #    can skip is not one, and it is the cheapest lane besides.
    linux_x64 = _job_block(ci, "e2e-linux-x64", "windows-e2e-policy")
    assert "run_platform_e2e" not in linux_x64
    assert "needs.ci-mode.outputs.mode != 'minimal'" in linux_x64

    # 2. The broader gate still protects macOS x64 in full mode. ARM64 is
    #    explicitly requested by ci-test as well, so it bypasses path policy.
    assert "run_platform_e2e" in policy
    assert "needs.windows-e2e-policy.outputs.run_platform_e2e == 'true'" in _job_block(
        ci, "e2e-macos-x64-build"
    )
    assert "needs.ci-mode.outputs.mode != 'minimal'" in _job_block(
        ci, "e2e-macos-arm64-build"
    )
    assert "fast-build" not in _job_block(ci, "e2e-macos-x64-build")


def test_cross_workflow_bootstraps_toolchain_dependencies_through_soldr() -> None:
    cross = (WORKFLOWS / "_ci-cross-build-linux.yml").read_text(encoding="utf-8")

    assert "cross-targets:" not in cross
    assert (
        'soldr prepare --target "${{ inputs.target }}" --github-env "$GITHUB_ENV"'
        in cross
    )
    assert "soldr --no-cache cargo build --profile ci-bootstrap" in cross
    for unmanaged_installer in [
        "sudo apt-get",
        "mlugg/setup-zig",
        "taiki-e/install-action",
        "run: rustup ",
        "\n          cargo build --profile ci-bootstrap",
        "\n            cargo zigbuild ",
        "\n            cargo build ",
        "nextest_cmd=(cargo nextest archive)",
    ]:
        assert unmanaged_installer not in cross


def test_catalogue_download_consumers_require_sha256_metadata() -> None:
    cross = (WORKFLOWS / "cross-compile-all-targets.yml").read_text(encoding="utf-8")
    baseline = (WORKFLOWS / "baseline-zero-deps.yml").read_text(encoding="utf-8")
    fetch = (REPO_ROOT / ".github" / "scripts" / "fetch_or_build_tool.sh").read_text(
        encoding="utf-8"
    )
    downloader = (
        REPO_ROOT / ".github" / "scripts" / "download_catalogued_asset.py"
    ).read_text(encoding="utf-8")

    assert "--json cargo-zigbuild" in baseline
    helper_invocations = [
        line
        for line in cross.splitlines()
        if line.strip().startswith(".github/scripts/fetch_or_build_tool.sh")
    ]
    assert len(helper_invocations) == 4
    assert "download_catalogued_asset.py" in baseline
    assert "download_catalogued_asset.py" in fetch
    assert 'asset_url="verified"' in fetch
    assert "-> $asset_url" not in fetch
    assert "sha256 mismatch" in downloader
    assert 'metadata.get("parts")' in downloader


def test_cross_release_archives_never_bundle_standalone_zccache() -> None:
    """zccache is embedded through its crate API, never shipped as a sidecar."""
    cross = (WORKFLOWS / "cross-compile-all-targets.yml").read_text(encoding="utf-8")

    for forbidden in [
        "dist/zccache-stage",
        "Fetch matched zccache release",
        "download_catalogued_asset.py",
        '"binaries": [',
        "zccache-daemon",
        "zccache-fp",
    ]:
        assert forbidden not in cross

    assert cross.count("stage_release_binaries.py") == 2
    assert cross.count("python3 .github/scripts/release_manifest.py") == 2
    assert '"embedded": True' in (
        REPO_ROOT / ".github" / "scripts" / "release_manifest.py"
    ).read_text(encoding="utf-8")


def test_embedded_zccache_pin_has_no_platform_asset_gate() -> None:
    assert not (REPO_ROOT / ".github" / "scripts" / "check_zccache_asset.py").exists()
    assert not (REPO_ROOT / ".github" / "scripts" / "zccache_version.py").exists()

    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    instructions = (REPO_ROOT / "CLAUDE.md").read_text(encoding="utf-8")
    trust_boundaries = (REPO_ROOT / "docs" / "TRUST_BOUNDARIES.md").read_text(
        encoding="utf-8"
    )
    vendoring = (REPO_ROOT / "docs" / "VENDORING.md").read_text(encoding="utf-8")
    assert "check_zccache_asset.py" not in ci
    assert "check_zccache_asset.py" not in instructions
    assert "six platform support archives" not in instructions
    assert "managed `zccache` download" not in trust_boundaries
    assert "MANAGED_ZCCACHE_VERSION" not in vendoring
    assert "embed/manifest.json" not in vendoring


def test_linux_zig_cross_lanes_use_current_checkout_soldr_bootstrap() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    cross = (WORKFLOWS / "_ci-cross-build-linux.yml").read_text(encoding="utf-8")

    lane_names = [
        ("e2e-linux-arm64-build", "e2e-linux-arm64"),
        ("e2e-linux-x64-musl-build", "e2e-linux-x64-musl"),
        ("e2e-linux-arm64-musl-build", "e2e-linux-arm64-musl"),
    ]
    for job, next_job in lane_names:
        block = _job_block(ci, job, next_job)
        # soldr#3018: gated lanes carry
        # `needs: [e2e-cross-bootstrap-soldr, windows-e2e-policy]`, so assert
        # the dependency rather than the scalar spelling of it.
        assert "e2e-cross-bootstrap-soldr" in block.split("uses:")[0]
        assert "bootstrap_artifact_name: soldr-ci-bootstrap-linux-gnu" in block

    download = cross[
        cross.index("      - name: Download shared bootstrap soldr artifact") :
    ]
    download = download[: download.index("      - name:", 10)]
    assert "inputs.bootstrap_artifact_name != ''" in download
    assert "contains(inputs.target" not in download

    expose = cross[cross.index("      - name: Expose bootstrap soldr on PATH") :]
    expose = expose[: expose.index("      #", 10)]
    assert "inputs.bootstrap_artifact_name != ''" in expose


def test_native_linux_integration_backstop_runs_on_pull_requests() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    block = _job_block(ci, "build-linux-x64", "pep517-daemon-smoke")
    # The serial gate stays behind the real Lint job and CI mode. A docs-only
    # PR reports a cheap Lint context, so the host lane skips only that case.
    assert "\n    needs: [ci-mode, path-selection, lint]\n" in block
    assert "needs.path-selection.outputs.docs_only != 'true'" in block
    assert "needs.lint.result == 'success'" in block
    assert "soldr#1676" in block
    assert "canonical native exception" in block


def test_host_validation_opportunistically_reuses_exact_sha_bootstrap() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    host_template = (WORKFLOWS / "_build-and-test.yml").read_text(encoding="utf-8")
    producer = _job_block(ci, "e2e-cross-bootstrap-soldr", "e2e-linux-x64-gnu-build")
    host = _job_block(ci, "build-linux-x64", "pep517-daemon-smoke")
    producer_header = producer[: producer.index("    steps:")]
    verify = _step_block(ci, "Verify bootstrap soldr")
    upload = _step_block(ci, "Upload bootstrap soldr artifact")
    host_build = _step_block(host_template, "Build ci-test driver")

    # The host must not wait for the producer: under the serial gate the
    # producer runs AFTER the host (stage 3), so the host always takes the
    # local source-build fallback rather than extending the native critical
    # path. The producer runs for opt-in ci-test and full modes.
    assert "needs.ci-mode.outputs.mode != 'minimal'" in producer_header
    assert "\n    needs: [ci-mode, build-linux-x64]\n" in producer_header
    assert re.search(r"(?m)^    needs: \[ci-mode, path-selection, lint\]$", host)
    assert not re.search(r"(?m)^    needs: e2e-cross-bootstrap-soldr", host)
    # The producer's artifact cannot exist when the host starts, so the host
    # no longer asks for it; the template skips the download on an empty name.
    assert "source_driver_artifact_name:" not in host
    assert "if: inputs.source_driver_artifact_name != ''" in host_template

    assert (
        # zackees/ci.yml#6: `-pr-<N>` in PR context, empty elsewhere.
        "bootstrap-soldr-blessed-linux-gnu-dev-v1${{ env.PR_CACHE_TAG }}-"
        "${{ needs.ci-mode.outputs.checkout_sha }}" in producer
    )
    assert "key: rustup-1.98.1-linux-x64-v1" in producer
    assert "rustup toolchain install 1.98.1 --profile minimal" in producer
    assert "toolchain: 1.98.1" in host_template
    assert "cargo build --profile dev --package soldr-cli" in producer
    assert "soldr cargo build --profile dev -p soldr-cli" in host_build
    assert "target/x86_64-unknown-linux-gnu/debug/soldr" in producer
    assert "soldr-source-driver/${{ inputs.target }}/debug/soldr" in host_template
    assert "--bin soldr" in producer
    assert "--target x86_64-unknown-linux-gnu" in producer
    assert "--features" not in producer
    assert (
        'printf \'%s\\n\' "${{ needs.ci-mode.outputs.checkout_sha }}" > "$RUNNER_TEMP/soldr-bin/source-sha"'
        in verify
    )
    assert "path: ${{ runner.temp }}/soldr-bin" in upload

    # source-sha is adjacent artifact metadata, not an executable or directory
    # contract. Every pre-existing consumer addresses only the soldr file and
    # therefore safely ignores the marker added for host provenance checks.
    cross_template = (WORKFLOWS / "_ci-cross-build-linux.yml").read_text(
        encoding="utf-8"
    )
    expose = _step_block(cross_template, "Expose bootstrap soldr on PATH")
    wheel = _step_block(ci, "Build the wheel through `soldr wheel`")
    assert "$RUNNER_TEMP/soldr-bin/soldr" in expose
    assert 'driver="$RUNNER_TEMP/soldr-bin/soldr"' in wheel
    assert "source-sha" not in expose
    assert "source-sha" not in wheel


def test_pep517_platform_smokes_run_on_pull_requests() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    block = _job_block(ci, "pep517-daemon-smoke", "e2e-linux-x64")

    # No macos-* runner exists anywhere, so this dedicated matrix keeps only
    # Windows. The shipped macOS x64 wheel is installed and imported with a
    # pinned portable CPython inside the Recovery release guest; the heavier
    # downstream build-backend replay remains separate from this matrix.
    assert '"name":"macos-arm64"' not in block
    assert '"name":"windows-x64"' in block
    assert "needs.ci-mode.outputs.mode == 'full'" in block
    assert "fromJSON('[" in block

    replay = (WORKFLOWS / "macos-recovery-replay.yml").read_text(encoding="utf-8")
    x64_run = _job_block(replay, "replay")
    assert _job_input(x64_run, "run_pep517_smoke") == "false"
    assert "no Python" in x64_run or "no Python/maturin" in x64_run

    target_run = (WORKFLOWS / "_ci-target-run.yml").read_text(encoding="utf-8")
    assert "run_pep517_smoke:" in target_run
    # soldr#3076: the Recovery guest has no Python/maturin, so this step
    # group is also excluded by execution mode as a second, workflow-level
    # line of defense (not just the false input above).
    for step_name in [
        "Build soldr wheel under test",
        "Run downstream PEP 517 daemon smoke",
    ]:
        step = _step_block(target_run, step_name)
        assert (
            "if: ${{ inputs.run_pep517_smoke && matrix.replay.run_followup "
            "&& inputs.target_execution != 'x86_64-recovery' }}" in step
        )
    release = (WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8")
    macos_release_smoke = _job_block(release, "smoke_macos_x64", "smoke_windows")
    assert "--require-wheel-import" in macos_release_smoke
    # The smoke must run after (and never gate) the archive replay.
    assert target_run.index(
        "      - name: Run owned pre-built native tests\n"
    ) < target_run.index("      - name: Build soldr wheel under test\n")


def test_n_minus_one_build_uses_clean_paired_toolchain_homes() -> None:
    build_all = (WORKFLOWS / "build-all-from-linux.yml").read_text(encoding="utf-8")
    isolation = _step_block(build_all, "Isolate N-1 toolchain homes")
    toolchain = _step_block(build_all, "Install Rust toolchain + cross target")
    build = _step_block(
        build_all, "Build soldr — soldr build --release --target ${{ matrix.alias }}"
    )

    assert 'echo "CARGO_HOME=$RUNNER_TEMP/soldr-cargo" >> "$GITHUB_ENV"' in isolation
    assert 'echo "RUSTUP_HOME=$RUNNER_TEMP/soldr-rustup" >> "$GITHUB_ENV"' in isolation
    assert build_all.index(
        "      - name: Isolate N-1 toolchain homes\n"
    ) < build_all.index("      - name: Install Rust toolchain + cross target\n")
    assert "          components: rustfmt, clippy\n" in toolchain
    assert "export CARGO_HOME=" not in build
    assert "export RUSTUP_HOME=" not in build
    assert "soldr rustup show home" not in build


def test_manual_cross_compile_workflows_use_blessed_supported_targets() -> None:
    build_all = (WORKFLOWS / "build-all-from-linux.yml").read_text(encoding="utf-8")
    cross_all = (WORKFLOWS / "cross-compile-all-targets.yml").read_text(
        encoding="utf-8"
    )

    for target in [
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
    ]:
        assert target in build_all
        assert target in cross_all

    assert "soldr build --release" in build_all
    for friendly in [
        "linux-arm",
        "linux-x86-musl",
        "linux-arm-musl",
        "windows-x86",
        "windows-arm",
    ]:
        start = cross_all.index(f"          - friendly: {friendly}\n")
        next_entry = cross_all.find("          - friendly:", start + 1)
        end = next_entry if next_entry != -1 else len(cross_all)
        assert "tool:" not in cross_all[start:end]
    assert "soldr build \\\n" in cross_all
    assert "matrix.tool" not in cross_all


def test_production_cross_workflows_do_not_select_legacy_backends() -> None:
    workflows = [
        WORKFLOWS / "cross-compile-all-targets.yml",
        WORKFLOWS / "_ci-cross-build-linux.yml",
    ]
    helper = REPO_ROOT / ".github" / "scripts" / "fetch_or_build_tool.sh"
    forbidden = [
        "matrix.tool",
        "make_zig_cc_wrappers.sh",
        "cross-targets:",
        "cross-" + "tool:",
        "soldr " + "cargo zigbuild",
        "soldr " + "cargo xwin",
    ]
    for path in [*workflows, helper]:
        body = path.read_text(encoding="utf-8")
        executable = "\n".join(
            line for line in body.splitlines() if not line.lstrip().startswith("#")
        )
        for token in forbidden:
            assert token not in executable, (
                f"{path.relative_to(REPO_ROOT)} selects {token!r}"
            )

    cross_all_executable = "\n".join(
        line
        for line in workflows[0].read_text(encoding="utf-8").splitlines()
        if not line.lstrip().startswith("#")
    )
    helper_selectors = re.findall(
        r"fetch_or_build_tool\.sh\s*\\\s*\n"
        r"\s*\S+\s+(?:\"[^\"]*\"|'[^']*'|\S+)\s+(\S+)",
        cross_all_executable,
    )
    assert len(helper_selectors) == 4
    assert set(helper_selectors) == {"soldr-build"}

    legacy_command = re.compile(r"(?m)^\s*(?:soldr\s+)?cargo\s+(?:zigbuild|xwin)\b")
    for path in workflows:
        body = path.read_text(encoding="utf-8").replace("\\\n", " ")
        assert legacy_command.search(body) is None

    ci_cross = workflows[1].read_text(encoding="utf-8")
    assert "soldr cargo clippy \\\n            --target" in ci_cross
    assert "soldr cargo nextest archive" in ci_cross
    assert "Validate native target lifecycle" in cross_all_executable
    assert "Validate target lint and test lifecycle" in cross_all_executable
    assert cross_all_executable.count("soldr cargo test --no-run") >= 2


def test_normal_gnu_lifecycle_has_no_zig_fallback() -> None:
    """#2237: GNU stays catalogue-backed. soldr#2519 strengthened this.

    This used to assert that `linux_cross.rs` carried a comment promising GNU
    never reached the Zig fallback, and that it had no GNU match arms. That
    module is now deleted outright -- removing `SOLDR_USE_LEGACY_ZIGBUILD` made
    it unreachable -- so the guarantee is structural rather than textual: there
    is no Zig fallback module for any target to reach.
    """
    lifecycle = (
        REPO_ROOT / "crates" / "soldr-cli" / "src" / "target_lifecycle.rs"
    ).read_text(encoding="utf-8")
    prepare = (REPO_ROOT / "crates" / "soldr-cli" / "src" / "prepare_cmd.rs").read_text(
        encoding="utf-8"
    )
    prepare_tests = (
        REPO_ROOT / "crates" / "soldr-cli" / "src" / "prepare_cmd_tests.rs"
    ).read_text(encoding="utf-8")
    cross = (WORKFLOWS / "_ci-cross-build-linux.yml").read_text(encoding="utf-8")

    assert not (REPO_ROOT / "crates" / "soldr-cli" / "src" / "linux_cross.rs").exists()
    assert "no catalogue-backed GNU/Linux toolchain is available" in lifecycle
    assert "abi == Some(TargetAbi::Musl)" in lifecycle
    assert "GNU/Linux toolchain" in prepare
    assert "GNU uses the catalogue-backed toolchain" in prepare_tests

    executable = "\n".join(
        line for line in cross.splitlines() if not line.lstrip().startswith("#")
    )
    assert not re.search(
        r"(?:soldr\s+)?cargo\s+zigbuild\b.*unknown-linux-gnu", executable
    )
    assert "soldr build + catalogue-backed GCC" in cross


def test_all_miss_cross_builds_bound_compile_concurrency() -> None:
    """#2453: cache-disabled hosted lanes must retain memory headroom.

    #3047 (following #2996): Swatinem/rust-cache was retired from this lane --
    `soldr cook` is the only durable dependency cache left here, so the old
    `shared-key: cross-build-<target>-v7` assertion is inverted below into a
    regression guard against reintroducing rust-cache.
    """
    cross = (WORKFLOWS / "_ci-cross-build-linux.yml").read_text(encoding="utf-8")
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")

    cross_job = _job_block(cross, "cross-build", "")
    wheel_job = _job_block(ci, "wheel-cross-verify")
    assert 'CARGO_BUILD_JOBS: "1"' in cross_job
    assert 'SOLDR_JOBS: "1"' in cross_job
    assert "Enlarge swap (OOM headroom)" in cross_job
    assert "CARGO_BUILD_JOBS: ${{ matrix.jobs }}" in wheel_job
    assert "SOLDR_JOBS: ${{ matrix.jobs }}" in wheel_job
    assert 'CARGO_PROFILE_CI_NEXTEST_CODEGEN_UNITS: "4"' in cross_job
    # soldr#3047 / soldr#2996: cook is the only durable dependency cache in
    # this lane now -- Swatinem/rust-cache (and its shared-key) must not
    # reappear as a step. The retirement is explained in a prose comment
    # that names the action for context, so check for the `uses:` line
    # specifically rather than the bare word.
    assert "uses: Swatinem/rust-cache" not in cross_job
    assert "shared-key:" not in cross_job
    # zackees/ci.yml#209 (CACHE-025): the wheel lane has a soldr, so its
    # rust-cache step was retired rather than kept as a bootstrap exception.
    assert "uses: Swatinem/rust-cache" not in wheel_job
    assert "shared-key:" not in wheel_job

    # soldr#3148 step 2: the nextest archive is no longer capped. The old
    # $GITHUB_ENV cap was only half applied (the daemon had already resolved
    # its limit), heavy test links get measured exclusive admission
    # (soldr#3154), and swap is enabled. The archive must unset inherited caps
    # itself -- setup-soldr exports them job-wide -- while the two
    # wrapper-less bootstrap steps, where admission cannot engage, keep theirs.
    assert "Select nextest archive resources" not in cross_job
    archive = _step_block(cross, "Build nextest archive")
    # Order within the `run:` script only: the step's `env:` comment also
    # names `soldr cargo nextest archive`.
    archive_run = archive[archive.index("        run: |") :]
    assert "unset CARGO_BUILD_JOBS SOLDR_JOBS" in archive_run
    assert archive_run.index("unset CARGO_BUILD_JOBS SOLDR_JOBS") < archive_run.index(
        "soldr cargo nextest archive \\"
    )
    for bootstrap in (
        "Bootstrap soldr for SDK / blessed-build prep",
        "Build current Soldr GNU proof driver",
    ):
        step = _step_block(cross, bootstrap)
        assert 'RUSTC_WRAPPER: ""' in step
        assert 'CARGO_BUILD_JOBS: "1"' in step and 'SOLDR_JOBS: "1"' in step


def test_external_zccache_bootstraps_get_exclusive_service_access() -> None:
    bootstrap = (WORKFLOWS / "_bootstrap-e2e.yml").read_text(encoding="utf-8")
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    build_and_test = (WORKFLOWS / "_build-and-test.yml").read_text(encoding="utf-8")

    # soldr#3148: the bootstrap build runs the 0.9.16 pin, which carries
    # soldr#3211, so exclusive admission owns concurrency with no cap.
    assert "CARGO_BUILD_JOBS:" not in bootstrap
    assert "SOLDR_JOBS:" not in bootstrap
    assert "Enlarge swap (OOM headroom)" in bootstrap
    lint_job = _job_block(ci, "lint")
    assert "CARGO_BUILD_JOBS" not in lint_job
    assert "SOLDR_JOBS" not in lint_job
    assert "CARGO_BUILD_JOBS:" not in build_and_test
    assert "SOLDR_JOBS:" not in build_and_test
    # zccache admits registered amalgamations and large native units
    # exclusively, so ordinary compilation can retain full host parallelism
    # without reintroducing the historical heavyweight-link overlap. Test
    # execution uses four processes; nested compilers still enter the same
    # exclusive-aware service admission path.
    assert "exclusive compiler admission" in build_and_test
    assert 'NEXTEST_TEST_THREADS: "4"' in build_and_test
    assert "Enlarge swap (OOM headroom)" in build_and_test


def test_non_rust_lint_job_has_no_empty_environment_mapping() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    lint_job = _job_block(ci, "lint")

    assert "\n    env:\n    steps:" not in lint_job


def test_gnu_catalogue_fixture_is_part_of_both_gnu_ci_lanes() -> None:
    """#2236: CI must execute the mixed-language catalogue proof, not just compile Soldr."""
    cross = (WORKFLOWS / "_ci-cross-build-linux.yml").read_text(encoding="utf-8")
    proof = (REPO_ROOT / ".github/scripts/gnu_linux_toolchain_e2e.py").read_text(
        encoding="utf-8"
    )

    assert "Prove catalogue GNU lifecycle without Zig" in cross
    assert "contains(inputs.target, 'unknown-linux-gnu')" in cross
    assert "gnu_linux_toolchain_e2e.py" in cross
    assert "Build current Soldr GNU proof driver" in cross
    assert "target/x86_64-unknown-linux-gnu/ci-bootstrap/soldr" in cross
    for required in (
        "cc::Build",
        "pkg_config::Config",
        "cmake-probe",
        "verify_glibc_baseline.py",
        "SOLDR_GNU_LINUX_TOOLCHAIN_ROOT",
        "CMAKE_C_COMPILER",
        'cmake_definition("CMAKE_C_COMPILER")',
        'cmake_definition("CMAKE_SYSROOT")',
        "SOLDR_CACHE_DIR",
        "SOLDR_TEST_NO_NETWORK",
        "prepared.tar.zst",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-gnu",
    ):
        assert required in proof


def test_mac_x64_distribution_uses_pinned_setup_soldr_and_the_blessed_build() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    release = (WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8")
    support_fetch = (
        REPO_ROOT / ".github" / "scripts" / "fetch_release_support_binaries.py"
    ).read_text(encoding="utf-8")
    install = (REPO_ROOT / "scripts" / "install.js").read_text(encoding="utf-8")
    npm_docs = (REPO_ROOT / "docs" / "NPM_PUBLISHING.md").read_text(encoding="utf-8")
    verification_docs = (REPO_ROOT / "docs" / "RELEASE_VERIFICATION.md").read_text(
        encoding="utf-8"
    )

    mac_build = _job_block(ci, "e2e-macos-x64-build")
    assert "if: false" not in mac_build
    assert "target: x86_64-apple-darwin" in mac_build

    # soldr#3071: macOS x64 release binaries now build through the same
    # Linux-cross blessed path as Windows -- no native macos-* runner. The
    # matrix itself is contract-generated (soldr#2469 step 2.1) and the
    # ubuntu-24.04 build-runner fact is pinned in
    # test_canonical_target_contract.py against release.build.
    assert "include: ${{ fromJSON(needs.prepare.outputs.build_matrix) }}" in release
    assert (
        '"x86_64-apple-darwin": ToolchainPlatform("darwin", "x86_64")' in support_fetch
    )
    # soldr#2469 step 2.2: the GNU-Linux `soldr prepare` hook moved into
    # `native_release_build.py matrix-binary`. The invariant is unchanged --
    # a GNU release build must still export its prepared target env -- so the
    # assertion follows the logic rather than being dropped.
    assert _matrix_binary_source_prepares_gnu_linux()
    assert (
        "uses: zackees/setup-soldr@a07bab94f16124b5c6857b137a237a53a61e06d1" in release
    )
    assert "version: 0.9.27" not in release  # one pin: setup-soldr's default
    assert "cross-targets: ${{ matrix.setup_target }}" in release
    assert "target-wheel-hook" in release
    # soldr#2469 step 2.2: the GitHub gate delegates both release lookup
    # and contract asset generation to a unit-tested script; the
    # prepare-side gate imports the same generator (release_detect.py).
    assert "verify_github_release_assets.py" in release

    # soldr#3071: neither the native-SDKROOT step nor the native-binary
    # build step exist any more -- apple-darwin builds through the same
    # blessed step every other release target does.
    assert "Restore the native macOS SDK root" not in release
    assert "Build native macOS release binary through pinned Soldr" not in release
    blessed_build = _step_block(release, "Build release binary (soldr-driven)")
    assert "apple-darwin" not in blessed_build
    assert "matrix.target != 'aarch64-unknown-linux-musl'" in blessed_build

    assert '"darwin-x64": { triple: "x86_64-apple-darwin"' in install
    assert "intentionally not published" not in install
    assert "x86_64-apple-darwin" in npm_docs
    assert "docker-mac-x64" in npm_docs
    assert "macos-15-intel" not in npm_docs
    assert "x86_64-apple-darwin" in verification_docs
    assert "Mach-O x86_64" in verification_docs


def test_linux_arm64_release_uses_matching_supported_hosts() -> None:
    """GNU ARM cross-builds on x64 while musl ARM builds and smokes natively.

    soldr#2469 step 2.1: the release matrix is contract-generated, so the
    host assignment now lives in `release.build` of canonical-targets.json.
    """
    contract = json.loads(
        (REPO_ROOT / "ci" / "canonical-targets.json").read_text(encoding="utf-8")
    )
    build = {
        entry["triple"]: entry["release"]["build"]
        for entry in contract["targets"]
        if entry["release"]["status"] == "included"
    }
    assert build["aarch64-unknown-linux-gnu"]["runner"] == "ubuntu-24.04"
    assert build["aarch64-unknown-linux-musl"]["runner"] == "ubuntu-24.04-arm"


def test_windows_gnu_artifacts_keep_the_exe_suffix_across_build_and_replay() -> None:
    cross_build = (
        REPO_ROOT / ".github" / "workflows" / "_ci-cross-build-linux.yml"
    ).read_text(encoding="utf-8")
    target_run = (REPO_ROOT / ".github" / "workflows" / "_ci-target-run.yml").read_text(
        encoding="utf-8"
    )

    assert cross_build.count('case "$target" in *-pc-windows-*) suffix=".exe"') == 2
    assert (
        "case '${{ inputs.target }}' in *-pc-windows-*) suffix=\".exe\"" in target_run
    )


def _matrix_binary_source_prepares_gnu_linux() -> bool:
    """True when the matrix release build still runs `soldr prepare` for GNU."""
    source = (REPO_ROOT / ".github" / "scripts" / "native_release_build.py").read_text(
        encoding="utf-8"
    )
    return (
        '"-unknown-linux-gnu"' in source
        and '"prepare"' in source
        and '"--github-env"' in source
    )


def test_release_wheels_use_soldr_with_legacy_hooks_for_other_platforms() -> None:
    """GNU uses soldr wheel; the legacy builders keep their prepared environment."""
    release = (WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8")

    assert _matrix_binary_source_prepares_gnu_linux()
    assert (
        "uses: zackees/setup-soldr@a07bab94f16124b5c6857b137a237a53a61e06d1" in release
    )
    assert "version: 0.9.27" not in release  # one pin: setup-soldr's default
    assert "cross-targets: ${{ matrix.setup_target }}" in release
    assert ".github/scripts/prepare_release_wheel.py" in release
    assert '--runner-os "$RUNNER_OS"' in release
    wheel_preparer = (
        REPO_ROOT / ".github" / "scripts" / "prepare_release_wheel.py"
    ).read_text(encoding="utf-8")
    assert "build_release_wheel.py" in wheel_preparer
    assert 'subprocess.run(["uv", "python", "install", "3.13"]' in wheel_preparer
    assert '"uv",\n        "run",\n        "--no-project",' in wheel_preparer
    assert '"--target",\n        target,' in wheel_preparer
    assert "maturin --zig" not in release
    assert "Setup zig for Linux wheel lanes" not in release
    assert "lzma_pkgconfig" not in release
    # soldr#2469 step 2.1: the build matrix (including the ARM64-musl
    # native-runner exception) is contract-generated; its per-target facts
    # are pinned in test_canonical_target_contract.py.
    assert "include: ${{ fromJSON(needs.prepare.outputs.build_matrix) }}" in release
    assert "startsWith(matrix.target, 'aarch64-')" not in release
    native_arm_musl = _step_block(release, "Build ARM64 musl release binary natively")
    assert "CC_aarch64_unknown_linux_musl: musl-gcc" in native_arm_musl
    assert ".github/scripts/native_release_build.py binary" in native_arm_musl

    target_hook_wheel = _step_block(release, "Build wheel through Soldr")
    assert (
        "if: ${{ !contains(matrix.target, 'unknown-linux-musl') }}" in target_hook_wheel
    )
    assert 'CARGO_BUILD_JOBS: "1"' in target_hook_wheel
    assert 'SOLDR_JOBS: "1"' in target_hook_wheel

    native_arm_wheel = _step_block(release, "Build musl wheel in an explicit uv venv")
    assert "CC_aarch64_unknown_linux_musl: musl-gcc" in native_arm_wheel
    assert 'CARGO_BUILD_JOBS: "1"' in native_arm_wheel
    assert 'SOLDR_JOBS: "1"' in native_arm_wheel
    assert ".github/scripts/native_release_build.py musl-wheel" in native_arm_wheel
    assert '--target "${{ matrix.target }}"' in native_arm_wheel

    native_helper = (
        REPO_ROOT / ".github" / "scripts" / "native_release_build.py"
    ).read_text(encoding="utf-8")
    assert '["uv", "venv", "--python", "3.13", "--clear", str(venv)]' in native_helper
    assert 'MATURIN_CONTRACT["pypi_package"]' in native_helper
    assert "MATURIN_CONTRACT['managed_version']" in native_helper
    assert '"--no-binary",' in native_helper
    assert "cargo_via_soldr_rustup.sh" in native_helper
    assert '"musllinux_1_2"' in native_helper

    cargo_bridge = (
        REPO_ROOT / ".github" / "scripts" / "cargo_via_soldr_rustup.sh"
    ).read_text(encoding="utf-8")
    assert (
        'exec "$SOLDR_RELEASE_DRIVER" rustup run "$SOLDR_RELEASE_TOOLCHAIN" cargo "$@"'
        in cargo_bridge
    )


def test_release_target_prepare_retries_transient_setup_failure() -> None:
    release = (WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8")
    setup = _step_block(release, "Install pinned Soldr and prepare target environment")
    retry = _step_block(
        release, "Retry target preparation after setup transport failure"
    )
    materialize = _step_block(
        release, "Materialize installed Soldr as release build driver"
    )
    wheel = _step_block(release, "Build wheel through Soldr")

    assert "continue-on-error: ${{ matrix.setup_target != '' }}" in setup
    assert (
        "if: steps.setup_soldr.outcome == 'failure' && matrix.setup_target != ''"
        in retry
    )
    assert ".github/scripts/prepare_release_target.py" in retry
    assert '--github-env "$GITHUB_ENV"' in retry
    assert "Get-Command soldr -ErrorAction Stop" in materialize
    assert ".github/scripts/prepare_release_wheel.py" in wheel
    assert "--wheel-hook '${{ steps.setup_soldr.outputs.target-wheel-hook }}'" in wheel


def test_release_supports_isolated_npm_recovery_from_an_immutable_ref() -> None:
    release = (WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8")
    prepare = _job_block(release, "prepare", "build")
    publish_npm = _job_block(release, "publish-npm")

    assert "      npm_release_ref:\n" in release
    assert '        type: string\n        default: ""\n' in release
    assert "if: inputs.npm_release_ref == ''" in prepare
    assert "needs.prepare.result == 'skipped'" in publish_npm
    assert "inputs.npm_release_ref != ''" in publish_npm
    assert "needs.prepare.outputs.should_publish_npm == 'true'" in publish_npm
    assert "needs.verify_github_release.result == 'success'" in publish_npm
    assert (
        "ref: ${{ inputs.npm_release_ref != '' && "
        "format('refs/tags/{0}', inputs.npm_release_ref) || "
        "needs.prepare.outputs.commit_sha }}" in publish_npm
    )
    assert "name: Checkout release controls" in publish_npm
    assert "path: release-control" in publish_npm
    assert "path: release-source" in publish_npm
    assert "working-directory: release-source" in publish_npm
    assert ".github/scripts/validate_npm_release_recovery.py" in publish_npm
    assert ".github/scripts/publish_npm_package.py" in publish_npm
    assert "environment: release" in publish_npm
    assert "id-token: write" in publish_npm


def test_windows_wheel_does_not_reuse_archive_executable_output() -> None:
    """PEP 517 must not rebuild the archive lane's still-open soldr.exe."""

    release = (WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8")
    matrix = release.split("      matrix:\n", 1)[1].split("\n    steps:\n", 1)[0]
    wheel_step = _step_block(release, "Build wheel through Soldr")
    wheel_smoke = _step_block(release, "Smoke test wheel")

    assert "build_driver:" not in matrix
    # soldr#2469 step 2.1: the Windows Linux-cross entries now come from the
    # contract-generated matrix (see test_canonical_target_contract.py).
    assert "include: ${{ fromJSON(needs.prepare.outputs.build_matrix) }}" in matrix
    assert ".github/scripts/prepare_release_wheel.py" in wheel_step
    assert "--target-dir target" not in wheel_step
    assert "!contains(matrix.target, 'pc-windows-msvc')" in wheel_smoke

    # The bootstrap-driver step still resolves the suffix inline; the release
    # build step's copy moved into `native_release_build.matrix_driver()`
    # (soldr#2469 step 2.2). Both must still be Windows-aware, so each is
    # asserted where its logic actually lives.
    assert 'case "$RUNNER_OS" in' in _step_block(
        release, "Restore executable bit on bootstrap driver"
    )
    native_builder = (
        REPO_ROOT / ".github" / "scripts" / "native_release_build.py"
    ).read_text(encoding="utf-8")
    assert 'RUNNER_OS") == "Windows"' in native_builder
    assert 'f"soldr{suffix}"' in native_builder

    wheel_preparer = (
        REPO_ROOT / ".github" / "scripts" / "prepare_release_wheel.py"
    ).read_text(encoding="utf-8")
    assert "runner_binary_suffix(runner_os)" in wheel_preparer
    package_archive = _step_block(
        release, "Package combined archive (tar.zst level 19)"
    )
    archive_packager = (
        REPO_ROOT / ".github" / "scripts" / "package_release_archive.py"
    ).read_text(encoding="utf-8")
    assert ".github/scripts/package_release_archive.py" in package_archive
    assert "runner_binary_suffix(runner_os)" in archive_packager

    archive_smoke = _step_block(release, "Smoke test combined tar.zst archive")
    archive_smoker = (
        REPO_ROOT / ".github" / "scripts" / "release_archive_smoke.py"
    ).read_text(encoding="utf-8")
    assert ".github/scripts/release_archive_smoke.py" in archive_smoke
    assert "runner_binary_suffix(runner_os)" in archive_smoker

    smoke_windows = _job_block(release, "smoke_windows", "publish")
    assert "runner: windows-2025" in smoke_windows
    assert "target: x86_64-pc-windows-msvc" in smoke_windows
    assert "runner: windows-11-arm" in smoke_windows
    assert "target: aarch64-pc-windows-msvc" in smoke_windows
    # soldr#2469 step 2.1: the ARM64 Windows Linux-cross build entry lives
    # in the contract's release.build block.
    contract = json.loads(
        (REPO_ROOT / "ci" / "canonical-targets.json").read_text(encoding="utf-8")
    )
    arm_win = next(
        entry["release"]["build"]
        for entry in contract["targets"]
        if entry["triple"] == "aarch64-pc-windows-msvc"
    )
    assert arm_win["runner"] == "ubuntu-24.04"


def test_partial_immutable_release_can_recover_missing_pypi_wheels() -> None:
    release = (WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8")
    pypi_job = _job_block(release, "publish-pypi", "publish-npm")

    assert (
        "github_release_immutable: ${{ steps.validate.outputs.github_release_immutable }}"
        in release
    )
    # soldr#2469 step 2.2: the release lookup moved out of inline bash into
    # release_detect.py, so assert the property (the immutable flag is derived
    # from the real release object) at its new home. The recovery behaviour
    # itself — immutable-and-incomplete still lets PyPI republish — is covered
    # directly in tests/test_release_detect.py, which the inline bash could
    # never be tested for.
    detector = (REPO_ROOT / ".github" / "scripts" / "release_detect.py").read_text(
        encoding="utf-8"
    )
    assert "releases/tags/" in detector and "immutable" in detector, (
        "release detection must read the real GitHub release object to learn "
        "whether it is immutable"
    )
    assert "github_release_immutable != 'true'" in release
    assert "needs.verify_github_release.result == 'success'" in pypi_job
    assert "github.event_name == 'workflow_dispatch'" in pypi_job
    assert "inputs.force_pypi_publish" in pypi_job
    assert "skip-existing: true" in pypi_job


def test_cross_compile_docs_match_current_blessed_surfaces() -> None:
    docs = (REPO_ROOT / "docs" / "CROSS_COMPILE.md").read_text(encoding="utf-8")
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    release = (WORKFLOWS / "auto-release.yml").read_text(encoding="utf-8")

    assert "soldr build --release --target x86_64-pc-windows-msvc" in docs
    assert "soldr cargo xwin build --release" in docs
    assert "_cross-build-windows-host.yml" not in docs
    assert "cross-build-from-windows-x64-linux" not in docs
    assert "build-macos-x64.yml" not in ci
    assert "runs_on: macos-15-intel" in ci
    assert "runs_on: macos-15" in ci
    assert "soldr#1237" not in release
    assert "x86_64-apple-darwin: **intentionally omitted**" not in release


def test_every_artifact_consumer_derives_soldr_daemon() -> None:
    """soldr#3539: soldr-ci-e2e artifacts ship `soldr` once.

    A job that uses `package/soldr-daemon` from a downloaded artifact must
    first derive it with `cp ... soldr ... soldr-daemon`; the reld live-fetch
    jobs were missed the first time and failed with `No such file`.
    """
    workflows = Path(__file__).parents[1] / ".github" / "workflows"
    derive = re.compile(r"cp \S*soldr\S* +\S*(soldr-daemon|daemon_bin)")
    for workflow in sorted(workflows.glob("*.yml")):
        text = workflow.read_text(encoding="utf-8")
        for job in re.split(r"\n  [A-Za-z0-9_-]+:\n", text):
            uses = "package/soldr-daemon" in job or "$daemon_bin" in job
            downloads = "soldr-ci-e2e" in job or "download-artifact" in job
            if uses and downloads:
                assert derive.search(job), (
                    f"{workflow.name}: a job uses soldr-daemon without deriving it"
                )
