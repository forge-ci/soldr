Byte-for-byte copies of every JSON fixture in
`forge-ci/signal.forge-ci.com:proto/fixtures/` at commit
`02069c72a3f5cac9d9e48a77dc7e7d0244b23b74` (signal PR #24, feat/host-registry).

The canonical types are `proto/src/lib.rs`, deliberately copied in
`src/proto.rs`. Fixtures cover challenge, ping, both hello/heartbeat forms,
passing/failing pong, close control frames and the Ed25519 signing vector.
The signature seed is PUBLIC RFC8032 TEST MATERIAL, never a production key.

The conformance test round-trips every fixture and verifies the signature
and exact signing-input layout. Close fixtures describe WebSocket control
frames rather than application-level JSON messages.
