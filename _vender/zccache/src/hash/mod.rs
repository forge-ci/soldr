//! Hashing utilities and effect-free request-key encoding for zccache.
//!
//! Disable default features for the encoder-only surface. Native callers retain
//! the existing content hashing and cache-key APIs through the default feature.

pub mod request_fingerprint;

pub mod cache_key;
pub mod link_cache_key;
mod native;
pub use native::*;
