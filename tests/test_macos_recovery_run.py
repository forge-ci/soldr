"""Unit coverage for ci/macos_recovery_run.py (soldr#3076/#3078).

The `macos-recovery-replay.yml` workflow (nightly, on dispatch, and on PRs
labelled `macos-replay` -- soldr#3116; via `_ci-target-run.yml`)
executes this script's guest script inside a zackees/docker-mac-x64
Recovery guest and verifies the collected results with it. The guest never
runs under test here -- these pin the script's text, the collected-result
parsing, and the post-collection ownership/coverage verification, which are
the pure/subprocess-only surfaces this module exposes.
"""

import json
import os
import subprocess
import sys
from pathlib import Path

from conftest import (
    assert_recovery_verify_collected_contract,
    load_script_module,
    write_collected_recovery_summary,
)

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

REPO_ROOT = Path(__file__).resolve().parents[1]
MODULE = load_script_module(
    REPO_ROOT / "ci" / "macos_recovery_run.py", "macos_recovery_run"
)


def test_build_guest_script_is_posix_sh_and_self_contained() -> None:
    script = MODULE.build_guest_script()
    assert script.startswith("#!/bin/sh")
    assert "fetch soldr /tmp/soldr x" in script
    assert "/tmp/soldr --version" in script
    assert "/tmp/soldr --help" in script
    assert 'exit "$FAIL"' in script


def test_build_guest_script_declares_every_check() -> None:
    """Every name in CHECKS must be either a `fetch NAME` call (whose
    generic `fetch()` helper records `fetch_NAME` dynamically) or a literal
    `record NAME pass` call on its success path."""
    script = MODULE.build_guest_script()
    for name in MODULE.CHECKS:
        if name.startswith("fetch_"):
            file_name = name[len("fetch_") :]
            assert f"fetch {file_name} " in script, name
        else:
            assert f"record {name} pass" in script, name


def test_build_guest_script_replay_stages_are_present() -> None:
    script = MODULE.build_guest_script()
    assert "diskutil eraseDisk APFS Work" in script
    assert "/tmp/work" in script
    assert "nextest list \\" in script
    assert "nextest run $REUSE_ARGS \\" in script
    assert '--extract-to "$WORK/extract"' in script
    assert "--partition hash:1/1" in script
    assert "--no-fail-fast" in script
    assert 'TMPDIR="$WORK/tmp"' in script
    # soldr#3454: the native wrapper runs every test; no exec shim replaces it.
    assert 'exec "$@"' not in script
    assert "nextest_timeout_wrapper" not in script
    assert "-name soldr-nextest-wrapper" in script
    assert "RUSTUP_TOOLCHAIN" in script
    assert "SOLDR_TEST_WORKSPACE_ROOT" in script
    assert "SOLDR_TEST_FIXTURES_DIR" in script
    assert "SOLDR_USE_SYSTEM_CMAKE=1" in script


def test_guest_script_uses_requested_partition_for_list_and_run() -> None:
    script = MODULE.build_guest_script(partition="hash:2/3")
    assert script.count("--partition hash:2/3") == 2
    assert "--partition hash:1/1" not in script


def test_guest_script_rejects_invalid_partition() -> None:
    import pytest

    for partition in ("hash:0/3", "hash:4/3", "hash:1/0", "hash:1/3; touch /tmp/x"):
        with pytest.raises(ValueError):
            MODULE.build_guest_script(partition=partition)


def test_guest_script_samples_memory_around_nextest_run() -> None:
    """soldr#3136: memory evidence must bracket the suite and stop with it."""
    script = MODULE.build_guest_script()
    run = script.index("nextest run $REUSE_ARGS \\")
    start = script.index("MEM_SAMPLER_PID=$!")
    stop = script.index('kill "$MEM_SAMPLER_PID" 2>/dev/null')
    assert script.index("mem_sample() {") < start < run < stop
    assert (
        f"( while :; do sleep {MODULE.MEM_SAMPLE_SECS} >/dev/null 2>&1; mem_sample; done ) &"
    ) in script
    # Outside the continued nextest command, which comments would split.
    assert script.index('echo $? > "$WORK/nextest-run.rc"') < stop


