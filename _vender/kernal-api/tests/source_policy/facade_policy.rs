//! Source-level release guards for the public facade and protocol boundary.

use std::path::{Path, PathBuf};

#[test]
fn http_parser_version_is_pinned_for_published_consumers() {
    let manifest = include_str!("../../Cargo.toml");
    assert!(manifest
        .contains("hyper = { version = \"=1.11.0\", default-features = false, optional = true }"));
    for feature in ["http-client", "http-client-rustls"] {
        let prefix = format!("{feature} = [\"dep:bytes\", \"dep:hyper\", ");
        assert!(
            manifest.contains(&prefix),
            "{feature} must pin hyper's parser"
        );
    }
}

/// `crash-handler` exports unmangled signal and exception symbols
/// (`pthread_create`, `ehsetjmp`, ...). A second locked version anywhere in
/// the graph, such as through zccache, fails every all-features native link.
#[test]
fn crash_handler_is_locked_at_exactly_one_version() {
    let lock = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock"))
        .expect("read Cargo.lock");
    let versions: Vec<&str> = lock
        .split("[[package]]")
        .filter(|package| {
            package
                .lines()
                .any(|line| line.trim_end() == "name = \"crash-handler\"")
        })
        .filter_map(|package| {
            package
                .lines()
                .find_map(|line| line.trim_end().strip_prefix("version = "))
        })
        .collect();
    assert_eq!(versions, ["\"0.7.0\""]);
}

/// Build-script resource embedding is a feature of this one package, not a
/// separately published companion: the optional resource compiler stays out
/// of every graph that does not ask, out of `full`, and the fixture consumer
/// keeps the documented same-name build-dependency shape.
#[test]
fn build_resources_is_an_opt_in_feature_of_the_one_package() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read manifest");
    assert!(
        manifest.contains("build-resources = [\"dep:embed-resource\"]"),
        "build-resources must own exactly the resource compiler"
    );
    assert!(
        manifest.contains("embed-resource = { version = \"=3.0.11\", optional = true }"),
        "the resource compiler must be an optional, exact-pinned dependency"
    );
    assert!(
        !manifest.contains("members ="),
        "kernal-api is one package; build helpers are features, not workspace members"
    );
    assert!(
        !root.join("crates").exists(),
        "no separately published companion package may return"
    );
    let full = manifest
        .split("full = [")
        .nth(1)
        .and_then(|features| features.split(']').next())
        .expect("locate full feature");
    assert!(
        !full.contains("build-resources"),
        "full is the runtime surface; the build-script helper stays out"
    );
    let lib = std::fs::read_to_string(root.join("src/lib.rs")).expect("read facade root");
    assert!(
        lib.contains("#[cfg(feature = \"build-resources\")]\npub mod build_resources;"),
        "default builds must omit the build_resources module"
    );

    let consumer = std::fs::read_to_string(root.join("tests/build-resources-consumer/Cargo.toml"))
        .expect("read build-resources consumer manifest");
    assert!(
        consumer.contains("[workspace]")
            && consumer.contains(
                "[build-dependencies]\nkernal-api = { path = \"../..\", default-features = false, features = [\"build-resources\"] }"
            ),
        "the consumer must take build-resources as a same-name build-dependency outside this workspace"
    );
    let build_script =
        std::fs::read_to_string(root.join("tests/build-resources-consumer/build.rs"))
            .expect("read build-resources consumer build script");
    assert!(
        build_script.contains("embed_windows_app_resources(&resources)"),
        "the consumer build script must still embed resources"
    );
}

pub(crate) fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut sources = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).expect("read source directory") {
            let path = entry.expect("source entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                sources.push(path);
            }
        }
    }
    sources
}

fn workflow_job<'a>(workflow: &'a str, name: &str) -> &'a str {
    let marker = format!("  {name}:\n");
    let start = workflow
        .find(&marker)
        .unwrap_or_else(|| panic!("release workflow must contain the {name} job"));
    let body = &workflow[start + marker.len()..];
    for (index, _) in body.match_indices("\n  ") {
        if body.as_bytes().get(index + 3) != Some(&b' ') {
            return &body[..index];
        }
    }
    body
}

#[test]
fn implementation_crates_are_not_publicly_reexported() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    // The raw platform crates are listed beside the backends: #77 collapsed
    // the per-host bindings into this crate's private HAL, so re-exporting one
    // hands a client the host vocabulary the facade exists to absorb. This
    // scan is text, so it covers the `cfg`-elided hosts the Linux lint job
    // never compiles.
    let forbidden = [
        "pub use addr2line",
        "pub use blake3",
        "pub use console_api",
        "pub use console_subscriber",
        "pub use crash_handler",
        "pub use framehop",
        "pub use globset",
        "pub use interprocess",
        "pub use jwalk",
        "pub use libc",
        "pub use libsqlite3_sys",
        "pub use mach2",
        "pub use memmap2",
        "pub use mimalloc_pprof",
        "pub use notify",
        "pub use pdb_addr2line",
        "pub use portable_pty",
        "pub use reflink_copy",
        "pub use running_process",
        "pub use rusqlite",
        "pub use sysinfo",
        "pub use tokio",
        "pub use widestring",
        "pub use winapi",
        "pub use windows_sys",
    ];
    for path in rust_sources(&root) {
        let source = std::fs::read_to_string(&path).expect("read Rust source");
        for spelling in forbidden {
            assert!(
                !source.contains(spelling),
                "{} exposes forbidden backend spelling {spelling:?}",
                path.display()
            );
        }
    }
}

