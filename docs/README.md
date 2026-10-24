# soldr documentation

External-facing docs grouped by topic. `API.md` is the source-of-truth
contract for what soldr exposes to consumers; the others document
operational and integration details.

## Files

- `API.md` / `API_BOUNDARY.md` — public soldr CLI contract and the
  invariants that bound it.
- `BUILD_PROFILE_LEVERS.md` — why `-Zshare-generics=y` and
  `incremental = true` are deliberately not set in any zackees Rust repo
  (#1505 audit).
- `CI_CACHE.md` / `CI_CACHE_PHASE1.md` — usage guide and historical
  rollout notes for `zackees/setup-soldr@v0` in external CI.
- `CONTRIBUTING_TESTS.md` — portable and native platform test conventions,
  including how archived tests reach target runners.
- `NATIVE_SQLITE_BENCHMARK.md` — cross-platform validation of the
  default-on native C/C++ compiler cache (#310, #312).
- `NPM_PUBLISHING.md`, `PYPI_TRUSTED_PUBLISHING.md` — release plumbing.
- `RELEASE_*.md` — release checklists and verification flow.
- `RUSTUP_REPLACEMENT_ANALYSIS.md` — design discussion for the rustup
  replacement path.
- `SETUP_SOLDR_EXPORTER.md` / `SETUP_SOLDR_PUBLIC_ACTION.md` —
  exporter design and the public action contract.
- `TARGET_GC_PROPOSAL.md` — Rust
  artifact cache / target-dir GC design notes.
- `TRUST_BOUNDARIES.md` — trust-mode semantics.
- `VERDICT_LEDGER.md` — the verdict ledger: checks skip when their exact
  inputs already passed (core, adapters, key, record, store; soldr#3894).
