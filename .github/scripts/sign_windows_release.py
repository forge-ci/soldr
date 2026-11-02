#!/usr/bin/env python3
"""Authenticode-sign the Windows release binaries through Azure Artifact Signing.

Both ``*-pc-windows-msvc`` release lanes cross-build on Linux runners, so the
signer is jsign (https://github.com/ebourg/jsign), a pure-Java Authenticode
implementation with a ``TRUSTEDSIGNING`` keystore for Azure Artifact Signing.
The jar is pinned by version and SHA-256 and refused on mismatch.

The workflow runs ``azure/login`` (OIDC, no client secret) first; this script
then asks the Azure CLI for a short-lived code-signing access token and hands
it to jsign through the environment, never on argv.

Two surfaces ship Windows executables and both are signed:

* ``--package-dir``: the staged archive contents. This must run before
  ``release_manifest.py`` so the manifest digests describe the signed bytes.
* ``--wheel``: the maturin wheel, which links its own ``soldr.exe``. Each PE
  member is signed in place and the wheel's ``RECORD`` is rewritten with the
  new digests and sizes.

Every signed file is then checked structurally (PE security directory holds a
PKCS#7 ``WIN_CERTIFICATE`` naming the expected publisher and carrying an
RFC 3161 timestamp); any unsigned PE fails the step. Native Windows smoke
(``ci/smoke_release_artifacts.py``) re-verifies the chain with
``Get-AuthenticodeSignature``.

Usage (CI):
    python3 .github/scripts/sign_windows_release.py sign --package-dir dist/package
    python3 .github/scripts/sign_windows_release.py sign --wheel dist/soldr-*.whl
    python3 .github/scripts/sign_windows_release.py verify --package-dir dist/package
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import os
import shutil
import struct
import sys
import tempfile
import urllib.request
import zipfile
from dataclasses import dataclass
from pathlib import Path

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

JSIGN_VERSION = "7.6"
JSIGN_URL = f"https://github.com/ebourg/jsign/releases/download/{JSIGN_VERSION}/jsign-{JSIGN_VERSION}.jar"
JSIGN_SHA256 = "6cf58aa080e7ff82656f320cff8d4a3bff30408df8766806dcba3740dd839fa5"
DEFAULT_JSIGN_DIR = Path("target/jsign")

TIMESTAMP_URL = "http://timestamp.acs.microsoft.com"
CODESIGNING_RESOURCE = "https://codesigning.azure.net"
TOKEN_ENV = "SOLDR_AZURE_SIGNING_TOKEN"

PE_SUFFIXES = (".exe", ".dll", ".pyd")

# WIN_CERTIFICATE: wRevision 0x0200, wCertificateType WIN_CERT_TYPE_PKCS_SIGNED_DATA.
WIN_CERT_REVISION_2 = 0x0200
WIN_CERT_TYPE_PKCS_SIGNED_DATA = 0x0002
# DER of OID 1.3.6.1.4.1.311.3.3.1 (Microsoft RFC 3161 timestamp countersignature).
RFC3161_TIMESTAMP_OID = bytes.fromhex("060a2b060104018237030301")
SECURITY_DIRECTORY_INDEX = 4


class SigningError(RuntimeError):
    """A signing precondition or post-signing check failed."""


@dataclass(frozen=True)
class PeImage:
    """One Windows PE to verify: a display name and its bytes."""

    name: str
    data: bytes


@dataclass(frozen=True)
class SigningConfig:
    """Azure Artifact Signing coordinates, read from the job environment."""

    endpoint_host: str
    account: str
    profile: str
    publisher: str


def endpoint_host(endpoint: str) -> str:
    """``https://eus.codesigning.azure.net/`` -> ``eus.codesigning.azure.net``."""
    host = endpoint.strip().removeprefix("https://").removeprefix("http://")
    host = host.split("/", 1)[0]
    if not host.endswith(".codesigning.azure.net"):
        raise SigningError(f"unexpected Artifact Signing endpoint: {endpoint!r}")
    return host


def _required_env(env: dict[str, str], name: str) -> str:
    value = env.get(name, "").strip()
    if not value:
        raise SigningError(f"{name} is not set (expected an org Actions secret)")
    return value


def config_from_env(env: dict[str, str]) -> SigningConfig:
    return SigningConfig(
        endpoint_host=endpoint_host(_required_env(env, "AZURE_SIGNING_ENDPOINT")),
        account=_required_env(env, "AZURE_SIGNING_ACCOUNT"),
        profile=_required_env(env, "AZURE_SIGNING_PROFILE"),
        publisher=publisher_common_name(_required_env(env, "AZURE_SIGN_PUBLISHER")),
    )


