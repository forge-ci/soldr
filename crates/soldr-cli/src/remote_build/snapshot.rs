use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{ambient_authority, fs};
use kernal_api::hash::Sha256Hasher;

use crate::platform::wait;

use super::wire::Source;

pub(super) struct Snapshot {
    pub commit: String,
    pub files: Vec<Source>,
    pub blobs: BTreeMap<String, Vec<u8>>,
}

fn git(directory: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let result = wait::output_for(git_command(directory).args(args), wait::PROBE)?;
    if !result.status.success() {
        return Err(io::Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&result.stderr).trim()
        )));
    }
    Ok(result.stdout)
}

fn git_command(directory: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(directory)
        .env("GIT_NO_REPLACE_OBJECTS", "1");
    command
}

fn utf8(bytes: &[u8]) -> io::Result<&str> {
    std::str::from_utf8(bytes).map_err(io::Error::other)
}

pub(super) fn verify_parent(
    directory: &Path,
    commit: &str,
    expected: Option<&str>,
) -> io::Result<()> {
    // Read the object itself so a shallow boundary cannot hide its parent.
    let object = git(directory, &["cat-file", "commit", commit])?;
    let parent = object
        .split(|byte| *byte == b'\n')
        .take_while(|line| !line.is_empty())
        .find_map(|line| line.strip_prefix(b"parent "))
        .map(utf8)
        .transpose()?;
    if parent != expected {
        return Err(io::Error::other(
            "remote manifest parent-commit-hash does not match the first Git parent",
        ));
    }
    Ok(())
}

pub(super) fn digest(bytes: &[u8]) -> String {
    Sha256Hasher::digest(bytes).to_hex()
}

pub(super) fn relative_path(path: &str) -> io::Result<&Path> {
    let parsed = Path::new(path);
    if path.is_empty()
        || path.contains(['\\', ':', '\0'])
        || parsed.components().any(
            |part| !matches!(part, Component::Normal(name) if !name.eq_ignore_ascii_case(".git")),
        )
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(io::Error::other(format!("invalid remote path {path:?}")));
    }
    Ok(parsed)
}

