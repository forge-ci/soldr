"""Parity guard for soldr's canonical target contract (issues #1695/#2336)."""

from __future__ import annotations

import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path

from conftest import RELEASE_INCLUDED_TRIPLES, load_script_module

ROOT = Path(__file__).resolve().parents[1]
CONTRACT = ROOT / "ci" / "canonical-targets.json"
CI_WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "auto-release.yml"


def contract_targets() -> list[dict]:
    payload = json.loads(CONTRACT.read_text(encoding="utf-8"))
    assert payload["schema_version"] == 1
    targets = payload["targets"]
    assert len(targets) == 9, "canonical contract must contain exactly nine targets"
    assert len({row["triple"] for row in targets}) == 9, "duplicate canonical triple"
    assert len({row["alias"] for row in targets}) == 9, "duplicate canonical alias"
    return targets


def rust_string_list(path: Path, constant: str) -> list[str]:
    text = path.read_text(encoding="utf-8")
    match = re.search(rf"pub const {constant}[^=]*= &\[(.*?)\];", text, re.DOTALL)
    assert match, f"missing Rust constant {constant} in {path.relative_to(ROOT)}"
    return re.findall(r'"([^"]+)"', match.group(1))


@dataclass(frozen=True, order=True)
class TableRow:
    """One `("key", "value")` row of a Rust tuple table."""

    key: str
    value: str


@dataclass(frozen=True)
class RustTable:
    """A Rust `&[(&str, &str)]` table; a repeated key keeps its last value."""

    rows: tuple[TableRow, ...]

    def get(self, key: str) -> str | None:
        found = None
        for row in self.rows:
            if row.key == key:
                found = row.value
        return found

    def deduplicated(self) -> list[TableRow]:
        return sorted({row.key: row for row in self.rows}.values())


def rust_tuple_table(path: Path, constant: str) -> RustTable:
    text = path.read_text(encoding="utf-8")
    match = re.search(rf"pub const {constant}[^=]*= &\[(.*?)\];", text, re.DOTALL)
    assert match, f"missing Rust table {constant} in {path.relative_to(ROOT)}"
    return RustTable(
        tuple(
            TableRow(key, value)
            for key, value in re.findall(r'\("([^"]+)",\s*"([^"]+)"\)', match.group(1))
        )
    )


def workflow_job(text: str, job: str) -> str:
    match = re.search(
        rf"^  {re.escape(job)}:\s*$(.*?)(?=^  [\w-]+:\s*$|\Z)",
        text,
        re.MULTILINE | re.DOTALL,
    )
    assert match, f"canonical CI job `{job}` is missing"
    return match.group(1)


def test_runtime_and_manifest_mirrors_match_contract() -> None:
    rows = contract_targets()
    expected_triples = [row["triple"] for row in rows]
    expected_aliases = sorted(
        {row["alias"]: TableRow(row["alias"], row["triple"]) for row in rows}.values()
    )

    core = rust_string_list(
        ROOT / "crates" / "soldr-core" / "src" / "core" / "canonical_targets.rs",
        "CANONICAL_TARGETS",
    )
    aliases = rust_tuple_table(
        ROOT / "crates" / "soldr-cli" / "src" / "target_alias.rs",
        "CANONICAL_ALIASES",
    )
    manifest_section = (
        (ROOT / "Cargo.toml")
        .read_text(encoding="utf-8")
        .split("[workspace.metadata.soldr]", 1)[1]
        .split("\n[", 1)[0]
    )
    manifest_targets = re.findall(r'"([^-"\s]+(?:-[^"\s]+){2,})"', manifest_section)

    assert core == expected_triples, (
        "CANONICAL_TARGETS drifted from canonical-targets.json"
    )
    assert aliases.deduplicated() == expected_aliases, (
        "CANONICAL_ALIASES drifted from canonical-targets.json"
    )
    assert manifest_targets == expected_triples, (
        "Cargo target metadata drifted from canonical-targets.json"
    )


