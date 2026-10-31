//! soldr#4017: a fresh checkout must reuse a *warm* equivalent-root context.
//!
//! Soldr pins `ZCCACHE_WORKTREE_ROOT` to the checkout, so a registry crate's
//! dependency-graph context is registered once per checkout. Those instances
//! share one logical (root-normalized) key. A fresh checkout rebases its state
//! from an existing instance; zccache took whichever was first in the index,
//! and after a daemon restart that order is arbitrary. One instance left
//! `Cold` (an interrupted or failed compile in some other checkout) therefore
//! made every new checkout inherit `Cold` and miss with `context_not_found`,
//! even though a sibling held a valid artifact. On a real host the persisted
//! graph held 417 logical keys mixing warm and cold instances; `itoa` missed in
//! every new checkout while re-runs in the same path hit.
//!
//! The fix lives in the vendored zccache (`_vender/zccache`,
//! `DepGraph::best_equivalent_candidate`); this pins it from soldr's side.

use std::path::Path;

use zccache::core::NormalizedPath;
use zccache::depgraph::{
    CacheVerdict, CompileContext, ContextKey, ContextState, DepGraph, ScanResult,
};
use zccache::hash::{hash_bytes, ContentHash};

const REGISTRY_SOURCE: &str = "/registry/src/index.crates.io-0000/regdep-1.0.0/src/lib.rs";

fn registry_context() -> CompileContext {
    CompileContext {
        source_file: NormalizedPath::from(REGISTRY_SOURCE),
        include_search: Default::default(),
        defines: Vec::new(),
        undefines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
        compiler_hash: hash_bytes(b"soldr-4017-rustc"),
    }
}

fn same_content(path: &Path) -> Option<ContentHash> {
    Some(hash_bytes(path.to_string_lossy().as_bytes()))
}

#[test]
fn fresh_checkout_inherits_the_warm_equivalent_context_not_a_cold_one() {
    let graph = DepGraph::new();
    let logical_key = ContextKey::from_raw([0x17; 32]);
    let register = |checkout: &str| {
        graph.register_rustc_with_key_and_root_result(
            logical_key,
            registry_context(),
            Some(NormalizedPath::from(checkout)),
            Vec::new(),
            None,
        )
    };

    // A checkout whose compile never updated the graph stays Cold.
    let abandoned = register("/checkouts/abandoned");
    assert_eq!(abandoned.state, ContextState::Cold);

    // Another checkout compiles the crate and holds a valid artifact.
    let warm = register("/checkouts/warm");
    let artifact = graph
        .update(
            &warm.map_key,
            ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            },
            same_content,
        )
        .expect("the warm checkout's compile updates its context");

    let fresh = register("/checkouts/fresh");
    assert!(fresh.rebased_from_equivalent_root);
    assert_eq!(
        fresh.state,
        ContextState::Warm,
        "a fresh checkout must rebase from the warm sibling, not the cold one"
    );
    assert!(
        matches!(
            graph.check(&fresh.map_key, |_| true, same_content),
            CacheVerdict::Hit { artifact_key } if artifact_key == artifact
        ),
        "the fresh checkout must hit the warm sibling's artifact"
    );
}
