//! Private, compile-time-selected Component hash candidate for the experiment.
use super::OperationError;

mod bindings {
    wit_bindgen::generate!({ path: "src/guest_component_hash.wit", world: "hash-client" });
}
use bindings::kernal::hash_experiment::hashes;

const HASH_INPUT_CHUNK_BYTES: usize = 256;

fn input_chunk(bytes: &[u8], last: bool) -> hashes::InputChunk {
    let word = |offset| {
        let mut word = [0_u8; 8];
        if offset < bytes.len() {
            let end = bytes.len().min(offset + word.len());
            word[..end - offset].copy_from_slice(&bytes[offset..end]);
        }
        u64::from_le_bytes(word)
    };
    hashes::InputChunk {
        word_0: word(0),
        word_1: word(8),
        word_2: word(16),
        word_3: word(24),
        word_4: word(32),
        word_5: word(40),
        word_6: word(48),
        word_7: word(56),
        word_8: word(64),
        word_9: word(72),
        word_10: word(80),
        word_11: word(88),
        word_12: word(96),
        word_13: word(104),
        word_14: word(112),
        word_15: word(120),
        word_16: word(128),
        word_17: word(136),
        word_18: word(144),
        word_19: word(152),
        word_20: word(160),
        word_21: word(168),
        word_22: word(176),
        word_23: word(184),
        word_24: word(192),
        word_25: word(200),
        word_26: word(208),
        word_27: word(216),
        word_28: word(224),
        word_29: word(232),
        word_30: word(240),
        word_31: word(248),
        used: bytes.len() as u16,
        last,
    }
}

fn error(value: hashes::Error) -> OperationError {
    match value {
        hashes::Error::Rejected => OperationError::Rejected,
        hashes::Error::Failed => OperationError::Failed,
    }
}

pub(super) struct Blake3Hasher {
    inner: hashes::Hasher,
}

impl Blake3Hasher {
    pub(super) async fn new() -> Result<Self, OperationError> {
        Ok(Self {
            inner: hashes::create().map_err(error)?,
        })
    }

    pub(super) async fn update(&mut self, bytes: &[u8]) -> Result<(), OperationError> {
        if bytes.len() > 64 * 1024 {
            return Err(OperationError::Rejected);
        }
        // The stream is private Component plumbing; the public facade remains
        // a bounded slice. Poll the import once to install its reader before
        // writing, then let the scalar final frame authorize the one commit.
        let (mut writer, reader) =
            bindings::wit_stream::new::<bindings::kernal::hash_experiment::hashes::InputChunk>();
        let mut update = std::pin::pin!(self.inner.update(reader));
        if let Some(result) = std::future::poll_fn(|context| {
            std::task::Poll::Ready(match std::future::Future::poll(update.as_mut(), context) {
                std::task::Poll::Pending => None,
                std::task::Poll::Ready(result) => Some(result),
            })
        })
        .await
        {
            return result.map_err(error);
        }
        let chunks = bytes
            .chunks(HASH_INPUT_CHUNK_BYTES)
            .enumerate()
            .map(|(index, chunk)| {
                input_chunk(
                    chunk,
                    index + 1 == bytes.len().div_ceil(HASH_INPUT_CHUNK_BYTES),
                )
            })
            .chain((bytes.is_empty()).then_some(input_chunk(&[], true)))
            .collect();
        if !writer.write_all(chunks).await.is_empty() {
            return Err(OperationError::Rejected);
        }
        drop(writer);
        update.await.map_err(error)
    }

    pub(super) async fn finalize(self) -> Result<[u8; 32], OperationError> {
        hashes::finish(self.inner)
            .map_err(error)?
            .try_into()
            .map_err(|_| OperationError::Failed)
    }
}
