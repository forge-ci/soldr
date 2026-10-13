#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import json
import os
import re
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover
    # Python 3.10 has no tomllib; callers check for None. Not a constant
    # despite living at module scope, and the binding is deliberately
    # module-or-None.
    tomllib = None  # type: ignore[assignment]  # pylint: disable=invalid-name


def _normalize_list(value: Any) -> list[str]:
    if not value:
        return []
    if isinstance(value, str):
        return [part.strip() for part in value.split(",") if part.strip()]
    return [str(item).strip() for item in value if str(item).strip()]


@dataclass(frozen=True)
class ToolchainSpec:
    """The resolved rust toolchain request (channel, profile, extras)."""

    channel: str
    profile: str
    components: tuple[str, ...]
    targets: tuple[str, ...]
    source: str
    file_hash: str


def load_toolchain_spec(
    workspace: Path,
    toolchain_file: str,
    toolchain_override: str,
) -> ToolchainSpec:
    channel = "stable"
    profile = "minimal"
    components: list[str] = []
    targets: list[str] = []
    source = "default"
    file_hash = "none"

    if toolchain_file:
        path = workspace / toolchain_file
        if path.exists():
            source = str(path.relative_to(workspace))
            file_bytes = path.read_bytes()
            file_hash = hashlib.sha256(file_bytes).hexdigest()[:16]
            if tomllib is None:
                raise RuntimeError("python tomllib support is required for setup-soldr")
            data = tomllib.loads(file_bytes.decode("utf-8"))
            toolchain = data.get("toolchain", {})
            if isinstance(toolchain, dict):
                channel = str(toolchain.get("channel", channel))
                profile = str(toolchain.get("profile", profile))
                components = _normalize_list(toolchain.get("components"))
                targets = _normalize_list(toolchain.get("targets"))

    if toolchain_override:
        channel = toolchain_override.strip()
        source = "input"

    return ToolchainSpec(
        channel=channel,
        profile=profile,
        components=tuple(components),
        targets=tuple(targets),
        source=source,
        file_hash=file_hash,
    )


def _sanitize_fragment(value: str) -> str:
    return re.sub(r"[^A-Za-z0-9._-]+", "-", value).strip("-") or "default"


def _is_empty_or_latest(value: str) -> bool:
    normalized = value.strip().lower()
    return not normalized or normalized == "latest"


def _normalize_release_tag(value: str) -> str:
    cleaned = value.strip()
    if not cleaned:
        return ""
    return cleaned[1:] if cleaned.startswith("v") else cleaned


def resolve_latest_soldr_release(repo: str) -> str:
    """Resolve the latest published soldr release tag.

    Returns the resolved release tag (without a leading ``v``) when the
    GitHub API answers successfully. Returns an empty string on any
    failure so the caller can fall back to the literal ``latest`` label
    and keep working offline or under GitHub API outages.
    """

    url = f"https://api.github.com/repos/{repo.strip()}/releases/latest"
    headers = {
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
        "User-Agent": "setup-soldr-action",
    }
    token = _github_token_for_repo(repo)
    if token:
        headers["Authorization"] = f"Bearer {token}"
    try:
        request = urllib.request.Request(url, headers=headers)
        with urllib.request.urlopen(request, timeout=10) as response:
            payload = json.load(response)
    except (
        urllib.error.URLError,
        urllib.error.HTTPError,
        TimeoutError,
        ValueError,
        OSError,
    ):
        return ""
    tag = payload.get("tag_name")
    if not isinstance(tag, str):
        return ""
    return _normalize_release_tag(tag)


def _github_token_for_repo(repo: str) -> str:
    explicit = os.environ.get("SETUP_SOLDR_GITHUB_TOKEN", "").strip()
    if explicit:
        return explicit

    token = os.environ.get("GITHUB_TOKEN", "").strip()
    if not token:
        return ""

    current_repo = os.environ.get("GITHUB_REPOSITORY", "").strip().lower()
    if current_repo == repo.strip().lower():
        return token
    return ""


def normalize_bool_input(value: str, *, name: str, default: bool) -> bool:
    """Normalize a YAML-style boolean input. Empty means default; otherwise
    accept the canonical truthy/falsy strings GitHub Actions uses. Raises
    on invalid input so a typo doesn't silently flip policy.

    The accepted truthy/falsy spellings mirror those honored by
    `SOLDR_NATIVE_CACHE` on the runtime side (`crates/soldr-cli/src/
    native_cc.rs::env_is_falsy`) so the action input and the env-var
    opt-out agree.
    """
    if not value or not value.strip():
        return default
    normalized = value.strip().lower()
    if normalized in {"1", "true", "yes", "on"}:
        return True
    if normalized in {"0", "false", "no", "off"}:
        return False
    raise RuntimeError(
        f"invalid {name} value {value!r}; expected true/false (yes/no, 1/0, on/off)"
    )


