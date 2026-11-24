//! soldr#3075: when a broker stands down because its install directory is gone.

use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};

const TICK: Duration = Duration::from_millis(2);

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .start_paused(true)
        .enable_all()
        .build()
        .expect("runtime")
}

/// Run the watch against a scripted presence sequence for at most `budget`.
/// Returns whether shutdown was requested and how many checks were made.
fn watch_for(budget: Duration, mut script: impl FnMut(usize) -> Option<bool>) -> (bool, usize) {
    let shutdown = Arc::new(ShutdownSignal::default());
    let checks = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&checks);
    let watch = run_home_watch(Arc::clone(&shutdown), TICK, move || {
        script(counter.fetch_add(1, Ordering::Relaxed))
    });
    let _ = runtime().block_on(async { tokio::time::timeout(budget, watch).await });
    (shutdown.is_requested(), checks.load(Ordering::Relaxed))
}

#[test]
fn a_vanished_install_directory_stands_the_broker_down() {
    let (requested, checks) = watch_for(Duration::from_secs(10), |_| Some(false));
    assert!(
        requested,
        "a broker whose home is gone must request shutdown"
    );
    assert_eq!(
        checks,
        crate::daemon::lifecycle::DAEMON_IMAGE_MISSING_STRIKES as usize,
        "it stands down on the confirming strike, not the first miss"
    );
}

#[test]
fn a_present_install_directory_keeps_the_broker_serving() {
    let (requested, checks) = watch_for(Duration::from_millis(200), |_| Some(true));
    assert!(checks > 3, "the watch must keep checking ({checks} checks)");
    assert!(!requested);
}

/// Not knowing where the image lives is not evidence it was deleted.
#[test]
fn an_undeterminable_path_is_never_a_strike() {
    let (requested, checks) = watch_for(Duration::from_millis(200), |_| None);
    assert!(checks > 3, "the watch must keep checking ({checks} checks)");
    assert!(!requested);
}

/// Absence must be sustained: one sighting between misses resets the count.
#[test]
fn an_intermittent_miss_does_not_retire_the_broker() {
    let strikes = crate::daemon::lifecycle::DAEMON_IMAGE_MISSING_STRIKES as usize;
    let (requested, checks) = watch_for(Duration::from_millis(200), |check| {
        Some(check % strikes == strikes - 1)
    });
    assert!(
        checks > strikes * 2,
        "the watch must keep checking ({checks} checks)"
    );
    assert!(!requested);
}

/// Any other shutdown ends the watch rather than leaving it parked.
#[test]
fn an_external_shutdown_ends_the_watch() {
    let shutdown = Arc::new(ShutdownSignal::default());
    shutdown.request();
    runtime().block_on(async {
        tokio::time::timeout(
            Duration::from_secs(5),
            run_home_watch(Arc::clone(&shutdown), BROKER_HOME_CHECK_INTERVAL, || {
                Some(true)
            }),
        )
        .await
        .expect("a requested shutdown must end the watch promptly");
    });
}

#[test]
fn install_directory_presence_follows_the_directory_not_the_image() {
    let temp = tempfile::tempdir().expect("temp dir");
    let directory = temp.path().join(".soldr").join("broker");
    let executable = directory.join("soldr-broker");
    std::fs::create_dir_all(&directory).expect("broker directory");

    // No image on disk: the directory is what counts, so an image replaced
    // in place never looks like a deleted home.
    assert_eq!(install_directory_present(&executable), Some(true));

    std::fs::remove_dir_all(temp.path().join(".soldr")).expect("delete home");
    assert_eq!(install_directory_present(&executable), Some(false));

    assert_eq!(install_directory_present(Path::new("soldr-broker")), None);
}
