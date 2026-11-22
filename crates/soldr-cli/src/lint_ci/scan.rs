//! Filesystem discovery and best-effort line normalization for the CI
//! policy engine (soldr#2038).
//!
//! This module deliberately does NOT parse YAML/shell into an AST. CI
//! surfaces mix YAML, inline `run:` shell, and referenced helper scripts;
//! a line/command-oriented scan with a few normalization passes is both
//! robust and provider-agnostic. The passes are:
//!
//! - discover executable surfaces (`.github/workflows`, `.github/actions`,
//!   `.github/scripts`, plus helper scripts referenced from a `run:`/`uses:`
//!   line),
//! - strip full-line and inline `#` comments (so prose/legacy mentions in
//!   comments are never flagged) while still reading inline suppressions,
//!   join shell `\` line-continuations into one logical command,
//! - remove balanced quoted spans for *tool* detection (so `echo "... cargo
//!   zigbuild ..."` summaries are not mistaken for invocations) while keeping
//!   the original text for `--target` extraction.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// An inline suppression directive: `soldr-lint-ci: allow <rule|all> [-- reason]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Suppression {
    All,
    Rules(Vec<String>),
}

impl Suppression {
    pub fn allows(&self, rule: &str) -> bool {
        match self {
            Suppression::All => true,
            Suppression::Rules(rules) => rules.iter().any(|r| r == rule),
        }
    }
}

/// One logical command line after normalization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogicalLine {
    /// 1-based line number where this logical line begins.
    pub line: u32,
    /// Comment-stripped text with quotes preserved — used for `--target`
    /// extraction (matrix placeholders live inside quotes).
    pub original: String,
    /// Comment-stripped text with balanced quoted spans removed — used for
    /// tool-token detection so quoted echoes are not flagged.
    pub tool_code: String,
    /// Whether the first physical line was a pure `#` comment.
    pub is_comment: bool,
    /// Whether this line is an executable command surface. For YAML this is
    /// only the content of `run:` blocks / inline `run:` values (never prose
    /// keys such as `name:`, `key:`, or `description:`). For shell/Python/
    /// PowerShell helper scripts every non-comment line is a command.
    pub is_command: bool,
}

/// A scanned CI surface file with everything the rules need.
#[derive(Clone, Debug)]
pub struct ScannedFile {
    /// Repo-root-relative path, `/`-separated.
    pub rel_path: String,
    pub lines: Vec<LogicalLine>,
    /// Concrete Apple/Windows/Linux target triples declared in matrix blocks.
    pub matrix_targets: Vec<String>,
    /// Whether the file mentions any Apple/Windows triple anywhere (used to
    /// decide whether an unresolved target warrants a warning).
    pub capable: bool,
    /// physical-line -> suppression directive present on that line.
    pub suppressions: BTreeMap<u32, Suppression>,
}

const SUPPRESS_MARKER: &str = "soldr-lint-ci:";

/// Discover the executable CI surfaces under `root`.
pub fn discover(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let gh = root.join(".github");

    collect_yaml(&gh.join("workflows"), &mut out);
    collect_action_yaml(&gh.join("actions"), &mut out);
    collect_all(&gh.join("scripts"), &mut out);

    for name in [
        "lint",
        "test",
        "push",
        "justfile",
        "Makefile",
        "local-gate.toml",
        "bosn.toml",
    ] {
        let path = root.join(name);
        if path.is_file() {
            out.push(path);
        }
    }
    walk(&root.join("ci"), &mut |path| {
        if has_ext(path, "py") {
            out.push(path.to_path_buf());
        }
    });

    // Best-effort: helper scripts referenced from a run:/uses: line.
    let referenced = collect_referenced_scripts(root, &out);
    for path in referenced {
        if !out.contains(&path) {
            out.push(path);
        }
    }

    out.sort();
    out.dedup();
    out
}

fn collect_yaml(dir: &Path, out: &mut Vec<PathBuf>) {
    walk(dir, &mut |path| {
        if has_ext(path, "yml") || has_ext(path, "yaml") {
            out.push(path.to_path_buf());
        }
    });
}

fn collect_action_yaml(dir: &Path, out: &mut Vec<PathBuf>) {
    walk(dir, &mut |path| {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == "action.yml" || name == "action.yaml" {
            out.push(path.to_path_buf());
        }
    });
}

fn collect_all(dir: &Path, out: &mut Vec<PathBuf>) {
    walk(dir, &mut |path| out.push(path.to_path_buf()));
}

