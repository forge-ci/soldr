//! Line-atomic terminal writes for relayed child output (soldr#3929).
//!
//! soldr relays cargo's stdout and stderr from reader threads that see
//! raw 8 KiB pipe chunks, which end mid-line, while the cache-state tail
//! prints its own lines from another thread. When a harness captures
//! soldr's stdout and stderr into one pipe, a cache line written between
//! two halves of a relayed line splices into it (`test a ... o` + cache
//! line + `k`), and a test report then miscounts the pass lines.
//!
//! Every terminal writer therefore emits only whole records, each in a
//! single `write_all` under one process-wide lock. A `\r` ends a record
//! too, so progress redraws stay live; a record longer than
//! [`MAX_PENDING`] is released as-is so memory stays bounded.
//!
//! Only the terminal copy goes through here. The raw capture channel
//! bytes are untouched.

use std::io::{self, Write};
use std::sync::Mutex;

/// Serializes every whole-record terminal write across threads.
static TERMINAL_LINES: Mutex<()> = Mutex::new(());

/// Longest partial record held back before it is released unterminated.
const MAX_PENDING: usize = 64 * 1024;

/// Write `line` in one call under the terminal lock, then flush.
pub(crate) fn emit_whole_line<W: Write>(w: &mut W, line: &[u8]) -> io::Result<()> {
    let _guard = TERMINAL_LINES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    w.write_all(line)?;
    w.flush()
}

/// Buffers relayed bytes and releases them one whole record at a time.
pub(crate) struct LineAtomicWriter<W: Write> {
    inner: W,
    pending: Vec<u8>,
}

impl<W: Write> LineAtomicWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self {
            inner,
            pending: Vec::new(),
        }
    }

    /// Release the partial last record and flush. Call once at EOF.
    pub(crate) fn finish(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            emit_whole_line(&mut self.inner, &line)?;
        }
        self.inner.flush()
    }
}

impl<W: Write> Write for LineAtomicWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut rest = buf;
        while let Some(pos) = rest.iter().position(|&b| b == b'\n' || b == b'\r') {
            let (head, tail) = rest.split_at(pos + 1);
            if self.pending.is_empty() {
                emit_whole_line(&mut self.inner, head)?;
            } else {
                let mut line = std::mem::take(&mut self.pending);
                line.extend_from_slice(head);
                emit_whole_line(&mut self.inner, &line)?;
            }
            rest = tail;
        }
        self.pending.extend_from_slice(rest);
        if self.pending.len() > MAX_PENDING {
            let line = std::mem::take(&mut self.pending);
            emit_whole_line(&mut self.inner, &line)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Only the inner writer: releasing the partial record here would
        // reopen the splice window at every 8 KiB chunk (cf. soldr#3099).
        self.inner.flush()
    }
}

impl<W: Write> Drop for LineAtomicWriter<W> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Records each `write` call separately.
    #[derive(Default)]
    struct Recorder {
        writes: Vec<Vec<u8>>,
    }

    impl Write for &mut Recorder {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.writes.push(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            // One byte per call widens any splice window a non-atomic
            // writer would leave open.
            let Some(&b) = buf.first() else {
                return Ok(0);
            };
            self.0.lock().expect("sink").push(b);
            Ok(1)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_chunk_split_mid_line_is_written_once_whole() {
        let mut rec = Recorder::default();
        {
            let mut w = LineAtomicWriter::new(&mut rec);
            w.write_all(b"test a ... o").expect("write");
            w.flush().expect("flush");
            w.write_all(b"k\n").expect("write");
            w.flush().expect("flush");
        }
        assert_eq!(rec.writes, vec![b"test a ... ok\n".to_vec()]);
    }

    #[test]
    fn concurrent_whole_line_emits_never_splice() {
        let sink = SharedSink::default();
        let relay_sink = sink.clone();
        let relay = std::thread::spawn(move || {
            let mut text = Vec::new();
            for i in 0..1000 {
                text.extend_from_slice(format!("test t{i} ... ok\n").as_bytes());
            }
            let mut w = LineAtomicWriter::new(relay_sink);
            for chunk in text.chunks(7) {
                w.write_all(chunk).expect("write");
                w.flush().expect("flush");
            }
            w.finish().expect("finish");
        });
        let mut cache_sink = sink.clone();
        let cache = std::thread::spawn(move || {
            for i in 0..1000 {
                emit_whole_line(&mut cache_sink, format!("cache HIT crate{i}\n").as_bytes())
                    .expect("emit");
            }
        });
        relay.join().expect("relay");
        cache.join().expect("cache");
        let out = String::from_utf8(sink.0.lock().expect("sink").clone()).expect("utf8");
        let mut ok_lines = 0;
        for line in out.lines() {
            let is_test = line.starts_with("test t") && line.ends_with(" ... ok");
            let is_cache = line.starts_with("cache HIT crate")
                && line["cache HIT crate".len()..]
                    .chars()
                    .all(|c| c.is_ascii_digit());
            assert!(is_test || is_cache, "spliced line: {line:?}");
            if is_test {
                ok_lines += 1;
            }
        }
        assert_eq!(ok_lines, 1000);
    }

    #[test]
    fn carriage_return_releases_a_record_promptly() {
        let mut rec = Recorder::default();
        {
            let mut w = LineAtomicWriter::new(&mut rec);
            w.write_all(b"Building [=>  ] 1/9\rBuil").expect("write");
            assert_eq!(w.inner.writes, vec![b"Building [=>  ] 1/9\r".to_vec()]);
            std::mem::forget(w);
        }
    }

    #[test]
    fn partial_record_is_released_on_finish_not_flush() {
        let mut rec = Recorder::default();
        {
            let mut w = LineAtomicWriter::new(&mut rec);
            w.write_all(b"prompt> ").expect("write");
            w.flush().expect("flush");
            assert!(w.inner.writes.is_empty());
            w.finish().expect("finish");
            assert_eq!(w.inner.writes, vec![b"prompt> ".to_vec()]);
        }
        assert_eq!(rec.writes.len(), 1, "drop must not re-emit");
    }

    #[test]
    fn an_overlong_record_is_emitted_without_unbounded_growth() {
        let mut rec = Recorder::default();
        {
            let mut w = LineAtomicWriter::new(&mut rec);
            let chunk = vec![b'x'; 8192];
            for _ in 0..32 {
                w.write_all(&chunk).expect("write");
                assert!(w.pending.len() <= MAX_PENDING);
            }
            w.finish().expect("finish");
        }
        let total: usize = rec.writes.iter().map(Vec::len).sum();
        assert_eq!(total, 8192 * 32);
        assert!(rec.writes.iter().all(|w| w.len() <= MAX_PENDING + 8192));
    }
}
