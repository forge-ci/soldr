//! Integration-test *link-target* census for `soldr ci-test` (soldr#2936,
//! phase 4 of soldr#2931).
//!
//! # Why this exists
//!
//! soldr accumulated 98 top-level files under `crates/soldr-cli/tests/` one
//! reasonable PR at a time. Each top-level file under `crates/<crate>/tests/`
//! is its own Cargo *target*, and each target links the whole dependency graph
//! statically. Nothing in the tooling said a word until the CI test archive
//! reached 3.3 GB and exhausted a hosted runner's disk.
//!
//! The signal the repo was missing is therefore a count of **link products**,
//! not of `#[test]` functions. Many tests in few binaries is the goal state;
//! few tests in many binaries is the failure. That distinction is the whole
//! point of this module, so the counting rule below follows Cargo's real
//! target-discovery semantics rather than "every `.rs` file under `tests/`":
//!
//! * `tests/<name>.rs` — one target per file.
//! * `tests/<dir>/main.rs` — one target for the whole directory; its sibling
//!   `.rs` files are `mod`s of it and cost nothing extra. That is exactly the
//!   consolidation soldr#2931 is performing, so counting the siblings would
//!   report the fix as if it were the problem.
//! * A directory under `tests/` with no `main.rs` is not a target at all —
//!   Cargo ignores it (`tests/common/`, `tests/fixtures/`).
//! * `autotests = false` turns the auto-discovery above off entirely, and
//!   explicit `[[test]]` entries are targets regardless.
//!
//! # This never fails a build
//!
//! Every fallible step here degrades to "count what we could read". An
//! unparseable manifest, a missing `tests/` directory, or an unreadable member
//! is skipped rather than raised: a census that can break `soldr ci-test`
//! would be worse than the disk exhaustion it warns about.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::color_choice::{paint, YELLOW};

/// Overrides [`DEFAULT_WARN_THRESHOLD`]. `0` disables the warning entirely.
pub(crate) const WARN_COUNT_ENV_VAR: &str = "SOLDR_TEST_TARGET_WARN_COUNT";

/// Target count above which the census shouts.
///
/// 50 is deliberately well under the ~98 that actually broke a runner: the
/// warning has to arrive while consolidation is still a morning's work.
pub(crate) const DEFAULT_WARN_THRESHOLD: u64 = 50;

/// Second line of the soldr#2936 warning, verbatim.
const CONSOLIDATE_LINE: &str =
    "THIS WILL RESULT IN LOTS OF STATIC LINKING, PLEASE CONSOLIDATE INTO TEST CATEGORIES";

/// The configured threshold for this process.
///
/// This is a *count*, not a flag, so `core::env_flag` — soldr#2740's one
/// definition of "is this variable on" — deliberately does not apply; that
/// module parses on/off spellings. The idiom for a numeric knob in this crate
/// is trim-then-parse with the default on any failure, and "warn, never fail"
/// makes that the only defensible reading of a malformed value: a typo in an
/// advisory threshold must not break `soldr ci-test`.
pub(crate) fn warn_threshold() -> u64 {
    let Ok(raw) = std::env::var(WARN_COUNT_ENV_VAR) else {
        return DEFAULT_WARN_THRESHOLD;
    };
    match raw.trim().parse::<u64>() {
        Ok(threshold) => threshold,
        Err(_) => DEFAULT_WARN_THRESHOLD,
    }
}

/// Every integration-test link target in the workspace containing `start`.
///
/// `start` is the nearest manifest directory, which is not necessarily the
/// workspace root: `soldr ci-test` run from inside `crates/soldr-cli` must
/// still count the workspace its `--workspace` stages will compile, not just
/// the one package underfoot.
pub(crate) fn count_workspace_test_targets(start: &Path) -> u64 {
    let root = resolve_workspace_root(start);
    let mut total = 0;
    for dir in workspace_member_dirs(&root) {
        total += crate_test_target_names(&dir).len() as u64;
    }
    total
}

