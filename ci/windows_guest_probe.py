"""Dispatch-only dockur/Windows feasibility probe for soldr#3295.

The Linux cross-build is owned by _ci-cross-build-linux.yml. This helper only
stages its verified archive and native nextest, boots an ephemeral guest, and
turns measurements (including an honest no-go) into a durable summary.

Modes:

* ``cold`` installs a fresh evaluation guest, replays the archive, and (with
  ``--export-disk true``) shuts it down cleanly and writes the installed disk
  as a sparse ``tar.zst`` for the restore job.
* ``restore`` downloads that same-run disk artifact on a fresh runner,
  extracts it, boots it without setup media, and replays the archive again.
  This measures what a cached installed image would cost, without ever
  writing the image to the repository's Actions cache.
* ``delete-artifact`` removes the run's disk artifact once restore finished.
"""

# No ``from __future__ import annotations``: ci/winvm_lane.py loads this file by
# path without registering it in sys.modules, which dataclasses cannot resolve
# string annotations against.
import argparse
import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tarfile
import time
import urllib.request
import uuid
from dataclasses import dataclass, field, fields, is_dataclass, replace
from datetime import datetime
from pathlib import Path

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

# Upstream dockur/windows v6.03, commit f6fcb46958fb9635df49e2af09f7c800b65a89e3.
# Pin the published multi-arch image digest, not the moving tag or stale fork.
IMAGE = "ghcr.io/dockur/windows@sha256:743847e75b776790c059f33ac6654f84727ba36a6d458a61e37cb2b2f043d168"
GUEST_EDITIONS = {
    "server-core": ("2025", "core", "Windows Server 2025 Core evaluation"),
    "win11-enterprise": ("11e", None, "Windows 11 Enterprise evaluation"),
}
# Microsoft's documented permalink, pinned to the exact signed package read
# for this one-off probe. A changed permalink payload fails closed for review.
VC_REDIST_URL = "https://aka.ms/vc14/vc_redist.x64.exe"
VC_REDIST_SHA256 = "843068991daaa1f73ad9f6239bce4d0f6a07a51f18c37ea2a867e9beca71295c"
MAX_VC_REDIST_BYTES = 64 * 1024**2
MINGIT_URL = "https://github.com/git-for-windows/git/releases/download/v2.55.0.windows.5/MinGit-2.55.0.5-64-bit.zip"
MINGIT_SHA256 = "56d7b226b7693196cfc71fef26568f536c4a021ab6c37ff2db4287bed908e96e"
MAX_MINGIT_BYTES = 64 * 1024**2
# A cold or restored guest must reach a usable shell inside this budget for
# its path to be a go (soldr#3295: "if the cold install takes under ~10 min,
# don't cache").
SHELL_BUDGET_SECONDS = 600
DISK_ARTIFACT = "windows-guest-disk-image"
DISK_EXPORT_NAME = "windows-guest-disk.tar.zst"
# dockur writes windows.boot when a guest shuts down after booting from its
# installed disk; without it a restore would silently reinstall from media.
DOCKUR_INSTALLED_MARKER = "windows.boot"
COLD_MARKERS = (
    "guest-shell-ready.txt",
    "runtime-install-start.txt",
    "runtime-ready.txt",
    "git-install-start.txt",
    "tools-ready.txt",
)
WARM_MARKERS = ("warm-guest-shell-ready.txt", "warm-tools-ready.txt")
CAPABILITY_NAMES = (
    "powershell",
    "job_objects",
    "conpty",
    "webview2",
    "gpu_rendering",
    "admin",
    "vcruntime140",
    "vcruntime140_after",
    "git",
    "warm_task",
)


NOT_MEASURED = "not-measured"
COLD_TIMING_KEYS = (
    "iso_download_seconds",
    "media_prep_seconds",
    "install_seconds",
    "boot_seconds",
    "install_plus_first_boot_seconds",
    "firmware_disk_boots",
    "runtime_prep_seconds",
    "runtime_install_seconds",
    "git_prep_seconds",
    "replay_seconds",
    "usable_shell_seconds",
)
RESTORE_TIMING_KEYS = (
    "restore_download_seconds",
    "restore_extract_seconds",
    "restored_boot_seconds",
    "restored_firmware_to_shell_seconds",
    "restored_replay_seconds",
    "restore_to_shell_seconds",
)
CACHE_BUDGET_KEYS = (
    "compressed_bytes",
    "budget_total_max_bytes",
    "budget_fail_total_bytes",
    "unallocated_bytes",
    "live_usage_bytes",
    "fits_unallocated",
    "share_of_total_budget",
    "projected_live_bytes",
    "projected_over_fail_total",
    "cache_viable",
)
NEXTEST_KEYS = ("status", "exit_code", "run", "passed", "failed")
REPORT_COLD_KEYS = (
    "schema_version",
    "decision",
    "reason",
    "host",
    "timings",
    "disk",
    "cache",
    "nextest",
    "guest_error",
    "capabilities",
    "edition",
    "dockur_version",
    "image",
)
REPORT_RESTORE_KEYS = (
    "schema_version",
    "mode",
    "decision",
    "reason",
    "reasons",
    "host",
    "timings",
    "reinstalled",
    "cache",
    "nextest",
    "guest_error",
    "capabilities",
    "edition",
    "dockur_version",
    "image",
)


@dataclass(frozen=True, kw_only=True)
class Emitting:
    """A record that serializes exactly the keys named in ``emit``.

    Absent keys stay absent in the JSON (distinct from a key measured as null),
    which keeps the emitted documents identical to the former dict payloads.
    """

    emit: tuple[str, ...] = ()

    def json_keys(self) -> tuple[str, ...]:
        """The field names this record serializes, in output order."""
        return self.emit


@dataclass(frozen=True, kw_only=True)
class GpuRendering:
    """The guest's GPU probe: acceleration verdict plus adapter names."""

    accelerated: bool | None = None
    adapters: tuple[str, ...] = ()

    def __str__(self) -> str:
        return str({"accelerated": self.accelerated, "adapters": list(self.adapters)})


CapabilityValue = str | bool | int | float | GpuRendering | None


@dataclass(frozen=True, kw_only=True)
# pylint: disable-next=too-many-instance-attributes
class GuestCapabilities:
    """Each capability the guest probe reports; unmeasured stays explicit."""

    powershell: CapabilityValue = NOT_MEASURED
    job_objects: CapabilityValue = NOT_MEASURED
    conpty: CapabilityValue = NOT_MEASURED
    webview2: CapabilityValue = NOT_MEASURED
    gpu_rendering: CapabilityValue = NOT_MEASURED
    admin: CapabilityValue = NOT_MEASURED
    vcruntime140: CapabilityValue = NOT_MEASURED
    vcruntime140_after: CapabilityValue = NOT_MEASURED
    git: CapabilityValue = NOT_MEASURED
    warm_task: CapabilityValue = NOT_MEASURED


