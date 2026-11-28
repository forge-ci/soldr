//! Installed applications, launching them, and opening URIs with the user's default handler.
//!
//! Linux reads XDG `.desktop` entries, Windows the Start Menu shortcuts, macOS the `.app`
//! bundles in the standard Applications folders. Entries carry an opaque target the facade
//! hands back to the same host on [`launch`], never a parsed command line.

use crate::native_app_catalog;
use std::path::PathBuf;

/// One launchable application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppEntry {
    /// Stable identity: the desktop-file id, shortcut path, or bundle path.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Extra search terms (XDG `Keywords`, `GenericName`).
    pub keywords: Vec<String>,
    /// Icon name or path, when the host provides one.
    pub icon: Option<String>,
    pub(crate) target: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchError {
    InvalidInput,
    NotFound,
    Failed,
}

/// Every visible application, sorted by name, first definition of each id winning.
pub async fn list_apps() -> Vec<AppEntry> {
    let mut apps = native_app_catalog::list_apps().await;
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

/// Start `app` detached from the caller; returns once the process was spawned.
pub async fn launch(app: &AppEntry) -> Result<(), LaunchError> {
    native_app_catalog::launch(app).await
}

/// How [`open_uri_with`] hands the URI over.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenOptions {
    /// Ask for a new window. Honored where the app declares one: on Linux an entry's
    /// `[Desktop Action new-window]`; Windows and macOS open the URI in the app as it chooses.
    pub new_window: bool,
}

fn check_uri(uri: &str) -> Result<(), LaunchError> {
    let allowed = ["http://", "https://", "file://", "mailto:"];
    if uri.len() > 8192
        || uri.chars().any(char::is_control)
        || !allowed.iter().any(|p| {
            uri.get(..p.len())
                .is_some_and(|s| s.eq_ignore_ascii_case(p))
        })
    {
        return Err(LaunchError::InvalidInput);
    }
    Ok(())
}

/// Open an `http`, `https`, `file` or `mailto` URI with the user's default handler.
pub async fn open_uri(uri: &str) -> Result<(), LaunchError> {
    check_uri(uri)?;
    native_app_catalog::open_uri(uri).await
}

/// Open the same URIs as [`open_uri`] with `app` instead of the default handler.
pub async fn open_uri_with(
    app: &AppEntry,
    uri: &str,
    options: OpenOptions,
) -> Result<(), LaunchError> {
    check_uri(uri)?;
    native_app_catalog::open_uri_with(app, uri, options).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_uri_rejects_other_schemes_before_touching_the_host() {
        for bad in [
            "javascript:alert(1)",
            "/etc/passwd",
            "ftp://x",
            "https://a\nb",
            "",
        ] {
            assert_eq!(open_uri(bad).await, Err(LaunchError::InvalidInput), "{bad}");
        }
    }

    #[tokio::test]
    async fn open_uri_with_applies_the_same_scheme_allowlist() {
        let app = AppEntry {
            id: "x".into(),
            name: "x".into(),
            keywords: Vec::new(),
            icon: None,
            target: PathBuf::from("/nonexistent"),
        };
        for bad in ["javascript:alert(1)", "ftp://x", "https://a\nb"] {
            assert_eq!(
                open_uri_with(&app, bad, OpenOptions::default()).await,
                Err(LaunchError::InvalidInput),
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn the_host_lists_applications() {
        let apps = list_apps().await;
        assert!(apps
            .windows(2)
            .all(|w| w[0].name.to_lowercase() <= w[1].name.to_lowercase()));
    }
}
