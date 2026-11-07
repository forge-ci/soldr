"""A workflow that reaches macOS must not use bash-4-only builtins.

macOS ships **bash 3.2** and always will: bash went GPLv3 at 4.0 and Apple will
not ship that. So `mapfile` / `readarray`, introduced in bash 4.0, do not exist
on a native macOS runner.

This is not hypothetical -- it stopped the v0.8.30 release:

    /Users/runner/work/_temp/....sh: line 2: mapfile: command not found
    ##[error]Process completed with exit code 127

in `Ratchet the macOS minimum version (soldr#1060)`. Publishing is gated on
`build.result == 'success'`, so nothing shipped; the release just stopped.

Two things let it survive introduction unnoticed:

1. **The step only runs during a release.** Every PR parsed the same workflow
   file without ever reaching that job, so CI was green the whole time it was
   broken. It failed the first time it was ever executed.
2. **The sibling darwin lane passed.** `macOS x64 (cross-compiled)` builds on a
   *Linux* runner, where bash is 5.x, so an `apple-darwin`-gated step ran there
   happily. Only the native `macOS ARM64` lane has bash 3.2 -- which is why
   "the other darwin lane is fine" was not evidence of anything.

The rule is per **file**, not per step, deliberately. Deciding which steps can
land on macOS means resolving `runs-on`, matrix includes and `if:` expressions,
and a rule that subtle is one refactor away from silently not applying. "This
file runs on macOS somewhere, so its shell stays bash-3.2-portable" needs no
resolution and cannot drift. The portable loop costs three lines and works on
every runner, so there is nothing to trade away.

Stdlib only, like the other workflow guards here -- pyyaml is not installed in
CI's lint environment, and a guard that silently skips is worse than no guard.
"""

from __future__ import annotations

import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

WORKFLOWS = sorted((REPO_ROOT / ".github" / "workflows").glob("*.y*ml"))

# Shell that CONTRIBUTORS run, which is a wider macOS surface than CI: a Mac
# developer's `/bin/bash` is 3.2 whether or not any workflow is involved.
# `install.sh` is the sharp one -- it detects `Darwin` explicitly, so it is
# meant to run there, and it used `readarray`.
SHELL_SCRIPTS = sorted(
    path
    for path in list(REPO_ROOT.glob("*.sh"))
    + list((REPO_ROOT / "bench").rglob("*.sh"))
    + list((REPO_ROOT / "perf").rglob("*.sh"))
    + list((REPO_ROOT / "ci").rglob("*.sh"))
    # Relative to the repo root, not absolute: a checkout can itself live
    # under a directory named here (a git worktree under .claude/worktrees
    # does), and an absolute-parts filter then excludes every file. The
    # anti-vacuity test below caught exactly that.
    if "_vender" not in path.relative_to(REPO_ROOT).parts
)

# Introduced in bash 4.0. Replacement:
#   arr=()
#   while IFS= read -r line; do arr+=("$line"); done < <(...)
BASH4_BUILTINS = ("mapfile", "readarray", "coproc")

# bash 4.0 added more than builtins, and each fails differently -- a syntax
# error rather than "command not found" -- but all fail on macOS. Covering only
# `mapfile` would leave the next one to be found the same way this one was: by
# a release stopping.
#
# `declare -A` / `local -A`  associative arrays (bash 4.0)
# `${v^^}` `${v,,}` `${v^}` `${v,}`  case modification (bash 4.0)
# `|&`  shorthand for `2>&1 |` (bash 4.0)
# `&>>` append both streams (bash 4.0; plain `&>` is fine in 3.2)
BASH4_SYNTAX = (
    (
        re.compile(r"\b(?:declare|local|typeset)\s+-[A-Za-z]*A"),
        "declare -A (associative array)",
    ),
    # `[^{]` guards GitHub's own `${{ ... }}` expressions, which are not shell.
    (re.compile(r"\$\{[^{}]*[\^,]{1,2}\}"), "${v^^} / ${v,,} case modification"),
    (re.compile(r"(?<![|&>])\|&(?!&)"), "|& (use 2>&1 |)"),
    (re.compile(r"&>>"), "&>> (use >> file 2>&1)"),
)

# A runner label naming macOS: `runs-on: macos-15`, `macos-latest`, a matrix
# `runner: macos-14`, etc.
MACOS_RUNNER = re.compile(r"\bmacos-[a-z0-9.]+\b", re.IGNORECASE)


def _reaches_macos(text: str) -> bool:
    return bool(MACOS_RUNNER.search(text))


# A builtin is in command position at the start of a line OR after a separator
# -- `foo=(); mapfile -t foo < <(...)` is one line and was previously invisible
# to this scan.
_SEPARATORS = r"(?:^|[;&|]\s*|&&\s*|\|\|\s*)"
BASH4_BUILTIN_RE = re.compile(
    _SEPARATORS + r"(" + "|".join(BASH4_BUILTINS) + r")\s", re.MULTILINE
)


