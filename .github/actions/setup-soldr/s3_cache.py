#!/usr/bin/env python3
"""Optional S3-compatible remote cache for setup-soldr (stdlib only).

Off unless the caller sets the ``s3-bucket`` input *and* supplies
``SOLDR_S3_CACHE_ACCESS_KEY`` / ``SOLDR_S3_CACHE_SECRET_KEY`` from workflow
secrets. Without them ``restore`` reports ``enabled=false`` and the action
keeps using actions/cache exactly as before.

Layout: ``<family>/<key>.tar.zst`` where ``key`` is the cache key
``resolve_setup.py`` already computed, so its restore prefixes become S3
``ListObjectsV2`` prefixes (newest ``LastModified`` wins, as with
actions/cache).

Eviction is owned by the merge-queue singleton: only a ``merge_group`` run
sweeps the bucket down to ``s3-cache-limit``. Pull-request and push runs
only read and write their own keys, so concurrent runs never race a sweep.

When restore finds a family completely empty the remote was wiped; the save
step then *seeds* the runner-local L2 directory with its archive (and
re-uploads) instead of dropping the work.

Usage: ``s3_cache.py restore`` / ``s3_cache.py save`` (env-driven).
"""

from __future__ import annotations

import hashlib
import hmac
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Callable, Iterable, Union
from urllib.parse import quote

ACCESS_KEY_ENV = "SOLDR_S3_CACHE_ACCESS_KEY"
SECRET_KEY_ENV = "SOLDR_S3_CACHE_SECRET_KEY"
STATE_ENV = "SOLDR_S3_CACHE_STATE"
DEFAULT_LIMIT = "50G"
MIN_EVICT_AGE = timedelta(minutes=30)
FAMILIES = ("setup", "build")
SINGLETON_EVENT = "merge_group"
UNSIGNED_PAYLOAD = "UNSIGNED-PAYLOAD"

# (status, headers, body); body is bytes, a readable file or None.
Transport = Callable[[str, str, dict, object], "tuple[int, dict, bytes]"]
Archive = Union[bytes, Path]


# --- size parsing ----------------------------------------------------------

_SIZE_RE = re.compile(r"^\s*(\d+(?:\.\d+)?)\s*([KMGT]?)(I?B?)\s*$", re.IGNORECASE)
_EXP = {"": 0, "K": 1, "M": 2, "G": 3, "T": 4}


def parse_size(text: str) -> int:
    """``50G``/``50GiB`` are binary, ``50GB`` decimal, bare digits are bytes."""
    match = _SIZE_RE.match(text or "")
    if not match:
        raise ValueError(f"invalid size {text!r}; expected e.g. 50G, 512MiB, 1TB")
    number, unit, tail = match.group(1), match.group(2).upper(), match.group(3).upper()
    if not unit and tail not in ("", "B"):
        raise ValueError(f"invalid size {text!r}")
    base = 1000 if tail == "B" and unit else 1024
    return int(float(number) * base ** _EXP[unit])


def resolve_limit(text: str) -> int:
    return parse_size(text.strip() or DEFAULT_LIMIT)


# --- SigV4 -------------------------------------------------------------------


def _hmac(key: bytes, msg: str) -> bytes:
    return hmac.new(key, msg.encode("utf-8"), hashlib.sha256).digest()


def sigv4_authorization(
    *,
    method: str,
    host: str,
    path: str,
    query: dict,
    headers: dict,
    payload_sha256: str,
    amz_date: str,
    region: str,
    access_key: str,
    secret_key: str,
    service: str = "s3",
) -> str:
    signed = {k.lower(): str(v).strip() for k, v in headers.items()}
    signed["host"] = host
    signed["x-amz-content-sha256"] = payload_sha256
    signed["x-amz-date"] = amz_date
    names = sorted(signed)
    canonical_query = "&".join(
        f"{quote(str(k), safe='-_.~')}={quote(str(v), safe='-_.~')}"
        for k, v in sorted(query.items())
    )
    canonical = "\n".join(
        [
            method,
            quote(path, safe="/-_.~"),
            canonical_query,
            "".join(f"{n}:{signed[n]}\n" for n in names),
            ";".join(names),
            payload_sha256,
        ]
    )
    day = amz_date[:8]
    scope = f"{day}/{region}/{service}/aws4_request"
    to_sign = "\n".join(
        [
            "AWS4-HMAC-SHA256",
            amz_date,
            scope,
            hashlib.sha256(canonical.encode("utf-8")).hexdigest(),
        ]
    )
    key = _hmac(("AWS4" + secret_key).encode("utf-8"), day)
    for part in (region, service, "aws4_request"):
        key = _hmac(key, part)
    signature = hmac.new(key, to_sign.encode("utf-8"), hashlib.sha256).hexdigest()
    return (
        f"AWS4-HMAC-SHA256 Credential={access_key}/{scope},"
        f"SignedHeaders={';'.join(names)},Signature={signature}"
    )


