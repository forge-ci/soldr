"""soldr#2469 step 2.2: release-auto.yml inline logic shrinks monotonically.

The 0.9.0 incident chain ran through inline YAML no test could exercise.
Step 2.2's end state is release decision/state logic living in unit-tested
scripts; this ratchet is the enforcement half — every change to
release-auto.yml may keep or shrink its inline ``run:`` block footprint,
never grow it. Extraction PRs lower the baseline in the same reviewed diff.

The metric is deliberately dumb: non-empty lines inside ``run: |`` / ``run: >``
blocks. Dumb metrics make bad proxies for quality but excellent ratchets —
there is no way to add an inline branch of release logic without the number
going up.
"""

from __future__ import annotations

import re
from pathlib import Path

WORKFLOW = Path(__file__).parents[1] / ".github" / "workflows" / "release-auto.yml"

# Measured 2026-08-17 (soldr#2469 step 2.2 ratchet start) at 187, then:
#   160 — PyPI wheel-visibility poll -> `.github/scripts/verify_pypi_wheels.py`
#   117 — tag pre-create + draft-release create -> `.github/scripts/release_publish.py`
#    97 — matrix release-binary build -> `native_release_build.py matrix-binary`
# Lower this number in the same PR whenever extraction shrinks the workflow;
# never raise it — new release logic belongs in a `ci/*.py` or
# `.github/scripts/*.py` script with unit tests, invoked from a one-line `run:`.
INLINE_RUN_LINE_CEILING = 97


def count_inline_run_lines(text: str) -> int:
    lines = text.splitlines()
    total = 0
    in_block = False
    block_indent = 0
    for line in lines:
        stripped = line.strip()
        indent = len(line) - len(line.lstrip(" "))
        if in_block:
            if stripped and indent <= block_indent:
                in_block = False
            else:
                if stripped:
                    total += 1
                continue
        if re.match(r"^run:\s*[|>]", stripped):
            in_block = True
            block_indent = indent
    return total


def test_release_workflow_inline_logic_only_shrinks() -> None:
    measured = count_inline_run_lines(WORKFLOW.read_text(encoding="utf-8"))
    assert measured <= INLINE_RUN_LINE_CEILING, (
        f"release-auto.yml grew to {measured} inline run-block lines "
        f"(ceiling {INLINE_RUN_LINE_CEILING}). Inline release logic is "
        "untestable (soldr#2469 step 2.2) — move the addition into a "
        "unit-tested ci/*.py or .github/scripts/*.py script and invoke it "
        "from a one-line run:."
    )
    # The ratchet only bites if the pinned number tracks reality downward.
    slack = INLINE_RUN_LINE_CEILING - measured
    assert slack <= 50, (
        f"release-auto.yml shrank to {measured} inline run-block lines but "
        f"the ceiling still sits at {INLINE_RUN_LINE_CEILING}; lower "
        "INLINE_RUN_LINE_CEILING to match so the reclaimed headroom cannot "
        "silently grow back"
    )


def test_the_metric_counts_block_scalars_only() -> None:
    sample = "\n".join(
        [
            "steps:",
            "  - name: one-liner",
            "    run: python3 ci/thing.py",
            "  - name: block",
            "    run: |",
            "      a=1",
            "",
            "      b=2",
            "  - name: after",
            "    run: >-",
            "      python3 x.py",
            "      --flag",
        ]
    )
    assert count_inline_run_lines(sample) == 4
