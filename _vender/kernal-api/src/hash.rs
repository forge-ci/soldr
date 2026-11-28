//! Kernel-owned content hashing: BLAKE3 and optional `hash-sha256` compatibility.
//!
//! The concrete operations here deliberately expose only `kernal-api`
//! semantic values. The BLAKE3 implementation remains a private dependency,
//! so callers can hash bytes, readers, and files -- incrementally, with
//! optional key-derivation domain separation, and with an optional
//! memory-mapped file read -- without coupling their APIs to
//! implementation-crate types.

use std::fmt;
use std::io::{self, Read};
use std::path::Path;

#[cfg(feature = "hash-sha256")]
mod sha256;
#[cfg(feature = "hash-sha256")]
pub use sha256::{sha256_bytes, sha256_file, sha256_reader, Sha256Digest, Sha256Hasher};

#[cfg(feature = "fs")]
mod tree;
#[cfg(feature = "fs")]
pub use tree::{blake3_tree, TreeHashOptions};

/// The number of bytes in a BLAKE3 content digest.
pub const BLAKE3_DIGEST_LENGTH: usize = 32;

const READ_BUFFER_LENGTH: usize = 64 * 1024;

/// Input size at or above which the memory-mapped path hashes in parallel.
///
/// Below this, thread hand-off costs more than it saves. The threshold matches
/// the one first-party clients measured (~4x on a four-core runner for a 100 MB
/// binary), so migrating onto this facade preserves their throughput rather
/// than silently dropping to a single core.
const PARALLEL_HASH_THRESHOLD_BYTES: u64 = 128 * 1024;

/// An opaque, kernel-owned BLAKE3 content digest.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Blake3Digest([u8; BLAKE3_DIGEST_LENGTH]);

impl Blake3Digest {
    /// Wrap a canonical 32-byte digest encoding.
    ///
    /// This is infallible: every fixed-size array is a valid digest
    /// encoding. Use this to read a digest back out of persisted cache
    /// metadata that already carries a correct-length array.
    pub const fn from_bytes(bytes: [u8; BLAKE3_DIGEST_LENGTH]) -> Self {
        Self(bytes)
    }

    /// Borrow the canonical 32-byte digest encoding.
    pub fn as_bytes(&self) -> &[u8; BLAKE3_DIGEST_LENGTH] {
        &self.0
    }

    /// Render the canonical lower-case hexadecimal encoding.
    pub fn to_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";