def _mem_sample_functions(script: str) -> str:
    start = script.index("na() {")
    end = script.index("\n}\n", script.index("mem_sample() {")) + len("\n}\n")
    return script[start:end]


def _run_mem_sample(tmp_path: Path, stub_dir: Path) -> str:
    runner = tmp_path / "run.sh"
    runner.write_text(
        _mem_sample_functions(MODULE.build_guest_script()) + "mem_sample\n",
        encoding="utf-8",
    )
    # Stubs shadow `sysctl`/`ps`; the text tools resolve from the real PATH.
    path = f"{stub_dir}{os.pathsep}{os.environ.get('PATH', '')}"
    result = run_captured(
        ["sh", str(runner)],
        capture_output=True,
        text=True,
        check=True,
        env={"PATH": path},
    )
    return result.stdout.strip()


def _stub(stub_dir: Path, name: str, body: str) -> None:
    tool = stub_dir / name
    tool.write_text(f"#!/bin/sh\n{body}\n", encoding="utf-8")
    tool.chmod(0o755)


def test_mem_sample_reports_macos_memory_probes(tmp_path: Path) -> None:
    """One line: free memory, per-process aggregates, then top RSS."""
    stubs = tmp_path / "stubs"
    stubs.mkdir()
    _stub(
        stubs,
        "sysctl",
        'case "$2" in\n'
        "  vm.page_free_count) echo 2560 ;;\n"
        "  hw.pagesize) echo 4096 ;;\n"
        "  kern.memorystatus_vm_pressure_level) echo 4 ;;\n"
        "  vm.page_speculative_count) echo 256 ;;\n"
        "  vm.page_purgeable_count) echo 512 ;;\n"
        "  vm.page_pageable_external_count) echo 25600 ;;\n"
        "  vm.compressor_bytes_used) echo 314572800 ;;\n"
        "  vm.swapusage) echo 'total = 2048.00M  used = 1024.00M  free = 1024.00M' ;;\n"
        "  vm.loadavg) echo '{ 3.10 2.00 1.50 }' ;;\n"
        "  *) exit 1 ;;\n"
        "esac",
    )
    _stub(
        stubs,
        "ps",
        "printf '%s\\n' "
        "'2097152 /usr/bin/rustc' "
        "'204800 /tmp/h1/.soldr/broker/soldr-broker' "
        "'153600 /tmp/h2/.soldr/broker/soldr-broker' "
        "'10240 soldr-daemon' "
        "'512000 /Volumes/Work/soldr' "
        "'4096 /Volumes/Work/.soldr/bin/rustup' "
        "'2048 /bin/sh'",
    )
    line = _run_mem_sample(tmp_path, stubs)
    assert "\n" not in line
    assert line.startswith("[mem] t=")
    assert "free=10M" in line
    assert "pressure=4" in line
    assert "reclaim_spec/purge/ext=1/2/100M" in line
    assert "comp=300M" in line
    assert (
        "procs=7 daemon=1/10M broker=2/350M soldr=1/500M rustup=1/4M cargo=0/0M rustc=1/2048M"
    ) in line
    assert "top=[rustc:2048M soldr:500M soldr-broker:200M ]" in line
    assert "swap_used=1024.00M" in line
    assert "load=[ 3.10 2.00 1.50 ]" in line
    # The aggregates precede top/swap/load so a truncated sample keeps them.
    assert line.index("procs=") < line.index("top=") < line.index("load=")


