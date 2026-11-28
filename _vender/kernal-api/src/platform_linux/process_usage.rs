//! Linux per-process CPU and resident-memory readings from `/proc`.

/// Linux loses a reaped child's accounting, and its PID may be reissued.
pub const PEAK_RSS_READABLE_AFTER_EXIT: bool = false;

/// Upper bound on processes one tree reading visits.
pub const MAX_TREE_RSS_PROCESSES: usize = 4096;

/// `utime + stime` in clock ticks from `/proc/<pid>/stat`.
pub fn cpu_ticks_for_pid(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `comm` may contain spaces or ')'; fixed fields start after the last ')'.
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let user = fields.get(11)?.parse::<u64>().ok()?;
    let system = fields.get(12)?.parse::<u64>().ok()?;
    Some(user.wrapping_add(system))
}

fn status_kib(pid: u32, key: &str) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kib = status
        .lines()
        .find_map(|line| line.strip_prefix(key))?
        .trim()
        .strip_suffix("kB")?
        .trim();
    Some(kib.parse::<u64>().ok()?.saturating_mul(1024))
}

/// `VmHWM`, the resident high-water mark.
pub fn peak_rss_bytes_for_pid(pid: u32) -> Option<u64> {
    status_kib(pid, "VmHWM:")
}

/// Current `VmRSS` of `pid` plus every live descendant.
pub fn tree_rss_bytes_for_pid(pid: u32) -> Option<u64> {
    let mut total = status_kib(pid, "VmRSS:")?;
    let mut seen = std::collections::HashSet::from([pid]);
    let mut stack = vec![pid];
    while let Some(parent) = stack.pop() {
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{parent}/task")) else {
            continue;
        };
        for task in tasks.flatten() {
            let Ok(children) = std::fs::read_to_string(task.path().join("children")) else {
                continue;
            };
            for child in children
                .split_whitespace()
                .filter_map(|value| value.parse::<u32>().ok())
            {
                if seen.len() >= MAX_TREE_RSS_PROCESSES || !seen.insert(child) {
                    continue;
                }
                if let Some(bytes) = status_kib(child, "VmRSS:") {
                    total = total.saturating_add(bytes);
                }
                stack.push(child);
            }
        }
    }
    Some(total)
}

#[cfg(all(test, feature = "crash"))]
mod crash_sampler_budget_tests {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    use crate::crash::{self, CrashMetadata, CrashPolicy};

    const CHILD_ENV: &str = "KERNAL_API_CRASH_BUDGET_CHILD";
    const SHORT_CHILD_ENV: &str = "KERNAL_API_CRASH_SHORT_CHILD";
    const PARKED_THREADS: usize = 16;
    const WINDOW: Duration = Duration::from_secs(3);

    fn sampler_cpu_ns() -> u64 {
        let tasks = std::fs::read_dir("/proc/self/task").expect("task directory");
        for task in tasks.flatten() {
            let name = std::fs::read_to_string(task.path().join("comm")).unwrap_or_default();
            if name.trim() == "rp-crash-sample" {
                let stat = std::fs::read_to_string(task.path().join("schedstat"))
                    .expect("sampler schedstat");
                return stat
                    .split_whitespace()
                    .next()
                    .expect("CPU nanoseconds")
                    .parse()
                    .expect("numeric CPU nanoseconds");
            }
        }
        panic!("rp-crash-sampler thread not found");
    }

    fn measure_child() {
        let parked = Arc::new((Mutex::new(false), Condvar::new()));
        let threads: Vec<_> = (0..PARKED_THREADS)
            .map(|_| {
                let parked = Arc::clone(&parked);
                std::thread::spawn(move || {
                    let (lock, wake) = &*parked;
                    let stopped = lock.lock().expect("park lock");
                    drop(wake.wait_while(stopped, |stopped| !*stopped).expect("park wait"));
                })
            })
            .collect();
        let guard = crash::install(
            CrashPolicy::On,
            CrashMetadata {
                app_class: "test".into(),
                app_name: "crash-sampler-cpu-budget".into(),
                app_version: "0".into(),
                instance_name: String::new(),
                creation_time_ms: 0,
                cwd: String::new(),
            },
        )
        .expect("install native capture");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !guard.sample_ready() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(guard.sample_ready(), "sampler never produced a snapshot");
        assert!(
            guard.sample_thread_count() >= PARKED_THREADS,
            "pre-crash sample omitted parked threads"
        );
        // Let startup work finish before measuring steady-state idle cost.
        std::thread::sleep(Duration::from_millis(500));
        let before = sampler_cpu_ns();
        let started = Instant::now();
        std::thread::sleep(WINDOW);
        let fraction = sampler_cpu_ns().saturating_sub(before) as f64
            / started.elapsed().as_nanos() as f64;
        drop(guard);
        let (lock, wake) = &*parked;
        *lock.lock().expect("park lock") = true;
        wake.notify_all();
        for thread in threads {
            thread.join().expect("join parked thread");
        }
        assert!(
            fraction <= 0.02,
            "idle sampler used {:.1}% of one core with {PARKED_THREADS} parked threads (budget 2%)",
            fraction * 100.0
        );
    }

