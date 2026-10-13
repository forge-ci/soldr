#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""Parent-cache (worktree-cache-sharing) benchmark -- issue #352.

Runs locally (CLI) and in CI (same script). Three timed builds:

  A) Build the source repo cold -> populates the isolated zccache root.
  B) Build a sibling git worktree at the same HEAD with
     ZCCACHE_PATH_REMAP=auto. Should hit the parent cache populated by A.
  C) Build the same sibling worktree with SOLDR_PATH_REMAP=off and a fresh
     target/. No remap -> no parent-cache sharing -> full cold-build cost.

Speedup ratio = C / B. Script exits non-zero if ratio < threshold.

Build output streams to stdout with hh:mm:ss timestamps so slow steps are
visible mid-run. Final summary is a markdown table that pastes cleanly
into issue / PR comments.

Usage:
    bench/parent_cache_bench.py                          # build current repo
    bench/parent_cache_bench.py --target zccache         # clone + build zccache
    bench/parent_cache_bench.py --target repo:URL        # clone + build arbitrary repo
    bench/parent_cache_bench.py --threshold 10           # require 10x speedup
    bench/parent_cache_bench.py --keep                   # keep tmp dirs on exit
    THRESHOLD=8 bench/parent_cache_bench.py              # threshold via env

Targets:
    self      Build the current checkout. Lightweight. Default.
    zccache   git-clone https://github.com/zackees/zccache into a tempdir;
              use that as the source repo. Adds clone time (~10-30s) but
              gives a large, real-world build where compile time dominates
              cargo orchestration -- what you want for measuring the
              parent-cache speedup.
    repo:URL  Same as zccache but with an arbitrary repo URL.

Requires: python>=3.10, git, soldr on PATH. uv to launch the script.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

ZCCACHE_REPO_URL = "https://github.com/zackees/zccache"
DEFAULT_THRESHOLD = 5.0

# Scratch directories live under ~/.soldr/bench/ rather than %TEMP%
# so users can add a single Defender (or other AV) exclusion that covers
# every bench run. Random suffix avoids collisions between concurrent runs.
SOLDR_BENCH_ROOT = Path.home() / ".soldr" / "bench"


@dataclass
class BuildResult:
    label: str
    seconds: float


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Parent-cache benchmark for soldr issue #352",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument(
        "--target",
        default="self",
        help="Build target: 'self', 'zccache', or 'repo:URL'.",
    )
    parser.add_argument(
        "--threshold",
        type=float,
        default=float(os.environ.get("THRESHOLD", DEFAULT_THRESHOLD)),
        help=f"Minimum speedup ratio to pass (default {DEFAULT_THRESHOLD}).",
    )
    parser.add_argument(
        "--keep",
        action="store_true",
        help="Don't delete scratch directories on exit.",
    )
    parser.add_argument(
        "--stall-profile-seconds",
        type=int,
        default=int(os.environ.get("STALL_PROFILE_SECONDS", 600)),
        help=(
            "Once a stage has been running this long without finishing, start "
            "snapshotting process CPU/memory/IO every 30s so it's possible to "
            "diagnose what's stalling. Default 600 (10 min). Pass 0 to disable."
        ),
    )
    return parser.parse_args()


def resolve_target(target: str) -> str | None:
    """Return the clone URL for non-self targets, None for self."""
    if target == "self":
        return None
    if target == "zccache":
        return ZCCACHE_REPO_URL
    if target.startswith("repo:"):
        url = target[len("repo:") :]
        if not url:
            print(
                "error: --target repo: requires a URL after the colon", file=sys.stderr
            )
            sys.exit(2)
        return url
    print(
        f"error: unknown --target '{target}' (expected self, zccache, or repo:URL)",
        file=sys.stderr,
    )
    sys.exit(2)