def test_mem_sample_degrades_to_na_when_probes_fail(tmp_path: Path) -> None:
    """A Recovery guest missing a tool or sysctl key still prints a line."""
    stubs = tmp_path / "stubs"
    stubs.mkdir()
    _stub(stubs, "sysctl", "exit 1")
    _stub(stubs, "ps", "exit 1")
    line = _run_mem_sample(tmp_path, stubs)
    assert line.startswith("[mem] t=")
    assert "reclaim_spec/purge/ext=//M" in line, line
    for probe in (
        "free=n/a",
        "pressure=n/a",
        "comp=n/a",
        "procs=n/a",
        "top=[n/a]",
        "swap_used=n/a",
        "load=[n/a]",
    ):
        assert probe in line, line


def _command_substitutions(script: str) -> list[str]:
    """Every `$( ... )` span, matched by parenthesis depth."""
    spans = []
    start = script.find("$(")
    while start != -1:
        depth, index = 0, start + 1
        while index < len(script):
            if script[index] == "(":
                depth += 1
            elif script[index] == ")":
                depth -= 1
                if depth == 0:
                    break
            index += 1
        spans.append(script[start : index + 1])
        start = script.find("$(", start + 2)
    return spans


def test_no_case_statement_inside_a_command_substitution() -> None:
    """Recovery's bash 3.2 cannot parse `case` inside `$( )`.

    `bash -n` here is bash 5, which accepts it, so only a replay could catch
    it: run 34853028731 died in `nextest_run` with `syntax error near
    unexpected token ;;` and no suite ran at all.
    """
    script = MODULE.build_guest_script()
    offenders = [span for span in _command_substitutions(script) if "case " in span]
    assert not offenders, offenders