# --- client ------------------------------------------------------------------


@dataclass(frozen=True)
class S3Object:
    name: str
    size: int
    last_modified: datetime


def urllib_transport(method: str, url: str, headers: dict, body: Any):
    request = urllib.request.Request(url, data=body, method=method, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=300) as response:
            return response.status, dict(response.headers), response.read()
    except urllib.error.HTTPError as err:
        return err.code, dict(err.headers or {}), err.read() or b""


class S3Error(RuntimeError):
    pass


class S3Client:
    """Path-style S3 client: ``https://<endpoint>/<bucket>/<key>``."""

    def __init__(
        self,
        *,
        bucket: str,
        endpoint: str,
        region: str,
        access_key: str,
        secret_key: str,
        transport: Transport = urllib_transport,
        clock: Callable[[], datetime] = lambda: datetime.now(timezone.utc),
    ) -> None:
        host = re.sub(r"^https?://", "", endpoint).rstrip("/")
        self._host = host
        self._bucket = bucket
        self._region = region
        self._access_key = access_key
        self._secret_key = secret_key
        self._transport = transport
        self._clock = clock

    def __repr__(self) -> str:  # never expose credentials
        return f"S3Client(bucket={self._bucket!r}, endpoint={self._host!r})"

    def _request(self, method, key="", query=None, body=None, payload=None):
        query = query or {}
        path = f"/{self._bucket}" + (f"/{key}" if key else "")
        if payload is None:
            payload = hashlib.sha256(
                body if isinstance(body, bytes) else b""
            ).hexdigest()
        amz_date = self._clock().strftime("%Y%m%dT%H%M%SZ")
        headers = {}
        if body is not None and not isinstance(body, bytes):
            headers["content-length"] = str(os.fstat(body.fileno()).st_size)
        auth = sigv4_authorization(
            method=method,
            host=self._host,
            path=path,
            query=query,
            headers=headers,
            payload_sha256=payload,
            amz_date=amz_date,
            region=self._region,
            access_key=self._access_key,
            secret_key=self._secret_key,
        )
        headers.update(
            {
                "x-amz-date": amz_date,
                "x-amz-content-sha256": payload,
                "Authorization": auth,
            }
        )
        url = f"https://{self._host}{quote(path, safe='/-_.~')}"
        if query:
            url += "?" + "&".join(
                f"{quote(str(k), safe='-_.~')}={quote(str(v), safe='-_.~')}"
                for k, v in sorted(query.items())
            )
        return self._transport(method, url, headers, body)

    def put(self, name: str, data: Archive) -> None:
        if isinstance(data, Path):
            with data.open("rb") as fh:
                status, _, body = self._request(
                    "PUT", name, body=fh, payload=UNSIGNED_PAYLOAD
                )
        else:
            status, _, body = self._request("PUT", name, body=data)
        if status >= 300:
            raise S3Error(f"PUT {name} -> HTTP {status}: {body[:200]!r}")

    def get(self, name: str) -> bytes | None:
        status, _, body = self._request("GET", name)
        if status == 404:
            return None
        if status >= 300:
            raise S3Error(f"GET {name} -> HTTP {status}")
        return body

    def head(self, name: str) -> bool:
        status, _, _ = self._request("HEAD", name)
        if status == 404:
            return False
        if status >= 300:
            raise S3Error(f"HEAD {name} -> HTTP {status}")
        return True

    def delete(self, name: str) -> None:
        status, _, _ = self._request("DELETE", name)
        if status >= 300 and status != 404:
            raise S3Error(f"DELETE {name} -> HTTP {status}")

    def list(self, prefix: str = "") -> list[S3Object]:
        out: list[S3Object] = []
        token = ""
        while True:
            query = {"list-type": "2", "prefix": prefix}
            if token:
                query["continuation-token"] = token
            status, _, body = self._request("GET", query=query)
            if status >= 300:
                raise S3Error(f"LIST {prefix!r} -> HTTP {status}")
            root = ET.fromstring(body)
            for el in root.iter():
                if _local(el.tag) != "Contents":
                    continue
                vals = {_local(c.tag): (c.text or "") for c in el}
                out.append(
                    S3Object(
                        name=vals["Key"],
                        size=int(vals.get("Size", "0")),
                        last_modified=_parse_time(vals["LastModified"]),
                    )
                )
            truncated = _find_text(root, "IsTruncated").lower() == "true"
            token = _find_text(root, "NextContinuationToken")
            if not truncated or not token:
                return out


