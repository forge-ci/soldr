"""Contract tests for the dispatch-only Windows guest feasibility probe."""

import hashlib
import importlib.util
import json
import urllib.request
from argparse import Namespace
from contextlib import nullcontext
from dataclasses import replace
from datetime import datetime, timezone
from io import BytesIO
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "ci" / "windows_guest_probe.py"
spec = importlib.util.spec_from_file_location("windows_guest_probe", SCRIPT)
assert spec is not None and spec.loader is not None
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


def test_summary_keeps_unmeasured_capabilities_explicit():
    report = probe.make_report(
        host=probe.HostInfo(kvm=False, free_bytes=20_000_000_000),
        timings=probe.PhaseTimings(),
        guest=None,
        disk=probe.DiskMeasure(),
    )
    assert report.decision == "no-go"
    assert report.reason == "KVM is unavailable on this runner"
    assert report.nextest.status == "not-run"
    assert report.capabilities.powershell == "not-measured"
    assert report.timings.iso_download_seconds is None
    assert report.timings.runtime_prep_seconds is None
    assert report.timings.runtime_install_seconds is None
    assert report.timings.git_prep_seconds is None


def test_vc_redist_download_is_pinned_and_rejects_drift(tmp_path, monkeypatch):
    payload = b"test redistributable payload"
    expected = hashlib.sha256(payload).hexdigest()
    monkeypatch.setattr(
        urllib.request, "urlopen", lambda *_args, **_kwargs: BytesIO(payload)
    )
    probe._fetch_vc_redist(tmp_path, expected_sha256=expected)
    assert (tmp_path / "vc_redist.x64.exe").read_bytes() == payload
    try:
        probe._fetch_vc_redist(tmp_path, expected_sha256="0" * 64)
    except OSError as exc:
        assert "sha256" in str(exc).lower()
    else:
        raise AssertionError("redistributable checksum drift must fail")


def test_mingit_download_is_pinned_and_rejects_drift(tmp_path, monkeypatch):
    payload = b"test MinGit archive"
    expected = hashlib.sha256(payload).hexdigest()
    monkeypatch.setattr(
        urllib.request, "urlopen", lambda *_args, **_kwargs: BytesIO(payload)
    )
    probe._fetch_mingit(tmp_path, expected_sha256=expected)
    assert (tmp_path / "mingit.zip").read_bytes() == payload
    try:
        probe._fetch_mingit(tmp_path, expected_sha256="0" * 64)
    except OSError as exc:
        assert "sha256" in str(exc).lower()
    else:
        raise AssertionError("MinGit checksum drift must fail")


def test_summary_requires_real_replay_counts_for_go():
    guest = probe.GuestResult(
        nextest=probe.NextestResult(run=8, passed=8, failed=0, exit_code=0),
        capabilities=probe.GuestCapabilities(powershell="5.1", admin=True),
    )
    report = probe.make_report(
        host=probe.HostInfo(kvm=True, free_bytes=60_000_000_000),
        timings=probe.PhaseTimings(
            iso_download_seconds=80, install_seconds=400, boot_seconds=65
        ),
        guest=guest,
        disk=probe.DiskMeasure(
            image_bytes=12_000_000_000, compressed_bytes=6_000_000_000
        ),
    )
    assert report.decision == "go"
    assert report.nextest.passed == 8
    assert report.timings.usable_shell_seconds == 545


def test_desktop_report_identifies_edition_and_selector():
    report = probe.make_report(
        host=probe.HostInfo(kvm=False),
        timings=probe.PhaseTimings(),
        guest=None,
        disk=probe.DiskMeasure(),
        guest_edition="win11-enterprise",
    )
    assert report.edition == "Windows 11 Enterprise evaluation"
    assert report.dockur_version == "11e"


def test_failed_replay_is_not_a_go():
    guest = probe.GuestResult(
        nextest=probe.NextestResult(run=1, passed=0, failed=1, exit_code=100)
    )
    report = probe.make_report(
        host=probe.HostInfo(kvm=True, free_bytes=60_000_000_000),
        timings=probe.PhaseTimings(
            iso_download_seconds=80, install_seconds=400, boot_seconds=65
        ),
        guest=guest,
        disk=probe.DiskMeasure(),
    )
    assert report.decision == "no-go"


def test_guest_exception_is_named_in_no_go_report():
    report = probe.make_report(
        host=probe.HostInfo(kvm=True),
        timings=probe.PhaseTimings(observed_total_seconds=510),
        guest=probe.GuestResult(
            error="native executable could not start",
            nextest=probe.NextestResult(status="not-run", run=0),
        ),
        disk=probe.DiskMeasure(),
    )
    assert report.decision == "no-go"
    assert report.guest_error == "native executable could not start"
    assert "native executable could not start" in report.reason