@dataclass(frozen=True, kw_only=True)
class NextestResult(Emitting):
    """The native nextest replay counts the guest reports."""

    status: str | None = None
    exit_code: int | None = None
    run: int | None = None
    passed: int | None = None
    failed: int | None = None

    def passed_nonempty_set(self) -> bool:
        """True only for a clean exit that ran at least one test."""
        return self.exit_code == 0 and (self.run or 0) > 0 and (self.failed or 0) == 0


NEXTEST_NOT_RUN = NextestResult(status="not-run", emit=("status",))


@dataclass(frozen=True, kw_only=True)
class GuestResult:
    """What the guest probe script wrote to its result file."""

    error: str = ""
    nextest: NextestResult = NEXTEST_NOT_RUN
    capabilities: GuestCapabilities = GuestCapabilities()


@dataclass(frozen=True, kw_only=True)
# pylint: disable-next=too-many-instance-attributes
class HostInfo:
    """Runner facts and any infrastructure errors met along the way."""

    kvm: bool | None = None
    free_bytes: int | None = None
    probe_error: str | None = None
    preflight: str | None = None
    export_error: str | None = None
    diagnostic_error: str | None = None
    cleanup_error: str | None = None

    def json_keys(self) -> tuple[str, ...]:
        """kvm and free_bytes always; error fields only when set."""
        extras = (
            "probe_error",
            "preflight",
            "export_error",
            "diagnostic_error",
            "cleanup_error",
        )
        return (
            "kvm",
            "free_bytes",
            *(name for name in extras if getattr(self, name) is not None),
        )


@dataclass(frozen=True, kw_only=True)
# pylint: disable-next=too-many-instance-attributes
class PhaseTimings(Emitting):
    """Measured phase durations; None means unknown, never zero."""

    iso_download_seconds: float | None = None
    media_prep_seconds: float | None = None
    install_seconds: float | None = None
    boot_seconds: float | None = None
    install_plus_first_boot_seconds: float | None = None
    firmware_disk_boots: int | None = None
    observed_total_seconds: float | None = None
    runtime_prep_seconds: float | None = None
    runtime_install_seconds: float | None = None
    git_prep_seconds: float | None = None
    replay_seconds: float | None = None
    usable_shell_seconds: float | None = None
    restore_download_seconds: float | None = None
    restore_extract_seconds: float | None = None
    restored_boot_seconds: float | None = None
    restored_firmware_to_shell_seconds: float | None = None
    restored_replay_seconds: float | None = None
    restore_to_shell_seconds: float | None = None
    reinstalled: bool | None = None


@dataclass(frozen=True, kw_only=True)
class DiskMeasure(Emitting):
    """Installed-disk size measurements; ``emit`` names the measured keys."""

    image_bytes: int | None = None
    allocated_bytes: int | None = None
    compressed_bytes: int | None = None
    restore_seconds: float | None = None
    cache_viable: str | None = None
    export_seconds: float | None = None


@dataclass(frozen=True, kw_only=True)
# pylint: disable-next=too-many-instance-attributes
class CacheBudget(Emitting):
    """An installed-disk entry placed against ci/cache-ownership.json."""

    emit: tuple[str, ...] = CACHE_BUDGET_KEYS
    compressed_bytes: int | None = None
    budget_total_max_bytes: int = 0
    budget_fail_total_bytes: int = 0
    unallocated_bytes: int = 0
    live_usage_bytes: int | None = None
    fits_unallocated: bool | None = None
    share_of_total_budget: float | None = None
    projected_live_bytes: int | None = None
    projected_over_fail_total: bool | None = None
    cache_viable: str = NOT_MEASURED


CACHE_NOT_MEASURED = CacheBudget(emit=("cache_viable",))


@dataclass(frozen=True, kw_only=True)
class CacheManifest:
    """The cache-ownership budget numbers this probe consults."""

    total_max_bytes: int = 0
    fail_total_bytes: int = 0
    family_max_bytes: tuple[int, ...] = ()


@dataclass(frozen=True, kw_only=True)
# pylint: disable-next=too-many-instance-attributes
class ProbeReport(Emitting):
    """The durable go/no-go document; ``emit`` selects the cold or restore shape."""

    decision: str
    reason: str
    host: HostInfo
    timings: PhaseTimings
    cache: CacheBudget
    nextest: NextestResult
    capabilities: GuestCapabilities
    edition: str
    dockur_version: str
    disk: DiskMeasure | None = None
    guest_error: str | None = None
    mode: str | None = None
    reasons: tuple[str, ...] = ()
    reinstalled: bool | None = None
    schema_version: int = 1
    image: str = IMAGE


@dataclass(frozen=True, kw_only=True)
class MarkerTime:
    """Host-observed creation time of one guest marker file."""

    name: str
    at: float | None


@dataclass(frozen=True, kw_only=True)
class GuestSession:
    """Everything a booted-and-stopped dockur session observed."""

    started: float
    markers: tuple[MarkerTime, ...]
    guest: GuestResult | None
    result_seen: float | None
    log: str

    def marker(self, name: str) -> float | None:
        """Host time of the named marker, or None when it never appeared."""
        return next((mark.at for mark in self.markers if mark.name == name), None)


@dataclass(frozen=True, kw_only=True)
class LogEvent:
    """One `docker logs -t` line: host timestamp and lowercased message."""

    when: float
    message: str


@dataclass(frozen=True, kw_only=True)
class SummaryRow:
    """One key/value row of the Markdown summary table."""

    key: str
    value: object


@dataclass(frozen=True, kw_only=True)
# pylint: disable-next=too-many-instance-attributes
class ProbeContext:
    """Immutable per-run paths and identifiers."""

    repo: Path
    artifact: Path
    scratch: Path
    shared: Path
    oem: Path
    storage: Path
    repository: str | None
    guest_edition: str
    name: str
    log_path: Path


@dataclass(slots=True)
class RunProgress:
    """What a run has measured so far, kept even if a later step raises."""

    host: HostInfo = field(default_factory=HostInfo)
    timings: PhaseTimings = field(default_factory=PhaseTimings)
    guest: GuestResult | None = None
    disk: DiskMeasure = field(default_factory=DiskMeasure)
    compressed_bytes: int | None = None
    attempted_container: bool = False


