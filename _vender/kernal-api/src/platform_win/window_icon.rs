//! Windows console window-icon mechanics.

use crate::platform::window_icon::{
    IconDegradedReason, IconError, IconScope, IconSource, IconSupport, IconUnsupportedReason,
    StockIcon,
};
    use std::os::windows::ffi::OsStrExt as _;

    use winapi::shared::minwindef::{BOOL, DWORD, FALSE, LPARAM, TRUE};
    use winapi::shared::windef::{HICON, HWND};
    use winapi::um::wincon::GetConsoleWindow;
    use winapi::um::winuser::{
        CreateIconFromResourceEx, EnumWindows, GetClassNameW, GetWindowThreadProcessId, LoadIconW,
        LoadImageW, SendMessageW, IDI_APPLICATION, IDI_ERROR, IDI_INFORMATION, IDI_SHIELD,
        IDI_WARNING, IMAGE_ICON, LR_DEFAULTSIZE, LR_LOADFROMFILE, WM_SETICON,
    };

    /// `wParam` values for `WM_SETICON`.
    const ICON_SMALL: usize = 0;
    const ICON_BIG: usize = 1;

    /// Window class of the classic console host.
    ///
    /// This is the discriminator that matters. Windows Terminal hosts the
    /// session in a pseudo-console whose `GetConsoleWindow` handle belongs to
    /// a hidden window of a different class — `WM_SETICON` against it
    /// succeeds and changes nothing visible.
    const CONHOST_CLASS: &str = "ConsoleWindowClass";

    fn console_window() -> Option<HWND> {
        let hwnd = unsafe { GetConsoleWindow() };
        (!hwnd.is_null()).then_some(hwnd)
    }

    fn class_name(hwnd: HWND) -> String {
        let mut buffer = [0u16; 256];
        let len = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
        if len <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buffer[..len as usize])
    }

    /// The console window a scope names, if there is one.
    fn window_for(scope: IconScope) -> Option<HWND> {
        match scope {
            IconScope::Host => console_window(),
            IconScope::Child { pid } => console_window_of_pid(pid),
        }
    }

    /// Find the console window owned by `pid`.
    ///
    /// A process has at most one console window, so the first match is the
    /// answer. The class is checked here as well as in the support probe
    /// because a process can own windows that are not its console.
    fn console_window_of_pid(pid: u32) -> Option<HWND> {
        struct Search {
            pid: u32,
            found: HWND,
        }

        unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let search = &mut *(lparam as *mut Search);
            let mut owner: DWORD = 0;
            GetWindowThreadProcessId(hwnd, &mut owner);
            if owner == search.pid && class_name(hwnd) == CONHOST_CLASS {
                search.found = hwnd;
                return FALSE; // stop: a process has one console window
            }
            TRUE
        }

        let mut search = Search {
            pid,
            found: std::ptr::null_mut(),
        };
        unsafe { EnumWindows(Some(visit), &mut search as *mut Search as LPARAM) };
        (!search.found.is_null()).then_some(search.found)
    }

    pub fn icon_support(scope: IconScope) -> IconSupport {
        if let IconScope::Child { pid } = scope {
            return match console_window_of_pid(pid) {
                Some(_) => IconSupport::Available,
                // Either the child has no console of its own (it inherited
                // ours, or was created with CREATE_NO_WINDOW), or it has
                // already exited. Both mean there is no window to target.
                None => IconSupport::Unsupported(IconUnsupportedReason::ChildHasNoConsole),
            };
        }
        // Checked before the window class because it yields a remedy the
        // class check cannot: Windows Terminal *does* support a per-profile
        // icon, just not one set at runtime. "Set the profile's icon field"
        // is actionable; "your host owns its decoration" is not.
        if std::env::var_os("WT_SESSION").is_some() {
            return IconSupport::Degraded(IconDegradedReason::WindowsTerminal);
        }
        let Some(hwnd) = console_window() else {
            return IconSupport::Unsupported(IconUnsupportedReason::NoConsole);
        };
        if class_name(hwnd) == CONHOST_CLASS {
            return IconSupport::Available;
        }
        IconSupport::Degraded(IconDegradedReason::NonClassicWindowsHost)
    }

    /// Load an icon from a file, letting the OS pick the best size.
    fn load_from_path(path: &std::path::Path) -> Result<HICON, IconError> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);

        // LR_DEFAULTSIZE picks the system's preferred size from a multi-image
        // .ico rather than whichever image happens to be first.
        let icon = unsafe {
            LoadImageW(
                std::ptr::null_mut(),
                wide.as_ptr(),
                IMAGE_ICON,
                0,
                0,
                LR_LOADFROMFILE | LR_DEFAULTSIZE,
            )
        } as HICON;
        if icon.is_null() {
            return Err(IconError::Load {
                path: path.to_path_buf(),
                source: std::io::Error::last_os_error(),
            });
        }
        Ok(icon)
    }

    /// Load an icon from `.ico` bytes held in memory.
    ///
    /// There is no `LoadImage` equivalent that takes a whole `.ico` from
    /// memory, so the directory is walked here to find one image and
    /// `CreateIconFromResourceEx` is given exactly that span. The bytes are
    /// treated as untrusted: `crate::platform::window_icon::ico::best_image` bounds-checks every
    /// offset before we hand a length to the OS, which would otherwise read
    /// whatever follows in our address space.
    fn load_from_bytes(bytes: &[u8]) -> Result<HICON, IconError> {
        let span = crate::platform::window_icon::ico::best_image(bytes).map_err(IconError::Decode)?;
        let image = &bytes[span.offset..span.offset + span.len];

        // 0x00030000 is the icon resource version the API expects.
        const ICON_RESOURCE_VERSION: DWORD = 0x0003_0000;
        let icon = unsafe {
            CreateIconFromResourceEx(
                image.as_ptr() as *mut u8,
                image.len() as DWORD,
                TRUE,
                ICON_RESOURCE_VERSION,
                0,
                0,
                LR_DEFAULTSIZE,
            )
        };
        if icon.is_null() {
            return Err(IconError::Apply(std::io::Error::last_os_error()));
        }
        Ok(icon)
    }

    /// Load an icon the OS already provides.
    ///
    /// These are shared resources owned by the system, so unlike the file and
    /// byte paths there is nothing to free and no data to validate — the only
    /// failure is the OS declining to hand one over.
    pub fn load_stock(stock: StockIcon) -> Result<HICON, IconError> {
        let name = match stock {
            StockIcon::Application => IDI_APPLICATION,
            StockIcon::Warning => IDI_WARNING,
            StockIcon::Error => IDI_ERROR,
            StockIcon::Information => IDI_INFORMATION,
            StockIcon::Shield => IDI_SHIELD,
        };
        // A null hInstance asks for a system icon rather than one from this
        // module's resources.
        let icon = unsafe { LoadIconW(std::ptr::null_mut(), name) };
        if icon.is_null() {
            return Err(IconError::Apply(std::io::Error::last_os_error()));
        }
        Ok(icon)
    }

