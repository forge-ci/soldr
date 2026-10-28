from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

# Use regular-file capture without an installed Python dependency.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "src"))
# pylint: disable-next=wrong-import-position
from soldr._process import (  # noqa: E402 -- source-relative bootstrap precedes this import
    checked_output,
)

REPO_ROOT = Path(__file__).resolve().parents[2]
SETUP_SOLDR_REPO = "https://github.com/zackees/setup-soldr.git"
SETUP_SOLDR_RELEASE_REF = "refs/tags/v0"
OLD_SETUP_SOLDR_SHA = "1937c19529f3690df5553a36dd33f39ccb20b070"
SETUP_SOLDR_V0_2_SHA = "13b2e37f3ee8dc6867f08d3b2fe49ece4783dba2"
SETUP_SOLDR_V0_4_3_SHA = "6c48a0946390a3520a853e30fe417db7465b9119"
SETUP_SOLDR_V0_9_12_SHA = "cca74625e75e70b56f1805fa6eeee9069f945d48"
SETUP_SOLDR_V0_9_73_SHA = "62d1596b70168e422156f12273a2ed476d3a16dc"
# The pin that predated setup-soldr#502 (installing `latest` picked the
# `-symbols` debug sidecar archive for soldr 0.9.12+ and failed).
SETUP_SOLDR_PRE_502_SHA = "5f1f68dcb8377818413c28ce52214261ae8ff771"
# The pin between setup-soldr#502 and #504 (readiness lookup still
# unauthenticated; v0 moved to bb28e96d when #504 merged, soldr#3101).
SETUP_SOLDR_PRE_504_SHA = "850244f88d111f6cc5dfe9c1018c20fdd9493ecb"
# The v0.9.80 pin, before setup-soldr#539/#541 made Dylint save its caches.
SETUP_SOLDR_V0_9_80_SHA = "4df8db93438594f50505574d9dc8117505d33362"
SETUP_SOLDR_USE_RE = re.compile(
    r"\buses:\s*(zackees/setup-soldr(?:/[A-Za-z0-9_.-]+)?)@([^\s#]+)"
)
FULL_SHA_RE = re.compile(r"^[0-9a-f]{40}$")
AUTOFIX_BRANCH_PREFIX = "ci/update-setup-soldr-v0"
AUTOFIX_ISSUE_TITLE = "Update setup-soldr workflow pin to current v0"
GIT_LS_REMOTE_TIMEOUT_SECS = 300
SUBPROCESS_TIMEOUT_SECS = 300


def resolve_setup_soldr_release_sha() -> str:
    output = checked_output(
        [
            "git",
            "ls-remote",
            "--exit-code",
            SETUP_SOLDR_REPO,
            SETUP_SOLDR_RELEASE_REF,
            f"{SETUP_SOLDR_RELEASE_REF}^{{}}",
        ],
        encoding="utf-8",
        timeout=GIT_LS_REMOTE_TIMEOUT_SECS,
    )
    assert isinstance(output, str), "UTF-8 capture must return text"
    refs: dict[str, str] = {}
    for line in output.splitlines():
        sha, ref = line.split(maxsplit=1)
        refs[ref] = sha
    return refs.get(f"{SETUP_SOLDR_RELEASE_REF}^{{}}", refs[SETUP_SOLDR_RELEASE_REF])


def executable_workflow_lines(text: str) -> list[str]:
    return [
        line.strip()
        for line in text.splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]


def setup_soldr_refs(text: str) -> list[str]:
    return [
        match.group(2)
        for line in executable_workflow_lines(text)
        if (match := SETUP_SOLDR_USE_RE.search(line))
    ]


def workflow_paths(repo_root: Path = REPO_ROOT) -> list[Path]:
    """Return every GitHub Actions workflow, regardless of YAML extension."""
    workflows_dir = repo_root / ".github" / "workflows"
    return sorted(
        [*workflows_dir.glob("*.yml"), *workflows_dir.glob("*.yaml")],
        key=lambda path: path.name,
    )


def workflow_text(repo_root: Path = REPO_ROOT) -> str:
    return "\n".join(
        path.read_text(encoding="utf-8") for path in workflow_paths(repo_root)
    )


