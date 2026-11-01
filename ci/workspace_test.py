#!/usr/bin/env python3
"""Run soldr's workspace tests after handing the cache route to source Soldr.

The published Soldr bootstrap exists only to compile the checkout.  Source
integration tests execute ``target/debug/soldr`` and must not inherit the
bootstrap daemon route: their wire protocol may legitimately differ.

Two environments run this plan:

* ``--host`` -- the local gate's ``tests`` lane (owner directive 2026-10-16:
  no bosn). The suite starts soldr daemons and brokers and touches soldr
  state roots (soldr#3516), so the host run first moves every per-user
  identity variable the broker and daemon resolve a root from -- ``HOME``,
  ``XDG_CONFIG_HOME``, ``XDG_RUNTIME_DIR``, ``TMPDIR`` and ``SOLDR_CACHE_DIR``
  (``broker_spawn.rs`` ``HOME_IDENTITY_ENV_VARS``) -- into a private root
  under this worktree's git dir. Only then does it set ``SOLDR_TEST_ISOLATED``,
  and only for its own children. The bootstrap's ``cache shutdown`` and
  ``broker remove`` therefore reach the private broker, never the caller's
  live one. It fails closed: it refuses to start when the private root would
  be the caller's real home, or when the nextest guard no longer refuses an
  unmarked run.
* default -- the legacy container layout (``/repo``, ``/target``,
  ``/opt/soldr-bootstrap``), whose image already isolates and sets the marker.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
# pylint: disable-next=wrong-import-position
from crate_graph import (  # noqa: E402 -- script-relative import precedes this
    changed_paths,
    reverse_dependency_closure,
    workspace_crates,
)


@dataclass(frozen=True)
class EnvVar:
    name: str
    value: str


@dataclass(frozen=True)
class Step:
    argv: list[str]
    env: tuple[EnvVar, ...]
    # Variables removed from the inherited environment (the image's
    # CARGO_BUILD_JOBS cap, which CI's ci-test lane also unsets).
    unset: tuple[str, ...] = ()
    # Prefixes removed likewise: the image's dev-loop CARGO_PROFILE_*
    # overrides (release at opt-level 0, incremental) make a build differ
    # from CI's -- a Dylint library built that way lost its `dylint_version`
    # export (zackees/ci.yml#172).
    unset_prefixes: tuple[str, ...] = ()


@dataclass(frozen=True)
class WorkspaceTestPlan:
    """The handoff, by phase rather than by position (soldr#3454).

    ``main`` runs ``setup`` unconditionally, then ``source_start`` and
    ``validation`` under a guard that always runs ``teardown``. Naming the
    phases keeps an added step from silently shifting which one is guarded.
    """

    setup: tuple[Step, ...]
    source_start: Step
    validation: Step
    teardown: tuple[Step, ...]

    @property
    def steps(self) -> list[Step]:
        return [*self.setup, self.source_start, self.validation, *self.teardown]


# soldr#3731: soldr-cli's guard tests read the whole workspace tree, so a
# change to any crate can reach them; it is always selected.
ALWAYS_SELECTED = ("soldr-cli",)
# Paths inside a crate that are test infrastructure or a dependency edge
# rather than crate source: a change to one runs the full suite.
CRATE_FULL_RUN_PARTS = ("tests", "Cargo.toml")


@dataclass(frozen=True)
class Selection:
    """The packages to test, or None for the whole workspace, and why."""

    packages: tuple[str, ...] | None
    reason: str


def select_packages(root: Path, changed: list[str] | None) -> Selection:
    """Narrow the suite to the reverse-dependency closure of the changed
    crates only on proof (soldr#3731); anything uncertain runs it all.

    A crate's test binaries are built from it and its path-dependency
    closure, against an unchanged Cargo.lock. If every changed path is crate
    source, a crate outside the changed crates' reverse closure builds the
    same test binaries as on the base, so its tests cannot change outcome.
    """
    full = "tests lane: FULL workspace"
    if not changed:
        return Selection(None, f"{full} -- change set unknown or empty")
    try:
        crates = {c.name for c in workspace_crates(root)}
    except (OSError, ValueError, tomllib.TOMLDecodeError) as error:
        return Selection(None, f"{full} -- crate graph unreadable ({error})")
    touched: set[str] = set()
    for path in changed:
        parts = path.split("/")
        if (
            len(parts) < 3
            or parts[0] != "crates"
            or parts[1] not in crates
            or parts[2] in CRATE_FULL_RUN_PARTS
        ):
            return Selection(None, f"{full} -- {path} is not crate source")
        touched.add(parts[1])
    selected = reverse_dependency_closure(root, tuple(sorted(touched)))
    packages = tuple(sorted(selected | set(ALWAYS_SELECTED)))
    skipped = sorted(crates - set(packages))
    return Selection(
        packages,
        f"tests lane: SELECTED {list(packages)} (soldr#3731) -- changes touch only "
        f"{sorted(touched)}; skipped {skipped}, outside their reverse-dependency "
        "closure",
    )


def container_path(path: Path) -> str:
    """Render a Linux container path even when the contract test runs on Windows."""
    return path.as_posix()


def workspace_test_plan(
    *, target: Path, bootstrap: Path, packages: tuple[str, ...] | None = None
) -> WorkspaceTestPlan:
    """Return the ordered bootstrap-to-source validation handoff.

    ``packages`` narrows both test stages (soldr#3731); None is --workspace.
    """
    scope = " ".join(f"-p {p}" for p in packages) if packages else "--workspace"
    source = target / "debug" / "soldr"
    source_text = container_path(source)
    wrapper_text = container_path(target / "debug" / "soldr-nextest-wrapper")
    base_env = (EnvVar("CARGO_TARGET_DIR", container_path(target)),)
    source_unset = ("CARGO_BUILD_JOBS", "SOLDR_JOBS")
    source_unset_prefixes = ("CARGO_PROFILE_",)
    return WorkspaceTestPlan(
        setup=(
            Step(
                [
                    container_path(bootstrap),
                    "cargo",
                    "build",
                    "-p",
                    "soldr-cli",
                    "--bin",
                    "soldr",
                    "-p",
                    "soldr-nextest-wrapper",
                    "--bin",
                    "soldr-nextest-wrapper",
                ],
                base_env,
            ),
            # soldr#3454: the black-box suites of the Nextest run-wrapper (the
            # only implementation is the native binary built above). The env
            # var is always set here, so the suites really run instead of
            # skipping for want of a binary. Needs no daemon.
            Step(
                [
                    "uv",
                    "run",
                    "--no-project",
                    "--with",
                    "pytest>=8.0",
                    "python",
                    "-m",
                    "pytest",
                    "-q",
                    "tests/test_nextest_timeout_wrapper.py",
                    "tests/test_nextest_memory_guard.py",
                ],
                (EnvVar("SOLDR_NEXTEST_WRAPPER_UNDER_TEST", wrapper_text),),
            ),
            Step(
                [
                    container_path(bootstrap),
                    "cache",
                    "shutdown",
                    "--shutdown-timeout-seconds",
                    "30",
                ],
                (),
            ),
            Step([container_path(bootstrap), "broker", "remove"], ()),
        ),
        # The daemon fixes its admission limit at startup. Remove the image's
        # dev-loop overrides here as well as for Cargo, or the source route
        # keeps the image's two-compile ceiling for the whole validation run.
        source_start=Step(
            [source_text, "daemon", "start"],
            (),
            unset=source_unset,
            unset_prefixes=source_unset_prefixes,
        ),
        # zackees/ci.yml#168/#172: exactly the two *test* stages of the frozen
        # DAG CI's build-linux-x64 lane runs (`soldr ci-test --explain-plan`:
        # `nextest` and `doctests`). Its lint stages -- rustfmt, Clippy,
        # Dylint, dependency policy -- are host-safe and run on the host in
        # the local gate's `rust` lane (`soldr lint rust`, `soldr lint deps`);
        # only the tests start soldr daemons, so only they need isolation.
        # Doctests first: nextest's integration tests relink target/debug/soldr
        # (the bin under the test feature set) in place, after which a later
        # compile runs through a different soldr image than the daemon
        # started above, which refuses it ("root ownership is busy ... a
        # different Soldr version or daemon image").
        # nextest, not a bare `cargo test --workspace`: libtest's shared
        # process let one panicking test poison a crate-wide env lock and fail
        # six unrelated tests, which nextest's process-per-test model cannot.
        validation=Step(
            [
                "sh",
                "-c",
                f'"{source_text}" cargo test {scope} --doc'
                f' && "{source_text}" cargo nextest run --no-fail-fast {scope} --lib --tests',
            ],
            base_env,
            unset=source_unset,
            unset_prefixes=source_unset_prefixes,
        ),
        teardown=(
            Step(
                [source_text, "cache", "shutdown", "--shutdown-timeout-seconds", "30"],
                (),
            ),
            Step([source_text, "broker", "remove"], ()),
        ),
    )


def run_step(step: Step, *, repo: Path) -> None:
    env = os.environ.copy()
    for name in step.unset:
        env.pop(name, None)
    for name in [n for n in env if n.startswith(step.unset_prefixes)]:
        env.pop(name)
    env.update((var.name, var.value) for var in step.env)
    subprocess.run(step.argv, cwd=repo, env=env, check=True)


def cleanup(steps: list[Step], *, repo: Path, preserve_primary: bool) -> None:
    """Run all source-route cleanup steps without hiding a test failure."""
    failures: list[subprocess.CalledProcessError | OSError] = []
    for step in steps:
        try:
            run_step(step, repo=repo)
        except (subprocess.CalledProcessError, OSError) as error:
            failures.append(error)
            print(f"cleanup failed: {' '.join(step.argv)}", file=sys.stderr)
    if failures and not preserve_primary:
        raise failures[0]


ISOLATION_MARKER = "SOLDR_TEST_ISOLATED"
GUARD = Path(".github/scripts/nextest_wrapper.sh")
# nextest_wrapper.sh's exit code for an unmarked, non-CI run.
GUARD_REFUSAL = 97


class IsolationError(RuntimeError):
    """The host run cannot prove it is isolated, so it does not start."""


@dataclass(frozen=True)
class HostLayout:
    home: Path
    target: Path
    tmp: Path
    runtime: Path


def host_layout(git_dir: Path) -> HostLayout:
    """A private root per worktree (its own git dir), kept between runs so
    the target dir and the private soldr cache stay warm. The runtime dir
    holds the broker socket, so it is short and under /tmp (sun_path)."""
    root = git_dir.resolve() / "local-gate" / "tests-host"
    tag = hashlib.sha256(str(root).encode()).hexdigest()[:10]
    return HostLayout(
        home=root / "home",
        target=root / "target",
        tmp=root / "tmp",
        runtime=Path(f"/tmp/soldr-gate-{os.getuid()}-{tag}"),
    )


def host_environment(
    layout: HostLayout, caller: dict[str, str], real_home: Path
) -> dict[str, str]:
    """The child environment: private identity roots, the caller's
    toolchain and uv caches (read-mostly, outside soldr's state), and the
    isolation marker last."""
    if layout.home.resolve() == real_home.resolve():
        raise IsolationError(f"private HOME {layout.home} is the caller's home")
    env = {
        k: v
        for k, v in caller.items()
        if k not in ("CI", ISOLATION_MARKER, "SOLDR_RUSTC_WRAPPER")
    }
    env.update(
        {
            "HOME": str(layout.home),
            "XDG_CONFIG_HOME": str(layout.home / ".config"),
            "XDG_RUNTIME_DIR": str(layout.runtime),
            "TMPDIR": str(layout.tmp),
            "SOLDR_CACHE_DIR": str(layout.home / ".soldr" / "bootstrap"),
            "CARGO_HOME": caller.get("CARGO_HOME") or str(real_home / ".cargo"),
            "RUSTUP_HOME": caller.get("RUSTUP_HOME") or str(real_home / ".rustup"),
            "UV_CACHE_DIR": caller.get("UV_CACHE_DIR")
            or str(real_home / ".cache" / "uv"),
            "UV_PYTHON_INSTALL_DIR": caller.get("UV_PYTHON_INSTALL_DIR")
            or str(real_home / ".local" / "share" / "uv" / "python"),
        }
    )
    gitconfig = real_home / ".gitconfig"
    if "GIT_CONFIG_GLOBAL" not in env and gitconfig.is_file():
        env["GIT_CONFIG_GLOBAL"] = str(gitconfig)
    soldr_root = real_home / ".soldr"
    for name in ("HOME", "SOLDR_CACHE_DIR", "XDG_CONFIG_HOME", "TMPDIR"):
        value = Path(env[name]).resolve()
        if value == real_home.resolve() or value.is_relative_to(soldr_root):
            raise IsolationError(f"{name}={value} reaches the caller's soldr state")
    env[ISOLATION_MARKER] = "1"
    return env


def assert_guard_refuses(repo: Path, caller: dict[str, str]) -> None:
    """Fail closed unless the nextest guard still refuses an unmarked run:
    the marker is only meaningful while the guard checks it."""
    env = {k: v for k, v in caller.items() if k not in ("CI", ISOLATION_MARKER)}
    env["SOLDR_NEXTEST_NATIVE_WRAPPER"] = "/bin/true"
    code = subprocess.run(
        ["sh", str(repo / GUARD), "true"], env=env, check=False
    ).returncode
    if code != GUARD_REFUSAL:
        raise IsolationError(
            f"{GUARD} exited {code} without {ISOLATION_MARKER}, expected "
            f"{GUARD_REFUSAL}: the isolation guard is not in force"
        )


def prepare_host(repo: Path) -> tuple[Path, Path]:
    """Isolate this process's environment; return (target, bootstrap)."""
    caller = dict(os.environ)
    bootstrap = shutil.which("soldr", path=caller.get("PATH"))
    if bootstrap is None:
        raise IsolationError("no `soldr` on PATH to bootstrap the checkout")
    git_dir = subprocess.run(
        ["git", "rev-parse", "--absolute-git-dir"],
        cwd=repo,
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    ).stdout.strip()
    layout = host_layout(Path(git_dir))
    assert_guard_refuses(repo, caller)
    env = host_environment(layout, caller, Path.home())
    for path in (layout.home, layout.target, layout.tmp):
        path.mkdir(parents=True, exist_ok=True)
    layout.runtime.mkdir(mode=0o700, exist_ok=True)
    os.chmod(layout.runtime, 0o700)
    os.environ.clear()
    os.environ.update(env)
    print(
        f"tests lane: host run, private HOME={layout.home}, "
        f"XDG_RUNTIME_DIR={layout.runtime}, {ISOLATION_MARKER}=1 for children only",
        flush=True,
    )
    return layout.target, Path(bootstrap).resolve()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path("/repo"))
    parser.add_argument("--target", type=Path, default=Path("/target"))
    parser.add_argument(
        "--bootstrap", type=Path, default=Path("/opt/soldr-bootstrap/bin/soldr")
    )
    parser.add_argument(
        "--host",
        action="store_true",
        help="run on this host from the current checkout, in a private soldr root",
    )
    args = parser.parse_args(argv)
    if args.host:
        args.repo = Path.cwd()
        try:
            args.target, args.bootstrap = prepare_host(args.repo)
        except IsolationError as error:
            print(f"tests lane: REFUSED -- {error}", file=sys.stderr, flush=True)
            return 2
    # zackees/ci.yml#196: echo the gate's nonce from the tree this run tests,
    # so the gate can prove it tested the worktree that invoked it.
    nonce = args.repo / ".gate-nonce"
    seen = nonce.read_text(encoding="utf-8").strip() if nonce.is_file() else "(none)"
    print(f"gate-nonce: {seen}", flush=True)

    selection = select_packages(args.repo, changed_paths(args.repo))
    print(selection.reason, flush=True)
    plan = workspace_test_plan(
        target=args.target, bootstrap=args.bootstrap, packages=selection.packages
    )
    for step in plan.setup:
        run_step(step, repo=args.repo.resolve())
    try:
        run_step(plan.source_start, repo=args.repo.resolve())
        run_step(plan.validation, repo=args.repo.resolve())
    except BaseException:
        cleanup(list(plan.teardown), repo=args.repo.resolve(), preserve_primary=True)
        raise
    cleanup(list(plan.teardown), repo=args.repo.resolve(), preserve_primary=False)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