pub(super) fn capture(directory: &Path) -> io::Result<Snapshot> {
    let commit = git(directory, &["rev-parse", "--verify", "HEAD^{commit}"])
        .map_err(|_| io::Error::other("Remote build requires a commit. Create a commit and tidy up your repository before retrying."))?;
    let commit = utf8(&commit)?.trim().to_owned();
    let root = git(directory, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(utf8(&root)?.trim_end_matches(['\r', '\n']));
    let status = git(
        &root,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?;
    if !status.is_empty() {
        return Err(io::Error::other(
            "Remote build requires a clean repository. Create a commit and tidy up your repository before retrying.",
        ));
    }

    let tree = git(&root, &["ls-tree", "-r", "-z", "--full-tree", &commit])?;
    let mut reader = running_process::spawn(
        git_command(&root).args(["cat-file", "--batch"]),
        running_process::SpawnStdio {
            stdin: running_process::StdioSource::Pipe,
            stdout: running_process::StdioSource::Pipe,
            stderr: running_process::StdioSource::Parent,
            drain_timeout: Some(wait::HELPER_JOIN),
            show_console: false,
        },
    )?;
    let input = reader
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing Git input"))?;
    let output = reader
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing Git output"))?;
    let snapshot = read_tree(commit, &tree, input, BufReader::new(output));
    let status = reader.wait()?;
    let snapshot = snapshot?;
    if status != 0 {
        return Err(io::Error::other(
            "git cat-file failed while reading the commit",
        ));
    }
    Ok(snapshot)
}

pub(super) fn worktree_root(directory: &Path) -> io::Result<PathBuf> {
    let root = git(directory, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(utf8(&root)?.trim_end_matches(['\r', '\n'])))
}

/// Snapshot the actual files Cargo sees without modifying the Git index.
pub(super) fn capture_worktree(directory: &Path) -> io::Result<Snapshot> {
    let commit = git(directory, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let commit = utf8(&commit)?.trim().to_owned();
    let root = worktree_root(directory)?;
    let listing = git(
        &root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let directory = fs::Dir::open_ambient_dir(&root, ambient_authority())?;
    let mut files = Vec::new();
    let mut blobs = BTreeMap::new();
    let mut remaining = 1024 * 1024 * 1024;
    let paths: BTreeSet<_> = listing
        .split(|byte| *byte == 0)
        .filter(|p| !p.is_empty())
        .collect();
    for name in paths {
        let name = utf8(name)?;
        let path = relative_path(name)?;
        let metadata = match directory.symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let symlink = metadata.is_symlink();
        let data = if symlink {
            let target = directory.read_link_contents(path)?;
            let target = target
                .to_str()
                .ok_or_else(|| io::Error::other("symlink target must be UTF-8"))?;
            relative_path(target)?;
            target.as_bytes().to_vec()
        } else if metadata.is_file() {
            read_worktree_file(&directory, path, remaining.min(super::wire::MAX_FRAME))?
        } else {
            return Err(io::Error::other(format!(
                "unsupported worktree file {name:?}"
            )));
        };
        remaining = remaining
            .checked_sub(data.len())
            .ok_or_else(|| io::Error::other("remote worktree exceeds its total size limit"))?;
        let hash = digest(&data);
        blobs.entry(hash.clone()).or_insert(data);
        files.push(Source {
            path: name.to_owned(),
            digest: hash,
            executable: !symlink
                && crate::platform::fs::permissions::mode(&root.join(path))
                    .is_some_and(|mode| mode & 0o111 != 0),
            symlink,
        });
    }
    Ok(Snapshot {
        commit,
        files,
        blobs,
    })
}

fn read_worktree_file(directory: &fs::Dir, path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let mut options = fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory.open_with(path, &options)?;
    let mut data = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut data)?;
    if data.len() > limit {
        return Err(io::Error::other(
            "remote worktree file exceeds its size limit",
        ));
    }
    Ok(data)
}

fn read_tree(
    commit: String,
    tree: &[u8],
    mut input: impl Write,
    mut output: impl BufRead,
) -> io::Result<Snapshot> {
    let mut files = Vec::new();
    let mut blobs = BTreeMap::new();
    let mut objects = BTreeMap::<String, String>::new();
    for entry in tree
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let (metadata, path) = utf8(entry)?
            .split_once('\t')
            .ok_or_else(|| io::Error::other("invalid git tree record"))?;
        relative_path(path)?;
        let mut fields = metadata.split_whitespace();
        let mode = fields.next();
        if !matches!(mode, Some("100644" | "100755" | "120000")) || fields.next() != Some("blob") {
            return Err(io::Error::other(format!(
                "unsupported Git file mode for {path}"
            )));
        }
        let object = fields
            .next()
            .ok_or_else(|| io::Error::other("missing Git object ID"))?;
        let hash = if let Some(hash) = objects.get(object) {
            hash.clone()
        } else {
            let data = read_blob(object, &mut input, &mut output)?;
            let hash = digest(&data);
            objects.insert(object.to_owned(), hash.clone());
            blobs.insert(hash.clone(), data);
            hash
        };
        files.push(Source {
            path: path.to_owned(),
            digest: hash,
            executable: mode == Some("100755"),
            symlink: mode == Some("120000"),
        });
    }
    Ok(Snapshot {
        commit,
        files,
        blobs,
    })
}

fn read_blob(object: &str, mut input: impl Write, mut output: impl BufRead) -> io::Result<Vec<u8>> {
    writeln!(input, "{object}")?;
    input.flush()?;
    let mut header = String::new();
    output.read_line(&mut header)?;
    let mut parts = header.split_whitespace();
    if parts.next() != Some(object) || parts.next() != Some("blob") {
        return Err(io::Error::other("unexpected Git blob header"));
    }
    let size: usize = parts
        .next()
        .ok_or_else(|| io::Error::other("missing Git blob length"))?
        .parse()
        .map_err(io::Error::other)?;
    if size > super::wire::MAX_FRAME {
        return Err(io::Error::other(
            "committed file exceeds the remote build message limit",
        ));
    }
    let mut data = vec![0; size];
    output.read_exact(&mut data)?;
    let mut terminator = [0];
    output.read_exact(&mut terminator)?;
    if terminator != *b"\n" {
        return Err(io::Error::other("missing Git blob terminator"));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_worktree_snapshot_captures_edits_and_excludes_ignored_files() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        git(root, &["init"]).unwrap();
        std::fs::write(root.join(".gitignore"), "/ignored\n").unwrap();
        std::fs::write(root.join("edited"), "old edit").unwrap();
        std::fs::write(root.join("deleted"), "old deletion").unwrap();
        git(root, &["add", "."]).unwrap();
        git(
            root,
            &[
                "-c",
                "user.name=Soldr Test",
                "-c",
                "user.email=soldr-test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "initial",
            ],
        )
        .unwrap();
        std::fs::write(root.join("edited"), "new edit").unwrap();
        std::fs::rename(root.join("deleted"), root.join("ignored")).unwrap();
        std::fs::write(root.join("added"), "new file").unwrap();
        std::fs::write(root.join("ignored"), "private").unwrap();
        let index_before = git(root, &["ls-files", "--stage"]).unwrap();

        let snapshot = capture_worktree(root).unwrap();
        let sources: BTreeMap<_, _> = snapshot
            .files
            .iter()
            .map(|source| {
                (
                    source.path.as_str(),
                    snapshot.blobs[&source.digest].as_slice(),
                )
            })
            .collect();
        assert_eq!(sources["edited"], b"new edit");
        assert_eq!(sources["added"], b"new file");
        assert!(!sources.contains_key("deleted"));
        assert!(!sources.contains_key("ignored"));
        assert_eq!(git(root, &["ls-files", "--stage"]).unwrap(), index_before);
        assert!(capture(root).is_err());
    }
}
