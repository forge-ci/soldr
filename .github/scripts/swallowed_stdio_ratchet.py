#!/usr/bin/env python3
"""Ratchet: don't grow the count of swallowed child-process stdio sites.

Part of the stdout/stderr swallowing audit (owner directive: "We should always
handle stdout, not just swallow it, and stderr as well.", soldr#3389). Rust
production code is covered by the `ban_swallowed_child_stdio` Dylint
(soldr#3387); this is the Python/shell/workflow half plus the check that every
Dylint `allow`/`expect` escape hatch for that lint carries a reason
(soldr#3388).

## What is flagged

* **Python** (`*.py`): a call with `stdout=` or `stderr=` set to
  `subprocess.DEVNULL` (any alias, e.g. `sp.DEVNULL`), a bare `DEVNULL` name
  (`from subprocess import DEVNULL`), or `open(os.devnull, ...)`. `stdin=` is
  never flagged -- closing a child's input is not swallowing its diagnostics.
  Detection is AST-based (not text/regex), so it cannot be fooled by a string
  or comment that merely mentions the pattern, and v1 does not attempt to
  detect output that is captured and then never printed on failure.
* **Shell** (`*.sh`, the extensionless repo-root scripts `install`/`lint`/
  `test`, any other extensionless tracked file whose first line is a `sh`/
  `bash` shebang, and `run:` blocks inside `.github/workflows/*.yml` +
  `.github/actions/**/action.yml`): a redirect of stdout/stderr to
  `/dev/null` -- `2>/dev/null`, `>/dev/null 2>&1`, `&>/dev/null`,
  `1>/dev/null`, `2> /dev/null` (optional spaces). A presence check --
  `command -v X >/dev/null 2>&1` -- is exempt; `type X` / `hash X` are judged
  equivalent (same "does this exist" idiom) and exempted the same way.
* **Rust** (`*.rs`, repo-wide): an `allow(ban_swallowed_child_stdio)` or
  `expect(ban_swallowed_child_stdio)` -- bare or inside
  `#[cfg_attr(dylint_lib = "ban_swallowed_child_stdio", ...)]`, whichever form
  is actually written -- that carries no reason. This repo's seven existing
  sites (PR #3393) spell the reason as a `// reason: ...` comment on the line
  (or the first line of a multi-line comment block) immediately above the
  attribute, never as an inline `reason = "..."` attribute argument, so that
  is what this checks for; an inline `reason = "..."` on the same line is
  also accepted in case that spelling is adopted later. Comments and string
  literals are masked first (reusing
  `platform_cfg_boundary_ratchet.mask_comments_and_strings`), so a doc-comment
  example or an error-message string that merely mentions the attribute text
  is not mistaken for a real one.

## Escape hatch

A per-line marker exempts that line outright, regardless of language:
`# stdio-ok: <reason>` in Python/shell/YAML, `// stdio-ok: <reason>` in Rust.
The reason must be non-empty -- a bare marker with nothing after the colon
does not exempt anything, so it cannot be used as a silent switch.

## Ratchet semantics

Baseline = the violation set at the merge base; new entries fail, removals
always pass, an untouched file is never re-examined (same "cost lands on the
change that causes it" shape as `loc_ratchet.py`, which this also borrows
`resolve_base`/`NoMergeBase` from). Only files the diff actually touched
(`git diff --diff-filter=AM <base> HEAD`) are scanned for the check --
scanning every tracked file at both refs on every PR would be needlessly
slow, and it cannot change the answer: an untouched file has identical
content at both refs, so its hit set (and thus its keys) cannot differ.

A violation is keyed as `(path, normalized_text, occurrence_index)`, not by
line number. `loc_ratchet.py` can key on line count because "did the file
grow" is a single number; a stdio site is identified by *what* it says, and
a completely unrelated edit earlier in the same file (e.g. adding an import)
shifts every following line number without changing what any of them say.
Keying on line number would report every such shift as a new violation on
every touched file, which is exactly the false-positive shape `loc_ratchet.py`
was rewritten once already to avoid (comparing against the base tip instead
of the merge base). `normalized_text` strips all whitespace so reformatting
alone (e.g. black re-wrapping a call) does not change the key either.
`occurrence_index` disambiguates two textually-identical sites in the same
file, counted in top-to-bottom order, so it stays stable under a pure
insertion/deletion elsewhere in the file.

A deleted file simply stops contributing keys -- nothing to compare, nothing
flagged. A renamed file is not tracked as a rename: the old path's keys
disappear (an allowed removal) and the new path's keys are evaluated as if
new, so a content-preserving rename typically needs its `stdio-ok` marker (or
Rust reason comment) to travel with it. That is a deliberate v1
simplification, not an attempt at rename-detection; renames are rare enough
next to this ratchet's actual target (new call sites) that solving it is not
worth the added complexity.

## Usage

    swallowed_stdio_ratchet.py --base-ref origin/main [--base-sha SHA]
    swallowed_stdio_ratchet.py --list   # full current inventory, exit 0

`--list` (alias `--all`) scans every tracked file (not just the diff) and
prints the whole current inventory with a per-language count, for burn-down
tracking -- it never fails the build.
"""

