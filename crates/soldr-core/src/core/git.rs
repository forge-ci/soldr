//! Lightweight `git` subprocess wrappers used by `soldr cook` to
//! detect the workspace's `origin` remote and gate cross-repo cook
//! sharing on whether `Cargo.lock` is committed and not gitignored
//! (issue #577, meta #579).
//!
//! Every helper here is best-effort — a missing `git`, a missing
//! `.git/`, or a network-less repo never bubbles into the cook
//! command's exit code. Callers branch on `None` / `false` and either
//! emit a sharing-disabled warning or quietly skip the indexing step.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Explicit Git configuration that can write CRLF into a working tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrlfCheckoutSetting {
    /// `core.autocrlf=true` converts text files to CRLF on checkout.
    AutoCrlf,
    /// `core.eol=crlf` selects CRLF for files governed by Git's text rules.
    CoreEol,
}

/// Return the effective explicit CRLF checkout setting for `workspace_root`.
///
/// This is deliberately configuration-based instead of walking tracked files:
/// the cargo front door calls it once per build, so its cost is one small Git
/// subprocess and never scales with repository size. `core.autocrlf=input`
/// wins over `core.eol=crlf` because input mode performs no checkout conversion.
pub fn crlf_checkout_setting(workspace_root: &Path) -> Option<CrlfCheckoutSetting> {
    let repo_root = find_git_worktree_root(workspace_root)?;
    // Git owns its config boolean grammar. Let it canonicalize accepted
    // spellings (including `1` and mixed case). `input` is not a boolean,
    // but it explicitly disables checkout conversion and takes precedence
    // over `core.eol`.
    let raw_autocrlf = run_git(&repo_root, ["config", "--get", "core.autocrlf"]);
    if raw_autocrlf
        .as_deref()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("input"))
    {
        return None;
    }
    if run_git(
        &repo_root,
        ["config", "--type=bool", "--get", "core.autocrlf"],
    )
    .is_some_and(|value| value.trim() == "true")
    {
        return Some(CrlfCheckoutSetting::AutoCrlf);
    }

    let eol = run_git(&repo_root, ["config", "--get", "core.eol"])?;
    eol.trim()
        .eq_ignore_ascii_case("crlf")
        .then_some(CrlfCheckoutSetting::CoreEol)
}

/// Walk up from `start` looking for a `.git` directory **or** file.
/// Git worktrees use a `.git` file that points at the real gitdir, so
/// both shapes count.  Mirrors `zccache::find_git_worktree_root`
/// but lives here so the cook surface can pull it from `core` without
/// dragging in zccache code.
pub fn find_git_worktree_root(start: &Path) -> Option<PathBuf> {
    let mut current: PathBuf = start.to_path_buf();
    loop {
        let dot_git = current.join(".git");
        if dot_git.is_dir() || dot_git.is_file() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Return the workspace's `origin` remote URL in normalized form, or
/// `None` when no `.git/` exists, no `origin` is configured, or `git`
/// is not on `PATH`.
pub fn origin_url(workspace_root: &Path) -> Option<String> {
    let repo_root = find_git_worktree_root(workspace_root)?;
    let raw = run_git(&repo_root, ["config", "--get", "remote.origin.url"])?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(normalize_origin_url(trimmed))
}

/// Return the current checked-out branch name, or `None` for detached
/// HEAD / no-git workspaces.
pub fn current_branch_name(workspace_root: &Path) -> Option<String> {
    let repo_root = find_git_worktree_root(workspace_root)?;
    let raw = run_git(&repo_root, ["branch", "--show-current"])?;
    let branch = raw.trim();
    if branch.is_empty() || branch == "HEAD" {
        return None;
    }
    Some(branch.to_string())
}

/// Branch preference list for same-origin cook fallback hydration.
///
/// Exact recipe hits do not use this. On a recipe miss, the daemon may
/// seed `target/` from a compatible same-repo artifact. Ranking prefers
/// the current branch first, then common mainline branches. This
/// captures the dominant `main -> feature` flow without network access.
pub fn branch_lineage(workspace_root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(branch) = current_branch_name(workspace_root) {
        push_unique(&mut out, branch);
    }
    for branch in ["main", "master", "trunk", "develop"] {
        push_unique(&mut out, branch.to_string());
    }
    out
}

fn push_unique(out: &mut Vec<String>, value: String) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return;
    }
    if out.iter().any(|existing| existing == trimmed) {
        return;
    }
    out.push(trimmed.to_string());
}