def _json_default(obj: object) -> object:
    """``json.dumps`` hook turning the frozen records into JSON objects."""
    if isinstance(obj, GpuRendering):
        return {"accelerated": obj.accelerated, "adapters": list(obj.adapters)}
    if not is_dataclass(obj) or isinstance(obj, type):
        raise TypeError(f"not JSON serializable: {type(obj).__name__}")
    if isinstance(obj, (Emitting, HostInfo)):
        keys = obj.json_keys()
    else:
        keys = tuple(item.name for item in fields(obj))
    return {key: getattr(obj, key) for key in keys}


def _summary_rows(record: Emitting | HostInfo) -> list[SummaryRow]:
    """The record's emitted keys as table rows."""
    return [
        SummaryRow(key=key, value=getattr(record, key)) for key in record.json_keys()
    ]


def _capability_value(raw: object) -> CapabilityValue:
    if isinstance(raw, dict) and "accelerated" in raw:
        adapters = raw.get("adapters")
        if adapters is None:
            adapters = ()
        elif isinstance(adapters, (str, bytes)):
            adapters = (adapters,)
        return GpuRendering(
            accelerated=raw.get("accelerated"),
            adapters=tuple(str(adapter) for adapter in adapters),
        )
    if raw is None or isinstance(raw, (str, bool, int, float)):
        return raw
    return str(raw)


def _nextest_from_json(raw: object) -> NextestResult:
    if not isinstance(raw, dict) or not raw:
        return NEXTEST_NOT_RUN
    return NextestResult(
        emit=tuple(key for key in NEXTEST_KEYS if key in raw),
        status=raw.get("status"),
        exit_code=raw.get("exit_code"),
        run=raw.get("run"),
        passed=raw.get("passed"),
        failed=raw.get("failed"),
    )


def _guest_from_json(data: object) -> GuestResult | None:
    """Parse the guest's result document; empty or non-object means no result."""
    if not isinstance(data, dict) or not data:
        return None
    raw_caps = data.get("capabilities")
    caps = raw_caps if isinstance(raw_caps, dict) else {}
    return GuestResult(
        error=str(data.get("error") or "")[:500],
        nextest=_nextest_from_json(data.get("nextest")),
        capabilities=GuestCapabilities(
            **{
                name: _capability_value(caps[name])
                for name in CAPABILITY_NAMES
                if name in caps
            }
        ),
    )


def _manifest_from_json(data: object) -> CacheManifest:
    """The budget numbers from an ownership manifest, zeros when absent."""
    budget = data.get("budget") if isinstance(data, dict) else None
    if not isinstance(budget, dict):
        return CacheManifest()
    families = budget.get("families")
    if not isinstance(families, dict):
        families = {}
    return CacheManifest(
        total_max_bytes=int(budget.get("total_max_bytes") or 0),
        fail_total_bytes=int(budget.get("fail_total_bytes") or 0),
        family_max_bytes=tuple(
            int(family.get("max_bytes") or 0)
            for family in families.values()
            if isinstance(family, dict)
        ),
    )


def make_report(  # noqa: C901
    *,
    host: HostInfo,
    timings: PhaseTimings,
    guest: GuestResult | None,
    disk: DiskMeasure,
    guest_edition: str = "server-core",
    cache: CacheBudget | None = None,
) -> ProbeReport:
    """Never turn missing evidence into a success or a measured zero."""
    usable_shell = timings.observed_total_seconds
    phases = (
        timings.iso_download_seconds,
        timings.install_seconds,
        timings.boot_seconds,
    )
    if usable_shell is None and all(
        isinstance(value, (int, float)) for value in phases
    ):
        total = 0.0
        for value in phases:
            if isinstance(value, (int, float)):
                total += value
        usable_shell = total
    measured = replace(
        timings, usable_shell_seconds=usable_shell, emit=COLD_TIMING_KEYS
    )
    nextest = guest.nextest if guest else NEXTEST_NOT_RUN
    guest_error = guest.error if guest else ""
    capabilities = guest.capabilities if guest else GuestCapabilities()
    if host.probe_error:
        reason = f"Probe infrastructure error: {host.probe_error}"
    elif not host.kvm:
        reason = "KVM is unavailable on this runner"
    elif not guest:
        reason = "Windows did not reach the probe script within the boot budget"
    elif guest_error:
        reason = f"Windows guest probe failed: {guest_error}"
    elif not nextest.passed_nonempty_set():
        reason = "Native nextest replay did not pass a nonempty test set"
    elif usable_shell is None:
        reason = "Install and first-boot timing could not be measured"
    elif usable_shell > SHELL_BUDGET_SECONDS:
        reason = "Cold boot exceeded the ten-minute nightly budget"
    else:
        reason = "Cold boot and native replay fit the probe budget"
    return ProbeReport(
        emit=REPORT_COLD_KEYS,
        decision=(
            "go"
            if reason == "Cold boot and native replay fit the probe budget"
            else "no-go"
        ),
        reason=reason,
        host=host,
        timings=measured,
        disk=disk,
        cache=cache or CACHE_NOT_MEASURED,
        nextest=nextest,
        guest_error=guest_error or None,
        capabilities=capabilities,
        edition=GUEST_EDITIONS[guest_edition][2],
        dockur_version=GUEST_EDITIONS[guest_edition][0],
    )


def make_restore_report(
    *,
    host: HostInfo,
    timings: PhaseTimings,
    guest: GuestResult | None,
    cache: CacheBudget,
    guest_edition: str = "server-core",
) -> ProbeReport:
    """Judge the cached-image path: restore, boot, replay, and budget together."""
    parts = (
        timings.restore_download_seconds,
        timings.restore_extract_seconds,
        timings.restored_boot_seconds,
    )
    total = (
        float(sum(part for part in parts if isinstance(part, (int, float))))
        if all(isinstance(part, (int, float)) for part in parts)
        else None
    )
    measured = replace(
        timings, restore_to_shell_seconds=total, emit=RESTORE_TIMING_KEYS
    )
    nextest = guest.nextest if guest else NEXTEST_NOT_RUN
    guest_error = guest.error if guest else ""
    reasons: list[str] = []
    if host.probe_error:
        reasons.append(f"Probe infrastructure error: {host.probe_error}")
    elif not host.kvm:
        reasons.append("KVM is unavailable on this runner")
    if timings.reinstalled:
        reasons.append("Restored disk reinstalled Windows from setup media")
    if not reasons and not guest:
        reasons.append("Restored image did not reach the warm probe within budget")
    if guest_error:
        reasons.append(f"Warm guest probe failed: {guest_error}")
    if guest and not nextest.passed_nonempty_set():
        reasons.append("Native nextest replay on the restored image did not pass")
    if cache.cache_viable == "no":
        reasons.append("Compressed image does not fit the repository cache budget")
    if total is None and not reasons:
        reasons.append("Restore-to-shell time could not be measured")
    elif total is not None and total > SHELL_BUDGET_SECONDS:
        reasons.append("Restore plus boot exceeded the ten-minute nightly budget")
    return ProbeReport(
        emit=REPORT_RESTORE_KEYS,
        mode="restore",
        decision="no-go" if reasons else "go",
        reason=reasons[0] if reasons else "Restored image boot and replay fit",
        reasons=tuple(reasons),
        host=host,
        timings=measured,
        reinstalled=timings.reinstalled,
        cache=cache,
        nextest=nextest,
        guest_error=guest_error or None,
        capabilities=guest.capabilities if guest else GuestCapabilities(),
        edition=GUEST_EDITIONS[guest_edition][2],
        dockur_version=GUEST_EDITIONS[guest_edition][0],
    )


