//! Test-gated native experiment. No guest resource or public API yet.
use std::fs::File;
use std::io::{self, Seek, Write};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use openssl::symm::{Cipher, Crypter, Mode};

const CHUNK: usize = 64 * 1024;

#[cfg(test)]
#[path = "authenticated_reader_tests.rs"]
pub(crate) mod reader_tests;

#[derive(Clone, Debug)]
pub(crate) struct StagingBudget(Arc<StagingBudgetState>);

#[derive(Debug)]
struct StagingBudgetState {
    maximum: u64,
    used: AtomicU64,
}

impl StagingBudget {
    pub(crate) fn new(maximum: u64) -> Self {
        Self(Arc::new(StagingBudgetState {
            maximum,
            used: AtomicU64::new(0),
        }))
    }

    pub(crate) fn used(&self) -> u64 {
        self.0.used.load(Ordering::SeqCst)
    }

    fn reserve(&self, bytes: u64) -> io::Result<Reservation> {
        self.0
            .used
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                current
                    .checked_add(bytes)
                    .filter(|next| *next <= self.0.maximum)
            })
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "staging quota exhausted"))?;
        Ok(Reservation {
            budget: self.clone(),
            bytes,
        })
    }
}

#[test]
fn shared_staging_budget_has_one_ceiling_under_contention() {
    let budget = StagingBudget::new(16);
    let acquired = std::sync::Barrier::new(9);
    let release = std::sync::Barrier::new(9);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let budget = budget.clone();
                let acquired = &acquired;
                let release = &release;
                scope.spawn(move || {
                    let reservation = budget.reserve(8);
                    acquired.wait();
                    release.wait();
                    let accepted = reservation.is_ok();
                    drop(reservation);
                    accepted
                })
            })
            .collect();
        acquired.wait();
        let retained = budget.used();
        release.wait();
        let accepted = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|accepted| *accepted)
            .count();
        assert_eq!(retained, 16);
        assert_eq!(accepted, 2);
    });
    assert_eq!(budget.used(), 0);
    let reservation = budget.reserve(16).unwrap();
    assert!(budget.clone().reserve(1).is_err());
    drop(reservation);
    assert_eq!(budget.used(), 0);

    let maximum = StagingBudget::new(u64::MAX);
    let reservation = maximum.reserve(u64::MAX).unwrap();
    assert!(maximum.reserve(1).is_err());
    drop(reservation);
    assert_eq!(maximum.used(), 0);
}

