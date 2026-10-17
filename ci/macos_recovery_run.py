#!/usr/bin/env python3
"""Per-PR macOS x86_64 execution lane, hosted in a Recovery guest (soldr#3076/#3078).

`e2e-macos-x64` in `ci.yml` cross-builds `x86_64-apple-darwin` on Linux (via
`_ci-cross-build-linux.yml`) and replays the packaged nextest archive inside a
`zackees/docker-mac-x64` Recovery guest -- a real x86_64 macOS **Recovery**
guest, no baked image, no secret, no ssh (soldr#3076). Recovery boots fresh
per action invocation, runs exactly one script (typed into a GUI Terminal and
fetched back over HTTP), and `/tmp` is a ramdisk that nothing survives past
that one boot. There is no toolchain to provision ahead of time and no room
for the decompressed nextest archive on the ramdisk -- but Recovery *can*
format and mount the action's blank 64 GB qcow2 disk, which gives the guest
script a real, persistent-for-the-boot volume to extract onto and provision a
managed rustup toolchain under (soldr#3078).

So this lane replays the same positively-owned host-sensitive nextest
partition every other `target-run` lane replays (`_ci-target-run.yml`'s "Run
owned pre-built native tests" step), just packed into one guest script
instead of a dozen workflow steps, because Recovery has no per-command exec:

    emit-guest-script --output PATH [--partition hash:K/N]
        Write the bash-3.2/POSIX-sh-compatible script the guest runs. The
        partition is validated before it is embedded in the guest script.

    verify-collected --collected DIR --guest-exit-code CODE [--manifest ...]
        Read the guest's collected results (the action's `collect` tarball,
        already extracted by the workflow) plus its `exit-code` output, and
        fail with a named diagnostic per check. When `--manifest`,
        `--repo-root`, and `--target` are given, also runs the ownership
        inventory validation and the coverage summary check against the
        `all-list.json` / `list.json` / `junit.xml` the guest produced --
        the same two checks the native `target-run` path runs, just deferred
        to after collection because Recovery has no Linux-side inventory to
        validate the filter against before it boots (see
        `target_run_ownership.build_filter_expression`).

Guest facts this script is built around (see the PR description for the
source): Ventura Recovery, root, one script per boot; `sh`/`bash 3.2`,
`curl`, `tar` (bsdtar, no zstd); the action's blank qcow2 disk can be
formatted with `diskutil eraseDisk APFS Work <disk>` and mounted at
`/Volumes/Work`, falling back to `/tmp/work` (ramdisk) if that fails;
`cargo-nextest` decodes `.tar.zst` itself, so only the workspace and fixtures
tarballs need to be plain, uncompressed `.tar` for bsdtar to extract.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import asdict, dataclass
from pathlib import Path

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

GUEST_HTTP_BASE = "http://10.0.2.2:8000"
RESULTS_FILE = "summary.txt"
# The action attaches a blank 64 GiB qcow2 as the guest hard disk (docker-mac-x64
# action.yml: `qemu-img create -f qcow2 ... 64G`). `diskutil list` never prints
# the media model, so the disk is picked by size: the only whole disk at or
# above this floor. The Base System image (~2 GB) and OpenCore (~1 GB) sit far
# below it.
MIN_SCRATCH_DISK_BYTES = 60 * 1000 * 1000 * 1000

# soldr#3076/#3078: every stage this lane's guest script runs, in the order
# it runs them. The first four are the original binary-only smoke (kept as
# the first stage); everything after `storage` is the archive replay added
# by soldr#3078. `verify_collected` treats this generically -- it only needs
# every name here to appear as a `pass` line in the guest's `summary.txt` --
# so the guest script is the single place that has to stay in sync with it.
CHECKS = (
    "arch",
    "fetch_soldr",
    "version",
    "help",
    "storage",
    "fetch_soldr-daemon",
    "fetch_cargo-nextest",
    "fetch_tests.tar.zst",
    "fetch_fixtures.tar",
    "fetch_workspace.tar",
    "fetch_filter.txt",
    "fetch_nextest-version.txt",
    "extract_workspace",
    "extract_fixtures",
    "toolchain_ensure",
    "toolchain_link",
    "nextest_version",
    "nextest_list_all",
    "native_wrapper",
    "nextest_list_selected",
    "nextest_run",
)

# Share-dir file names the Linux-side prep step must populate before the
# guest boots (soldr#3078). "soldr" is fetched by the first stage instead --
# it is needed before `storage` even runs, and there is no reason to fetch it
# twice into two locations for the same boot.
REPLAY_SHARE_FILES = (
    "soldr-daemon",
    "cargo-nextest",
    "tests.tar.zst",
    "fixtures.tar",
    "workspace.tar",
    "filter.txt",
    "nextest-version.txt",
)

# ~2 MiB: enough of `nextest run`'s tail to diagnose a failure without
# ballooning the diagnostics artifact (the collected directory is uploaded
# whole; the extracted nextest archive itself is never copied there).
NEXTEST_LOG_TAIL_BYTES = 2_000_000


@dataclass(frozen=True)
class Readiness:
    """Whether the executor may run by default, and what still blocks it."""

    default_enabled: bool
    blocked_by: tuple[str, ...]
    unavailable_action: str


@dataclass(frozen=True)
class ReuseArguments:
    """The preferred and fallback nextest archive-reuse argument lists."""

    preferred: tuple[str, ...]
    fallback: tuple[str, ...]


@dataclass(frozen=True)
class SelectionFilter:
    """Where the nextest filter expression comes from and how it is passed."""

    source_file: str
    expression_variable: str
    argument: str


@dataclass(frozen=True)
class NextestContract:
    """The nextest invocations the guest replays."""

    argv_prefix: tuple[str, ...]
    archive: str
    inventory_arguments: tuple[str, ...]
    reuse_arguments: ReuseArguments
    selection_filter: SelectionFilter
    selected_list_arguments: tuple[str, ...]
    run_arguments: tuple[str, ...]
    zero_tests_is_failure: bool


@dataclass(frozen=True)
class GuestEnvVar:
    """One environment variable the guest script must export."""

    name: str
    value: str


@dataclass(frozen=True)
class GuestCapabilities:
    """What the Recovery guest can and cannot do."""

    shell: str
    execution_model: str
    python3: bool
    git: bool
    dyld_shared_cache: bool
    system_c_compiler: bool
    xcrun: bool
    sdk: bool
    preinstalled_rust_toolchain: bool
    toolchain_provisioning: str
    isolated_toolchain_homes: bool
    tmp_storage: str
    scratch_storage: str
    workspace_source_staged: bool
    fixtures_staged: bool


@dataclass(frozen=True)
class ResultGate:
    """What a Recovery run must prove before it counts as passing."""

    guest_exit_code_must_be_zero: bool
    all_checks_must_pass: tuple[str, ...]
    requires_inventory: bool
    requires_junit: bool
    diagnostics_are_always_collected: bool


@dataclass(frozen=True)
class ExecutorContract:
    """The versioned handoff from soldr#3084 to soldr#3294 Phase 2."""

    schema_version: int
    superseded_issue: str
    owner_issue: str
    target: str
    backend: str
    readiness: Readiness
    nextest: NextestContract
    required_guest_env: tuple[GuestEnvVar, ...]
    required_share_files: tuple[str, ...]
    capabilities: GuestCapabilities
    result_gate: ResultGate

    def to_dict(self) -> dict:
        """The JSON document; guest env is keyed by variable name."""
        document = asdict(self)
        document["required_guest_env"] = {
            var.name: var.value for var in self.required_guest_env
        }
        return document


