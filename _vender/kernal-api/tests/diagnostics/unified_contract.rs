//! The shared systems pieces must coexist in one final executable.

#[cfg(feature = "tokio-console")]
use std::time::Duration;

use kernal_api::{shell_spec, ProcessCaptureError, SpawnSpec, StreamMode};

#[test]
fn async_process_hal_captures_output() {
    let runtime = kernal_api::async_engine::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("facade-owned runtime");
    runtime
        .run_within(
            async {
                #[cfg(windows)]
                let spec = shell_spec("echo kernal-api & echo kernal-api-error 1>&2");
                #[cfg(not(windows))]
                let spec = shell_spec("printf kernal-api; printf kernal-api-error >&2");

                let output = spec
                    .stdout(StreamMode::Piped)
                    .stderr(StreamMode::Piped)
                    .spawn()
                    .await
                    .expect("spawn through the shared HAL")
                    .wait_with_output_bounded_within(1024, std::time::Duration::from_secs(5))
                    .await
                    .expect("capture output");

                assert!(output.status.success());
                assert!(String::from_utf8_lossy(&output.stdout).contains("kernal-api"));
                assert!(String::from_utf8_lossy(&output.stderr).contains("kernal-api-error"));

                let missing = SpawnSpec::new("kernal-api-program-that-does-not-exist")
                    .spawn()
                    .await;
                assert!(missing.is_err());
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn async_process_hal_reports_a_finite_capture_that_exceeds_its_aggregate_bound() {
    let runtime = kernal_api::async_engine::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("facade-owned runtime");
    runtime
        .run_within(
            async {
                #[cfg(windows)]
                let spec = shell_spec("<nul set /p =12345");
                #[cfg(not(windows))]
                let spec = shell_spec("printf 12345");

                // A finite child reaches EOF after its excess output is drained, so
                // the error below is the output limit, not the time budget.
                let error = spec
                    .stdout(StreamMode::Piped)
                    .stderr(StreamMode::Piped)
                    .spawn()
                    .await
                    .expect("spawn through the shared HAL")
                    .wait_with_output_bounded_within(4, std::time::Duration::from_secs(2))
                    .await
                    .expect_err("aggregate output limit must reject retained output");

                assert!(matches!(
                    error,
                    ProcessCaptureError::OutputLimitExceeded { limit: 4 }
                ));
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn async_process_hal_kills_and_does_not_signal_an_unowned_group() {
    let runtime = kernal_api::async_engine::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("facade-owned runtime");
    runtime
        .run_within(
            async {
                #[cfg(windows)]
                let spec = shell_spec("ping 127.0.0.1 -n 30 > nul");
                #[cfg(not(windows))]
                let spec = shell_spec("sleep 30");

                let mut child = spec
                    .stdin(StreamMode::Null)
                    .create_process_group(false)
                    .spawn()
                    .await
                    .expect("spawn a killable child through the shared HAL");
                assert!(child.id().is_some(), "a live child has an identity");

                assert!(!child
                    .terminate_group_soft()
                    .await
                    .expect("an unowned group is a safe no-op"));
                child.kill().await.expect("hard kill");
                assert_eq!(child.id(), None, "a killed child PID is not reusable");
                assert!(!child
                    .wait_within(std::time::Duration::from_secs(5))
                    .await
                    .expect("reap killed child")
                    .success());
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn async_process_hal_soft_terminates_an_owned_group() {
    let runtime = kernal_api::async_engine::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("facade-owned runtime");
    runtime
        .run_within(
            async {
                #[cfg(windows)]
                let spec = shell_spec("ping 127.0.0.1 -n 30 > nul");
                #[cfg(not(windows))]
                let spec = shell_spec("sleep 30");

                let mut child = spec
                    .stdin(StreamMode::Null)
                    .create_process_group(true)
                    .spawn()
                    .await
                    .expect("spawn a child-owned group through the shared HAL");
                assert!(child
                    .terminate_group_soft()
                    .await
                    .expect("request graceful group termination"));

                // SIGTERM ends the POSIX fixture, making delivery observable. Windows
                // Ctrl+Break is advisory for the console provided by the test harness;
                // delivery above is the portable contract, so retain a bounded cleanup.
                #[cfg(not(windows))]
                {
                    let status = child
                        .wait_within(std::time::Duration::from_secs(5))
                        .await
                        .expect("the owned group signal ends the POSIX child");
                    assert!(!status.success());
                }
                #[cfg(windows)]
                {
                    let _ = child.wait_within(std::time::Duration::from_secs(5)).await;
                    let _ = child.kill().await;
                }
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn async_process_hal_does_not_soft_signal_a_completed_group() {
    let runtime = kernal_api::async_engine::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("facade-owned runtime");
    runtime
        .run_within(
            async {
                #[cfg(windows)]
                let spec = shell_spec("exit /b 0");
                #[cfg(not(windows))]
                let spec = shell_spec("exit 0");

                let mut child = spec
                    .create_process_group(true)
                    .spawn()
                    .await
                    .expect("spawn a child-owned group through the shared HAL");
                assert!(child
                    .wait_within(std::time::Duration::from_secs(5))
                    .await
                    .expect("wait for completed child")
                    .success());
                assert!(!child
                    .terminate_group_soft()
                    .await
                    .expect("completed child has no group left to signal"));
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn async_process_hal_clears_pid_after_a_natural_wait() {
    let runtime = kernal_api::async_engine::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("facade-owned runtime");
    runtime
        .run_within(
            async {
                #[cfg(windows)]
                let spec = shell_spec("exit /b 0");
                #[cfg(not(windows))]
                let spec = shell_spec("exit 0");

                let mut child = spec.spawn().await.expect("spawn a naturally exiting child");
                assert!(child.id().is_some(), "a live child has an identity");
                assert!(child
                    .wait_within(std::time::Duration::from_secs(5))
                    .await
                    .expect("wait for child")
                    .success());
                assert_eq!(child.id(), None, "a waited child PID is not reusable");
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[cfg(feature = "profile")]
#[test]
fn cpu_and_async_profiles_share_one_pprof_schema() {
    use kernal_api::profile::async_profile::{to_pprof, TaskSample};
    use kernal_api::profile::export::to_pprof_bytes;
    use kernal_api::profile::symbolize::Frame;
    use kernal_api::profile::{ProfileMetrics, ResolvedSample, SessionResult};

    let cpu = SessionResult {
        samples: vec![ResolvedSample {
            os_tid: 7,
            frames: vec![Frame {
                function: "worker".to_string(),
                module: "fixture".to_string(),
                relative_address: 0x10,
            }],
            truncated: false,
        }],
        metrics: ProfileMetrics {
            samples_captured: 1,
            duration_nanos: 1_000_000,
            hz: 99,
            ..ProfileMetrics::default()
        },
        start_unix_nanos: 1,
        period_nanos: 10_101_010,
    };
    assert!(!to_pprof_bytes(&cpu).is_empty());

    let tasks = vec![TaskSample {
        spawn_stack: vec!["root".into(), "waiting".into()],
        idle_nanos: 9,
        busy_nanos: 1,
        scheduled_nanos: 2,
        polls: 3,
        wakes: 4,
        name: "task".into(),
    }];
    assert!(!to_pprof(&tasks).is_empty());
}

#[cfg(feature = "allocator")]
#[test]
fn allocator_facade_exposes_dormant_heap_profiler() {
    assert!(!kernal_api::allocator::is_enabled());
    let _allocator_type = std::any::TypeId::of::<kernal_api::allocator::Allocator>();
}

#[cfg(feature = "tokio-console")]
#[test]
fn runtime_profile_configuration_is_product_neutral() {
    let config = kernal_api::async_engine::DiagnosticsConfig::new("127.0.0.1:6669")
        .with_publish_interval(Duration::from_millis(20));
    assert_eq!(config.bind(), "127.0.0.1:6669");
    assert_eq!(config.publish_interval(), Some(Duration::from_millis(20)));
}
