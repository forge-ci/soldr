import tempfile
import unittest
from pathlib import Path

from _script_loader import load_script_module

SCRIPT = Path(__file__).with_name("report_free_space.py")
report_free_space = load_script_module(SCRIPT, "report_free_space")


class ReportFreeSpaceTests(unittest.TestCase):
    def test_a_readable_path_renders_free_total_and_used(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            line = report_free_space.render("workspace", Path(raw))
        self.assertIn("target-run disk: workspace=", line)
        self.assertIn("free=", line)
        self.assertIn("total=", line)
        self.assertIn("GiB", line)

    def test_an_unreadable_path_is_reported_not_raised(self) -> None:
        """soldr#2734: the reading is taken while the disk may be full.

        A diagnostic that raises on the exact condition it exists to
        describe is worse than none, so an unreadable path must render a
        line rather than propagate the OSError.
        """
        missing = Path(tempfile.gettempdir()) / "soldr-2734-definitely-not-here"
        line = report_free_space.render("workspace", missing)
        self.assertIn("unreadable", line)

    def test_both_volumes_are_measured(self) -> None:
        """Workspace and temp are frequently different volumes on Windows
        runners, and soldr#2734's isolated-home tests fill the temp one."""
        names = [volume.name for volume in report_free_space.volumes(Path.cwd())]
        self.assertEqual(names, ["workspace", "temp"])

    def test_main_always_succeeds_even_for_a_bad_workspace(self) -> None:
        """The lane must never go red because the disk report struggled."""
        missing = Path(tempfile.gettempdir()) / "soldr-2734-also-not-here"
        self.assertEqual(
            report_free_space.main(["--workspace", str(missing), "--label", "after"]),
            0,
        )


if __name__ == "__main__":
    unittest.main()