    /// #357: the native sampler must stay below two percent with parked threads.
    #[test]
    fn sampler_idle_cpu_under_two_percent() {
        if std::env::var_os(CHILD_ENV).is_some() {
            measure_child();
            return;
        }
        let spool = tempfile::tempdir().expect("private crash spool");
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .arg("sampler_idle_cpu_under_two_percent")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_ENV, "1")
            .env(crash::spool::SPOOL_DIR_ENV, spool.path().join("spool"))
            .env_remove(crash::NO_CRASH_HANDLER_ENV)
            .status()
            .expect("run isolated sampler budget test");
        assert!(status.success(), "isolated sampler budget failed: {status}");
    }

    #[test]
    fn short_lived_capture_adds_at_most_ten_milliseconds() {
        if std::env::var_os(SHORT_CHILD_ENV).is_some() {
            let started = Instant::now();
            let guard = crash::install(
                CrashPolicy::On,
                CrashMetadata {
                    app_class: "test".into(),
                    app_name: "crash-short-lived-budget".into(),
                    app_version: "0".into(),
                    instance_name: String::new(),
                    creation_time_ms: 0,
                    cwd: String::new(),
                },
            )
            .expect("install native capture");
            std::thread::sleep(Duration::from_millis(50));
            drop(guard);
            assert!(
                started.elapsed() <= Duration::from_millis(60),
                "install + 50 ms + teardown exceeded the 10 ms overhead budget: {:?}",
                started.elapsed()
            );
            return;
        }
        let spool = tempfile::tempdir().expect("private crash spool");
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .arg("short_lived_capture_adds_at_most_ten_milliseconds")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(SHORT_CHILD_ENV, "1")
            .env(crash::spool::SPOOL_DIR_ENV, spool.path().join("spool"))
            .env_remove(crash::NO_CRASH_HANDLER_ENV)
            .status()
            .expect("run isolated short-lived budget test");
        assert!(status.success(), "short-lived budget failed: {status}");
    }

    #[test]
    fn newly_loaded_module_is_attributed_by_cached_session() {
        let fixture = tempfile::tempdir().expect("isolated dlopen fixture");
        let source = fixture.path().join("new-image.c");
        let library = fixture.path().join("libnew-image.so");
        std::fs::write(&source, "void crash_sampler_new_image(const volatile int *stop) { while (!*stop) {} }\n")
            .expect("write dlopen fixture");
        let built = std::process::Command::new("cc")
            .args(["-shared", "-fPIC", "-Wl,--build-id=sha1", "-o"])
            .arg(&library)
            .arg(&source)
            .status()
            .expect("build dlopen fixture with a GNU build ID");
        assert!(built.success(), "cannot build dlopen fixture: {built}");

        let mut session = crate::snapshot::SessionResolver::new(
            &crate::snapshot::SnapshotConfig::default(),
        );
        session.capture().expect("capture before dlopen");
        let library_c = std::ffi::CString::new(library.to_string_lossy().as_bytes())
            .expect("fixture path has no NUL");
        let handle = unsafe { libc::dlopen(library_c.as_ptr(), libc::RTLD_NOW) };
        assert!(!handle.is_null(), "dlopen rejected the fixture");
        let symbol = unsafe { libc::dlsym(handle, c"crash_sampler_new_image".as_ptr()) };
        assert!(!symbol.is_null(), "new module has no test function");
        let function: unsafe extern "C" fn(*const i32) = unsafe { std::mem::transmute(symbol) };
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || unsafe {
            function(worker_stop.as_ptr());
        });
        std::thread::sleep(Duration::from_millis(10));
        let snapshot = session.capture().expect("capture after dlopen");
        stop.store(1, std::sync::atomic::Ordering::Release);
        worker.join().expect("join library worker");
        let module = session
            .module_inventory()
            .iter()
            .find(|module| module.path.as_deref() == library.to_str())
            .expect("cached inventory includes the newly loaded library");
        let expected_id = module.debug_id.clone();
        let attributed = crate::snapshot::attribute::attribute(
            &snapshot,
            session.module_inventory(),
        );
        let index = attributed.threads.iter().flat_map(|thread| &thread.frames)
            .filter_map(|frame| frame.module_index)
            .find(|&index| attributed.modules[index as usize].path.as_deref() == library.to_str())
            .expect("live frame in newly loaded image is attributed") as usize;
        assert_eq!(attributed.modules[index].path.as_deref(), library.to_str());
        assert_eq!(attributed.modules[index].debug_id, expected_id);
        unsafe { libc::dlclose(handle) };
    }
}
