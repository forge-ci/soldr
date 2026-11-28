//! Microphone capture as mono 16 kHz `f32` chunks, without exposing the audio backend.
//!
//! ALSA (which PipeWire and PulseAudio serve) on Linux, WASAPI on Windows, CoreAudio on macOS.
//! The device runs on a dedicated thread because some backends' streams are not `Send`.

use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};
use tokio::sync::mpsc;

/// Output rate: what speech models such as Whisper consume.
pub const SAMPLE_RATE: u32 = 16_000;
/// Chunks buffered before the oldest are dropped (about 10 s at typical callback sizes).
const BUFFERED_CHUNKS: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioError {
    /// No input device is present (common on CI runners and servers).
    NoDevice,
    /// The OS refused access (macOS microphone permission, Windows privacy setting).
    Denied,
    /// The device exists but could not be opened or started.
    Unavailable,
}

/// Mix interleaved frames down to one channel.
pub(crate) fn downmix(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    let channels = channels.max(1);
    out.extend(
        interleaved
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32),
    );
}

/// Streaming linear resampler that carries its phase across chunks.
pub(crate) struct Resampler {
    step: f64,
    pos: f64,
    prev: f32,
}

impl Resampler {
    pub(crate) fn new(from_rate: u32, to_rate: u32) -> Self {
        Self {
            step: f64::from(from_rate) / f64::from(to_rate),
            pos: 1.0,
            prev: 0.0,
        }
    }

    pub(crate) fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        // Index 0 is the previous chunk's last sample, so interpolation spans chunk borders.
        let sample = |i: usize| if i == 0 { self.prev } else { input[i - 1] };
        let last = input.len() as f64;
        let mut t = self.pos;
        while t <= last {
            let i = t.floor() as usize;
            let frac = (t - i as f64) as f32;
            let a = sample(i);
            let b = if i as f64 >= last { a } else { sample(i + 1) };
            out.push(a + (b - a) * frac);
            t += self.step;
        }
        self.pos = t - last;
        self.prev = input[input.len() - 1];
    }
}

/// Downmix one device callback's interleaved frames and resample them to [`SAMPLE_RATE`].
fn to_output_chunk(
    data: &[f32],
    channels: usize,
    resampler: &mut Resampler,
    mono: &mut Vec<f32>,
) -> Vec<f32> {
    mono.clear();
    downmix(data, channels, mono);
    let mut out = Vec::with_capacity(mono.len());
    resampler.push(mono, &mut out);
    out
}

/// Recorded or generated input played through [`Capture::open_test_source`].
#[derive(Clone, Debug, PartialEq)]
pub struct TestSource {
    /// Interleaved frames, `channels` samples each.
    pub samples: Vec<f32>,
    /// The rate the samples were recorded at; resampled to [`SAMPLE_RATE`].
    pub rate: u32,
    /// Channels per frame; downmixed to mono.
    pub channels: u16,
    /// Frames per simulated device callback.
    pub frames_per_callback: usize,
}

impl TestSource {
    /// `samples` as mono input already at [`SAMPLE_RATE`], in 10 ms callbacks.
    pub fn mono_16k(samples: Vec<f32>) -> Self {
        Self {
            samples,
            rate: SAMPLE_RATE,
            channels: 1,
            frames_per_callback: (SAMPLE_RATE / 100) as usize,
        }
    }

    /// A sine tone of `frequency` Hz and `amplitude`, `duration` long, as [`Self::mono_16k`].
    pub fn sine(frequency: f32, amplitude: f32, duration: Duration) -> Self {
        let count = (duration.as_secs_f64() * f64::from(SAMPLE_RATE)).round() as usize;
        let step = std::f32::consts::TAU * frequency / SAMPLE_RATE as f32;
        Self::mono_16k(
            (0..count)
                .map(|n| amplitude * (step * n as f32).sin())
                .collect(),
        )
    }
}

/// A running capture from the default input device. Dropping it stops the device.
pub struct Capture {
    chunks: mpsc::Receiver<Vec<f32>>,
    stop: Option<std_mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    device_rate: u32,
}

impl Capture {
    /// Open and start the default input device.
    pub async fn open_default() -> Result<Self, AudioError> {
        let (chunk_tx, chunks) = mpsc::channel(BUFFERED_CHUNKS);
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let (stop, stop_rx) = std_mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("kernal-api-audio".into())
            .spawn(move || run_device(chunk_tx, ready_tx, stop_rx))
            .map_err(|_| AudioError::Unavailable)?;
        let device_rate = tokio::task::spawn_blocking(move || ready_rx.recv())
            .await
            .map_err(|_| AudioError::Unavailable)?
            .map_err(|_| AudioError::Unavailable)??;
        Ok(Self {
            chunks,
            stop: Some(stop),
            thread: Some(thread),
            device_rate,
        })
    }

    /// Play `source` through the same downmix, resampling and chunk queue as a
    /// real device, with no hardware: for testing capture consumers on hosts
    /// without a microphone.
    ///
    /// Every callback's chunk is delivered (none are dropped, unlike a live
    /// device that outpaces its reader), as fast as they are read. When the
    /// source is exhausted the capture reports stopped:
    /// [`Self::next_chunk_within`] returns `None` once the queue drains.
    pub fn open_test_source(source: TestSource) -> Result<Self, AudioError> {
        let channels = usize::from(source.channels);
        if source.rate == 0 || channels == 0 || source.frames_per_callback == 0 {
            return Err(AudioError::Unavailable);
        }
        let (chunk_tx, chunks) = mpsc::channel(BUFFERED_CHUNKS);
        let device_rate = source.rate;
        let thread = std::thread::Builder::new()
            .name("kernal-api-audio-test".into())
            .spawn(move || {
                let mut resampler = Resampler::new(source.rate, SAMPLE_RATE);
                let mut mono = Vec::new();
                for callback in source.samples.chunks(source.frames_per_callback * channels) {
                    let out = to_output_chunk(callback, channels, &mut resampler, &mut mono);
                    if chunk_tx.blocking_send(out).is_err() {
                        return;
                    }
                }
            })
            .map_err(|_| AudioError::Unavailable)?;
        Ok(Self {
            chunks,
            stop: None,
            thread: Some(thread),
            device_rate,
        })
    }

