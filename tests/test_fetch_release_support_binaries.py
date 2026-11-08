"""Tests for the extracted release support-binary staging gate (soldr#2469)."""

from __future__ import annotations

import hashlib
import io
import json
import tarfile
import urllib.error
import zipfile
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "auto-release.yml"
CONTRACT = REPO_ROOT / "ci" / "canonical-targets.json"

support = load_script_module(
    SCRIPTS / "fetch_release_support_binaries.py", "fetch_release_support_binaries"
)


def zip_with(path: str, contents: bytes) -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        archive.writestr(path, contents)
    return buffer.getvalue()


def tar_with(path: str, contents: bytes) -> bytes:
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        member = tarfile.TarInfo(path)
        member.size = len(contents)
        archive.addfile(member, io.BytesIO(contents))
    return buffer.getvalue()


class TestTargetPlatformMapping:
    def test_every_release_target_has_an_exact_toolchain_mapping(self) -> None:
        targets = json.loads(CONTRACT.read_text(encoding="utf-8"))["targets"]
        for target in targets:
            if target["release"]["status"] != "included":
                continue
            mapping = support.platform_for_target(target["triple"])
            assert mapping.os
            assert mapping.arch

    def test_unknown_target_fails_with_the_target_name(self) -> None:
        with pytest.raises(support.SupportBinaryError, match="made-up-target"):
            support.platform_for_target("made-up-target")

    def test_windows_gets_an_executable_suffix(self) -> None:
        assert support.binary_suffix("x86_64-pc-windows-msvc") == ".exe"
        assert support.binary_suffix("x86_64-unknown-linux-gnu") == ""


