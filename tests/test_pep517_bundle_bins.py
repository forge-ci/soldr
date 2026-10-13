"""soldr#3239: `[tool.soldr.pep517] bundle-bins` stages Cargo bins into the wheel."""

from __future__ import annotations

import base64
import contextlib
import csv
import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import types
import unittest
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from unittest import mock

from conftest import load_script_module

SRC = Path(__file__).parents[1] / "src" / "soldr"
WHEEL_NAME = "demo-0.1.0-cp310-abi3-linux_x86_64.whl"
PYPROJECT = """\
[build-system]
requires = ["soldr"]
build-backend = "soldr"

[project]
name = "demo"
# a comment with # and [brackets] = "noise"

[tool.soldr.pep517]
bundle-bins = [
  { bin = "demo-cli", package = "demo" },  # installed onto PATH
  { bin = "helper", dest = 'platlib/demo/_bin' },
]

[tool.maturin]
module-name = "demo._native"
"""


def _helper() -> Any:
    return load_script_module(SRC / "_bundle_bins.py", "soldr_test_bundle_bins")


def _record_hash(data: bytes) -> str:
    digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
    return f"sha256={digest.decode()}"


def _write_wheel(path: Path, *, purelib: bool = False) -> None:
    files = {
        "demo/__init__.py": b"from ._native import *\n",
        "demo/_native.abi3.so": b"\x7fELF extension",
        "demo-0.1.0.dist-info/METADATA": b"Metadata-Version: 2.1\nName: demo\n",
        "demo-0.1.0.dist-info/WHEEL": (
            "Wheel-Version: 1.0\nRoot-Is-Purelib: "
            + ("true" if purelib else "false")
            + "\nTag: cp310-abi3-linux_x86_64\n"
        ).encode(),
    }
    with zipfile.ZipFile(path, "w") as wheel:
        rows = []
        for name, data in files.items():
            wheel.writestr(name, data)
            rows.append(f"{name},{_record_hash(data)},{len(data)}")
        rows.append("demo-0.1.0.dist-info/RECORD,,")
        wheel.writestr("demo-0.1.0.dist-info/RECORD", "\n".join(rows) + "\n")


class ConfigTest(unittest.TestCase):
    def setUp(self) -> None:
        self.helper = _helper()

    def _read(self, text: str) -> list[Any]:
        with tempfile.TemporaryDirectory() as raw:
            pyproject = Path(raw) / "pyproject.toml"
            pyproject.write_text(text, encoding="utf-8")
            return self.helper.read_bundle_bins(pyproject)

    def test_reads_entries_with_default_dest(self) -> None:
        self.assertEqual(
            self._read(PYPROJECT),
            [
                self.helper.BundleBin(bin="demo-cli", package="demo", dest="scripts"),
                self.helper.BundleBin(bin="helper", dest="platlib/demo/_bin"),
            ],
        )

    def test_python310_fallback_parser_matches_tomllib(self) -> None:
        """3.10 has no tomllib; the narrow parser must agree on the documented shape."""
        self.assertEqual(
            self.helper._fallback_entries(PYPROJECT),
            [
                {"bin": "demo-cli", "package": "demo"},
                {"bin": "helper", "dest": "platlib/demo/_bin"},
            ],
        )
        with mock.patch.dict(sys.modules, {"tomllib": None}):
            fallback = self._read(PYPROJECT)
        self.assertEqual(fallback, self._read(PYPROJECT))

    def test_fallback_parser_accepts_single_line_array(self) -> None:
        text = '[tool.soldr.pep517]\nbundle-bins = [{ bin = "x" }]\n[other]\nbundle-bins = [{ bin = "y" }]\n'
        self.assertEqual(self.helper._fallback_entries(text), [{"bin": "x"}])

    def test_absent_configuration_is_empty(self) -> None:
        self.assertEqual(
            self._read('[tool.soldr.pep517]\ndelegate-backend = "x"\n'), []
        )
        with tempfile.TemporaryDirectory() as raw:
            self.assertEqual(
                self.helper.read_bundle_bins(Path(raw) / "missing.toml"), []
            )

    def test_invalid_entries_are_refused_by_name(self) -> None:
        cases = {
            'bundle-bins = "demo"': "array of tables",
            'bundle-bins = ["demo"]': "must be a table",
            'bundle-bins = [{ package = "demo" }]': "needs `bin`",
            'bundle-bins = [{ bin = "demo", target = "x" }]': "unknown key(s) target",
            'bundle-bins = [{ bin = "demo", dest = "bin" }]': "<scheme>",
            'bundle-bins = [{ bin = "demo", dest = "scripts/../x" }]': "<scheme>",
            'bundle-bins = [{ bin = "demo", dest = "platlib//x" }]': "<scheme>",
            'bundle-bins = [{ bin = "demo", dest = "platlib\\\\x" }]': "<scheme>",
            'bundle-bins = [{ bin = "../demo" }]': "needs `bin`",
        }
        for body, message in cases.items():
            with self.subTest(body=body):
                with self.assertRaises(self.helper.BundleBinsError) as raised:
                    self._read(f"[tool.soldr.pep517]\n{body}\n")
                self.assertIn(message, str(raised.exception))


