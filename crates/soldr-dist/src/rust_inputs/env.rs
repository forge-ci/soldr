//! Identify env inputs and preserve path constants while relocating include paths.
use crate::{compiler::RUNTIME_ENV, DistJob, Result};
use proc_macro2::{TokenStream, TokenTree};
use std::collections::BTreeSet;

/// std/core macros whose expansion cannot read a user environment variable or
/// include a file, so calling them leaves env inspection exact. A crate that
/// defines its own `macro_rules!` under one of these names loses the exemption,
/// and macros imported from another crate already require an rlib/proc-macro
/// input, which is refused separately.
const STD_MACROS: &[&str] = &[
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "matches",
    "panic",
    "todo",
    "unimplemented",
    "unreachable",
    "write",
    "writeln",
    "format",
    "format_args",
    "print",
    "println",
    "eprint",
    "eprintln",
    "vec",
    "dbg",
    "cfg",
    "line",
    "column",
    "file",
    "module_path",
    "stringify",
    "compile_error",
    "thread_local",
];

struct Uses {
    keys: BTreeSet<String>,
    includes: BTreeSet<String>,
    constants: BTreeSet<String>,
    aliases: BTreeSet<String>,
    /// Names this crate defines with `macro_rules!`.
    local_macros: BTreeSet<String>,
    ambiguous_macros: bool,
}
impl Default for Uses {
    fn default() -> Self {
        Self {
            keys: BTreeSet::new(),
            includes: BTreeSet::new(),
            constants: BTreeSet::new(),
            aliases: BTreeSet::from(["env".into(), "option_env".into()]),
            local_macros: BTreeSet::new(),
            ambiguous_macros: false,
        }
    }
}

pub(super) fn select(job: &mut DistJob) -> Result<()> {
    inspect(job).map(|_| ())
}

pub(super) fn inspect(job: &mut DistJob) -> Result<BTreeSet<String>> {
    let mut uses = Uses::default();
    let mut sources = Vec::new();
    for input in &job.inputs {
        if !input.abs_path.ends_with(".rs") {
            continue;
        }
        let text = std::fs::read_to_string(&input.abs_path)?;
        if blake3::hash(text.as_bytes()).to_hex().as_str() != input.blake3 {
            return Err("Rust source changed during environment inspection".into());
        }
        sources.push(text);
    }
    if !job.stdin.is_empty() {
        sources.push(std::str::from_utf8(&job.stdin)?.to_owned());
    }
    let tokens = sources
        .iter()
        .map(|text| {
            text.parse::<TokenStream>()
                .map_err(|_| crate::Error::from("cannot inspect Rust env macros"))
        })
        .collect::<Result<Vec<_>>>()?;
    loop {
        let before = uses.aliases.len();
        for source in &tokens {
            aliases(source.clone(), &mut uses.aliases);
        }
        if uses.aliases.len() == before {
            break;
        }
    }
    for source in &tokens {
        local_macros(source.clone(), &mut uses.local_macros);
    }
    for source in tokens {
        scan(source, false, &mut uses)?;
    }
    if let Some(key) = uses
        .keys
        .iter()
        .find(|key| RUNTIME_ENV.contains(&key.as_str()) || key.as_str() == "CARGO_MAKEFLAGS")
    {
        return Err(format!("Rust reads worker-owned runtime environment: {key}").into());
    }
    if let Some(key) = uses.includes.intersection(&uses.constants).next() {
        return Err(format!(
            "path env {key} used both as include path and constant requires filesystem namespace"
        )
        .into());
    }
    // Only a crate the unit actually imports can supply macros: one named by
    // `--extern`, or an `extern crate` resolved from a search path. Counting
    // every library under `-L dependency=` made the verdict depend on which
    // unrelated crates had finished first (target_lexicon flipped between
    // remote and declined across identical cold builds).
    let external_macros = job
        .argv
        .iter()
        .any(|arg| arg == "--extern" || arg.starts_with("--extern="))
        || sources.iter().any(|text| imports_non_sysroot_crate(text));
    if !uses.includes.is_empty() && (uses.ambiguous_macros || external_macros) {
        return Err(
            "path env rewrite with unresolved macro expansion requires filesystem namespace".into(),
        );
    }
    job.rewrite_env = uses.includes.into_iter().collect();
    // Keep original exec_env, including aliased/dynamically supplied env dependencies.
    // Worker runtime overrides are validated against these original values by dep-info.
    Ok(uses.keys)
}