def _local(tag: str) -> str:
    return tag.rsplit("}", 1)[-1]


def _find_text(root: ET.Element, name: str) -> str:
    for el in root:
        if _local(el.tag) == name:
            return el.text or ""
    return ""


def _parse_time(text: str) -> datetime:
    return datetime.strptime(text[:19], "%Y-%m-%dT%H:%M:%S").replace(
        tzinfo=timezone.utc
    )


# --- key layout / restore ----------------------------------------------------


def object_name(family: str, key: str) -> str:
    return f"{family}/{key}.tar.zst"


@dataclass(frozen=True)
class RestoreHit:
    name: str
    exact: bool


@dataclass(frozen=True)
class RestoreState:
    hit: bool
    wipe_detected: bool
    exact: bool = False
    name: str = ""


def find_restore_object(
    client: S3Client, family: str, key: str, restore_prefixes: Iterable[str]
) -> RestoreHit | None:
    exact = object_name(family, key)
    if client.head(exact):
        return RestoreHit(exact, True)
    for prefix in restore_prefixes:
        if not prefix:
            continue
        found = [
            o for o in client.list(f"{family}/{prefix}") if o.name.endswith(".tar.zst")
        ]
        if found:
            newest = max(found, key=lambda o: (o.last_modified, o.name))
            return RestoreHit(newest.name, False)
    return None


def restore_state(
    client: S3Client, family: str, key: str, restore_prefixes: Iterable[str]
) -> RestoreState:
    hit = find_restore_object(client, family, key, restore_prefixes)
    if hit:
        return RestoreState(
            hit=True, wipe_detected=False, exact=hit.exact, name=hit.name
        )
    # A miss in a family that still holds other objects is an ordinary cold
    # key; a family with *nothing* in it means the remote was wiped.
    return RestoreState(hit=False, wipe_detected=not client.list(f"{family}/"))


# --- janitor -----------------------------------------------------------------


def sweep_allowed(event_name: str) -> bool:
    """Only the serialized merge-queue run may evict (decentralized safety)."""
    return event_name == SINGLETON_EVENT


@dataclass
class EvictionPlan:
    before: int
    after: int
    to_delete: list[S3Object] = field(default_factory=list)


def plan_eviction(
    objects: list[S3Object], *, limit: int, now: datetime, protected: set[str]
) -> EvictionPlan:
    total = sum(o.size for o in objects)
    plan = EvictionPlan(before=total, after=total)
    if total < limit:
        return plan
    candidates = sorted(
        (
            o
            for o in objects
            if o.name not in protected and now - o.last_modified >= MIN_EVICT_AGE
        ),
        key=lambda o: (o.last_modified, o.name),
    )
    for obj in candidates:
        if plan.after < limit:
            break
        plan.to_delete.append(obj)
        plan.after -= obj.size
    return plan


def run_janitor(
    client: S3Client, *, limit: int, now: datetime, protected: set[str]
) -> EvictionPlan:
    objects = [o for fam in FAMILIES for o in client.list(f"{fam}/")]
    plan = plan_eviction(objects, limit=limit, now=now, protected=protected)
    print(f"s3-cache janitor: limit={limit} before={plan.before} after={plan.after}")
    for obj in plan.to_delete:
        print(
            f"s3-cache janitor: evict {obj.name} size={obj.size} "
            f"last_modified={obj.last_modified.isoformat()}"
        )
        client.delete(obj.name)
    if not plan.to_delete:
        print("s3-cache janitor: nothing to evict")
    return plan


# --- save --------------------------------------------------------------------


def seed_l2(l2_root: Path, family: str, key: str, archive: Archive) -> Path:
    dest = l2_root / object_name(family, key)
    dest.parent.mkdir(parents=True, exist_ok=True)
    if isinstance(archive, Path):
        shutil.copyfile(archive, dest)
    else:
        dest.write_bytes(archive)
    return dest