#[test]
fn process_substrate_is_exact_feature_minimal_and_private() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read manifest");
    assert!(
        manifest.contains(
            "running-process = { version = \"=4.10.21\", default-features = false, features = [\"kernel-substrate\", \"ape-loader\"] }"
        ),
        "the facade must retain the exact published running-process pin and minimal default feature set"
    );
    assert!(
        manifest.contains("independent-spawn = [\"running-process/independent-spawn\"]"),
        "the facade-owned independent-spawn capability must remain an explicit opt-in"
    );
    assert!(
        manifest.contains("# Exact first-party pre-1.0 pin."),
        "the released process substrate must retain its exact first-party pin rationale"
    );

    let release_workflow = std::fs::read_to_string(root.join(".github/workflows/release.yml"))
        .expect("read release workflow");
    let release_guard = workflow_job(&release_workflow, "release-guard");
    assert!(
        release_guard.contains("uv run --no-project --with tomli==2.2.1 python")
            && release_guard.contains("ci/check_release_process_substrate.py"),
        "the dedicated release guard must invoke the TOML-aware process-substrate validator"
    );
    for cargo_job in ["validate-and-package", "symbolizer-workers"] {
        assert!(
            workflow_job(&release_workflow, cargo_job).contains("needs: release-guard"),
            "{cargo_job} must depend on release-guard before invoking soldr/cargo"
        );
    }
    // publish-crates runs in auto-release.yml, the workflow registered as the
    // crate's trusted publisher. It needs the whole `release` workflow, which
    // is release.yml -- release-guard included -- so the guard still runs
    // before cargo publish.
    let auto_release = std::fs::read_to_string(root.join(".github/workflows/auto-release.yml"))
        .expect("read auto-release workflow");
    assert!(
        workflow_job(&auto_release, "release").contains("uses: ./.github/workflows/release.yml"),
        "auto-release's release job must run release.yml, where release-guard lives"
    );
    let full_ci_gate = workflow_job(&auto_release, "full-ci-gate");
    assert!(
        full_ci_gate.contains("ci/release_ci_gate.py")
            && full_ci_gate.contains("CANDIDATE_SHA: ${{ inputs.candidate_sha }}")
            && full_ci_gate.contains("FULL_CI_RUN_ID: ${{ inputs.full_ci_run_id }}"),
        "full-ci-gate must verify full CI for the exact release candidate SHA"
    );
    assert!(
        workflow_job(&auto_release, "release").contains("needs: [prepare, full-ci-gate]"),
        "release must wait for exact-SHA full CI before tagging or publishing"
    );
    assert!(
        workflow_job(&auto_release, "publish-crates")
            .contains("needs: [prepare, full-ci-gate, release]"),
        "publish-crates must depend on exact-SHA full CI and the whole release workflow"
    );
    assert!(
        !release_workflow.contains("cargo publish"),
        "cargo publish must stay in auto-release.yml's publish-crates job"
    );

    let lib = std::fs::read_to_string(root.join("src/lib.rs")).expect("read facade root");
    assert!(
        !lib.contains("tokio::process"),
        "the migrated process facade must not retain a Tokio child fallback"
    );
    assert!(
        !lib.contains("configure_command("),
        "the migrated process facade must not retain native spawn configuration"
    );
    let adapter = std::fs::read_to_string(root.join("src/process_adapter.rs"))
        .expect("read private process adapter");
    for mapping in [
        ".create_process_group(create_process_group)",
        ".kill_when_owner_dies(kill_when_owner_dies)",
        ".nice(priority.substrate_nice())",
    ] {
        assert!(
            adapter.contains(mapping),
            "the private adapter must preserve SpawnSpec's {mapping} policy"
        );
    }

    for path in rust_sources(&root.join("src")) {
        let source = std::fs::read_to_string(&path).expect("read Rust source");
        for line in source.lines() {
            let line = line.trim_start();
            if line.starts_with("pub ") {
                assert!(
                    !line.contains("running_process"),
                    "{} exposes a running-process type in {line:?}",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn sqlite_is_opt_in_bundled_and_backend_private() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read manifest");
    assert!(
        manifest.contains("sqlite = [\"dep:rusqlite\", \"dep:tempfile\"]"),
        "SQLite must remain an opt-in capability"
    );
    assert!(
        manifest.contains("rusqlite = { version = \"=0.40.2\", default-features = false, features = [\"bundled\", \"backup\"], optional = true }"),
        "SQLite must use the audited bundled backend only behind its feature"
    );
    let full = manifest
        .split("full = [")
        .nth(1)
        .and_then(|tail| tail.split(']').next())
        .expect("locate full feature");
    assert!(
        !full.contains("sqlite"),
        "full must not enable application storage"
    );
    let lib = std::fs::read_to_string(root.join("src/lib.rs")).expect("read facade root");
    assert!(
        lib.contains("#[cfg(feature = \"sqlite\")]\npub mod sqlite;"),
        "the facade module must be feature-gated"
    );
}

#[test]
fn daemon_identity_remains_opt_in_and_out_of_full() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read manifest");
    assert!(
        manifest.contains("daemon-identity = [\"running-process/backend-identity\"]"),
        "daemon identity must compose only the direct backend-identity substrate feature"
    );

    let full = manifest
        .split("full = [")
        .nth(1)
        .and_then(|tail| tail.split(']').next())
        .expect("locate full feature");
    assert!(
        !full.contains("daemon-identity"),
        "full must retain the established heavyweight feature set without direct-daemon identity"
    );

    let lib = std::fs::read_to_string(root.join("src/lib.rs")).expect("read facade root");
    assert!(
        lib.contains("#[cfg(feature = \"daemon-identity\")]\npub mod daemon_identity;"),
        "default builds must omit the daemon identity facade module"
    );
}

/// The broker v2 lifecycle shares the substrate's heavy `client` graph, so it
/// stays opt-in and outside `full`, and delegates the wire to the substrate.
#[test]
fn broker_v2_remains_opt_in_out_of_full_and_an_adapter() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read manifest");
    assert!(
        manifest.contains("broker-v2 = [\"running-process/client\", \"daemon-identity\", \"ipc\"]"),
        "broker-v2 must compose only the substrate client, identity, and ipc"
    );
    let full = manifest
        .split("full = [")
        .nth(1)
        .and_then(|tail| tail.split(']').next())
        .expect("locate full feature");
    assert!(
        !full.contains("broker-v2"),
        "full must not pull the broker v2 graph"
    );
    let adapter =
        std::fs::read_to_string(root.join("src/broker_v2.rs")).expect("read broker v2 facade");
    assert!(
        adapter.contains("backend::connect_service_with_deadline("),
        "the facade must delegate the v2 Hello to the substrate"
    );
    for forbidden in [
        "write_frame(",
        "read_frame(",
        "encode_to_vec(",
        "HelloReply::decode(",
    ] {
        assert!(
            !adapter.contains(forbidden),
            "the broker v2 facade must not reimplement the broker wire ({forbidden:?})"
        );
    }
}

/// The broker client adapter selects the substrate's heavy `client` feature,
/// so it stays opt-in, outside `full`, and an adapter rather than a second
/// broker implementation.
#[test]
fn broker_client_remains_opt_in_out_of_full_and_an_adapter() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read manifest");
    assert!(
        manifest.contains("broker-client = [\"running-process/client\"]"),
        "broker-client must compose only the substrate's client feature"
    );
    let full = manifest
        .split("full = [")
        .nth(1)
        .and_then(|tail| tail.split(']').next())
        .expect("locate full feature");
    assert!(
        !full.contains("broker-client"),
        "full must not pull the broker client's CLI/config/IPC graph"
    );
    let lib = std::fs::read_to_string(root.join("src/lib.rs")).expect("read facade root");
    assert!(
        lib.contains("#[cfg(feature = \"broker-client\")]\npub mod broker_client;"),
        "default builds must omit the broker client facade module"
    );
    let adapter = std::fs::read_to_string(root.join("src/broker_client.rs"))
        .expect("read broker client facade");
    assert!(
        adapter.contains("backend::connect_to_backend("),
        "the facade must delegate the broker connect to the substrate"
    );
    for forbidden in [
        "write_frame(",
        "read_frame(",
        "encode_to_vec(",
        "HelloReply::decode(",
    ] {
        assert!(
            !adapter.contains(forbidden),
            "the broker client facade must not reimplement the broker wire ({forbidden:?})"
        );
    }
}

