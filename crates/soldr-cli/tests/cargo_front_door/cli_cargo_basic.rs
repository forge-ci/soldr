#![allow(unused_imports)]

use crate::common;

use crate::common::*;
use serde_json::Value;
use std::io::Write;
use std::process::Command;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[test]
fn cargo_front_door_runs_real_cargo() {
    let cache_root = unique_temp_dir("cargo-version");
    let output = common::isolated_soldr_command()
        .args(["--no-cache", "cargo", "--version"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr cargo --version");

    assert!(output.status.success(), "cargo front door failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("cargo"),
        "unexpected cargo output: {stdout}"
    );
    assert!(
        !stderr.contains("soldr: fetching cargo"),
        "cargo front door should not fetch cargo: {stderr}"
    );
}

#[test]
fn cargo_front_door_consumes_no_cache_flag() {
    let cache_root = unique_temp_dir("cargo-no-cache");
    let output = common::isolated_soldr_command()
        .args(["--no-cache", "cargo", "--version"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr --no-cache cargo --version");

    assert!(
        output.status.success(),
        "cargo front door with top-level --no-cache failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("cargo"),
        "unexpected cargo output with --no-cache: {stdout}"
    );
    assert!(
        !stderr.contains("unexpected argument '--no-cache'"),
        "--no-cache should be consumed by soldr, not forwarded to cargo: {stderr}"
    );
}

#[test]
fn cargo_front_door_preserves_live_target_hash_families() {
    let root = unique_temp_dir("cargo-preserves-target-families");
    let workspace = root.join("workspace");
    let tool_dir = root.join("tool");
    let cache_root = root.join("soldr-cache");
    let cargo_log = root.join("cargo.log");
    let deps = workspace.join("target").join("debug").join("deps");
    fs::create_dir_all(workspace.join("src")).expect("workspace src");
    fs::create_dir_all(&tool_dir).expect("tool dir");
    fs::create_dir_all(&deps).expect("target deps");
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"preserve_families\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    fs::write(workspace.join("src/lib.rs"), "pub fn ok() {}\n").expect("source");

    let first_family = deps.join("libshared-aaaaaaaaaaaaa.rlib");
    let second_family = deps.join("libshared-bbbbbbbbbbbbb.rlib");
    fs::write(&first_family, b"first").expect("first hash family");
    fs::write(&second_family, b"second").expect("second hash family");

    let cargo = fake_script_path(&tool_dir, "cargo");
    write_fake_script(&cargo, &fake_cargo_toolchain_recorder_script(&cargo_log));

    for attempt in 1..=2 {
        let output = common::isolated_soldr_command()
            .args(["--no-cache", "cargo", "check"])
            .current_dir(&workspace)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_CARGO_BIN", &cargo)
            .env("ZCCACHE_DISABLE", "1")
            .output()
            .expect("run soldr cargo check");

        assert!(
            output.status.success(),
            "fake Cargo invocation {attempt} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            first_family.exists() && second_family.exists(),
            "the Cargo front door must not delete coexisting target hash families \
             during unchanged invocation {attempt}; first_exists={} second_exists={}\n\
             stderr:\n{}",
            first_family.exists(),
            second_family.exists(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn zthreads_retry_replays_private_front_door_contract() {
    let root = unique_temp_dir("zthreads-retry-contract");
    let tool_dir = root.join("tool");
    let cache_root = root.join("soldr-cache");
    let log_path = root.join("cargo.log");
    fs::create_dir_all(&tool_dir).expect("tool dir");
    let cargo = fake_script_path(&tool_dir, "cargo");
    write_fake_script(&cargo, &fake_zthreads_retry_cargo_script(&log_path));

    let output = zthreads_retry_command(&cargo, &cache_root)
        .output()
        .expect("run stable-rustc fallback contract probe");

    assert!(
        output.status.success(),
        "fallback should succeed on the second Cargo attempt\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let log = fs::read_to_string(&log_path).expect("read fake Cargo log");
    let build_attempts: Vec<&str> = log
        .lines()
        .filter(|line| line.ends_with("args=build"))
        .collect();
    assert_eq!(
        build_attempts.len(),
        2,
        "the fallback must run exactly one initial and one retry attempt: {log}",
    );
    assert!(
        build_attempts.iter().any(|line| line.contains(
            "attempt=1 trust=inherited sentinel= wrapper= cache=0 args=build"
        )),
        "the retry must preserve trusted workspace state, remain uncached, and keep its internal sentinel out of Cargo: {log}",
    );
}

#[test]
fn zthreads_retry_failure_stops_after_one_retry() {
    let root = unique_temp_dir("zthreads-retry-once");
    let tool_dir = root.join("tool");
    let cache_root = root.join("soldr-cache");
    let log_path = root.join("cargo.log");
    fs::create_dir_all(&tool_dir).expect("tool dir");
    let cargo = fake_script_path(&tool_dir, "cargo");
    write_fake_script(&cargo, &fake_zthreads_retry_cargo_script(&log_path));

    let output = zthreads_retry_command(&cargo, &cache_root)
        .env("SOLDR_TEST_ZTHREADS_RETRY_FAIL", "1")
        .output()
        .expect("run failing stable-rustc fallback probe");

    assert!(
        !output.status.success(),
        "a failed retry must propagate its nonzero status"
    );
    let log = fs::read_to_string(&log_path).expect("read fake Cargo log");
    let build_attempts: Vec<&str> = log
        .lines()
        .filter(|line| line.ends_with("args=build"))
        .collect();
    assert_eq!(
        build_attempts.len(),
        2,
        "a matching diagnostic on the retry must not recurse into a third attempt: {log}",
    );
    assert!(
        build_attempts[0].starts_with("attempt= "),
        "the first Cargo attempt must not carry the recursion marker: {log}",
    );
    assert!(
        build_attempts[1].starts_with("attempt=1 "),
        "the sole retry must carry the recursion marker: {log}",
    );
}

#[test]
fn cargo_front_door_maps_plus_toolchain_to_rustup_toolchain_env() {
    let cache_root = unique_temp_dir("cargo-plus-toolchain");
    let tool_dir = unique_temp_dir("cargo-plus-toolchain-bin");
    let log_path = cache_root.join("cargo.log");
    let cargo = fake_script_path(&tool_dir, "cargo");
    let rustc = fake_script_path(&tool_dir, "rustc");
    // soldr#3195: the front door installs the requested toolchain before it
    // execs cargo. Without a fake rustup that was a real `rustup toolchain
    // install nightly-2026-03-26` on every host lacking that nightly.
    let rustup = fake_script_path(&tool_dir, "rustup");

    write_fake_script(&cargo, &fake_cargo_toolchain_recorder_script(&log_path));
    write_fake_script(&rustc, &fake_rustc_script(&log_path));
    write_fake_script(
        &rustup,
        if matches!(
            soldr_platform::host::facts::os(),
            soldr_platform::host::facts::HostOs::Windows
        ) {
            "@echo off\nexit /b 0\n"
        } else {
            "#!/bin/sh\nexit 0\n"
        },
    );

    let output = common::isolated_soldr_command()
        .args([
            "--no-cache",
            "cargo",
            "+nightly-2026-03-26",
            "test",
            "--manifest-path",
            "dylints/ban_manual_slash_normalize/Cargo.toml",
        ])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        // soldr#3203: the relative `--manifest-path` resolves under this crate,
        // whose workspace target is the suite's own `target/`.
        .env("CARGO_TARGET_DIR", cache_root.join("target"))
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .expect("failed to run soldr cargo +toolchain test");

    assert!(
        output.status.success(),
        "cargo +toolchain front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake cargo log");
    assert!(
        log.contains("toolchain=nightly-2026-03-26"),
        "front door should map +toolchain to RUSTUP_TOOLCHAIN: {log}"
    );
    assert!(
        log.contains(
            "args=test\u{1f}--manifest-path\u{1f}dylints/ban_manual_slash_normalize/Cargo.toml"
        ),
        "front door should strip +toolchain before execing concrete cargo: {log}"
    );
}

#[test]
fn cargo_multicall_shim_routes_rustc_through_cargo_front_door() {
    let root = unique_temp_dir("cargo-rustc-multicall");
    let shim_dir = root.join("shims");
    let log_path = root.join("cargo.log");
    fs::create_dir_all(&shim_dir).expect("create shim dir");
    let cargo_shim = shim_dir.join(
        if matches!(
            soldr_platform::host::facts::os(),
            soldr_platform::host::facts::HostOs::Windows
        ) {
            "cargo.exe"
        } else {
            "cargo"
        },
    );
    let soldr = common::soldr_bin();
    fs::copy(&soldr, &cargo_shim).expect("copy soldr as cargo multicall shim");
    let cargo = install_logging_fake_cargo(&log_path);

    let mut command = Command::new(&cargo_shim);
    common::scrub_outer_soldr_env(&mut command);
    // soldr#3452: start where no toolchain pin exists above (the front door
    // resolves a pin from any ancestor, like rustup).
    command.current_dir(common::pin_free_working_dir());
    let output = command
        .args([
            "rustc",
            "--profile",
            "release",
            "--message-format",
            "json-render-diagnostics",
        ])
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_CACHE_DIR", root.join("cache"))
        // soldr#3203: from this crate Cargo's target is the repository's own
        // `target/`, which the running suite lives in.
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env("ZCCACHE_DISABLE", "1")
        .output()
        .expect("run cargo multicall shim");

    assert!(
        output.status.success(),
        "cargo rustc must route through the cargo front door\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("Unrecognized option: 'profile'"),
        "cargo-only --profile flag reached rustc"
    );
    let invocations = read_logged_cargo_invocations(&log_path);
    assert_eq!(
            invocations.iter().find(|argv| {
                argv.get(1).is_some_and(|arg| arg == "--profile")
                    && argv.get(3).is_some_and(|arg| arg == "--message-format")
            }),
            Some(&vec![
                "rustc".to_string(),
                "--profile".to_string(),
                "release".to_string(),
                "--message-format".to_string(),
                "json-render-diagnostics".to_string(),
            ]),
            "cargo front door must preserve the requested argv alongside any metadata or GC probes: {invocations:?}"
        );
}

fn fake_cargo_toolchain_recorder_script(log_path: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             setlocal enabledelayedexpansion\n\
             set \"args=\"\n\
             :loop\n\
             if \"%~1\"==\"\" goto done\n\
             if defined args (set \"args=!args!\u{1f}%~1\") else (set \"args=%~1\")\n\
             shift\n\
             goto loop\n\
             :done\n\
             echo toolchain=%RUSTUP_TOOLCHAIN% args=!args!>>\"{}\"\n\
             exit /b 0\n",
            log_path.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             sep=$(printf '\\037')\n\
             out=\"\"\n\
             first=1\n\
             for arg in \"$@\"; do\n\
               if [ $first -eq 1 ]; then\n\
                 out=\"$arg\"\n\
                 first=0\n\
               else\n\
                 out=\"$out${{sep}}$arg\"\n\
               fi\n\
             done\n\
             printf 'toolchain=%s args=%s\\n' \"${{RUSTUP_TOOLCHAIN:-}}\" \"$out\" >> \"{}\"\n\
             exit 0\n",
            log_path.display()
        )
    }
}

fn fake_zthreads_retry_cargo_script(log_path: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             set \"attempt=\"\n\
             if exist \"%SOLDR_TEST_ZTHREADS_ATTEMPT_MARKER%\" set \"attempt=1\"\n\
             echo attempt=%attempt% trust=%SOLDR_TARGET_CACHE_MODE% sentinel=%SOLDR_INTERNAL_ZTHREADS_FALLBACK_ATTEMPTED% wrapper=%RUSTC_WRAPPER% cache=%SOLDR_CACHE_ENABLED% args=%*>>\"{}\"\n\
             if /I not \"%~1\"==\"build\" exit /b 0\n\
             if not exist \"%SOLDR_TEST_ZTHREADS_ATTEMPT_MARKER%\" (\n\
               type nul > \"%SOLDR_TEST_ZTHREADS_ATTEMPT_MARKER%\"\n\
               echo error: the option `Z` is only accepted on the nightly compiler 1>&2\n\
               exit /b 1\n\
             )\n\
             if defined SOLDR_TEST_ZTHREADS_RETRY_FAIL (\n\
               echo error: the option `Z` is only accepted on the nightly compiler 1>&2\n\
               exit /b 1\n\
             )\n\
             exit /b 0\n",
            log_path.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             attempt=''\n\
             if [ -e \"$SOLDR_TEST_ZTHREADS_ATTEMPT_MARKER\" ]; then attempt=1; fi\n\
             printf 'attempt=%s trust=%s sentinel=%s wrapper=%s cache=%s args=%s\\n' \"$attempt\" \"${{SOLDR_TARGET_CACHE_MODE:-}}\" \"${{SOLDR_INTERNAL_ZTHREADS_FALLBACK_ATTEMPTED:-}}\" \"${{RUSTC_WRAPPER:-}}\" \"${{SOLDR_CACHE_ENABLED:-}}\" \"$*\" >> \"{}\"\n\
             if [ \"$1\" != 'build' ]; then exit 0; fi\n\
             if [ ! -e \"$SOLDR_TEST_ZTHREADS_ATTEMPT_MARKER\" ]; then\n\
               : > \"$SOLDR_TEST_ZTHREADS_ATTEMPT_MARKER\"\n\
               echo 'error: the option `Z` is only accepted on the nightly compiler' >&2\n\
               exit 1\n\
             fi\n\
             if [ -n \"${{SOLDR_TEST_ZTHREADS_RETRY_FAIL:-}}\" ]; then\n\
               echo 'error: the option `Z` is only accepted on the nightly compiler' >&2\n\
               exit 1\n\
             fi\n\
             exit 0\n",
            log_path.display()
        )
    }
}

fn zthreads_retry_command(cargo: &Path, cache_root: &Path) -> Command {
    let mut command = common::isolated_soldr_command();
    command
        .args([
            "--no-cache",
            "--trust-inherited-soldr-env",
            "cargo",
            "build",
            "--no-gc-target",
            "--no-trampoline",
        ])
        .env("SOLDR_TEST_CARGO_BIN", cargo)
        .env("SOLDR_CACHE_DIR", cache_root)
        // soldr#3203: keep the no-cache retry out of the suite's own `target/`.
        .env("CARGO_TARGET_DIR", cache_root.with_file_name("target"))
        .env("SOLDR_TARGET_CACHE_MODE", "inherited")
        .env(
            "SOLDR_TEST_ZTHREADS_ATTEMPT_MARKER",
            cache_root.join("fake-cargo-attempted"),
        )
        .env("RUSTFLAGS", "-Zthreads=8")
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("RUSTC_BOOTSTRAP")
        .env_remove("SOLDR_INTERNAL_ZTHREADS_FALLBACK_ATTEMPTED")
        .env_remove("SOLDR_TEST_ZTHREADS_RETRY_FAIL");
    command
}

fn fake_timeout_then_success_cargo_script(marker: &Path, log_path: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             echo cargo %*>>\"{1}\"\n\
             if exist \"{0}\" (\n\
               if exist target\\debug\\incremental\\poison\\state.bin (\n\
                 echo Poisoned incremental state still present 1>&2\n\
                 ping -n 6 127.0.0.1 >nul\n\
               )\n\
               exit /b 0\n\
             )\n\
             type nul > \"{0}\"\n\
             if not exist target\\debug\\incremental\\poison mkdir target\\debug\\incremental\\poison\n\
             echo stale>target\\debug\\incremental\\poison\\state.bin\n\
             echo Blocking waiting for file lock on artifact directory 1>&2\n\
             ping -n 6 127.0.0.1 >nul\n\
             exit /b 0\n",
            marker.display(),
            log_path.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             echo \"cargo $*\" >> \"{1}\"\n\
             if [ -f \"{0}\" ]; then\n\
               if [ -f target/debug/incremental/poison/state.bin ]; then\n\
                 echo 'Poisoned incremental state still present' >&2\n\
                 sleep 5\n\
               fi\n\
               exit 0\n\
             fi\n\
             : > \"{0}\"\n\
             mkdir -p target/debug/incremental/poison\n\
             printf stale > target/debug/incremental/poison/state.bin\n\
             echo 'Blocking waiting for file lock on artifact directory' >&2\n\
             sleep 5\n\
             exit 0\n",
            marker.display(),
            log_path.display()
        )
    }
}

fn fake_long_running_cargo_script(mode: &str, log_path: &Path, lock_path: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        let body = match mode {
            "progress" => String::from(
                "for /L %%i in (1,1,3) do (\n  echo cargo progress %%i\n  ping -n 2 127.0.0.1 >nul\n)\n",
            ),
            "cpu" => String::from(
                "powershell -NoProfile -Command \"$until=[DateTime]::UtcNow.AddSeconds(2); $n=0; while ([DateTime]::UtcNow -lt $until) { $n++ }\" >nul\n",
            ),
            "lock" => format!(
                "echo locked>\"{0}\"\nstart /B \"\" powershell -NoProfile -Command \"Start-Sleep -Seconds 2; Remove-Item -LiteralPath '{0}' -Force\"\n:wait_lock\nif exist \"{0}\" (\n  ping -n 2 127.0.0.1 >nul\n  goto wait_lock\n)\n",
                lock_path.display()
            ),
            other => panic!("unknown fake cargo mode: {other}"),
        };
        format!(
            "@echo off\necho cargo %*>>\"{}\"\n{}exit /b 0\n",
            log_path.display(),
            body
        )
    } else {
        let body = match mode {
            "progress" => String::from(
                "i=1\nwhile [ \"$i\" -le 3 ]; do\n  echo \"cargo progress $i\"\n  sleep 1\n  i=$((i + 1))\ndone\n",
            ),
            "cpu" => String::from(
                "sleep 2 &\ncpu_timer=$!\nwhile kill -0 \"$cpu_timer\" 2>/dev/null; do\n  :\ndone\nwait \"$cpu_timer\"\n",
            ),
            "lock" => format!(
                "lock='{0}'\n: > \"$lock\"\n(sleep 2; rm -f \"$lock\") &\nunlocker=$!\nwhile [ -e \"$lock\" ] && kill -0 \"$unlocker\" 2>/dev/null; do sleep 0.1; done\nwait \"$unlocker\"\n[ ! -e \"$lock\" ]\n",
                lock_path.display()
            ),
            other => panic!("unknown fake cargo mode: {other}"),
        };
        format!(
            "#!/bin/sh\nset -eu\necho \"cargo $*\" >> \"{}\"\n{}",
            log_path.display(),
            body
        )
    }
}

fn fake_always_slow_cargo_script(log_path: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\necho cargo %*>>\"{0}\"\nif \"%~1\"==\"metadata\" exit /b 0\nping -n 6 127.0.0.1 >nul\nexit /b 0\n",
            log_path.display(),
        )
    } else {
        format!(
            "#!/bin/sh\necho \"cargo $*\" >> \"{0}\"\n[ \"${{1:-}}\" = metadata ] && exit 0\nsleep 5\nexit 0\n",
            log_path.display(),
        )
    }
}

/// This bounds the whole test fixture, including a cold Soldr front door. The
/// Cargo product timeout under test remains four seconds.
const CARGO_TIMEOUT_TEST_EXECUTION_BUDGET: Duration = Duration::from_secs(90);

fn wait_for_timeout_test_completion(
    child: common::tracked_child::TrackedChild,
    label: &str,
) -> std::process::Output {
    let output = child.wait_bounded(CARGO_TIMEOUT_TEST_EXECUTION_BUDGET);
    assert!(
        !output.timed_out,
        "{label}: outer test execution exceeded {:?} ({}); stdout:\n{}\nstderr:\n{}",
        CARGO_TIMEOUT_TEST_EXECUTION_BUDGET,
        output.disposition(),
        output.stdout_lossy(),
        output.stderr_lossy(),
    );
    output.into_output()
}

fn assert_startup_phases_in_order(stderr: &str, expected: &[&str]) {
    let phases: Vec<&str> = stderr
        .lines()
        .filter_map(|line| line.strip_prefix("soldr front-door: startup phase="))
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    let mut phases = phases.into_iter();
    for expected_phase in expected {
        assert!(
            phases.any(|phase| phase == *expected_phase),
            "missing ordered startup phase {expected_phase:?}; trace: {stderr}"
        );
    }
}

fn fake_cargo_with_descendant_script(log_path: &Path, survived_path: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\necho cargo %*>>\"{0}\"\nif \"%~1\"==\"metadata\" exit /b 0\nstart /B \"\" cmd /C \"ping -n 4 127.0.0.1 ^>nul ^& type nul ^> ^\"{1}^\"\"\nping -n 11 127.0.0.1 >nul\nexit /b 0\n",
            log_path.display(),
            survived_path.display()
        )
    } else {
        format!(
            "#!/bin/sh\necho \"cargo $*\" >> \"{0}\"\n[ \"${{1:-}}\" = metadata ] && exit 0\n(sleep 3; : > '{1}') &\nsleep 10\nexit 0\n",
            log_path.display(),
            survived_path.display()
        )
    }
}

