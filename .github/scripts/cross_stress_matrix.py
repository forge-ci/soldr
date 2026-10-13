#!/usr/bin/env python3
"""Drive the cross-compilation stress matrix (soldr#2337).

`cross-compile-stress.yml` is a `workflow_dispatch`-only sweep whose whole
point is to *surface gaps*: every host OS cross-builds the soldr binary for
the other two OSes, on both x64 and arm host runners, across the full Windows
`{arch} x {abi}` grid. Per CLAUDE.md, the non-trivial logic — the cell ->
runner/target mapping, the known-gap classification, and the pass/fail grid —
lives here so it is unit-testable without pushing a branch.

Three subcommands, one per phase of the workflow:

    matrix         Expand the dispatch inputs into a `fromJSON` build matrix.
    verify-binary  Classify one built (or missing) binary into a result row.
    summarize      Fold the per-cell result rows into a grid + an exit code.

The exit-code contract matters: a *known* gap that fails does NOT turn the run
red (it is expected until the tracked issue lands), but any other failing cell
does. That is what makes this a regression gate rather than a wall of red.

Usage:

    python3 .github/scripts/cross_stress_matrix.py matrix \
        --host-arch both --include-known-gaps true --output "$GITHUB_OUTPUT"

    python3 .github/scripts/cross_stress_matrix.py verify-binary \
        --host-os linux --host-arch x64 \
        --target x86_64-apple-darwin \
        --binary target/x86_64-apple-darwin/debug/soldr \
        --build-outcome success --output result.json

    python3 .github/scripts/cross_stress_matrix.py summarize \
        --results-dir results --output "$GITHUB_STEP_SUMMARY"
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import asdict, dataclass
from pathlib import Path

SCHEMA_VERSION = 1

# The three OSes in the sweep and, for each, the two OTHER OSes it must
# cross-build for. A host never targets itself — that is a native build, not a
# cross one, and it is covered by the ordinary CI lanes.
HOST_OSES: tuple[str, ...] = ("linux", "macos", "windows")
CROSS_TARGET_OSES: dict[str, tuple[str, ...]] = {
    "linux": ("macos", "windows"),
    "macos": ("linux", "windows"),
    "windows": ("linux", "macos"),
}

HOST_ARCHES: tuple[str, ...] = ("x64", "arm64")


@dataclass(frozen=True)
class HostKey:
    """A host machine class: its OS and CPU architecture."""

    os: str
    arch: str


# HostKey -> GitHub-hosted runner label. Kept in step with the
# runners `ci/canonical-targets.json` already names for the same machines.
RUNNERS: dict[HostKey, str] = {
    HostKey("linux", "x64"): "ubuntu-24.04",
    HostKey("linux", "arm64"): "ubuntu-24.04-arm",
    HostKey("macos", "x64"): "macos-15-intel",
    HostKey("macos", "arm64"): "macos-15",
    HostKey("windows", "x64"): "windows-2025",
    HostKey("windows", "arm64"): "windows-11-arm",
}


@dataclass(frozen=True)
class TargetSpec:
    """One sweep target: triple, alias, arch, abi, exe suffix, binary format."""

    target_os: str
    triple: str
    alias: str
    arch: str
    abi: str
    exe_suffix: str
    expected_format: str


@dataclass(frozen=True)
class MatrixCell:
    """One build-matrix `include` entry (string values only, per GitHub)."""

    host_os: str
    host_arch: str
    runner: str
    target: str
    alias: str
    exe_suffix: str
    name: str


@dataclass(frozen=True)
class CellResult:
    """One cell's verdict row, serialized as the per-cell result JSON."""

    schema_version: int
    host_os: str
    host_arch: str
    target: str
    target_os: str
    target_arch: str
    abi: str
    alias: str
    known_gap: bool
    gap_reason: str
    build_outcome: str
    expected_format: str
    detected_format: str
    format_ok: bool
    status: str


@dataclass(frozen=True)
class Summary:
    """Rendered Markdown grid plus the process exit code it implies."""

    markdown: str
    exit_code: int