from __future__ import annotations

import argparse
import ast
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

_SCRIPT_DIR = Path(__file__).resolve().parent
if str(_SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(_SCRIPT_DIR))

# soldr#2740: one parser per concept -- reuse loc_ratchet's proven merge-base
# resolution (it already handles the shallow-checkout skip case correctly)
# and platform_cfg_boundary_ratchet's Rust comment/string masker, rather than
# growing a third copy of either.
from loc_ratchet import (  # noqa: E402  # pylint: disable=wrong-import-position
    NoMergeBase,
    resolve_base,
)
from platform_cfg_boundary_ratchet import (  # noqa: E402  # pylint: disable=wrong-import-position
    mask_comments_and_strings,
)

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

EXCLUDED_DIR_NAMES = {"_vender", "target", ".venv"}
SHELL_ROOT_NAMES = {"install", "lint", "test"}
SHEBANG_RE = re.compile(r"^#!.*\bsh\b")

STDIO_OK_RE = re.compile(r"#\s*stdio-ok:\s*(\S.*)$")
STDIO_OK_RE_RUST = re.compile(r"//\s*stdio-ok:\s*(\S.*)$")

DEVNULL_REDIRECT_RE = re.compile(r"(?:&>|[012]?>)\s*/dev/null")
PRESENCE_CHECK_RE = re.compile(r"\b(?:command\s+-v|type|hash)\s+\S+")

ALLOW_EXPECT_RE = re.compile(
    r"\b(?:allow|expect)\s*\(\s*ban_swallowed_child_stdio\s*\)"
)
ATTR_START_RE = re.compile(r"#!?\[")
REASON_COMMENT_RE = re.compile(r"^\s*//+!?\s*reason:\s*(\S.*)$")
INLINE_REASON_RE = re.compile(r'reason\s*=\s*"([^"]*)"')

RUN_KEY_RE = re.compile(r"^(?P<indent>[ \t]*)-?\s*run:\s*(?P<rest>.*)$")
BLOCK_SCALAR_RE = re.compile(r"^[|>][+-]?\d*\s*(#.*)?$")


@dataclass(frozen=True)
class Hit:
    """One flagged source line: its 1-based line number and stripped text."""

    line: int
    text: str


@dataclass(frozen=True)
class ViolationKey:
    """Line-number-independent identity of a violation (see module docs)."""

    path: str
    normalized_text: str
    occurrence: int


@dataclass(frozen=True)
class EvaluationResult:
    """Outcome of `evaluate`: the new violations and the files examined."""

    new: list["Violation"]
    checked: int


@dataclass(frozen=True)
class Violation:
    path: str
    line: int
    lang: str
    text: str
    occurrence: int

    def key(self) -> ViolationKey:
        return ViolationKey(self.path, "".join(self.text.split()), self.occurrence)

    def describe(self) -> str:
        marker = (
            "// stdio-ok: <reason>" if self.lang == "rust" else "# stdio-ok: <reason>"
        )
        return (
            f"{self.path}:{self.line}: {self.text}\n"
            f"      forward/log the output per soldr#3389, or add `{marker}`"
        )


# --------------------------------------------------------------------------
# Path classification
# --------------------------------------------------------------------------


