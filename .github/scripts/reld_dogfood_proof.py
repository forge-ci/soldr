#!/usr/bin/env python3
"""CI proof for soldr#3276 §6: dogfooding `linker = "reld"`.

Builds `soldr-cli` through the locally built `soldr` binary with verbose
rustc invocations captured, then asserts two things about the *same*
rustc invocation that links the final `soldr` binary:

1. The linker soldr injected resolves to an existing, absolute-path
   executable derived from a managed/fetched `reld` -- never the bare
   literal `reld` name that clang's `--ld-path=` rejects
   (`clang: error: invalid linker name in argument '--ld-path=reld'`,
   the soldr#3277-shaped symptom this proof exists to catch).
2. `--cfg tokio_unstable` (from this repo's own `.cargo/config.toml`
   `[build] rustflags`) is still present on that same invocation, proving
   the reld linker injection did not silently replace the project's
   `[build] rustflags` the way a naive `CARGO_TARGET_*_RUSTFLAGS`
   injection would (soldr#3277).

Run from the repository root with the workspace already declaring
`linker = "reld"` in `[workspace.metadata.soldr]`:

    uv run --no-project --python 3.13 python .github/scripts/reld_dogfood_proof.py \
        --soldr-bin target/debug/soldr

Exits non-zero with a descriptive message on any failure. Intentionally a
narrow, single-purpose script per soldr repo convention: complex CI logic
belongs in `ci/*.py` / `.github/scripts/*.py`, not inline workflow YAML.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys
import time
from pathlib import Path

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

# A `-C linker=` value soldr injects is either the direct resolved `reld`
# executable (Windows/macOS, absolute) or a generated content-addressed clang
# driver shim (Linux). The shim is passed by its file name -- `clang-<digest>`
# / `cc-<digest>` -- with its directory on the cargo child's search path, so
# no cache-root path enters the compile's cache key (soldr#3766). It must
# never be the bare `reld` name clang rejects.
BARE_RELD_NAMES = {"reld", "reld.exe"}
SHIM_NAME = re.compile(r"^(clang|cc)-[0-9a-f]{20}$")


def soldr_root(soldr_bin: pathlib.Path, repo_root: pathlib.Path) -> pathlib.Path | None:
    """The state root the build's own soldr selected (`soldr logs paths`)."""
    result = run_captured(
        [str(soldr_bin), "logs", "paths", "--json"],
        cwd=repo_root,
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
    )
    try:
        root = json.loads(result.stdout).get("root")
    except json.JSONDecodeError:
        return None
    return pathlib.Path(root) if root else None


def locate_shim(name: str, root: pathlib.Path | None) -> pathlib.Path | None:
    """Find a generated shim by name under the soldr root (soldr#3766)."""
    if root is None:
        return None
    for candidate in root.glob(f"**/linker-shims/v1/{name}"):
        if candidate.is_file():
            return candidate
    return None


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--soldr-bin",
        default="target/debug/soldr",
        help="Path to the soldr binary to drive the build with.",
    )
    parser.add_argument(
        "--package",
        default="soldr-cli",
        help="Cargo package to build (must produce a linked binary).",
    )
    parser.add_argument(
        "--repo-root",
        default=".",
        help="Repository root (defaults to the current directory).",
    )
    return parser.parse_args()


def force_rebuild(repo_root: pathlib.Path) -> None:
    """Change the CLI binary's entry point so the link step always reruns.

    A bare `touch()` is not reliable here: soldr's own compile cache keys on
    content (blake3), and in CI this script typically runs immediately after
    a step that already built the same package, so cargo's own fingerprint
    may also see nothing new to do. Appending a harmless, uniquely-timestamped
    comment changes the file's content hash, guaranteeing both cargo and
    soldr's cache see a real change and actually re-invoke rustc for the
    final link -- without it, an already-cached build produces zero rustc
    invocations under `-v` and the proof would vacuously "pass" by finding
    nothing to check.
    """
    entry = repo_root / "crates" / "soldr-cli" / "src" / "main.rs"
    marker = f"\n// reld_dogfood_proof cache-bust: {time.time_ns()}\n"
    with entry.open("a", encoding="utf-8") as handle:
        handle.write(marker)


def run_verbose_build(
    soldr_bin: pathlib.Path, package: str, repo_root: pathlib.Path
) -> str:
    result = run_captured(
        [str(soldr_bin), "cargo", "build", "-p", package, "-v"],
        cwd=repo_root,
        capture_output=True,
        text=True,
        timeout=900,
        check=False,
    )
    combined = result.stdout + result.stderr
    if result.returncode != 0:
        print(combined, file=sys.stderr)
        raise SystemExit(
            f"reld_dogfood_proof: `soldr cargo build -p {package} -v` failed "
            f"(exit {result.returncode}) -- this is the exact dogfooding build "
            "soldr#3276 §6 requires to succeed."
        )
    return combined


ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")


