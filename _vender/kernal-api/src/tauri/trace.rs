//! Bounded acceptance diagnostics; no guest-controlled strings or payloads.
use super::*;

/// One host-observed event from a Wasm webview proof.
#[derive(Clone, Debug)]
pub struct WebviewTestTraceEvent {
    /// Monotonic time since this host's recorder was created.
    pub elapsed: Duration,
    /// Fixed host-owned event name.
    pub phase: &'static str,
    /// Generated submission opcode, when applicable.
    pub opcode: Option<u32>,
    /// Hub counts after joined root cleanup, when applicable.
    pub observation: Option<WebviewTestObservation>,
}

pub(super) struct Recorder {
    #[cfg(any(feature = "wasm-sketch-host", test))]
    start: Instant,
    state: Mutex<(Vec<WebviewTestTraceEvent>, usize)>,
}

impl Recorder {
    pub(super) fn new() -> Self {
        Self {
            #[cfg(any(feature = "wasm-sketch-host", test))]
            start: Instant::now(),
            state: Mutex::new((Vec::new(), 0)),
        }
    }

    #[cfg(any(feature = "wasm-sketch-host", test))]
    pub(super) fn record(
        &self,
        at: Instant,
        phase: &'static str,
        opcode: Option<u32>,
        observation: Option<WebviewTestObservation>,
    ) {
        let mut state = self.state.lock().expect("proof trace lock poisoned");
        if state.0.len() == 512 {
            state.1 = state.1.saturating_add(1);
            return;
        }
        state.0.push(WebviewTestTraceEvent {
            elapsed: at.saturating_duration_since(self.start),
            phase,
            opcode,
            observation,
        });
    }

    pub(super) fn snapshot(&self) -> (Vec<WebviewTestTraceEvent>, usize) {
        self.state
            .lock()
            .expect("proof trace lock poisoned")
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recorder_bounds_storage_and_reports_loss() {
        let recorder = Recorder::new();
        for _ in 0..600 {
            recorder.record(Instant::now(), "poll", None, None);
        }
        let (events, lost) = recorder.snapshot();
        assert_eq!(events.len(), 512);
        assert_eq!(lost, 88);
        assert!(events
            .windows(2)
            .all(|pair| pair[0].elapsed <= pair[1].elapsed));
    }
}
