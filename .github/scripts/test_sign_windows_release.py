"""Tests for the Azure Artifact Signing release step.

Signing itself needs Azure credentials and only runs in the release workflow;
these tests cover everything around it: the pinned jsign command line, the
structural signature check that fails the step on an unsigned PE, and the
wheel RECORD rewrite that keeps a re-signed wheel installable. PE images are
assembled with ``struct.pack`` so no binary fixture ships.
"""

from __future__ import annotations

import struct
import zipfile
from pathlib import Path

import pytest
from _script_loader import load_script_module

SCRIPT = Path(__file__).resolve().parent / "sign_windows_release.py"
sign = load_script_module(SCRIPT)

PUBLISHER = "Zachary Vorhies"


def make_pe(cert: bytes | None = None, *, pe32_plus: bool = True) -> bytes:
    """A minimal PE whose security directory points at ``cert`` (if any)."""
    pe_offset = 0x40
    image = bytearray(0x200)
    image[0:2] = b"MZ"
    struct.pack_into("<I", image, 0x3C, pe_offset)
    image[pe_offset : pe_offset + 4] = b"PE\0\0"
    optional = pe_offset + 24
    struct.pack_into("<H", image, optional, 0x20B if pe32_plus else 0x10B)
    if cert is not None:
        directories = optional + (112 if pe32_plus else 96)
        struct.pack_into("<II", image, directories + 4 * 8, len(image), len(cert))
        image += cert
    return bytes(image)


def win_certificate(payload: bytes) -> bytes:
    return struct.pack("<IHH", 8 + len(payload), 0x0200, 0x0002) + payload


SIGNED_PAYLOAD = b"\x30\x82" + PUBLISHER.encode() + sign.RFC3161_TIMESTAMP_OID


def test_unsigned_pe_is_rejected() -> None:
    assert "no Authenticode signature" in sign.signature_problem(make_pe(), PUBLISHER)


@pytest.mark.parametrize("pe32_plus", [True, False])
def test_timestamped_signature_by_publisher_passes(pe32_plus: bool) -> None:
    image = make_pe(win_certificate(SIGNED_PAYLOAD), pe32_plus=pe32_plus)
    assert sign.signature_problem(image, PUBLISHER) is None


def test_wrong_publisher_and_missing_timestamp_are_rejected() -> None:
    other = make_pe(win_certificate(b"CN=Someone Else" + sign.RFC3161_TIMESTAMP_OID))
    assert "publisher" in sign.signature_problem(other, PUBLISHER)
    untimed = make_pe(win_certificate(PUBLISHER.encode()))
    assert "timestamp" in sign.signature_problem(untimed, PUBLISHER)


def test_verify_package_dir_fails_on_any_unsigned_exe(tmp_path: Path) -> None:
    (tmp_path / "soldr.exe").write_bytes(make_pe(win_certificate(SIGNED_PAYLOAD)))
    (tmp_path / "crgx.exe").write_bytes(make_pe())
    (tmp_path / "soldr.pdb").write_bytes(b"not a PE")
    with pytest.raises(sign.SigningError, match="crgx.exe"):
        sign.verify_package_dir(tmp_path, PUBLISHER)


def test_verify_requires_at_least_one_pe(tmp_path: Path) -> None:
    with pytest.raises(sign.SigningError, match="no Windows PE"):
        sign.verify_package_dir(tmp_path, PUBLISHER)


def test_endpoint_and_publisher_normalisation() -> None:
    host = sign.endpoint_host("https://eus.codesigning.azure.net/")
    assert host == "eus.codesigning.azure.net"
    with pytest.raises(sign.SigningError):
        sign.endpoint_host("https://example.com/")
    dn = "CN=Zachary Vorhies, O=Zachary Vorhies, L=Berkeley, C=US"
    assert sign.publisher_common_name(dn) == PUBLISHER
    assert sign.publisher_common_name(PUBLISHER) == PUBLISHER


def test_jsign_command_keeps_the_token_off_argv() -> None:
    config = sign.SigningConfig("eus.codesigning.azure.net", "acct", "prof", PUBLISHER)
    command = sign.jsign_command(Path("jsign.jar"), config, [Path("a.exe")])
    assert command[command.index("--storetype") + 1] == "TRUSTEDSIGNING"
    assert command[command.index("--alias") + 1] == "acct/prof"
    assert command[command.index("--storepass") + 1] == f"env:{sign.TOKEN_ENV}"
    assert (
        command[command.index("--tsaurl") + 1] == "http://timestamp.acs.microsoft.com"
    )
    assert command[command.index("--alg") + 1] == "SHA-256"
    assert command[-1] == "a.exe"


def test_rebuilt_wheel_records_signed_bytes(tmp_path: Path) -> None:
    wheel = tmp_path / "soldr-1.0.0-py3-none-win_amd64.whl"
    exe = "soldr-1.0.0.data/scripts/soldr.exe"
    record = "soldr-1.0.0.dist-info/RECORD"
    unsigned = make_pe()
    with zipfile.ZipFile(wheel, "w", zipfile.ZIP_DEFLATED) as out:
        out.writestr(exe, unsigned)
        out.writestr("soldr/__init__.py", b"")
        out.writestr(
            record,
            f"{exe},{sign.record_hash(unsigned)},{len(unsigned)}\n"
            "soldr/__init__.py,sha256=47DEQpj8HBSa-_TImW-5JCeuQeRkm5NMpJWZG3hSuFU,0\n"
            f"{record},,\n",
        )
    signed = make_pe(win_certificate(SIGNED_PAYLOAD))

    sign.rebuild_wheel(wheel, {exe: signed})

    with zipfile.ZipFile(wheel) as archive:
        assert archive.read(exe) == signed
        assert archive.getinfo(exe).compress_type == zipfile.ZIP_DEFLATED
        rows = archive.read(record).decode().splitlines()
    assert rows[0] == f"{exe},{sign.record_hash(signed)},{len(signed)}"
    assert rows[1].startswith("soldr/__init__.py,sha256=47DEQ")
    assert rows[2] == f"{record},,"
    sign.verify_wheel(wheel, PUBLISHER)
