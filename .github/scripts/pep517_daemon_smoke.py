#!/usr/bin/env python3
"""Build a tiny downstream wheel through soldr's PEP 517 backend.

This smoke is intentionally native-runner only. It installs the soldr wheel
under test into an isolated venv, then runs `pip wheel --no-build-isolation`
against a tiny Rust binary project whose `build-backend` is `soldr`.
`SOLDR_DAEMON_REQUIRED=1` makes daemon startup regressions fail loudly instead
of silently falling back to direct rustc.
"""

from __future__ import annotations

import argparse
import glob
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import TextIO

DEFAULT_RUST_TOOLCHAIN = "1.98.1"
OUTER_SOLDR_ENV = (
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "SOLDR_BROKER_SERVICE",
    "SOLDR_INTERNAL_DAEMON_EXE",
)

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
if hasattr(sys.stderr, "reconfigure"):
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--wheel",
        nargs="+",
        required=True,
        help="Wheel path or glob. Exactly one wheel must match.",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=180,
        help="Timeout in seconds for the downstream pip wheel smoke.",
    )
    parser.add_argument(
        "--log-dir",
        type=Path,
        help="Directory to preserve Soldr and daemon logs for CI artifacts.",
    )
    return parser.parse_args()


def resolve_wheel(patterns: list[str]) -> Path:
    matches: list[Path] = []
    for pattern in patterns:
        expanded = [Path(p) for p in glob.glob(pattern)]
        matches.extend(expanded or [Path(pattern)])
    wheels = [p.resolve() for p in matches if p.is_file() and p.suffix == ".whl"]
    if len(wheels) != 1:
        raise SystemExit(f"expected exactly one wheel, found {len(wheels)}: {wheels}")
    return wheels[0]


def venv_bin(venv: Path) -> Path:
    return venv / ("Scripts" if os.name == "nt" else "bin")


def scrub_outer_soldr_env(env: dict[str, str]) -> None:
    """Drop, in place, the outer setup-soldr variables from a smoke environment.

    Keeps the installed wheel under test independent of setup-soldr.
    """
    for name in OUTER_SOLDR_ENV:
        env.pop(name, None)


def run(
    args: list[str],
    *,
    cwd: Path | None = None,
    env: dict[str, str] | None = None,
    timeout: int | None = None,
) -> None:
    print(f"+ {' '.join(args)}", flush=True)
    subprocess.run(args, cwd=cwd, env=env, timeout=timeout, check=True)


def write_project(project: Path) -> None:
    (project / "src").mkdir(parents=True)
    (project / "rust-toolchain.toml").write_text(
        f"""\
[toolchain]
channel = "{os.environ.get("RUSTUP_TOOLCHAIN") or DEFAULT_RUST_TOOLCHAIN}"
profile = "minimal"
""",
        encoding="utf-8",
    )
    (project / "pyproject.toml").write_text(
        """\
[build-system]
requires = ["soldr"]
build-backend = "soldr"

[project]
name = "soldr-pep517-daemon-smoke"
version = "0.1.0"

[tool.maturin]
bindings = "bin"
manifest-path = "Cargo.toml"
""",
        encoding="utf-8",
    )
    (project / "Cargo.toml").write_text(
        """\
[package]
name = "soldr_pep517_daemon_smoke"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "soldr-pep517-daemon-smoke"
path = "src/main.rs"
""",
        encoding="utf-8",
    )
    (project / "src" / "main.rs").write_text(
        'fn main() { println!("soldr pep517 daemon smoke"); }\n',
        encoding="utf-8",
    )