def is_excluded(path: str) -> bool:
    parts = PurePosixPath(path).parts
    return any(part in EXCLUDED_DIR_NAMES for part in parts[:-1])


def is_workflow_path(path: str) -> bool:
    posix = PurePosixPath(path)
    if path.startswith(".github/workflows/") and posix.suffix in (".yml", ".yaml"):
        return True
    if path.startswith(".github/actions/") and posix.name in (
        "action.yml",
        "action.yaml",
    ):
        return True
    return False


def classify(path: str, source: str | None) -> str | None:
    posix = PurePosixPath(path)
    suffix = posix.suffix
    if suffix == ".py":
        return "python"
    if suffix == ".rs":
        return "rust"
    if is_workflow_path(path):
        return "workflow"
    if suffix == ".sh":
        return "shell"
    if suffix == "" and (posix.name in SHELL_ROOT_NAMES or source is not None):
        if source is None:
            return None
        first_line = source.split("\n", 1)[0]
        if SHEBANG_RE.match(first_line):
            return "shell"
    return None


# --------------------------------------------------------------------------
# Python: AST-based DEVNULL detection
# --------------------------------------------------------------------------


def _is_devnull_value(node: ast.AST) -> bool:
    if isinstance(node, ast.Attribute) and node.attr == "DEVNULL":
        return True
    if isinstance(node, ast.Name) and node.id == "DEVNULL":
        return True
    if isinstance(node, ast.Call):
        func = node.func
        func_name = None
        if isinstance(func, ast.Name):
            func_name = func.id
        elif isinstance(func, ast.Attribute):
            func_name = func.attr
        if func_name == "open" and node.args:
            first = node.args[0]
            if isinstance(first, ast.Attribute) and first.attr == "devnull":
                return True
            if isinstance(first, ast.Name) and first.id == "devnull":
                return True
    return False


def _has_marker(lines: list[str], start: int, end: int) -> bool:
    for lineno in range(start, end + 1):
        if 1 <= lineno <= len(lines):
            match = STDIO_OK_RE.search(lines[lineno - 1])
            if match and match.group(1).strip():
                return True
    return False


def python_hits(source: str) -> list[Hit]:
    try:
        tree = ast.parse(source)
    except (SyntaxError, ValueError):
        return []
    lines = source.splitlines()
    hits: list[Hit] = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        for kw in node.keywords:
            if kw.arg not in ("stdout", "stderr"):
                continue
            if not _is_devnull_value(kw.value):
                continue
            start = getattr(node, "lineno", kw.lineno)
            end = getattr(node, "end_lineno", kw.lineno)
            if _has_marker(lines, start, end):
                continue
            segment = ast.get_source_segment(source, kw) or f"{kw.arg}=..."
            hits.append(Hit(kw.lineno, segment.strip()))
    hits.sort(key=lambda hit: (hit.line, hit.text))
    return hits


# --------------------------------------------------------------------------
# Shell / workflow: redirect-to-/dev/null detection
# --------------------------------------------------------------------------


def _shell_line_hit(line: str) -> str | None:
    if PRESENCE_CHECK_RE.search(line):
        return None
    marker = STDIO_OK_RE.search(line)
    if marker and marker.group(1).strip():
        return None
    if not DEVNULL_REDIRECT_RE.search(line):
        return None
    return line.strip()


def shell_hits(source: str) -> list[Hit]:
    hits: list[Hit] = []
    for lineno, line in enumerate(source.splitlines(), start=1):
        text = _shell_line_hit(line)
        if text:
            hits.append(Hit(lineno, text))
    return hits


def yaml_shell_lines(lines: list[str]) -> list[Hit]:
    """A `Hit` (line number, text) for every physical line inside a `run:` step.

    Text-based rather than a full YAML parse: GitHub Actions' `run:` block
    scalars follow a fixed indentation convention (content is indented
    strictly more than the `run:` key itself), which this walks directly.
    That is enough to find the shell lines without pulling in a YAML line-
    number-tracking loader for a check that only needs the raw text.
    """

    result: list[Hit] = []
    index = 0
    total = len(lines)
    while index < total:
        match = RUN_KEY_RE.match(lines[index])
        if not match:
            index += 1
            continue
        indent = len(match.group("indent"))
        rest = match.group("rest").strip()
        if rest and not BLOCK_SCALAR_RE.match(rest):
            result.append(Hit(index + 1, rest))
            index += 1
            continue
        index += 1
        while index < total:
            candidate = lines[index]
            if candidate.strip() == "":
                index += 1
                continue
            candidate_indent = len(candidate) - len(candidate.lstrip(" \t"))
            if candidate_indent <= indent:
                break
            result.append(Hit(index + 1, candidate))
            index += 1
    return result