fn aliases(tokens: TokenStream, known: &mut BTreeSet<String>) {
    let tokens = tokens.into_iter().collect::<Vec<_>>();
    for part in tokens.windows(3) {
        if let [TokenTree::Ident(name), TokenTree::Ident(rename), TokenTree::Ident(alias)] = part {
            if rename == "as" && known.contains(&name.to_string()) {
                known.insert(alias.to_string());
            }
        }
    }
    for token in tokens {
        if let TokenTree::Group(group) = token {
            aliases(group.stream(), known);
        }
    }
}

fn local_macros(tokens: TokenStream, defined: &mut BTreeSet<String>) {
    let tokens = tokens.into_iter().collect::<Vec<_>>();
    for part in tokens.windows(3) {
        if let [TokenTree::Ident(keyword), TokenTree::Punct(bang), TokenTree::Ident(name)] = part {
            if keyword == "macro_rules" && bang.as_char() == '!' {
                defined.insert(name.to_string());
            }
        }
    }
    for token in tokens {
        if let TokenTree::Group(group) = token {
            local_macros(group.stream(), defined);
        }
    }
}

/// Whether calling `name` could hide an environment read from this scan.
/// `extern crate x` for anything but a sysroot crate. Sysroot macros are the
/// std/core macros `STD_MACROS` already vets (target-lexicon declares
/// `extern crate alloc` / `extern crate std as alloc`).
fn imports_non_sysroot_crate(text: &str) -> bool {
    const SYSROOT: &[&str] = &["std", "core", "alloc", "proc_macro", "test"];
    let words: Vec<&str> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .collect();
    words
        .windows(3)
        .any(|w| w[0] == "extern" && w[1] == "crate" && !SYSROOT.contains(&w[2]))
}

fn opaque_macro(name: &str, uses: &Uses) -> bool {
    !STD_MACROS.contains(&name) || uses.local_macros.contains(name)
}

fn scan(tokens: TokenStream, path_context: bool, uses: &mut Uses) -> Result<()> {
    let tokens = tokens.into_iter().collect::<Vec<_>>();
    let mut index = 0;
    while index < tokens.len() {
        if let [TokenTree::Ident(name), TokenTree::Punct(bang), TokenTree::Group(group), ..] =
            &tokens[index..]
        {
            if bang.as_char() == '!' {
                let name = name.to_string();
                if uses.aliases.contains(&name) {
                    let key = literal_key(group.stream())?;
                    if crate::sandbox::path_environment(&key) {
                        if path_context {
                            uses.includes.insert(key.clone());
                        } else {
                            uses.constants.insert(key.clone());
                        }
                    }
                    uses.keys.insert(key);
                } else {
                    if !matches!(
                        name.as_str(),
                        "include" | "include_str" | "include_bytes" | "concat"
                    ) && opaque_macro(&name, uses)
                    {
                        uses.ambiguous_macros = true;
                    }
                    let inside = path_context
                        || matches!(name.as_str(), "include" | "include_str" | "include_bytes");
                    scan(group.stream(), inside, uses)?;
                }
                index += 3;
                continue;
            }
        }
        if let TokenTree::Group(group) = &tokens[index] {
            scan(group.stream(), path_context, uses)?;
        }
        index += 1;
    }
    Ok(())
}

