Canonical source: forge-ci/signal.forge-ci.com, `proto/src/lib.rs`, commit
`bfcedba9a7c570d3e08741db5e8b6f67be8f9d99` (PR #24, feat/host-registry).

That commit has no `proto/fixtures/` directory. These fixtures are derived
from its types and signing_bytes contract, rather than copied upstream.
Each external message is represented. The signature vector uses RFC 8032's
first public test seed, a fixed 32-byte nonce encoded in standard base64,
and an independently calculated signature (Python cryptography Ed25519).
This is PUBLIC TEST MATERIAL, never a production key.

When upstream fixtures land, replace these with byte-for-byte copies, record
that source commit, and extend the single integration target's fixture list.