class BuildTest(unittest.TestCase):
    def setUp(self) -> None:
        self.helper = _helper()

    def test_command_uses_blessed_build_surface(self) -> None:
        entry = self.helper.BundleBin(bin="demo-cli", package="demo")
        self.assertEqual(
            self.helper.cargo_build_command(
                entry,
                manifest_path=Path("crates/demo/Cargo.toml"),
                profile_args=["--profile", "dev"],
                target_args=["--target", "aarch64-apple-darwin"],
            ),
            [
                "soldr",
                "build",
                "--bin",
                "demo-cli",
                "--message-format=json-render-diagnostics",
                "--package",
                "demo",
                "--manifest-path",
                str(Path("crates/demo/Cargo.toml")),
                "--profile",
                "dev",
                "--target",
                "aarch64-apple-darwin",
            ],
        )

    def test_executable_is_taken_from_the_matching_bin_artifact(self) -> None:
        stdout = "\n".join(
            [
                "warning: noise",
                '{"reason":"compiler-artifact","target":{"name":"demo-cli","kind":["lib"]},"executable":null}',
                '{"reason":"compiler-artifact","target":{"name":"other","kind":["bin"]},"executable":"/t/other"}',
                '{"reason":"compiler-artifact","target":{"name":"demo-cli","kind":["bin"]},"executable":"/t/demo-cli"}',
                '{"reason":"build-finished","success":true}',
            ]
        )
        self.assertEqual(
            self.helper.executable_from_messages(stdout, "demo-cli"),
            Path("/t/demo-cli"),
        )
        with self.assertRaises(self.helper.BundleBinsError):
            self.helper.executable_from_messages(stdout, "missing")

    def test_failed_build_names_the_bin(self) -> None:
        entry = self.helper.BundleBin(bin="demo-cli")

        def run(*_args: Any, **_kwargs: Any) -> Any:
            return subprocess.CompletedProcess([], 101, stdout="")

        with self.assertRaises(self.helper.BundleBinsError) as raised:
            self.helper.build_bundle_bin(entry, ["soldr", "build"], {}, run=run)
        self.assertIn("exited with 101", str(raised.exception))
        self.assertIn("demo-cli", str(raised.exception))

    def test_successful_build_returns_the_reported_file(self) -> None:
        entry = self.helper.BundleBin(bin="demo-cli")
        with tempfile.TemporaryDirectory() as raw:
            executable = Path(raw) / "demo-cli"
            executable.write_bytes(b"bin")
            message = json.dumps(
                {
                    "reason": "compiler-artifact",
                    "target": {"name": "demo-cli", "kind": ["bin"]},
                    "executable": str(executable),
                }
            )

            def run(command: Any, **kwargs: Any) -> Any:
                self.assertEqual(kwargs["env"], {"K": "V"})
                self.assertTrue(kwargs["capture_stdout"])
                return subprocess.CompletedProcess(command, 0, stdout=message)

            self.assertEqual(
                self.helper.build_bundle_bin(entry, ["soldr"], {"K": "V"}, run=run),
                executable,
            )


