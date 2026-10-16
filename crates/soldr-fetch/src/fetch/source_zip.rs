//! Source-tree acquisition via GitHub codeload zips (soldr#2310, Phase 1).
//!
//! Unlike [`super::archive`], which is binary-name oriented and writes a
//! single named binary into `paths.bin/<name>-<ver>/`, this module streams
//! a `codeload.github.com/<o>/<r>/zip/<ref>` archive — a *whole source
//! tree* — and unpacks every entry into a destination directory so the
//! result can be built with `cargo install --path <extracted>`.
//!
//! codeload archives are dynamically generated: no `Content-Length`, no
//! `Range`, so this is a single streamed GET (aria2-style multi-connection
//! cannot help — confirmed via aria2#2070/#2197). Bytes are streamed to a
//! temp file first (reusing the bounded-memory idle/safety watchdogs in
//! [`super::stream_download`]) and only then unzipped.

use std::path::{Path, PathBuf};

use crate::core::SoldrError;

use super::stream_download::{
    asset_http_client, get_request, send_asset_request, stream_response_to_temp_file,
    ASSET_HEADER_TIMEOUT, ASSET_IDLE_TIMEOUT,
};

/// Result of a source-zip acquisition.
#[derive(Debug, Clone)]
pub struct ExtractedSource {
    /// The repository root inside `dest_dir` — codeload zips wrap the
    /// whole tree in a single top-level `repo-<ref>/` directory, so this
    /// is that directory (the one holding `Cargo.toml`), not `dest_dir`.
    pub root: PathBuf,
    /// sha256 of the downloaded zip bytes (the acquisition pin).
    pub sha256: String,
    /// Size of the downloaded zip in bytes.
    pub bytes: u64,
}

/// Stream a codeload zip at `url` and extract its whole tree into
/// `dest_dir`, returning the repository root directory inside it.
///
/// Callers should obtain `token` from
/// `crate::fetch::github::github_auth_token_for(owner, repo)` so Actions
/// tokens are never sent to foreign repos.
///
/// `token`, when present, is attached as a bearer credential so private
/// repositories resolve. A 404/403 with no token is surfaced with a
/// directive to set `GITHUB_TOKEN` rather than a bare "not found".
pub async fn stream_and_extract_source_zip(
    url: &str,
    dest_dir: &Path,
    token: Option<&str>,
) -> Result<ExtractedSource, SoldrError> {
    let client = asset_http_client("install source zip download")?;
    let mut request = get_request(&client, url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = send_asset_request(request, url, ASSET_HEADER_TIMEOUT).await?;

    let status = response.status();
    if !status.is_success() {
        if (status.as_u16() == 404 || status.as_u16() == 403) && token.is_none() {
            return Err(SoldrError::Network(format!(
                "source zip {url} returned HTTP {status}. If this is a private repo, set \
                 GITHUB_TOKEN (or GH_TOKEN / SOLDR_GITHUB_TOKEN) and retry."
            )));
        }
        return Err(SoldrError::Network(format!(
            "source zip {url} failed: HTTP {status}"
        )));
    }

    let downloaded = stream_response_to_temp_file(response, url, ASSET_IDLE_TIMEOUT).await?;
    let sha256 = downloaded.sha256().to_string();
    let bytes = downloaded.bytes();

    std::fs::create_dir_all(dest_dir).map_err(SoldrError::Io)?;
    let file = std::fs::File::open(downloaded.path())?;
    extract_zip_tree(file, dest_dir)?;
    let root = single_top_level_dir(dest_dir)?;

    Ok(ExtractedSource {
        root,
        sha256,
        bytes,
    })
}

/// Extract every file entry of a zip into `dest_dir`, preserving the
/// archive's internal directory structure (guarding against zip-slip).
fn extract_zip_tree<R: std::io::Read + std::io::Seek>(
    reader: R,
    dest_dir: &Path,
) -> Result<(), SoldrError> {
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| SoldrError::Archive(e.to_string()))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| SoldrError::Archive(e.to_string()))?;

        // Reject absolute paths / `..` traversal (zip-slip). `enclosed_name`
        // returns `None` for anything that would escape the destination.
        let Some(relative) = entry.enclosed_name() else {
            return Err(SoldrError::Archive(format!(
                "source zip entry has an unsafe path: {}",
                String::from_utf8_lossy(entry.name_raw())
            )));
        };
        let out_path = dest_dir.join(&relative);

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&out_path)?;
        std::io::copy(&mut entry, &mut out)?;

        {
            if let Some(mode) = entry.unix_mode() {
                crate::platform::fs::permissions::restore_mode(&out_path, Some(mode))?;
            }
        }
    }
    Ok(())
}

/// codeload zips wrap the tree in exactly one top-level directory. Return
/// it, or an error if the archive shape is unexpected.
fn single_top_level_dir(dest_dir: &Path) -> Result<PathBuf, SoldrError> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(dest_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    match dirs.len() {
        1 => Ok(dirs.remove(0)),
        n => Err(SoldrError::Archive(format!(
            "expected a single top-level directory in the source zip, found {n} in {}",
            dest_dir.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut zip = zip::ZipWriter::new(cursor);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, data) in entries {
                zip.start_file(*name, opts).unwrap();
                zip.write_all(data).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn extract_zip_tree_writes_full_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = make_zip(&[
            ("repo-abc123/Cargo.toml", b"[package]"),
            ("repo-abc123/src/main.rs", b"fn main() {}"),
        ]);
        extract_zip_tree(std::io::Cursor::new(zip), tmp.path()).unwrap();
        let root = single_top_level_dir(tmp.path()).unwrap();
        assert!(root.join("Cargo.toml").is_file());
        assert!(root.join("src/main.rs").is_file());
    }

    #[test]
    fn extract_zip_tree_rejects_zip_slip() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = make_zip(&[("../escape.txt", b"pwned")]);
        let err = extract_zip_tree(std::io::Cursor::new(zip), tmp.path())
            .expect_err("zip-slip must be rejected");
        assert!(format!("{err}").contains("unsafe path"), "{err}");
    }

    #[test]
    fn single_top_level_dir_requires_exactly_one() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("a")).unwrap();
        std::fs::create_dir(tmp.path().join("b")).unwrap();
        assert!(single_top_level_dir(tmp.path()).is_err());
    }
}
