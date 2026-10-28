use crate::common::*;
use std::path::Path;

fn fake_global_soldr(log: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             if \"%~1\"==\"--version\" (\n\
               echo soldr 999.0.0\n\
               exit /b 0\n\
             )\n\
             echo %*>>\"{}\"\n\
             exit /b 73\n",
            log.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then\n\
               echo 'soldr 999.0.0'\n\
               exit 0\n\
             fi\n\
             printf '%s\\n' \"$*\" >> \"{}\"\n\
             exit 73\n",
            log.display()
        )
    }
}

#[test]
fn explicit_opt_out_keeps_the_source_driver_when_a_newer_global_exists() {
    let fixture = unique_temp_dir("source-driver-no-delegation");
    std::fs::write(
        fixture.join("Cargo.toml"),
        "[workspace]\n\n[workspace.metadata.soldr]\nprefer_newer_global = true\n",
    )
    .expect("write opt-in manifest");
    let global_bin_dir = fixture.join("global-bin");
    std::fs::create_dir_all(&global_bin_dir).expect("create global bin dir");
    let log = fixture.join("global-invocation.log");
    let global_soldr = fake_script_path(&global_bin_dir, "soldr");
    write_fake_script(&global_soldr, &fake_global_soldr(&log));

    let output = isolated_soldr_command()
        .env(
            soldr_cli::global_upgrade::GLOBAL_DELEGATION_DISABLE_ENV_VAR,
            "1",
        )
        .env("SOLDR_BROKER_AUTOSPAWN", "0")
        .env("SOLDR_CACHE_DIR", fixture.join("cache"))
        .env("PATH", prepend_to_path(&global_bin_dir))
        .current_dir(&fixture)
        .arg("status")
        .output()
        .expect("run source soldr");
    assert_ne!(
        output.status.code(),
        Some(73),
        "newer global driver took ownership"
    );
    assert!(!log.exists(), "global driver was invoked");
}

#[test]
fn project_policy_delegates_to_newer_global_soldr() {
    let fixture = unique_temp_dir("newer-global-soldr");
    std::fs::write(
        fixture.join("Cargo.toml"),
        "[workspace]\n\n[workspace.metadata.soldr]\nprefer_newer_global = true\n",
    )
    .expect("write opt-in manifest");

    let global_bin_dir = fixture.join("global-bin");
    std::fs::create_dir_all(&global_bin_dir).expect("create global bin dir");
    let log = fixture.join("global-invocation.log");
    let global_soldr = fake_script_path(&global_bin_dir, "soldr");
    write_fake_script(&global_soldr, &fake_global_soldr(&log));

    let output = isolated_soldr_command()
        // soldr#2785: the harness disables the delegation probe for every
        // other fixture. This file is the policy's own coverage, so it opts
        // back in -- otherwise the two negative tests below would pass
        // vacuously, asserting "no delegation" while the harness had turned
        // delegation off entirely.
        .env_remove(soldr_cli::global_upgrade::GLOBAL_DELEGATION_DISABLE_ENV_VAR)
        .arg("status")
        .current_dir(&fixture)
        .env("PATH", prepend_to_path(&global_bin_dir))
        .env("SOLDR_CACHE_DIR", fixture.join("cache"))
        .output()
        .expect("run local soldr");

    assert_eq!(
        output.status.code(),
        Some(73),
        "newer global soldr must own the invocation\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&log)
            .expect("read delegated invocation")
            .trim(),
        "status"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("delegating to newer global soldr"),
        "delegation should be visible on stderr"
    );
}

