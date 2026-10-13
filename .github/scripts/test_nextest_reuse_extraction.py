import tempfile
import unittest
from pathlib import Path

from _script_loader import load_sibling_script

nextest_reuse_extraction = load_sibling_script("nextest_reuse_extraction")


def _extraction(root: Path, *, metadata_at: str = "target/nextest") -> Path:
    """Build a plausible extracted-archive tree under ``root``."""
    extract = root / "nextest-archive"
    (extract / "target" / "debug" / "deps").mkdir(parents=True)
    metadata_dir = extract / metadata_at
    metadata_dir.mkdir(parents=True, exist_ok=True)
    (metadata_dir / "binaries-metadata.json").write_text("{}", encoding="utf-8")
    (metadata_dir / "cargo-metadata.json").write_text("{}", encoding="utf-8")
    return extract


class ReuseArgsTests(unittest.TestCase):
    def test_a_complete_extraction_yields_reuse_flags(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            extract = _extraction(Path(raw))
            args = nextest_reuse_extraction.reuse_args(extract)
        self.assertIsNotNone(args)
        self.assertIn("--binaries-metadata", args)
        self.assertIn("--cargo-metadata", args)
        self.assertIn("--target-dir-remap", args)
        # No second decompression: soldr#2933's entire point.
        self.assertNotIn("--archive-file", args)

    def test_metadata_is_found_at_the_archive_root_too(self) -> None:
        """The layout inside a nextest archive is not pinned down from
        inside this repo, so the search is a bounded walk rather than a
        hardcoded path."""
        with tempfile.TemporaryDirectory() as raw:
            extract = _extraction(Path(raw), metadata_at=".")
            self.assertIsNotNone(nextest_reuse_extraction.reuse_args(extract))

    def test_missing_metadata_yields_none(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            extract = Path(raw) / "nextest-archive"
            (extract / "target").mkdir(parents=True)
            self.assertIsNone(nextest_reuse_extraction.reuse_args(extract))

    def test_missing_target_dir_yields_none(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            extract = Path(raw) / "nextest-archive"
            extract.mkdir(parents=True)
            (extract / "binaries-metadata.json").write_text("{}", encoding="utf-8")
            (extract / "cargo-metadata.json").write_text("{}", encoding="utf-8")
            self.assertIsNone(nextest_reuse_extraction.reuse_args(extract))


class FallbackTests(unittest.TestCase):
    def test_the_fallback_still_names_the_destination(self) -> None:
        """Degraded to one re-extraction, but never back onto the implicit
        OS temp volume that soldr#2933 exists to get off."""
        args = nextest_reuse_extraction.fallback_args(
            Path("artifact/tests.tar.zst"), Path("D:/soldr-ci/nextest-archive")
        )
        self.assertEqual(
            args,
            [
                "--archive-file",
                "artifact/tests.tar.zst",
                "--extract-to",
                "D:/soldr-ci/nextest-archive",
                "--extract-overwrite",
            ],
        )


class ResolveTests(unittest.TestCase):
    def test_an_unusable_extraction_falls_back(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            extract = Path(raw) / "nextest-archive"
            resolution = nextest_reuse_extraction.resolve(
                extract, Path("artifact/tests.tar.zst")
            )
        self.assertIn("--archive-file", resolution.flags)
        self.assertIn("no reuse metadata found", resolution.reason)

    def test_reuse_can_be_forced_off_without_a_code_change(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            extract = _extraction(Path(raw))
            resolution = nextest_reuse_extraction.resolve(
                extract, Path("artifact/tests.tar.zst"), allow_reuse=False
            )
        self.assertIn("--archive-file", resolution.flags)
        self.assertIn("SOLDR_TARGET_RUN_EXTRACT_REUSE", resolution.reason)


class EnvTests(unittest.TestCase):
    def test_reuse_is_on_by_default(self) -> None:
        self.assertTrue(nextest_reuse_extraction.reuse_enabled({}))

    def test_off_values_disable_reuse(self) -> None:
        for value in ["0", "off", "OFF", "false", "no", " off "]:
            self.assertFalse(
                nextest_reuse_extraction.reuse_enabled(
                    {"SOLDR_TARGET_RUN_EXTRACT_REUSE": value}
                ),
                value,
            )

    def test_any_other_value_leaves_reuse_on(self) -> None:
        self.assertTrue(
            nextest_reuse_extraction.reuse_enabled(
                {"SOLDR_TARGET_RUN_EXTRACT_REUSE": "1"}
            )
        )


class MainTests(unittest.TestCase):
    def test_main_emits_one_argument_per_line(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            extract = _extraction(Path(raw))
            status = nextest_reuse_extraction.main(
                [
                    "--extract-dir",
                    str(extract),
                    "--archive",
                    "artifact/tests.tar.zst",
                ]
            )
        self.assertEqual(status, 0)


class NewlineTests(unittest.TestCase):
    def test_flags_are_written_with_lf_even_on_a_crlf_stream(self) -> None:
        """soldr#2933: Git Bash reads these records one argument at a time.

        Its ``while IFS= read -r`` loop strips ``\n`` but leaves ``\r``.
        A Windows text-mode writer therefore handed nextest
        ``--binaries-metadata\r`` and produced an opaque flag-parse error.
        This real TextIO stream retains default newline translation so the
        producer's LF-only contract is exercised.
        """
        import sys

        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            extract = _extraction(root)
            out = root / "flags.txt"
            original = sys.stdout
            with out.open("w", encoding="utf-8") as stream:
                sys.stdout = stream
                try:
                    status = nextest_reuse_extraction.main(
                        [
                            "--extract-dir",
                            str(extract),
                            "--archive",
                            str(root / "tests.tar.zst"),
                        ]
                    )
                finally:
                    sys.stdout = original
            self.assertEqual(status, 0)
            written = out.read_bytes()
            self.assertNotIn(b"\r", written)
            self.assertIn(b"--binaries-metadata\n", written)


if __name__ == "__main__":
    unittest.main()
