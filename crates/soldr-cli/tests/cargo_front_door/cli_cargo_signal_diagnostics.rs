//! Real-child failure reporting when Cargo dies with an inherited stderr writer.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use crate::common::*;

use std::path::PathBuf;
use std::time::Duration;

struct ReleaseWriter(PathBuf);

impl Drop for ReleaseWriter {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, b"release");
    }
}

#[test]
fn signaled_cargo_reports_signal_even_when_descendant_holds_stderr() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let root = unique_temp_dir("cargo-signal-diagnostics");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let release = root.join("release-writer");
    let _release_on_drop = ReleaseWriter(release.clone());
    let log = root.join("fake-tool.log");
    let (cargo, rustc, _) = install_fake_toolchain(&log);
    write_fake_script(
        &cargo,
        "#!/bin/sh\ncase \"$1\" in --version) echo 'cargo 1.0.0-test'; exit 0;; metadata) echo '{}'; exit 0;; esac\n(printf ready > \"$SIGNAL_TEST_READY\"; count=0; while [ ! -f \"$SIGNAL_TEST_RELEASE\" ] && [ \"$count\" -lt 600 ]; do sleep 0.05; count=$((count + 1)); done) >/dev/null &\ncount=0; while [ ! -f \"$SIGNAL_TEST_READY\" ] && [ \"$count\" -lt 500 ]; do sleep 0.01; count=$((count + 1)); done\n[ -f \"$SIGNAL_TEST_READY\" ] || { echo 'descendant readiness timed out' >&2; exit 1; }\necho 'diagnostic-marker-3938' >&2\nkill -TERM $$\n",
    );
    let mut command = isolated_soldr_command();
    command
        .args(["--no-cache", "cargo", "build"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_CACHE_DIR", root.join("cache"))
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env("SIGNAL_TEST_READY", root.join("descendant-ready"))
        .env("SIGNAL_TEST_RELEASE", &release)
        .env("SOLDR_CARGO_WAIT_TIMEOUT_SECS", "45")
        .env("SOLDR_NO_CARGO_TIMEOUT_RETRY", "1")
        .env_remove("SOLDR_EMBED_PACKED_DWARF")
        .env_remove("SOLDR_DEBUG");
    let output = crate::common::tracked_child::spawn_tracked(&mut command)
        .expect("spawn soldr")
        .wait_bounded(Duration::from_secs(60));
    std::fs::write(&release, b"release").expect("release descendant writer");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.timed_out, "soldr timed out: {stderr}");
    assert!(
        !output.status.expect("soldr exit status").success(),
        "signaled Cargo must fail"
    );
    assert!(
        stderr.contains("diagnostic-marker-3938"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("cargo diagnostic stderr pipe did not close"),
        "descendant did not hold the diagnostic pipe through the grace period: {stderr}"
    );
    assert!(
        stderr.contains("cargo terminated by signal 15"),
        "stderr: {stderr}"
    );
}
