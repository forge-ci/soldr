import io
import json
import tarfile
from types import SimpleNamespace
from pathlib import Path
import pytest
from conftest import load_script_module

ROOT = Path(__file__).resolve().parents[1]
publisher = load_script_module(ROOT / ".github/scripts/publish_npm_package.py")


def test_no_source_publication_fallback(tmp_path):
    with pytest.raises(TypeError):
        publisher.publish(tmp_path)


@pytest.fixture
def frozen(tmp_path):
    source = json.loads((ROOT / "package.json").read_text())
    canonical = json.loads((ROOT / "ci/canonical-targets.json").read_text())
    (tmp_path / "ci").mkdir()
    (tmp_path / "ci/canonical-targets.json").write_text(json.dumps(canonical))
    (tmp_path / "package.json").write_text(json.dumps(source))
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w:gz") as archive:
        data = json.dumps(source).encode()
        entry = tarfile.TarInfo("package/package.json")
        entry.size = len(data)
        archive.addfile(entry, io.BytesIO(data))
    package = {"package_json": source, "archive": out.getvalue()}
    rc = publisher.contract
    directive = {
        "schema_version": 1,
        "repository": "zackees/soldr",
        "issue": 3343,
        "attempt_id": "12345678-1234-4234-8234-123456789abc",
        "candidate_sha": "a" * 40,
        "version": source["version"],
        "tag": "v" + source["version"],
        "mode": "partial",
        "targets": ["x86_64-unknown-linux-gnu"],
        "destinations": ["npm"],
        "partial_reason": "recovery",
        "expected_artifacts": [],
        "contract_sha256": rc.canonical_digest(canonical),
    }
    info = rc.freeze_info(directive, canonical, [], npm_package=package)
    tarball = tmp_path / info["npm_package"]["filename"]
    tarball.write_bytes(package["archive"])
    infofile = tmp_path / "info.json"
    infofile.write_text(json.dumps(info))
    return tmp_path, tarball, infofile, info["npm_package"]


def response(value, code=0):
    return SimpleNamespace(returncode=code, stdout=json.dumps(value), stderr="error")


def remote(identity):
    return {"version": identity["version"], "dist.integrity": identity["integrity"]}


E404 = {"error": {"code": "E404", "summary": "Not found", "detail": "Version absent"}}


def test_exact_bytes_resume_skips_without_pack(monkeypatch, frozen):
    source, tarball, info, identity = frozen
    calls = []

    def run(args, cwd):
        calls.append(args)
        return response(remote(identity))

    monkeypatch.setattr(publisher, "run_npm", run)
    assert publisher.publish(source, tarball=tarball, info=info) is False
    assert len(calls) == 1 and calls[0][0] == "view"


def test_exact_publish_and_confirmation(monkeypatch, frozen):
    source, tarball, info, identity = frozen
    calls = []
    replies = iter([response(E404, 1), response({}), response(remote(identity))])

    def run(args, cwd):
        calls.append(args)
        return next(replies)

    monkeypatch.setattr(publisher, "run_npm", run)
    assert publisher.publish(source, tarball=tarball, info=info) is True
    assert Path(calls[1][1]).name == tarball.name
    assert calls[1][1] != str(tarball.absolute())
    assert calls[1][0:1] + calls[1][2:] == [
        "publish",
        "--ignore-scripts",
        "--access",
        "public",
        "--registry",
        publisher.REGISTRY,
    ]
    assert all(args[0] != "pack" for args in calls)


@pytest.mark.parametrize(
    "reply",
    [
        response("0.9.27"),
        response({"version": "0.9.27", "dist.integrity": "sha512-wrong"}),
        response({"error": {"code": "E403", "summary": "denied", "detail": "no"}}, 1),
        response({"error": {"code": "E404"}}, 1),
        SimpleNamespace(returncode=1, stdout="", stderr="network"),
        response({}),
    ],
)
def test_lookup_failures_never_publish(monkeypatch, frozen, reply):
    source, tarball, info, _ = frozen
    calls = []

    def run(args, cwd):
        calls.append(args)
        return reply

    monkeypatch.setattr(publisher, "run_npm", run)
    with pytest.raises(RuntimeError):
        publisher.publish(source, tarball=tarball, info=info)
    assert len(calls) == 1 and calls[0][0] == "view"


@pytest.mark.parametrize(
    "mutation", ["missing", "changed", "symlink", "info", "version"]
)
def test_local_refusal_before_registry(monkeypatch, frozen, mutation):
    source, tarball, info, _ = frozen
    if mutation == "missing":
        tarball = source / "missing.tgz"
    elif mutation == "changed":
        tarball.write_bytes(b"changed")
    elif mutation == "symlink":
        link = source / "link.tgz"
        link.symlink_to(tarball)
        tarball = link
    elif mutation == "info":
        info.write_text("{}")
    else:
        package = json.loads((source / "package.json").read_text())
        package["version"] = "9.9.9"
        (source / "package.json").write_text(json.dumps(package))

    def forbidden(*args):
        raise AssertionError("registry called")

    monkeypatch.setattr(publisher, "run_npm", forbidden)
    with pytest.raises((RuntimeError, ValueError, OSError)):
        publisher.publish(source, tarball=tarball, info=info)