def test_dockur_log_timings_do_not_invent_a_boot_boundary():
    log = "\n".join(
        [
            "2026-09-22T00:00:10Z Downloading Windows Server 2025...",
            "2026-09-22T00:01:10Z Extracting Windows Server 2025 image...",
            "2026-09-22T00:02:00Z Booting Windows using QEMU v10...",
        ]
    )
    shell_ready = datetime(2026, 9, 22, 0, 5, tzinfo=timezone.utc).timestamp()
    timings = probe._phase_timings(log, shell_ready - 300, shell_ready)
    assert timings.iso_download_seconds == 60
    # Without firmware boot markers there is no install/first-boot boundary:
    # report only the combined interval, never a guessed split.
    assert timings.boot_seconds is None
    assert timings.install_seconds is None
    assert timings.install_plus_first_boot_seconds == 180


def test_preflight_reports_no_go_and_removes_its_own_scratch(tmp_path, monkeypatch):
    monkeypatch.setattr(probe, "_kvm_usable", lambda: False)
    scratch = tmp_path / "unique-scratch"
    output = tmp_path / "result.json"
    args = Namespace(
        repo=ROOT,
        artifact=tmp_path / "missing",
        scratch=scratch,
        output=output,
        run_id="test",
        timeout_seconds=1,
    )
    assert probe.run_probe(args) == 0
    report = json.loads(output.read_text())
    assert report["reason"] == "KVM is unavailable on this runner"
    assert not scratch.exists()


def test_root_owned_scratch_parent_is_prepared_for_runner(tmp_path, monkeypatch):
    scratch = tmp_path / "unique-scratch"
    mkdir = Path.mkdir
    calls = []

    def root_owned_mkdir(path, *args, **kwargs):
        if path == scratch:
            raise PermissionError("root-owned /mnt")
        return mkdir(path, *args, **kwargs)

    def fake_run(*command, **_kwargs):
        calls.append(command)
        mkdir(scratch)
        return SimpleNamespace(stdout="", stderr="", returncode=0)

    monkeypatch.setattr(Path, "mkdir", root_owned_mkdir)
    monkeypatch.setattr(probe, "_run", fake_run)
    probe._prepare_scratch(scratch, privileged_root=tmp_path)
    assert scratch.is_dir()
    assert calls == [
        (
            "sudo",
            "install",
            "-d",
            "-o",
            str(probe.os.getuid()),
            "-g",
            str(probe.os.getgid()),
            str(scratch),
        )
    ]


def test_scratch_setup_error_reports_no_go_and_removes_partial_directory(
    tmp_path, monkeypatch
):
    scratch = tmp_path / "scratch"

    def partial_setup(path):
        path.mkdir()
        raise OSError("simulated sudo install failure")

    monkeypatch.setattr(probe, "_prepare_scratch", partial_setup)
    output = tmp_path / "result.json"
    args = Namespace(
        repo=ROOT,
        artifact=tmp_path,
        scratch=scratch,
        output=output,
        run_id="test",
        timeout_seconds=1,
    )
    assert probe.run_probe(args) == 0
    report = json.loads(output.read_text())
    assert report["decision"] == "no-go"
    assert "simulated sudo install failure" in report["reason"]
    assert not scratch.exists()


def test_root_owned_guest_storage_gets_scoped_privileged_cleanup(tmp_path, monkeypatch):
    scratch = tmp_path / "soldr-windows-guest-test"
    scratch.mkdir()
    rmtree = probe.shutil.rmtree
    calls = []

    def denied(_path):
        raise PermissionError("root-owned Docker storage")

    def fake_run(*command, **_kwargs):
        calls.append(command)
        rmtree(scratch)
        return SimpleNamespace(stdout="", stderr="", returncode=0)

    monkeypatch.setattr(probe.shutil, "rmtree", denied)
    monkeypatch.setattr(probe, "_run", fake_run)
    probe._remove_scratch(scratch, "test", privileged_root=tmp_path)
    assert not scratch.exists()
    assert calls == [("sudo", "rm", "-rf", "--", str(scratch))]


def test_privileged_cleanup_refuses_unowned_path(tmp_path, monkeypatch):
    scratch = tmp_path / "unrelated"
    scratch.mkdir()

    def denied(_path):
        raise PermissionError("root-owned")

    monkeypatch.setattr(probe.shutil, "rmtree", denied)

    def unexpected_run(*_args, **_kwargs):
        raise AssertionError("sudo must not remove an unrelated directory")

    monkeypatch.setattr(probe, "_run", unexpected_run)
    try:
        probe._remove_scratch(scratch, "test", privileged_root=tmp_path)
    except PermissionError:
        pass
    else:
        raise AssertionError("unowned scratch path should be rejected")


def test_privileged_cleanup_failure_is_not_hidden(tmp_path, monkeypatch):
    scratch = tmp_path / "soldr-windows-guest-test"
    scratch.mkdir()
    rmtree = probe.shutil.rmtree

    def denied(_path):
        raise PermissionError("root-owned")

    def sudo_denied(*_args, **_kwargs):
        raise OSError("sudo rm denied")

    monkeypatch.setattr(probe.shutil, "rmtree", denied)
    monkeypatch.setattr(probe, "_run", sudo_denied)
    try:
        probe._remove_scratch(scratch, "test", privileged_root=tmp_path)
    except OSError as exc:
        assert "sudo rm denied" in str(exc)
    else:
        raise AssertionError("failed privileged cleanup must be reported")
    finally:
        rmtree(scratch)