class WheelTest(unittest.TestCase):
    def setUp(self) -> None:
        self.helper = _helper()

    def test_arcname_follows_wheel_install_schemes(self) -> None:
        arcname = self.helper.wheel_arcname
        self.assertEqual(
            arcname("demo-0.1.0", "scripts", "x", False), "demo-0.1.0.data/scripts/x"
        )
        self.assertEqual(
            arcname("demo-0.1.0", "platlib/demo/_bin", "x", False), "demo/_bin/x"
        )
        self.assertEqual(
            arcname("demo-0.1.0", "platlib/demo", "x", True),
            "demo-0.1.0.data/platlib/demo/x",
        )
        self.assertEqual(arcname("demo-0.1.0", "purelib", "x", True), "x")
        self.assertEqual(
            arcname("demo-0.1.0", "data/share", "x", False),
            "demo-0.1.0.data/data/share/x",
        )

    def test_added_bins_are_executable_and_recorded(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            wheel = Path(raw) / WHEEL_NAME
            _write_wheel(wheel)
            cli = Path(raw) / "demo-cli"
            cli.write_bytes(b"\x7fELF cli")
            helper_bin = Path(raw) / "helper.exe"
            helper_bin.write_bytes(b"MZ helper")
            added = self.helper.add_files_to_wheel(
                wheel,
                [
                    (self.helper.BundleBin(bin="demo-cli"), cli),
                    (
                        self.helper.BundleBin(bin="helper", dest="platlib/demo/_bin"),
                        helper_bin,
                    ),
                ],
            )
            self.assertEqual(
                added, ["demo-0.1.0.data/scripts/demo-cli", "demo/_bin/helper.exe"]
            )
            with zipfile.ZipFile(wheel) as archive:
                self.assertIsNone(archive.testzip())
                names = archive.namelist()
                self.assertEqual(names[-1], "demo-0.1.0.dist-info/RECORD")
                for name in added:
                    self.assertEqual(
                        (archive.getinfo(name).external_attr >> 16) & 0o777, 0o755
                    )
                self.assertEqual(archive.read("demo/_bin/helper.exe"), b"MZ helper")
                rows = list(csv.reader(io.StringIO(archive.read(names[-1]).decode())))
                recorded = {row[0]: row for row in rows}
                self.assertEqual(set(recorded), set(names))
                self.assertEqual(recorded["demo-0.1.0.dist-info/RECORD"][1:], ["", ""])
                for name in names[:-1]:
                    data = archive.read(name)
                    self.assertEqual(
                        recorded[name][1:], [_record_hash(data), str(len(data))]
                    )
            self.assertEqual(
                sorted(p.name for p in Path(raw).iterdir()),
                sorted([WHEEL_NAME, "demo-cli", "helper.exe"]),
            )

    def test_collision_is_refused_without_touching_the_wheel(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            wheel = Path(raw) / WHEEL_NAME
            _write_wheel(wheel)
            before = wheel.read_bytes()
            clash = Path(raw) / "__init__.py"
            clash.write_bytes(b"x")
            with self.assertRaises(self.helper.BundleBinsError):
                self.helper.add_files_to_wheel(
                    wheel,
                    [(self.helper.BundleBin(bin="x", dest="platlib/demo"), clash)],
                )
            self.assertEqual(wheel.read_bytes(), before)


class SoldrWheelCliTest(unittest.TestCase):
    """zackees/soldr#3468: `soldr wheel` runs this module's `stage` CLI after
    maturin, so the native wheel verb and the PEP 517 backend bundle bins
    through one implementation."""

    def setUp(self) -> None:
        self.helper = _helper()

    def _stage(self, argv: "list[str]") -> "tuple[int, list[list[str]], Path]":
        calls: "list[list[str]]" = []
        raw = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, raw, True)
        root = Path(raw)
        (root / "pyproject.toml").write_text(PYPROJECT, encoding="utf-8")
        wheel = root / WHEEL_NAME
        _write_wheel(wheel)

        def run(command: Any, **_kwargs: Any) -> Any:
            calls.append(list(command))
            bin_name = command[command.index("--bin") + 1]
            executable = root / bin_name
            executable.write_bytes(b"built " + bin_name.encode())
            message = json.dumps(
                {
                    "reason": "compiler-artifact",
                    "target": {"name": bin_name, "kind": ["bin"]},
                    "executable": str(executable),
                }
            )
            return subprocess.CompletedProcess(command, 0, stdout=message)

        with mock.patch.object(self.helper, "run_captured", run):
            code = self.helper.main(
                [
                    "stage",
                    "--wheel",
                    str(wheel),
                    "--pyproject",
                    str(root / "pyproject.toml"),
                    *argv,
                ]
            )
        return code, calls, wheel

    def test_stage_builds_every_bin_with_the_given_soldr_target_and_profile(
        self,
    ) -> None:
        code, calls, wheel = self._stage(
            [
                "--soldr",
                "/opt/soldr",
                "--manifest-path",
                "crates/demo/Cargo.toml",
                "--target",
                "x86_64-unknown-linux-gnu",
                "--profile-arg=--release",
            ]
        )
        self.assertEqual(code, 0)
        self.assertEqual(
            calls[0],
            [
                "/opt/soldr",
                "build",
                "--bin",
                "demo-cli",
                "--message-format=json-render-diagnostics",
                "--package",
                "demo",
                "--manifest-path",
                "crates/demo/Cargo.toml",
                "--release",
                "--target",
                "x86_64-unknown-linux-gnu",
            ],
        )
        self.assertEqual(len(calls), 2)
        with zipfile.ZipFile(wheel) as archive:
            names = archive.namelist()
            self.assertIn("demo-0.1.0.data/scripts/demo-cli", names)
            self.assertIn("demo/_bin/helper", names)
            record = archive.read("demo-0.1.0.dist-info/RECORD").decode()
        self.assertIn("demo-0.1.0.data/scripts/demo-cli,sha256=", record)

    def test_stage_without_target_or_profile_builds_the_host_dev_bin(self) -> None:
        code, calls, _wheel = self._stage([])
        self.assertEqual(code, 0)
        self.assertEqual(calls[0][0], "soldr")
        self.assertNotIn("--target", calls[0])
        self.assertNotIn("--release", calls[0])
        self.assertNotIn("--manifest-path", calls[0])

    def test_stage_reports_a_failed_bin_build_and_exits_nonzero(self) -> None:
        raw = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, raw, True)
        root = Path(raw)
        (root / "pyproject.toml").write_text(PYPROJECT, encoding="utf-8")
        wheel = root / WHEEL_NAME
        _write_wheel(wheel)
        before = wheel.read_bytes()

        def run(command: Any, **_kwargs: Any) -> Any:
            return subprocess.CompletedProcess(command, 101, stdout="")

        stderr = io.StringIO()
        with mock.patch.object(self.helper, "run_captured", run):
            with contextlib.redirect_stderr(stderr):
                code = self.helper.main(
                    [
                        "stage",
                        "--wheel",
                        str(wheel),
                        "--pyproject",
                        str(root / "pyproject.toml"),
                    ]
                )
        self.assertEqual(code, 1)
        self.assertIn("demo-cli", stderr.getvalue())
        self.assertEqual(wheel.read_bytes(), before)


