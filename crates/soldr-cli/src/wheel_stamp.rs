//! Stamp a built wheel's `Generator:` field so soldr's involvement is
//! provable from the artifact's bytes alone, not just trusted from the build
//! log (zackees/soldr#3433, feeding zackees/ci.yml#6's `PKG-004`).
//!
//! maturin writes `Generator: maturin <version>` into every
//! `*.dist-info/WHEEL` it produces, whether invoked directly or through
//! soldr's `maturin build` / `maturin pep517 build-wheel` dispatch. Nothing
//! in that file names soldr, so a wheel built through the soldr PEP 517
//! backend or `soldr wheel` looks identical, from the artifact alone, to one
//! built by bare `maturin`. This module rewrites the `Generator:` line to
//! `soldr <soldr version> (<original generator>)` after a successful build,
//! and recomputes the wheel's `RECORD` so the artifact stays internally
//! consistent (`pip`/`wheel` verify `RECORD` hashes against a wheel's own
//! contents).
//!
//! Idempotent: a wheel whose `Generator:` already starts with `soldr ` is
//! left untouched (for example, a wheel restored from soldr's own PEP 517
//! wheel cache, where the cached copy was already stamped by an earlier
//! build in the same invocation shape).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// Whether a maturin argv (as seen by soldr's dispatcher, argv\[0\] onward
/// starting at the maturin subcommand) produces a standalone `.whl` file
/// worth stamping. `maturin pep517 write-dist-info` only writes a loose
/// dist-info directory (no wheel yet); `maturin develop` installs straight
/// into a venv. Both are excluded — same set `pyo3_detect::maturin_args_are_build`
/// treats as a "build", intersected with the ones that leave a `.whl` on disk.
pub fn maturin_args_produce_wheel(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("build") => true,
        Some("pep517") => matches!(args.get(1).map(String::as_str), Some("build-wheel")),
        _ => false,
    }
}

/// `maturin_output_dir`, but only when `args` is a wheel-producing maturin
/// invocation (re-derives that with `pyo3_detect::maturin_args_are_build` so
/// the call site stays a single short expression).
pub fn wheel_dir_for_stamp(args: &[String], workspace_root: &Path) -> Option<PathBuf> {
    (crate::pyo3_detect::maturin_args_are_build(args) && maturin_args_produce_wheel(args))
        .then(|| maturin_output_dir(args, workspace_root))
}

/// Resolve the directory maturin will write its `.whl` into for this argv:
/// an explicit `--out`/`-o` (soldr's own PEP 517 backend always passes
/// `--out`; `soldr wheel` does not), else maturin's own default,
/// `<workspace>/target/wheels`.
pub fn maturin_output_dir(args: &[String], workspace_root: &Path) -> PathBuf {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--out" || arg == "-o" {
            if let Some(value) = iter.next() {
                return PathBuf::from(value);
            }
        }
        if let Some(value) = arg.strip_prefix("--out=") {
            return PathBuf::from(value);
        }
    }
    workspace_root.join("target").join("wheels")
}

/// Rewrite `wheel`'s dist-info `WHEEL` file in place so its `Generator:`
/// line names soldr, then recompute `RECORD`.
///
/// Returns `Ok(true)` if the wheel was changed, `Ok(false)` if it was
/// already stamped. Any I/O or archive-shape problem is returned as an
/// error; callers should treat a failure here as a warning, never a reason
/// to fail the build itself — stamping is provenance metadata, not a build
/// correctness requirement.
pub fn stamp_wheel_generator(wheel: &Path, soldr_version: &str) -> std::io::Result<bool> {
    let file = std::fs::File::open(wheel)?;
    let mut archive = ZipArchive::new(file).map_err(zip_to_io)?;

    let dist_info = find_dist_info_dir(&archive)?;
    let wheel_entry_name = format!("{dist_info}/WHEEL");
    let record_entry_name = format!("{dist_info}/RECORD");

    let original_wheel_text = read_entry_to_string(&mut archive, &wheel_entry_name)?;
    let Some(new_wheel_text) = stamped_generator_text(&original_wheel_text, soldr_version) else {
        return Ok(false);
    };

    let temp_path = temp_path_for(wheel);
    {
        let out_file = std::fs::File::create(&temp_path)?;
        let mut writer = ZipWriter::new(out_file);
        let mut record_rows: Vec<(String, String, String)> = Vec::new();

        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).map_err(zip_to_io)?;
            let name = String::from_utf8_lossy(entry.name_raw()).into_owned();
            if name == record_entry_name {
                continue; // rewritten below, once every other row is known
            }
            let is_dir = entry.is_dir();
            // Keep every entry's original compression method, including the
            // WHEEL text we're about to replace.
            let mut options: SimpleFileOptions =
                SimpleFileOptions::default().compression_method(entry.compression());
            if let Some(mode) = entry.unix_mode() {
                options = options.unix_permissions(mode);
            }

            let data: Vec<u8> = if name == wheel_entry_name {
                new_wheel_text.clone().into_bytes()
            } else {
                let mut buf = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut buf)?;
                buf
            };

            if is_dir {
                writer.add_directory(&name, options).map_err(zip_to_io)?;
            } else {
                writer.start_file(&name, options).map_err(zip_to_io)?;
                writer.write_all(&data)?;
                record_rows.push((name, record_hash(&data), data.len().to_string()));
            }
        }

        record_rows.push((record_entry_name.clone(), String::new(), String::new()));
        let record_text = render_record(&record_rows);
        let options: SimpleFileOptions =
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        writer
            .start_file(&record_entry_name, options)
            .map_err(zip_to_io)?;
        writer.write_all(record_text.as_bytes())?;
        writer.finish().map_err(zip_to_io)?;
    }
    std::fs::rename(&temp_path, wheel)?;
    Ok(true)
}

