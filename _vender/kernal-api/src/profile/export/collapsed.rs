//! Collapsed (folded) stacks (S15 / #644).
//!
//! One line per unique stack: `root;mid;leaf <count>`. The format Brendan
//! Gregg's flamegraph tooling reads, and the feed for the daemon's own flame
//! graph — which is why it is worth emitting even though pprof is richer: it
//! is the one format a person can read with `sort` and `grep`.

use crate::profile::SessionResult;

/// Render folded stacks, hottest first.
pub fn to_collapsed(result: &SessionResult) -> String {
    let mut out = String::new();
    for (stack, count) in result.folded() {
        // Semicolons separate frames, so one inside a frame name would create
        // a phantom frame and reparent everything below it. Replaced rather
        // than escaped, because the consuming tools have no escape syntax.
        let joined: Vec<String> = stack.iter().map(|frame| frame.replace(';', ":")).collect();
        out.push_str(&joined.join(";"));
        out.push(' ');
        out.push_str(&count.to_string());
        out.push('\n');
    }
    out
}