def soldr_log_paths(cache_dir: Path) -> list[Path]:
    """Return only Soldr-owned diagnostics, excluding fetched tool payloads."""
    wanted_suffixes = {".log", ".jsonl", ".txt"}
    wanted_names = {"compile-daemon-unavailable", "daemon.pid"}
    log_roots = [
        cache_dir / "cache" / "soldr-daemon",
        cache_dir / "cache" / "zccache" / "logs",
        cache_dir / "cache" / "zccache" / "daemon-state",
        cache_dir / "cache" / "zccache" / "history",
        cache_dir / "logs",
        cache_dir / "runtime" / "soldr-daemon",
    ]
    candidates = [cache_dir / "daemon-spawn.log"]
    for root in log_roots:
        try:
            candidates.extend(root.rglob("*"))
        except OSError:
            continue

    seen: set[Path] = set()
    logs: list[Path] = []
    for path in candidates:
        try:
            resolved = path.resolve()
            if resolved in seen or not path.is_file():
                continue
            seen.add(resolved)
            if (
                path.suffix.lower() not in wanted_suffixes
                and path.name not in wanted_names
            ):
                continue
        except OSError:
            continue
        logs.append(path)
    return sorted(logs)


def print_soldr_logs(cache_dir: Path, *, stream: TextIO = sys.stdout) -> None:
    print(f"[pep517-smoke:logs] cache directory: {cache_dir}", file=stream, flush=True)
    try:
        if not cache_dir.exists():
            print(
                "[pep517-smoke:logs] cache directory does not exist",
                file=stream,
                flush=True,
            )
            return
    except OSError as exc:
        print(
            f"[pep517-smoke:logs] cache directory unreadable: {exc}",
            file=stream,
            flush=True,
        )
        return
    # Log dumping is best-effort diagnostics: after the running-process
    # v2 broker migration (#1501) the daemon's runtime dirs under the
    # cache can be permission-locked while the daemon is alive, and a
    # bare `exists()` / `rglob()` then raises WinError 5 *after* the
    # smoke has already succeeded (soldr#1509). Never let the log dump
    # fail the job.
    for path in soldr_log_paths(cache_dir):
        try:
            rel = path.relative_to(cache_dir)
            text = path.read_text(encoding="utf-8", errors="replace")
            size = path.stat().st_size
        except OSError as exc:
            print(
                f"[pep517-smoke:logs] {path}: unreadable: {exc}",
                file=stream,
                flush=True,
            )
            continue
        print(
            f"[pep517-smoke:daemon-log] {rel} ({size} bytes)",
            file=stream,
            flush=True,
        )
        print(text[-16_000:], file=stream, flush=True)


def print_broker_spawn_log(*, stream: TextIO = sys.stdout) -> None:
    """Expose detached broker startup errors alongside smoke diagnostics."""
    path = Path.home() / ".soldr" / "broker" / "broker-spawn.log"
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError as exc:
        print(
            f"[pep517-smoke:broker-log] unavailable ({path}): {exc}",
            file=stream,
            flush=True,
        )
        return
    print(
        f"[pep517-smoke:broker-log] {path} ({path.stat().st_size} bytes)",
        file=stream,
        flush=True,
    )
    print(text[-16_000:], file=stream, flush=True)


def failure_summary(error: BaseException, log_dir: Path | None) -> str:
    """Return one stable stderr line that points to the actionable diagnostics."""
    if isinstance(error, subprocess.CalledProcessError):
        command = " ".join(str(part) for part in error.cmd)
        detail = f"downstream pip wheel exited {error.returncode}: {command}"
    elif isinstance(error, subprocess.TimeoutExpired):
        detail = f"downstream pip wheel timed out after {error.timeout}s: {error.cmd}"
    else:
        detail = str(error)
    artifact = f" artifact directory: {log_dir}" if log_dir else ""
    return (
        f"[pep517-smoke:failure] {detail}. "
        "Search [pep517-smoke:daemon-log] and [pep517-smoke:broker-log]."
        f"{artifact}"
    )


