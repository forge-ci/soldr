//! End-to-end #6 owner-death evidence for the private process substrate adapter.
//!
//! The helper is a separate process on purpose: dropping a `PlatformChild`
//! only drops a facade handle, while this contract requires cleanup when the
//! *spawning owner process* exits.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kernal_api::{shell_spec, SpawnSpec, StreamMode};
use sysinfo::{Pid, ProcessStatus, System};

const HELPER_ENV: &str = "KERNAL_API_OWNER_DEATH_HELPER";
const PID_FILE_ENV: &str = "KERNAL_API_OWNER_DEATH_PID_FILE";
const HELPER_TEST: &str =
    "process_owner_death_contract::owner_death_helper_spawns_an_adapted_child";

#[test]
fn owner_death_helper_spawns_an_adapted_child() {
    if std::env::var_os(HELPER_ENV).is_none() {
        return;
    }

    let pid_file = std::env::var_os(PID_FILE_ENV).expect("owner-death helper PID file");
    let runtime = kernal_api::async_engine::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("facade runtime");
    let pid = runtime
        .run_within(
            async {
                long_running_command()
                    .stdin(StreamMode::Null)
                    .kill_when_owner_dies(true)
                    .spawn()
                    .await
                    .expect("spawn child through the adapter")
                    .id()
                    .expect("live adapted child has a PID")
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    // Publish atomically: the owner polls for this path, so it must never see
    // it exist before its contents are complete (kernal-api#569).
    let pid_file = std::path::PathBuf::from(pid_file);
    let staging = pid_file.with_extension("pid.tmp");
    std::fs::write(&staging, pid.to_string()).expect("stage adapted child PID");
    std::fs::rename(&staging, &pid_file).expect("publish adapted child PID");
}

#[test]
fn adapted_child_dies_when_its_spawning_owner_exits() {
    let directory = tempfile::tempdir().expect("owner-death test directory");
    let pid_file = directory.path().join("adapted-child.pid");
    let mut helper = Command::new(std::env::current_exe().expect("test executable"))
        .arg("--exact")
        .arg(HELPER_TEST)
        .arg("--nocapture")
        .env(HELPER_ENV, "1")
        .env(PID_FILE_ENV, &pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start isolated spawning owner");

    let pid = wait_for_pid_file(&pid_file);
    let status = helper.wait().expect("wait for isolated spawning owner");
    assert!(status.success(), "owner helper exits cleanly: {status}");

    let deadline = Instant::now() + Duration::from_secs(5);
    while child_is_live(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    let survived = child_is_live(pid);
    if survived {
        terminate_test_child(pid);
    }
    assert!(
        !survived,
        "adapted child {pid} survived its spawning owner's exit"
    );
}

fn long_running_command() -> SpawnSpec {
    #[cfg(windows)]
    {
        shell_spec("ping 127.0.0.1 -n 31 > nul")
    }
    #[cfg(not(windows))]
    {
        shell_spec("sleep 30")
    }
}

fn wait_for_pid_file(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(contents) = std::fs::read_to_string(path) {
            return contents.trim().parse().expect("numeric adapted child PID");
        }
        assert!(
            Instant::now() < deadline,
            "owner helper did not publish an adapted child PID"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn child_is_live(pid: u32) -> bool {
    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid]),
        true,
        sysinfo::ProcessRefreshKind::everything(),
    );
    !matches!(
        system.process(pid).map(|process| process.status()),
        None | Some(ProcessStatus::Zombie)
    )
}

fn terminate_test_child(pid: u32) {
    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid]),
        true,
        sysinfo::ProcessRefreshKind::everything(),
    );
    if let Some(process) = system.process(pid) {
        let _ = process.kill();
    }
}