/// The stderr payload for an over-threshold workspace, or `None` when the
/// census is within budget (or switched off with `0`).
///
/// Pure, so both renderings are unit-asserted without capturing stderr.
pub(crate) fn render_warning(
    count: u64,
    threshold: u64,
    github_actions: bool,
    use_color: bool,
) -> Option<String> {
    if threshold == 0 || count <= threshold {
        return None;
    }
    // The two shout lines are the soldr#2936 contract; the third is what makes
    // the warning actionable (the real count, and how to retune or silence it).
    let detected = format!("WARNING: LOTS AND LOTS OF TESTS DETECTED (>{threshold})");
    let detail = format!(
        "soldr ci-test: {count} integration-test link targets in this workspace; each one \
         statically links the whole dependency graph. Consolidate into \
         `tests/<category>/main.rs` modules (soldr#2931). Set {WARN_COUNT_ENV_VAR}=<n> to \
         retune, or 0 to silence."
    );

    let mut lines: Vec<String> = Vec::new();
    lines.push(paint(&detected, YELLOW, use_color));
    lines.push(paint(CONSOLIDATE_LINE, YELLOW, use_color));
    lines.push(detail.clone());
    if github_actions {
        // A workflow-command annotation is one line by definition, so the
        // block's newlines are percent-encoded or Actions truncates it at the
        // first one. This is what lifts the warning out of the raw log and onto
        // the run summary / PR checks page, and it is *additional* to the human
        // block rather than a replacement for it.
        let block = [detected.as_str(), CONSOLIDATE_LINE, detail.as_str()];
        let body = block.join("%0A");
        lines.push(format!("::warning::{body}"));
    }
    Some(lines.join("\n"))
}

/// Emit the census warning on stderr when the workspace is over threshold.
///
/// Advisory only: this never touches an exit code (soldr#2936). soldr's own
/// workspace is expected to trip it until soldr#2931's consolidation lands,
/// and that is the honest reading, not a bug.
pub(crate) fn warn_if_excessive(count: u64, threshold: u64) {
    let actions = github_actions();
    if let Some(message) = render_warning(
        count,
        threshold,
        actions,
        crate::color_choice::stderr_enabled(),
    ) {
        eprintln!("{message}");
    }
}

/// `GITHUB_ACTIONS` is defined by GitHub, not by soldr, so it takes the
/// foreign denylist rule (soldr#2740).
fn github_actions() -> bool {
    crate::core::foreign_flag("GITHUB_ACTIONS")
}

/// The manifest directory that owns the `[workspace]` table for `start`.
///
/// An explicit `package.workspace` pointer wins, as in Cargo; otherwise the
/// nearest ancestor manifest declaring `[workspace]` does. Falling back to
/// `start` is safe — a lone package is its own workspace.
fn resolve_workspace_root(start: &Path) -> PathBuf {
    if let Some(manifest) = read_manifest(start) {
        if manifest.get("workspace").is_some() {
            return start.to_path_buf();
        }
        let package = manifest.get("package");
        let pointer = package.and_then(|table| table.get("workspace"));
        if let Some(relative) = pointer.and_then(toml::Value::as_str) {
            let candidate = start.join(relative);
            if candidate.join("Cargo.toml").is_file() {
                return candidate;
            }
        }
    }
    for ancestor in start.ancestors().skip(1) {
        let Some(manifest) = read_manifest(ancestor) else {
            continue;
        };
        if manifest.get("workspace").is_some() {
            return ancestor.to_path_buf();
        }
    }
    start.to_path_buf()
}

