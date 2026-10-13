#!/usr/bin/env python3
"""Fail when a `.proto` schema disagrees with its hand-written prost types.

soldr#2753: the `.proto` files are documented as the source of truth for the
daemon wire format and the rust-plan manifests, but nothing read them. They
had drifted three ways, including `wire.proto` referencing a message it never
defined -- which means `protoc` would have rejected the file outright.

soldr does not run `prost-build`; the Rust types carry hand-written
`#[derive(Message)]` + `#[prost(...)]` attributes. So the schema can only be
kept honest by comparing the two artifacts directly, which is what this does:
it extracts (message, field, tag) triples from each side and reports every
disagreement.

Run standalone:

    python3 .github/scripts/check_proto_drift.py

Exit status is 0 when every pair agrees, 1 otherwise.
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

# Each entry pairs a schema with the module carrying its prost types.
PAIRS = [
    (
        "crates/soldr-core/src/core/wire.proto",
        "crates/soldr-core/src/core/wire_proto.rs",
    ),
    (
        "crates/soldr-cache/src/cache_lib/manifest.proto",
        "crates/soldr-cache/src/cache_lib/save.rs",
    ),
]


# Messages whose Rust type lives in another repository.
#
# An entry here is a reviewed exception naming where the type actually lives,
# not a way to silence a finding. soldr#2996 emptied this map when the target
# cache was removed: its only entries mirrored zccache's bundle-manifest types
# for `rust_plan_manifest.proto`, which no longer exists.


@dataclass(frozen=True)
class CrossRepoMessage:
    """One allowlisted message: the proto it lives in and where it is implemented."""

    proto_rel: str
    message: str
    location: str


CROSS_REPO_MESSAGES: tuple[CrossRepoMessage, ...] = ()

# soldr copies of schemas owned by a vendored repo. Keeping the copy identical
# to its origin is the whole point -- soldr#2753's second drift was this
# mirror going stale after the upstream gained two fields, which left tags 15
# and 16 reading as free on soldr's side.


@dataclass
class Message:
    """One protobuf message: field name -> tag, flattened across `oneof`s.

    A `oneof` shares its parent message's tag space, and prost models it as a
    single struct field plus an enum. Flattening both sides to name -> tag is
    what makes them comparable.
    """

    fields: dict = field(default_factory=dict)


def strip_comments(text):
    """Drop `//` line comments. Neither file type uses `/* */`."""
    return "\n".join(
        line[: line.index("//")] if "//" in line else line for line in text.splitlines()
    )


def balanced_body(text, open_index):
    """Return the brace-balanced body starting just after `open_index`."""
    depth = 1
    i = open_index
    while i < len(text) and depth:
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
        i += 1
    return text[open_index : i - 1], i


def pascal_to_snake(name):
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


# `map<k, v> name = N;` -- the angle brackets are why a naive field regex
# misses these. Matched first so the generic pattern cannot mis-read them.
MAP_FIELD = re.compile(r"\bmap\s*<[^>]*>\s+(\w+)\s*=\s*(\d+)\s*;")
SCALAR_FIELD = re.compile(
    r"(?:^|\s)(?:repeated\s+|optional\s+|required\s+)?[\w.]+\s+(\w+)\s*=\s*(\d+)\s*;"
)
ONEOF = re.compile(r"\boneof\s+(\w+)\s*\{")
MESSAGE = re.compile(r"\bmessage\s+(\w+)\s*\{")
# Enums are legal field types too, so a reference to one is not a dangling
# message reference. Omitting this read `CacheLayerKind` as undefined.
ENUM = re.compile(r"\benum\s+(\w+)\s*\{")


def flat_proto_fields(body):
    fields = {}
    for name, tag in MAP_FIELD.findall(body):
        fields[name] = int(tag)
    stripped = MAP_FIELD.sub(" ", body)
    for name, tag in SCALAR_FIELD.findall(stripped):
        fields[name] = int(tag)
    return fields


def proto_fields(body):
    fields = {}
    # `oneof` members live in the parent's tag space; lift them, then remove
    # the block so the outer scan does not see them twice.
    remainder = body
    while True:
        found = ONEOF.search(remainder)
        if not found:
            break
        inner, end = balanced_body(remainder, found.end())
        fields.update(flat_proto_fields(inner))
        remainder = remainder[: found.start()] + remainder[end:]
    fields.update(flat_proto_fields(remainder))
    return fields


def parse_proto(text):
    text = strip_comments(text)
    messages = {}
    for match in MESSAGE.finditer(text):
        body, _ = balanced_body(text, match.end())
        messages[match.group(1)] = Message(fields=proto_fields(body))
    return messages


def proto_enum_names(text):
    """Names of `enum` declarations, which are legal field types."""
    return {match.group(1) for match in ENUM.finditer(strip_comments(text))}


RS_STRUCT = re.compile(
    r"#\[derive\([^)]*\bMessage\b[^)]*\)\]\s*(?:#\[[^\]]*\]\s*)*"
    r"(?:pub(?:\([^)]*\))?\s+)?struct\s+(\w+)\s*\{"
)
RS_ENUM = re.compile(
    r"#\[derive\([^)]*\bOneof\b[^)]*\)\]\s*(?:#\[[^\]]*\]\s*)*"
    r"(?:pub(?:\([^)]*\))?\s+)?enum\s+(\w+)\s*\{"
)
RS_FIELD = re.compile(
    r"#\[prost\((?P<attr>[^\]]*)\)\]\s*(?:pub(?:\([^)]*\))?\s+)?(?P<name>\w+)\s*:"
)
RS_VARIANT = re.compile(r"#\[prost\((?P<attr>[^\]]*)\)\]\s*(?P<name>\w+)\s*\(")
TAG = re.compile(r'\btag\s*=\s*"(\d+)"')
ONEOF_ATTR = re.compile(r'\boneof\s*=\s*"([\w:]+)"')


def rust_oneof_enums(text):
    """Map each `Oneof` enum to {snake_case variant name: tag}."""
    enums = {}
    for match in RS_ENUM.finditer(text):
        body, _ = balanced_body(text, match.end())
        variants = {}
        for variant in RS_VARIANT.finditer(body):
            tag = TAG.search(variant.group("attr"))
            if tag:
                variants[pascal_to_snake(variant.group("name"))] = int(tag.group(1))
        enums[match.group(1)] = variants
    return enums


def parse_rust(text):
    """Extract prost messages, resolving `oneof` fields through their enums."""
    oneof_enums = rust_oneof_enums(text)
    messages = {}
    for match in RS_STRUCT.finditer(text):
        body, _ = balanced_body(text, match.end())
        fields = {}
        for found in RS_FIELD.finditer(body):
            attr = found.group("attr")
            oneof = ONEOF_ATTR.search(attr)
            if oneof:
                # The struct field itself carries no tag; its variants do.
                enum_name = oneof.group(1).rsplit("::", 1)[-1]
                fields.update(oneof_enums.get(enum_name, {}))
                continue
            tag = TAG.search(attr)
            if tag:
                fields[found.group("name")] = int(tag.group(1))
        messages[match.group(1)] = Message(fields=fields)
    return messages


def compare(proto, rust):
    problems = []
    for name in sorted(set(proto) - set(rust)):
        problems.append(
            f"message `{name}` is defined in the schema but has no prost struct"
        )
    for name in sorted(set(rust) - set(proto)):
        problems.append(
            f"message `{name}` has a prost struct but is not defined in the schema"
        )
    for name in sorted(set(proto) & set(rust)):
        proto_fields_ = proto[name].fields
        rust_fields = rust[name].fields
        for missing in sorted(set(rust_fields) - set(proto_fields_)):
            problems.append(
                f"{name}.{missing} (tag {rust_fields[missing]}) is serialized by the "
                f"Rust type but absent from the schema -- the schema "
                f"understates its used tag space"
            )
        for extra in sorted(set(proto_fields_) - set(rust_fields)):
            problems.append(
                f"{name}.{extra} (tag {proto_fields_[extra]}) is in the schema but "
                f"not on the Rust type"
            )
        for shared in sorted(set(proto_fields_) & set(rust_fields)):
            if proto_fields_[shared] != rust_fields[shared]:
                problems.append(
                    f"{name}.{shared} tag mismatch: schema {proto_fields_[shared]}, "
                    f"Rust {rust_fields[shared]}"
                )
    return problems


MESSAGE_TYPED_FIELD = re.compile(
    r"(?:^|\s)(?:repeated\s+|optional\s+|required\s+)?([A-Z][\w.]*)\s+\w+\s*=\s*\d+\s*;"
)


def undefined_references(text, defined):
    """Message types referenced by a field but never defined in the file.

    This is what made `wire.proto` invalid protobuf without anyone noticing.
    Only capitalised (message-typed) references are considered, so scalar
    types such as `string` and `uint64` are naturally skipped.
    """
    stripped = strip_comments(text)
    # Drop message headers so `message Foo {` is not read as a reference.
    stripped = MESSAGE.sub(" { ", stripped)
    stripped = MAP_FIELD.sub(" ", stripped)
    referenced = set()
    for match in MESSAGE_TYPED_FIELD.finditer(stripped):
        referenced.add(match.group(1).split(".")[-1])
    return sorted(referenced - defined)


def check_pair(proto_rel, rust_rel, root):
    proto_path = root / proto_rel
    rust_path = root / rust_rel
    for path in (proto_path, rust_path):
        if not path.exists():
            return [f"{path.relative_to(root)}: file not found"]
    proto_text = proto_path.read_text(encoding="utf-8")
    rust_text = rust_path.read_text(encoding="utf-8")
    proto = parse_proto(proto_text)
    rust = parse_rust(rust_text)
    problems = [
        f"schema references undefined message `{name}` -- protoc would reject this file"
        for name in undefined_references(
            proto_text, set(proto) | proto_enum_names(proto_text)
        )
    ]
    # Types implemented in another repo have no local prost struct by design.
    for entry in CROSS_REPO_MESSAGES:
        if entry.proto_rel == proto_rel:
            proto.pop(entry.message, None)
    problems.extend(compare(proto, rust))
    return problems


def main():
    root = Path(__file__).resolve().parents[2]
    failures = 0
    for proto_rel, rust_rel in PAIRS:
        problems = check_pair(proto_rel, rust_rel, root)
        if problems:
            failures += 1
            print(f"FAIL {proto_rel}")
            print(f"     vs {rust_rel}")
            for problem in problems:
                print(f"     - {problem}")
        else:
            print(f"ok   {proto_rel}")
    if failures:
        print()
        print(
            f"{failures} schema/type pair(s) disagree. The `.proto` files are "
            f"the documented source of truth (soldr#2753); update whichever "
            f"side is wrong so they match."
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
