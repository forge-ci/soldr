"""Contract tests for the reld dogfood proof's build-log parsing (soldr#3893)."""

import importlib.util
from pathlib import Path

SCRIPT = Path(__file__).parents[1] / ".github" / "scripts" / "reld_dogfood_proof.py"
_spec = importlib.util.spec_from_file_location("reld_dogfood_proof", SCRIPT)
assert _spec and _spec.loader
module = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(module)

LINK = (
    "rustc --crate-name soldr --edition=2021 crates/soldr-cli/src/main.rs "
    "--crate-type bin -C linker=/opt/reld --cfg tokio_unstable"
)


def test_single_line_invocation_is_found_whole() -> None:
    log = f"   Compiling soldr-cli\n     Running `{LINK}`\n"
    assert module.final_link_invocations(log, "soldr") == [LINK]


def test_invocation_split_by_an_interleaved_line_keeps_its_tail() -> None:
    # Another writer on the same stream (soldr's timestamped stderr) can land
    # a newline mid-invocation; the project rustflags at the tail must still
    # be attributed to it.
    head, tail = LINK.split(" -C linker=")
    log = (
        f"   93.05      Running `{head}\n"
        "   93.06 soldr: cache 12 HIT\n"
        f" -C linker={tail}`\n"
    )
    [invocation] = module.final_link_invocations(log, "soldr")
    assert "--cfg tokio_unstable" in invocation
    assert module.extract_linker_path(invocation) == "/opt/reld"


def test_non_bin_and_foreign_crates_are_ignored() -> None:
    log = (
        "     Running `rustc --crate-name tokio --crate-type lib`\n"
        "     Running `rustc --crate-name other --crate-type bin`\n"
    )
    assert module.final_link_invocations(log, "soldr") == []