/// Directories of every workspace member, plus the root when the root manifest
/// is itself a package (soldr's root is virtual, but a single-crate consumer's
/// is not).
fn workspace_member_dirs(root: &Path) -> Vec<PathBuf> {
    let Some(manifest) = read_manifest(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    if manifest.get("package").is_some() {
        dirs.push(root.to_path_buf());
    }
    let Some(workspace) = manifest.get("workspace") else {
        return dirs;
    };
    let mut excluded: BTreeSet<String> = BTreeSet::new();
    for value in string_array(workspace.get("exclude")) {
        excluded.insert(normalize_relative(&value));
    }
    for pattern in string_array(workspace.get("members")) {
        for candidate in expand_member_pattern(root, &pattern) {
            let relative = match candidate.strip_prefix(root) {
                Ok(suffix) => normalize_relative(&suffix.display().to_string()),
                Err(_) => String::new(),
            };
            if excluded.contains(&relative) || dirs.contains(&candidate) {
                continue;
            }
            dirs.push(candidate);
        }
    }
    dirs
}

/// Expand one `[workspace].members` entry into concrete directories.
///
/// Known limit: `*` matches within a single path component, which covers the
/// `crates/*` form the ecosystem actually uses. A recursive `**` is treated as
/// a single-level wildcard rather than special-cased; an under-count here only
/// weakens an advisory warning, so a full glob dependency is not worth the
/// surface.
fn expand_member_pattern(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut current = vec![root.to_path_buf()];
    for component in normalize_relative(pattern).split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        let mut next: Vec<PathBuf> = Vec::new();
        for base in &current {
            if !component.contains('*') {
                next.push(base.join(component));
                continue;
            }
            let Ok(entries) = std::fs::read_dir(base) else {
                continue;
            };
            let mut matched: Vec<PathBuf> = Vec::new();
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !entry.path().is_dir() {
                    continue;
                }
                if component_matches(component, &name) {
                    matched.push(entry.path());
                }
            }
            matched.sort();
            next.extend(matched);
        }
        current = next;
    }
    current.retain(|path| path.join("Cargo.toml").is_file());
    current
}

/// `*`-only wildcard match for a single path component.
fn component_matches(pattern: &str, name: &str) -> bool {
    let mut cursor = name;
    let segments: Vec<&str> = pattern.split('*').collect();
    for (index, segment) in segments.iter().enumerate() {
        if segment.is_empty() {
            continue;
        }
        if index == 0 {
            let Some(rest) = cursor.strip_prefix(segment) else {
                return false;
            };
            cursor = rest;
        } else if index == segments.len() - 1 {
            return cursor.len() >= segment.len() && cursor.ends_with(segment);
        } else {
            let Some(offset) = cursor.find(segment) else {
                return false;
            };
            cursor = &cursor[offset + segment.len()..];
        }
    }
    // A trailing `*` (or a bare `*`) leaves whatever remains unconstrained.
    segments.last().is_none_or(|segment| segment.is_empty()) || cursor.is_empty()
}

/// The names of every integration-test target one package produces.
///
/// Names rather than a bare count, because an explicit `[[test]]` entry that
/// re-declares an auto-discovered file is still one link product; a set makes
/// that dedup fall out instead of double-counting it.
fn crate_test_target_names(manifest_dir: &Path) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    let Some(manifest) = read_manifest(manifest_dir) else {
        return names;
    };
    let Some(package) = manifest.get("package") else {
        // Virtual manifest: it declares members, never targets.
        return names;
    };

    let explicit = manifest.get("test").and_then(toml::Value::as_array);
    for entry in explicit.into_iter().flatten() {
        if let Some(name) = entry.get("name").and_then(toml::Value::as_str) {
            names.insert(name.to_string());
        } else if let Some(path) = entry.get("path").and_then(toml::Value::as_str) {
            if let Some(stem) = Path::new(path).file_stem() {
                names.insert(stem.to_string_lossy().into_owned());
            }
        }
    }

    let autotests = package.get("autotests").and_then(toml::Value::as_bool);
    if autotests.unwrap_or(true) {
        let tests_dir = manifest_dir.join("tests");
        names.extend(auto_discovered_test_targets(&tests_dir));
    }
    names
}

