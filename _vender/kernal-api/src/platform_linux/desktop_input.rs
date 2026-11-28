//! Client of the privileged `kernal-inputd` daemon, the only process that opens /dev/uinput.
//!
//! Wire format, one request per connection: a header line `<VERB> <arg> <arg>\n`, optionally
//! followed by a payload of the byte length the header names; the daemon answers one line,
//! `OK`, `DENIED` or `ERR`.
use crate::desktop_input::{InputError, MouseButton};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;

/// The daemon's socket; group-owned so only members of the daemon's group can connect.
pub(crate) const SOCKET: &str = "/run/kernal-inputd/socket";
const BUDGET: Duration = Duration::from_secs(5);

pub(crate) fn encode(verb: &str, args: &[&str], payload: &[u8]) -> Vec<u8> {
    let mut out = verb.as_bytes().to_vec();
    for arg in args {
        out.push(b' ');
        out.extend_from_slice(arg.as_bytes());
    }
    out.push(b'\n');
    out.extend_from_slice(payload);
    out
}

async fn request(message: Vec<u8>) -> Result<(), InputError> {
    let exchange = async {
        let mut stream = UnixStream::connect(SOCKET)
            .await
            .map_err(|_| InputError::Unavailable)?;
        stream
            .write_all(&message)
            .await
            .map_err(|_| InputError::Unavailable)?;
        let mut reply = String::new();
        BufReader::new(stream)
            .read_line(&mut reply)
            .await
            .map_err(|_| InputError::Unavailable)?;
        match reply.trim_end() {
            "OK" => Ok(()),
            "DENIED" => Err(InputError::Denied),
            _ => Err(InputError::Unavailable),
        }
    };
    tokio::time::timeout(BUDGET, exchange)
        .await
        .map_err(|_| InputError::Unavailable)?
}

pub(crate) async fn type_text(text: &str) -> Result<(), InputError> {
    request(encode("TYPE", &[&text.len().to_string()], text.as_bytes())).await
}

pub(crate) async fn pointer_move_by(dx: i32, dy: i32) -> Result<(), InputError> {
    request(encode("MOVE", &[&dx.to_string(), &dy.to_string()], &[])).await
}

pub(crate) async fn click(button: MouseButton) -> Result<(), InputError> {
    let name = match button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
    };
    request(encode("CLICK", &[name], &[])).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_a_header_line_plus_a_sized_payload() {
        assert_eq!(encode("TYPE", &["5"], "héy".as_bytes()), b"TYPE 5\nh\xc3\xa9y");
        assert_eq!(encode("MOVE", &["-3", "4"], &[]), b"MOVE -3 4\n");
    }
}