        let mut rendered = String::with_capacity(BLAKE3_DIGEST_LENGTH * 2);
        for byte in self.0 {
            rendered.push(HEX[usize::from(byte >> 4)] as char);
            rendered.push(HEX[usize::from(byte & 0x0f)] as char);
        }
        rendered
    }

    /// Parse the canonical hexadecimal encoding produced by [`Self::to_hex`].
    ///
    /// Both lower- and upper-case hexadecimal digits are accepted. The input
    /// must be exactly `BLAKE3_DIGEST_LENGTH * 2` characters.
    pub fn from_hex(hex: &str) -> Result<Self, Blake3HexDecodeError> {
        let hex = hex.as_bytes();
        if hex.len() != BLAKE3_DIGEST_LENGTH * 2 {
            return Err(Blake3HexDecodeError::InvalidLength { length: hex.len() });
        }

        let mut bytes = [0_u8; BLAKE3_DIGEST_LENGTH];
        for (target, pair) in bytes.iter_mut().zip(hex.as_chunks::<2>().0) {
            let high = hex_digit(pair[0]).ok_or(Blake3HexDecodeError::InvalidCharacter)?;
            let low = hex_digit(pair[1]).ok_or(Blake3HexDecodeError::InvalidCharacter)?;
            *target = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for Blake3Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl std::str::FromStr for Blake3Digest {
    type Err = Blake3HexDecodeError;

    fn from_str(hex: &str) -> Result<Self, Self::Err> {
        Self::from_hex(hex)
    }
}

/// Failure parsing a [`Blake3Digest`] from its canonical hexadecimal
/// encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Blake3HexDecodeError {
    /// The input was not exactly `BLAKE3_DIGEST_LENGTH * 2` bytes long.
    #[error("BLAKE3 hex digest must be {} characters, got {length}", BLAKE3_DIGEST_LENGTH * 2)]
    InvalidLength {
        /// The number of bytes actually supplied.
        length: usize,
    },
    /// The input contained a byte outside `[0-9a-fA-F]`.
    #[error("BLAKE3 hex digest contained a non-hexadecimal character")]
    InvalidCharacter,
}

/// Hash an in-memory byte slice with BLAKE3.
pub fn blake3_bytes(bytes: &[u8]) -> Blake3Digest {
    Blake3Digest(*blake3::hash(bytes).as_bytes())
}

/// An incremental BLAKE3 hasher.
///
/// Feed input through repeated [`Self::update`] calls -- from many buffers,
/// without concatenating them into one allocation -- then call
/// [`Self::finalize`] to compute the digest of everything hashed so far.
/// `Blake3Hasher` also implements [`std::io::Write`] so it composes with
/// ordinary reader/writer plumbing.
#[derive(Clone)]
pub struct Blake3Hasher(blake3::Hasher);

impl Blake3Hasher {
    /// Create a hasher using the standard BLAKE3 hash function.
    pub fn new() -> Self {
        Self(blake3::Hasher::new())
    }

    /// Create a hasher using BLAKE3's key-derivation mode, domain-separated
    /// by `context`.
    ///
    /// `context` should be a hardcoded, globally unique, application-specific
    /// string -- for example `"kernal-api 2026-01-01 12:00:00 cache key
    /// derivation"`. Two hashers created with different `context` values
    /// never produce the same digest for the same input, which is the
    /// derive-key contract this constructor preserves for callers that
    /// already depend on it to reproduce existing derived keys.
    pub fn new_derive_key(context: &str) -> Self {
        Self(blake3::Hasher::new_derive_key(context))
    }

    /// Feed more bytes into the running digest.
    /// Hash `bytes` across the shared thread pool.
    ///
    /// Kept private: the facade decides when parallelism pays, so callers get
    /// the fast path from [`Blake3ReadOptions::memory_map`] without having to
    /// reason about a threshold themselves.
    pub(crate) fn update_parallel(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update_rayon(bytes);
        self
    }

    pub fn update(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update(bytes);
        self
    }

    /// Compute the digest of every byte fed so far.
    ///
    /// This does not consume or reset the hasher: further [`Self::update`]
    /// calls continue accumulating from the same running state, and calling
    /// this again later returns the digest of the combined input.
    pub fn finalize(&self) -> Blake3Digest {
        Blake3Digest(*self.0.finalize().as_bytes())
    }
}

impl Default for Blake3Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Blake3Hasher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Blake3Hasher")
            .finish_non_exhaustive()
    }
}

impl io::Write for Blake3Hasher {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Bounds for a synchronous reader or file hashing operation.
///
/// These operations are synchronous and intentionally do not introduce an
/// async cancellation contract. Run them through [`crate::async_engine`]'s
/// blocking lane when an async caller needs scheduling isolation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Blake3ReadOptions {
    maximum_bytes: Option<u64>,
    memory_map: bool,
}

impl Blake3ReadOptions {
    /// Create an unbounded reader/file hashing policy that reads through the
    /// caller-provided reader or file handle.
    pub const fn new() -> Self {
        Self {
            maximum_bytes: None,
            memory_map: false,
        }
    }

    /// Reject input that contains more than `maximum_bytes` bytes.
    pub const fn maximum_bytes(mut self, maximum_bytes: u64) -> Self {
        self.maximum_bytes = Some(maximum_bytes);
        self
    }

    /// Return the configured input limit, if any.
    pub const fn maximum_byte_count(self) -> Option<u64> {
        self.maximum_bytes
    }

    /// Request a memory-mapped read for [`blake3_file`] instead of a
    /// buffered stream read.
    ///
    /// This is a performance request, not a guarantee: [`blake3_file`] falls
    /// back to its ordinary buffered read whenever mapping does not apply
    /// (an empty file) or the operating system fails to establish the
    /// mapping. It has no effect on [`blake3_reader`], which is never given a
    /// file handle to map.
    ///
    /// # Preconditions
    ///
    /// The mapped file must not be modified or truncated by another process
    /// or thread while this call is hashing it. The mapping observes the
    /// file's live backing storage: concurrent mutation can be read back as
    /// torn content, and a concurrent truncation can fault the process
    /// (`SIGBUS` on Unix, an access violation on Windows) instead of failing
    /// cleanly. Callers that need change detection, and every caller that
    /// cannot guarantee exclusive access while hashing, must compare file
    /// metadata before and after hashing rather than relying on this call to
    /// report a concurrent modification as an error.
    pub const fn memory_map(mut self, memory_map: bool) -> Self {
        self.memory_map = memory_map;
        self
    }

