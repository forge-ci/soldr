#!/usr/bin/env python3
"""Shared fail-closed HTTP policy and origin list for soldr-toolchain consumers."""

from __future__ import annotations

import http.client
import json
import os
import sys
import urllib.error
import urllib.request
from typing import Any, Callable, Mapping, TextIO, TypeVar
from urllib.parse import urlsplit, urlunsplit

MAX_URL_BYTES = 8192


def validate_https_url(value: object, *, label: str) -> str:
    """Require a bounded, credential-free absolute HTTPS URL."""
    if not isinstance(value, str) or not value or len(value) > MAX_URL_BYTES:
        raise SystemExit(f"{label} has invalid URL length")
    if any(ord(char) < 32 or ord(char) == 127 for char in value):
        raise SystemExit(f"{label} contains control characters")
    try:
        parsed = urlsplit(value)
        hostname = parsed.hostname
        username = parsed.username
        password = parsed.password
        _ = parsed.port
    except ValueError as error:
        raise SystemExit(f"{label} is not a valid absolute URL: {error}") from error
    if (
        parsed.scheme != "https"
        or hostname is None
        or username is not None
        or password is not None
    ):
        raise SystemExit(f"{label} must be credential-free absolute HTTPS")
    return value


def display_url(value: object) -> str:
    """Render a URL for logs without userinfo, query credentials, or fragments."""
    if not isinstance(value, str):
        return "<invalid-url>"
    try:
        parsed = urlsplit(value)
        hostname = parsed.hostname
        port = parsed.port
    except ValueError:
        return "<invalid-url>"
    if not parsed.scheme or hostname is None:
        return "<invalid-url>"
    display_host = f"[{hostname}]" if ":" in hostname else hostname
    netloc = f"{display_host}:{port}" if port is not None else display_host
    return urlunsplit((parsed.scheme, netloc, parsed.path, "", ""))


class SafeRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Retain the credential-free HTTPS policy across every redirect hop."""

    max_redirections = 5

    # urllib defines this six-argument callback; the override must match it.
    # pylint: disable-next=too-many-positional-arguments
    def redirect_request(
        self,
        req: urllib.request.Request,
        fp: Any,
        code: int,
        msg: str,
        headers: Any,
        newurl: str,
    ) -> urllib.request.Request | None:
        validate_https_url(newurl, label=f"redirect from {display_url(req.full_url)}")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


SAFE_URL_OPENER = urllib.request.build_opener(SafeRedirectHandler())


def open_url(request: urllib.request.Request, *, timeout: int) -> Any:
    """Open a request through the shared redirect-revalidating opener."""
    validate_https_url(request.full_url, label="request URL")
    return SAFE_URL_OPENER.open(request, timeout=timeout)


# --- Toolchain origin list (redirect manifest + failover) -------------------
#
# Mirrors crates/soldr-fetch/src/fetch/toolchain_origin.rs; setup-soldr
# implements the same rules. Explicit origins skip the redirect manifest;
# otherwise the manifest (short timeout) names preferred origins, the
# built-in list is appended, and a fetch moves to the next origin only on a
# network error, timeout, HTTP 404 or 5xx. Content errors never fail over.

FORGE_CI_TOOLCHAIN_ORIGIN = "https://forge-ci.github.io/soldr-toolchain"
LEGACY_TOOLCHAIN_ORIGIN = "https://zackees.github.io/soldr-toolchain"
BUILTIN_TOOLCHAIN_ORIGINS = (FORGE_CI_TOOLCHAIN_ORIGIN, LEGACY_TOOLCHAIN_ORIGIN)
DEFAULT_TOOLCHAIN_ORIGIN = BUILTIN_TOOLCHAIN_ORIGINS[0]
DEFAULT_TOOLCHAIN_REDIRECT_URL = "https://forge-ci.github.io/soldr-redirect.v1.json"
TOOLCHAIN_ORIGIN_ENV_VAR = "SOLDR_TOOLCHAIN_ORIGIN"
TOOLCHAIN_REDIRECT_URL_ENV_VAR = "SOLDR_TOOLCHAIN_REDIRECT_URL"
TOOLCHAIN_REDIRECT_ENV_VAR = "SOLDR_TOOLCHAIN_REDIRECT"
REDIRECT_TIMEOUT_SECS = 5
MAX_REDIRECT_BODY_BYTES = 64 * 1024
MAX_REDIRECT_ORIGINS = 16
_OFF_VALUES = frozenset({"0", "false", "no", "off"})

T = TypeVar("T")


class OriginFetchError(SystemExit):
    """A toolchain fetch failed; ``failover`` says another origin may work."""

    def __init__(self, message: str, *, failover: bool) -> None:
        super().__init__(message)
        self.failover = failover


def status_fails_over(status: int) -> bool:
    """HTTP 404 and 5xx mean "this origin cannot serve it, another might"."""
    return status == 404 or 500 <= status <= 599


def is_failover_error(error: BaseException) -> bool:
    """Whether ``error`` is an unavailable origin rather than bad content."""
    if isinstance(error, OriginFetchError):
        return error.failover
    if isinstance(error, urllib.error.HTTPError):
        return status_fails_over(error.code)
    return isinstance(
        error,
        (
            urllib.error.URLError,
            TimeoutError,
            ConnectionError,
            http.client.IncompleteRead,
        ),
    )


def parse_redirect_manifest(raw: bytes) -> list[str] | None:
    """Return the manifest's origins, or None when it is not acceptable."""
    if len(raw) > MAX_REDIRECT_BODY_BYTES:
        return None
    try:
        payload = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return None
    if not isinstance(payload, dict):
        return None
    schema = payload.get("schema_version")
    if isinstance(schema, bool) or schema != 1:
        return None
    toolchain = payload.get("toolchain")
    origins = toolchain.get("origins") if isinstance(toolchain, dict) else None
    if not isinstance(origins, list) or not origins:
        return None
    if len(origins) > MAX_REDIRECT_ORIGINS:
        return None
    accepted: list[str] = []
    for origin in origins:
        try:
            validate_https_url(origin, label="redirect origin")
        except SystemExit:
            return None
        parsed = urlsplit(origin)
        if parsed.query or parsed.fragment:
            return None
        accepted.append(origin.rstrip("/"))
    return accepted


