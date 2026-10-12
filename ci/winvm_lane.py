"""The `winvm` lane: soldr's Windows-MSVC target-run suite in a local Windows VM.

zackees/ci.yml#202 (non-native platform lanes) and #198 (ci-attestations).
It attests `rust/x86_64-pc-windows-msvc/test`: the same owned test partition
`_ci-target-run.yml` replays natively on windows-2025 (selected by
`ci/target-run-ownership.json`, nextest profile `target-run`), built on this
Linux host with soldr and replayed by nextest inside a warm dockur/windows VM
-- never on the host (GATE-005). The archive build, the signed VC++ runtime
and the pinned MinGit are the guest probe's (`ci/windows_guest_probe.py`,
soldr#3295); the guest half is `ci/winvm_guest.ps1`.

Host-optional (local-gate.toml `optional = true`, ci_lint 9687a0b+): with no
`dockur-win` container or no usable /dev/kvm this exits 75 (EX_TEMPFAIL),
local-gate records `winvm:n/a`, never caches or attests the lane, and the
gate still passes -- the gate stays unattested, so CI and the release run it.
Every other problem is an ordinary failure.

The VM (one-time setup, not done by this script): a dockur/windows container
named `dockur-win` (`SOLDR_WINVM_CONTAINER`), KVM, `/shared` bind-mounted from
the host, SSH (OpenSSH server, default shell PowerShell) published on a host
port, and the key at `<shared>/../vm_key` (`SOLDR_WINVM_SSH_KEY`) authorised
for user `Docker`. The guest pins itself on every run: Windows Update is
disabled by policy (`HKLM\\SOFTWARE\\Policies\\Microsoft\\Windows\\
WindowsUpdate\\AU` NoAutoUpdate=1), its services (wuauserv, UsoSvc,
WaaSMedicSvc: Start=4) and the UpdateOrchestrator tasks, because an
auto-installed KB (KB5050575) once changed the warm image under the lane;
`C:\\swv` is excluded from Defender. A stopped VM is started and stopped
again afterwards; a running one is left running.

Selection (soldr#3556, zackees/ci.yml policy-general "Selecting full PR
coverage"): native VM coverage is selected for platform *implementation*
changes only -- `crates/soldr-platform` (the one place host selection may
live, enforced by the cfg ratchet), the toolchain/lockfile/workspace build
inputs, and this lane's own inputs. A CI-only or shared-logic diff exits 75
like an absent VM: not run, not attested, never a fabricated pass. Remote
CI and the release candidate's full run keep the Windows coverage.

Results land in `target/winvm-lane/` (JUnit, the nextest log, timings).
"""

from __future__ import annotations

import importlib.util
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import time
import tomllib
import xml.etree.ElementTree as ET
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import asdict, dataclass, replace
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(Path(__file__).resolve().parent))
# pylint: disable-next=wrong-import-position
from crate_graph import changed_paths  # noqa: E402 -- script-relative import

TARGET = "x86_64-pc-windows-msvc"
PROFILE = "ci-nextest"
NOT_APPLICABLE = 75  # EX_TEMPFAIL: local-gate's "not on this host"
GATE = f"rust/{TARGET}/test"
OUT = ROOT / "target" / "winvm-lane"
CACHE = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "soldr-winvm"
SSH_USER = "Docker"
GUEST_SCRIPT = ROOT / "ci" / "winvm_guest.ps1"
SHARE_SUBDIR = "soldr-winvm"
GUEST_SHARE = "\\\\host.lan\\Data\\" + SHARE_SUBDIR
BOOT_TIMEOUT = 600.0
RUN_TIMEOUT = 3 * 3600.0


@dataclass(frozen=True)
class Skip:
    """A test the VM cannot pass, kept out of the attested selection."""

    filterset: str
    reason: str