@dataclass(frozen=True)
class BinBuildCall:
    """One recorded `build_bundle_bin` invocation."""

    entry: Any
    command: list[str]
    rustc_wrapper: str | None
    pyo3_python: str | None


class BackendIntegrationTest(unittest.TestCase):
    """The native maturin hooks build and stage configured bins."""

    def setUp(self) -> None:
        self.backend = load_script_module(
            SRC / "__init__.py", "soldr_bundle_bins_backend"
        )
        self.helper = self.backend._sibling_module("_bundle_bins")
        self.scratch = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.scratch, ignore_errors=True)
        self.root = self.scratch / "project"
        self.root.mkdir()
        (self.root / "pyproject.toml").write_text(PYPROJECT, encoding="utf-8")
        (self.root / "Cargo.toml").write_text(
            "[package]\nname = 'demo'\n", encoding="utf-8"
        )
        self.out = self.scratch / "out"
        self.out.mkdir()

        @contextlib.contextmanager
        def no_op_build_lease(_environment: Any) -> Any:
            yield

        for patcher in (
            mock.patch.object(self.backend, "_project_root", return_value=self.root),
            mock.patch.object(self.backend, "_hold_build_lease", no_op_build_lease),
            mock.patch.dict(
                os.environ,
                {
                    "SOLDR_CACHE_DIR": str(self.scratch / "cache"),
                    "SOLDR_PEP517_STABLE_TARGET_DIR": "0",
                    "SOLDR_PEP517_WHEEL_CACHE": "off",
                    "SOLDR_PEP517_STATS": "off",
                },
            ),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)
        for name in ("SOLDR_PEP517_PROFILE", "PYO3_PYTHON"):
            env_patch = mock.patch.dict(os.environ)
            env_patch.start()
            self.addCleanup(env_patch.stop)
            os.environ.pop(name, None)

    def _build(self, hook: str, config_settings: Any = None) -> list[BinBuildCall]:
        calls: list[BinBuildCall] = []

        def maturin(subcommand: str, *args: str, **_kwargs: Any) -> None:
            self.assertEqual(subcommand, "build-wheel")
            _write_wheel(self.out / WHEEL_NAME)

        def build_bin(entry: Any, command: list[str], env: dict[str, str]) -> Path:
            calls.append(
                BinBuildCall(
                    entry, command, env.get("RUSTC_WRAPPER"), env.get("PYO3_PYTHON")
                )
            )
            path = self.scratch / entry.bin
            path.write_bytes(entry.bin.encode())
            return path

        with mock.patch.object(self.backend, "_maturin_pep517", maturin):
            with mock.patch.object(self.helper, "build_bundle_bin", build_bin):
                result = getattr(self.backend, hook)(str(self.out), config_settings)
        self.assertEqual(result, WHEEL_NAME)
        return calls

    def test_wheel_bundles_each_bin_with_maturins_profile_and_interpreter(self) -> None:
        calls = self._build("build_wheel", {"--target": "x86_64-unknown-linux-gnu"})
        self.assertEqual([call.entry.bin for call in calls], ["demo-cli", "helper"])
        command = calls[0].command
        self.assertEqual(command[:4], ["soldr", "build", "--bin", "demo-cli"])
        self.assertIn("--package", command)
        self.assertEqual(
            command[-4:], ["--profile", "dev", "--target", "x86_64-unknown-linux-gnu"]
        )
        self.assertEqual(
            calls[0].rustc_wrapper, os.environ.get("RUSTC_WRAPPER", "soldr")
        )
        self.assertIsNone(
            calls[0].pyo3_python,
            "a cross target must not inherit the host interpreter",
        )
        with zipfile.ZipFile(self.out / WHEEL_NAME) as archive:
            self.assertEqual(
                archive.read("demo-0.1.0.data/scripts/demo-cli"), b"demo-cli"
            )
            self.assertEqual(archive.read("demo/_bin/helper"), b"helper")

    def test_maturin_default_profile_is_mirrored_when_soldr_profile_is_disabled(
        self,
    ) -> None:
        os.environ["SOLDR_PEP517_PROFILE"] = "none"
        wheel_call = self._build("build_wheel")[0]
        self.assertEqual(wheel_call.command[-1], "--release")
        self.assertEqual(wheel_call.pyo3_python, sys.executable)
        editable_command = self._build("build_editable")[0].command
        self.assertNotIn("--release", editable_command)
        self.assertNotIn("--profile", editable_command)

    def test_manifest_path_follows_tool_maturin(self) -> None:
        (self.root / "pyproject.toml").write_text(
            PYPROJECT.replace(
                'module-name = "demo._native"',
                'module-name = "demo._native"\nmanifest-path = "crates/demo/Cargo.toml"',
            ),
            encoding="utf-8",
        )
        command = self._build("build_wheel", {"profile": "release"})[0].command
        manifest = command[command.index("--manifest-path") + 1]
        self.assertEqual(Path(manifest), self.root / "crates" / "demo" / "Cargo.toml")
        self.assertEqual(command[-2:], ["--profile", "release"])

    def test_build_sdist_patches_the_trimmed_workspace(self) -> None:
        # soldr#3239 / zackees/soldr#3444: `build_sdist` must patch a
        # maturin-trimmed root Cargo.toml back so `demo-cli` (no Cargo
        # dependency edge to the `demo` extension crate named by
        # `manifest-path`) survives as a workspace member.
        (self.root / "pyproject.toml").write_text(
            PYPROJECT.replace('package = "demo"', 'package = "demo-cli"'),
            encoding="utf-8",
        )
        sdist_name = "demo-0.1.0.tar.gz"

        def maturin(subcommand: str, *_args: str, **_kwargs: Any) -> None:
            self.assertEqual(subcommand, "write-sdist")
            root_cargo = '[workspace]\nmembers = [\n    "crates/demo",\n]\n'
            extension_cargo = '[package]\nname = "demo"\n'
            cli_cargo = '[package]\nname = "demo-cli"\n\n[[bin]]\nname = "demo-cli"\npath = "src/main.rs"\n'
            with tarfile.open(self.out / sdist_name, "w:gz") as archive:
                for name, text in (
                    ("demo-0.1.0/Cargo.toml", root_cargo),
                    ("demo-0.1.0/crates/demo/Cargo.toml", extension_cargo),
                    ("demo-0.1.0/crates/demo-cli/Cargo.toml", cli_cargo),
                ):
                    data = text.encode("utf-8")
                    info = tarfile.TarInfo(name)
                    info.size = len(data)
                    archive.addfile(info, io.BytesIO(data))

        with mock.patch.object(self.backend, "_maturin_pep517", maturin):
            result = self.backend.build_sdist(str(self.out))
        self.assertEqual(result, sdist_name)
        with tarfile.open(self.out / sdist_name, "r:gz") as archive:
            extracted = archive.extractfile("demo-0.1.0/Cargo.toml")
            assert extracted is not None
            text = extracted.read().decode("utf-8")
        import tomllib

        parsed = tomllib.loads(text)
        self.assertIn("crates/demo-cli", parsed["workspace"]["members"])
        self.assertIn("crates/demo", parsed["workspace"]["members"])

    def test_no_backend_function_is_shadowed_by_a_sibling_module(self) -> None:
        """Importing `soldr.<name>` rebinds the package attribute `<name>`.

        A backend function named like a sibling module is replaced by that
        module the first time the submodule loads, so a second hook call in the
        same process fails with "'module' object is not callable".
        """
        siblings = {path.stem for path in SRC.glob("*.py")} - {"__init__"}
        shadowed = sorted(
            name
            for name in siblings
            if isinstance(getattr(self.backend, name, None), types.FunctionType)
        )
        self.assertEqual(shadowed, [])

    def test_delegate_backend_and_bundle_bins_are_mutually_exclusive(self) -> None:
        delegate: Any = types.ModuleType("pep517_bundle_delegate")
        delegate.build_wheel = lambda *_args, **_kwargs: WHEEL_NAME
        with mock.patch.dict(sys.modules, {"pep517_bundle_delegate": delegate}):
            with mock.patch.object(
                self.backend,
                "_project_soldr_options",
                return_value={"delegate-backend": "pep517_bundle_delegate"},
            ):
                with self.assertRaises(RuntimeError) as raised:
                    self.backend.build_wheel(str(self.out))
        self.assertIn("bundle-bins", str(raised.exception))

    def test_projects_without_bundle_bins_are_untouched(self) -> None:
        (self.root / "pyproject.toml").write_text(
            "[project]\nname = 'demo'\n", encoding="utf-8"
        )
        self.assertEqual(self._build("build_wheel"), [])
        with zipfile.ZipFile(self.out / WHEEL_NAME) as archive:
            self.assertFalse(any(".data/" in name for name in archive.namelist()))


