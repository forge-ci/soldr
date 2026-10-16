//! Build-script and proc-macro links use the system `cc` (soldr#3801, PR 2).
//!
//! Cargo applies the target's `-C linker=` to host tools too, so on a native
//! build the first build script links through the managed clang shim within a
//! second of cargo starting and would wait on the background LLVM download.
//! A build script or proc-macro runs only on the host and needs no managed
//! LLVM, so the wrapper rewrites its link to the system `cc` whenever one is
//! on `PATH` -- **regardless of fetch state**, so the argv the compiler is
//! keyed on never depends on download timing. zccache keys a linked unit by
//! `<linker> --version`; the system `cc` gives one stable key per host. Only
//! final artifact links still go through the shim and wait.
//!
//! The rewrite also drops the Fast linker's driver arguments
//! (`--ld-path=...`, `-fuse-ld=...`): rustflags apply to every unit, and gcc
//! rejects both. On an `x86_64-unknown-linux-gnu` host it appends
//! `-C linker-features=-lld` as well: rustc itself emits `-fuse-ld=lld` there,
//! and when `-C link-self-contained=-linker` withholds its bundled `gcc-ld`
//! dir, gcc fails with `cannot find 'ld'` on any host without an `ld.lld`.
//! The flag is stable only for that triple, so no other host gets it.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// A build-script or proc-macro unit, the two host-only link shapes.
fn is_host_tool_unit(args: &[String]) -> bool {
    let build_script = args
        .windows(2)
        .any(|w| w[0] == "--crate-name" && w[1].starts_with("build_script_"))
        || args.iter().any(|a| {
            a.strip_prefix("--crate-name=")
                .is_some_and(|n| n.starts_with("build_script_"))
        });
    let proc_macro = args
        .windows(2)
        .any(|w| w[0] == "--crate-type" && w[1] == "proc-macro")
        || args.iter().any(|a| {
            a.strip_prefix("--crate-type=")
                .is_some_and(|v| v.split(',').any(|t| t == "proc-macro"))
        });
    build_script || proc_macro
}

/// Is this `link-arg(s)` value a clang-driver argument gcc would reject?
fn incompatible(value: &str) -> bool {
    value.starts_with("--ld-path=") || value.starts_with("-fuse-ld=")
}

/// `Some(replacement)` for a codegen option of `-C link-arg=` / `link-args=`
/// that is wholly incompatible (`None` inside = drop it), or a filtered
/// `link-args=` value.
enum LinkArgEdit {
    Keep,
    Drop,
    Replace(String),
}

fn edit_link_arg(codegen: &str) -> LinkArgEdit {
    if let Some(value) = codegen.strip_prefix("link-arg=") {
        return if incompatible(value) {
            LinkArgEdit::Drop
        } else {
            LinkArgEdit::Keep
        };
    }
    if let Some(value) = codegen.strip_prefix("link-args=") {
        let kept: Vec<&str> = value
            .split_whitespace()
            .filter(|w| !incompatible(w))
            .collect();
        let total = value.split_whitespace().count();
        return match kept.len() {
            0 if total > 0 => LinkArgEdit::Drop,
            n if n == total => LinkArgEdit::Keep,
            _ => LinkArgEdit::Replace(format!("link-args={}", kept.join(" "))),
        };
    }
    LinkArgEdit::Keep
}

/// The one triple where rustc emits `-fuse-ld=lld` on its own and where
/// `-C linker-features` is stable.
const RUSTC_LLD_TRIPLE: &str = "x86_64-unknown-linux-gnu";

/// The value of the last `--target` in `args`, any spelling.
fn target_arg(args: &[String]) -> Option<&str> {
    let mut found = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--target" {
            found = iter.next().map(String::as_str);
        } else if let Some(value) = arg.strip_prefix("--target=") {
            found = Some(value);
        }
    }
    found
}

/// Rewrite one host-tool link: `-C linker=` becomes `cc`, incompatible
/// link-args are removed, and on an x86_64 GNU/Linux `host` rustc's own lld
/// feature is switched off. Returns `None` when `args` is not a build-script
/// or proc-macro link through the managed shim, or `cc` is `None`.
///
/// Pure: reads no fetch state, so the result is the same whether the managed
/// LLVM is pending or complete.
pub(crate) fn rewrite_host_tool_link(
    args: &[String],
    cc: Option<&Path>,
    host: &str,
) -> Option<Vec<String>> {
    let cc = cc?.to_str()?;
    if !is_host_tool_unit(args)
        || !crate::llvm_fetch_wait::invocation_links(args)
        || !crate::llvm_fetch_wait::linker_arg(args)
            .is_some_and(crate::llvm_fetch_wait::names_managed_shim)
    {
        return None;
    }
    let mut out = Vec::with_capacity(args.len());
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let separate = arg == "-C" || arg == "--codegen";
        let codegen = if separate {
            iter.clone().next().map(String::as_str)
        } else if let Some(rest) = arg.strip_prefix("--codegen=") {
            Some(rest)
        } else {
            arg.strip_prefix("-C")
        };
        let Some(codegen) = codegen else {
            out.push(arg.clone());
            continue;
        };
        let prefix = if separate {
            None
        } else if arg.starts_with("--codegen=") {
            Some("--codegen=")
        } else {
            Some("-C")
        };
        let rebuilt = |value: &str| match prefix {
            Some(p) => vec![format!("{p}{value}")],
            None => vec![arg.clone(), value.to_string()],
        };
        let new_value = if codegen.starts_with("linker=") {
            Some(format!("linker={cc}"))
        } else {
            match edit_link_arg(codegen) {
                LinkArgEdit::Keep => None,
                LinkArgEdit::Replace(v) => Some(v),
                LinkArgEdit::Drop => {
                    if separate {
                        iter.next();
                    }
                    continue;
                }
            }
        };
        match new_value {
            Some(v) => {
                if separate {
                    iter.next();
                }
                out.extend(rebuilt(&v));
            }
            None => out.push(arg.clone()),
        }
    }
    if host == RUSTC_LLD_TRIPLE && target_arg(args).is_none_or(|target| target == host) {
        out.extend(["-C".to_string(), "linker-features=-lld".to_string()]);
    }
    Some(out)
}

/// The first executable `cc` on `path`, or `None`.
pub(crate) fn find_cc(path: Option<&OsStr>) -> Option<PathBuf> {
    let name = format!("cc{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(path?)
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
}

/// [`rewrite_host_tool_link`] with the system `cc` from this process's `PATH`.
/// Never rewrites on Windows, where `cc` is not the link driver.
pub(crate) fn rewrite_with_system_cc(args: &[String]) -> Option<Vec<String>> {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return None;
    }
    let path = std::env::var_os("PATH");
    rewrite_host_tool_link(
        args,
        find_cc(path.as_deref()).as_deref(),
        crate::platform::host::facts::triple(),
    )
}

#[cfg(test)]
#[path = "host_tool_link_tests.rs"]
mod tests;