def test_scratch_cleanup_failure_is_a_failing_no_go(tmp_path, monkeypatch):
    scratch = tmp_path / "soldr-windows-guest-test"
    output = tmp_path / "result.json"
    monkeypatch.setattr(probe, "_kvm_usable", lambda: False)

    def denied(_path):
        raise PermissionError("root-owned")

    monkeypatch.setattr(probe.shutil, "rmtree", denied)
    args = Namespace(
        repo=ROOT,
        artifact=tmp_path,
        scratch=scratch,
        output=output,
        run_id="test",
        timeout_seconds=1,
    )
    assert probe.run_probe(args) == 1
    report = json.loads(output.read_text())
    assert report["decision"] == "no-go"
    assert "cleanup failed" in report["reason"].lower()


def test_shell_and_replay_have_distinct_timestamps(tmp_path, monkeypatch):  # noqa: C901
    monkeypatch.setattr(probe, "_kvm_usable", lambda: True)
    monkeypatch.setattr(
        probe.shutil, "disk_usage", lambda _: SimpleNamespace(free=60 * 1024**3)
    )
    monkeypatch.setattr(probe.time, "sleep", lambda _: None)
    clock = iter([105.0, 107.0, 110.0])
    monkeypatch.setattr(probe.time, "time", lambda: next(clock))
    marker_times = iter([107.0, 107.5, 108.0, 108.2, 108.7, 110.0])
    monkeypatch.setattr(probe, "_host_created_at", lambda *_: next(marker_times))
    shared = tmp_path / "scratch" / "shared"

    def stage(_repo, _artifact, shared_dir, oem_dir):
        shared_dir.mkdir()
        oem_dir.mkdir()

    monkeypatch.setattr(probe, "_stage_payload", stage)
    monkeypatch.setattr(probe, "_disk_measure", lambda _: probe.DiskMeasure())
    inspections = 0

    def fake_run(*command, **_kwargs):
        nonlocal inspections
        if command[:2] == ("docker", "inspect") and len(command) == 3:
            return SimpleNamespace(stdout="", stderr="No such object", returncode=1)
        if command[:2] == ("docker", "inspect"):
            inspections += 1
            if inspections == 1:
                (shared / "guest-shell-ready.txt").write_text("ready")
            elif inspections == 2:
                (shared / "runtime-install-start.txt").write_text("ready")
            elif inspections == 3:
                (shared / "runtime-ready.txt").write_text("ready")
            elif inspections == 4:
                (shared / "git-install-start.txt").write_text("ready")
            elif inspections == 5:
                (shared / "tools-ready.txt").write_text("ready")
            elif inspections == 6:
                (shared / "guest-result.json").write_text(
                    json.dumps(
                        {
                            "nextest": {
                                "run": 8,
                                "passed": 8,
                                "failed": 0,
                                "exit_code": 0,
                            }
                        }
                    )
                )
            return SimpleNamespace(stdout="true", stderr="", returncode=0)
        return SimpleNamespace(stdout="", stderr="", returncode=0)

    monkeypatch.setattr(probe, "_run", fake_run)
    output = tmp_path / "result.json"
    args = Namespace(
        repo=ROOT,
        artifact=tmp_path,
        scratch=tmp_path / "scratch",
        output=output,
        run_id="test",
        timeout_seconds=1,
    )
    assert probe.run_probe(args) == 0
    report = json.loads(output.read_text())
    assert report["timings"]["usable_shell_seconds"] == 2
    assert report["timings"]["runtime_prep_seconds"] == 0.5
    assert report["timings"]["runtime_install_seconds"] == 0.5
    assert report["timings"]["git_prep_seconds"] == 0.5
    assert round(report["timings"]["replay_seconds"], 1) == 1.3
    assert report["decision"] == "go"
    assert (
        json.loads((tmp_path / "windows-guest-raw-result.json").read_text())["nextest"][
            "passed"
        ]
        == 8
    )
    assert not shared.exists()


