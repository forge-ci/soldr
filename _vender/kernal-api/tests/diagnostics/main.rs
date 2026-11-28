//! Crash, allocator, symbol, and whole-binary coexistence proofs.
//!
//! One linked test binary per category, not per file: each module below was
//! its own top-level integration test, and each one statically linked this
//! crate's whole graph. Test IDs are now `diagnostics::<module>::<test>`.
//! See AGENTS.md for the rule.
//!
//! `allocator_heap_profile` is deliberately NOT here. It starts the heap
//! profiler, which is process-global and one-way, while `unified_contract`
//! asserts the profiler is dormant. In separate binaries both held; sharing
//! one made them order-dependent -- nextest hid it by running each test in its
//! own process, and `cargo test` failed. Process-global state is the reason a
//! test earns its own binary.

mod crash_facade;
mod debug_symbol_split;
mod unified_contract;
