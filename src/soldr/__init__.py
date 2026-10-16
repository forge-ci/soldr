"""soldr — Instant tools. Instant builds.

PEP 517 build backend that delegates to a managed maturin binary.

Set in your pyproject.toml::

    [build-system]
    requires = ["soldr"]
    build-backend = "soldr"

soldr fetches a pinned maturin from GitHub Releases on first build
(cached under ~/.soldr/bin/) and delegates to it via
``soldr maturin pep517 <hook>``. Builds run with ``RUSTC_WRAPPER=soldr``
and ``ZCCACHE_PATH_REMAP=auto`` so rustc invocations are cached and
git-worktree caches share via path normalization.

``requires = ["soldr"]`` alone is sufficient — there is no separate
``maturin`` Python dependency to add, because soldr handles maturin
acquisition itself.
"""

import codecs
import hashlib
import importlib
import importlib.util
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import sysconfig
import threading
import time
from contextlib import contextmanager
from pathlib import Path
from typing import Any, BinaryIO, Iterator, Mapping, Optional, TextIO

if __package__ == "soldr":
    from ._process import run_captured
else:
    # Backend fixtures load this file directly under an isolated module name.
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
    run_captured = importlib.import_module("soldr._process").run_captured

_FAST_PROFILE_ENV = "SOLDR_PEP517_PROFILE"
_STATS_ENV = "SOLDR_PEP517_STATS"
_WHEEL_CACHE_ENV = "SOLDR_PEP517_WHEEL_CACHE"
_DISABLE_PROFILE_VALUES = {"", "none", "default", "off", "false", "0"}
_DELEGATE_BACKEND_SECTION = "tool.soldr.pep517"
_PEP517_ENV_KEYS = {
    "RUSTC_WRAPPER",
    "ZCCACHE_PATH_REMAP",
    "ZCCACHE_STAGED_ARTIFACTS",
    "SOLDR_PEP517_LINKER",
    "SOLDR_PEP517_PROJECT_ID",
    "CARGO_TARGET_DIR",
    "CARGO_PROFILE_DEV_OPT_LEVEL",
    "CARGO_PROFILE_DEV_CODEGEN_UNITS",
    "CARGO_PROFILE_DEV_DEBUG",
    "CARGO_PROFILE_DEV_LTO",
    "CARGO_PROFILE_DEV_INCREMENTAL",
    "CARGO_BUILD_TARGET",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTFLAGS",
    "SOLDR_CACHE_DIR",
    "SOLDR_PEP517_PROFILE",
    "SOLDR_LINKER",
}
_PEP517_TARGET_SCHEMA = b"pep517-target-v3"
_WHEEL_CACHE_SCHEMA = b"pep517-wheel-cache-v1"
_WHEEL_CACHE_IGNORED_DIRECTORIES = {
    "build",
    ".git",
    ".hg",
    ".mypy_cache",
    ".nox",
    ".pytest_cache",
    ".ruff_cache",
    ".svn",
    ".tox",
    ".venv",
    "__pycache__",
    "node_modules",
    "target",
    "venv",
}
_WHEEL_CACHE_IGNORED_DIRECTORY_SUFFIXES = (".dist-info", ".egg-info")
_WHEEL_CACHE_IGNORED_RELATIVE_DIRECTORIES = {
    ".claude/worktrees",
    ".claude/workspaces",
    ".codex/worktrees",
    ".clud",
}
_SOLDR_ROOT_CACHE: "dict[tuple[str, ...], Path]" = {}
_SOLDR_ROOT_CACHE_LOCK = threading.Lock()
_FAST_DEV_PROFILE_DEFAULTS = {
    "CARGO_PROFILE_DEV_OPT_LEVEL": ("opt-level", "0"),
    "CARGO_PROFILE_DEV_CODEGEN_UNITS": ("codegen-units", "256"),
    "CARGO_PROFILE_DEV_DEBUG": ("debug", "line-tables-only"),
    "CARGO_PROFILE_DEV_LTO": ("lto", "false"),
    "CARGO_PROFILE_DEV_INCREMENTAL": ("incremental", "true"),
}


def _project_root() -> Path:
    """Find the project root used by the PEP 517 invocation.

    PEP 517 frontends invoke the backend from the source tree, while the
    backend package itself lives in the isolated build environment. Walking
    from the current directory therefore works for both an in-tree backend
    test and an installed soldr wheel.
    """
    current = Path.cwd().resolve()
    for directory in (current, *current.parents):
        if (directory / "pyproject.toml").is_file():
            return directory
    return current


def _strip_toml_comment(line: str) -> str:
    quote = None
    for index, char in enumerate(line):
        if quote:
            if char == quote:
                quote = None
        elif char in "\"'":
            quote = char
        elif char == "#":
            return line[:index]
    return line


def _toml_section_values(path: Path, section: str) -> "dict[str, str]":  # noqa: C901
    """Read the small TOML subset needed before Python 3.11's tomllib.

    The backend must remain dependency-free in an isolated build environment.
    Use tomllib when available and a deliberately narrow fallback parser on
    Python 3.10. Any present key is recorded, with bare values (integers and
    the like) kept verbatim. A malformed or unreadable optional config is ignored here;
    maturin/Cargo remains responsible for reporting the authoritative TOML
    error during the build.
    """
    try:
        # pylint: disable=import-outside-toplevel  # 3.10 has no tomllib
        import tomllib  # type: ignore[import-not-found]

        with path.open("rb") as stream:
            document = tomllib.load(stream)
        value: Any = document
        for component in section.split("."):
            if not isinstance(value, dict):
                return {}
            value = value.get(component)
        if not isinstance(value, dict):
            return {}
        return {
            str(key): str(item)
            for key, item in value.items()
            if isinstance(item, (str, int, float, bool))
        }
    except (ImportError, OSError, ValueError):
        pass

    values: dict[str, str] = {}
    active = False
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError:
        return values
    for line in lines:
        stripped = _strip_toml_comment(line).strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            active = stripped[1:-1].strip() == section
            continue
        if not active or "=" not in stripped:
            continue
        key, raw = (part.strip() for part in stripped.split("=", 1))
        if not re.fullmatch(r"[A-Za-z0-9_-]+", key):
            continue
        match = re.fullmatch(r"[\"'](.*)[\"']", raw)
        if match:
            values[key] = match.group(1)
        elif raw in {"true", "false"}:
            values[key] = raw
        elif raw:
            values[key] = raw
    return values


def _project_maturin_options() -> "dict[str, str]":
    return _toml_section_values(_project_root() / "pyproject.toml", "tool.maturin")


def _project_dev_profile_options() -> "dict[str, str]":
    return _toml_section_values(_project_root() / "Cargo.toml", "profile.dev")


def _project_soldr_options() -> "dict[str, str]":
    return _toml_section_values(
        _project_root() / "pyproject.toml", _DELEGATE_BACKEND_SECTION
    )


def _delegate_backend_name() -> Optional[str]:
    value = _project_soldr_options().get("delegate-backend")
    return value.strip() if value and value.strip() else None


def _delegate_backend() -> object | None:
    name = _delegate_backend_name()
    if not name:
        return None
    if _project_bundle_bins():
        raise RuntimeError(
            "soldr PEP 517 bundle-bins is applied by soldr's native maturin build; "
            "remove delegate-backend or stage the bins inside the delegate"
        )
    module_name, separator, attribute = name.partition(":")
    if module_name == "soldr" or module_name.startswith("soldr."):
        raise RuntimeError(
            "soldr PEP 517 delegate-backend cannot delegate back to soldr; "
            "select a concrete backend such as setuptools.build_meta"
        )
    backend = importlib.import_module(module_name)
    if separator and attribute:
        return getattr(backend, attribute)
    if separator:
        raise RuntimeError(f"invalid soldr delegate-backend `{name}`")
    return backend


@contextmanager
def _managed_pep517_environment(
    config_settings: Optional[dict] = None,
    *,
    editable: bool = False,
) -> Iterator[None]:
    """Apply soldr's child-build environment around a delegated backend."""
    prepared = _prep_env(config_settings, editable=editable)
    previous: dict[str, str | None] = {
        key: os.environ.get(key) for key in _PEP517_ENV_KEYS
    }
    try:
        for key in _PEP517_ENV_KEYS:
            value = prepared.get(key)
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
        yield
    finally:
        for key, previous_value in previous.items():
            if previous_value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = previous_value


