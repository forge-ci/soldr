"""The local gate's wiring stays consistent (zackees/ci.yml#166, #168).

`ci-lint local-gate lint` (a lint-lane check) proves the workflow side over
the network; these offline asserts keep the pieces that ci-lint cannot see
in step: the one ci_lint ref named in three places, and the isolation
guard's actual behaviour.
"""

from __future__ import annotations

import os
import re
import subprocess
import tempfile
import threading
import tomllib
from dataclasses import dataclass
from pathlib import Path

import pytest
from conftest import load_script_module

ROOT = Path(__file__).resolve().parent.parent
GATE = load_script_module(ROOT / "ci" / "local_gate.py", "soldr_local_gate_wiring")
WRAPPER = ROOT / ".github" / "scripts" / "nextest_wrapper.sh"


# Every copy of the ci-lint pin outside ci.toml (soldr#3616). Workflow `ref:`
# lines must stay literal YAML, so this list is what keeps them equal.
CI_LINT_PIN_SITES = (
    ".github/workflows/ci.yml",
    ".github/workflows/ci-pre.yml",
    ".github/workflows/_build-and-test.yml",
)


def test_one_ci_lint_ref_everywhere() -> None:
    ref = GATE.ci_lint_ref()
    assert ref == GATE.CI_LINT_REF
    for site in CI_LINT_PIN_SITES:
        text = (ROOT / site).read_text(encoding="utf-8")
        pinned = set(re.findall(r"zackees/ci\.yml@([0-9a-f]{40})", text))
        refs = re.findall(r"repository: zackees/ci\.yml\n\s+ref: ([0-9a-f]{40})", text)
        assert pinned | set(refs), f"{site}: no ci-lint pin found"
        assert pinned | set(refs) == {ref}, (
            f"{site}: pins {pinned | set(refs)} != ci.toml {ref}"
        )
    # No other tracked workflow may check out zackees/ci.yml unlisted.
    for wf in (ROOT / ".github" / "workflows").glob("*.y*ml"):
        if "repository: zackees/ci.yml" in wf.read_text(encoding="utf-8"):
            assert f".github/workflows/{wf.name}" in CI_LINT_PIN_SITES, wf.name


def test_local_gate_toml_does_not_repeat_the_pin() -> None:
    """soldr#3556: local-gate.toml is a mandatory input of every lane; ci.toml
    is the one canonical pin (soldr#3616), so no copy may live here."""
    declaration = (ROOT / "local-gate.toml").read_text(encoding="utf-8")
    assert not re.search(r"[0-9a-f]{40}", declaration), (
        "local-gate.toml duplicates a pin"
    )


def test_ci_lint_ref_rejects_malformed_linter(tmp_path: Path) -> None:
    bad = tmp_path / "ci.toml"
    bad.write_text('linter = "zackees/ci.yml@main"\n', encoding="utf-8")
    with pytest.raises(SystemExit):
        GATE.ci_lint_ref(bad)


def test_lint_job_runs_only_the_lint_lane() -> None:
    gate = tomllib.loads((ROOT / "local-gate.toml").read_text(encoding="utf-8"))["gate"]
    assert gate["mirrors"] == ["ci.yml:lint"]
    workflow = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    assert (
        "run: uv run --no-project --python 3.13 python ci/local_gate.py --lane lint"
        in workflow
    )


def test_attested_skip_is_declared_and_wired() -> None:
    """zackees/ci.yml#190 (GATE-008): the skip jobs consume ci-mode's
    `trusted` output, the protected `Lint` status still reports through
    lint-docs, and pushes to main always run (verify never trusts them)."""
    gate = tomllib.loads((ROOT / "local-gate.toml").read_text(encoding="utf-8"))["gate"]
    trust = gate["trust"]
    assert trust["mode"] == "enforce"
    assert trust["skip"] == ["ci.yml:lint", "ci.yml:build-linux-x64"]
    assert trust.get("audit-rate", 10) >= 2
    workflow = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    assert "local-gate verify --repo . --trust --github-output" in workflow
    assert "trusted: ${{ steps.gate.outputs.trusted }}" in workflow
    # zackees/ci.yml#198 (GATE-010): each skip job consumes its own per-job
    # decision, and Linux x64 still runs when only Lint was skipped.
    assert "needs.ci-mode.outputs.skip_lint != 'true'" in workflow
    assert "needs.ci-mode.outputs.skip_build-linux-x64 != 'true'" in workflow
    assert (
        "(needs.lint.result == 'success' || needs.ci-mode.outputs.skip_lint == 'true')"
        in workflow
    )
    assert "needs.ci-mode.outputs.skip_lint == 'true'" in workflow  # lint-docs
    assert "\n    branches:\n      - main\n" in workflow