fn temp_path_for(wheel: &Path) -> std::path::PathBuf {
    let mut name = wheel
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".soldr-stamp.{}.tmp", std::process::id()));
    wheel.with_file_name(name)
}

fn zip_to_io(err: zip::result::ZipError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, err)
}

/// Find the single top-level `*.dist-info` directory from a wheel's
/// `RECORD` entries, the same way `add_files_to_wheel` does on the Python
/// side (`src/soldr/_bundle_bins.py::_dist_info_directory`).
fn find_dist_info_dir<R: Read + std::io::Seek>(archive: &ZipArchive<R>) -> std::io::Result<String> {
    let mut candidates: Vec<String> = archive
        .file_names()
        .filter_map(|name| {
            let name = name.ok()?;
            let (dir, rest) = name.split_once('/')?;
            if rest == "RECORD" && dir.ends_with(".dist-info") {
                Some(dir.to_string())
            } else {
                None
            }
        })
        .collect();
    candidates.sort();
    candidates.dedup();
    match candidates.as_slice() {
        [only] => Ok(only.clone()),
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "expected exactly one top-level *.dist-info/RECORD in the wheel, found {other:?}"
            ),
        )),
    }
}

fn read_entry_to_string<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> std::io::Result<String> {
    let mut entry = archive.by_name(name).map_err(zip_to_io)?;
    let mut text = String::new();
    entry.read_to_string(&mut text)?;
    Ok(text)
}

/// Rewrite the `Generator:` line of a `*.dist-info/WHEEL` file's text.
///
/// Returns `None` (no change) if the existing generator already names
/// soldr, so repeated stamping (e.g. a cache-restored wheel re-entering
/// this path) stays idempotent.
fn stamped_generator_text(original: &str, soldr_version: &str) -> Option<String> {
    let mut changed = false;
    let mut out = String::with_capacity(original.len() + 32);
    for line in original.split_inclusive('\n') {
        let (content, ending) = split_line_ending(line);
        if let Some(value) = content.strip_prefix("Generator:") {
            let value = value.trim();
            if value.starts_with("soldr ") {
                return None; // already stamped
            }
            out.push_str(&format!(
                "Generator: soldr {soldr_version} ({value}){ending}"
            ));
            changed = true;
        } else {
            out.push_str(line);
        }
    }
    changed.then_some(out)
}

fn split_line_ending(line: &str) -> (&str, &str) {
    if let Some(stripped) = line.strip_suffix("\r\n") {
        (stripped, "\r\n")
    } else if let Some(stripped) = line.strip_suffix('\n') {
        (stripped, "\n")
    } else {
        (line, "")
    }
}

fn record_hash(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    format!("sha256={}", base64_urlsafe_nopad(&digest))
}

fn base64_urlsafe_nopad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        let indices = [
            (n >> 18) & 0x3f,
            (n >> 12) & 0x3f,
            (n >> 6) & 0x3f,
            n & 0x3f,
        ];
        let take = match chunk.len() {
            1 => 2,
            2 => 3,
            _ => 4,
        };
        for &idx in &indices[..take] {
            out.push(ALPHABET[idx as usize] as char);
        }
    }
    out
}

