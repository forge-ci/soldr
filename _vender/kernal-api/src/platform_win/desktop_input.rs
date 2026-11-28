//! `SendInput` injection: Unicode key events for text, relative mouse events for the pointer.
use crate::desktop_input::{InputError, MouseButton};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEINPUT, SendInput,
};

fn key(unit: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: unit,
                dwFlags: KEYEVENTF_UNICODE | flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse(dx: i32, dy: i32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<(), InputError> {
    let count = u32::try_from(inputs.len()).map_err(|_| InputError::InvalidInput)?;
    // SAFETY: `inputs` is a valid slice of initialized INPUT records of the stated size.
    let sent = unsafe { SendInput(count, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    // SendInput reports 0 when UIPI blocks injection into a higher-integrity window.
    if sent == count { Ok(()) } else { Err(InputError::Denied) }
}

pub(crate) fn text_inputs(text: &str) -> Vec<INPUT> {
    text.encode_utf16()
        .flat_map(|unit| [key(unit, 0), key(unit, KEYEVENTF_KEYUP)])
        .collect()
}

pub(crate) async fn type_text(text: &str) -> Result<(), InputError> {
    send(&text_inputs(text))
}

pub(crate) async fn pointer_move_by(dx: i32, dy: i32) -> Result<(), InputError> {
    send(&[mouse(dx, dy, MOUSEEVENTF_MOVE)])
}

pub(crate) async fn click(button: MouseButton) -> Result<(), InputError> {
    let (down, up) = match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    };
    send(&[mouse(0, 0, down), mouse(0, 0, up)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_becomes_utf16_down_up_pairs() {
        // "é😀" = 1 BMP unit + 1 surrogate pair = 3 units, 6 events.
        let inputs = text_inputs("é😀");
        assert_eq!(inputs.len(), 6);
        // SAFETY: every record built by text_inputs is a keyboard input.
        let flags: Vec<u32> = inputs.iter().map(|i| unsafe { i.Anonymous.ki.dwFlags }).collect();
        assert_eq!(flags[0], KEYEVENTF_UNICODE);
        assert_eq!(flags[1], KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
    }

    // Moves the real pointer of whatever desktop runs the suite, under every
    // other test in parallel; run it deliberately on an interactive session.
    #[tokio::test]
    #[ignore = "injects real input into the host desktop"]
    async fn pointer_round_trip_is_accepted_by_the_desktop() {
        pointer_move_by(5, 5).await.expect("move");
        pointer_move_by(-5, -5).await.expect("move back");
    }
}