def test_ci_attestations_cover_every_skip_job() -> None:
    """zackees/ci.yml#198: every [gate.trust] skip job is mapped to gates,
    and every gate names a declared lane (ci-lint local-gate lint checks the
    same; this keeps it offline)."""
    gate = tomllib.loads((ROOT / "local-gate.toml").read_text(encoding="utf-8"))["gate"]
    text = (ROOT / "ci-attestations.yml").read_text(encoding="utf-8")
    for job in gate["trust"]["skip"]:
        assert f"  {job}:" in text, job
    for lane in re.findall(r"\{lane: ([a-z-]+)\}", text):
        assert lane in gate["lanes"], lane
    pre = (ROOT / ".github" / "workflows" / "ci-pre.yml").read_text(encoding="utf-8")
    assert "ci_lint attest keys" in pre
    assert pre.count("steps.att.outputs.stem_") == 16


# soldr#3703: the scan/Python lanes never compile into the shared target
# dir, so ci-lint runs them concurrently beside the heavy (compiling) chain.
# Every compiling lane must stay heavy: two cargo builds at once would contend
# for the target dir and every core.
LIGHT_LANES = {"py-static", "guards", "ci-lint"}


def test_only_the_non_compiling_lanes_are_light() -> None:
    lanes = tomllib.loads((ROOT / "local-gate.toml").read_text(encoding="utf-8"))[
        "gate"
    ]["lanes"]
    light = {name for name, lane in lanes.items() if lane.get("weight") == "light"}
    assert light == LIGHT_LANES
    for name, lane in lanes.items():
        assert lane.get("weight", "heavy") in {"light", "heavy"}, name


# zackees/ci.yml#399: the `tests` lane builds in its private root's own
# target dir, so it is its own heavy group and runs beside the host chain.
# Every host-compiling heavy lane shares target/ and must stay in one group.
def test_only_the_isolated_tests_lane_has_its_own_heavy_group() -> None:
    lanes = tomllib.loads((ROOT / "local-gate.toml").read_text(encoding="utf-8"))[
        "gate"
    ]["lanes"]
    groups = {name: lane["group"] for name, lane in lanes.items() if "group" in lane}
    assert groups == {"tests": "isolated"}


def test_the_isolated_test_run_proves_its_tree() -> None:
    """zackees/ci.yml#196: the test check carries a nonce the driver must echo
    from the tree it tests, so a run against another worktree fails the gate
    instead of attesting the wrong tree."""
    tests = next(c for c in GATE.checks() if c.lane == "tests")
    assert tests.tree_nonce
    task = (ROOT / "ci" / "workspace_test.py").read_text(encoding="utf-8")
    assert '".gate-nonce"' in task and "gate-nonce: " in task
    assert GATE.NONCE_FILE in (ROOT / ".gitignore").read_text(encoding="utf-8")


def test_the_test_suite_only_runs_isolated_locally() -> None:
    lanes = {check.name: check for check in GATE.checks()}
    tests = lanes["soldr tests (isolated, host private root)"]
    assert tests.argv == (
        "uv",
        "run",
        "--no-project",
        "--python",
        "3.13",
        "python",
        "ci/workspace_test.py",
        "--host",
    )
    gate = tomllib.loads((ROOT / "local-gate.toml").read_text(encoding="utf-8"))
    assert tuple(gate["gate"]["isolation"]["runner"]) == tests.argv
    assert all("bosn" not in check.argv[0] for check in GATE.checks())
    for check in GATE.checks():
        if check.lane != "tests":
            assert "nextest" not in check.argv and "ci-test" not in check.argv, check


@dataclass(frozen=True)
class WrapperRun:
    returncode: int
    stderr: str
    stdout: str


def _shim(args: list[str], native: str | None, **extra: str) -> WrapperRun:
    """Run nextest_wrapper.sh with stderr captured through a file, not a pipe
    (zackees/ci.yml PY-003). ``native`` sets SOLDR_NEXTEST_NATIVE_WRAPPER."""
    env = {
        k: v
        for k, v in os.environ.items()
        if k
        not in ("CI", "SOLDR_TEST_ISOLATED", "SOLDR_NEXTEST_NATIVE_WRAPPER", "CARGO")
    }
    env.update(extra)
    if native is not None:
        env["SOLDR_NEXTEST_NATIVE_WRAPPER"] = native
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        proc = subprocess.run(
            ["sh", str(WRAPPER), *args],
            env=env,
            stdout=out,
            stderr=err,
            check=False,
        )
        out.seek(0)
        err.seek(0)
        return WrapperRun(
            proc.returncode,
            err.read().decode("utf-8", errors="replace"),
            out.read().decode("utf-8", errors="replace"),
        )


