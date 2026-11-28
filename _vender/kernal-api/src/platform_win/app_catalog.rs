//! Start Menu shortcuts, launched and opened through `ShellExecuteW`.
use crate::app_catalog::{AppEntry, LaunchError, OpenOptions};
use std::os::windows::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

fn start_menus() -> Vec<PathBuf> {
    ["APPDATA", "ProgramData"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|root| Path::new(&root).join(r"Microsoft\Windows\Start Menu\Programs"))
        .collect()
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: u8) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && depth < 4 {
            walk(&path, out, depth + 1);
        } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) {
            out.push(path);
        }
    }
}

pub(crate) async fn list_apps() -> Vec<AppEntry> {
    tokio::task::spawn_blocking(|| {
        let mut files = Vec::new();
        for root in start_menus() {
            walk(&root, &mut files, 0);
        }
        files
            .into_iter()
            .filter_map(|path| {
                let name = path.file_stem()?.to_str()?.to_owned();
                // Uninstallers and readmes are shortcuts too; they are not applications.
                let lower = name.to_lowercase();
                if lower.contains("uninstall") || lower.contains("readme") {
                    return None;
                }
                Some(AppEntry {
                    id: path.to_string_lossy().into_owned(),
                    name,
                    keywords: Vec::new(),
                    icon: None,
                    target: path,
                })
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}

fn shell_open(target: &std::ffi::OsStr, parameters: Option<&std::ffi::OsStr>) -> Result<(), LaunchError> {
    let verb = wide("open".as_ref());
    let file = wide(target);
    let parameters = parameters.map(wide);
    // SAFETY: every string is a NUL-terminated UTF-16 buffer alive for the call.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            parameters.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // Values above 32 mean success; 2 and 3 are file/path not found.
    match result as isize {
        r if r > 32 => Ok(()),
        2 | 3 => Err(LaunchError::NotFound),
        _ => Err(LaunchError::Failed),
    }
}

pub(crate) async fn launch(app: &AppEntry) -> Result<(), LaunchError> {
    shell_open(app.target.as_os_str(), None)
}

pub(crate) async fn open_uri(uri: &str) -> Result<(), LaunchError> {
    shell_open(uri.as_ref(), None)
}

/// The shortcut runs with the URI, quoted, as its only argument.
pub(crate) async fn open_uri_with(app: &AppEntry, uri: &str, _options: OpenOptions) -> Result<(), LaunchError> {
    // check_uri refused control characters; a quote would end the argument early.
    if uri.contains('"') {
        return Err(LaunchError::InvalidInput);
    }
    shell_open(app.target.as_os_str(), Some(format!("\"{uri}\"").as_ref()))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn the_runner_ships_applications() {
        assert!(!super::list_apps().await.is_empty());
    }
}
