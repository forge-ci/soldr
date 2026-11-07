---
name: soldr-release
description: Release a new soldr version end to end — pick and bump the version (Cargo.toml, package.json, Cargo.lock), merge the bump PR, dispatch exact-SHA full CI, then dispatch release-auto.yml and verify GitHub Release, PyPI, and npm. Use whenever the user asks to release, publish, ship, cut, tag, or bump soldr, asks what the last released version is, or asks why a release run failed or how to recover one.
---

# soldr-release

A release is **two `workflow_dispatch` runs on one exact merged commit**
(soldr#3346). Merging a version bump releases nothing by itself —
`release-auto.yml` has no push trigger. RELEASE.md is the long-form
reference; this is the procedure.

```
bump PR merged ─► CI dispatch (candidate_sha) ─► Full coverage green
                                                        │
                   release-auto.yml dispatch (candidate_sha + full_ci_run_id)
                                                        │
      full_ci_gate ─► build ─► smokes ─► GitHub Release ─► verify ─► PyPI ─► npm ─► completeness
```

## 1. Read the live state first

Never trust memory or docs for "what is released". Run all of these:

```bash
grep -m1 '^version' Cargo.toml; grep '"version"' package.json
curl -s https://pypi.org/pypi/soldr/json | python3 -c "import json,sys;print(json.load(sys.stdin)['info']['version'])"
npm view @zackees/soldr version
gh release list --repo zackees/soldr --limit 3
gh run list --repo zackees/soldr --workflow release-auto.yml --limit 5
```

The last released version is the one present on **all three** surfaces
(GitHub Release, PyPI with 8 wheels, npm). A version that is on one surface
but not the others is a partial release: recover it (step 5), don't bump it.

## 2. Make `main` releasable

- `main`'s own `CI` run must be green. Check
  `gh run list --repo zackees/soldr --branch main --workflow ci.yml --limit 3`.
  A red `Lint` on `main` fails full CI too, so fix it in the release PR.
- The version is strictly greater than PyPI's latest and absent from
  `git ls-remote --tags origin vX.Y.Z`. Default to a patch bump.

## 3. Bump PR

On a branch in this checkout (no worktrees; see CLAUDE.md):

1. `Cargo.toml` `[workspace.package].version` and `package.json` `"version"`.
2. Refresh `Cargo.lock` with a no-op `soldr cargo build -p soldr-cli`.
3. `soldr cargo nextest run -p soldr-cli --test guards -E 'test(/^version_lockstep::/)'`
4. Cheap validation is never skipped: run
   `uv run --no-project python .github/scripts/platform_cfg_boundary_ratchet.py`
   and `./lint`. A `cfg!(...)` outside the platform boundary is the usual
   `Lint` failure.
5. Push, open the PR, wait with the `pr-wait-fast` skill, merge when green.
   Merging to `main` is the owner's call unless they asked you to release,
   which authorizes it.

## 4. Exact-SHA full CI, then the release

```bash
SHA=$(git ls-remote origin refs/heads/main | cut -f1)   # the merged bump commit; confirm with git log
gh workflow run ci.yml --repo zackees/soldr --ref main -f candidate_sha=$SHA
# find the run: its title is exactly "CI full <SHA>"
gh run list --repo zackees/soldr --workflow ci.yml --event workflow_dispatch --limit 5 \
  --json databaseId,displayTitle,status,conclusion
```

- The full run takes about an hour. Wait with a background poll or the Monitor
  tool, not a foreground `sleep`. It must finish `success` with both
  `CI mode` and `Full coverage` jobs successful.
- If it fails, triage by job name (CLAUDE.md "Triaging a red lane"). Rerun
  flakes with `gh run rerun <id> --failed`; the rerun keeps the same run ID.
  A real fix means a new commit, a new SHA, and a new CI dispatch.

```bash
gh workflow run release-auto.yml --repo zackees/soldr --ref main \
  -f candidate_sha=$SHA -f full_ci_run_id=<CI run ID>
```

What the gates check (`.github/scripts/release_full_ci_gate.py`):

- `candidate_sha` is a full lowercase 40-char SHA, and it is reachable from
  `origin/main`.
- The CI run is a `workflow_dispatch` of `ci.yml` from `main` in
  `zackees/soldr`, titled exactly `CI full <SHA>`, completed successfully.

`release_detect.py` then decides per surface. GitHub is published when the
release is missing or incomplete and not immutable. PyPI is published when it
has zero files for the version, or `force_pypi_publish` is set. npm is
published when the version is absent. Nothing to do on any surface gives
`should_release=false`, and every job is skipped. That's expected.

## 5. Failures and recovery

- **Re-dispatch the same SHA.** Every surface is recomputed from live state,
  so completed surfaces are skipped. v0.9.22 needed three dispatches: macOS
  Intel smoke, then post-publish Dylint smoke, then green.
- An **immutable GitHub Release is never republished**. If it is incomplete,
  cut a new patch version.
- `force_pypi_publish=true` only after a PyPI-side problem with the current
  version. `npm_release_ref=<ref>` publishes only npm.
- **Never** create or push `vX.Y.Z` tags by hand. The workflow mints them.

## 6. Verify and report

- The release run is green, including `Release surface completeness gate`.
- `gh release view vX.Y.Z --repo zackees/soldr` is non-draft, with archives
  and `SHA256SUMS.txt`.
- PyPI `soldr X.Y.Z` has 8 wheels. npm `@zackees/soldr` reports `X.Y.Z`.

Report the version, the bump PR URL, the CI run URL, and the release run URL.
