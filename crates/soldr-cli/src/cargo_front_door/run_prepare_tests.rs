use super::prepared_nightly_covers_library_manifest;

#[test]
fn prepared_dylint_nightly_only_reuses_covered_library_manifests() {
    use crate::core::RustToolchainManifest;

    let channel = "nightly-2026-08-15-x86_64-unknown-linux-gnu";
    let manifest = RustToolchainManifest {
        channel: Some("nightly-2026-08-15".into()),
        profile: Some("minimal".into()),
        components: Some(vec!["rustc-dev".into(), "rust-src".into()]),
        ..RustToolchainManifest::default()
    };
    assert!(prepared_nightly_covers_library_manifest(channel, &manifest));
    for override_manifest in [
        RustToolchainManifest {
            channel: Some("nightly-2026-08-16".into()),
            ..manifest.clone()
        },
        RustToolchainManifest {
            channel: Some("nightly-2026-08-15-aarch64-unknown-linux-gnu".into()),
            ..manifest.clone()
        },
        RustToolchainManifest {
            components: Some(vec!["rustfmt".into()]),
            ..manifest.clone()
        },
        RustToolchainManifest {
            targets: Some(vec!["wasm32-unknown-unknown".into()]),
            ..manifest.clone()
        },
        RustToolchainManifest {
            profile: Some("complete".into()),
            ..manifest.clone()
        },
    ] {
        assert!(!prepared_nightly_covers_library_manifest(
            channel,
            &override_manifest
        ));
    }
}
