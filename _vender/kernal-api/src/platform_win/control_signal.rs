//! Control signals as per-PID named events, plus console close and shutdown.
//!
//! Each signal is an auto-reset event named
//! `Local\kernal-api-control-<pid>-<signal>` in the session namespace. The
//! receiver creates all three, so a client can only reach a process that is
//! listening; a waiter thread forwards each set event to the async side.

use std::ffi::c_void;
use std::io;
use std::thread::JoinHandle;

use tokio::signal::windows::{ctrl_close, ctrl_shutdown, CtrlClose, CtrlShutdown};
use tokio::sync::mpsc;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, OpenEventW, SetEvent, WaitForMultipleObjects, EVENT_MODIFY_STATE, INFINITE,
};

use crate::control_signal::ControlSignal;

const SIGNALS: [ControlSignal; 3] = [
    ControlSignal::User1,
    ControlSignal::User2,
    ControlSignal::Terminate,
];

fn event_name(pid: u32, signal: ControlSignal) -> Vec<u16> {
    let suffix = match signal {
        ControlSignal::User1 => "user1",
        ControlSignal::User2 => "user2",
        ControlSignal::Terminate => "terminate",
    };
    format!("Local\\kernal-api-control-{pid}-{suffix}")
        .encode_utf16()
        .chain(Some(0))
        .collect()
}

/// An owned event handle.
struct Event(*mut c_void);

// SAFETY: a kernel event handle may be used and closed from any thread.
unsafe impl Send for Event {}
// SAFETY: every operation on it (`SetEvent`, waits) is thread-safe.
unsafe impl Sync for Event {}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle is owned and closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

impl Event {
    /// Create an auto-reset event, refusing one that already exists.
    fn create(name: Option<&[u16]>) -> io::Result<Self> {
        let name_ptr = name.map_or(std::ptr::null(), <[u16]>::as_ptr);
        // SAFETY: default security, auto-reset, unsignalled, and a
        // NUL-terminated name or null.
        let handle = unsafe { CreateEventW(std::ptr::null(), 0, 0, name_ptr) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let event = Self(handle);
        // SAFETY: reads this thread's last-error value set by CreateEventW.
        if name.is_some() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "this process's control events already exist",
            ));
        }
        Ok(event)
    }

    fn set(&self) -> io::Result<()> {
        // SAFETY: the handle is live and carries EVENT_MODIFY_STATE.
        if unsafe { SetEvent(self.0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

pub(crate) struct Receiver {
    signals: mpsc::UnboundedReceiver<ControlSignal>,
    close: CtrlClose,
    shutdown: CtrlShutdown,
    stop: std::sync::Arc<Event>,
    waiter: Option<JoinHandle<()>>,
}

impl Receiver {
    pub(crate) fn listen() -> io::Result<Self> {
        let pid = std::process::id();
        let events = SIGNALS
            .iter()
            .map(|signal| Event::create(Some(&event_name(pid, *signal))))
            .collect::<io::Result<Vec<_>>>()?;
        let stop = std::sync::Arc::new(Event::create(None)?);
        let close = ctrl_close()?;
        let shutdown = ctrl_shutdown()?;
        let (sender, signals) = mpsc::unbounded_channel();
        let waiter_stop = std::sync::Arc::clone(&stop);
        let waiter = std::thread::Builder::new()
            .name("kernal-control-signal".into())
            .spawn(move || wait_loop(&events, &waiter_stop, &sender))?;
        Ok(Self {
            signals,
            close,
            shutdown,
            stop,
            waiter: Some(waiter),
        })
    }

    pub(crate) async fn recv(&mut self) -> Option<ControlSignal> {
        tokio::select! {
            got = self.signals.recv() => got,
            got = self.close.recv() => got.map(|()| ControlSignal::Terminate),
            got = self.shutdown.recv() => got.map(|()| ControlSignal::Terminate),
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        // A failed SetEvent would leave the waiter blocked forever; joining it
        // then would hang this drop, so only join once it was told to stop.
        if self.stop.set().is_ok() {
            if let Some(waiter) = self.waiter.take() {
                let _ = waiter.join();
            }
        }
    }
}

fn wait_loop(events: &[Event], stop: &Event, sender: &mpsc::UnboundedSender<ControlSignal>) {
    let mut handles: Vec<*mut c_void> = events.iter().map(|event| event.0).collect();
    handles.push(stop.0);
    let count = u32::try_from(handles.len()).expect("four handles");
    loop {
        // SAFETY: every handle stays open for this loop's lifetime: `events`
        // is owned by this thread and `stop` by an Arc it holds.
        let woke = unsafe { WaitForMultipleObjects(count, handles.as_ptr(), 0, INFINITE) };
        let index = woke.wrapping_sub(WAIT_OBJECT_0) as usize;
        let Some(signal) = SIGNALS.get(index) else {
            // The stop event, or a failed wait: either way, stop forwarding.
            return;
        };
        if sender.send(*signal).is_err() {
            return;
        }
    }
}

pub(crate) fn send(pid: u32, signal: ControlSignal) -> io::Result<()> {
    let name = event_name(pid, signal);
    // SAFETY: modify-state access, no inheritance, NUL-terminated name.
    let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return Err(
            if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no process with that PID is listening for control signals",
                )
            } else {
                error
            },
        );
    }
    Event(handle).set()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pid_without_a_receiver_is_not_found() {
        let error = send(u32::MAX, ControlSignal::User1).expect_err("nobody listens");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
