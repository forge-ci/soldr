#!/usr/bin/env python3
"""Create the release tag and the draft GitHub Release, idempotently.

The two mutating steps of the publish job, extracted from auto-release.yml
(soldr#2469 step 2.2). Both were inline bash: check-then-act, with a
`GITHUB_STEP_SUMMARY` block emitting copy-pasteable recovery commands when the
token cannot perform the mutation (soldr#1252).

Behavior is preserved exactly, including the check-first idempotency that makes
a rerun after a partial success safe -- an existing tag is a skip, and an
existing release is an asset re-upload with `--clobber` rather than a failure.

One thing genuinely changes. The inline steps passed `dist/*` to `gh`, relying
on the shell to expand it; here the glob is expanded in Python. That closes a
real hole: with no matches, bash passes the literal string `dist/*` through and
`gh` fails with a confusing "path not found" deep inside the upload, so an
empty `dist/` looked like a `gh` problem. `release_assets()` raises a named
error instead, and it is tested.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import random
import re
import subprocess
import sys
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Callable, Sequence

sys.path.insert(0, str(Path(__file__).resolve().parent))
# pylint: disable-next=wrong-import-position
import release_completeness  # noqa: E402 -- script-relative import

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    run_captured,
)

Runner = Callable[[Sequence[str]], subprocess.CompletedProcess]

BLOCKED_NOTE = (
    "Registry publishes are blocked until the GitHub release tag/assets "
    "can be created. See soldr#1252."
)


def default_runner(args: Sequence[str]) -> subprocess.CompletedProcess:
    """Run a command, capturing output; never raises on non-zero."""
    return run_captured(list(args), capture_output=True, text=True, check=False)


EXCERPT_CHARS = 500


def gh_output_excerpt(completed: subprocess.CompletedProcess) -> str:
    """One-line excerpt of a gh call's stderr (else stdout) for `::error::`.

    soldr#3385: the canned annotation used to drop gh's real reason
    (permission / rate limit / API error). Mirrors
    `stage_release_binaries.run_tool`, which folds output into its error.
    """
    text = (completed.stderr or "").strip() or (completed.stdout or "").strip()
    flat = " ".join(text.split())
    return flat[:EXCERPT_CHARS] if flat else "<no output>"


def gh_output_block(completed: subprocess.CompletedProcess) -> str:
    """Markdown block with gh's full captured output for the step summary."""
    return "\n".join(
        [
            f"gh exited with {completed.returncode}:",
            "```",
            (completed.stdout or "").rstrip(),
            (completed.stderr or "").rstrip(),
            "```",
            "",
        ]
    )


def forward_stderr(args: Sequence[str], completed: subprocess.CompletedProcess) -> None:
    """Always forward gh's stderr, prefixed, on success too (soldr#3389)."""
    prefix = " ".join(list(args)[:3])
    for line in (completed.stderr or "").splitlines():
        if line.strip():
            print(f"{prefix}: {line}", file=sys.stderr)


def logged(run: Runner) -> Runner:
    """Wrap a runner so every call's stderr is forwarded to our stderr."""

    def wrapper(args: Sequence[str]) -> subprocess.CompletedProcess:
        completed = run(args)
        forward_stderr(args, completed)
        return completed

    return wrapper


# soldr#3343: transient GitHub API / upload failures are retried; anything
# else (auth, permission, 404 probes, immutable-release 422, conflicts) is
# returned at once, so it fails fast instead of burning ten attempts.
RETRY_ATTEMPTS = 10
TRANSIENT = re.compile(
    r"HTTP 5\d\d|HTTP 429|rate limit|timed? ?out|timeout|connection reset"
    r"|connection refused|EOF|temporar(il)?y unavailable|bad gateway"
    r"|service unavailable|gateway time-?out",
    re.IGNORECASE,
)


def is_transient(completed: subprocess.CompletedProcess) -> bool:
    text = f"{completed.stderr or ''}\n{completed.stdout or ''}"
    return completed.returncode != 0 and TRANSIENT.search(text) is not None


def backoff_delay(attempt: int) -> float:
    """Bounded exponential backoff with full jitter: <= min(60, 2**attempt) s."""
    return random.uniform(0, min(60.0, 2.0**attempt))


def retrying(
    run: Runner,
    *,
    attempts: int = RETRY_ATTEMPTS,
    sleep: Callable[[float], None] | None = None,
) -> Runner:
    """Retry a call while it fails transiently, logging every attempt."""

    def wrapper(args: Sequence[str]) -> subprocess.CompletedProcess:
        completed = run(args)
        for attempt in range(1, attempts):
            if not is_transient(completed):
                return completed
            delay = backoff_delay(attempt)
            print(
                f"transient failure ({gh_output_excerpt(completed)}); "
                f"attempt {attempt + 1}/{attempts} of `{' '.join(list(args)[:3])}` "
                f"in {delay:.1f}s",
                file=sys.stderr,
            )
            (sleep or time.sleep)(delay)
            completed = run(args)
        return completed

    return wrapper