pub fn set_icon(scope: IconScope, source: &IconSource) -> Result<(), IconError> {
        let hwnd = window_for(scope)
            .ok_or(IconError::Unsupported(IconUnsupportedReason::TargetDisappeared))?;

        let icon = match source {
            IconSource::Path(path) => load_from_path(path)?,
            IconSource::Bytes(bytes) => load_from_bytes(bytes)?,
            IconSource::Stock(stock) => load_stock(*stock)?,
        };

        // Both slots: the small icon is the title bar and Alt+Tab, the big one
        // is the taskbar. Setting only one leaves the other stale, which looks
        // like a partial failure to a user.
        unsafe {
            SendMessageW(hwnd, WM_SETICON, ICON_SMALL, icon as isize);
            SendMessageW(hwnd, WM_SETICON, ICON_BIG, icon as isize);
        }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_terminal_is_detected_by_env_and_names_its_remedy() {
        let previous = std::env::var_os("WT_SESSION");
        unsafe { std::env::set_var("WT_SESSION", "test-session") };
        let support = icon_support(IconScope::Host);
        match previous {
            Some(value) => unsafe { std::env::set_var("WT_SESSION", value) },
            None => unsafe { std::env::remove_var("WT_SESSION") },
        }
        match support {
            IconSupport::Degraded(IconDegradedReason::WindowsTerminal) => {}
            other => panic!("WT_SESSION must yield Degraded, got {other:?}"),
        }
    }

    #[test]
    fn the_os_supplies_every_stock_icon() {
        for stock in [
            StockIcon::Application,
            StockIcon::Warning,
            StockIcon::Error,
            StockIcon::Information,
            StockIcon::Shield,
        ] {
            let icon = load_stock(stock)
                .unwrap_or_else(|error| panic!("the OS declined {stock:?}: {error}"));
            assert!(!icon.is_null(), "{stock:?} produced a null icon");
        }
    }

    #[test]
    fn a_childless_pid_reason_names_the_console_window() {
        let support = icon_support(IconScope::Child { pid: 0 });
        assert_eq!(
            support,
            IconSupport::Unsupported(IconUnsupportedReason::ChildHasNoConsole)
        );
    }
}
