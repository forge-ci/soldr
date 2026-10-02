#!/usr/bin/env python3
"""Create the release tag and the draft GitHub Release, idempotently.

The two mutating steps of the publish job, extracted from release-auto.yml
(soldr#2469 step 2.2). Both were inline bash: check-then-act, with a
`GITHUB_STEP_SUMMARY` block emitting copy-pasteable recovery commands when the
token cannot perform the mutation (soldr#1252).

Retries preserve immutable identity: an existing tag must peel to the exact
candidate and existing assets must match local name, size, and SHA-256. Missing
assets may be added; conflicting assets are never replaced. These primitives
require the caller's frozen candidate and pretag approval; they do not supply
that approval themselves.

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
import re
import os
import subprocess
import sys
from pathlib import Path
from typing import Callable, Sequence

Runner = Callable[[Sequence[str]], subprocess.CompletedProcess]

BLOCKED_NOTE = (
    "Registry publishes are blocked until the GitHub release tag/assets "
    "can be created. See soldr#1252."
)


def default_runner(args: Sequence[str]) -> subprocess.CompletedProcess:
    """Run a command, capturing output; never raises on non-zero."""
    return subprocess.run(list(args), capture_output=True, text=True, check=False)


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
            f"gh run download {run_id} --repo {repo} "
            "--pattern 'release-soldr-*' --dir dist",
            f"gh release create {tag} dist/* --repo {repo} "
            f"--draft --verify-tag --generate-notes --target {sha} --title {tag}",
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


def validate_identity(repo: str, tag: str, sha: str) -> None:
    """Never substitute a branch or abbreviated commit for the frozen candidate."""
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
        raise ValueError("invalid release repository")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+-]*", tag) or ".." in tag:
        raise ValueError("invalid immutable release tag")
    if not re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", sha):
        raise ValueError("candidate must be an exact 40- or 64-character commit SHA")


def api_document(run: Runner, endpoint: str, *, pages: bool = False):
    args = ["gh", "api", endpoint]
    if pages:
        args.extend(["--paginate", "--slurp"])
    result = run(args)
    if result.returncode != 0:
        # Auth/rate-limit/server failures are not evidence of absence.
        if not pages and re.search(r"\bHTTP 404\b", result.stderr or ""):
            return None
        raise ValueError(f"GitHub observation failed: {gh_output_excerpt(result)}")
    try:
        document = json.loads(result.stdout)
        if document is None:
            raise ValueError("GitHub observation returned null instead of proof")
        return document
    except (TypeError, json.JSONDecodeError) as error:
        raise ValueError("GitHub observation returned no valid JSON proof") from error


def matching_tag(repo: str, tag: str, sha: str, run: Runner) -> bool:
    ref = api_document(run, f"repos/{repo}/git/ref/tags/{tag}")
    if ref is None:
        return False
    if not isinstance(ref, dict) or ref.get("ref") != f"refs/tags/{tag}":
        raise ValueError("tag reference identity mismatch")
    obj = ref.get("object")
    seen = set()
    for _ in range(16):
        if (
            not isinstance(obj, dict)
            or not isinstance(obj.get("sha"), str)
            or not re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", obj["sha"])
        ):
            raise ValueError("tag target lacks exact object identity")
        oid = obj["sha"]
        if obj.get("type") == "commit":
            if oid != sha:
                raise ValueError("immutable tag points at a different candidate")
            return True
        if obj.get("type") != "tag" or oid in seen:
            raise ValueError("tag target is not a commit or bounded annotated tag")
        seen.add(oid)
        peeled = api_document(run, f"repos/{repo}/git/tags/{oid}")
        if not isinstance(peeled, dict) or peeled.get("sha") != oid:
            raise ValueError("annotated tag object identity mismatch")
        obj = peeled.get("object")
    raise ValueError("annotated tag nesting exceeds bound")


def ensure_tag(repo: str, tag: str, sha: str, run: Runner) -> int:
    """Create only an absent tag, and prove its exact peeled target on retry."""
    run = logged(run)
    validated = False
    try:
        validate_identity(repo, tag, sha)
        validated = True
        if matching_tag(repo, tag, sha, run):
            print(f"tag {tag} already matches immutable candidate {sha}")
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
        if not matching_tag(repo, tag, sha, run):
            raise ValueError(
                f"tag creation lacks exact target proof: {gh_output_excerpt(created)}"
            )
        return 0
    except (ValueError, OSError) as error:
        print(f"::error::Immutable tag prerequisite failed: {error}. {BLOCKED_NOTE}")
        recovery = tag_recovery_summary(repo, tag, sha) if validated else ""
        append_summary(recovery + f"\nRefusal: {error}\n")
        return 1


def expected_assets(assets: list[str]) -> dict[str, tuple[int, str]]:
    expected = {}
    for asset in assets:
        path = Path(asset)
        if path.is_symlink() or not re.fullmatch(
            r"[A-Za-z0-9][A-Za-z0-9._+-]*", path.name
        ):
            raise ValueError("release assets require safe regular filenames")
        digest = hashlib.sha256()
        size = 0
        with path.open("rb") as handle:
            for block in iter(lambda: handle.read(1 << 20), b""):
                size += len(block)
                digest.update(block)
        expected[path.name] = (size, "sha256:" + digest.hexdigest())
    return expected


def release_document(repo: str, tag: str, run: Runner):
    release = api_document(run, f"repos/{repo}/releases/tags/{tag}")
    if release is None:
        return None
    if (
        not isinstance(release, dict)
        or release.get("tag_name") != tag
        or type(release.get("id")) is not int
        or release["id"] <= 0
        or type(release.get("draft")) is not bool
    ):
        raise ValueError("release identity mismatch")
    return release


def missing_assets(repo: str, release: dict, expected: dict, run: Runner) -> set[str]:
    pages = api_document(
        run, f"repos/{repo}/releases/{release['id']}/assets?per_page=100", pages=True
    )
    if (
        not isinstance(pages, list)
        or not pages
        or any(not isinstance(page, list) for page in pages)
    ):
        raise ValueError("asset pagination returned no complete list proof")
    seen = set()
    identities = set()
    for page in pages:
        for asset in page:
            if not isinstance(asset, dict):
                raise ValueError("malformed release asset")
            name = asset.get("name")
            identity = asset.get("id")
            if (
                not isinstance(name, str)
                or name not in expected
                or name in seen
                or type(identity) is not int
                or identity <= 0
                or identity in identities
                or asset.get("state") != "uploaded"
                or type(asset.get("size")) is not int
                or (asset["size"], asset.get("digest")) != expected[name]
            ):
                raise ValueError(
                    f"immutable release asset conflict or missing SHA-256 proof: {name}"
                )
            seen.add(name)
            identities.add(identity)
    return set(expected) - seen


def create_draft_release(
    repo: str, tag: str, sha: str, *, run_id: str, dist: Path, run: Runner
) -> int:
    """Reconcile only missing assets; never replace immutable existing bytes."""
    run = logged(run)
    write_output("created", "false")
    validated = False
    try:
        validate_identity(repo, tag, sha)
        if not re.fullmatch(r"[0-9]+", run_id):
            raise ValueError("invalid release run identity")
        validated = True
        assets = release_assets(dist)
        expected = expected_assets(assets)
        if not matching_tag(repo, tag, sha, run):
            raise ValueError("release requires an existing exact candidate tag")
        release = release_document(repo, tag, run)
        if release is None:
            if not matching_tag(repo, tag, sha, run):
                raise ValueError("candidate tag disappeared before draft creation")
            changed = run(
                [
                    "gh",
                    "release",
                    "create",
                    tag,
                    *assets,
                    "--repo",
                    repo,
                    "--draft",
                    "--verify-tag",
                    "--generate-notes",
                    "--target",
                    sha,
                    "--title",
                    tag,
                ]
            )
        else:
            missing = missing_assets(repo, release, expected, run)
            if missing:
                if release["draft"] is not True:
                    raise ValueError(
                        "published release is incomplete; missing assets cannot be exposed by retry"
                    )
                # Verify candidate again immediately before any mutation.
                if not matching_tag(repo, tag, sha, run):
                    raise ValueError("candidate tag disappeared before upload")
                current = release_document(repo, tag, run)
                if (
                    current is None
                    or current["id"] != release["id"]
                    or current["draft"] is not True
                ):
                    raise ValueError(
                        "release is no longer the same draft before upload"
                    )
                changed = run(
                    [
                        "gh",
                        "release",
                        "upload",
                        tag,
                        *[asset for asset in assets if Path(asset).name in missing],
                        "--repo",
                        repo,
                    ]
                )
            else:
                changed = None
        if changed is not None and changed.returncode != 0:
            raise ValueError(
                f"GitHub release mutation failed: {gh_output_excerpt(changed)}"
            )
        if not matching_tag(repo, tag, sha, run):
            raise ValueError("candidate tag disappeared after release reconciliation")
        final = release_document(repo, tag, run)
        if final is None or (release is not None and final["id"] != release["id"]):
            raise ValueError("release identity changed during reconciliation")
        if missing_assets(repo, final, expected, run):
            raise ValueError("release reconciliation left missing expected assets")
        # Also detect local artifact changes during upload/retry.
        if release_assets(dist) != assets or expected_assets(assets) != expected:
            raise ValueError("local expected assets changed during reconciliation")
        write_output("created", "true")
        return 0
    except (ValueError, OSError) as error:
        print(
            f"::error::Immutable release prerequisite failed: {error}. {BLOCKED_NOTE}"
        )
        recovery = release_recovery_summary(repo, tag, sha, run_id) if validated else ""
        append_summary(recovery + f"\nRefusal: {error}\n")
        return 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["ensure-tag", "create-draft"])
    parser.add_argument("--tag", required=True)
    parser.add_argument("--sha", required=True)
    parser.add_argument(
        "--repo", default=os.environ.get("GITHUB_REPOSITORY", "zackees/soldr")
    )
    parser.add_argument("--dist", default="dist")
    parser.add_argument("--run-id", default=os.environ.get("GITHUB_RUN_ID", ""))
    args = parser.parse_args(argv)

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
