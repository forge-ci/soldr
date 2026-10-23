import os
import unittest
from pathlib import Path
from unittest import mock

from _script_loader import load_script_module

SCRIPT = Path(__file__).with_name("audit_policy.py")
policy = load_script_module(SCRIPT, "audit_policy")


class AuditPolicyTests(unittest.TestCase):
    """soldr#3785: advisories block only where the change owns its deps."""

    def run_main(self, event, audit_code, changed):
        env = {k: v for k, v in os.environ.items() if k != "GITHUB_EVENT_NAME"}
        if event is not None:
            env["GITHUB_EVENT_NAME"] = event
        diffs = []

        def diff(base):
            diffs.append(base)
            return changed

        with mock.patch.dict(os.environ, env, clear=True):
            code = policy.main(
                ["--base-ref", "origin/main"], audit=lambda: audit_code, diff=diff
            )
        return code, diffs

    def test_a_clean_audit_always_passes(self):
        self.assertEqual(self.run_main("push", 0, None)[0], 0)
        self.assertEqual(self.run_main("pull_request", 0, None)[0], 0)

    def test_a_finding_blocks_main_and_release_events_without_diffing(self):
        for event in ("push", "workflow_dispatch", "schedule"):
            code, diffs = self.run_main(event, 1, ["README.md"])
            self.assertEqual(code, 1, event)
            self.assertEqual(diffs, [], event)

    def test_a_finding_blocks_a_pr_that_changes_dependencies(self):
        for path in ("Cargo.lock", "crates/soldr-cli/Cargo.toml"):
            self.assertEqual(self.run_main("pull_request", 1, [path])[0], 1, path)

    def test_a_finding_reports_without_blocking_an_unrelated_pr(self):
        code, diffs = self.run_main("pull_request", 1, ["ci/local_gate.py"])
        self.assertEqual(code, 0)
        self.assertEqual(diffs, ["origin/main"])
        # A local run (no event) follows the PR rule.
        self.assertEqual(self.run_main(None, 1, ["src/lib.rs"])[0], 0)

    def test_an_unknown_diff_fails_closed(self):
        self.assertEqual(self.run_main("pull_request", 1, None)[0], 1)

    def test_manifest_detection_matches_the_file_name_only(self):
        self.assertTrue(policy.is_dependency_manifest("Cargo.lock"))
        self.assertTrue(policy.is_dependency_manifest("a\\b\\Cargo.toml"))
        self.assertFalse(policy.is_dependency_manifest("docs/Cargo.toml.md"))
        self.assertFalse(policy.is_dependency_manifest("rust-toolchain.toml"))


if __name__ == "__main__":
    unittest.main()