#[test]
fn daemon_frame_v1_remains_transport_free_and_product_neutral() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read manifest");
    assert!(
        manifest.contains("daemon-frame-v1 = [\"running-process/frame-v1-codec\"]"),
        "the facade feature must select only the upstream frame-only codec"
    );
    let feature = manifest
        .split("daemon-frame-v1 = [")
        .nth(1)
        .and_then(|tail| tail.split(']').next())
        .expect("locate daemon-frame-v1 feature");
    for forbidden in [
        "backend-identity",
        "client",
        "ipc",
        "blake3",
        "sha2",
        "getrandom",
        "tokio",
    ] {
        assert!(
            !feature.contains(forbidden),
            "daemon-frame-v1 must not select {forbidden:?}"
        );
    }

    let full = manifest
        .split("full = [")
        .nth(1)
        .and_then(|tail| tail.split(']').next())
        .expect("locate full feature");
    assert!(
        !full.contains("daemon-frame-v1"),
        "full must retain its established heavyweight feature set without daemon-frame-v1"
    );

    let lib = std::fs::read_to_string(root.join("src/lib.rs")).expect("read facade root");
    assert!(
        lib.contains("#[cfg(feature = \"daemon-frame-v1\")]\npub mod daemon_frame_v1;"),
        "default builds must omit the daemon frame facade module"
    );

    let frame = std::fs::read_to_string(root.join("src/daemon_frame_v1.rs"))
        .expect("read daemon-frame facade");
    assert!(
        !frame.contains("0x7A63"),
        "zccache's product protocol identifier must not be owned by kernal-api"
    );
    for line in frame
        .lines()
        .map(str::trim_start)
        .filter(|line| line.starts_with("pub "))
    {
        for forbidden in [
            "running_process",
            "prost",
            "BytesMut",
            "tokio",
            "RawFd",
            "RawHandle",
        ] {
            assert!(
                !line.contains(forbidden),
                "daemon-frame facade leaks {forbidden:?}: {line}"
            );
        }
    }
}