fn render_record(rows: &[(String, String, String)]) -> String {
    let mut text = String::new();
    for (name, hash, size) in rows {
        text.push_str(name);
        text.push(',');
        text.push_str(hash);
        text.push(',');
        text.push_str(size);
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn make_wheel(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut zip = ZipWriter::new(cursor);
            let opts: SimpleFileOptions =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            for (name, data) in entries {
                zip.start_file(*name, opts).unwrap();
                zip.write_all(data).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    fn read_text(bytes: &[u8], name: &str) -> String {
        let mut archive = ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut entry = archive.by_name(name).unwrap();
        let mut text = String::new();
        entry.read_to_string(&mut text).unwrap();
        text
    }

    const WHEEL_TEXT: &str = "Wheel-Version: 1.0\nGenerator: maturin (1.14.1-post.1)\nRoot-Is-Purelib: false\nTag: cp310-abi3-manylinux_2_17_x86_64\n";
    const METADATA_TEXT: &str = "Metadata-Version: 2.1\nName: demo\nVersion: 0.1.0\n";

    fn base_entries() -> Vec<(&'static str, &'static [u8])> {
        vec![
            ("demo-0.1.0.dist-info/WHEEL", WHEEL_TEXT.as_bytes()),
            ("demo-0.1.0.dist-info/METADATA", METADATA_TEXT.as_bytes()),
            (
                "demo-0.1.0.dist-info/RECORD",
                b"placeholder,placeholder,0\n",
            ),
        ]
    }

    #[test]
    fn stamps_generator_and_rewrites_record() {
        let tmp = tempfile::tempdir().unwrap();
        let wheel_path = tmp
            .path()
            .join("demo-0.1.0-cp310-abi3-manylinux_2_17_x86_64.whl");
        std::fs::write(&wheel_path, make_wheel(&base_entries())).unwrap();

        let changed = stamp_wheel_generator(&wheel_path, "0.9.25").unwrap();
        assert!(changed);

        let bytes = std::fs::read(&wheel_path).unwrap();
        let wheel_text = read_text(&bytes, "demo-0.1.0.dist-info/WHEEL");
        assert!(
            wheel_text.contains("Generator: soldr 0.9.25 (maturin (1.14.1-post.1))"),
            "unexpected WHEEL text: {wheel_text}"
        );
        // METADATA is untouched.
        assert_eq!(
            read_text(&bytes, "demo-0.1.0.dist-info/METADATA"),
            METADATA_TEXT
        );
        // RECORD lists WHEEL with a real hash now, and lists itself with an
        // empty hash/size per the wheel spec's self-referential row.
        let record_text = read_text(&bytes, "demo-0.1.0.dist-info/RECORD");
        assert!(record_text.contains("demo-0.1.0.dist-info/WHEEL,sha256="));
        assert!(record_text.contains("demo-0.1.0.dist-info/RECORD,,\n"));
    }

    #[test]
    fn already_stamped_wheel_is_left_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let wheel_path = tmp
            .path()
            .join("demo-0.1.0-cp310-abi3-manylinux_2_17_x86_64.whl");
        let mut entries = base_entries();
        let stamped_wheel_text =
            "Wheel-Version: 1.0\nGenerator: soldr 0.9.25 (maturin (1.14.1-post.1))\n";
        entries[0] = ("demo-0.1.0.dist-info/WHEEL", stamped_wheel_text.as_bytes());
        let original_bytes = make_wheel(&entries);
        std::fs::write(&wheel_path, &original_bytes).unwrap();

        let changed = stamp_wheel_generator(&wheel_path, "0.9.25").unwrap();
        assert!(!changed);
        assert_eq!(std::fs::read(&wheel_path).unwrap(), original_bytes);
    }

    #[test]
    fn produce_wheel_covers_build_and_pep517_build_wheel_only() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(maturin_args_produce_wheel(&s(&["build", "--release"])));
        assert!(maturin_args_produce_wheel(&s(&[
            "pep517",
            "build-wheel",
            "--out",
            "/tmp"
        ])));
        assert!(!maturin_args_produce_wheel(&s(&["develop"])));
        assert!(!maturin_args_produce_wheel(&s(&[
            "pep517",
            "write-dist-info"
        ])));
        assert!(!maturin_args_produce_wheel(&s(&["pep517"])));
        assert!(!maturin_args_produce_wheel(&s(&[])));
    }

    #[test]
    fn output_dir_prefers_explicit_out_over_default() {
        let root = Path::new("/workspace");
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            maturin_output_dir(&s(&["build", "--out", "/tmp/wheels"]), root),
            PathBuf::from("/tmp/wheels")
        );
        assert_eq!(
            maturin_output_dir(&s(&["build", "--out=/tmp/wheels2"]), root),
            PathBuf::from("/tmp/wheels2")
        );
        assert_eq!(
            maturin_output_dir(&s(&["build", "--release"]), root),
            root.join("target").join("wheels")
        );
        // `-o` after `--` is a passthrough value, not soldr's own flag.
        assert_eq!(
            maturin_output_dir(&s(&["build", "--", "-o", "ignored"]), root),
            root.join("target").join("wheels")
        );
    }

    #[test]
    fn missing_dist_info_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let wheel_path = tmp.path().join("not-a-wheel.whl");
        std::fs::write(&wheel_path, make_wheel(&[("readme.txt", b"hi")])).unwrap();

        let err = stamp_wheel_generator(&wheel_path, "0.9.25").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