    /// The device's native rate, before resampling to [`SAMPLE_RATE`].
    pub fn device_rate(&self) -> u32 {
        self.device_rate
    }

    /// Take one buffered chunk without waiting.
    pub fn try_chunk(&mut self) -> Option<Vec<f32>> {
        self.chunks.try_recv().ok()
    }

    /// Wait up to `budget` for the next chunk; `None` on timeout or when the device stopped.
    pub async fn next_chunk_within(&mut self, budget: Duration) -> Option<Vec<f32>> {
        tokio::time::timeout(budget, self.chunks.recv())
            .await
            .ok()
            .flatten()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        drop(self.stop.take());
        // Unblocks a test source waiting on a full queue.
        self.chunks.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn open_error(err: &dyn std::fmt::Display) -> AudioError {
    let text = err.to_string().to_ascii_lowercase();
    if text.contains("permission") || text.contains("denied") || text.contains("access") {
        AudioError::Denied
    } else {
        AudioError::Unavailable
    }
}

fn run_device(
    chunks: mpsc::Sender<Vec<f32>>,
    ready: std_mpsc::SyncSender<Result<u32, AudioError>>,
    stop: std_mpsc::Receiver<()>,
) {
    let started = (|| {
        let device = cpal::default_host()
            .default_input_device()
            .ok_or(AudioError::NoDevice)?;
        let config = device.default_input_config().map_err(|e| open_error(&e))?;
        let rate = config.sample_rate();
        let channels = usize::from(config.channels());
        let mut resampler = Resampler::new(rate, SAMPLE_RATE);
        let mut mono = Vec::new();
        let stream = device
            .build_input_stream(
                config.config(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let out = to_output_chunk(data, channels, &mut resampler, &mut mono);
                    let _ = chunks.try_send(out);
                },
                |_err| {},
                None,
            )
            .map_err(|e| open_error(&e))?;
        stream.play().map_err(|e| open_error(&e))?;
        Ok((stream, rate))
    })();
    match started {
        Ok((stream, rate)) => {
            let _ = ready.send(Ok(rate));
            // Blocks until `Capture` drops its sender; the stream lives on this thread.
            let _ = stop.recv();
            drop(stream);
        }
        Err(err) => {
            let _ = ready.send(Err(err));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_averages_each_frame() {
        let mut out = Vec::new();
        downmix(&[1.0, 0.0, 0.5, 0.5, -1.0, 1.0], 2, &mut out);
        assert_eq!(out, [0.5, 0.5, 0.0]);
    }

    #[test]
    fn resampling_48k_to_16k_keeps_duration_across_chunks() {
        let mut r = Resampler::new(48_000, SAMPLE_RATE);
        let mut out = Vec::new();
        for _ in 0..10 {
            r.push(&[0.25; 480], &mut out);
        }
        // 4800 input samples at 48 kHz = 0.1 s = 1600 samples at 16 kHz.
        assert!((1599..=1601).contains(&out.len()), "{}", out.len());
        assert!(out[10..].iter().all(|s| (s - 0.25).abs() < 1e-6));
    }

    #[test]
    fn resampling_interpolates_a_ramp() {
        let mut r = Resampler::new(2, 4);
        let mut out = Vec::new();
        r.push(&[0.0, 1.0], &mut out);
        r.push(&[2.0], &mut out);
        assert_eq!(out, [0.0, 0.5, 1.0, 1.5, 2.0]);
    }

    #[tokio::test]
    async fn a_test_source_is_delivered_whole_then_stops() {
        // Stereo 48 kHz: left 0.5, right 0.0, so the mono 16 kHz output is 0.25.
        let frames = 48_000 / 2;
        let source = TestSource {
            samples: [0.5, 0.0].repeat(frames),
            rate: 48_000,
            channels: 2,
            frames_per_callback: 480,
        };
        let mut capture = Capture::open_test_source(source).expect("test source");
        assert_eq!(capture.device_rate(), 48_000);
        let mut out = Vec::new();
        while let Some(chunk) = capture.next_chunk_within(Duration::from_secs(5)).await {
            out.extend(chunk);
        }
        assert!((7_999..=8_001).contains(&out.len()), "{}", out.len());
        assert!(out[10..].iter().all(|s| (s - 0.25).abs() < 1e-6));
    }

    #[test]
    fn dropping_an_unread_test_source_does_not_hang() {
        let source = TestSource::sine(440.0, 0.5, Duration::from_secs(60));
        drop(Capture::open_test_source(source).expect("test source"));
    }

    #[tokio::test]
    async fn open_default_reports_a_typed_outcome() {
        // CI runners have no microphone; either outcome must arrive without hanging or panicking.
        match Capture::open_default().await {
            Ok(mut capture) => {
                assert!(capture.device_rate() > 0);
                let _ = capture.next_chunk_within(Duration::from_secs(2)).await;
            }
            Err(err) => assert!(matches!(
                err,
                AudioError::NoDevice | AudioError::Unavailable | AudioError::Denied
            )),
        }
    }
}
