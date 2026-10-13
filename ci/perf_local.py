#!/usr/bin/env python3
"""Run Cargo commands in one recycled Docker runner with warm volumes.

A ``soldr-perf-local-<slug>-<hash>`` container mounts the repository's shared
git root read-only at ``/repo``. Linked worktrees below that root reuse the
same runner by changing only the ``docker exec`` working directory. Cargo target state,
Toolchain home, Soldr home, uv cache, and the Python environment live in named
volumes and survive runner resets.

The container and volume names are derived from the shared git root, so
sibling checkouts (``soldr``, ``soldr2``, ``soldr3``) each get their own
runner and their own warm volumes. They previously shared one global
``soldr-perf-local`` container while locking per-root, so a run started in
one checkout would ``docker rm -f`` another checkout's running container and
both would fight over a single Cargo target volume across different branches.

Usage::

    uv run --no-project python ci/perf_local.py cargo build --release
    uv run --no-project python ci/perf_local.py cargo test --workspace
    uv run --no-project python ci/perf_local.py smoke
    uv run --no-project python ci/perf_local.py smoke-console
    uv run --no-project python ci/perf_local.py smoke-debug
    uv run --no-project python ci/perf_local.py --status
    uv run --no-project python ci/perf_local.py --stop
    uv run --no-project python ci/perf_local.py --reset-runner
    uv run --no-project python ci/perf_local.py --wipe

``--stop`` and ``--reset-runner`` preserve warm volumes. ``--wipe`` is the
explicit destructive operation that removes both the runner and its volumes.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator, TypedDict

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

IMAGE = "soldr-cook-dev"
DOCKERFILE = "docker/cook-shared-cache/Dockerfile"
DOCKER_CONTEXT = "docker/cook-shared-cache"
CONTAINER_PREFIX = "soldr-perf-local"
VOLUME_PREFIX = "soldr-perf"
# Bumped from "1": schema 1 runners are the pre-per-root shared containers.
RUNNER_SCHEMA = "7"
LABEL_PREFIX = "io.soldr.perf-local"
PTRACE_ENV = "SOLDR_PERF_LOCAL_PTRACE"
BUILDER_NAME = "soldr-perf-local"
GC_MAX_AGE_SECS = 24 * 60 * 60
MAX_RUNNER_GROUPS = 3
GC_STATE_DIR = Path.home() / ".soldr" / "perf-local-gc"
RUNNER_VOLUME_BUDGET_BYTES = 50 * (1 << 30)
DEBUG_TRACE_RELATIVE = Path(".perf-local") / "debug-trace"

# The global names used before per-root isolation. Both this script and
# bench/cook_in_docker.sh have moved off them, but a sibling checkout that has
# not yet picked up those changes still drives them, so nothing here ever
# removes them — `--status` only reports them so their disk is not mistaken
# for this runner's.
SHARED_CONTAINER = "soldr-perf-local"
SHARED_VOLUMES = ("soldr-perf-target", "soldr-perf-cargo-home", "soldr-perf-soldr-home")

USAGE = """\
usage: python ci/perf_local.py cargo <args...>
       python ci/perf_local.py smoke
       python ci/perf_local.py smoke-console
       python ci/perf_local.py smoke-debug
       python ci/perf_local.py --status
       python ci/perf_local.py --stop
       python ci/perf_local.py --reset-runner
       python ci/perf_local.py --wipe

Every argument after `cargo` is forwarded verbatim to Cargo in the recycled
runner. --stop and --reset-runner preserve named volumes; --wipe removes them.

The runner and its volumes are per-checkout-root, so sibling checkouts
(soldr, soldr2, soldr3) never share or evict each other's. --status prints
which root the current runner belongs to.
"""


@dataclass(frozen=True)
class Runner:
    """Docker resource names for one shared git root."""

    source_root: Path
    container: str
    target: str
    cargo_home: str
    soldr_home: str
    uv_cache: str
    venv: str

    @property
    def volumes(self) -> tuple[str, ...]:
        return (self.target, self.cargo_home, self.soldr_home, self.uv_cache, self.venv)


@dataclass(frozen=True)
class VolumeRow:
    """A runner volume and where docker mounts it."""

    name: str
    mountpoint: str


class GcCandidate(TypedDict):
    source_root: Path
    last_used_epoch: float


def root_slug(source_root: Path) -> str:
    """Readable, docker-safe fragment of the root's directory name."""
    raw = source_root.resolve().name.lower()
    cleaned = "".join(char if char.isalnum() else "-" for char in raw).strip("-")
    return cleaned[:24] or "repo"