/// A soldr that hands the whole invocation to a newer global soldr must not
/// start the broker first. The broker is a stable singleton that is never
/// replaced automatically (soldr#2549), so spawning it from the *older* image
/// pins every later build on the machine to that image. Observed: `pip install .`
/// with a backend pinned to soldr 0.9.6 started a 0.9.6 broker, then delegated
/// to the global 0.9.22, and every later 0.9.22 build warned about the
/// mismatched broker. The newer soldr's own front door owns the spawn.
#[test]
fn delegating_invocation_never_spawns_its_own_broker() {
    let fixture = unique_temp_dir("delegate-no-broker-spawn");
    std::fs::write(
        fixture.join("Cargo.toml"),
        "[workspace]\n\n[workspace.metadata.soldr]\nprefer_newer_global = true\n",
    )
    .expect("write opt-in manifest");

    let global_bin_dir = fixture.join("global-bin");
    std::fs::create_dir_all(&global_bin_dir).expect("create global bin dir");
    let log = fixture.join("global-invocation.log");
    let global_soldr = fake_script_path(&global_bin_dir, "soldr");
    write_fake_script(&global_soldr, &fake_global_soldr(&log));

    let home = fixture.join("isolated-home");
    std::fs::create_dir_all(&home).expect("create isolated home");

    let output = isolated_soldr_command()
        // soldr#2785: opt back in to the delegation probe the harness disables.
        .env_remove(soldr_cli::global_upgrade::GLOBAL_DELEGATION_DISABLE_ENV_VAR)
        .arg("status")
        .current_dir(&fixture)
        .env("PATH", prepend_to_path(&global_bin_dir))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("SOLDR_CACHE_DIR", fixture.join("cache"))
        .output()
        .expect("run local soldr");

    assert_eq!(
        output.status.code(),
        Some(73),
        "newer global soldr must own the invocation\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let staged_broker_dir = home.join(".soldr").join("broker");
    let staged: Vec<_> = std::fs::read_dir(&staged_broker_dir)
        .map(|entries| entries.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(
        staged.is_empty(),
        "the delegating (older) soldr must not stage or spawn a broker before \
         handing off; found in {}: {staged:?}",
        staged_broker_dir.display()
    );
}

/// #2521 D root cause: in a `prefer_newer_global` checkout the delegation
/// probe ran `<global soldr> --version` as a child of EVERY invocation —
/// including `soldr broker status` — and a released soldr's front door stages
/// a broker under the inherited HOME and spawns `broker serve` before printing
/// its version. The broker-absent target-run tests then found "a broker"
/// running in their isolated homes: the one their own probe child had just
/// created. Broker-family commands are now delegation-exempt, so the probe
/// never fires and the isolated home stays broker-free.
#[test]
fn broker_status_in_opted_in_checkout_neither_probes_nor_spawns() {
    let fixture = unique_temp_dir("broker-status-no-delegate");
    std::fs::write(
        fixture.join("Cargo.toml"),
        "[workspace]\n\n[workspace.metadata.soldr]\nprefer_newer_global = true\n",
    )
    .expect("write opt-in manifest");

    let global_bin_dir = fixture.join("global-bin");
    std::fs::create_dir_all(&global_bin_dir).expect("create global bin dir");
    let log = fixture.join("global-invocation.log");
    let global_soldr = fake_script_path(&global_bin_dir, "soldr");
    write_fake_script(&global_soldr, &fake_global_soldr(&log));

    let home = fixture.join("isolated-home");
    std::fs::create_dir_all(&home).expect("create isolated home");

    let output = isolated_soldr_command()
        // soldr#2785: the harness disables the delegation probe for every
        // other fixture. This file is the policy's own coverage, so it opts
        // back in -- otherwise the two negative tests below would pass
        // vacuously, asserting "no delegation" while the harness had turned
        // delegation off entirely.
        .env_remove(soldr_cli::global_upgrade::GLOBAL_DELEGATION_DISABLE_ENV_VAR)
        .args(["broker", "status"])
        .current_dir(&fixture)
        .env("PATH", prepend_to_path(&global_bin_dir))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .output()
        .expect("run soldr broker status");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "broker status must stay a successful local probe:\n{combined}"
    );
    assert!(
        combined.contains("not running"),
        "an isolated home has no broker and none may be manufactured:\n{combined}"
    );
    assert!(
        !log.exists(),
        "broker status must not probe the global soldr; probe log:\n{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    let staged_broker_dir = home.join(".soldr").join("broker");
    let staged: Vec<_> = std::fs::read_dir(&staged_broker_dir)
        .map(|entries| entries.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(
        staged.is_empty(),
        "no broker image or logs may be staged into the isolated home by a \
         read-only status probe; found: {staged:?}"
    );
}

// The delegation policy must NOT apply to `RUSTC_WRAPPER` callbacks
// (#1847). Cargo issues one per compile unit, and `probe_version`
// spawns the global soldr to read its `--version` — 52-61 ms measured,
// which dominated the wrapper hot path. Delegating here would also swap
// the compiler wrapper (and its daemon/cache peer) mid-build.
//
// Companion to `project_policy_delegates_to_newer_global_soldr` above:
// that asserts a user-facing invocation still hands off, this asserts a
// wrapper invocation never does. Same opt-in manifest, same newer fake
// global on PATH — only the invocation shape differs.
#[test]
fn wrapper_invocations_never_delegate_to_newer_global_soldr() {
    let fixture = unique_temp_dir("wrapper-no-global-delegate");
    std::fs::write(
        fixture.join("Cargo.toml"),
        "[workspace]\n\n[workspace.metadata.soldr]\nprefer_newer_global = true\n",
    )
    .expect("write opt-in manifest");

    let global_bin_dir = fixture.join("global-bin");
    std::fs::create_dir_all(&global_bin_dir).expect("create global bin dir");
    let log = fixture.join("global-invocation.log");
    let global_soldr = fake_script_path(&global_bin_dir, "soldr");
    write_fake_script(&global_soldr, &fake_global_soldr(&log));

    // A wrapper invocation is `soldr <rustc-like-path> <args>`; the stem
    // is what `is_wrapper_invocation` matches on. `--print sysroot` is a
    // non-cacheable probe, so it passes straight through to this stub
    // without involving the daemon.
    let fake_rustc = fake_script_path(&fixture, "rustc");
    let body = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        "@echo off\necho fake-sysroot\nexit /b 0\n"
    } else {
        "#!/bin/sh\necho fake-sysroot\nexit 0\n"
    };
    write_fake_script(&fake_rustc, body);

    let output = isolated_soldr_command()
        // soldr#2785: the harness disables the delegation probe for every
        // other fixture. This file is the policy's own coverage, so it opts
        // back in -- otherwise the two negative tests below would pass
        // vacuously, asserting "no delegation" while the harness had turned
        // delegation off entirely.
        .env_remove(soldr_cli::global_upgrade::GLOBAL_DELEGATION_DISABLE_ENV_VAR)
        .arg(&fake_rustc)
        .arg("--print")
        .arg("sysroot")
        .current_dir(&fixture)
        .env("PATH", prepend_to_path(&global_bin_dir))
        .env("SOLDR_CACHE_DIR", fixture.join("cache"))
        .output()
        .expect("run local soldr in wrapper mode");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_ne!(
        output.status.code(),
        Some(73),
        "wrapper invocation must not hand off to the global soldr\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !log.exists(),
        "global soldr must not be invoked from a wrapper callback; log:\n{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    assert!(
        !stderr.contains("delegating to newer global soldr"),
        "wrapper invocation should not announce delegation\nstderr:\n{stderr}"
    );
}
