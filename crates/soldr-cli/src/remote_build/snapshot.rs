use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

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

pub(super) fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
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