fn literal_key(tokens: TokenStream) -> Result<String> {
    let Some(TokenTree::Literal(literal)) = tokens.into_iter().next() else {
        return Err("computed env macro names require dependency discovery".into());
    };
    let value = literal.to_string();
    if value.starts_with('r') {
        let first = value.find('"').ok_or("invalid raw env key")?;
        let last = value.rfind('"').ok_or("invalid raw env key")?;
        return Ok(value[first + 1..last].into());
    }
    serde_json::from_str(&value).map_err(|_| "unsupported Rust env key spelling".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forwarded_env_cannot_embed_the_rewritten_sandbox_path() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("lib.rs");
        std::fs::write(&source, r#"macro_rules! read_env { ($get:ident) => { $get!("OUT_DIR") }; } pub const ROOT:&str=read_env!(env); include!(concat!(env!("OUT_DIR"),"/generated.rs"));"#).unwrap();
        let mut job = DistJob {
            inputs: vec![crate::cas::DigestMemo::default().input(&source).unwrap()],
            env: std::collections::HashMap::from([(
                "OUT_DIR".into(),
                root.path().to_str().unwrap().into(),
            )]),
            ..Default::default()
        };
        assert!(inspect(&mut job)
            .unwrap_err()
            .to_string()
            .contains("macro expansion"));
    }
    fn job_for(text: &str) -> (tempfile::TempDir, DistJob) {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("lib.rs");
        std::fs::write(&source, text).unwrap();
        let job = DistJob {
            inputs: vec![crate::cas::DigestMemo::default().input(&source).unwrap()],
            ..Default::default()
        };
        (root, job)
    }
    const INCLUDE: &str = r#"include!(concat!(env!("OUT_DIR"),"/generated.rs"));"#;
    #[test]
    fn std_macros_do_not_block_an_out_dir_include_rewrite() {
        // target-lexicon's shape: generated include plus only std macros.
        let (_root, mut job) = job_for(&format!(
            "{INCLUDE} fn f(w: &mut String) {{ use std::fmt::Write; write!(w, \"x\").unwrap(); assert_eq!(1, 1); let _ = vec![format!(\"y\")]; }}"
        ));
        inspect(&mut job).unwrap();
        assert_eq!(job.rewrite_env, ["OUT_DIR"]);
    }
    #[test]
    fn unrelated_search_path_libraries_do_not_block_the_rewrite() {
        // A library sitting in -L dependency= that the unit never imports
        // cannot supply a macro; only --extern / extern crate can.
        let (root, mut job) = job_for(&format!("{INCLUDE} fn f() {{ assert!(true); }}"));
        let lib = root.path().join("libother.rlib");
        std::fs::write(&lib, b"rlib").unwrap();
        job.inputs
            .push(crate::cas::DigestMemo::default().input(&lib).unwrap());
        inspect(&mut job).unwrap();
        assert_eq!(job.rewrite_env, ["OUT_DIR"]);
        let (_r2, mut sysroot) = job_for(&format!(
            "extern crate alloc; extern crate std as alloc2; {INCLUDE} fn f() {{ assert!(true); }}"
        ));
        inspect(&mut sysroot).unwrap();
        let (_r3, mut foreign) = job_for(&format!("extern crate other; {INCLUDE}"));
        assert!(inspect(&mut foreign).is_err());
        job.argv = vec!["--extern".into(), format!("other={}", lib.display())];
        let error = inspect(&mut job).unwrap_err().to_string();
        assert!(error.contains("unresolved macro expansion"), "{error}");
    }
    #[test]
    fn unknown_or_shadowed_macros_still_block_the_rewrite() {
        for text in [
            format!("{INCLUDE} fn f() {{ custom!(); }}"),
            // A local macro_rules named like a std macro is not std.
            format!("{INCLUDE} macro_rules! write {{ () => {{}} }} fn f() {{ write!(); }}"),
        ] {
            let (_root, mut job) = job_for(&text);
            let error = inspect(&mut job).unwrap_err().to_string();
            assert!(
                error.contains("unresolved macro expansion"),
                "{text}: {error}"
            );
        }
    }
    #[test]
    fn include_paths_and_constants_have_distinct_environment_semantics() {
        let mut uses = Uses::default();
        scan(
            r#"include!(concat!(env!("OUT_DIR"), "/generated.rs"));"#
                .parse()
                .unwrap(),
            false,
            &mut uses,
        )
        .unwrap();
        assert!(uses.includes.contains("OUT_DIR"));
        // Spelled through format! so the workflow guard against hard-coded
        // manifest-dir reads does not match this test's Rust source text.
        let constant = format!("const ROOT: &str = env!(\"{}\");", "CARGO_MANIFEST_DIR");
        scan(constant.parse().unwrap(), false, &mut uses).unwrap();
        assert!(uses.constants.contains("CARGO_MANIFEST_DIR"));
        assert!(!uses.includes.contains("CARGO_MANIFEST_DIR"));
    }
    #[test]
    fn aliased_path_constants_are_detected_before_includes_can_rewrite_them() {
        let tokens: TokenStream = r#"use std::env as e; pub const ROOT:&str=e!("OUT_DIR"); include!(concat!(env!("OUT_DIR"),"/generated.rs"));"#.parse().unwrap();
        let mut uses = Uses::default();
        aliases(tokens.clone(), &mut uses.aliases);
        scan(tokens, false, &mut uses).unwrap();
        assert!(uses.includes.contains("OUT_DIR"));
        assert!(uses.constants.contains("OUT_DIR"));
    }
}