def test_operational_failure_reports_no_go_and_cleans_unique_container(
    tmp_path, monkeypatch
):
    monkeypatch.setattr(probe, "_kvm_usable", lambda: True)
    monkeypatch.setattr(
        probe.shutil, "disk_usage", lambda _: SimpleNamespace(free=60 * 1024**3)
    )
    monkeypatch.setattr(probe, "_stage_payload", lambda *_: None)
    calls = []

    def fake_run(*command, **_kwargs):
        calls.append(command)
        if command[:2] == ("docker", "run"):
            raise OSError("simulated Docker launch failure")
        if command[:2] == ("docker", "inspect"):
            return SimpleNamespace(stdout="", stderr="No such object", returncode=1)
        return SimpleNamespace(stdout="", stderr="", returncode=0)

    monkeypatch.setattr(probe, "_run", fake_run)
    output = tmp_path / "result.json"
    args = Namespace(
        repo=ROOT,
        artifact=tmp_path,
        scratch=tmp_path / "scratch",
        output=output,
        run_id="test",
        timeout_seconds=1,
    )
    assert probe.run_probe(args) == 0
    report = json.loads(output.read_text())
    assert report["decision"] == "no-go"
    assert "simulated Docker launch failure" in report["reason"]
    assert any(command[:3] == ("docker", "rm", "-f") for command in calls)
    assert not (tmp_path / "scratch").exists()


def test_desktop_launch_uses_11e_without_server_core_override(tmp_path, monkeypatch):
    monkeypatch.setattr(probe, "_kvm_usable", lambda: True)
    monkeypatch.setattr(
        probe.shutil, "disk_usage", lambda _: SimpleNamespace(free=60 * 1024**3)
    )
    monkeypatch.setattr(probe, "_stage_payload", lambda *_: None)
    launches = []

    def fake_run(*command, **_kwargs):
        if command[:2] == ("docker", "run"):
            launches.append(command)
            raise OSError("simulated Docker launch failure")
        if command[:2] == ("docker", "inspect"):
            return SimpleNamespace(stdout="", stderr="No such object", returncode=1)
        return SimpleNamespace(stdout="", stderr="", returncode=0)

    monkeypatch.setattr(probe, "_run", fake_run)
    output = tmp_path / "result.json"
    args = Namespace(
        repo=ROOT,
        artifact=tmp_path,
        scratch=tmp_path / "scratch",
        output=output,
        run_id="test",
        timeout_seconds=1,
        guest_edition="win11-enterprise",
    )
    assert probe.run_probe(args) == 0
    assert len(launches) == 1
    assert "VERSION=11e" in launches[0]
    assert "EDITION=core" not in launches[0]
    assert (
        json.loads(output.read_text())["edition"] == "Windows 11 Enterprise evaluation"
    )


def test_host_marker_time_uses_file_change_time(tmp_path):
    marker = tmp_path / "ready"
    started = probe.time.time() - 1
    marker.write_text("ready")
    created = probe._host_created_at(marker, started)
    assert created is not None
    assert started <= created <= probe.time.time()


def test_cleanup_failure_is_reported_and_fails_workflow(tmp_path, monkeypatch):
    monkeypatch.setattr(probe, "_kvm_usable", lambda: True)
    monkeypatch.setattr(
        probe.shutil, "disk_usage", lambda _: SimpleNamespace(free=60 * 1024**3)
    )
    monkeypatch.setattr(probe, "_stage_payload", lambda *_: None)

    def fake_run(*command, **_kwargs):
        if command[:2] == ("docker", "run"):
            raise OSError("launch failed")
        return SimpleNamespace(stdout="", stderr="", returncode=0)

    monkeypatch.setattr(probe, "_run", fake_run)
    output = tmp_path / "result.json"
    args = Namespace(
        repo=ROOT,
        artifact=tmp_path,
        scratch=tmp_path / "scratch",
        output=output,
        run_id="test",
        timeout_seconds=1,
    )
    assert probe.run_probe(args) == 1
    report = json.loads(output.read_text())
    assert report["decision"] == "no-go"
    assert "cleanup failed" in report["reason"].lower()


def test_log_failure_still_attempts_container_removal(tmp_path, monkeypatch):
    monkeypatch.setattr(probe, "_kvm_usable", lambda: True)
    monkeypatch.setattr(
        probe.shutil, "disk_usage", lambda _: SimpleNamespace(free=60 * 1024**3)
    )
    monkeypatch.setattr(probe, "_stage_payload", lambda *_: None)
    calls = []

    def fake_run(*command, **_kwargs):
        calls.append(command)
        if command[:2] == ("docker", "run"):
            raise OSError("launch failed")
        if command[:2] == ("docker", "logs"):
            raise OSError("log unavailable")
        if command[:2] == ("docker", "inspect"):
            return SimpleNamespace(stdout="", stderr="No such object", returncode=1)
        return SimpleNamespace(stdout="", stderr="", returncode=0)

    monkeypatch.setattr(probe, "_run", fake_run)
    args = Namespace(
        repo=ROOT,
        artifact=tmp_path,
        scratch=tmp_path / "scratch",
        output=tmp_path / "result.json",
        run_id="test",
        timeout_seconds=1,
    )
    assert probe.run_probe(args) == 0
    assert any(command[:3] == ("docker", "rm", "-f") for command in calls)
    assert not (tmp_path / "scratch").exists()


