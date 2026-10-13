from __future__ import annotations

import ast
import json
import re
from dataclasses import dataclass
from pathlib import Path

from conftest import COOK_CACHE_ALLOWLIST_INPUT, WORKSPACE_CRATES, load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "actions" / "setup-soldr" / "resolve_setup.py"


def _load_module():
    return load_script_module(SCRIPT_PATH, "resolve_setup")


def test_resolve_setup_uses_token_for_current_repo(monkeypatch) -> None:
    module = _load_module()
    monkeypatch.setenv("GITHUB_TOKEN", "repo-token")
    monkeypatch.setenv("GITHUB_REPOSITORY", "zackees/soldr")
    monkeypatch.delenv("SETUP_SOLDR_GITHUB_TOKEN", raising=False)

    assert module._github_token_for_repo("zackees/soldr") == "repo-token"


def test_resolve_setup_skips_repo_token_for_cross_repo(monkeypatch) -> None:
    module = _load_module()
    monkeypatch.setenv("GITHUB_TOKEN", "repo-token")
    monkeypatch.setenv("GITHUB_REPOSITORY", "caller/app")
    monkeypatch.delenv("SETUP_SOLDR_GITHUB_TOKEN", raising=False)

    assert module._github_token_for_repo("zackees/soldr") == ""


def test_resolve_setup_accepts_explicit_setup_token(monkeypatch) -> None:
    module = _load_module()
    monkeypatch.setenv("GITHUB_TOKEN", "repo-token")
    monkeypatch.setenv("GITHUB_REPOSITORY", "caller/app")
    monkeypatch.setenv("SETUP_SOLDR_GITHUB_TOKEN", "explicit-token")

    assert module._github_token_for_repo("zackees/soldr") == "explicit-token"


def test_load_toolchain_spec_reads_rust_toolchain_toml() -> None:
    module = _load_module()

    spec = module.load_toolchain_spec(REPO_ROOT, "rust-toolchain.toml", "")

    assert spec.channel == "1.98.1"
    assert spec.profile == "minimal"
    assert "rustfmt" in spec.components
    assert "clippy" in spec.components
    assert spec.source == "rust-toolchain.toml"
    assert spec.file_hash != "none"


def test_load_toolchain_spec_defaults_when_file_missing(tmp_path: Path) -> None:
    module = _load_module()

    spec = module.load_toolchain_spec(tmp_path, "missing.toml", "")

    assert spec == module.ToolchainSpec(
        channel="stable",
        profile="minimal",
        components=(),
        targets=(),
        source="default",
        file_hash="none",
    )


def test_load_toolchain_spec_prefers_explicit_override() -> None:
    module = _load_module()

    spec = module.load_toolchain_spec(REPO_ROOT, "rust-toolchain.toml", "stable")

    assert spec.channel == "stable"
    assert spec.profile == "minimal"
    assert spec.source == "input"


def test_normalize_list_accepts_csv_strings() -> None:
    module = _load_module()

    value = module._normalize_list("rustfmt, clippy, ")

    assert value == ["rustfmt", "clippy"]


def test_toolchain_signature_payload_is_json_serializable() -> None:
    module = _load_module()

    spec = module.load_toolchain_spec(REPO_ROOT, "rust-toolchain.toml", "")

    payload = {
        "channel": spec.channel,
        "profile": spec.profile,
        "components": list(spec.components),
        "targets": list(spec.targets),
        "source": spec.source,
        "file_hash": spec.file_hash,
        "soldr_repo": "zackees/soldr",
        "soldr_version": "0.7.4",
    }

    assert json.loads(json.dumps(payload))["channel"] == "1.98.1"