def release_assets(dist: Path) -> list[str]:
    """Files to upload, sorted for a deterministic command line.

    Raises when empty: the inline `dist/*` silently degraded to a literal
    argument here, which surfaced as an unrelated-looking `gh` error.
    """
    if not dist.is_dir():
        raise FileNotFoundError(f"release asset directory does not exist: {dist}")
    files = sorted(str(path) for path in dist.iterdir() if path.is_file())
    if not files:
        raise FileNotFoundError(f"no release assets found in {dist}")
    return files


def tag_recovery_summary(repo: str, tag: str, sha: str) -> str:
    """Copy-pasteable recovery for a tag the token could not create."""
    return "\n".join(
        [
            "## Manual release recovery needed (tag)",
            "```",
            f"gh api repos/{repo}/git/refs -X POST -f ref=refs/tags/{tag} -f sha={sha}",
            "```",
            "",
        ]
    )


def release_recovery_summary(repo: str, tag: str, sha: str, run_id: str) -> str:
    """Copy-pasteable recovery for a release the token could not create."""
    return "\n".join(
        [
            "## Manual release recovery needed (GitHub release)",
            "```",
            f"gh run download {run_id} --repo {repo} --pattern 'release-soldr-*' --dir dist",
            f"gh release create {tag} dist/* --repo {repo} "
            f"--generate-notes --target {sha} --title {tag}",
            "```",
            "",
        ]
    )


def append_summary(text: str) -> None:
    """Append to the job summary when GitHub provides one."""
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not path:
        return
    with open(path, "a", encoding="utf-8") as handle:
        handle.write(text)


def write_output(name: str, value: str) -> None:
    """Set a step output when GitHub provides the file."""
    path = os.environ.get("GITHUB_OUTPUT")
    if not path:
        return
    with open(path, "a", encoding="utf-8") as handle:
        handle.write(f"{name}={value}\n")


def ensure_tag(repo: str, tag: str, sha: str, run: Runner) -> int:
    """Create `tag` at `sha` unless it already exists."""
    run = retrying(logged(run))
    if run(["gh", "api", f"repos/{repo}/git/refs/tags/{tag}"]).returncode == 0:
        print(f"tag {tag} already exists - skipping create")
        return 0
    created = run(
        [
            "gh",
            "api",
            f"repos/{repo}/git/refs",
            "-X",
            "POST",
            "-f",
            f"ref=refs/tags/{tag}",
            "-f",
            f"sha={sha}",
        ]
    )
    if created.returncode == 0:
        return 0
    print(
        f"::error::GITHUB_TOKEN could not create tag {tag} via /git/refs. "
        f"{BLOCKED_NOTE} gh output: {gh_output_excerpt(created)}"
    )
    append_summary(tag_recovery_summary(repo, tag, sha) + gh_output_block(created))
    return 1


def create_draft_release(
    repo: str, tag: str, sha: str, *, run_id: str, dist: Path, run: Runner
) -> int:
    """Create the draft release, or re-upload assets if it already exists."""
    assets = release_assets(dist)
    run = retrying(logged(run))
    if run(["gh", "release", "view", tag, "--repo", repo]).returncode == 0:
        print(f"release {tag} already exists; uploading/replacing assets")
        uploaded = run(
            ["gh", "release", "upload", tag, *assets, "--repo", repo, "--clobber"]
        )
        if uploaded.returncode != 0:
            # The inline version failed here too -- `set -e` aborted the step
            # on a non-zero `gh release upload`, so `created=true` was never
            # written. What it did not do was say anything: the step died with
            # gh's own message and no recovery block. This emits the same
            # guidance the create path already emitted, for the same reason.
            print(
                f"::error::GITHUB_TOKEN could not re-upload assets for {tag}. "
                f"{BLOCKED_NOTE} gh output: {gh_output_excerpt(uploaded)}"
            )
            write_output("created", "false")
            append_summary(
                release_recovery_summary(repo, tag, sha, run_id)
                + gh_output_block(uploaded)
            )
            return 1
        write_output("created", "true")
        return 0

    created = run(
        [
            "gh",
            "release",
            "create",
            tag,
            *assets,
            "--repo",
            repo,
            "--draft",
            "--generate-notes",
            "--target",
            sha,
            "--title",
            tag,
        ]
    )
    if created.returncode == 0:
        write_output("created", "true")
        return 0
    write_output("created", "false")
    print(
        f"::error::GITHUB_TOKEN could not create the GitHub release for {tag}. "
        "Registry publishes are blocked until the GitHub release exists with "
        "the expected assets. See soldr#1252. "
        f"gh output: {gh_output_excerpt(created)}"
    )
    append_summary(
        release_recovery_summary(repo, tag, sha, run_id) + gh_output_block(created)
    )
    return 1


