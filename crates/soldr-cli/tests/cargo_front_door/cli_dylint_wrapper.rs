use crate::common;

use crate::common::{fake_script_path, isolated_soldr_command, prepend_to_path, unique_temp_dir};
use std::fs;
use std::path::{Path, PathBuf};

fn write_script(path: &Path, body: String) {
    fs::write(path, body).expect("write fake tool");
    soldr_platform::fs::permissions::make_executable(path).expect("chmod fake tool");
}

fn install_dylint_toolchain(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let tools = root.join("tools");
    fs::create_dir_all(&tools).expect("create fake tool dir");
    let log = root.join("tool.log");
    let cargo = fake_script_path(&tools, "cargo");
    let rustc = fake_script_path(&tools, "rustc");
    let zccache = fake_script_path(&tools, "zccache");
    let cargo_dylint = fake_script_path(&tools, "cargo-dylint");
    let dylint_link = fake_script_path(&tools, "dylint-link");
    let dylint_driver = fake_script_path(&tools, "dylint-driver");

    write_script(
        &cargo,
        format!(
            r#"#!/bin/sh
echo "cargo argv=$* wrapper=${{RUSTC_WRAPPER:-}}" >> "{log}"
if [ "$1" = "metadata" ]; then
  printf '{{"packages":[],"workspace_members":[],"workspace_default_members":[],"resolve":null,"target_directory":"target","version":1,"workspace_root":"."}}\n'
  exit 0
fi
exec "{cargo_dylint}" "$@"
"#,
            log = log.display(),
            cargo_dylint = cargo_dylint.display(),
        ),
    );
    write_script(
        &cargo_dylint,
        format!(
            r#"#!/bin/sh
echo "cargo-dylint argv=$* wrapper=${{RUSTC_WRAPPER:-}}" >> "{log}"
case "${{1:-}}" in
  --version) printf 'cargo-dylint 6.2.0\n'; exit 0 ;;
  --help) exit 0 ;;
esac
echo "cargo-dylint pair wrapper=${{RUSTC_WRAPPER:-<unset>}} mirror=${{SOLDR_EFFECTIVE_RUSTC_WRAPPER:-<unset>}}" >> "{log}"
case "${{RUSTC_WRAPPER:-}}" in
  /*/soldr-dylint) ;;
  *) echo "RUSTC_WRAPPER is not an absolute soldr-dylint path: ${{RUSTC_WRAPPER:-}}" >&2; exit 91 ;;
esac
if [ "${{DYLINT_TEST_FAIL:-0}}" != "1" ]; then
  "$RUSTC_WRAPPER" "$RUSTC" --crate-name dylint_direct --emit link direct.rs || exit $?
fi
RUSTC_WORKSPACE_WRAPPER="{driver}" \
  "$RUSTC_WRAPPER" "{driver}" "$RUSTC" --crate-name dylint_nested --emit link nested.rs
"#,
            log = log.display(),
            driver = dylint_driver.display(),
        ),
    );
    write_script(
        &zccache,
        format!(
            r#"#!/bin/sh
echo "zccache compiler=$1 argv=$*" >> "{log}"
compiler="$1"
shift
exec "$compiler" "$@"
"#,
            log = log.display(),
        ),
    );
    write_script(&dylint_link, "#!/bin/sh\nexit 0\n".to_string());
    write_script(
        &dylint_driver,
        format!(
            r#"#!/bin/sh
echo "dylint-driver argv=$*" >> "{log}"
compiler="$1"
shift
exec "$compiler" "$@"
"#,
            log = log.display(),
        ),
    );
    write_script(
        &rustc,
        format!(
            r#"#!/bin/sh
echo "rustc argv=$*" >> "{log}"
if [ "${{1:-}}" = "-vV" ]; then
  printf 'rustc 1.89.0-nightly\nrelease: 1.89.0-nightly\ncommit-hash: 0123456789abcdef0123456789abcdef01234567\nhost: x86_64-unknown-linux-gnu\n'
  exit 0
fi
if [ "${{DYLINT_TEST_FAIL:-0}}" = "1" ]; then
  echo "dylint nested diagnostic on stdout"
  echo "dylint nested diagnostic on stderr" >&2
  exit 7
fi
echo "dylint compile diagnostic on stdout"
echo "dylint compile diagnostic on stderr" >&2
output_path=
crate_name=rust_out
emit_link=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) shift; output_path=$1 ;;
    --crate-name) shift; crate_name=$1 ;;
    --emit) shift; case "$1" in *link*) emit_link=1 ;; esac ;;
  esac
  shift
done
if [ -z "$output_path" ] && [ "$emit_link" = "1" ]; then
  output_path=$crate_name
fi
if [ -n "$output_path" ]; then
  mkdir -p "$(dirname "$output_path")"
  : > "$output_path"
fi
"#,
            log = log.display(),
        ),
    );

    let driver_root = root.join("drivers");
    let driver_channel = format!(
        "nightly-2026-05-26-{}",
        soldr_cli::pyo3_detect::host_triple()
    );
    let prebuilt_driver = driver_root.join(driver_channel).join("dylint-driver");
    fs::create_dir_all(prebuilt_driver.parent().expect("driver parent"))
        .expect("create prebuilt driver dir");
    write_script(
        &prebuilt_driver,
        "#!/bin/sh\nprintf 'dylint-driver 6.2.0\\n'\n".to_string(),
    );

    (cargo, rustc, zccache, driver_root)
}

fn dylint_command(root: &Path) -> std::process::Command {
    let (cargo, rustc, _zccache, driver_root) = install_dylint_toolchain(root);
    let channel = format!(
        "nightly-2026-05-26-{}",
        soldr_cli::pyo3_detect::host_triple()
    );
    let identity = format!("{channel}|1.89.0-nightly|0123456789abcdef0123456789abcdef01234567");
    let mut command = isolated_soldr_command();
    command
        .current_dir(root)
        .env("PATH", prepend_to_path(&root.join("tools")))
        .env("SOLDR_HOME", root.join("soldr-home"))
        .env("SOLDR_CACHE_DIR", root.join("cache"))
        .env("SOLDR_TEST_CARGO_BIN", cargo)
        .env("SOLDR_TEST_RUSTC_BIN", rustc)
        .env("DYLINT_DRIVER_PATH", driver_root)
        .env("SOLDR_DYLINT_CONFIGURED_TOOLCHAIN", channel)
        .env("SOLDR_DYLINT_CONFIGURED_RUSTC_RELEASE", "1.89.0-nightly")
        .env(
            "SOLDR_DYLINT_CONFIGURED_RUSTC_COMMIT_HASH",
            "0123456789abcdef0123456789abcdef01234567",
        )
        .env("SOLDR_DYLINT_PREPARED_IDENTITY", identity)
        // soldr#2436 phase 1 (D9): every dylint containment test carries
        // the tripwire, so any surviving implicit source-build path fails
        // with a distinctive message instead of silently compiling.
        .env("SOLDR_TEST_FORBID_SOURCE_BUILD", "1");
    command
}

fn install_conflicting_rustup(root: &Path) -> PathBuf {
    let tools = root.join("tools");
    fs::create_dir_all(&tools).expect("create fake tool dir");
    let rustup = fake_script_path(&tools, "rustup");
    let rustc = fake_script_path(&tools, "rustc");
    let log = root.join("tool.log");
    write_script(
        &rustup,
        format!(
            r#"#!/bin/sh
echo "rustup argv=$*" >> "{log}"
case "${{1:-}}" in
  which) printf '%s\n' "{rustc}"; exit 0 ;;
  toolchain)
    case "${{2:-}}" in
      install)
        channel="${{3:-}}"
        if [ "${{4:-}}" != "--profile" ] || [ "${{5:-}}" != "minimal" ] || [ "${{6:-}}" != "--no-self-update" ]; then
          echo "unexpected fake rustup install invocation: $*" >&2
          exit 87
        fi
        toolchain_dir="$RUSTUP_HOME/toolchains/$channel"
        mkdir -p "$toolchain_dir/bin" "$toolchain_dir/lib/rustlib"
        : > "$toolchain_dir/bin/rustc"
        : > "$toolchain_dir/lib/rustlib/multirust-channel-manifest.toml"
        mkdir -p "$toolchain_dir/lib/rustlib/{host}/lib"
        : > "$toolchain_dir/lib/rustlib/{host}/lib/libcore-0123456789abcdef.rlib"
        : > "$toolchain_dir/lib/rustlib/{host}/lib/libstd-0123456789abcdef.rlib"
        exit 0
        ;;
    esac
    ;;
  component)
    case "${{2:-}}" in
      list) printf '%s\n' 'rustc-dev-x86_64-unknown-linux-gnu'; exit 0 ;;
      add)
        echo "error: failed to install component: 'rust-src', detected conflict: 'library/.cargo/config.toml'" >&2
        exit 101
        ;;
    esac
    ;;
esac
echo "unexpected fake rustup invocation: $*" >&2
exit 87
"#,
            log = log.display(),
            rustc = rustc.display(),
            host = soldr_cli::pyo3_detect::host_triple(),
        ),
    );
    rustup
}

#[test]
fn dylint_front_door_preserves_direct_and_nested_compiler_chains() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let root = unique_temp_dir("dylint-wrapper-success");
    let _broker = crate::common::BrokerHomeGuard::new(&root.join("cache"), &root.join("home"));
    let output = dylint_command(&root)
        .args(["cargo", "dylint", "--all"])
        .output()
        .expect("run soldr cargo dylint");

    assert!(
        output.status.success(),
        "soldr cargo dylint failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fs::read_to_string(root.join("tool.log")).expect("read fake tool log");
    assert!(
        log.contains("cargo-dylint argv=dylint --all"),
        "logical cargo-dylint argv was not preserved: {log}"
    );
    assert!(
        log.lines().any(|line| line.contains("cargo-dylint")
            && line.contains("wrapper=/")
            && line.contains("/soldr-dylint")),
        "cargo-dylint did not receive an absolute dedicated wrapper: {log}"
    );
    assert!(
        log.contains("dylint-driver argv=") && log.contains("dylint_nested"),
        "Dylint driver was not executed with the nested rustc invocation: {log}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("dylint compile diagnostic on stdout")
            && String::from_utf8_lossy(&output.stderr)
                .contains("dylint compile diagnostic on stderr"),
        "successful compiler diagnostics were not replayed"
    );
}

/// soldr#2634: the dylint branch re-points `RUSTC_WRAPPER` at the dedicated
/// `soldr-dylint` shim AFTER the cache plan already stamped the rustc shim
/// into the soldr#2545 effective-wrapper mirror. If only `RUSTC_WRAPPER`
/// moves, every nested front-door re-entry under cargo-dylint (its `cargo
/// metadata` probe first) fails the drift guard — and cargo-dylint swallows
/// that as "No libraries were found", exiting 0 having linted nothing.
#[test]
fn dylint_wrapper_shim_keeps_the_effective_mirror_paired() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let root = unique_temp_dir("dylint-wrapper-mirror");
    let _broker = crate::common::BrokerHomeGuard::new(&root.join("cache"), &root.join("home"));
    let output = dylint_command(&root)
        .args(["cargo", "dylint", "--all"])
        .output()
        .expect("run soldr cargo dylint");

    assert!(
        output.status.success(),
        "soldr cargo dylint failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fs::read_to_string(root.join("tool.log")).expect("read fake tool log");
    let pairs: Vec<&str> = log
        .lines()
        .filter(|line| line.starts_with("cargo-dylint pair "))
        .collect();
    assert!(
        !pairs.is_empty(),
        "no wrapper/mirror pair line in fake tool log: {log}"
    );
    for pair in pairs {
        let field = |name: &str| {
            pair.split_whitespace()
                .find_map(|token| token.strip_prefix(name))
                .unwrap_or_else(|| panic!("no `{name}` field in pair line: {pair}"))
        };
        let wrapper = field("wrapper=");
        let mirror = field("mirror=");
        assert!(
            wrapper.ends_with("soldr-dylint"),
            "cargo-dylint did not receive the dedicated wrapper shim: {pair}"
        );
        assert_eq!(
            wrapper, mirror,
            "the soldr#2545 effective-wrapper mirror must move together with \
             RUSTC_WRAPPER when the dylint shim is applied: {pair}"
        );
    }
}

#[test]
fn dylint_front_door_preserves_failing_nested_diagnostics_and_exit() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let root = unique_temp_dir("dylint-wrapper-failure");
    let _broker = crate::common::BrokerHomeGuard::new(&root.join("cache"), &root.join("home"));
    let output = dylint_command(&root)
        .args(["dylint", "--all"])
        .env("DYLINT_TEST_FAIL", "1")
        .output()
        .expect("run failing soldr dylint");

    assert_eq!(output.status.code(), Some(7));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("dylint nested diagnostic on stdout"),
        "failing stdout diagnostic was not replayed"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("dylint nested diagnostic on stderr"),
        "failing stderr diagnostic was not replayed"
    );
    let log = fs::read_to_string(root.join("tool.log")).expect("read fake tool log");
    assert!(
        log.contains("cargo-dylint argv=dylint --all")
            && log.contains("dylint-driver argv=")
            && log.contains("dylint_nested"),
        "top-level soldr dylint did not preserve the nested failure chain: {log}"
    );
}

/// soldr#2947: rustup itself can fail a required Dylint component add with a
/// conflict. Its live stderr is the evidence for the narrowly conditional
/// reset instruction; Soldr must not auto-uninstall a user toolchain or append
/// the generic "Soldr fault" annotation.
#[test]
fn dylint_component_conflict_preserves_rustup_stderr_and_names_reset() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let root = unique_temp_dir("dylint-component-conflict");
    let _broker = crate::common::BrokerHomeGuard::new(&root.join("cache"), &root.join("home"));
    let channel = format!(
        "nightly-2026-05-26-{}",
        soldr_cli::pyo3_detect::host_triple()
    );
    let rustup = install_conflicting_rustup(&root);
    let rustup_home = root.join("rustup-home");
    let output = dylint_command(&root)
        .env_remove("SOLDR_DYLINT_PREPARED_IDENTITY")
        .env("SOLDR_TEST_RUSTUP_BIN", rustup)
        .env("RUSTUP_HOME", &rustup_home)
        .args(["dylint", "--all"])
        .output()
        .expect("run soldr dylint with a conflicting rustup component");

    // The command surface retains its established error exit, while the
    // rustup child status is preserved in the Dylint diagnostic below.
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("detected conflict"),
        "live rustup stderr was not relayed: {stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "rustup failed to add rust-src to {channel} (exit 101)"
        )),
        "rustup's exit status was not preserved: {stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "If rustup output above says `detected conflict`, run `soldr rustup toolchain uninstall {channel}` then rerun `soldr dylint`."
        )),
        "missing exact conflict remedy: {stderr}"
    );
    assert!(
        !stderr.contains("this is a fault in soldr itself"),
        "a child-explained rustup failure must not be annotated as a Soldr fault: {stderr}"
    );
    let log = fs::read_to_string(root.join("tool.log")).expect("read fake tool log");
    assert!(
        log.contains(&format!(
            "rustup argv=toolchain install {channel} --profile minimal --no-self-update"
        )),
        "the cold Dylint path must install the qualified nightly before adding components: {log}"
    );
    assert!(
        log.contains("rustup argv=component add --toolchain") && log.contains(" rust-src"),
        "the required rust-src add did not reach fake rustup: {log}"
    );
    assert!(
        !log.contains("toolchain uninstall"),
        "Soldr must only instruct the operator; it must not uninstall automatically: {log}"
    );
}

#[test]
fn missing_prebuilt_driver_fails_before_cargo_dylint_launch() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let root = unique_temp_dir("dylint-missing-prebuilt");
    let _broker = crate::common::BrokerHomeGuard::new(&root.join("cache"), &root.join("home"));
    let mut command = dylint_command(&root);
    fs::remove_dir_all(root.join("drivers")).expect("remove prebuilt driver fixture");
    // soldr#3163: with the prebuilt driver gone, soldr asks the toolchain
    // catalogue for one before refusing. Left enabled, this test downloaded
    // the real catalogue, and ran out the full 45 s retry budget against an
    // unreachable one. The behaviour under test is the refusal when no driver
    // is available, so keep the catalogue out of it.
    command.env("SOLDR_MANIFEST_DISABLE", "1");
    // soldr#3163: the same path also runs `rustup component add rustc-dev` and
    // `llvm-tools-preview` for the fixture's nightly. The fixture fakes cargo,
    // rustc and the driver but not rustup, so on any host without that exact
    // nightly (every CI runner) the real rustup installed it: ~1.6 GB, 28-39 s
    // on every gate, 58 s locally with only that toolchain removed. A fake
    // rustup keeps the test on the refusal it asserts.
    let fake_rustup = root.join("tools").join("rustup");
    write_script(
        &fake_rustup,
        format!(
            "#!/bin/sh\necho \"rustup argv=$*\" >> \"{}\"\nexit 0\n",
            root.join("tool.log").display()
        ),
    );
    command.env("SOLDR_TEST_RUSTUP_BIN", &fake_rustup);

    let output = command
        .args(["dylint", "--all"])
        .output()
        .expect("run soldr dylint without a prebuilt driver");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    // soldr#2945: the message no longer blames the host or the Dylint
    // version. It could not: 6.0.3 publishes a driver for all eight supported
    // triples, and the real cause was soldr asking for a *nightly* nobody has
    // ever built one for. The driver identity is what is missing, so that is
    // what the assertion pins.
    assert!(
        stderr.contains("no usable Dylint driver for"),
        "unexpected stderr: {stderr}"
    );
    assert!(
        stderr.contains("Corrective action:"),
        "unexpected stderr: {stderr}"
    );
    let log = fs::read_to_string(root.join("tool.log")).unwrap_or_default();
    assert!(
        !log.contains("cargo-dylint argv=dylint"),
        "cargo-dylint lint execution must not launch when its driver is absent: {log}"
    );
}

/// soldr#2436 phase 1 (A3): a driver whose version probe hangs must be
/// killed AND reaped within the probe's 2-second deadline — bounded
/// failure, no orphaned child, no compiler process spawned.
#[test]
fn hanging_driver_probe_is_killed_and_reaped() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let root = unique_temp_dir("dylint-hang-probe");
    let _broker = crate::common::BrokerHomeGuard::new(&root.join("cache"), &root.join("home"));
    let mut command = dylint_command(&root);
    let channel = format!(
        "nightly-2026-05-26-{}",
        soldr_cli::pyo3_detect::host_triple()
    );
    let hung_driver = root.join("drivers").join(channel).join("dylint-driver");
    write_script(&hung_driver, "#!/bin/sh\nsleep 60\n".to_string());

    let started = std::time::Instant::now();
    let output = command
        .args(["dylint", "--all"])
        .output()
        .expect("run soldr dylint with a hanging driver probe");
    let elapsed = started.elapsed();

    assert_eq!(output.status.code(), Some(1));
    // Probe deadline is 2s; 30s leaves a 15x scheduler margin while
    // still proving the failure is bounded, not a hang.
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "hanging probe must fail bounded, took {elapsed:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("version probe exceeded the 2-second deadline"),
        "unexpected stderr: {stderr}"
    );
    assert!(
        !stderr.contains("test tripwire"),
        "the source-build chokepoint must not be reached: {stderr}"
    );

    // Reap check (Linux): no process may survive whose cmdline names the
    // hung driver script. /proc scan, runtime-branched — no cfg.
    let needle = hung_driver.display().to_string();
    let survivors: Vec<String> = std::fs::read_dir("/proc")
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let cmdline = entry.path().join("cmdline");
                    std::fs::read(cmdline).ok().and_then(|bytes| {
                        let text = String::from_utf8_lossy(&bytes).replace('\0', " ");
                        text.contains(&needle).then_some(text)
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(
        survivors.is_empty(),
        "hung driver child was not reaped: {survivors:?}"
    );
}

/// soldr#2436 phase 1 (A6): an inherited Dylint scope is trusted — a
/// recursive invocation whose parent already established the scope must
/// NOT re-run the driver preflight (its parent owns that verdict), while
/// the entrypoint case without a driver stays a hard failure (covered by
/// missing_prebuilt_driver_fails_before_cargo_dylint_launch above).
#[test]
fn inherited_dylint_scope_skips_the_driver_preflight() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let root = unique_temp_dir("dylint-inherited-scope");
    let _broker = crate::common::BrokerHomeGuard::new(&root.join("cache"), &root.join("home"));
    let mut command = dylint_command(&root);
    fs::remove_dir_all(root.join("drivers")).expect("remove prebuilt driver fixture");
    let channel = format!(
        "nightly-2026-05-26-{}",
        soldr_cli::pyo3_detect::host_triple()
    );

    let output = command
        .env("SOLDR_DYLINT_TOOLCHAIN", channel)
        .args(["cargo", "dylint", "--all"])
        .output()
        .expect("run recursive soldr cargo dylint with inherited scope");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("is not built for this machine"),
        "inherited scope must not re-run the driver preflight: {stderr}"
    );
    assert!(
        !stderr.contains("test tripwire"),
        "inherited scope must not reach a source-build chokepoint: {stderr}"
    );
    assert!(
        output.status.success(),
        "inherited-scope invocation failed\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
}

/// soldr#2436 phase 6 (fact 6): one successful fake-tool dylint run must
/// leave the prepared marker behind, so the second run takes the warm path
/// (cached identity, no catalogue fetch).
#[test]
fn dylint_run_writes_the_prepared_marker_for_the_warm_path() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let root = unique_temp_dir("dylint-wrapper-marker");
    let _broker = crate::common::BrokerHomeGuard::new(&root.join("cache"), &root.join("home"));
    let output = dylint_command(&root)
        .args(["cargo", "dylint", "--all"])
        .output()
        .expect("run soldr cargo dylint");
    assert!(
        output.status.success(),
        "soldr cargo dylint failed\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // SoldrPaths roots at SOLDR_CACHE_DIR (= <root>/cache in this harness);
    // prepare_resolved derives the marker name from the configured release
    // (1.89.0-nightly -> "1.89").
    let marker = root
        .join("cache")
        .join("dylint")
        .join("prepared")
        .join("v1")
        .join("1.89.identity");
    assert!(
        marker.is_file(),
        "the prepared marker must exist after a successful dylint run \
         (looked at {}); without it every run repeats the cold catalogue \
         fetch + verification",
        marker.display()
    );
}

/// soldr#2634 finding 3: a shim whose dispatch fails prints its own
/// diagnostic and exits non-zero. The soldr#2024 exit annotation must NOT
/// follow it — "soldr emitted no diagnostic" directly under a printed
/// diagnostic turns a clear error into a self-contradictory one.
#[test]
fn shim_dispatch_failure_is_not_annotated_as_silent() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let root = unique_temp_dir("dylint-shim-spoke");
    let _broker = crate::common::BrokerHomeGuard::new(&root.join("cache"), &root.join("home"));
    let shim = root.join("soldr-dylint");
    std::fs::hard_link(common::soldr_bin(), &shim)
        .or_else(|_| std::fs::copy(common::soldr_bin(), &shim).map(|_| ()))
        .expect("materialize soldr-dylint shim name");
    soldr_platform::fs::permissions::make_executable(&shim).expect("chmod shim");

    let mut command = std::process::Command::new(&shim);
    common::scrub_outer_soldr_env(&mut command);
    // No broker, no session env, no daemon: dispatch must fail.
    command
        .arg("/no/such/rustc")
        .arg("--crate-name")
        .arg("probe")
        .env("SOLDR_CACHE_DIR", root.join("cache"))
        .env("HOME", root.join("home"))
        .env("USERPROFILE", root.join("home"));
    let output = command.output().expect("run soldr-dylint shim");

    assert!(
        !output.status.success(),
        "dispatch against a nonexistent rustc with no route must fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("wrapper dispatch failed"),
        "the shim must explain its failure: {stderr}"
    );
    assert!(
        !stderr.contains("soldr emitted no diagnostic"),
        "the soldr#2024 silent-exit annotation must not contradict the \
         diagnostic printed right above it: {stderr}"
    );
}