#[test]
fn daemon_registration_remains_opt_in_and_client_free() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read manifest");
    assert!(
        manifest.contains("daemon-registration = [\"running-process/daemon-registration\"]"),
        "daemon registration must select only the direct registration substrate"
    );
    let feature = manifest
        .split("daemon-registration = [")
        .nth(1)
        .and_then(|tail| tail.split(']').next())
        .expect("locate daemon-registration feature");
    for forbidden in [
        "backend-identity",
        "client",
        "ipc",
        "blake3",
        "tokio",
        "runtime",
    ] {
        assert!(
            !feature.contains(forbidden),
            "daemon-registration must not select {forbidden:?}"
        );
    }

    let full = manifest
        .split("full = [")
        .nth(1)
        .and_then(|tail| tail.split(']').next())
        .expect("locate full feature");
    assert!(
        !full.contains("daemon-registration"),
        "full must retain its established heavyweight feature set without daemon registration"
    );

    let lib = std::fs::read_to_string(root.join("src/lib.rs")).expect("read facade root");
    assert!(
        lib.contains("#[cfg(feature = \"daemon-registration\")]\npub mod daemon_registration;"),
        "default builds must omit the daemon-registration facade module"
    );

    let registration = std::fs::read_to_string(root.join("src/daemon_registration.rs"))
        .expect("read daemon-registration facade");
    for forbidden in ["protocol_v2", "0x7A63", "zccache"] {
        assert!(
            !registration.contains(forbidden),
            "daemon-registration must not own {forbidden:?}"
        );
    }
    for line in registration
        .lines()
        .map(str::trim_start)
        .filter(|line| line.starts_with("pub "))
    {
        for forbidden in [
            "running_process",
            "prost",
            "backend::",
            "tokio",
            "BytesMut",
            "RawFd",
            "RawHandle",
            "platform",
        ] {
            assert!(
                !line.contains(forbidden),
                "daemon-registration facade leaks {forbidden:?}: {line}"
            );
        }
    }
    assert!(
        registration.contains("self.inner.install().map_err(service_error)")
            && registration.contains("self.inner.install_in(root.as_ref()).map_err(service_error)")
            && !registration.contains("fs::write"),
        "service-definition persistence must delegate to the frozen upstream non-atomic v1 writer"
    );

    let consumer_root = root.join("tests/daemon-registration-consumer");
    let consumer_manifest = std::fs::read_to_string(consumer_root.join("Cargo.toml"))
        .expect("read external daemon-registration consumer manifest");
    assert!(
        consumer_manifest.contains("[workspace]")
            && consumer_manifest
                .contains("default-features = false, features = [\"daemon-registration\"]"),
        "external consumer must compile the opt-in facade outside this workspace"
    );
    let consumer_source = std::fs::read_to_string(consumer_root.join("src/main.rs"))
        .expect("read external daemon-registration consumer source");
    // A forbidden-substring loop is vacuously true on an emptied file, so
    // require the calls this fixture claims to make. CI's `facade-consumers`
    // job is what actually compiles it; this is only the backstop against the
    // fixture being hollowed out.
    for required in [
        "use kernal_api::daemon_registration::",
        "CacheManifestBuilder::new",
        "ServiceDefinitionBuilder::shared_broker",
    ] {
        assert!(
            consumer_source.contains(required),
            "external consumer must still exercise {required:?}"
        );
    }
    for forbidden in ["running_process", "prost", "tokio", "platform"] {
        assert!(
            !consumer_source.contains(forbidden),
            "external consumer must not require {forbidden:?}"
        );
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Existing function complexity measured at ratchet adoption"
)]
fn daemon_registration_v2_remains_opt_in_and_client_free() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read manifest");
    assert!(
        manifest.contains("daemon-registration-v2 = [\"running-process/daemon-registration-v2\"]"),
        "daemon registration v2 must select only the direct v2 registration substrate"
    );
    let feature = manifest
        .split("daemon-registration-v2 = [")
        .nth(1)
        .and_then(|tail| tail.split(']').next())
        .expect("locate daemon-registration-v2 feature");
    for forbidden in [
        "daemon-registration\"",
        "backend-identity",
        "client",
        "ipc",
        "blake3",
        "sha2",
        "tokio",
        "runtime",
    ] {
        assert!(
            !feature.contains(forbidden),
            "daemon-registration-v2 must not select {forbidden:?}"
        );
    }

    let full = manifest
        .split("full = [")
        .nth(1)
        .and_then(|tail| tail.split(']').next())
        .expect("locate full feature");
    assert!(
        !full.contains("daemon-registration-v2"),
        "full must retain its established heavyweight feature set without daemon registration v2"
    );

    let lib = std::fs::read_to_string(root.join("src/lib.rs")).expect("read facade root");
    assert!(
        lib.contains(
            "#[cfg(feature = \"daemon-registration-v2\")]\npub mod daemon_registration_v2;"
        ),
        "default builds must omit the daemon-registration-v2 facade module"
    );

    let registration = std::fs::read_to_string(root.join("src/daemon_registration_v2.rs"))
        .expect("read daemon-registration-v2 facade");
    for forbidden in ["protocol_v2", "http_server", "zccache"] {
        assert!(
            !registration.contains(forbidden),
            "daemon-registration-v2 must not own {forbidden:?}"
        );
    }
    for line in registration
        .lines()
        .map(str::trim_start)
        .filter(|line| line.starts_with("pub "))
    {
        for forbidden in [
            "running_process",
            "prost",
            "backend",
            "tokio",
            "BytesMut",
            "RawFd",
            "RawHandle",
            "platform",
        ] {
            assert!(
                !line.contains(forbidden),
                "daemon-registration-v2 facade leaks {forbidden:?}: {line}"
            );
        }
    }
    assert!(
        registration.contains("backend::write_service_definition_v2")
            && !registration.contains("fs::write"),
        "v2 persistence must delegate to the frozen upstream non-atomic writer"
    );

    let consumer_root = root.join("tests/daemon-registration-v2-consumer");
    let consumer_manifest = std::fs::read_to_string(consumer_root.join("Cargo.toml"))
        .expect("read external daemon-registration-v2 consumer manifest");
    assert!(
        consumer_manifest.contains("[workspace]")
            && consumer_manifest
                .contains("default-features = false, features = [\"daemon-registration-v2\"]"),
        "external consumer must compile the opt-in v2 facade outside this workspace"
    );
    let consumer_source = std::fs::read_to_string(consumer_root.join("src/main.rs"))
        .expect("read external daemon-registration-v2 consumer source");
    // Same backstop as the v1 consumer above: keep the emptied-fixture case
    // from passing the forbidden-substring loop by default.
    for required in [
        "use kernal_api::daemon_registration_v2::",
        "ServiceDefinitionBuilder::shared_broker",
        "service_definition_path(",
    ] {
        assert!(
            consumer_source.contains(required),
            "external v2 consumer must still exercise {required:?}"
        );
    }
    for forbidden in ["running_process", "prost", "tokio", "platform"] {
        assert!(
            !consumer_source.contains(forbidden),
            "external consumer must not require {forbidden:?}"
        );
    }
}