def _wrapper(**extra: str) -> WrapperRun:
    return _shim(["true"], "/bin/true", **extra)


def _echo_script(path: Path, label: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f'#!/bin/sh\necho "{label} $*"\n', encoding="utf-8")
    path.chmod(0o755)
    return path


@pytest.mark.skipif(os.name == "nt", reason="the nextest run-wrapper is Unix-only")
def test_isolation_guard_refuses_a_developer_host() -> None:
    refused = _wrapper()
    assert refused.returncode != 0
    assert "ci/workspace_test.py --host" in refused.stderr
    assert _wrapper(CI="true").returncode == 0
    assert _wrapper(SOLDR_TEST_ISOLATED="1").returncode == 0


@pytest.mark.skipif(os.name == "nt", reason="the nextest run-wrapper is Unix-only")
def test_isolation_guard_runs_before_any_wrapper_resolution(tmp_path: Path) -> None:
    native = _echo_script(tmp_path / "native", "native")
    refused = _shim(["true"], str(native))
    assert refused.returncode == 97
    assert "native" not in refused.stdout


@pytest.mark.skipif(os.name == "nt", reason="the nextest run-wrapper is Unix-only")
def test_an_explicit_native_wrapper_is_execed_with_the_test_argv(
    tmp_path: Path,
) -> None:
    """soldr#3454 resolution rule, step 1: SOLDR_NEXTEST_NATIVE_WRAPPER wins."""
    native = _echo_script(tmp_path / "native", "native")
    profile = tmp_path / "target" / "debug"
    _echo_script(profile / "soldr-nextest-wrapper", "derived")
    test_binary = _echo_script(profile / "deps" / "suite-0123", "test")
    run = _shim([str(test_binary), "--exact", "a::b"], str(native), CI="true")
    assert run.returncode == 0, run.stderr
    assert run.stdout.strip() == f"native {test_binary} --exact a::b"


@pytest.mark.skipif(os.name == "nt", reason="the nextest run-wrapper is Unix-only")
@pytest.mark.parametrize("runner", [[], ["/usr/bin/env"]])
def test_the_wrapper_is_found_beside_the_test_binary_profile_dir(
    tmp_path: Path, runner: list[str]
) -> None:
    """Step 2: `<profile>/deps/<test>` -> `<profile>/soldr-nextest-wrapper`.

    The same relative layout holds for a workspace build, a `--target` build
    (`<target>/<triple>/<profile>`) and an extracted Nextest archive, and the
    test binary is found even behind a target runner.
    """
    profile = tmp_path / "extract" / "target" / "x86_64-apple-darwin" / "ci-nextest"
    _echo_script(profile / "soldr-nextest-wrapper", "derived")
    test_binary = _echo_script(profile / "deps" / "suite-0123", "test")
    argv = [*runner, str(test_binary), "--exact", "a::b"]
    run = _shim(argv, None, SOLDR_TEST_ISOLATED="1")
    assert run.returncode == 0, run.stderr
    assert run.stdout.strip() == "derived " + " ".join(argv)


UNIX_ONLY = pytest.mark.skipif(
    os.name == "nt", reason="the nextest run-wrapper is Unix-only"
)


@UNIX_ONLY
def test_a_missing_wrapper_without_cargo_refuses_instead_of_running_unwrapped(
    tmp_path: Path,
) -> None:
    """Step 4: nothing beside the test binary and no $CARGO (an archive host,
    where a build must not be attempted) -> loud refusal."""
    test_binary = _echo_script(tmp_path / "target" / "debug" / "deps" / "t", "test")
    run = _shim([str(test_binary)], None, CI="true")
    assert run.returncode == 98
    assert "soldr-nextest-wrapper unavailable" in run.stderr
    assert "no $CARGO to build it with" in run.stderr
    assert "soldr cargo build -p soldr-nextest-wrapper" in run.stderr
    assert "test" not in run.stdout, "the test must not run unwrapped"


@UNIX_ONLY
def test_no_test_binary_under_deps_refuses(tmp_path: Path) -> None:
    run = _shim(["/bin/true"], None, CI="true", CARGO="/bin/false")
    assert run.returncode == 98
    assert "no test binary under a deps/ directory" in run.stderr


@dataclass(frozen=True)
class FakeCargo:
    """A fake `$CARGO` executable and the log of argv it was invoked with."""

    path: Path
    log: Path