/// Canonicalize a git remote URL for use as a cross-repo prefetch
/// hint. The output is purely a hint — the authoritative cache key
/// is the recipe hash — so we lean conservative: best-effort lower-
/// case host, strip credentials and trailing `.git`, drop default
/// ports, and rewrite `git@host:owner/repo` SSH shorthand into the
/// equivalent `https://host/owner/repo`.
///
/// Returns the input verbatim (after trimming) when none of the
/// rules apply, so opaque shapes (`file:///`, custom schemes) round-
/// trip unchanged.
pub fn normalize_origin_url(input: &str) -> String {
    let input = input.trim();

    // `git@github.com:owner/repo.git` is not a valid URL but is the
    // dominant SSH shorthand. Rewrite to `https://github.com/owner/repo`
    // before further normalization.
    if let Some(rewritten) = rewrite_scp_style(input) {
        return normalize_via_url(&rewritten).unwrap_or(rewritten);
    }

    match normalize_via_url(input) {
        Some(s) => s,
        None => input.to_string(),
    }
}

fn rewrite_scp_style(input: &str) -> Option<String> {
    // SCP-style: `user@host:path` where `path` has no leading slash.
    // Must contain a single `@` before a `:`, the `:` cannot be part
    // of a scheme (no `://`), and the host segment cannot contain
    // path separators.
    if input.contains("://") {
        return None;
    }
    let (left, right) = input.split_once(':')?;
    if right.starts_with('/') {
        return None;
    }
    let (_user, host) = left.split_once('@')?;
    if host.is_empty() || host.contains('/') {
        return None;
    }
    let path = right.trim_start_matches('/');
    Some(format!("https://{host}/{path}"))
}

fn normalize_via_url(input: &str) -> Option<String> {
    let mut parsed = url::Url::parse(input).ok()?;
    // Strip credentials.
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    // Lowercase scheme + host. `url::Url::set_*` rejects ports for
    // some schemes; ignore those errors.
    let scheme = parsed.scheme().to_ascii_lowercase();
    if let Err(_e) = parsed.set_scheme(&scheme) {
        // `set_scheme` refuses some transitions (e.g. between special
        // and non-special schemes). Fall through with original.
    }
    if let Some(host) = parsed.host_str() {
        let lower = host.to_ascii_lowercase();
        let _ = parsed.set_host(Some(&lower));
    }
    // Drop default port.
    if let Some(p) = parsed.port() {
        let default = match parsed.scheme() {
            "http" => Some(80),
            "https" => Some(443),
            "ssh" | "git" => Some(22),
            _ => None,
        };
        if Some(p) == default {
            let _ = parsed.set_port(None);
        }
    }
    // Strip trailing `.git` and any single trailing `/`. `url` keeps
    // the path normalized but we mutate the path manually to be safe.
    let path = parsed.path().to_string();
    let mut trimmed = path.trim_end_matches('/').to_string();
    if let Some(stripped) = trimmed.strip_suffix(".git") {
        trimmed = stripped.to_string();
    }
    parsed.set_path(&trimmed);
    Some(parsed.to_string())
}