#[test]
fn process_session_surface_keeps_backend_and_native_status_types_private() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let lib = std::fs::read_to_string(root.join("src/lib.rs")).expect("read facade root");
    let session = lib
        .split("/// Explicit bounds and terminal-owner policy for [`ProcessSession`].")
        .nth(1)
        .and_then(|surface| {
            surface
                .split("/// Status-preserving result of bounded child-output capture.")
                .next()
        })
        .expect("locate public process-session facade");
    for forbidden in [
        "running_process",
        "tokio::",
        "std::process::Command",
        "std::process::Child",
        "ExitStatus",
    ] {
        assert!(
            !session.contains(forbidden),
            "process-session facade leaks {forbidden:?}"
        );
    }
    let session_exit = lib
        .split("pub struct ProcessSessionExit")
        .nth(1)
        .and_then(|surface| {
            surface
                .split("/// Signed compatibility termination code")
                .next()
        })
        .expect("locate facade-owned session exit status");
    assert!(
        session_exit.contains("native_status: u32") && session_exit.contains("signal: Option<i32>"),
        "session exit must retain a facade-owned native status plus Unix signal semantics"
    );
}

/// Owned-crate spellings that name a backend type wherever they appear in a
/// type position. Mirrors `OWNED_IMPLEMENTATION_CRATES` in
/// `dylints/kernal_api_boundary`.
const OWNED_BACKEND_PATHS: [&str; 25] = [
    "addr2line::",
    "blake3::",
    "console_api::",
    "console_subscriber::",
    "crash_handler::",
    "framehop::",
    "globset::",
    "interprocess::",
    "jwalk::",
    "libc::",
    "libsqlite3_sys::",
    "mach2::",
    "memmap2::",
    "mimalloc_pprof::",
    "notify::",
    "pdb_addr2line::",
    "portable_pty::",
    "reflink_copy::",
    "running_process::",
    "rusqlite::",
    "sysinfo::",
    "tokio::",
    "widestring::",
    "winapi::",
    "windows_sys::",
];

/// A Dylint pass resolves types, but only for the code the compiler compiles,
/// and `default = []`. A `dylints` job without `--all-features` skips `crash`,
/// `wasm`, `symbolize`, and every other gated module while still reporting
/// green, which is the failure this guard exists to prevent (#108).
#[test]
fn the_dylint_job_lints_every_feature_gated_module() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow =
        std::fs::read_to_string(root.join(".github/workflows/ci.yml")).expect("read CI workflow");
    assert!(
        workflow_job(&workflow, "dylints")
            .contains("--all --workspace -- --all-features --all-targets"),
        "the boundary lints must run over every feature-gated module and target"
    );
}

/// An always-on companion to `kernal_api_boundary`, whose HIR pass sees only
/// the code compiled on the lint host -- every feature since #108, but never a
/// `cfg(windows)` or `cfg(target_os = "macos")` body, because that job runs on
/// Linux. This scan is deliberately coarse: it covers the single-line shapes
/// -- `pub` items, every variant of a `pub enum`, and the `pub` fields of a
/// `pub struct` -- and leaves wrapped signatures, bounds, and alias chasing to
/// the lint.
#[test]
fn backend_types_are_absent_from_public_type_positions() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for path in rust_sources(&root) {
        let source = std::fs::read_to_string(&path).expect("read Rust source");
        for (line, position) in public_type_positions(&source) {
            for spelling in OWNED_BACKEND_PATHS {
                assert!(
                    !position.contains(spelling),
                    "{}:{line} names backend type {spelling:?} in a public type position: {}",
                    path.display(),
                    position.trim()
                );
            }
        }
    }
}