def cache_budget(
    compressed_bytes: int | None,
    manifest: CacheManifest,
    live_usage_bytes: int | None,
) -> CacheBudget:
    """Place an installed-disk entry against ci/cache-ownership.json's budget.

    The families already sum to ``total_max_bytes``, so a new entry fits only
    in unallocated room; anything larger must evict or defund existing
    families. The live projection uses GitHub's reported usage when available.
    """
    total = manifest.total_max_bytes
    fail_total = manifest.fail_total_bytes
    unallocated = max(0, total - sum(manifest.family_max_bytes))
    result = CacheBudget(
        compressed_bytes=compressed_bytes,
        budget_total_max_bytes=total,
        budget_fail_total_bytes=fail_total,
        unallocated_bytes=unallocated,
        live_usage_bytes=live_usage_bytes,
    )
    if compressed_bytes is None:
        return result
    fits = compressed_bytes <= unallocated
    projected = None
    if live_usage_bytes is not None:
        projected = live_usage_bytes + compressed_bytes
    return replace(
        result,
        fits_unallocated=fits,
        share_of_total_budget=round(compressed_bytes / total, 3) if total else None,
        projected_live_bytes=projected,
        projected_over_fail_total=(
            projected > fail_total if projected is not None else None
        ),
        cache_viable="unproven-restore" if fits else "no",
    )


def _live_cache_usage(repository: str | None) -> int | None:
    """GitHub's live Actions-cache usage, or None when the API is unavailable."""
    if not repository:
        return None
    try:
        usage = _run(
            "gh",
            "api",
            f"repos/{repository}/actions/cache/usage",
            "--jq",
            ".active_caches_size_in_bytes",
            check=False,
            capture=True,
        )
    except OSError:
        return None
    try:
        return int(usage.stdout.strip()) if usage.returncode == 0 else None
    except ValueError:
        return None


def delete_run_artifact(repository: str, run_id: str, name: str) -> int:
    """Delete this run's named artifact(s); return how many were removed."""
    listing = _run(
        "gh",
        "api",
        f"repos/{repository}/actions/runs/{run_id}/artifacts",
        "--jq",
        f'.artifacts[] | select(.name == "{name}") | .id',
        capture=True,
    )
    removed = 0
    for artifact_id in listing.stdout.split():
        _run(
            "gh",
            "api",
            "-X",
            "DELETE",
            f"repos/{repository}/actions/artifacts/{artifact_id}",
        )
        removed += 1
    return removed


def _read_guest_result(path: Path) -> GuestResult | None:
    if not path.is_file():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8-sig"))
    except (OSError, ValueError):
        return None
    return _guest_from_json(data)


def _run(
    *args: str, check: bool = True, capture: bool = False
) -> subprocess.CompletedProcess[str]:
    return run_captured(args, check=check, text=True, capture_output=capture)


def _fetch_pinned(
    *, url: str, destination: Path, expected_sha256: str, max_bytes: int
) -> None:
    """Stream a pinned one-off tool payload, rejecting drift and oversize files."""
    digest = hashlib.sha256()
    size = 0
    with urllib.request.urlopen(url, timeout=120) as response:
        with destination.open("wb") as output:
            while chunk := response.read(1024 * 1024):
                size += len(chunk)
                if size > max_bytes:
                    raise OSError(f"{destination.name} exceeds size limit")
                digest.update(chunk)
                output.write(chunk)
    if digest.hexdigest() != expected_sha256:
        raise OSError(f"{destination.name} sha256 mismatch: {digest.hexdigest()}")


def _fetch_vc_redist(shared: Path, *, expected_sha256: str = VC_REDIST_SHA256) -> None:
    """Stage one exact Microsoft runtime installer; never trust a moving URL."""
    _fetch_pinned(
        url=VC_REDIST_URL,
        destination=shared / "vc_redist.x64.exe",
        expected_sha256=expected_sha256,
        max_bytes=MAX_VC_REDIST_BYTES,
    )


def _fetch_mingit(shared: Path, *, expected_sha256: str = MINGIT_SHA256) -> None:
    """Stage official MinGit for the four git fixture tests in Server Core."""
    _fetch_pinned(
        url=MINGIT_URL,
        destination=shared / "mingit.zip",
        expected_sha256=expected_sha256,
        max_bytes=MAX_MINGIT_BYTES,
    )


def _stage_payload(
    repo: Path, artifact: Path, shared: Path, oem: Path, *, fetch_tools: bool = True
) -> Path:
    archive = artifact / "windows-guest-probe-msvc-tests.tar.zst"
    nextest = artifact / "package" / "tools" / "cargo-nextest.exe"
    if not archive.is_file() or not nextest.is_file():
        raise FileNotFoundError(
            f"cross-build bundle missing archive or native nextest under {artifact}"
        )
    shared.mkdir(parents=True)
    oem.mkdir(parents=True)
    shutil.copy2(archive, shared / "tests.tar.zst")
    shutil.copy2(nextest, shared / "cargo-nextest.exe")
    shutil.copy2(repo / "ci" / "windows_guest_probe.ps1", oem / "probe.ps1")
    shutil.copy2(repo / "ci" / "windows_guest_probe.bat", oem / "install.bat")
    if fetch_tools:
        # A restored image already has both installed; the warm probe
        # verifies that instead of reinstalling them.
        _fetch_vc_redist(shared)
        _fetch_mingit(shared)
    workspace = shared / "workspace"
    workspace.mkdir()
    # Nextest requires a real workspace manifest at --workspace-remap. Use the
    # same commit as the cross-build, with no target/, .git/, or local leftovers.
    with subprocess.Popen(
        ("git", "-C", str(repo), "archive", "--format=tar", "HEAD"),
        stdout=subprocess.PIPE,
    ) as process:
        assert process.stdout is not None
        with tarfile.open(fileobj=process.stdout, mode="r|") as source:
            source.extractall(workspace, filter="data")
        if process.wait() != 0:
            raise OSError("git archive failed")
    return archive


