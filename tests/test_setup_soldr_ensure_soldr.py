from __future__ import annotations

from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "actions" / "setup-soldr" / "ensure_soldr.py"


def _load_module():
    return load_script_module(SCRIPT_PATH, "ensure_soldr")


def test_request_headers_include_github_token_when_present(monkeypatch) -> None:
    module = _load_module()
    monkeypatch.setenv("GITHUB_TOKEN", "test-token")
    monkeypatch.setenv("GITHUB_REPOSITORY", "zackees/soldr")
    monkeypatch.delenv("SETUP_SOLDR_GITHUB_TOKEN", raising=False)

    headers = module._request_headers("zackees/soldr")

    values = {header.name: header.value for header in headers}
    assert values["Authorization"] == "Bearer test-token"
    assert values["User-Agent"] == "setup-soldr-action"


def test_request_headers_omit_authorization_when_token_missing(monkeypatch) -> None:
    module = _load_module()
    monkeypatch.delenv("GITHUB_TOKEN", raising=False)
    monkeypatch.delenv("SETUP_SOLDR_GITHUB_TOKEN", raising=False)

    headers = module._request_headers("zackees/soldr")

    assert "Authorization" not in [header.name for header in headers]


def test_request_headers_skip_repo_scoped_token_for_cross_repo(monkeypatch) -> None:
    module = _load_module()
    monkeypatch.setenv("GITHUB_TOKEN", "test-token")
    monkeypatch.setenv("GITHUB_REPOSITORY", "caller/app")
    monkeypatch.delenv("SETUP_SOLDR_GITHUB_TOKEN", raising=False)

    headers = module._request_headers("zackees/soldr")

    assert "Authorization" not in [header.name for header in headers]


def test_request_headers_allow_explicit_setup_token_for_cross_repo(monkeypatch) -> None:
    module = _load_module()
    monkeypatch.setenv("GITHUB_TOKEN", "repo-token")
    monkeypatch.setenv("GITHUB_REPOSITORY", "caller/app")
    monkeypatch.setenv("SETUP_SOLDR_GITHUB_TOKEN", "explicit-token")

    headers = module._request_headers("zackees/soldr")

    authorization = [h.value for h in headers if h.name == "Authorization"]
    assert authorization == ["Bearer explicit-token"]


def test_export_bundle_env_writes_local_dirs_when_bundle_present(
    tmp_path: Path,
    monkeypatch,
) -> None:
    module = _load_module()
    install_dir = tmp_path / "bin"
    install_dir.mkdir()
    github_env = tmp_path / "github.env"
    suffix = ".exe" if module.os.name == "nt" else ""

    for base in (module.CRGX_BUNDLED_BINARY, module.CARGO_CHEF_BUNDLED_BINARY):
        install_dir.joinpath(f"{base}{suffix}").write_text("binary", encoding="utf-8")

    monkeypatch.setenv("GITHUB_ENV", str(github_env))
    monkeypatch.delenv(module.CRGX_LOCAL_DIR_ENV, raising=False)
    monkeypatch.delenv(module.CARGO_CHEF_LOCAL_DIR_ENV, raising=False)

    module._export_bundle_env(install_dir)

    assert github_env.read_text(encoding="utf-8") == (
        f"{module.CRGX_LOCAL_DIR_ENV}={install_dir}\n"
        f"{module.CARGO_CHEF_LOCAL_DIR_ENV}={install_dir}\n"
    )


def test_export_bundle_env_preserves_explicit_overrides(
    tmp_path: Path,
    monkeypatch,
) -> None:
    module = _load_module()
    install_dir = tmp_path / "bin"
    install_dir.mkdir()
    github_env = tmp_path / "github.env"
    suffix = ".exe" if module.os.name == "nt" else ""

    for base in (module.CRGX_BUNDLED_BINARY, module.CARGO_CHEF_BUNDLED_BINARY):
        install_dir.joinpath(f"{base}{suffix}").write_text("binary", encoding="utf-8")

    monkeypatch.setenv("GITHUB_ENV", str(github_env))
    monkeypatch.setenv(module.CRGX_LOCAL_DIR_ENV, str(tmp_path / "crgx-override"))
    monkeypatch.setenv(module.CARGO_CHEF_LOCAL_DIR_ENV, str(tmp_path / "chef-override"))

    module._export_bundle_env(install_dir)

    assert github_env.read_text(encoding="utf-8") == ""


def test_installed_version_handles_output_larger_than_pipe_capacity(
    tmp_path: Path, monkeypatch
) -> None:
    import sys

    module = _load_module()
    binary = tmp_path / "soldr"
    binary.write_text("fixture", encoding="utf-8")
    original_run = module.subprocess.run

    def run_probe(command, **kwargs):
        assert command == [str(binary), "version", "--json"]
        assert "capture_output" not in kwargs
        assert kwargs["stdout"].fileno() >= 0
        return original_run(
            [
                sys.executable,
                "-c",
                "import json; print(json.dumps({'soldr_version': '0.9.29', "
                "'padding': 'x' * 262144}))",
            ],
            **kwargs,
        )

    monkeypatch.setattr(module.subprocess, "run", run_probe)
    assert module._installed_version(binary) == "0.9.29"