# target_os -> TargetSpec rows.
#
# The Windows row is deliberately the full `{x64, arm} x {msvc, gnu}` grid. The
# arm GNU-family triple is `-gnullvm`: there is no plain `aarch64-pc-windows-gnu`.
TARGETS: dict[str, tuple[TargetSpec, ...]] = {
    "windows": (
        TargetSpec(
            "windows", "x86_64-pc-windows-msvc", "win-x64", "x64", "msvc", ".exe", "pe"
        ),
        TargetSpec(
            "windows",
            "aarch64-pc-windows-msvc",
            "win-arm64",
            "arm64",
            "msvc",
            ".exe",
            "pe",
        ),
        TargetSpec(
            "windows",
            "x86_64-pc-windows-gnu",
            "win-x64-gnu",
            "x64",
            "gnu",
            ".exe",
            "pe",
        ),
        TargetSpec(
            "windows",
            "aarch64-pc-windows-gnullvm",
            "win-arm64-gnullvm",
            "arm64",
            "gnullvm",
            ".exe",
            "pe",
        ),
    ),
    "macos": (
        TargetSpec("macos", "x86_64-apple-darwin", "mac-x64", "x64", "", "", "macho"),
        TargetSpec(
            "macos", "aarch64-apple-darwin", "mac-arm64", "arm64", "", "", "macho"
        ),
    ),
    "linux": (
        TargetSpec(
            "linux", "x86_64-unknown-linux-gnu", "linux-x64", "x64", "gnu", "", "elf"
        ),
        TargetSpec(
            "linux",
            "aarch64-unknown-linux-gnu",
            "linux-arm64",
            "arm64",
            "gnu",
            "",
            "elf",
        ),
    ),
}

# First-four-byte signatures, lower-case hex. PE is the 2-byte "MZ" stub; the
# Mach-O set covers thin 64-bit (both endiannesses) and the fat/universal magic.
MAGIC: dict[str, tuple[str, ...]] = {
    "elf": ("7f454c46",),
    "pe": ("4d5a",),
    "macho": ("cffaedfe", "feedfacf", "cafebabe", "cefaedfe", "feedface"),
}

# The two gaps this matrix exists to make visible. Both are the current,
# deliberate limits of the blessed win-gnu surface, each with a tracking issue.
GNU_FROM_NON_WINDOWS_GAP = (
    "x86_64-pc-windows-gnu cross-compiles only from a Windows x64 host today "
    "(blessed_build.rs:209-214); there is no host-neutral mingw-w64 sysroot "
    "asset. Tracked by soldr#2336 and zackees/soldr-toolchain#114."
)
GNULLVM_GAP = (
    "aarch64-pc-windows-gnullvm (the arm Windows GNU-family target) is rejected "
    "as a follow-up today (prepare_cmd.rs:417-422). Tracked by soldr#2338."
)


def target_spec(target: str) -> TargetSpec:
    """Return the `TargetSpec` for the *target* triple.

    Raises `KeyError` naming the triple when it is not one of the sweep's
    targets, so a typo in a workflow argument fails loudly rather than
    classifying an unknown binary as a silent pass.
    """
    for specs in TARGETS.values():
        for spec in specs:
            if spec.triple == target:
                return spec
    raise KeyError(f"unknown target triple: {target}")


def known_gap(host_os: str, target: str) -> str:
    """The reason *target* is a known gap from *host_os*, or "" when supported.

    win-gnu is only a gap from a non-Windows host — the blessed Windows-hosted
    path works — but `gnullvm` is unsupported from every host.
    """
    spec = target_spec(target)
    if spec.abi == "gnullvm":
        return GNULLVM_GAP
    if spec.abi == "gnu" and spec.target_os == "windows" and host_os != "windows":
        return GNU_FROM_NON_WINDOWS_GAP
    return ""


def _host_arches(host_arch: str) -> tuple[str, ...]:
    """Resolve the `--host-arch` selector into the host arches to sweep."""
    if host_arch == "both":
        return HOST_ARCHES
    if host_arch in HOST_ARCHES:
        return (host_arch,)
    raise ValueError(
        f"invalid --host-arch {host_arch!r}; expected one of: "
        f"both, {', '.join(HOST_ARCHES)}"
    )


def build_cells(host_arch: str, include_known_gaps: bool) -> list[MatrixCell]:
    """Expand the dispatch inputs into the list of matrix cells.

    Each cell is a flat string-only record because a GitHub Actions matrix
    `include` entry carries string values only. `verify-binary` re-derives the
    format/gap facts from `(host_os, target)` rather than trusting fields
    threaded through the matrix, so the tables here stay the single source.
    """
    cells: list[MatrixCell] = []
    for host_os in HOST_OSES:
        for arch in _host_arches(host_arch):
            runner = RUNNERS[HostKey(host_os, arch)]
            for target_os in CROSS_TARGET_OSES[host_os]:
                for spec in TARGETS[target_os]:
                    if known_gap(host_os, spec.triple) and not include_known_gaps:
                        continue
                    cells.append(
                        MatrixCell(
                            host_os=host_os,
                            host_arch=arch,
                            runner=runner,
                            target=spec.triple,
                            alias=spec.alias,
                            exe_suffix=spec.exe_suffix,
                            name=f"{host_os}-{arch} -> {spec.alias}",
                        )
                    )
    return cells