def test_action_python_helpers_have_entrypoints() -> None:
    action_text = (REPO_ROOT / "action.yml").read_text(encoding="utf-8")
    script_paths = sorted(
        {
            REPO_ROOT / match.group(1)
            for match in re.finditer(
                r"github\.action_path \}\}/([^\"']+\.py)", action_text
            )
        }
    )

    assert script_paths, "expected action.yml to invoke Python helper scripts"
    for script_path in script_paths:
        tree = ast.parse(script_path.read_text(encoding="utf-8"))
        assert any(
            isinstance(node, ast.FunctionDef) and node.name == "main"
            for node in tree.body
        ), f"{script_path.relative_to(REPO_ROOT)} should define main()"
        assert any(
            isinstance(node, ast.If)
            and isinstance(node.test, ast.Compare)
            and isinstance(node.test.left, ast.Name)
            and node.test.left.id == "__name__"
            and any(
                isinstance(comparator, ast.Constant) and comparator.value == "__main__"
                for comparator in node.test.comparators
            )
            for node in tree.body
        ), f"{script_path.relative_to(REPO_ROOT)} should invoke main() as a script"


def test_setup_soldr_smoke_tests_disable_nested_cache() -> None:
    workflow = (
        REPO_ROOT / ".github" / "workflows" / "setup-soldr-action.yml"
    ).read_text(encoding="utf-8")

    assert "Remove-Item Env:ZCCACHE_CACHE_DIR" in workflow
    assert "$binaryName = Split-Path -Leaf (Get-Command soldr).Source" in workflow
    assert "[System.IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)" in workflow
    assert "$rustcVersion = & soldr rustc -vV" in workflow
    assert "$rustcExitCode = $LASTEXITCODE" in workflow
    assert "$hostTriple = $hostLine -replace '^host:\\s*', ''" in workflow
    assert "$candidates = if ($IsWindows)" in workflow
    assert 'Join-Path "debug" $binaryName' in workflow
    assert 'Join-Path (Join-Path $hostTriple "debug") $binaryName' in workflow
    assert '$stagingDir = Join-Path $env:RUNNER_TEMP "soldr-under-test"' in workflow
    assert "Copy-Item -LiteralPath $builtSoldr -Destination $soldrUnderTest" in workflow
    assert "$versionJson = & $soldrUnderTest version --json" in workflow
    assert ".soldr_version" in workflow
    assert "Get-Content package.json -Raw" in workflow
    assert (
        "& $soldrUnderTest --no-cache cargo nextest run -p soldr-cli --tests"
        in workflow
    )
    assert "id: dogfood-build-cache" in workflow
    assert "setup-soldr-dogfood-zccache-v1-" in workflow
    assert "dogfood-build-cache-hit=" in workflow
    assert "dogfood-build-seconds=" in workflow
    assert "dogfood-test-seconds=" in workflow
    assert "Stop dogfood zccache before cache save" in workflow
    assert "& $soldr.Source zccache stop" in workflow
    assert "ACTION_CACHE_DIR: ${{ steps.setup.outputs.cache-dir }}" not in workflow
    assert "[System.IO.Path]::GetFullPath($env:SOLDR_CACHE_DIR)" in workflow
    assert 'CARGO_BUILD_JOBS: "1"' in workflow
    assert 'SOLDR_JOBS: "1"' in workflow
    assert "Enlarge swap (OOM headroom)" in workflow


def test_no_workflow_opts_out_of_the_reentrancy_guard() -> None:
    """soldr#2739 inverted this check.

    It used to assert that the smoke job *exported*
    ``SOLDR_REENTRANCY_GUARD: strict``. Enforcement is the default now, so
    that export is redundant and requiring it would recreate the fragility
    soldr#2698 exposed, where a new lane escaped the sweep and ran unguarded.

    The live risk runs the other way: a lane can only lose enforcement by
    actively opting out, so that is what is pinned.
    """
    workflows = (REPO_ROOT / ".github" / "workflows").glob("*.yml")
    offenders = [
        path.name
        for path in workflows
        if re.search(
            r"SOLDR_REENTRANCY_GUARD\s*[:=]\s*[\"']?off",
            path.read_text(encoding="utf-8"),
        )
    ]
    assert not offenders, (
        "these workflows disable the re-entrancy guard; the hatch is "
        f"emergency-only and must not be committed: {offenders}"
    )