# Committed exclusions, each with its reason. Empty means the whole owned
# partition runs; every entry here is coverage the attestation does NOT
# carry, so CI-on-main and the release's native runs remain its proof.
_NO_MSVC = (
    "links a real crate with the MSVC target: the VM has no Visual Studio "
    "(vswhere/kernel32.lib absent) and soldr's managed MSVC 14.44.35207 bundle "
    "is not yet in the soldr-toolchain catalogue (soldr#1064); windows-2025 "
    "runners ship Visual Studio"
)
SKIPS: tuple[Skip, ...] = tuple(
    Skip(f"test(={name})", _NO_MSVC)
    for name in (
        "cli_cargo_native_cc::no_cache_global_disables_native_too",
        "cli_cargo_run_trampoline::binary_swap_with_matching_mtime_size_is_detected",
        "cli_cargo_run_trampoline::cold_invocation_writes_sidecar_and_runs_binary",
        "cli_wrapper::cargo_front_door_forces_msvc_target_even_with_polluted_path",
        "cli_daemon_lifecycle::cargo_test_recovers_after_daemon_stop_without_herd_spawning",
    )
)


@dataclass(frozen=True)
class Vm:
    container: str
    shared: Path
    ssh_key: Path
    port: str
    running: bool


@dataclass(frozen=True)
class SuiteCounts:
    binary: str
    tests: int
    failures: int
    skipped: int

    @property
    def passed(self) -> int:
        return self.tests - self.failures - self.skipped


@dataclass(frozen=True)
class Timings:
    """Wall-clock seconds per phase; None = not run (or the VM was already up)."""

    vm_boot: float | None = None
    build: float | None = None
    guest_prepare: float | None = None
    test_run: float | None = None


def _since(start: float) -> float:
    return round(time.monotonic() - start, 1)


class LaneError(Exception):
    pass


def _run_to_file(argv: list[str], log: Path, *, timeout: float | None = None) -> int:
    """Output goes to a file, never a pipe (zackees/ci.yml PY-003)."""
    with open(log, "wb") as fh:
        try:
            return subprocess.run(
                argv,
                cwd=ROOT,
                stdin=subprocess.DEVNULL,
                stdout=fh,
                stderr=subprocess.STDOUT,
                check=False,
                timeout=timeout,
            ).returncode
        except subprocess.TimeoutExpired:
            fh.write(f"\nwinvm lane: timed out after {timeout:.0f}s\n".encode())
            return 124


def _tail(log: Path, chars: int = 8000) -> str:
    return log.read_text(encoding="utf-8", errors="replace")[-chars:]


def _kvm_usable() -> bool:
    try:
        if not stat.S_ISCHR(Path("/dev/kvm").stat().st_mode):
            return False
    except OSError:
        return False
    return os.access("/dev/kvm", os.R_OK | os.W_OK)


def find_vm(container: str) -> Vm | None:
    """The VM's shared folder, SSH port and key, from `docker inspect`."""
    if shutil.which("docker") is None:
        return None
    log = OUT / "docker-inspect.json"
    if _run_to_file(["docker", "container", "inspect", container], log) != 0:
        return None
    info = json.loads(log.read_text(encoding="utf-8"))[0]
    shared = next(
        (
            Path(m["Source"])
            for m in info.get("Mounts", [])
            if m.get("Destination") == "/shared"
        ),
        None,
    )
    bindings = (info.get("HostConfig") or {}).get("PortBindings") or {}
    ports = bindings.get("22/tcp") or []
    if shared is None or not ports:
        raise LaneError(
            f"container {container} has no /shared mount or no published 22/tcp"
        )
    key = Path(os.environ.get("SOLDR_WINVM_SSH_KEY") or shared.parent / "vm_key")
    if not key.is_file():
        raise LaneError(f"SSH key {key} is missing (set SOLDR_WINVM_SSH_KEY)")
    return Vm(
        container, shared, key, ports[0]["HostPort"], bool(info["State"]["Running"])
    )


def ssh_argv(vm: Vm, command: str) -> list[str]:
    return [
        "ssh", "-i", str(vm.ssh_key), "-p", vm.port,
        "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR",
        "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=30",
        f"{SSH_USER}@127.0.0.1", command,
    ]  # fmt: skip