#[test]
fn fake_long_running_cargo_script_propagates_failures() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let root = unique_temp_dir("fake-long-running-cargo-failures");
    let tool_dir = root.join("tool");
    let cargo = fake_script_path(&tool_dir, "cargo");
    let lock_path = root.join("unused.lock");
    fs::create_dir_all(&tool_dir).expect("tool dir");

    let missing_log = root.join("missing").join("cargo.log");
    write_fake_script(
        &cargo,
        &fake_long_running_cargo_script("cpu", &missing_log, &lock_path),
    );
    let setup_failure = Command::new(&cargo)
        .arg("build")
        .output()
        .expect("run fake cargo with missing log directory");
    assert!(
        !setup_failure.status.success(),
        "fake cargo must propagate setup failures"
    );

    let log_path = root.join("cargo.log");
    write_fake_script(
        &cargo,
        &fake_long_running_cargo_script("cpu", &log_path, &lock_path),
    );
    let runtime_failure = Command::new(&cargo)
        .arg("build")
        .env("PATH", "")
        .output()
        .expect("run fake cargo without sleep on PATH");
    assert!(
        !runtime_failure.status.success(),
        "fake cargo must propagate runtime failures"
    );
}

#[test]
fn cargo_without_timeout_allows_progress_cpu_and_lock_waits() {
    let root = unique_temp_dir("cargo-no-wall-clock-timeout");
    let workspace = root.join("workspace");
    let tool_dir = root.join("tool");
    let cache_root = root.join("soldr-cache");
    let log_path = root.join("cargo.log");
    let lock_path = root.join("legitimate.lock");
    fs::create_dir_all(workspace.join("src")).expect("workspace src");
    fs::create_dir_all(&tool_dir).expect("tool dir");
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"no_timeout\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    fs::write(workspace.join("src/lib.rs"), "pub fn ok() {}\n").expect("source");
    let cargo = fake_script_path(&tool_dir, "cargo");

    for mode in ["progress", "cpu", "lock"] {
        write_fake_script(
            &cargo,
            &fake_long_running_cargo_script(mode, &log_path, &lock_path),
        );
        let started = Instant::now();
        let output = common::isolated_soldr_command()
            .args(["--no-cache", "cargo", "build"])
            .current_dir(&workspace)
            .env("SOLDR_TEST_CARGO_BIN", &cargo)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env_remove("SOLDR_CARGO_WAIT_TIMEOUT_SECS")
            .output()
            .unwrap_or_else(|err| panic!("run fake {mode} cargo child: {err}"));

        assert!(
            output.status.success(),
            "{mode} child must complete without a default deadline\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            started.elapsed() >= Duration::from_secs(1),
            "{mode} child should outlive the one-second simulated timeout used by timeout tests"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("timed out after"),
            "{mode} child was unexpectedly timed out: {stderr}"
        );
        if mode == "progress" {
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("cargo progress"),
                "progressing child output must remain visible"
            );
        }
    }
}