def effective_origins(manifest_origins: list[str]) -> list[str]:
    """Manifest origins, then the built-ins, de-duplicated in order."""
    result: list[str] = []
    for origin in [*manifest_origins, *BUILTIN_TOOLCHAIN_ORIGINS]:
        origin = origin.strip().rstrip("/")
        if origin not in result:
            result.append(origin)
    return result


def fetch_redirect_origins(url: str) -> list[str] | None:
    """One bounded attempt at the redirect manifest; never raises."""
    try:
        request = urllib.request.Request(url, headers={"Accept-Encoding": "identity"})
        with open_url(request, timeout=REDIRECT_TIMEOUT_SECS) as response:
            raw = response.read(MAX_REDIRECT_BODY_BYTES + 1)
    except (SystemExit, OSError, ValueError, http.client.HTTPException):
        return None
    return parse_redirect_manifest(raw)


def toolchain_origins(
    explicit: str | None = None,
    *,
    environ: Mapping[str, str] | None = None,
    fetch: Callable[[str], list[str] | None] = fetch_redirect_origins,
) -> list[str]:
    """Ordered origins to try for a soldr-toolchain file.

    ``explicit`` (a ``--origin`` argument) or ``SOLDR_TOOLCHAIN_ORIGIN`` wins
    and skips the redirect manifest. ``SOLDR_TOOLCHAIN_REDIRECT=0`` uses only
    the built-in list. Otherwise the manifest at ``SOLDR_TOOLCHAIN_REDIRECT_URL``
    (default ``DEFAULT_TOOLCHAIN_REDIRECT_URL``) is consulted; an unreachable
    or invalid manifest falls back to the built-in list.
    """
    env = os.environ if environ is None else environ
    chosen = (explicit or env.get(TOOLCHAIN_ORIGIN_ENV_VAR, "")).strip()
    if chosen:
        return [chosen.rstrip("/")]
    if env.get(TOOLCHAIN_REDIRECT_ENV_VAR, "").strip().lower() in _OFF_VALUES:
        return effective_origins([])
    url = env.get(TOOLCHAIN_REDIRECT_URL_ENV_VAR, "").strip()
    return effective_origins(fetch(url or DEFAULT_TOOLCHAIN_REDIRECT_URL) or [])


def with_origin_failover(
    origins: list[str],
    action: Callable[[str], T],
    *,
    label: str,
    stderr: TextIO | None = None,
) -> T:
    """Run ``action(origin)`` for each origin until one is not unavailable."""
    if not origins:
        raise SystemExit(f"no soldr-toolchain origin configured for {label}")
    for index, origin in enumerate(origins):
        try:
            return action(origin)
        except (SystemExit, OSError, http.client.HTTPException) as error:
            if index == len(origins) - 1 or not is_failover_error(error):
                raise
            print(
                f"{label} unavailable at {display_url(origin)}; "
                f"trying {display_url(origins[index + 1])}",
                file=stderr or sys.stderr,
            )
    raise AssertionError("origin loop exhausted without returning or raising")