def executor_contract() -> ExecutorContract:
    """Return the versioned handoff from soldr#3084 to soldr#3294 Phase 2.

    This is deliberately executable rather than prose-only.  The current
    workflow records it beside every Recovery diagnostic bundle, while the
    eventual ``soldr ci-test --target`` executor can consume the same boundary
    without rediscovering the archive, staging, capability, and result rules.

    ``default_enabled`` remains false until the reliability issues named here
    are resolved.  This contract does not claim Phase 2 is complete.
    """
    return ExecutorContract(
        schema_version=1,
        superseded_issue="soldr#3084",
        owner_issue="soldr#3294",
        target="x86_64-apple-darwin",
        backend="macos-recovery",
        readiness=Readiness(
            default_enabled=False,
            blocked_by=("soldr#3088", "soldr#3136"),
            unavailable_action="fail-closed-with-no-run-guidance",
        ),
        nextest=NextestContract(
            # cargo-nextest is a Cargo subcommand shim; omitting the second
            # `nextest` silently routes `run` to Cargo instead.
            argv_prefix=("cargo-nextest", "nextest"),
            archive="tests.tar.zst",
            inventory_arguments=(
                "--archive-file=$WORK/tests.tar.zst",
                "--extract-to=$WORK/extract",
                "--workspace-remap=$WORK/workspace",
                "--profile=target-run",
                "--message-format=json-pretty",
            ),
            reuse_arguments=ReuseArguments(
                preferred=(
                    "--binaries-metadata=$REUSE_BIN_META",
                    "--cargo-metadata=$REUSE_CARGO_META",
                    "--target-dir-remap=$WORK/extract/target",
                ),
                fallback=(
                    "--archive-file=$WORK/tests.tar.zst",
                    "--extract-to=$WORK/extract",
                    "--extract-overwrite",
                ),
            ),
            selection_filter=SelectionFilter(
                source_file="$WORK/filter.txt",
                expression_variable="$FILTER",
                argument="-E",
            ),
            selected_list_arguments=(
                "$REUSE_ARGS",
                "--workspace-remap=$WORK/workspace",
                "--profile=target-run",
                "--partition=$REPLAY_PARTITION",
                "-E=$FILTER",
                "--message-format=json-pretty",
            ),
            run_arguments=(
                "$REUSE_ARGS",
                "--workspace-remap=$WORK/workspace",
                "--profile=target-run",
                "--partition=$REPLAY_PARTITION",
                "-E=$FILTER",
                "--no-fail-fast",
            ),
            zero_tests_is_failure=True,
        ),
        required_guest_env=(
            GuestEnvVar("HOME", "$WORK/home"),
            GuestEnvVar("TMPDIR", "$WORK/tmp"),
            GuestEnvVar("SOLDR_BIN", "/tmp/soldr"),
            GuestEnvVar("SOLDR_INTERNAL_DAEMON_EXE", "$WORK/soldr-daemon"),
            GuestEnvVar("NEXTEST_BIN", "$WORK/cargo-nextest"),
            GuestEnvVar("SOLDR_TEST_FIXTURES_DIR", "$WORK/fixtures"),
            GuestEnvVar("SOLDR_TEST_WORKSPACE_ROOT", "$WORK/workspace"),
            GuestEnvVar("SOLDR_USE_SYSTEM_CMAKE", "1"),
            GuestEnvVar("SOLDR_TARGET_WARN_FREE_GB", "1"),
            GuestEnvVar("SOLDR_TARGET_BLOCK_FREE_GB", "1"),
            GuestEnvVar("RUSTUP_TOOLCHAIN", "channel-from-toolchain-ensure"),
            GuestEnvVar("CARGO_HOME", "$HOME/.soldr/cargo"),
            GuestEnvVar("RUSTUP_HOME", "$HOME/.soldr/rustup"),
            GuestEnvVar("PATH", "$WORK/shims:$HOME/.soldr/cargo/bin:$PATH"),
        ),
        required_share_files=tuple(REPLAY_SHARE_FILES),
        capabilities=GuestCapabilities(
            shell="bash-3.2-posix-sh",
            execution_model="one-script-per-boot-no-command-exec",
            python3=False,
            git=False,
            dyld_shared_cache=False,
            system_c_compiler=False,
            xcrun=False,
            sdk=False,
            preinstalled_rust_toolchain=False,
            toolchain_provisioning="soldr-managed-during-guest-script",
            isolated_toolchain_homes=False,
            tmp_storage="ramdisk-bounded-by-guest-memory",
            scratch_storage="formatted-qcow2-with-tmp-fallback",
            workspace_source_staged=True,
            fixtures_staged=True,
        ),
        result_gate=ResultGate(
            guest_exit_code_must_be_zero=True,
            all_checks_must_pass=tuple(CHECKS),
            requires_inventory=True,
            requires_junit=True,
            diagnostics_are_always_collected=True,
        ),
    )