/// Recursively invoke `visit` for every file under `dir`.
fn walk(dir: &Path, visit: &mut dyn FnMut(&Path)) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => walk(&path, visit),
            Ok(ft) if ft.is_file() => visit(&path),
            _ => {}
        }
    }
}

fn has_ext(path: &Path, ext: &str) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some(ext)
}

/// Scan `run:`/`uses:` lines of already-discovered YAML for repo-relative
/// helper-script paths (`*.sh`, `*.py`, `*.ps1`) that exist on disk.
fn collect_referenced_scripts(root: &Path, discovered: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for path in discovered {
        if !(has_ext(path, "yml") || has_ext(path, "yaml")) {
            continue;
        }
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(_) => continue,
        };
        for token in text.split(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
            let token = token.trim_start_matches("./");
            if !(token.ends_with(".sh") || token.ends_with(".py") || token.ends_with(".ps1")) {
                continue;
            }
            if token.contains("${{") || token.starts_with('-') {
                continue;
            }
            let candidate = root.join(token);
            if candidate.is_file() {
                out.push(candidate);
            }
        }
    }
    out
}

/// Read and normalize one file. Returns `None` if it cannot be read as UTF-8.
pub fn scan_file(root: &Path, path: &Path) -> Option<ScannedFile> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(scan_text(rel_path(root, path), &text))
}

/// Normalize raw file text into a [`ScannedFile`]. Split out from
/// [`scan_file`] so unit tests can drive in-memory fixtures. The command-
/// surface model is chosen from the `rel_path` extension: `.yml`/`.yaml`
/// files are treated as GitHub Actions YAML (only `run:` content is a
/// command); everything else is treated as a shell/script file.
pub fn scan_text(rel_path: String, text: &str) -> ScannedFile {
    let is_yaml = rel_path.ends_with(".yml") || rel_path.ends_with(".yaml");
    let physical: Vec<&str> = text.lines().collect();

    let mut suppressions = BTreeMap::new();
    for (idx, raw) in physical.iter().enumerate() {
        if let Some(sup) = parse_suppression(raw) {
            suppressions.insert((idx + 1) as u32, sup);
        }
    }

    let lines = build_logical_lines(&physical, is_yaml);
    let matrix_targets = collect_matrix_targets(&physical);
    let capable = text_is_capable(text);

    ScannedFile {
        rel_path,
        lines,
        matrix_targets,
        capable,
        suppressions,
    }
}

fn rel_path(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

/// Build logical lines: mark command surfaces, strip comments, then join
/// shell `\` continuations.
fn build_logical_lines(physical: &[&str], is_yaml: bool) -> Vec<LogicalLine> {
    let command_flags = if is_yaml {
        yaml_command_flags(physical)
    } else {
        // Non-YAML helper script: every physical line is a command surface.
        vec![true; physical.len()]
    };

    let mut out = Vec::new();
    let mut idx = 0;
    while idx < physical.len() {
        let start = (idx + 1) as u32;
        let first = physical[idx];
        let is_comment = first.trim_start().starts_with('#');
        let is_command = command_flags[idx];
        let mut code = strip_inline_comment(first);
        // Join shell backslash-continuations (the continuation lines are
        // absorbed into this logical command).
        while code.trim_end().ends_with('\\') && idx + 1 < physical.len() {
            let trimmed = code.trim_end();
            code = trimmed[..trimmed.len() - 1].to_string();
            idx += 1;
            code.push(' ');
            code.push_str(strip_inline_comment(physical[idx]).trim());
        }
        out.push(LogicalLine {
            line: start,
            tool_code: strip_balanced_quotes(&code),
            original: code,
            is_comment,
            is_command,
        });
        idx += 1;
    }
    out
}

/// Classify each physical YAML line as a command surface or not by tracking
/// `run:` inline values and `run: |` / `run: >` block scalars.
fn yaml_command_flags(physical: &[&str]) -> Vec<bool> {
    #[derive(Clone, Copy)]
    enum State {
        Outside,
        /// A `run:` block scalar was opened at this key indent; the content
        /// indent is not known until the first non-blank content line.
        Pending(usize),
        /// Inside a block scalar whose content is indented to this column.
        InBlock(usize),
    }

    let mut flags = vec![false; physical.len()];
    let mut state = State::Outside;

    for (idx, raw) in physical.iter().enumerate() {
        let indent = leading_ws(raw);
        let blank = raw.trim().is_empty();

        loop {
            match state {
                State::Pending(_) => {
                    if blank {
                        break;
                    }
                    state = State::InBlock(indent);
                    flags[idx] = true;
                    break;
                }
                State::InBlock(content_indent) => {
                    if blank || indent >= content_indent {
                        flags[idx] = true;
                        break;
                    }
                    // Dedent ends the block; re-evaluate this line fresh.
                    state = State::Outside;
                    continue;
                }
                State::Outside => {
                    if let Some(kind) = run_declaration(raw) {
                        match kind {
                            RunKind::Block => state = State::Pending(indent),
                            RunKind::Inline => flags[idx] = true,
                        }
                    }
                    break;
                }
            }
        }
    }
    flags
}

enum RunKind {
    /// `run: |` / `run: >` — content lives on following indented lines.
    Block,
    /// `run: <command>` — the command is on this line.
    Inline,
}

/// Detect a `run:` key on a YAML line and whether it opens a block scalar.
fn run_declaration(raw: &str) -> Option<RunKind> {
    let mut rest = raw.trim_start();
    // A step is a sequence item: allow leading `- ` markers.
    while let Some(stripped) = rest.strip_prefix("- ") {
        rest = stripped.trim_start();
    }
    let value = rest.strip_prefix("run:")?.trim();
    if value.is_empty() || value.starts_with('|') || value.starts_with('>') {
        Some(RunKind::Block)
    } else {
        Some(RunKind::Inline)
    }
}

fn leading_ws(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

/// Strip an inline ` #...` comment. Best-effort: cuts at the first `#`
/// preceded by whitespace or at column 0. Leaves `#` embedded in a token
/// (e.g. an URL fragment) intact.
fn strip_inline_comment(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            return line[..i].to_string();
        }
        i += 1;
    }
    line.to_string()
}

