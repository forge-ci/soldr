#!/usr/bin/env python3
"""Tests for the positive native target-run ownership contract."""

from __future__ import annotations

import json
import re
import tempfile
import unittest
from dataclasses import dataclass
from pathlib import Path

import target_run_ownership as ownership


@dataclass(frozen=True)
class Suite:
    package: str
    binary: str
    tests: tuple[str, ...]

    def payload(self) -> object:
        return {
            "package-name": self.package,
            "binary-name": self.binary,
            "testcases": {name: {"ignored": False} for name in self.tests},
        }


@dataclass(frozen=True)
class Classification:
    classification_id: str
    package: str
    binary: str
    disposition: str
    modules: tuple[str, ...] | None = None

    def payload(self) -> object:
        result = {
            "id": self.classification_id,
            "package": self.package,
            "binary": self.binary,
            "disposition": self.disposition,
            "reason": f"Focused reason for {self.classification_id}.",
        }
        if self.modules is not None:
            return {**result, "modules": list(self.modules)}
        return result


@dataclass(frozen=True)
class Selector:
    selector_id: str
    source_id: str
    test_name: str | None = None
    test_prefix: str | None = None
    targets: tuple[str, ...] | None = None

    def payload(self) -> object:
        result = {
            "id": self.selector_id,
            "source_id": self.source_id,
            "reason": f"Focused reason for {self.selector_id}.",
        }
        if self.test_name is not None:
            result["test_name"] = self.test_name
        if self.test_prefix is not None:
            result["test_prefix"] = self.test_prefix
        if self.targets is not None:
            return {**result, "targets": list(self.targets)}
        return result


def inventory(*suites: Suite) -> object:
    return {
        "test-count": sum(len(suite.tests) for suite in suites),
        "rust-suites": {
            f"suite-{index}": suite.payload() for index, suite in enumerate(suites)
        },
    }


def suite(package: str, binary: str, *tests: str) -> Suite:
    return Suite(package, binary, tests)


def classification(
    classification_id: str,
    package: str,
    binary: str,
    disposition: str,
    *,
    modules: list[str] | None = None,
) -> Classification:
    return Classification(
        classification_id,
        package,
        binary,
        disposition,
        None if modules is None else tuple(modules),
    )


def selector(
    selector_id: str,
    source_id: str,
    *,
    test_name: str | None = None,
    test_prefix: str | None = None,
    targets: list[str] | None = None,
) -> Selector:
    return Selector(
        selector_id,
        source_id,
        test_name,
        test_prefix,
        None if targets is None else tuple(targets),
    )


def manifest(
    *classifications: Classification,
    selectors: list[Selector] | None = None,
) -> object:
    return {
        "schema_version": 2,
        "policy_issue": "soldr#2999",
        "source_classifications": [item.payload() for item in classifications],
        "replay_selectors": [item.payload() for item in selectors or []],
    }


