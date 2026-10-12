#!/usr/bin/env python3
"""Build and verify Soldr's positive native target-run selection.

The nextest archive remains complete. Source classifications explain why every
integration module that touches real host facilities either runs once on the
canonical Linux host or needs native target replay. Only separate, positive
test/module replay selectors enter the target-run filter: classifying source
never implicitly selects a whole category binary.
"""

from __future__ import annotations

import argparse
import json
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any

SCHEMA_VERSION = 2
SAFE_NAME = re.compile(r"^[A-Za-z0-9_-]+$")
SAFE_TEST = re.compile(r"^[A-Za-z0-9_:]+$")
SAFE_TARGET = re.compile(r"^[A-Za-z0-9_.-]+$")
MODULE_DECLARATION = re.compile(
    r"(?m)(?P<attrs>(?:^\s*#\[[^]]+\]\s*\n)*)"
    r"^\s*(?:pub\s+)?mod\s+(?P<name>[A-Za-z0-9_]+)\s*;"
)
TEST_DECLARATION = re.compile(
    r"(?m)(?P<attrs>(?:^\s*#\[[^]]+\]\s*\n)+)"
    r"^\s*(?:pub(?:\([^\n)]+\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+"
    r"(?P<name>[A-Za-z0-9_]+)\s*\("
)
TEST_ATTRIBUTE = re.compile(r"(?m)^\s*#\[(?:[A-Za-z0-9_]+::)?test(?:\([^]]*\))?\]")
ATTRIBUTE_LINE = re.compile(r"(?m)^\s*#\[(?P<body>[^]]+)\]\s*$")
HOST_CFG = re.compile(
    r"cfg(?:_attr)?\s*\([^]]*(?:windows|unix|target_os|target_arch|target_env)"
)
HOST_CFG_TERM = re.compile(r"\b(?:windows|unix|target_os|target_arch|target_env)\b")
DISPOSITIONS = {"native-linux-once", "target-replay"}

HOST_MARKERS = (
    "std::process::",
    "std::fs::",
    "std::os::",
    "Command::new(",
    "tempfile::",
    "tokio::process::",
    "running_process::",
    "soldr_platform::fs::",
    "soldr_platform::host::",
    "soldr_platform::ipc::",
    "soldr_platform::process::",
)


@dataclass(frozen=True, order=True)
class SourceKey:
    """One classified test source: a whole binary or one module of it."""

    package: str
    binary: str
    module: str | None


@dataclass(frozen=True, order=True)
class CategoryBinary:
    package: str
    binary: str


@dataclass(frozen=True, order=True)
class InventoryTest:
    """One test discovered in the nextest archive inventory."""

    package: str
    binary: str
    name: str

    def test_id(self) -> str:
        return f"{self.package}::{self.binary}::{self.name}"


@dataclass(frozen=True)
class SelectorCount:
    selector_id: str
    count: int


@dataclass(frozen=True)
class DeclaredTest:
    name: str
    attributes: str


@dataclass(frozen=True)
class SourceClassification:
    id: str
    package: str
    binary: str
    disposition: str
    reason: str
    modules: tuple[str, ...] | None = None

    def source_keys(self) -> tuple[SourceKey, ...]:
        if self.modules is None:
            return (SourceKey(self.package, self.binary, None),)
        return tuple(
            SourceKey(self.package, self.binary, module) for module in self.modules
        )

    def contains_test(self, test_name: str) -> bool:
        return self.modules is None or any(
            test_name.startswith(f"{module}::") for module in self.modules
        )


@dataclass(frozen=True)
class ReplaySelector:
    id: str
    source: SourceClassification
    reason: str
    test_name: str | None = None
    test_prefix: str | None = None
    targets: tuple[str, ...] | None = None

    def applies_to(self, target: str) -> bool:
        return self.targets is None or target in self.targets

    def matches(self, package: str, binary: str, test_name: str) -> bool:
        if self.source.package != package or self.source.binary != binary:
            return False
        if self.test_name is not None:
            return test_name == self.test_name
        assert self.test_prefix is not None
        return test_name.startswith(self.test_prefix)

    def filter_expression(self) -> str:
        expression = f"package({self.source.package}) & binary({self.source.binary})"
        if self.test_name is not None:
            expression += f" & test(/^{self.test_name}$/)"
        else:
            expression += f" & test(/^{self.test_prefix}/)"
        return f"({expression})"