def test_build_guest_script_is_valid_posix_sh_syntax() -> None:
    """`bash -n` catches gross syntax breakage even though this is /bin/sh."""
    script = MODULE.build_guest_script()
    result = run_captured(
        ["bash", "-n", "/dev/stdin"],
        input=script,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr


def test_parse_summary_splits_status_and_detail() -> None:
    text = "arch=pass:x86_64\nversion=fail:not soldr\n"
    results = {r.name: r for r in MODULE.parse_summary(text)}
    assert results["arch"] == MODULE.CheckResult("arch", True, "x86_64")
    assert results["version"] == MODULE.CheckResult("version", False, "not soldr")


def test_parse_summary_flags_malformed_lines() -> None:
    results = {r.name: r for r in MODULE.parse_summary("garbage line with no equals\n")}
    assert results["summary_line_1"].passed is False


def _passing_summary_lines() -> list[str]:
    return [f"{name}=pass:ok" for name in MODULE.CHECKS]


def test_verify_collected_matches_the_shared_recovery_contract(tmp_path: Path) -> None:
    assert_recovery_verify_collected_contract(
        MODULE, tmp_path, passing_lines=_passing_summary_lines()
    )


def test_main_emit_guest_script_writes_the_output_file(tmp_path: Path) -> None:
    output = tmp_path / "recovery-run.sh"
    rc = MODULE.main(["emit-guest-script", "--output", str(output)])
    assert rc == 0
    assert output.read_text(encoding="utf-8").startswith("#!/bin/sh")


def _contract_document() -> dict:
    """The executor contract exactly as the emitted JSON document carries it."""
    return json.loads(json.dumps(MODULE.executor_contract().to_dict()))


def test_executor_contract_owns_every_3084_handoff_edge() -> None:
    """soldr#3084 may close only when soldr#3294 owns every sharp edge."""
    contract = _contract_document()

    assert contract["schema_version"] == 1
    assert contract["superseded_issue"] == "soldr#3084"
    assert contract["owner_issue"] == "soldr#3294"
    assert contract["target"] == "x86_64-apple-darwin"
    assert contract["backend"] == "macos-recovery"
    assert contract["readiness"] == {
        "default_enabled": False,
        "blocked_by": ["soldr#3088", "soldr#3136"],
        "unavailable_action": "fail-closed-with-no-run-guidance",
    }
    assert contract["nextest"]["argv_prefix"] == ["cargo-nextest", "nextest"]
    assert contract["nextest"]["archive"] == "tests.tar.zst"
    assert contract["nextest"]["inventory_arguments"] == [
        "--archive-file=$WORK/tests.tar.zst",
        "--extract-to=$WORK/extract",
        "--workspace-remap=$WORK/workspace",
        "--profile=target-run",
        "--message-format=json-pretty",
    ]
    assert contract["nextest"]["reuse_arguments"] == {
        "preferred": [
            "--binaries-metadata=$REUSE_BIN_META",
            "--cargo-metadata=$REUSE_CARGO_META",
            "--target-dir-remap=$WORK/extract/target",
        ],
        "fallback": [
            "--archive-file=$WORK/tests.tar.zst",
            "--extract-to=$WORK/extract",
            "--extract-overwrite",
        ],
    }
    assert contract["nextest"]["selection_filter"] == {
        "source_file": "$WORK/filter.txt",
        "expression_variable": "$FILTER",
        "argument": "-E",
    }
    assert contract["nextest"]["selected_list_arguments"] == [
        "$REUSE_ARGS",
        "--workspace-remap=$WORK/workspace",
        "--profile=target-run",
        "--partition=$REPLAY_PARTITION",
        "-E=$FILTER",
        "--message-format=json-pretty",
    ]
    assert contract["nextest"]["run_arguments"] == [
        "$REUSE_ARGS",
        "--workspace-remap=$WORK/workspace",
        "--profile=target-run",
        "--partition=$REPLAY_PARTITION",
        "-E=$FILTER",
        "--no-fail-fast",
    ]
    assert contract["nextest"]["zero_tests_is_failure"] is True
    assert contract["required_guest_env"] == {
        "HOME": "$WORK/home",
        "TMPDIR": "$WORK/tmp",
        "SOLDR_BIN": "/tmp/soldr",
        "SOLDR_INTERNAL_DAEMON_EXE": "$WORK/soldr-daemon",
        "NEXTEST_BIN": "$WORK/cargo-nextest",
        "SOLDR_TEST_FIXTURES_DIR": "$WORK/fixtures",
        "SOLDR_TEST_WORKSPACE_ROOT": "$WORK/workspace",
        "SOLDR_USE_SYSTEM_CMAKE": "1",
        "SOLDR_TARGET_WARN_FREE_GB": "1",
        "SOLDR_TARGET_BLOCK_FREE_GB": "1",
        "RUSTUP_TOOLCHAIN": "channel-from-toolchain-ensure",
        "CARGO_HOME": "$HOME/.soldr/cargo",
        "RUSTUP_HOME": "$HOME/.soldr/rustup",
        "PATH": "$WORK/shims:$HOME/.soldr/cargo/bin:$PATH",
    }
    assert set(contract["required_share_files"]) == set(MODULE.REPLAY_SHARE_FILES)
    assert contract["capabilities"] == {
        "shell": "bash-3.2-posix-sh",
        "execution_model": "one-script-per-boot-no-command-exec",
        "python3": False,
        "git": False,
        "dyld_shared_cache": False,
        "system_c_compiler": False,
        "xcrun": False,
        "sdk": False,
        "preinstalled_rust_toolchain": False,
        "toolchain_provisioning": "soldr-managed-during-guest-script",
        "isolated_toolchain_homes": False,
        "tmp_storage": "ramdisk-bounded-by-guest-memory",
        "scratch_storage": "formatted-qcow2-with-tmp-fallback",
        "workspace_source_staged": True,
        "fixtures_staged": True,
    }


def test_executor_contract_matches_the_emitted_guest_program() -> None:
    contract = _contract_document()
    script = MODULE.build_guest_script()

    for name, value in contract["required_guest_env"].items():
        if value == "channel-from-toolchain-ensure":
            assert f'{name}="$CHANNEL"' in script
        else:
            assert f'{name}="{value}"' in script or f"{name}={value}" in script
        assert any(
            name in line.strip().removeprefix("export ").split()
            for line in script.splitlines()
            if line.strip().startswith("export ")
        )

    inventory = "\n".join(MODULE._stage_nextest_list_all())
    selected = "\n".join(MODULE._stage_nextest_list_selected("hash:2/3"))
    run = "\n".join(MODULE._stage_nextest_run("hash:2/3"))

    def assert_arguments(block: str, arguments: list[str]) -> None:
        for argument in arguments:
            argument = argument.replace("$REPLAY_PARTITION", "hash:2/3")
            if argument == "$REUSE_ARGS":
                assert "nextest " in block and "$REUSE_ARGS" in block
            elif "=" not in argument:
                assert argument in block
            else:
                option, value = argument.split("=", maxsplit=1)
                assert f'{option} "{value}"' in block or f"{option} {value}" in block

    assert '"$NEXTEST_BIN" nextest list' in inventory
    assert_arguments(inventory, contract["nextest"]["inventory_arguments"])
    for reuse_arguments in contract["nextest"]["reuse_arguments"].values():
        assert_arguments(inventory, reuse_arguments)

    filter_contract = contract["nextest"]["selection_filter"]
    for block in (selected, run):
        assert f'FILTER=$(cat "{filter_contract["source_file"]}")' in block
        assert (
            f'{filter_contract["argument"]} "{filter_contract["expression_variable"]}"'
            in block
        )
    assert_arguments(selected, contract["nextest"]["selected_list_arguments"])
    assert_arguments(run, contract["nextest"]["run_arguments"])
    assert contract["result_gate"] == {
        "guest_exit_code_must_be_zero": True,
        "all_checks_must_pass": list(MODULE.CHECKS),
        "requires_inventory": True,
        "requires_junit": True,
        "diagnostics_are_always_collected": True,
    }


def test_main_describe_executor_writes_stable_json(tmp_path: Path) -> None:
    output = tmp_path / "executor-contract.json"
    rc = MODULE.main(["describe-executor", "--output", str(output)])
    assert rc == 0
    assert json.loads(output.read_text(encoding="utf-8")) == _contract_document()


def test_main_verify_collected_delegates_to_verify_collected(tmp_path: Path) -> None:
    collected = write_collected_recovery_summary(
        tmp_path / "collected", _passing_summary_lines()
    )
    rc = MODULE.main(
        [
            "verify-collected",
            "--collected",
            str(collected),
            "--guest-exit-code",
            "0",
        ]
    )
    assert rc == 0


# --------------------------------------------------------------------------
# verify_replay_artifacts / --manifest wiring (soldr#3078)
# --------------------------------------------------------------------------

_TARGET = "x86_64-pc-windows-msvc"

_PACKAGE = "demo"
_BINARY = "native"


def _build_fake_repo_root(tmp_path: Path) -> Path:
    """A self-contained fixture repo root, isolated from the real crates/
    tree -- `validate_source_ownership` scans *every* crate it finds, so
    pointing it at the real `REPO_ROOT` with a manifest that only covers one
    dummy classification fails on every real host-sensitive test source the
    manifest does not mention. Mirrors
    `test_target_run_ownership.test_inverse_guard_requires_explicit_classification_but_not_replay`'s
    fixture shape."""
    root = tmp_path / "fake-repo"
    tests_dir = root / "crates" / _PACKAGE / "tests" / _BINARY
    tests_dir.mkdir(parents=True)
    (tests_dir / "main.rs").write_text("mod process;\n", encoding="utf-8")
    (tests_dir / "process.rs").write_text(
        "#[test]\nfn kills_tree() {}\n", encoding="utf-8"
    )
    return root


def _write_manifest(path: Path) -> None:
    path.write_text(
        json.dumps(
            {
                "schema_version": 2,
                "policy_issue": "soldr#2999",
                "source_classifications": [
                    {
                        "id": "demo-native",
                        "package": _PACKAGE,
                        "binary": _BINARY,
                        "disposition": "target-replay",
                        "reason": "test fixture",
                        "modules": ["process"],
                    }
                ],
                "replay_selectors": [
                    {
                        "id": "demo-process-module",
                        "source_id": "demo-native",
                        "test_prefix": "process::",
                        "reason": "test fixture",
                    }
                ],
            }
        ),
        encoding="utf-8",
    )


def _write_all_list(path: Path, *, matched: bool) -> None:
    test_name = "process::kills_tree" if matched else "unrelated::not_owned"
    path.write_text(
        json.dumps(
            {
                "test-count": 1,
                "rust-suites": {
                    "suite-0": {
                        "package-name": _PACKAGE,
                        "binary-name": _BINARY,
                        "testcases": {test_name: {"ignored": False}},
                    }
                },
            }
        ),
        encoding="utf-8",
    )


def _write_list_json(path: Path) -> None:
    path.write_text(
        json.dumps(
            {
                "test-count": 1,
                "rust-suites": {
                    "suite-0": {
                        "package-name": _PACKAGE,
                        "binary-name": _BINARY,
                        "testcases": {"process::kills_tree": {"ignored": False}},
                    }
                },
            }
        ),
        encoding="utf-8",
    )


def _write_junit(path: Path) -> None:
    path.write_text(
        '<?xml version="1.0"?>\n'
        '<testsuites><testsuite name="s" tests="1" failures="0" errors="0" '
        'skipped="0"/></testsuites>\n',
        encoding="utf-8",
    )


def test_verify_replay_artifacts_passes_for_a_consistent_replay(
    tmp_path: Path,
) -> None:
    manifest = tmp_path / "manifest.json"
    _write_manifest(manifest)
    repo_root = _build_fake_repo_root(tmp_path)
    collected = tmp_path / "collected"
    collected.mkdir()
    _write_all_list(collected / "all-list.json", matched=True)
    _write_list_json(collected / "list.json")
    _write_junit(collected / "junit.xml")

    rc = MODULE.verify_replay_artifacts(
        collected, manifest=manifest, repo_root=repo_root, target=_TARGET
    )
    assert rc == 0


def test_verify_replay_artifacts_fails_when_all_list_is_missing(
    tmp_path: Path,
) -> None:
    manifest = tmp_path / "manifest.json"
    _write_manifest(manifest)
    repo_root = _build_fake_repo_root(tmp_path)
    collected = tmp_path / "collected"
    collected.mkdir()
    _write_list_json(collected / "list.json")
    _write_junit(collected / "junit.xml")

    try:
        MODULE.verify_replay_artifacts(
            collected, manifest=manifest, repo_root=repo_root, target=_TARGET
        )
        raise AssertionError("expected SystemExit")
    except SystemExit as error:
        assert "all-list.json" in str(error)


def test_verify_replay_artifacts_fails_on_a_stale_selector(tmp_path: Path) -> None:
    """A selector matching zero tests in the guest's own inventory is the
    exact staleness case `build_selection` exists to catch -- verified here
    post-collection since the guest had no inventory to check it against."""
    manifest = tmp_path / "manifest.json"
    _write_manifest(manifest)
    repo_root = _build_fake_repo_root(tmp_path)
    collected = tmp_path / "collected"
    collected.mkdir()
    _write_all_list(collected / "all-list.json", matched=False)
    _write_list_json(collected / "list.json")
    _write_junit(collected / "junit.xml")

    try:
        MODULE.verify_replay_artifacts(
            collected, manifest=manifest, repo_root=repo_root, target=_TARGET
        )
        raise AssertionError("expected SystemExit")
    except SystemExit as error:
        assert "ownership" in str(error)


def test_verify_replay_artifacts_fails_when_junit_is_missing(tmp_path: Path) -> None:
    manifest = tmp_path / "manifest.json"
    _write_manifest(manifest)
    repo_root = _build_fake_repo_root(tmp_path)
    collected = tmp_path / "collected"
    collected.mkdir()
    _write_all_list(collected / "all-list.json", matched=True)
    _write_list_json(collected / "list.json")

    try:
        MODULE.verify_replay_artifacts(
            collected, manifest=manifest, repo_root=repo_root, target=_TARGET
        )
        raise AssertionError("expected SystemExit")
    except SystemExit as error:
        assert "coverage summary" in str(error)


def test_main_verify_collected_runs_replay_artifacts_when_manifest_is_given(
    tmp_path: Path,
) -> None:
    manifest = tmp_path / "manifest.json"
    _write_manifest(manifest)
    repo_root = _build_fake_repo_root(tmp_path)
    collected = write_collected_recovery_summary(
        tmp_path / "collected", _passing_summary_lines()
    )
    _write_all_list(collected / "all-list.json", matched=True)
    _write_list_json(collected / "list.json")
    _write_junit(collected / "junit.xml")

    rc = MODULE.main(
        [
            "verify-collected",
            "--collected",
            str(collected),
            "--guest-exit-code",
            "0",
            "--manifest",
            str(manifest),
            "--repo-root",
            str(repo_root),
            "--target",
            _TARGET,
        ]
    )
    assert rc == 0


def test_main_verify_collected_requires_repo_root_and_target_with_manifest(
    tmp_path: Path,
) -> None:
    manifest = tmp_path / "manifest.json"
    _write_manifest(manifest)
    collected = write_collected_recovery_summary(
        tmp_path / "collected", _passing_summary_lines()
    )

    try:
        MODULE.main(
            [
                "verify-collected",
                "--collected",
                str(collected),
                "--guest-exit-code",
                "0",
                "--manifest",
                str(manifest),
            ]
        )
        raise AssertionError("expected SystemExit")
    except SystemExit:
        pass


def test_guest_phase_timing_runs_under_posix_sh(tmp_path: Path) -> None:
    """Execute the generated phase functions with a deterministic clock."""
    script = MODULE.build_guest_script().split("stage_start core", 1)[0]
    script = script.replace("/tmp/results", str(tmp_path))
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    date = bin_dir / "date"
    date.write_text(
        '#!/bin/sh\nif [ -e "$DATE_STATE" ]; then echo 105; '
        'else : > "$DATE_STATE"; echo 100; fi\n',
        encoding="utf-8",
    )
    date.chmod(0o755)
    guest = tmp_path / "phase-test.sh"
    guest.write_text(
        script + "\nstage_start toolchain\nstage_end toolchain\n", encoding="utf-8"
    )
    with (tmp_path / "phase-output.log").open("w", encoding="utf-8") as output:
        subprocess.run(
            ["sh", str(guest)],
            env={
                **os.environ,
                "PATH": f"{bin_dir}:{os.environ['PATH']}",
                "DATE_STATE": str(tmp_path / "date-state"),
            },
            stdout=output,
            stderr=subprocess.STDOUT,
            check=True,
        )
    lines = (tmp_path / "phase-times.tsv").read_text(encoding="utf-8").splitlines()
    assert lines == [
        "stage\tevent\tunix_time\tduration_seconds",
        "toolchain\tstart\t100\t",
        "toolchain\tend\t105\t5",
    ]


def test_toolchain_stage_exposes_the_bootstrapped_rustup() -> None:
    """soldr#3846: the guest has no system rustup; after `toolchain ensure`
    the rustup soldr bootstrapped into its own root must be reachable by the
    tests: its homes are exported after ensure, and its cargo/bin joins PATH
    behind the shims once `toolchain link` succeeds."""
    script = "\n".join(MODULE._stage_toolchain())
    ensure = script.index("toolchain ensure --json")
    homes = script.index("export CARGO_HOME RUSTUP_HOME")
    link = script.index("toolchain link --shim-dir")
    path = script.index('PATH="$WORK/shims:$HOME/.soldr/cargo/bin:$PATH"')
    assert ensure < homes < link < path
    assert 'CARGO_HOME="$HOME/.soldr/cargo"' in script
    assert 'RUSTUP_HOME="$HOME/.soldr/rustup"' in script