def build_guest_script(partition: str = "hash:1/1") -> str:
    """The bash-3.2/POSIX-sh script the Recovery guest runs.

    Never raises on a failed stage: every stage runs (unless a hard
    prerequisite it depends on already failed) and is recorded in
    `/tmp/results/summary.txt` as a `name=pass[:detail]` /
    `name=fail[:detail]` line via the `record` shell function, and the
    script's own exit code (0 only if every stage passed) is the second,
    coarser signal `verify-collected` checks against the action's
    `exit-code` output -- the same belt-and-suspenders
    `smoke_release_artifacts.build_release_guest_script` uses.

    Stages that depend on an earlier stage's success track that with a
    plain `..._OK` shell variable and record a `fail:skipped: ...`
    diagnostic instead of running when the prerequisite did not hold, so a
    single early failure (say, the tests archive never fetched) still
    produces a summary that explains every stage it caused to be skipped,
    rather than stopping the script cold.
    """
    match = re.fullmatch(r"hash:([1-9][0-9]*)/([1-9][0-9]*)", partition)
    if match is None or int(match.group(1)) > int(match.group(2)):
        raise ValueError(f"invalid replay partition: {partition!r}")

    return "\n".join(
        [
            "#!/bin/sh",
            "set +e",
            "mkdir -p /tmp/results",
            f"SUMMARY=/tmp/results/{RESULTS_FILE}",
            ': > "$SUMMARY"',
            "FAIL=0",
            "PHASE_TIMES=/tmp/results/phase-times.tsv",
            "printf 'stage\\tevent\\tunix_time\\tduration_seconds\\n' > \"$PHASE_TIMES\"",
            "",
            "# record NAME STATUS [DETAIL] -- flat key=value line, one per stage.",
            "# DETAIL is sanitized to a single line and length-capped: a raw",
            "# newline in an error message would otherwise corrupt the flat",
            "# summary.txt format, which is parsed one line per stage.",
            "record() {",
            '  rname="$1"',
            '  rstatus="$2"',
            '  rdetail="$3"',
            '  if [ -n "$rdetail" ]; then',
            "    rdetail=$(printf '%s' \"$rdetail\" | tr '\\n\\r' '  ' | cut -c1-400)",
            '    printf \'%s=%s:%s\\n\' "$rname" "$rstatus" "$rdetail" >> "$SUMMARY"',
            "  else",
            '    printf \'%s=%s\\n\' "$rname" "$rstatus" >> "$SUMMARY"',
            "  fi",
            '  if [ "$rstatus" = fail ]; then',
            "    FAIL=1",
            "  fi",
            "}",
            "",
            "# These stages run serially in this one guest script; timestamps",
            "# remain on the ramdisk if the work volume or a later stage fails.",
            "stage_start() {",
            "  PHASE_STARTED=$(date +%s)",
            '  echo "[$1] start unix=$PHASE_STARTED"',
            '  printf \'%s\\tstart\\t%s\\t\\n\' "$1" "$PHASE_STARTED" >> "$PHASE_TIMES"',
            "}",
            "stage_end() {",
            "  PHASE_ENDED=$(date +%s)",
            "  PHASE_DURATION=$((PHASE_ENDED - PHASE_STARTED))",
            '  echo "[$1] end unix=$PHASE_ENDED duration_seconds=$PHASE_DURATION"',
            '  printf \'%s\\tend\\t%s\\t%s\\n\' "$1" "$PHASE_ENDED" "$PHASE_DURATION" >> "$PHASE_TIMES"',
            "}",
            "",
            "# fetch NAME DEST [x] -- GET http://10.0.2.2:8000/NAME into DEST,",
            "# chmod +x DEST when the third arg is 'x'. Records fetch_NAME.",
            "fetch() {",
            f'  if ! curl -fsS -o "$2" "{GUEST_HTTP_BASE}/$1"; then',
            f'    record "fetch_$1" fail "curl could not reach {GUEST_HTTP_BASE}/$1"',
            "    return 1",
            "  fi",
            '  if [ "$3" = x ]; then',
            '    chmod +x "$2"',
            "  fi",
            '  record "fetch_$1" pass',
            "  return 0",
            "}",
            "",
            *_stage_core_smoke(),
            *_stage_storage(),
            *_stage_fetch_replay_inputs(),
            *_stage_extract(),
            *_stage_toolchain(),
            *_stage_nextest_version(),
            *_stage_nextest_list_all(),
            *_stage_nextest_list_selected(partition),
            *_stage_nextest_run(partition),
            *_stage_collect_results(),
            'exit "$FAIL"',
            "",
        ]
    )


def _stage_core_smoke() -> list[str]:
    """arch / fetch_soldr / version / help -- the original soldr#3076 smoke.

    `/tmp/soldr` (ramdisk) rather than a path under the storage volume: this
    stage runs before `storage` even determines a work volume, and nothing
    later needs a second copy -- the ramdisk persists for the whole boot.
    """
    return [
        "stage_start core",
        "ARCH=$(uname -m)",
        'if [ "$ARCH" = x86_64 ]; then',
        '  record arch pass "$ARCH"',
        "else",
        '  record arch fail "unexpected uname -m $ARCH"',
        "fi",
        "",
        "fetch soldr /tmp/soldr x",
        "SOLDR_BIN=/tmp/soldr",
        "export SOLDR_BIN",
        "",
        "if [ -x /tmp/soldr ]; then",
        "  VOUT=$(/tmp/soldr --version 2>&1)",
        '  case "$VOUT" in',
        '    "soldr "*)',
        '      record version pass "$VOUT" ;;',
        "    *)",
        '      record version fail "$VOUT" ;;',
        "  esac",
        "",
        "  /tmp/soldr --help >/tmp/help.out 2>&1",
        "  HRC=$?",
        '  if [ "$HRC" -eq 0 ]; then',
        "    record help pass",
        "  else",
        '    record help fail "exit $HRC"',
        "  fi",
        "else",
        '  record version fail "soldr binary missing (fetch_soldr failed)"',
        '  record help fail "soldr binary missing (fetch_soldr failed)"',
        "fi",
        "stage_end core",
        "",
    ]


def _stage_storage() -> list[str]:
    """Format+mount the action's blank qcow2 disk, or fall back to /tmp/work.

    `diskutil list` does not print media model names, so every whole disk it
    lists is sized through `diskutil info` and the largest one at or above
    MIN_SCRATCH_DISK_BYTES is chosen; the Base System and OpenCore disks are
    both far smaller and can never be picked.
    """
    return [
        "stage_start storage",
        "diskutil list > /tmp/diskutil-list.txt 2>/tmp/diskutil-list.err",
        ": > /tmp/diskutil-info.txt",
        'DISK=""',
        "BEST_BYTES=0",
        "for dev in $(sed -n 's#^\\(/dev/disk[0-9]*\\).*#\\1#p' /tmp/diskutil-list.txt); do",
        '  info=$(diskutil info "$dev" 2>/dev/null)',
        "  printf '%s\\n---\\n' \"$info\" >> /tmp/diskutil-info.txt",
        "  printf '%s\\n' \"$info\" | grep -q 'Whole:.*Yes' || continue",
        "  bytes=$(printf '%s\\n' \"$info\" | sed -n 's/.*Disk Size:.*(\\([0-9][0-9]*\\) Bytes).*/\\1/p' | head -n1)",
        "  case \"$bytes\" in ''|*[!0-9]*) continue ;; esac",
        f'  if [ "$bytes" -ge {MIN_SCRATCH_DISK_BYTES} ] && [ "$bytes" -gt "$BEST_BYTES" ]; then',
        '    DISK="${dev#/dev/}"',
        '    BEST_BYTES="$bytes"',
        "  fi",
        "done",
        "",
        'WORK=""',
        'if [ -n "$DISK" ]; then',
        '  if diskutil eraseDisk APFS Work "/dev/$DISK" > /tmp/erase.log 2>&1 && [ -d /Volumes/Work ]; then',
        "    WORK=/Volumes/Work",
        "  fi",
        "fi",
        'if [ -z "$WORK" ]; then',
        "  WORK=/tmp/work",
        '  mkdir -p "$WORK"',
        "fi",
        "",
        "FREE_KB=$(df -k \"$WORK\" 2>/dev/null | awk 'NR==2{print $4}')",
        'case "$FREE_KB" in',
        "  ''|*[!0-9]*) FREE_GIB=unknown ;;",
        "  *) FREE_GIB=$((FREE_KB / 1048576)) ;;",
        "esac",
        'if [ "$WORK" = /Volumes/Work ]; then',
        '  record storage pass "disk=$DISK path=$WORK free_gib=$FREE_GIB"',
        "else",
        '  record storage pass "fallback path=$WORK free_gib=$FREE_GIB disk=${DISK:-none}"',
        "fi",
        "export WORK",
        'mkdir -p "$WORK/home" "$WORK/tmp"',
        'HOME="$WORK/home"',
        "# Recovery's default temp dirs (/tmp, /var/folders) are ~5 GiB tmpfs;",
        "# 13 of 19 failures on PR #3087's third run were ENOSPC from tests'",
        "# tempdirs. Point every temp_dir() consumer at the scratch disk.",
        'TMPDIR="$WORK/tmp"',
        "export HOME TMPDIR",
        "",
        "# Env every archived test expects (mirrors the native target-run",
        "# path's 'Resolve packaged target tools' step). CARGO_HOME/RUSTUP_HOME",
        "# are deliberately left unset -- 'soldr toolchain ensure' exports its",
        "# own managed homes.",
        'SOLDR_INTERNAL_DAEMON_EXE="$WORK/soldr-daemon"',
        'NEXTEST_BIN="$WORK/cargo-nextest"',
        'SOLDR_TEST_WORKSPACE_ROOT="$WORK/workspace"',
        'SOLDR_TEST_FIXTURES_DIR="$WORK/fixtures"',
        "SOLDR_USE_SYSTEM_CMAKE=1",
        "SOLDR_TARGET_WARN_FREE_GB=1",
        "SOLDR_TARGET_BLOCK_FREE_GB=1",
        "# zackees/ci.yml#168 (GATE-005): the nextest run-wrapper refuses to",
        "# run soldr's tests outside an isolated environment; a disposable",
        "# Recovery guest is one, and it has no CI=true of its own.",
        "SOLDR_TEST_ISOLATED=1",
        "export SOLDR_TEST_ISOLATED",
        "export SOLDR_INTERNAL_DAEMON_EXE NEXTEST_BIN SOLDR_TEST_WORKSPACE_ROOT",
        "export SOLDR_TEST_FIXTURES_DIR SOLDR_USE_SYSTEM_CMAKE",
        "export SOLDR_TARGET_WARN_FREE_GB SOLDR_TARGET_BLOCK_FREE_GB",
        "stage_end storage",
        "",
    ]