def test_workflow_is_dispatch_only_and_not_a_required_gate():
    workflow = (ROOT / ".github/workflows/windows-guest-probe.yml").read_text()
    assert "workflow_dispatch:" in workflow
    assert "pull_request:" not in workflow
    assert "push:" not in workflow
    assert "schedule:" not in workflow
    assert "_ci-cross-build-linux.yml" in workflow
    assert "windows_guest_probe.py" in workflow
    assert "windows-guest-nextest.log" in workflow
    assert "windows-guest-vc-redist.log" in workflow
    assert "windows-guest-raw-result.json" in workflow
    assert "guest_edition:" in workflow
    assert "win11-enterprise" in workflow


def test_guest_installs_signed_runtime_before_native_replay():
    guest = (ROOT / "ci" / "windows_guest_probe.ps1").read_text()
    assert "Get-AuthenticodeSignature" in guest
    assert "vc_redist.x64.exe" in guest
    assert "runtime-install-start.txt" in guest
    assert "runtime-ready.txt" in guest
    assert guest.index("runtime-install-start.txt") < guest.index(
        "Invoke-Installer $localRuntime"
    )
    assert guest.index("runtime-ready.txt") < guest.index(
        "$result.nextest = Invoke-Replay"
    )


def test_guest_native_stderr_cannot_abort_nextest_replay():
    guest = (ROOT / "ci" / "windows_guest_probe.ps1").read_text()
    assert "# Windows PowerShell 5.1 promotes a native program's stderr" in guest
    assert "'Continue'" in guest
    assert guest.index("$ErrorActionPreference = 'Continue'") < guest.index(
        "& $nextest nextest run"
    )
    assert guest.index("$exitCode = $LASTEXITCODE") < guest.index(
        "$ErrorActionPreference = $savedErrorActionPreference"
    )


def test_guest_stages_mingit_before_full_selected_replay():
    guest = (ROOT / "ci" / "windows_guest_probe.ps1").read_text()
    assert "mingit.zip" in guest
    assert "Expand-Archive" in guest
    assert "cmd\\git.exe" in guest
    assert "tools-ready.txt" in guest
    assert guest.index("Expand-Archive -LiteralPath") < guest.index(
        "$result.nextest = Invoke-Replay"
    )
    assert "package(soldr-core)" in guest
    assert "not test(" not in guest


# --- soldr#3295 reopen: install/first-boot split, desktop hang, cache restore ---

FIRMWARE_LOG = "\n".join(
    [
        "2026-09-22T00:00:10Z Downloading Windows Server 2025...",
        "2026-09-22T00:01:10Z Extracting Windows Server 2025 image...",
        "2026-09-22T00:02:00Z Booting Windows using QEMU v10...",
        '2026-09-22T00:02:02Z BdsDxe: starting Boot0001 "UEFI QEMU DVD-ROM QM00013 "',
        '2026-09-22T00:07:02Z BdsDxe: starting Boot0004 "Windows Boot Manager" from HD',
        '2026-09-22T00:09:02Z BdsDxe: starting Boot0004 "Windows Boot Manager" from HD',
    ]
)


def _at(minute: int, second: int = 0) -> float:
    return datetime(2026, 9, 22, 0, minute, second, tzinfo=timezone.utc).timestamp()


def test_firmware_boot_markers_split_install_from_first_boot():
    shell_ready = _at(11, 2)
    timings = probe._phase_timings(FIRMWARE_LOG, _at(0), shell_ready)
    assert timings.iso_download_seconds == 60
    assert timings.media_prep_seconds == 50
    # Setup media boot (00:02:02) to the last firmware boot from disk (00:09:02).
    assert timings.install_seconds == 420
    # The last disk boot is the installed OS's first boot: OOBE, logon, OEM.
    assert timings.boot_seconds == 120
    assert timings.install_plus_first_boot_seconds == 542
    assert timings.firmware_disk_boots == 2


def test_disk_boot_after_the_shell_is_not_the_first_boot():
    log = FIRMWARE_LOG + (
        '\n2026-09-22T00:30:00Z BdsDxe: starting Boot0004 "Windows Boot Manager" from HD'
    )
    timings = probe._phase_timings(log, _at(0), _at(11, 2))
    assert timings.boot_seconds == 120
    assert timings.firmware_disk_boots == 2


def test_disk_boots_without_setup_media_are_not_called_an_install():
    log = "\n".join(
        [
            "2026-09-22T00:02:00Z Booting Windows using QEMU v10...",
            '2026-09-22T00:02:30Z BdsDxe: starting Boot0004 "Windows Boot Manager" from HD',
        ]
    )
    timings = probe._phase_timings(log, _at(0), _at(4))
    assert timings.install_seconds is None
    assert timings.boot_seconds is None


BUDGET_MANIFEST = probe.CacheManifest(
    total_max_bytes=9 * 1024**3,
    fail_total_bytes=10_200_547_328,
    family_max_bytes=(5 * 1024**3, 4 * 1024**3),
)