def verify_setup_soldr_pins(repo_root: Path = REPO_ROOT) -> None:
    text = workflow_text(repo_root)
    refs = setup_soldr_refs(text)
    current_release_sha = resolve_setup_soldr_release_sha()
    errors: list[str] = []

    for old_sha in [
        OLD_SETUP_SOLDR_SHA,
        SETUP_SOLDR_V0_2_SHA,
        SETUP_SOLDR_V0_4_3_SHA,
        SETUP_SOLDR_V0_9_12_SHA,
        SETUP_SOLDR_V0_9_73_SHA,
        SETUP_SOLDR_PRE_502_SHA,
        SETUP_SOLDR_PRE_504_SHA,
        SETUP_SOLDR_V0_9_80_SHA,
    ]:
        if old_sha in text:
            errors.append(f"stale setup-soldr SHA remains in workflows: {old_sha}")

    if not refs:
        errors.append("no zackees/setup-soldr workflow uses were found")

    for ref in refs:
        if not FULL_SHA_RE.fullmatch(ref):
            errors.append(
                f"zackees/setup-soldr must be pinned to a full SHA under repo ruleset: {ref}"
            )
        elif ref != current_release_sha:
            errors.append(
                f"zackees/setup-soldr pin {ref} does not match current v0 {current_release_sha}"
            )

    if errors:
        if truthy_env("SETUP_SOLDR_PIN_AUTOFIX"):
            try:
                create_or_update_pin_pr(repo_root, current_release_sha, errors)
            # Best-effort autofix: any failure here must be reported
            # alongside the original pin error, never replace it.
            # pylint: disable-next=broad-exception-caught
            except Exception as exc:
                errors.append(
                    f"failed to create setup-soldr pin update issue/PR: {exc}"
                )
        raise SystemExit("\n".join(errors))

    print(f"zackees/setup-soldr workflow pins match v0: {current_release_sha}")


def truthy_env(name: str) -> bool:
    value = os.environ.get(name, "").strip().lower()
    return value not in {"", "0", "false", "no", "off"}


def create_or_update_pin_pr(
    repo_root: Path, current_release_sha: str, errors: list[str]
) -> None:
    owner_repo = os.environ["GITHUB_REPOSITORY"]
    owner, repo = owner_repo.split("/", 1)
    token = os.environ["GITHUB_TOKEN"]
    run_url = github_run_url()
    branch = f"{AUTOFIX_BRANCH_PREFIX}-{current_release_sha[:12]}"

    update_workflow_pins(repo_root, current_release_sha)
    ensure_git_identity(repo_root)
    run(["git", "checkout", "-B", branch], cwd=repo_root)
    run(["git", "add", ".github/workflows"], cwd=repo_root)
    if (
        run(
            ["git", "diff", "--cached", "--quiet"], cwd=repo_root, check=False
        ).returncode
        == 0
    ):
        print("setup-soldr pins already match after rewrite; no update commit needed.")
    else:
        run(
            ["git", "commit", "-m", "ci: update setup-soldr workflow pins to v0"],
            cwd=repo_root,
        )
    run(
        ["git", "push", "--force-with-lease", "origin", f"HEAD:refs/heads/{branch}"],
        cwd=repo_root,
    )

    pr_url = ensure_update_pr(
        owner,
        repo,
        token,
        branch=branch,
        current_release_sha=current_release_sha,
        errors=errors,
        run_url=run_url,
    )
    ensure_update_issue(
        owner,
        repo,
        token,
        current_release_sha=current_release_sha,
        errors=errors,
        pr_url=pr_url,
        run_url=run_url,
    )


def update_workflow_pins(repo_root: Path, current_release_sha: str) -> None:
    for path in workflow_paths(repo_root):
        lines = path.read_text(encoding="utf-8").splitlines(keepends=True)
        updated_lines = []
        for line in lines:
            if line.lstrip().startswith("#"):
                updated_lines.append(line)
                continue
            updated_lines.append(
                SETUP_SOLDR_USE_RE.sub(
                    lambda match: f"uses: {match.group(1)}@{current_release_sha}",
                    line,
                )
            )
        updated = "".join(updated_lines)
        text = "".join(lines)
        if updated != text:
            path.write_text(updated, encoding="utf-8")


def ensure_git_identity(repo_root: Path) -> None:
    run(["git", "config", "user.name", "github-actions[bot]"], cwd=repo_root)
    run(
        [
            "git",
            "config",
            "user.email",
            "41898282+github-actions[bot]@users.noreply.github.com",
        ],
        cwd=repo_root,
    )