def _verify_module():
    """The sibling asset gate, loaded on demand (it imports this directory)."""
    # pylint: disable-next=import-outside-toplevel
    import verify_github_release_assets

    return verify_github_release_assets


def publish_release(
    repo: str,
    tag: str,
    run: Runner,
    fetch: Callable[[str, str], dict] | None = None,
) -> int:
    """Verify the draft's full asset set, then make it public (soldr#2469 4.2).

    A published immutable release can no longer gain assets, so an incomplete
    draft must never be published.
    """
    verify = _verify_module()
    failures = verify.verify_release_assets(
        tag, (fetch or verify.fetch_release)(tag, repo), draft=True
    )
    if failures:
        print(f"::error::draft {tag} is incomplete; not publishing it")
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    published = retrying(logged(run))(
        ["gh", "release", "edit", tag, "--repo", repo, "--draft=false"]
    )
    if published.returncode == 0:
        return 0
    print(
        f"::error::could not publish the draft GitHub release {tag}. "
        f"gh output: {gh_output_excerpt(published)}"
    )
    append_summary(gh_output_block(published))
    return 1


INFO_SCHEMA = 1


@dataclass(frozen=True)
class ReleaseFile:
    name: str
    bytes: int
    sha256: str


@dataclass(frozen=True)
class ReleaseInfo:
    """The release-level record of the artifact set (soldr#3343).

    One document naming the version, candidate SHA, immutable tag, the
    target-contract revision, and every artifact's size and SHA-256. It
    complements the per-archive manifest.json and SHA256SUMS; a resumed or
    re-uploaded release must match it byte for byte.
    """

    schema_version: int
    version: str
    tag: str
    commit_sha: str
    run_id: str
    target_contract_sha256: str
    targets: list[str]
    files: list[ReleaseFile]


def release_info(
    dist: Path,
    *,
    version: str,
    sha: str,
    contract: Path,
    triples: list[str],
    run_id: str,
) -> ReleaseInfo:
    files = [
        ReleaseFile(
            name=Path(name).name,
            bytes=Path(name).stat().st_size,
            sha256=hashlib.sha256(Path(name).read_bytes()).hexdigest(),
        )
        for name in release_assets(dist)
    ]
    return ReleaseInfo(
        schema_version=INFO_SCHEMA,
        version=version,
        tag=version if version.startswith("v") else f"v{version}",
        commit_sha=sha,
        run_id=run_id,
        target_contract_sha256=hashlib.sha256(contract.read_bytes()).hexdigest(),
        targets=triples,
        files=files,
    )


def info_name(version: str) -> str:
    return f"soldr-{version}-info.json"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "command", choices=["ensure-tag", "create-draft", "write-info", "publish"]
    )
    parser.add_argument("--tag", required=True)
    parser.add_argument("--sha", required=True)
    parser.add_argument(
        "--repo", default=os.environ.get("GITHUB_REPOSITORY", "zackees/soldr")
    )
    parser.add_argument("--dist", default="dist")
    parser.add_argument("--run-id", default=os.environ.get("GITHUB_RUN_ID", ""))
    args = parser.parse_args(argv)

    if args.command == "write-info":
        dist = Path(args.dist)
        info = release_info(
            dist,
            version=args.tag,
            sha=args.sha,
            contract=release_completeness.CONTRACT,
            triples=release_completeness.included_triples(),
            run_id=args.run_id,
        )
        target = dist / info_name(args.tag)
        target.write_text(json.dumps(asdict(info), indent=2) + "\n", encoding="utf-8")
        print(f"wrote {target} ({len(info.files)} files)")
        return 0
    if args.command == "publish":
        return publish_release(args.repo, args.tag, default_runner)
    if args.command == "ensure-tag":
        return ensure_tag(args.repo, args.tag, args.sha, default_runner)
    try:
        return create_draft_release(
            args.repo,
            args.tag,
            args.sha,
            run_id=args.run_id,
            dist=Path(args.dist),
            run=default_runner,
        )
    except FileNotFoundError as error:
        print(f"::error::{error}", file=sys.stderr)
        write_output("created", "false")
        return 1


if __name__ == "__main__":
    sys.exit(main())
