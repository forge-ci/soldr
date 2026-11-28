//! Host-neutral terminal-input queue used when native console capture is unavailable.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use thiserror::Error;

pub const NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV: &str =
    "KERNAL_API_NATIVE_TERMINAL_INPUT_TRACE_PATH";

#[derive(Debug, Error)]
pub enum TerminalInputError {
    #[error("terminal input capture timed out")]
    Timeout,
    #[error("terminal input capture is closed")]
    Closed,
    #[error("terminal input I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("terminal input error: {0}")]
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TerminalInputEventRecord {
    pub data: Vec<u8>,
    pub submit: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub virtual_key_code: u16,
    pub repeat_count: u16,
}

pub struct TerminalInputState {
    pub events: VecDeque<TerminalInputEventRecord>,
    pub closed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TerminalInputWaitOutcome {
    Event(TerminalInputEventRecord),
    Timeout,
    Closed,
}

pub fn unix_now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

pub fn wait_for_terminal_input_event(
    state: &Arc<Mutex<TerminalInputState>>,
    condvar: &Arc<Condvar>,
    timeout: Option<Duration>,
) -> TerminalInputWaitOutcome {
    let deadline = timeout.map(|duration| Instant::now() + duration);
    let mut guard = state.lock().expect("terminal input mutex poisoned");
    loop {
        if let Some(event) = guard.events.pop_front() {
            return TerminalInputWaitOutcome::Event(event);
        }
        if guard.closed {
            return TerminalInputWaitOutcome::Closed;
        }
        match deadline {
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    return TerminalInputWaitOutcome::Timeout;
                }
                let (next, result) = condvar
                    .wait_timeout(guard, deadline.saturating_duration_since(now))
                    .expect("terminal input mutex poisoned");
                guard = next;
                if result.timed_out() && guard.events.is_empty() {
                    return TerminalInputWaitOutcome::Timeout;
                }
            }
            None => {
                guard = condvar.wait(guard).expect("terminal input mutex poisoned");
            }
        }
    }
}

pub struct TerminalInputCore {
    pub state: Arc<Mutex<TerminalInputState>>,
    pub condvar: Arc<Condvar>,
    pub stop: Arc<AtomicBool>,
    pub capturing: Arc<AtomicBool>,
    pub worker: Mutex<Option<thread::JoinHandle<()>>>,
}

impl Default for TerminalInputCore {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalInputCore {
    pub fn supported() -> bool {
        false
    }

    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(TerminalInputState {
                events: VecDeque::new(),
                closed: true,
            })),
            condvar: Arc::new(Condvar::new()),
            stop: Arc::new(AtomicBool::new(false)),
            capturing: Arc::new(AtomicBool::new(false)),
            worker: Mutex::new(None),
        }
    }

    pub fn next_event(&self) -> Option<TerminalInputEventRecord> {
        self.state
            .lock()
            .expect("terminal input mutex poisoned")
            .events
            .pop_front()
    }

    pub fn available(&self) -> bool {
        !self
            .state
            .lock()
            .expect("terminal input mutex poisoned")
            .events
            .is_empty()
    }

    pub fn capturing(&self) -> bool {
        self.capturing.load(Ordering::Acquire)
    }

    pub fn original_console_mode(&self) -> Option<u32> {
        None
    }

    pub fn active_console_mode(&self) -> Option<u32> {
        None
    }

    /// Wait at most `timeout` for the next event, returning
    /// [`TerminalInputError::Timeout`] on expiry.
    // Delegates to the deprecated optional form until it is removed (#433).
    #[allow(deprecated)]
    pub fn wait_for_event_within(
        &self,
        timeout: Duration,
    ) -> Result<TerminalInputEventRecord, TerminalInputError> {
        self.wait_for_event(Some(timeout.as_secs_f64()))
    }

    #[deprecated(note = "`None` waits forever; use `wait_for_event_within` (#433)")]
    pub fn wait_for_event(
        &self,
        timeout: Option<f64>,
    ) -> Result<TerminalInputEventRecord, TerminalInputError> {
        match wait_for_terminal_input_event(
            &self.state,
            &self.condvar,
            timeout.map(Duration::from_secs_f64),
        ) {
            TerminalInputWaitOutcome::Event(event) => Ok(event),
            TerminalInputWaitOutcome::Timeout => Err(TerminalInputError::Timeout),
            TerminalInputWaitOutcome::Closed => Err(TerminalInputError::Closed),
        }
    }

    pub fn drain_events(&self) -> Vec<TerminalInputEventRecord> {
        self.state
            .lock()
            .expect("terminal input mutex poisoned")
            .events
            .drain(..)
            .collect()
    }

    pub fn stop_impl(&self) -> Result<(), std::io::Error> {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self
            .worker
            .lock()
            .expect("terminal input worker mutex poisoned")
            .take()
        {
            let _ = worker.join();
        }
        self.capturing.store(false, Ordering::Release);
        let mut state = self.state.lock().expect("terminal input mutex poisoned");
        state.closed = true;
        self.condvar.notify_all();
        Ok(())
    }

    pub fn start_impl(&self) -> Result<(), std::io::Error> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "NativeTerminalInput is only available on Windows consoles",
        ))
    }
}

impl Drop for TerminalInputCore {
    fn drop(&mut self) {
        let _ = self.stop_impl();
    }
}
