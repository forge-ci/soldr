#!/usr/bin/env python3
"""Publish only frozen npm bytes; caller authenticates candidate/info independently.

These file/schema checks grant no release authority. Existing source/version and
full-CI workflow gates remain prerequisites; this script never packs source.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import selectors
import stat
import signal
import shutil
import tempfile
import subprocess
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "release_contract_npm", ROOT / "ci/release_contract.py"
)
contract = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(contract)
REGISTRY = "https://registry.npmjs.org/"
MAX_OUTPUT = 1_048_576
TIMEOUT = 120


def bounded_read(path: Path, limit: int) -> bytes:
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        before = os.fstat(stream.fileno())
        if not stat.S_ISREG(before.st_mode) or not 0 < before.st_size <= limit:
            raise RuntimeError("input must be a bounded nonempty regular file")
        data = stream.read(limit + 1)
        after = os.fstat(stream.fileno())
        if len(data) != before.st_size or any(
            getattr(before, key) != getattr(after, key)
            for key in (
                "st_dev",
                "st_ino",
                "st_mode",
                "st_nlink",
                "st_size",
                "st_mtime_ns",
                "st_ctime_ns",
            )
        ):
            raise RuntimeError("input changed while reading")
    return data


def run_npm(arguments, source_dir):
    """Preserve inherited OIDC environment; bound both output and wall time."""
    process = subprocess.Popen(
        ["npm", *arguments],
        cwd=source_dir,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    output = {process.stdout: bytearray(), process.stderr: bytearray()}
    deadline = time.monotonic() + TIMEOUT
    try:
        with selectors.DefaultSelector() as selector:
            for stream in output:
                selector.register(stream, selectors.EVENT_READ)
            while selector.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise RuntimeError("npm deadline exceeded")
                for key, _ in selector.select(min(remaining, 0.1)):
                    chunk = os.read(key.fd, 65536)
                    if not chunk:
                        selector.unregister(key.fileobj)
                    else:
                        output[key.fileobj].extend(chunk)
                        if sum(map(len, output.values())) > MAX_OUTPUT:
                            raise RuntimeError("npm output ceiling exceeded")
            result = process.wait(timeout=max(0.001, deadline - time.monotonic()))
        return subprocess.CompletedProcess(
            arguments,
            result,
            bytes(output[process.stdout]).decode("utf-8"),
            bytes(output[process.stderr]).decode("utf-8"),
        )
    finally:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
        process.stdout.close()
        process.stderr.close()


def lookup(identity, source_dir):
    result = run_npm(
        [
            "view",
            f"{identity['name']}@{identity['version']}",
            "version",
            "dist.integrity",
            "--json",
            "--registry",
            REGISTRY,
        ],
        source_dir,
    )
    if result.returncode:
        try:
            error = json.loads(result.stdout)
        except (ValueError, TypeError) as exc:
            raise RuntimeError("npm lookup failed without valid error schema") from exc
        if (
            isinstance(error, dict)
            and set(error) == {"error"}
            and isinstance(error["error"], dict)
            and set(error["error"]) == {"code", "summary", "detail"}
            and error["error"]["code"] == "E404"
            and all(
                isinstance(error["error"][key], str) for key in ("summary", "detail")
            )
        ):
            return False
        raise RuntimeError("npm lookup failed; absence not proven")
    try:
        remote = json.loads(result.stdout)
    except ValueError as exc:
        raise RuntimeError("malformed npm registry identity") from exc
    if (
        not isinstance(remote, dict)
        or remote.get("version") != identity["version"]
        or remote.get("dist.integrity") != identity["integrity"]
    ):
        raise RuntimeError("npm registry identity conflicts with frozen bytes")
    return True


def publish(source_dir: Path, *, tarball: Path, info: Path) -> bool:
    cleanup = shutil.which("safe-rm")
    if cleanup is None:
        raise RuntimeError("safe-rm cleanup dependency unavailable")
    source_dir = source_dir.resolve()
    source_bytes = bounded_read(source_dir / "package.json", 1_048_576)
    info_bytes = bounded_read(info, 4_194_304)
    archive = bounded_read(tarball, 100_000_000)
    source = json.loads(source_bytes)
    frozen = json.loads(info_bytes)
    if (
        not isinstance(frozen, dict)
        or type(frozen.get("schema_version")) is not int
        or frozen["schema_version"] != 1
    ):
        raise RuntimeError("invalid frozen info schema")
    directive = frozen.get("directive")
    if not isinstance(directive, dict) or "npm" not in directive.get(
        "destinations", []
    ):
        raise RuntimeError("npm is not declared")
    canonical = json.loads(
        bounded_read(source_dir / "ci/canonical-targets.json", 4_194_304)
    )
    contract.validate_directive(directive, canonical)
    package = {"package_json": source, "archive": archive}
    identity = contract.freeze_npm_package(package, directive["version"])
    contract.require_frozen_info(
        frozen, directive, canonical, frozen.get("files"), npm_package=package
    )
    if tarball.name != identity["filename"]:
        raise RuntimeError("tarball filename differs from frozen identity")

    def unchanged():
        if (
            bounded_read(tarball, 100_000_000) != archive
            or bounded_read(info, 4_194_304) != info_bytes
            or bounded_read(source_dir / "package.json", 1_048_576) != source_bytes
        ):
            raise RuntimeError("frozen input changed")

    if lookup(identity, source_dir):
        unchanged()
        return False
    unchanged()
    stage = Path(tempfile.mkdtemp(prefix="soldr-npm-verified-", dir="/tmp"))
    try:
        staged = stage / identity["filename"]
        fd = os.open(staged, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(archive)
        staged.chmod(0o400)
        result = run_npm(
            [
                "publish",
                str(staged),
                "--ignore-scripts",
                "--access",
                "public",
                "--registry",
                REGISTRY,
            ],
            source_dir,
        )
        unchanged()
        if result.returncode:
            raise RuntimeError("npm publication failed: " + result.stderr[-2048:])
        if not lookup(identity, source_dir):
            raise RuntimeError(
                "npm publication incomplete: registry identity not visible"
            )
        unchanged()
        return True
    finally:
        # Delete only the newly owned staging directory through session trash.
        subprocess.run(
            [cleanup, "-r", str(stage)],
            check=True,
            timeout=10,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-dir", type=Path, default=Path.cwd())
    parser.add_argument("--tarball", type=Path, required=True)
    parser.add_argument("--info", type=Path, required=True)
    args = parser.parse_args()
    publish(args.source_dir, tarball=args.tarball, info=args.info)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (
        OSError,
        ValueError,
        TypeError,
        RuntimeError,
        subprocess.TimeoutExpired,
    ) as exc:
        raise SystemExit(f"npm publication failed: {exc}") from exc