def _fake_cargo(tmp_path: Path, *, build: bool = True) -> FakeCargo:
    """A `$CARGO` that logs its argv and (optionally) links an echo wrapper at
    `<--target-dir>/[<--target>/]<profile dir>/soldr-nextest-wrapper`."""
    log = tmp_path / "cargo.log"
    cargo = tmp_path / "bin" / "cargo-under-test"
    cargo.parent.mkdir(parents=True, exist_ok=True)
    body = (
        "#!/bin/sh\n"
        f'echo "$*" >> "{log}"\n'
        "sleep 0.3\n"
        'dir=""; triple=""; profile=""\n'
        "while [ $# -gt 0 ]; do\n"
        '  case "$1" in\n'
        '  --target-dir) dir="$2"; shift ;;\n'
        '  --target) triple="$2"; shift ;;\n'
        '  --profile) profile="$2"; shift ;;\n'
        "  esac\n"
        "  shift\n"
        "done\n"
        '[ "$profile" = test ] && profile=debug\n'
        '[ -n "$triple" ] && dir="$dir/$triple"\n'
    )
    if build:
        body += (
            'mkdir -p "$dir/$profile"\n'
            'out="$dir/$profile/soldr-nextest-wrapper"\n'
            'printf \'#!/bin/sh\\necho "built $*"\\n\' > "$out"\n'
            'chmod +x "$out"\n'
        )
    else:
        body += "exit 101\n"
    cargo.write_text(body, encoding="utf-8")
    cargo.chmod(0o755)
    return FakeCargo(path=cargo, log=log)