/// A public declaration wrapped across line breaks must still be examined.
///
/// #147: the scan read `pub` lines one at a time, so a backend path on a
/// continuation line was invisible to it. The resolving Dylint could not cover
/// the gap either -- its job runs on Linux, so a `#[cfg(windows)]` body is
/// never compiled and never linted. Both guards reported clean while a raw
/// `winapi::` type sat in a public Windows signature.
#[test]
fn wrapped_public_signatures_are_scanned_across_line_breaks() {
    let source = "\
pub fn wrapped_backend_parameter(
    record: &libc::timespec,
) -> Option<u32> {
    0
}

pub(crate) fn private_wrapped_backend_parameter(
    record: &libc::timespec,
) -> Option<u32> {
    0
}

pub fn wrapped_backend_in_a_comment(
    // libc::timespec is mentioned only here
    record: u32,
) -> Option<u32> {
    0
}
";
    let positions = public_type_positions(source);
    let naming_backend: Vec<&(usize, String)> = positions
        .iter()
        .filter(|(_, position)| position.contains("libc::"))
        .collect();

    assert!(
        !naming_backend.is_empty(),
        "a backend path in a wrapped public signature must be scanned: {positions:?}"
    );
    // Exactly one: the `pub(crate)` declaration is not a public type position,
    // and a comment is not a type position at all.
    assert_eq!(
        naming_backend.len(),
        1,
        "only the public signature should be reported: {naming_backend:?}"
    );
    assert!(
        naming_backend[0].0 == 1,
        "the position should be reported at the declaration's first line: {:?}",
        naming_backend[0]
    );
}

/// Split a single-line tuple-struct declaration into its header -- name,
/// generics, and bounds -- and its parenthesized field list.
fn tuple_struct_split(line: &str) -> Option<(&str, &str)> {
    if !line.starts_with("pub struct") {
        return None;
    }
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    (open < close).then(|| (&line[..open], &line[open + 1..close]))
}

/// Split a single-line brace-struct declaration the same way, so its field
/// visibilities can be read rather than assumed public.
fn inline_brace_struct_split(line: &str) -> Option<(&str, &str)> {
    if !line.starts_with("pub struct") {
        return None;
    }
    let open = line.find('{')?;
    let close = line.rfind('}')?;
    (open < close).then(|| (&line[..open], &line[open + 1..close]))
}

/// The one-based line number and type-bearing text of every public type
/// position in `source`.
///
/// A declaration that continues past its first line is **joined before it is
/// examined**. That is the whole point of the joining: a per-line scan cannot
/// see a backend path sitting on a continuation line, and that is precisely the
/// shape that let a raw `winapi::` type reach a public Windows signature while
/// both guards reported clean (#147).
fn public_type_positions(source: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = source.lines().collect();
    let mut positions = Vec::new();
    // Indentation of the `pub enum`/`pub struct` header whose body is open,
    // plus whether every field of that body is public. A `pub(crate)` field
    // does not begin with `pub `, so it is not a public type position; every
    // field of a public enum variant is as visible as the enum itself.
    let mut open: Option<(usize, bool)> = None;
    let mut index = 0;
    while index < lines.len() {
        let raw = lines[index];
        let indent = raw.len() - raw.trim_start().len();
        let line = strip_trailing_comment(raw.trim_start());
        if line.is_empty() {
            index += 1;
            continue;
        }
        if let Some((body_indent, fields_are_public)) = open {
            if line == "}" && indent == body_indent {
                open = None;
            } else if fields_are_public || line.starts_with("pub ") {
                positions.push((index + 1, line.to_string()));
            }
            index += 1;
            continue;
        }
        if !line.starts_with("pub ") {
            index += 1;
            continue;
        }
        let (joined, last) = joined_declaration(&lines, index);
        let line = joined.as_str();
        if line.is_empty() {
            index = last + 1;
            continue;
        }
        // A `pub const`/`pub static` initializer is a value, not a type, and
        // the fields of a tuple struct carry their own visibility.
        let declaration = if line.starts_with("pub const") || line.starts_with("pub static") {
            line.split('=').next().unwrap_or(line).to_string()
        } else if let Some((header, fields)) = tuple_struct_split(line) {
            if fields
                .split(',')
                .any(|field| field.trim_start().starts_with("pub "))
            {
                line.to_string()
            } else {
                header.to_string()
            }
        } else if let Some((header, fields)) = inline_brace_struct_split(line) {
            // `pub struct S { a: T }` written on one line never opens a body
            // for the loop above to walk, so without this the private fields
            // are read as public type positions. Same rule as the tuple case.
            if fields
                .split(',')
                .any(|field| field.trim_start().starts_with("pub "))
            {
                line.to_string()
            } else {
                header.to_string()
            }
        } else {
            line.to_string()
        };
        positions.push((index + 1, declaration.clone()));
        if declaration.ends_with('{') {
            let variant_fields_are_public = declaration.starts_with("pub enum");
            if variant_fields_are_public || declaration.starts_with("pub struct") {
                open = Some((indent, variant_fields_are_public));
            }
        }
        index = last + 1;
    }
    positions
}

