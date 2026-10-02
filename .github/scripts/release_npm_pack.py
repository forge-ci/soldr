#!/usr/bin/env python3
"""Retain verified npm pack bytes. Caller must authenticate source and freezes.

No registry operations or release authority. Resume copies exact frozen bytes
and never invokes npm. Controller/tool-version pinning remains separate.
"""

import argparse
import hashlib
import gzip
import io
import tarfile
import importlib.util
import json
import os
import shutil
import stat
import subprocess
import tempfile
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "npm_pack_publisher", Path(__file__).with_name("publish_npm_package.py")
)
publisher = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(publisher)


def strict_json(data):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate JSON key")
            result[key] = value
        return result

    return json.loads(data, object_pairs_hook=unique)


def require(value, message):
    if not value:
        raise ValueError(message)


def verify_receipt(receipt, identity, archive):
    keys = {
        "id",
        "name",
        "version",
        "size",
        "unpackedSize",
        "shasum",
        "integrity",
        "filename",
        "files",
        "entryCount",
        "bundled",
    }
    require(
        isinstance(receipt, dict) and set(receipt) == keys, "npm pack receipt fields"
    )
    for key in ("name", "version", "size", "integrity", "filename"):
        require(
            type(receipt[key]) is type(identity[key]) and receipt[key] == identity[key],
            "npm pack identity mismatch",
        )
    require(
        receipt["id"] == identity["name"] + "@" + identity["version"],
        "npm pack id mismatch",
    )
    require(
        receipt["shasum"] == hashlib.sha1(archive).hexdigest(),
        "npm pack shasum mismatch",
    )
    expected = {
        row["name"].removeprefix("package/"): row["size"] for row in identity["files"]
    }
    require(
        isinstance(receipt["files"], list) and len(receipt["files"]) == len(expected),
        "npm pack inventory mismatch",
    )
    # The contract has already bounded decompression and validated every
    # member; additionally bind npm's mode observations to the retained tar.
    with gzip.GzipFile(fileobj=io.BytesIO(archive)) as stream:
        expanded = stream.read(publisher.contract.MAX_NPM_EXPANDED_BYTES + 1)
    require(
        len(expanded) <= publisher.contract.MAX_NPM_EXPANDED_BYTES,
        "npm expanded byte ceiling",
    )
    with tarfile.open(fileobj=io.BytesIO(expanded), mode="r:") as parsed:
        modes = {entry.name.removeprefix("package/"): entry.mode for entry in parsed}
    seen = set()
    for row in receipt["files"]:
        require(
            isinstance(row, dict) and set(row) == {"path", "size", "mode"},
            "npm pack file fields",
        )
        require(
            isinstance(row["path"], str)
            and row["path"] in expected
            and row["path"] not in seen
            and type(row["size"]) is int
            and row["size"] == expected[row["path"]]
            and type(row["mode"]) is int
            and 0 <= row["mode"] <= 0o7777
            and row["mode"] == modes[row["path"]],
            "npm pack inventory mismatch",
        )
        seen.add(row["path"])
    require(
        type(receipt["entryCount"]) is int and receipt["entryCount"] == len(expected),
        "npm pack entry count",
    )
    require(
        type(receipt["unpackedSize"]) is int
        and receipt["unpackedSize"] == sum(expected.values()),
        "npm pack unpacked size",
    )
    require(receipt["bundled"] == [], "bundled npm dependencies unsupported")


