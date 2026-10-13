"""Regression locks for checkout-built Soldr isolation in CI smokes."""

from pathlib import Path
from tempfile import TemporaryDirectory
from unittest import mock

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"


def _fill_outer_env(env: dict[str, str]) -> None:
    env.update(
        {
            "PATH": "/tools",
            "RUSTC_WRAPPER": "/setup-soldr/shims/rustc",
            "RUSTC_WORKSPACE_WRAPPER": "/setup-soldr/shims/workspace-rustc",
            "SOLDR_BROKER_SERVICE": "old-route",
            "SOLDR_INTERNAL_DAEMON_EXE": "/setup-soldr/soldr-daemon",
        }
    )


def _assert_isolated(env: dict[str, str]) -> None:
    assert env == {"PATH": "/tools"}


def test_gnu_proof_uses_checkout_owned_wrapper_and_daemon() -> None:
    module = load_script_module(
        SCRIPTS / "gnu_linux_toolchain_e2e.py", "gnu_linux_toolchain_e2e_env"
    )
    source: dict[str, str] = {}
    _fill_outer_env(source)
    _assert_isolated(module.fresh_checkout_env(source))
    assert source["SOLDR_BROKER_SERVICE"] == "old-route"
    env: dict[str, str] = {}
    _fill_outer_env(env)
    with mock.patch.object(module.subprocess, "run") as run:
        module.stop_soldr_broker("soldr", env)
    # Routes are path-derived (soldr#2479); the checkout-built binary's
    # broker is the isolated one, so no program name is passed.
    run.assert_called_once_with(
        ["soldr", "broker", "stop"],
        env=env,
        timeout=20,
        check=False,
    )


def test_pep517_smoke_uses_wheel_owned_wrapper_and_daemon() -> None:
    module = load_script_module(
        SCRIPTS / "pep517_daemon_smoke.py", "pep517_daemon_smoke_env"
    )
    source: dict[str, str] = {}
    _fill_outer_env(source)
    scrubbed = dict(source)
    module.scrub_outer_soldr_env(scrubbed)
    _assert_isolated(scrubbed)
    assert source["SOLDR_BROKER_SERVICE"] == "old-route"
    env: dict[str, str] = {}
    _fill_outer_env(env)
    with mock.patch.object(module.subprocess, "run") as run:
        module.stop_soldr_broker(Path("soldr"), env)
    run.assert_called_once_with(
        ["soldr", "broker", "stop"],
        env=env,
        timeout=20,
        check=False,
    )


def test_pep517_smoke_preserves_root_daemon_spawn_log() -> None:
    module = load_script_module(
        SCRIPTS / "pep517_daemon_smoke.py", "pep517_daemon_smoke_logs"
    )
    with TemporaryDirectory() as tmp_str:
        tmp = Path(tmp_str)
        cache_dir = tmp / "cache"
        cache_dir.mkdir()
        (cache_dir / "daemon-spawn.log").write_text("daemon startup failed")
        fetched = cache_dir / "bin" / "syslib"
        fetched.mkdir(parents=True)
        (fetched / "license.txt").write_text("not a Soldr log")
        log_dir = tmp / "artifacts"

        module.archive_soldr_logs(cache_dir, log_dir)

        assert (
            log_dir / "cache" / "daemon-spawn.log"
        ).read_text() == "daemon startup failed"
        assert not (log_dir / "cache" / "bin" / "syslib" / "license.txt").exists()


def test_pep517_failure_summary_has_searchable_log_markers() -> None:
    module = load_script_module(
        SCRIPTS / "pep517_daemon_smoke.py", "pep517_daemon_smoke_failure_summary"
    )
    error = module.subprocess.CalledProcessError(1, ["python", "-m", "pip", "wheel"])

    summary = module.failure_summary(error, Path("/tmp/pep517-soldr-logs"))

    assert "[pep517-smoke:failure]" in summary
    assert "[pep517-smoke:daemon-log]" in summary
    assert "[pep517-smoke:broker-log]" in summary
    assert "exited 1" in summary


def test_source_wheel_smoke_uses_the_wheel_under_test_as_backend() -> None:
    source = (SCRIPTS / "source_wheel_shim_smoke.py").read_text(encoding="utf-8")
    assert '"--no-build-isolation"' in source
    assert source.index('"--no-build-isolation"') < source.index('"--no-cache-dir"')


def test_temporary_root_teardown_stops_the_daemon_before_the_broker() -> None:
    """The rmtree race is the daemon's, not the broker's (soldr#2521 B2).

    `broker stop` reports `daemon routes retained` by design — the broker is
    a stable singleton and stopping it must not take compile daemons with it
    (soldr#2549). So a teardown that only stopped the broker left the actual
    writer running, and `TemporaryDirectory.__exit__` raced it into
    `OSError: [Errno 39] Directory not empty: 'restored-soldr'` on `main`
    (run 32071372721).

    Order matters as much as coverage: stopping the broker first would leave
    the daemon running with nothing left to reach it through.
    """
    module = load_script_module(
        SCRIPTS / "gnu_linux_toolchain_e2e.py", "gnu_linux_toolchain_e2e_teardown"
    )
    env: dict[str, str] = {}
    _fill_outer_env(env)
    with mock.patch.object(module.subprocess, "run") as run:
        module.stop_soldr_root("soldr", env)

    verbs = [call.args[0][1:] for call in run.call_args_list]
    assert verbs == [["daemon", "stop"], ["broker", "stop"]], verbs
    for call in run.call_args_list:
        assert call.kwargs["env"] is env
        assert call.kwargs["check"] is False


def test_temporary_root_cleanup_never_fails_the_lane() -> None:
    """An undeleted temp file must not turn a passing e2e red.

    The runner discards this filesystem moments later, so a straggler is
    worth nothing next to a red lane. This is the safety net behind
    `stop_soldr_root`, not a substitute for it.
    """
    source = (SCRIPTS / "gnu_linux_toolchain_e2e.py").read_text(encoding="utf-8")
    assert "ignore_cleanup_errors=True" in source
    assert "stop_soldr_root(soldr, restored_env)" in source
