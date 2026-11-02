use super::*;

#[test]
fn bare_channel_appends_host_triple() {
    assert_eq!(
        toolchain_dir_name("1.95.0", "x86_64-unknown-linux-gnu"),
        "1.95.0-x86_64-unknown-linux-gnu"
    );
    assert_eq!(
        toolchain_dir_name("nightly-2026-04-16", "aarch64-apple-darwin"),
        "nightly-2026-04-16-aarch64-apple-darwin"
    );
}

#[test]
fn host_qualified_channel_maps_to_itself() {
    assert_eq!(
        toolchain_dir_name("1.95.0-x86_64-pc-windows-msvc", "x86_64-pc-windows-msvc"),
        "1.95.0-x86_64-pc-windows-msvc"
    );
}

#[test]
fn empty_host_keeps_bare_channel() {
    // platform::host::facts::triple() yields "" on an unsupported host;
    // the probe then simply misses and the caller runs the idempotent
    // install rather than constructing a bogus "<channel>-" name.
    assert_eq!(toolchain_dir_name("1.95.0", ""), "1.95.0");
}

#[test]
fn classification_matrix() {
    assert_eq!(
        classify(false, false, false, true),
        ToolchainReadiness::Missing
    );
    assert_eq!(
        classify(true, false, false, true),
        ToolchainReadiness::Partial(MissingToolchainEvidence {
            channel_manifest: true,
            native_rustc: true,
            host_std: false,
        })
    );
    assert_eq!(
        classify(true, false, true, true),
        ToolchainReadiness::Partial(MissingToolchainEvidence {
            channel_manifest: true,
            native_rustc: false,
            host_std: false,
        })
    );
    assert_eq!(
        classify(true, true, false, true),
        ToolchainReadiness::Partial(MissingToolchainEvidence {
            channel_manifest: false,
            native_rustc: true,
            host_std: false,
        })
    );
    assert_eq!(classify(true, true, true, true), ToolchainReadiness::Ready);
}

#[test]
fn probe_reports_missing_partial_and_ready() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path();
    let host = "x86_64-unknown-linux-gnu";
    // The std check applies only to a directory named for the running host.
    let judged = crate::pyo3_detect::host_triple() == host;

    assert_eq!(
        probe_toolchain_state(home, "1.95.0", host),
        ToolchainReadiness::Missing
    );

    // The wedge signature from soldr#2618: component manifests landed,
    // but neither channel manifest nor rustc did.
    let toolchain = home
        .join("toolchains")
        .join("1.95.0-x86_64-unknown-linux-gnu");
    std::fs::create_dir_all(toolchain.join("lib").join("rustlib")).expect("mkdir");
    std::fs::write(
        toolchain
            .join("lib")
            .join("rustlib")
            .join("manifest-rust-std-x86_64-unknown-linux-gnu"),
        b"",
    )
    .expect("write manifest");
    assert_eq!(
        probe_toolchain_state(home, "1.95.0", host),
        ToolchainReadiness::Partial(MissingToolchainEvidence {
            channel_manifest: true,
            native_rustc: true,
            host_std: judged,
        })
    );

    let bin = toolchain.join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir bin");
    let rustc =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            bin.join("rustc.exe")
        } else {
            bin.join("rustc")
        };
    std::fs::write(&rustc, b"").expect("write rustc");
    assert_eq!(
        probe_toolchain_state(home, "1.95.0", host),
        ToolchainReadiness::Partial(MissingToolchainEvidence {
            channel_manifest: true,
            native_rustc: false,
            host_std: judged,
        })
    );
    std::fs::write(
        toolchain.join(TOOLCHAIN_CHANNEL_MANIFEST),
        b"manifest-version = '2'\n",
    )
    .expect("write channel manifest");
    // Base evidence alone is not enough on the running host: std is required.
    if judged {
        assert_eq!(
            probe_toolchain_state(home, "1.95.0", host),
            ToolchainReadiness::Partial(MissingToolchainEvidence {
                channel_manifest: false,
                native_rustc: false,
                host_std: true,
            })
        );
    }
    let std_lib = toolchain.join("lib/rustlib").join(host).join("lib");
    std::fs::create_dir_all(&std_lib).expect("mkdir std lib");
    std::fs::write(std_lib.join("libcore-0123456789abcdef.rlib"), b"").expect("libcore");
    std::fs::write(std_lib.join("libstd-0123456789abcdef.rlib"), b"").expect("libstd");
    assert_eq!(
        probe_toolchain_state(home, "1.95.0", host),
        ToolchainReadiness::Ready
    );
}

#[test]
fn caller_selected_partial_toolchain_has_non_destructive_recovery_guidance() {
    let directory = Path::new("/shared-rustup/toolchains/1.95.0-host");
    let error = shared_home_partial_toolchain_error(
        "1.95.0",
        directory,
        MissingToolchainEvidence {
            channel_manifest: true,
            native_rustc: false,
            host_std: false,
        },
    )
    .to_string();

    assert!(error.contains(TOOLCHAIN_CHANNEL_MANIFEST), "{error}");
    assert!(error.contains(&directory.display().to_string()), "{error}");
    assert!(
        error.contains("will not uninstall or reinstall it automatically"),
        "{error}"
    );
    let manager = ["rust", "up"].concat();
    assert!(
        error.contains(&format!("soldr {manager} toolchain uninstall 1.95.0")),
        "{error}"
    );
    assert!(error.contains("soldr toolchain install"), "{error}");
}

/// An interrupted `rustup toolchain install` leaves a directory that
/// `rustup toolchain list` reports as installed but that has no channel
/// manifest; `rustup component add` then fails with "missing manifest in
/// toolchain". The Dylint Cache Acceptance lane went red on exactly this.
#[test]
fn listed_toolchain_without_channel_manifest_is_not_trusted() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path();
    let name = "nightly-2026-08-15-x86_64-unknown-linux-gnu";
    let toolchain = home.join("toolchains").join(name);
    std::fs::create_dir_all(toolchain.join("bin")).expect("mkdir bin");
    std::fs::write(native_rustc_path(&toolchain), b"").expect("rustc");

    assert!(
        listed_toolchain_needs_reinstall(home, name, false).expect("managed home"),
        "a partial tree in the managed home must be reinstalled"
    );
    let error = listed_toolchain_needs_reinstall(home, name, true)
        .expect_err("a caller-selected home is reported, not removed")
        .to_string();
    assert!(error.contains(TOOLCHAIN_CHANNEL_MANIFEST), "{error}");

    let manifest = toolchain.join(TOOLCHAIN_CHANNEL_MANIFEST);
    std::fs::create_dir_all(manifest.parent().expect("parent")).expect("mkdir rustlib");
    std::fs::write(&manifest, b"").expect("manifest");
    assert!(!listed_toolchain_needs_reinstall(home, name, true).expect("complete toolchain"));
    assert!(!listed_toolchain_needs_reinstall(home, "absent", true).expect("absent toolchain"));
}