def test_release_included_targets_have_execution_or_tracked_exception() -> None:
    """A shipped target may not silently stop at successful cross-compilation."""

    for row in contract_targets():
        if row["release"]["status"] != "included":
            continue
        ci = row["ci"]
        if isinstance(ci.get("run_job"), str):
            continue
        exception = ci.get("execution_exception")
        assert isinstance(exception, dict), (
            f"release-included target {row['triple']} has no execution job or "
            "tracked execution_exception"
        )
        issue = exception.get("issue")
        disposition = exception.get("release_disposition")
        assert isinstance(issue, int) and issue > 0, (
            f"{row['triple']} execution_exception must name a tracking issue"
        )
        assert isinstance(disposition, str) and disposition.strip(), (
            f"{row['triple']} execution_exception must state its release disposition"
        )


def test_ci_and_blessed_alias_workflow_cover_every_target() -> None:
    rows = contract_targets()
    workflow = CI_WORKFLOW.read_text(encoding="utf-8")
    for row in rows:
        ci = row["ci"]
        # Dispatch below falls through to the `cross` arm, where a null
        # `run_job` would blow up inside `workflow_job` with a TypeError
        # rather than a readable assertion. Pin the enum so a typo'd kind
        # fails here, naming itself.
        assert ci["kind"] in {
            "native",
            "cross",
            "cross-build",
        }, f"{ci['build_job']} has unknown ci.kind {ci['kind']!r}"
        build = workflow_job(workflow, ci["build_job"])
        assert row["triple"] in build, (
            f"{ci['build_job']} no longer builds {row['triple']}"
        )
        if ci["kind"] == "native":
            assert "_bootstrap-e2e.yml" in build, (
                f"{ci['build_job']} lost native build/run coverage"
            )
            assert ci["runner"] in build, f"{ci['build_job']} runner drifted"
            continue
        if ci["kind"] == "cross-build":
            # soldr#1978 item 3. A target whose only available runner is the
            # cross-build host itself gets no replay job: the split exists to
            # reach a target-native runner, and there is none to reach. Pin
            # the absence, so the degenerate pair cannot come back unnoticed.
            assert "_ci-cross-build-linux.yml" in build, (
                f"{ci['build_job']} lost Linux cross-build coverage"
            )
            assert ci["run_job"] is None, (
                f"{ci['build_job']} is cross-build but names a run job"
            )
            # The whole justification for dropping the replay is that the
            # contract runner and the cross-build host are the same image. If
            # either side moves, the split becomes non-degenerate again and
            # this target needs its `target-run` back.
            cross_build = (
                ROOT / ".github" / "workflows" / "_ci-cross-build-linux.yml"
            ).read_text(encoding="utf-8")
            # Whole-line match: `runs-on: ubuntu-24.04` is a *prefix* of
            # `runs-on: ubuntu-24.04-arm`, so a substring check would keep
            # passing after the host moved to ARM -- precisely the drift this
            # assertion exists to catch.
            assert re.search(
                rf"^\s*runs-on: {re.escape(ci['runner'])}\s*$",
                cross_build,
                re.MULTILINE,
            ), f"{ci['build_job']} host no longer matches its contract runner"
            # With no replay, compiling the workspace's test binaries for this
            # target is the only remaining target-specific test coverage. It
            # is cheap to delete by accident while "cleaning up the lane with
            # no consumer", so pin that the archive is still built and that
            # only the unread *upload* was dropped.
            assert "upload_test_archive: false" in build, (
                f"{ci['build_job']} still uploads an artifact nothing replays"
            )
            cross_build_archive = re.search(
                r"^\s*- name: Upload artifact\n\s*if: inputs\.upload_test_archive$",
                cross_build,
                re.MULTILINE,
            )
            assert cross_build_archive, (
                "the upload gate vanished from _ci-cross-build-linux.yml; "
                "build-only lanes would resume uploading unread archives"
            )
            assert "nextest archive" in cross_build or "nextest_cmd" in cross_build, (
                f"{ci['build_job']} no longer compiles test binaries for its target"
            )
            replay = ci["build_job"].removesuffix("-build")
            assert not re.search(
                rf"^  {re.escape(replay)}:\s*$", workflow, re.MULTILINE
            ), (
                f"{replay} reappeared: a target-run on {ci['runner']} replays "
                f"{row['triple']} on the image it was built on (soldr#1978 item 3)"
            )
            continue
        run = workflow_job(workflow, ci["run_job"])
        assert "_ci-cross-build-linux.yml" in build, (
            f"{ci['build_job']} lost Linux cross-build coverage"
        )
        assert "_ci-target-run.yml" in run, (
            f"{ci['run_job']} lost native execution coverage"
        )
        # soldr#3018: gated lanes carry `needs: [<build job>, windows-e2e-policy]`,
        # so assert the dependency rather than the scalar spelling of it. The
        # property under test is that the run job is not detached from its
        # build job, which a list satisfies just as well.
        needs_line = next(
            (line for line in run.splitlines() if line.strip().startswith("needs:")),
            "",
        )
        assert ci["build_job"] in needs_line, (
            f"{ci['run_job']} is detached from its build job"
        )
        assert row["triple"] in run and ci["runner"] in run, (
            f"{ci['run_job']} target/runner drifted"
        )
        if ci.get("execution") == "x86_64-recovery":
            assert ci["runner"] == "ubuntu-24.04", (
                f"{row['triple']} Recovery replay must run on an ubuntu-24.04 host "
                "(no macos-* GitHub Actions runner, owner mandate 2026-09-02)"
            )
            assert "target_execution: x86_64-recovery" in run, (
                f"{ci['run_job']} can silently substitute a different execution mode"
            )

    blessed = (ROOT / ".github" / "workflows" / "build-all-from-linux.yml").read_text(
        encoding="utf-8"
    )
    matrix = dict(re.findall(r"\{ alias: ([^,]+),\s+target: ([^,]+),", blessed))
    expected = {row["alias"]: row["triple"] for row in rows}
    assert matrix == expected, (
        "blessed build alias matrix drifted from canonical-targets.json"
    )
    assert '--target "${{ matrix.input || matrix.alias }}"' in blessed, (
        "workflow no longer exercises canonical build inputs through soldr build"
    )
    assert "input: x86_64-pc-windows-gnu" in blessed, (
        "the N-1 bootstrap must use the GNU triple until a released soldr "
        "contains the new canonical alias"
    )