def _disk_measure(storage: Path, *, compress: bool = True) -> DiskMeasure:
    images = [
        path
        for path in storage.rglob("*")
        if path.is_file() and path.suffix in (".img", ".qcow2", ".raw")
    ]
    if not images:
        return DiskMeasure(
            emit=("image_bytes", "compressed_bytes", "restore_seconds", "cache_viable"),
            cache_viable=NOT_MEASURED,
        )
    image = max(images, key=lambda path: path.stat().st_size)
    if not compress:
        # The export already measured the compressed size of this storage.
        return DiskMeasure(
            emit=("image_bytes", "allocated_bytes"),
            image_bytes=image.stat().st_size,
            allocated_bytes=image.stat().st_blocks * 512,
        )
    # Measure a compressed stream, without creating a multi-GB cache artifact.
    with subprocess.Popen(
        ("zstd", "-q", "-T0", "-3", "-c", str(image)), stdout=subprocess.PIPE
    ) as zstd:
        assert zstd.stdout is not None
        compressed_count = 0
        while chunk := zstd.stdout.read(1024 * 1024):
            compressed_count += len(chunk)
        compressed = compressed_count if zstd.wait() == 0 else None
    return DiskMeasure(
        emit=(
            "image_bytes",
            "allocated_bytes",
            "compressed_bytes",
            "restore_seconds",
            "cache_viable",
        ),
        image_bytes=image.stat().st_size,
        allocated_bytes=image.stat().st_blocks * 512,
        compressed_bytes=compressed,
        cache_viable="unproven" if compressed is not None else NOT_MEASURED,
    )


def _log_events(log: str) -> list[LogEvent]:
    """Host timestamp and lowercased message for each `docker logs -t` line."""
    events = []
    for line in log.splitlines():
        try:
            timestamp, message = line.split(" ", 1)
            when = datetime.fromisoformat(timestamp.replace("Z", "+00:00")).timestamp()
        except ValueError:
            continue
        events.append(LogEvent(when=when, message=message.lower()))
    return events


def _firmware_boot(message: str) -> str | None:
    """Classify an OVMF `BdsDxe: starting BootNNNN` line by its boot device.

    The firmware prints one of these per VM boot, so they are hardware-level
    phase boundaries: setup media (DVD-ROM) for Windows Setup, then the
    installed disk's Windows Boot Manager for every reboot after that.
    """
    if "bdsdxe: starting" not in message:
        return None
    if "dvd-rom" in message:
        return "setup-media"
    if "windows boot manager" in message:
        return "disk"
    return None


def _phase_timings(log: str, started: float, shell_ready: float | None) -> PhaseTimings:
    """Use dockur's and the firmware's own announcements; absent means unknown."""
    events: dict[str, float] = {}
    disk_boots: list[float] = []
    for event in _log_events(log):
        when, lowered = event.when, event.message
        if "downloading" in lowered and ("windows" in lowered or "server" in lowered):
            events.setdefault("download_start", when)
        if "extracting" in lowered and "image" in lowered:
            events.setdefault("download_end", when)
        if "booting windows" in lowered and "qemu" in lowered:
            events.setdefault("qemu_start", when)
        boot = _firmware_boot(lowered)
        if boot == "setup-media":
            events.setdefault("setup_media_boot", when)
        elif boot == "disk":
            disk_boots.append(when)
    download_end = events.get("download_end")
    qemu_start = events.get("qemu_start")
    setup_media = events.get("setup_media_boot")
    # Only disk boots after Setup started from media and before the OEM shell
    # marker belong to the install; a later reboot is not the first boot.
    installed_boots = [
        when
        for when in disk_boots
        if setup_media is not None
        and when >= setup_media
        and (shell_ready is None or when <= shell_ready)
    ]
    last_disk = installed_boots[-1] if installed_boots else None
    return PhaseTimings(
        iso_download_seconds=(
            max(0, download_end - events["download_start"])
            if download_end and "download_start" in events
            else None
        ),
        media_prep_seconds=(
            max(0, qemu_start - download_end) if qemu_start and download_end else None
        ),
        # Windows Setup: from booting setup media to the final reboot into
        # the installed disk (WinPE copy plus the specialize pass).
        install_seconds=(
            max(0, last_disk - setup_media) if setup_media and last_disk else None
        ),
        # First boot of the installed OS: OOBE, auto-logon, and dockur's
        # FirstLogonCommands until the OEM probe's shell marker.
        boot_seconds=(
            max(0, shell_ready - last_disk) if shell_ready and last_disk else None
        ),
        install_plus_first_boot_seconds=(
            max(0, shell_ready - qemu_start) if shell_ready and qemu_start else None
        ),
        firmware_disk_boots=len(installed_boots) if setup_media else None,
        observed_total_seconds=(max(0, shell_ready - started) if shell_ready else None),
    )


def _restore_timings(
    log: str, started: float, shell_ready: float | None
) -> PhaseTimings:
    """Boot timings of a restored disk; any setup-media boot means reinstall."""
    reinstalled = False
    first_disk = None
    for event in _log_events(log):
        when = event.when
        boot = _firmware_boot(event.message)
        if boot == "setup-media":
            reinstalled = True
        elif boot == "disk" and first_disk is None:
            first_disk = when
    return PhaseTimings(
        reinstalled=reinstalled,
        restored_boot_seconds=(max(0, shell_ready - started) if shell_ready else None),
        restored_firmware_to_shell_seconds=(
            max(0, shell_ready - first_disk)
            if shell_ready and first_disk and not reinstalled
            else None
        ),
    )


def _export_disk(storage: Path, destination: Path) -> DiskMeasure:
    """Write dockur's installed storage (minus setup media) as sparse tar.zst."""
    if not (storage / DOCKUR_INSTALLED_MARKER).exists():
        raise OSError(
            f"dockur never wrote {DOCKUR_INSTALLED_MARKER}; refusing to export an "
            "unfinished install"
        )
    began = time.monotonic()
    # Docker created the storage files as root; read them with sudo, but
    # write the archive as the runner so normal cleanup can remove it.
    with subprocess.Popen(
        (
            "sudo",
            "tar",
            "--sparse",
            "--exclude=*.iso",
            "--exclude=./tmp",
            "-C",
            str(storage),
            "-cf",
            "-",
            ".",
        ),
        stdout=subprocess.PIPE,
    ) as tar:
        with destination.open("wb") as output:
            zstd = subprocess.run(
                ("zstd", "-q", "-T0", "-3", "-c"),
                stdin=tar.stdout,
                stdout=output,
                check=False,
            )
        tar_status = tar.wait()
    if tar_status != 0 or zstd.returncode != 0:
        raise OSError("disk export failed (tar or zstd exited non-zero)")
    return DiskMeasure(
        emit=("compressed_bytes", "export_seconds"),
        compressed_bytes=destination.stat().st_size,
        export_seconds=time.monotonic() - began,
    )


