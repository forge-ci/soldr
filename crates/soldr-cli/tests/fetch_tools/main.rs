//! Tool resolution, fetch, manifest lookup, and build-from-source surfaces.
//!
//! soldr#2934: one linked test binary per category instead of one per source
//! file. Each module below was previously its own top-level test binary, so
//! test IDs are now `<module>::<test_name>`.

#[path = "../common/mod.rs"]
mod common;

mod cli_build_from_source;
mod cli_cc;
mod cli_cc_cross;
mod cli_global_upgrade;
mod cli_ld_link;
mod cli_maturin;
mod cli_rust_analyzer;
mod cli_wheel;
mod embed_first_resolver;
mod fetch_crgx;
mod manifest_lookup;
mod manifest_lookup_disable;
mod manifest_lookup_url_override;