@dataclass(frozen=True)
class CanonicalTarget:
    triple: str
    arch: str
    os: str
    env: str
    unix: bool


@dataclass(frozen=True)
class CanonicalContract:
    targets: tuple[CanonicalTarget, ...]
    replayed: frozenset[str]
    failures: tuple[str, ...]


@dataclass(frozen=True)
class HostCfgPredicate:
    operation: str
    value: str | None = None
    children: tuple[HostCfgPredicate, ...] = ()

    def matches(self, target: CanonicalTarget) -> bool:
        if self.operation == "windows":
            return target.os == "windows"
        if self.operation == "unix":
            return target.unix
        if self.operation == "target_os":
            return target.os == self.value
        if self.operation == "target_arch":
            return target.arch == self.value
        if self.operation == "target_env":
            return target.env == self.value
        if self.operation == "all":
            return all(child.matches(target) for child in self.children)
        if self.operation == "any":
            return any(child.matches(target) for child in self.children)
        if self.operation == "not":
            return not self.children[0].matches(target)
        raise AssertionError(f"unknown host cfg operation: {self.operation}")


@dataclass(frozen=True)
class GatedTest:
    name: str
    predicate: HostCfgPredicate


@dataclass(frozen=True)
class PlatformGatedTest:
    source: SourceKey
    test_name: str
    predicate: HostCfgPredicate


@dataclass(frozen=True)
class OwnershipManifest:
    classifications: tuple[SourceClassification, ...]
    selectors: tuple[ReplaySelector, ...]


@dataclass(frozen=True)
class Selection:
    target: str
    discovered_count: int
    selected_count: int
    test_ids: tuple[str, ...]
    filter_expression: str
    selector_counts: tuple[SelectorCount, ...]


