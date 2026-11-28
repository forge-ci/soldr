//! Carbon `RegisterEventHotKey` backend. Needs no Accessibility permission, but events arrive
//! through the application event target, so the host must run the main run loop
//! (`NSApplication`, Qt, or `CFRunLoopRun` on the main thread).
use crate::global_hotkey::{Chord, HotkeyError, HotkeyEvent, HotkeyOptions, Key};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::mpsc::Sender;

type OsStatus = i32;
type EventTargetRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventHotKeyRef = *mut c_void;
type EventRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventHandlerProc = extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OsStatus;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct EventHotKeyId {
    signature: u32,
    id: u32,
}

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: EventHandlerProc,
        num_types: u32,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut EventHandlerRef,
    ) -> OsStatus;
    fn RemoveEventHandler(handler: EventHandlerRef) -> OsStatus;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        id: EventHotKeyId,
        target: EventTargetRef,
        options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> OsStatus;
    fn UnregisterEventHotKey(hotkey: EventHotKeyRef) -> OsStatus;
    fn GetEventKind(event: EventRef) -> u32;
    fn GetEventParameter(
        event: EventRef,
        name: u32,
        desired_type: u32,
        actual_type: *mut u32,
        buffer_size: usize,
        actual_size: *mut usize,
        data: *mut c_void,
    ) -> OsStatus;
}

const fn four_cc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}
const SIGNATURE: u32 = four_cc(b"kapi");
const K_EVENT_CLASS_KEYBOARD: u32 = four_cc(b"keyb");
const K_EVENT_PARAM_DIRECT_OBJECT: u32 = four_cc(b"----");
const TYPE_EVENT_HOT_KEY_ID: u32 = four_cc(b"hkid");
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
const K_EVENT_HOT_KEY_RELEASED: u32 = 6;
const CMD_KEY: u32 = 1 << 8;
const SHIFT_KEY: u32 = 1 << 9;
const OPTION_KEY: u32 = 1 << 11;
const CONTROL_KEY: u32 = 1 << 12;

struct Context {
    id: u32,
    events: Sender<HotkeyEvent>,
}

extern "C" fn on_hotkey(_call: EventHandlerCallRef, event: EventRef, data: *mut c_void) -> OsStatus {
    // SAFETY: `data` is the `Context` boxed in `register`, alive until `Handle` drops after
    // removing this handler; `event` is valid for the duration of the callback.
    unsafe {
        let context = &*(data as *const Context);
        let mut hotkey = EventHotKeyId::default();
        let status = GetEventParameter(
            event,
            K_EVENT_PARAM_DIRECT_OBJECT,
            TYPE_EVENT_HOT_KEY_ID,
            std::ptr::null_mut(),
            std::mem::size_of::<EventHotKeyId>(),
            std::ptr::null_mut(),
            (&mut hotkey as *mut EventHotKeyId).cast(),
        );
        if status == 0 && hotkey.signature == SIGNATURE && hotkey.id == context.id {
            let event = match GetEventKind(event) {
                K_EVENT_HOT_KEY_PRESSED => HotkeyEvent::Pressed,
                K_EVENT_HOT_KEY_RELEASED => HotkeyEvent::Released,
                _ => return 0,
            };
            let _ = context.events.try_send(event);
        }
    }
    0
}

pub(crate) struct Handle {
    hotkey: EventHotKeyRef,
    handler: EventHandlerRef,
    context: *mut Context,
}

// SAFETY: the raw refs are opaque Carbon tokens only passed back to Carbon on drop.
unsafe impl Send for Handle {}
// SAFETY: `&Handle` exposes no Carbon state.
unsafe impl Sync for Handle {}

impl Handle {
    pub(crate) fn is_active(&self) -> bool {
        !self.hotkey.is_null()
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: refs came from successful Register/Install calls; the handler is removed before
        // its context is freed.
        unsafe {
            UnregisterEventHotKey(self.hotkey);
            RemoveEventHandler(self.handler);
            drop(Box::from_raw(self.context));
        }
    }
}

pub(crate) fn key_code(chord: &Chord) -> (u32, u32) {
    let m = chord.modifiers;
    let mut mods = 0;
    for (on, flag) in [
        (m.ctrl, CONTROL_KEY),
        (m.alt, OPTION_KEY),
        (m.shift, SHIFT_KEY),
        (m.meta, CMD_KEY),
    ] {
        if on {
            mods |= flag;
        }
    }
    const LETTERS: [u32; 26] = [
        0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16,
        6,
    ];
    const DIGITS: [u32; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
    const FUNCTION: [u32; 12] = [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111];
    let code = match chord.key {
        Key::Char(c @ 'a'..='z') => LETTERS[usize::from(c as u8 - b'a')],
        Key::Char(c) => DIGITS[usize::from(c as u8 - b'0')],
        Key::Space => 49,
        Key::Enter => 36,
        Key::Escape => 53,
        Key::Tab => 48,
        Key::Function(n) => FUNCTION[usize::from(n - 1)],
    };
    (mods, code)
}

pub(crate) async fn register(
    options: HotkeyOptions,
    events: Sender<HotkeyEvent>,
) -> Result<Handle, HotkeyError> {
    static NEXT_ID: AtomicU32 = AtomicU32::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (mods, code) = key_code(&options.chord);
    let context = Box::into_raw(Box::new(Context { id, events }));
    let types = [
        EventTypeSpec {
            event_class: K_EVENT_CLASS_KEYBOARD,
            event_kind: K_EVENT_HOT_KEY_PRESSED,
        },
        EventTypeSpec {
            event_class: K_EVENT_CLASS_KEYBOARD,
            event_kind: K_EVENT_HOT_KEY_RELEASED,
        },
    ];
    // SAFETY: out pointers are valid locals; `context` outlives the handler (freed in `Drop`, or
    // below on failure after the handler is removed).
    unsafe {
        let target = GetApplicationEventTarget();
        let mut handler: EventHandlerRef = std::ptr::null_mut();
        if InstallEventHandler(target, on_hotkey, 2, types.as_ptr(), context.cast(), &mut handler)
            != 0
        {
            drop(Box::from_raw(context));
            return Err(HotkeyError::Unavailable);
        }
        let mut hotkey: EventHotKeyRef = std::ptr::null_mut();
        let hotkey_id = EventHotKeyId {
            signature: SIGNATURE,
            id,
        };
        if RegisterEventHotKey(code, mods, hotkey_id, target, 0, &mut hotkey) != 0 {
            RemoveEventHandler(handler);
            drop(Box::from_raw(context));
            return Err(HotkeyError::Conflict);
        }
        Ok(Handle {
            hotkey,
            handler,
            context,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_chords_to_carbon_key_codes() {
        assert_eq!(
            key_code(&Chord::parse("Meta+Space").unwrap()),
            (CMD_KEY, 49)
        );
        assert_eq!(key_code(&Chord::parse("Ctrl+k").unwrap()), (CONTROL_KEY, 40));
        assert_eq!(key_code(&Chord::parse("Alt+0").unwrap()).1, 29);
        assert_eq!(key_code(&Chord::parse("F9").unwrap()).1, 101);
    }

    #[tokio::test]
    async fn registers_and_releases_a_chord() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let options =
            HotkeyOptions::new("kernal-api-test", "test", Chord::parse("Ctrl+Alt+Shift+F12").unwrap())
                .unwrap();
        let handle = register(options, tx).await.expect("register");
        assert!(handle.is_active());
    }
}