def _extract_disk(archive: Path, storage: Path) -> float:
    """Restore dockur storage from the exported tar.zst; return seconds."""
    began = time.monotonic()
    with subprocess.Popen(
        ("zstd", "-q", "-d", "-c", str(archive)), stdout=subprocess.PIPE
    ) as zstd:
        tar = subprocess.run(
            ("tar", "--sparse", "-xf", "-", "-C", str(storage)),
            stdin=zstd.stdout,
            check=False,
        )
        zstd_status = zstd.wait()
    if zstd_status != 0 or tar.returncode != 0:
        raise OSError("disk restore failed (zstd or tar exited non-zero)")
    return time.monotonic() - began


def _write_summary(report: ProbeReport, path: Path) -> None:
    restore = report.mode == "restore"
    title = "restored-image boot" if restore else "cold boot"
    lines = [
        f"## Windows guest probe (#3295): {title}",
        "",
        f"**{report.decision.upper()}** — {report.reason}",
        "",
    ]
    for reason in report.reasons[1:]:
        lines.append(f"- also: {reason}")
    lines.extend(["", "| Measure | Result |", "|---|---|"])
    lines.append(f"| edition | {report.edition} |")
    lines.append(f"| dockur_version | {report.dockur_version} |")
    if restore:
        lines.append(f"| reinstalled | {report.reinstalled} |")
    sections = (
        ("", report.timings),
        ("disk.", report.disk),
        ("cache.", report.cache),
        ("nextest.", report.nextest),
    )
    for prefix, record in sections:
        for row in _summary_rows(record) if record is not None else ():
            shown = row.value if row.value is not None else "not measured"
            lines.append(f"| {prefix}{row.key} | {shown} |")
    for capability in fields(report.capabilities):
        lines.append(
            f"| {capability.name} | {getattr(report.capabilities, capability.name)} |"
        )
    lines.append("")
    lines.append(
        "This is one ephemeral evaluation boot, not a CI gate or a reusable activated image."
    )
    lines.append(
        "`install_seconds` runs from the firmware's setup-media boot to its last "
        "boot from the installed disk; `boot_seconds` runs from that boot to the "
        "OEM probe's shell marker. Both are unmeasured, never zero, when the "
        "firmware markers are absent."
    )
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def _kvm_usable() -> bool:
    device = Path("/dev/kvm")
    try:
        if not stat.S_ISCHR(device.stat().st_mode):
            return False
    except OSError:
        return False
    if not os.access(device, os.R_OK | os.W_OK):
        # The stock Ubuntu runner's KVM node may be root-only. The macOS
        # Recovery action enables the same device for its QEMU container.
        _run("sudo", "chmod", "0666", str(device), check=False, capture=True)
    return os.access(device, os.R_OK | os.W_OK)


def _host_created_at(path: Path, started: float) -> float | None:
    """Read host-kernel ctime, not a guest clock or delayed poll timestamp."""
    try:
        created = path.stat().st_ctime
    except OSError:
        return None
    return created if started <= created <= time.time() + 1 else None


def _prepare_scratch(scratch: Path, *, privileged_root: Path = Path("/mnt")) -> None:
    """Create the run-owned scratch directory on the runner's large /mnt disk."""
    try:
        scratch.mkdir(parents=True)
    except PermissionError as exc:
        if scratch.parent != privileged_root:
            raise
        # GitHub's Ubuntu runner exposes the large /mnt filesystem but keeps
        # its top level root-owned. Grant only this unique run directory to the
        # runner, so normal cleanup can remove every guest byte afterward.
        _run(
            "sudo",
            "install",
            "-d",
            "-o",
            str(os.getuid()),
            "-g",
            str(os.getgid()),
            str(scratch),
        )
        if not scratch.is_dir() or not os.access(scratch, os.W_OK):
            raise PermissionError(
                f"scratch is not writable after setup: {scratch}"
            ) from exc


def _remove_scratch(
    scratch: Path, run_id: str, *, privileged_root: Path = Path("/mnt")
) -> None:
    """Remove only this run's scratch, even if Docker made children root-owned."""
    try:
        shutil.rmtree(scratch)
    except PermissionError as exc:
        if scratch.parent != privileged_root or scratch.name != (
            f"soldr-windows-guest-{run_id}"
        ):
            raise
        _run("sudo", "rm", "-rf", "--", str(scratch))
        if scratch.exists():
            raise OSError(
                f"scratch still exists after privileged cleanup: {scratch}"
            ) from exc


def _docker_run_args(
    name: str, guest_edition: str, storage: Path, shared: Path, oem: Path
) -> list[str]:
    dockur_version, dockur_edition, _ = GUEST_EDITIONS[guest_edition]
    docker_args = [
        "docker",
        "run",
        "-d",
        "--name",
        name,
        "--device=/dev/kvm",
        "--device=/dev/net/tun",
        "--cap-add=NET_ADMIN",
        "--stop-timeout=120",
        "-e",
        f"VERSION={dockur_version}",
    ]
    if dockur_edition is not None:
        docker_args.extend(("-e", f"EDITION={dockur_edition}"))
    docker_args.extend(
        (
            "-e",
            "DISK_SIZE=32G",
            "-e",
            "RAM_SIZE=8G",
            "-e",
            "CPU_CORES=4",
            "-e",
            "LOG=Y",
            "-v",
            f"{storage}:/storage",
            "-v",
            f"{shared}:/shared",
            "-v",
            f"{oem}:/oem",
            IMAGE,
        )
    )
    return docker_args


def _guest_session(
    *,
    name: str,
    guest_edition: str,
    storage: Path,
    shared: Path,
    oem: Path,
    markers: tuple[str, ...],
    result_file: str,
    timeout_seconds: int,
    log_path: Path,
) -> GuestSession:
    """Boot dockur, record host times of guest markers, then stop it cleanly."""
    started = time.time()
    _run(*_docker_run_args(name, guest_edition, storage, shared, oem))
    seen: dict[str, float | None] = dict.fromkeys(markers)
    guest = None
    result_seen = None
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        for marker in markers:
            if seen[marker] is None and (shared / marker).is_file():
                seen[marker] = _host_created_at(shared / marker, started)
        if (shared / result_file).is_file():
            result_seen = _host_created_at(shared / result_file, started)
            guest = _read_guest_result(shared / result_file)
            break
        state = _run(
            "docker", "inspect", "--format", "{{.State.Running}}", name, capture=True
        )
        if state.stdout.strip() != "true":
            break
        time.sleep(10)
    # SIGTERM makes dockur send ACPI shutdown and wait for Windows, so the
    # disk is consistent and dockur records windows.boot for a later restore.
    _run("docker", "stop", "--time", "120", name, check=False, capture=True)
    log = _run("docker", "logs", "--timestamps", name, check=False, capture=True)
    log_path.write_text(log.stdout + log.stderr, encoding="utf-8")
    return GuestSession(
        started=started,
        markers=tuple(MarkerTime(name=name, at=at) for name, at in seen.items()),
        guest=guest,
        result_seen=result_seen,
        log=log.stdout + log.stderr,
    )