def wait_for_ssh(vm: Vm) -> float:
    """Seconds until the guest answered SSH."""
    start = time.monotonic()
    deadline = time.monotonic() + BOOT_TIMEOUT
    log = OUT / "ssh-wait.log"
    while time.monotonic() < deadline:
        if _run_to_file(ssh_argv(vm, "echo ready"), log, timeout=30) == 0:
            return _since(start)
        time.sleep(5)
    raise LaneError(
        f"the VM did not answer SSH within {BOOT_TIMEOUT:.0f}s: {_tail(log, 400)}"
    )


def build() -> Path:
    """Archive the workspace for Windows MSVC, as the CI cross-build does."""
    archive = OUT / "tests.tar.zst"
    archive.unlink(missing_ok=True)
    log = OUT / "build.log"
    print(f"winvm lane: building the {TARGET} nextest archive (soldr)", flush=True)
    code = _run_to_file(
        [
            "soldr", "cargo", "nextest", "archive",
            "--cargo-profile", PROFILE, "--target", TARGET, "--workspace",
            "--archive-file", str(archive), "--archive-format", "tar-zst",
        ],
        log,
    )  # fmt: skip
    if code != 0:
        raise LaneError(f"archive build failed (exit {code}):\n{_tail(log)}")
    soldr = ROOT / "target" / TARGET / PROFILE / "soldr.exe"
    if not soldr.is_file():
        code = _run_to_file(
            [
                "soldr",
                "build",
                "--profile",
                PROFILE,
                "--target",
                TARGET,
                "--package",
                "soldr-cli",
                "--bin",
                "soldr",
            ],
            OUT / "build-soldr.log",
        )
        if code != 0 or not soldr.is_file():
            raise LaneError(
                f"soldr.exe build failed (exit {code}):\n{_tail(OUT / 'build-soldr.log')}"
            )
    return archive