def _write_env(name: str, value: str) -> None:
    output = os.environ.get("GITHUB_ENV")
    if not output:
        return
    with open(output, "a", encoding="utf-8") as fh:
        fh.write(f"{name}={value}\n")


def _write_path(value: str) -> None:
    output = os.environ.get("GITHUB_PATH")
    if not output:
        return
    with open(output, "a", encoding="utf-8") as fh:
        fh.write(f"{value}\n")


def _write_outputs(values: dict[str, str]) -> None:
    output = os.environ.get("GITHUB_OUTPUT")
    if not output:
        return
    with open(output, "a", encoding="utf-8") as fh:
        for key, value in values.items():
            if "\n" in value:
                # Use GitHub's heredoc delimiter form for multi-line outputs.
                delimiter = (
                    f"ghadelim_{hashlib.sha256(value.encode()).hexdigest()[:16]}"
                )
                fh.write(f"{key}<<{delimiter}\n{value}\n{delimiter}\n")
            else:
                fh.write(f"{key}={value}\n")


def main() -> None:
    workspace = Path(os.environ["ACTION_WORKSPACE"]).resolve()
    runner_temp = Path(os.environ.get("RUNNER_TEMP", workspace / ".tmp")).resolve()

    requested_cache_dir = os.environ.get("INPUT_CACHE_DIR", "").strip()
    cache_root = (
        Path(requested_cache_dir).expanduser().resolve()
        if requested_cache_dir
        else (runner_temp / "setup-soldr")
    )
    soldr_root = cache_root / "soldr"
    cargo_home = cache_root / "cargo"
    rustup_home = cache_root / "rustup"
    bin_dir = cache_root / "bin"
    shim_dir = cache_root / "shims"
    zccache_cache_dir = soldr_root / "cache" / "zccache"
    soldr_binary = "soldr.exe" if os.name == "nt" else "soldr"
    soldr_path = bin_dir / soldr_binary

    for path in (
        cache_root,
        soldr_root,
        soldr_root / "cache",
        soldr_root / "bin",
        cargo_home,
        cargo_home / "bin",
        rustup_home,
        bin_dir,
        shim_dir,
        zccache_cache_dir,
    ):
        path.mkdir(parents=True, exist_ok=True)

    toolchain = load_toolchain_spec(
        workspace=workspace,
        toolchain_file=os.environ.get("INPUT_TOOLCHAIN_FILE", "rust-toolchain.toml"),
        toolchain_override=os.environ.get("INPUT_TOOLCHAIN", ""),
    )

    soldr_repo = (
        os.environ.get("INPUT_REPO", "zackees/soldr").strip() or "zackees/soldr"
    )
    soldr_version = os.environ.get("INPUT_VERSION", "").strip()

    # When the caller asks for "latest" (explicit or implicit), resolve the
    # release tag before baking it into the cache key. Without this, the
    # setup cache records the literal string "latest" and warm runs keep
    # reusing whichever soldr/zccache bundle was first saved, even after a
    # newer release ships. Fall back to the literal label when GitHub is
    # unreachable so the action still works offline or on rate-limited
    # runners. See issue #214.
    resolved_soldr_version = soldr_version
    if _is_empty_or_latest(soldr_version):
        resolved_soldr_version = resolve_latest_soldr_release(soldr_repo)
    cache_key_soldr_version = _normalize_release_tag(resolved_soldr_version) or "latest"

    toolchain_signature = {
        "channel": toolchain.channel,
        "profile": toolchain.profile,
        "components": list(toolchain.components),
        "targets": list(toolchain.targets),
        "source": toolchain.source,
        "file_hash": toolchain.file_hash,
        "soldr_repo": soldr_repo,
        "soldr_version": cache_key_soldr_version,
    }
    digest = hashlib.sha256(
        json.dumps(toolchain_signature, sort_keys=True).encode("utf-8")
    ).hexdigest()[:16]
    runner_os = _sanitize_fragment(os.environ.get("ACTION_OS", os.name).lower())
    runner_arch = _sanitize_fragment(os.environ.get("ACTION_ARCH", "unknown").lower())
    cache_prefix = f"setup-soldr-v0-{runner_os}-{runner_arch}"
    cache_key = f"{cache_prefix}-{digest}"

    suffix = os.environ.get("INPUT_CACHE_KEY_SUFFIX", "").strip()
    sanitized_suffix = _sanitize_fragment(suffix) if suffix else ""
    if suffix:
        cache_key = f"{cache_key}-{sanitized_suffix}"

    # Build-artifact cache (Soldr-owned zccache compilation cache).
    # Key shape: setup-soldr-buildcache-v1-{os}-{arch}-{toolchain-digest}-{sha}.
    # Restore falls back through the same toolchain lineage and then any
    # OS+arch cache, letting GitHub's own-branch -> PR base -> default branch
    # restore order provide parent -> child lineage without user config.
    github_sha = os.environ.get("GITHUB_SHA", "").strip() or "nosha"
    build_cache_prefix = f"setup-soldr-buildcache-v1-{runner_os}-{runner_arch}"
    build_cache_toolchain_prefix = f"{build_cache_prefix}-{digest}-"
    build_cache_key = f"{build_cache_toolchain_prefix}{github_sha}"

    target_cache_deprecated = os.environ.get(
        "INPUT_TARGET_CACHE", ""
    ).strip().lower() in {"1", "true", "yes", "on"} or os.environ.get(
        "INPUT_TARGET_CACHE_MODE", ""
    ).strip().lower() not in {
        "",
        "off",
    }
    if target_cache_deprecated:
        print(
            "setup-soldr: target-cache / target-cache-mode are deprecated no-ops; "
            "soldr#2996 removed the target cache (soldr cook + zccache own "
            "compiler caching).",
            flush=True,
        )
    target_cache_enabled = False
    target_cache_effective_mode = "off"

    if suffix:
        build_cache_key = f"{build_cache_key}-{sanitized_suffix}"

    # Native compiler cache policy (issue #311). Default-on at the action
    # level mirrors the runtime default-on in `soldr-cli/src/native_cc.rs`.
    # When the input is explicitly false, write SOLDR_NATIVE_CACHE=0 to
    # GITHUB_ENV so every later `soldr cargo ...` step in the job skips
    # CC / CXX wrapping. When true, we don't write the env var: the
    # runtime's unset-is-truthy policy keeps native caching on.
    native_cache_enabled = normalize_bool_input(
        os.environ.get("INPUT_NATIVE_CACHE", "true"),
        name="native-cache",
        default=True,
    )
    if not native_cache_enabled:
        _write_env("SOLDR_NATIVE_CACHE", "0")

    _write_env("SOLDR_CACHE_DIR", str(soldr_root))
    _write_env("CARGO_HOME", str(cargo_home))
    _write_env("RUSTUP_HOME", str(rustup_home))
    _write_env("ZCCACHE_CACHE_DIR", str(zccache_cache_dir))
    _write_env("SETUP_SOLDR_TOOLCHAIN_CHANNEL", toolchain.channel)
    _write_env("SETUP_SOLDR_TOOLCHAIN_PROFILE", toolchain.profile)
    _write_env(
        "SETUP_SOLDR_TOOLCHAIN_COMPONENTS", json.dumps(list(toolchain.components))
    )
    _write_env("SETUP_SOLDR_TOOLCHAIN_TARGETS", json.dumps(list(toolchain.targets)))
    if os.environ.get("INPUT_TRUST_MODE", "").strip():
        _write_env("SOLDR_TRUST_MODE", os.environ["INPUT_TRUST_MODE"].strip())

    _write_path(str(bin_dir))
    _write_path(str(cargo_home / "bin"))

    _write_outputs(
        {
            "cache_root": str(cache_root),
            "cache_key": cache_key,
            "cache_restore_prefix": f"{cache_prefix}-",
            "build_cache_key": build_cache_key,
            "build_cache_restore_key_toolchain": build_cache_toolchain_prefix,
            "build_cache_restore_key_os_arch": f"{build_cache_prefix}-",
            "build_cache_path": str(zccache_cache_dir),
            "target_cache_enabled": str(target_cache_enabled).lower(),
            "target_cache_mode": target_cache_effective_mode,
            "native_cache_enabled": str(native_cache_enabled).lower(),
            "soldr_root": str(soldr_root),
            "cargo_home": str(cargo_home),
            "rustup_home": str(rustup_home),
            "bin_dir": str(bin_dir),
            "shim_dir": str(shim_dir),
            "soldr_path": str(soldr_path),
            "soldr_repo": soldr_repo,
            "soldr_version_requested": soldr_version,
            "soldr_version_resolved": resolved_soldr_version,
            "toolchain_channel": toolchain.channel,
            "toolchain_profile": toolchain.profile,
            "toolchain_source": toolchain.source,
            "toolchain": toolchain.channel,
        }
    )


if __name__ == "__main__":
    main()