/// The code on `line`, without a trailing `//` comment.
///
/// Comments must not contribute type positions: a wrapped signature carrying a
/// comment that names a backend crate would otherwise be reported as a
/// violation of this policy by its own documentation.
fn strip_trailing_comment(line: &str) -> &str {
    match line.find("//") {
        Some(index) => line[..index].trim_end(),
        None => line,
    }
}

/// The declaration beginning at `lines[start]`, joined across line breaks, and
/// the index of its last line.
///
/// Ends at the `{` that opens a body or the `;` that closes the declaration,
/// counting `(`/`[` nesting so a delimiter inside a parameter list does not end
/// it early. A declaration never wraps past the end of the source.
fn joined_declaration(lines: &[&str], start: usize) -> (String, usize) {
    let mut joined = String::new();
    let mut depth: usize = 0;
    for (offset, raw) in lines[start..].iter().enumerate() {
        let text = strip_trailing_comment(raw.trim());
        if text.is_empty() {
            continue;
        }
        if !joined.is_empty() {
            joined.push(' ');
        }
        joined.push_str(text);
        for character in text.chars() {
            match character {
                '{' | ';' if depth == 0 => {
                    return (joined.trim_end().to_string(), start + offset);
                }
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    (joined.trim_end().to_string(), lines.len().saturating_sub(1))
}

#[test]
fn json_backend_is_confined_to_owned_document_and_firefox_adapters() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for path in rust_sources(&root) {
        let relative = path.strip_prefix(&root).expect("source below root");
        let normalized = relative.to_string_lossy().replace('\\', "/");
        let source = std::fs::read_to_string(&path).expect("read Rust source");
        if source.contains("serde_json") {
            assert!(
                matches!(
                    normalized.as_str(),
                    "json.rs" | "profile/export/firefox.rs" | "profile/tests.rs"
                ),
                "{} uses JSON outside the owned adapter boundaries",
                path.display()
            );
        }
    }
}

/// The window-icon capability is GUI hosting, not part of the `default = []`
/// async process/host HAL, so it must be feature-gated like every peer
/// capability (`fs`, `fs-watch`, `ipc`, `pty`).
///
/// This is a manifest-and-source test rather than a `cargo tree` case in
/// `ci/check_compilation_boundary_dependencies.py` on purpose. That harness
/// proves a package is absent from the default graph, and `png`/`x11rb`
/// cannot be: `running-process-platform-internal` 4.10.12 declares both as
/// non-optional `cfg(target_os = "linux")` dependencies, and `running-process`
/// is a mandatory private dependency here. Gating this crate's own copy is
/// what is in this crate's power; the graph reduction arrives when the
/// substrate gates its own.
#[test]
fn window_icon_stays_an_opt_in_gui_capability() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read manifest");
    assert!(
        manifest.contains(r#"window-icon = ["dep:png", "dep:x11rb"]"#),
        "the window-icon feature must own both GUI backends"
    );
    for optional in [
        r#"png = { version = "=0.18.1", optional = true }"#,
        r#"x11rb = { version = "=0.13.2", optional = true }"#,
    ] {
        assert!(
            manifest.contains(optional),
            "this crate's own GUI backend must be optional: {optional:?}"
        );
    }
    let full = manifest
        .split("full = [")
        .nth(1)
        .and_then(|features| features.split(']').next())
        .expect("locate full feature");
    assert!(
        full.contains("window-icon"),
        "full must still offer the whole surface to diagnostic executables"
    );

    // Assert on the gate reaching each item rather than on an exact adjacent
    // line pair: a later `#[cfg_attr(docsrs, ...)]` between the gate and the
    // item, or a rustfmt rewrap, must not read as a policy violation.
    for (file, items) in [
        (
            "src/lib.rs",
            &["pub use platform_imp::{set_window_icon_impl, window_icon_support_impl};"][..],
        ),
        ("src/platform.rs", &["pub mod window_icon;"][..]),
        (
            "src/platform_linux.rs",
            &["mod window_icon;", "pub use window_icon::{"][..],
        ),
        (
            "src/platform_macos.rs",
            &["mod window_icon;", "pub use window_icon::{"][..],
        ),
        (
            "src/platform_win.rs",
            &["mod window_icon;", "pub use window_icon::{"][..],
        ),
    ] {
        let source =
            std::fs::read_to_string(root.join(file)).unwrap_or_else(|_| panic!("read {file}"));
        for item in items {
            assert!(
                item_is_window_icon_gated(&source, item),
                "{file} must place `{item}` behind #[cfg(feature = \"window-icon\")]"
            );
        }
    }
}

/// True when every occurrence of `item` in `source` is preceded, ignoring
/// attributes and blank lines, by the `window-icon` gate. The macOS and
/// Windows host selectors are elided on the Linux lint host, so they are
/// checked as text the way the rest of this file checks them.
fn item_is_window_icon_gated(source: &str, item: &str) -> bool {
    const GATE: &str = "#[cfg(feature = \"window-icon\")]";
    let lines: Vec<&str> = source.lines().map(str::trim).collect();
    let mut seen = false;
    for (index, line) in lines.iter().enumerate() {
        if !line.starts_with(item) {
            continue;
        }
        seen = true;
        let gated = lines[..index]
            .iter()
            .rev()
            .take_while(|previous| previous.starts_with('#') || previous.is_empty())
            .any(|previous| *previous == GATE);
        if !gated {
            return false;
        }
    }
    seen
}

/// The private `running-process` adapter is mandatory and non-optional
/// (`Cargo.toml`, asserted by `process_substrate_is_exact_feature_minimal_and_private`),
/// so no document may still describe it as pending or as living on a migration
/// branch. A client that believes it has not landed keeps its own direct
/// substrate dependency -- the duplicate compile unit the boundary Dylint
/// exists to prevent -- and `src/lib.rs` inlines `README.md`, so a stale claim
/// there is the crate-level rustdoc on docs.rs.
#[test]
fn documents_do_not_claim_the_process_substrate_is_still_pending() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    // Each entry pairs the exact wording that document carried before the
    // adapter landed with wording that must be present now. Both halves
    // matter: the banned list alone would pass vacuously against a document
    // that never used the phrase, which is how a stale claim survives.
    // Comparisons run on whitespace-collapsed text so a rewrap cannot slip a
    // banned sentence past a line-oriented match.
    for (document, stale, required) in [
        (
            "README.md",
            &[
                "adapter is implemented on the `feat/running-process-adapter` migration branch",
                "On the phase-1 migration branch, its bounded process adapter",
            ][..],
            "phase-1 adapter has landed",
        ),
        (
            "AGENTS.md",
            &[
                "will privately depend on `running-process` when phase 1 lands",
                "Keep the broker implementation in `running-process` during phase 1",
            ][..],
            "privately depends on `running-process`",
        ),
        (
            "ARCHITECTURE.md",
            &["In the target architecture it depends on `running-process`"][..],
            "bounded process adapter has landed",
        ),
        (
            "COMPATIBILITY.md",
            &["That private dependency has not landed in the current release"][..],
            "That private dependency has landed",
        ),
        (
            "DYLINT.md",
            &["It is allowed inside `kernal-api`; phase 1 will add the private adapter"][..],
            "where the private adapter now lives",
        ),
    ] {
        let text = std::fs::read_to_string(root.join(document))
            .unwrap_or_else(|_| panic!("read {document}"));
        let collapsed = collapse_whitespace(&text);
        for banned in stale {
            assert!(
                !collapsed.contains(&collapse_whitespace(banned)),
                "{document} still describes the landed substrate as pending: {banned:?}"
            );
        }
        assert!(
            collapsed.contains(&collapse_whitespace(required)),
            "{document} must state that the substrate landed: {required:?}"
        );
    }
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `full` deliberately excludes the four daemon slices, so a docs.rs feature
/// set of `["full"]` alone renders none of them: four public modules would be
/// `cfg`'d out of the only documentation clients read. Keep the documentation
/// feature set a superset of the daemon features, and keep `--cfg docsrs`
/// backed by the attribute that acts on it rather than dead config.
#[test]
fn published_documentation_renders_every_public_module() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read manifest");
    let metadata = manifest
        .split("[package.metadata.docs.rs]")
        .nth(1)
        .and_then(|section| section.split("targets = [").next())
        .expect("locate docs.rs metadata");
    for feature in [
        "independent-spawn",
        "daemon-identity",
        "daemon-frame-v1",
        "daemon-registration",
        "daemon-registration-v2",
        "broker-client",
        "broker-v2",
    ] {
        assert!(
            metadata.contains(&format!("\"{feature}\"")),
            "docs.rs must document the opt-in {feature} module that full excludes"
        );
    }
    assert!(
        metadata.contains(r#"rustdoc-args = ["--cfg", "docsrs"]"#),
        "docs.rs metadata must still pass the cfg the crate root acts on"
    );

    let lib = std::fs::read_to_string(root.join("src/lib.rs")).expect("read facade root");
    assert!(
        lib.contains("#![cfg_attr(docsrs, feature(doc_cfg))]"),
        "`--cfg docsrs` is dead config without the attribute that emits feature badges"
    );

    // Every module the metadata exists to render must actually be public.
    for (feature, module) in [
        ("daemon-identity", "daemon_identity"),
        ("daemon-frame-v1", "daemon_frame_v1"),
        ("daemon-registration", "daemon_registration"),
        ("daemon-registration-v2", "daemon_registration_v2"),
        ("broker-client", "broker_client"),
        ("broker-v2", "broker_v2"),
    ] {
        assert!(
            lib.contains(&format!(
                "#[cfg(feature = \"{feature}\")]\npub mod {module};"
            )),
            "{module} must stay a public module gated on {feature}"
        );
    }

    // Each opt-in module needs a README entry, since a reader who never opens
    // Cargo.toml is how these stayed invisible.
    let readme = std::fs::read_to_string(root.join("README.md")).expect("read README");
    for feature in [
        "daemon-identity",
        "daemon-frame-v1",
        "daemon-registration",
        "daemon-registration-v2",
        "broker-client",
        "broker-v2",
        "window-icon",
    ] {
        assert!(
            readme.contains(&format!("`{feature}`")),
            "README's feature list must name {feature}"
        );
    }
}