def _bash4_command_lines(text: str) -> "list[tuple[int, str]]":
    """Lines using a bash-4-only builtin or syntax.

    Comments are skipped: the words also appear in the comments explaining why
    they are not used, and flagging those would make the fix look like the bug.
    """
    hits = []
    for number, line in enumerate(text.splitlines(), start=1):
        stripped = line.strip()
        if stripped.startswith("#"):
            continue
        if BASH4_BUILTIN_RE.search(stripped):
            hits.append((number, stripped))
            continue
        for pattern, _label in BASH4_SYNTAX:
            if pattern.search(stripped):
                hits.append((number, stripped))
                break
    return hits


def test_macos_capable_workflows_avoid_bash4_builtins() -> None:
    offenders = []
    for workflow in WORKFLOWS:
        text = workflow.read_text(encoding="utf-8")
        if not _reaches_macos(text):
            continue
        for number, line in _bash4_command_lines(text):
            offenders.append(f"{workflow.name}:{number}: {line}")
    assert not offenders, (
        "macOS ships bash 3.2, which has no `mapfile`/`readarray`; these would "
        "fail with 'command not found' (exit 127) on a native macOS runner:\n  "
        + "\n  ".join(offenders)
        + '\nUse: arr=(); while IFS= read -r line; do arr+=("$line"); done < <(...)'
    )


def test_the_scan_still_sees_macos_workflows_and_command_lines() -> None:
    # Without this, a regex that stopped matching would make the assertion above
    # vacuously true -- the failure mode the guard exists to catch.
    macos_workflows = [
        w.name for w in WORKFLOWS if _reaches_macos(w.read_text(encoding="utf-8"))
    ]
    # Four today. The bar is deliberately below that: this asserts the scan is
    # still finding workflows at all, not the exact inventory, which would turn
    # every unrelated workflow addition into a failure here.
    assert len(macos_workflows) >= 3, (
        f"expected several macOS workflows, got {macos_workflows}"
    )
    assert "release-auto.yml" in macos_workflows, (
        "release-auto.yml is the workflow this guard exists for -- if it is no "
        f"longer detected as macOS-capable the rule is not applying: {macos_workflows}"
    )

    # And the command-position detector must actually fire on the real shape.
    sample = "foo=()\nmapfile -t foo < <(find .)\n# mapfile is not used here\n"
    hits = _bash4_command_lines(sample)
    assert len(hits) == 1, f"detector should find exactly the command line, got {hits}"
    assert hits[0][0] == 2


def test_every_bash4_construct_is_detected() -> None:
    """One case per construct, so widening the rule cannot silently half-apply.

    `mapfile` was the one that stopped a release; the others fail the same way
    on the same runner and were simply not reached yet.
    """
    cases = {
        "mapfile": "mapfile -t out < <(find .)",
        "readarray": "readarray -t out < <(find .)",
        "coproc": "coproc reader { cat; }",
        "declare -A": "declare -A seen=([a]=1)",
        "local -A": "local -A seen",
        "upper": 'x="${name^^}"',
        "lower": 'x="${name,,}"',
        "first-char": 'x="${name^}"',
        "pipe-both": "make |& tee log",
        "append-both": "make &>> log",
    }
    missed = [label for label, line in cases.items() if not _bash4_command_lines(line)]
    assert not missed, f"these bash-4-only constructs are not detected: {missed}"


def test_portable_and_unrelated_shell_is_not_flagged() -> None:
    """False positives would push people to disable the guard."""
    benign = [
        # The portable replacement the failure message recommends.
        'arr=(); while IFS= read -r line; do arr+=("$line"); done < <(find .)',
        # GitHub's own expression syntax is not shell and must not trip `${v,,}`.
        "name: ${{ matrix.target }}",
        "if: ${{ github.event_name == 'push' }}",
        # bash 3.2 redirections that merely look similar.
        "make &> log",
        "make 2>&1 | tee log",
        "x=$((1 & 2))",
        # Ordinary array use.
        "declare -a plain",
        # The word inside prose, which the comment skip already covers.
        "# mapfile is deliberately not used here",
    ]
    flagged = [line for line in benign if _bash4_command_lines(line)]
    assert not flagged, f"portable shell must not be flagged: {flagged}"


def test_contributor_shell_scripts_avoid_bash4() -> None:
    """The wider surface: shell a developer runs on their own Mac.

    A workflow only reaches macOS through a runner label, but `./install.sh`
    reaches it through a person. It branches on `Darwin` explicitly, so macOS
    is a supported platform for it -- and it used `readarray`, which means a
    stock Mac (bash 3.2, frozen there by GPLv3) could not run the installer.
    """
    offenders = []
    for script in SHELL_SCRIPTS:
        for number, line in _bash4_command_lines(script.read_text(encoding="utf-8")):
            offenders.append(
                f"{script.relative_to(REPO_ROOT).as_posix()}:{number}: {line}"
            )
    assert not offenders, (
        "these run on contributor machines, including macOS with bash 3.2:\n  "
        + "\n  ".join(offenders)
    )


def test_the_shell_scan_still_finds_scripts() -> None:
    # Same anti-vacuity guard as the workflow scan: a glob that stopped
    # matching would make the assertion above pass by finding nothing.
    names = {s.name for s in SHELL_SCRIPTS}
    assert len(SHELL_SCRIPTS) >= 10, f"expected many shell scripts, got {sorted(names)}"
    assert "install.sh" in names, (
        "install.sh is the script this rule most exists for -- if it is no "
        f"longer scanned the rule is not applying: {sorted(names)}"
    )