def ensure_update_pr(
    owner: str,
    repo: str,
    token: str,
    *,
    branch: str,
    current_release_sha: str,
    errors: list[str],
    run_url: str | None,
) -> str:
    existing = github_api(
        "GET",
        f"/repos/{owner}/{repo}/pulls?"
        + urllib.parse.urlencode({"state": "open", "head": f"{owner}:{branch}"}),
        token,
    )
    if existing:
        return existing[0]["html_url"]

    body = "\n".join(
        [
            "## Summary",
            f"- update executable `zackees/setup-soldr` workflow pins to current `@v0` `{current_release_sha}`",
            "- keep repository SHA-pinning ruleset satisfied while tracking the public major tag",
            "",
            "## Drift Detected",
            *[f"- {error}" for error in errors],
            *(["", f"Detected by: {run_url}"] if run_url else []),
            "",
            "## Test Plan",
            "- [ ] CI pin verifier reruns before any external setup-soldr action step",
        ]
    )
    created = github_api(
        "POST",
        f"/repos/{owner}/{repo}/pulls",
        token,
        {
            "title": "ci: update setup-soldr workflow pins to v0",
            "head": branch,
            "base": "main",
            "body": body,
        },
    )
    if not isinstance(created, dict):
        raise SystemExit(f"unexpected response creating pull request: {created!r}")
    return created["html_url"]


def ensure_update_issue(
    owner: str,
    repo: str,
    token: str,
    *,
    current_release_sha: str,
    errors: list[str],
    pr_url: str,
    run_url: str | None,
) -> None:
    issue = find_open_issue(owner, repo, token, AUTOFIX_ISSUE_TITLE)
    if issue is None:
        body = issue_body(current_release_sha, errors, pr_url, run_url)
        github_api(
            "POST",
            f"/repos/{owner}/{repo}/issues",
            token,
            {"title": AUTOFIX_ISSUE_TITLE, "body": body},
        )
        return

    body = issue.get("body") or ""
    if current_release_sha in body:
        return
    comments = github_api(
        "GET",
        f"/repos/{owner}/{repo}/issues/{issue['number']}/comments",
        token,
    )
    if any(current_release_sha in (comment.get("body") or "") for comment in comments):
        return
    github_api(
        "POST",
        f"/repos/{owner}/{repo}/issues/{issue['number']}/comments",
        token,
        {"body": issue_body(current_release_sha, errors, pr_url, run_url)},
    )


def find_open_issue(owner: str, repo: str, token: str, title: str) -> dict | None:
    issues = github_api(
        "GET",
        f"/repos/{owner}/{repo}/issues?{urllib.parse.urlencode({'state': 'open', 'per_page': 100})}",
        token,
    )
    for issue in issues:
        if "pull_request" not in issue and issue.get("title") == title:
            return issue
    return None


def issue_body(
    current_release_sha: str, errors: list[str], pr_url: str, run_url: str | None
) -> str:
    return "\n".join(
        [
            "`zackees/setup-soldr@v0` moved, but this repository requires full-SHA action pins.",
            "",
            f"Current `@v0`: `{current_release_sha}`",
            f"Update PR: {pr_url}",
            *(["", f"Detected by: {run_url}"] if run_url else []),
            "",
            "Verifier output:",
            *[f"- {error}" for error in errors],
        ]
    )


def github_run_url() -> str | None:
    server_url = os.environ.get("GITHUB_SERVER_URL")
    repository = os.environ.get("GITHUB_REPOSITORY")
    run_id = os.environ.get("GITHUB_RUN_ID")
    if not (server_url and repository and run_id):
        return None
    return f"{server_url}/{repository}/actions/runs/{run_id}"


def github_api(
    method: str, path: str, token: str, payload: dict | None = None
) -> dict | list:
    data = None
    headers = {
        "Accept": "application/vnd.github+json",
        "Authorization": f"Bearer {token}",
        "X-GitHub-Api-Version": "2022-11-28",
    }
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(
        f"https://api.github.com{path}",
        data=data,
        headers=headers,
        method=method,
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            raw = response.read().decode("utf-8")
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", errors="replace")
        raise RuntimeError(
            f"GitHub API {method} {path} failed: {exc.code} {detail}"
        ) from exc
    return json.loads(raw) if raw else {}


def run(args: list[str], cwd: Path, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(args, cwd=cwd, check=check, timeout=SUBPROCESS_TIMEOUT_SECS)


def main() -> int:
    verify_setup_soldr_pins()
    return 0


if __name__ == "__main__":
    sys.exit(main())
