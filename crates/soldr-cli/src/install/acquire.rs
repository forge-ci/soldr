//! Acquisition ladder + source build for `soldr install` (soldr#2310).
//!
//! Given a fully-[`ResolvedInstall`], choose the fastest viable lane
//! (§4 of the issue) and materialize an on-disk source tree, then build
//! it with `cargo install --path` through soldr's compile-cache wrapper.

use std::path::{Path, PathBuf};

use crate::binaries::resolve_toolchain_binary;
use crate::build_from_source_cmd::apply_source_build_cache_wrapper;
use crate::core::{
    suppress_windows_console_window, InstallerWatchdogConfig, SoldrError, SoldrPaths,
};

use super::cache;
use super::fill;
use super::place::binary_ext_for_triple;
use super::plan::{AcquisitionPlan, ResolvedInstall};
use super::refs::{codeload_zip_url_for_sha, Form, Ref};
use super::target::InstallTarget;
use crate::fetch::install_api::release::ReleaseAsset;

pub(crate) const INSTALL_TIMEOUT_ENV_VAR: &str = "SOLDR_INSTALL_BUILD_TIMEOUT_SECS";

/// Choose the acquisition lane. Pure — the sha/release and prebuilt-asset
/// lookups have already happened in [`super::resolve`] /
/// [`super::lookup_prebuilt`].
///
/// A matching release asset wins unless `--build` forced source
/// (soldr#3697); `--prebuilt` with no matching asset is an error.
pub(crate) fn plan_acquisition(
    resolved: &ResolvedInstall,
    prebuilt: Option<&ReleaseAsset>,
) -> Result<AcquisitionPlan, SoldrError> {
    match (resolved.form, prebuilt) {
        (Form::Build, _) => {}
        (_, Some(asset)) => {
            return Ok(AcquisitionPlan::ReleaseAsset {
                url: asset.url.clone(),
                asset_name: asset.name.clone(),
                bytes: asset.bytes,
                sha256: asset.sha256.clone(),
            })
        }
        (Form::Prebuilt, None) => {
            return Err(SoldrError::Other(format!(
                "install: --prebuilt: no release asset of {} matches {}",
                resolved.name, resolved.triple
            )))
        }
        (Form::Auto, None) => {}
    }
    Ok(match &resolved.target {
        InstallTarget::Local(path) => AcquisitionPlan::LocalPath(path.clone()),
        InstallTarget::GitHub {
            host, owner, repo, ..
        } => {
            // Phase 1: GitHub → codeload zip (by resolved sha). Non-GitHub
            // hosts fall back to a shallow clone.
            if host.eq_ignore_ascii_case("github.com") && !resolved.sha.is_empty() {
                AcquisitionPlan::CodeloadZip {
                    url: codeload_zip_url_for_sha(owner, repo, &resolved.sha),
                    approx_bytes: None,
                }
            } else {
                AcquisitionPlan::ShallowClone {
                    clone_url: clone_url(host, owner, repo),
                }
            }
        }
    })
}

/// Binary names a prebuilt archive must contain: `--bin`, else the tool name.
pub(crate) fn prebuilt_binary_names(resolved: &ResolvedInstall) -> Vec<String> {
    if resolved.bins.is_empty() {
        vec![resolved.name.clone()]
    } else {
        resolved.bins.clone()
    }
}

/// The https clone URL for a remote target.
pub(crate) fn clone_url(host: &str, owner: &str, repo: &str) -> String {
    format!("https://{host}/{owner}/{repo}.git")
}

