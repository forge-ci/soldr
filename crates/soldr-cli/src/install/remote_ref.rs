//! Resolve a non-GitHub ref to an immutable commit sha (soldr#3691).

use crate::core::{suppress_windows_console_window, SoldrError};

use super::refs::Ref;

/// Resolve `git_ref` on `clone_url` to the commit sha it names right now,
/// via `git ls-remote`. A `Ref::Rev` already names a commit and is returned
/// as-is. The result keys the source cache, so a moved branch, tag or HEAD
/// lands in a fresh entry instead of reusing the first commit ever cloned.
pub(crate) fn resolve_remote_sha(clone_url: &str, git_ref: &Ref) -> Result<String, SoldrError> {
    let (pattern, wanted): (String, Vec<String>) = match git_ref {
        Ref::Rev(r) => return Ok(r.clone()),
        Ref::Head => ("HEAD".into(), vec!["HEAD".into()]),
        Ref::Branch(b) => (format!("refs/heads/{b}"), vec![format!("refs/heads/{b}")]),
        // Prefer the peeled commit of an annotated tag over the tag object.
        Ref::Tag(t) => (
            format!("refs/tags/{t}*"),
            vec![format!("refs/tags/{t}^{{}}"), format!("refs/tags/{t}")],
        ),
    };
    let mut command = std::process::Command::new("git");
    command.args(["ls-remote", clone_url, &pattern]);
    // `ls-remote` needs no repository, but git still discovers one from the
    // cwd and dies (exit 128) on a dangling `.git` gitfile, e.g. a worktree
    // mounted into a container without its parent repo. `clone_url` is always
    // absolute, so a neutral cwd is safe.
    command.current_dir(std::env::temp_dir());
    suppress_windows_console_window(&mut command);
    let out = crate::platform::wait::output_for(&mut command, crate::platform::wait::NETWORK_PROBE)
        .map_err(|e| SoldrError::Other(format!("install: failed to spawn git ls-remote: {e}")))?;
    if !out.status.success() {
        return Err(SoldrError::Other(format!(
            "install: git ls-remote {clone_url} {pattern} failed with status {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let listing = String::from_utf8_lossy(&out.stdout);
    let rows: Vec<(&str, &str)> = listing
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .collect();
    wanted
        .iter()
        .find_map(|name| {
            rows.iter()
                .find(|(_, r)| r == name)
                .map(|(sha, _)| sha.to_string())
        })
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "install: {} not found on {clone_url}",
                git_ref.describe()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SoldrPaths;
    use crate::install::acquire::acquire_shallow_clone;
    use crate::install::plan::ResolvedInstall;
    use crate::install::refs::Form;
    use crate::install::target::InstallTarget;
    use std::path::{Path, PathBuf};

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

    fn commit(work: &Path, file: &str) -> String {
        std::fs::write(work.join(file), file).unwrap();
        git(work, &["add", "."]);
        git(work, &["commit", "-q", "-m", file]);
        git(work, &["rev-parse", "HEAD"])
    }

    fn resolved(git_ref: Ref, sha: String) -> ResolvedInstall {
        ResolvedInstall {
            name: "foo".into(),
            target: InstallTarget::GitHub {
                host: "git.example.com".into(),
                owner: "o".into(),
                repo: "r".into(),
                url_ref: None,
                url_release: None,
                run_id: None,
            },
            git_ref,
            sha,
            release: None,
            release_note: None,
            form: Form::Auto,
            triple: "x86_64-unknown-linux-gnu".into(),
            debug: false,
            bins: vec![],
            features: vec![],
            locked: false,
            install_root: PathBuf::from("/nonexistent"),
        }
    }

    /// soldr#3691: a second install after upstream moves must use the new
    /// commit, and the cache key must be the real sha.
    #[test]
    fn new_upstream_commit_is_used_on_reinstall() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("Cargo.toml"), "[package]\nname = \"foo\"\n").unwrap();
        let first = commit(&work, "one.txt");
        git(&work, &["tag", "-a", "v1", "-m", "v1"]);
        let bare = tmp.path().join("bare.git");
        let bare_s = bare.to_str().unwrap();
        git(
            tmp.path(),
            &["clone", "-q", "--bare", work.to_str().unwrap(), bare_s],
        );
        let url = format!("file://{}", bare.display());
        let paths = SoldrPaths::with_root(tmp.path().join("soldr-home"));
        paths.ensure_dirs().unwrap();

        let install = |git_ref: Ref| {
            let sha = resolve_remote_sha(&url, &git_ref).expect("resolve");
            let checkout = acquire_shallow_clone(&paths, &resolved(git_ref, sha.clone()), &url)
                .expect("clone");
            (sha, git(&checkout, &["rev-parse", "HEAD"]))
        };

        assert_eq!(install(Ref::Head), (first.clone(), first.clone()));
        assert_eq!(
            install(Ref::Tag("v1".into())),
            (first.clone(), first.clone())
        );

        let second = commit(&work, "two.txt");
        git(&work, &["push", "-q", bare_s, "main"]);

        assert_eq!(install(Ref::Head), (second.clone(), second.clone()));
        let branch = install(Ref::Branch("main".into()));
        assert_eq!(branch, (second.clone(), second.clone()));
        assert!(resolve_remote_sha(&url, &Ref::Branch("nope".into())).is_err());
    }
}