def _stage_fetch_replay_inputs() -> list[str]:
    return [
        "stage_start fetch_replay_inputs",
        'fetch soldr-daemon "$WORK/soldr-daemon" x',
        "FETCH_SOLDR_DAEMON_OK=$?",
        'fetch cargo-nextest "$WORK/cargo-nextest" x',
        "FETCH_CARGO_NEXTEST_OK=$?",
        'fetch tests.tar.zst "$WORK/tests.tar.zst"',
        "FETCH_TESTS_OK=$?",
        'fetch fixtures.tar "$WORK/fixtures.tar"',
        "FETCH_FIXTURES_TAR_OK=$?",
        'fetch workspace.tar "$WORK/workspace.tar"',
        "FETCH_WORKSPACE_TAR_OK=$?",
        'fetch filter.txt "$WORK/filter.txt"',
        "FETCH_FILTER_OK=$?",
        'fetch nextest-version.txt "$WORK/nextest-version.txt"',
        "FETCH_NEXTVER_OK=$?",
        "stage_end fetch_replay_inputs",
        "",
    ]


def _stage_extract() -> list[str]:
    return [
        "stage_start extract_workspace",
        'if [ "$FETCH_WORKSPACE_TAR_OK" -eq 0 ]; then',
        '  mkdir -p "$WORK/workspace"',
        '  if tar -xf "$WORK/workspace.tar" -C "$WORK/workspace"; then',
        "    record extract_workspace pass",
        "    EXTRACT_WORKSPACE_OK=0",
        "  else",
        '    record extract_workspace fail "tar extraction of workspace.tar failed"',
        "    EXTRACT_WORKSPACE_OK=1",
        "  fi",
        "else",
        '  record extract_workspace fail "skipped: workspace.tar was not fetched"',
        "  EXTRACT_WORKSPACE_OK=1",
        "fi",
        "stage_end extract_workspace",
        "",
        "stage_start extract_fixtures",
        'if [ "$FETCH_FIXTURES_TAR_OK" -eq 0 ]; then',
        '  mkdir -p "$WORK/fixtures"',
        '  if tar -xf "$WORK/fixtures.tar" -C "$WORK/fixtures"; then',
        "    record extract_fixtures pass",
        "    EXTRACT_FIXTURES_OK=0",
        "  else",
        '    record extract_fixtures fail "tar extraction of fixtures.tar failed"',
        "    EXTRACT_FIXTURES_OK=1",
        "  fi",
        "else",
        '  record extract_fixtures fail "skipped: fixtures.tar was not fetched"',
        "  EXTRACT_FIXTURES_OK=1",
        "fi",
        "stage_end extract_fixtures",
        "",
    ]


def _stage_toolchain() -> list[str]:
    """`soldr toolchain ensure` + `toolchain link`, run from the extracted
    workspace root so `rust-toolchain.toml` resolves the same way it does on
    every other target-run lane.

    There is no Python in the guest to run `toolchain_ensure_channel.py`, so
    the resolved channel is pulled out of the ensure JSON with `sed` instead
    -- tolerant of the same rustup-preamble noise that script's docstring
    describes, since it only looks for a line naming `"channel"` rather than
    parsing the payload as a whole.
    """
    return [
        "stage_start toolchain",
        'if [ "$EXTRACT_WORKSPACE_OK" -eq 0 ] && cd "$WORK/workspace"; then',
        '  "$SOLDR_BIN" toolchain ensure --json > "$WORK/toolchain-ensure.json" 2>&1',
        "  TE_RC=$?",
        '  if [ "$TE_RC" -eq 0 ]; then',
        "    CHANNEL=$(sed -n "
        + '\'s/.*"channel"[[:space:]]*:[[:space:]]*"\\([^"]*\\)".*/\\1/p\''
        + ' "$WORK/toolchain-ensure.json" | head -n1)',
        '    if [ -n "$CHANNEL" ]; then',
        '      record toolchain_ensure pass "channel=$CHANNEL"',
        '      RUSTUP_TOOLCHAIN="$CHANNEL"',
        "      export RUSTUP_TOOLCHAIN",
        # soldr#3846: the guest has no system rustup. `toolchain ensure`
        # bootstraps one into soldr's own root (`<root>/cargo/bin`, toolchains
        # under `<root>/rustup`), which is never on PATH, so every test that
        # resolves the real toolchain through `rustup` (with an isolated HOME
        # and no bootstrap allowed) failed with `rustup which cargo: No such
        # file or directory`. Expose that rustup the way a system install is
        # exposed on the Linux lanes.
        # The guest sets no SOLDR_CACHE_DIR, so soldr's root is $HOME/.soldr;
        # its cargo/bin joins PATH (behind the shims) once `toolchain link`
        # succeeds below.
        '      CARGO_HOME="$HOME/.soldr/cargo"',
        '      RUSTUP_HOME="$HOME/.soldr/rustup"',
        "      export CARGO_HOME RUSTUP_HOME",
        "      TOOLCHAIN_ENSURE_OK=0",
        "    else",
        '      record toolchain_ensure fail "ensure succeeded but JSON names no channel"',
        "      TOOLCHAIN_ENSURE_OK=1",
        "    fi",
        "  else",
        '    record toolchain_ensure fail "soldr toolchain ensure exited $TE_RC"',
        "    TOOLCHAIN_ENSURE_OK=1",
        "  fi",
        "else",
        '  record toolchain_ensure fail "skipped: workspace extraction did not succeed"',
        "  TOOLCHAIN_ENSURE_OK=1",
        "fi",
        "",
        'if [ "$TOOLCHAIN_ENSURE_OK" -eq 0 ]; then',
        '  "$SOLDR_BIN" toolchain link --shim-dir "$WORK/shims" --json > "$WORK/toolchain-link.json" 2>&1',
        "  TL_RC=$?",
        '  if [ "$TL_RC" -eq 0 ]; then',
        "    record toolchain_link pass",
        '    PATH="$WORK/shims:$HOME/.soldr/cargo/bin:$PATH"',
        "    export PATH",
        "    TOOLCHAIN_LINK_OK=0",
        "  else",
        '    record toolchain_link fail "soldr toolchain link exited $TL_RC"',
        "    TOOLCHAIN_LINK_OK=1",
        "  fi",
        "else",
        '  record toolchain_link fail "skipped: toolchain_ensure did not succeed"',
        "  TOOLCHAIN_LINK_OK=1",
        "fi",
        "stage_end toolchain",
        "",
    ]