def _between(later: float | None, earlier: float | None) -> float | None:
    return (
        max(0, later - earlier) if later is not None and earlier is not None else None
    )


def _load_manifest(path: Path) -> CacheManifest:
    """The ownership manifest with its `budget` composed from ci.toml (soldr#3618)."""
    sys.path.insert(0, str(Path(__file__).resolve().parents[1] / ".github" / "scripts"))
    try:
        # pylint: disable-next=import-outside-toplevel
        import cache_families

        data = json.loads(path.read_text(encoding="utf-8"))
        if isinstance(data, dict):
            data = cache_families.with_budget(data)
    except (ImportError, OSError, ValueError):
        return CacheManifest()
    return _manifest_from_json(data)


def _set_output(name: str, value: str) -> None:
    if output_path := os.environ.get("GITHUB_OUTPUT"):
        with Path(output_path).open("a", encoding="utf-8") as output:
            output.write(f"{name}={value}\n")


def _cold_body(
    ctx: ProbeContext, progress: RunProgress, args: argparse.Namespace
) -> None:
    storage = ctx.storage
    storage.mkdir()
    _stage_payload(ctx.repo, ctx.artifact, ctx.shared, ctx.oem)
    progress.attempted_container = True
    session = _guest_session(
        name=ctx.name,
        guest_edition=ctx.guest_edition,
        storage=storage,
        shared=ctx.shared,
        oem=ctx.oem,
        markers=COLD_MARKERS,
        result_file="guest-result.json",
        timeout_seconds=args.timeout_seconds,
        log_path=ctx.log_path,
    )
    progress.guest = guest = session.guest
    shell_ready = session.marker("guest-shell-ready.txt")
    progress.timings = replace(
        _phase_timings(session.log, session.started, shell_ready),
        runtime_prep_seconds=_between(
            session.marker("runtime-install-start.txt"), shell_ready
        ),
        runtime_install_seconds=_between(
            session.marker("runtime-ready.txt"),
            session.marker("runtime-install-start.txt"),
        ),
        git_prep_seconds=_between(
            session.marker("tools-ready.txt"), session.marker("git-install-start.txt")
        ),
        replay_seconds=_between(session.result_seen, session.marker("tools-ready.txt")),
    )
    exported = None
    export_dir = getattr(args, "export_dir", None)
    warm_ready = (
        guest is not None
        and bool(guest.capabilities.warm_task)
        and guest.capabilities.warm_task != NOT_MEASURED
    )
    if getattr(args, "export_disk", "false") == "true" and export_dir and warm_ready:
        try:
            _prepare_scratch(Path(export_dir))
            exported = _export_disk(storage, Path(export_dir) / DISK_EXPORT_NAME)
        except (OSError, subprocess.CalledProcessError) as exc:
            progress.host = replace(progress.host, export_error=str(exc)[:500])
    if exported is None:
        progress.disk = _disk_measure(storage)
    else:
        progress.disk = replace(
            _disk_measure(storage, compress=False),
            emit=(
                "image_bytes",
                "allocated_bytes",
                "compressed_bytes",
                "export_seconds",
            ),
            compressed_bytes=exported.compressed_bytes,
            export_seconds=exported.export_seconds,
        )
        _set_output("disk_exported", "true")


def _restore_body(
    ctx: ProbeContext, progress: RunProgress, args: argparse.Namespace
) -> None:
    storage = ctx.storage
    download = ctx.scratch / "download"
    download.mkdir()
    began = time.monotonic()
    _run(
        "gh",
        "run",
        "download",
        str(args.disk_run_id),
        "-R",
        str(ctx.repository),
        "-n",
        DISK_ARTIFACT,
        "-D",
        str(download),
    )
    progress.timings = replace(
        progress.timings, restore_download_seconds=time.monotonic() - began
    )
    archive = download / DISK_EXPORT_NAME
    progress.compressed_bytes = archive.stat().st_size
    storage.mkdir()
    progress.timings = replace(
        progress.timings, restore_extract_seconds=_extract_disk(archive, storage)
    )
    archive.unlink()
    _stage_payload(ctx.repo, ctx.artifact, ctx.shared, ctx.oem, fetch_tools=False)
    progress.attempted_container = True
    session = _guest_session(
        name=ctx.name,
        guest_edition=ctx.guest_edition,
        storage=storage,
        shared=ctx.shared,
        oem=ctx.oem,
        markers=WARM_MARKERS,
        result_file="warm-guest-result.json",
        timeout_seconds=args.timeout_seconds,
        log_path=ctx.log_path,
    )
    progress.guest = session.guest
    restored = _restore_timings(
        session.log, session.started, session.marker("warm-guest-shell-ready.txt")
    )
    progress.timings = replace(
        progress.timings,
        reinstalled=restored.reinstalled,
        restored_boot_seconds=restored.restored_boot_seconds,
        restored_firmware_to_shell_seconds=restored.restored_firmware_to_shell_seconds,
        restored_replay_seconds=_between(
            session.result_seen, session.marker("warm-tools-ready.txt")
        ),
    )


def _remove_container(name: str) -> str | None:
    """Remove this run's uniquely named container; return an error, if any."""
    try:
        removal = _run("docker", "rm", "-f", name, check=False, capture=True)
        still_there = _run("docker", "inspect", name, check=False, capture=True)
    except OSError as exc:
        return str(exc)[:500]
    if (
        still_there.returncode == 0
        or ("No such object" not in still_there.stderr)
        or (removal.returncode != 0 and "No such container" not in removal.stderr)
    ):
        return f"container {name} removal unverified"
    return None


