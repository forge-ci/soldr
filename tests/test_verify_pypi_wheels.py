"""soldr#2469 step 2.2: the PyPI wheel-visibility gate, out of YAML.

The inline block this replaces compared `len(urls)` against a literal `8`.
These tests pin the two things that changes: the expected set is derived from
the target contract, and the check is by filename rather than by count.
"""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "verify_pypi_wheels.py"


def load_module():
    spec = importlib.util.spec_from_file_location("verify_pypi_wheels", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules["verify_pypi_wheels"] = module
    spec.loader.exec_module(module)
    return module


MODULE = load_module()


def test_expected_wheels_come_from_the_contract() -> None:
    wheels = MODULE.expected_wheels("v1.2.3")
    contract_triples = MODULE.RELEASE_COMPLETENESS.included_triples()
    assert len(wheels) == len(contract_triples)
    assert all(name.startswith("soldr-1.2.3-py3-none-") for name in wheels)
    assert all(name.endswith(".whl") for name in wheels)


def test_version_is_accepted_with_or_without_the_v_prefix() -> None:
    assert MODULE.expected_wheels("v1.2.3") == MODULE.expected_wheels("1.2.3")


def test_a_complete_set_reports_nothing_missing() -> None:
    assert MODULE.missing_wheels("v1.2.3", MODULE.expected_wheels("v1.2.3")) == []


def test_a_missing_wheel_is_named() -> None:
    expected = MODULE.expected_wheels("v1.2.3")
    missing = MODULE.missing_wheels("v1.2.3", expected[1:])
    assert missing == [expected[0]]


def test_the_right_count_of_wrong_wheels_does_not_satisfy_the_gate() -> None:
    """The 0.9.0 failure class: counts agreed while the set was wrong.

    The inline block asserted `count >= 8`, so eight files of any names
    passed. Checking by name is the whole point of the extraction.
    """
    expected = MODULE.expected_wheels("v1.2.3")
    decoys = [
        f"soldr-1.2.3-py3-none-bogus_{index}.whl" for index in range(len(expected))
    ]
    assert len(decoys) == len(expected)
    assert MODULE.missing_wheels("v1.2.3", decoys) == expected


def test_extra_unexpected_files_do_not_mask_a_missing_wheel() -> None:
    expected = MODULE.expected_wheels("v1.2.3")
    published = [*expected[1:], "soldr-1.2.3.tar.gz", "soldr-1.2.3-py3-none-any.whl"]
    assert MODULE.missing_wheels("v1.2.3", published) == [expected[0]]


def test_published_filenames_tolerates_a_malformed_payload() -> None:
    assert MODULE.published_filenames({}) == []
    assert MODULE.published_filenames({"urls": "not-a-list"}) == []
    assert MODULE.published_filenames({"urls": [{"no_filename": 1}, "junk"]}) == []
    assert MODULE.published_filenames({"urls": [{"filename": "a.whl"}]}) == ["a.whl"]


def test_polling_stops_as_soon_as_the_set_is_complete() -> None:
    expected = MODULE.expected_wheels("v1.2.3")
    calls: list[int] = []
    slept: list[float] = []

    def fetch(version: str) -> list[str]:
        assert version == "1.2.3", version
        calls.append(1)
        # Incomplete on the first look, complete on the second.
        return expected[1:] if len(calls) == 1 else expected

    missing = MODULE.wait_for_wheels(
        "v1.2.3",
        max_attempts=10,
        deadline_seconds=1e9,
        poll_seconds=15.0,
        fetch=fetch,
        sleep=slept.append,
        now=lambda: 0.0,
        log=lambda message: None,
    )
    assert missing == []
    assert len(calls) == 2
    # One sleep between the two attempts, and none after success.
    assert slept == [15.0]


def test_polling_gives_up_at_the_attempt_cap_and_names_what_is_missing() -> None:
    expected = MODULE.expected_wheels("v1.2.3")
    missing = MODULE.wait_for_wheels(
        "v1.2.3",
        max_attempts=3,
        deadline_seconds=1e9,
        poll_seconds=0.0,
        fetch=lambda _version=None: expected[2:],
        sleep=lambda seconds: None,
        now=lambda: 0.0,
        log=lambda message: None,
    )
    assert missing == expected[:2]


def test_polling_gives_up_at_the_wall_clock_deadline() -> None:
    """The attempt cap alone is not enough: a slow index can outlast it."""
    expected = MODULE.expected_wheels("v1.2.3")
    attempts: list[int] = []

    def fetch(version: str) -> list[str]:
        assert version == "1.2.3", version
        attempts.append(1)
        return []

    missing = MODULE.wait_for_wheels(
        "v1.2.3",
        max_attempts=1000,
        deadline_seconds=100.0,
        poll_seconds=0.0,
        fetch=fetch,
        sleep=lambda seconds: None,
        now=lambda: 500.0,
        log=lambda message: None,
    )
    assert missing == expected
    # Deadline already passed, so exactly one attempt was made.
    assert len(attempts) == 1


def test_the_workflow_invokes_the_script_and_keeps_no_inline_wheel_count() -> None:
    workflow = (REPO_ROOT / ".github" / "workflows" / "release-auto.yml").read_text(
        encoding="utf-8"
    )
    assert "verify_pypi_wheels.py" in workflow
    assert "expected=8" not in workflow, (
        "the expected wheel count must come from the target contract "
        "(soldr#2469 step 2.1), not a literal in the workflow"
    )