def _require_nonempty_string(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{label} must be a non-empty string")
    return value


def _parse_targets(value: Any, label: str) -> tuple[str, ...] | None:
    if value is None:
        return None
    if not isinstance(value, list) or not value:
        raise ValueError(f"{label} must be a non-empty array")
    targets = tuple(
        _require_nonempty_string(target, f"{label}[{index}]")
        for index, target in enumerate(value)
    )
    if any(not SAFE_TARGET.fullmatch(target) for target in targets):
        raise ValueError(f"{label} contains an unsafe target triple")
    if len(set(targets)) != len(targets):
        raise ValueError(f"{label} contains duplicates")
    return targets


def _target_scopes_overlap(
    left: tuple[str, ...] | None, right: tuple[str, ...] | None
) -> bool:
    return left is None or right is None or not set(left).isdisjoint(right)


def _selector_patterns_overlap(left: ReplaySelector, right: ReplaySelector) -> bool:
    if (
        left.source.package != right.source.package
        or left.source.binary != right.source.binary
    ):
        return False
    if not _target_scopes_overlap(left.targets, right.targets):
        return False
    if left.test_name is not None and right.test_name is not None:
        return left.test_name == right.test_name
    if left.test_prefix is not None and right.test_prefix is not None:
        return left.test_prefix.startswith(
            right.test_prefix
        ) or right.test_prefix.startswith(left.test_prefix)
    exact = left.test_name if left.test_name is not None else right.test_name
    prefix = left.test_prefix if left.test_prefix is not None else right.test_prefix
    assert exact is not None and prefix is not None
    return exact.startswith(prefix)


def parse_manifest(payload: object) -> OwnershipManifest:  # noqa: C901
    if not isinstance(payload, dict):
        raise ValueError("target-run ownership manifest must be a JSON object")
    if payload.get("schema_version") != SCHEMA_VERSION:
        raise ValueError(
            f"target-run ownership schema_version must be {SCHEMA_VERSION}"
        )
    if payload.get("policy_issue") != "soldr#2999":
        raise ValueError("target-run ownership policy_issue must be soldr#2999")
    raw_classifications = payload.get("source_classifications")
    raw_selectors = payload.get("replay_selectors")
    if not isinstance(raw_classifications, list):
        raise ValueError("target-run source_classifications must be an array")
    if not isinstance(raw_selectors, list):
        raise ValueError("target-run replay_selectors must be an array")

    classifications: list[SourceClassification] = []
    classifications_by_id: dict[str, SourceClassification] = {}
    classified_sources: dict[SourceKey, str] = {}
    for index, raw in enumerate(raw_classifications):
        label = f"source_classifications[{index}]"
        if not isinstance(raw, dict):
            raise ValueError(f"{label} must be an object")
        allowed = {"id", "package", "binary", "modules", "disposition", "reason"}
        extra = set(raw) - allowed
        missing = {"id", "package", "binary", "disposition", "reason"} - set(raw)
        if extra or missing:
            raise ValueError(
                f"{label} keys disagree: missing={sorted(missing)} extra={sorted(extra)}"
            )
        classification_id = _require_nonempty_string(raw["id"], f"{label}.id")
        package = _require_nonempty_string(raw["package"], f"{label}.package")
        binary = _require_nonempty_string(raw["binary"], f"{label}.binary")
        disposition = _require_nonempty_string(
            raw["disposition"], f"{label}.disposition"
        )
        reason = _require_nonempty_string(raw["reason"], f"{label}.reason")
        if not SAFE_NAME.fullmatch(classification_id):
            raise ValueError(f"{label}.id contains unsafe characters")
        if not SAFE_NAME.fullmatch(package) or not SAFE_NAME.fullmatch(binary):
            raise ValueError(
                f"{label} package/binary contains unsafe filter characters"
            )
        if disposition not in DISPOSITIONS:
            raise ValueError(
                f"{label}.disposition must be one of {sorted(DISPOSITIONS)}"
            )
        raw_modules = raw.get("modules")
        modules: tuple[str, ...] | None
        if raw_modules is None:
            modules = None
        elif not isinstance(raw_modules, list) or not raw_modules:
            raise ValueError(f"{label}.modules must be a non-empty array")
        else:
            modules = tuple(
                _require_nonempty_string(module, f"{label}.modules[{module_index}]")
                for module_index, module in enumerate(raw_modules)
            )
            if any(not SAFE_NAME.fullmatch(module) for module in modules):
                raise ValueError(f"{label}.modules contains unsafe characters")
            if len(set(modules)) != len(modules):
                raise ValueError(f"{label}.modules contains duplicates")
        if classification_id in classifications_by_id:
            raise ValueError(f"duplicate source classification id: {classification_id}")
        classification = SourceClassification(
            classification_id, package, binary, disposition, reason, modules
        )
        for source_key in classification.source_keys():
            if source_key in classified_sources:
                raise ValueError(
                    "overlapping source classifications: "
                    f"{classified_sources[source_key]} and {classification_id} both classify "
                    f"{source_key}"
                )
            classified_sources[source_key] = classification_id
        classifications.append(classification)
        classifications_by_id[classification_id] = classification

    selectors: list[ReplaySelector] = []
    selector_ids: set[str] = set()
    for index, raw in enumerate(raw_selectors):
        label = f"replay_selectors[{index}]"
        if not isinstance(raw, dict):
            raise ValueError(f"{label} must be an object")
        allowed = {
            "id",
            "source_id",
            "test_name",
            "test_prefix",
            "targets",
            "reason",
        }
        extra = set(raw) - allowed
        missing = {"id", "source_id", "reason"} - set(raw)
        if extra or missing:
            raise ValueError(
                f"{label} keys disagree: missing={sorted(missing)} extra={sorted(extra)}"
            )
        selector_id = _require_nonempty_string(raw["id"], f"{label}.id")
        source_id = _require_nonempty_string(raw["source_id"], f"{label}.source_id")
        reason = _require_nonempty_string(raw["reason"], f"{label}.reason")
        if not SAFE_NAME.fullmatch(selector_id):
            raise ValueError(f"{label}.id contains unsafe characters")
        if selector_id in selector_ids:
            raise ValueError(f"duplicate replay selector id: {selector_id}")
        selector_ids.add(selector_id)
        source = classifications_by_id.get(source_id)
        if source is None:
            raise ValueError(
                f"{label}.source_id refers to stale classification {source_id}"
            )
        if source.disposition != "target-replay":
            raise ValueError(
                f"{label} selects {source_id}, which is classified native-linux-once"
            )
        has_name = "test_name" in raw
        has_prefix = "test_prefix" in raw
        if has_name == has_prefix:
            raise ValueError(
                f"{label} must contain exactly one of test_name/test_prefix"
            )
        test_name = (
            _require_nonempty_string(raw["test_name"], f"{label}.test_name")
            if has_name
            else None
        )
        test_prefix = (
            _require_nonempty_string(raw["test_prefix"], f"{label}.test_prefix")
            if has_prefix
            else None
        )
        pattern = test_name if test_name is not None else test_prefix
        assert pattern is not None
        if not SAFE_TEST.fullmatch(pattern):
            raise ValueError(f"{label} selector contains unsafe filter characters")
        if not source.contains_test(pattern):
            raise ValueError(
                f"{label} selector {pattern!r} is outside classified source {source_id}"
            )
        selector = ReplaySelector(
            selector_id,
            source,
            reason,
            test_name,
            test_prefix,
            _parse_targets(raw.get("targets"), f"{label}.targets"),
        )
        for earlier in selectors:
            if _selector_patterns_overlap(earlier, selector):
                raise ValueError(
                    f"overlapping replay selectors: {earlier.id} and {selector.id}"
                )
        selectors.append(selector)

    for classification in classifications:
        if classification.disposition != "target-replay":
            continue
        source_selectors = [
            selector
            for selector in selectors
            if selector.source.id == classification.id
        ]
        if not source_selectors:
            raise ValueError(
                f"target-replay classification {classification.id} has no positive selector"
            )
        for module in classification.modules or ():
            module_prefix = f"{module}::"
            if not any(
                (selector.test_name or selector.test_prefix or "").startswith(
                    module_prefix
                )
                for selector in source_selectors
            ):
                raise ValueError(
                    f"target-replay source {classification.id} module {module} "
                    "has no positive selector"
                )
    return OwnershipManifest(tuple(classifications), tuple(selectors))


def load_manifest(path: Path) -> OwnershipManifest:
    if not path.is_file():
        raise ValueError(f"target-run ownership manifest is missing: {path}")
    return parse_manifest(json.loads(path.read_text(encoding="utf-8")))


def _inventory_tests(payload: object) -> tuple[InventoryTest, ...]:
    if not isinstance(payload, dict):
        raise ValueError("nextest list JSON must be an object")
    suites = payload.get("rust-suites")
    if not isinstance(suites, dict):
        raise ValueError("nextest list JSON is missing rust-suites object")
    tests: list[InventoryTest] = []
    for suite_id, raw_suite in suites.items():
        if not isinstance(raw_suite, dict):
            raise ValueError(f"nextest suite {suite_id!r} must be an object")
        package = raw_suite.get("package-name")
        binary = raw_suite.get("binary-name")
        testcases = raw_suite.get("testcases")
        if not isinstance(package, str) or not isinstance(binary, str):
            raise ValueError(
                f"nextest suite {suite_id!r} is missing package-name/binary-name"
            )
        if not isinstance(testcases, dict):
            raise ValueError(f"nextest suite {suite_id!r} is missing testcases")
        tests.extend(
            InventoryTest(package, binary, name)
            for name in testcases
            if isinstance(name, str)
        )
    declared_count = payload.get("test-count")
    if not isinstance(declared_count, int) or isinstance(declared_count, bool):
        raise ValueError("nextest list JSON is missing integer test-count")
    if declared_count != len(tests):
        raise ValueError(
            "nextest list test-count disagrees with suites: "
            f"declared={declared_count} observed={len(tests)}"
        )
    return tuple(sorted(tests))


def build_filter_expression(manifest: OwnershipManifest, target: str) -> str:
    """The ownership filter expression, without a nextest inventory.

    ``build_selection`` needs a real ``nextest list`` inventory only to
    validate that every selector actually matches something (a selector
    matching zero tests is stale). The expression it returns,
    ``Selection.filter_expression``, is a pure function of the selectors
    alone -- `` " | ".join(selector.filter_expression() for selector in
    selectors)`` -- and can be computed before that inventory exists.

    This is what lets a Recovery guest (soldr#3076/soldr#3078), which has no
    Linux-side nextest to list against before it boots, still be handed the
    positive filter it should replay. The inventory validation this skips is
    not dropped: it still runs after the guest reports back, against the
    ``nextest list`` JSON the guest itself produced, via ``build_selection``.
    """
    selectors = tuple(
        selector for selector in manifest.selectors if selector.applies_to(target)
    )
    if not selectors:
        raise ValueError(
            f"target-run ownership selects no selectors for target {target}; "
            "the filter would replay nothing"
        )
    return " | ".join(selector.filter_expression() for selector in selectors)


def build_selection(
    manifest: OwnershipManifest, inventory: object, target: str
) -> Selection:
    selectors = tuple(
        selector for selector in manifest.selectors if selector.applies_to(target)
    )
    tests = _inventory_tests(inventory)
    selected: set[InventoryTest] = set()
    selector_counts: list[SelectorCount] = []
    for selector in selectors:
        matches = {
            test
            for test in tests
            if selector.matches(test.package, test.binary, test.name)
        }
        if not matches:
            raise ValueError(
                f"target-run selector {selector.id!r} matched no tests for target {target}; "
                "the selector is stale or the archive lost required native coverage"
            )
        overlap = selected.intersection(matches)
        if overlap:
            example = sorted(overlap)[0].test_id()
            raise ValueError(
                f"target-run selector {selector.id!r} overlaps an earlier selector at {example}"
            )
        selected.update(matches)
        selector_counts.append(SelectorCount(selector.id, len(matches)))
    if not selected:
        raise ValueError(f"target-run ownership selects zero tests for target {target}")

    test_ids = tuple(test.test_id() for test in sorted(selected))
    expression = " | ".join(selector.filter_expression() for selector in selectors)
    return Selection(
        target=target,
        discovered_count=len(tests),
        selected_count=len(selected),
        test_ids=test_ids,
        filter_expression=expression,
        selector_counts=tuple(selector_counts),
    )


def _is_host_sensitive(source: str) -> bool:
    return HOST_CFG.search(source) is not None or any(
        marker in source for marker in HOST_MARKERS
    )


def _declared_tests(source: str) -> tuple[DeclaredTest, ...]:
    """Return directly declared test names and their attribute blocks."""

    return tuple(
        DeclaredTest(match.group("name"), match.group("attrs"))
        for match in TEST_DECLARATION.finditer(source)
        if TEST_ATTRIBUTE.search(match.group("attrs")) is not None
    )


def _split_cfg_arguments(value: str) -> tuple[str, ...]:  # noqa: C901
    arguments: list[str] = []
    start = 0
    depth = 0
    quoted = False
    escaped = False
    for index, character in enumerate(value):
        if quoted:
            if escaped:
                escaped = False
            elif character == "\\":
                escaped = True
            elif character == '"':
                quoted = False
            continue
        if character == '"':
            quoted = True
        elif character == "(":
            depth += 1
        elif character == ")":
            depth -= 1
            if depth < 0:
                raise ValueError(f"unbalanced host cfg expression: {value!r}")
        elif character == "," and depth == 0:
            arguments.append(value[start:index].strip())
            start = index + 1
    if quoted or depth != 0:
        raise ValueError(f"unbalanced host cfg expression: {value!r}")
    arguments.append(value[start:].strip())
    if any(not argument for argument in arguments):
        raise ValueError(f"empty host cfg operand: {value!r}")
    return tuple(arguments)


def _parse_host_cfg(expression: str) -> HostCfgPredicate:
    expression = expression.strip()
    if expression in {"windows", "unix"}:
        return HostCfgPredicate(expression)
    keyed = re.fullmatch(
        r'(target_os|target_arch|target_env)\s*=\s*"([A-Za-z0-9_.-]*)"',
        expression,
    )
    if keyed is not None:
        return HostCfgPredicate(keyed.group(1), keyed.group(2))
    compound = re.fullmatch(r"(all|any|not)\s*\((.*)\)", expression)
    if compound is not None:
        operation = compound.group(1)
        children = tuple(
            _parse_host_cfg(argument)
            for argument in _split_cfg_arguments(compound.group(2))
        )
        if operation == "not" and len(children) != 1:
            raise ValueError("host cfg not(...) requires exactly one operand")
        return HostCfgPredicate(operation, children=children)
    raise ValueError(
        "unsupported or ambiguous host cfg; expected windows/unix, "
        "target_os/target_arch/target_env, or all/any/not over those predicates: "
        f"{expression!r}"
    )


def _attributes_host_cfg(attributes: str) -> HostCfgPredicate | None:
    predicates: list[HostCfgPredicate] = []
    for match in ATTRIBUTE_LINE.finditer(attributes):
        body = match.group("body").strip()
        if body.startswith("cfg_attr(") and HOST_CFG_TERM.search(body) is not None:
            raise ValueError(
                "unsupported or ambiguous host cfg: cfg_attr does not gate test execution"
            )
        if not body.startswith("cfg("):
            continue
        if not body.endswith(")"):
            if HOST_CFG_TERM.search(body) is not None:
                raise ValueError(f"unsupported or ambiguous host cfg: {body!r}")
            continue
        expression = body[4:-1]
        if HOST_CFG_TERM.search(expression) is not None:
            predicates.append(_parse_host_cfg(expression))
    if HOST_CFG.search(attributes) is not None and not predicates:
        raise ValueError(
            f"unsupported or ambiguous host cfg attributes: {attributes!r}"
        )
    if not predicates:
        return None
    if len(predicates) == 1:
        return predicates[0]
    return HostCfgPredicate("all", children=tuple(predicates))


def _platform_gated_tests(
    source: str, module: str | None, *, module_attributes: str = ""
) -> tuple[GatedTest, ...]:
    """Return test IDs and the host cfg that controls their compilation."""

    module_predicate = _attributes_host_cfg(module_attributes)
    tests: list[GatedTest] = []
    for declared in _declared_tests(source):
        name = declared.name
        test_predicate = _attributes_host_cfg(declared.attributes)
        predicates = tuple(
            predicate
            for predicate in (module_predicate, test_predicate)
            if predicate is not None
        )
        if not predicates:
            continue
        predicate = (
            predicates[0]
            if len(predicates) == 1
            else HostCfgPredicate("all", children=predicates)
        )
        test_name = f"{module}::{name}" if module is not None else name
        tests.append(GatedTest(test_name, predicate))
    return tuple(tests)


def _canonical_target(triple: str) -> CanonicalTarget:
    arch = triple.split("-", 1)[0]
    if "-pc-windows-" in triple:
        return CanonicalTarget(triple, arch, "windows", triple.rsplit("-", 1)[1], False)
    if triple.endswith("-apple-darwin"):
        return CanonicalTarget(triple, arch, "macos", "", True)
    if "-unknown-linux-" in triple:
        return CanonicalTarget(triple, arch, "linux", triple.rsplit("-", 1)[1], True)
    raise ValueError(f"unsupported canonical target triple for host cfg: {triple}")


def _load_canonical_targets(repo_root: Path) -> CanonicalContract:
    contract_path = repo_root / "ci" / "canonical-targets.json"
    if not contract_path.is_file():
        # Focused source fixtures need not reproduce the repository CI contract.
        return CanonicalContract((), frozenset(), ())
    payload = json.loads(contract_path.read_text(encoding="utf-8"))
    raw_targets = payload.get("targets") if isinstance(payload, dict) else None
    if not isinstance(raw_targets, list):
        return CanonicalContract(
            (),
            frozenset(),
            (f"canonical target contract has no targets array: {contract_path}",),
        )

    canonical: dict[str, CanonicalTarget] = {}
    replayed: set[str] = set()
    failures: list[str] = []
    for raw in raw_targets:
        if not isinstance(raw, dict):
            failures.append(
                f"canonical target contract contains an invalid target: {raw!r}"
            )
            continue
        triple = raw.get("triple")
        if not isinstance(triple, str):
            failures.append(
                f"canonical target contract contains an invalid target: {raw!r}"
            )
            continue
        try:
            canonical[triple] = _canonical_target(triple)
        except ValueError as error:
            failures.append(str(error))
            continue
        ci = raw.get("ci")
        if (
            isinstance(ci, dict)
            and ci.get("kind") == "cross"
            and isinstance(ci.get("run_job"), str)
        ):
            replayed.add(triple)
    return CanonicalContract(
        tuple(canonical.values()), frozenset(replayed), tuple(failures)
    )


def _validate_selector_target_scopes(
    manifest: OwnershipManifest,
    canonical: dict[str, CanonicalTarget],
    replayed: frozenset[str],
) -> list[str]:
    """Reject target scopes that cannot reach any canonical target-run lane."""

    if not canonical:
        return []

    failures: list[str] = []
    for selector in manifest.selectors:
        if selector.targets is None:
            continue
        unknown = sorted(set(selector.targets) - set(canonical))
        if unknown:
            failures.append(
                f"target-run selector {selector.id} names non-canonical targets: {unknown}"
            )
        if set(selector.targets).isdisjoint(replayed):
            failures.append(
                f"target-run selector {selector.id} applies to no canonical replay lane"
            )
    return failures


def validate_source_ownership(manifest: OwnershipManifest, repo_root: Path) -> None:  # noqa: C901
    """Fail on unclassified host source or a stale/overlapping classification."""

    classifications = {
        key: classification
        for classification in manifest.classifications
        for key in classification.source_keys()
    }
    crates_root = repo_root / "crates"
    failures: list[str] = []
    observed_sources: set[SourceKey] = set()
    host_sensitive_sources: set[SourceKey] = set()
    category_binaries: set[CategoryBinary] = set()
    platform_gated_tests: list[PlatformGatedTest] = []

    contract = _load_canonical_targets(repo_root)
    canonical = {target.triple: target for target in contract.targets}
    replayed = contract.replayed
    failures.extend(contract.failures)
    failures.extend(_validate_selector_target_scopes(manifest, canonical, replayed))

    if crates_root.is_dir():
        for crate in sorted(path for path in crates_root.iterdir() if path.is_dir()):
            package = crate.name
            tests_root = crate / "tests"
            if not tests_root.is_dir():
                continue

            for main_path in sorted(tests_root.glob("*/main.rs")):
                binary = main_path.parent.name
                category_binaries.add(CategoryBinary(package, binary))
                main_source = main_path.read_text(encoding="utf-8")
                for declaration in MODULE_DECLARATION.finditer(main_source):
                    module = declaration.group("name")
                    if module == "common":
                        continue
                    module_path = main_path.parent / f"{module}.rs"
                    source_key = SourceKey(package, binary, module)
                    observed_sources.add(source_key)
                    if not module_path.is_file():
                        failures.append(
                            f"{main_path.relative_to(repo_root)} declares missing {module}.rs"
                        )
                        continue
                    module_source = module_path.read_text(encoding="utf-8")
                    if _is_host_sensitive(module_source):
                        host_sensitive_sources.add(source_key)
                    platform_gated_tests.extend(
                        PlatformGatedTest(source_key, gated.name, gated.predicate)
                        for gated in _platform_gated_tests(
                            module_source,
                            module,
                            module_attributes=declaration.group("attrs"),
                        )
                    )

            for source_path in sorted(tests_root.glob("*.rs")):
                binary = source_path.stem
                source_key = SourceKey(package, binary, None)
                observed_sources.add(source_key)
                source = source_path.read_text(encoding="utf-8")
                if _is_host_sensitive(source):
                    host_sensitive_sources.add(source_key)
                platform_gated_tests.extend(
                    PlatformGatedTest(source_key, gated.name, gated.predicate)
                    for gated in _platform_gated_tests(source, None)
                )

    platform_root = repo_root / "crates" / "soldr-platform"
    if platform_root.is_dir():
        platform_key = SourceKey("soldr-platform", "soldr_platform", None)
        observed_sources.add(platform_key)
        host_sensitive_sources.add(platform_key)

    for source_key in sorted(host_sensitive_sources):
        if source_key not in classifications:
            module_note = (
                f" module={source_key.module}" if source_key.module is not None else ""
            )
            failures.append(
                "unclassified host-sensitive test source: "
                f"package={source_key.package} binary={source_key.binary}{module_note}"
            )

    for gated_test in sorted(
        platform_gated_tests, key=lambda item: (item.source, item.test_name)
    ):
        package = gated_test.source.package
        binary = gated_test.source.binary
        test_name = gated_test.test_name
        predicate = gated_test.predicate
        matching_selectors = tuple(
            selector.matches(package, binary, test_name)
            for selector in manifest.selectors
        )
        selectors = tuple(
            selector
            for selector, matches in zip(
                manifest.selectors, matching_selectors, strict=True
            )
            if matches
        )
        if not selectors:
            failures.append(
                "platform-gated test lacks a positive replay selector: "
                f"package={package} binary={binary} test={test_name}"
            )
            continue
        if canonical and not any(
            predicate.matches(canonical[target])
            for selector in selectors
            for target in (selector.targets or tuple(sorted(replayed)))
            if target in replayed and target in canonical
        ):
            failures.append(
                "platform-gated test has no replay selector on a compatible target: "
                f"package={package} binary={binary} test={test_name}"
            )

    for source_key, classification in classifications.items():
        if source_key not in observed_sources:
            failures.append(
                f"stale source classification {classification.id}: no source for {source_key}"
            )
        if (
            source_key.module is None
            and CategoryBinary(source_key.package, source_key.binary)
            in category_binaries
        ):
            failures.append(
                f"source classification {classification.id} covers category binary "
                f"{source_key.package}::{source_key.binary} without explicit modules"
            )

    if failures:
        raise ValueError("\n".join(failures))


def append_summary(path: Path, selection: Selection) -> None:
    with path.open("a", encoding="utf-8", newline="\n") as stream:
        stream.write("\n### Native target-run ownership\n\n")
        stream.write(f"- Target: `{selection.target}`\n")
        stream.write(
            f"- Complete archive inventory: {selection.discovered_count} tests\n"
        )
        stream.write(
            f"- Positively selected for native replay: {selection.selected_count} tests\n"
        )
        stream.write("\n| Replay selector | Tests |\n| --- | ---: |\n")
        for entry in selection.selector_counts:
            stream.write(f"| `{entry.selector_id}` | {entry.count} |\n")


def write_filter(path: Path, expression: str) -> None:
    """Write one filter line without host-native newline translation."""

    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as stream:
        stream.write(expression)
        stream.write("\n")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--repo-root", type=Path, required=True)
    parser.add_argument("--list-json", type=Path)
    parser.add_argument("--target")
    parser.add_argument("--filter-output", type=Path)
    parser.add_argument("--github-summary", type=Path)
    parser.add_argument("--check-source-only", action="store_true")
    parser.add_argument(
        "--filter-only",
        action="store_true",
        help=(
            "write the target-run filter expression without a nextest "
            "inventory (soldr#3078: the Recovery guest needs the filter "
            "before it can produce one). The inventory validation still "
            "runs later via the normal --list-json selection mode, against "
            "the inventory that guest run produces."
        ),
    )
    args = parser.parse_args(argv)

    manifest = load_manifest(args.manifest)
    validate_source_ownership(manifest, args.repo_root)
    if args.check_source_only:
        return 0
    if args.filter_only:
        if args.target is None or args.filter_output is None:
            parser.error("--filter-only requires --target and --filter-output")
        expression = build_filter_expression(manifest, args.target)
        write_filter(args.filter_output, expression)
        print(f"target-run ownership (filter-only): target={args.target}")
        return 0
    if args.list_json is None or args.target is None or args.filter_output is None:
        parser.error("selection requires --list-json, --target, and --filter-output")
    inventory = json.loads(args.list_json.read_text(encoding="utf-8"))
    if not isinstance(inventory, dict):
        raise ValueError("nextest list JSON must be an object")
    selection = build_selection(manifest, inventory, args.target)
    write_filter(args.filter_output, selection.filter_expression)
    if args.github_summary is not None:
        append_summary(args.github_summary, selection)
    print(
        "target-run ownership: "
        f"target={selection.target} discovered={selection.discovered_count} "
        f"selected={selection.selected_count}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
