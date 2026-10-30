"""Unit tests for setup-soldr's optional S3-compatible remote cache.

No network: every test drives `s3_cache` through an in-memory fake transport.
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "actions" / "setup-soldr" / "s3_cache.py"

NOW = datetime(2026, 10, 30, 12, 0, 0, tzinfo=timezone.utc)
EMPTY_SHA = hashlib.sha256(b"").hexdigest()


@dataclass(frozen=True)
class StoredObject:
    body: bytes
    last_modified: datetime


@dataclass(frozen=True)
class ClientCall:
    method: str
    key: str


@pytest.fixture(name="s3")
def _s3():
    return load_script_module(SCRIPT_PATH, "s3_cache")


class FakeTransport:  # pylint: disable=too-few-public-methods
    """In-memory S3: objects keyed by name -> StoredObject."""

    def __init__(self, s3, objects=None):
        self.s3 = s3
        self.objects: dict[str, StoredObject] = dict(objects or {})
        self.deleted: list[str] = []
        self.calls: list[ClientCall] = []

    def __call__(self, method, url, headers, body):
        from urllib.parse import parse_qs, unquote, urlsplit

        parts = urlsplit(url)
        assert "Authorization" in headers
        key = unquote(parts.path.split("/", 2)[2]) if parts.path.count("/") >= 2 else ""
        query = {k: v[0] for k, v in parse_qs(parts.query).items()}
        self.calls.append(ClientCall(method, key))
        if method == "GET" and "list-type" in query:
            prefix = query.get("prefix", "")
            items = sorted(
                (k, v) for k, v in self.objects.items() if k.startswith(prefix)
            )
            xml = ["<ListBucketResult>", "<IsTruncated>false</IsTruncated>"]
            for name, obj in items:
                xml.append(
                    f"<Contents><Key>{name}</Key><Size>{len(obj.body)}</Size>"
                    f"<LastModified>{obj.last_modified.strftime('%Y-%m-%dT%H:%M:%S.000Z')}"
                    "</LastModified></Contents>"
                )
            xml.append("</ListBucketResult>")
            return 200, {}, "".join(xml).encode()
        if method == "PUT":
            data = body if isinstance(body, bytes) else body.read()
            self.objects[key] = StoredObject(data, NOW)
            return 200, {}, b""
        if method == "DELETE":
            self.objects.pop(key, None)
            self.deleted.append(key)
            return 204, {}, b""
        if method in ("GET", "HEAD"):
            if key not in self.objects:
                return 404, {}, b""
            return 200, {}, b"" if method == "HEAD" else self.objects[key].body
        raise AssertionError(method)


def _client(s3, transport):
    return s3.S3Client(
        bucket="bkt",
        endpoint="nyc3.digitaloceanspaces.com",
        region="nyc3",
        access_key="AK",
        secret_key="SK",
        transport=transport,
    )


# --- SigV4 ---------------------------------------------------------------


def test_sigv4_matches_aws_documented_get_object_vector(s3) -> None:
    # AWS S3 docs, "Signature Calculations ... Example: GET Object".
    auth = s3.sigv4_authorization(
        method="GET",
        host="examplebucket.s3.amazonaws.com",
        path="/test.txt",
        query={},
        headers={"range": "bytes=0-9"},
        payload_sha256=EMPTY_SHA,
        amz_date="20130524T000000Z",
        region="us-east-1",
        access_key="AKIAIOSFODNN7EXAMPLE",
        secret_key="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
    )
    assert auth == (
        "AWS4-HMAC-SHA256 "
        "Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,"
        "SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,"
        "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
    )


# --- size parsing ---------------------------------------------------------


@pytest.mark.parametrize(
    ("text", "expected"),
    [
        ("50G", 50 * 1024**3),
        ("50GiB", 50 * 1024**3),
        ("50GB", 50 * 1000**3),
        ("1.5T", int(1.5 * 1024**4)),
        ("1TB", 1000**4),
        ("512M", 512 * 1024**2),
        ("10k", 10 * 1024),
        ("10KB", 10_000),
        ("123", 123),
        ("7 MiB", 7 * 1024**2),
    ],
)
def test_parse_size_accepts_decimal_and_binary_suffixes(s3, text, expected) -> None:
    assert s3.parse_size(text) == expected


@pytest.mark.parametrize("text", ["", "G", "-1G", "5X", "abc", "1.2.3G"])
def test_parse_size_rejects_garbage(s3, text) -> None:
    with pytest.raises(ValueError):
        s3.parse_size(text)


def test_default_limit_is_50g_when_empty(s3) -> None:
    assert s3.resolve_limit("") == 50 * 1024**3


# --- key layout + prefix fallback ----------------------------------------


def test_object_layout_is_family_slash_key(s3) -> None:
    assert (
        s3.object_name("setup", "setup-soldr-v0-linux-x64-abc")
        == "setup/setup-soldr-v0-linux-x64-abc.tar.zst"
    )


def test_restore_prefers_exact_key_then_newest_prefix_match(s3) -> None:
    old = NOW - timedelta(days=2)
    new = NOW - timedelta(days=1)
    t = FakeTransport(
        s3,
        {
            "build/setup-soldr-buildcache-v1-linux-x64-d1-sha1.tar.zst": StoredObject(
                b"a", old
            ),
            "build/setup-soldr-buildcache-v1-linux-x64-d1-sha2.tar.zst": StoredObject(
                b"b", new
            ),
            "build/setup-soldr-buildcache-v1-linux-x64-d0-sha9.tar.zst": StoredObject(
                b"c", NOW
            ),
        },
    )
    c = _client(s3, t)
    hit = s3.find_restore_object(
        c,
        "build",
        "setup-soldr-buildcache-v1-linux-x64-d1-sha3",
        [
            "setup-soldr-buildcache-v1-linux-x64-d1-",
            "setup-soldr-buildcache-v1-linux-x64-",
        ],
    )
    assert hit.name == "build/setup-soldr-buildcache-v1-linux-x64-d1-sha2.tar.zst"
    assert hit.exact is False

    exact = s3.find_restore_object(
        c, "build", "setup-soldr-buildcache-v1-linux-x64-d1-sha1", []
    )
    assert exact.exact is True


def test_restore_falls_through_to_broader_prefix(s3) -> None:
    t = FakeTransport(
        s3,
        {
            "build/setup-soldr-buildcache-v1-linux-x64-d0-x.tar.zst": StoredObject(
                b"c", NOW
            )
        },
    )
    hit = s3.find_restore_object(
        _client(s3, t),
        "build",
        "setup-soldr-buildcache-v1-linux-x64-d1-sha3",
        [
            "setup-soldr-buildcache-v1-linux-x64-d1-",
            "setup-soldr-buildcache-v1-linux-x64-",
        ],
    )
    assert hit.name.endswith("-d0-x.tar.zst")


def test_client_roundtrips_put_get_head_delete(s3) -> None:
    t = FakeTransport(s3)
    c = _client(s3, t)
    c.put("setup/k.tar.zst", b"payload")
    assert c.head("setup/k.tar.zst") is True
    assert c.get("setup/k.tar.zst") == b"payload"
    c.delete("setup/k.tar.zst")
    assert c.head("setup/k.tar.zst") is False


# --- janitor --------------------------------------------------------------


def _objs(s3, specs):
    return [
        s3.S3Object(name=n, size=sz, last_modified=NOW - age) for n, sz, age in specs
    ]


def test_janitor_noop_under_watermark(s3) -> None:
    objs = _objs(s3, [("a", 10, timedelta(days=3)), ("b", 10, timedelta(days=2))])
    plan = s3.plan_eviction(objs, limit=100, now=NOW, protected=set())
    assert plan.to_delete == []
    assert plan.before == 20 and plan.after == 20


def test_janitor_deletes_lru_until_below_watermark(s3) -> None:
    objs = _objs(
        s3,
        [
            ("newest", 40, timedelta(hours=2)),
            ("oldest", 40, timedelta(days=5)),
            ("middle", 40, timedelta(days=1)),
        ],
    )
    plan = s3.plan_eviction(objs, limit=100, now=NOW, protected=set())
    assert [o.name for o in plan.to_delete] == ["oldest"]
    assert plan.before == 120 and plan.after == 80
    plan = s3.plan_eviction(objs, limit=50, now=NOW, protected=set())
    assert [o.name for o in plan.to_delete] == ["oldest", "middle"]


def test_janitor_never_deletes_current_run_or_young_objects(s3) -> None:
    objs = _objs(
        s3,
        [
            ("mine", 100, timedelta(days=9)),
            ("young", 100, timedelta(minutes=29)),
            ("old", 100, timedelta(days=1)),
        ],
    )
    plan = s3.plan_eviction(objs, limit=1, now=NOW, protected={"mine"})
    assert [o.name for o in plan.to_delete] == ["old"]
    assert plan.after == 200  # cannot reach watermark safely; stops


def test_janitor_run_is_idempotent_and_logs(s3, capsys) -> None:
    t = FakeTransport(
        s3,
        {
            "setup/a.tar.zst": StoredObject(b"x" * 60, NOW - timedelta(days=3)),
            "setup/b.tar.zst": StoredObject(b"x" * 60, NOW - timedelta(days=1)),
        },
    )
    c = _client(s3, t)
    s3.run_janitor(c, limit=100, now=NOW, protected=set())
    assert t.deleted == ["setup/a.tar.zst"]
    s3.run_janitor(c, limit=100, now=NOW, protected=set())
    assert t.deleted == ["setup/a.tar.zst"]
    out = capsys.readouterr().out
    assert "before=120" in out and "after=60" in out
    assert "evict setup/a.tar.zst" in out
    assert "SK" not in out


# --- merge-queue singleton gating -----------------------------------------


def test_sweep_runs_only_on_merge_group(s3) -> None:
    assert s3.sweep_allowed("merge_group") is True


@pytest.mark.parametrize(
    "event", ["pull_request", "push", "pull_request_target", "workflow_dispatch", ""]
)
def test_sweep_skipped_off_merge_group(s3, event) -> None:
    assert s3.sweep_allowed(event) is False


def test_save_on_pull_request_never_deletes(s3, tmp_path) -> None:
    t = FakeTransport(
        s3, {"setup/old.tar.zst": StoredObject(b"x" * 500, NOW - timedelta(days=30))}
    )
    s3.save_phase(
        _client(s3, t),
        family="setup",
        key="k1",
        archive=b"y" * 10,
        event_name="pull_request",
        limit=1,
        now=NOW,
        state=s3.RestoreState(hit=True, wipe_detected=False),
        l2_root=tmp_path,
    )
    assert not t.deleted
    assert "setup/k1.tar.zst" in t.objects


def test_save_on_merge_group_sweeps_but_spares_own_object(s3, tmp_path) -> None:
    t = FakeTransport(
        s3, {"setup/old.tar.zst": StoredObject(b"x" * 500, NOW - timedelta(days=30))}
    )
    s3.save_phase(
        _client(s3, t),
        family="setup",
        key="k1",
        archive=b"y" * 10,
        event_name="merge_group",
        limit=1,
        now=NOW,
        state=s3.RestoreState(hit=True, wipe_detected=False),
        l2_root=tmp_path,
    )
    assert t.deleted == ["setup/old.tar.zst"]
    assert "setup/k1.tar.zst" in t.objects


# --- wipe -> seed ----------------------------------------------------------


def test_restore_detects_wipe_when_family_is_empty(s3) -> None:
    state = s3.restore_state(_client(s3, FakeTransport(s3)), "setup", "k1", ["k"])
    assert state.hit is False and state.wipe_detected is True


def test_restore_miss_without_wipe_when_family_has_other_objects(s3) -> None:
    t = FakeTransport(s3, {"setup/other.tar.zst": StoredObject(b"z", NOW)})
    state = s3.restore_state(_client(s3, t), "setup", "k1", ["k1-"])
    assert state.hit is False and state.wipe_detected is False


def test_wipe_detected_takes_seed_path_and_writes_l2(s3, tmp_path) -> None:
    t = FakeTransport(s3)
    action = s3.save_phase(
        _client(s3, t),
        family="setup",
        key="k1",
        archive=b"warm",
        event_name="pull_request",
        limit=10**9,
        now=NOW,
        state=s3.RestoreState(hit=False, wipe_detected=True),
        l2_root=tmp_path,
    )
    assert action == "seed"
    seeded = tmp_path / "setup" / "k1.tar.zst"
    assert seeded.read_bytes() == b"warm"
    assert "setup/k1.tar.zst" in t.objects  # remote re-seeded too


def test_no_wipe_takes_normal_save_path(s3, tmp_path) -> None:
    t = FakeTransport(s3)
    action = s3.save_phase(
        _client(s3, t),
        family="setup",
        key="k1",
        archive=b"warm",
        event_name="pull_request",
        limit=10**9,
        now=NOW,
        state=s3.RestoreState(hit=False, wipe_detected=False),
        l2_root=tmp_path,
    )
    assert action == "save"
    assert not (tmp_path / "setup").exists()
    assert "setup/k1.tar.zst" in t.objects


def test_exact_hit_skips_upload(s3, tmp_path) -> None:
    t = FakeTransport(s3, {"setup/k1.tar.zst": StoredObject(b"old", NOW)})
    action = s3.save_phase(
        _client(s3, t),
        family="setup",
        key="k1",
        archive=b"new",
        event_name="pull_request",
        limit=10**9,
        now=NOW,
        state=s3.RestoreState(hit=True, wipe_detected=False, exact=True),
        l2_root=tmp_path,
    )
    assert action == "skip"
    assert t.objects["setup/k1.tar.zst"].body == b"old"


# --- enablement -------------------------------------------------------------


def test_disabled_without_bucket_or_credentials(s3) -> None:
    assert s3.config_from_env({}) is None
    assert s3.config_from_env({"INPUT_S3_BUCKET": "b"}) is None
    cfg = s3.config_from_env(
        {
            "INPUT_S3_BUCKET": "b",
            "INPUT_S3_ENDPOINT": "nyc3.digitaloceanspaces.com",
            "SOLDR_S3_CACHE_ACCESS_KEY": "a",
            "SOLDR_S3_CACHE_SECRET_KEY": "s",
        }
    )
    assert cfg is not None
    assert cfg.region == "nyc3"  # derived from the DO endpoint
    assert cfg.limit == 50 * 1024**3
    assert "secret_key='s'" not in repr(cfg)
    assert "access_key='a'" not in repr(cfg)


# --- CLI restore -> save round trip (fake transport, real tar+zstd) ----------


@pytest.mark.skipif(
    __import__("shutil").which("zstd") is None, reason="zstd not on PATH"
)
@pytest.mark.parametrize(
    ("event", "evicts"), [("merge_group", True), ("pull_request", False)]
)
def test_cli_wipe_seeds_l2_and_gates_sweep(
    s3, tmp_path, monkeypatch, event, evicts
) -> None:
    root = tmp_path / "setup-soldr"
    (root / "soldr").mkdir(parents=True)
    (root / "soldr" / "marker").write_text("warm")
    t = FakeTransport(s3)
    monkeypatch.setattr(s3.Config, "client", lambda self: _client(s3, t))
    env = {
        "INPUT_S3_BUCKET": "bkt",
        "INPUT_S3_ENDPOINT": "nyc3.digitaloceanspaces.com",
        "INPUT_S3_CACHE_LIMIT": "1",
        "SOLDR_S3_CACHE_ACCESS_KEY": "AK",
        "SOLDR_S3_CACHE_SECRET_KEY": "SK",
        "INPUT_CACHE": "true",
        "INPUT_BUILD_CACHE": "false",
        "SETUP_CACHE_ROOT": str(root),
        "SETUP_CACHE_KEY": "setup-soldr-v0-linux-x64-d1",
        "SETUP_CACHE_RESTORE_PREFIX": "setup-soldr-v0-linux-x64-",
        "RUNNER_TEMP": str(tmp_path),
    }
    monkeypatch.delenv("GITHUB_OUTPUT", raising=False)
    monkeypatch.delenv("GITHUB_ENV", raising=False)
    assert s3.cmd_restore(env) == 0
    # A stale object from another family member appears after restore (old).
    t.objects["build/stale.tar.zst"] = StoredObject(b"x" * 100, NOW - timedelta(days=9))
    env[s3.STATE_ENV] = str(tmp_path / "setup-soldr-s3-state.json")
    env["SOLDR_S3_EVENT_NAME"] = event
    assert s3.cmd_save(env) == 0
    assert (
        tmp_path / "setup-soldr-l2" / "setup" / "setup-soldr-v0-linux-x64-d1.tar.zst"
    ).is_file()
    assert "setup/setup-soldr-v0-linux-x64-d1.tar.zst" in t.objects
    assert ("build/stale.tar.zst" in t.deleted) is evicts
