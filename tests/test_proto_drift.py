"""Tests for `.github/scripts/check_proto_drift.py` (soldr#2753).

The point of these is that the checker must *detect* drift. A schema checker
that only ever passes is worse than none, because it advertises a guarantee it
does not provide -- which is exactly how soldr#2753 happened in the first
place. So most of these feed it known-bad input and assert it complains.
"""

from __future__ import annotations

import importlib.util
import re
import sys
from pathlib import Path

from conftest import lint_lane_text

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / ".github" / "scripts" / "check_proto_drift.py"


def _load():
    spec = importlib.util.spec_from_file_location("check_proto_drift", SCRIPT)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["check_proto_drift"] = module
    spec.loader.exec_module(module)
    return module


drift = _load()


# --------------------------------------------------------------------------
# The real tree must agree. This is the regression guard proper.
# --------------------------------------------------------------------------


def test_repository_schemas_agree_with_their_prost_types() -> None:
    for proto_rel, rust_rel in drift.PAIRS:
        problems = drift.check_pair(proto_rel, rust_rel, ROOT)
        assert problems == [], f"{proto_rel} drifted: {problems}"


def test_every_configured_pair_exists() -> None:
    for proto_rel, rust_rel in drift.PAIRS:
        assert (ROOT / proto_rel).is_file(), proto_rel
        assert (ROOT / rust_rel).is_file(), rust_rel


# --------------------------------------------------------------------------
# Parsing shapes that previously produced false positives.
# --------------------------------------------------------------------------


def test_oneof_members_share_the_parent_tag_space() -> None:
    proto = drift.parse_proto("""
        message Req {
          oneof kind {
            Unit status = 1;
            Unit shutdown = 2;
          }
          string trailing = 3;
        }
        """)
    assert proto["Req"].fields == {"status": 1, "shutdown": 2, "trailing": 3}


def test_map_fields_are_parsed() -> None:
    """`map<k, v>` has angle brackets a naive field regex skips."""
    proto = drift.parse_proto("""
        message Profile {
          map<string, uint64> counters = 1;
          map<string, uint64> timings_ns = 2;
        }
        """)
    assert proto["Profile"].fields == {"counters": 1, "timings_ns": 2}


def test_enum_references_are_not_undefined_messages() -> None:
    text = """
        message M { Kind kind = 1; }
        enum Kind { A = 0; }
        """
    defined = set(drift.parse_proto(text)) | drift.proto_enum_names(text)
    assert drift.undefined_references(text, defined) == []


def test_rust_oneof_is_resolved_through_its_enum() -> None:
    rust = drift.parse_rust("""
        #[derive(Clone, PartialEq, Message)]
        pub struct Req {
            #[prost(oneof = "ReqKind", tags = "1,2")]
            pub kind: Option<ReqKind>,
        }

        #[derive(Clone, PartialEq, Oneof)]
        pub enum ReqKind {
            #[prost(message, tag = "1")]
            RecordTargetTouch(Touch),
            #[prost(message, tag = "2")]
            Status(Unit),
        }
        """)
    assert rust["Req"].fields == {"record_target_touch": 1, "status": 2}


def test_comments_do_not_create_fields() -> None:
    proto = drift.parse_proto("""
        message M {
          // string ghost = 9;
          string real = 1;
        }
        """)
    assert proto["M"].fields == {"real": 1}


# --------------------------------------------------------------------------
# Detection: each is a drift the checker must not miss.
# --------------------------------------------------------------------------


def test_detects_undefined_message_reference() -> None:
    """soldr#2753 drift 1 -- this made wire.proto invalid protobuf."""
    text = "message R { Missing warning = 16; }"
    defined = set(drift.parse_proto(text)) | drift.proto_enum_names(text)
    assert drift.undefined_references(text, defined) == ["Missing"]


def test_detects_field_serialized_by_rust_but_absent_from_schema() -> None:
    """soldr#2753 drift 2 -- understated tag space, as in soldr#1838."""
    proto = drift.parse_proto("message Plan { string a = 1; }")
    rust = drift.parse_rust("""
        #[derive(Clone, PartialEq, Message)]
        pub struct Plan {
            #[prost(string, tag = "1")]
            pub a: String,
            #[prost(bool, tag = "16")]
            pub cargo_artifacts_complete: bool,
        }
        """)
    problems = drift.compare(proto, rust)
    assert any("cargo_artifacts_complete" in p and "tag 16" in p for p in problems)
    assert any("understates its used tag space" in p for p in problems)