class TestCatalogueIntegrity:
    def test_signed_download_failure_never_logs_query_token(
        self,
        tmp_path: Path,
        monkeypatch: pytest.MonkeyPatch,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        secret = "release-token-must-not-leak"
        url = f"https://example.invalid/tool.zip?token={secret}"

        def failed_read(_url: str, timeout: int) -> bytes:
            del timeout
            raise urllib.error.URLError(f"transport included {secret}")

        monkeypatch.setattr(support, "read_url", failed_read)

        with pytest.raises(support.SupportBinaryError) as raised:
            support.download_verified([url], "0" * 64, tmp_path / "tool.zip")

        output = capsys.readouterr()
        assert secret not in output.out
        assert secret not in output.err
        assert secret not in str(raised.value)

    def test_descriptor_digest_mismatch_is_rejected_before_json_is_trusted(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        descriptor = b'{"releases": []}'
        monkeypatch.setattr(support, "read_url", lambda _url, timeout: descriptor)
        index = {
            "tools": {
                "crgx": {
                    "descriptor": {
                        "url": "catalogues/crgx.json",
                        "sha256": "0" * 64,
                    }
                }
            }
        }

        with pytest.raises(support.SupportBinaryError, match="sha256 mismatch"):
            support.load_tool_catalog("https://example.invalid", index, "crgx", "#47")

    @pytest.mark.parametrize("digest", ["", "not-a-sha256"])
    def test_missing_or_malformed_descriptor_digest_is_rejected_before_download(
        self, digest: str, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(
            support,
            "read_url",
            lambda url, timeout: pytest.fail(f"unexpected download: {url} ({timeout})"),
        )
        index = {
            "tools": {
                "crgx": {
                    "descriptor": {"url": "catalogues/crgx.json", "sha256": digest}
                }
            }
        }

        with pytest.raises(support.SupportBinaryError, match="lacks a valid sha256"):
            support.load_tool_catalog("https://example.invalid", index, "crgx", "#47")

    def test_fetch_checks_asset_before_extracting(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        archive = zip_with("bin/crgx", b"crgx")
        monkeypatch.setattr(
            support,
            "load_tool_catalog",
            lambda *_args: support.ToolCatalog(
                "https://example.invalid/crgx.json",
                {
                    "releases": [
                        {
                            "version": "v1.2.3",
                            "platforms": [
                                {
                                    "platform": support.platform_for_target(
                                        "x86_64-unknown-linux-gnu"
                                    ).to_dict(),
                                    "asset": {
                                        "filename": "crgx.zip",
                                        "urls": ["https://example.invalid/crgx.zip"],
                                        "sha256": "0" * 64,
                                        "size_bytes": len(archive),
                                    },
                                }
                            ],
                        }
                    ]
                },
            ),
        )
        monkeypatch.setattr(support, "read_url", lambda _url, timeout: archive)
        monkeypatch.setattr(
            support,
            "extract_archive",
            lambda *_args: pytest.fail("must not extract a digest mismatch"),
        )

        with pytest.raises(support.SupportBinaryError, match="sha256 mismatch"):
            support.fetch_tool(
                origin="https://example.invalid",
                index={},
                target="x86_64-unknown-linux-gnu",
                tool="crgx",
                version="1.2.3",
                output_dir=tmp_path / "package",
                driver=tmp_path / "soldr",
                issue_url="#47",
                github_env=tmp_path / "github-env",
            )


class TestStaging:
    def test_multipart_asset_is_reconstructed_and_verified(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        archive = zip_with("release/bin/crgx", b"multipart")
        split = len(archive) // 2
        chunks = [archive[:split], archive[split:]]
        asset = {
            "filename": "crgx.zip",
            "sha256": hashlib.sha256(archive).hexdigest(),
            "size_bytes": len(archive),
            "parts": [
                {
                    "number": number,
                    "sha256": hashlib.sha256(chunk).hexdigest(),
                    "size_bytes": len(chunk),
                    "urls": [f"https://parts.invalid/{number}"],
                }
                for number, chunk in enumerate(chunks, start=1)
            ],
        }
        catalog = {
            "releases": [
                {
                    "version": "v1.2.3",
                    "platforms": [
                        {
                            "platform": support.platform_for_target(
                                "x86_64-unknown-linux-gnu"
                            ).to_dict(),
                            "asset": asset,
                        }
                    ],
                }
            ]
        }
        monkeypatch.setattr(
            support,
            "load_tool_catalog",
            lambda *_args: support.ToolCatalog(
                "https://example.invalid/crgx.json", catalog
            ),
        )
        monkeypatch.setattr(
            support,
            "read_url",
            lambda url, timeout: chunks[int(url.rsplit("/", 1)[1]) - 1],
        )
        package = tmp_path / "package"
        package.mkdir()
        github_env = tmp_path / "github-env"
        github_env.touch()

        support.fetch_tool(
            origin="https://example.invalid",
            index={},
            target="x86_64-unknown-linux-gnu",
            tool="crgx",
            version="1.2.3",
            output_dir=package,
            driver=tmp_path / "soldr",
            issue_url="#47",
            github_env=github_env,
        )

        assert (package / "crgx").read_bytes() == b"multipart"

    def test_multipart_part_over_publication_limit_is_rejected(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        asset = {
            "filename": "crgx.zip",
            "sha256": "0" * 64,
            "size_bytes": 1,
            "parts": [
                {
                    "number": 1,
                    "sha256": "1" * 64,
                    "size_bytes": support.MAX_PART_BYTES + 1,
                    "urls": ["https://parts.invalid/1"],
                }
            ],
        }
        monkeypatch.setattr(
            support,
            "read_url",
            lambda *_args: pytest.fail(
                "oversized part must be rejected before download"
            ),
        )

        with pytest.raises(support.SupportBinaryError, match="invalid size_bytes"):
            support.download_catalogued_asset(asset, tmp_path / "crgx.zip")

    @pytest.mark.parametrize(
        ("field", "value", "message"),
        [
            ("asset_size", 0, "asset has invalid size_bytes"),
            ("asset_size", True, "asset has invalid size_bytes"),
            ("part_number", True, "non-contiguous parts"),
            ("part_size", 0, "part 1 has invalid size_bytes"),
            ("part_size", True, "part 1 has invalid size_bytes"),
        ],
    )
    def test_multipart_requires_exact_positive_integers(
        self,
        field: str,
        value: object,
        message: str,
        tmp_path: Path,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        parts = [
            {
                "number": 1,
                "sha256": "1" * 64,
                "size_bytes": 1,
                "urls": ["https://parts.invalid/1"],
            }
        ]
        asset = {
            "filename": "crgx.zip",
            "sha256": "0" * 64,
            "size_bytes": 1,
            "parts": parts,
        }
        if field == "asset_size":
            asset["size_bytes"] = value
        elif field == "part_number":
            parts[0]["number"] = value
        else:
            parts[0]["size_bytes"] = value
        monkeypatch.setattr(
            support,
            "read_url",
            lambda *_args: pytest.fail("malformed metadata must fail before download"),
        )

        with pytest.raises(support.SupportBinaryError, match=message):
            support.download_catalogued_asset(asset, tmp_path / "crgx.zip")

    def test_verified_zip_stages_preferred_bin_and_writes_provenance(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        archive = zip_with("release/bin/crgx", b"preferred")
        digest = hashlib.sha256(archive).hexdigest()
        platform = support.platform_for_target("x86_64-unknown-linux-gnu").to_dict()
        catalog = {
            "releases": [
                {
                    "version": "v1.2.3",
                    "platforms": [
                        {
                            "platform": platform,
                            "asset": {
                                "filename": "crgx.zip",
                                "urls": [
                                    "https://primary.invalid/crgx.zip",
                                    "https://fallback.invalid/crgx.zip",
                                ],
                                "sha256": digest,
                                "size_bytes": len(archive),
                            },
                        }
                    ],
                }
            ]
        }
        monkeypatch.setattr(
            support,
            "load_tool_catalog",
            lambda *_args: support.ToolCatalog(
                "https://example.invalid/crgx.json", catalog
            ),
        )
        attempts: list[str] = []

        def fake_read_url(url: str, timeout: int) -> bytes:
            attempts.append(url)
            assert timeout == 600
            if url == "https://primary.invalid/crgx.zip":
                raise urllib.error.URLError("primary mirror unavailable")
            assert url == "https://fallback.invalid/crgx.zip"
            return archive

        monkeypatch.setattr(support, "read_url", fake_read_url)
        package = tmp_path / "package"
        package.mkdir()
        github_env = tmp_path / "github-env"
        github_env.touch()

        support.fetch_tool(
            origin="https://example.invalid",
            index={},
            target="x86_64-unknown-linux-gnu",
            tool="crgx",
            version="1.2.3",
            output_dir=package,
            driver=tmp_path / "soldr",
            issue_url="#47",
            github_env=github_env,
        )

        assert (package / "crgx").read_bytes() == b"preferred"
        assert attempts == [
            "https://primary.invalid/crgx.zip",
            "https://fallback.invalid/crgx.zip",
        ]
        assert (
            github_env.read_text(encoding="utf-8")
            == "CRGX_SOURCE_COMMIT=soldr-toolchain:v1.2.3\n"
        )

    def test_cargo_chef_provenance_uses_its_distinct_environment_variable(
        self, tmp_path: Path
    ) -> None:
        github_env = tmp_path / "github-env"
        github_env.touch()

        support.write_source_commit(github_env, "cargo-chef", "v0.1.73")

        assert (
            github_env.read_text(encoding="utf-8")
            == "CARGO_CHEF_SOURCE_COMMIT=soldr-toolchain:v0.1.73\n"
        )


class TestArchiveSafety:
    def test_zip_path_traversal_is_rejected_before_extraction(
        self, tmp_path: Path
    ) -> None:
        archive = tmp_path / "malicious.zip"
        archive.write_bytes(zip_with("../outside", b"not allowed"))
        extract_dir = tmp_path / "extract"
        extract_dir.mkdir()

        with pytest.raises(support.SupportBinaryError, match="unsafe path"):
            support.extract_archive(
                archive,
                extract_dir,
                "x86_64-unknown-linux-gnu",
                tmp_path / "soldr",
            )

        assert not (tmp_path / "outside").exists()

    def test_tar_path_traversal_is_rejected_before_extraction(
        self, tmp_path: Path
    ) -> None:
        archive = tmp_path / "malicious.tar.gz"
        archive.write_bytes(tar_with("../outside", b"not allowed"))
        extract_dir = tmp_path / "extract"
        extract_dir.mkdir()

        with pytest.raises(support.SupportBinaryError, match="unsafe path"):
            support.extract_archive(
                archive,
                extract_dir,
                "x86_64-unknown-linux-gnu",
                tmp_path / "soldr",
            )

        assert not (tmp_path / "outside").exists()


def test_workflow_invokes_the_script_instead_of_inlining_the_gate() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert ".github/scripts/fetch_release_support_binaries.py" in workflow
    assert "def platform_for_target(target: str)" not in workflow
    assert "def fetch_tool(tool: str, version: str)" not in workflow
