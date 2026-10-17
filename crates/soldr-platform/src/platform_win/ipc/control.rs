#![allow(missing_docs)]

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::time::Duration;

pub struct ControlStream {
    commands: mpsc::Sender<IoCommand>,
    recv_timeout: Duration,
    send_timeout: Duration,
}

enum IoCommand {
    Read {
        len: usize,
        timeout: Duration,
        reply: mpsc::Sender<io::Result<Vec<u8>>>,
    },
    Write {
        data: Vec<u8>,
        timeout: Duration,
        reply: mpsc::Sender<io::Result<usize>>,
    },
    Flush {
        timeout: Duration,
        reply: mpsc::Sender<io::Result<()>>,
    },
}

impl ControlStream {
    fn spawn(
        endpoint: String,
        connect_timeout: Duration,
        recv_timeout: Duration,
        send_timeout: Duration,
    ) -> io::Result<Self> {
        let (commands, receiver) = mpsc::channel();
        let (startup_tx, startup_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("soldr-broker-control-io".into())
            .spawn(move || io_worker(endpoint, connect_timeout, receiver, startup_tx))?;
        control_reply(
            startup_rx,
            "connect",
            connect_timeout.saturating_add(Duration::from_secs(1)),
        )?;
        Ok(Self {
            commands,
            recv_timeout,
            send_timeout,
        })
    }

    fn send_command(&self, command: IoCommand) -> io::Result<()> {
        self.commands.send(command).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Windows broker control I/O worker stopped",
            )
        })
    }
}

fn io_worker(
    endpoint: String,
    connect_timeout: Duration,
    commands: mpsc::Receiver<IoCommand>,
    startup: mpsc::Sender<io::Result<()>>,
) {
    use interprocess::os::windows::named_pipe::{pipe_mode::Bytes, tokio::DuplexPipeStream};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = startup.send(Err(error));
            return;
        }
    };
    let mut stream =
        match crate::platform::wait::block_on_for(
            &runtime,
            DuplexPipeStream::<Bytes>::connect_by_path_with_wait_mode(
                endpoint.as_str(),
                interprocess::ConnectWaitMode::Timeout(connect_timeout),
            ),
            connect_timeout + crate::platform::wait::PROBE,
        )
        .unwrap_or_else(|error| Err(error.into()))
        {
            Ok(stream) => stream,
            Err(error) => {
                let _ = startup.send(Err(error));
                return;
            }
        };
    if startup.send(Ok(())).is_err() {
        return;
    }

    loop {
        // Bounded receive; ends when the last command sender is dropped.
        let command = match crate::platform::wait::recv_for(&commands, std::time::Duration::from_secs(1)) {
            Ok(command) => command,
            Err(crate::platform::wait::WaitError::TimedOut(_)) => continue,
            Err(crate::platform::wait::WaitError::Disconnected) => break,
        };
        match command {
            IoCommand::Read {
                len,
                timeout,
                reply,
            } => {
                let mut data = vec![0_u8; len];
                let result = crate::platform::wait::block_on_for(&runtime, async {
                    tokio::time::timeout(timeout, stream.read(&mut data))
                        .await
                        .map_err(|_| control_timeout("read", timeout))?
                        .map(|read| {
                            data.truncate(read);
                            data
                        })
                }, timeout + crate::platform::wait::PROBE)
                .unwrap_or_else(|error| Err(error.into()));
                let _ = reply.send(result);
            }
            IoCommand::Write {
                data,
                timeout,
                reply,
            } => {
                let result = crate::platform::wait::block_on_for(&runtime, async {
                    tokio::time::timeout(timeout, stream.write(&data))
                        .await
                        .map_err(|_| control_timeout("write", timeout))?
                }, timeout + crate::platform::wait::PROBE)
                .unwrap_or_else(|error| Err(error.into()));
                let _ = reply.send(result);
            }
            IoCommand::Flush { timeout, reply } => {
                let result = crate::platform::wait::block_on_for(&runtime, async {
                    tokio::time::timeout(timeout, stream.flush())
                        .await
                        .map_err(|_| control_timeout("flush", timeout))?
                }, timeout + crate::platform::wait::PROBE)
                .unwrap_or_else(|error| Err(error.into()));
                let _ = reply.send(result);
            }
        }
    }
}

fn control_reply<T>(
    reply: mpsc::Receiver<io::Result<T>>,
    operation: &str,
    timeout: Duration,
) -> io::Result<T> {
    match reply.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(control_timeout(operation, timeout)),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "Windows broker control I/O worker stopped",
        )),
    }
}

impl Read for ControlStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let timeout = self.recv_timeout;
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send_command(IoCommand::Read {
            len: buf.len(),
            timeout,
            reply: reply_tx,
        })?;
        let data = control_reply(
            reply_rx,
            "read worker",
            timeout.saturating_add(Duration::from_secs(1)),
        )?;
        buf[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }
}

impl Write for ControlStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let timeout = self.send_timeout;
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send_command(IoCommand::Write {
            data: buf.to_vec(),
            timeout,
            reply: reply_tx,
        })?;
        control_reply(
            reply_rx,
            "write worker",
            timeout.saturating_add(Duration::from_secs(1)),
        )
    }

    fn flush(&mut self) -> io::Result<()> {
        let timeout = self.send_timeout;
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send_command(IoCommand::Flush {
            timeout,
            reply: reply_tx,
        })?;
        control_reply(
            reply_rx,
            "flush worker",
            timeout.saturating_add(Duration::from_secs(1)),
        )
    }
}

pub fn connect(endpoint: String, route_timeout: Duration) -> io::Result<ControlStream> {
    // Interprocess 2.4.3's high-level Windows local-socket adapters discard
    // ConnectWaitMode, so the worker uses its low-level named-pipe Tokio API
    // directly. That API preserves the bounded connect and supports
    // cancellation of each overlapped I/O future at its deadline. Keeping the
    // runtime on the worker also remains safe when synchronous daemon control
    // is called from inside Soldr's existing Tokio runtime.
    ControlStream::spawn(endpoint, route_timeout, route_timeout, route_timeout)
}

pub fn configure_timeouts(
    stream: &mut ControlStream,
    recv_timeout: Duration,
    send_timeout: Duration,
) -> io::Result<()> {
    stream.recv_timeout = recv_timeout;
    stream.send_timeout = send_timeout;
    Ok(())
}

fn control_timeout(operation: &str, timeout: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "Windows broker control {operation} timed out after {}ms",
            timeout.as_millis()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_pipe_is_bounded_inside_tokio_runtime() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let endpoint = format!(
                r"\\.\pipe\soldr-missing-control-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            );
            let timeout = Duration::from_millis(100);
            let started = std::time::Instant::now();
            let result = ControlStream::spawn(endpoint, timeout, timeout, timeout);
            assert!(result.is_err());
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "missing named pipe exceeded its bounded connect: {:?}",
                started.elapsed()
            );
        });
    }
}
