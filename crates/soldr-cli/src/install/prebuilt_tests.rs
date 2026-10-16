//! soldr#3697: hermetic tests for the prebuilt release-asset lane. A
//! loopback HTTP server stands in for GitHub Releases; nothing leaves the
//! host.

use std::path::PathBuf;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::acquire;
use super::plan::{AcquisitionPlan, ResolvedInstall};
use super::refs::{Form, Ref};
use super::target::InstallTarget;
use crate::core::SoldrPaths;
use crate::fetch::install_api::release::ReleaseAsset;

const TRIPLE: &str = "x86_64-unknown-linux-gnu";
const BINARY_BODY: &[u8] = b"#!/bin/sh\necho prebuilt-foo\n";

/// A release zip holding `foo-<triple>/foo`, like real tool releases.
fn fixture_zip() -> Vec<u8> {
    use std::io::Write;
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file(format!("foo-{TRIPLE}/foo"), opts).unwrap();
        zip.write_all(BINARY_BODY).unwrap();
        zip.finish().unwrap();
    }
    cursor.into_inner()
}

/// Serve `body` to every GET on a loopback port; returns the asset URL.
async fn serve(body: Vec<u8>, asset_name: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let body = body.clone();
            tokio::spawn(async move {
                let mut req = [0u8; 2048];
                let _ = socket.read(&mut req).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            });
        }
    });
    format!("http://{addr}/download/v1.2.3/{asset_name}")
}

fn resolved_release(root: PathBuf) -> ResolvedInstall {
    ResolvedInstall {
        name: "foo".into(),
        target: InstallTarget::GitHub {
            host: "github.com".into(),
            owner: "o".into(),
            repo: "foo".into(),
            url_ref: None,
            url_release: None,
            run_id: None,
        },
        git_ref: Ref::Tag("v1.2.3".into()),
        sha: "0123456789abcdef0123456789abcdef01234567".into(),
        release: None,
        release_note: Some("release v1.2.3".into()),
        form: Form::Auto,
        triple: TRIPLE.into(),
        debug: false,
        bins: vec![],
        features: vec![],
        locked: false,
        install_root: root,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn release_asset_plan_installs_verified_binary_without_build() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(tmp.path().join("soldr-home"));
    paths.ensure_dirs().unwrap();
    let zip = fixture_zip();
    let asset_name = format!("foo-{TRIPLE}.zip");
    let url = serve(zip.clone(), &asset_name).await;
    let resolved = resolved_release(tmp.path().join("installed"));
    // Explicit pin: the fixture's own sha256, as GitHub's `digest` reports it.
    let asset = ReleaseAsset {
        name: asset_name,
        url,
        bytes: zip.len() as u64,
        sha256: Some(crate::fetch::trust::sha256_of(&zip)),
    };
    let plan = acquire::plan_acquisition(&resolved, Some(&asset)).unwrap();
    assert!(
        matches!(plan, AcquisitionPlan::ReleaseAsset { .. }),
        "{plan:?}"
    );

    let binary = acquire::acquire_source(&paths, &resolved, &plan)
        .await
        .expect("prebuilt release asset must be acquired");
    assert!(binary.is_file(), "{}", binary.display());
    assert_eq!(std::fs::read(&binary).unwrap(), BINARY_BODY);
    // Landed in the locked tool cache, not a cargo build root.
    assert!(binary.starts_with(&paths.bin), "{}", binary.display());

    let placed = super::place::place_binary(
        &resolved.name,
        &binary,
        &resolved.install_root,
        TRIPLE,
        false,
    )
    .unwrap();
    assert_eq!(std::fs::read(&placed.binary).unwrap(), BINARY_BODY);
}

#[tokio::test(flavor = "multi_thread")]
async fn release_asset_sha256_mismatch_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(tmp.path().join("soldr-home"));
    paths.ensure_dirs().unwrap();
    let zip = fixture_zip();
    let asset_name = format!("foo-{TRIPLE}.zip");
    let url = serve(zip.clone(), &asset_name).await;
    let resolved = resolved_release(tmp.path().join("installed"));
    let asset = ReleaseAsset {
        name: asset_name,
        url,
        bytes: zip.len() as u64,
        sha256: Some("0".repeat(64)),
    };
    let plan = acquire::plan_acquisition(&resolved, Some(&asset)).unwrap();
    let err = acquire::acquire_source(&paths, &resolved, &plan)
        .await
        .expect_err("a sha256 mismatch must refuse the install");
    assert!(err.to_string().contains("sha256 mismatch"), "{err}");
    assert!(!paths.bin.join("install-foo-v1.2.3").exists());
}

fn some_asset() -> ReleaseAsset {
    ReleaseAsset {
        name: "foo.zip".into(),
        url: "http://127.0.0.1:9/foo.zip".into(),
        bytes: 1,
        sha256: None,
    }
}

#[test]
fn plan_prefers_asset_falls_back_to_source_and_honors_form() {
    let mut r = resolved_release(PathBuf::from("/unused"));
    assert!(matches!(
        acquire::plan_acquisition(&r, Some(&some_asset())).unwrap(),
        AcquisitionPlan::ReleaseAsset { .. }
    ));
    // No matching asset under the default form: source fallback.
    assert!(matches!(
        acquire::plan_acquisition(&r, None).unwrap(),
        AcquisitionPlan::CodeloadZip { .. }
    ));
    r.form = Form::Build;
    assert!(matches!(
        acquire::plan_acquisition(&r, Some(&some_asset())).unwrap(),
        AcquisitionPlan::CodeloadZip { .. }
    ));
    r.form = Form::Prebuilt;
    let err = acquire::plan_acquisition(&r, None).unwrap_err();
    assert!(err.to_string().contains("--prebuilt"), "{err}");
}
