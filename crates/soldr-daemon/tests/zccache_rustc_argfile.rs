//! soldr#4163: a long rustc command line must reach rustc unchanged.
//!
//! When a rustc command line is too long for Windows, cargo runs the wrapper
//! as `rustc @argfile` with one argument per line. rustc reads that file the
//! same way (`rustc_driver_impl/src/args.rs`): every line is one argument,
//! verbatim, with no quoting and no nested `@` expansion. The embedded
//! zccache up to 1.15.13 expanded the file with the GCC/Clang grammar, which
//! splits on whitespace. `--check-cfg cfg(feature, values("a", "b"))` reached
//! rustc as several arguments and the compile failed with "multiple input
//! filenames provided" (talos-vox `cargo check --features kernal-api --lib`).
//!
//! The fix lives in zccache (`expand_response_files_in_for`, published in
//! zccache 1.15.14 via 1b27e47c, 55c8d1db and a3813202). This test pins it
//! from soldr's side, covering the reader, zccache's own spill writer, and
//! literal `@` arguments.

use std::path::Path;

use zccache::compiler::response_file::{
    expand_response_files_in, expand_response_files_in_for, parse_response_file_content_for,
    preserve_rustc_literal_at_args, read_response_file_text_for, write_response_file_if_needed,
};
use zccache::compiler::CompilerFamily;

/// zccache's spill threshold is 30,000 bytes; stay well past it.
const LONG_COMMAND_LINE: usize = 40_000;

/// Write `args` the way cargo writes a rustc argfile: one argument per line.
fn write_cargo_argfile(dir: &Path, args: &[String]) -> String {
    let mut content = String::new();
    for arg in args {
        content.push_str(arg);
        content.push('\n');
    }
    std::fs::write(dir.join("rustc.args"), content).expect("write argfile");
    "@rustc.args".to_owned()
}

/// A cargo-shaped rustc argv whose arguments contain spaces, double and
/// single quotes, backslashes and a literal `@`, padded past the spill
/// threshold with the same kind of `--check-cfg` values.
fn long_rustc_args() -> Vec<String> {
    let mut args: Vec<String> = [
        "--crate-name",
        "talos_vox",
        "--edition=2021",
        "src/lib.rs",
        "--crate-type",
        "lib",
        "--check-cfg",
        r#"cfg(feature, values("cohere", "kernal-api", "default"))"#,
        "--cfg",
        r#"feature="kernal-api""#,
        "-C",
        "link-arg=it's a 'single' quote",
        "-L",
        r"native=C:\Program Files\Some SDK\lib",
        "-C",
        "link-arg=@literal-not-an-argfile",
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect();
    let mut index = 0;
    while args.iter().map(|arg| arg.len() + 1).sum::<usize>() < LONG_COMMAND_LINE {
        args.push("--check-cfg".to_owned());
        args.push(format!(
            r#"cfg(feature_{index}, values("a {index}", "b {index}", "c\"{index}"))"#
        ));
        index += 1;
    }
    args
}

#[test]
fn rustc_argfile_reader_keeps_every_line_as_one_argument() {
    let dir = tempfile::tempdir().expect("tempdir");
    let args = long_rustc_args();
    assert!(args.iter().map(|arg| arg.len() + 1).sum::<usize>() >= LONG_COMMAND_LINE);
    let at_arg = write_cargo_argfile(dir.path(), &args);

    let expanded = expand_response_files_in_for(
        std::slice::from_ref(&at_arg),
        dir.path(),
        CompilerFamily::Rustc,
    )
    .expect("expand rustc argfile");
    assert_eq!(expanded, args, "rustc argfile must round-trip unchanged");

    // The pre-1.15.14 reader: the GCC grammar splits the same file apart.
    // This is the exact failure soldr#4163 reported.
    let gcc = expand_response_files_in(std::slice::from_ref(&at_arg), dir.path())
        .expect("expand with the GCC grammar");
    assert!(
        gcc.iter().any(|arg| arg == "values(cohere,"),
        "the GCC grammar is expected to split cargo's argfile on whitespace"
    );
    assert_ne!(gcc, args);
}

#[test]
fn rustc_argfile_does_not_expand_nested_at_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    // `@missing` is a literal argument inside a rustc argfile; rustc never
    // opens it, so expansion must not try to read it either.
    let args = vec!["--cfg".to_owned(), "@missing".to_owned()];
    let at_arg = write_cargo_argfile(dir.path(), &args);
    let expanded = expand_response_files_in_for(&[at_arg], dir.path(), CompilerFamily::Rustc)
        .expect("nested @ lines are literal for rustc");
    assert_eq!(expanded, args);

    // Passed straight to rustc, `@missing` would be opened as an argfile.
    // zccache moves each literal `@` argument into its own one-line argfile.
    let preserved = preserve_rustc_literal_at_args(&expanded, dir.path())
        .expect("write literal argfiles")
        .expect("a literal @ argument needs protection");
    let rewritten = preserved.args();
    assert_eq!(rewritten[0], "--cfg");
    let file = rewritten[1].strip_prefix('@').expect("argfile reference");
    let text = read_response_file_text_for(Path::new(file), CompilerFamily::Rustc)
        .expect("read literal argfile");
    assert_eq!(
        parse_response_file_content_for(&text, CompilerFamily::Rustc),
        vec!["@missing".to_owned()]
    );
}

#[test]
fn rustc_spill_writer_round_trips_through_the_rustc_reader() {
    let dir = tempfile::tempdir().expect("tempdir");
    let args = long_rustc_args();
    let spilled = write_response_file_if_needed(&args, dir.path(), CompilerFamily::Rustc)
        .expect("write spill response file");

    // zccache spills only where CreateProcess limits the command line. A
    // runtime check keeps this test outside the platform-cfg boundary; the
    // target-replay ownership entry runs it on the Windows targets.
    if std::env::consts::OS != "windows" {
        assert!(spilled.is_none(), "non-Windows hosts pass argv directly");
        return;
    }
    let spilled = spilled.expect("a 40K-byte command line spills on Windows");
    let text = read_response_file_text_for(spilled.path.as_path(), CompilerFamily::Rustc)
        .expect("read spill file");
    assert_eq!(
        parse_response_file_content_for(&text, CompilerFamily::Rustc),
        args,
        "spill file must hold one verbatim argument per line"
    );
}

#[test]
fn rustc_spill_writer_refuses_arguments_with_newlines() {
    // rustc's format cannot carry a newline inside an argument, so zccache
    // must fall back to passing argv directly rather than split it.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut args = long_rustc_args();
    args.push("--cfg".to_owned());
    args.push("line one\nline two".to_owned());
    let spilled = write_response_file_if_needed(&args, dir.path(), CompilerFamily::Rustc)
        .expect("write spill response file");
    assert!(spilled.is_none());

    let mut crlf = long_rustc_args();
    crlf.push("carriage\rreturn".to_owned());
    let spilled = write_response_file_if_needed(&crlf, dir.path(), CompilerFamily::Rustc)
        .expect("write spill response file");
    assert!(spilled.is_none());
}