/// Acquire `plan`. For source lanes this returns the directory that holds
/// the crate's `Cargo.toml` (ready for `cargo install --path`); for
/// [`AcquisitionPlan::ReleaseAsset`] it returns the downloaded, verified,
/// extracted binary itself (no build).
pub(crate) async fn acquire_source(
    paths: &SoldrPaths,
    resolved: &ResolvedInstall,
    plan: &AcquisitionPlan,
) -> Result<PathBuf, SoldrError> {
    match plan {
        AcquisitionPlan::LocalPath(path) => {
            let canonical = path.canonicalize().map_err(|e| {
                SoldrError::Other(format!(
                    "install: local path {} is not accessible: {e}",
                    path.display()
                ))
            })?;
            Ok(canonical)
        }
        AcquisitionPlan::CodeloadZip { url, .. } => {
            acquire_codeload_zip(paths, resolved, url).await
        }
        AcquisitionPlan::ShallowClone { clone_url } => {
            acquire_shallow_clone(paths, resolved, clone_url)
        }
        AcquisitionPlan::ReleaseAsset {
            url,
            asset_name,
            bytes,
            sha256,
        } => {
            let asset = ReleaseAsset {
                name: asset_name.clone(),
                url: url.clone(),
                bytes: *bytes,
                sha256: sha256.clone(),
            };
            let names = prebuilt_binary_names(resolved);
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let version = resolved
                .git_ref
                .as_api_ref()
                .unwrap_or(resolved.sha.as_str())
                .replace(['/', '\\'], "_");
            crate::fetch::install_api::release::install_release_asset(
                paths,
                &format!("install-{}", resolved.name),
                &version,
                &asset,
                &resolved.triple,
                &names,
            )
            .await
        }
    }
}

fn cache_dir_for(paths: &SoldrPaths, resolved: &ResolvedInstall) -> Option<PathBuf> {
    match &resolved.target {
        InstallTarget::GitHub {
            host, owner, repo, ..
        } => Some(cache::source_cache_dir(
            paths,
            host,
            owner,
            repo,
            &resolved.sha,
        )),
        InstallTarget::Local(_) => None,
    }
}

async fn acquire_codeload_zip(
    paths: &SoldrPaths,
    resolved: &ResolvedInstall,
    url: &str,
) -> Result<PathBuf, SoldrError> {
    let cache_dir = cache_dir_for(paths, resolved)
        .ok_or_else(|| SoldrError::Other("install: codeload requires a GitHub target".into()))?;

    // Cache hit: a completed, content-addressed extraction is immutable.
    // Otherwise extract into a locked private staging dir (soldr#3689).
    let slot = match fill::begin(&cache_dir)? {
        fill::Fill::Hit(dir) => return single_crate_root(&dir),
        fill::Fill::Fill(slot) => slot,
    };

    // soldr#3639: repo-scoped token, never the workflow token for a foreign repo.
    let token = match &resolved.target {
        InstallTarget::GitHub { owner, repo, .. } => {
            crate::fetch::github::github_auth_token_for(owner, repo)
        }
        _ => None,
    };
    crate::fetch::source_zip::stream_and_extract_source_zip(url, slot.staging(), token.as_deref())
        .await?;

    let published = slot.publish()?;
    single_crate_root(&published)
}

pub(super) fn acquire_shallow_clone(
    paths: &SoldrPaths,
    resolved: &ResolvedInstall,
    clone_url: &str,
) -> Result<PathBuf, SoldrError> {
    let cache_dir = cache_dir_for(paths, resolved)
        .ok_or_else(|| SoldrError::Other("install: clone requires a remote target".into()))?;

    let slot = match fill::begin(&cache_dir)? {
        fill::Fill::Hit(dir) => return single_crate_root(&dir),
        fill::Fill::Fill(slot) => slot,
    };

    let checkout = slot.staging().join("checkout");
    // soldr#3690/#3691: fetch the resolved commit by sha into a fresh repo.
    // `git clone --branch` accepts only branches and tags, and the cache key
    // is that sha, so the checkout must be exactly it (not whatever the ref
    // names by the time the clone runs).
    let sha = match &resolved.git_ref {
        _ if !resolved.sha.is_empty() => resolved.sha.as_str(),
        Ref::Rev(rev) => rev.as_str(),
        _ => "HEAD",
    };
    let steps: Vec<Vec<std::ffi::OsString>> = vec![
        vec!["init".into(), "-q".into(), checkout.clone().into()],
        vec![
            "-C".into(),
            checkout.clone().into(),
            "fetch".into(),
            "--depth".into(),
            "1".into(),
            clone_url.into(),
            sha.into(),
        ],
        vec![
            "-C".into(),
            checkout.clone().into(),
            "checkout".into(),
            "-q".into(),
            "--detach".into(),
            "FETCH_HEAD".into(),
        ],
    ];
    for args in steps {
        let mut command = std::process::Command::new("git");
        command.args(&args);
        suppress_windows_console_window(&mut command);
        let status =
            crate::platform::wait::status_for(&mut command, crate::platform::wait::LONG_CHILD)
                .map_err(|e| SoldrError::Other(format!("install: failed to spawn git: {e}")))?;
        if !status.success() {
            // Dropping `slot` removes the unpublished staging dir.
            return Err(SoldrError::Other(format!(
                "install: git {args:?} for {clone_url} failed with status {status}"
            )));
        }
    }

    Ok(slot.publish()?.join("checkout"))
}