def workflow_hits(source: str) -> list[Hit]:
    hits: list[Hit] = []
    for shell_line in yaml_shell_lines(source.splitlines()):
        offending = _shell_line_hit(shell_line.text)
        if offending:
            hits.append(Hit(shell_line.line, offending))
    return hits


# --------------------------------------------------------------------------
# Rust: allow/expect without a reason
# --------------------------------------------------------------------------


def _rust_marker_reason(line: str) -> str | None:
    match = STDIO_OK_RE_RUST.search(line)
    if match and match.group(1).strip():
        return match.group(1).strip()
    return None


def rust_hits(source: str) -> list[Hit]:  # noqa: C901
    # `mask_comments_and_strings` is exactly length-preserving (every branch
    # consumes as many input characters as it appends), so an offset into
    # `masked` is always the same offset into `source` -- but it is NOT
    # newline-preserving: a backslash-newline line continuation inside a
    # string literal (used by this repo's own lint-message `format!()`
    # strings) masks the newline itself to a space, which would silently
    # collapse two lines into one and desync line numbers if `masked` were
    # split into lines and indexed. Recomputing every line number from the
    # matching offset into the ORIGINAL `source` sidesteps that entirely.
    masked = mask_comments_and_strings(source)
    lines = source.splitlines()
    hits: list[Hit] = []
    for match in ALLOW_EXPECT_RE.finditer(masked):
        match_line = source[: match.start()].count("\n")
        if match_line >= len(lines):
            continue

        # Nearest preceding `#[` / `#![` in the masked text, i.e. skipping
        # over any that are only text inside a comment or string literal.
        window_start = max(0, match.start() - 400)
        attr_pos = match.start()
        for candidate in ATTR_START_RE.finditer(
            masked, window_start, match.start() + 1
        ):
            attr_pos = candidate.start()
        attr_line = source[:attr_pos].count("\n")

        collected: list[str] = []
        cursor = attr_line - 1
        while cursor >= 0 and lines[cursor].strip().startswith("//"):
            collected.append(lines[cursor])
            cursor -= 1

        reason = None
        for comment_line in collected:
            found = REASON_COMMENT_RE.match(comment_line)
            if found and found.group(1).strip():
                reason = found.group(1).strip()
                break

        inline = INLINE_REASON_RE.search(lines[match_line])
        if inline and inline.group(1).strip():
            reason = inline.group(1).strip()

        if reason:
            continue

        exempt = _rust_marker_reason(lines[match_line]) is not None
        if not exempt:
            exempt = any(
                _rust_marker_reason(comment_line) for comment_line in collected
            )
        if exempt:
            continue

        hits.append(Hit(match_line + 1, lines[match_line].strip()))
    return hits


# --------------------------------------------------------------------------
# Per-file hit -> Violation, with the occurrence index
# --------------------------------------------------------------------------


def file_hits(lang: str, source: str) -> list[Hit]:
    if lang == "python":
        return python_hits(source)
    if lang == "shell":
        return shell_hits(source)
    if lang == "workflow":
        return workflow_hits(source)
    if lang == "rust":
        return rust_hits(source)
    return []


def file_violations(path: str, lang: str, source: str) -> list[Violation]:
    hits = sorted(file_hits(lang, source), key=lambda item: item.line)
    counts: dict[str, int] = {}
    violations = []
    for hit in hits:
        lineno, text = hit.line, hit.text
        normalized = "".join(text.split())
        occurrence = counts.get(normalized, 0)
        counts[normalized] = occurrence + 1
        violations.append(
            Violation(
                path=path, line=lineno, lang=lang, text=text, occurrence=occurrence
            )
        )
    return violations


def source_violations(path: str, source: str | None) -> list[Violation]:
    if source is None:
        return []
    lang = classify(path, source)
    if lang is None:
        return []
    return file_violations(path, lang, source)