#[test]
fn encrypted_large_zip_reuses_bounded_extractor_only_after_authentication() {
    use std::io::Read;
    const LENGTH: u64 = 17 * 1024 * 1024;
    // Success, bad tag, per-entry limit, entry-count limit, and unsafe path.
    for case in 0..5 {
        let mut writer = zip::ZipWriter::new(tempfile::tempfile().unwrap());
        let name = if case == 4 { "../escaped" } else { "payload" };
        writer
            .start_file(
                name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        let chunk = [0x5a; CHUNK];
        for _ in 0..LENGTH / CHUNK as u64 {
            writer.write_all(&chunk).unwrap();
        }
        let mut source = writer.finish().unwrap();
        let length = source.metadata().unwrap().len();
        assert!(length > 16 * 1024 * 1024);
        source.rewind().unwrap();
        let used = StagingBudget::new(length);
        let key = [13; 16];
        let nonce = [17; 12];
        let aad = b"synthetic ZIP envelope";
        let mut pending = Authentication::begin(&key, &nonce, aad, length, &used).unwrap();
        let mut encoder =
            Crypter::new(Cipher::aes_128_gcm(), Mode::Encrypt, &key, Some(&nonce)).unwrap();
        encoder.aad_update(aad).unwrap();
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("output");
        let mut input = [0; CHUNK];
        let mut ciphertext = [0; CHUNK + 16];
        loop {
            let read = source.read(&mut input).unwrap();
            if read == 0 {
                break;
            }
            let count = encoder.update(&input[..read], &mut ciphertext).unwrap();
            pending.update(&ciphertext[..count]).unwrap();
            assert!(!output.exists());
            assert_eq!(used.used(), length);
        }
        assert_eq!(encoder.finalize(&mut ciphertext).unwrap(), 0);
        let mut tag = [0; 16];
        encoder.get_tag(&mut tag).unwrap();
        if case == 1 {
            tag[0] ^= 1;
        }
        let authenticated = pending.authenticate(&tag);
        if case == 1 {
            assert_eq!(
                authenticated.unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert!(!output.exists());
        } else {
            let limits = super::ExtractionLimits {
                max_input_bytes: length,
                max_entry_bytes: if case == 2 { LENGTH - 1 } else { LENGTH },
                max_output_bytes: LENGTH,
                max_entries: if case == 3 { 0 } else { 1 },
                ..super::ExtractionLimits::default()
            };
            let result = authenticated.unwrap().extract(&output, limits);
            if case == 0 {
                result.unwrap();
                let mut file = File::open(output.join("payload")).unwrap();
                let mut total = 0;
                loop {
                    let count = file.read(&mut input).unwrap();
                    if count == 0 {
                        break;
                    }
                    assert!(input[..count].iter().all(|byte| *byte == 0x5a));
                    total += count as u64;
                }
                assert_eq!(total, LENGTH);
            } else {
                assert!(result.is_err());
                assert!(!output.join("payload").exists());
                assert!(!root.path().join("escaped").exists());
            }
        }
        assert_eq!(used.used(), 0);
    }
}

#[derive(Debug)]
struct Reservation {
    budget: StagingBudget,
    bytes: u64,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.0.used.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}
#[derive(Debug)]
pub(crate) struct Authenticated {
    file: File,
    _reservation: Reservation,
}

impl Authenticated {
    pub(crate) fn into_reader(
        self,
        limits: super::ExtractionLimits,
    ) -> io::Result<AuthenticatedReader> {
        let Self {
            file,
            _reservation: reservation,
        } = self;
        Ok(AuthenticatedReader {
            reader: super::zip_reader::Reader::new(file, limits)?,
            _reservation: reservation,
        })
    }

    #[cfg(any(feature = "wasm-sketch-host", feature = "tauri-webview"))]
    pub(crate) fn belongs_to(&self, budget: &StagingBudget) -> bool {
        Arc::ptr_eq(&self._reservation.budget.0, &budget.0)
    }

    pub(crate) fn extract(
        self,
        destination: &std::path::Path,
        limits: super::ExtractionLimits,
    ) -> io::Result<()> {
        let Self {
            file,
            _reservation: reservation,
        } = self;
        let result = super::extract_file(file, destination, super::ArchiveFormat::Zip, limits);
        // Keep staged storage charged until the extractor has closed its
        // source, including every error path. No source pathname is reopened.
        drop(reservation);
        result
    }
}

pub(crate) struct AuthenticatedReader {
    // Declaration order closes the ZIP/file before releasing its charge.
    reader: super::zip_reader::Reader,
    _reservation: Reservation,
}

impl AuthenticatedReader {
    pub(crate) fn entry(&mut self, index: usize) -> io::Result<Option<super::zip_reader::Entry>> {
        self.reader.entry(index)
    }

    pub(crate) fn copy_entry(&mut self, index: usize, sink: &mut impl Write) -> io::Result<u64> {
        self.reader.copy_entry(index, sink)
    }
}

struct Pending {
    file: File,
    cipher: Crypter,
    expected: u64,
    written: u64,
    reservation: Reservation,
}

// There is intentionally no read, seek, path, or file accessor on this type.
pub(crate) struct Authentication {
    pending: Option<Pending>,
}

impl Authentication {
    pub(crate) fn begin(
        key: &[u8; 16],
        nonce: &[u8; 12],
        aad: &[u8],
        expected: u64,
        budget: &StagingBudget,
    ) -> io::Result<Self> {
        if aad.len() > 16 * 1024 + 12 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let reservation = budget.reserve(expected)?;
        let mut cipher = Crypter::new(Cipher::aes_128_gcm(), Mode::Decrypt, key, Some(nonce))
            .map_err(io::Error::other)?;
        cipher.aad_update(aad).map_err(io::Error::other)?;
        Ok(Self {
            pending: Some(Pending {
                file: tempfile::tempfile()?,
                cipher,
                expected,
                written: 0,
                reservation,
            }),
        })
    }

    pub(crate) fn update(&mut self, ciphertext: &[u8]) -> io::Result<()> {
        // Taking ownership poisons the operation on any error, closing its
        // private staging even when the caller retains this failed wrapper.
        let mut state = self.pending.take().ok_or(io::ErrorKind::BrokenPipe)?;
        if ciphertext.len() > CHUNK || ciphertext.len() as u64 > state.expected - state.written {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut plaintext = [0; CHUNK + 16];
        let count = state
            .cipher
            .update(ciphertext, &mut plaintext)
            .map_err(io::Error::other)?;
        if count != ciphertext.len() {
            return Err(io::Error::other("unexpected GCM output length"));
        }
        state.file.write_all(&plaintext[..count])?;
        state.written += count as u64;
        self.pending = Some(state);
        Ok(())
    }

    pub(crate) fn authenticate(mut self, tag: &[u8; 16]) -> io::Result<Authenticated> {
        let mut state = self.pending.take().ok_or(io::ErrorKind::BrokenPipe)?;
        if state.written != state.expected {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        state.cipher.set_tag(tag).map_err(io::Error::other)?;
        let mut tail = [0; 16];
        let count = state
            .cipher
            .finalize(&mut tail)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "authentication failed"))?;
        if count != 0 {
            return Err(io::Error::other("unexpected GCM final output"));
        }
        state.file.flush()?;
        state.file.rewind()?;
        Ok(Authenticated {
            file: state.file,
            _reservation: state.reservation,
        })
    }
}

#[test]
fn nist_single_message_vector_requires_valid_tag_before_returning_file() {
    use std::io::Read;
    let ciphertext = [
        0x03, 0x88, 0xda, 0xce, 0x60, 0xb6, 0xa3, 0x92, 0xf3, 0x28, 0xc2, 0xb9, 0x71, 0xb2, 0xfe,
        0x78,
    ];
    let tag = [
        0xab, 0x6e, 0x47, 0xd4, 0x2c, 0xec, 0x13, 0xbd, 0xf5, 0x3a, 0x67, 0xb2, 0x12, 0x57, 0xbd,
        0xdf,
    ];
    for corrupt in [false, true] {
        let used = StagingBudget::new(16);
        let mut pending = Authentication::begin(&[0; 16], &[0; 12], &[], 16, &used).unwrap();
        assert_eq!(used.used(), 16);
        assert!(Authentication::begin(&[0; 16], &[0; 12], &[], 1, &used).is_err());
        pending.update(&ciphertext[..7]).unwrap();
        pending.update(&ciphertext[7..]).unwrap();
        let mut supplied = tag;
        supplied[0] ^= u8::from(corrupt);
        let result = pending.authenticate(&supplied);
        if corrupt {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        } else {
            let mut plain = [1; 16];
            result.unwrap().file.read_exact(&mut plain).unwrap();
            assert_eq!(plain, [0; 16]);
        }
        assert_eq!(used.used(), 0);
    }
}

#[test]
fn quota_and_update_errors_never_leave_pending_staging() {
    let used = StagingBudget::new(16);
    assert!(Authentication::begin(&[0; 16], &[0; 12], &[], 17, &used).is_err());
    let mut pending = Authentication::begin(&[0; 16], &[0; 12], &[], 16, &used).unwrap();
    assert!(pending.update(&[0; 17]).is_err());
    assert!(pending.pending.is_none());
    assert_eq!(used.used(), 0);
    assert_eq!(
        pending.update(&[]).unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    let pending = Authentication::begin(&[0; 16], &[0; 12], &[], 16, &used).unwrap();
    assert_eq!(
        pending.authenticate(&[0; 16]).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
    assert_eq!(used.used(), 0);
    drop(Authentication::begin(&[0; 16], &[0; 12], &[], 16, &used).unwrap());
    assert_eq!(used.used(), 0);
}

#[test]
fn large_single_message_authenticates_aad_ciphertext_and_tag_with_bounded_updates() {
    use std::io::{Read, SeekFrom};
    const LENGTH: u64 = 17 * 1024 * 1024;
    for mutation in 0..4 {
        let used = StagingBudget::new(LENGTH);
        let key = [7; 16];
        let nonce = [9; 12];
        let aad = b"synthetic envelope";
        let supplied_aad: &[u8] = if mutation == 1 {
            b"different envelope"
        } else {
            aad
        };
        let mut pending = Authentication::begin(&key, &nonce, supplied_aad, LENGTH, &used).unwrap();
        let mut encoder =
            Crypter::new(Cipher::aes_128_gcm(), Mode::Encrypt, &key, Some(&nonce)).unwrap();
        encoder.aad_update(aad).unwrap();
        let plain = [0x5a; CHUNK];
        let mut encrypted = [0; CHUNK + 16];
        for index in 0..LENGTH / CHUNK as u64 {
            let count = encoder.update(&plain, &mut encrypted).unwrap();
            if mutation == 2 && index == 0 {
                encrypted[0] ^= 1;
            }
            pending.update(&encrypted[..count]).unwrap();
            assert_eq!(used.used(), LENGTH);
        }
        assert_eq!(encoder.finalize(&mut encrypted).unwrap(), 0);
        let mut tag = [0; 16];
        encoder.get_tag(&mut tag).unwrap();
        if mutation == 3 {
            tag[0] ^= 1;
        }
        let result = pending.authenticate(&tag);
        if mutation == 0 {
            let mut authenticated = result.unwrap();
            assert_eq!(used.used(), LENGTH);
            assert_eq!(authenticated.file.metadata().unwrap().len(), LENGTH);
            authenticated
                .file
                .seek(SeekFrom::Start(LENGTH - CHUNK as u64))
                .unwrap();
            let mut tail = [0; CHUNK];
            authenticated.file.read_exact(&mut tail).unwrap();
            assert_eq!(tail, plain);
        } else {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
        assert_eq!(used.used(), 0);
    }
}