def root_tag(source_root: Path) -> str:
    """Stable per-root suffix.

    Case-folded because Windows paths are case-insensitive: `C:\\...\\Soldr2`
    and `c:\\...\\soldr2` are the same checkout and must not get two runners.
    """
    canonical = str(source_root.resolve()).casefold().encode("utf-8")
    return hashlib.sha256(canonical).hexdigest()[:8]


def runner_for(source_root: Path) -> Runner:
    suffix = f"{root_slug(source_root)}-{root_tag(source_root)}"
    return Runner(
        source_root=source_root,
        container=f"{CONTAINER_PREFIX}-{suffix}",
        target=f"{VOLUME_PREFIX}-target-{suffix}",
        cargo_home=f"{VOLUME_PREFIX}-cargo-home-{suffix}",
        soldr_home=f"{VOLUME_PREFIX}-soldr-home-{suffix}",
        uv_cache=f"{VOLUME_PREFIX}-uv-cache-{suffix}",
        venv=f"{VOLUME_PREFIX}-venv-{suffix}",
    )


def _canonical_root(path: Path) -> str:
    return str(path.resolve()).casefold()


def incremental_gc_candidate(
    candidates: list[GcCandidate],
    *,
    current_root: Path,
    now_epoch: float,
    max_age_secs: float = GC_MAX_AGE_SECS,
) -> GcCandidate | None:
    """Choose at most one stale runner, always protecting the current root."""
    current = _canonical_root(current_root)
    count_pressure = len(candidates) > MAX_RUNNER_GROUPS
    eligible = []
    for candidate in candidates:
        source_root = candidate["source_root"]
        if _canonical_root(source_root) == current:
            continue
        missing = not source_root.exists()
        last_used = candidate["last_used_epoch"]
        if missing or count_pressure or now_epoch - last_used >= max_age_secs:
            eligible.append(
                (0 if missing else 1, last_used, str(source_root).casefold(), candidate)
            )
    if not eligible:
        return None
    eligible.sort(key=lambda item: (item[0], item[1]))
    return eligible[0][3]


def activity_marker(source_root: Path) -> Path:
    return GC_STATE_DIR / f"{root_tag(source_root)}.last-used"


def mark_runner_used(source_root: Path) -> float:
    """Persist host-side activity because Docker volumes expose no last-use time."""
    marker = activity_marker(source_root)
    marker.parent.mkdir(parents=True, exist_ok=True)
    marker.touch()
    return marker.stat().st_mtime