/// Cargo's auto-discovery rule, and only that rule: `tests/*.rs` plus
/// `tests/<dir>/main.rs`.
fn auto_discovered_test_targets(tests_dir: &Path) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(tests_dir) else {
        return names;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            // Sibling modules of a `main.rs` are compiled *into* it — the
            // consolidated shape soldr#2931 is moving toward — so the whole
            // directory is one target, and a directory without `main.rs`
            // (tests/common/, tests/fixtures/) is none at all.
            if path.join("main.rs").is_file() {
                names.insert(name);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            if let Some(stem) = path.file_stem() {
                names.insert(stem.to_string_lossy().into_owned());
            }
        }
    }
    names
}

fn read_manifest(dir: &Path) -> Option<toml::Value> {
    let contents = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    toml::from_str(&contents).ok()
}

fn string_array(value: Option<&toml::Value>) -> Vec<String> {
    let mut values: Vec<String> = Vec::new();
    let Some(entries) = value.and_then(toml::Value::as_array) else {
        return values;
    };
    for entry in entries {
        if let Some(text) = entry.as_str() {
            values.push(text.to_string());
        }
    }
    values
}

fn normalize_relative(value: &str) -> String {
    value.replace('\\', "/").trim_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EnvVarGuard;
    use crate::TEST_PROCESS_ENV_LOCK;

    const VIRTUAL_ROOT: &str = "[workspace]\nmembers = [\"crates/*\"]\n";
    const ALPHA: &str = "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n";
    const BETA: &str = "[package]\nname = \"beta\"\nversion = \"0.1.0\"\n";
    const EXPLICIT_TEST: &str = "\n[[test]]\nname = \"one\"\npath = \"tests/one.rs\"\n";

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&path, contents).expect("write file");
    }

    /// A workspace whose single member holds `count` top-level test files —
    /// the shape soldr#2936 exists to notice.
    fn workspace_with_top_level_tests(count: u64) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "Cargo.toml", VIRTUAL_ROOT);
        write(dir.path(), "crates/alpha/Cargo.toml", ALPHA);
        for index in 0..count {
            let relative = format!("crates/alpha/tests/t{index}.rs");
            write(dir.path(), &relative, "");
        }
        dir
    }

    #[test]
    fn counts_link_targets_not_test_functions() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "Cargo.toml", VIRTUAL_ROOT);
        write(dir.path(), "crates/alpha/Cargo.toml", ALPHA);
        // One target: a standalone file, however many `#[test]`s it holds.
        let solo = "#[test] fn a() {}\n#[test] fn b() {}\n";
        write(dir.path(), "crates/alpha/tests/solo.rs", solo);
        // One target: a category directory, however many modules it holds.
        write(dir.path(), "crates/alpha/tests/cat/main.rs", "mod one;\n");
        write(dir.path(), "crates/alpha/tests/cat/one.rs", "");
        write(dir.path(), "crates/alpha/tests/cat/two.rs", "");
        // Zero targets: support directories Cargo never links.
        write(dir.path(), "crates/alpha/tests/common/helpers.rs", "");
        write(dir.path(), "crates/alpha/tests/fixtures/data.txt", "");
        write(dir.path(), "crates/beta/Cargo.toml", BETA);
        write(dir.path(), "crates/beta/tests/only.rs", "");

        assert_eq!(count_workspace_test_targets(dir.path()), 3);
    }

    #[test]
    fn autotests_false_leaves_only_the_declared_targets() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "Cargo.toml", VIRTUAL_ROOT);
        let manifest = format!("{ALPHA}autotests = false\n{EXPLICIT_TEST}");
        write(dir.path(), "crates/alpha/Cargo.toml", &manifest);
        write(dir.path(), "crates/alpha/tests/one.rs", "");
        write(dir.path(), "crates/alpha/tests/two.rs", "");
        write(dir.path(), "crates/alpha/tests/three.rs", "");

        assert_eq!(count_workspace_test_targets(dir.path()), 1);
    }

    #[test]
    fn an_explicit_entry_for_a_discovered_file_is_one_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "Cargo.toml", VIRTUAL_ROOT);
        let manifest = format!("{ALPHA}{EXPLICIT_TEST}");
        write(dir.path(), "crates/alpha/Cargo.toml", &manifest);
        write(dir.path(), "crates/alpha/tests/one.rs", "");

        assert_eq!(count_workspace_test_targets(dir.path()), 1);
    }

    #[test]
    fn excluded_members_and_virtual_roots_contribute_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = format!("{VIRTUAL_ROOT}exclude = [\"crates/beta\"]\n");
        write(dir.path(), "Cargo.toml", &root);
        write(dir.path(), "crates/alpha/Cargo.toml", ALPHA);
        write(dir.path(), "crates/alpha/tests/one.rs", "");
        write(dir.path(), "crates/beta/Cargo.toml", BETA);
        write(dir.path(), "crates/beta/tests/two.rs", "");

        assert_eq!(count_workspace_test_targets(dir.path()), 1);
    }

    /// `soldr ci-test` run from inside a member still counts the whole
    /// workspace, because its stages compile the whole workspace.
    #[test]
    fn a_member_directory_resolves_up_to_the_workspace_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "Cargo.toml", VIRTUAL_ROOT);
        write(dir.path(), "crates/alpha/Cargo.toml", ALPHA);
        write(dir.path(), "crates/alpha/tests/one.rs", "");
        write(dir.path(), "crates/beta/Cargo.toml", BETA);
        write(dir.path(), "crates/beta/tests/one.rs", "");

        let member = dir.path().join("crates/alpha");
        assert_eq!(count_workspace_test_targets(dir.path()), 2);
        assert_eq!(
            count_workspace_test_targets(&member),
            2,
            "a member must resolve up to the workspace it belongs to"
        );
    }

    /// A single-crate repo is its own workspace and still gets a census.
    #[test]
    fn a_lone_package_counts_its_own_targets() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "Cargo.toml", ALPHA);
        write(dir.path(), "tests/one.rs", "");
        write(dir.path(), "tests/two.rs", "");

        assert_eq!(count_workspace_test_targets(dir.path()), 2);
    }

    #[test]
    fn component_wildcards_match_a_single_path_component() {
        assert!(component_matches("*", "anything"));
        assert!(component_matches("soldr-*", "soldr-cli"));
        assert!(component_matches("*-cli", "soldr-cli"));
        assert!(component_matches("soldr-*-cli", "soldr-x-cli"));
        assert!(!component_matches("soldr-*", "zccache"));
        assert!(!component_matches("*-cli", "soldr-daemon"));
    }

    /// Both halves wired together: a real directory tree, counted, then judged
    /// against the default threshold.
    #[test]
    fn a_workspace_one_target_over_the_default_warns() {
        let under = workspace_with_top_level_tests(DEFAULT_WARN_THRESHOLD);
        let count = count_workspace_test_targets(under.path());
        assert_eq!(count, DEFAULT_WARN_THRESHOLD);
        assert!(
            render_warning(count, DEFAULT_WARN_THRESHOLD, false, false).is_none(),
            "exactly {DEFAULT_WARN_THRESHOLD} targets is still within budget"
        );

        let over = workspace_with_top_level_tests(DEFAULT_WARN_THRESHOLD + 1);
        let count = count_workspace_test_targets(over.path());
        assert_eq!(count, DEFAULT_WARN_THRESHOLD + 1);
        let warning = render_warning(count, DEFAULT_WARN_THRESHOLD, false, false)
            .expect("one target over the threshold must warn");
        assert!(warning.contains("LOTS AND LOTS OF TESTS DETECTED"));
    }

    /// The boundary the message names: `>N`, not `>=N`.
    #[test]
    fn the_threshold_is_exclusive_at_below_and_above() {
        assert!(
            render_warning(49, 50, false, false).is_none(),
            "below the threshold must stay quiet"
        );
        assert!(
            render_warning(50, 50, false, false).is_none(),
            "at the threshold is still within budget -- the message says `>N`"
        );
        let warning = render_warning(51, 50, false, false).expect("above must warn");
        assert!(warning.contains("WARNING: LOTS AND LOTS OF TESTS DETECTED (>50)"));
        assert!(warning.contains(CONSOLIDATE_LINE));
        assert!(
            warning.contains("51 integration-test link targets"),
            "the actual count has to be actionable: {warning}"
        );
    }

    #[test]
    fn zero_disables_the_warning_at_any_count() {
        for count in [0, 51, 10_000] {
            assert!(
                render_warning(count, 0, true, true).is_none(),
                "{WARN_COUNT_ENV_VAR}=0 must silence the census at {count}"
            );
        }
    }

    /// Terminal rendering is ANSI yellow and carries no workflow command;
    /// Actions rendering carries both — the human block is painted too, since
    /// soldr#3437 made `GITHUB_ACTIONS` *imply* color — with the block's
    /// newlines encoded so the annotation is not truncated at the first one.
    ///
    /// Soldr#3437 changed the Actions case: it used to be called with
    /// `use_color = false`, pinning the divergent rule this issue removed.
    #[test]
    fn actions_annotates_while_a_terminal_only_colors() {
        let head = "WARNING: LOTS AND LOTS OF TESTS DETECTED";
        let terminal = render_warning(60, 50, false, true).expect("over");
        let painted = crate::color_choice::paint(&format!("{head} (>50)"), YELLOW, true);
        assert!(terminal.contains(&painted), "{terminal}");
        assert!(
            !terminal.contains("::warning::"),
            "a plain terminal must not emit a workflow command: {terminal}"
        );

        let plain = render_warning(60, 50, false, false).expect("over");
        assert!(!plain.contains('\u{1b}'), "no ANSI expected: {plain}");

        let actions = render_warning(60, 50, true, true).expect("over");
        let annotation = actions
            .lines()
            .find(|line| line.starts_with("::warning::"))
            .expect("Actions must get an annotation line");
        assert!(annotation.contains(head));
        assert!(annotation.contains(CONSOLIDATE_LINE));
        assert!(
            annotation.contains("%0A"),
            "a multi-line annotation must encode its newlines: {annotation}"
        );
        assert!(
            !annotation.contains('\u{1b}'),
            "a workflow command must stay plain even when the human block is \
             painted: {annotation}"
        );
        // Color-agnostic under the soldr#3437 ruling: the human block is
        // painted, so its line starts with an ANSI escape, not the text —
        // and the annotation line contains `head` too, so exclude it by its
        // workflow-command prefix rather than by position.
        assert!(
            actions
                .lines()
                .any(|line| !line.starts_with("::warning::") && line.contains(head)),
            "the annotation is *additional* to the human block: {actions}"
        );
        assert!(
            actions.contains(&painted),
            "soldr#3437: Actions implies color for the human block too: {actions}"
        );
    }

    #[test]
    fn the_env_var_overrides_the_default_and_survives_garbage() {
        // soldr#1663 / #1899: recover the barrier rather than unwrapping it. A
        // panic anywhere under the *shared* environment lock poisons it for
        // every other module, so a bare `.unwrap()` here would turn one
        // unrelated failure into a cascade of extra ones.
        let _lock = TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        {
            let _guard = EnvVarGuard::remove(WARN_COUNT_ENV_VAR);
            assert_eq!(warn_threshold(), DEFAULT_WARN_THRESHOLD);
        }
        {
            let _guard = EnvVarGuard::set(WARN_COUNT_ENV_VAR, " 12 ");
            assert_eq!(warn_threshold(), 12, "trimmed, then parsed");
        }
        {
            let _guard = EnvVarGuard::set(WARN_COUNT_ENV_VAR, "0");
            assert_eq!(warn_threshold(), 0);
            let threshold = warn_threshold();
            assert!(render_warning(999, threshold, false, false).is_none());
        }
        // Warn, never fail: a malformed or empty threshold falls back to the
        // default rather than erroring out of `soldr ci-test`.
        for garbage in ["", "   ", "fifty", "-1", "12.5"] {
            let _guard = EnvVarGuard::set(WARN_COUNT_ENV_VAR, garbage);
            assert_eq!(
                warn_threshold(),
                DEFAULT_WARN_THRESHOLD,
                "{garbage:?} must fall back to the default"
            );
        }
    }
}