def test_cache_budget_rejects_an_image_with_no_unallocated_room():
    budget = probe.cache_budget(4_891_000_000, BUDGET_MANIFEST, 8_847_955_590)
    assert budget.unallocated_bytes == 0
    assert budget.fits_unallocated is False
    assert budget.projected_live_bytes == 8_847_955_590 + 4_891_000_000
    assert budget.projected_over_fail_total is True
    assert budget.share_of_total_budget == round(4_891_000_000 / (9 * 1024**3), 3)
    assert budget.cache_viable == "no"


def test_cache_budget_without_live_usage_does_not_invent_a_projection():
    budget = probe.cache_budget(None, BUDGET_MANIFEST, None)
    assert budget.projected_live_bytes is None
    assert budget.cache_viable == "not-measured"


def test_restored_boot_timings_come_from_the_restored_container_log():
    log = "\n".join(
        [
            "2026-09-22T01:00:05Z Booting Windows using QEMU v10...",
            '2026-09-22T01:00:07Z BdsDxe: starting Boot0004 "Windows Boot Manager" from HD',
        ]
    )
    started = datetime(2026, 9, 22, 1, 0, 0, tzinfo=timezone.utc).timestamp()
    timings = probe._restore_timings(log, started, started + 67)
    assert timings.reinstalled is False
    assert timings.restored_boot_seconds == 67
    assert timings.restored_firmware_to_shell_seconds == 60


def test_restored_disk_that_boots_setup_media_is_a_reinstall():
    log = '2026-09-22T01:00:07Z BdsDxe: starting Boot0001 "UEFI QEMU DVD-ROM QM00013 "'
    started = datetime(2026, 9, 22, 1, 0, 0, tzinfo=timezone.utc).timestamp()
    timings = probe._restore_timings(log, started, None)
    assert timings.reinstalled is True
    report = probe.make_restore_report(
        host=probe.HostInfo(kvm=True),
        timings=replace(
            timings, restore_download_seconds=60, restore_extract_seconds=30
        ),
        guest=None,
        cache=probe.cache_budget(1, BUDGET_MANIFEST, 0),
    )
    assert report.decision == "no-go"
    assert "reinstall" in report.reason.lower()


def _green_restore_timings(**overrides):
    return replace(
        probe.PhaseTimings(
            reinstalled=False,
            restore_download_seconds=90.0,
            restore_extract_seconds=40.0,
            restored_boot_seconds=70.0,
            restored_firmware_to_shell_seconds=60.0,
            restored_replay_seconds=30.0,
        ),
        **overrides,
    )


def test_restore_go_requires_replay_time_and_budget():
    guest = probe.GuestResult(
        nextest=probe.NextestResult(run=220, passed=220, failed=0, exit_code=0)
    )
    host = probe.HostInfo(kvm=True)
    fits = probe.CacheBudget(cache_viable="unproven-restore", fits_unallocated=True)
    report = probe.make_restore_report(
        host=host, timings=_green_restore_timings(), guest=guest, cache=fits
    )
    assert report.decision == "go"
    assert report.timings.restore_to_shell_seconds == 200
    no_room = probe.CacheBudget(cache_viable="no", fits_unallocated=False)
    report = probe.make_restore_report(
        host=host, timings=_green_restore_timings(), guest=guest, cache=no_room
    )
    assert report.decision == "no-go"
    assert any("budget" in reason for reason in report.reasons)
    slow = _green_restore_timings(restore_download_seconds=700.0)
    report = probe.make_restore_report(host=host, timings=slow, guest=guest, cache=fits)
    assert report.decision == "no-go"
    assert any("ten-minute" in reason for reason in report.reasons)


def test_export_refuses_a_disk_dockur_never_marked_installed(tmp_path):
    storage = tmp_path / "storage"
    storage.mkdir()
    (storage / "data.img").write_bytes(b"\0" * 16)
    try:
        probe._export_disk(storage, tmp_path / "disk.tar.zst")
    except OSError as exc:
        assert "windows.boot" in str(exc)
    else:
        raise AssertionError("an unfinished install must not be exported")


def test_export_is_a_sparse_tar_without_the_iso(tmp_path, monkeypatch):
    storage = tmp_path / "storage"
    storage.mkdir()
    (storage / "windows.boot").write_text("")
    commands = []

    def fake_popen(command, **_kwargs):
        commands.append(command)
        return nullcontext(
            SimpleNamespace(stdout=BytesIO(b"tar-stream"), wait=lambda: 0)
        )

    def fake_zstd(command, **kwargs):
        commands.append(command)
        kwargs["stdout"].write(kwargs["stdin"].read())
        return SimpleNamespace(returncode=0)

    monkeypatch.setattr(probe.subprocess, "Popen", fake_popen)
    monkeypatch.setattr(probe.subprocess, "run", fake_zstd)
    destination = tmp_path / "disk.tar.zst"
    measured = probe._export_disk(storage, destination)
    tar = commands[0]
    assert tar[:2] == ("sudo", "tar")
    assert "--sparse" in tar and "--exclude=*.iso" in tar
    assert commands[1][0] == "zstd"
    assert measured.compressed_bytes == len(b"tar-stream")
    assert measured.export_seconds >= 0


