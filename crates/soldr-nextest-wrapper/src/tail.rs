//! The last bytes a stream produced, for memory-signature matching.

use soldr_platform::sync::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

const TAIL_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct OutputTail {
    limit: usize,
    data: Arc<Mutex<VecDeque<u8>>>,
}

impl Default for OutputTail {
    fn default() -> Self {
        Self::with_limit(TAIL_BYTES)
    }
}

impl OutputTail {
    pub fn with_limit(limit: usize) -> Self {
        Self {
            limit,
            data: Arc::default(),
        }
    }

    pub fn feed(&self, chunk: &[u8]) {
        let mut data = self.data.lock();
        data.extend(chunk);
        let excess = data.len().saturating_sub(self.limit);
        data.drain(..excess);
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.data.lock().iter().copied().collect()
    }
}
