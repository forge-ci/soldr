//! XDG desktop entries (Desktop Entry Specification 1.5) and `xdg-open`.
use crate::app_catalog::{AppEntry, LaunchError, OpenOptions};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/share")));
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(system.split(':').filter(|s| !s.is_empty()).map(PathBuf::from))
        .collect()
}

/// Desktop-file id: the path under `applications/` with `/` replaced by `-`.
fn desktop_id(root: &Path, file: &Path) -> Option<String> {
    Some(file.strip_prefix(root).ok()?.to_str()?.replace('/', "-"))
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: u8) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && depth < 4 {
            walk(&path, out, depth + 1);
        } else if path.extension().is_some_and(|e| e == "desktop") {
            out.push(path);
        }
    }
}

/// The `[Desktop Entry]` group of a desktop file, when it is a visible application.
pub(crate) fn parse_entry(id: String, path: &Path, text: &str) -> Option<AppEntry> {
    let mut in_group = false;
    let (mut name, mut exec, mut icon) = (None, None, None);
    let mut keywords = Vec::new();
    let mut is_app = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        let Some((key, value)) = line.split_once('=').filter(|_| in_group) else { continue };
        match key.trim() {
            "Type" => is_app = value.trim() == "Application",
            "Name" => name = Some(value.trim().to_owned()),
            "Exec" => exec = Some(value.trim().to_owned()),
            "Icon" => icon = Some(value.trim().to_owned()),
            "GenericName" => keywords.push(value.trim().to_owned()),
            "Keywords" => keywords.extend(
                value.split(';').map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned),
            ),
            "NoDisplay" | "Hidden" if value.trim() == "true" => return None,
            _ => {}
        }
    }
    (is_app && exec.is_some()).then_some(())?;
    Some(AppEntry {
        id,
        name: name?,
        keywords,
        icon,
        target: path.to_path_buf(),
    })
}

pub(crate) async fn list_apps() -> Vec<AppEntry> {
    tokio::task::spawn_blocking(|| {
        let mut seen = HashSet::new();
        let mut apps = Vec::new();
        for root in data_dirs().into_iter().map(|d| d.join("applications")) {
            let mut files = Vec::new();
            walk(&root, &mut files, 0);
            for file in files {
                let Some(id) = desktop_id(&root, &file) else { continue };
                // Earlier data dirs shadow later ones, hidden entries included.
                if !seen.insert(id.clone()) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&file) else { continue };
                apps.extend(parse_entry(id, &file, &text));
            }
        }
        apps
    })
    .await
    .unwrap_or_default()
}

/// Split an `Exec` value into argv and expand its field codes (spec section "The Exec key").
/// `uri` fills the first `%u`/`%U`/`%f`/`%F`, or is appended when the line has none.
pub(crate) fn exec_argv(exec: &str, name: &str, desktop_file: &Path, uri: Option<&str>) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_arg = false;
    let mut chars = exec.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                in_arg = true;
            }
            '\\' if quoted => current.push(chars.next()?),
            ' ' | '\t' if !quoted => {
                if in_arg {
                    args.push(std::mem::take(&mut current));
                    in_arg = false;
                }
            }
            _ => {
                current.push(c);
                in_arg = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if in_arg {
        args.push(current);
    }
    let mut argv = Vec::new();
    let mut uri = uri;
    for arg in args {
        match arg.as_str() {
            "%f" | "%F" | "%u" | "%U" => argv.extend(uri.take().map(str::to_owned)),
            "%i" | "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => continue,
            "%c" => argv.push(name.to_owned()),
            "%k" => argv.push(desktop_file.to_string_lossy().into_owned()),
            _ => argv.push(arg.replace("%%", "%")),
        }
    }
    argv.extend(uri.map(str::to_owned));
    (!argv.is_empty()).then_some(argv)
}

fn spawn_detached(argv: &[String]) -> Result<(), LaunchError> {
    let mut command = std::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => LaunchError::NotFound,
        _ => LaunchError::Failed,
    })?;
    // Reap the child so a long-lived caller does not collect zombies.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// The `Exec=` line of `section` (`[Desktop Entry]`, `[Desktop Action new-window]`, ...).
fn section_exec<'a>(text: &'a str, section: &str) -> Option<&'a str> {
    text.lines()
        .skip_while(|l| l.trim() != section)
        .skip(1)
        .take_while(|l| !l.trim_start().starts_with('['))
        .find_map(|l| l.trim().strip_prefix("Exec="))
}

