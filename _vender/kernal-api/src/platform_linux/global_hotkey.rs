//! XDG `GlobalShortcuts` portal backend (KDE Plasma 6, GNOME 48+, wlroots portals).
//!
//! Works on Wayland and X11 sessions alike; the desktop may ask the user to confirm the chord.
use crate::global_hotkey::{Chord, HotkeyError, HotkeyEvent, HotkeyOptions, Key};
use futures_util::StreamExt as _;
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use tokio::sync::mpsc::Sender;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, Proxy};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.portal.GlobalShortcuts";

pub(crate) struct Handle {
    task: tokio::task::JoinHandle<()>,
    active: Arc<AtomicBool>,
}

impl Handle {
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

impl Drop for Handle {
    // The task owns the only connection; closing it ends the portal session and the binding.
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn token() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "kernal_api_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

pub(crate) fn trigger(chord: &Chord) -> String {
    let m = chord.modifiers;
    let mut out = String::new();
    for (on, name) in [
        (m.ctrl, "CTRL+"),
        (m.alt, "ALT+"),
        (m.shift, "SHIFT+"),
        (m.meta, "LOGO+"),
    ] {
        if on {
            out.push_str(name);
        }
    }
    match chord.key {
        Key::Char(c) => out.push(c),
        Key::Space => out.push_str("space"),
        Key::Enter => out.push_str("Return"),
        Key::Escape => out.push_str("Escape"),
        Key::Tab => out.push_str("Tab"),
        Key::Function(n) => out.push_str(&format!("F{n}")),
    }
    out
}

/// Call a portal method that answers through an `org.freedesktop.portal.Request` object.
async fn request<B>(
    conn: &Connection,
    portal: &Proxy<'_>,
    method: &str,
    handle_token: &str,
    body: &B,
) -> Result<HashMap<String, OwnedValue>, HotkeyError>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let sender = conn
        .unique_name()
        .ok_or(HotkeyError::Unavailable)?
        .trim_start_matches(':')
        .replace('.', "_");
    let path = format!("{PATH}/request/{sender}/{handle_token}");
    let req = Proxy::new(conn, DEST, path, "org.freedesktop.portal.Request")
        .await
        .map_err(|_| HotkeyError::Unavailable)?;
    let mut responses = req
        .receive_signal("Response")
        .await
        .map_err(|_| HotkeyError::Unavailable)?;
    portal
        .call_method(method, body)
        .await
        .map_err(|_| HotkeyError::Unavailable)?;
    let msg = responses.next().await.ok_or(HotkeyError::Unavailable)?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = msg
        .body()
        .deserialize()
        .map_err(|_| HotkeyError::Unavailable)?;
    match code {
        0 => Ok(results),
        1 => Err(HotkeyError::Conflict),
        _ => Err(HotkeyError::Unavailable),
    }
}

pub(crate) async fn register(
    options: HotkeyOptions,
    events: Sender<HotkeyEvent>,
) -> Result<Handle, HotkeyError> {
    let conn = Connection::session()
        .await
        .map_err(|_| HotkeyError::Unavailable)?;
    let portal = Proxy::new(&conn, DEST, PATH, IFACE)
        .await
        .map_err(|_| HotkeyError::Unavailable)?;

    let create_token = token();
    // KDE files the binding under this token, so it must be stable across runs to persist.
    let session_token = options.id.replace(['.', '-'], "_");
    let created = request(
        &conn,
        &portal,
        "CreateSession",
        &create_token,
        &(HashMap::from([
            ("handle_token", Value::from(create_token.as_str())),
            ("session_handle_token", Value::from(session_token.as_str())),
        ]),),
    )
    .await?;
    let session = match created.get("session_handle").map(|v| &**v) {
        Some(Value::Str(s)) => s.to_string(),
        Some(Value::ObjectPath(p)) => p.to_string(),
        _ => return Err(HotkeyError::Unavailable),
    };
    let session_path = ObjectPath::try_from(session.as_str()).map_err(|_| HotkeyError::Unavailable)?;

    let trigger = trigger(&options.chord);
    let bind_token = token();
    let shortcut = HashMap::from([
        ("description", Value::from(options.description.as_str())),
        ("preferred_trigger", Value::from(trigger.as_str())),
    ]);
    let bound = request(
        &conn,
        &portal,
        "BindShortcuts",
        &bind_token,
        &(
            &session_path,
            vec![(options.id.as_str(), shortcut)],
            "",
            HashMap::from([("handle_token", Value::from(bind_token.as_str()))]),
        ),
    )
    .await?;
    if let Some(Value::Array(granted)) = bound.get("shortcuts").map(|v| &**v) {
        if granted.is_empty() {
            return Err(HotkeyError::Conflict);
        }
    }

    let mut activated = portal
        .receive_signal("Activated")
        .await
        .map_err(|_| HotkeyError::Unavailable)?;
    let mut deactivated = portal
        .receive_signal("Deactivated")
        .await
        .map_err(|_| HotkeyError::Unavailable)?;
    let active = Arc::new(AtomicBool::new(true));
    let task_active = Arc::clone(&active);
    let id = options.id;
    let task = tokio::spawn(async move {
        let _conn = conn;
        loop {
            let (msg, event) = tokio::select! {
                Some(m) = activated.next() => (m, HotkeyEvent::Pressed),
                Some(m) = deactivated.next() => (m, HotkeyEvent::Released),
                else => break,
            };
            let ours = msg
                .body()
                .deserialize::<(OwnedObjectPath, String, u64, HashMap<String, OwnedValue>)>()
                .is_ok_and(|(s, shortcut, _, _)| s.as_str() == session && shortcut == id);
            if ours {
                let _ = events.try_send(event);
            }
        }
        task_active.store(false, Ordering::Release);
    });
    Ok(Handle { task, active })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_trigger_uses_xdg_modifier_and_keysym_names() {
        assert_eq!(trigger(&Chord::parse("Meta+Space").unwrap()), "LOGO+space");
        assert_eq!(
            trigger(&Chord::parse("Ctrl+Shift+k").unwrap()),
            "CTRL+SHIFT+k"
        );
        assert_eq!(trigger(&Chord::parse("Alt+F4").unwrap()), "ALT+F4");
    }
}
