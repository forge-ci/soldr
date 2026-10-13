"""zccache and running-process must stay registry dependencies (soldr#2835).

soldr#2837 replaced the `_vender/zccache` and `_vender/running-process`
submodules with exact crates.io dependencies. The measured effect on the
lockfile:

    before (48ca69f7)   22 packages, every one a LOCAL PATH unit
    after  (f95833af)    3 packages, every one registry-sourced

Nineteen packages left the graph entirely. That is the whole point of the
change: a local path unit is owned by the checkout that contains it, so every
fresh Soldr checkout recompiles it, while a registry package can be shared
across checkouts as an external dependency artifact.

None of that was guarded. A `path = ` dep, a `[patch]` redirect, or a
resubmitted gitlink would put the 19 packages back, and the only symptom would
be a build that got slower -- which no test fails on, and which reads as
ordinary CI noise. (The old `verify_vendor_state.py` gate did not cover it
either -- it only enforced discipline while vendoring was active, so it was
inert after soldr#2837 and was deleted as such in soldr#3324.)

The checks are deliberately specific to these two dependency families.
`[patch.crates-io]` is still legitimately used for `notify` and
`filetime_creation` (soldr#2406), and `_vender/` still holds the cap-std
family, so a blanket "no path deps" rule would be wrong.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

# The dependency families this issue externalized. Matched as a substring of
# the package name so the internal crates (`zccache-core`,
# `running-process-platform-internal`, ...) are covered without listing all 22.
EXTERNALIZED = ("zccache", "running-process")


def is_externalized(name: str) -> bool:
    return any(family in name for family in EXTERNALIZED)


def workspace_manifests() -> list[Path]:
    """Every Cargo.toml Soldr owns: the root plus each workspace member.

    `_vender/` is excluded deliberately -- those are vendored third-party
    sources whose own manifests are not Soldr's to constrain.
    """
    manifests = [REPO_ROOT / "Cargo.toml"]
    manifests.extend(sorted((REPO_ROOT / "crates").glob("*/Cargo.toml")))
    return [path for path in manifests if path.is_file()]


@dataclass(frozen=True)
class LockedPackage:
    """A Cargo.lock package; `source` is `None` for a local path."""

    name: str
    source: str | None


def locked_packages() -> list[LockedPackage]:
    """Every package in Cargo.lock."""
    text = (REPO_ROOT / "Cargo.lock").read_text(encoding="utf-8")
    packages: list[LockedPackage] = []
    for block in text.split("[[package]]"):
        name = re.search(r'^name = "([^"]+)"', block, flags=re.MULTILINE)
        if not name:
            continue
        source = re.search(r'^source = "([^"]+)"', block, flags=re.MULTILINE)
        packages.append(
            LockedPackage(name.group(1), source.group(1) if source else None)
        )
    return packages


# ------------------------------ the denominator ------------------------------


def test_the_scan_reaches_the_manifests_and_the_lockfile() -> None:
    """A guard that scans nothing reports clean (soldr#2008).

    Without this, a moved crates/ directory or a renamed lockfile would leave
    every assertion below vacuously true.
    """
    manifests = workspace_manifests()
    assert len(manifests) >= 6, f"expected the root + 5 member crates: {manifests}"

    packages = locked_packages()
    assert len(packages) > 100, f"Cargo.lock looks unparsed: {len(packages)} packages"

    externalized = [pkg.name for pkg in packages if is_externalized(pkg.name)]
    assert externalized, (
        "no zccache/running-process package in Cargo.lock at all -- either the "
        "dependency was dropped or the name match has stopped working, and "
        "every check below would pass without looking at anything"
    )


# ------------------------------- lockfile source -----------------------------


def test_every_externalized_package_comes_from_a_registry() -> None:
    local = [
        pkg.name
        for pkg in locked_packages()
        if is_externalized(pkg.name) and not pkg.source
    ]
    assert not local, (
        "these resolve to a local path, so every checkout compiles its own copy "
        f"instead of sharing a registry artifact (soldr#2835): {sorted(local)}"
    )


def test_no_externalized_package_comes_from_git() -> None:
    """A git dependency is not a path dep, but it is not shareable either.

    #2837 replaced a `running-process` git dep as well as the submodules; a
    revert to `git = ` would satisfy the path check above while reintroducing a
    source cargo cannot share as a published artifact.
    """
    git_sourced = [
        pkg.name
        for pkg in locked_packages()
        if is_externalized(pkg.name) and pkg.source and pkg.source.startswith("git+")
    ]
    assert not git_sourced, (
        f"expected registry sources, found git: {sorted(git_sourced)}"
    )


# --------------------------------- manifests ---------------------------------


@dataclass(frozen=True)
class DependencySpec:
    """One dependency-ish table entry: its name and spec text."""

    name: str
    spec: str


def dependency_specs(manifest_text: str) -> list[DependencySpec]:
    """Every dependency-ish table entry.

    Text-scanned rather than TOML-parsed on purpose: this needs to catch the
    inline form (`zccache = { path = "..." }`), the section form
    (`[dependencies.zccache]`), and `[patch.crates-io]` entries alike, and the
    thing being detected is a substring in all three.
    """
    specs: list[DependencySpec] = []
    section = ""
    for line in manifest_text.splitlines():
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            section = stripped[1:-1]
            continue
        if stripped.startswith("#") or "=" not in stripped:
            continue
        key = stripped.split("=", 1)[0].strip()
        if section.split(".")[-1] and section.startswith(
            (
                "dependencies",
                "dev-dependencies",
                "build-dependencies",
                "patch",
                "workspace",
            )
        ):
            specs.append(DependencySpec(key, stripped))
        # `[dependencies.zccache]` style: the name is in the section header.
        if "." in section and section.rsplit(".", 1)[0].endswith("dependencies"):
            specs.append(DependencySpec(section.rsplit(".", 1)[1], stripped))
    return specs


def test_no_manifest_declares_a_path_dependency_on_them() -> None:
    offenders: list[str] = []
    for manifest in workspace_manifests():
        text = manifest.read_text(encoding="utf-8")
        for dep in dependency_specs(text):
            if (
                is_externalized(dep.name)
                and "path" in dep.spec
                and "_vender" in dep.spec
            ):
                offenders.append(f"{manifest.relative_to(REPO_ROOT)}: {dep.spec}")
    assert not offenders, "path dependencies reintroduced (soldr#2835):\n" + "\n".join(
        offenders
    )


def test_no_patch_redirects_them_into_the_tree() -> None:
    """`[patch.crates-io]` itself stays allowed -- `notify` and
    `filetime_creation` legitimately use it (soldr#2406). Only these two
    families may not."""
    offenders: list[str] = []
    for manifest in workspace_manifests():
        in_patch = False
        for line in manifest.read_text(encoding="utf-8").splitlines():
            stripped = line.strip()
            if stripped.startswith("[") and stripped.endswith("]"):
                in_patch = stripped[1:-1].startswith("patch")
                continue
            if not in_patch or stripped.startswith("#") or "=" not in stripped:
                continue
            name = stripped.split("=", 1)[0].strip()
            if is_externalized(name):
                offenders.append(f"{manifest.relative_to(REPO_ROOT)}: {stripped}")
    assert not offenders, (
        "patched back to a local checkout (soldr#2835):\n" + "\n".join(offenders)
    )


# --------------------------------- gitlinks ----------------------------------


def test_no_submodule_is_declared_for_them() -> None:
    """`.gitmodules` was removed entirely by #2837; if it returns, these two
    may not be in it."""
    gitmodules = REPO_ROOT / ".gitmodules"
    if not gitmodules.is_file():
        return
    text = gitmodules.read_text(encoding="utf-8")
    offenders = [
        family
        for family in EXTERNALIZED
        if f"_vender/{family}" in text or f"path = _vender/{family}" in text
    ]
    assert not offenders, f"submodule gitlink reintroduced (soldr#2835): {offenders}"
