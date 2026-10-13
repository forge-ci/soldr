from __future__ import annotations

import json
import re
import shlex
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
GUARDRAILS_PATH = REPO_ROOT / "contracts" / "zccache-integration-guardrails.v1.json"
DOCS_PATH = REPO_ROOT / "docs" / "ZCCACHE_INTEGRATION_GUARDRAILS.md"
CLI_TESTS_DIR = REPO_ROOT / "crates" / "soldr-cli" / "tests"

# `-E 'test(/^<module>::/)'` -- the nextest filter form that narrows a category
# target back down to one module (soldr#2934).
NEXTEST_MODULE_FILTER = re.compile(r"^test\(/\^(?P<module>[A-Za-z0-9_]+)::/\)$")

REQUIRED_GUARDRAIL_IDS = {
    "embedded-runtime-topology",
    "embedded-session-env",
    "disabled-and-non-build",
    "embedded-flush-shutdown",
    "setup-action-outputs",
    "release-npm-staging",
    "perf-cold-warm",
    "perf-worktree-share",
    "perf-touch-no-change",
    "perf-build-then-check",
    "native-cc-cache",
    "monolith-migration-ratchet",
}

REQUIRED_COMMAND_IDS = {
    "rust-embedded-runtime-topology",
    "rust-zccache-entry",
    "rust-wrapper-env",
    "rust-cache-cli",
    "rust-cache-session",
    "rust-native-cache-unit",
    "rust-native-cache-integration",
    "rust-wrapper-perf",
    "rust-watchdog-lint",
    "python-setup-contracts",
    "node-npm-contract",
    "perf-cold-warm",
    "perf-matrix-worktree-touch",
}


def _guardrails() -> dict:
    return json.loads(GUARDRAILS_PATH.read_text(encoding="utf-8"))


def test_guardrail_contract_has_required_axes_and_commands() -> None:
    contract = _guardrails()

    assert contract["schema_version"] == 1
    assert contract["parent_issue"] == "https://github.com/zackees/soldr/issues/543"
    assert contract["wave_issue"] == "https://github.com/zackees/soldr/issues/548"

    commands = contract["validation_commands"]
    command_ids = {command["id"] for command in commands}
    assert len(commands) == len(command_ids), "validation command IDs must be unique"
    assert command_ids == REQUIRED_COMMAND_IDS
    for command in commands:
        assert command["gate"] in {"hard", "report-only"}
        assert command["command"].strip()

    guardrails = contract["guardrails"]
    guardrail_ids = {guardrail["id"] for guardrail in guardrails}
    assert len(guardrails) == len(guardrail_ids), "guardrail IDs must be unique"
    assert guardrail_ids == REQUIRED_GUARDRAIL_IDS
    referenced_command_ids: set[str] = set()
    for guardrail in guardrails:
        assert guardrail["gate"] in {"hard", "report-only"}
        assert guardrail["axis"]
        assert guardrail["covers"]
        assert guardrail["validation_command_ids"]
        assert set(guardrail["validation_command_ids"]).issubset(command_ids)
        referenced_command_ids.update(guardrail["validation_command_ids"])
    assert referenced_command_ids == command_ids


def test_guardrail_test_files_exist() -> None:
    contract = _guardrails()

    for guardrail in contract["guardrails"]:
        for rel_path in guardrail["test_files"]:
            path = REPO_ROOT / rel_path
            assert path.exists(), (
                f"{guardrail['id']} references missing path: {rel_path}"
            )


def _harness_tokens(command: str) -> list[str] | None:
    """Tokens for a command that runs cargo's test harness, else ``None``."""
    if " cargo test " in command or " cargo nextest run " in command:
        return shlex.split(command)
    return None


def _module_filter(tokens: list[str], test_index: int) -> str | None:
    """The single test module a ``--test <category>`` invocation narrows to.

    Handles both runner spellings: nextest's ``-E 'test(/^<module>::/)'``
    expression and plain ``cargo test``'s positional ``<module>::`` substring
    filter. Returns ``None`` when the invocation does not narrow at all.
    """
    if "-E" in tokens:
        index = tokens.index("-E")
        if index + 1 >= len(tokens):
            return None
        match = NEXTEST_MODULE_FILTER.match(tokens[index + 1])
        return match.group("module") if match else None

    for token in tokens[test_index + 2 :]:
        if token == "--":
            break
        if token.startswith("-"):
            continue
        if "::" in token:
            return token.split("::", 1)[0]
    return None


def test_rust_validation_command_targets_exist() -> None:
    contract = _guardrails()

    checked_targets = 0
    for validation in contract["validation_commands"]:
        command = validation["command"]
        tokens = _harness_tokens(command)
        if tokens is None:
            continue

        if "--test" in tokens:
            index = tokens.index("--test")
            assert index + 1 < len(tokens), f"{validation['id']} has a bare --test flag"
            target = tokens[index + 1]
            # soldr#2934 grouped the soldr-cli integration tests into category
            # targets: a directory of module files with a `main.rs` entrypoint,
            # not a single top-level `<target>.rs`.
            main_rs = CLI_TESTS_DIR / target / "main.rs"
            assert main_rs.is_file(), (
                f"{validation['id']} references missing cargo test target: {target} "
                f"({main_rs.relative_to(REPO_ROOT)})"
            )

            # A category target is far wider than the guardrail it stands for,
            # so every command must also name the module it actually owns.
            module = _module_filter(tokens, index)
            assert module is not None, (
                f"{validation['id']} selects the whole `{target}` target; add a "
                "module filter (nextest: -E 'test(/^<module>::/)', cargo test: "
                "a positional `<module>::`) so the guardrail stays targeted"
            )
            module_rs = CLI_TESTS_DIR / target / f"{module}.rs"
            assert module_rs.is_file(), (
                f"{validation['id']} filters on missing test module: {module} "
                f"({module_rs.relative_to(REPO_ROOT)})"
            )
            checked_targets += 1

        if "--lib" in tokens:
            lib_path = REPO_ROOT / "crates" / "soldr-cli" / "src" / "lib.rs"
            assert lib_path.is_file(), (
                f"{validation['id']} requires missing cargo lib target: "
                f"{lib_path.relative_to(REPO_ROOT)}"
            )

    # Keeps the check from silently going vacuous: it previously matched only
    # `cargo test` while every contract command used `cargo nextest run`, so it
    # validated nothing at all.
    assert checked_targets, "no --test guardrail command was validated"


def test_guardrail_docs_cover_every_id_and_command() -> None:
    contract = _guardrails()
    docs = DOCS_PATH.read_text(encoding="utf-8")

    assert "contracts/zccache-integration-guardrails.v1.json" in docs
    assert "issue #543" in docs
    for guardrail in contract["guardrails"]:
        assert f"`{guardrail['id']}`" in docs
    for command in contract["validation_commands"]:
        assert command["command"] in docs


def test_npm_package_includes_guardrail_contract() -> None:
    package = json.loads((REPO_ROOT / "package.json").read_text(encoding="utf-8"))

    assert "contracts/zccache-runtime.v1.json" in package["files"]
    assert "contracts/zccache-integration-guardrails.v1.json" in package["files"]
