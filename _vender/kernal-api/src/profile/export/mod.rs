//! Profile exports (S15 / #644).
//!
//! Three formats, all folded from the same [`SessionResult::folded`] output so
//! they cannot disagree about what was hot:
//!
//! - [`pprof`] — Google's protobuf, what `go tool pprof` and most viewers read.
//! - [`firefox`] — the Firefox Profiler's processed-profile JSON.
//! - [`collapsed`] — Brendan Gregg's folded stacks, and the feed for the
//!   flame graph in the daemon's UI.
//!
//! The pprof schema is vendored (`proto/pprof/profile.proto`) and encoded
//! directly rather than through the `pprof` crate, which carries an open
//! RUSTSEC unsoundness advisory. All that was wanted from it was a wire
//! format, and a `.proto` file costs less than a dependency with a security
//! caveat.

pub mod collapsed;
pub mod firefox;
pub mod pprof;

use crate::profile::SessionResult;

#[doc(inline)]
pub use collapsed::to_collapsed;
#[doc(inline)]
pub use firefox::to_firefox_json;
#[doc(inline)]
pub use pprof::{to_pprof_bytes, to_pprof_gzip};

/// Every export of one session, for a caller that wants all of them.
#[derive(Debug)]
pub struct Exports {
    /// Gzipped pprof protobuf, the `.pb.gz` convention.
    pub pprof_gzip: Vec<u8>,
    /// Firefox Profiler JSON.
    pub firefox_json: String,
    /// Collapsed stacks.
    pub collapsed: String,
}

/// Render all three formats.
pub fn all(result: &SessionResult) -> std::io::Result<Exports> {
    Ok(Exports {
        pprof_gzip: to_pprof_gzip(result)?,
        firefox_json: to_firefox_json(result),
        collapsed: to_collapsed(result),
    })
}