def test_post_publish_missing_is_incomplete(monkeypatch, frozen):
    source, tarball, info, _ = frozen
    replies = iter([response(E404, 1), response({}), response(E404, 1)])
    monkeypatch.setattr(publisher, "run_npm", lambda *args: next(replies))
    with pytest.raises(RuntimeError, match="incomplete"):
        publisher.publish(source, tarball=tarball, info=info)


def test_mutation_during_lookup_refuses_upload(monkeypatch, frozen):
    source, tarball, info, _ = frozen

    def run(*args):
        tarball.write_bytes(b"changed")
        return response(E404, 1)

    monkeypatch.setattr(publisher, "run_npm", run)
    with pytest.raises(RuntimeError, match="changed"):
        publisher.publish(source, tarball=tarball, info=info)


@pytest.mark.parametrize("case", ["output", "deadline"])
def test_subprocess_bounds(monkeypatch, tmp_path, case):
    import os

    executable = tmp_path / "npm"
    executable.write_text(
        "#!/usr/bin/env python3\n"
        + (
            'print("x"*10000)\n'
            if case == "output"
            else "import time\ntime.sleep(10)\n"
        )
    )
    executable.chmod(0o755)
    monkeypatch.setenv("PATH", str(tmp_path) + os.pathsep + os.environ["PATH"])
    monkeypatch.setattr(publisher, "MAX_OUTPUT", 100)
    monkeypatch.setattr(publisher, "TIMEOUT", 0.1)
    with pytest.raises(RuntimeError, match=r"ceiling|deadline"):
        publisher.run_npm([], tmp_path)


def test_publish_submits_verified_snapshot(monkeypatch, frozen):
    source, tarball, info, _identity = frozen
    verified = tarball.read_bytes()
    stagepaths = []

    def run(args, cwd):
        if args[0] == "view":
            return response(E404, 1)
        tarball.write_bytes(b"replacement")
        stage = Path(args[1])
        stagepaths.append(stage)
        assert stage.read_bytes() == verified
        assert stage.stat().st_mode & 0o777 == 0o400
        assert stage.parent.stat().st_mode & 0o777 == 0o700
        return response({})

    monkeypatch.setattr(publisher, "run_npm", run)
    with pytest.raises(RuntimeError, match="changed"):
        publisher.publish(source, tarball=tarball, info=info)
    assert stagepaths and not stagepaths[0].parent.exists()


def test_old_atime_read_allowed(tmp_path):
    import os

    path = tmp_path / "bytes"
    path.write_bytes(b"original")
    os.utime(path, ns=(1, 1))
    assert publisher.bounded_read(path, 100) == b"original"


def test_deadline_kills_inherited_pipe_child(monkeypatch, tmp_path):
    import os
    import time

    executable = tmp_path / "npm"
    pidfile = tmp_path / "child.pid"
    executable.write_text(
        '#!/usr/bin/env python3\nimport subprocess,sys,time,pathlib\nchild=subprocess.Popen([sys.executable,"-c","import time; time.sleep(30)"])\npathlib.Path("child.pid").write_text(str(child.pid))\ntime.sleep(30)\n'
    )
    executable.chmod(0o755)
    monkeypatch.setenv("PATH", str(tmp_path) + os.pathsep + os.environ["PATH"])
    monkeypatch.setattr(publisher, "TIMEOUT", 0.2)
    with pytest.raises(RuntimeError, match="deadline"):
        publisher.run_npm([], tmp_path)
    pid = int(pidfile.read_text())
    statefile = Path(f"/proc/{pid}/stat")
    for _ in range(50):
        if not statefile.exists() or statefile.read_text().split()[2] == "Z":
            break
        time.sleep(0.01)
    # The owned group child cannot keep running or hold pipes. Grandchild
    # zombie reaping belongs to its OS adoptive parent, not subprocess.wait.
    assert not statefile.exists() or statefile.read_text().split()[2] == "Z"


def test_missing_cleanup_dependency_refuses_before_registry_or_stage(
    monkeypatch, frozen
):
    source, tarball, info, _ = frozen
    import shutil

    monkeypatch.setattr(shutil, "which", lambda *args: None)

    def forbidden(*args, **kwargs):
        raise AssertionError("registry or stage reached")

    monkeypatch.setattr(publisher, "run_npm", forbidden)
    monkeypatch.setattr(publisher.tempfile, "mkdtemp", forbidden)
    with pytest.raises(RuntimeError, match="safe-rm cleanup dependency unavailable"):
        publisher.publish(source, tarball=tarball, info=info)