def test_delete_artifact_removes_only_the_named_run_artifact(monkeypatch):
    calls = []

    def fake_run(*command, **_kwargs):
        calls.append(command)
        if "-X" not in command:
            return SimpleNamespace(stdout="111\n222\n", stderr="", returncode=0)
        return SimpleNamespace(stdout="", stderr="", returncode=0)

    monkeypatch.setattr(probe, "_run", fake_run)
    assert probe.delete_run_artifact("o/r", "42", "windows-guest-disk-image") == 2
    assert "repos/o/r/actions/runs/42/artifacts" in calls[0]
    assert calls[1][-1] == "repos/o/r/actions/artifacts/111"
    assert calls[2][-1] == "repos/o/r/actions/artifacts/222"


def test_workflow_measures_restore_in_a_second_job_and_deletes_the_disk():
    workflow = (ROOT / ".github/workflows/windows-guest-probe.yml").read_text()
    assert "measure_restore:" in workflow
    assert "--mode restore" in workflow
    assert "--mode delete-artifact" in workflow
    assert "windows-guest-disk-image" in workflow
    assert "retention-days: 1" in workflow
    assert "actions/cache" not in workflow
    # /mnt is root-owned: an unprivileged rm of the export dir fails the job
    # and skips the restore (run 36241951118).
    assert 'sudo rm -rf -- "$EXPORT_DIR"' in workflow


def test_guest_never_shell_executes_a_share_hosted_installer():
    guest = (ROOT / "ci" / "windows_guest_probe.ps1").read_text()
    assert "Start-Process" not in guest
    assert "UseShellExecute = $false" in guest
    assert guest.index("Copy-Item -LiteralPath $runtime") < guest.index(
        "Invoke-Installer $localRuntime"
    )
    assert "WaitForExit($TimeoutSeconds * 1000)" in guest


def test_guest_registers_a_warm_logon_probe_for_the_restored_disk():
    guest = (ROOT / "ci" / "windows_guest_probe.ps1").read_text()
    assert "Register-ScheduledTask" in guest
    assert "-Mode warm" in guest
    assert "$prefix = if ($Mode -eq 'warm') { 'warm-' }" in guest
    # The warm task is registered only after the cold replay produced a result.
    assert guest.index("$result.nextest = Invoke-Replay") < guest.index(
        "Register-ScheduledTask"
    )


def _restore_args(tmp_path, **overrides):
    values = {
        "mode": "restore",
        "repo": ROOT,
        "artifact": tmp_path,
        "scratch": tmp_path / "scratch",
        "output": tmp_path / "restore.json",
        "run_id": "7-restore",
        "timeout_seconds": 1,
        "disk_run_id": "7",
        "repository": "o/r",
        "budget_manifest": None,
    }
    values.update(overrides)
    return Namespace(**values)


def _restore_run_stub(shared, calls):
    """A `_run` stub that plays back one warm-replay restore, recording calls.

    Each `docker inspect` advances the replay by one step, so the caller sees
    the shell-ready, tools-ready, then result checkpoints in order; anything
    else answers with an inert success.
    """
    state = {"inspections": 0}

    def fake_run(*command, **_kwargs):
        calls.append(command)
        if command[:3] == ("gh", "run", "download"):
            target = Path(command[command.index("-D") + 1])
            (target / probe.DISK_EXPORT_NAME).write_bytes(b"x" * 1234)
        if command[:2] == ("gh", "api"):
            return SimpleNamespace(stdout="8000000000\n", stderr="", returncode=0)
        if command[:2] == ("docker", "logs"):
            return SimpleNamespace(
                stdout='1970-01-01T00:16:50Z BdsDxe: starting Boot0004 "Windows Boot '
                'Manager" from HD\n',
                stderr="",
                returncode=0,
            )
        if command[:2] == ("docker", "inspect") and len(command) == 3:
            return SimpleNamespace(stdout="", stderr="No such object", returncode=1)
        if command[:2] == ("docker", "inspect"):
            state["inspections"] += 1
            _advance_warm_replay(shared, state["inspections"])
            return SimpleNamespace(stdout="true", stderr="", returncode=0)
        return SimpleNamespace(stdout="", stderr="", returncode=0)

    return fake_run


def _advance_warm_replay(shared, inspection):
    """Write the checkpoint the n-th `docker inspect` observes."""
    if inspection == 1:
        (shared / "warm-guest-shell-ready.txt").write_text("ready")
    elif inspection == 2:
        (shared / "warm-tools-ready.txt").write_text("ready")
    elif inspection == 3:
        (shared / "warm-guest-result.json").write_text(
            json.dumps(
                {
                    "mode": "warm",
                    "nextest": {
                        "run": 220,
                        "passed": 220,
                        "failed": 0,
                        "exit_code": 0,
                    },
                }
            )
        )


