//! soldr#3801: the install lock, the bounded wait and its fallbacks.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn write_stamp(install_dir: &Path) {
    std::fs::write(install_dir.join(".complete"), "stamp\n").unwrap();
}

/// A fake download+extract: wipes the dir, writes a half-extracted tree, then
/// stamps last -- the shape `fetch_managed_llvm` has under the lock.
async fn fake_install(
    install_dir: PathBuf,
    installs: Arc<AtomicUsize>,
    partial_seen: Arc<AtomicUsize>,
) {
    let _lock = lock_install(&install_dir).await.unwrap();
    if llvm_install_complete(&install_dir) {
        // The second caller: the first finished while we were blocked.
        assert!(install_dir.join("payload").is_file());
        return;
    }
    if install_dir.exists() {
        std::fs::remove_dir_all(&install_dir).unwrap();
    }
    std::fs::create_dir_all(&install_dir).unwrap();
    std::fs::write(install_dir.join("partial"), b"x").unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    if !install_dir.join("partial").is_file() {
        partial_seen.fetch_add(1, Ordering::SeqCst);
    }
    std::fs::write(install_dir.join("payload"), b"y").unwrap();
    write_stamp(&install_dir);
    installs.fetch_add(1, Ordering::SeqCst);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_fetchers_install_once_and_the_second_sees_the_stamp() {
    let temp = tempfile::tempdir().unwrap();
    let install_dir = temp.path().join("llvm-test");
    let installs = Arc::new(AtomicUsize::new(0));
    let corrupted = Arc::new(AtomicUsize::new(0));
    let a = tokio::spawn(fake_install(
        install_dir.clone(),
        installs.clone(),
        corrupted.clone(),
    ));
    let b = tokio::spawn(fake_install(
        install_dir.clone(),
        installs.clone(),
        corrupted.clone(),
    ));
    a.await.unwrap();
    b.await.unwrap();
    assert_eq!(
        installs.load(Ordering::SeqCst),
        1,
        "exactly one fetcher installs"
    );
    assert_eq!(
        corrupted.load(Ordering::SeqCst),
        0,
        "no fetcher wiped another's tree"
    );
    assert!(install_dir.join("payload").is_file());
    assert!(llvm_install_complete(&install_dir));
}

#[tokio::test]
async fn a_complete_install_is_returned_without_touching_the_lock_or_ensure() {
    let temp = tempfile::tempdir().unwrap();
    let install_dir = temp.path().join("llvm-test");
    std::fs::create_dir_all(&install_dir).unwrap();
    write_stamp(&install_dir);
    wait_for_install(&install_dir, Duration::from_secs(1), || async {
        panic!("ensure must not run for a complete install")
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_failed_fetch_surfaces_the_ensure_error_on_the_waiter() {
    let temp = tempfile::tempdir().unwrap();
    let install_dir = temp.path().join("llvm-test");
    let error = wait_for_install(&install_dir, Duration::from_secs(5), || async {
        Err(SoldrError::Other("catalogue offline".into()))
    })
    .await
    .unwrap_err();
    assert!(error.to_string().contains("catalogue offline"), "{error}");
}

#[tokio::test]
async fn a_killed_fetcher_with_no_stamp_makes_the_waiter_run_ensure_itself() {
    let temp = tempfile::tempdir().unwrap();
    let install_dir = temp.path().join("llvm-test");
    // The fetcher died holding nothing and leaving a half-extracted tree.
    std::fs::create_dir_all(install_dir.join("half")).unwrap();
    let ran = Arc::new(AtomicUsize::new(0));
    let ran_in = ran.clone();
    let dir = install_dir.clone();
    wait_for_install(&install_dir, Duration::from_secs(5), move || async move {
        ran_in.fetch_add(1, Ordering::SeqCst);
        write_stamp(&dir);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    assert!(llvm_install_complete(&install_dir));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hung_fetcher_hits_the_wait_limit_with_a_clear_error() {
    let temp = tempfile::tempdir().unwrap();
    let install_dir = temp.path().join("llvm-test");
    let _held = lock_install(&install_dir).await.unwrap();
    let started = Instant::now();
    let error = wait_for_install(&install_dir, Duration::from_millis(300), || async {
        panic!("ensure must not run while the lock is held")
    })
    .await
    .unwrap_err();
    let text = error.to_string();
    assert!(
        text.contains("timed out") && text.contains(LLVM_FETCH_WAIT_ENV_VAR),
        "{text}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn an_ensure_that_outlives_the_budget_is_cut_off() {
    let temp = tempfile::tempdir().unwrap();
    let install_dir = temp.path().join("llvm-test");
    let error = wait_for_install(&install_dir, Duration::from_millis(200), || async {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(())
    })
    .await
    .unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error}");
}

#[test]
fn the_wait_budget_parses_positive_seconds_and_defaults_otherwise() {
    assert_eq!(parse_wait_secs(None), DEFAULT_LLVM_FETCH_WAIT);
    assert_eq!(parse_wait_secs(Some("45")), Duration::from_secs(45));
    assert_eq!(parse_wait_secs(Some(" 7 ")), Duration::from_secs(7));
    for bad in ["0", "", "soon", "-3"] {
        assert_eq!(
            parse_wait_secs(Some(bad)),
            DEFAULT_LLVM_FETCH_WAIT,
            "{bad:?}"
        );
    }
}
