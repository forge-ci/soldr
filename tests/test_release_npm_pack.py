import json
import subprocess
import pytest
from types import SimpleNamespace
from pathlib import Path
from conftest import load_script_module


def test_producer_exists():
    module = load_script_module(
        Path(__file__).parents[1] / ".github/scripts/release_npm_pack.py"
    )
    assert callable(module.pack)


ROOT = Path(__file__).parents[1]
producer = load_script_module(ROOT / ".github/scripts/release_npm_pack.py")


@pytest.fixture
def source(tmp_path):
    workspace = tmp_path / "source"
    workspace.mkdir()
    (workspace / "package.json").write_text(
        json.dumps(
            {
                "name": "@zackees/soldr",
                "version": "1.2.3",
                "files": ["hello.js"],
                "scripts": {"prepack": "node guard.js"},
            }
        )
    )
    (workspace / "hello.js").write_text('console.log("fixture");')
    (workspace / "guard.js").write_text(
        'require("fs").writeFileSync("guard-ran", "yes");'
    )
    output = tmp_path / "output"
    output.mkdir(mode=0o700)
    return workspace, output


def clean(result):
    subprocess.run(["safe-rm", "-r", str(Path(result["tarball"]).parent)], check=True)


def test_real_pack_and_resume_no_repack(source, monkeypatch):
    workspace, output = source
    result = producer.pack(workspace, version="1.2.3", output_parent=output)
    try:
        assert (workspace / "guard-ran").read_text() == "yes"
        assert result["npm_identity"]["filename"] == "zackees-soldr-1.2.3.tgz"
        assert len(result["npm_identity"]["files"]) == 2

        def forbidden(*args):
            raise AssertionError("resume invoked npm")

        monkeypatch.setattr(producer.publisher, "run_npm", forbidden)
        resumed = producer.pack(
            workspace,
            version="1.2.3",
            output_parent=output,
            frozen_identity=result["npm_identity"],
            frozen_tarball=Path(result["tarball"]),
        )
        try:
            assert resumed["reused_frozen"] is True
            assert (
                Path(resumed["tarball"]).read_bytes()
                == Path(result["tarball"]).read_bytes()
            )
        finally:
            clean(resumed)
    finally:
        clean(result)


def test_real_guard_failure_cleans(source):
    workspace, output = source
    (workspace / "guard.js").write_text("process.exit(19)")
    with pytest.raises(ValueError, match="npm pack failed"):
        producer.pack(workspace, version="1.2.3", output_parent=output)
    assert list(output.iterdir()) == []


@pytest.mark.parametrize("value", [[], [{}, {}], [{"filename": "../escape"}]])
def test_bad_receipts_clean(source, monkeypatch, value):
    workspace, output = source
    monkeypatch.setattr(
        producer.publisher,
        "run_npm",
        lambda *args: SimpleNamespace(
            returncode=0, stdout=json.dumps(value), stderr=""
        ),
    )
    with pytest.raises(ValueError):
        producer.pack(workspace, version="1.2.3", output_parent=output)
    assert list(output.iterdir()) == []


def test_duplicate_receipt_fields(source, monkeypatch):
    workspace, output = source
    monkeypatch.setattr(
        producer.publisher,
        "run_npm",
        lambda *args: SimpleNamespace(
            returncode=0, stdout='[{"filename":"x","filename":"y"}]', stderr=""
        ),
    )
    with pytest.raises(ValueError, match="duplicate"):
        producer.pack(workspace, version="1.2.3", output_parent=output)
    assert list(output.iterdir()) == []


def test_timeout_cleanup(source, monkeypatch):
    workspace, output = source
    (workspace / "guard.js").write_text("setTimeout(()=>{},30000)")
    monkeypatch.setattr(producer.publisher, "TIMEOUT", 0.2)
    with pytest.raises(RuntimeError, match="deadline"):
        producer.pack(workspace, version="1.2.3", output_parent=output)
    assert list(output.iterdir()) == []


def test_identity_mismatch(source):
    workspace, output = source
    result = producer.pack(workspace, version="1.2.3", output_parent=output)
    try:
        frozen = dict(result["npm_identity"], sha256="a" * 64)
        with pytest.raises(ValueError, match="identity mismatch"):
            producer.pack(
                workspace,
                version="1.2.3",
                output_parent=output,
                frozen_identity=frozen,
                frozen_tarball=Path(result["tarball"]),
            )
        assert len(list(output.iterdir())) == 1
    finally:
        clean(result)


@pytest.mark.parametrize("mutation", ["size", "integrity", "inventory", "mode"])
def test_real_pack_receipt_mismatch(source, monkeypatch, mutation):
    workspace, output = source
    original = producer.publisher.run_npm

    def changed(*args):
        result = original(*args)
        rows = json.loads(result.stdout)
        if mutation == "size":
            rows[0]["size"] += 1
        elif mutation == "integrity":
            rows[0]["integrity"] = "sha512-wrong"
        elif mutation == "mode":
            rows[0]["files"][0]["mode"] ^= 1
        else:
            rows[0]["files"][0]["path"] = "foreign"
        return SimpleNamespace(returncode=0, stdout=json.dumps(rows), stderr="")

    monkeypatch.setattr(producer.publisher, "run_npm", changed)
    with pytest.raises(ValueError):
        producer.pack(workspace, version="1.2.3", output_parent=output)
    assert not list(output.iterdir())


def test_source_mutation_refused(source):
    workspace, output = source
    (workspace / "guard.js").write_text(
        'const fs=require("fs");const p=JSON.parse(fs.readFileSync("package.json"));p.description="changed";fs.writeFileSync("package.json",JSON.stringify(p));'
    )
    with pytest.raises(ValueError, match="metadata changed"):
        producer.pack(workspace, version="1.2.3", output_parent=output)
    assert not list(output.iterdir())


def test_parent_symlink_refused(source):
    workspace, output = source
    link = output.parent / "alias"
    link.symlink_to(output, target_is_directory=True)
    with pytest.raises(ValueError, match="symlink"):
        producer.pack(workspace, version="1.2.3", output_parent=link)


def test_missing_cleanup_preflight(source, monkeypatch):
    workspace, output = source
    monkeypatch.setattr(producer.shutil, "which", lambda *args: None)

    def forbidden(*args, **kwargs):
        raise AssertionError("stage or npm reached")

    monkeypatch.setattr(producer.tempfile, "mkdtemp", forbidden)
    monkeypatch.setattr(producer.publisher, "run_npm", forbidden)
    with pytest.raises(ValueError, match="cleanup dependency"):
        producer.pack(workspace, version="1.2.3", output_parent=output)


def test_ambient_ignore_scripts_does_not_skip_guard(source, monkeypatch):
    workspace, output = source
    monkeypatch.setenv("NPM_CONFIG_IGNORE_SCRIPTS", "true")
    (workspace / ".npmrc").write_text("ignore-scripts=true\nforeground-scripts=true\n")
    (workspace / "guard.js").write_text(
        'console.log("guard stdout is not receipt JSON");require("fs").writeFileSync("guard-ran","yes");'
    )
    result = producer.pack(workspace, version="1.2.3", output_parent=output)
    try:
        assert (workspace / "guard-ran").read_text() == "yes"
    finally:
        clean(result)