def save_phase(
    client: S3Client,
    *,
    family: str,
    key: str,
    archive: Archive,
    event_name: str,
    limit: int,
    now: datetime,
    state: RestoreState,
    l2_root: Path,
    protected: set[str] | None = None,
) -> str:
    """Returns ``skip`` (exact hit), ``seed`` (wipe detected) or ``save``."""
    name = object_name(family, key)
    protected = set(protected or ()) | {name}
    if state.exact:
        action = "skip"
        print(f"s3-cache: exact hit for {name}; not re-uploading")
    else:
        if state.wipe_detected:
            action = "seed"
            dest = seed_l2(l2_root, family, key, archive)
            print(f"s3-cache: remote wipe detected; seeded L2 cache at {dest}")
        else:
            action = "save"
        client.put(name, archive)
        print(f"s3-cache: uploaded {name}")
    if sweep_allowed(event_name):
        run_janitor(client, limit=limit, now=now, protected=protected)
    else:
        print(
            f"s3-cache: eviction skipped on {event_name or 'unknown'} (merge_group only)"
        )
    return action


# --- config / CLI ------------------------------------------------------------


@dataclass(frozen=True)
class Config:
    bucket: str
    endpoint: str
    region: str
    limit: int
    access_key: str = field(repr=False)
    secret_key: str = field(repr=False)

    def client(self) -> S3Client:
        return S3Client(
            bucket=self.bucket,
            endpoint=self.endpoint,
            region=self.region,
            access_key=self.access_key,
            secret_key=self.secret_key,
        )


def config_from_env(env: dict) -> Config | None:
    bucket = env.get("INPUT_S3_BUCKET", "").strip()
    endpoint = env.get("INPUT_S3_ENDPOINT", "").strip()
    access = env.get(ACCESS_KEY_ENV, "").strip()
    secret = env.get(SECRET_KEY_ENV, "").strip()
    if not (bucket and endpoint and access and secret):
        return None
    region = env.get("INPUT_S3_REGION", "").strip()
    if not region:
        host = re.sub(r"^https?://", "", endpoint)
        match = re.match(r"^([a-z0-9-]+)\.digitaloceanspaces\.com", host)
        region = match.group(1) if match else "us-east-1"
    return Config(
        bucket=bucket,
        endpoint=endpoint,
        region=region,
        limit=resolve_limit(env.get("INPUT_S3_CACHE_LIMIT", "")),
        access_key=access,
        secret_key=secret,
    )


def _families_from_env(env: dict) -> list[dict]:
    out = []
    if env.get("INPUT_CACHE", "").strip().lower() == "true":
        out.append(
            {
                "family": "setup",
                "path": env["SETUP_CACHE_ROOT"],
                "key": env["SETUP_CACHE_KEY"],
                "prefixes": [env.get("SETUP_CACHE_RESTORE_PREFIX", "")],
            }
        )
    if env.get("INPUT_BUILD_CACHE", "").strip().lower() == "true":
        out.append(
            {
                "family": "build",
                "path": env["SETUP_BUILD_CACHE_PATH"],
                "key": env["SETUP_BUILD_CACHE_KEY"],
                "prefixes": [
                    env.get("SETUP_BUILD_CACHE_RESTORE_TOOLCHAIN", ""),
                    env.get("SETUP_BUILD_CACHE_RESTORE_OS_ARCH", ""),
                ],
            }
        )
    return out


def _l2_root(env: dict) -> Path:
    explicit = env.get("SOLDR_S3_L2_DIR", "").strip()
    if explicit:
        return Path(explicit)
    # Sibling of the action cache root, so the setup archive never nests it.
    return Path(env["SETUP_CACHE_ROOT"]).parent / "setup-soldr-l2"


def _write_kv(var: str, values: dict) -> None:
    target = os.environ.get(var)
    if not target:
        return
    with open(target, "a", encoding="utf-8") as fh:
        for k, v in values.items():
            fh.write(f"{k}={v}\n")


def _zstd_tar(args: list[str]) -> None:
    if not shutil.which("zstd"):
        raise S3Error("zstd is not on PATH; cannot (un)pack .tar.zst")
    subprocess.run(["tar", "--use-compress-program=zstd -T0", *args], check=True)


def _l2_lookup(
    l2_root: Path, family: str, key: str, prefixes: list[str]
) -> Path | None:
    exact = l2_root / object_name(family, key)
    if exact.is_file():
        return exact
    for prefix in prefixes:
        if prefix:
            found = sorted(
                (l2_root / family).glob(f"{prefix}*.tar.zst"),
                key=lambda p: p.stat().st_mtime,
            )
            if found:
                return found[-1]
    return None