def _defender_exclusion_paths() -> list[str] | None:
    """Return Defender's current exclusion paths, or None if unavailable.

    Quietly returns None if we're not on Windows, PowerShell isn't on PATH,
    Defender isn't installed, or the user lacks permission to query it.
    Failure here is informational only -- it should never break the bench.
    """
    if sys.platform != "win32":
        return None
    if shutil.which("powershell") is None:
        return None
    try:
        result = run_captured(
            [
                "powershell",
                "-NoProfile",
                "-Command",
                "(Get-MpPreference).ExclusionPath",
            ],
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if result.returncode != 0:
        return None
    return [line.strip() for line in result.stdout.splitlines() if line.strip()]


def _warn_if_no_defender_exclusion(path: Path) -> None:
    """Print a one-line warning if Defender's exclusion list doesn't cover path."""
    exclusions = _defender_exclusion_paths()
    if exclusions is None:
        return  # silent: not on Windows, or query failed
    needle = str(path).lower()
    covered = any(needle.startswith(excl.lower()) for excl in exclusions)
    if not covered:
        script = Path(__file__).parent / "add_defender_exclusions.ps1"
        print(
            f"WARNING: {path} is not in Windows Defender's exclusion list.\n"
            f"  Defender real-time scanning of fresh cache writes can add 10+ minutes\n"
            f"  to cold builds on Windows. Run once as administrator to fix:\n"
            f"      powershell -ExecutionPolicy Bypass -File {script}\n",
            file=sys.stderr,
        )


def preflight(target: str) -> None:
    """Bail with a clear message if the environment is missing pieces."""
    if shutil.which("git") is None:
        print("error: git not on PATH", file=sys.stderr)
        sys.exit(2)
    if shutil.which("soldr") is None:
        print(
            "error: soldr not on PATH. Install soldr or add it to PATH first.",
            file=sys.stderr,
        )
        sys.exit(2)
    if target == "self":
        cwd = Path.cwd()
        if not (cwd / ".git").exists():
            print(
                "error: --target self must run from a git repo root "
                "(no .git found in cwd)",
                file=sys.stderr,
            )
            sys.exit(2)


def run_streaming(
    label: str,
    cmd: list[str],
    cwd: Path,
    env: dict[str, str] | None = None,
) -> None:
    """Run cmd, streaming stdout+stderr with hh:mm:ss timestamps.

    Raises CalledProcessError on non-zero exit.
    """
    full_env = os.environ.copy()
    if env:
        full_env.update(env)
    proc = subprocess.Popen(
        cmd,
        cwd=str(cwd),
        env=full_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        bufsize=1,
        text=True,
    )
    assert proc.stdout is not None
    for raw_line in proc.stdout:
        line = raw_line.rstrip("\n")
        stamp = datetime.now().strftime("%H:%M:%S")
        print(f"[{stamp}] [{label}] {line}", flush=True)
    rc = proc.wait()
    if rc != 0:
        raise subprocess.CalledProcessError(rc, cmd)


@dataclass(frozen=True)
class ProcessCpu:
    """Cumulative CPU seconds of one process id."""

    pid: int
    cpu_seconds: float


@dataclass(frozen=True)
class ProcessCpuTimes:
    """Cumulative CPU time per pid at one snapshot."""

    samples: tuple[ProcessCpu, ...] = ()

    def cpu_for(self, pid: int) -> float | None:
        for sample in self.samples:
            if sample.pid == pid:
                return sample.cpu_seconds
        return None


def _capture_process_snapshot(
    label: str,
    elapsed_seconds: float,
    previous_cpu_times: ProcessCpuTimes,
) -> ProcessCpuTimes:
    """Print top-N processes by CPU-time delta since the last snapshot.

    Compares cumulative CPU time per pid between successive calls. The
    delta is the wall-CPU each process consumed during the interval --
    high values point at the process that's actually doing work (or being
    actively scanned by Defender / locking the disk / etc.). Defender's
    real-time scanner runs in MsMpEng.exe, so seeing it dominate is the
    "Defender is the bottleneck" signal we wanted.

    Best-effort: returns the previous map unchanged on any failure.
    Windows-only; macOS/Linux callers should never reach here.
    """
    try:
        result = run_captured(
            [
                "powershell",
                "-NoProfile",
                "-Command",
                "Get-Process | Where-Object { $_.CPU -ne $null } | "
                "Select-Object Name,Id,CPU,WorkingSet,Handles | "
                "ConvertTo-Json -Compress",
            ],
            capture_output=True,
            text=True,
            timeout=20,
        )
        if result.returncode != 0 or not result.stdout.strip():
            return previous_cpu_times
        parsed = json.loads(result.stdout)
        if isinstance(parsed, dict):
            parsed = [parsed]
        current: list[ProcessCpu] = []
        rows = []
        for p in parsed:
            pid = int(p["Id"])
            cpu = float(p["CPU"])
            current.append(ProcessCpu(pid, cpu))
            before = previous_cpu_times.cpu_for(pid)
            delta = cpu - (cpu if before is None else before)
            rows.append((delta, p))
        rows.sort(key=lambda r: r[0], reverse=True)
        stamp = datetime.now().strftime("%H:%M:%S")
        print(
            f"[{stamp}] [profile-{label}] STALL DETECTED at elapsed={elapsed_seconds:.0f}s; "
            f"top processes by CPU-time delta this 30s window:",
            flush=True,
        )
        for delta, p in rows[:8]:
            name = str(p.get("Name", "?"))
            pid = int(p.get("Id", 0))
            mem_mb = int(p.get("WorkingSet", 0)) // 1_048_576
            handles = int(p.get("Handles", 0))
            print(
                f"[{stamp}] [profile-{label}] "
                f"{name:30s} pid={pid:6d} cpu+={delta:7.2f}s mem={mem_mb:5d}MB handles={handles}",
                flush=True,
            )
        return ProcessCpuTimes(tuple(current))
    except (
        subprocess.SubprocessError,
        OSError,
        json.JSONDecodeError,
        ValueError,
    ) as exc:
        print(f"[profile-{label}] snapshot failed: {exc}", flush=True)
        return previous_cpu_times


def _stall_profiler(
    label: str,
    start_time: float,
    stop_event: threading.Event,
    threshold_seconds: int,
    interval_seconds: int = 30,
) -> None:
    """Background thread: snapshots process activity once a stage stalls.

    No-op on non-Windows or when threshold_seconds <= 0. Otherwise wakes
    every interval_seconds; once elapsed exceeds threshold, dumps a
    snapshot and continues until stop_event is set.
    """
    if sys.platform != "win32" or threshold_seconds <= 0:
        return
    cpu_times = ProcessCpuTimes()
    while not stop_event.is_set():
        elapsed = time.monotonic() - start_time
        if elapsed >= threshold_seconds:
            cpu_times = _capture_process_snapshot(label, elapsed, cpu_times)
        if stop_event.wait(interval_seconds):
            break


def time_build(
    label: str,
    cwd: Path,
    env: dict[str, str] | None = None,
    stall_profile_seconds: int = 600,
) -> BuildResult:
    """Time a soldr-wrapped cargo workspace release build.

    If the stage runs longer than stall_profile_seconds (default 10 min),
    a background watchdog starts snapshotting per-process CPU and IO
    every 30s so we can attribute the wait to a specific process
    (MsMpEng.exe = Defender, zccache.exe = daemon, etc.). Set to 0 to
    disable.
    """
    cmd = ["soldr", "cargo", "build", "--workspace", "--release"]
    start = time.monotonic()
    stop_event = threading.Event()
    profiler: threading.Thread | None = None
    if sys.platform == "win32" and stall_profile_seconds > 0:
        profiler = threading.Thread(
            target=_stall_profiler,
            args=(label, start, stop_event, stall_profile_seconds),
            daemon=True,
        )
        profiler.start()
    try:
        run_streaming(label, cmd, cwd, env=env)
    finally:
        stop_event.set()
        if profiler is not None:
            profiler.join(timeout=5)
    end = time.monotonic()
    return BuildResult(label=label, seconds=end - start)


def render_table(
    target: str,
    target_url: str | None,
    a: BuildResult,
    b: BuildResult,
    c: BuildResult,
    ratio: float,
    threshold: float,
) -> str:
    lines = [
        "==========================================================",
        "                  PARENT CACHE BENCHMARK",
        "==========================================================",
        "",
        f"target: {target}" + (f" ({target_url})" if target_url else ""),
        "",
        "| Stage                                                   | Wall seconds |",
        "|---------------------------------------------------------|--------------|",
        f"| A: worktree A populate (cold)                           | {a.seconds:12.2f} |",
        f"| B: worktree B warm with ZCCACHE_PATH_REMAP=auto         | {b.seconds:12.2f} |",
        f"| C: worktree B control with SOLDR_PATH_REMAP=off         | {c.seconds:12.2f} |",
        "",
        f"Speedup ratio (C / B): {ratio:.2f}x",
        f"Threshold:             {threshold:.2f}x",
        "",
    ]
    return "\n".join(lines)


def append_github_outputs(
    a: BuildResult,
    b: BuildResult,
    c: BuildResult,
    ratio: float,
    threshold: float,
) -> None:
    """If running under GitHub Actions, write the standard output files."""
    out = os.environ.get("GITHUB_OUTPUT")
    if out:
        with open(out, "a", encoding="utf-8") as fh:
            fh.write(f"a_seconds={a.seconds:.2f}\n")
            fh.write(f"b_seconds={b.seconds:.2f}\n")
            fh.write(f"c_seconds={c.seconds:.2f}\n")
            fh.write(f"ratio={ratio:.2f}\n")
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as fh:
            fh.write("### Parent-cache benchmark (issue #352)\n\n")
            fh.write("| Stage | Wall seconds |\n")
            fh.write("|---|---|\n")
            fh.write(f"| A: worktree A populate (cold) | `{a.seconds:.2f}` |\n")
            fh.write(
                f"| B: worktree B warm `ZCCACHE_PATH_REMAP=auto` | `{b.seconds:.2f}` |\n"
            )
            fh.write(
                f"| C: worktree B control `SOLDR_PATH_REMAP=off` | `{c.seconds:.2f}` |\n"
            )
            fh.write(f"\nSpeedup ratio (C / B): `{ratio:.2f}x`\n\n")
            fh.write(f"Threshold: `{threshold:.2f}x`\n")


def main() -> int:  # noqa: C901
    args = parse_args()
    target_url = resolve_target(args.target)
    preflight(args.target)

    # Predictable parent so users can add `~/.soldr/bench` once to their
    # antivirus exclusion list and never re-pay the per-run flush penalty
    # we see in Defender real-time scanning of fresh artifact writes.
    SOLDR_BENCH_ROOT.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    suffix = os.urandom(3).hex()
    scratch_root = SOLDR_BENCH_ROOT / f"parent-cache.{stamp}.{suffix}"
    scratch_root.mkdir(parents=True, exist_ok=False)
    soldr_cache_isolated = scratch_root / "zccache"
    worktree_b = scratch_root / "worktree-b"
    cloned_source_root: Path | None = None

    if sys.platform == "win32":
        _warn_if_no_defender_exclusion(SOLDR_BENCH_ROOT)
        _warn_if_no_defender_exclusion(Path.home() / ".soldr" / "cache")

    # Force a fresh zccache root so the only cache state in this benchmark
    # comes from worktree A's build. Without this, a cache left over from
    # earlier local builds would mask whether worktree B is hitting the
    # parent cache or its own warm cache.
    os.environ["SOLDR_CACHE_DIR"] = str(soldr_cache_isolated)

    try:
        # If a non-self target was selected, clone the source into the
        # scratch root and use the clone as worktree A. Otherwise A is
        # whatever git repo the user invoked us from.
        if target_url:
            cloned_source_root = scratch_root / "source"
            print(f"Cloning {target_url} into {cloned_source_root} ...")
            run_streaming(
                "clone",
                ["git", "clone", "--depth", "1", target_url, str(cloned_source_root)],
                cwd=Path.cwd(),
            )
            worktree_a = cloned_source_root
        else:
            worktree_a = Path.cwd()

        print(
            f"=== parent-cache benchmark ({datetime.now().isoformat(timespec='seconds')}) ==="
        )
        print(f"  target:                          {args.target}")
        if target_url:
            print(f"  source URL:                      {target_url}")
        print(f"  threshold:                       {args.threshold:.2f}x")
        print(f"  SOLDR_CACHE_DIR (isolated):      {soldr_cache_isolated}")
        print(f"  worktree A:                      {worktree_a}")
        print(f"  worktree B (will be created at): {worktree_b}")
        print()

        # Stage A: build the source repo cold.
        print("--- Stage A: build worktree A (cold, populate parent cache) ---")
        a = time_build(
            "A", cwd=worktree_a, stall_profile_seconds=args.stall_profile_seconds
        )
        print(f"Stage A finished in {a.seconds:.2f}s\n")

        # Stage B setup: add the sibling worktree.
        print("--- Stage B setup: add sibling worktree at HEAD ---")
        if worktree_b.exists():
            shutil.rmtree(worktree_b)
        run_streaming(
            "B-setup",
            ["git", "worktree", "add", str(worktree_b), "HEAD"],
            cwd=worktree_a,
        )
        print()

        # Stage B: warm build with remap explicit (pinned even though it's
        # the default -- guards against drift if soldr's default flips).
        print("--- Stage B: build worktree B (warm, ZCCACHE_PATH_REMAP=auto) ---")
        b = time_build(
            "B-warm",
            cwd=worktree_b,
            env={"ZCCACHE_PATH_REMAP": "auto"},
            stall_profile_seconds=args.stall_profile_seconds,
        )
        print(f"Stage B finished in {b.seconds:.2f}s\n")

        # Stage C: control build, no remap, fresh target/.
        shutil.rmtree(worktree_b / "target", ignore_errors=True)
        print(
            "--- Stage C: build worktree B "
            "(control, SOLDR_PATH_REMAP=off, fresh target/) ---"
        )
        c = time_build(
            "C-control",
            cwd=worktree_b,
            env={"SOLDR_PATH_REMAP": "off"},
            stall_profile_seconds=args.stall_profile_seconds,
        )
        print(f"Stage C finished in {c.seconds:.2f}s\n")

        # Ratio.
        ratio = float("inf") if b.seconds <= 0 else c.seconds / b.seconds

        # Print the summary.
        print(render_table(args.target, target_url, a, b, c, ratio, args.threshold))

        # Publish to GitHub Actions output if applicable.
        append_github_outputs(a, b, c, ratio, args.threshold)

        # Gate.
        if ratio == float("inf"):
            print("warm build measured 0s; treating as pass.")
            return 0
        if ratio < args.threshold:
            print(
                f"FAIL: parent-cache speedup {ratio:.2f}x is below threshold "
                f"{args.threshold:.2f}x",
                file=sys.stderr,
            )
            return 1
        print(
            f"PASS: parent-cache speedup {ratio:.2f}x meets threshold "
            f"{args.threshold:.2f}x"
        )
        return 0
    except subprocess.CalledProcessError as exc:
        print(
            f"command failed (rc={exc.returncode}): {' '.join(exc.cmd)}",
            file=sys.stderr,
        )
        return 1
    finally:
        if args.keep:
            print()
            print("kept scratch dirs:")
            print(f"  zccache: {soldr_cache_isolated}")
            print(f"  worktree: {worktree_b}")
            if cloned_source_root:
                print(f"  cloned source: {cloned_source_root}")
        else:
            # When target is "self", the sibling worktree is registered
            # against the user's repo. Use git worktree remove so the parent
            # repo's .git/worktrees bookkeeping is cleaned up too.
            if worktree_b.exists() and cloned_source_root is None:
                try:
                    run_captured(
                        ["git", "worktree", "remove", "--force", str(worktree_b)],
                        check=False,
                        capture_output=True,
                    )
                except OSError:
                    pass
            shutil.rmtree(scratch_root, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
