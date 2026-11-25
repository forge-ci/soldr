//! Native cargo lifetime contracts: a separate cargo process group must not
//! survive a killed Soldr parent (Linux), and catchable signals must reach cargo.

use crate::common::*;
use soldr_platform::process::{inspect, test_child};
use std::time::{Duration, Instant};
use test_child::TestSignal;

struct Cleanup(Vec<u32>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for &pid in &self.0 {
            test_child::signal_group(pid, TestSignal::Kill);
            test_child::signal_process(pid, TestSignal::Kill);
        }
    }
}

fn run_signal_case(signal: TestSignal, debug: bool, ignore: bool) {
    let root = unique_temp_dir("cargo-parent-death");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let pid_file = root.join("cargo.pid");
    let descendant_file = root.join("descendant.pid");
    let forwarded = root.join("forwarded");
    let (cargo, rustc, _) = install_fake_toolchain(&root.join("tools.log"));
    let script = if signal == TestSignal::Kill {
        // exec keeps the recorded pid and the kernel parent-death setting.
        "echo $$ > \"$CARGO_LIFETIME_PID\"; echo lifetime-ready; exec sleep 60".to_string()
    } else {
        let action = if ignore { ":" } else { "exit 0" };
        format!("trap 'echo TERM > \"$CARGO_LIFETIME_FORWARDED\"; {action}' TERM; trap 'echo INT > \"$CARGO_LIFETIME_FORWARDED\"; {action}' INT; echo $$ > \"$CARGO_LIFETIME_PID\"; echo lifetime-ready; while :; do sleep 0.05; done")
    };
    let script = if signal != TestSignal::Kill && !ignore {
        format!("sh -c 'trap \"\" TERM INT; echo $$ > \"$CARGO_LIFETIME_DESCENDANT\"; echo descendant-ready; exec sleep 60' &\n{script}")
    } else {
        script
    };
    write_fake_script(&cargo, &format!("#!/bin/sh\ncase \"$1\" in --version) echo 'cargo 1.0.0-test'; exit 0;; metadata) echo '{{}}'; exit 0;; esac\n{script}\n"));
    let mut command = isolated_soldr_command();
    command
        .args(["--no-cache", "cargo", "build"])
        .current_dir(&workspace)
        // ci-test keeps its stages in one group. This fixture exercises Cargo
        // in a separate group owned by the nested Soldr process.
        .env_remove("SOLDR_INTERNAL_INHERIT_PROCESS_GROUP")
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_CACHE_DIR", root.join("cache"))
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env("CARGO_LIFETIME_PID", &pid_file)
        .env("CARGO_LIFETIME_DESCENDANT", &descendant_file)
        .env("CARGO_LIFETIME_FORWARDED", &forwarded)
        .env("SOLDR_DEBUG_TRACE", if debug { "1" } else { "0" });
    let mut child = crate::common::tracked_child::spawn_tracked(&mut command).expect("spawn soldr");
    let mut cleanup = Cleanup(vec![child.pid()]);
    assert!(
        child.wait_for_stdout("lifetime-ready", Instant::now() + Duration::from_secs(20)),
        "fake cargo did not start: {}",
        child.wait_bounded(Duration::from_secs(1)).stderr_lossy()
    );
    let cargo_pid = std::fs::read_to_string(&pid_file)
        .expect("cargo pid")
        .trim()
        .parse()
        .expect("numeric pid");
    cleanup.0.push(cargo_pid);
    if signal != TestSignal::Kill && !ignore {
        assert!(child.wait_for_stdout("descendant-ready", Instant::now() + Duration::from_secs(5)));
        cleanup.0.push(
            std::fs::read_to_string(descendant_file)
                .expect("descendant pid")
                .trim()
                .parse()
                .expect("numeric descendant pid"),
        );
    }
    test_child::signal_process(child.pid(), signal);
    let output = child.wait_bounded(Duration::from_secs(5));
    assert!(
        !output.timed_out,
        "Soldr did not exit: {}",
        output.stderr_lossy()
    );
    assert!(
        !output.status.expect("exit status").success(),
        "interruption must fail"
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let survivor = cleanup.0[1..]
            .iter()
            .copied()
            .find(|pid| inspect::is_alive(*pid));
        let Some(pid) = survivor else {
            break;
        };
        assert!(
            Instant::now() < deadline,
            "PID {pid} survived {signal:?} (cargo PID {cargo_pid})"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(output.pipes_closed, "cargo retained its inherited pipes");
    cleanup.0.clear();
    if signal != TestSignal::Kill {
        let expected = if signal == TestSignal::Interrupt {
            "INT"
        } else {
            "TERM"
        };
        assert_eq!(
            std::fs::read_to_string(forwarded)
                .expect("forwarded signal")
                .trim(),
            expected
        );
    }
}

#[test]
fn sigkill_of_soldr_kills_cargo_in_its_separate_group() {
    if soldr_platform::host::facts::os() != soldr_platform::host::facts::HostOs::Linux {
        eprintln!("skip: parent-death signals require Linux");
        return;
    }
    run_signal_case(TestSignal::Kill, false, false);
    run_signal_case(TestSignal::Kill, true, false);
}

#[test]
fn cargo_receives_termination_and_interrupt_and_ignoring_child_is_killed() {
    if soldr_platform::host::facts::os() == soldr_platform::host::facts::HostOs::Windows {
        eprintln!("skip: POSIX signal forwarding contract");
        return;
    }
    run_signal_case(TestSignal::Terminate, false, false);
    run_signal_case(TestSignal::Interrupt, false, false);
    run_signal_case(TestSignal::Terminate, true, false);
    run_signal_case(TestSignal::Interrupt, true, false);
    run_signal_case(TestSignal::Terminate, false, true);
}