/// True when `Cargo.lock` exists at the workspace root and is tracked
/// by git. Returns `false` when there is no `.git/`, no
/// `Cargo.lock`, or `git ls-files --error-unmatch` exits non-zero.
// reason: exit status is the whole answer; `--error-unmatch` prints
// "did not match" on stderr for every untracked lockfile, which is the
// expected `false` case, not a diagnostic (soldr#3389 excluded).
#[cfg_attr(
    dylint_lib = "ban_swallowed_child_stdio",
    allow(ban_swallowed_child_stdio)
)]
pub fn cargo_lock_is_tracked(workspace_root: &Path) -> bool {
    let lockfile = workspace_root.join("Cargo.lock");
    if !lockfile.is_file() {
        return false;
    }
    if find_git_worktree_root(workspace_root).is_none() {
        return false;
    }
    let status = crate::platform::wait::status_for(
        Command::new("git")
            .arg("-C")
            .arg(workspace_root)
            .args(["ls-files", "--error-unmatch", "Cargo.lock"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
        crate::platform::wait::PROBE,
    );
    matches!(status, Ok(s) if s.success())
}

/// True when `Cargo.lock` would be ignored by git per the workspace's
/// `.gitignore` rules. `git check-ignore` exits 0 only when the path
/// matches an ignore rule (a tracked file shadows this — git treats
/// tracked files as un-ignorable — so the combination
/// `tracked + ignored` is impossible by definition). Returns `false`
/// when git is unavailable or there is no `.git/`.
// reason: `check-ignore -q` communicates only through its exit status;
// a missing git degrades to `false` by design (soldr#3389 excluded).
#[cfg_attr(
    dylint_lib = "ban_swallowed_child_stdio",
    allow(ban_swallowed_child_stdio)
)]
pub fn cargo_lock_is_gitignored(workspace_root: &Path) -> bool {
    if find_git_worktree_root(workspace_root).is_none() {
        return false;
    }
    let status = crate::platform::wait::status_for(
        Command::new("git")
            .arg("-C")
            .arg(workspace_root)
            .args(["check-ignore", "-q", "Cargo.lock"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
        crate::platform::wait::PROBE,
    );
    matches!(status, Ok(s) if s.success())
}

fn run_git<I, S>(repo_root: &Path, args: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let out = crate::platform::wait::output_for(
        Command::new("git").arg("-C").arg(repo_root).args(args),
        crate::platform::wait::PROBE,
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_local_git_config(repo: &Path, key: &str, value: &str) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["config", "--local", key, value])
            .status()
            .expect("git config");
        assert!(status.success());
    }

    fn init_test_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("tempdir");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["init", "-q"])
            .status()
            .expect("git init");
        assert!(status.success());
        repo
    }

    #[test]
    fn crlf_checkout_setting_detects_autocrlf_true() {
        let repo = init_test_repo();
        set_local_git_config(repo.path(), "core.autocrlf", "TRUE");
        set_local_git_config(repo.path(), "core.eol", "lf");

        assert_eq!(
            crlf_checkout_setting(repo.path()),
            Some(CrlfCheckoutSetting::AutoCrlf)
        );
    }

    #[test]
    fn crlf_checkout_setting_uses_git_boolean_grammar() {
        let repo = init_test_repo();
        set_local_git_config(repo.path(), "core.autocrlf", "1");
        set_local_git_config(repo.path(), "core.eol", "lf");

        assert_eq!(
            crlf_checkout_setting(repo.path()),
            Some(CrlfCheckoutSetting::AutoCrlf)
        );
    }

    #[test]
    fn crlf_checkout_setting_treats_autocrlf_input_as_lf_checkout() {
        let repo = init_test_repo();
        set_local_git_config(repo.path(), "core.autocrlf", "input");
        set_local_git_config(repo.path(), "core.eol", "CRLF");

        assert_eq!(crlf_checkout_setting(repo.path()), None);
    }

    #[test]
    fn crlf_checkout_setting_detects_explicit_core_eol_crlf() {
        let repo = init_test_repo();
        set_local_git_config(repo.path(), "core.autocrlf", "false");
        set_local_git_config(repo.path(), "core.eol", "crlf");

        assert_eq!(
            crlf_checkout_setting(repo.path()),
            Some(CrlfCheckoutSetting::CoreEol)
        );
    }

    #[test]
    fn crlf_checkout_setting_ignores_lf_configuration() {
        let repo = init_test_repo();
        set_local_git_config(repo.path(), "core.autocrlf", "false");
        set_local_git_config(repo.path(), "core.eol", "lf");

        assert_eq!(crlf_checkout_setting(repo.path()), None);
    }

    #[test]
    fn rewrite_scp_style_canonicalizes_ssh_shorthand() {
        assert_eq!(
            rewrite_scp_style("git@github.com:zackees/soldr.git"),
            Some("https://github.com/zackees/soldr.git".to_string())
        );
        assert_eq!(
            rewrite_scp_style("git@github.com:zackees/soldr"),
            Some("https://github.com/zackees/soldr".to_string())
        );
    }

    #[test]
    fn rewrite_scp_style_ignores_full_urls() {
        assert_eq!(
            rewrite_scp_style("https://github.com/zackees/soldr.git"),
            None
        );
        assert_eq!(
            rewrite_scp_style("ssh://git@github.com/zackees/soldr"),
            None
        );
    }

    #[test]
    fn normalize_strips_credentials_and_dot_git() {
        let out = normalize_origin_url("https://user:pass@GitHub.com/Owner/Repo.git");
        assert_eq!(out, "https://github.com/Owner/Repo");
    }

    #[test]
    fn normalize_drops_default_port() {
        assert_eq!(
            normalize_origin_url("https://github.com:443/zackees/soldr"),
            "https://github.com/zackees/soldr"
        );
        assert_eq!(
            normalize_origin_url("http://github.com:80/zackees/soldr"),
            "http://github.com/zackees/soldr"
        );
    }

    #[test]
    fn normalize_canonicalizes_scp_form_to_https() {
        assert_eq!(
            normalize_origin_url("git@github.com:Owner/Repo.git"),
            "https://github.com/Owner/Repo"
        );
    }

    #[test]
    fn normalize_round_trips_unknown_schemes_verbatim() {
        assert_eq!(
            normalize_origin_url("file:///srv/git/repo.git"),
            "file:///srv/git/repo"
        );
    }

    #[test]
    fn branch_lineage_dedups_current_main() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["init", "-q", "-b", "main"])
            .status()
            .expect("git init");
        assert!(status.success());

        let lineage = branch_lineage(dir.path());
        assert_eq!(lineage.first().map(String::as_str), Some("main"));
        assert_eq!(lineage.iter().filter(|b| b.as_str() == "main").count(), 1);
        assert!(lineage.iter().any(|b| b == "master"));
    }
}