def test_cache_delta_experiment_quiesces_before_packaging() -> None:
    setup_action = (
        REPO_ROOT / ".github" / "actions" / "cache-delta-experiment" / "action.yml"
    ).read_text(encoding="utf-8")
    cleanup_action = (
        REPO_ROOT
        / ".github"
        / "actions"
        / "cache-delta-experiment-cleanup"
        / "action.yml"
    ).read_text(encoding="utf-8")
    workflow = (
        REPO_ROOT / ".github" / "workflows" / "cache-delta-experiment.yml"
    ).read_text(encoding="utf-8")
    shutdown_command = (
        "soldr cache shutdown --shutdown-timeout-seconds 60 --json "
        "|| soldr zccache stop || true"
    )

    assert "Flush + shutdown cache before packaging delta" in setup_action
    assert "soldr cache flush --json || true" in setup_action
    assert shutdown_command in setup_action
    assert re.search(
        r"- name: Package build delta\s+id: delta-tar\s+if: always\(\)",
        setup_action,
    )
    assert re.search(
        r"- name: Stash state for cleanup action\s+if: always\(\)",
        setup_action,
    )

    assert "Quiesce cache before cleanup packaging" in cleanup_action
    assert "soldr cache flush --json || true" in cleanup_action
    assert shutdown_command in cleanup_action
    assert 'find . -type f -print0 > "$LIST"' in cleanup_action
    assert 'tar --null -T "$LIST"' in cleanup_action
    assert (
        "tar --use-compress-program='zstd -19 -T0' -cf \"$ARCHIVE\" ."
        not in cleanup_action
    )

    assert "Flush + shutdown baseline cache" in workflow
    # soldr#3386: the workflow no longer hides both failures behind
    # `|| true`; it tries shutdown, warns, then falls back to zccache stop.
    assert (
        "if ! soldr cache shutdown --shutdown-timeout-seconds 60 --json; then"
        in workflow
    )
    assert "soldr zccache stop ||" in workflow
    assert 'rm -f "${ZCCACHE_CACHE_DIR}/daemon.sock"' in workflow
    assert 'find . -type f -print0 > "$list"' in workflow
    assert 'tar --null -T "$list"' in workflow
    assert (
        'tar --use-compress-program=\'zstd -19 -T0\' -cf "$archive" -C "$zccache_dir" .'
        not in workflow
    )

    assert setup_action.index(
        "Flush + shutdown cache before packaging delta"
    ) < setup_action.index("Package build delta")
    assert cleanup_action.index(
        "Quiesce cache before cleanup packaging"
    ) < cleanup_action.index("Package cook layer")


def test_build_and_test_documents_removed_windows_pdb_artifacts() -> None:
    workflow = (REPO_ROOT / ".github" / "workflows" / "_build-and-test.yml").read_text(
        encoding="utf-8"
    )

    assert "soldr#1335" in workflow
    assert "Upload Windows test PDBs on failure" in workflow
    assert "CARGO_PROFILE_DEV_DEBUG" not in workflow
    assert "CARGO_PROFILE_TEST_DEBUG" not in workflow
    assert "target/${{ inputs.target }}/debug/**/*.pdb" not in workflow


