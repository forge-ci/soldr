#![cfg(feature = "hash-sha256")]

use kernal_api::hash::{sha256_bytes, sha256_file, sha256_reader, Sha256Hasher};
use std::io::{self, Read};

#[test]
fn sha256_known_vectors_and_incremental_chunks() {
    for (bytes, expected) in [
        (
            &b""[..],
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        (
            &b"abc"[..],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
    ] {
        assert_eq!(sha256_bytes(bytes).to_hex(), expected);
        let mut state = Sha256Hasher::new();
        for byte in bytes {
            state.update([*byte]);
        }
        assert_eq!(format!("{:x}", state.finalize()), expected);
    }
    let bytes = vec![b'a'; 1_000_000];
    let expected = "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0";
    for chunk_size in [1, 63, 64, 65, 65536] {
        let mut state = Sha256Hasher::new();
        for chunk in bytes.chunks(chunk_size) {
            state.update(chunk);
        }
        assert_eq!(state.finalize().to_hex(), expected);
    }
    assert_eq!(
        sha256_reader(&bytes[..], 1_000_000).unwrap().to_hex(),
        expected
    );
}

#[test]
fn sha256_stream_limits_and_file_errors() {
    assert!(sha256_reader(&b"abc"[..], 2).is_err());
    assert!(sha256_reader(&b"x"[..], 0).is_err());
    assert_eq!(sha256_reader(io::empty(), 0).unwrap(), sha256_bytes(b""));
    assert_eq!(
        sha256_reader(&b"abc"[..], u64::MAX).unwrap(),
        sha256_bytes(b"abc")
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("artifact");
    assert!(sha256_file(&path, 1024).is_err());
    std::fs::write(&path, b"abc").unwrap();
    assert_eq!(sha256_file(&path, 3).unwrap(), sha256_bytes(b"abc"));
    assert!(sha256_file(&path, 2).is_err());
    std::fs::write(&path, b"abd").unwrap();
    assert_ne!(sha256_file(&path, 3).unwrap(), sha256_bytes(b"abc"));
}

#[test]
fn sha256_reader_retries_interruptions_and_propagates_errors() {
    struct InterruptedOnce(bool);
    impl Read for InterruptedOnce {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            if !self.0 {
                self.0 = true;
                Err(io::ErrorKind::Interrupted.into())
            } else {
                Ok(0)
            }
        }
    }
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::PermissionDenied.into())
        }
    }
    assert_eq!(
        sha256_reader(InterruptedOnce(false), 1).unwrap(),
        sha256_bytes(b"")
    );
    assert_eq!(
        sha256_reader(Broken, 1).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
}