def archive_soldr_logs(cache_dir: Path, log_dir: Path | None) -> None:
    """Copy the smoke's textual diagnostics before its temporary dir is removed."""
    if log_dir is None:
        return
    try:
        log_dir.mkdir(parents=True, exist_ok=True)
        for path in soldr_log_paths(cache_dir):
            destination = log_dir / "cache" / path.relative_to(cache_dir)
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, destination)

        broker_log = Path.home() / ".soldr" / "broker" / "broker-spawn.log"
        if broker_log.is_file():
            destination = log_dir / "broker" / broker_log.name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(broker_log, destination)
    except OSError as exc:
        # Artifact collection must never mask the smoke result.
        print(
            f"[pep517-smoke:logs] could not preserve Soldr logs: {exc}",
            file=sys.stderr,
            flush=True,
        )


def stop_soldr_daemon(soldr: Path, env: dict[str, str]) -> None:
    try:
        subprocess.run(
            [str(soldr), "daemon", "stop"],
            env=env,
            timeout=20,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        print(f"warning: best-effort `soldr daemon stop` failed: {exc}", flush=True)


def stop_soldr_broker(soldr: Path, env: dict[str, str]) -> None:
    # Routes are path-derived (soldr#2479), so the wheel-installed binary's
    # broker is the isolated one; no program name is needed to reach it.
    try:
        subprocess.run(
            [str(soldr), "broker", "stop"],
            env=env,
            timeout=20,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        print(f"warning: best-effort isolated broker stop failed: {exc}", flush=True)


def main() -> int:
    args = parse_args()
    wheel = resolve_wheel(args.wheel)
    with tempfile.TemporaryDirectory(
        prefix="soldr-pep517-smoke-",
        ignore_cleanup_errors=True,
    ) as tmp_str:
        tmp = Path(tmp_str)
        venv = tmp / "venv"
        project = tmp / "project"
        wheelhouse = tmp / "wheelhouse"
        cache_dir = tmp / "soldr-cache"
        target_dir = tmp / "target"

        run([sys.executable, "-m", "venv", str(venv)])
        python = venv_bin(venv) / ("python.exe" if os.name == "nt" else "python")
        run([str(python), "-m", "pip", "install", "--upgrade", "pip", "wheel"])
        run([str(python), "-m", "pip", "install", str(wheel)])
        soldr = venv_bin(venv) / ("soldr.exe" if os.name == "nt" else "soldr")
        run([str(soldr), "--version"])
        run([str(soldr), "zccache", "--version"])

        write_project(project)
        wheelhouse.mkdir()
        env = os.environ.copy()
        scrub_outer_soldr_env(env)
        env["PATH"] = str(venv_bin(venv)) + os.pathsep + env.get("PATH", "")
        env["SOLDR_DAEMON_REQUIRED"] = "1"
        env.setdefault("SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS", "5000")
        env["SOLDR_CACHE_DIR"] = str(cache_dir)
        env["CARGO_TARGET_DIR"] = str(target_dir)
        env["SOLDR_PEP517_STABLE_TARGET_DIR"] = "0"

        try:
            run(
                [
                    str(python),
                    "-m",
                    "pip",
                    "wheel",
                    "--no-build-isolation",
                    "--no-deps",
                    "--wheel-dir",
                    str(wheelhouse),
                    str(project),
                ],
                env=env,
                timeout=args.timeout,
            )
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as exc:
            stop_soldr_daemon(soldr, env)
            print(failure_summary(exc, args.log_dir), file=sys.stderr, flush=True)
            print_soldr_logs(cache_dir, stream=sys.stderr)
            print_broker_spawn_log(stream=sys.stderr)
            raise
        else:
            stop_soldr_daemon(soldr, env)
        finally:
            stop_soldr_broker(soldr, env)
            archive_soldr_logs(cache_dir, args.log_dir)

        built = sorted(wheelhouse.glob("soldr_pep517_daemon_smoke-*.whl"))
        if len(built) != 1:
            print_soldr_logs(cache_dir)
            raise SystemExit(f"expected one downstream wheel, found {built}")
        print(f"PEP 517 daemon smoke built {built[0].name}", flush=True)
        print_soldr_logs(cache_dir)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
