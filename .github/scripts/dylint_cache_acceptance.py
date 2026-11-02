#!/usr/bin/env python3
"""Run the real cargo-dylint 6.0.3 cache acceptance matrix in Docker/Linux."""

from __future__ import annotations

import io
import json
import os
import runpy
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import cast

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

ROOT = Path(__file__).resolve().parents[2]

BASH = r"""
set -euo pipefail
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE
export CARGO_HOME=/root/.cargo
export SOLDR_CACHE_DIR=/tmp/dylint-acceptance/cache
export SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS=120000
export SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS=1
rm -rf /tmp/dylint-acceptance
mkdir -p /tmp/dylint-acceptance/diagnostics
SOLDR=/target/debug/soldr
REPO="$(pwd)"
WATCHDOG_IDLE_SECS=180
WATCHDOG_ABSOLUTE_SECS=1800
WATCHDOG_POLL_SECS=10
WATCHDOG_INACTIVE_GRACE_SECS=60
WATCHDOG_POST_CAPTURE_SECS=300
# The external watchdog below is progress-aware. Keep soldr's wall-clock
# timeout disabled: a legitimate first catalogue fetch plus per-nightly driver
# build can exceed 15 minutes. The watchdog still captures and
# terminates a semantic stall, including a continuously busy-spin process.
export SOLDR_CARGO_WAIT_TIMEOUT_SECS=0
export SOLDR_DAEMON_TOKIO_CONSOLE=1
export SOLDR_DAEMON_TOKIO_CONSOLE_PUBLISH_INTERVAL_MS=20
export ZCCACHE_INNER_TRACE=/tmp/dylint-acceptance/diagnostics/context-registration-trace.jsonl
MODE="${SOLDR_DYLINT_ACCEPTANCE_MODE:-full}"

cp -a "$REPO/ci/fixtures/dylint-cache" /tmp/dylint-acceptance/a
git init -q /tmp/dylint-acceptance/a
git config --global --add safe.directory /tmp/dylint-acceptance/a
git -C /tmp/dylint-acceptance/a rev-parse --git-dir >/dev/null
git -C /tmp/dylint-acceptance/a config user.email fixture@soldr.invalid
git -C /tmp/dylint-acceptance/a config user.name "Soldr Fixture"
git -C /tmp/dylint-acceptance/a add .
git -C /tmp/dylint-acceptance/a commit -qm fixture
git -C /tmp/dylint-acceptance/a worktree add -q /tmp/dylint-acceptance/b HEAD

snapshot_zccache_logs() {
  name="$1"
  cache_root="$SOLDR_CACHE_DIR/cache/zccache"
  snapshot_dir="/tmp/dylint-acceptance/diagnostics/$name-zccache"
  mkdir -p "$snapshot_dir"
  test -d "$cache_root" || return 0
  (
    cd "$cache_root"
    find . -type f \
      \( -name 'compile_journal.jsonl*' \
         -o -name 'last-session*.json*' \
         -o -name 'last-session*.log*' \
         -o -name 'daemon.log*' \
         -o -name 'depgraph.bin' \) \
      -exec cp --parents -p '{}' "$snapshot_dir/" ';'
  )
}

snapshot_soldr_daemon_logs() {
  name="$1"
  snapshot_dir="/tmp/dylint-acceptance/diagnostics/$name-soldr-daemon"
  mkdir -p "$snapshot_dir"
  for source in \
    "$SOLDR_CACHE_DIR/daemon-spawn.log" \
    "$SOLDR_CACHE_DIR/cache/soldr-daemon/lifecycle.jsonl" \
    "$SOLDR_CACHE_DIR/cache/soldr-daemon/daemon.log" \
    "$SOLDR_CACHE_DIR/cache/soldr-daemon/daemon.pid"; do
    test -f "$source" || continue
    cp -p "$source" "$snapshot_dir/$(basename "$source")"
  done
}

run_case() {
  name="$1"; work="$2"; target="$3"
  emit_stats="${4:-1}"
  live_log="/tmp/dylint-acceptance/diagnostics/$name-live.log"
  : >"$live_log"
  start="$(date +%s%3N)"
  (
    cd "$work"
    CARGO_TARGET_DIR="$target" \
      SOLDR_DAEMON_TOKIO_CONSOLE_RECORD_PATH="/tmp/dylint-acceptance/diagnostics/$name.tokio" \
      "$SOLDR" cargo dylint --all 2>&1 | tee -a "$live_log"
  ) &
  command_pid="$!"
  (
    last_progress="$(meaningful_output_size "$live_log")"
    read -r last_cpu last_io last_target last_pids \
      < <(process_activity_counters "$command_pid" "$target")
    idle_secs=0
    elapsed_secs=0
    inactive_secs=0
    post_capture_secs=0
    captured=0
    while kill -0 "$command_pid" 2>/dev/null; do
      sleep "$WATCHDOG_POLL_SECS"
      elapsed_secs="$((elapsed_secs + WATCHDOG_POLL_SECS))"
      current_progress="$(meaningful_output_size "$live_log")"
      read -r current_cpu current_io current_target current_pids \
        < <(process_activity_counters "$command_pid" "$target")
      cpu_delta="$((current_cpu - last_cpu))"
      io_delta="$((current_io - last_io))"
      semantic_progress=0
      # CPU/I/O is deliberately not semantic progress: a busy-spin hang
      # must still trigger a stack/Tokio snapshot. Output, artifacts, and
      # process-phase changes are the signals that postpone capture.
      [[ "$current_progress" == "$last_progress" &&
         "$current_target" == "$last_target" &&
         "$current_pids" == "$last_pids" ]] || semantic_progress=1
      process_active=0
      [[ "$cpu_delta" -lt 1000000000 &&
         "$io_delta" -lt 8388608 ]] || process_active=1
      last_progress="$current_progress"
      last_cpu="$current_cpu"
      last_io="$current_io"
      last_target="$current_target"
      last_pids="$current_pids"

      if [[ "$semantic_progress" -eq 1 ]]; then
        idle_secs=0
        inactive_secs=0
        post_capture_secs=0
        captured=0
      else
        idle_secs="$((idle_secs + WATCHDOG_POLL_SECS))"
      fi

      if [[ "$captured" -eq 1 ]]; then
        # Activity protects a healthy, quiet compiler from immediate
        # termination, but only for a bounded post-capture grace period.
        post_capture_secs="$((post_capture_secs + WATCHDOG_POLL_SECS))"
        if [[ "$process_active" -eq 1 ]]; then
          inactive_secs=0
        else
          inactive_secs="$((inactive_secs + WATCHDOG_POLL_SECS))"
        fi
        if [[ "$inactive_secs" -ge "$WATCHDOG_INACTIVE_GRACE_SECS" ||
              "$post_capture_secs" -ge "$WATCHDOG_POST_CAPTURE_SECS" ]]; then
          terminate_scope "$command_pid"
          break
        fi
        continue
      fi
      absolute_deadline=0
      if [[ "$elapsed_secs" -ge "$WATCHDOG_ABSOLUTE_SECS" ]]; then
        absolute_deadline=1
        trigger="exceeded the ${WATCHDOG_ABSOLUTE_SECS}s absolute case deadline"
      elif [[ "$idle_secs" -ge "$WATCHDOG_IDLE_SECS" ]]; then
        trigger="produced no meaningful output for ${WATCHDOG_IDLE_SECS}s"
      else
        continue
      fi

      dump="/tmp/dylint-acceptance/diagnostics/$name-stacks.txt"
      fired="/tmp/dylint-acceptance/diagnostics/$name-watchdog-fired"
      : >"$fired"
      {
        echo "WATCHDOG: $name $trigger"
        date -u
        echo "=== process tree ==="
        ps -eo pid,ppid,pgid,stat,etimes,wchan:32,args --forest
        echo "=== native stacks ==="
        pids=()
        mapfile -t descendants < <(descendant_pids "$command_pid")
        daemon_pid="$(verified_daemon_pid || true)"
        candidates=("$command_pid" "${descendants[@]}")
        [[ -z "$daemon_pid" ]] || candidates+=("$daemon_pid")
        for pid in "${candidates[@]}"; do
          test -r "/proc/$pid/cmdline" || continue
          command_line="$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)"
          case "$command_line" in
            *"/target/debug/soldr"*|*"cargo build"*|*"cargo-dylint"*|*"dylint-driver"*|*"rustc"*|*"zccache"*)
              pids+=("$pid")
              ;;
          esac
        done
        printf 'scoped pids: %s\n' "${pids[*]:-(none)}"
        export -f dump_one_pid
        timeout 120s bash -c '
          for pid in "$@"; do
            dump_one_pid "$pid"
          done
        ' bash "${pids[@]}" || echo "WATCHDOG: native stack collection hit its 120s global budget"
      } >"$dump" 2>&1
      cat "$dump" >&2
      if [[ "$absolute_deadline" -eq 1 ]]; then
        terminate_scope "$command_pid"
        break
      fi
      captured=1
      inactive_secs=0
      post_capture_secs=0
    done
  ) &
  watchdog_pid="$!"
  set +e
  wait "$command_pid"
  status="$?"
  set -e
  fired="/tmp/dylint-acceptance/diagnostics/$name-watchdog-fired"
  if [[ -e "$fired" ]]; then
    wait "$watchdog_pid" 2>/dev/null || true
  else
    kill "$watchdog_pid" 2>/dev/null || true
    wait "$watchdog_pid" 2>/dev/null || true
  fi
  snapshot_zccache_logs "$name"
  snapshot_soldr_daemon_logs "$name"
  if [[ "$status" -ne 0 ]]; then
    echo "Dylint library target contents after failure:" >&2
    find "$target/dylint/libraries" -maxdepth 5 -type f -print 2>/dev/null | sort >&2 || true
    return "$status"
  fi
  if [[ "$emit_stats" != 1 ]]; then
    return 0
  fi
  end="$(date +%s%3N)"
  # The Cargo front door finalizes session stats before returning. Its
  # command-lifetime daemon may already be stopped here, so an additional
  # `cache flush` would turn the valid NotRunning state into a harness error.
  (cd "$work" && "$SOLDR" cache report --json) > "/tmp/dylint-acceptance/$name.json"
  jq -cn --arg name "$name" --argjson wall_ms "$((end-start))" \
    --slurpfile report "/tmp/dylint-acceptance/$name.json" \
    '{name:$name,wall_ms:$wall_ms,
      stats_present:($report[0].session_stats_present == true and
        ($report[0].last_session | type) == "object"),
      hits:($report[0].last_session.stats.hits // $report[0].last_session.hits // 0),
      misses:($report[0].last_session.stats.misses // $report[0].last_session.misses // 0)}'
}

meaningful_output_size() {
  # soldr's once-per-minute heartbeat proves that its parent wait loop is
  # alive, but it must not mask a child that has stopped making progress.
  awk '
    !index($0, "soldr: cargo diagnostic capture still running") {
      bytes += length($0) + 1
    }
    END { print bytes + 0 }
  ' "$1"
}

descendant_pids() {
  parent="$1"
  while read -r child; do
    [[ -n "$child" ]] || continue
    printf '%s\n' "$child"
    descendant_pids "$child"
  done < <(pgrep -P "$parent" 2>/dev/null || true)
}

verified_daemon_pid() {
  pid_file="$SOLDR_CACHE_DIR/cache/soldr-daemon/daemon.pid"
  [[ -r "$pid_file" ]] || return 1
  read -r pid <"$pid_file"
  [[ "$pid" =~ ^[0-9]+$ && -r "/proc/$pid/environ" ]] || return 1
  exe="$(readlink -f "/proc/$pid/exe" 2>/dev/null || true)"
  stem="$(basename "$exe")"
  case "$stem" in
    soldr|soldr-daemon) ;;
    *) return 1 ;;
  esac
  tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null |
    grep -Fxq "SOLDR_CACHE_DIR=$SOLDR_CACHE_DIR" || return 1
  printf '%s\n' "$pid"
}

scoped_pids() {
  root="$1"
  kill -0 "$root" 2>/dev/null && printf '%s\n' "$root"
  descendant_pids "$root"
  verified_daemon_pid || true
}

process_activity_counters() {
  root="$1"
  target="$2"
  cpu=0
  io=0
  pid_list=""
  while read -r pid; do
    [[ -n "$pid" && -r "/proc/$pid/schedstat" ]] || continue
    read -r runtime _ <"/proc/$pid/schedstat" || continue
    bytes="$(awk '/^(read_bytes|write_bytes):/ { bytes += $2 } END { print bytes + 0 }' \
      "/proc/$pid/io" 2>/dev/null || printf '0\n')"
    cpu="$((cpu + runtime))"
    io="$((io + bytes))"
    pid_list="${pid_list},${pid}"
  done < <(scoped_pids "$root" | sort -n -u)
  target_state="-"
  if [[ -d "$target" ]]; then
    target_state="$(
      find "$target" -type f -printf '%T@:%s\n' 2>/dev/null |
        sort -n | tail -n 1
    )"
  fi
  printf '%s %s %s %s\n' \
    "$cpu" "$io" "${target_state:--}" "${pid_list:--}"
}

terminate_scope() {
  root="$1"
  mapfile -t scoped < <(scoped_pids "$root" | sort -rn -u)
  ((${#scoped[@]} == 0)) || kill -TERM "${scoped[@]}" 2>/dev/null || true
  for _ in {1..5}; do
    sleep 1
    mapfile -t survivors < <(scoped_pids "$root" | sort -rn -u)
    ((${#survivors[@]} == 0)) && return 0
  done
  # Re-scan after the grace period so children created during TERM cannot
  # escape. Every PID is either rooted at this command or is the daemon
  # verified against this acceptance cache and executable identity.
  mapfile -t survivors < <(scoped_pids "$root" | sort -rn -u)
  ((${#survivors[@]} == 0)) || kill -KILL "${survivors[@]}" 2>/dev/null || true
}

dump_one_pid() {
  pid="$1"
  test -r "/proc/$pid/status" || return 0
  echo "--- pid=$pid exe=$(readlink -f "/proc/$pid/exe" 2>/dev/null || true) ---"
  timeout 12s gdb -q -n -batch \
    -ex "set pagination off" \
    -ex "set print thread-events off" \
    -ex "info threads" \
    -ex "thread apply all bt full 64" \
    -p "$pid" 2>&1 || true
}

run_symbolized_watchdog_smoke() {
  fake_cargo="/tmp/dylint-acceptance/hanging-cargo"
  dump="/tmp/dylint-acceptance/diagnostics/watchdog-symbol-smoke-stacks.txt"
  cat >"$fake_cargo" <<'EOF'
#!/bin/sh
echo "watchdog symbol smoke: deliberately waiting" >&2
exec sleep 120
EOF
  chmod +x "$fake_cargo"

  (
    cd /tmp/dylint-acceptance/a
    exec env \
      SOLDR_TEST_CARGO_BIN="$fake_cargo" \
      SOLDR_CARGO_WAIT_TIMEOUT_SECS=0 \
      "$SOLDR" cargo metadata >/dev/null 2>&1
  ) &
  command_pid="$!"
  (
    # This is a deliberately short watchdog deadline for the symbol smoke.
    # The real acceptance cases retain the three-minute semantic-idle bound.
    sleep 5
    {
      echo "WATCHDOG SYMBOL SMOKE: pid=$command_pid"
      dump_one_pid "$command_pid"
    } >"$dump" 2>&1
    terminate_scope "$command_pid"
  ) &
  smoke_watchdog_pid="$!"
  set +e
  wait "$command_pid"
  set -e
  wait "$smoke_watchdog_pid"

  # Prove that ptrace worked and the deliberately unstripped Soldr binary
  # resolved both Rust function names and source line information.
  grep -Fq 'exe=/target/debug/soldr' "$dump"
  grep -Eq 'soldr_(cli|daemon|core)::' "$dump"
  grep -Eq 'crates/soldr-(cli|daemon|core)/src/[^ ]*\.rs:[0-9]+' "$dump"
  touch /tmp/dylint-acceptance/diagnostics/watchdog-symbol-smoke-passed
}

hash_libraries() {
  name="$1"
  target="$2"
  find "$target/dylint/libraries" -type f \
    \( -name '*.so' -o -name '*.dylib' -o -name '*.dll' \) \
    -print0 2>/dev/null |
    sort -z |
    while IFS= read -r -d '' library; do
      printf 'DYLINT_LIBRARY_HASH %s %s\n' \
        "$name" "$(sha256sum "$library")"
    done
}

# The cold case intentionally owns first-time cargo-dylint and driver
# preparation so the same watchdog covers tool bootstrap as well as linting.
# Keep target directories beneath their worktree roots. zccache deliberately
# normalizes paths inside each root; arbitrary external target directories
# are distinct user-selected paths and therefore are not cross-worktree keys.
run_case cold /tmp/dylint-acceptance/a /tmp/dylint-acceptance/a/target
# The symbol smoke is killed after a few seconds by design. Run it only once
# the cold case has fully acquired the pinned toolchain: killing the first
# soldr invocation interrupted `rustup toolchain install`, leaving a nightly
# with no channel manifest that every later case then failed on.
if [[ "$MODE" == "full" ]]; then
  run_symbolized_watchdog_smoke
fi
hash_libraries cold /tmp/dylint-acceptance/a/target
if [[ "$MODE" == "sibling-diagnostic" ]]; then
  run_case sibling_worktree /tmp/dylint-acceptance/b /tmp/dylint-acceptance/b/target
  hash_libraries sibling_worktree /tmp/dylint-acceptance/b/target
else
  run_case warm_same_target /tmp/dylint-acceptance/a /tmp/dylint-acceptance/a/target
  rm -rf /tmp/dylint-acceptance/a/target
  run_case warm_clean_target /tmp/dylint-acceptance/a /tmp/dylint-acceptance/a/target
  hash_libraries warm_clean_target /tmp/dylint-acceptance/a/target
  run_case sibling_worktree /tmp/dylint-acceptance/b /tmp/dylint-acceptance/b/target
  hash_libraries sibling_worktree /tmp/dylint-acceptance/b/target
  printf '\npub fn changed_source() -> usize { 7 }\n' >> /tmp/dylint-acceptance/b/src/lib.rs
  run_case changed_source /tmp/dylint-acceptance/b /tmp/dylint-acceptance/b/target

  rm -rf /tmp/dylint-acceptance/target-diagnostic
  printf '\npub fn dylint_fixture_violation() {}\n' \
    >> /tmp/dylint-acceptance/a/src/lib.rs
  for pass in cold replay; do
    name="diagnostic-$pass"
    output="/tmp/dylint-acceptance/diagnostics/$name-live.log"
    run_case "$name" /tmp/dylint-acceptance/a \
      /tmp/dylint-acceptance/target-diagnostic 0
    grep -F "soldr Dylint fixture diagnostic" "$output" >/dev/null
    rm -rf /tmp/dylint-acceptance/target-diagnostic
  done
fi
"""