def test_release_inclusions_and_exclusions_match_contract() -> None:
    rows = contract_targets()
    workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")
    # soldr#2469 step 2.2: the inline expected_assets lists were replaced by
    # generation from the contract via release_completeness.py. Both asset
    # gates (prepare + verify_github_release) must call the generator, and
    # no hand-maintained inline asset list may reappear.
    # The prepare-side gate now reaches the generator by importing it, because
    # its whole step moved into release_detect.py. Assert the property (both
    # gates derive from the contract), not the call syntax: checking only the
    # CLI flag would have quietly passed a rewrite that hardcoded the list in
    # Python instead.
    assert "verify_github_release_assets.py" in workflow
    asset_verifier = (
        ROOT / ".github" / "scripts" / "verify_github_release_assets.py"
    ).read_text(encoding="utf-8")
    assert "expected_github_assets" in asset_verifier
    assert "release_completeness.py" in asset_verifier
    detector = (ROOT / ".github" / "scripts" / "release_detect.py").read_text(
        encoding="utf-8"
    )
    assert "expected_github_assets" in detector and (
        "from release_completeness import" in detector
    ), (
        "the prepare-side gate must derive its expected assets from "
        "release_completeness (i.e. from ci/canonical-targets.json), not "
        "from a list of its own"
    )
    assert not re.search(r'"soldr-.*\.tar\.zst"', detector), (
        "a hand-maintained asset list reappeared in release_detect.py; the "
        "contract is the single source (soldr#2469 step 2.2)"
    )
    assert "x86_64-unknown-linux-gnu.tar.zst" not in workflow, (
        "an inline release asset list reappeared in auto-release.yml; the "
        "contract script is the single source (soldr#2469 step 2.2)"
    )
    for row in rows:
        release = row["release"]
        if release["status"] == "documented-exclusion":
            assert release["workflow_marker"] in workflow, (
                f"release exclusion for {row['triple']} is undocumented"
            )