def _stage_nextest_version() -> list[str]:
    return [
        "stage_start nextest_version",
        'if [ "$FETCH_CARGO_NEXTEST_OK" -eq 0 ] && [ "$FETCH_NEXTVER_OK" -eq 0 ]; then',
        '  ACTUAL=$("$NEXTEST_BIN" nextest --version 2>&1 '
        + "| sed -n 's/.*[^0-9]\\([0-9][0-9]*\\.[0-9][0-9]*\\.[0-9][0-9]*\\).*/\\1/p' "
        + "| head -n1)",
        '  EXPECTED=$(cat "$WORK/nextest-version.txt")',
        '  if [ -n "$ACTUAL" ] && [ "$ACTUAL" = "$EXPECTED" ]; then',
        '    record nextest_version pass "$ACTUAL"',
        "    NEXTEST_VERSION_OK=0",
        "  else",
        '    record nextest_version fail "expected=$EXPECTED actual=${ACTUAL:-<none>}"',
        "    NEXTEST_VERSION_OK=1",
        "  fi",
        "else",
        '  record nextest_version fail "skipped: cargo-nextest or nextest-version.txt not fetched"',
        "  NEXTEST_VERSION_OK=1",
        "fi",
        "stage_end nextest_version",
        "",
    ]


def _stage_nextest_list_all() -> list[str]:
    """Inventory pass: also the only extraction (soldr#2933's rule, ported).

    `--archive-file` + `--extract-to` decompresses the whole archive into
    `$WORK/extract`; `nextest_list_selected` / `nextest_run` below reuse that
    extraction (or fall back to re-extracting) instead of paying for it
    twice, the same rule `nextest_reuse_extraction.py` encodes for the
    native path -- ported here as a `find`-based shallow search since the
    guest has no Python to run that script.
    """
    return [
        "stage_start nextest_list_all",
        'if [ "$FETCH_TESTS_OK" -eq 0 ] && [ "$EXTRACT_WORKSPACE_OK" -eq 0 ] && [ "$NEXTEST_VERSION_OK" -eq 0 ]; then',
        # nextest canonicalizes --extract-to before extracting and exits 96 when
        # it does not exist yet (seen live on release run 33820395040).
        '  mkdir -p "$WORK/extract"',
        '  "$NEXTEST_BIN" nextest list \\',
        '    --archive-file "$WORK/tests.tar.zst" \\',
        '    --extract-to "$WORK/extract" \\',
        '    --workspace-remap "$WORK/workspace" \\',
        "    --profile target-run \\",
        '    --message-format json-pretty > "$WORK/all-list.json" 2> "$WORK/all-list.stderr"',
        "  LA_RC=$?",
        '  if [ "$LA_RC" -eq 0 ]; then',
        "    record nextest_list_all pass",
        "    LIST_ALL_OK=0",
        "  else",
        '    record nextest_list_all fail "nextest list exited $LA_RC: $(tail -c 400 "$WORK/all-list.stderr")"',
        "    LIST_ALL_OK=1",
        "  fi",
        "else",
        '  record nextest_list_all fail "skipped: tests.tar.zst/workspace extraction/nextest_version not ready"',
        "  LIST_ALL_OK=1",
        "fi",
        "",
        "# Reuse flags for the two nextest passes below (soldr#2933, ported for",
        "# a guest with no Python to run nextest_reuse_extraction.py): a",
        "# shallow search for the reuse metadata nextest's own extraction",
        "# wrote, falling back to a second --archive-file/--extract-to pass",
        "# aimed at the same directory when it is not found.",
        'REUSE_BIN_META=""',
        'REUSE_CARGO_META=""',
        'if [ "$LIST_ALL_OK" -eq 0 ]; then',
        '  REUSE_BIN_META=$(find "$WORK/extract" -maxdepth 4 -type f -name binaries-metadata.json 2>/dev/null | head -n1)',
        '  REUSE_CARGO_META=$(find "$WORK/extract" -maxdepth 4 -type f -name cargo-metadata.json 2>/dev/null | head -n1)',
        "fi",
        'if [ -n "$REUSE_BIN_META" ] && [ -n "$REUSE_CARGO_META" ] && [ -d "$WORK/extract/target" ]; then',
        '  REUSE_ARGS="--binaries-metadata $REUSE_BIN_META --cargo-metadata $REUSE_CARGO_META --target-dir-remap $WORK/extract/target"',
        '  NEXTEST_TARGET_DIR="$WORK/extract/target"',
        "else",
        '  REUSE_ARGS="--archive-file $WORK/tests.tar.zst --extract-to $WORK/extract --extract-overwrite"',
        '  NEXTEST_TARGET_DIR="$WORK/extract/target"',
        "fi",
        "",
        "# soldr#3454: every test runs through the archived native wrapper; name",
        "# its absence here rather than as N identical test refusals.",
        'NATIVE_WRAPPER=""',
        'if [ "$LIST_ALL_OK" -eq 0 ]; then',
        '  NATIVE_WRAPPER=$(find "$WORK/extract/target" -maxdepth 3 -type f -name soldr-nextest-wrapper 2>/dev/null | head -n1)',
        '  if [ -n "$NATIVE_WRAPPER" ] && [ -x "$NATIVE_WRAPPER" ]; then',
        '    record native_wrapper pass "$NATIVE_WRAPPER"',
        "  else",
        '    record native_wrapper fail "soldr-nextest-wrapper is missing from the extracted archive"',
        "  fi",
        "else",
        '  record native_wrapper fail "skipped: nextest_list_all did not extract the archive"',
        "fi",
        "stage_end nextest_list_all",
        "",
    ]