@UNIX_ONLY
@pytest.mark.parametrize(
    ("layout", "expected"),
    [
        (("debug",), ["--profile", "test"]),
        (
            ("x86_64-unknown-linux-gnu", "ci-nextest"),
            ["--profile", "ci-nextest", "--target", "x86_64-unknown-linux-gnu"],
        ),
    ],
)
def test_a_scoped_run_builds_the_wrapper_once_into_the_test_profile_dir(
    tmp_path: Path, layout: tuple[str, ...], expected: list[str]
) -> None:
    """Step 3 (soldr#3454): `nextest run -p soldr-cli` never builds the
    wrapper's package, so the first test builds it with the run's own $CARGO,
    for the profile and triple its own path names, and concurrent first tests
    share that one build."""
    target = tmp_path / "target"
    target.mkdir()
    # Cargo's rustc info cache marks the target root (a bosn volume has no
    # CACHEDIR.TAG, because Cargo did not create the directory).
    (target / ".rustc_info.json").write_text("{}")
    test_binary = _echo_script(target.joinpath(*layout, "deps", "t-0123"), "test")
    fake = _fake_cargo(tmp_path)
    cargo, log = fake.path, fake.log
    runs: list[WrapperRun] = []

    def one() -> None:
        runs.append(
            _shim(
                [str(test_binary), "--exact", "a::b"], None, CI="true", CARGO=str(cargo)
            )
        )

    threads = [threading.Thread(target=one) for _ in range(4)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    for run in runs:
        assert run.returncode == 0, run.stderr
        assert run.stdout.strip() == f"built {test_binary} --exact a::b"
    builds = log.read_text(encoding="utf-8").splitlines()
    assert len(builds) == 1, builds
    argv = builds[0].split()
    assert argv[:5] == [
        "build",
        "-p",
        "soldr-nextest-wrapper",
        "--bin",
        "soldr-nextest-wrapper",
    ]
    for flag, value in zip(expected[::2], expected[1::2], strict=True):
        assert argv[argv.index(flag) + 1] == value
    assert argv[argv.index("--target-dir") + 1] == str(target)
    assert not list(target.rglob(".soldr-nextest-wrapper.build-lock")), (
        "lock left behind"
    )


@UNIX_ONLY
def test_a_dashed_target_dir_name_is_not_mistaken_for_a_triple(
    tmp_path: Path,
) -> None:
    target = tmp_path / "my-target-dir"
    test_binary = _echo_script(target / "debug" / "deps" / "t", "test")
    fake = _fake_cargo(tmp_path)
    cargo, log = fake.path, fake.log
    run = _shim([str(test_binary)], None, CI="true", CARGO=str(cargo))
    assert run.returncode == 0, run.stderr
    argv = log.read_text(encoding="utf-8").split()
    assert "--target" not in argv
    assert argv[argv.index("--target-dir") + 1] == str(target)


@UNIX_ONLY
def test_a_failed_build_refuses_and_releases_its_lock(tmp_path: Path) -> None:
    profile = tmp_path / "target" / "debug"
    test_binary = _echo_script(profile / "deps" / "t", "test")
    cargo = _fake_cargo(tmp_path, build=False).path
    run = _shim([str(test_binary)], None, CI="true", CARGO=str(cargo))
    assert run.returncode == 98
    assert "did not produce" in run.stderr
    assert "test" not in run.stdout
    assert not (profile / ".soldr-nextest-wrapper.build-lock").exists()


@UNIX_ONLY
def test_a_dead_builders_lock_is_taken_over(tmp_path: Path) -> None:
    profile = tmp_path / "target" / "debug"
    test_binary = _echo_script(profile / "deps" / "t", "test")
    stale = profile / ".soldr-nextest-wrapper.build-lock"
    stale.mkdir()
    (stale / "pid").write_text("999999999\n")
    fake = _fake_cargo(tmp_path)
    cargo, log = fake.path, fake.log
    run = _shim([str(test_binary)], None, CI="true", CARGO=str(cargo))
    assert run.returncode == 0, run.stderr
    assert run.stdout.strip() == f"built {test_binary}"
    assert len(log.read_text(encoding="utf-8").splitlines()) == 1


def _workspace_test():  # type: ignore[no-untyped-def]
    return load_script_module(
        ROOT / "ci" / "workspace_test.py", "soldr_workspace_test_host"
    )


def test_host_run_moves_every_identity_root_off_the_callers_state(
    tmp_path: Path,
) -> None:
    """soldr#3516: the host run's daemons resolve a private root, and the
    marker is set only together with that root."""
    ws = _workspace_test()
    real_home = tmp_path / "real"
    layout = ws.host_layout(tmp_path / "gitdir")
    shims = real_home / ".soldr" / "v0.9.48" / "shims"
    caller = {
        "PATH": f"{shims}:{real_home / '.clud' / 'state' / 'shims'}:/bin",
        "CI": "true",
        "SOLDR_TEST_ISOLATED": "0",
    }
    env = ws.host_environment(layout, caller, real_home)
    assert env["SOLDR_TEST_ISOLATED"] == "1"
    assert "CI" not in env
    for name in ("HOME", "XDG_CONFIG_HOME", "TMPDIR", "SOLDR_CACHE_DIR"):
        assert not Path(env[name]).is_relative_to(real_home), name
    assert env["XDG_RUNTIME_DIR"] == str(layout.runtime)
    assert env["CARGO_HOME"] == str(real_home / ".cargo")
    assert env["PATH"] == "/bin"


def test_host_run_refuses_the_callers_home(tmp_path: Path) -> None:
    ws = _workspace_test()
    layout = ws.host_layout(tmp_path)
    with pytest.raises(ws.IsolationError):
        ws.host_environment(layout, {}, layout.home)


@pytest.mark.skipif(os.name == "nt", reason="the nextest run-wrapper is Unix-only")
def test_host_run_fails_closed_without_the_guard(tmp_path: Path) -> None:
    ws = _workspace_test()
    ws.assert_guard_refuses(ROOT, dict(os.environ))
    (tmp_path / ".github" / "scripts").mkdir(parents=True)
    (tmp_path / ".github" / "scripts" / "nextest_wrapper.sh").write_text(
        "exit 0\n", encoding="utf-8"
    )
    with pytest.raises(ws.IsolationError):
        ws.assert_guard_refuses(tmp_path, dict(os.environ))


def test_main_publisher_promotes_merged_pr_evidence() -> None:
    """Merged PR trailers become main cache evidence without skipping main jobs."""
    pre = (ROOT / ".github" / "workflows" / "ci-pre.yml").read_text(encoding="utf-8")
    assert "--github-context" in pre
    assert "GITHUB_TOKEN: ${{ github.token }}" in pre
    assert "args=" not in pre.split("  attestations:", 1)[1]
    assert f"ref: {GATE.CI_LINT_REF}" in pre


def test_fleet_evidence_dumps_are_not_tracked(tmp_path: Path) -> None:
    """soldr#3946: bosn fleet run dumps are run metadata, never source.

    PY-003 forbids capture_output here, so the listing goes to a file.
    """
    ignore = (ROOT / ".gitignore").read_text(encoding="utf-8")
    assert "/ci/fleet-evidence/" in ignore
    listing = tmp_path / "ls-files.txt"
    with listing.open("w", encoding="utf-8") as handle:
        subprocess.run(
            ["git", "ls-files", "ci/fleet-evidence"],
            cwd=ROOT,
            stdout=handle,
            check=True,
        )
    assert listing.read_text(encoding="utf-8").strip() == ""