def test_restore_mode_times_download_extract_boot_and_warm_replay(
    tmp_path, monkeypatch
):
    monkeypatch.setattr(probe, "_kvm_usable", lambda: True)
    monkeypatch.setattr(
        probe.shutil, "disk_usage", lambda _: SimpleNamespace(free=60 * 1024**3)
    )
    monkeypatch.setattr(probe.time, "sleep", lambda _: None)
    monkeypatch.setattr(probe.time, "time", lambda: 1000.0)
    marker_times = iter([1070.0, 1080.0, 1110.0])
    monkeypatch.setattr(probe, "_host_created_at", lambda *_: next(marker_times))
    monkeypatch.setattr(probe, "_extract_disk", lambda archive, storage: 40.0)
    staged = []

    def stage(_repo, _artifact, shared_dir, oem_dir, *, fetch_tools=True):
        staged.append(fetch_tools)
        shared_dir.mkdir()
        oem_dir.mkdir()

    monkeypatch.setattr(probe, "_stage_payload", stage)
    shared = tmp_path / "scratch" / "shared"
    calls = []
    fake_run = _restore_run_stub(shared, calls)
    monkeypatch.setattr(probe, "_run", fake_run)
    args = _restore_args(tmp_path)
    assert probe.run_probe(args) == 0
    report = json.loads(args.output.read_text())
    assert staged == [False]
    assert report["mode"] == "restore"
    assert report["reinstalled"] is False
    assert report["timings"]["restore_extract_seconds"] == 40.0
    assert report["timings"]["restored_boot_seconds"] == 70.0
    assert report["timings"]["restored_firmware_to_shell_seconds"] == 60.0
    assert report["timings"]["restored_replay_seconds"] == 30.0
    assert report["nextest"]["passed"] == 220
    assert report["cache"]["compressed_bytes"] == 1234
    assert report["cache"]["live_usage_bytes"] == 8_000_000_000
    # The real manifest's families already fill the whole budget.
    assert report["cache"]["cache_viable"] == "no"
    assert report["decision"] == "no-go"
    download = next(c for c in calls if c[:3] == ("gh", "run", "download"))
    assert download[3] == "7" and "windows-guest-disk-image" in download
    assert (tmp_path / "windows-guest-restore-raw-result.json").is_file()
    assert (tmp_path / "windows-guest-restore-container.log").is_file()
    assert not (tmp_path / "scratch").exists()


def test_cold_mode_exports_only_a_disk_with_the_warm_task(tmp_path, monkeypatch):
    monkeypatch.setattr(probe, "_kvm_usable", lambda: True)
    monkeypatch.setattr(
        probe.shutil, "disk_usage", lambda _: SimpleNamespace(free=60 * 1024**3)
    )
    monkeypatch.setattr(probe.time, "sleep", lambda _: None)
    monkeypatch.setattr(probe, "_host_created_at", lambda *_: None)
    monkeypatch.setattr(
        probe,
        "_stage_payload",
        lambda _r, _a, shared_dir, oem_dir: (shared_dir.mkdir(), oem_dir.mkdir()),
    )
    monkeypatch.setattr(
        probe,
        "_disk_measure",
        lambda _s, compress=True: probe.DiskMeasure(
            image_bytes=1, emit=("image_bytes",)
        ),
    )
    exports = []

    def fake_export(storage, destination):
        exports.append(destination)
        return probe.DiskMeasure(compressed_bytes=99, export_seconds=3.0)

    monkeypatch.setattr(probe, "_export_disk", fake_export)
    github_output = tmp_path / "github-output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(github_output))
    shared = tmp_path / "scratch" / "shared"
    warm_task = True

    def fake_run(*command, **_kwargs):
        if command[:2] == ("docker", "inspect") and len(command) == 3:
            return SimpleNamespace(stdout="", stderr="No such object", returncode=1)
        if command[:2] == ("docker", "inspect"):
            (shared / "guest-result.json").write_text(
                json.dumps({"capabilities": {"warm_task": warm_task}})
            )
            return SimpleNamespace(stdout="true", stderr="", returncode=0)
        return SimpleNamespace(stdout="", stderr="", returncode=0)

    monkeypatch.setattr(probe, "_run", fake_run)

    def cold_args(name):
        return Namespace(
            mode="cold",
            repo=ROOT,
            artifact=tmp_path,
            scratch=tmp_path / "scratch",
            output=tmp_path / f"{name}.json",
            run_id="test",
            timeout_seconds=1,
            export_disk="true",
            export_dir=str(tmp_path / "export"),
            repository=None,
        )

    assert probe.run_probe(cold_args("with-task")) == 0
    report = json.loads((tmp_path / "with-task.json").read_text())
    assert exports == [tmp_path / "export" / probe.DISK_EXPORT_NAME]
    assert report["disk"]["compressed_bytes"] == 99
    assert report["disk"]["export_seconds"] == 3.0
    assert github_output.read_text() == "disk_exported=true\n"

    warm_task = False
    exports.clear()
    github_output.unlink()
    assert probe.run_probe(cold_args("without-task")) == 0
    assert not exports
    assert not github_output.exists()