def _stage_nextest_list_selected(partition: str) -> list[str]:
    return [
        "stage_start nextest_list_selected",
        'if [ "$LIST_ALL_OK" -eq 0 ] && [ "$FETCH_FILTER_OK" -eq 0 ]; then',
        '  FILTER=$(cat "$WORK/filter.txt")',
        "  # shellcheck disable=SC2086",
        '  "$NEXTEST_BIN" nextest list $REUSE_ARGS \\',
        '    --workspace-remap "$WORK/workspace" \\',
        "    --profile target-run \\",
        f"    --partition {partition} \\",
        '    -E "$FILTER" \\',
        '    --message-format json-pretty > "$WORK/list.json" 2> "$WORK/list.stderr"',
        "  LS_RC=$?",
        '  if [ "$LS_RC" -eq 0 ]; then',
        "    record nextest_list_selected pass",
        "    LIST_SELECTED_OK=0",
        "  else",
        '    record nextest_list_selected fail "nextest list exited $LS_RC: $(tail -c 400 "$WORK/list.stderr")"',
        "    LIST_SELECTED_OK=1",
        "  fi",
        "else",
        '  record nextest_list_selected fail "skipped: nextest_list_all or filter.txt not ready"',
        "  LIST_SELECTED_OK=1",
        "fi",
        "stage_end nextest_list_selected",
        "",
    ]


# Seconds between guest memory samples printed during `nextest run`.
MEM_SAMPLE_SECS = 30


def _memory_sampler_function() -> list[str]:
    """`mem_sample`: print one `[mem] ...` line describing guest memory.

    soldr#3136: the Recovery guest intermittently freezes mid-suite (5 of 8
    nightly replays 09-07..09-14) and the stall-collect cannot open a second
    Terminal, so nothing under `/tmp/results` survives a freeze. The only
    channel that does is the heartbeat tail of this script's stdout, so the
    sample goes there. Every probe degrades to `n/a` when Recovery lacks the
    tool or the key, rather than failing the stage.

    Field order is deliberate: the heartbeat is a 4000-byte tail, so a sample
    can be cut off mid-line. Free memory and the per-process aggregates come
    first, since they separate "processes accumulate across tests" from "one
    process grows", which is the question run 34847511968 left open. `name=N/SM`
    is the count and summed RSS of processes with that executable name.

    `reclaim_spec/purge/ext` is speculative, purgeable and file-backed pageable
    memory in MiB, and `comp` is compressor occupancy. Run 34863589272 showed
    `free` at 3M while the pressure level stayed normal: macOS keeps
    reclaimable cache out of `free`, so these separate a real exhaustion from a
    full cache (soldr#3136).
    """
    return [
        "# soldr#3136: memory evidence for a guest freeze; see _memory_sampler_function.",
        "na() {",
        "  if [ -n \"$1\" ]; then printf '%s' \"$1\"; else printf 'n/a'; fi",
        "}",
        "# proc_aggregates: read `rss comm` lines on stdin and print the process",
        "# count plus count/summed RSS per executable name. A function, not an",
        "# inline block: Recovery's bash 3.2 cannot parse `case` inside `$( )`",
        "# (run 34853028731: syntax error near unexpected token `;;').",
        "proc_aggregates() {",
        "  n=0; dn=0; dk=0; bn=0; bk=0; sn=0; sk=0; un=0; uk=0; cn=0; ck=0; rn=0; rk=0",
        "  while read -r ms_rss ms_comm; do",
        '    case "$ms_rss" in',
        "      ''|*[!0-9]*) continue ;;",
        "    esac",
        "    n=$((n + 1))",
        '    case "${ms_comm##*/}" in',
        "      soldr-daemon) dn=$((dn + 1)); dk=$((dk + ms_rss)) ;;",
        "      soldr-broker) bn=$((bn + 1)); bk=$((bk + ms_rss)) ;;",
        "      soldr) sn=$((sn + 1)); sk=$((sk + ms_rss)) ;;",
        "      rustup|rustup-init) un=$((un + 1)); uk=$((uk + ms_rss)) ;;",
        "      cargo) cn=$((cn + 1)); ck=$((ck + ms_rss)) ;;",
        "      rustc) rn=$((rn + 1)); rk=$((rk + ms_rss)) ;;",
        "    esac",
        "  done",
        '  [ "$n" -gt 0 ] || return 0',
        "  printf '%s daemon=%s/%sM broker=%s/%sM soldr=%s/%sM rustup=%s/%sM' \\",
        '    "$n" "$dn" "$((dk / 1024))" "$bn" "$((bk / 1024))" "$sn" "$((sk / 1024))" \\',
        '    "$un" "$((uk / 1024))"',
        "  printf ' cargo=%s/%sM rustc=%s/%sM' \\",
        '    "$cn" "$((ck / 1024))" "$rn" "$((rk / 1024))"',
        "}",
        "# sysctl_mib KEY PAGESIZE: a page-count sysctl in MiB, empty when absent.",
        "sysctl_mib() {",
        '  sm_pages=$(sysctl -n "$1" 2>/dev/null)',
        '  case "$sm_pages$2" in',
        "    ''|*[!0-9]*) ;;",
        "    *) printf '%s' \"$((sm_pages * $2 / 1048576))\" ;;",
        "  esac",
        "}",
        "mem_sample() {",
        '  ms_free=""; ms_pressure=""; ms_swap=""; ms_load=""; ms_procs=""; ms_top=""',
        '  ms_reclaim=""; ms_comp=""',
        "  if command -v sysctl >/dev/null 2>&1; then",
        "    ms_pages=$(sysctl -n vm.page_free_count 2>/dev/null)",
        "    ms_pagesize=$(sysctl -n hw.pagesize 2>/dev/null)",
        '    case "$ms_pages$ms_pagesize" in',
        "      ''|*[!0-9]*) ;;",
        '      *) ms_free="$((ms_pages * ms_pagesize / 1048576))M" ;;',
        "    esac",
        "    ms_pressure=$(sysctl -n kern.memorystatus_vm_pressure_level 2>/dev/null)",
        '    ms_reclaim=$(sysctl_mib vm.page_speculative_count "$ms_pagesize")/$(sysctl_mib \\',
        '      vm.page_purgeable_count "$ms_pagesize")/$(sysctl_mib \\',
        '      vm.page_pageable_external_count "$ms_pagesize")',
        "    ms_comp=$(sysctl -n vm.compressor_bytes_used 2>/dev/null)",
        '    case "$ms_comp" in',
        "      ''|*[!0-9]*) ms_comp='' ;;",
        '      *) ms_comp="$((ms_comp / 1048576))M" ;;',
        "    esac",
        "    ms_swap=$(sysctl -n vm.swapusage 2>/dev/null \\",
        "      | sed -n 's/.*used = \\([^ ]*\\).*/\\1/p')",
        "    ms_load=$(sysctl -n vm.loadavg 2>/dev/null | tr -d '{}' | tr -s ' ')",
        "  fi",
        "  if command -v ps >/dev/null 2>&1; then",
        "    ms_procs=$(ps -Ao rss=,comm= 2>/dev/null | proc_aggregates)",
        "    ms_top=$(ps -Ao rss=,comm= 2>/dev/null | sort -rn | head -n 3 \\",
        "      | while read -r ms_rss ms_comm; do \\",
        '          printf \'%s:%sM \' "${ms_comm##*/}" "$((ms_rss / 1024))"; done)',
        "  fi",
        '  echo "[mem] t=$(date +%H:%M:%S) free=$(na "$ms_free")" \\',
        '    "pressure=$(na "$ms_pressure") reclaim_spec/purge/ext=$(na "$ms_reclaim")M" \\',
        '    "comp=$(na "$ms_comp") procs=$(na "$ms_procs")" \\',
        '    "top=[$(na "$ms_top")] swap_used=$(na "$ms_swap") load=[$(na "$ms_load")]"',
        "}",
    ]