def strip_ansi(text: str) -> str:
    r"""Remove ANSI escape/color codes.

    `CARGO_TERM_COLOR=always` is set in CI, so cargo bolds/colors the word
    "Running" with escape sequences inserted *between* "Running" and the
    backtick that starts the command -- e.g. `Running\x1b[0m \`rustc ...\``.
    A literal substring check for "Running `" then never matches even though
    the invocation is right there, so strip escapes before scanning.
    """
    return ANSI_ESCAPE_RE.sub("", text)


def final_link_invocations(build_log: str, package_bin_crate: str) -> list[str]:
    """Every rustc invocation line that links a `bin` crate-type artifact."""
    # `str.splitlines()` also splits on bare `\r` (and other unicode line
    # separators), and soldr's own progress banner writes `\r`-terminated
    # updates to the same stream cargo's `-v` output goes to. That can slice
    # a single long rustc invocation line into fragments at a `\r`, so a
    # substring check against any one fragment misses it even though the
    # full text is present in `build_log`. Split on `\n` only.
    lines = []
    for line in strip_ansi(build_log).split("\n"):
        if "Running `" not in line:
            continue
        if "--crate-type bin" not in line:
            continue
        if (
            f"--crate-name {package_bin_crate}" not in line
            and "--crate-name soldr " not in line
        ):
            continue
        lines.append(line)
    return lines


def extract_linker_path(invocation: str) -> str | None:
    match = re.search(r"-C ?linker=(\S+)", invocation)
    return match.group(1) if match else None


def main() -> int:  # noqa: C901
    args = parse_args()
    repo_root = pathlib.Path(args.repo_root).resolve()
    soldr_bin = (repo_root / args.soldr_bin).resolve()
    if not soldr_bin.is_file():
        raise SystemExit(f"reld_dogfood_proof: soldr binary not found at {soldr_bin}")

    force_rebuild(repo_root)
    build_log = run_verbose_build(soldr_bin, args.package, repo_root)

    root = soldr_root(soldr_bin, repo_root)
    invocations = final_link_invocations(build_log, "soldr")
    if not invocations:
        print(build_log, file=sys.stderr)
        raise SystemExit(
            "reld_dogfood_proof: no `--crate-type bin` rustc invocation was "
            "captured under `-v` -- the build may have been fully cached. "
            "This script must run against a build that actually relinks. "
            "(full build log printed above for diagnosis)"
        )

    failures: list[str] = []
    for invocation in invocations:
        linker = extract_linker_path(invocation)
        if linker is None:
            # No -C linker= at all means soldr injected nothing for this
            # target -- acceptable only if the project itself declared a
            # linker some other way, but soldr's own Cargo.toml declares
            # `linker = "reld"`, so soldr must always inject one here.
            failures.append(f"no `-C linker=` on invocation: {invocation[:200]}...")
            continue

        bare_name = pathlib.Path(linker).name
        if bare_name in BARE_RELD_NAMES and not pathlib.Path(linker).is_absolute():
            failures.append(
                f"linker value is a bare unresolved `{linker}` "
                f"(clang would reject --ld-path={linker}): {invocation[:200]}..."
            )
            continue
        shim = None
        if SHIM_NAME.match(linker):
            shim = locate_shim(linker, root)
            if shim is None:
                failures.append(
                    f"generated linker shim {linker} not found under soldr's roots"
                )
                continue
        elif not pathlib.Path(linker).is_absolute():
            failures.append(
                f"linker value is not an absolute path or a shim name: {linker}"
            )
            continue
        elif not pathlib.Path(linker).is_file():
            failures.append(f"injected linker path does not exist on disk: {linker}")
            continue

        # On Linux this is a generated clang driver shim; read it back and
        # confirm the embedded --ld-path= is itself an absolute, existing
        # reld executable -- not a second layer of indirection to a bare name.
        if shim is not None:
            shim_body = shim.read_text(encoding="utf-8")
            shim_match = re.search(r"--ld-path=(\S+)", shim_body)
            if shim_match is None:
                failures.append(f"linker shim has no --ld-path=: {shim_body}")
                continue
            reld_path = shim_match.group(1).strip("'\"")
            if not pathlib.Path(reld_path).is_absolute():
                failures.append(f"shim's --ld-path= is not absolute: {reld_path}")
                continue
            if not pathlib.Path(reld_path).is_file():
                failures.append(f"shim's --ld-path= target does not exist: {reld_path}")
                continue

        if "--cfg tokio_unstable" not in invocation:
            failures.append(
                "the reld linker invocation lost `--cfg tokio_unstable` -- "
                "the project's own [build] rustflags in .cargo/config.toml "
                f"were clobbered (soldr#3277-shaped regression): {invocation[:200]}..."
            )

    if failures:
        print("reld_dogfood_proof: FAILED", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    print(
        f"reld_dogfood_proof: OK -- {len(invocations)} link invocation(s) used a "
        "resolved absolute reld linker path and retained --cfg tokio_unstable."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
