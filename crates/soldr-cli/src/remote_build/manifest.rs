use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};

const MAX_BYTES: usize = 4096;
const GITHUB_PREFIX: &str = "https://github.com/";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    github: Github,
    user_id: u64,
    #[serde(rename = "parent-commit-hash")]
    parent_commit: Option<String>,
    #[serde(
        rename = "parent-branch",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    parent_branch: Option<String>,
    #[serde(rename = "last-commit-hash")]
    last_commit: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Github {
    url: String,
}

impl Manifest {
    pub(super) fn repository(&self) -> String {
        let url = self.github.url.to_ascii_lowercase();
        url.strip_suffix(".git").unwrap_or(&url).to_owned()
    }

    pub(super) fn user(&self) -> String {
        self.user_id.to_string()
    }

    pub(super) fn repository_slug(&self) -> String {
        let repository = self.repository();
        repository[GITHUB_PREFIX.len()..].to_owned()
    }

    pub(super) fn branch_component(&self) -> String {
        self.parent_branch
            .as_deref()
            .unwrap_or("unknown")
            .bytes()
            .map(|byte| {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.') {
                    char::from(byte).to_string()
                } else {
                    format!("%{byte:02X}")
                }
            })
            .collect()
    }

    pub(super) fn commit(&self) -> &str {
        &self.last_commit
    }

    pub(super) fn parent(&self) -> Option<&str> {
        self.parent_commit.as_deref()
    }
}

pub(super) fn read(path: Option<&Path>, commit: &str) -> io::Result<Option<Manifest>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    decode(Some(&bytes), commit)
}

pub(super) fn decode(bytes: Option<&[u8]>, commit: &str) -> io::Result<Option<Manifest>> {
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    if bytes.len() > MAX_BYTES {
        return Err(io::Error::other("remote session manifest exceeds 4 KiB"));
    }
    let manifest: Option<Manifest> = serde_json::from_slice(bytes).map_err(io::Error::other)?;
    if let Some(manifest) = &manifest {
        if manifest.last_commit != commit {
            return Err(io::Error::other(
                "remote manifest last-commit-hash does not match the source commit",
            ));
        }
        if manifest.parent().is_some_and(|hash| {
            hash.len() != commit.len()
                || !hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        }) {
            return Err(io::Error::other(
                "invalid remote manifest parent-commit-hash",
            ));
        }
        validate_repository(&manifest.github.url)?;
        if !matches!(commit.len(), 40 | 64)
            || !commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(io::Error::other("invalid remote manifest last-commit-hash"));
        }
        if manifest.parent_branch.as_ref().is_some_and(|branch| {
            branch.is_empty()
                || branch.chars().any(char::is_control)
                || manifest.branch_component().len() > 128
        }) {
            return Err(io::Error::other(
                "invalid or oversized remote manifest parent-branch",
            ));
        }
    }
    Ok(manifest)
}

fn validate_repository(url: &str) -> io::Result<()> {
    let path = url.strip_prefix(GITHUB_PREFIX).unwrap_or_default();
    let valid_component = |component: &str| {
        !matches!(component, "" | "." | "..")
            && component
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    };
    if path.split_once('/').is_some_and(|(owner, repo)| {
        valid_component(owner)
            && valid_component(
                repo.to_ascii_lowercase()
                    .strip_suffix(".git")
                    .unwrap_or(repo),
            )
    }) {
        Ok(())
    } else {
        Err(io::Error::other(
            "remote manifest github.url must be an HTTPS github.com repository URL",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_null_and_rejects_a_manifest_for_a_different_commit() {
        let commit = "a".repeat(40);
        assert!(decode(None, &commit).unwrap().is_none());
        assert!(decode(Some(b"null"), &commit).unwrap().is_none());
        let data = serde_json::json!({
            "github": {"url": "https://github.com/forge-ci/soldr"},
            "user_id": 1234,
            "parent-commit-hash": "b".repeat(40),
            "last-commit-hash": commit,
        });
        let bytes = serde_json::to_vec(&data).unwrap();
        let manifest = decode(Some(&bytes), &commit).unwrap().unwrap();
        assert_eq!(manifest.user_id, 1234);
        assert_eq!(serde_json::to_value(manifest).unwrap(), data);
        assert!(decode(Some(&bytes), &"c".repeat(40)).is_err());
    }

    #[test]
    fn only_accepts_github_repository_urls() {
        assert!(validate_repository("https://github.com/forge-ci/soldr.git").is_ok());
        assert!(validate_repository("https://github.com/forge-ci/../soldr").is_err());
        assert!(validate_repository("https://github.com.evil.invalid/forge-ci/soldr").is_err());
        assert!(validate_repository("https://github.com/forge-ci/soldr?x=y").is_err());
        assert!(validate_repository("https://github.com/forge-ci/...git").is_err());
    }
}