/// Remove balanced single/double-quoted spans that both open and close on
/// the same line, so tool tokens inside quoted echoes are not detected.
fn strip_balanced_quotes(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' || c == '\'' {
            let quote = c;
            let mut closed = false;
            let mut span = String::new();
            for inner in chars.by_ref() {
                if inner == quote {
                    closed = true;
                    break;
                }
                span.push(inner);
            }
            if !closed {
                // Unbalanced (quote opened on a previous line): keep content
                // so cross-line single-quoted `bash -c '...'` blocks still
                // expose their commands per physical line.
                out.push(quote);
                out.push_str(&span);
            } else {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_suppression(line: &str) -> Option<Suppression> {
    let marker = line.find(SUPPRESS_MARKER)?;
    let rest = line[marker + SUPPRESS_MARKER.len()..].trim();
    let rest = rest.strip_prefix("allow").unwrap_or(rest).trim();
    // Everything before an optional `--` reason separator names rules.
    let head = rest.split("--").next().unwrap_or("").trim();
    if head.is_empty() || head == "all" {
        return Some(Suppression::All);
    }
    let rules: Vec<String> = head
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    if rules.is_empty() {
        Some(Suppression::All)
    } else {
        Some(Suppression::Rules(rules))
    }
}

/// Collect concrete target triples declared in matrix blocks (`target:`,
/// `native_target:`, or a bare `- <triple>` list item).
fn collect_matrix_targets(physical: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for raw in physical {
        let trimmed = raw.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        let is_target_key = trimmed.starts_with("target:")
            || trimmed.starts_with("native_target:")
            || trimmed.starts_with("host_target:")
            || trimmed.starts_with("- target:")
            || trimmed.starts_with("host:");
        let is_bare_item = trimmed.starts_with("- ") && !trimmed.contains(':');
        if !(is_target_key || is_bare_item) {
            continue;
        }
        let code = strip_inline_comment(trimmed);
        for token in tokenize(&code) {
            if classify_triple(&token) != TargetKind::Unknown {
                out.push(token);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn text_is_capable(text: &str) -> bool {
    tokenize(text).any(|t| {
        matches!(
            classify_triple(&t),
            TargetKind::Apple | TargetKind::WindowsMsvc | TargetKind::WindowsGnu
        )
    })
}

/// Split on whitespace and common shell separators, trimming quotes/commas.
pub fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '(' || c == ')')
        .map(|t| t.trim_matches(|c| c == '"' || c == '\'').to_string())
        .filter(|t| !t.is_empty())
}

/// The classification of a target triple string, for policy purposes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    Apple,
    WindowsMsvc,
    WindowsGnu,
    Linux,
    /// A placeholder such as `${{ matrix.target }}` or `$TARGET`.
    Placeholder,
    Unknown,
}

/// Classify a `--target` value or a bare token.
pub fn classify_triple(value: &str) -> TargetKind {
    let v = value.trim_matches(|c| c == '"' || c == '\'' || c == '=');
    if v.contains("${{") || v.contains('$') || v.contains("{{") {
        return TargetKind::Placeholder;
    }
    if v.contains("-apple-darwin") || v.contains("-apple-ios") || v.contains("-apple-") {
        return TargetKind::Apple;
    }
    if v.contains("-pc-windows-msvc") {
        return TargetKind::WindowsMsvc;
    }
    if v.contains("-pc-windows-gnu") || v.contains("-w64-windows-gnu") {
        return TargetKind::WindowsGnu;
    }
    if v.contains("-linux-") || v.ends_with("-linux") || v.contains("manylinux") {
        return TargetKind::Linux;
    }
    TargetKind::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_full_line_and_inline_comments() {
        let f = scan_text(
            "x.sh".into(),
            "# cargo xwin build --target x86_64-pc-windows-msvc\nsoldr build --target x # cargo zigbuild",
        );
        assert!(f.lines[0].is_comment);
        assert_eq!(f.lines[0].tool_code.trim(), "");
        assert_eq!(f.lines[1].tool_code.trim(), "soldr build --target x");
    }

    #[test]
    fn balanced_quotes_removed_for_tool_code_but_kept_in_original() {
        let f = scan_text(
            "x.sh".into(),
            "echo \"soldr cargo zigbuild --target aarch64-apple-darwin\"",
        );
        assert!(!f.lines[0].tool_code.contains("zigbuild"));
        assert!(f.lines[0].original.contains("zigbuild"));
    }

    #[test]
    fn unbalanced_quote_keeps_command_visible() {
        // `bash -c '` opens a single-quote that closes on a later physical
        // line; the command line itself must stay visible.
        let f = scan_text("x.sh".into(), "soldr cargo xwin build --target x");
        assert!(f.lines[0].tool_code.contains("cargo xwin"));
    }

    #[test]
    fn backslash_continuation_joins() {
        let f = scan_text(
            "x.sh".into(),
            "cargo zigbuild \\\n  --target aarch64-apple-darwin",
        );
        assert_eq!(f.lines.len(), 1);
        assert!(f.lines[0]
            .original
            .contains("--target aarch64-apple-darwin"));
        assert_eq!(f.lines[0].line, 1);
    }

    #[test]
    fn suppression_parses_rule_and_reason() {
        let sup = parse_suppression(
            "  soldr cargo xwin # soldr-lint-ci: allow cross-compile-surface -- legacy test",
        )
        .unwrap();
        assert!(sup.allows("cross-compile-surface"));
        assert!(!sup.allows("other-rule"));
        let all = parse_suppression("# soldr-lint-ci: allow all").unwrap();
        assert!(all.allows("anything"));
    }

    #[test]
    fn matrix_targets_collected_from_declarations_only() {
        let f = scan_text(
            "wf.yml".into(),
            "    matrix:\n      target:\n        - x86_64-unknown-linux-gnu\n        - aarch64-apple-darwin\n    run: echo x86_64-pc-windows-msvc",
        );
        assert!(f
            .matrix_targets
            .contains(&"x86_64-unknown-linux-gnu".to_string()));
        assert!(f
            .matrix_targets
            .contains(&"aarch64-apple-darwin".to_string()));
        // The windows triple only appears in an echo, not a matrix decl.
        assert!(!f
            .matrix_targets
            .contains(&"x86_64-pc-windows-msvc".to_string()));
    }

    #[test]
    fn classify_triple_kinds() {
        assert_eq!(classify_triple("aarch64-apple-darwin"), TargetKind::Apple);
        assert_eq!(
            classify_triple("x86_64-pc-windows-msvc"),
            TargetKind::WindowsMsvc
        );
        assert_eq!(
            classify_triple("x86_64-unknown-linux-gnu"),
            TargetKind::Linux
        );
        assert_eq!(
            classify_triple("${{ matrix.target }}"),
            TargetKind::Placeholder
        );
        assert_eq!(
            classify_triple("wasm32-unknown-unknown"),
            TargetKind::Unknown
        );
    }
}
