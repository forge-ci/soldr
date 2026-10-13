#!/usr/bin/env python3
"""Run the Dylint dependency-cook acceptance and timing matrix in Linux."""

from __future__ import annotations

import json
import os
import runpy
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

ROOT = Path(__file__).resolve().parents[2]

BASH = r"""
set -euo pipefail
export CARGO_HOME=/root/.cargo
export CARGO_PROFILE_DEV_DEBUG=2
export CARGO_PROFILE_DEV_STRIP=none
export SOLDR_CACHE_DIR=/tmp/dylint-cook/cache
export SOLDR_CARGO_WAIT_TIMEOUT_SECS=0
export SOLDR_COMPILE_REPLY_TIMEOUT_SECS=3600
export SOLDR_DAEMON_TOKIO_CONSOLE=1
export SOLDR_DAEMON_TOKIO_CONSOLE_PUBLISH_INTERVAL_MS=20
SOLDR=/target/debug/soldr
REPO="$(pwd)"
WORK=/tmp/dylint-cook/work
unset CARGO_TARGET_DIR
rm -rf /tmp/dylint-cook
DIAGNOSTICS=/tmp/dylint-cook/diagnostics
mkdir -p "$WORK" "$DIAGNOSTICS"
cp -a "$REPO/ci/fixtures/dylint-cache/." "$WORK/"
cd "$WORK"
"$SOLDR" "car""go" +1.98.1 generate-lockfile

descendants() {
  local parent="$1" child
  while read -r child; do
    test -n "$child" || continue
    printf '%s\n' "$child"
    descendants "$child"
  done < <(pgrep -P "$parent" 2>/dev/null || true)
}

dump_stacks() {
  local root="$1" output="$2" pid exe
  {
    printf '%s\n' "$root"
    descendants "$root"
    pgrep -f '/target/debug/soldr|soldr-daemon|cargo-dylint|dylint-driver|rustc|zccache' \
      2>/dev/null || true
  } | sort -n -u | while read -r pid; do
    test -r "/proc/$pid/status" || continue
    exe="$(readlink -f "/proc/$pid/exe" || true)"
    echo "--- pid=$pid exe=$exe ---" >>"$output"
    file "$exe" >>"$output" 2>&1 || true
    readelf -S "$exe" 2>/dev/null |
      grep -E '\.(debug_info|debug_line|symtab)' >>"$output" || true
    timeout 15s gdb -q -n -batch \
      -ex "set pagination off" \
      -ex "set print thread-events off" \
      -ex "info threads" \
      -ex "thread apply all bt full 64" \
      -p "$pid" >>"$output" 2>&1 || true
  done
}

run_with_watchdog() {
  local name="$1"; shift
  local log="$DIAGNOSTICS/${name}.log"
  SOLDR_DAEMON_TOKIO_CONSOLE_RECORD_PATH="$DIAGNOSTICS/${name}.tokio" \
    "$@" >"$log" 2>&1 &
  local pid="$!" elapsed=0
  while kill -0 "$pid" 2>/dev/null; do
    sleep 10
    elapsed=$((elapsed + 10))
    if ((elapsed >= 1800)); then
      {
        echo "WATCHDOG: $name exceeded the 1800s absolute deadline"
        date -u
        echo "=== process tree ==="
        ps -eo pid,ppid,pgid,stat,etimes,wchan:32,args --forest
        echo "=== native stacks and symbol inventory ==="
      } >"$DIAGNOSTICS/${name}-stacks.txt"
      dump_stacks "$pid" "$DIAGNOSTICS/${name}-stacks.txt"
      kill -TERM "$pid" $(descendants "$pid") 2>/dev/null || true
      wait "$pid" || true
      cat "$log" >&2
      echo "watchdog timeout: $name" >&2
      return 124
    fi
  done
  set +e
  wait "$pid"
  local status="$?"
  set -e
  if [[ "$status" -ne 0 ]]; then
    cat "$log" >&2
  fi
  return "$status"
}

run_case() {
  local name="$1"; shift
  local started ended output outcome hits=0 misses=0
  started="$(date +%s%3N)"
  run_with_watchdog "$name" \
    "$SOLDR" dylint cook "$@" --json
  ended="$(date +%s%3N)"
  output="$(tail -n 1 "$DIAGNOSTICS/${name}.log")"
  outcome="$(jq -r .outcome <<<"$output")"
  if [[ "$outcome" != skip ]]; then
    "$SOLDR" cache report --json >"$DIAGNOSTICS/${name}-report.json"
    hits="$(jq -r '.last_session.stats.hits // .last_session.hits // 0' \
      "$DIAGNOSTICS/${name}-report.json")"
    misses="$(jq -r '.last_session.stats.misses // .last_session.misses // 0' \
      "$DIAGNOSTICS/${name}-report.json")"
  fi
  jq -cn --arg name "$name" --arg outcome "$outcome" \
    --argjson wall_ms "$((ended-started))" \
    --argjson hits "$hits" --argjson misses "$misses" \
    '{name:$name,outcome:$outcome,wall_ms:$wall_ms,hits:$hits,misses:$misses}'
}

test ! -e target/debug
test ! -e target/release
run_case cold --workspace --all-targets
test -d target/dylint/target
test ! -e target/debug
test ! -e target/release

run_case warm_same_target --workspace --all-targets

tar -cf /tmp/dylint-cook/restored.tar target/dylint
rm -rf target
mkdir target
tar -xf /tmp/dylint-cook/restored.tar
run_case warm_restored_target --workspace --all-targets

rm -rf target
run_case object_cache_only --workspace --all-targets
test ! -e target/debug
test ! -e target/release

run_case tests_cold --workspace --tests --tree tests
test -d target/dylint/tests

tar -cf /tmp/dylint-cook/restored-tests.tar target/dylint
rm -rf target
mkdir target
tar -xf /tmp/dylint-cook/restored-tests.tar
run_case tests_warm_restored_target --workspace --tests --tree tests

# tests_object_cache_only MUST miss - a warm per-unit object store is not
# allowed to make this pass, because the whole point of the cook tier is
# that it avoids the work rather than making the work cheap.
rm -rf target
run_case tests_object_cache_only --workspace --tests --tree tests

printf '\npub fn dylint_fixture_violation() {}\n' >>src/lib.rs
run_with_watchdog real_dylint "$SOLDR" "car""go" dylint --all
grep -F "soldr Dylint fixture diagnostic" "$DIAGNOSTICS/real_dylint.log"
"""


