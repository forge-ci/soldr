import json
import tempfile
import unittest
from pathlib import Path

from _script_loader import load_script_module

SCRIPT = Path(__file__).with_name("collect_fixture_diagnostics.py")
collector = load_script_module(SCRIPT, "collect_fixture_diagnostics")


def write(path: Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")


class CollectFixtureDiagnosticsTests(unittest.TestCase):
    def build_tree(self, temp: Path) -> Path:
        """A miniature of what the breach fixture leaves under the temp dir."""

        cache = temp / "soldr-rss-breach-cache-b-1700000000000"
        write(
            cache / "cache" / "soldr-daemon" / "lifecycle.jsonl", '{"event":"spawn"}\n'
        )
        write(
            cache / "cache" / "soldr-daemon" / "rss-ceiling-v1.json",
            '{"breached":true}',
        )
        write(
            cache
            / "cache"
            / "soldr-daemon"
            / "memory-breach-1700000000000-42"
            / "summary.json",
            '{"role":"daemon"}',
        )
        # soldr#3053: the cgroup boundary + in-flight compile list written
        # beside summary.json must be uploaded too -- it is the half of the
        # dump that names WHICH units the daemon was holding.
        write(
            cache
            / "cache"
            / "soldr-daemon"
            / "memory-breach-1700000000000-42"
            / "cgroup.json",
            '{"cgroup_current_bytes":1,"in_flight_compiles":[]}',
        )
        write(cache / "daemon-spawn.log", "spawn failed\n")
        # Noise that must never be uploaded: a compiler cache object and a
        # binary heap profile living in the same roots.
        write(cache / "cache" / "zccache" / "objects" / "aa" / "blob.bin", "x" * 4096)
        write(
            cache
            / "cache"
            / "soldr-daemon"
            / "memory-breach-1700000000000-42"
            / "heap.pprof",
            "y" * 4096,
        )

        home = temp / "soldr-rss-breach-home-1700000000000"
        write(
            home / ".config" / "running-process" / "soldr-broker" / "broker-spawn.log",
            "broker up\n",
        )

        # A sibling temp directory that is not a fixture root at all.
        write(temp / "unrelated-dir" / "lifecycle.jsonl", "not ours\n")
        return temp

    def test_only_whitelisted_files_from_fixture_roots_are_collected(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            temp = self.build_tree(Path(raw) / "scan")
            out = Path(raw) / "out"
            index = collector.collect(
                output=out,
                roots=[temp],
                prefixes=collector.DEFAULT_PREFIXES,
            )

            copied = sorted(entry.path.replace("\\", "/") for entry in index.copied)
            self.assertEqual(
                copied,
                [
                    "soldr-rss-breach-cache-b-1700000000000/cache/soldr-daemon/"
                    "lifecycle.jsonl",
                    "soldr-rss-breach-cache-b-1700000000000/cache/soldr-daemon/"
                    "memory-breach-1700000000000-42/cgroup.json",
                    "soldr-rss-breach-cache-b-1700000000000/cache/soldr-daemon/"
                    "memory-breach-1700000000000-42/summary.json",
                    "soldr-rss-breach-cache-b-1700000000000/cache/soldr-daemon/"
                    "rss-ceiling-v1.json",
                    "soldr-rss-breach-cache-b-1700000000000/daemon-spawn.log",
                    "soldr-rss-breach-home-1700000000000/.config/running-process/"
                    "soldr-broker/broker-spawn.log",
                ],
            )
            # The cache object, the heap profile and the non-fixture directory
            # are absent, not merely unlisted.
            self.assertEqual(list(out.rglob("blob.bin")), [])
            self.assertEqual(list(out.rglob("heap.pprof")), [])
            self.assertEqual(list(out.rglob("unrelated-dir")), [])

    def test_index_is_written_even_when_nothing_matches(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            empty = Path(raw) / "empty"
            empty.mkdir()
            out = Path(raw) / "out"
            collector.collect(output=out, roots=[empty], prefixes=("soldr-rss-",))
            index = json.loads((out / "index.json").read_text(encoding="utf-8"))
            self.assertEqual(index["copied"], [])
            self.assertEqual(index["fixture_roots"], [])
            self.assertEqual(index["schema_version"], 1)

    def test_an_oversized_log_keeps_its_tail(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            temp = Path(raw) / "scan"
            root = temp / "soldr-rss-breach-cache-a-1"
            write(root / "daemon-spawn.log", "old\n" * 100 + "LAST LINE\n")
            out = Path(raw) / "out"
            index = collector.collect(
                output=out,
                roots=[temp],
                prefixes=("soldr-rss-",),
                max_file_bytes=32,
            )
            entry = index.copied[0]
            self.assertTrue(entry.truncated)
            self.assertEqual(entry.bytes, 32)
            body = (out / "soldr-rss-breach-cache-a-1" / "daemon-spawn.log").read_text(
                encoding="utf-8"
            )
            self.assertTrue(body.endswith("LAST LINE\n"), body)

    def test_the_total_cap_stops_copying_and_records_the_skip(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            temp = Path(raw) / "scan"
            root = temp / "soldr-rss-breach-cache-a-1"
            write(root / "daemon-spawn.log", "a" * 64)
            write(root / "logs" / "auto-gc.log", "b" * 64)
            out = Path(raw) / "out"
            index = collector.collect(
                output=out,
                roots=[temp],
                prefixes=("soldr-rss-",),
                max_total_bytes=64,
            )
            self.assertEqual(len(index.copied), 1)
            self.assertEqual(len(index.skipped), 1)
            self.assertEqual(index.skipped[0].reason, "total-cap")

    def test_main_returns_zero_and_writes_the_output_directory(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            temp = self.build_tree(Path(raw) / "scan")
            out = Path(raw) / "out"
            code = collector.main(
                ["--output", str(out), "--root", str(temp), "--prefix", "soldr-rss-"]
            )
            self.assertEqual(code, 0)
            self.assertTrue((out / "index.json").is_file())

    def test_a_missing_root_is_not_an_error(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            out = Path(raw) / "out"
            code = collector.main(
                ["--output", str(out), "--root", str(Path(raw) / "nope")]
            )
            self.assertEqual(code, 0)


if __name__ == "__main__":
    unittest.main()