def test_catalogue_mappings_cover_every_target() -> None:
    rows = contract_targets()
    script = ROOT / ".github" / "scripts" / "fetch_catalogued_nextest.py"
    sys.path.insert(0, str(script.parent))
    module = load_script_module(script, "fetch_catalogued_nextest")

    for row in rows:
        expected = module.CatalogueQuery(*row["catalogue"]["nextest"])
        assert module.query_for_target(row["triple"]) == expected, (
            f"cargo-nextest mapping drifted for {row['triple']}"
        )

    syslib_tables = {
        "bzip2_sysroot.rs": "BZIP2_TARGETS",
        "lzma_sysroot.rs": "LZMA_TARGETS",
        "mimalloc_sysroot.rs": "MIMALLOC_TARGETS",
        "python_sysroot.rs": "PYTHON_SYSROOT_TARGETS",
        "sqlite_sysroot.rs": "SQLITE_TARGETS",
        "uv_tool.rs": "UV_TOOL_HOSTS",
        "zlib_ng_sysroot.rs": "ZLIB_NG_TARGETS",
        "zstd_sysroot.rs": "ZSTD_TARGETS",
    }
    fetch = ROOT / "crates" / "soldr-fetch" / "src" / "fetch"
    for filename, constant in syslib_tables.items():
        mapping = rust_tuple_table(fetch / filename, constant)
        for row in rows:
            exclusions = set(row["catalogue"].get("syslib_exclusions", []))
            if filename in exclusions:
                assert mapping.get(row["triple"]) is None, (
                    f"{constant} unexpectedly covers explicitly excluded "
                    f"target {row['triple']}"
                )
                continue
            expected_slug = (
                row["catalogue"].get("host_tool_slug", row["catalogue"]["syslib_slug"])
                if filename == "uv_tool.rs"
                else row["catalogue"]["syslib_slug"]
            )
            assert mapping.get(row["triple"]) == expected_slug, (
                f"{constant} mapping drifted for {row['triple']}"
            )


def test_documented_alias_table_matches_contract() -> None:
    docs = (ROOT / "docs" / "CROSS_COMPILE.md").read_text(encoding="utf-8")
    table = docs.split("<!-- canonical-target-contract:start -->", 1)[1].split(
        "<!-- canonical-target-contract:end -->", 1
    )[0]
    for row in contract_targets():
        assert f"| `{row['alias']}` | `{row['triple']}` |" in table, (
            f"documentation row missing for {row['alias']} -> {row['triple']}"
        )


def test_npm_install_selectors_match_contract() -> None:
    """soldr#2469 step 2.1: the npm installer's platform->triple map must
    cover exactly the release-included contract targets. PR #2455 shrank the
    matrix, canonical targets, and npm selectors together, so nothing failed
    while five targets vanished -- this cross-consumer check is the guard
    that pattern was missing (install.js is a consumer the other tests in
    this file do not read)."""
    contract = json.loads(CONTRACT.read_text(encoding="utf-8"))
    included = {
        entry["triple"]
        for entry in contract["targets"]
        if entry["release"]["status"] == "included"
    }
    install_js = (ROOT / "scripts" / "install.js").read_text(encoding="utf-8")
    selector_triples = set(
        re.findall(r'triple:\s*"([a-z0-9_]+-[a-z0-9-]+)"', install_js)
    )
    assert selector_triples == included, (
        "scripts/install.js platform selectors drifted from "
        f"canonical-targets.json: selectors={sorted(selector_triples)} "
        f"contract={sorted(included)}"
    )


def test_release_asset_expectation_matches_contract() -> None:
    """soldr#2469 step 2.1: the full public release surface for N included
    targets is N archives + N wheels + SHA256SUMS. Pin the arithmetic to the
    contract so a target removal shows up here as well as in the matrix (a
    removal must be an explicit reviewed decision, never a side effect)."""
    contract = json.loads(CONTRACT.read_text(encoding="utf-8"))
    included = [
        entry
        for entry in contract["targets"]
        if entry["release"]["status"] == "included"
    ]
    expected_assets = 2 * len(included) + 1  # archives + wheels + SHA256SUMS
    assert len(included) == 8 and expected_assets == 17, (
        "the supported release surface changed size "
        f"({len(included)} targets -> {expected_assets} public assets). If "
        "this is intentional it needs an explicit compatibility decision in "
        "canonical-targets.json reviewed on its own (soldr#2469 step 2.1), "
        "plus updates to every consumer this suite checks."
    )