def run_probe(args: argparse.Namespace) -> int:  # noqa: C901
    mode = getattr(args, "mode", "cold")
    repository = getattr(args, "repository", None) or os.environ.get(
        "GITHUB_REPOSITORY"
    )
    if mode == "delete-artifact":
        if not repository:
            raise ValueError("delete-artifact needs --repository or GITHUB_REPOSITORY")
        removed = delete_run_artifact(repository, str(args.disk_run_id), DISK_ARTIFACT)
        print(f"deleted {removed} {DISK_ARTIFACT} artifact(s)")
        return 0
    guest_edition = getattr(args, "guest_edition", "server-core")
    if guest_edition not in GUEST_EDITIONS:
        raise ValueError(f"unsupported guest edition: {guest_edition}")
    repo, artifact, scratch = (
        Path(args.repo).resolve(),
        Path(args.artifact).resolve(),
        Path(args.scratch).resolve(),
    )
    output = Path(args.output).resolve()
    if scratch.exists():
        raise FileExistsError(
            f"refusing to remove pre-existing scratch path: {scratch}"
        )
    if scratch == output or scratch in output.parents:
        raise ValueError("output must live outside ephemeral guest scratch")
    output.parent.mkdir(parents=True, exist_ok=True)
    restore = mode == "restore"
    names = (
        {
            "warm-guest-result.json": "windows-guest-restore-raw-result.json",
            "warm-nextest.log": "windows-guest-restore-nextest.log",
        }
        if restore
        else {
            "guest-result.json": "windows-guest-raw-result.json",
            "nextest.log": "windows-guest-nextest.log",
            "vc-redist-install.log": "windows-guest-vc-redist.log",
        }
    )
    ctx = ProbeContext(
        repo=repo,
        artifact=artifact,
        scratch=scratch,
        shared=scratch / "shared",
        oem=scratch / "oem",
        storage=scratch / "storage",
        repository=repository,
        guest_edition=guest_edition,
        name=f"soldr-windows-probe-{args.run_id}-{uuid.uuid4().hex[:12]}",
        log_path=output.parent
        / (
            "windows-guest-restore-container.log"
            if restore
            else "windows-guest-container.log"
        ),
    )
    progress = RunProgress()
    try:
        _prepare_scratch(scratch)
        progress.host = replace(progress.host, kvm=_kvm_usable())
        free_bytes = shutil.disk_usage(scratch).free
        progress.host = replace(progress.host, free_bytes=free_bytes)
        if progress.host.kvm and free_bytes >= 32 * 1024**3:
            (_restore_body if restore else _cold_body)(ctx, progress, args)
        else:
            progress.host = replace(
                progress.host,
                preflight=(
                    "insufficient disk (<32 GiB)"
                    if progress.host.kvm
                    else "KVM unavailable"
                ),
            )
    except (OSError, subprocess.CalledProcessError, tarfile.TarError) as exc:
        progress.host = replace(progress.host, probe_error=str(exc)[:500])
    finally:
        for guest_name, host_name in names.items():
            guest_log = scratch / "shared" / guest_name
            try:
                if guest_log.is_file():
                    shutil.copy2(guest_log, output.parent / host_name)
            except OSError as exc:
                progress.host = replace(progress.host, diagnostic_error=str(exc)[:500])
        if progress.attempted_container:
            try:
                if not ctx.log_path.is_file():
                    log = _run(
                        "docker",
                        "logs",
                        "--timestamps",
                        ctx.name,
                        check=False,
                        capture=True,
                    )
                    ctx.log_path.write_text(log.stdout + log.stderr, encoding="utf-8")
            except OSError as exc:
                progress.host = replace(progress.host, diagnostic_error=str(exc)[:500])
            # The random suffix makes this name exclusive to this run,
            # including when `docker run` created a container then failed.
            if removal_error := _remove_container(ctx.name):
                progress.host = replace(progress.host, cleanup_error=removal_error)
        # All multi-GB guest state is ephemeral and never enters the Actions cache.
        try:
            if scratch.exists():
                _remove_scratch(scratch, str(args.run_id))
        except (OSError, subprocess.CalledProcessError) as exc:
            progress.host = replace(progress.host, cleanup_error=str(exc)[:500])
    host = progress.host
    compressed = (
        progress.compressed_bytes if restore else progress.disk.compressed_bytes
    )
    manifest_path = Path(
        getattr(args, "budget_manifest", None) or repo / "ci" / "cache-ownership.json"
    )
    cache = cache_budget(
        compressed,
        _load_manifest(manifest_path),
        _live_cache_usage(repository) if compressed is not None else None,
    )
    if restore:
        report = make_restore_report(
            host=host,
            timings=progress.timings,
            guest=progress.guest,
            cache=cache,
            guest_edition=guest_edition,
        )
    else:
        report = make_report(
            host=host,
            timings=progress.timings,
            guest=progress.guest,
            disk=progress.disk,
            guest_edition=guest_edition,
            cache=cache,
        )
    if host.preflight == "insufficient disk (<32 GiB)":
        report = replace(
            report,
            decision="no-go",
            reason="Runner scratch has less than dockur's 32 GiB minimum",
        )
    if host.cleanup_error:
        report = replace(
            report,
            decision="no-go",
            reason=f"Guest cleanup failed: {host.cleanup_error}",
        )
    output.write_text(
        json.dumps(report, indent=2, default=_json_default) + "\n", encoding="utf-8"
    )
    _write_summary(report, output.with_suffix(".md"))
    if summary_path := os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(summary_path).open("a", encoding="utf-8") as summary:
            summary.write(output.with_suffix(".md").read_text(encoding="utf-8"))
    return 1 if host.cleanup_error else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--mode", choices=("cold", "restore", "delete-artifact"), default="cold"
    )
    parser.add_argument("--repo")
    parser.add_argument("--artifact")
    parser.add_argument("--scratch")
    parser.add_argument("--output")
    parser.add_argument("--run-id")
    parser.add_argument(
        "--guest-edition", choices=tuple(GUEST_EDITIONS), default="server-core"
    )
    parser.add_argument("--timeout-seconds", type=int, default=3600)
    parser.add_argument(
        "--export-disk",
        choices=("true", "false"),
        default="false",
        help="cold: write the installed disk for this run's restore job",
    )
    parser.add_argument("--export-dir", help="cold: where the disk tar.zst goes")
    parser.add_argument("--disk-run-id", help="restore/delete: run owning the disk")
    parser.add_argument("--repository", help="owner/repo (default GITHUB_REPOSITORY)")
    parser.add_argument("--budget-manifest", help="default ci/cache-ownership.json")
    args = parser.parse_args()
    required: tuple[str, ...] = (
        ("disk_run_id",)
        if args.mode == "delete-artifact"
        else ("repo", "artifact", "scratch", "output", "run_id")
    )
    if args.mode == "restore":
        required += ("disk_run_id",)
    missing = [name for name in required if getattr(args, name) in (None, "")]
    if missing:
        parser.error(f"--mode {args.mode} requires: {', '.join(missing)}")
    return run_probe(args)


if __name__ == "__main__":
    raise SystemExit(main())