# --------------------------------------------------------------------------
# git plumbing
# --------------------------------------------------------------------------


def _run(args: list[str]) -> str:
    return run_captured(args, check=True, capture_output=True, text=True).stdout


def changed_files(base: str) -> list[str]:
    raw = _run(["git", "diff", "--name-only", "--diff-filter=AM", base, "HEAD"])
    return sorted(p for p in raw.splitlines() if p.strip() and not is_excluded(p))


def tracked_files_worktree() -> list[str]:
    raw = _run(["git", "ls-files"])
    return sorted(p for p in raw.splitlines() if p.strip() and not is_excluded(p))


def read_worktree(path: str) -> str | None:
    try:
        with open(path, "rb") as handle:
            blob = handle.read()
    except OSError:
        return None
    return blob.decode("utf-8", errors="replace")


def read_at_ref(ref: str, path: str) -> str | None:
    try:
        blob = run_captured(
            ["git", "show", f"{ref}:{path}"], check=True, capture_output=True
        ).stdout
    except subprocess.CalledProcessError:
        return None
    return blob.decode("utf-8", errors="replace")


# --------------------------------------------------------------------------
# Evaluation
# --------------------------------------------------------------------------


def evaluate(base_ref: str, base_sha: str | None = None) -> EvaluationResult:
    base = resolve_base(base_ref, base_sha)
    new: list[Violation] = []
    checked = 0
    for path in changed_files(base):
        current_source = read_worktree(path)
        if current_source is None:
            continue  # deleted on this branch -- nothing to compare
        lang = classify(path, current_source)
        if lang is None:
            continue
        checked += 1
        baseline_keys = {
            v.key() for v in source_violations(path, read_at_ref(base, path))
        }
        for violation in source_violations(path, current_source):
            if violation.key() not in baseline_keys:
                new.append(violation)
    new.sort(key=lambda v: (v.path, v.line))
    return EvaluationResult(new, checked)


def full_inventory() -> list[Violation]:
    violations: list[Violation] = []
    for path in tracked_files_worktree():
        source = read_worktree(path)
        if source is None:
            continue
        lang = classify(path, source)
        if lang is None:
            continue
        violations.extend(file_violations(path, lang, source))
    violations.sort(key=lambda v: (v.lang, v.path, v.line))
    return violations


def _print_inventory(inventory: list[Violation]) -> None:
    by_lang: dict[str, int] = {}
    for violation in inventory:
        by_lang[violation.lang] = by_lang.get(violation.lang, 0) + 1
    print(f"swallowed_stdio_ratchet: {len(inventory)} total, by language:")
    for lang in sorted(by_lang):
        print(f"  {lang}: {by_lang[lang]}")
    for violation in inventory:
        print(f"  - {violation.path}:{violation.line}: {violation.text}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--base-ref", default="origin/main")
    parser.add_argument(
        "--base-sha",
        default=None,
        help="exact base commit; skips merge-base, which a shallow CI checkout cannot compute",
    )
    parser.add_argument(
        "--list",
        "--all",
        dest="list_all",
        action="store_true",
        help="print the full current inventory (burn-down tracking) and exit 0",
    )
    args = parser.parse_args(argv)

    if args.list_all:
        _print_inventory(full_inventory())
        return 0

    try:
        result = evaluate(args.base_ref, args.base_sha)
        new, checked = result.new, result.checked
    except NoMergeBase:
        print(
            f"swallowed_stdio_ratchet: skipped — no merge base with {args.base_ref} "
            "(shallow checkout?). Not failing the build on a comparison that "
            "could not be made.",
            file=sys.stderr,
        )
        return 0

    if not new:
        print(
            f"swallowed_stdio_ratchet: {checked} changed file(s) checked, no new violations."
        )
        return 0

    print("swallowed_stdio_ratchet: FAIL", file=sys.stderr)
    for violation in new:
        print(f"  - {violation.describe()}", file=sys.stderr)
    print(
        "\nThis is a ratchet (soldr#3388, meta soldr#3389): swallowed child "
        "stdout/stderr must not grow. Forward and log the output per the owner "
        "directive, or mark a deliberate, reasoned exception inline.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