class SdistWorkspacePatchTest(unittest.TestCase):
    """soldr#3239 / zackees/soldr#3444: reproduce and fix the sdist-then-wheel
    failure when a `bundle-bins` package isn't a Cargo dependency of the
    extension crate, so maturin's trimmed sdist workspace drops it."""

    def setUp(self) -> None:
        tempdir = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tempdir, ignore_errors=True)
        self.sdist = tempdir / "demo-0.1.0.tar.gz"

    def _write_sdist(self, *, trimmed: bool) -> None:
        # `trimmed=True` is what maturin's sdist writer produces today for
        # the reported bug: `demo-cli` has no Cargo dependency edge to
        # `demo-py` (the `manifest-path` crate), so it is dropped from
        # `[workspace] members`. `trimmed=False` is the already-fixed shape.
        root_cargo = """\
[workspace]
resolver = "3"
members = [
    "crates/demo-py",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
"""
        if not trimmed:
            root_cargo = root_cargo.replace(
                'members = [\n    "crates/demo-py",\n]',
                'members = [\n    "crates/demo-py",\n    "crates/demo-cli",\n]',
            )
        extension_cargo = """\
[package]
name = "demo-py"
version = "0.1.0"
edition = "2021"
"""
        cli_cargo = """\
[package]
name = "demo-cli"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "demo-cli"
path = "src/main.rs"
"""
        with tarfile.open(self.sdist, "w:gz") as archive:
            for name, text in (
                ("demo-0.1.0/Cargo.toml", root_cargo),
                ("demo-0.1.0/crates/demo-py/Cargo.toml", extension_cargo),
                ("demo-0.1.0/crates/demo-cli/Cargo.toml", cli_cargo),
            ):
                data = text.encode("utf-8")
                info = tarfile.TarInfo(name)
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))

    def _entries(self) -> Any:
        bundle_bin = _helper().BundleBin
        return [bundle_bin(bin="demo-cli", package="demo-cli")]

    def _root_cargo_toml(self) -> str:
        with tarfile.open(self.sdist, "r:gz") as archive:
            extracted = archive.extractfile("demo-0.1.0/Cargo.toml")
            assert extracted is not None
            return extracted.read().decode("utf-8")

    def test_dropped_package_reproduces_and_is_not_a_workspace_member(self) -> None:
        # RED: this is exactly what maturin's sdist writer produces today --
        # `demo-cli` has no Cargo dependency edge to `demo-py` (the
        # `manifest-path` crate), so it is trimmed from `[workspace] members`
        # even though its files were copied into the sdist. A later
        # `soldr build --package demo-cli --manifest-path
        # <sdist>/crates/demo-py/Cargo.toml` would fail with "package ID
        # specification `demo-cli` did not match any packages", matching the
        # reproduction in zackees/soldr#3444.
        self._write_sdist(trimmed=True)
        import tomllib

        parsed = tomllib.loads(self._root_cargo_toml())
        self.assertNotIn("crates/demo-cli", parsed["workspace"]["members"])

    def test_patch_adds_the_missing_bundle_bins_package(self) -> None:
        self._write_sdist(trimmed=True)
        helper = _helper()
        added = helper.patch_sdist_workspace_members(self.sdist, self._entries())
        self.assertEqual(added, ["crates/demo-cli"])
        import tomllib

        parsed = tomllib.loads(self._root_cargo_toml())
        self.assertIn("crates/demo-cli", parsed["workspace"]["members"])
        self.assertIn("crates/demo-py", parsed["workspace"]["members"])
        # The extension crate's own Cargo.toml is untouched.
        with tarfile.open(self.sdist, "r:gz") as archive:
            extension = archive.extractfile("demo-0.1.0/crates/demo-py/Cargo.toml")
            assert extension is not None
            self.assertIn('name = "demo-py"', extension.read().decode("utf-8"))

    def test_already_present_member_is_left_alone(self) -> None:
        self._write_sdist(trimmed=False)
        helper = _helper()
        added = helper.patch_sdist_workspace_members(self.sdist, self._entries())
        self.assertEqual(added, [])

    def test_no_bundle_bins_entries_is_a_noop(self) -> None:
        self._write_sdist(trimmed=True)
        helper = _helper()
        added = helper.patch_sdist_workspace_members(self.sdist, [])
        self.assertEqual(added, [])
        import tomllib

        parsed = tomllib.loads(self._root_cargo_toml())
        self.assertNotIn("crates/demo-cli", parsed["workspace"]["members"])


if __name__ == "__main__":
    unittest.main()