def pack(
    source_dir, *, version, output_parent, frozen_identity=None, frozen_tarball=None
):
    cleanup = shutil.which("safe-rm")
    require(cleanup is not None, "safe-rm cleanup dependency unavailable")
    parent = Path(output_parent)
    # Reject symlink traversal in every parent component before using it.
    require(
        parent.is_absolute() and ".." not in parent.parts,
        "absolute output parent required",
    )
    for part in [parent, *parent.parents]:
        require(not part.is_symlink(), "output parent symlink refused")
    parent_stat = parent.stat()
    require(
        stat.S_ISDIR(parent_stat.st_mode)
        and parent_stat.st_uid == os.getuid()
        and parent_stat.st_mode & 0o077 == 0,
        "output parent must be owner-private directory",
    )
    require(
        (frozen_identity is None) == (frozen_tarball is None),
        "resume identity and tarball required together",
    )
    source_dir = Path(source_dir).resolve()
    source_bytes = publisher.bounded_read(source_dir / "package.json", 1_048_576)
    source = strict_json(source_bytes)
    require(
        isinstance(source, dict) and source.get("version") == version,
        "npm source version mismatch",
    )
    stage = Path(tempfile.mkdtemp(prefix="npm-pack-", dir=parent))
    retained = False
    try:
        if frozen_identity is not None:
            archive = publisher.bounded_read(Path(frozen_tarball), 100_000_000)
            identity = publisher.contract.freeze_npm_package(
                {"package_json": source, "archive": archive}, version
            )
            require(
                publisher.contract.canonical_digest(identity)
                == publisher.contract.canonical_digest(frozen_identity),
                "frozen npm identity mismatch",
            )
            receipt = None
        else:
            result = publisher.run_npm(
                [
                    "pack",
                    "--json",
                    "--pack-destination",
                    str(stage),
                    "--workspaces=false",
                    "--ignore-scripts=false",
                    "--foreground-scripts=false",
                ],
                source_dir,
            )
            require(result.returncode == 0, "npm pack failed: " + result.stderr[-2048:])
            listing = strict_json(result.stdout)
            require(
                isinstance(listing, list) and len(listing) == 1,
                "exactly one npm pack receipt required",
            )
            receipt = listing[0]
            require(
                isinstance(receipt, dict)
                and isinstance(receipt.get("filename"), str)
                and Path(receipt["filename"]).name == receipt["filename"]
                and receipt["filename"] not in ("", ".", ".."),
                "unsafe npm pack filename",
            )
            archive = publisher.bounded_read(stage / receipt["filename"], 100_000_000)
            identity = publisher.contract.freeze_npm_package(
                {"package_json": source, "archive": archive}, version
            )
            verify_receipt(receipt, identity, archive)
            require(
                {p.name for p in stage.iterdir()} == {identity["filename"]},
                "unexpected pack output",
            )
        require(
            publisher.bounded_read(source_dir / "package.json", 1_048_576)
            == source_bytes,
            "source metadata changed during pack",
        )
        staged = stage / identity["filename"]
        if receipt is None:
            fd = os.open(staged, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "wb") as stream:
                stream.write(archive)
        staged.chmod(0o400)
        require(
            stage.stat().st_uid == os.getuid()
            and stage.stat().st_mode & 0o777 == 0o700,
            "private stage changed",
        )
        result = {
            "schema_version": 1,
            "tarball": str(staged),
            "receipt": str(stage / "receipt.json"),
            "npm_identity": identity,
            "source_authority": "caller-must-authenticate",
            "reused_frozen": receipt is None,
            "pack_receipt": receipt,
        }
        fd = os.open(
            stage / "receipt.json", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o400
        )
        with os.fdopen(fd, "w") as stream:
            json.dump(result, stream, sort_keys=True)
        retained = True
        return result
    finally:
        if not retained:
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
    parser.add_argument("--source-dir", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--output-parent", type=Path, required=True)
    parser.add_argument("--frozen-identity", type=Path)
    parser.add_argument("--frozen-tarball", type=Path)
    args = parser.parse_args()
    identity = (
        strict_json(publisher.bounded_read(args.frozen_identity, 4_194_304))
        if args.frozen_identity
        else None
    )
    print(
        json.dumps(
            pack(
                args.source_dir,
                version=args.version,
                output_parent=args.output_parent,
                frozen_identity=identity,
                frozen_tarball=args.frozen_tarball,
            ),
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        raise SystemExit(f"npm pack refused: {error}") from error