def binary_format(head: bytes) -> str | None:
    """Identify an executable format from its leading magic bytes, or None."""
    signature = head[:4].hex()
    for fmt, magics in MAGIC.items():
        if any(signature.startswith(magic) for magic in magics):
            return fmt
    return None


def evaluate_cell(
    host_os: str,
    host_arch: str,
    target: str,
    binary: Path | None,
    build_outcome: str,
) -> CellResult:
    """Classify one cell's outcome into a result row.

    `build_outcome` is the GitHub `steps.<id>.outcome` of the build step
    ("success" / "failure"). A cell passes only when the build succeeded AND
    the produced binary carries the target's expected magic bytes — a silent
    host-target fallthrough is exactly the gap this sweep hunts, and it shows
    up as the wrong format rather than a missing file.
    """
    spec = target_spec(target)
    expected = spec.expected_format
    gap = known_gap(host_os, target)

    detected: str | None = None
    if binary is not None and binary.is_file():
        detected = binary_format(binary.read_bytes()[:4])
    format_ok = detected == expected
    ok = build_outcome == "success" and format_ok

    if gap:
        status = "known-gap-pass" if ok else "known-gap-fail"
    else:
        status = "pass" if ok else "fail"

    return CellResult(
        schema_version=SCHEMA_VERSION,
        host_os=host_os,
        host_arch=host_arch,
        target=target,
        target_os=spec.target_os,
        target_arch=spec.arch,
        abi=spec.abi,
        alias=spec.alias,
        known_gap=bool(gap),
        gap_reason=gap,
        build_outcome=build_outcome,
        expected_format=expected,
        detected_format=detected or "",
        format_ok=format_ok,
        status=status,
    )


_STATUS_ICON = {
    "pass": "✅",
    "fail": "❌",
    "known-gap-fail": "⚠️",
    "known-gap-pass": "🎉",
}


def render_summary(results: list[CellResult]) -> Summary:
    """Render the result rows as a Markdown grid and compute the exit code.

    Exit code is 1 when any non-gap cell failed (a real regression) and 0
    otherwise. A `known-gap-fail` is expected, so it never fails the run; a
    `known-gap-pass` is a happy surprise worth a callout because the tracked
    gap may have closed and the classification here should be revisited.
    """
    rows = sorted(
        results,
        key=lambda r: (r.host_os, r.host_arch, r.target),
    )
    lines = [
        "## Cross-compilation stress matrix",
        "",
        "| Host | Target | Status | Detail |",
        "| --- | --- | --- | --- |",
    ]
    real_failures = 0
    resolved_gaps = 0
    for row in rows:
        status = row.status
        if status == "fail":
            real_failures += 1
        if status == "known-gap-pass":
            resolved_gaps += 1
        icon = _STATUS_ICON.get(status, "?")
        detail = row.gap_reason if row.known_gap else _detail(row)
        host = f"{row.host_os}-{row.host_arch}"
        lines.append(f"| {host} | {row.target} | {icon} {status} | {detail} |")

    lines += [
        "",
        f"**{len(rows)} cells** — {real_failures} unexpected failure(s), "
        f"{resolved_gaps} known gap(s) now passing.",
    ]
    if resolved_gaps:
        lines.append(
            "> A known gap now builds. Update the classification in "
            "`.github/scripts/cross_stress_matrix.py` and close the tracking issue."
        )
    return Summary("\n".join(lines) + "\n", 1 if real_failures else 0)


def _detail(row: CellResult) -> str:
    """One-line explanation of a non-gap row's outcome for the grid."""
    if row.status == "pass":
        return f"built {row.detected_format}"
    if row.build_outcome != "success":
        return "build step failed"
    return (
        f"format mismatch: expected {row.expected_format}, "
        f"got {row.detected_format or 'no binary'}"
    )


