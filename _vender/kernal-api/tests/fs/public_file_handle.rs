//! Consumer-style contract for the publicly nameable regular-file handle.
//!
//! A client that ports a `std::fs::File` call site onto the facade must be able
//! to name the handle type in its own public signatures without naming
//! `std::fs` itself, which the platform-boundary lint bans outside the
//! adapters. This module compiles against `kernal_api` as an external consumer
//! and exercises the re-export through real file I/O.

use std::io::{Read, Write};

use kernal_api::platform::fs::{create_file, open_read_regular_file, open_shared_append, File};

#[test]
fn file_handle_is_publicly_nameable_and_usable() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("handle.txt");

    let mut writer: File = create_file(&path).expect("create handle");
    writer
        .write_all(b"kernal-api public handle\n")
        .expect("write");
    drop(writer);

    let mut appender: File = open_shared_append(&path).expect("open append handle");
    appender.write_all(b"second line\n").expect("append");
    drop(appender);

    let mut reader: File = open_read_regular_file(&path).expect("open read handle");
    let mut contents = String::new();
    reader.read_to_string(&mut contents).expect("read");
    assert_eq!(contents, "kernal-api public handle\nsecond line\n");
}