class TargetRunOwnershipTests(unittest.TestCase):
    def test_source_classification_does_not_implicitly_select_a_whole_binary(
        self,
    ) -> None:
        declared = manifest(
            classification(
                "broker-native-once",
                "soldr-cli",
                "broker",
                "native-linux-once",
                modules=["portable_process_contract"],
            ),
            classification(
                "broker-target-replay",
                "soldr-cli",
                "broker",
                "target-replay",
                modules=["kill_contract"],
            ),
            selectors=[
                selector(
                    "broker-kill-smoke",
                    "broker-target-replay",
                    test_name="kill_contract::recovers_after_kill",
                )
            ],
        )
        discovered = inventory(
            suite(
                "soldr-cli",
                "broker",
                "portable_process_contract::parses_status",
                "kill_contract::recovers_after_kill",
                "kill_contract::portable_planning_helper",
            )
        )

        selected = ownership.build_selection(
            ownership.parse_manifest(declared), discovered, "x86_64-apple-darwin"
        )

        self.assertEqual(
            selected.test_ids,
            ("soldr-cli::broker::kill_contract::recovers_after_kill",),
        )
        self.assertIn(
            "test(/^kill_contract::recovers_after_kill$/)",
            selected.filter_expression,
        )

    def test_positive_module_prefix_selects_only_the_declared_module(self) -> None:
        declared = manifest(
            classification(
                "platform-native",
                "soldr-platform",
                "soldr_platform",
                "target-replay",
            ),
            selectors=[
                selector(
                    "windows-process-module",
                    "platform-native",
                    test_prefix="platform_win::process::",
                )
            ],
        )
        discovered = inventory(
            suite(
                "soldr-platform",
                "soldr_platform",
                "platform_win::process::kills_tree",
                "platform_win::process::reads_pid",
                "portable::parses_name",
            )
        )

        selected = ownership.build_selection(
            ownership.parse_manifest(declared), discovered, "x86_64-pc-windows-msvc"
        )

        self.assertEqual(selected.selected_count, 2)
        self.assertTrue(
            all("platform_win::process::" in test for test in selected.test_ids)
        )

    def test_stale_selector_is_fatal_instead_of_silently_dropping_coverage(
        self,
    ) -> None:
        declared = manifest(
            classification(
                "broker-native",
                "soldr-cli",
                "broker",
                "target-replay",
                modules=["new_name"],
            ),
            selectors=[
                selector(
                    "renamed-test",
                    "broker-native",
                    test_name="new_name::old_test",
                )
            ],
        )
        discovered = inventory(suite("soldr-cli", "broker", "new_name::works"))

        with self.assertRaisesRegex(ValueError, "matched no tests"):
            ownership.build_selection(
                ownership.parse_manifest(declared), discovered, "aarch64-apple-darwin"
            )

    def test_platform_specific_selector_applies_only_to_declared_targets(self) -> None:
        declared = manifest(
            classification(
                "pipe-native",
                "soldr-daemon",
                "daemon_windows_pipe_peer",
                "target-replay",
            ),
            classification(
                "platform-native", "soldr-platform", "soldr_platform", "target-replay"
            ),
            selectors=[
                selector(
                    "windows-pipe",
                    "pipe-native",
                    test_prefix="accepted_pipe_",
                    targets=["x86_64-pc-windows-msvc"],
                ),
                selector(
                    "platform-host",
                    "platform-native",
                    test_prefix="host::",
                ),
            ],
        )
        discovered = inventory(
            suite(
                "soldr-daemon",
                "daemon_windows_pipe_peer",
                "accepted_pipe_reports_the_os_observed_client",
            ),
            suite("soldr-platform", "soldr_platform", "host::facts::matches"),
        )

        windows = ownership.build_selection(
            ownership.parse_manifest(declared), discovered, "x86_64-pc-windows-msvc"
        )
        darwin = ownership.build_selection(
            ownership.parse_manifest(declared), discovered, "x86_64-apple-darwin"
        )

        self.assertEqual(windows.selected_count, 2)
        self.assertEqual(darwin.selected_count, 1)
        self.assertNotIn("daemon_windows_pipe_peer", darwin.filter_expression)

    def test_selector_target_scope_must_reach_a_canonical_replay_lane(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            contract = root / "ci" / "canonical-targets.json"
            contract.parent.mkdir(parents=True)
            contract.write_text(
                """{
  "targets": [
    {
      "triple": "x86_64-pc-windows-msvc",
      "ci": {"kind": "cross", "run_job": "windows-x64"}
    }
  ]
}
""",
                encoding="utf-8",
            )
            declared = manifest(
                classification(
                    "pipe-native",
                    "soldr-daemon",
                    "daemon_windows_pipe_peer",
                    "target-replay",
                ),
                selectors=[
                    selector(
                        "misspelled-windows-target",
                        "pipe-native",
                        test_prefix="accepted_pipe_",
                        targets=["x86_64-pc-windwos-msvc"],
                    )
                ],
            )

            with self.assertRaisesRegex(ValueError, "non-canonical targets"):
                ownership.validate_source_ownership(
                    ownership.parse_manifest(declared), root
                )

    def test_filter_file_is_one_lf_terminated_line(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "filter.txt"
            ownership.write_filter(path, "package(soldr-cli) & binary(broker)")

            self.assertEqual(
                path.read_bytes(), b"package(soldr-cli) & binary(broker)\n"
            )

    def test_empty_target_selection_is_fatal(self) -> None:
        declared = manifest(
            classification(
                "windows-native", "soldr-platform", "soldr_platform", "target-replay"
            ),
            selectors=[
                selector(
                    "windows-only",
                    "windows-native",
                    test_prefix="platform_win::",
                    targets=["x86_64-pc-windows-msvc"],
                )
            ],
        )
        discovered = inventory(
            suite(
                "soldr-platform",
                "soldr_platform",
                "platform_win::process::works",
            )
        )

        with self.assertRaisesRegex(ValueError, "selects zero tests"):
            ownership.build_selection(
                ownership.parse_manifest(declared),
                discovered,
                "aarch64-unknown-linux-gnu",
            )

    def test_inverse_guard_requires_explicit_classification_but_not_replay(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            category = root / "crates" / "demo" / "tests" / "native"
            category.mkdir(parents=True)
            (category / "main.rs").write_text("mod os_contract;\n", encoding="utf-8")
            (category / "os_contract.rs").write_text(
                "#[test]\nfn child_lifetime() {\n"
                '    let _ = std::process::Command::new("demo");\n'
                "}\n",
                encoding="utf-8",
            )

            with self.assertRaisesRegex(
                ValueError, "unclassified host-sensitive test source"
            ):
                ownership.validate_source_ownership(
                    ownership.parse_manifest(manifest()), root
                )

            declared = manifest(
                classification(
                    "demo-linux-once",
                    "demo",
                    "native",
                    "native-linux-once",
                    modules=["os_contract"],
                )
            )
            ownership.validate_source_ownership(
                ownership.parse_manifest(declared), root
            )

    def test_platform_gated_test_in_classified_module_requires_replay_selector(
        self,
    ) -> None:
        """A module-level owner must not hide a newly added native test.

        This is the inverse half of the positive declaration: adding a second
        platform-gated test to a source that already has one exact replay
        selector must turn the guard red until that test is positively owned.
        """

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            category = root / "crates" / "demo" / "tests" / "native"
            category.mkdir(parents=True)
            (category / "main.rs").write_text("mod os_contract;\n", encoding="utf-8")
            (category / "os_contract.rs").write_text(
                "#[cfg(windows)]\n"
                "#[test]\n"
                "fn already_owned() {}\n\n"
                "#[cfg(windows)]\n"
                "#[test]\n"
                "fn newly_unowned() {}\n",
                encoding="utf-8",
            )
            declared = manifest(
                classification(
                    "demo-target-replay",
                    "demo",
                    "native",
                    "target-replay",
                    modules=["os_contract"],
                ),
                selectors=[
                    selector(
                        "already-owned",
                        "demo-target-replay",
                        test_name="os_contract::already_owned",
                        targets=["x86_64-pc-windows-msvc"],
                    )
                ],
            )

            with self.assertRaisesRegex(
                ValueError,
                "platform-gated test lacks a positive replay selector.*newly_unowned",
            ):
                ownership.validate_source_ownership(
                    ownership.parse_manifest(declared), root
                )

    def test_platform_gated_test_selector_must_reach_compatible_target(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            category = root / "crates" / "demo" / "tests" / "native"
            category.mkdir(parents=True)
            (category / "main.rs").write_text("mod os_contract;\n", encoding="utf-8")
            (category / "os_contract.rs").write_text(
                "#[cfg(windows)]\n#[test]\nfn windows_only() {}\n",
                encoding="utf-8",
            )
            contract = root / "ci" / "canonical-targets.json"
            contract.parent.mkdir(parents=True)
            contract.write_text(
                """{
  "targets": [
    {"triple": "x86_64-pc-windows-msvc", "ci": {"kind": "cross", "run_job": "win"}},
    {"triple": "x86_64-apple-darwin", "ci": {"kind": "cross", "run_job": "mac"}}
  ]
}
""",
                encoding="utf-8",
            )
            declared = manifest(
                classification(
                    "demo-target-replay",
                    "demo",
                    "native",
                    "target-replay",
                    modules=["os_contract"],
                ),
                selectors=[
                    selector(
                        "wrong-host",
                        "demo-target-replay",
                        test_name="os_contract::windows_only",
                        targets=["x86_64-apple-darwin"],
                    )
                ],
            )

            with self.assertRaisesRegex(
                ValueError,
                "platform-gated test has no replay selector on a compatible target.*windows_only",
            ):
                ownership.validate_source_ownership(
                    ownership.parse_manifest(declared), root
                )

            compatible = manifest(
                classification(
                    "demo-target-replay",
                    "demo",
                    "native",
                    "target-replay",
                    modules=["os_contract"],
                ),
                selectors=[
                    selector(
                        "windows-host",
                        "demo-target-replay",
                        test_name="os_contract::windows_only",
                        targets=["x86_64-pc-windows-msvc"],
                    )
                ],
            )
            ownership.validate_source_ownership(
                ownership.parse_manifest(compatible), root
            )

    def test_module_level_host_cfg_is_propagated_to_declared_tests(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            category = root / "crates" / "demo" / "tests" / "native"
            category.mkdir(parents=True)
            (category / "main.rs").write_text(
                '#[cfg(target_os = "windows")]\nmod os_contract;\n',
                encoding="utf-8",
            )
            (category / "os_contract.rs").write_text(
                "#[test]\nfn module_gated() {}\n",
                encoding="utf-8",
            )
            contract = root / "ci" / "canonical-targets.json"
            contract.parent.mkdir(parents=True)
            contract.write_text(
                """{
  "targets": [
    {"triple": "aarch64-pc-windows-msvc", "ci": {"kind": "cross", "run_job": "win"}},
    {"triple": "aarch64-apple-darwin", "ci": {"kind": "cross", "run_job": "mac"}}
  ]
}
""",
                encoding="utf-8",
            )
            declared = manifest(
                classification(
                    "demo-target-replay",
                    "demo",
                    "native",
                    "target-replay",
                    modules=["os_contract"],
                ),
                selectors=[
                    selector(
                        "wrong-module-host",
                        "demo-target-replay",
                        test_name="os_contract::module_gated",
                        targets=["aarch64-apple-darwin"],
                    )
                ],
            )

            with self.assertRaisesRegex(
                ValueError,
                "platform-gated test has no replay selector on a compatible target.*module_gated",
            ):
                ownership.validate_source_ownership(
                    ownership.parse_manifest(declared), root
                )

    def test_supported_host_cfg_predicates_match_canonical_target_facts(self) -> None:
        windows = ownership._canonical_target("x86_64-pc-windows-msvc")
        mac_arm = ownership._canonical_target("aarch64-apple-darwin")
        linux_arm = ownership._canonical_target("aarch64-unknown-linux-musl")

        self.assertTrue(ownership._parse_host_cfg("windows").matches(windows))
        self.assertFalse(ownership._parse_host_cfg("unix").matches(windows))
        self.assertTrue(ownership._parse_host_cfg("unix").matches(mac_arm))
        self.assertTrue(
            ownership._parse_host_cfg('target_os = "macos"').matches(mac_arm)
        )
        self.assertTrue(
            ownership._parse_host_cfg('target_arch = "aarch64"').matches(linux_arm)
        )
        self.assertTrue(
            ownership._parse_host_cfg('target_env = "musl"').matches(linux_arm)
        )
        self.assertTrue(
            ownership._parse_host_cfg(
                'all(unix, target_os = "linux", target_arch = "aarch64", '
                'target_env = "musl")'
            ).matches(linux_arm)
        )
        self.assertTrue(ownership._parse_host_cfg("not(windows)").matches(mac_arm))

    def test_host_cfg_with_unmodeled_operand_fails_closed(self) -> None:
        with self.assertRaisesRegex(ValueError, "unsupported or ambiguous host cfg"):
            ownership._parse_host_cfg('all(windows, feature = "live-host")')

        with self.assertRaisesRegex(ValueError, "unsupported or ambiguous host cfg"):
            ownership._attributes_host_cfg(
                '#[cfg(all(windows, feature = "live-host"))]\n#[test]\n'
            )

    def test_target_replay_classification_requires_a_positive_selector(self) -> None:
        declared = manifest(
            classification(
                "orphan-replay",
                "soldr-cli",
                "broker",
                "target-replay",
                modules=["cli_kill_matrix"],
            )
        )

        with self.assertRaisesRegex(ValueError, "has no positive selector"):
            ownership.parse_manifest(declared)

    def test_every_module_in_a_target_replay_class_requires_a_selector(self) -> None:
        declared = manifest(
            classification(
                "lifecycle-native",
                "soldr-cli",
                "broker",
                "target-replay",
                modules=["cli_kill_matrix", "cli_broker_stop"],
            ),
            selectors=[
                selector(
                    "kill-only",
                    "lifecycle-native",
                    test_name="cli_kill_matrix::recovers",
                )
            ],
        )

        with self.assertRaisesRegex(
            ValueError, "module cli_broker_stop has no positive selector"
        ):
            ownership.parse_manifest(declared)

    def test_native_linux_once_source_cannot_be_selected_for_replay(self) -> None:
        declared = manifest(
            classification(
                "portable-process",
                "soldr-cli",
                "broker",
                "native-linux-once",
                modules=["cli_broker_status"],
            ),
            selectors=[
                selector(
                    "bad-selector",
                    "portable-process",
                    test_prefix="cli_broker_status::",
                )
            ],
        )

        with self.assertRaisesRegex(ValueError, "classified native-linux-once"):
            ownership.parse_manifest(declared)

    def test_selector_must_stay_inside_its_classified_modules(self) -> None:
        declared = manifest(
            classification(
                "kill-native",
                "soldr-cli",
                "broker",
                "target-replay",
                modules=["cli_kill_matrix"],
            ),
            selectors=[
                selector(
                    "outside-source",
                    "kill-native",
                    test_prefix="cli_broker_status::",
                )
            ],
        )

        with self.assertRaisesRegex(ValueError, "outside classified source"):
            ownership.parse_manifest(declared)

    def test_overlapping_source_classifications_are_rejected(self) -> None:
        declared = manifest(
            classification(
                "first", "soldr-cli", "broker", "native-linux-once", modules=["same"]
            ),
            classification(
                "second", "soldr-cli", "broker", "native-linux-once", modules=["same"]
            ),
        )

        with self.assertRaisesRegex(ValueError, "overlapping source classifications"):
            ownership.parse_manifest(declared)

    def test_overlapping_replay_selectors_are_rejected_before_inventory(self) -> None:
        declared = manifest(
            classification(
                "kill-native",
                "soldr-cli",
                "broker",
                "target-replay",
                modules=["cli_kill_matrix"],
            ),
            selectors=[
                selector(
                    "whole-module",
                    "kill-native",
                    test_prefix="cli_kill_matrix::",
                ),
                selector(
                    "one-test",
                    "kill-native",
                    test_name="cli_kill_matrix::recovers",
                ),
            ],
        )

        with self.assertRaisesRegex(ValueError, "overlapping replay selectors"):
            ownership.parse_manifest(declared)

    def test_unsafe_filter_characters_are_rejected(self) -> None:
        declared = manifest(
            classification(
                "kill-native",
                "soldr-cli",
                "broker",
                "target-replay",
                modules=["cli_kill_matrix"],
            ),
            selectors=[
                selector(
                    "unsafe",
                    "kill-native",
                    test_prefix="cli_kill_matrix::.*",
                )
            ],
        )

        with self.assertRaisesRegex(ValueError, "unsafe filter characters"):
            ownership.parse_manifest(declared)

    def test_stale_source_classification_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "crates" / "demo" / "tests").mkdir(parents=True)
            declared = manifest(
                classification("missing-source", "demo", "gone", "native-linux-once")
            )

            with self.assertRaisesRegex(ValueError, "stale source classification"):
                ownership.validate_source_ownership(
                    ownership.parse_manifest(declared), root
                )

    def test_category_classification_must_name_modules(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            category = root / "crates" / "demo" / "tests" / "native"
            category.mkdir(parents=True)
            (category / "main.rs").write_text("mod os_contract;\n", encoding="utf-8")
            (category / "os_contract.rs").write_text(
                "#[test]\nfn works() {}\n", encoding="utf-8"
            )
            declared = manifest(
                classification("whole-category", "demo", "native", "native-linux-once")
            )

            with self.assertRaisesRegex(ValueError, "without explicit modules"):
                ownership.validate_source_ownership(
                    ownership.parse_manifest(declared), root
                )

    def test_repository_manifest_covers_host_sensitive_sources(self) -> None:
        repo_root = Path(__file__).resolve().parents[2]
        declared = ownership.load_manifest(
            repo_root / "ci" / "target-run-ownership.json"
        )

        ownership.validate_source_ownership(declared, repo_root)

    def test_repository_selectors_name_tests_that_exist(self) -> None:
        # A renamed test leaves its selector matching nothing, which only the
        # full-mode target-run lanes discover (soldr#3366 renamed
        # legacy_sweep_retains_version_with_unreadable_linked_tree). Catch it
        # here, against the sources, on every run.
        repo_root = Path(__file__).resolve().parents[2]
        manifest = json.loads(
            (repo_root / "ci" / "target-run-ownership.json").read_text(encoding="utf-8")
        )
        sources = "\n".join(
            path.read_text(encoding="utf-8", errors="replace")
            for path in (repo_root / "crates").rglob("*.rs")
        )
        stale = [
            selector["id"]
            for entries in manifest.values()
            if isinstance(entries, list)
            for selector in entries
            if isinstance(selector, dict) and "test_name" in selector
            if not re.search(
                rf"\bfn\s+{re.escape(selector['test_name'].split('::')[-1])}\b",
                sources,
            )
        ]
        self.assertEqual(stale, [], "selectors naming tests that no longer exist")

    def test_filter_only_matches_the_expression_build_selection_would_emit(
        self,
    ) -> None:
        """soldr#3078: the Recovery guest needs a filter before it has an
        inventory to validate one against. `--filter-only` must produce
        byte-identical output to what the inventory-validated path would
        write for the same manifest/target."""
        declared = manifest(
            classification(
                "platform-native", "soldr-platform", "soldr_platform", "target-replay"
            ),
            selectors=[
                selector(
                    "windows-process-module",
                    "platform-native",
                    test_prefix="platform_win::process::",
                )
            ],
        )
        discovered = inventory(
            suite(
                "soldr-platform",
                "soldr_platform",
                "platform_win::process::kills_tree",
            )
        )

        via_selection = ownership.build_selection(
            ownership.parse_manifest(declared), discovered, "x86_64-pc-windows-msvc"
        ).filter_expression
        via_filter_only = ownership.build_filter_expression(
            ownership.parse_manifest(declared), "x86_64-pc-windows-msvc"
        )

        self.assertEqual(via_selection, via_filter_only)

    def test_filter_only_is_fatal_when_no_selector_applies_to_the_target(
        self,
    ) -> None:
        declared = manifest(
            classification(
                "windows-native", "soldr-platform", "soldr_platform", "target-replay"
            ),
            selectors=[
                selector(
                    "windows-only",
                    "windows-native",
                    test_prefix="platform_win::",
                    targets=["x86_64-pc-windows-msvc"],
                )
            ],
        )

        with self.assertRaisesRegex(ValueError, "selects no selectors"):
            ownership.build_filter_expression(
                ownership.parse_manifest(declared), "aarch64-unknown-linux-gnu"
            )

    def test_filter_only_against_the_repository_manifest_matches_darwin(self) -> None:
        """Exercises the exact call the Recovery guest prep step makes."""
        repo_root = Path(__file__).resolve().parents[2]
        declared = ownership.load_manifest(
            repo_root / "ci" / "target-run-ownership.json"
        )

        expression = ownership.build_filter_expression(declared, "x86_64-apple-darwin")

        self.assertTrue(expression)
        self.assertIn("package(", expression)

    def test_main_filter_only_writes_the_expression_without_an_inventory(
        self,
    ) -> None:
        """The exact CLI shape the Recovery guest prep step invokes (soldr#3078)."""
        repo_root = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as temporary:
            filter_output = Path(temporary) / "filter.txt"

            rc = ownership.main(
                [
                    "--manifest",
                    str(repo_root / "ci" / "target-run-ownership.json"),
                    "--repo-root",
                    str(repo_root),
                    "--filter-only",
                    "--target",
                    "x86_64-apple-darwin",
                    "--filter-output",
                    str(filter_output),
                ]
            )

            self.assertEqual(rc, 0)
            content = filter_output.read_text(encoding="utf-8")
            self.assertTrue(content.strip())
            self.assertIn("package(", content)


if __name__ == "__main__":
    unittest.main()