/// A codeload extraction yields a single `repo-<sha>/` dir; a resumed
/// cache dir needs that dir re-discovered.
fn single_crate_root(cache_dir: &Path) -> Result<PathBuf, SoldrError> {
    // Prefer a `checkout` subdir (shallow clone), else the single wrapped
    // codeload dir, else the cache dir itself if it holds a Cargo.toml.
    let checkout = cache_dir.join("checkout");
    if checkout.join("Cargo.toml").is_file() {
        return Ok(checkout);
    }
    if cache_dir.join("Cargo.toml").is_file() {
        return Ok(cache_dir.to_path_buf());
    }
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(cache_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && entry.file_name() != ".last-use" {
            dirs.push(entry.path());
        }
    }
    dirs.retain(|d| d.file_name().map(|n| n != "checkout").unwrap_or(true));
    match dirs.len() {
        1 => Ok(dirs.remove(0)),
        _ => Err(SoldrError::Other(format!(
            "install: could not locate crate root in cached source at {}",
            cache_dir.display()
        ))),
    }
}

/// Build `source_dir` with `cargo install --path` into a staging root and
/// return the produced binary path. Routes rustc through soldr's
/// compile-cache wrapper (like `build-from-source`).
pub(crate) fn cargo_install_from_path(
    source_dir: &Path,
    resolved: &ResolvedInstall,
    staging_root: &Path,
) -> Result<PathBuf, SoldrError> {
    let cargo = resolve_toolchain_binary("cargo")?;
    std::fs::create_dir_all(staging_root)?;
    let staging_bin = staging_root.join("bin");

    let mut command = std::process::Command::new(&cargo);
    command
        .arg("install")
        .arg("--path")
        .arg(source_dir)
        .arg("--root")
        .arg(staging_root)
        .arg("--force");
    if resolved.locked {
        command.arg("--locked");
    }
    if resolved.debug {
        command.arg("--debug");
    }
    for bin in &resolved.bins {
        command.arg("--bin").arg(bin);
    }
    if !resolved.features.is_empty() {
        command.arg("--features").arg(resolved.features.join(","));
    }
    command
        .arg("--target")
        .arg(&resolved.triple)
        // Neutral working dir + scrub inherited wrappers, exactly like
        // build-from-source, before opting back into the cache wrapper.
        .current_dir(source_dir)
        .env_remove("MAKEFLAGS")
        .env_remove("CARGO_MAKEFLAGS")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER");
    apply_source_build_cache_wrapper(&mut command);
    suppress_windows_console_window(&mut command);

    let status = crate::exit_guard::run_child_command(
        &mut command,
        &format!("install: cargo install --path {}", source_dir.display()),
        "install",
        InstallerWatchdogConfig::from_env(INSTALL_TIMEOUT_ENV_VAR),
    )?;
    if !status.success() {
        return Err(SoldrError::Other(format!(
            "install: cargo install --path {} failed with status {status}",
            source_dir.display()
        )));
    }

    // Find the produced binary. When `--bin` narrowed it, prefer that name;
    // otherwise take the tool's inferred name, else the first binary present.
    let ext = binary_ext_for_triple(&resolved.triple);
    let mut candidates: Vec<String> = Vec::new();
    if let Some(first) = resolved.bins.first() {
        candidates.push(format!("{first}{ext}"));
    }
    candidates.push(format!("{}{ext}", resolved.name));
    for name in &candidates {
        let p = staging_bin.join(name);
        if p.is_file() {
            return Ok(p);
        }
    }
    // Fall back to the single binary cargo produced.
    let produced: Vec<PathBuf> = std::fs::read_dir(&staging_bin)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default();
    match produced.len() {
        1 => Ok(produced.into_iter().next().unwrap()),
        0 => Err(SoldrError::Other(format!(
            "install: cargo install produced no binary in {}",
            staging_bin.display()
        ))),
        _ => Err(SoldrError::Other(format!(
            "install: cargo install produced multiple binaries in {}; pass --bin <name>",
            staging_bin.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved_local(path: &str) -> ResolvedInstall {
        ResolvedInstall {
            name: "foo".into(),
            target: InstallTarget::Local(PathBuf::from(path)),
            git_ref: Ref::Head,
            sha: String::new(),
            release: None,
            release_note: None,
            form: Form::Auto,
            triple: "x86_64-unknown-linux-gnu".into(),
            debug: false,
            bins: vec![],
            features: vec![],
            locked: false,
            install_root: PathBuf::from("/tmp/installed"),
        }
    }

    #[test]
    fn plan_local_path_is_local_lane() {
        let r = resolved_local(".");
        assert!(matches!(
            plan_acquisition(&r, None).unwrap(),
            AcquisitionPlan::LocalPath(_)
        ));
    }

    #[test]
    fn plan_github_with_sha_is_codeload() {
        let mut r = resolved_local(".");
        r.target = InstallTarget::GitHub {
            host: "github.com".into(),
            owner: "zackees".into(),
            repo: "clud".into(),
            url_ref: None,
            url_release: None,
            run_id: None,
        };
        r.sha = "9f2c1ab3".into();
        match plan_acquisition(&r, None).unwrap() {
            AcquisitionPlan::CodeloadZip { url, .. } => {
                assert!(
                    url.contains("codeload.github.com/zackees/clud/zip/9f2c1ab3"),
                    "{url}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn plan_non_github_is_shallow_clone() {
        let mut r = resolved_local(".");
        r.target = InstallTarget::GitHub {
            host: "gitlab.com".into(),
            owner: "o".into(),
            repo: "r".into(),
            url_ref: None,
            url_release: None,
            run_id: None,
        };
        r.sha = "abc".into();
        assert!(matches!(
            plan_acquisition(&r, None).unwrap(),
            AcquisitionPlan::ShallowClone { .. }
        ));
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// soldr#3690: `--rev <sha>` on the ShallowClone lane must check out
    /// exactly that commit; `git clone --branch <sha>` rejects a sha.
    #[test]
    fn shallow_clone_rev_checks_out_exact_sha() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("Cargo.toml"), "[package]\nname = \"foo\"\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "first"]);
        let first = git(&work, &["rev-parse", "HEAD"]);
        std::fs::write(work.join("second.txt"), "2").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "second"]);
        let bare = tmp.path().join("bare.git");
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        let url = format!("file://{}", bare.display());

        let root = tmp.path().join("soldr-home");
        let paths = SoldrPaths::with_root(root);
        paths.ensure_dirs().unwrap();
        let mut r = resolved_local(".");
        r.target = InstallTarget::GitHub {
            host: "git.example.com".into(),
            owner: "o".into(),
            repo: "r".into(),
            url_ref: None,
            url_release: None,
            run_id: None,
        };
        r.git_ref = Ref::Rev(first.clone());
        r.sha = first.clone();

        let checkout = acquire_shallow_clone(&paths, &r, &url).expect("rev clone");
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), first);
        assert!(checkout.join("Cargo.toml").is_file());
        assert!(!checkout.join("second.txt").exists());
    }
}
