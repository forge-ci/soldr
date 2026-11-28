//! Win32 `RegisterHotKey` on a dedicated message thread. Windows reports presses only.
use crate::global_hotkey::{Chord, HotkeyError, HotkeyEvent, HotkeyOptions, Key};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use tokio::sync::mpsc::Sender;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
    VK_ESCAPE, VK_F1, VK_RETURN, VK_SPACE, VK_TAB,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, WM_HOTKEY, WM_QUIT,
};

const HOTKEY_ID: i32 = 1;

pub(crate) struct Handle {
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
    active: Arc<AtomicBool>,
}

impl Handle {
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: posting to a thread id we own; a failed post means the thread already exited.
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) fn virtual_key(chord: &Chord) -> (u32, u32) {
    let m = chord.modifiers;
    let mut mods = MOD_NOREPEAT;
    for (on, flag) in [
        (m.ctrl, MOD_CONTROL),
        (m.alt, MOD_ALT),
        (m.shift, MOD_SHIFT),
        (m.meta, MOD_WIN),
    ] {
        if on {
            mods |= flag;
        }
    }
    let vk = match chord.key {
        Key::Char(c) => u32::from(c.to_ascii_uppercase()),
        Key::Space => u32::from(VK_SPACE),
        Key::Enter => u32::from(VK_RETURN),
        Key::Escape => u32::from(VK_ESCAPE),
        Key::Tab => u32::from(VK_TAB),
        Key::Function(n) => u32::from(VK_F1) + u32::from(n) - 1,
    };
    (mods, vk)
}

pub(crate) async fn register(
    options: HotkeyOptions,
    events: Sender<HotkeyEvent>,
) -> Result<Handle, HotkeyError> {
    let (mods, vk) = virtual_key(&options.chord);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let active = Arc::new(AtomicBool::new(true));
    let thread_active = Arc::clone(&active);
    let thread = std::thread::Builder::new()
        .name("kernal-api-hotkey".into())
        .spawn(move || {
            // SAFETY: Win32 message-queue calls on this thread's own queue; `msg` is a valid out
            // pointer and the hotkey is unregistered on the thread that registered it.
            unsafe {
                let mut msg: MSG = std::mem::zeroed();
                PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
                if RegisterHotKey(std::ptr::null_mut(), HOTKEY_ID, mods, vk) == 0 {
                    let _ = ready_tx.send(Err(HotkeyError::Conflict));
                    thread_active.store(false, Ordering::Release);
                    return;
                }
                let _ = ready_tx.send(Ok(GetCurrentThreadId()));
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    if msg.message == WM_HOTKEY && msg.wParam == HOTKEY_ID as usize {
                        let _ = events.try_send(HotkeyEvent::Pressed);
                    }
                }
                UnregisterHotKey(std::ptr::null_mut(), HOTKEY_ID);
            }
            thread_active.store(false, Ordering::Release);
        })
        .map_err(|_| HotkeyError::Unavailable)?;
    let thread_id = tokio::task::spawn_blocking(move || ready_rx.recv())
        .await
        .map_err(|_| HotkeyError::Unavailable)?
        .map_err(|_| HotkeyError::Unavailable)??;
    Ok(Handle {
        thread_id,
        thread: Some(thread),
        active,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_chords_to_win32_virtual_keys() {
        let (mods, vk) = virtual_key(&Chord::parse("Meta+Space").unwrap());
        assert_eq!((mods, vk), (MOD_NOREPEAT | MOD_WIN, u32::from(VK_SPACE)));
        assert_eq!(virtual_key(&Chord::parse("Ctrl+k").unwrap()).1, u32::from(b'K'));
        assert_eq!(virtual_key(&Chord::parse("F12").unwrap()).1, u32::from(VK_F1) + 11);
    }
}
