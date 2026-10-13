"""Tests for the cross-compilation stress matrix helper (soldr#2337).

These pin the three things the workflow cannot check for itself without a
push: the shape of the generated matrix (every host targets the other two
OSes, the full Windows arch x abi grid, correct runners), the known-gap
classification (win-gnu-from-non-Windows and gnullvm, with their tracking
issues), and the exit-code contract of the summary (known gaps do not turn the
run red; real failures do).
"""

from __future__ import annotations

import json
from dataclasses import asdict
from pathlib import Path

import pytest
from _script_loader import load_sibling_script


@pytest.fixture(scope="module")
def mod():
    return load_sibling_script("cross_stress_matrix")


# --- matrix shape ---------------------------------------------------------


def test_full_matrix_has_every_host_and_arch(mod):
    cells = mod.build_cells("both", include_known_gaps=True)
    # 3 hosts x 2 arches x (linux 8, macos 8, windows 8) = 32 cells.
    # linux/macos hosts each emit 6 targets (2 + 4 windows), windows emits 4.
    assert len(cells) == (6 + 6 + 4) * 2


def test_no_host_ever_targets_its_own_os(mod):
    cells = mod.build_cells("both", include_known_gaps=True)
    for cell in cells:
        target_os = mod.target_spec(cell.target).target_os
        assert target_os != cell.host_os, cell


def test_windows_targets_span_arch_and_abi(mod):
    cells = mod.build_cells("both", include_known_gaps=True)
    win_targets = {
        c.target
        for c in cells
        if c.host_os == "linux" and c.target.endswith(("msvc", "gnu", "gnullvm"))
    }
    assert win_targets == {
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
        "x86_64-pc-windows-gnu",
        "aarch64-pc-windows-gnullvm",
    }


def test_runner_mapping_matches_table(mod):
    cells = mod.build_cells("both", include_known_gaps=True)
    seen = {(c.host_os, c.host_arch): c.runner for c in cells}
    assert seen[("linux", "arm64")] == "ubuntu-24.04-arm"
    assert seen[("macos", "x64")] == "macos-15-intel"
    assert seen[("windows", "arm64")] == "windows-11-arm"


def test_host_arch_filter_narrows_to_one_arch(mod):
    cells = mod.build_cells("x64", include_known_gaps=True)
    assert cells, "x64 selector must still produce cells"
    assert {c.host_arch for c in cells} == {"x64"}


def test_invalid_host_arch_is_rejected(mod):
    with pytest.raises(ValueError):
        mod.build_cells("s390x", include_known_gaps=True)


def test_excluding_known_gaps_drops_gnu_and_gnullvm(mod):
    cells = mod.build_cells("both", include_known_gaps=False)
    for cell in cells:
        assert mod.known_gap(cell.host_os, cell.target) == "", cell
    # The 8 gap cells (win-gnu + gnullvm from linux/macos x both arches) go.
    assert len(cells) == 32 - 8


# --- known-gap classification --------------------------------------------


def test_win_gnu_from_linux_is_a_tracked_gap(mod):
    reason = mod.known_gap("linux", "x86_64-pc-windows-gnu")
    assert "soldr#2336" in reason and "soldr-toolchain#114" in reason


def test_win_gnu_from_windows_host_is_supported(mod):
    # The blessed path is Windows-hosted, so from Windows it is not a gap.
    assert mod.known_gap("windows", "x86_64-pc-windows-gnu") == ""


def test_gnullvm_is_a_gap_from_every_host(mod):
    for host in ("linux", "macos", "windows"):
        assert "soldr#2338" in mod.known_gap(host, "aarch64-pc-windows-gnullvm")


def test_msvc_and_unix_targets_are_never_gaps(mod):
    assert mod.known_gap("linux", "x86_64-pc-windows-msvc") == ""
    assert mod.known_gap("windows", "aarch64-apple-darwin") == ""
    assert mod.known_gap("macos", "x86_64-unknown-linux-gnu") == ""


def test_unknown_triple_raises(mod):
    with pytest.raises(KeyError):
        mod.target_spec("mips64-unknown-linux-gnuabi64")


# --- binary format detection ---------------------------------------------


@pytest.mark.parametrize(
    "magic,expected",
    [
        (b"\x7fELF\x02\x01", "elf"),
        (b"MZ\x90\x00", "pe"),
        (b"\xcf\xfa\xed\xfe", "macho"),
        (b"\xca\xfe\xba\xbe", "macho"),
        (b"nope", None),
    ],
)
def test_binary_format_reads_magic(mod, magic, expected):
    assert mod.binary_format(magic) == expected