#[test]
fn cargo_invalid_timeout_fails_before_spawning_cargo() {
    let root = unique_temp_dir("cargo-invalid-wall-clock-timeout");
    let tool_dir = root.join("tool");
    let log_path = root.join("cargo.log");
    fs::create_dir_all(&tool_dir).expect("tool dir");
    let cargo = fake_script_path(&tool_dir, "cargo");
    write_fake_script(&cargo, &fake_always_slow_cargo_script(&log_path));

    for value in ["", "invalid", "-1", "18446744073709551616"] {
        let output = common::isolated_soldr_command()
            .args(["--no-cache", "cargo", "build"])
            .env("SOLDR_TEST_CARGO_BIN", &cargo)
            .env("SOLDR_CACHE_DIR", root.join("soldr-cache"))
            .env("SOLDR_CARGO_WAIT_TIMEOUT_SECS", value)
            .output()
            .unwrap_or_else(|err| panic!("run invalid timeout {value:?}: {err}"));
        assert!(!output.status.success(), "{value:?} must be rejected");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("SOLDR_CARGO_WAIT_TIMEOUT_SECS"),
            "diagnostic must name the invalid variable"
        );
        assert!(!log_path.exists(), "invalid config must not spawn Cargo");
    }
}

#[test]
fn cargo_explicit_timeout_kills_and_reaps_descendants() {
    let root = unique_temp_dir("cargo-timeout-process-tree");
    let workspace = root.join("workspace");
    let tool_dir = root.join("tool");
    let cache_root = root.join("soldr-cache");
    let log_path = root.join("cargo.log");
    let survived_path = root.join("descendant-survived");
    fs::create_dir_all(workspace.join("src")).expect("workspace src");
    fs::create_dir_all(&tool_dir).expect("tool dir");
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"timeout_tree\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    fs::write(workspace.join("src/lib.rs"), "pub fn ok() {}\n").expect("source");
    let cargo = fake_script_path(&tool_dir, "cargo");
    write_fake_script(
        &cargo,
        &fake_cargo_with_descendant_script(&log_path, &survived_path),
    );

    let output = common::isolated_soldr_command()
        .args(["--no-cache", "cargo", "build"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_CARGO_WAIT_TIMEOUT_SECS", "1")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("soldr cargo build with descendant");
    assert!(!output.status.success(), "explicit timeout must fail");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("timed out after 1 seconds"),
        "explicit timeout should retain timeout diagnostics"
    );

    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !survived_path.exists(),
        "the timed-out Cargo descendant escaped process-tree termination"
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn cargo_timeout_cleans_incremental_and_next_run_succeeds() {
    let root = unique_temp_dir("cargo-timeout-cleanup");
    let workspace = root.join("workspace");
    let tool_dir = root.join("tool");
    let cache_root = root.join("soldr-cache");
    let log_path = root.join("cargo.log");
    let marker = root.join("first-run.marker");
    fs::create_dir_all(workspace.join("src")).expect("workspace src");
    fs::create_dir_all(&tool_dir).expect("tool dir");
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"timeout_cleanup\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    fs::write(workspace.join("src/lib.rs"), "pub fn ok() {}\n").expect("source");
    let cargo = fake_script_path(&tool_dir, "cargo");
    write_fake_script(
        &cargo,
        &fake_timeout_then_success_cargo_script(&marker, &log_path),
    );

    let first = common::isolated_soldr_command()
        .args(["--no-cache", "cargo", "build"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_CARGO_WAIT_TIMEOUT_SECS", "1")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("first soldr cargo build");

    assert!(
        !first.status.success(),
        "first fake cargo invocation should time out\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let first_stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        first_stderr.contains("timed out after 1 seconds"),
        "timeout should be explicit: {first_stderr}"
    );
    assert!(
        first_stderr.contains("soldr cleanup after abort"),
        "timeout should report cleanup: {first_stderr}"
    );
    assert!(
        first_stderr.contains("removed 1 incremental/ dir(s)"),
        "timeout cleanup should remove poisoned incremental dir: {first_stderr}"
    );
    assert!(
        !workspace.join("target/debug/incremental").exists(),
        "aborted-build cleanup should remove target/debug/incremental"
    );
    let abort_log_path = cache_root.join("logs").join("cargo-aborts.jsonl");
    let abort_log = fs::read_to_string(&abort_log_path).unwrap_or_else(|err| {
        panic!(
            "first timeout should persist cargo abort log at {}: {err}",
            abort_log_path.display()
        )
    });
    let abort_record: Value = serde_json::from_str(
        abort_log
            .lines()
            .next()
            .expect("cargo abort log should have one record"),
    )
    .expect("cargo abort log should be JSON");
    assert_eq!(abort_record["event"], Value::from("cargo_abort"));
    assert_eq!(abort_record["timeout"], Value::from(true));
    assert_eq!(abort_record["auto_retry_planned"], Value::from(false));
    assert_eq!(
        abort_record["cleanup"]["incremental_dirs_removed"],
        Value::from(1)
    );
    assert_eq!(
        abort_record["recovery"]["retry_with_zccache_disabled"]["argv"],
        serde_json::json!(["soldr", "cargo", "build"])
    );
    assert_eq!(
        abort_record["recovery"]["retry_with_zccache_disabled"]["env"]["ZCCACHE_DISABLE"],
        Value::from("1")
    );
    assert!(!abort_record["recovery"].to_string().contains("--no-cache"));

    let second = common::isolated_soldr_command()
        .args(["--no-cache", "cargo", "build"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_CARGO_WAIT_TIMEOUT_SECS", "1")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("second soldr cargo build");

    assert!(
        second.status.success(),
        "second fake cargo invocation should complete after cleanup\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    let second_stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        !second_stderr.contains("Poisoned incremental state still present"),
        "second invocation should not observe stale incremental state: {second_stderr}"
    );
    let log = fs::read_to_string(&log_path).expect("fake cargo log");
    assert!(
        log.lines().count() >= 2,
        "fake cargo should have been invoked twice: {log}"
    );
}

#[test]
fn cargo_timeout_retries_once_without_cache() {
    let root = unique_temp_dir("cargo-timeout-retry");
    let workspace = root.join("workspace");
    let tool_dir = root.join("tool");
    let cache_root = root.join("soldr-cache");
    let log_path = root.join("cargo.log");
    let fake_tool_log = root.join("fake-toolchain.log");
    let marker = root.join("first-run.marker");
    fs::create_dir_all(workspace.join("src")).expect("workspace src");
    fs::create_dir_all(&tool_dir).expect("tool dir");
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"timeout_retry\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    fs::write(workspace.join("src/lib.rs"), "pub fn ok() {}\n").expect("source");
    let cargo = fake_script_path(&tool_dir, "cargo");
    let (_unused_cargo, rustc, _zccache) = install_fake_toolchain(&fake_tool_log);
    write_fake_script(
        &cargo,
        &fake_timeout_then_success_cargo_script(&marker, &log_path),
    );

    let output = common::isolated_soldr_command()
        .args(["cargo", "build"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_CARGO_WAIT_TIMEOUT_SECS", "1")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("soldr cargo build with timeout retry");

    assert!(
            output.status.success(),
            "cached run should time out once, retry without cache, and succeed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("retrying timed-out cargo run without cache"),
        "timeout should announce the no-cache retry: {stderr}"
    );
    assert!(
        stderr.contains("no-cache cargo retry exited with code 0"),
        "successful retry should report its exit code: {stderr}"
    );
    assert!(
        !workspace.join("target/debug/incremental").exists(),
        "aborted cached run should clean target/debug/incremental before retry"
    );

    let abort_log_path = cache_root.join("logs").join("cargo-aborts.jsonl");
    let abort_log = fs::read_to_string(&abort_log_path).unwrap_or_else(|err| {
        panic!(
            "timeout retry should persist cargo abort log at {}: {err}",
            abort_log_path.display()
        )
    });
    let abort_record: Value = serde_json::from_str(
        abort_log
            .lines()
            .next()
            .expect("cargo abort log should have one record"),
    )
    .expect("cargo abort log should be JSON");
    assert_eq!(abort_record["event"], Value::from("cargo_abort"));
    assert_eq!(abort_record["timeout"], Value::from(true));
    assert_eq!(abort_record["timeout_config"]["explicit"], true);
    assert_eq!(
        abort_record["timeout_config"]["source"],
        "SOLDR_CARGO_WAIT_TIMEOUT_SECS"
    );
    assert_eq!(abort_record["timeout_config"]["duration_secs"], 1);
    assert_eq!(abort_record["auto_retry_planned"], Value::from(true));
    assert_eq!(
        abort_record["recovery"]["retry_with_zccache_disabled"]["argv"],
        serde_json::json!(["soldr", "cargo", "build"])
    );
    assert!(!abort_record["recovery"].to_string().contains("--no-cache"));

    let log = fs::read_to_string(&log_path).expect("fake cargo log");
    assert!(
        log.lines().count() >= 2,
        "fake cargo should have been invoked by the timed-out run and the no-cache retry: {log}"
    );
}

#[test]
fn cargo_timeout_during_no_cache_retry_does_not_recurse() {
    let root = unique_temp_dir("cargo-timeout-retry-no-recursion");
    let workspace = root.join("workspace");
    let tool_dir = root.join("tool");
    let cache_root = root.join("soldr-cache");
    let log_path = root.join("cargo.log");
    let fake_tool_log = root.join("fake-toolchain.log");
    fs::create_dir_all(workspace.join("src")).expect("workspace src");
    fs::create_dir_all(&tool_dir).expect("tool dir");
    fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname = \"timeout_retry_no_recursion\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
    fs::write(workspace.join("src/lib.rs"), "pub fn ok() {}\n").expect("source");
    let cargo = fake_script_path(&tool_dir, "cargo");
    let (_unused_cargo, rustc, _zccache) = install_fake_toolchain(&fake_tool_log);
    write_fake_script(&cargo, &fake_always_slow_cargo_script(&log_path));

    let mut command = common::isolated_soldr_command();
    command
        .args(["cargo", "build"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_CARGO_WAIT_TIMEOUT_SECS", "4")
        .env("SOLDR_STARTUP_TRACE", "1")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env_remove("ZCCACHE_DISABLE");
    let output = wait_for_timeout_test_completion(
        common::tracked_child::spawn_tracked(&mut command)
            .expect("spawn soldr cargo build with retry timeout"),
        "timed-out cargo retry attempt",
    );

    assert!(
        !output.status.success(),
        "both explicitly timed runs should fail\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr
            .matches("retrying timed-out cargo run without cache")
            .count(),
        1,
        "the no-cache retry must not recursively retry: {stderr}"
    );
    let log = fs::read_to_string(&log_path).expect("fake cargo log");
    let build_invocations = log.lines().filter(|line| *line == "cargo build").count();
    assert_eq!(
        build_invocations, 2,
        "expected the cached attempt and exactly one no-cache retry: {log}"
    );
    let abort_log = fs::read_to_string(cache_root.join("logs/cargo-aborts.jsonl"))
        .expect("both timeout attempts should be logged");
    let records: Vec<Value> = abort_log
        .lines()
        .map(|line| serde_json::from_str(line).expect("abort record JSON"))
        .collect();
    assert_eq!(records.len(), 2, "each timed-out attempt must be logged");
    assert_eq!(records[0]["auto_retry_planned"], true);
    assert_eq!(records[1]["auto_retry_planned"], false);
    assert_ne!(
        records[0]["session_id"], records[1]["session_id"],
        "retry metadata must not be attributed to the earlier invocation"
    );
}

#[test]
fn cargo_explicit_timeout_retry_can_be_disabled() {
    let root = unique_temp_dir("cargo-timeout-retry-disabled");
    let workspace = root.join("workspace");
    let tool_dir = root.join("tool");
    let cache_root = root.join("soldr-cache");
    let log_path = root.join("cargo.log");
    let fake_tool_log = root.join("fake-toolchain.log");
    fs::create_dir_all(workspace.join("src")).expect("workspace src");
    fs::create_dir_all(&tool_dir).expect("tool dir");
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"timeout_retry_disabled\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    fs::write(workspace.join("src/lib.rs"), "pub fn ok() {}\n").expect("source");
    let cargo = fake_script_path(&tool_dir, "cargo");
    let (_unused_cargo, rustc, _zccache) = install_fake_toolchain(&fake_tool_log);
    write_fake_script(&cargo, &fake_always_slow_cargo_script(&log_path));

    let mut command = common::isolated_soldr_command();
    command
        .args(["cargo", "build"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_CARGO_WAIT_TIMEOUT_SECS", "4")
        .env("SOLDR_NO_CARGO_TIMEOUT_RETRY", "1")
        .env("SOLDR_STARTUP_TRACE", "1")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env_remove("ZCCACHE_DISABLE");
    let output = wait_for_timeout_test_completion(
        common::tracked_child::spawn_tracked(&mut command)
            .expect("spawn soldr cargo build with retry disabled"),
        "retry-disabled timed-out cargo attempt",
    );

    assert!(!output.status.success(), "explicit timeout must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("timed out after 4 seconds"),
        "the unchanged four-second Cargo timeout must fire: {stderr}"
    );
    assert!(
        !stderr.contains("retrying timed-out cargo run without cache"),
        "retry opt-out must be honored: {stderr}"
    );
    let log = fs::read_to_string(&log_path).expect("fake cargo log");
    assert_eq!(
        log.lines().filter(|line| *line == "cargo build").count(),
        1,
        "retry opt-out must leave exactly one build attempt: {log}"
    );
    assert_startup_phases_in_order(
        &stderr,
        &[
            "cargo_front_door_entered",
            "cargo_front_door_toolchain_resolved",
            "cargo_front_door_pre_spawn",
            "command_dispatch",
        ],
    );
    let abort_log = fs::read_to_string(cache_root.join("logs/cargo-aborts.jsonl"))
        .expect("timeout should be logged");
    let record: Value = serde_json::from_str(abort_log.lines().next().expect("one abort record"))
        .expect("abort record JSON");
    assert_eq!(record["auto_retry_planned"], false);
}

#[test]
fn cargo_build_warns_when_disk_space_is_low() {
    let cache_root = unique_temp_dir("cargo-low-disk");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);

    let output = common::isolated_soldr_command()
        .args(["--no-cache", "cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_FREE_DISK_BYTES", "1500000000")
        .env("CARGO_TARGET_DIR", cache_root.join("target"))
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with low-disk override");

    assert!(
        output.status.success(),
        "cargo build with low-disk warning failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("disk space is low"),
        "low-disk warning missing from stderr: {stderr}"
    );
    assert!(
        stderr.contains("Run `soldr gc`"),
        "low-disk warning should recommend soldr gc: {stderr}"
    );
}

#[test]
fn cargo_build_warns_in_yellow_when_git_autocrlf_is_true() {
    let root = unique_temp_dir("cargo-crlf-warning");
    let repo = root.join("repo");
    let cache_root = root.join("soldr-cache");
    let log_path = root.join("tool.log");
    fs::create_dir_all(&repo).expect("repo directory");
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"crlf_warning\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    let git_init = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["init", "-q"])
        .status()
        .expect("git init");
    assert!(git_init.success());
    let git_config = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["config", "--local", "core.autocrlf", "true"])
        .status()
        .expect("git config");
    assert!(git_config.success());
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);

    let output = common::isolated_soldr_command()
        .args(["--no-cache", "cargo", "build"])
        .current_dir(&repo)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("SOLDR_TEST_FREE_DISK_BYTES", "100000000000")
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env("GITHUB_ACTIONS", "true")
        .env_remove("NO_COLOR")
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("soldr cargo build");

    assert!(
        output.status.success(),
        "cargo build with CRLF warning failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("\x1b[33mwarning\x1b[0m: Git CRLF checkout mode"),
        "yellow CRLF warning missing from stderr: {stderr}"
    );
    assert!(stderr.contains("core.autocrlf=true"), "{stderr}");
    assert!(stderr.contains("avoidable recompiles"), "{stderr}");
}

#[test]
fn cargo_build_ignores_disk_space_detection_failures() {
    let cache_root = unique_temp_dir("cargo-low-disk-error");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);

    let output = common::isolated_soldr_command()
        .args(["--no-cache", "cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_FREE_DISK_BYTES", "error")
        .env("CARGO_TARGET_DIR", cache_root.join("target"))
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with disk-probe error");

    assert!(
        output.status.success(),
        "disk-space detection failure must not fail build\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("disk space is low"),
        "disk-space probe failure should not emit low-disk warning: {stderr}"
    );
}

#[test]
fn cargo_subcommand_rejects_no_cache_flag() {
    let cache_root = unique_temp_dir("cargo-subcommand-no-cache");
    let output = common::isolated_soldr_command()
        .args(["cargo", "--no-cache", "--version"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr cargo --no-cache --version");

    assert!(
        !output.status.success(),
        "cargo subcommand form should no longer accept --no-cache"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--no-cache"),
        "expected cargo-subcommand form to fail mentioning --no-cache: {stderr}"
    );
}
#[test]
fn windows_worktree_copy_relocates_wrapper_and_original_dir_can_be_removed() {
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let cache_root = unique_temp_dir("windows-self-relocate-cache");
    let worktree = unique_temp_dir("windows-self-relocate-worktree");
    let source_dir = worktree.join("target").join("debug");
    fs::create_dir_all(&source_dir).expect("failed to create copied exe dir");
    let copied_soldr = source_dir.join("soldr.exe");
    fs::copy(common::soldr_bin(), &copied_soldr)
        .expect("failed to copy soldr exe into temporary worktree");

    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(&copied_soldr)
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        // Strip any relocation guard the parent process might be carrying
        // (it inherits these when the test suite itself is run via
        // `soldr cargo test`, because the outer soldr self-relocates and
        // exports SOLDR_RELOCATED_EXE / SOLDR_ORIGINAL_EXE in its env).
        // Leaving them set short-circuits relocation_guard_active() inside
        // the copied soldr, so RUSTC_WRAPPER would point at the worktree copy
        // instead of the cache-root/version/shims compiler identity asserted
        // below.
        .env_remove("SOLDR_RELOCATED_EXE")
        .env_remove("SOLDR_ORIGINAL_EXE")
        .output()
        .expect("failed to run copied soldr exe");

    assert!(
        output.status.success(),
        "copied soldr front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    let wrapper = logged_cargo_wrapper(&log).expect("fake cargo should log RUSTC_WRAPPER");
    let expected_shim_dir = cache_root
        .join(format!("v{}", env!("CARGO_PKG_VERSION")))
        .join("shims");
    assert!(
        path_display_variants(&expected_shim_dir)
            .iter()
            .any(|path| wrapper.contains(path)),
        "RUSTC_WRAPPER should point at the versioned compiler-shim directory: {log}"
    );
    assert!(
        !path_display_variants(&copied_soldr)
            .iter()
            .any(|path| wrapper.contains(path)),
        "RUSTC_WRAPPER should not point at the original worktree copy: {log}"
    );

    fs::remove_dir_all(&worktree)
        .expect("temporary worktree should be removable after soldr exits");
    assert!(!worktree.exists());
}

#[test]
fn cargo_front_door_defaults_non_msvc_dev_debug_off_and_warns_once_per_repo() {
    // soldr#3740: the host target must be non-MSVC. An explicit
    // `--target x86_64-unknown-linux-gnu` used to select it everywhere, but
    // that now routes through the managed GNU toolchain from the catalogue,
    // which this offline fixture cannot fetch -- the build failed before the
    // warning was ever checked, and the `#[ignore]` hid it.
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        eprintln!("skipping: the Windows host target is MSVC (soldr#3740)");
        return;
    }
    let cache_root = unique_temp_dir("cargo-debug-default-off");
    let repo = unique_temp_dir("cargo-debug-default-repo");
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("failed to seed Cargo.toml");
    fs::create_dir_all(repo.join("src")).expect("failed to create src dir");
    fs::write(repo.join("src").join("lib.rs"), "").expect("failed to seed lib.rs");

    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let cargo_home = cache_root.join("cargo-home");

    let first = common::isolated_soldr_command()
        .args(["cargo", "build"])
        .current_dir(&repo)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", &cargo_home)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("CARGO_PROFILE_DEV_DEBUG")
        .env_remove("CARGO_PROFILE_TEST_DEBUG")
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run first soldr cargo build");
    assert!(
        first.status.success(),
        "first build failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );

    let second = common::isolated_soldr_command()
        .args(["cargo", "build"])
        .current_dir(&repo)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", &cargo_home)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("CARGO_PROFILE_DEV_DEBUG")
        .env_remove("CARGO_PROFILE_TEST_DEBUG")
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run second soldr cargo build");
    assert!(
        second.status.success(),
        "second build failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cargo_profile_env CARGO_PROFILE_DEV_DEBUG=false"),
        "soldr should inject the dev profile debug override when unspecified: {log}"
    );

    let first_stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        first_stderr.contains("Cargo profile.dev.debug is unspecified")
            && first_stderr.contains("CARGO_PROFILE_DEV_DEBUG=false"),
        "first invocation should warn about the defaulted debug setting: {first_stderr}"
    );
    let second_stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        !second_stderr.contains("Cargo profile.dev.debug is unspecified"),
        "second invocation for the same repo should not repeat the debug-default warning: {second_stderr}"
    );
}

#[test]
fn cargo_front_door_respects_dev_debug_in_cargo_config_toml() {
    let cache_root = unique_temp_dir("cargo-debug-config-explicit");
    let repo = unique_temp_dir("cargo-debug-config-repo");
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("failed to seed Cargo.toml");
    fs::create_dir_all(repo.join(".cargo")).expect("failed to create .cargo dir");
    fs::write(
        repo.join(".cargo").join("config.toml"),
        "[profile.dev]\ndebug = true\n",
    )
    .expect("failed to seed .cargo/config.toml");

    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let output = common::isolated_soldr_command()
        .args(["cargo", "build"])
        .current_dir(&repo)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", cache_root.join("cargo-home"))
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("CARGO_PROFILE_DEV_DEBUG")
        .env_remove("CARGO_PROFILE_TEST_DEBUG")
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with explicit config debug");

    assert!(
        output.status.success(),
        "build with explicit cargo config debug failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        !log.contains("cargo_profile_env CARGO_PROFILE_DEV_DEBUG=false"),
        "explicit .cargo/config.toml profile.dev.debug must not be overridden: {log}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("Cargo profile.dev.debug is unspecified"),
        "explicit .cargo/config.toml profile.dev.debug should suppress warning: {stderr}"
    );
}