def publisher_common_name(publisher: str) -> str:
    """Accept either a bare name or a full subject DN (``CN=..., O=...``)."""
    for part in publisher.split(","):
        key, _, value = part.strip().partition("=")
        if key.strip().upper() == "CN" and value.strip():
            return value.strip().strip('"')
    return publisher.strip()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def fetch_jsign(dest_dir: Path) -> Path:
    """Download the pinned jsign jar once and verify its SHA-256."""
    jar = dest_dir / f"jsign-{JSIGN_VERSION}.jar"
    if jar.is_file() and sha256_file(jar) == JSIGN_SHA256:
        return jar
    dest_dir.mkdir(parents=True, exist_ok=True)
    partial = jar.with_suffix(".partial")
    with urllib.request.urlopen(JSIGN_URL, timeout=120) as response:
        with partial.open("wb") as out:
            shutil.copyfileobj(response, out)
    actual = sha256_file(partial)
    if actual != JSIGN_SHA256:
        partial.unlink()
        raise SigningError(
            f"jsign {JSIGN_VERSION} sha256 mismatch: got {actual}, pinned {JSIGN_SHA256}"
        )
    partial.replace(jar)
    print(f"jsign {JSIGN_VERSION}: sha256 verified ({JSIGN_SHA256})")
    return jar


def access_token() -> str:
    """Mint an Artifact Signing token from the ``azure/login`` session."""
    completed = run_captured(
        [
            "az",
            "account",
            "get-access-token",
            "--resource",
            CODESIGNING_RESOURCE,
            "--query",
            "accessToken",
            "-o",
            "tsv",
        ],
        capture_stdout=True,
        text=True,
        check=True,
    )
    token = completed.stdout.strip()
    if not token:
        raise SigningError("az returned an empty code-signing access token")
    if os.environ.get("GITHUB_ACTIONS") == "true":
        print(f"::add-mask::{token}", flush=True)
    return token


def jsign_command(jar: Path, config: SigningConfig, files: list[Path]) -> list[str]:
    return [
        "java",
        "-jar",
        str(jar),
        "--storetype",
        "TRUSTEDSIGNING",
        "--keystore",
        config.endpoint_host,
        "--storepass",
        f"env:{TOKEN_ENV}",
        "--alias",
        f"{config.account}/{config.profile}",
        "--alg",
        "SHA-256",
        "--tsaurl",
        TIMESTAMP_URL,
        "--tsmode",
        "RFC3161",
        "--replace",
        *[str(path) for path in files],
    ]


def is_pe_name(name: str) -> bool:
    return name.lower().endswith(PE_SUFFIXES)


def pe_files(directory: Path) -> list[Path]:
    return sorted(p for p in directory.rglob("*") if p.is_file() and is_pe_name(p.name))


def authenticode_blob(data: bytes) -> bytes | None:
    """Return the PE's WIN_CERTIFICATE table, or None when it has none."""
    if data[:2] != b"MZ" or len(data) < 0x40:
        raise SigningError("not a PE image (missing MZ header)")
    (pe_offset,) = struct.unpack_from("<I", data, 0x3C)
    if data[pe_offset : pe_offset + 4] != b"PE\0\0":
        raise SigningError("not a PE image (missing PE signature)")
    optional = pe_offset + 24
    (magic,) = struct.unpack_from("<H", data, optional)
    directories = {0x10B: optional + 96, 0x20B: optional + 112}.get(magic)
    if directories is None:
        raise SigningError(f"unknown PE optional-header magic {magic:#x}")
    entry = directories + SECURITY_DIRECTORY_INDEX * 8
    offset, size = struct.unpack_from("<II", data, entry)
    if offset == 0 or size == 0:
        return None
    return data[offset : offset + size]


def signature_problem(data: bytes, publisher: str) -> str | None:
    """Describe why ``data`` is not a timestamped signature by ``publisher``."""
    blob = authenticode_blob(data)
    if blob is None or len(blob) < 8:
        return "no Authenticode signature (empty PE security directory)"
    _length, revision, cert_type = struct.unpack_from("<IHH", blob, 0)
    if revision != WIN_CERT_REVISION_2 or cert_type != WIN_CERT_TYPE_PKCS_SIGNED_DATA:
        return f"unexpected WIN_CERTIFICATE revision/type {revision:#x}/{cert_type:#x}"
    if publisher.encode("utf-8") not in blob:
        return f"signature does not name publisher {publisher!r}"
    if RFC3161_TIMESTAMP_OID not in blob:
        return "signature carries no RFC 3161 timestamp"
    return None


