//! Cost of a session spawn relative to `std::process::Command` for a trivial
//! child (#366). Run explicitly; a wall-clock budget would be flaky on shared
//! runners, so this reports the per-option overhead instead of asserting one.
//!
//! `cargo test --release --test process session_spawn_cost -- --ignored --nocapture`
//!
//! Finding: a bare session tracks `std::process::Command`. Each of
//! `kill_when_owner_dies` and a best-effort priority adds ~0.4-0.8 ms, because
//! the substrate installs both in `pre_exec`, which takes std off its
//! `posix_spawn` path onto `fork`+`exec`; that fork scales with the parent's
//! resident size. Spawn admission is free.
//!
//! The child is this test binary listing a filter that matches nothing: it
//! exists on every host and exits 0, and std and every session spawn the same
//! child, so the reported overhead is comparable across options.

use std::process::{Command, Stdio};
use std::time::Instant;

use kernal_api::{ProcessPriority, ProcessSessionOptions, SpawnAdmission, SpawnSpec, StreamMode};

const SPAWNS: u32 = 200;
const TRIVIAL_ARGS: [&str; 2] = ["--list", "__session_spawn_cost_matches_nothing__"];

fn trivial_child() -> std::path::PathBuf {
    std::env::current_exe().expect("test binary path")
}

fn piped() -> SpawnSpec {
    TRIVIAL_ARGS
        .iter()
        .fold(SpawnSpec::new(trivial_child()), |spec, arg| spec.arg(*arg))
        .stdout(StreamMode::Piped)
        .stderr(StreamMode::Piped)
}

async fn session_ms(spec: impl Fn() -> SpawnSpec) -> f64 {
    let start = Instant::now();
    for _ in 0..SPAWNS {
        let session = spec()
            .spawn_session(ProcessSessionOptions::default())
            .await
            .expect("start session");
        while session.next_output().await.is_some() {}
        session.wait().await.expect("wait child");
    }
    start.elapsed().as_secs_f64() * 1e3 / f64::from(SPAWNS)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement, not a pass/fail budget"]
async fn session_spawn_overhead_against_std_command() {
    for round in 0..3 {
        let start = Instant::now();
        for _ in 0..SPAWNS {
            let out = Command::new(trivial_child())
                .args(TRIVIAL_ARGS)
                .stdin(Stdio::null())
                .output()
                .expect("std spawn");
            assert!(out.status.success());
        }
        let baseline = start.elapsed().as_secs_f64() * 1e3 / f64::from(SPAWNS);
        let added = |label: &str, ms: f64| {
            eprintln!(
                "round {round}: {label:<22} +{:.3} ms over std {baseline:.3} ms",
                ms - baseline
            );
        };
        added("bare session", session_ms(piped).await);
        added(
            "kill_when_owner_dies",
            session_ms(|| piped().kill_when_owner_dies(true)).await,
        );
        added(
            "priority_best_effort",
            session_ms(|| piped().priority_best_effort(ProcessPriority::Low)).await,
        );
        added(
            "spawn_admission",
            session_ms(|| piped().spawn_admission(SpawnAdmission::new(|| Ok(())))).await,
        );
    }
}