def test_detects_tag_mismatch() -> None:
    proto = drift.parse_proto("message M { string a = 1; }")
    rust = drift.parse_rust("""
        #[derive(Clone, PartialEq, Message)]
        pub struct M {
            #[prost(string, tag = "2")]
            pub a: String,
        }
        """)
    problems = drift.compare(proto, rust)
    assert any("tag mismatch" in p for p in problems)


def test_detects_message_missing_from_schema() -> None:
    proto = drift.parse_proto("message Kept { string a = 1; }")
    rust = drift.parse_rust("""
        #[derive(Clone, PartialEq, Message)]
        pub struct Kept {
            #[prost(string, tag = "1")]
            pub a: String,
        }

        #[derive(Clone, PartialEq, Message)]
        pub struct Undocumented {
            #[prost(bool, tag = "1")]
            pub emit: bool,
        }
        """)
    problems = drift.compare(proto, rust)
    assert any(
        "Undocumented" in p and "not defined in the schema" in p for p in problems
    )


def test_detects_schema_message_with_no_rust_type() -> None:
    proto = drift.parse_proto("message Orphan { string a = 1; }")
    problems = drift.compare(proto, {})
    assert any("Orphan" in p and "no prost struct" in p for p in problems)


def test_agreeing_pair_reports_nothing() -> None:
    proto = drift.parse_proto("message M { string a = 1; bool b = 2; }")
    rust = drift.parse_rust("""
        #[derive(Clone, PartialEq, Message)]
        pub struct M {
            #[prost(string, tag = "1")]
            pub a: String,
            #[prost(bool, tag = "2")]
            pub b: bool,
        }
        """)
    assert drift.compare(proto, rust) == []


# --------------------------------------------------------------------------
# Cross-repo types.
# --------------------------------------------------------------------------


def test_cross_repo_entries_name_where_the_type_lives() -> None:
    """An exception must identify the released external implementation."""
    for entry in drift.CROSS_REPO_MESSAGES:
        assert entry.message
        assert "zccache 1.13.7" in entry.location


# --------------------------------------------------------------------------
# CI wiring.
# --------------------------------------------------------------------------


def test_ci_runs_the_drift_check() -> None:
    workflow = lint_lane_text()
    assert "check_proto_drift.py" in workflow


# --------------------------------------------------------------------------
# CLAUDE.md must name the real schema/type pairs (soldr#3649).
# --------------------------------------------------------------------------


def test_claude_md_serialization_paths_exist_and_are_checked_pairs() -> None:
    lines = (ROOT / "CLAUDE.md").read_text(encoding="utf-8").splitlines()
    start = next(i for i, ln in enumerate(lines) if ln.startswith("## Serialization"))
    end = next(
        (i for i in range(start + 1, len(lines)) if lines[i].startswith("## ")),
        len(lines),
    )
    section = lines[start:end]
    bullet_start = next(
        i
        for i, ln in enumerate(section)
        if ln.startswith(
            "- **Binary transports and persisted-state metadata MUST use Protocol Buffers**"
        )
    )
    bullet_end = next(
        (
            i
            for i in range(bullet_start + 1, len(section))
            if section[i].startswith("- **")
        ),
        len(section),
    )
    bullet = "\n".join(section[bullet_start:bullet_end])

    tokens = [
        t for t in re.findall(r"`([^`\s]+\.(?:rs|proto))`", bullet) if "{" not in t
    ]
    pair_paths = {p for pair in drift.PAIRS for p in pair}
    full_tokens = [t for t in tokens if "/" in t]
    for token in tokens:
        if "/" in token:
            assert (ROOT / token).exists(), f"CLAUDE.md names missing path: {token}"
        else:
            candidates = pair_paths | set(full_tokens)
            assert any(c.endswith("/" + token) for c in candidates), (
                f"CLAUDE.md bare filename matches no checked pair or named path: {token}"
            )

    for path in sorted(pair_paths):
        assert path in bullet, f"CLAUDE.md does not name checked pair path: {path}"
    pair_protos = {proto for proto, _ in drift.PAIRS}
    for token in full_tokens:
        if token.endswith(".proto"):
            assert token in pair_protos, f"CLAUDE.md names .proto not in PAIRS: {token}"
