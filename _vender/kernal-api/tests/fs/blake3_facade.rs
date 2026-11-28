//! Consumer-style contracts for the kernel-owned BLAKE3 surface (#8).
//!
//! This fixture imports only `kernal_api`; clients do not need to name the
//! implementation crate to hash bytes, readers, or files.

use std::io::{self, Cursor, Read};

use kernal_api::hash::{
    blake3_bytes, blake3_file, blake3_open_file, blake3_reader, Blake3HashErrorKind,
    Blake3ReadOptions,
};

const EMPTY_BLAKE3: &str = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
const ABC_BLAKE3: &str = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";

#[test]
fn hashes_published_byte_vectors_through_kernel_owned_types() {
    let empty = blake3_bytes(b"");
    let abc = blake3_bytes(b"abc");

    assert_eq!(empty.to_hex(), EMPTY_BLAKE3);
    assert_eq!(abc.to_hex(), ABC_BLAKE3);
    assert_eq!(abc.as_bytes().len(), 32);
}

#[test]
fn hashes_equivalent_reader_and_file_content() {
    let content = b"kernal-api BLAKE3 facade\n";
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("content.bin");
    std::fs::write(&path, content).expect("write fixture");

    let from_reader = blake3_reader(Cursor::new(content), Blake3ReadOptions::default())
        .expect("hash reader through facade");
    let from_file =
        blake3_file(&path, Blake3ReadOptions::default()).expect("hash file through facade");

    assert_eq!(from_reader, blake3_bytes(content));
    assert_eq!(from_file, from_reader);
}

#[test]
fn reader_and_file_failures_have_deterministic_kernel_owned_kinds() {
    let reader_error = blake3_reader(FailingReader, Blake3ReadOptions::default())
        .expect_err("reader failure must remain observable");
    assert_eq!(reader_error.kind(), Blake3HashErrorKind::Read);
    assert_eq!(reader_error.io_error_kind(), Some(io::ErrorKind::Other));

    let directory = tempfile::tempdir().expect("temporary directory");
    let missing = blake3_file(
        directory.path().join("missing"),
        Blake3ReadOptions::default(),
    )
    .expect_err("missing file must fail at open");
    assert_eq!(missing.kind(), Blake3HashErrorKind::Open);
    assert_eq!(missing.io_error_kind(), Some(io::ErrorKind::NotFound));
}

#[test]
fn configured_size_bound_rejects_reader_and_file_content_before_hashing_extra_bytes() {
    let options = Blake3ReadOptions::new().maximum_bytes(3);
    assert_eq!(options.maximum_byte_count(), Some(3));
    assert_eq!(
        blake3_reader(Cursor::new(b"abc"), options).expect("content at the bound is accepted"),
        blake3_bytes(b"abc")
    );

    let reader_error = blake3_reader(Cursor::new(b"abcd"), options)
        .expect_err("reader content exceeds configured bound");
    assert_eq!(
        reader_error.kind(),
        Blake3HashErrorKind::SizeLimitExceeded { maximum_bytes: 3 }
    );
    assert_eq!(reader_error.io_error_kind(), None);

    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("too-large.bin");
    std::fs::write(&path, b"abcd").expect("write fixture");
    let file_error =
        blake3_file(&path, options).expect_err("file content exceeds configured bound");
    assert_eq!(
        file_error.kind(),
        Blake3HashErrorKind::SizeLimitExceeded { maximum_bytes: 3 }
    );
}

struct FailingReader;

impl Read for FailingReader {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("fixture read failure"))
    }
}

/// A client that already holds an open handle must reach the same digest as
/// the path-taking entry point, on both the mapped and the buffered path.
///
/// The size is deliberately above the internal parallel-hash threshold: that
/// is the case a client opens a handle for in the first place, and hashing a
/// large mapped file in parallel is the path most likely to diverge from a
/// buffered read of the same bytes.
#[test]
fn an_open_handle_hashes_the_same_as_its_path() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("large.bin");
    let contents = vec![0x5a_u8; 512 * 1024];
    std::fs::write(&path, &contents).expect("write fixture");

    let expected = blake3_bytes(&contents);
    let mapped = Blake3ReadOptions::new().memory_map(true);
    let buffered = Blake3ReadOptions::new();

    let file = std::fs::File::open(&path).expect("open fixture");
    assert_eq!(
        blake3_open_file(&file, mapped).expect("mapped handle hash"),
        expected,
        "the mapped path over an open handle must match the content digest"
    );

    let file = std::fs::File::open(&path).expect("reopen fixture");
    assert_eq!(
        blake3_open_file(&file, buffered).expect("buffered handle hash"),
        expected,
        "the buffered fallback must agree with the mapped path"
    );

    assert_eq!(
        blake3_file(&path, mapped).expect("path hash"),
        expected,
        "the handle and path entry points must not disagree"
    );
}

/// An empty file cannot be mapped, so the mapped request must fall back
/// rather than fail -- `memory_map` is documented as a performance request.
#[test]
fn an_empty_open_handle_falls_back_to_the_buffered_read() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("empty.bin");
    std::fs::write(&path, b"").expect("write fixture");

    let file = std::fs::File::open(&path).expect("open fixture");
    let digest = blake3_open_file(&file, Blake3ReadOptions::new().memory_map(true))
        .expect("empty file still hashes");
    assert_eq!(digest.to_hex(), EMPTY_BLAKE3);
}

/// The configured bound is enforced against an open handle too, and against
/// the file's exact length before any mapping is attempted.
#[test]
fn an_open_handle_still_honors_the_size_limit() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("bounded.bin");
    std::fs::write(&path, b"abcd").expect("write fixture");

    let file = std::fs::File::open(&path).expect("open fixture");
    let error = blake3_open_file(
        &file,
        Blake3ReadOptions::new().maximum_bytes(3).memory_map(true),
    )
    .expect_err("handle content exceeds configured bound");
    assert_eq!(
        error.kind(),
        Blake3HashErrorKind::SizeLimitExceeded { maximum_bytes: 3 }
    );
}