def main() -> int:  # noqa: C901
    mode = os.environ.get("SOLDR_DYLINT_ACCEPTANCE_MODE", "full")
    if mode not in {"full", "sibling-diagnostic"}:
        print(f"error: unsupported Dylint acceptance mode: {mode}", file=sys.stderr)
        return 5
    common_dir = run_captured(
        ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    common = Path(common_dir.stdout.strip()).resolve()
    source_root = (
        common.parent if common_dir.returncode == 0 and common.name == ".git" else ROOT
    )
    perf_local = runpy.run_path(str(ROOT / "ci" / "perf_local.py"))
    runner = perf_local["runner_for"](source_root)
    runner_container = runner.container
    relative = ROOT.resolve().relative_to(source_root.resolve())
    workdir = "/repo" if relative == Path(".") else f"/repo/{relative.as_posix()}"
    bootstrap = subprocess.run(
        [
            sys.executable,
            str(ROOT / "ci" / "perf_local.py"),
            "cargo",
            "--config",
            'build.rustflags=["--cfg","tokio_unstable"]',
            "--config",
            "profile.dev.debug=2",
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
    if bootstrap.returncode != 0:
        return bootstrap.returncode
    command = [
        "docker",
        "exec",
        "-i",
        "-e",
        f"SOLDR_DYLINT_ACCEPTANCE_MODE={mode}",
        "-w",
        workdir,
        runner_container,
        "bash",
        "-s",
    ]
    diagnostics = (
        Path(os.environ.get("RUNNER_TEMP", tempfile.gettempdir()))
        / "soldr-dylint-diagnostics"
    )
    shutil.rmtree(diagnostics, ignore_errors=True)
    diagnostics.mkdir(parents=True, exist_ok=True)
    try:
        output_lines: list[str] = []
        with subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
        ) as process:
            assert process.stdin is not None
            assert process.stdout is not None
            # `TextIOWrapper` translates `\n` to CRLF on a Windows caller.
            # Bash receives this recipe over stdin inside the Linux runner, so
            # write UTF-8 bytes directly and preserve Unix line endings.
            stdin = cast(io.TextIOWrapper, process.stdin)
            stdin.buffer.write(BASH.encode())
            stdin.buffer.flush()
            process.stdin.close()
            for line in process.stdout:
                output_lines.append(line)
                print(line, end="", flush=True)
            returncode = process.wait()
        if returncode != 0:
            return returncode
        rows = []
        for line in output_lines:
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            if (
                isinstance(row, dict)
                and {
                    "name",
                    "wall_ms",
                    "stats_present",
                    "hits",
                    "misses",
                }
                <= row.keys()
            ):
                rows.append(row)
        expected = (
            ["cold", "sibling_worktree"]
            if mode == "sibling-diagnostic"
            else [
                "cold",
                "warm_same_target",
                "warm_clean_target",
                "sibling_worktree",
                "changed_source",
            ]
        )
        if [row["name"] for row in rows] != expected:
            print(f"error: incomplete scenario output: {rows}", file=sys.stderr)
            return 2
        by_name = {row["name"]: row for row in rows}
        checks = [
            (
                all(
                    row["stats_present"]
                    and isinstance(row["hits"], int)
                    and isinstance(row["misses"], int)
                    for row in rows
                ),
                "every scenario must have integer session stats",
            ),
            (by_name["cold"]["misses"] > 0, "cold run must report misses"),
            (by_name["sibling_worktree"]["hits"] > 0, "sibling worktree must hit"),
        ]
        if mode == "full":
            checks.extend(
                [
                    (
                        by_name["warm_clean_target"]["hits"] > 0,
                        "clean-target rebuild must hit",
                    ),
                    (
                        by_name["changed_source"]["misses"] > 0,
                        "changed source must miss changed units",
                    ),
                    # A5 gate (soldr#2436 phase 6 / soldr#2645): a second
                    # identical run must compile under 20% of what the cold
                    # run compiled — the #2435 pathology was exactly a warm
                    # run recompiling the world. A fully-fresh second run
                    # (zero wrapper invocations) passes trivially, which is
                    # correct: no compiles is the best possible warm result;
                    # the warm-clean-target hits>0 check above is what
                    # proves the cache actually serves.
                    (
                        by_name["warm_same_target"]["misses"] * 5
                        < by_name["cold"]["misses"],
                        "second identical run must compile <20% of the cold run",
                    ),
                ]
            )
        for passed, message in checks:
            if not passed:
                print(f"error: {message}: {rows}", file=sys.stderr)
                return 3
        summary = os.environ.get("GITHUB_STEP_SUMMARY")
        if summary:
            with open(summary, "a", encoding="utf-8") as output:
                output.write("## Dylint 6.0.3 cache acceptance\n\n")
                output.write(
                    "| Scenario | Wall ms | Hits | Misses |\n|---|---:|---:|---:|\n"
                )
                for row in rows:
                    output.write(
                        f"| {row['name']} | {row['wall_ms']} | {row['hits']} | {row['misses']} |\n"
                    )
        return 0
    except OSError as error:
        print(f"error: failed to execute Docker acceptance: {error}", file=sys.stderr)
        return 4
    finally:
        copied = run_captured(
            [
                "docker",
                "cp",
                f"{runner_container}:/tmp/dylint-acceptance/diagnostics/.",
                str(diagnostics),
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        if copied.returncode != 0 and "No such container" not in copied.stderr:
            print(
                f"warning: failed to copy watchdog diagnostics: {copied.stderr.strip()}",
                file=sys.stderr,
            )
        run_captured(
            [
                "docker",
                "exec",
                runner_container,
                "rm",
                "-rf",
                "/tmp/dylint-acceptance",
            ],
            capture_output=True,
            check=False,
        )


if __name__ == "__main__":
    sys.exit(main())