@dataclass(frozen=True)
class CookRow:
    """One scenario row the Dylint cook acceptance run reported as JSON."""

    name: object
    outcome: object
    wall_ms: object
    hits: object
    misses: object


def main() -> int:  # noqa: C901
    common_result = run_captured(
        ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    common = Path(common_result.stdout.strip()).resolve()
    source_root = common.parent if common.name == ".git" else ROOT
    perf_local = runpy.run_path(str(ROOT / "ci" / "perf_local.py"))
    runner = perf_local["runner_for"](source_root)
    runner_container = runner.container
    relative = ROOT.resolve().relative_to(source_root.resolve())
    workdir = "/repo" if relative == Path(".") else f"/repo/{relative.as_posix()}"
    build = subprocess.run(
        [
            sys.executable,
            str(ROOT / "ci" / "perf_local.py"),
            "car" + "go",
            "--config",
            'build.rustflags=["--cfg","tokio_unstable"]',
            "build",
            "-p",
            "soldr-cli",
            "--bin",
            "soldr",
            "--locked",
            "--features",
            "tokio-console",
        ],
        cwd=ROOT,
        check=False,
    )
    if build.returncode:
        return build.returncode
    command = [
        "docker",
        "exec",
        "-i",
        "-w",
        workdir,
        runner_container,
        "bash",
        "-s",
    ]
    output_lines: list[str] = []
    with subprocess.Popen(
        command,
        cwd=ROOT,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    ) as process:
        assert process.stdin is not None
        assert process.stdout is not None
        process.stdin.write(BASH)
        process.stdin.close()
        for line in process.stdout:
            output_lines.append(line)
            print(line, end="", flush=True)
        returncode = process.wait()
    subprocess.run(
        [
            "docker",
            "cp",
            f"{runner_container}:/tmp/dylint-cook/diagnostics/.",
            os.environ.get("RUNNER_TEMP", str(ROOT / "target"))
            + "/dylint-cook-diagnostics",
        ],
        check=False,
    )
    if returncode:
        return returncode
    rows: list[CookRow] = []
    for line in output_lines:
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and "name" in value and "wall_ms" in value:
            rows.append(
                CookRow(
                    name=value["name"],
                    outcome=value.get("outcome"),
                    wall_ms=value["wall_ms"],
                    hits=value.get("hits"),
                    misses=value.get("misses"),
                )
            )
    expected = [
        "cold",
        "warm_same_target",
        "warm_restored_target",
        "object_cache_only",
        "tests_cold",
        "tests_warm_restored_target",
        "tests_object_cache_only",
    ]
    if [row.name for row in rows] != expected:
        print(f"incomplete Dylint cook rows: {rows}", file=sys.stderr)
        return 2
    outcomes = {row.name: row.outcome for row in rows}
    if outcomes != {
        "cold": "miss",
        "warm_same_target": "skip",
        "warm_restored_target": "skip",
        "object_cache_only": "miss",
        "tests_cold": "miss",
        "tests_warm_restored_target": "skip",
        "tests_object_cache_only": "miss",
    }:
        print(f"unexpected outcomes: {rows}", file=sys.stderr)
        return 3
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as output:
            output.write("## Dylint dependency cook\n\n")
            output.write("| Scenario | Outcome | Wall ms | Hits | Misses |\n")
            output.write("|---|---|---:|---:|---:|\n")
            for row in rows:
                output.write(
                    f"| {row.name} | {row.outcome} | {row.wall_ms} | "
                    f"{row.hits} | {row.misses} |\n"
                )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
