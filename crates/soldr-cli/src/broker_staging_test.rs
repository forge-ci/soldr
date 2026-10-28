//! Debug-only synchronization for staging lease regression tests.

use std::time::{Duration, Instant};

pub(super) fn pause_after_sync() {
    let Some(delay) = std::env::var("SOLDR_TEST_BROKER_POST_SYNC_PAUSE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return;
    };
    if let Some(ready) = std::env::var_os("SOLDR_TEST_BROKER_POST_SYNC_READY") {
        let _ = std::fs::write(ready, b"ready");
    }
    if let Some(resume) = std::env::var_os("SOLDR_TEST_BROKER_POST_SYNC_RESUME") {
        let deadline = Instant::now() + Duration::from_millis(delay);
        while !std::path::Path::new(&resume).exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    } else {
        std::thread::sleep(Duration::from_millis(delay));
    }
    if let Some(marker) = std::env::var_os("SOLDR_TEST_BROKER_POST_SYNC_MARKER") {
        let _ = std::fs::write(marker, b"completed");
    }
}

pub(super) fn note_publication() {
    if let Some(marker) = std::env::var_os("SOLDR_TEST_BROKER_PUBLISHED_MARKER") {
        let _ = std::fs::write(marker, b"published");
    }
}