    /// Return whether a memory-mapped read was requested.
    pub const fn memory_map_requested(self) -> bool {
        self.memory_map
    }
}

/// Stable category for a reader or file hashing failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Blake3HashErrorKind {
    /// Opening a requested file failed.
    Open,
    /// Reading requested content failed.
    Read,
    /// The input contained more bytes than the configured maximum.
    SizeLimitExceeded {
        /// The configured maximum number of input bytes.
        maximum_bytes: u64,
    },
}

/// Failure from a reader or file BLAKE3 operation.
#[derive(Debug)]
pub struct Blake3HashError {
    kind: Blake3HashErrorKind,
    source: Option<io::Error>,
}

impl Blake3HashError {
    fn open(source: io::Error) -> Self {
        Self {
            kind: Blake3HashErrorKind::Open,
            source: Some(source),
        }
    }

    fn read(source: io::Error) -> Self {
        Self {
            kind: Blake3HashErrorKind::Read,
            source: Some(source),
        }
    }

    fn size_limit_exceeded(maximum_bytes: u64) -> Self {
        Self {
            kind: Blake3HashErrorKind::SizeLimitExceeded { maximum_bytes },
            source: None,
        }
    }

    /// Return the stable semantic failure category.
    pub const fn kind(&self) -> Blake3HashErrorKind {
        self.kind
    }

    /// Return the native I/O category when an open or read caused the error.
    pub fn io_error_kind(&self) -> Option<io::ErrorKind> {
        self.source.as_ref().map(io::Error::kind)
    }
}

impl fmt::Display for Blake3HashError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            Blake3HashErrorKind::Open => formatter.write_str("failed to open BLAKE3 input"),
            Blake3HashErrorKind::Read => formatter.write_str("failed to read BLAKE3 input"),
            Blake3HashErrorKind::SizeLimitExceeded { maximum_bytes } => {
                write!(
                    formatter,
                    "BLAKE3 input exceeds the {maximum_bytes}-byte limit"
                )
            }
        }
    }
}

impl std::error::Error for Blake3HashError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

/// Hash content from a blocking byte reader with BLAKE3.
///
/// When a maximum is configured, the operation reads at most one byte beyond
/// the accepted prefix to distinguish EOF from an oversized input. That byte
/// is never incorporated into the returned digest. This function never maps
/// memory: [`Blake3ReadOptions::memory_map`] only affects [`blake3_file`].
pub fn blake3_reader(
    mut reader: impl Read,
    options: Blake3ReadOptions,
) -> Result<Blake3Digest, Blake3HashError> {
    let mut hasher = Blake3Hasher::new();
    let mut buffer = [0_u8; READ_BUFFER_LENGTH];
    let mut bytes_hashed = 0_u64;

    loop {
        let read_length = next_read_length(options.maximum_bytes, bytes_hashed);
        let read = reader
            .read(&mut buffer[..read_length])
            .map_err(Blake3HashError::read)?;
        if read == 0 {
            return Ok(hasher.finalize());
        }

        let read = u64::try_from(read).expect("buffer length always fits in u64");
        if let Some(maximum_bytes) = options.maximum_bytes {
            if read > maximum_bytes.saturating_sub(bytes_hashed) {
                return Err(Blake3HashError::size_limit_exceeded(maximum_bytes));
            }
            bytes_hashed += read;
        }

        hasher.update(&buffer[..usize::try_from(read).expect("read length fits usize")]);
    }
}

/// Hash a file's content with BLAKE3.
///
/// Honors [`Blake3ReadOptions::memory_map`]: when requested, the file is
/// mapped and hashed in one pass instead of streamed through the ordinary
/// buffered reader, falling back to the buffered path whenever mapping does
/// not apply.
pub fn blake3_file(
    path: impl AsRef<Path>,
    options: Blake3ReadOptions,
) -> Result<Blake3Digest, Blake3HashError> {
    let file = std::fs::File::open(path).map_err(Blake3HashError::open)?;
    let mapped_digest = if options.memory_map {
        blake3_file_memory_mapped(&file, options)?
    } else {
        None
    };
    match mapped_digest {
        Some(digest) => Ok(digest),
        None => blake3_reader(file, options),
    }
}