def verify_paths(named: list[PeImage], publisher: str) -> None:
    if not named:
        raise SigningError("no Windows PE files found to verify")
    failures = []
    for image in named:
        name = image.name
        problem = signature_problem(image.data, publisher)
        if problem:
            failures.append(f"  {name}: {problem}")
        else:
            print(f"signed: {name}")
    if failures:
        raise SigningError("unsigned Windows release binaries:\n" + "\n".join(failures))


def verify_package_dir(package_dir: Path, publisher: str) -> None:
    verify_paths(
        [PeImage(str(p), p.read_bytes()) for p in pe_files(package_dir)], publisher
    )


def verify_wheel(wheel: Path, publisher: str) -> None:
    with zipfile.ZipFile(wheel) as archive:
        named = [
            PeImage(f"{wheel.name}!{info.filename}", archive.read(info))
            for info in archive.infolist()
            if is_pe_name(info.filename)
        ]
    verify_paths(named, publisher)


def record_hash(data: bytes) -> str:
    digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
    return "sha256=" + digest.decode("ascii")


def rewrite_record(record: str, replaced: dict[str, bytes]) -> str:
    """Update RECORD rows for every member whose bytes changed."""
    lines = []
    for line in record.splitlines():
        path = line.rsplit(",", 2)[0] if line.count(",") >= 2 else line
        if path in replaced:
            data = replaced[path]
            line = f"{path},{record_hash(data)},{len(data)}"
        lines.append(line)
    return "\n".join(lines) + "\n"


def rebuild_wheel(wheel: Path, replaced: dict[str, bytes]) -> None:
    """Rewrite ``wheel`` with ``replaced`` member bytes and a matching RECORD."""
    rebuilt = wheel.with_suffix(".signing")
    with zipfile.ZipFile(wheel) as source, zipfile.ZipFile(rebuilt, "w") as out:
        for info in source.infolist():
            data = replaced.get(info.filename)
            if data is None:
                data = source.read(info)
                if info.filename.endswith(".dist-info/RECORD"):
                    data = rewrite_record(data.decode("utf-8"), replaced).encode()
            out.writestr(info, data, compress_type=info.compress_type)
    rebuilt.replace(wheel)


def run_jsign(jar: Path, config: SigningConfig, files: list[Path], token: str) -> None:
    if not files:
        raise SigningError("no Windows PE files found to sign")
    env = dict(os.environ)
    env[TOKEN_ENV] = token
    print(f"jsign: signing {len(files)} file(s) with {config.account}/{config.profile}")
    run_captured(jsign_command(jar, config, files), env=env, check=True)


def sign_wheel(jar: Path, config: SigningConfig, wheel: Path, token: str) -> None:
    with tempfile.TemporaryDirectory(prefix="soldr-wheel-sign-") as tmp:
        staging = Path(tmp)
        members: dict[str, Path] = {}
        with zipfile.ZipFile(wheel) as archive:
            for index, info in enumerate(archive.infolist()):
                if is_pe_name(info.filename):
                    local = staging / f"{index}-{Path(info.filename).name}"
                    local.write_bytes(archive.read(info))
                    members[info.filename] = local
        run_jsign(jar, config, list(members.values()), token)
        rebuild_wheel(wheel, {name: p.read_bytes() for name, p in members.items()})


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("mode", choices=("sign", "verify"))
    surface = parser.add_mutually_exclusive_group(required=True)
    surface.add_argument("--package-dir", type=Path)
    surface.add_argument("--wheel", type=Path, nargs="+")
    parser.add_argument("--jsign-dir", type=Path, default=DEFAULT_JSIGN_DIR)
    args = parser.parse_args(argv)

    try:
        config = config_from_env(dict(os.environ))
        if args.mode == "sign":
            jar = fetch_jsign(args.jsign_dir)
            token = access_token()
            if args.package_dir is not None:
                run_jsign(jar, config, pe_files(args.package_dir), token)
            else:
                for wheel in args.wheel:
                    sign_wheel(jar, config, wheel, token)
        if args.package_dir is not None:
            verify_package_dir(args.package_dir, config.publisher)
        else:
            for wheel in args.wheel:
                verify_wheel(wheel, config.publisher)
    except SigningError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