# --- cell evaluation ------------------------------------------------------


def _write(path: Path, data: bytes) -> Path:
    path.write_bytes(data)
    return path


def test_matching_binary_passes(mod, tmp_path):
    binary = _write(tmp_path / "soldr", b"\x7fELF\x02\x01\x01\x00")
    row = mod.evaluate_cell(
        "macos", "arm64", "x86_64-unknown-linux-gnu", binary, "success"
    )
    assert row.status == "pass"
    assert row.format_ok is True


def test_wrong_format_is_a_real_failure(mod, tmp_path):
    # A Windows PE where an ELF was expected — the silent-fallthrough gap.
    binary = _write(tmp_path / "soldr", b"MZ\x90\x00")
    row = mod.evaluate_cell(
        "macos", "arm64", "x86_64-unknown-linux-gnu", binary, "success"
    )
    assert row.status == "fail"


def test_failed_build_of_a_gap_is_a_known_gap_fail(mod):
    row = mod.evaluate_cell("linux", "x64", "x86_64-pc-windows-gnu", None, "failure")
    assert row.status == "known-gap-fail"
    assert row.known_gap is True


def test_a_gap_that_builds_is_flagged_as_resolved(mod, tmp_path):
    binary = _write(tmp_path / "soldr.exe", b"MZ\x90\x00")
    row = mod.evaluate_cell("linux", "x64", "x86_64-pc-windows-gnu", binary, "success")
    assert row.status == "known-gap-pass"


# --- summary + exit code --------------------------------------------------


def test_summary_is_green_when_only_known_gaps_fail(mod):
    rows = [
        mod.evaluate_cell("linux", "x64", "x86_64-pc-windows-gnu", None, "failure"),
        mod.evaluate_cell(
            "linux", "x64", "aarch64-pc-windows-gnullvm", None, "failure"
        ),
    ]
    assert mod.render_summary(rows).exit_code == 0


def test_summary_is_red_on_a_real_failure(mod):
    rows = [
        mod.evaluate_cell("macos", "x64", "x86_64-unknown-linux-gnu", None, "failure"),
    ]
    result = mod.render_summary(rows)
    summary, code = result.markdown, result.exit_code
    assert code == 1
    assert "1 unexpected failure" in summary


def test_summary_calls_out_a_resolved_gap(mod, tmp_path):
    binary = _write(tmp_path / "soldr.exe", b"MZ\x90\x00")
    rows = [
        mod.evaluate_cell("linux", "x64", "x86_64-pc-windows-gnu", binary, "success"),
    ]
    result = mod.render_summary(rows)
    summary, code = result.markdown, result.exit_code
    assert code == 0
    assert "known gap now builds" in summary.lower() or "now passing" in summary


# --- CLI plumbing ---------------------------------------------------------


def test_matrix_cli_writes_github_output(mod, tmp_path, capsys):
    out = tmp_path / "gh_out"
    rc = mod.main(
        [
            "matrix",
            "--host-arch",
            "x64",
            "--include-known-gaps",
            "false",
            "--output",
            str(out),
        ]
    )
    assert rc == 0
    written = out.read_text(encoding="utf-8")
    assert written.startswith("matrix=")
    payload = json.loads(written[len("matrix=") :])
    assert payload.get("include")


def test_verify_binary_cli_writes_a_result_row(mod, tmp_path):
    binary = _write(tmp_path / "soldr", b"\x7fELF\x02\x01")
    out = tmp_path / "result.json"
    rc = mod.main(
        [
            "verify-binary",
            "--host-os",
            "macos",
            "--host-arch",
            "arm64",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--binary",
            str(binary),
            "--build-outcome",
            "success",
            "--output",
            str(out),
        ]
    )
    assert rc == 0
    row = json.loads(out.read_text(encoding="utf-8"))
    assert row["status"] == "pass"
    assert row["schema_version"] == mod.SCHEMA_VERSION


def test_summarize_cli_reads_result_dir(mod, tmp_path):
    (tmp_path / "a.json").write_text(
        json.dumps(
            asdict(
                mod.evaluate_cell(
                    "macos", "x64", "x86_64-unknown-linux-gnu", None, "failure"
                )
            )
        ),
        encoding="utf-8",
    )
    rc = mod.main(["summarize", "--results-dir", str(tmp_path)])
    assert rc == 1


def test_summarize_errors_on_empty_dir(mod, tmp_path):
    assert mod.main(["summarize", "--results-dir", str(tmp_path)]) == 1