/// Hash an already-open file's content with BLAKE3.
///
/// Identical to [`blake3_file`] except that the caller supplies the handle.
/// A caller that has already opened and stated the file -- to compare
/// metadata before and after hashing, say -- would otherwise have to reopen
/// and restate it just to reach the mapped, parallel path, which is both
/// slower and a TOCTOU widening: the reopened path can resolve to a
/// different inode than the handle it already holds.
///
/// Honors [`Blake3ReadOptions::memory_map`] exactly as [`blake3_file`] does,
/// including the parallel hash above the internal threshold and the fallback
/// to the buffered read whenever mapping does not apply.
///
/// # Preconditions
///
/// The mapped path reads the whole file regardless of the handle's cursor,
/// but the buffered fallback reads from the current position. Pass a handle
/// positioned at byte zero so both paths return the same digest.
///
/// # Errors
///
/// Returns [`Blake3HashError`] when the file cannot be stated or read, or
/// when a configured [`Blake3ReadOptions::maximum_bytes`] is exceeded.
pub fn blake3_open_file(
    file: &std::fs::File,
    options: Blake3ReadOptions,
) -> Result<Blake3Digest, Blake3HashError> {
    let mapped_digest = if options.memory_map {
        blake3_file_memory_mapped(file, options)?
    } else {
        None
    };
    match mapped_digest {
        Some(digest) => Ok(digest),
        None => blake3_reader(file, options),
    }
}

/// Hash an already-open file by memory-mapping it, honoring the configured
/// size limit against the file's exact length.
///
/// Returns `Ok(None)` when the mapping does not apply -- the file is empty,
/// since `memmap2` cannot map a zero-length file, or the operating system
/// refused to establish the mapping. The caller falls back to the ordinary
/// buffered path in both cases, which is what
/// [`Blake3ReadOptions::memory_map`] documents as its performance-request
/// contract. A configured size limit is still enforced against the file's
/// exact length before any mapping is attempted, so an oversized input is
/// rejected rather than silently re-read.
fn blake3_file_memory_mapped(
    file: &std::fs::File,
    options: Blake3ReadOptions,
) -> Result<Option<Blake3Digest>, Blake3HashError> {
    let length = file.metadata().map_err(Blake3HashError::read)?.len();
    if length == 0 {
        return Ok(None);
    }
    if let Some(maximum_bytes) = options.maximum_bytes {
        if length > maximum_bytes {
            return Err(Blake3HashError::size_limit_exceeded(maximum_bytes));
        }
    }

    // SAFETY: `memmap2::Mmap::map` requires that the mapped file not be
    // truncated or otherwise modified for the lifetime of the mapping, or
    // the process may fault (SIGBUS on Unix, an access violation on
    // Windows) instead of the OS returning bytes. `Blake3ReadOptions::
    // memory_map`'s documentation makes that precondition part of its
    // public opt-in contract, so a caller that requests it has already
    // accepted the risk. `file` is a live, already-open handle borrowed for
    // the duration of this call, so it outlives the mapping created from
    // it. The mapping is read exactly once, immediately, to feed the
    // hasher below, and is dropped at the end of this function without
    // being retained, resized, or written through.
    let Ok(mapped) = (unsafe { memmap2::Mmap::map(file) }) else {
        return Ok(None);
    };
    let mut hasher = Blake3Hasher::new();
    if length >= PARALLEL_HASH_THRESHOLD_BYTES {
        hasher.update_parallel(&mapped);
    } else {
        hasher.update(&mapped);
    }
    Ok(Some(hasher.finalize()))
}

