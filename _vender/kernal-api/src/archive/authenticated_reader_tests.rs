use super::*;
use crate::archive::ExtractionLimits;
use std::io::Read;

pub(crate) fn fixture(name: &str, length: u64) -> (Authenticated, StagingBudget) {
    let mut writer = zip::ZipWriter::new(tempfile::tempfile().unwrap());
    writer
        .start_file(
            name,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
    let chunk = [0x5a; CHUNK];
    let mut remaining = length;
    while remaining > 0 {
        let count = remaining.min(CHUNK as u64) as usize;
        writer.write_all(&chunk[..count]).unwrap();
        remaining -= count as u64;
    }
    authenticate_file(writer.finish().unwrap())
}

fn authenticate_file(mut file: File) -> (Authenticated, StagingBudget) {
    let length = file.metadata().unwrap().len();
    file.rewind().unwrap();
    let budget = StagingBudget::new(length);
    let mut pending =
        Authentication::begin(&[7; 16], &[8; 12], b"reader", length, &budget).unwrap();
    let mut encoder = Crypter::new(
        Cipher::aes_128_gcm(),
        Mode::Encrypt,
        &[7; 16],
        Some(&[8; 12]),
    )
    .unwrap();
    encoder.aad_update(b"reader").unwrap();
    let mut input = [0; CHUNK];
    let mut output = [0; CHUNK + 16];
    loop {
        let count = file.read(&mut input).unwrap();
        if count == 0 {
            break;
        }
        let count = encoder.update(&input[..count], &mut output).unwrap();
        pending.update(&output[..count]).unwrap();
    }
    assert_eq!(encoder.finalize(&mut output).unwrap(), 0);
    let mut tag = [0; 16];
    encoder.get_tag(&mut tag).unwrap();
    (pending.authenticate(&tag).unwrap(), budget)
}

#[test]
fn authenticated_reader_streams_large_entry_and_retains_storage_on_worker() {
    const LENGTH: u64 = 17 * 1024 * 1024;
    let (authenticated, budget) = fixture("payload", LENGTH);
    let staged = budget.used();
    assert!(staged > 16 * 1024 * 1024);
    let mut reader = authenticated
        .into_reader(ExtractionLimits {
            max_entry_bytes: LENGTH,
            max_output_bytes: LENGTH,
            ..ExtractionLimits::default()
        })
        .unwrap();
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let record = reader.entry(0).unwrap().unwrap();
                assert_eq!(record.name, "payload");
                assert_eq!(record.bytes, LENGTH);
                assert!(reader.entry(1).unwrap().is_none());
                struct Sink<'a> {
                    count: u64,
                    budget: &'a StagingBudget,
                    staged: u64,
                }
                impl Write for Sink<'_> {
                    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                        assert!(bytes.len() <= CHUNK);
                        assert!(bytes.iter().all(|byte| *byte == 0x5a));
                        assert_eq!(self.budget.used(), self.staged);
                        self.count += bytes.len() as u64;
                        Ok(bytes.len())
                    }
                    fn flush(&mut self) -> io::Result<()> {
                        Ok(())
                    }
                }
                let mut sink = Sink {
                    count: 0,
                    budget: &budget,
                    staged,
                };
                assert_eq!(reader.copy_entry(0, &mut sink).unwrap(), LENGTH);
                assert_eq!(sink.count, LENGTH);
                // Re-reading cannot reuse the already consumed aggregate budget.
                assert!(reader.copy_entry(0, &mut sink).is_err());
                assert_eq!(sink.count, LENGTH);
            })
            .join()
            .unwrap();
    });
    assert_eq!(budget.used(), staged);
    drop(reader);
    assert_eq!(budget.used(), 0);
}

#[test]
fn authenticated_reader_rejects_metadata_paths_and_entry_limits() {
    for case in 0..6 {
        let (authenticated, budget) = fixture(if case == 3 { "../escape" } else { "payload" }, 5);
        let mut limits = ExtractionLimits::default();
        match case {
            0 => limits.max_entries = 0,
            1 => limits.max_metadata_bytes = 0,
            2 => limits.max_path_bytes = 3,
            4 => limits.max_entry_bytes = 4,
            5 => limits.max_input_bytes = 0,
            _ => (),
        }
        match authenticated.into_reader(limits) {
            Ok(mut reader) => {
                assert!((2..5).contains(&case));
                assert!(reader.entry(0).is_err());
                assert!(reader.copy_entry(0, &mut io::sink()).is_err());
            }
            Err(_) => assert!(case < 2 || case == 5),
        }
        assert_eq!(budget.used(), 0);
    }
}

#[test]
fn authenticated_reader_failed_sink_cannot_resume_or_retry() {
    let (authenticated, budget) = fixture("payload", 8);
    let mut reader = authenticated
        .into_reader(ExtractionLimits::default())
        .unwrap();
    struct FailsAfterPartialWrite {
        written: usize,
    }
    impl Write for FailsAfterPartialWrite {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            if self.written == 0 {
                self.written = 1;
                Ok(1)
            } else {
                Err(io::Error::other("synthetic sink failure"))
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut sink = FailsAfterPartialWrite { written: 0 };
    assert!(reader.copy_entry(0, &mut sink).is_err());
    assert_eq!(sink.written, 1);
    assert!(reader.copy_entry(0, &mut io::sink()).is_err());
    assert!(reader.entry(0).is_err());
    assert!(budget.used() > 0);
    drop(reader);
    assert_eq!(budget.used(), 0);
}

#[test]
fn authenticated_reader_rejects_links_and_directories_but_allows_empty_files() {
    for link in [false, true] {
        let mut writer = zip::ZipWriter::new(tempfile::tempfile().unwrap());
        let options = zip::write::SimpleFileOptions::default();
        if link {
            writer.add_symlink("link", "payload", options).unwrap();
        } else {
            writer.add_directory("directory/", options).unwrap();
        }
        let (authenticated, budget) = authenticate_file(writer.finish().unwrap());
        let mut reader = authenticated
            .into_reader(ExtractionLimits::default())
            .unwrap();
        assert!(reader.entry(0).is_err());
        assert!(reader.copy_entry(0, &mut io::sink()).is_err());
        drop(reader);
        assert_eq!(budget.used(), 0);
    }
    let (authenticated, budget) = fixture("empty", 0);
    let mut reader = authenticated
        .into_reader(ExtractionLimits {
            max_entry_bytes: 0,
            max_output_bytes: 0,
            ..ExtractionLimits::default()
        })
        .unwrap();
    assert_eq!(reader.entry(0).unwrap().unwrap().bytes, 0);
    assert_eq!(reader.copy_entry(0, &mut io::sink()).unwrap(), 0);
    drop(reader);
    assert_eq!(budget.used(), 0);
}