def load_results(results_dir: Path) -> list[CellResult]:
    """Load every `*.json` result row beneath *results_dir*."""
    rows: list[CellResult] = []
    for path in sorted(results_dir.rglob("*.json")):
        payload = json.loads(path.read_text(encoding="utf-8"))
        if isinstance(payload, dict):
            rows.append(
                CellResult(
                    schema_version=payload["schema_version"],
                    host_os=payload["host_os"],
                    host_arch=payload["host_arch"],
                    target=payload["target"],
                    target_os=payload["target_os"],
                    target_arch=payload["target_arch"],
                    abi=payload["abi"],
                    alias=payload["alias"],
                    known_gap=payload["known_gap"],
                    gap_reason=payload["gap_reason"],
                    build_outcome=payload["build_outcome"],
                    expected_format=payload["expected_format"],
                    detected_format=payload["detected_format"],
                    format_ok=payload["format_ok"],
                    status=payload["status"],
                )
            )
    return rows


def _parse_bool(value: str) -> bool:
    """Parse a GitHub Actions boolean input ("true"/"false") strictly."""
    lowered = value.strip().lower()
    if lowered in ("true", "1", "yes"):
        return True
    if lowered in ("false", "0", "no", ""):
        return False
    raise ValueError(f"expected a boolean, got {value!r}")


def _append_output(output: str | None, payload: str) -> None:
    """Append `matrix=<json>` to the `$GITHUB_OUTPUT` file when given one."""
    if output:
        with open(output, "a", encoding="utf-8") as handle:
            handle.write(payload + "\n")


def cmd_matrix(args: argparse.Namespace) -> int:
    """`matrix` subcommand: emit the `fromJSON` build matrix."""
    try:
        cells = build_cells(args.host_arch, _parse_bool(args.include_known_gaps))
    except ValueError as exc:
        print(f"cross_stress_matrix: {exc}", file=sys.stderr)
        return 1
    payload = json.dumps(
        {"include": [asdict(cell) for cell in cells]}, separators=(",", ":")
    )
    print(f"matrix={payload}")
    _append_output(args.output, f"matrix={payload}")
    return 0


def cmd_verify_binary(args: argparse.Namespace) -> int:
    """`verify-binary` subcommand: record one cell's result row.

    Always returns 0 — this is a recorder, not a gate. The verdict lives in the
    written row's `status`; `summarize` is what folds those into an exit code.
    """
    try:
        row = evaluate_cell(
            args.host_os,
            args.host_arch,
            args.target,
            Path(args.binary) if args.binary else None,
            args.build_outcome,
        )
    except KeyError as exc:
        print(f"cross_stress_matrix: {exc}", file=sys.stderr)
        return 2
    payload = json.dumps(asdict(row), separators=(",", ":"))
    print(f"{row.host_os}-{row.host_arch} {row.target}: {row.status}")
    if args.output:
        Path(args.output).write_text(payload + "\n", encoding="utf-8")
    return 0


def cmd_summarize(args: argparse.Namespace) -> int:
    """`summarize` subcommand: render the grid and set the run's exit code."""
    results = load_results(Path(args.results_dir))
    if not results:
        print("cross_stress_matrix: no result rows found", file=sys.stderr)
        return 1
    summary = render_summary(results)
    print(summary.markdown)
    if args.output:
        with open(args.output, "a", encoding="utf-8") as handle:
            handle.write(summary.markdown)
    return summary.exit_code


def build_parser() -> argparse.ArgumentParser:
    """Construct the argument parser with its three subcommands."""
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    matrix = sub.add_parser("matrix", help="emit the fromJSON build matrix")
    matrix.add_argument("--host-arch", default="both")
    matrix.add_argument("--include-known-gaps", default="true")
    matrix.add_argument("--output", default=None)
    matrix.set_defaults(func=cmd_matrix)

    verify = sub.add_parser("verify-binary", help="record one cell's result")
    verify.add_argument("--host-os", required=True)
    verify.add_argument("--host-arch", required=True)
    verify.add_argument("--target", required=True)
    verify.add_argument("--binary", default=None)
    verify.add_argument("--build-outcome", required=True)
    verify.add_argument("--output", default=None)
    verify.set_defaults(func=cmd_verify_binary)

    summarize = sub.add_parser("summarize", help="render grid + set exit code")
    summarize.add_argument("--results-dir", required=True)
    summarize.add_argument("--output", default=None)
    summarize.set_defaults(func=cmd_summarize)
    return parser


def main(argv: list[str] | None = None) -> int:
    """CLI entry point; dispatches to the selected subcommand."""
    args = build_parser().parse_args(argv)
    func: object = args.func
    assert callable(func)
    return int(func(args))


if __name__ == "__main__":
    raise SystemExit(main())