fn run_entry(app: &AppEntry, uri: Option<&str>, new_window: bool) -> Result<(), LaunchError> {
    let text = std::fs::read_to_string(&app.target).map_err(|_| LaunchError::NotFound)?;
    let exec = new_window
        .then(|| section_exec(&text, "[Desktop Action new-window]"))
        .flatten()
        .or_else(|| section_exec(&text, "[Desktop Entry]"))
        .ok_or(LaunchError::NotFound)?;
    let argv = exec_argv(exec, &app.name, &app.target, uri).ok_or(LaunchError::InvalidInput)?;
    spawn_detached(&argv)
}

pub(crate) async fn launch(app: &AppEntry) -> Result<(), LaunchError> {
    run_entry(app, None, false)
}

pub(crate) async fn open_uri_with(app: &AppEntry, uri: &str, options: OpenOptions) -> Result<(), LaunchError> {
    run_entry(app, Some(uri), options.new_window)
}

pub(crate) async fn open_uri(uri: &str) -> Result<(), LaunchError> {
    spawn_detached(&["xdg-open".into(), uri.into()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_visible_applications_only() {
        let text = "[Desktop Entry]\nType=Application\nName=Files\nExec=dolphin %u\nIcon=system-file-manager\nKeywords=browser;folder;\nGenericName=File Manager\n[Desktop Action new]\nName=Ignored\n";
        let app = parse_entry("org.kde.dolphin.desktop".into(), Path::new("/x"), text).unwrap();
        assert_eq!(app.name, "Files");
        assert_eq!(app.keywords, ["browser", "folder", "File Manager"]);
        assert!(parse_entry("a".into(), Path::new("/x"), &text.replace("Type=Application", "Type=Link")).is_none());
        assert!(parse_entry("a".into(), Path::new("/x"), &text
            .replace("Icon=", "NoDisplay=true\nIcon=")).is_none());
    }

    #[test]
    fn expands_exec_field_codes() {
        let file = Path::new("/apps/x.desktop");
        assert_eq!(exec_argv("dolphin %u", "Files", file, None).unwrap(), ["dolphin"]);
        assert_eq!(
            exec_argv("\"/opt/My App/run\" --name %c --file %k 100%%", "Files", file, None).unwrap(),
            ["/opt/My App/run", "--name", "Files", "--file", "/apps/x.desktop", "100%"]
        );
        assert!(exec_argv("\"unterminated", "x", file, None).is_none());
    }

    #[test]
    fn a_uri_fills_the_first_field_code_or_is_appended() {
        let file = Path::new("/x.desktop");
        let uri = Some("https://go/docs");
        assert_eq!(exec_argv("brave %U", "B", file, uri).unwrap(), ["brave", "https://go/docs"]);
        assert_eq!(exec_argv("a %u %u", "A", file, uri).unwrap(), ["a", "https://go/docs"]);
        assert_eq!(exec_argv("brave --new-window", "B", file, uri).unwrap(), ["brave", "--new-window", "https://go/docs"]);
    }

    #[test]
    fn new_window_uses_the_entrys_new_window_action() {
        let text = "[Desktop Entry]\nExec=brave %U\n[Desktop Action new-window]\nExec=brave --new-window\n";
        assert_eq!(section_exec(text, "[Desktop Action new-window]"), Some("brave --new-window"));
        assert_eq!(section_exec(text, "[Desktop Entry]"), Some("brave %U"));
        assert_eq!(section_exec("[Desktop Entry]\nExec=a\n", "[Desktop Action new-window]"), None);
    }

    #[tokio::test]
    async fn launches_an_entry_from_a_private_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("launched");
        let apps = dir.path().join("applications");
        std::fs::create_dir_all(&apps).unwrap();
        let file = apps.join("kernal-test.desktop");
        std::fs::write(
            &file,
            format!("[Desktop Entry]\nType=Application\nName=Kernal Test\nExec=touch {}\n", marker.display()),
        )
        .unwrap();
        let app = parse_entry("kernal-test.desktop".into(), &file, &std::fs::read_to_string(&file).unwrap()).unwrap();
        launch(&app).await.unwrap();
        for _ in 0..50 {
            if marker.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("launched entry did not run");
    }
}
