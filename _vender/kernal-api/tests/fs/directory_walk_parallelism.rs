#![cfg(feature = "fs")]

//! Where a directory walk runs, and why a dedicated pool is not the same
//! walk on a busy machine (soldr#2760); and that no mode ever ends a walk
//! short without an error item (kernal-api#563).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use kernal_api::platform::fs::{DirectoryWalk, DirectoryWalkParallelism};

const SATURATION_ENV: &str = "KERNAL_API_WALK_SATURATION_CHILD";
const SATURATION_TEST: &str = "directory_walk_parallelism::saturated_shared_pool_child";
/// The shared pool size the saturation child runs with, pinned so the test
/// knows exactly how many blocked reads saturate it.
const SHARED_POOL_THREADS: usize = 2;

fn small_tree(root: &Path) {
    fs::create_dir_all(root.join("nested/deeper")).unwrap();
    fs::write(root.join("top.txt"), "top").unwrap();
    fs::write(root.join("nested/mid.txt"), "mid").unwrap();
    fs::write(root.join("nested/deeper/low.txt"), "low").unwrap();
}

fn walked(walk: DirectoryWalk) -> std::io::Result<BTreeSet<PathBuf>> {
    walk.walk()
        .map(|entry| entry.map(|entry| entry.path().to_path_buf()))
        .collect()
}

#[test]
fn every_parallelism_yields_the_same_entries() {
    let dir = tempfile::tempdir().unwrap();
    small_tree(dir.path());
    let root = dir.path().to_path_buf();

    let shared =
        walked(DirectoryWalk::new(root.clone()).parallelism(DirectoryWalkParallelism::SharedPool))
            .unwrap();
    assert!(shared.contains(&root.join("nested/deeper/low.txt")));
    let sequential =
        walked(DirectoryWalk::new(root.clone()).parallelism(DirectoryWalkParallelism::Sequential))
            .unwrap();
    assert_eq!(sequential, shared);
    assert_eq!(walked(DirectoryWalk::new(root.clone())).unwrap(), shared);
    for threads in [0, 1, 3] {
        let dedicated = walked(
            DirectoryWalk::new(root.clone())
                .parallelism(DirectoryWalkParallelism::DedicatedPool { threads }),
        )
        .unwrap();
        assert_eq!(dedicated, shared, "threads = {threads}");
    }
    assert_eq!(
        DirectoryWalkParallelism::default(),
        DirectoryWalkParallelism::SharedPool
    );
}

/// A saturated shared pool ends a shared walk with an explicit
/// `ResourceBusy` error item -- never a shorter all-`Ok` stream -- while the
/// sequential mode and a dedicated pool walk the same tree to completion.
///
/// Run in a child process: saturating the process-wide pool would make any
/// concurrently running shared walk in this binary fail for the same reason.
#[test]
fn a_dedicated_pool_walk_survives_a_saturated_shared_pool() {
    let executable = std::env::current_exe().expect("test executable");
    let status = std::process::Command::new(executable)
        .args([SATURATION_TEST, "--exact", "--ignored", "--nocapture"])
        .env(SATURATION_ENV, "1")
        .env("RAYON_NUM_THREADS", SHARED_POOL_THREADS.to_string())
        .status()
        .expect("run the saturation child");
    assert!(status.success(), "saturation child failed: {status}");
}

/// Re-executed by [`a_dedicated_pool_walk_survives_a_saturated_shared_pool`];
/// a no-op otherwise.
#[test]
#[ignore = "helper process for a_dedicated_pool_walk_survives_a_saturated_shared_pool"]
fn saturated_shared_pool_child() {
    if std::env::var_os(SATURATION_ENV).is_none() {
        return;
    }

    // Occupy every shared-pool worker: each `blocker/dN` read offers `sub` to
    // the prune predicate, which parks until the gate opens.
    let blocker = tempfile::tempdir().unwrap();
    for index in 0..SHARED_POOL_THREADS * 2 {
        fs::create_dir_all(blocker.path().join(format!("d{index}/sub"))).unwrap();
    }
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let parked = Arc::new(AtomicUsize::new(0));
    let saturating = {
        let gate = Arc::clone(&gate);
        let parked = Arc::clone(&parked);
        let walk = DirectoryWalk::new(blocker.path().to_path_buf())
            .parallelism(DirectoryWalkParallelism::SharedPool)
            .prune_directories(move |directory| {
                if directory.file_name().is_some_and(|name| name == "sub") {
                    parked.fetch_add(1, Ordering::SeqCst);
                    let (open, opened) = &*gate;
                    let guard = open.lock().unwrap();
                    // Bounded, so a failed assertion below cannot hang the child.
                    let _released = opened
                        .wait_timeout_while(guard, Duration::from_secs(30), |open| !*open)
                        .unwrap();
                }
                true
            });
        std::thread::spawn(move || walk.walk().count())
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while parked.load(Ordering::SeqCst) < SHARED_POOL_THREADS {
        assert!(Instant::now() < deadline, "the shared pool never saturated");
        std::thread::sleep(Duration::from_millis(5));
    }

    let target = tempfile::tempdir().unwrap();
    small_tree(target.path());
    let root = target.path().to_path_buf();
    let run = |walk: DirectoryWalk| {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || sender.send(walked(walk)));
        receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the walk must finish rather than hang")
    };

    let dedicated = run(DirectoryWalk::new(root.clone())
        .parallelism(DirectoryWalkParallelism::DedicatedPool { threads: 2 }))
    .expect("a dedicated-pool walk does not depend on the shared pool");
    assert!(dedicated.contains(&root.join("nested/deeper/low.txt")));

    let sequential =
        run(DirectoryWalk::new(root.clone()).parallelism(DirectoryWalkParallelism::Sequential))
            .expect("a sequential walk does not depend on the shared pool");
    assert_eq!(sequential, dedicated);

    // The truncation contract: whatever the shared walk yields, either it is
    // the whole tree or one of its items is the busy error. An all-`Ok`
    // stream that is short of the tree is the bug this guards against.
    let items: Vec<std::io::Result<PathBuf>> = {
        let walk = DirectoryWalk::new(root).parallelism(DirectoryWalkParallelism::SharedPool);
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            sender.send(
                walk.walk()
                    .map(|entry| entry.map(|entry| entry.into_path()))
                    .collect(),
            )
        });
        receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the walk must finish rather than hang")
    };
    let busy = items.iter().any(|item| {
        item.as_ref()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::ResourceBusy)
    });
    let complete: BTreeSet<PathBuf> = items.into_iter().filter_map(Result::ok).collect();
    assert!(
        busy,
        "a walk on the saturated shared pool gives up, and says so; that is \
         what the sequential and dedicated modes exist to avoid"
    );
    assert!(
        complete.len() < dedicated.len(),
        "the busy error marks a walk that really is short"
    );

    let (open, opened) = &*gate;
    *open.lock().unwrap() = true;
    opened.notify_all();
    saturating.join().expect("saturating walk");
}