def _probe_module():
    spec = importlib.util.spec_from_file_location(
        "soldr_windows_guest_probe", ROOT / "ci" / "windows_guest_probe.py"
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def cached_tools() -> Path:
    """cargo-nextest.exe (catalogued), VC++ runtime and MinGit (pinned
    SHA-256), fetched once into the host cache."""
    known = (ROOT / "crates/soldr-fetch/src/fetch/known_tools.rs").read_text(
        encoding="utf-8"
    )
    match = re.search(r'CARGO_NEXTEST_PINNED_VERSION: &str = "([^"]+)"', known)
    if match is None:
        raise LaneError("CARGO_NEXTEST_PINNED_VERSION not found")
    tools = CACHE / f"tools-nextest-{match.group(1)}"
    tools.mkdir(parents=True, exist_ok=True)
    if not (tools / "cargo-nextest.exe").is_file():
        log = OUT / "fetch-nextest.log"
        code = _run_to_file(
            [
                sys.executable, ".github/scripts/fetch_catalogued_nextest.py",
                "--target", TARGET, "--version", match.group(1), "--output-dir", str(tools),
            ],
            log,
        )  # fmt: skip
        if code != 0:
            raise LaneError(f"cargo-nextest.exe fetch failed:\n{_tail(log)}")
    # The probe's pinned-SHA fetchers, reused rather than copied.
    probe = _probe_module()
    if not (tools / "vc_redist.x64.exe").is_file():
        probe._fetch_vc_redist(tools)  # pylint: disable=protected-access
    if not (tools / "mingit.zip").is_file():
        probe._fetch_mingit(tools)  # pylint: disable=protected-access
    return tools


def stage(vm: Vm, archive: Path, tools: Path) -> Path:
    staging = vm.shared / SHARE_SUBDIR
    if staging.exists():
        shutil.rmtree(staging)
    staging.mkdir()
    shutil.copy2(archive, staging / "tests.tar.zst")
    shutil.copy2(
        ROOT / "target" / TARGET / PROFILE / "soldr.exe", staging / "soldr.exe"
    )
    for name in ("cargo-nextest.exe", "vc_redist.x64.exe", "mingit.zip"):
        shutil.copy2(tools / name, staging / name)
    shutil.copy2(GUEST_SCRIPT, staging / GUEST_SCRIPT.name)
    # --workspace-remap needs the archived commit's tree (the probe's choice):
    # HEAD, with no target/ or local leftovers. local-gate runs on a clean tree.
    with open(staging / "workspace.tar", "wb") as fh:
        code = subprocess.run(
            ["git", "archive", "--format=tar", "HEAD"], cwd=ROOT, stdout=fh, check=False
        ).returncode
    if code != 0:
        raise LaneError("git archive HEAD failed")
    return staging


def guest(vm: Vm, phase: str, channel: str, log: Path, timeout: float) -> int:
    command = (
        f"powershell -NoProfile -ExecutionPolicy Bypass -File {GUEST_SHARE}\\{GUEST_SCRIPT.name} "
        f"-Phase {phase} -Channel {channel}"
    )
    return _run_to_file(ssh_argv(vm, command), log, timeout=timeout)


def owned_filter(list_json: Path) -> str:
    filter_file = OUT / "owned-filter.txt"
    log = OUT / "ownership.log"
    code = _run_to_file(
        [
            sys.executable, ".github/scripts/target_run_ownership.py",
            "--manifest", "ci/target-run-ownership.json", "--repo-root", str(ROOT),
            "--list-json", str(list_json), "--target", TARGET, "--filter-output", str(filter_file),
        ],
        log,
    )  # fmt: skip
    if code != 0:
        raise LaneError(f"target_run_ownership.py failed:\n{_tail(log)}")
    owned = filter_file.read_text(encoding="utf-8").strip()
    if not owned:
        raise LaneError("target-run ownership emitted an empty filter")
    if not SKIPS:
        return owned
    print(f"winvm lane: excluding {len(SKIPS)} committed skip(s) (SKIPS)", flush=True)
    return f"({owned}) & not ({' | '.join(s.filterset for s in SKIPS)})"


def junit_counts(junit: Path) -> list[SuiteCounts]:
    counts: list[SuiteCounts] = []
    for suite in ET.parse(junit).getroot().iter("testsuite"):
        counts.append(
            SuiteCounts(
                suite.get("name", "?"),
                int(suite.get("tests", "0")),
                int(suite.get("failures", "0")) + int(suite.get("errors", "0")),
                int(suite.get("skipped", "0")),
            )
        )
    return counts


def lane(vm: Vm, boot: Future[float] | None, timings: list[Timings]) -> int:
    """`timings[0]` is updated as phases finish, so a failure still reports them."""
    start = time.monotonic()
    archive = build()
    tools = cached_tools()
    timings[0] = replace(timings[0], build=_since(start))
    # The VM boots while the host builds; this joins the two.
    timings[0] = replace(
        timings[0], vm_boot=boot.result() if boot is not None else None
    )
    wait_for_ssh(vm)
    staging = stage(vm, archive, tools)
    channel = tomllib.loads((ROOT / "rust-toolchain.toml").read_text(encoding="utf-8"))[
        "toolchain"
    ]["channel"]

    start = time.monotonic()
    print(
        "winvm lane: preparing the guest (tools, toolchain, archive inventory)",
        flush=True,
    )
    code = guest(vm, "prepare", channel, OUT / "guest-prepare.log", 3600)
    timings[0] = replace(timings[0], guest_prepare=_since(start))
    if code != 0:
        raise LaneError(
            f"guest prepare failed (exit {code}):\n{_tail(OUT / 'guest-prepare.log')}"
        )
    # Windows PowerShell 5.1 writes UTF-8 with a BOM; the helpers want none.
    inventory = (staging / "list.json").read_text(encoding="utf-8-sig")
    (OUT / "nextest-all-list.json").write_text(inventory, encoding="utf-8")
    (staging / "filter.txt").write_text(
        owned_filter(OUT / "nextest-all-list.json") + "\n", encoding="utf-8"
    )

    start = time.monotonic()
    print("winvm lane: running the owned Windows MSVC partition in the VM", flush=True)
    code = guest(vm, "run", channel, OUT / "nextest.log", RUN_TIMEOUT)
    timings[0] = replace(timings[0], test_run=_since(start))
    if not (staging / "junit.xml").is_file():
        raise LaneError(
            f"no JUnit came back (nextest exit {code}):\n{_tail(OUT / 'nextest.log')}"
        )
    shutil.copy2(staging / "junit.xml", OUT / "junit.xml")
    counts = junit_counts(OUT / "junit.xml")
    for suite in counts:
        print(
            f"winvm lane: {suite.binary}: {suite.passed} passed, {suite.failures} failed, {suite.skipped} skipped",
            flush=True,
        )
    total = sum(s.tests for s in counts)
    failed = sum(s.failures for s in counts)
    if code != 0 or failed or total == 0:
        sys.stdout.write(_tail(OUT / "nextest.log", 12000))
        print(
            f"winvm lane: FAILED -- nextest exit {code}, {failed} failed of {total}",
            flush=True,
        )
        return 1
    print(
        f"winvm lane: {total - sum(s.skipped for s in counts)}/{total} passed ({GATE})",
        flush=True,
    )
    return 0


# soldr#3556: paths whose change selects native Windows coverage.
PLATFORM_PREFIXES = ("crates/soldr-platform/", ".cargo/", "ci/windows_guest_probe")
PLATFORM_FILES = frozenset(
    {
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "ci/winvm_lane.py",
        "ci/winvm_guest.ps1",
        "ci/crate_graph.py",
        "ci/target-run-ownership.json",
        ".config/nextest.toml",
        ".github/scripts/target_run_ownership.py",
        ".github/scripts/fetch_catalogued_nextest.py",
        ".github/scripts/download_catalogued_asset.py",
        ".github/scripts/toolchain_asset_query.py",
    }
)


def selection_reason(changed: list[str] | None) -> str | None:
    """None to run; otherwise why native coverage is not selected."""
    if not changed:
        return None
    if any(
        path in PLATFORM_FILES or path.startswith(PLATFORM_PREFIXES) for path in changed
    ):
        return None
    return (
        f"winvm lane: not selected (soldr#3556) -- {len(changed)} changed path(s), "
        "none a platform implementation, toolchain or lane input; "
        f"gate {GATE} not attested (CI and the release run it)"
    )


def main() -> int:
    reason = selection_reason(changed_paths(ROOT))
    if reason is not None:
        print(reason, flush=True)
        return NOT_APPLICABLE
    OUT.mkdir(parents=True, exist_ok=True)
    container = os.environ.get("SOLDR_WINVM_CONTAINER", "dockur-win")
    try:
        vm = find_vm(container) if _kvm_usable() else None
    except LaneError as exc:
        print(f"winvm lane: {exc}", flush=True)
        return 1
    if vm is None:
        print(
            f"winvm unavailable on this host (no `{container}` container or no usable /dev/kvm) -- "
            f"gate {GATE} not attested (omission = not run; CI/release run it)",
            flush=True,
        )
        return NOT_APPLICABLE
    timings = [Timings()]
    started = False
    try:
        with ThreadPoolExecutor(max_workers=1) as pool:
            boot: Future[float] | None = None
            if not vm.running:
                if (
                    _run_to_file(
                        ["docker", "start", vm.container], OUT / "docker-start.log"
                    )
                    != 0
                ):
                    raise LaneError(
                        f"docker start {vm.container} failed: {_tail(OUT / 'docker-start.log', 400)}"
                    )
                started = True
                boot = pool.submit(wait_for_ssh, vm)
            return lane(vm, boot, timings)
    except LaneError as exc:
        print(f"winvm lane: {exc}", flush=True)
        return 1
    finally:
        if started:
            _run_to_file(
                ["docker", "stop", "--timeout", "120", vm.container],
                OUT / "docker-stop.log",
            )
        (OUT / "timings.json").write_text(
            json.dumps(asdict(timings[0]), indent=2) + "\n", encoding="utf-8"
        )
        print(f"winvm lane: timings (s) {json.dumps(asdict(timings[0]))}", flush=True)


if __name__ == "__main__":
    raise SystemExit(main())