fn next_read_length(maximum_bytes: Option<u64>, bytes_hashed: u64) -> usize {
    let Some(maximum_bytes) = maximum_bytes else {
        return READ_BUFFER_LENGTH;
    };

    let remaining = maximum_bytes.saturating_sub(bytes_hashed);
    let one_more_than_remaining =
        remaining.min(u64::try_from(READ_BUFFER_LENGTH - 1).expect("buffer length fits u64")) + 1;
    usize::try_from(one_more_than_remaining).expect("bounded buffer length fits usize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::str::FromStr as _;

    #[test]
    fn blake3_bytes_is_deterministic_and_input_sensitive() {
        assert_eq!(blake3_bytes(b"hello world"), blake3_bytes(b"hello world"));
        assert_ne!(blake3_bytes(b"hello"), blake3_bytes(b"world"));
    }

    #[test]
    fn digest_round_trips_through_bytes() {
        let digest = blake3_bytes(b"round trip");
        let round_tripped = Blake3Digest::from_bytes(*digest.as_bytes());
        assert_eq!(digest, round_tripped);
    }

    #[test]
    fn digest_round_trips_through_hex() {
        let digest = blake3_bytes(b"hex round trip");
        let hex = digest.to_hex();
        assert_eq!(Blake3Digest::from_hex(&hex).expect("valid hex"), digest);
        assert_eq!(Blake3Digest::from_str(&hex).expect("valid hex"), digest);
        assert_eq!(
            Blake3Digest::from_hex(&hex.to_uppercase()).expect("uppercase hex accepted"),
            digest
        );
    }

    #[test]
    fn digest_from_hex_rejects_wrong_length() {
        let error = Blake3Digest::from_hex("ab").expect_err("too short");
        assert_eq!(error, Blake3HexDecodeError::InvalidLength { length: 2 });

        let too_long = "a".repeat(BLAKE3_DIGEST_LENGTH * 2 + 1);
        let error = Blake3Digest::from_hex(&too_long).expect_err("too long");
        assert_eq!(
            error,
            Blake3HexDecodeError::InvalidLength {
                length: BLAKE3_DIGEST_LENGTH * 2 + 1
            }
        );
    }

    #[test]
    fn digest_from_hex_rejects_non_hex_characters() {
        let mut invalid = "0".repeat(BLAKE3_DIGEST_LENGTH * 2);
        invalid.replace_range(0..1, "g");
        let error = Blake3Digest::from_hex(&invalid).expect_err("non-hex character");
        assert_eq!(error, Blake3HexDecodeError::InvalidCharacter);
    }

    #[test]
    fn hasher_matches_blake3_bytes_for_a_single_update() {
        let mut hasher = Blake3Hasher::new();
        hasher.update(b"streamed input");
        assert_eq!(hasher.finalize(), blake3_bytes(b"streamed input"));
    }

    #[test]
    fn hasher_matches_blake3_bytes_across_many_chunked_updates() {
        let payload: Vec<u8> = (0..(200 * 1024)).map(|index| (index % 251) as u8).collect();
        let mut hasher = Blake3Hasher::new();
        for chunk in payload.chunks(7 * 1024 + 1) {
            hasher.update(chunk);
        }
        assert_eq!(hasher.finalize(), blake3_bytes(&payload));
    }

    #[test]
    fn hasher_finalize_does_not_consume_or_reset_the_running_state() {
        let mut hasher = Blake3Hasher::new();
        hasher.update(b"first");
        let first_digest = hasher.finalize();
        assert_eq!(
            hasher.finalize(),
            first_digest,
            "finalize must be repeatable"
        );

        hasher.update(b"second");
        let combined_digest = hasher.finalize();
        assert_eq!(combined_digest, blake3_bytes(b"firstsecond"));
        assert_ne!(combined_digest, first_digest);
    }

    #[test]
    fn hasher_write_matches_update() {
        let mut via_write = Blake3Hasher::new();
        via_write
            .write_all(b"written through io::Write")
            .expect("write never fails for an in-memory hasher");

        let mut via_update = Blake3Hasher::new();
        via_update.update(b"written through io::Write");

        assert_eq!(via_write.finalize(), via_update.finalize());
    }

    #[test]
    fn hasher_default_matches_new() {
        let mut default_hasher = Blake3Hasher::default();
        let mut new_hasher = Blake3Hasher::new();
        default_hasher.update(b"same input");
        new_hasher.update(b"same input");
        assert_eq!(default_hasher.finalize(), new_hasher.finalize());
    }

    #[test]
    fn derive_key_is_domain_separated_and_differs_from_plain_hashing() {
        let mut context_a_first = Blake3Hasher::new_derive_key("context-a");
        context_a_first.update(b"same input");
        let mut context_a_second = Blake3Hasher::new_derive_key("context-a");
        context_a_second.update(b"same input");
        assert_eq!(
            context_a_first.finalize(),
            context_a_second.finalize(),
            "the same context must be deterministic"
        );

        let mut context_b = Blake3Hasher::new_derive_key("context-b");
        context_b.update(b"same input");
        assert_ne!(
            context_a_first.finalize(),
            context_b.finalize(),
            "different contexts must derive different keys"
        );

        let mut plain = Blake3Hasher::new();
        plain.update(b"same input");
        assert_ne!(
            context_a_first.finalize(),
            plain.finalize(),
            "derive-key mode must differ from plain hashing"
        );
    }

    #[test]
    fn blake3_reader_matches_blake3_bytes() {
        let payload = b"reader payload".to_vec();
        let digest = blake3_reader(payload.as_slice(), Blake3ReadOptions::new())
            .expect("unbounded reader hashing succeeds");
        assert_eq!(digest, blake3_bytes(&payload));
    }

    #[test]
    fn blake3_reader_enforces_the_maximum_byte_count() {
        let payload = vec![0_u8; 100];
        let error = blake3_reader(
            payload.as_slice(),
            Blake3ReadOptions::new().maximum_bytes(99),
        )
        .expect_err("oversized input must be rejected");
        assert_eq!(
            error.kind(),
            Blake3HashErrorKind::SizeLimitExceeded { maximum_bytes: 99 }
        );
    }

    #[test]
    fn read_options_default_matches_new() {
        assert_eq!(Blake3ReadOptions::default(), Blake3ReadOptions::new());
        assert_eq!(Blake3ReadOptions::new().maximum_byte_count(), None);
        assert!(!Blake3ReadOptions::new().memory_map_requested());
        assert!(Blake3ReadOptions::new()
            .memory_map(true)
            .memory_map_requested());
    }

    #[test]
    fn blake3_file_matches_blake3_bytes_without_memory_map() {
        let temporary = tempfile::NamedTempFile::new().expect("temporary file");
        let payload = b"file payload without memory mapping".to_vec();
        std::fs::write(temporary.path(), &payload).expect("write fixture file");

        let digest =
            blake3_file(temporary.path(), Blake3ReadOptions::new()).expect("buffered file hash");
        assert_eq!(digest, blake3_bytes(&payload));
    }

    #[test]
    fn blake3_file_matches_blake3_bytes_with_memory_map() {
        let temporary = tempfile::NamedTempFile::new().expect("temporary file");
        // Comfortably larger than one read buffer so the mapped path is
        // exercised over more than a single page.
        let payload: Vec<u8> = (0..(256 * 1024)).map(|index| (index % 251) as u8).collect();
        std::fs::write(temporary.path(), &payload).expect("write fixture file");

        let digest = blake3_file(temporary.path(), Blake3ReadOptions::new().memory_map(true))
            .expect("memory-mapped file hash");
        assert_eq!(digest, blake3_bytes(&payload));
    }

    #[test]
    fn blake3_file_memory_map_falls_back_for_an_empty_file() {
        let temporary = tempfile::NamedTempFile::new().expect("temporary file");

        let digest = blake3_file(temporary.path(), Blake3ReadOptions::new().memory_map(true))
            .expect("empty file falls back to the buffered path");
        assert_eq!(digest, blake3_bytes(b""));
    }

    #[test]
    fn blake3_file_memory_map_enforces_the_maximum_byte_count() {
        let temporary = tempfile::NamedTempFile::new().expect("temporary file");
        let payload = vec![7_u8; 4096];
        std::fs::write(temporary.path(), &payload).expect("write fixture file");
        let payload_length = u64::try_from(payload.len()).expect("fixture length fits u64");

        let error = blake3_file(
            temporary.path(),
            Blake3ReadOptions::new()
                .memory_map(true)
                .maximum_bytes(payload_length - 1),
        )
        .expect_err("oversized mapped file must be rejected");
        assert_eq!(
            error.kind(),
            Blake3HashErrorKind::SizeLimitExceeded {
                maximum_bytes: payload_length - 1
            }
        );

        let digest = blake3_file(
            temporary.path(),
            Blake3ReadOptions::new()
                .memory_map(true)
                .maximum_bytes(payload_length),
        )
        .expect("input at exactly the limit is accepted");
        assert_eq!(digest, blake3_bytes(&payload));
    }

    #[test]
    fn blake3_file_reports_a_missing_file_as_an_open_failure() {
        let error = blake3_file(
            "kernal-api-hash-fixture-that-does-not-exist",
            Blake3ReadOptions::new(),
        )
        .expect_err("missing file must fail to open");
        assert_eq!(error.kind(), Blake3HashErrorKind::Open);
    }
}
