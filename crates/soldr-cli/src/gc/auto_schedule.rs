//! Cross-process admission and cooldown for post-build automatic GC.

pub(super) const AUTO_GC_THROTTLE_SECONDS: u64 = 5 * 60;

pub(super) fn auto_gc_throttle_expired(marker: &std::path::Path, throttle_seconds: u64) -> bool {
    let Ok(meta) = std::fs::metadata(marker) else {
        return true;
    };
    let Ok(modified) = meta.modified() else {
        return true;
    };
    let elapsed = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or(std::time::Duration::ZERO);
    elapsed.as_secs() >= throttle_seconds
}

pub(super) fn touch_auto_gc_marker(marker: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(marker, "")
}

/// All launchers share the reservation, and the worker holds the same
/// lock while running. The timestamp survives deferrals and worker exits.
pub(super) fn sweep_lock(marker: &std::path::Path) -> Option<std::fs::File> {
    let parent = marker.parent()?;
    std::fs::create_dir_all(parent).ok()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(marker.with_extension("lock"))
        .ok()?;
    file.try_lock().ok()?;
    Some(file)
}

pub(super) fn request_sweep(
    marker: &std::path::Path,
    launch: impl FnOnce() -> std::io::Result<()>,
) {
    if !auto_gc_throttle_expired(marker, AUTO_GC_THROTTLE_SECONDS) {
        return;
    }
    let Some(reservation) = sweep_lock(marker) else {
        return;
    };
    if !auto_gc_throttle_expired(marker, AUTO_GC_THROTTLE_SECONDS) {
        return;
    }
    if let Err(error) = touch_auto_gc_marker(marker) {
        tracing::warn!(%error, "could not reserve auto-GC sweep");
        return;
    }
    drop(reservation);
    if let Err(error) = launch() {
        tracing::warn!(%error, "could not launch auto-GC sweep");
    }
}

/// Run the worker under its admission lock and start the next cooldown
/// when it finishes, even when the work returned early.
pub(super) fn run_reserved_sweep(marker: &std::path::Path, sweep: impl FnOnce()) {
    let Some(_lock) = sweep_lock(marker) else {
        return;
    };
    sweep();
    if let Err(error) = touch_auto_gc_marker(marker) {
        tracing::warn!(%error, "could not refresh auto-GC cooldown");
    }
}
