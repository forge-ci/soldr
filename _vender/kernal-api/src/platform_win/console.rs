use std::collections::HashSet;
use std::time::{Duration, Instant};

use winapi::shared::minwindef::{BOOL, LPARAM, TRUE};
use winapi::shared::windef::HWND;
use winapi::um::winuser::{EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible};

use crate::platform::process::ConsoleWindowInfo;

fn enumerate_visible_windows() -> Vec<ConsoleWindowInfo> {
    let mut results = Vec::new();

    unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        if IsWindowVisible(hwnd) == 0 {
            return TRUE;
        }
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        let mut title_buf = [0u16; 512];
        let len = GetWindowTextW(hwnd, title_buf.as_mut_ptr(), title_buf.len() as i32);
        let title = if len > 0 {
            String::from_utf16_lossy(&title_buf[..len as usize])
        } else {
            String::new()
        };
        let results = &mut *(lparam as *mut Vec<ConsoleWindowInfo>);
        results.push(ConsoleWindowInfo {
            pid,
            title,
            hwnd: hwnd as u64,
        });
        TRUE
    }

    unsafe {
        EnumWindows(
            Some(enum_callback),
            &mut results as *mut Vec<ConsoleWindowInfo> as LPARAM,
        );
    }
    results
}

pub fn monitor_console_windows(duration: Duration) -> Vec<ConsoleWindowInfo> {
    let baseline: HashSet<u64> = enumerate_visible_windows()
        .iter()
        .map(|window| window.hwnd)
        .collect();
    let mut seen_new = HashSet::new();
    let mut new_windows = Vec::new();
    let start = Instant::now();
    while start.elapsed() < duration {
        std::thread::sleep(Duration::from_millis(50));
        for info in enumerate_visible_windows() {
            if !baseline.contains(&info.hwnd) && seen_new.insert(info.hwnd) {
                new_windows.push(info);
            }
        }
    }
    new_windows
}