@contextmanager
def _hold_build_lease(environment: dict[str, str]) -> Iterator[None]:
    """Hold Soldr's OS-backed root lease for an in-process delegate hook.

    The helper owns the actual file lock and reads a private stdin pipe. If
    this Python backend is killed, the pipe closes and the helper exits, so a
    long-lived daemon can never inherit an immortal lease.
    """
    process = subprocess.Popen(
        ["soldr", "gc", "hold-build-lease"],
        env=environment,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    assert process.stdin is not None
    assert process.stdout is not None
    assert process.stderr is not None
    ready = process.stdout.readline()
    if ready != b"ready\n":
        error = process.stderr.read().decode("utf-8", errors="replace").strip()
        process.wait()
        raise RuntimeError(f"soldr build lease helper failed to start: {error}")
    try:
        yield
    finally:
        process.stdin.close()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def _delegate_hook(
    name: str,
    *args: object,
    fallback: Optional[str] = None,
    _config_settings: Optional[dict] = None,
    **kwargs: object,
):
    backend = _delegate_backend()
    if backend is None:
        return None
    hook_name = name if hasattr(backend, name) else fallback
    if hook_name is None or not hasattr(backend, hook_name):
        return [] if name.startswith("get_requires_for_build_") else None
    with _managed_pep517_environment(
        _config_settings,
        editable=name.endswith("_editable"),
    ):
        with _hold_build_lease(dict(os.environ)):
            return getattr(backend, hook_name)(*args, **kwargs)


def _hash_identity_field(hasher: "hashlib._Hash", name: str, value: bytes) -> None:
    name_bytes = name.encode("utf-8")
    hasher.update(len(name_bytes).to_bytes(8, "little"))
    hasher.update(name_bytes)
    hasher.update(len(value).to_bytes(8, "little"))
    hasher.update(value)


def _project_build_identity(environment: Optional[dict[str, str]] = None) -> str:
    """Return a path-independent identity for the PEP build configuration.

    This selects a stable target namespace; it is not a replacement for
    Cargo's fingerprints. Rust source changes intentionally keep the same
    namespace so Cargo can reuse valid artifacts and decide what is stale.
    """
    root = _project_root()
    hasher = hashlib.sha256()
    _hash_identity_field(hasher, "schema", _PEP517_TARGET_SCHEMA)
    configuration_files = [
        "pyproject.toml",
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "rust-toolchain",
        ".cargo/config.toml",
        ".cargo/config",
    ]
    configuration_files.extend(
        str(path.relative_to(root)).replace("\\", "/")
        for path in sorted(root.glob("crates/**/Cargo.toml"))
    )
    for relative in configuration_files:
        path = root / relative
        try:
            contents = path.read_bytes()
        except OSError:
            continue
        _hash_identity_field(hasher, relative, contents)
    values = os.environ if environment is None else environment
    for name in (
        "CARGO_BUILD_TARGET",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "SOLDR_PEP517_PROFILE",
        "SOLDR_PEP517_LINKER",
        "SOLDR_LINKER",
        "CARGO_PROFILE_DEV_OPT_LEVEL",
        "CARGO_PROFILE_DEV_CODEGEN_UNITS",
        "CARGO_PROFILE_DEV_DEBUG",
        "CARGO_PROFILE_DEV_LTO",
        "CARGO_PROFILE_DEV_INCREMENTAL",
    ):
        _hash_identity_field(hasher, name, values.get(name, "").encode())
    return hasher.hexdigest()[:24]


def _pep517_target_dir(
    project_id: Optional[str] = None,
    environment: Optional[dict[str, str]] = None,
) -> Path:
    if project_id is None:
        project_id = _project_build_identity()
    values = os.environ if environment is None else environment
    return _wheel_cache_root(values) / "cargo-target" / "pep517" / project_id


def _setting_value(config_settings: Optional[dict], *keys: str) -> Optional[str]:
    if not config_settings:
        return None
    for key in keys:
        value = config_settings.get(key)
        if isinstance(value, (list, tuple)):
            value = value[-1] if value else None
        if value is not None and str(value).strip():
            return str(value).strip()
    return None


def _explicit_profile(
    config_settings: Optional[dict], *, editable: bool = False
) -> Optional[str]:
    keys: tuple[str, ...] = ("--profile", "profile")
    if editable:
        keys += ("editable-profile",)
    return _setting_value(config_settings, *keys)


def _profile_args(
    config_settings: Optional[dict], *, editable: bool = False
) -> "list[str]":
    """Select the fast local profile without overriding explicit settings."""
    explicit = _setting_value(
        config_settings,
        "--profile",
        "profile",
        "editable-profile" if editable else "profile",
    )
    if explicit:
        return ["--profile", explicit]

    selected = os.environ.get(_FAST_PROFILE_ENV)
    if selected is not None:
        selected = selected.strip()
        if selected.lower() in _DISABLE_PROFILE_VALUES:
            return []
        return ["--profile", selected]

    options = _project_maturin_options()
    configured = options.get(
        "editable-profile" if editable else "profile"
    ) or options.get("profile")
    if configured:
        return []

    return ["--profile", "dev"]


def _prep_env(
    config_settings: Optional[dict] = None,
    *,
    editable: bool = False,
) -> "dict[str, str]":
    env = os.environ.copy()
    if not env.get("SOLDR_CACHE_DIR", "").strip():
        env["SOLDR_CACHE_DIR"] = str(_selected_soldr_root(env))
    explicit_profile = _explicit_profile(config_settings, editable=editable)
    if explicit_profile:
        # Match _profile_args precedence for delegated backends: an explicit
        # PEP setting wins over SOLDR_PEP517_PROFILE from the caller. This
        # value is also included in the project identity below so dev/release
        # delegated builds never share an ambiguous target namespace.
        env[_FAST_PROFILE_ENV] = explicit_profile
    identity_environment = os.environ.copy()
    if explicit_profile:
        identity_environment[_FAST_PROFILE_ENV] = explicit_profile
    env.setdefault("RUSTC_WRAPPER", "soldr")
    env.setdefault("ZCCACHE_PATH_REMAP", "auto")
    # soldr#1867: a wheel build is cold and one-shot, so staged-artifact reuse
    # buys almost nothing here — but a building soldr that predates zccache
    # b81b8131 can serve a stale generation for a key it has already proven
    # non-deterministic. That surfaces as "could not compile <trivial crate>",
    # naming a different crate each run, with nothing pointing at the cache.
    # A caller-set value still wins.
    env.setdefault("ZCCACHE_STAGED_ARTIFACTS", "off")
    # Ask soldr's maturin dispatch to use the automatic fast-linker policy.
    # An explicit SOLDR_LINKER value still wins in the Rust child.
    env.setdefault("SOLDR_PEP517_LINKER", "auto")
    # These are defaults for the backend-selected local `dev` profile. A
    # project-level Cargo setting wins per field, and a caller-set environment
    # value always wins through setdefault. Release/custom profiles are not
    # affected.
    project_dev_options = _project_dev_profile_options()
    for environment_key, (cargo_key, default) in _FAST_DEV_PROFILE_DEFAULTS.items():
        if cargo_key not in project_dev_options:
            env.setdefault(environment_key, default)
    # Stable CARGO_TARGET_DIR for PEP 517 isolated builds. When pip/uv
    # build from an sdist they copy the sources to a throwaway temp dir,
    # so `<srcdir>/target/` is discarded after every build and cargo
    # runs cold each time (25-30s+ per `pip install`). Pinning the
    # target dir to a stable per-user path keeps cargo's incremental
    # fingerprint cache hot across isolated builds.
    #
    # Ingested from FastLED/fbuild's setup.py (`WHEEL_BUILD_TARGET_DIR`,
    # FastLED/fbuild#829): keep PEP builds separate from any dev
    # `<repo>/target/` so `pip install` and the dev CLI do not invalidate
    # each other's artifacts. The namespace is content-derived from the
    # project build configuration, so temporary PEP source directories
    # reuse the same target while unrelated projects do not share state.
    #
    # Escape hatches: a caller-provided CARGO_TARGET_DIR always wins
    # (setdefault), and SOLDR_PEP517_STABLE_TARGET_DIR=0 (or false/no/
    # off) skips the pin entirely.
    project_id = _project_build_identity(identity_environment)
    env.setdefault("SOLDR_PEP517_PROJECT_ID", project_id)
    knob = env.get("SOLDR_PEP517_STABLE_TARGET_DIR", "").strip().lower()
    if knob not in ("0", "false", "no", "off"):
        env.setdefault(
            "CARGO_TARGET_DIR",
            str(_pep517_target_dir(project_id, env)),
        )
    return env


def _frontend_is_verbose(env: "dict[str, str]") -> bool:
    """Best-effort verbosity detection for common PEP 517 frontends."""
    for key in ("PIP_VERBOSE", "UV_VERBOSE"):
        value = env.get(key, "").strip().lower()
        if value and value not in _DISABLE_PROFILE_VALUES:
            return True
    return False


def _stats_mode(env: "dict[str, str]") -> str:
    raw = env.get(_STATS_ENV)
    if raw is None:
        return "full" if _frontend_is_verbose(env) else "short"
    value = raw.strip().lower()
    if value in _DISABLE_PROFILE_VALUES:
        return "off"
    if value == "full":
        return "full"
    return "short"


def _session_command(
    subcommand: str, env: "dict[str, str]", *args: str
) -> "dict | None":
    """Run a best-effort session command without perturbing a wheel build."""
    try:
        result = run_captured(
            ["soldr", subcommand, *args, "--json"],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        return None
    if result.returncode != 0:
        return None
    try:
        payload = json.loads(result.stdout)
    except (TypeError, ValueError):
        return None
    return payload if isinstance(payload, dict) else None


def _emit_build_stats(
    env: "dict[str, str]",
    session_id: "str | None",
    elapsed_seconds: float,
    label: str,
) -> None:
    mode = _stats_mode(env)
    if mode == "off":
        return

    end = (
        _session_command("session-end", env, "--id", session_id)
        if session_id is not None
        else None
    )
    stats = end.get("stats") if end else None
    elapsed = f"{elapsed_seconds:.1f}s"
    if not isinstance(stats, dict):
        print(
            f"soldr PEP 517: built {label} in {elapsed} | cache stats unavailable",
            file=sys.stderr,
        )
        return

    hits = stats.get("hits", 0)
    misses = stats.get("misses", 0)
    hit_rate = stats.get("hit_rate", 0.0)
    saved_ms = stats.get("time_saved_ms", 0)
    try:
        rate = float(hit_rate) * 100.0
    except (TypeError, ValueError):
        rate = 0.0
    try:
        saved = float(saved_ms) / 1000.0
    except (TypeError, ValueError):
        saved = 0.0
    print(
        f"soldr PEP 517: built {label} in {elapsed} | "
        f"cache {hits} hits / {misses} misses ({rate:.1f}%) | "
        f"saved {saved:.1f}s",
        file=sys.stderr,
    )
    if mode == "full":
        print(
            "soldr PEP 517 details: "
            + json.dumps(
                {"build_seconds": round(elapsed_seconds, 3), "cache": stats},
                sort_keys=True,
            ),
            file=sys.stderr,
        )


# Idle watchdog for the maturin child (soldr#1803). The old fixed 600s
# wall-clock cap killed legitimate cold Rust release builds (observed:
# 15m22s) and misdiagnosed them as daemon wedges. The invariant now: as
# long as the build is producing output it is never killed; only sustained
# SILENCE trips the watchdog, because a wedged daemon/toolchain goes quiet
# while a big compile keeps printing "Compiling ..." lines.
_PEP517_IDLE_TIMEOUT_ENV = "SOLDR_PEP517_IDLE_TIMEOUT_SECS"
# 30 min, not lower: a large LTO link is the longest legitimately-SILENT
# phase a build has. Piped (non-TTY) cargo prints nothing between the last
# "Compiling" line and "Finished", so the watchdog must outlast the worst
# realistic link. Normal Cargo "Compiling ..." events provide first-line
# liveness without forcing TTY progress redraws into pip's captured log.
_PEP517_IDLE_TIMEOUT_DEFAULT = 1800.0
_PEP517_FAILURE_TAIL_CHARS = 64 * 1024
_PEP517_TIMEOUT_RELAY_DRAIN_SECONDS = 5.0
_ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")


def _pep517_idle_timeout(env: "dict[str, str]") -> "float | None":
    """Resolve the idle timeout: env override, 0 or negative disables."""
    raw = env.get(_PEP517_IDLE_TIMEOUT_ENV)
    if raw is None:
        return _PEP517_IDLE_TIMEOUT_DEFAULT
    try:
        value = float(raw)
    except ValueError:
        return _PEP517_IDLE_TIMEOUT_DEFAULT
    return None if value <= 0 else value


def _describe_pep517_exit(returncode: int) -> str:
    """Render an exit code, naming termination when that is what it is.

    soldr#2742: a build killed by an external harness surfaced through uv as
    ``Call to `soldr.build_editable` failed (exit code: 0xffffffff)`` with
    nothing to say the process had been *terminated* rather than having
    failed to compile. A reader cannot act on that: "killed by my hook's
    timeout" and "your code does not compile" need opposite responses.

    Two spellings, because the platforms disagree. POSIX reports a signalled
    child as a negative return code carrying the signal number. Windows has
    no signals here; ``TerminateProcess`` exit codes surface as large
    unsigned values, of which ``0xffffffff`` is the one this issue observed.
    Neither is distinguishable from a compiler exit code by magnitude alone,
    so both are named explicitly.
    """
    if returncode < 0:
        name = _signal_name(-returncode)
        return f"terminated by {name} (exit code {returncode})"
    if returncode >= 0xC0000000 or returncode == 0xFFFFFFFF:
        return (
            f"terminated (exit code {returncode} / 0x{returncode:08x}) -- "
            "an exit code in this range is a Windows termination or fault "
            "status, not a compiler exit code"
        )
    return f"exit code {returncode}"


def _signal_name(signum: int) -> str:
    try:
        return signal.Signals(signum).name
    except (ValueError, AttributeError):
        return f"signal {signum}"


_COMPILE_REPLY_TIMEOUT_ENV = "SOLDR_COMPILE_REPLY_TIMEOUT_SECS"

# Mirrors the Rust-side backstop documented in docs/DAEMON_TIMEOUTS.md. Only
# used to render advice; soldr-cli owns the real default.
_COMPILE_REPLY_TIMEOUT_DEFAULT_SECS = 1800

_PEP517_TERMINATED_HINT = (
    "soldr: the build process was terminated rather than failing on its own."
    " Something outside soldr stopped it -- a harness or hook timeout, a"
    " Ctrl-C, or the OOM killer. Diagnostics above (if any) are from before"
    " that point, so a queued-daemon warning there is the last known state,"
    " not the cause of this exit. To make soldr give up first and explain"
    " itself, set SOLDR_COMPILE_REPLY_TIMEOUT_SECS below the caller's"
    " timeout.\n"
)


def _effective_compile_reply_timeout(env: "dict[str, str]") -> "int | None":
    """The compile-reply backstop this build ran under, for advice only.

    ``None`` when the caller set something unparseable -- soldr-cli falls
    back to its documented default there, but saying so from here would be
    asserting behaviour this module does not own.
    """
    raw = env.get(_COMPILE_REPLY_TIMEOUT_ENV, "").strip()
    if not raw:
        return _COMPILE_REPLY_TIMEOUT_DEFAULT_SECS
    try:
        parsed = int(raw)
    except ValueError:
        return None
    return parsed if parsed > 0 else None


def _pep517_terminated_hint(env: "dict[str, str]") -> str:
    """The termination hint, naming the deadline the reader has to beat.

    soldr#2742: the advice was "set SOLDR_COMPILE_REPLY_TIMEOUT_SECS below
    the caller's timeout", which is only actionable if you know what it is
    now. The surprising part is that the default is thirty minutes, so a
    caller with any shorter budget is guaranteed to lose the race and see a
    bare kill instead of soldr's own diagnosis.
    """
    effective = _effective_compile_reply_timeout(env)
    if effective is None:
        return _PEP517_TERMINATED_HINT
    return _PEP517_TERMINATED_HINT.replace(
        "set SOLDR_COMPILE_REPLY_TIMEOUT_SECS below the caller's timeout.",
        f"set SOLDR_COMPILE_REPLY_TIMEOUT_SECS (currently {effective}s) below"
        " the caller's timeout.",
    )


def _pep517_exit_was_termination(returncode: int) -> bool:
    """True when the child was killed rather than exiting on its own."""
    return returncode < 0 or returncode >= 0xC0000000 or returncode == 0xFFFFFFFF


def _backend_termination_signals() -> "list[int]":
    """Signals a harness sends before resorting to an uncatchable kill.

    SIGTERM is the POSIX convention. SIGBREAK is its Windows counterpart --
    Ctrl-Break, which is what a Windows harness typically sends first.
    SIGINT covers a Ctrl-C delivered to us rather than to the child.
    """
    found = []
    for name in ("SIGTERM", "SIGBREAK", "SIGINT"):
        number = getattr(signal, name, None)
        if number is not None:
            found.append(number)
    return found


def _backend_termination_message(
    signum: int, cmd: "list[str]", env: "dict[str, str]", elapsed: float
) -> str:
    """What we print when the backend itself is signalled.

    Split out from the handler so it can be asserted on directly: the
    handler's last act is to re-raise the signal, so a test cannot call it
    and live to inspect the result.
    """
    return (
        f"\nsoldr: terminated by {_signal_name(signum)} after "
        f"{elapsed:.0f}s while running `{' '.join(cmd)}`.\n"
        + _pep517_terminated_hint(env)
    )


@contextmanager
def _explain_backend_termination(
    cmd: "list[str]", env: "dict[str, str]", started_at: float
) -> Iterator[None]:
    """Say why we died when *we* are signalled, not just when the child is.

    soldr#2744 named a terminated **child**. This is the other half of
    soldr#2742: the backend process itself being killed by whatever invoked
    it, which is what produced the reported bare ``exit code: 0xffffffff``.
    uv only ever sees our exit status, so a silent death is a number, and a
    number is not a diagnosis.

    This deliberately does **not** pre-empt the kill by shortening any
    deadline -- shortening the per-unit compile budget would fail builds that
    are merely large. It only makes the kill self-describing.

    Its limit is worth stating plainly: a hard ``TerminateProcess`` on
    Windows, or ``SIGKILL``, cannot be caught by anyone, so this helps only
    when the harness signals first. Harnesses that escalate straight to an
    uncatchable kill are unchanged, and for those the hint's advice to lower
    ``SOLDR_COMPILE_REPLY_TIMEOUT_SECS`` remains the only route.
    """
    previous: "list[tuple[int, Any]]" = []
    reported = threading.Event()

    def _report(signum: int, _frame: Any) -> None:
        # Signals coalesce badly: a harness often sends SIGTERM then SIGKILL,
        # and re-entering here would interleave two copies of the message.
        if not reported.is_set():
            reported.set()
            _write_pep517_text(
                sys.stderr,
                _backend_termination_message(
                    signum, cmd, env, time.perf_counter() - started_at
                ),
            )
            try:
                sys.stderr.flush()
            except (ValueError, OSError):
                pass
        # Restore the default disposition and re-raise, so our exit status
        # still reports the signal. Reporting must not change what the
        # caller observes -- only explain it.
        try:
            signal.signal(signum, signal.SIG_DFL)
            os.kill(os.getpid(), signum)
        except (OSError, ValueError, RuntimeError):
            os._exit(1)  # pylint: disable=protected-access

    for number in _backend_termination_signals():
        try:
            previous.append((number, signal.signal(number, _report)))
        except (ValueError, OSError, RuntimeError):
            # Not the main thread, or the platform refuses this signal.
            # A missing diagnostic must never break a working build.
            continue
    try:
        yield
    finally:
        for number, handler in previous:
            try:
                signal.signal(number, handler)
            except (ValueError, OSError, RuntimeError):
                pass


def _write_pep517_text(sink: TextIO, text: str) -> None:
    """Write decoded child text using the parent stream's encoding."""
    if not text:
        return
    try:
        sink.write(text)
    except UnicodeEncodeError:
        encoding = getattr(sink, "encoding", None) or "ascii"
        safe = text.encode(encoding, errors="backslashreplace").decode(encoding)
        sink.write(safe)
    sink.flush()


# soldr#1802 §4: the Python half of per-line elapsed-second stamping. The Rust
# front door (`cargo_front_door/timestamp_tee.rs`) already stamps cargo's
# output; this mirrors it exactly for the PEP 517 relay so pip/uv build logs
# carry the same `  12.34 ` prefixes. The env var, the default (on for non-TTY,
# off for a terminal), the CRLF-stamps-once rule, and the `{:>8.2}` format are
# all kept identical to the Rust side on purpose — two formats would defeat the
# "same format" acceptance criterion.
_TIMESTAMP_LINES_ENV_VAR = "SOLDR_TIMESTAMP_LINES"


def _should_timestamp_pep517(env_value: "str | None", is_terminal: bool) -> bool:
    """Mirror of Rust ``timestamp_tee::should_timestamp``.

    Default is on for a non-TTY sink (a CI/pip log read after the fact, where
    "which line cost 40s" is the whole question) and off for an interactive
    terminal (which already shows progress live). ``SOLDR_TIMESTAMP_LINES``
    overrides both directions.
    """
    if env_value is not None:
        v = env_value.strip().lower()
        if v in ("1", "true", "on"):
            return True
        if v in ("0", "false", "off"):
            return False
    return not is_terminal


def _pep517_epoch_anchor_wanted(github_actions: str | None) -> bool:
    """Mirror of the Rust ``epoch_anchor_wanted``: skip the ``# t0=`` anchor
    where the runner already stamps every line."""
    return not (github_actions or "").strip()


def _pep517_epoch_anchor_line(now_unix_ms: int) -> str:
    """One `# t0=<epoch-seconds>` line so absolute times are derivable.

    Byte-identical to Rust ``timestamp_tee::epoch_anchor_line``.
    """
    return f"# t0={now_unix_ms // 1000}.{now_unix_ms % 1000:03d}\n"


class _LineStamper:
    """Insert an elapsed-seconds prefix at each line start, color-preserving.

    Port of Rust ``TimestampedTee``: the prefix is plain text inserted only at
    column 0, so ANSI escapes inside a line pass through untouched. Both ``\\n``
    and ``\\r`` start a new line, so cargo's ``\\r`` progress redraws are stamped;
    a CRLF pair stamps once, not twice. State is per-stream (one instance per
    relay thread), matching the Rust design where stdout and stderr each own a
    tee.
    """

    def __init__(self, t0: float) -> None:
        self._t0 = t0
        self._at_line_start = True
        self._last_was_cr = False

    def _prefix(self) -> str:
        return f"{time.monotonic() - self._t0:>8.2f} "

    def stamp(self, text: str) -> str:
        if not text:
            return text
        out: list[str] = []
        for ch in text:
            is_lf = ch == "\n"
            is_cr = ch == "\r"
            # A CRLF pair is one terminator: the CR already set the flag, and
            # the newline must not draw a second prefix on the way to the same
            # new line.
            if self._at_line_start and not (is_lf and self._last_was_cr):
                out.append(self._prefix())
            out.append(ch)
            self._at_line_start = is_lf or is_cr
            self._last_was_cr = is_cr
        return "".join(out)


# Cargo prints the *entire* compiler invocation on its "process didn't exit
# successfully" line. That is thousands of characters (soldr#1878): it buries
# the real error and, when the byte-bounded failure tail slices through it,
# leaves a fragment starting mid-flag (`--crate-type lib --emit=dep-inf`). We
# keep the program + crate name and elide the flags.
_PROCESS_FAILED_RE = re.compile(
    r"^(?P<prefix>.*process didn't exit successfully:\s*)`(?P<cmd>.*)`(?P<suffix>.*)$"
)
# A single non-diagnostic line longer than this is truncated so no one line
# (e.g. a tail fragment of a compiler command) can dominate the excerpt.
_EXCERPT_LINE_CAP = 400


def _collapse_process_command(line: str) -> str:
    """Shorten Cargo's ``process didn't exit successfully: `<huge cmd>``` line.

    Keeps the program and ``--crate-name`` so the invocation is still
    identifiable, replaces the flag list with an ``(N args elided)`` note, and
    preserves the trailing ``(exit code: N)``. Returns the line unchanged when
    it does not match or the command is already short.
    """
    match = _PROCESS_FAILED_RE.match(line)
    if match is None:
        return line
    cmd = match.group("cmd")
    if len(cmd) <= 200:
        return line
    tokens = cmd.split()
    program = tokens[0] if tokens else "?"
    crate = ""
    for index, token in enumerate(tokens):
        if token == "--crate-name" and index + 1 < len(tokens):
            crate = f" --crate-name {tokens[index + 1]}"
            break
        if token.startswith("--crate-name="):
            crate = f" {token}"
            break
    elided = max(len(tokens) - 1, 0)
    return f"{match.group('prefix')}`{program}{crate} … ({elided} args elided)`{match.group('suffix')}"


def _cap_excerpt_line(line: str) -> str:
    """Bound one non-diagnostic line so a stray long fragment cannot swamp
    the excerpt. Compiler diagnostics are rendered separately and never
    reach here, so this only trims Cargo's own bookkeeping lines."""
    if len(line) <= _EXCERPT_LINE_CAP:
        return line
    return f"{line[:_EXCERPT_LINE_CAP].rstrip()} … (line truncated)"


def _pep517_failure_excerpt(stdout_tail: str, stderr_tail: str) -> str:
    """Return a bounded error-focused excerpt without Cargo progress redraws."""
    raw = f"{stderr_tail}\n{stdout_tail}"
    lines: list[str] = []
    for line in raw.replace("\r", "\n").splitlines():
        stripped = _ANSI_ESCAPE_RE.sub("", line).rstrip()
        compact = stripped.lstrip()
        if not compact:
            continue
        if compact.startswith("Building ["):
            continue
        if compact.startswith("{"):
            try:
                cargo_message = json.loads(compact)
            except json.JSONDecodeError:
                cargo_message = None
            if isinstance(cargo_message, dict) and cargo_message.get("reason") == (
                "compiler-message"
            ):
                message = cargo_message.get("message")
                rendered = (
                    message.get("rendered") if isinstance(message, dict) else None
                )
                if isinstance(rendered, str):
                    lines.extend(
                        _ANSI_ESCAPE_RE.sub("", item).rstrip()
                        for item in rendered.replace("\r", "\n").splitlines()
                        if item.strip()
                    )
            continue
        # Not a rendered compiler diagnostic -- Cargo's own bookkeeping. Elide
        # the giant invocation on the "process didn't exit successfully" line
        # and cap any other over-long line so the real error stays visible
        # (soldr#1878).
        lines.append(_cap_excerpt_line(_collapse_process_command(stripped)))

    if not lines:
        return ""

    markers = (
        "error",
        "fatal:",
        "caused by:",
        "failed",
        "could not compile",
        "linking with",
    )
    window = lines[-80:]
    marker_indexes = [
        index
        for index, line in enumerate(window)
        if line.lstrip().lower().startswith(markers)
    ]
    if marker_indexes:
        window = window[marker_indexes[0] :]
    else:
        window = window[-20:]
    return "\n".join(window)


def _pep517_failure_payload(
    excerpt: str, log_path: "Path | None", relays_complete: bool
) -> str:
    """What travels *with* the exception, not just to our stderr.

    soldr#1999 rule 2. The diagnostics were being written to stderr and then
    dropped at the exception boundary, so a consumer rendering from the
    exception saw an exit code and nothing else. Everything a reader needs to
    start in the right place goes here too.
    """
    parts: list[str] = []
    if excerpt:
        parts.append(excerpt)
    else:
        parts.append(
            "soldr: the PEP 517 build produced no diagnostics before failing (soldr#1878)."
        )
    if log_path is not None:
        qualifier = "full" if relays_complete else "possibly incomplete"
        parts.append(f"soldr: {qualifier} PEP 517 build log: {log_path}")
    return "\n".join(parts)


def _pep517_failure_message(exc: "subprocess.CalledProcessError") -> str:
    """The text a failing PEP 517 hook ends with (soldr#3401): the payload the
    streaming layer built (named cause, log path) plus the child's exit status."""
    payload = exc.output if isinstance(exc.output, str) and exc.output else ""
    header = f"soldr: the PEP 517 build failed (exit status {exc.returncode})."
    return f"{header}\n{payload}" if payload else header


def _open_pep517_log(
    cmd: "list[str]", env: "dict[str, str]"
) -> "tuple[Path | None, BinaryIO | None]":
    """Open a unique full-output log without making logging build-critical."""
    root = env.get("SOLDR_CACHE_DIR", "").strip()
    if not root:
        return None, None
    directory = Path(root).expanduser() / "logs" / "pep517"
    filename = f"build-{time.strftime('%Y%m%d-%H%M%S', time.gmtime())}-{os.getpid()}-{time.time_ns()}.log"
    path = directory / filename
    try:
        directory.mkdir(parents=True, exist_ok=True)
        log = path.open("xb")
        log.write(
            (f"command: {json.dumps(cmd, ensure_ascii=False)}\n\n").encode("utf-8")
        )
    except OSError:
        return None, None
    return path, log


def _close_pep517_log(log: "BinaryIO | None") -> None:
    if log is None:
        return
    try:
        log.close()
    except OSError:
        pass


def _discard_pep517_log(path: "Path | None") -> None:
    if path is None:
        return
    try:
        path.unlink()
    except OSError:
        pass


def _run_pep517_streaming(cmd: "list[str]", env: "dict[str, str]") -> None:  # noqa: C901
    """Run the maturin child, relaying output live, killing only on silence.

    Both child streams are piped unbuffered and relayed chunk-by-chunk to
    our own stdout/stderr with an immediate flush, so pip/uv users watch
    the build stream in real time (no collect-then-dump). Every chunk
    resets the idle deadline. Raises ``subprocess.TimeoutExpired`` only
    after ``_pep517_idle_timeout`` seconds with zero output, and
    ``subprocess.CalledProcessError`` on a nonzero exit, matching the
    contract callers relied on from ``check_call``.
    """
    idle_timeout = _pep517_idle_timeout(env)
    child_env = dict(env)
    # TTY progress redraws become hundreds of near-identical lines after pip
    # captures them. Cargo's normal "Compiling ..." events still reset the
    # watchdog, whose 30-minute default also covers a legitimately silent
    # link. Respect explicit caller overrides.
    child_env.setdefault("CARGO_TERM_PROGRESS_WHEN", "never")
    child_env.setdefault("CARGO_TERM_COLOR", "never")
    child_env.setdefault("NO_COLOR", "1")
    # soldr#1802: the child is soldr, whose Rust front door also stamps. We
    # stamp the relay here, so the child must not — force its stamping off to
    # avoid a doubled `  0.12   1.34 ` prefix. A caller who set it explicitly
    # still wins for their own shell; this only governs the child we spawn.
    child_env[_TIMESTAMP_LINES_ENV_VAR] = "0"
    # pylint: disable=consider-using-with  # the child outlives this scope;
    # it is waited on explicitly below. A `with` block would close its pipes
    # before the relay threads have drained them.
    process = subprocess.Popen(
        cmd,
        env=child_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
    )
    last_output = time.monotonic()
    output_lock = threading.Lock()
    tails = {"stdout": "", "stderr": ""}
    relay_errors: list[Exception] = []
    stdout_sink = sys.stdout
    stderr_sink = sys.stderr
    log_path, log = _open_pep517_log(cmd, child_env)

    # soldr#1802 §4: anchor once at relay start so every prefix is elapsed
    # seconds from the same t0, and stamp each stream independently (per-stream
    # line state, exactly like the Rust tees). Gate per sink so a redirected
    # stderr still gets stamps even when stdout is an interactive terminal. The
    # env override read here is the *caller's* value, captured before we forced
    # the child's to "0" above.
    stamp_t0 = time.monotonic()
    ts_override = env.get(_TIMESTAMP_LINES_ENV_VAR)

    def _stamper_for(sink: TextIO) -> "_LineStamper | None":
        is_tty = bool(getattr(sink, "isatty", lambda: False)())
        if not _should_timestamp_pep517(ts_override, is_tty):
            return None
        return _LineStamper(stamp_t0)

    stampers = {
        "stdout": _stamper_for(stdout_sink),
        "stderr": _stamper_for(stderr_sink),
    }
    # One `# t0=` header, on the stream that carries the build's progress
    # (stderr), mirroring the Rust front door's `eprint!` of the same line.
    # Best-effort like the log write: if the sink is already broken, do not let
    # the anchor turn a graceful "output relay failed" into a raw write error
    # from the main thread -- the relay surfaces a persistent sink failure.
    # Same rule as the Rust `epoch_anchor_wanted`: GitHub Actions already
    # prefixes every log line with a UTC timestamp, so there the anchor is
    # one more line per invocation that says nothing the log does not.
    if stampers["stderr"] is not None and _pep517_epoch_anchor_wanted(
        os.environ.get("GITHUB_ACTIONS")
    ):
        try:
            _write_pep517_text(
                stderr_sink, _pep517_epoch_anchor_line(int(time.time() * 1000))
            )
        except OSError:
            pass

    def relay(source: BinaryIO, sink: TextIO, tail_name: str) -> None:
        decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
        stamper = stampers[tail_name]

        def emit(text: str, raw: bytes = b"") -> None:
            nonlocal last_output
            with output_lock:
                last_output = time.monotonic()
                # The log and failure tails must see UNSTAMPED bytes: they feed
                # the diagnostic scanner and the archived build log, which must
                # not gain prefixes (soldr#1802 acceptance: parsers see raw).
                if log is not None and raw:
                    try:
                        log.write(raw)
                    except OSError:
                        pass
                tails[tail_name] = (tails[tail_name] + text)[
                    -_PEP517_FAILURE_TAIL_CHARS:
                ]
            _write_pep517_text(sink, stamper.stamp(text) if stamper else text)

        try:
            while True:
                chunk = source.read(8192)
                if not chunk:
                    break
                emit(decoder.decode(chunk), chunk)
            emit(decoder.decode(b"", final=True))
        except Exception as error:  # pylint: disable=broad-exception-caught
            # Deliberately broad: this runs on a daemon thread, where an
            # escaping exception is discarded by the interpreter and the
            # build hangs with no stated cause. Collected here and re-raised
            # on the main thread instead.
            with output_lock:
                relay_errors.append(error)

    assert process.stdout is not None
    assert process.stderr is not None
    relays = [
        threading.Thread(
            target=relay,
            args=(process.stdout, stdout_sink, "stdout"),
            daemon=True,
        ),
        threading.Thread(
            target=relay,
            args=(process.stderr, stderr_sink, "stderr"),
            daemon=True,
        ),
    ]
    for thread in relays:
        thread.start()
    returncode = None
    timed_out = False
    relay_failed = False
    relays_complete = False
    try:
        while True:
            try:
                returncode = process.wait(timeout=1)
                break
            except subprocess.TimeoutExpired:
                with output_lock:
                    relay_failed = bool(relay_errors)
                if relay_failed:
                    process.kill()
                    process.wait(timeout=10)
                    break
                if idle_timeout is None:
                    continue
                with output_lock:
                    idle = time.monotonic() - last_output
                if idle > idle_timeout:
                    process.kill()
                    process.wait(timeout=10)
                    timed_out = True
                    break
    finally:
        bounded_drain = timed_out or relay_failed
        for thread in relays:
            thread.join(
                timeout=_PEP517_TIMEOUT_RELAY_DRAIN_SECONDS if bounded_drain else None
            )
        if process.stdout is not None:
            process.stdout.close()
        if process.stderr is not None:
            process.stderr.close()
        if bounded_drain:
            for thread in relays:
                thread.join(timeout=1)
        relays_complete = not relay_errors and all(
            not thread.is_alive() for thread in relays
        )
        _close_pep517_log(log)
    if timed_out:
        assert idle_timeout is not None
        qualifier = "full " if relays_complete else "possibly incomplete "
        detail = (
            f"\nsoldr: {qualifier}PEP 517 build log: {log_path}\n" if log_path else ""
        )
        _write_pep517_text(stderr_sink, detail)
        raise subprocess.TimeoutExpired(cmd, idle_timeout)
    if relay_errors:
        detail = (
            f"; possibly incomplete PEP 517 build log: {log_path}" if log_path else ""
        )
        raise RuntimeError(
            f"soldr PEP 517 output relay failed{detail}"
        ) from relay_errors[0]
    assert returncode is not None
    if returncode != 0:
        excerpt = _pep517_failure_excerpt(tails["stdout"], tails["stderr"])
        summary = f"\nsoldr: PEP 517 build failed ({_describe_pep517_exit(returncode)})"
        if excerpt:
            summary += f"; relevant diagnostics:\n{excerpt}\n"
        else:
            # soldr#1878's exact signature: a non-zero exit with nothing to
            # show. Saying "no diagnostics" out loud is the difference between
            # a reader concluding their code is broken and knowing the build
            # died before it could explain itself.
            summary += (
                " and produced no diagnostics at all -- the build failed before"
                " it could explain why (soldr#1878).\n"
            )
        # Applies whether or not there were diagnostics: a build killed
        # mid-compile usually HAS output, and that output is the last
        # known state rather than the cause (soldr#2742).
        if _pep517_exit_was_termination(returncode):
            summary += _pep517_terminated_hint(env)
        if log_path is not None:
            qualifier = "full " if relays_complete else "possibly incomplete "
            summary += f"soldr: {qualifier}PEP 517 build log: {log_path}\n"
        _write_pep517_text(stderr_sink, summary)
        # soldr#1999 rule 2: no layer may replace a specific error with a
        # generic one. `CalledProcessError` was raised bare, so its `.output`
        # and `.stderr` were None -- everything above went to our stderr and
        # nothing travelled with the exception. A caller that renders from the
        # exception (pip and uv both do) therefore reported the build as
        # having produced nothing, discarding a diagnosis soldr already held.
        raise subprocess.CalledProcessError(
            returncode,
            cmd,
            output=_pep517_failure_payload(excerpt, log_path, relays_complete),
            stderr=excerpt or None,
        )
    _discard_pep517_log(log_path)


def _maturin_pep517(
    subcommand: str,
    *args: str,
    build_label: "str | None" = None,
    config_settings: Optional[dict] = None,
    editable: bool = False,
) -> None:
    env = _prep_env(config_settings, editable=editable)
    mode = _stats_mode(env)
    started_at = time.perf_counter()
    start = (
        _session_command("session-start", env)
        if build_label and mode != "off"
        else None
    )
    session_id = start.get("session_id") if start else None
    if isinstance(session_id, str):
        env["ZCCACHE_SESSION_ID"] = session_id
    else:
        session_id = None
    cmd = ["soldr", "maturin", "pep517", subcommand, *args]
    try:
        # soldr#2742: names a terminated *backend* (uv's bare 0xffffffff).
        with _explain_backend_termination(cmd, env, started_at):
            with _sibling_module("_tty_quiet").quiet_tty_echo():
                _run_pep517_streaming(cmd, env=env)
    except subprocess.TimeoutExpired as exc:
        if session_id is not None:
            _session_command("session-end", env, "--id", session_id)
        idle = _pep517_idle_timeout(env)
        raise RuntimeError(
            f"soldr maturin pep517 produced no output for {idle:.0f}s and was "
            "killed. A build that is still printing is never killed, so this "
            "usually means the toolchain is genuinely wedged - try "
            "`soldr status` to inspect the zccache daemon. Set "
            f"{_PEP517_IDLE_TIMEOUT_ENV}=<secs> to adjust (0 disables)."
        ) from exc
    except subprocess.CalledProcessError as exc:
        if session_id is not None:
            _session_command("session-end", env, "--id", session_id)
        # soldr#3401: the build already relayed its diagnosis to stderr. Ending
        # the hook with a bare exception makes the frontend print a Python
        # traceback that buries the one line that matters. `SystemExit` with a
        # message prints just the message and exits 1; pyproject-hooks does not
        # catch it, and pip and uv show the hook's stderr. The payload keeps the
        # named cause and the full-log path (soldr#1999 rule 2).
        raise SystemExit(_pep517_failure_message(exc)) from None
    except Exception:
        if session_id is not None:
            _session_command("session-end", env, "--id", session_id)
        raise
    if build_label:
        _emit_build_stats(
            env,
            session_id,
            time.perf_counter() - started_at,
            build_label,
        )


def _selected_soldr_root(environment: "Mapping[str, str]") -> Path:
    """Ask the selected soldr binary for its root.

    Every build defaults to ``~/.soldr`` (daemon generations are isolated by
    image hash); querying the executable on PATH keeps the Python backend
    aligned with the exact binary it will invoke, including any
    ``SOLDR_CACHE_DIR`` it honours, without duplicating Rust's root policy.
    """
    explicit = environment.get("SOLDR_CACHE_DIR", "").strip()
    if explicit:
        return Path(explicit).expanduser()
    queried = _query_soldr_root(environment)
    if queried is not None:
        return queried
    # Compatibility fallback for an older soldr binary whose version payload
    # predates `root_dir`. Such released binaries use the production root.
    return Path.home() / ".soldr"


def _query_soldr_root(environment: "Mapping[str, str]") -> "Path | None":
    # `status --json` has carried root_dir longer than `version --json`, so it
    # is a compatibility fallback for older development binaries. Neither
    # command starts a daemon.
    # One build hook prepares the environment several times while checking,
    # restoring, and storing the wheel cache. On Windows each CLI probe costs
    # hundreds of milliseconds even when Cargo has no work, so cache successful
    # answers for the lifetime of this short-lived backend process.
    key = (
        environment.get("PATH", ""),
        environment.get("PATHEXT", ""),
        environment.get("HOME", ""),
        environment.get("USERPROFILE", ""),
        environment.get("SOLDR_CACHE_DIR", ""),
        os.getcwd(),
    )
    with _SOLDR_ROOT_CACHE_LOCK:
        cached = _SOLDR_ROOT_CACHE.get(key)
        if cached is not None:
            return cached
        for subcommand in ("version", "status"):
            try:
                result = run_captured(
                    ["soldr", subcommand, "--json"],
                    env=environment,
                    capture_output=True,
                    text=True,
                    check=False,
                    timeout=5,
                )
                payload = json.loads(result.stdout) if result.returncode == 0 else None
                root = payload.get("root_dir") if isinstance(payload, dict) else None
                if isinstance(root, str) and root.strip():
                    selected = Path(root).expanduser()
                    if selected.is_absolute():
                        _SOLDR_ROOT_CACHE[key] = selected
                        return selected
            except (OSError, subprocess.SubprocessError, TypeError, ValueError):
                continue
    return None


def _wheel_cache_root(environment: "Mapping[str, str]") -> Path:
    return _selected_soldr_root(environment)


def _hash_metadata_tree(
    hasher: "hashlib._Hash",
    root: Path,
    label: str,
    ignored_directories: "set[str] | None" = None,
) -> None:
    """Hash paths plus metadata without reading staged native artifacts."""
    try:
        root = root.resolve()
    except OSError:
        return
    if not root.is_dir():
        _hash_identity_field(hasher, label, b"missing")
        return
    ignored = _WHEEL_CACHE_IGNORED_DIRECTORIES | (ignored_directories or set())

    for directory, directories, files in os.walk(root):
        current = Path(directory)
        try:
            current_relative = current.relative_to(root)
        except ValueError:
            current_relative = Path()
        directories[:] = sorted(
            item
            for item in directories
            if item not in ignored
            and not item.endswith(_WHEEL_CACHE_IGNORED_DIRECTORY_SUFFIXES)
            and (current_relative / item).as_posix()
            not in _WHEEL_CACHE_IGNORED_RELATIVE_DIRECTORIES
        )
        for filename in sorted(files):
            path = current / filename
            try:
                relative = path.relative_to(root).as_posix()
                stat = path.stat()
            except OSError:
                continue
            if not path.is_file():
                continue
            _hash_identity_field(
                hasher,
                f"{label}/{relative}",
                f"{stat.st_size}:{stat.st_mtime_ns}".encode(),
            )


def _hash_metadata_directory(
    hasher: "hashlib._Hash", metadata_directory: Optional[str]
) -> None:
    """Hash PEP 517 prepared metadata by content, not its temporary path."""
    if not metadata_directory:
        _hash_identity_field(hasher, "metadata", b"none")
        return
    root = Path(metadata_directory)
    if not root.is_dir():
        _hash_identity_field(hasher, "metadata", b"missing")
        return
    for directory, directories, files in os.walk(root):
        # setuptools leaves a regenerated ``*.egg-info`` tree beside the
        # PEP 517 ``*.dist-info`` result.  It is build bookkeeping rather
        # than hook metadata, and its SOURCES.txt changes after a first build.
        directories[:] = sorted(
            item for item in directories if not item.endswith(".egg-info")
        )
        current = Path(directory)
        for filename in sorted(files):
            path = current / filename
            try:
                relative = path.relative_to(root).as_posix()
                contents = path.read_bytes()
            except OSError:
                continue
            _hash_identity_field(hasher, f"metadata/{relative}", contents)


def _delegate_backend_stamp() -> str:
    """Return a version marker without baking an isolated-env path into a key."""
    name = _delegate_backend_name()
    if not name:
        return "maturin"
    module_name = name.partition(":")[0]
    package = module_name.split(".", 1)[0]
    try:
        # pylint: disable=import-outside-toplevel  # guarded by the except
        from importlib import metadata
    except ImportError:
        return name

    try:
        versions = metadata.packages_distributions().get(package, [])
        return (
            ";".join(
                f"{distribution}={metadata.version(distribution)}"
                for distribution in sorted(versions)
            )
            or name
        )
    except metadata.PackageNotFoundError:
        return name


def _wheel_cache_context(
    kind: str,
    config_settings: Optional[dict],
    metadata_directory: Optional[str],
) -> "tuple[Path, str] | None":
    environment = _prep_env(config_settings, editable=kind == "editable")
    cache_knob = environment.get(_WHEEL_CACHE_ENV)
    if cache_knob is not None and cache_knob.strip().lower() in _DISABLE_PROFILE_VALUES:
        return None

    hasher = hashlib.sha256()
    _hash_identity_field(hasher, "schema", _WHEEL_CACHE_SCHEMA)
    _hash_identity_field(hasher, "kind", kind.encode())
    _hash_identity_field(
        hasher,
        "config-settings",
        json.dumps(config_settings or {}, sort_keys=True, default=str).encode(),
    )
    _hash_identity_field(hasher, "backend", _delegate_backend_stamp().encode())
    _hash_identity_field(
        hasher,
        "python",
        f"{sys.implementation.name}:{sys.version_info[:2]}:{sysconfig.get_platform()}".encode(),
    )
    for name, value in sorted(environment.items()):
        if name.startswith(
            ("CARGO_", "MATURIN_", "PYO3_", "RUST", "SOLDR_")
        ) or name in {
            "AR",
            "CC",
            "CXX",
            "MACOSX_DEPLOYMENT_TARGET",
            "SDKROOT",
            "SOURCE_DATE_EPOCH",
        }:
            _hash_identity_field(hasher, f"environment/{name}", value.encode())
    root = _project_root()
    ignored_directories: set[str] = set()
    try:
        relative_cache_root = (
            (_wheel_cache_root(environment) / "pep517")
            .resolve()
            .relative_to(root.resolve())
        )
        if relative_cache_root.parts:
            ignored_directories.add(relative_cache_root.parts[0])
    except (OSError, ValueError):
        pass
    _hash_metadata_tree(hasher, root, "source", ignored_directories)
    _hash_metadata_directory(hasher, metadata_directory)
    return (
        _wheel_cache_root(environment)
        / "pep517"
        / "wheels"
        / _project_build_identity(environment)
        / kind,
        hasher.hexdigest(),
    )


def _wheel_cache_restore(
    context: "tuple[Path, str] | None", wheel_directory: str
) -> Optional[str]:
    if context is None:
        return None
    directory, fingerprint = context
    try:
        manifest = json.loads((directory / "manifest.json").read_text(encoding="utf-8"))
        filename = manifest["filename"]
        artifact_name = manifest["artifact"]
        artifact = directory / artifact_name
    except (KeyError, OSError, TypeError, ValueError):
        return None
    if (
        manifest.get("schema") != 1
        or manifest.get("fingerprint") != fingerprint
        or not isinstance(filename, str)
        or Path(filename).name != filename
        or not filename.endswith(".whl")
        or not isinstance(artifact_name, str)
        or Path(artifact_name).name != artifact_name
        or not artifact.is_file()
    ):
        return None
    destination = Path(wheel_directory) / filename
    try:
        destination.unlink(missing_ok=True)
        os.link(artifact, destination)
    except OSError:
        try:
            shutil.copy2(artifact, destination)
        except OSError:
            return None
    return filename


def _wheel_cache_store(
    context: "tuple[Path, str] | None", wheel_directory: str, filename: str
) -> None:
    if (
        context is None
        or Path(filename).name != filename
        or not filename.endswith(".whl")
    ):
        return
    source = Path(wheel_directory) / filename
    if not source.is_file():
        return
    directory, fingerprint = context
    artifact_name = f"{fingerprint}.whl"
    artifact = directory / artifact_name
    temporary = directory / f".{artifact_name}.{os.getpid()}.tmp"
    manifest = directory / "manifest.json"
    previous_artifact: Optional[str] = None
    try:
        directory.mkdir(parents=True, exist_ok=True)
        try:
            previous = json.loads(manifest.read_text(encoding="utf-8"))
            candidate = previous.get("artifact")
            if isinstance(candidate, str) and Path(candidate).name == candidate:
                previous_artifact = candidate
        except (OSError, TypeError, ValueError):
            pass
        temporary.unlink(missing_ok=True)
        try:
            os.link(source, temporary)
        except OSError:
            shutil.copy2(source, temporary)
        os.replace(temporary, artifact)
        manifest.write_text(
            json.dumps(
                {
                    "schema": 1,
                    "fingerprint": fingerprint,
                    "filename": filename,
                    "artifact": artifact_name,
                },
                sort_keys=True,
            ),
            encoding="utf-8",
        )
        if previous_artifact and previous_artifact != artifact_name:
            (directory / previous_artifact).unlink(missing_ok=True)
    except OSError:
        temporary.unlink(missing_ok=True)


def _wheel_cache_finish(
    kind: str,
    wheel_directory: str,
    config_settings: Optional[dict],
    metadata_directory: Optional[str],
    filename: str,
) -> str:
    # Re-scan after a successful build: setuptools/fbuild can create or update
    # staged files during packaging, and the next invocation must observe them.
    context = _wheel_cache_context(kind, config_settings, metadata_directory)
    _wheel_cache_store(context, wheel_directory, filename)
    _emit_wheel_cache_event(kind, config_settings, "stored", context)
    return filename


def _emit_wheel_cache_event(
    kind: str,
    config_settings: Optional[dict],
    state: str,
    context: "tuple[Path, str] | None",
) -> None:
    """Write cache-key diagnostics only when callers requested full stats."""
    env = _prep_env(config_settings, editable=kind == "editable")
    if _stats_mode(env) != "full":
        return
    payload: dict[str, str] = {"wheel_cache": state, "kind": kind}
    if context is not None:
        payload["fingerprint"] = context[1]
    print(
        f"soldr PEP 517 detail: {json.dumps(payload, sort_keys=True)}", file=sys.stderr
    )


def _emit_wheel_cache_hit(
    kind: str, config_settings: Optional[dict], elapsed_seconds: float
) -> None:
    env = _prep_env(config_settings, editable=kind == "editable")
    mode = _stats_mode(env)
    if mode == "off":
        return
    label = "editable wheel" if kind == "editable" else "wheel"
    print(
        f"soldr PEP 517: reused cached {label} in {elapsed_seconds:.1f}s | wheel cache hit",
        file=sys.stderr,
    )
    if mode == "full":
        print(
            "soldr PEP 517 details: "
            + json.dumps(
                {"build_seconds": round(elapsed_seconds, 3), "wheel_cache": "hit"},
                sort_keys=True,
            ),
            file=sys.stderr,
        )


def _target_args(config_settings: Optional[dict]) -> "list[str]":
    """Translate the highest-precedence PEP 517 target into maturin's flag."""
    if not config_settings:
        return []
    for key in ("--target", "target", "build-target"):
        value = config_settings.get(key)
        if isinstance(value, (list, tuple)):
            value = value[-1] if value else None
        if value is not None and str(value).strip():
            return ["--target", str(value).strip()]
    return []


def _sibling_module(stem: str) -> Any:
    """Load a sibling helper such as ``_bundle_bins`` (soldr#3239). Tests load
    this file standalone, outside the ``soldr`` package, where a relative
    import cannot resolve, so fall back to loading it by path."""
    if __package__:
        return importlib.import_module(f"{__package__}.{stem}")
    name = f"_soldr{stem}"
    module = sys.modules.get(name)
    if module is None:
        spec = importlib.util.spec_from_file_location(
            name, Path(__file__).with_name(f"{stem}.py")
        )
        if spec is None or spec.loader is None:
            raise ImportError(f"cannot load soldr's {stem} helper")
        module = importlib.util.module_from_spec(spec)
        sys.modules[name] = module
        spec.loader.exec_module(module)
    return module


def _project_bundle_bins() -> "list[Any]":
    return _sibling_module("_bundle_bins").read_bundle_bins(
        _project_root() / "pyproject.toml"
    )


def _bundle_bin_profile_args(
    config_settings: Optional[dict], *, editable: bool
) -> "list[str]":
    """Pick the Cargo profile maturin used, so the bin matches the extension."""
    selected = _profile_args(config_settings, editable=editable)
    if selected:
        return selected
    options = _project_maturin_options()
    configured = options.get(
        "editable-profile" if editable else "profile"
    ) or options.get("profile")
    if configured:
        return ["--profile", configured]
    # maturin's own default: release wheels, dev editables.
    return [] if editable else ["--release"]


def _stage_bundle_bins(
    wheel_directory: str,
    filename: str,
    config_settings: Optional[dict],
    *,
    editable: bool,
) -> str:
    """Build `[tool.soldr.pep517] bundle-bins` and stage them into the wheel."""
    helper = _sibling_module("_bundle_bins")
    entries = _project_bundle_bins()
    if not entries:
        return filename
    env = _prep_env(config_settings, editable=editable)
    target_args = _target_args(config_settings)
    if not target_args:
        # Same interpreter maturin was given, so PyO3's build script resolves
        # an identical configuration and Cargo reuses the extension's units.
        env.setdefault("PYO3_PYTHON", sys.executable)
    manifest = _project_maturin_options().get("manifest-path")
    manifest_path = _project_root() / manifest if manifest else None
    profile_args = _bundle_bin_profile_args(config_settings, editable=editable)

    def build(entry: Any) -> Path:
        command = helper.cargo_build_command(
            entry,
            manifest_path=manifest_path,
            profile_args=profile_args,
            target_args=target_args,
        )
        with _hold_build_lease(env):
            return helper.build_bundle_bin(entry, command, env)

    added = helper.bundle_into_wheel(Path(wheel_directory, filename), entries, build)
    print(f"soldr PEP 517: bundled {', '.join(added)}", file=sys.stderr)
    return filename


def _newest_entry(directory: str, suffix: str, *, want_dir: bool) -> str:
    entries = []
    for name in os.listdir(directory):
        if not name.endswith(suffix):
            continue
        path = Path(directory, name)
        if want_dir and not path.is_dir():
            continue
        if not want_dir and not path.is_file():
            continue
        entries.append((path.stat().st_mtime, name))
    if not entries:
        kind = "directory" if want_dir else "file"
        raise RuntimeError(
            f"soldr build backend: no {suffix} {kind} produced in {directory}"
        )
    entries.sort(reverse=True)
    return entries[0][1]


# PEP 517 parameter names are API; frontends may call them by keyword.


def get_requires_for_build_wheel(config_settings: Optional[dict] = None):
    delegated = _delegate_hook(
        "get_requires_for_build_wheel",
        config_settings=config_settings,
        _config_settings=config_settings,
    )
    if delegated is not None:
        return delegated
    return []


def get_requires_for_build_sdist(config_settings: Optional[dict] = None):
    delegated = _delegate_hook(
        "get_requires_for_build_sdist",
        config_settings=config_settings,
        _config_settings=config_settings,
    )
    if delegated is not None:
        return delegated
    return []


def get_requires_for_build_editable(config_settings: Optional[dict] = None):
    delegated = _delegate_hook(
        "get_requires_for_build_editable",
        config_settings=config_settings,
        _config_settings=config_settings,
    )
    if delegated is not None:
        return delegated
    return []


def prepare_metadata_for_build_wheel(
    metadata_directory: str,
    config_settings: Optional[dict] = None,
) -> str:
    delegated = _delegate_hook(
        "prepare_metadata_for_build_wheel",
        metadata_directory,
        config_settings=config_settings,
        _config_settings=config_settings,
    )
    if delegated is not None:
        return delegated
    target_args = _target_args(config_settings)
    _maturin_pep517(
        "write-dist-info",
        "--metadata-directory",
        metadata_directory,
        "--interpreter",
        sys.executable,
        *target_args,
        config_settings=config_settings,
    )
    return _newest_entry(metadata_directory, ".dist-info", want_dir=True)


def prepare_metadata_for_build_editable(
    metadata_directory: str,
    config_settings: Optional[dict] = None,
) -> str:
    delegated = _delegate_hook(
        "prepare_metadata_for_build_editable",
        metadata_directory,
        fallback="prepare_metadata_for_build_wheel",
        config_settings=config_settings,
        _config_settings=config_settings,
    )
    if delegated is not None:
        return delegated
    return prepare_metadata_for_build_wheel(metadata_directory, config_settings)


def build_wheel(
    wheel_directory: str,
    config_settings: Optional[dict] = None,
    metadata_directory: Optional[str] = None,
) -> str:
    started_at = time.perf_counter()
    context = _wheel_cache_context("wheel", config_settings, metadata_directory)
    cached = _wheel_cache_restore(context, wheel_directory)
    if cached is not None:
        _emit_wheel_cache_event("wheel", config_settings, "hit", context)
        _emit_wheel_cache_hit(
            "wheel", config_settings, time.perf_counter() - started_at
        )
        return cached
    _emit_wheel_cache_event("wheel", config_settings, "miss", context)
    delegated = _delegate_hook(
        "build_wheel",
        wheel_directory,
        config_settings=config_settings,
        metadata_directory=metadata_directory,
        _config_settings=config_settings,
    )
    if delegated is not None:
        return _wheel_cache_finish(
            "wheel", wheel_directory, config_settings, metadata_directory, delegated
        )
    # maturin pep517 build-wheel does not accept --metadata-directory;
    # the dist-info is regenerated, which PEP 517 explicitly permits.
    target_args = _target_args(config_settings)
    _maturin_pep517(
        "build-wheel",
        "--interpreter",
        sys.executable,
        "--out",
        wheel_directory,
        *_profile_args(config_settings),
        *target_args,
        build_label="wheel",
        config_settings=config_settings,
    )
    return _wheel_cache_finish(
        "wheel",
        wheel_directory,
        config_settings,
        metadata_directory,
        _stage_bundle_bins(
            wheel_directory,
            _newest_entry(wheel_directory, ".whl", want_dir=False),
            config_settings,
            editable=False,
        ),
    )


def build_editable(
    wheel_directory: str,
    config_settings: Optional[dict] = None,
    metadata_directory: Optional[str] = None,
) -> str:
    started_at = time.perf_counter()
    context = _wheel_cache_context("editable", config_settings, metadata_directory)
    cached = _wheel_cache_restore(context, wheel_directory)
    if cached is not None:
        _emit_wheel_cache_event("editable", config_settings, "hit", context)
        _emit_wheel_cache_hit(
            "editable", config_settings, time.perf_counter() - started_at
        )
        return cached
    _emit_wheel_cache_event("editable", config_settings, "miss", context)
    delegated = _delegate_hook(
        "build_editable",
        wheel_directory,
        config_settings=config_settings,
        metadata_directory=metadata_directory,
        _config_settings=config_settings,
    )
    if delegated is not None:
        return _wheel_cache_finish(
            "editable", wheel_directory, config_settings, metadata_directory, delegated
        )
    target_args = _target_args(config_settings)
    _maturin_pep517(
        "build-wheel",
        "--interpreter",
        sys.executable,
        "--out",
        wheel_directory,
        "--editable",
        *_profile_args(config_settings, editable=True),
        *target_args,
        build_label="editable wheel",
        config_settings=config_settings,
        editable=True,
    )
    return _wheel_cache_finish(
        "editable",
        wheel_directory,
        config_settings,
        metadata_directory,
        _stage_bundle_bins(
            wheel_directory,
            _newest_entry(wheel_directory, ".whl", want_dir=False),
            config_settings,
            editable=True,
        ),
    )


def build_sdist(
    sdist_directory: str,
    config_settings: Optional[dict] = None,
) -> str:
    delegated = _delegate_hook(
        "build_sdist",
        sdist_directory,
        config_settings=config_settings,
        _config_settings=config_settings,
    )
    if delegated is not None:
        return delegated
    _maturin_pep517(
        "write-sdist",
        "--sdist-directory",
        sdist_directory,
        config_settings=config_settings,
    )
    filename = _newest_entry(sdist_directory, ".tar.gz", want_dir=False)
    entries = _project_bundle_bins()
    if entries:
        # soldr#3239 / zackees/soldr#3444: maturin's sdist trims the root
        # workspace Cargo.toml to the Cargo dependency graph reachable from
        # `manifest-path`, which drops a `bundle-bins` package that has no
        # dependency edge to the extension crate even though its files are
        # still copied in. Patch the trimmed `members` array back so a
        # later `soldr build --package <it>` against the extracted sdist
        # can resolve the workspace.
        helper = _sibling_module("_bundle_bins")
        helper.patch_sdist_workspace_members(Path(sdist_directory, filename), entries)
    return filename