def _stage_nextest_run(partition: str) -> list[str]:
    return [
        "stage_start nextest_run",
        *_memory_sampler_function(),
        'if [ "$LIST_SELECTED_OK" -eq 0 ] && [ "$TOOLCHAIN_LINK_OK" -eq 0 ] \\',
        '  && [ "$FETCH_SOLDR_DAEMON_OK" -eq 0 ] && [ "$EXTRACT_FIXTURES_OK" -eq 0 ]; then',
        '  FILTER=$(cat "$WORK/filter.txt")',
        "  mem_sample",
        "  # The sleep's output is detached so a sampler killed mid-sleep cannot",
        "  # hold this script's stdout pipe open after the suite ends.",
        f"  ( while :; do sleep {MEM_SAMPLE_SECS} >/dev/null 2>&1; mem_sample; done ) &",
        "  MEM_SAMPLER_PID=$!",
        "  # --no-fail-fast, unlike the native lanes' --max-fail 3: one guest boot",
        "  # costs minutes, so a run must report every failure it can find.",
        "  # (Comments must stay OUT of the continued command below: PR #3087's",
        "  # third run split it and executed '--no-fail-fast' as a command.)",
        "  # soldr#3097: tee nextest's output into this script's stdout as well",
        "  # as the log file. The driver's only liveness signal is the guest",
        "  # heartbeat's tail of this stdout; with output going to the file",
        "  # alone, a running suite looked dead and was killed after the",
        "  # no-progress timeout (runs 33928513384, 33935578999). The exit code",
        "  # is carried through a file because Recovery's sh has no PIPESTATUS.",
        "  # shellcheck disable=SC2086",
        '  ( "$NEXTEST_BIN" nextest run $REUSE_ARGS \\',
        '      --workspace-remap "$WORK/workspace" \\',
        "      --profile target-run \\",
        f"      --partition {partition} \\",
        '      -E "$FILTER" \\',
        "      --no-fail-fast 2>&1; \\",
        '    echo $? > "$WORK/nextest-run.rc" ) | tee "$WORK/nextest-run.log"',
        '  NR_RC=$(cat "$WORK/nextest-run.rc" 2>/dev/null || echo 1)',
        '  kill "$MEM_SAMPLER_PID" 2>/dev/null',
        "  mem_sample",
        '  if [ "$NR_RC" -eq 0 ]; then',
        "    record nextest_run pass",
        "  else",
        '    record nextest_run fail "nextest run exited $NR_RC"',
        "  fi",
        "else",
        '  record nextest_run fail "skipped: nextest_list_selected/toolchain_link/soldr-daemon/fixtures not ready"',
        "fi",
        "stage_end nextest_run",
        "",
    ]


def _stage_collect_results() -> list[str]:
    """Copy only small diagnostics into /tmp/results/ -- never the extraction.

    `summary.txt` already lives at `/tmp/results/summary.txt` (every `record`
    call above wrote straight to it); everything else here is best-effort
    (`2>/dev/null`) so a missing file from an earlier failed stage does not
    itself fail the script -- the missing file is its own diagnostic once
    collected.
    """
    return [
        "stage_start collect_results",
        'cp "$WORK/toolchain-ensure.json" /tmp/results/ 2>/dev/null',
        'cp "$WORK/all-list.json" /tmp/results/ 2>/dev/null',
        'cp "$WORK/list.json" /tmp/results/ 2>/dev/null',
        "",
        "# The junit path depends on which nextest_run path actually executed",
        "# (target-dir-remap reuse vs. a fresh extract-to); check both rather",
        "# than assuming one.",
        'JUNIT_SRC=""',
        'if [ -f "$NEXTEST_TARGET_DIR/nextest/target-run/junit.xml" ]; then',
        '  JUNIT_SRC="$NEXTEST_TARGET_DIR/nextest/target-run/junit.xml"',
        'elif [ -f "$WORK/workspace/target/nextest/target-run/junit.xml" ]; then',
        '  JUNIT_SRC="$WORK/workspace/target/nextest/target-run/junit.xml"',
        "fi",
        'if [ -n "$JUNIT_SRC" ]; then',
        '  cp "$JUNIT_SRC" /tmp/results/junit.xml 2>/dev/null',
        "fi",
        "",
        'if [ -f "$WORK/nextest-run.log" ]; then',
        f'  tail -c {NEXTEST_LOG_TAIL_BYTES} "$WORK/nextest-run.log" > /tmp/results/nextest-run.log 2>/dev/null',
        "fi",
        "",
        "cp /tmp/diskutil-list.txt /tmp/diskutil-info.txt /tmp/erase.log /tmp/results/ 2>/dev/null",
        'cp "$WORK/all-list.stderr" /tmp/results/ 2>/dev/null',
        "df -h > /tmp/results/df.txt 2>&1",
        "env > /tmp/results/env.txt 2>&1",
        "stage_end collect_results",
        "",
    ]


def append_github_output_multiline(path: Path, name: str, value: str) -> None:
    """Append a multi-line `name<<EOF / value / EOF` block to `$GITHUB_OUTPUT`.

    Doing this in Python instead of a separate bash heredoc step keeps the
    workflow's inline `run:` footprint down and makes the delimiter handling
    testable instead of hand-typed YAML.
    """
    delimiter = f"GITHUB_OUTPUT_{name.upper()}_EOF"
    with path.open("a", encoding="utf-8") as handle:
        handle.write(f"{name}<<{delimiter}\n{value}{delimiter}\n")


@dataclass(frozen=True)
class CheckResult:
    """One `name=pass[:detail]` / `name=fail[:detail]` line of the summary."""

    name: str
    passed: bool
    detail: str


def parse_summary(text: str) -> list[CheckResult]:
    """Parse the guest's flat `key=value` results file.

    Shared shape with `smoke_release_artifacts.parse_summary`: each line is
    `name=pass[:detail]` or `name=fail[:detail]`. A malformed or truncated
    line (a wedged guest can be killed mid-write) is not silently dropped --
    it fails as its own diagnostic.
    """
    results: list[CheckResult] = []
    for lineno, raw in enumerate(text.splitlines(), start=1):
        line = raw.strip()
        if not line:
            continue
        name, sep, rest = line.partition("=")
        if not sep:
            results.append(
                CheckResult(f"summary_line_{lineno}", False, f"malformed line: {raw!r}")
            )
            continue
        status, _, detail = rest.partition(":")
        results.append(CheckResult(name, status == "pass", detail))
    return results