def _managed_runner_roots() -> list[Path]:
    result = run_captured(
        [
            "docker",
            "ps",
            "-a",
            "--filter",
            f"label={LABEL_PREFIX}.source-root",
            "--format",
            f'{{{{.Names}}}}\t{{{{.Label "{LABEL_PREFIX}.source-root"}}}}',
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    roots = set()
    for line in result.stdout.splitlines():
        name, separator, raw_root = line.partition("\t")
        if not separator or not raw_root.strip():
            continue
        source_root = Path(raw_root.strip())
        if name.strip() == runner_for(source_root).container:
            roots.add(source_root)
    return sorted(roots, key=lambda path: str(path).casefold())


def incremental_gc(current_root: Path) -> None:
    """Best-effort bounded GC: remove no more than one stale runner group."""
    now = time.time()
    candidates: list[GcCandidate] = []
    for source_root in _managed_runner_roots():
        marker = activity_marker(source_root)
        if source_root.exists() and not marker.exists():
            # Migration grace for runners created before activity tracking.
            last_used = mark_runner_used(source_root)
        else:
            try:
                last_used = marker.stat().st_mtime
            except OSError:
                last_used = now
        candidates.append({"source_root": source_root, "last_used_epoch": last_used})
    selected = incremental_gc_candidate(
        candidates, current_root=current_root, now_epoch=now
    )
    if selected is None:
        return
    stale_root = selected["source_root"]
    if not stale_root.exists():
        _remove_runner_group(runner_for(stale_root))
        return
    # Serialize against the candidate's command and re-check activity after
    # acquiring its lock. A long-running build can never be reaped mid-command.
    with runner_lock(stale_root):
        marker = activity_marker(stale_root)
        try:
            last_used = marker.stat().st_mtime
        except OSError:
            last_used = now
        # If this runner was used after planning, leave it alone and let the
        # next invocation select a genuinely older group.
        if last_used > selected["last_used_epoch"]:
            return
        count_pressure = len(candidates) > MAX_RUNNER_GROUPS
        if (
            stale_root.exists()
            and not count_pressure
            and time.time() - last_used < GC_MAX_AGE_SECS
        ):
            return
        _remove_runner_group(runner_for(stale_root))


def _remove_runner_group(stale: Runner) -> None:
    removed = subprocess.run(["docker", "rm", "-f", stale.container], check=False)
    volumes = subprocess.run(
        ["docker", "volume", "rm", "--force", *stale.volumes], check=False
    )
    if removed.returncode == 0 and volumes.returncode == 0:
        print(f"incremental gc: removed stale runner {stale.container}")
    else:
        print(
            f"warning: incremental gc could not fully remove {stale.container}",
            file=sys.stderr,
        )


def buildkit_prune_command() -> list[str]:
    return [
        "docker",
        "buildx",
        "prune",
        "--builder",
        BUILDER_NAME,
        "--filter",
        "until=24h",
        "--force",
    ]


def output_excerpt(result: subprocess.CompletedProcess, limit: int = 500) -> str:
    """One-line excerpt of a finished command's stderr (else stdout)."""
    text = (result.stderr or "").strip() or (result.stdout or "").strip()
    flat = " ".join(text.split())
    return flat[:limit] if flat else "<no output>"


def _run_small(args: list[str]) -> subprocess.CompletedProcess:
    """Run a small docker probe; always forward its stderr, prefixed (soldr#3386)."""
    result = run_captured(args, capture_output=True, text=True, check=False)
    prefix = " ".join(args[:3])
    for line in (result.stderr or "").splitlines():
        if line.strip():
            print(f"{prefix}: {line}", file=sys.stderr)
    return result


def _builder_exists() -> bool:
    return _run_small(["docker", "buildx", "inspect", BUILDER_NAME]).returncode == 0


def _ensure_builder() -> bool:
    version = _run_small(["docker", "buildx", "version"])
    if version.returncode != 0:
        print(
            "warning: docker buildx unavailable; falling back to docker build: "
            f"{output_excerpt(version)}",
            file=sys.stderr,
        )
        return False
    if _builder_exists():
        return True
    created = _run_small(["docker", "buildx", "create", "--name", BUILDER_NAME])
    if created.returncode != 0:
        print(
            f"warning: could not create BuildKit builder {BUILDER_NAME}; "
            f"falling back to docker build: {output_excerpt(created)}",
            file=sys.stderr,
        )
        return False
    return True


def incremental_buildkit_gc() -> None:
    """Prune only soldr's BuildKit records; never touch Docker's default builder."""
    if _builder_exists():
        result = _run_small(buildkit_prune_command())
        if result.returncode != 0:
            print(
                f"warning: soldr BuildKit GC failed: {output_excerpt(result)}",
                file=sys.stderr,
            )


def runner_over_budget(usage_bytes: int) -> bool:
    return usage_bytes > RUNNER_VOLUME_BUDGET_BYTES


def runner_volume_usage_bytes(runner: Runner) -> int | None:
    result = run_captured(
        [
            "docker",
            "exec",
            runner.container,
            "du",
            "-sb",
            "/target",
            "/root/.cargo",
            "/root/.soldr",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        return None
    try:
        return sum(int(line.split()[0]) for line in result.stdout.splitlines())
    except (IndexError, ValueError):
        return None


def enforce_runner_budget(runner: Runner, image_id: str) -> None:
    usage = runner_volume_usage_bytes(runner)
    if usage is None or not runner_over_budget(usage):
        return
    print(f"incremental gc: {runner.container} exceeds 50 GiB; rotating warm volumes")
    if wipe(runner) != 0:
        raise RuntimeError(f"unable to rotate over-budget runner {runner.container}")
    ensure_runner(runner, image_id)


def main(argv: list[str]) -> int:  # noqa: C901
    repo_root = Path(__file__).resolve().parent.parent
    source_root = shared_source_root(repo_root)
    runner = runner_for(source_root)
    os.chdir(repo_root)

    if not shutil.which("docker"):
        print("error: docker not on PATH", file=sys.stderr)
        return 2
    if not argv:
        print(USAGE, file=sys.stderr)
        return 2
    if argv[0] in ("-h", "--help"):
        print(USAGE)
        return 0
    if argv[0] == "--status":
        return status(runner)
    if argv[0] in ("--stop", "--reset-runner", "--wipe"):
        with runner_lock(source_root):
            if argv[0] == "--stop":
                return stop_runner(runner)
            if argv[0] == "--reset-runner":
                return reset_runner(runner)
            return wipe(runner)
    command_argv = container_argv(argv)
    smoke_command = argv in (["smoke"], ["smoke-console"], ["smoke-debug"])
    if argv[0].startswith("smoke") and not smoke_command:
        print(f"error: smoke commands take no arguments\n\n{USAGE}", file=sys.stderr)
        return 2
    if not smoke_command and argv[0] != "cargo":
        print(
            f"error: expected `cargo` as the first arg, got {argv[0]!r}\n\n{USAGE}",
            file=sys.stderr,
        )
        return 2

    # Serialize the full command. Cargo can lock a shared target itself, but
    # parallel LTO builds make every sample slower and invalidate perf data.
    # The lock is per-root, which is only safe because the runner and volumes
    # it guards are per-root too.
    mark_runner_used(source_root)
    incremental_gc(source_root)
    incremental_buildkit_gc()
    with runner_lock(source_root):
        try:
            image_id = ensure_image(repo_root)
        except RuntimeError as error:
            print(f"error: {error}", file=sys.stderr)
            return 1
        ensure_runner(runner, image_id)
        enforce_runner_budget(runner, image_id)
        workdir = container_workdir(source_root, repo_root)
        try:
            completed = subprocess.run(
                exec_command(runner, command_argv, workdir, tty=tty_enabled()),
                check=False,
            )
            if argv == ["smoke-debug"]:
                retain_debug_trace(runner)
            return completed.returncode
        finally:
            mark_runner_used(source_root)


def retain_debug_trace(runner: Runner) -> None:
    """Copy the smoke run's process-trace JSONL out of the runner (soldr#2546).

    Dev-built soldr roots at ``~/.soldr-dev`` inside the container; the
    timelines land under ``logs/debug-trace/``. The checkout is mounted
    read-only, so this explicit smoke-debug-only path copies them to
    ``.perf-local/debug-trace/`` on the host after the command completes.
    Best-effort: a smoke that spawned no traced child simply retains nothing.
    """
    output_dir = debug_trace_output_dir(runner)
    output_dir.mkdir(parents=True, exist_ok=True)
    for source_dir in (
        "/root/.soldr-dev/logs/debug-trace",
        "/root/.soldr/logs/debug-trace",
    ):
        exists = subprocess.run(
            ["docker", "exec", runner.container, "test", "-d", source_dir],
            check=False,
        )
        if exists.returncode == 0:
            subprocess.run(
                ["docker", "cp", f"{runner.container}:{source_dir}/.", str(output_dir)],
                check=False,
            )
    retained = sorted(output_dir.glob("*.jsonl"))
    if retained:
        print(
            "smoke-debug: process timelines retained in .perf-local/debug-trace/ "
            f"(latest: {retained[-1].name})"
        )
    else:
        print("smoke-debug: no process timelines were produced by this run")


def shared_source_root(repo_root: Path) -> Path:
    """Return the checkout root whose .git directory owns all worktrees."""
    out = run_captured(
        ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
        cwd=repo_root,
        capture_output=True,
        text=True,
        check=False,
    )
    if out.returncode == 0:
        common = Path(out.stdout.strip()).resolve()
        if common.name == ".git":
            return common.parent
    return repo_root.resolve()


def debug_trace_output_dir(runner: Runner) -> Path:
    """The only checkout-relative output written by the managed runner."""
    return runner.source_root / DEBUG_TRACE_RELATIVE


def container_workdir(source_root: Path, repo_root: Path) -> str:
    relative = repo_root.resolve().relative_to(source_root.resolve())
    return "/repo" if relative == Path(".") else f"/repo/{relative.as_posix()}"


def tty_enabled() -> bool:
    value = os.environ.get("SOLDR_PERF_LOCAL_TTY", "").strip().lower()
    return value in ("1", "true", "yes")


def container_argv(argv: list[str]) -> list[str]:
    """Resolve a public runner command to the process executed in Linux."""
    if argv == ["smoke"]:
        return ["bash", "ci/smoke_local.sh"]
    if argv == ["smoke-console"]:
        return [
            "env",
            "SOLDR_SMOKE_TOKIO_CONSOLE=1",
            "bash",
            "ci/smoke_local.sh",
        ]
    if argv == ["smoke-debug"]:
        # soldr#2546: the smoke pipeline under `soldr --debug`-equivalent
        # tracing — every front-door child spawn (and, on observed paths,
        # descendants) lands in the JSONL timelines that
        # `retain_debug_trace` copies out after the run.
        return [
            "env",
            "SOLDR_DEBUG_TRACE=1",
            "bash",
            "ci/smoke_local.sh",
        ]
    return argv


@dataclass(frozen=True)
class Label:
    """One docker label."""

    key: str
    value: str


@dataclass(frozen=True)
class InspectedObject:
    """The fields of `docker inspect` output this module consults."""

    image_id: str | None
    labels: tuple[Label, ...] | None

    def label_value(self, key: str) -> str | None:
        for label in self.labels or ():
            if label.key == key:
                return label.value
        return None


def inspected_object(raw: object) -> InspectedObject:
    """Parse one `docker inspect` JSON object, tolerating absent sections."""
    fields = raw if isinstance(raw, dict) else {}
    config = fields.get("Config")
    raw_labels = config.get("Labels") if isinstance(config, dict) else None
    labels = (
        tuple(
            Label(key, value)
            for key, value in raw_labels.items()
            if isinstance(value, str)
        )
        if isinstance(raw_labels, dict)
        else None
    )
    image_id = fields.get("Id")
    return InspectedObject(str(image_id) if image_id else None, labels)


def expected_labels(source_root: Path, image_id: str) -> tuple[Label, ...]:
    return (
        Label(f"{LABEL_PREFIX}.managed", "true"),
        Label(f"{LABEL_PREFIX}.schema", RUNNER_SCHEMA),
        Label(f"{LABEL_PREFIX}.image-id", image_id),
        Label(f"{LABEL_PREFIX}.source-root", str(source_root.resolve())),
        Label(f"{LABEL_PREFIX}.ptrace", "1" if ptrace_enabled() else "0"),
    )


def dockerfile_digest(repo_root: Path) -> str:
    return hashlib.sha256((repo_root / DOCKERFILE).read_bytes()).hexdigest()


def image_info() -> InspectedObject | None:
    inspected = run_captured(
        ["docker", "image", "inspect", IMAGE],
        capture_output=True,
        text=True,
        check=False,
    )
    if inspected.returncode != 0:
        return None
    decoded = json.loads(inspected.stdout)
    return inspected_object(decoded[0]) if decoded else None


def ensure_image(repo_root: Path) -> str:
    digest = dockerfile_digest(repo_root)
    digest_label = f"{LABEL_PREFIX}.dockerfile-sha256"
    info = image_info()
    if info is not None:
        if info.label_value(digest_label) == digest and info.image_id:
            return info.image_id

    if _ensure_builder():
        build_command = [
            "docker",
            "buildx",
            "build",
            "--builder",
            BUILDER_NAME,
            "--load",
        ]
    else:
        build_command = ["docker", "build"]
    build = subprocess.run(
        [
            *build_command,
            "--provenance=false",
            "--label",
            f"{LABEL_PREFIX}.managed=true",
            "--label",
            f"{digest_label}={digest}",
            "-f",
            DOCKERFILE,
            "-t",
            IMAGE,
            DOCKER_CONTEXT,
        ],
        check=False,
    )
    if build.returncode != 0:
        raise RuntimeError(f"docker build failed with exit code {build.returncode}")
    info = image_info()
    image_id = info.image_id if info is not None else None
    if not image_id:
        raise RuntimeError(f"unable to inspect image {IMAGE} after build")
    return str(image_id)


def runner_matches(info: InspectedObject, labels: tuple[Label, ...]) -> bool:
    return info.labels is not None and all(
        info.label_value(label.key) == label.value for label in labels
    )


def create_command(runner: Runner, image_id: str) -> list[str]:
    command = ["docker", "create", "--name", runner.container, "--init"]
    if ptrace_enabled():
        command.extend(["--cap-add=SYS_PTRACE", "--security-opt", "seccomp=unconfined"])
    for label in expected_labels(runner.source_root, image_id):
        command.extend(["--label", f"{label.key}={label.value}"])
    command.extend(
        [
            "--mount",
            f"type=bind,source={runner.source_root.resolve()},target=/repo,readonly",
            "-v",
            f"{runner.soldr_home}:/root/.soldr",
            "-v",
            f"{runner.uv_cache}:/root/.cache/uv",
            "-v",
            f"{runner.venv}:/venv",
            "-v",
            f"{runner.target}:/target",
            "-v",
            f"{runner.cargo_home}:/root/.cargo",
            "-e",
            "CARGO_TARGET_DIR=/target",
            # Keep temporary executables on the target volume. The smoke
            # suite materializes aliases of its large unoptimized test binary;
            # /tmp lives on the container overlay, so that otherwise degrades
            # every hardlink into a full copy plus content verification.
            "-e",
            "TMPDIR=/target/tmp",
            "-e",
            "CARGO_BUILD_JOBS=2",
            "-e",
            "SOLDR_JOBS=2",
            "-e",
            "UV_CACHE_DIR=/root/.cache/uv",
            "-e",
            "UV_PROJECT_ENVIRONMENT=/venv",
            # Docker Desktop commonly exposes host CPU count with a much
            # smaller VM memory budget. Bound nextest so every wrapped test
            # process can always start under the full smoke suite.
            "-e",
            "NEXTEST_TEST_THREADS=2",
            # soldr#2739 removed the explicit SOLDR_REENTRANCY_GUARD=strict
            # export here: enforcement is the default now, so the runner gets
            # the same invariant as every CI lane without opting in.
            "-w",
            "/repo",
            IMAGE,
            "tail",
            "-f",
            "/dev/null",
        ]
    )
    return command


def ptrace_enabled() -> bool:
    return os.environ.get(PTRACE_ENV, "").strip().lower() in ("1", "true", "yes")


def exec_command(
    runner: Runner, argv: list[str], workdir: str, *, tty: bool
) -> list[str]:
    command = ["docker", "exec"]
    if tty:
        command.append("-it")
    command.extend(["-w", workdir, runner.container, *argv])
    return command


def ensure_runner(runner: Runner, image_id: str) -> None:
    labels = expected_labels(runner.source_root, image_id)
    inspected = run_captured(
        ["docker", "inspect", runner.container],
        capture_output=True,
        text=True,
        check=False,
    )
    exists = inspected.returncode == 0
    if exists:
        info = inspected_object(json.loads(inspected.stdout)[0])
        # A name collision is now impossible across roots, so a mismatch only
        # means a stale image or schema for THIS root — safe to recreate.
        if not runner_matches(info, labels):
            subprocess.run(["docker", "rm", "-f", runner.container], check=True)
            exists = False
    if not exists:
        for volume in runner.volumes:
            subprocess.run(
                [
                    "docker",
                    "volume",
                    "create",
                    "--label",
                    f"{LABEL_PREFIX}.managed=true",
                    "--label",
                    f"{LABEL_PREFIX}.source-root={runner.source_root.resolve()}",
                    volume,
                ],
                check=True,
            )
        subprocess.run(create_command(runner, image_id), check=True)

    running = docker_output(
        ["inspect", "--format", "{{.State.Running}}", runner.container]
    )
    if running != "true":
        subprocess.run(["docker", "start", runner.container], check=True)
    subprocess.run(
        ["docker", "exec", runner.container, "mkdir", "-p", "/target/tmp"],
        check=True,
    )


def docker_output(args: list[str]) -> str:
    out = run_captured(["docker", *args], capture_output=True, text=True, check=False)
    return out.stdout.strip() if out.returncode == 0 else ""


def git_common_dir(root: Path) -> Path:
    """The `.git` directory shared by a checkout and all its worktrees.

    soldr#2008. In a **linked worktree** `.git` is a *file* containing
    `gitdir: <path>`, not a directory. Code that did
    `(root / ".git").mkdir(parents=True, exist_ok=True)` therefore raised
    `FileExistsError` -- `exist_ok` tolerates an existing directory, not an
    existing file. CLAUDE.md documents running this script from worktrees as
    the standard agent flow, so that path has to work.

    Resolved by reading the file rather than shelling out to
    `git rev-parse --git-common-dir`, because the callers that need this most
    are exactly the ones that stub `subprocess.run`; a subprocess-based lookup
    silently degrades to the wrong answer under test and is untestable in the
    place it matters.

    A worktree's `gitdir` points at `<common>/worktrees/<name>`, so the shared
    parent is two levels up. That matters: the runner lock exists to serialize
    every worktree against one container, and a per-worktree lock would
    serialize nothing.
    """
    candidate = root / ".git"
    if candidate.is_dir():
        return candidate
    if candidate.is_file():
        try:
            text = candidate.read_text(encoding="utf-8").strip()
        except OSError:
            return candidate
        if text.startswith("gitdir:"):
            target = Path(text[len("gitdir:") :].strip())
            if not target.is_absolute():
                target = (root / target).resolve()
            if target.parent.name == "worktrees":
                return target.parent.parent
            return target
    return candidate


@contextmanager
def runner_lock(source_root: Path) -> Iterator[None]:
    lock_path = git_common_dir(source_root) / "soldr-perf-local.lock"
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("a+b") as handle:
        if sys.platform == "win32":
            # Both platform imports need `import-error` suppressed, and each
            # for the OTHER platform's benefit: msvcrt is unresolvable when
            # linting on Linux (as CI does), fcntl when linting on Windows.
            # Suppressing only one makes the check pass or fail depending on
            # who ran it -- which is exactly what CI caught here.
            # pylint: disable-next=import-outside-toplevel,import-error
            import msvcrt  # Windows-only; cannot be imported at module scope

            handle.seek(0, os.SEEK_END)
            if handle.tell() == 0:
                handle.write(b"0")
                handle.flush()
            handle.seek(0)
            msvcrt.locking(handle.fileno(), msvcrt.LK_LOCK, 1)
            try:
                yield
            finally:
                handle.seek(0)
                msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
        else:
            # pylint: disable-next=import-outside-toplevel,import-error
            import fcntl  # Unix-only; unresolvable when linting on Windows

            fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
            try:
                yield
            finally:
                fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


def reset_runner(runner: Runner) -> int:
    if not docker_output(["inspect", "--format", "{{.Id}}", runner.container]):
        return 0
    return subprocess.run(
        ["docker", "rm", "-f", runner.container], check=False
    ).returncode


def stop_runner(runner: Runner) -> int:
    if not docker_output(["inspect", "--format", "{{.Id}}", runner.container]):
        return 0
    return subprocess.run(["docker", "stop", runner.container], check=False).returncode


def wipe(runner: Runner) -> int:
    reset_runner(runner)
    out = subprocess.run(
        ["docker", "volume", "rm", "--force", *runner.volumes],
        check=False,
    )
    return out.returncode


def status(runner: Runner) -> int:
    state = (
        docker_output(["inspect", "--format", "{{.State.Status}}", runner.container])
        or "(absent)"
    )
    print(f"root: {runner.source_root}")
    print(f"{runner.container}  {state}")
    rows: list[VolumeRow] = []
    for name in runner.volumes:
        mountpoint = docker_output(
            ["volume", "inspect", "--format", "{{.Mountpoint}}", name]
        )
        rows.append(VolumeRow(name, mountpoint or "(absent)"))
    width = max(len(row.name) for row in rows)
    for row in rows:
        print(f"{row.name:<{width}}  {row.mountpoint}")
    report_shared_resources()
    return 0


def report_shared_resources() -> None:
    """Report the non-per-root resources, which this runner no longer uses."""
    present = []
    if docker_output(["inspect", "--format", "{{.Id}}", SHARED_CONTAINER]):
        present.append(f"container {SHARED_CONTAINER}")
    for name in SHARED_VOLUMES:
        if docker_output(["volume", "inspect", "--format", "{{.Name}}", name]):
            present.append(f"volume {name}")
    if not present:
        return
    print()
    print("machine-wide resources (NOT owned by this root's runner):")
    for item in present:
        print(f"  {item}")
    print("  Left over from before per-root isolation. A sibling checkout")
    print("  still on the old scripts may be using them — check before")
    print("  removing, then reclaim the disk.")


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
