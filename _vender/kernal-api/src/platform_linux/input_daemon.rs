//! `kernal-inputd`: the one process that owns /dev/uinput. Clients reach it only through a
//! socket whose directory permissions the service manager sets, and each connection's peer
//! uid must be on the allowlist, so socket-group membership alone does not grant injection.
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};

const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const KEY_LEFTSHIFT: u16 = 42;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const MAX_TEXT_BYTES: usize = 16 * 1024;
const REQUEST_BUDGET: Duration = Duration::from_secs(5);

#[repr(C)]
struct UinputSetup {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputEvent {
    tv_sec: libc::time_t,
    tv_usec: libc::suseconds_t,
    kind: u16,
    code: u16,
    value: i32,
}

/// US-layout key code and whether Shift is held, for printable ASCII plus `\n` and `\t`.
pub(crate) fn us_key(c: char) -> Option<(u16, bool)> {
    const LETTERS: [u16; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47,
        17, 45, 21, 44,
    ];
    const DIGITS: [u16; 10] = [11, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    const SHIFTED_DIGITS: &str = ")!@#$%^&*(";
    const PUNCT: [(char, char, u16); 11] = [
        ('-', '_', 12),
        ('=', '+', 13),
        ('[', '{', 26),
        (']', '}', 27),
        ('\\', '|', 43),
        (';', ':', 39),
        ('\'', '"', 40),
        ('`', '~', 41),
        (',', '<', 51),
        ('.', '>', 52),
        ('/', '?', 53),
    ];
    Some(match c {
        'a'..='z' => (LETTERS[usize::from(c as u8 - b'a')], false),
        'A'..='Z' => (LETTERS[usize::from(c as u8 - b'A')], true),
        '0'..='9' => (DIGITS[usize::from(c as u8 - b'0')], false),
        ' ' => (57, false),
        '\n' => (28, false),
        '\t' => (15, false),
        _ => {
            if let Some(i) = SHIFTED_DIGITS.find(c) {
                return Some((DIGITS[i], true));
            }
            let &(_, shifted, code) = PUNCT.iter().find(|p| p.0 == c || p.1 == c)?;
            (code, c == shifted)
        }
    })
}

struct Device(File);

impl Device {
    fn open() -> io::Result<Self> {
        let file = OpenOptions::new().write(true).open("/dev/uinput")?;
        let fd = file.as_raw_fd();
        let ioctl = |request, arg: libc::c_int| {
            // SAFETY: uinput setup ioctls on an fd we own, each taking an int by value.
            if unsafe { libc::ioctl(fd, request, arg) } < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        };
        ioctl(UI_SET_EVBIT, EV_KEY.into())?;
        ioctl(UI_SET_EVBIT, EV_REL.into())?;
        ioctl(UI_SET_RELBIT, REL_X.into())?;
        ioctl(UI_SET_RELBIT, REL_Y.into())?;
        for code in (1..=88).chain([BTN_LEFT, BTN_RIGHT, BTN_MIDDLE]) {
            ioctl(UI_SET_KEYBIT, code.into())?;
        }
        let mut setup = UinputSetup {
            bustype: 0x06, // BUS_VIRTUAL
            vendor: 0x4b41,
            product: 0x0001,
            version: 1,
            name: [0; 80],
            ff_effects_max: 0,
        };
        setup.name[..14].copy_from_slice(b"kernal-inputd\0");
        // SAFETY: UI_DEV_SETUP reads a `struct uinput_setup`, which `UinputSetup` mirrors.
        if unsafe { libc::ioctl(fd, UI_DEV_SETUP, &setup) } < 0
            // SAFETY: UI_DEV_CREATE takes no argument.
            || unsafe { libc::ioctl(fd, UI_DEV_CREATE) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        // Give the compositor time to pick up the new device before the first event.
        std::thread::sleep(Duration::from_millis(200));
        Ok(Self(file))
    }

    fn emit(&mut self, kind: u16, code: u16, value: i32) -> io::Result<()> {
        let event = InputEvent {
            tv_sec: 0,
            tv_usec: 0,
            kind,
            code,
            value,
        };
        // SAFETY: `InputEvent` is a plain repr(C) struct; we view its bytes for the write.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&event as *const InputEvent).cast::<u8>(),
                std::mem::size_of::<InputEvent>(),
            )
        };
        self.0.write_all(bytes)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.emit(EV_SYN, 0, 0)
    }

    fn tap(&mut self, code: u16, shift: bool) -> io::Result<()> {
        if shift {
            self.emit(EV_KEY, KEY_LEFTSHIFT, 1)?;
        }
        self.emit(EV_KEY, code, 1)?;
        self.sync()?;
        self.emit(EV_KEY, code, 0)?;
        if shift {
            self.emit(EV_KEY, KEY_LEFTSHIFT, 0)?;
        }
        self.sync()
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: destroys the device this fd created.
        unsafe { libc::ioctl(self.0.as_raw_fd(), UI_DEV_DESTROY) };
    }
}

pub(crate) enum Request {
    Type(String),
    Move(i32, i32),
    Click(u16),
}

pub(crate) fn parse_header(line: &str) -> Option<(Request, usize)> {
    let mut words = line.split_ascii_whitespace();
    let request = match words.next()? {
        "TYPE" => {
            let len: usize = words.next()?.parse().ok()?;
            if len == 0 || len > MAX_TEXT_BYTES {
                return None;
            }
            return words.next().is_none().then_some((Request::Type(String::new()), len));
        }
        "MOVE" => {
            let dx: i32 = words.next()?.parse().ok()?;
            let dy: i32 = words.next()?.parse().ok()?;
            if dx.abs() > 10_000 || dy.abs() > 10_000 {
                return None;
            }
            Request::Move(dx, dy)
        }
        "CLICK" => Request::Click(match words.next()? {
            "left" => BTN_LEFT,
            "right" => BTN_RIGHT,
            "middle" => BTN_MIDDLE,
            _ => return None,
        }),
        _ => return None,
    };
    words.next().is_none().then_some((request, 0))
}

fn apply(device: &mut Device, request: &Request) -> io::Result<bool> {
    match request {
        Request::Type(text) => {
            let Some(keys) = text.chars().map(us_key).collect::<Option<Vec<_>>>() else {
                return Ok(false);
            };
            for (code, shift) in keys {
                device.tap(code, shift)?;
            }
        }
        Request::Move(dx, dy) => {
            device.emit(EV_REL, REL_X, *dx)?;
            device.emit(EV_REL, REL_Y, *dy)?;
            device.sync()?;
        }
        Request::Click(button) => device.tap(*button, false)?,
    }
    Ok(true)
}

async fn serve(stream: UnixStream, device: Arc<Mutex<Device>>, allowed: Arc<Vec<u32>>) {
    let reply = async {
        let uid = stream.peer_cred().ok()?.uid();
        let mut reader = BufReader::new(stream);
        if !allowed.contains(&uid) {
            return Some((reader, "DENIED"));
        }
        let mut line = String::new();
        (&mut reader).take(64).read_line(&mut line).await.ok()?;
        let Some((mut request, len)) = parse_header(&line) else {
            return Some((reader, "ERR"));
        };
        if let Request::Type(text) = &mut request {
            let mut bytes = vec![0; len];
            reader.read_exact(&mut bytes).await.ok()?;
            *text = String::from_utf8(bytes).ok()?;
        }
        let ok = tokio::task::spawn_blocking(move || {
            let mut device = device.lock().unwrap_or_else(|e| e.into_inner());
            apply(&mut device, &request)
        })
        .await
        .ok()?
        .ok()?;
        Some((reader, if ok { "OK" } else { "ERR" }))
    };
    if let Ok(Some((reader, answer))) = tokio::time::timeout(REQUEST_BUDGET, reply).await {
        let mut stream = reader.into_inner();
        let _ = stream.write_all(format!("{answer}\n").as_bytes()).await;
    }
}

/// Create the virtual device and serve `socket` until the process is stopped.
pub async fn run(socket: &Path, allowed_uids: Vec<u32>) -> io::Result<()> {
    let device = Arc::new(Mutex::new(Device::open()?));
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o660))?;
    let allowed = Arc::new(allowed_uids);
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(serve(stream, Arc::clone(&device), Arc::clone(&allowed)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_us_ascii_with_shift() {
        assert_eq!(us_key('a'), Some((30, false)));
        assert_eq!(us_key('A'), Some((30, true)));
        assert_eq!(us_key('0'), Some((11, false)));
        assert_eq!(us_key(')'), Some((11, true)));
        assert_eq!(us_key('?'), Some((53, true)));
        assert_eq!(us_key('/'), Some((53, false)));
        assert_eq!(us_key('é'), None);
    }

    #[test]
    fn headers_are_strict() {
        assert!(matches!(parse_header("TYPE 4\n"), Some((Request::Type(_), 4))));
        assert!(matches!(parse_header("MOVE -3 4\n"), Some((Request::Move(-3, 4), 0))));
        assert!(matches!(parse_header("CLICK left\n"), Some((Request::Click(BTN_LEFT), 0))));
        for bad in ["TYPE 0", "TYPE 999999", "MOVE 1", "MOVE 1 2 3", "CLICK x", "RM -rf"] {
            assert!(parse_header(bad).is_none(), "{bad}");
        }
    }
}
