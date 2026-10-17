//! Warn when Git is configured to materialize CRLF working-tree files.
//!
//! CRLF is valid source, but flipping between LF and CRLF changes compiler
//! input bytes and defeats cross-platform cache sharing. The check reads Git
//! configuration once per human-facing Soldr process; it never walks source
//! files and never runs in the rustc-wrapper hot path.

use crate::core::git::CrlfCheckoutSetting;
use crate::platform::sync::Mutex;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

static REPO_RESULTS: LazyLock<Mutex<BTreeMap<PathBuf, Option<CrlfCheckoutSetting>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

pub(super) fn maybe_emit_crlf_warning(repo_root: &Path) {
    let Some(setting) = crlf_setting_to_warn_once(repo_root) else {
        return;
    };
    eprintln!(
        "{}",
        crlf_warning_message(setting, repo_root, crate::color_choice::stderr_enabled())
    );
}

fn crlf_setting_to_warn_once(repo_root: &Path) -> Option<CrlfCheckoutSetting> {
    let key = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let mut results = REPO_RESULTS.lock();
    if results.contains_key(&key) {
        return None;
    }
    let setting = crate::core::git::crlf_checkout_setting(repo_root);
    results.insert(key, setting);
    setting
}

fn crlf_warning_message(setting: CrlfCheckoutSetting, repo_root: &Path, use_color: bool) -> String {
    let (configured_by, remediation) = match setting {
        CrlfCheckoutSetting::AutoCrlf => (
            "core.autocrlf=true",
            "`git config --local core.autocrlf input`",
        ),
        CrlfCheckoutSetting::CoreEol => ("core.eol=crlf", "`git config --local core.eol lf`"),
    };
    let warning = crate::color_choice::paint("warning", crate::color_choice::YELLOW, use_color);
    format!(
        "soldr: {warning}: Git CRLF checkout mode is enabled by `{configured_by}` for {}; \
         CRLF source bytes can cause avoidable recompiles and cross-platform cache misses. \
         Prefer {remediation} or enforce `* text=auto eol=lf` in `.gitattributes`.",
        repo_root.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo_with_autocrlf(value: &str) -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("tempdir");
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["init", "-q"])
            .status()
            .expect("git init");
        assert!(init.success());
        let config = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["config", "--local", "core.autocrlf", value])
            .status()
            .expect("git config");
        assert!(config.success());
        repo
    }

    #[test]
    fn crlf_probe_runs_once_for_lexical_aliases_of_a_repo() {
        let repo = init_repo_with_autocrlf("true");
        let nested = repo.path().join("nested");
        std::fs::create_dir(&nested).expect("nested directory");

        assert_eq!(
            crlf_setting_to_warn_once(repo.path()),
            Some(CrlfCheckoutSetting::AutoCrlf)
        );
        assert_eq!(crlf_setting_to_warn_once(&nested.join("..")), None);
    }

    #[test]
    fn no_warning_probe_result_is_cached_for_the_process() {
        let repo = init_repo_with_autocrlf("input");
        assert_eq!(crlf_setting_to_warn_once(repo.path()), None);

        let config = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["config", "--local", "core.autocrlf", "true"])
            .status()
            .expect("git config");
        assert!(config.success());
        assert_eq!(crlf_setting_to_warn_once(repo.path()), None);
    }

    #[test]
    fn crlf_warning_is_yellow_when_color_is_enabled() {
        let warning = crlf_warning_message(CrlfCheckoutSetting::AutoCrlf, Path::new("repo"), true);

        assert!(warning.contains("\x1b[33mwarning\x1b[0m"));
        assert!(warning.contains("`core.autocrlf=true`"));
        assert!(warning.contains("avoidable recompiles"));
    }

    #[test]
    fn core_eol_warning_names_its_specific_remediation() {
        let warning = crlf_warning_message(CrlfCheckoutSetting::CoreEol, Path::new("repo"), false);

        assert!(!warning.contains("\x1b["));
        assert!(warning.contains("`core.eol=crlf`"));
        assert!(warning.contains("`git config --local core.eol lf`"));
    }
}