def verify_collected(collected_dir: Path, *, guest_exit_code: str) -> int:
    """Read the collected Recovery results and fail with a named diagnostic."""
    summary_path = collected_dir / RESULTS_FILE
    if not summary_path.is_file():
        sys.exit(
            f"ERROR: {summary_path} is missing — the Recovery guest never wrote "
            "results (did the script even reach a shell? check the action's "
            "workdir/results/*.ppm screendumps)."
        )
    # Later lines win on a repeated name, as the keyed lookup always did.
    results = {
        result.name: result
        for result in parse_summary(
            summary_path.read_text(encoding="utf-8", errors="replace")
        )
    }

    failures: list[str] = []
    for name in CHECKS:
        if name not in results:
            failures.append(f"{name}: no result recorded (guest script exited early?)")
            continue
        if not results[name].passed:
            failures.append(f"{name}: {results[name].detail}")

    if guest_exit_code.strip() != "0":
        failures.append(
            f"guest script exit code {guest_exit_code!r} != '0' "
            "(see the per-check results above for which check failed)"
        )

    if failures:
        joined = "\n  - ".join(failures)
        sys.exit(f"ERROR: macOS Recovery target-run smoke failed:\n  - {joined}")

    print(
        f"macOS Recovery target-run smoke OK: {len(results)} checks recorded, all passed"
    )
    return 0


def verify_replay_artifacts(
    collected_dir: Path,
    *,
    manifest: Path,
    repo_root: Path,
    target: str,
    partition: str = "hash:1/1",
    github_summary: Path | None = None,
) -> int:
    """Run the same two checks the native `target-run` path runs, deferred.

    The Recovery guest cannot validate its own filter against a real nextest
    inventory before it boots (there is no Linux-side inventory yet, hence
    `target_run_ownership.py --filter-only`), so that validation -- a
    selector matching zero tests is stale -- runs here instead, against the
    `all-list.json` the guest's own `nextest_list_all` stage produced. The
    coverage reconciliation (`target_run_summary.py --require-junit`) is the
    same script + same flags the native path's step uses, run against the
    collected `list.json` / `junit.xml`.

    Invoked via subprocess rather than imported: the two scripts live under
    `.github/scripts/`, not `ci/`, and this mirrors exactly what the
    workflow step for the native path already runs, keeping this function's
    behavior identical to a passthrough of that shell.

    `scripts_dir` is resolved from this module's own location, not from
    `repo_root`: the two are the same soldr checkout in CI, but `repo_root`
    is the ownership manifest's *source-scan* root (what
    `validate_source_ownership` walks) and need not carry a `.github/`
    directory of its own in, say, a focused test fixture.
    """
    scripts_dir = Path(__file__).resolve().parents[1] / ".github" / "scripts"
    ownership_script = scripts_dir / "target_run_ownership.py"
    summary_script = scripts_dir / "target_run_summary.py"

    all_list = collected_dir / "all-list.json"
    list_json = collected_dir / "list.json"
    junit = collected_dir / "junit.xml"

    failures: list[str] = []

    if not all_list.is_file():
        failures.append(
            f"{all_list} is missing (nextest_list_all did not produce an inventory)"
        )
    else:
        filter_output = collected_dir / "_verify_filter.txt"
        result = run_captured(
            [
                sys.executable,
                str(ownership_script),
                "--manifest",
                str(manifest),
                "--repo-root",
                str(repo_root),
                "--list-json",
                str(all_list),
                "--target",
                target,
                "--filter-output",
                str(filter_output),
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            failures.append(
                "target-run ownership inventory validation failed:\n"
                f"{result.stdout}{result.stderr}".strip()
            )

    if not list_json.is_file():
        failures.append(
            f"{list_json} is missing (nextest_list_selected did not produce a list)"
        )
    else:
        summary_args = [
            sys.executable,
            str(summary_script),
            "--target",
            target,
            "--output",
            str(collected_dir / "target-run-summary.json"),
            "--list-json",
            str(list_json),
            "--junit",
            str(junit),
            "--require-junit",
            "--partition",
            partition,
        ]
        if github_summary is not None:
            summary_args += ["--github-summary", str(github_summary)]
        result = run_captured(summary_args, capture_output=True, text=True, check=False)
        if result.returncode != 0:
            failures.append(
                f"target-run coverage summary failed:\n{result.stdout}{result.stderr}".strip()
            )

    if failures:
        joined = "\n  - ".join(failures)
        sys.exit(
            f"ERROR: macOS Recovery replay artifact verification failed:\n  - {joined}"
        )

    print(
        "macOS Recovery replay artifacts verified: ownership inventory + coverage summary OK"
    )
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="subcommand", required=True)

    describe = subparsers.add_parser(
        "describe-executor",
        help="write the versioned macOS Recovery executor contract",
    )
    describe.add_argument("--output", required=True, type=Path)

    emit = subparsers.add_parser(
        "emit-guest-script", help="write the guest script that runs the replay"
    )
    emit.add_argument("--output", required=True, type=Path)
    emit.add_argument("--partition", default="hash:1/1")
    emit.add_argument(
        "--github-output",
        default=None,
        type=Path,
        help=(
            "also append the guest script to this $GITHUB_OUTPUT file as a "
            "'script' multi-line output, so the workflow needs no separate "
            "heredoc step to hand it to the docker-mac-x64 action's `run:` "
            "input"
        ),
    )

    verify = subparsers.add_parser(
        "verify-collected", help="verify the guest's collected results"
    )
    verify.add_argument("--collected", required=True, type=Path)
    verify.add_argument("--guest-exit-code", required=True)
    verify.add_argument(
        "--manifest",
        type=Path,
        default=None,
        help=(
            "ci/target-run-ownership.json; when given (with --repo-root and "
            "--target), also validates the ownership inventory and "
            "coverage summary from the collected all-list.json/list.json/"
            "junit.xml"
        ),
    )
    verify.add_argument("--repo-root", type=Path, default=None)
    verify.add_argument("--target", default=None)
    verify.add_argument("--partition", default="hash:1/1")
    verify.add_argument("--github-summary", type=Path, default=None)

    args = parser.parse_args(argv)

    if args.subcommand == "describe-executor":
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(
            json.dumps(executor_contract().to_dict(), indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        print(f"wrote Recovery executor contract to {args.output}")
        return 0

    if args.subcommand == "emit-guest-script":
        try:
            script_text = build_guest_script(partition=args.partition)
        except ValueError as exc:
            parser.error(str(exc))
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(script_text, encoding="utf-8")
        print(f"wrote Recovery guest script to {args.output}")
        if args.github_output is not None:
            append_github_output_multiline(args.github_output, "script", script_text)
            print(f"appended 'script' output to {args.github_output}")
        return 0

    verify_collected(args.collected, guest_exit_code=args.guest_exit_code)
    if args.manifest is not None:
        if args.repo_root is None or args.target is None:
            parser.error("--manifest requires --repo-root and --target")
        return verify_replay_artifacts(
            args.collected,
            manifest=args.manifest,
            repo_root=args.repo_root,
            target=args.target,
            partition=args.partition,
            github_summary=args.github_summary,
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