def _restore_family(client: S3Client, bucket: str, l2: Path, fam: dict) -> RestoreState:
    """Restore one family from S3, else from the L2 seed; never raises."""
    name = fam["family"]
    try:
        state = restore_state(client, name, fam["key"], fam["prefixes"])
    except (S3Error, OSError, ET.ParseError) as err:
        print(f"::warning::s3-cache: restore {name} failed: {err}")
        state = RestoreState(hit=False, wipe_detected=False)
    with tempfile.TemporaryDirectory() as tmp:
        source = None
        try:
            data = client.get(state.name) if state.hit else None
            if data is not None:
                source = Path(tmp) / "restore.tar.zst"
                source.write_bytes(data)
                print(f"s3-cache: restoring {name} from s3://{bucket}/{state.name}")
            if source is None:
                source = _l2_lookup(l2, name, fam["key"], fam["prefixes"])
                if source:
                    print(f"s3-cache: restoring {name} from L2 seed {source}")
            if source is not None:
                Path(fam["path"]).mkdir(parents=True, exist_ok=True)
                _zstd_tar(["-xf", str(source), "-C", fam["path"]])
        except (S3Error, OSError, subprocess.CalledProcessError) as err:
            print(f"::warning::s3-cache: restore {name} failed: {err}")
    return state


def cmd_restore(env: dict) -> int:
    cfg = config_from_env(env)
    if cfg is None:
        print("s3-cache: not configured (bucket/endpoint/credentials absent); disabled")
        _write_kv("GITHUB_OUTPUT", {"enabled": "false"})
        return 0
    client = cfg.client()
    l2 = _l2_root(env)
    states: dict[str, dict] = {}
    outputs = {"enabled": "true", "cache_hit": "false", "build_cache_hit": "false"}
    for fam in _families_from_env(env):
        name = fam["family"]
        state = _restore_family(client, cfg.bucket, l2, fam)
        states[name] = {
            **fam,
            "hit": state.hit,
            "wipe_detected": state.wipe_detected,
            "exact": state.exact,
        }
        if state.exact:
            outputs["cache_hit" if name == "setup" else "build_cache_hit"] = "true"
        if state.wipe_detected:
            print(
                f"s3-cache: {name} family is empty remotely (wipe detected); save will seed L2"
            )
    state_path = (
        Path(env.get("RUNNER_TEMP", tempfile.gettempdir()))
        / "setup-soldr-s3-state.json"
    )
    state_path.write_text(
        json.dumps({"l2_root": str(l2), "families": states}), encoding="utf-8"
    )
    _write_kv("GITHUB_ENV", {STATE_ENV: str(state_path)})
    _write_kv("GITHUB_OUTPUT", outputs)
    return 0


def cmd_save(env: dict) -> int:
    cfg = config_from_env(env)
    state_file = env.get(STATE_ENV, "").strip()
    if cfg is None or not state_file:
        print("s3-cache: not enabled; nothing to save")
        return 0
    saved = json.loads(Path(state_file).read_text(encoding="utf-8"))
    client = cfg.client()
    event = env.get("SOLDR_S3_EVENT_NAME", "").strip()
    protected: set[str] = set()
    for name, fam in saved["families"].items():
        state = RestoreState(
            hit=bool(fam.get("hit")),
            wipe_detected=bool(fam.get("wipe_detected")),
            exact=bool(fam.get("exact")),
        )
        protected.add(object_name(name, fam["key"]))
        with tempfile.TemporaryDirectory() as tmp:
            archive = Path(tmp) / "save.tar.zst"
            try:
                if not state.exact:
                    _zstd_tar(["-cf", str(archive), "-C", fam["path"], "."])
                save_phase(
                    client,
                    family=name,
                    key=fam["key"],
                    archive=archive,
                    event_name=event,
                    limit=cfg.limit,
                    now=datetime.now(timezone.utc),
                    state=state,
                    l2_root=Path(saved["l2_root"]),
                    protected=protected,
                )
            except (S3Error, OSError, subprocess.CalledProcessError) as err:
                print(f"::warning::s3-cache: save {name} failed: {err}")
    return 0


def main(argv: list[str]) -> int:
    if len(argv) != 2 or argv[1] not in ("restore", "save"):
        print("usage: s3_cache.py restore|save", file=sys.stderr)
        return 2
    env = dict(os.environ)
    return cmd_restore(env) if argv[1] == "restore" else cmd_save(env)


if __name__ == "__main__":
    sys.exit(main(sys.argv))
