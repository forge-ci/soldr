# Release Governance Checklist

This checklist covers the GitHub-side controls that matter for `soldr` releases but do not live entirely in the git tree.

Use it when auditing the repository settings before the first real public release and periodically afterward.

## Why This Exists

The repository now contains a validated release workflow, but some of the strongest release controls are still repository settings rather than committed files.

If those settings drift, the workflow may remain correct while the operational release posture becomes weaker than intended.

## Target State

The intended release-governance state is:

- release tags are protected by rulesets or an explicit platform constraint is documented
- the normal release path goes through `.github/workflows/release-auto.yml`
- the `release` environment exists and requires explicit approval
- immutable GitHub Releases are enabled
- GitHub Actions requires full-SHA pinning for third-party actions
- `main` is protected with pull-request and required-check enforcement
- maintainers can audit these settings without guessing where they live

## Current Observed Repository State

Observed on April 13, 2026 via GitHub API:

- an active repository tag ruleset protects `refs/tags/v*.*.*`
- the release-tag ruleset blocks tag creation, update, and deletion
- the release GitHub App is the only bypass actor for that ruleset
- the `release` environment exists and requires approval from `@zackees`
- `main` branch protection requires pull requests, conversation resolution, linear history, and the current CI plus per-target e2e checks
- immutable GitHub Releases are enabled
- GitHub Actions requires full-SHA pinning for third-party actions
- no published GitHub Releases yet

Those observations were produced with:

```bash
gh api repos/zackees/soldr/rulesets
gh api repos/zackees/soldr/rulesets/15033379
gh api repos/zackees/soldr/environments
gh api repos/zackees/soldr/branches/main/protection
gh api repos/zackees/soldr/actions/permissions
gh api repos/zackees/soldr/immutable-releases
gh api repos/zackees/soldr/releases?per_page=5
```

The ruleset now exists and is active. The critical detail is not just that a tag ruleset exists, but that the only bypass actor is the dedicated release GitHub App rather than a human maintainer or a PAT-backed workaround.

## Audit Steps

### 1. Check Rulesets

Confirm that release tags are governed by repository rulesets:

```bash
gh api repos/zackees/soldr/rulesets
```

What to look for:

- a ruleset that applies to release tag patterns such as `v*`
- the ruleset is active
- tag creation, update, and deletion are all blocked by default
- the only bypass actor is the dedicated release GitHub App
- protection against moving or deleting release tags
- restrictions that make the validated workflow the normal release path

Current constraint:

- On this personal repository, the built-in `github-actions` integration was not suitable as the bypass actor for a workflow-only tag rule.
- The current secure model uses a dedicated installed GitHub App instead.
- Do not replace the App bypass with a maintainer-user bypass or PAT-backed workaround and call it equivalent.

### 2. Check The Release Environment

Confirm that the `release` environment exists:

```bash
gh api repos/zackees/soldr/environments
```

What to look for:

- an environment named `release`
- required reviewer `@zackees`
- `protected_branches: true` and `custom_branch_policies: false`

The validated release workflow already targets `environment: release`, so this environment is part of the actual publication path rather than a dormant setting.

### 3. Check Branch Protection Or Equivalent Rulesets

Confirm that `main` is protected either by branch protection or a ruleset-based equivalent:

```bash
gh api repos/zackees/soldr/branches/main/protection
```

What to look for:

- pull requests are required for `main`
- the required-check list includes:
  - `Lint`
  - `Linux x64`
  - `macOS x64`
  - `Windows x64`
  - each per-target bootstrap badge check emitted from `ci.yml`
- force pushes and deletions are blocked
- linear history and conversation resolution are enabled

If the e2e workflow template changes job names, re-audit the required check list. The branch rule now depends on those contexts being stable and target-specific.

### 4. Check GitHub Actions Policy

Confirm that repository Actions settings require SHA pinning:

```bash
gh api repos/zackees/soldr/actions/permissions
```

What to look for:

- `enabled: true`
- `allowed_actions: "all"` or a narrower documented allowlist
- `sha_pinning_required: true`

### 5. Check Published Releases

Confirm that releases are being created through the validated workflow path:

```bash
gh api repos/zackees/soldr/releases?per_page=5
```

What to look for:

- release assets and checksum manifest are present
- releases correspond to expected version tags
- the release inventory matches the workflow outputs

### 6. Check Immutable Releases

Immutable releases are configured in repository or organization settings rather than in this repository's source tree.

Verify with:

```bash
gh api repos/zackees/soldr/immutable-releases
```

What to look for:

- `enabled: true`
- maintainers understand how draft release creation and publication interact with immutability

## Operational Rule

If any of the settings above are absent or weaker than intended, do not treat the repository as fully matching the planned release-governance model even if the workflows continue to pass.

## Related Documents

- [../SECURITY.md](../SECURITY.md)
- [RELEASE_VERIFICATION.md](./RELEASE_VERIFICATION.md)
- [TRUST_BOUNDARIES.md](./TRUST_BOUNDARIES.md)