def test_cross_build_nextest_archive_uses_ci_nextest_profile() -> None:
    workflow = (
        REPO_ROOT / ".github" / "workflows" / "_ci-cross-build-linux.yml"
    ).read_text(encoding="utf-8")
    cargo_toml = (REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8")

    assert "cargo nextest archive" in workflow
    assert 'ci_profile="ci-nextest"' in workflow
    assert '--cargo-profile "$ci_profile"' in workflow
    assert "CARGO_PROFILE_TEST_DEBUG: line-tables-only" in workflow
    assert "[profile.ci-nextest]" in cargo_toml
    assert "debug-assertions = true" in cargo_toml
    assert 'inherits = "ci-release"' in cargo_toml
    assert 'debug = "line-tables-only"' in cargo_toml
    assert 'strip = "none"' in cargo_toml


def test_ci_only_profiles_avoid_full_release_codegen() -> None:
    cargo_toml = (REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8")

    ci_bootstrap = re.search(
        r"(?ms)^\[profile\.ci-bootstrap\]\n(?P<body>.*?)(?=^\[)",
        cargo_toml,
    )
    ci_release = re.search(
        r"(?ms)^\[profile\.ci-release\]\n(?P<body>.*?)(?=^\[)",
        cargo_toml,
    )
    ci_nextest = re.search(
        r"(?ms)^\[profile\.ci-nextest\]\n(?P<body>.*?)(?=^\[)",
        cargo_toml,
    )

    assert ci_bootstrap is not None
    assert ci_release is not None
    assert ci_nextest is not None
    assert "opt-level = 0" in ci_bootstrap.group("body")
    assert "lto = false" in ci_bootstrap.group("body")
    assert "opt-level = 1" in ci_release.group("body")
    assert "lto = false" in ci_release.group("body")
    assert "opt-level = 0" in ci_nextest.group("body")


def test_cross_build_uses_deferred_cook_after_target_setup() -> None:
    workflow = (
        REPO_ROOT / ".github" / "workflows" / "_ci-cross-build-linux.yml"
    ).read_text(encoding="utf-8")

    prepare_idx = workflow.index("Prepare blessed target env for cook")
    cook_idx = workflow.index("Restore cooked dependency cache")
    clean_idx = workflow.index("Clean cooked first-party stubs")
    build_idx = workflow.index("Cross-build soldr (ci-nextest profile)")
    cook_step = workflow[cook_idx:clean_idx]
    clean_step = workflow[clean_idx:build_idx]

    assert "uses: zackees/setup-soldr/cook@" in cook_step
    assert (
        "flags: --profile ci-nextest --target ${{ inputs.target }} --workspace"
        in cook_step
    )
    assert (
        'soldr prepare --target "${{ inputs.target }}" --github-env "$GITHUB_ENV"'
        in workflow
    )
    # soldr#2996: cook is gated by an explicit allowlist, not by falling
    # through an exclusion list. Pinning the literal keeps a lane from
    # gaining cook silently -- adding one has to be a visible edit here too.
    # soldr#3121 added aarch64-pc-windows-msvc, x86_64-pc-windows-msvc and
    # aarch64-unknown-linux-gnu to the allowlist.
    assert COOK_CACHE_ALLOWLIST_INPUT in cook_step
    assert 'profile="ci-nextest"' in clean_step
    for package in WORKSPACE_CRATES:
        assert f"-p {package}" in clean_step
    assert '--target "$target" --profile "$profile"' in clean_step
    assert "SOLDR_BINARY=$RUNNER_TEMP/soldr-bin/soldr" in workflow

    assert prepare_idx < cook_idx < clean_idx < build_idx


def test_main_creates_cache_layout_and_outputs(tmp_path: Path, monkeypatch) -> None:
    module = _load_module()
    workspace = tmp_path / "workspace"
    runner_temp = tmp_path / "runner-temp"
    workspace.mkdir()
    runner_temp.mkdir()

    github_env = tmp_path / "github.env"
    github_output = tmp_path / "github.output"
    github_path = tmp_path / "github.path"

    monkeypatch.setenv("ACTION_WORKSPACE", str(workspace))
    monkeypatch.setenv("RUNNER_TEMP", str(runner_temp))
    monkeypatch.setenv("ACTION_OS", "Linux")
    monkeypatch.setenv("ACTION_ARCH", "X64")
    monkeypatch.setenv("INPUT_REPO", "zackees/soldr")
    monkeypatch.setenv("INPUT_VERSION", "")
    monkeypatch.setenv("INPUT_CACHE_DIR", "")
    monkeypatch.setenv("INPUT_CACHE_KEY_SUFFIX", "")
    monkeypatch.setenv("INPUT_TOOLCHAIN", "")
    monkeypatch.setenv("INPUT_TOOLCHAIN_FILE", "missing.toml")
    monkeypatch.setenv("INPUT_TRUST_MODE", "")
    monkeypatch.setenv("INPUT_TARGET_DIR", "custom-target")
    monkeypatch.setenv("GITHUB_SHA", "abc123")
    monkeypatch.setenv("GITHUB_ENV", str(github_env))
    monkeypatch.setenv("GITHUB_OUTPUT", str(github_output))
    monkeypatch.setenv("GITHUB_PATH", str(github_path))
    monkeypatch.setattr(module, "resolve_latest_soldr_release", lambda _repo: "")

    module.main()

    cache_root = runner_temp / "setup-soldr"
    assert cache_root.is_dir()
    assert (cache_root / "soldr").is_dir()
    assert (cache_root / "soldr" / "cache").is_dir()
    assert (cache_root / "soldr" / "cache" / "zccache").is_dir()
    assert (cache_root / "soldr" / "bin").is_dir()
    assert (cache_root / "cargo").is_dir()
    assert (cache_root / "cargo" / "bin").is_dir()
    assert (cache_root / "rustup").is_dir()
    assert (cache_root / "bin").is_dir()
    assert (cache_root / "shims").is_dir()

    outputs = github_output.read_text(encoding="utf-8")
    assert f"cache_root={cache_root}" in outputs
    assert "cache_key=setup-soldr-v0-linux-x64-" in outputs
    assert "cache_restore_prefix=setup-soldr-v0-linux-x64-" in outputs
    assert "build_cache_key=setup-soldr-buildcache-v1-linux-x64-" in outputs
    assert (
        "build_cache_restore_key_toolchain=setup-soldr-buildcache-v1-linux-x64-"
        in outputs
    )
    assert (
        "build_cache_restore_key_os_arch=setup-soldr-buildcache-v1-linux-x64-"
        in outputs
    )
    assert f"build_cache_path={cache_root / 'soldr' / 'cache' / 'zccache'}" in outputs
    assert f"shim_dir={cache_root / 'shims'}" in outputs
    assert "target_cache_enabled=false" in outputs
    assert "target_cache_mode=off" in outputs
    assert outputs.count("-abc123") >= 1
    assert "toolchain=stable" in outputs

    env_text = github_env.read_text(encoding="utf-8")
    assert f"SOLDR_CACHE_DIR={cache_root / 'soldr'}" in env_text
    assert f"ZCCACHE_CACHE_DIR={cache_root / 'soldr' / 'cache' / 'zccache'}" in env_text


@dataclass(frozen=True)
class _MainRun:
    outputs: str
    env_text: str


def _run_main_without_target_cache_inputs(
    tmp_path: Path, monkeypatch, *, cache: str | None, mode: str | None
) -> _MainRun:
    module = _load_module()
    workspace = tmp_path / "workspace"
    runner_temp = tmp_path / "runner-temp"
    workspace.mkdir()
    runner_temp.mkdir()
    (workspace / "Cargo.lock").write_text("# lock\n", encoding="utf-8")
    (workspace / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")

    github_env = tmp_path / "github.env"
    github_output = tmp_path / "github.output"
    github_path = tmp_path / "github.path"

    monkeypatch.setenv("ACTION_WORKSPACE", str(workspace))
    monkeypatch.setenv("RUNNER_TEMP", str(runner_temp))
    monkeypatch.setenv("ACTION_OS", "Linux")
    monkeypatch.setenv("ACTION_ARCH", "X64")
    monkeypatch.setenv("INPUT_REPO", "zackees/soldr")
    monkeypatch.setenv("INPUT_VERSION", "")
    monkeypatch.setenv("INPUT_CACHE_DIR", "")
    monkeypatch.setenv("INPUT_CACHE_KEY_SUFFIX", "")
    monkeypatch.setenv("INPUT_TOOLCHAIN", "")
    monkeypatch.setenv("INPUT_TOOLCHAIN_FILE", "missing.toml")
    monkeypatch.setenv("INPUT_TRUST_MODE", "")
    for name, value in (
        ("INPUT_TARGET_CACHE", cache),
        ("INPUT_TARGET_CACHE_MODE", mode),
    ):
        if value is None:
            monkeypatch.delenv(name, raising=False)
        else:
            monkeypatch.setenv(name, value)
    monkeypatch.setenv("INPUT_TARGET_DIR", "target")
    monkeypatch.setenv("GITHUB_SHA", "abc123")
    monkeypatch.setenv("GITHUB_ENV", str(github_env))
    monkeypatch.setenv("GITHUB_OUTPUT", str(github_output))
    monkeypatch.setenv("GITHUB_PATH", str(github_path))
    monkeypatch.setattr(module, "resolve_latest_soldr_release", lambda _repo: "")

    module.main()
    return _MainRun(
        outputs=github_output.read_text(encoding="utf-8"),
        env_text=github_env.read_text(encoding="utf-8"),
    )


def test_target_cache_defaults_off_and_writes_no_target_cache_env(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    run = _run_main_without_target_cache_inputs(
        tmp_path, monkeypatch, cache=None, mode=None
    )
    outputs, env_text = run.outputs, run.env_text
    assert "target_cache_enabled=false" in outputs
    assert "target_cache_mode=off" in outputs
    assert "SOLDR_TARGET_CACHE_" not in env_text
    assert "deprecated" not in capsys.readouterr().out


def test_target_cache_inputs_are_deprecated_noops(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    run = _run_main_without_target_cache_inputs(
        tmp_path, monkeypatch, cache="true", mode="full"
    )
    outputs, env_text = run.outputs, run.env_text
    assert "target_cache_enabled=false" in outputs
    assert "target_cache_mode=off" in outputs
    assert "SOLDR_TARGET_CACHE_" not in env_text
    captured = capsys.readouterr()
    text = captured.out + captured.err
    assert "target-cache" in text
    assert "soldr#2996" in text


@dataclass(frozen=True)
class SetupPaths:
    """The GitHub env/output/path files `_setup_main_env` points the action at."""

    github_env: Path
    github_output: Path
    github_path: Path


@dataclass(frozen=True)
class OutputLine:
    """One `name=value` line of a GITHUB_OUTPUT file."""

    name: str
    value: str


@dataclass(frozen=True)
class ActionOutputs:
    """The parsed `name=value` lines of a GITHUB_OUTPUT file (last one wins)."""

    lines: tuple[OutputLine, ...]

    def value(self, name: str) -> str:
        for line in reversed(self.lines):
            if line.name == name:
                return line.value
        raise KeyError(name)


def _setup_main_env(
    tmp_path: Path,
    monkeypatch,
    *,
    version: str = "",
) -> SetupPaths:
    workspace = tmp_path / "workspace"
    runner_temp = tmp_path / "runner-temp"
    workspace.mkdir()
    runner_temp.mkdir()

    github_env = tmp_path / "github.env"
    github_output = tmp_path / "github.output"
    github_path = tmp_path / "github.path"

    monkeypatch.setenv("ACTION_WORKSPACE", str(workspace))
    monkeypatch.setenv("RUNNER_TEMP", str(runner_temp))
    monkeypatch.setenv("ACTION_OS", "Linux")
    monkeypatch.setenv("ACTION_ARCH", "X64")
    monkeypatch.setenv("INPUT_REPO", "zackees/soldr")
    monkeypatch.setenv("INPUT_VERSION", version)
    monkeypatch.setenv("INPUT_CACHE_DIR", "")
    monkeypatch.setenv("INPUT_CACHE_KEY_SUFFIX", "")
    monkeypatch.setenv("INPUT_TOOLCHAIN", "")
    monkeypatch.setenv("INPUT_TOOLCHAIN_FILE", "missing.toml")
    monkeypatch.setenv("INPUT_TRUST_MODE", "")
    monkeypatch.setenv("INPUT_TARGET_DIR", "target")
    monkeypatch.setenv("GITHUB_SHA", "abc123")
    monkeypatch.setenv("GITHUB_ENV", str(github_env))
    monkeypatch.setenv("GITHUB_OUTPUT", str(github_output))
    monkeypatch.setenv("GITHUB_PATH", str(github_path))
    return SetupPaths(github_env, github_output, github_path)


def _collect_outputs(output_path: Path) -> ActionOutputs:
    return ActionOutputs(
        tuple(
            OutputLine(*line.split("=", 1))
            for line in output_path.read_text(encoding="utf-8").splitlines()
            if "=" in line
        )
    )


def test_latest_version_cache_key_changes_when_release_changes(
    tmp_path: Path, monkeypatch
) -> None:
    """When input is `latest`, the cache key must be derived from the
    resolved release tag so that a new soldr release invalidates the
    setup cache and managed zccache binaries get refreshed. See #214."""
    module = _load_module()

    calls = {"count": 0}

    def fake_resolver_0(_repo: str) -> str:
        calls["count"] += 1
        return "0.7.11"

    run_dir_a = tmp_path / "run-a"
    run_dir_a.mkdir()
    output_a = _setup_main_env(run_dir_a, monkeypatch).github_output
    monkeypatch.setattr(module, "resolve_latest_soldr_release", fake_resolver_0)
    module.main()
    outputs_a = _collect_outputs(output_a)

    run_dir_b = tmp_path / "run-b"
    run_dir_b.mkdir()
    output_b = _setup_main_env(run_dir_b, monkeypatch).github_output
    monkeypatch.setattr(module, "resolve_latest_soldr_release", lambda _repo: "0.7.12")
    module.main()
    outputs_b = _collect_outputs(output_b)

    assert outputs_a.value("cache_key") != outputs_b.value("cache_key")
    assert outputs_a.value("soldr_version_resolved") == "0.7.11"
    assert outputs_b.value("soldr_version_resolved") == "0.7.12"
    assert outputs_a.value("soldr_version_requested") == ""
    assert calls["count"] == 1


def test_latest_version_cache_key_falls_back_to_literal_when_resolution_fails(
    tmp_path: Path, monkeypatch
) -> None:
    """If the GitHub API is unreachable, the resolver returns an empty
    string; the action must still produce a deterministic cache key so
    the warm-run pipeline keeps working. See #214."""
    module = _load_module()

    run_dir_a = tmp_path / "run-a"
    run_dir_a.mkdir()
    output_a = _setup_main_env(run_dir_a, monkeypatch).github_output
    monkeypatch.setattr(module, "resolve_latest_soldr_release", lambda _repo: "")
    module.main()
    outputs_a = _collect_outputs(output_a)

    run_dir_b = tmp_path / "run-b"
    run_dir_b.mkdir()
    output_b = _setup_main_env(run_dir_b, monkeypatch).github_output
    monkeypatch.setattr(module, "resolve_latest_soldr_release", lambda _repo: "")
    module.main()
    outputs_b = _collect_outputs(output_b)

    assert outputs_a.value("cache_key") == outputs_b.value("cache_key")
    assert outputs_a.value("soldr_version_resolved") == ""


def test_pinned_version_input_skips_latest_resolution(
    tmp_path: Path, monkeypatch
) -> None:
    """A pinned `version: 0.7.10` input must not trigger a GitHub API
    call and must bake that exact version into the cache key."""
    module = _load_module()
    calls = {"count": 0}

    def _should_not_call(_repo: str) -> str:
        calls["count"] += 1
        return "should-not-be-used"

    github_output = _setup_main_env(
        tmp_path, monkeypatch, version="0.7.10"
    ).github_output
    monkeypatch.setattr(module, "resolve_latest_soldr_release", _should_not_call)
    module.main()

    outputs = _collect_outputs(github_output)
    assert calls["count"] == 0
    assert outputs.value("soldr_version_resolved") == "0.7.10"
    assert outputs.value("soldr_version_requested") == "0.7.10"


def test_native_cache_default_on_writes_no_env(tmp_path: Path, monkeypatch) -> None:
    """Default-on native-cache: the action MUST NOT write SOLDR_NATIVE_CACHE
    to GITHUB_ENV. The soldr runtime's unset-is-truthy policy keeps native
    wrapping enabled. Writing the env var unconditionally would mask user
    overrides that pre-populate it (e.g. `env: SOLDR_NATIVE_CACHE=0` on a
    later step)."""
    module = _load_module()
    paths = _setup_main_env(tmp_path, monkeypatch)
    github_env, github_output = paths.github_env, paths.github_output
    monkeypatch.setattr(module, "resolve_latest_soldr_release", lambda _repo: "")
    module.main()

    env_text = github_env.read_text(encoding="utf-8")
    assert "SOLDR_NATIVE_CACHE" not in env_text

    outputs = _collect_outputs(github_output)
    assert outputs.value("native_cache_enabled") == "true"


def test_native_cache_explicit_false_writes_opt_out(
    tmp_path: Path, monkeypatch
) -> None:
    """`native-cache: false` must write `SOLDR_NATIVE_CACHE=0` to GITHUB_ENV
    so every later `soldr cargo ...` step in the job sees the opt-out and
    skips native compiler wrapping. The corresponding output flips to
    `false` so workflows can echo the effective policy."""
    module = _load_module()
    paths = _setup_main_env(tmp_path, monkeypatch)
    github_env, github_output = paths.github_env, paths.github_output
    monkeypatch.setenv("INPUT_NATIVE_CACHE", "false")
    monkeypatch.setattr(module, "resolve_latest_soldr_release", lambda _repo: "")
    module.main()

    env_text = github_env.read_text(encoding="utf-8")
    assert "SOLDR_NATIVE_CACHE=0\n" in env_text

    outputs = _collect_outputs(github_output)
    assert outputs.value("native_cache_enabled") == "false"


def test_native_cache_explicit_true_does_not_write_env(
    tmp_path: Path, monkeypatch
) -> None:
    module = _load_module()
    paths = _setup_main_env(tmp_path, monkeypatch)
    github_env, github_output = paths.github_env, paths.github_output
    monkeypatch.setenv("INPUT_NATIVE_CACHE", "true")
    monkeypatch.setattr(module, "resolve_latest_soldr_release", lambda _repo: "")
    module.main()

    env_text = github_env.read_text(encoding="utf-8")
    assert "SOLDR_NATIVE_CACHE" not in env_text
    outputs = _collect_outputs(github_output)
    assert outputs.value("native_cache_enabled") == "true"


def test_native_cache_invalid_value_rejected(tmp_path: Path, monkeypatch) -> None:
    """A typo like `native-cache: maybe` must fail loudly. Silently
    falling back to `true` here would hide the misconfiguration on
    cached/warm runs."""
    import pytest

    module = _load_module()
    _setup_main_env(tmp_path, monkeypatch)
    monkeypatch.setenv("INPUT_NATIVE_CACHE", "maybe")
    monkeypatch.setattr(module, "resolve_latest_soldr_release", lambda _repo: "")

    with pytest.raises(RuntimeError, match="invalid native-cache value"):
        module.main()


def test_normalize_bool_input_accepts_canonical_spellings() -> None:
    module = _load_module()
    for v in ("1", "true", "TRUE", "Yes", " on "):
        assert module.normalize_bool_input(v, name="x", default=False) is True
    for v in ("0", "false", "FALSE", "No", " off "):
        assert module.normalize_bool_input(v, name="x", default=True) is False
    assert module.normalize_bool_input("", name="x", default=True) is True
    assert module.normalize_bool_input("   ", name="x", default=False) is False


def test_normalize_release_tag_strips_v_prefix() -> None:
    module = _load_module()
    assert module._normalize_release_tag("v0.7.11") == "0.7.11"
    assert module._normalize_release_tag("0.7.11") == "0.7.11"
    assert module._normalize_release_tag(" v1.0.0 ") == "1.0.0"
    assert module._normalize_release_tag("") == ""


def test_is_empty_or_latest_matches_expected_values() -> None:
    module = _load_module()
    assert module._is_empty_or_latest("")
    assert module._is_empty_or_latest("  ")
    assert module._is_empty_or_latest("latest")
    assert module._is_empty_or_latest("LATEST")
    assert not module._is_empty_or_latest("v0.7.10")
    assert not module._is_empty_or_latest("0.7.10")