def test_release_build_matrix_is_generated_from_the_contract() -> None:
    """soldr#2469 step 2.1: auto-release.yml's build matrix is generated
    from ci/canonical-targets.json, not hand-inlined. The hand-inlined
    matrix is exactly what let PR #2455 shrink the matrix and the contract
    together with nothing failing."""
    script = ROOT / ".github" / "scripts" / "release_completeness.py"
    module = load_script_module(script, "release_completeness_matrix")
    matrix = module.build_matrix()
    contract = json.loads(CONTRACT.read_text(encoding="utf-8"))
    included = [
        entry
        for entry in contract["targets"]
        if entry["release"]["status"] == "included"
    ]

    assert [row.target for row in matrix] == [e["triple"] for e in included]
    assert len({row.name for row in matrix}) == len(matrix)
    for row in matrix:
        assert set(vars(row)) == {
            "name",
            "runner",
            "target",
            "setup_target",
            "binary",
        }

    by_target = {row.target: row for row in matrix}
    # The two structural exceptions the old inline matrix encoded in
    # comments: ARM64 musl builds natively (its catalogue compiler is
    # i386-hosted) and Windows targets build as Linux cross lanes.
    arm_musl = by_target["aarch64-unknown-linux-musl"]
    assert arm_musl.runner == "ubuntu-24.04-arm"
    assert arm_musl.setup_target == ""
    for triple in ("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"):
        assert by_target[triple].runner == "ubuntu-24.04"
        assert by_target[triple].binary == "soldr.exe"


def test_release_workflow_consumes_the_generated_matrix() -> None:
    workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")
    assert "include: ${{ fromJSON(needs.prepare.outputs.build_matrix) }}" in workflow
    assert "--build-matrix" in workflow
    # No hand-inlined matrix entry may reappear.
    assert "- name: Linux x64 (glibc)" not in workflow
    assert workflow.count("setup_target: x86_64-unknown-linux-gnu") == 0


def test_release_workflow_calls_the_asset_generator_correctly() -> None:
    """The asset-list generator requires --version; a positional tag after
    the boolean flag is an argparse error that would kill the release run
    at prepare (found latent 2026-08-16)."""
    workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")
    for line in workflow.splitlines():
        if "--list-expected-github-assets" in line:
            assert re.search(
                r'--version "\$(?:version|tag)" --list-expected-github-assets\s*\)"$',
                line.strip(),
            ), f"malformed release_completeness.py invocation: {line.strip()}"


def test_target_removal_requires_a_compatibility_decision() -> None:
    """soldr#2469 step 2.1: removing a target relative to the previous
    supported release must be an explicitly reviewed decision recorded in
    the contract file — never reachable as a side effect. The baseline is
    the v0.9.1 supported set; when a future release intentionally drops a
    target, add a compatibility_decisions entry naming it (and update the
    baseline here in the same reviewed PR)."""
    baseline_v0_9_1 = set(RELEASE_INCLUDED_TRIPLES)
    contract = json.loads(CONTRACT.read_text(encoding="utf-8"))
    assert "compatibility_decisions" in contract, (
        "canonical-targets.json must carry the compatibility_decisions "
        "list (soldr#2469 step 2.1)"
    )
    included = {
        entry["triple"]
        for entry in contract["targets"]
        if entry["release"]["status"] == "included"
    }
    decided = {decision["triple"] for decision in contract["compatibility_decisions"]}
    for decision in contract["compatibility_decisions"]:
        assert {"triple", "decision", "reference"} <= set(decision), (
            "each compatibility decision needs at least triple + decision + "
            f"reference (a PR/issue): {decision}"
        )
    removed_without_decision = baseline_v0_9_1 - included - decided
    assert not removed_without_decision, (
        "targets removed from the supported release set without an explicit "
        "compatibility_decisions entry in canonical-targets.json: "
        f"{sorted(removed_without_decision)} (soldr#2469 step 2.1)"
    )
