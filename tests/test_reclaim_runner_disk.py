"""Unit tests for ci/reclaim_runner_disk.py."""

import importlib.util
import sys
from pathlib import Path
from unittest import mock

_PATH = Path(__file__).resolve().parents[1] / "ci" / "reclaim_runner_disk.py"
_SPEC = importlib.util.spec_from_file_location("reclaim_runner_disk", _PATH)
assert _SPEC and _SPEC.loader
reclaim = importlib.util.module_from_spec(_SPEC)
sys.modules["reclaim_runner_disk"] = reclaim
_SPEC.loader.exec_module(reclaim)


def test_refuses_off_github_hosted(tmp_path):
    with mock.patch.dict(reclaim.os.environ, {"GITHUB_ACTIONS": ""}, clear=True):
        with mock.patch.object(reclaim, "remove_dir") as rm:
            assert reclaim.main(["--volume", str(tmp_path)]) == 2
            rm.assert_not_called()


def test_refuses_self_hosted():
    env = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "self-hosted"}
    assert not reclaim.is_github_hosted_linux(env)


def _run(tmp_path, free_after_gib):
    usage = mock.Mock(free=int(free_after_gib * reclaim.GIB))
    env = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted"}
    with (
        mock.patch.dict(reclaim.os.environ, env, clear=True),
        mock.patch.object(reclaim.sys, "platform", "linux"),
        mock.patch.object(reclaim.shutil, "disk_usage", return_value=usage),
        mock.patch.object(reclaim, "remove_dir") as rm,
    ):
        code = reclaim.main(["--volume", str(tmp_path), "--require-gib", "20"])
    return code, rm


def test_removes_every_listed_dir_and_passes(tmp_path):
    code, rm = _run(tmp_path, 40)
    assert code == 0
    assert [c.args[0] for c in rm.call_args_list] == list(reclaim.RECLAIM_DIRS)


def test_fails_loudly_below_floor(tmp_path, capsys):
    code, _ = _run(tmp_path, 3.55)
    assert code == 1
    assert "FATAL" in capsys.readouterr().err


def test_never_removes_lane_toolchains():
    for keep in ("rust", "cargo", "uv", "powershell", "python"):
        assert not any(keep in d.lower() for d in reclaim.RECLAIM_DIRS)
