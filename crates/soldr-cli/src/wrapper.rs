//! `RUSTC_WRAPPER` invocation path: forwards rustc / clippy-driver through
//! the daemon's embedded zccache compile service via the broker SESSION
//! route, spills stdin to a temp file when cargo passes `-`. Extracted
//! from `main.rs` as part of issue #339. The legacy fork-zccache.exe
//! wrapper path was removed in #980 L1 second pass — embedded is mandatory.

use crate::core::{suppress_windows_console_window, SoldrError};
use crate::resolve_toolchain_binary;
use crate::startup_profile::WrapperProfile;

/// Known toolchain binaries that cargo may invoke through RUSTC_WRAPPER
/// or RUSTC_WORKSPACE_WRAPPER. When soldr is set as a wrapper, cargo
/// passes: `soldr <toolchain-binary> <rustc-args...>`
const WRAPPER_PASSTHROUGH_TOOLS: &[&str] = &["rustc", "clippy-driver"];

pub(crate) fn is_wrapper_invocation(arg: &str) -> bool {
    let stem = std::path::Path::new(arg)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(arg);

    WRAPPER_PASSTHROUGH_TOOLS.contains(&stem) || stem == "dylint-driver"
}

/// Detect rustc-style compiler invocations cargo issues through
/// `RUSTC_WRAPPER` that can never benefit from zccache (issue #980, L2):
///
///   * Any `--print`/`--print=...` probe — these don't compile, they
///     just print metadata (e.g. `--print sysroot`, `--print cfg`).
///   * `--emit` invocations whose emit-kind list is exclusively
///     `dep-info` — cargo's dep-graph refresh pass produces no object
///     code, so caching it via the wrapper is pure overhead.
///
/// `args` is the full argv as received in wrapper mode: `args[0]` is
/// the soldr binary, `args[1]` is the tool path (rustc / clippy-driver),
/// and `args[2..]` are the rustc-style arguments. With nested
/// `RUSTC_WRAPPER` + `RUSTC_WORKSPACE_WRAPPER=clippy-driver`,
/// `args[2]` is the real rustc path and `args[3..]` are the crate
/// arguments; scanning the whole tail still detects the no-op probes.
pub(crate) fn is_non_cacheable_rustc(args: &[String]) -> bool {
    if args.len() < 3 {
        return false;
    }
    let rest = &args[2..];

    // `--print <mode>` or `--print=<mode>` never compiles.
    if rest
        .iter()
        .any(|a| a == "--print" || a.starts_with("--print="))
    {
        return true;
    }

    // Version probes never compile. Cargo's very first call when it
    // resolves a toolchain is `rustc -vV`; the wrapper must direct-exec
    // it instead of routing through the daemon (which doesn't help and
    // costs the daemon-spawn latency on cold startups).
    if rest
        .iter()
        .any(|a| a == "-vV" || a == "-V" || a == "--version")
    {
        return true;
    }

    // Collect every emit-kind across all `--emit`/`--emit=` occurrences.
    // Cargo can spell either form (and may pass multiple), so accumulate
    // the union before deciding.
    let mut has_emit = false;
    let mut emit_kinds: Vec<&str> = Vec::new();
    let mut iter = rest.iter();
    while let Some(a) = iter.next() {
        if a == "--emit" {
            if let Some(v) = iter.next() {
                has_emit = true;
                emit_kinds.extend(v.split(','));
            }
        } else if let Some(v) = a.strip_prefix("--emit=") {
            has_emit = true;
            emit_kinds.extend(v.split(','));
        }
    }
    if has_emit && !emit_kinds.is_empty() {
        // `--emit=dep-info=foo.d` is still a dep-info-only emit; strip
        // the optional `=path` suffix before classifying.
        let only_dep_info = emit_kinds
            .iter()
            .all(|k| k.trim().split('=').next().unwrap_or("") == "dep-info");
        if only_dep_info {
            return true;
        }
    }

    false
}

fn routes_through_embedded_zccache(tool_stem: &str) -> bool {
    tool_stem == "dylint-driver" || WRAPPER_PASSTHROUGH_TOOLS.contains(&tool_stem)
}

/// Cargo nests the workspace compiler inside the outer wrapper as
/// `<outer> <workspace-compiler> <real-compiler> <compile-args...>`.
/// Once soldr has selected the workspace compiler, that wrapper-only
/// compiler identity must be consumed rather than forwarded as a source
/// input. Restrict the rewrite to known executable basenames so direct
/// invocations and ordinary source paths remain byte-for-byte intact.
fn normalize_nested_workspace_wrapper_args<'a>(
    args: &'a [String],
    tool_stem: &str,
) -> std::borrow::Cow<'a, [String]> {
    if tool_stem != WRAPPER_PASSTHROUGH_TOOLS[1] || args.len() < 3 {
        return std::borrow::Cow::Borrowed(args);
    }

    let nested_tool_name = std::path::Path::new(&args[2])
        .file_name()
        .and_then(std::ffi::OsStr::to_str);
    let expected_name = WRAPPER_PASSTHROUGH_TOOLS[0];
    let is_real_compiler = nested_tool_name.is_some_and(|name| {
        name.eq_ignore_ascii_case(expected_name)
            || name.rsplit_once('.').is_some_and(|(stem, extension)| {
                stem.eq_ignore_ascii_case(expected_name)
                    && ["exe", "cmd", "bat"]
                        .iter()
                        .any(|known| extension.eq_ignore_ascii_case(known))
            })
    });
    if !is_real_compiler {
        return std::borrow::Cow::Borrowed(args);
    }

    let mut normalized = Vec::with_capacity(args.len() - 1);
    normalized.extend_from_slice(&args[..2]);
    normalized.extend_from_slice(&args[3..]);
    std::borrow::Cow::Owned(normalized)
}

/// Explicit opt-in that permits a one-time dylint-driver source build
/// through the wrapper (soldr#2484). Off by default: binary-or-exit.
pub(crate) const ALLOW_DYLINT_DRIVER_BUILD_ENV_VAR: &str = "SOLDR_ALLOW_DYLINT_DRIVER_BUILD";

/// soldr#2945 made this `pub(crate)`: the Dylint front door honours the very
/// same switch, and two guards released by two spellings of "yes" would be the
/// soldr#2740 defect all over again.
pub(crate) fn allow_dylint_driver_build() -> bool {
    crate::core::flag(ALLOW_DYLINT_DRIVER_BUILD_ENV_VAR)
}

/// Detect a rustc invocation that compiles the `dylint_driver` crate
/// itself (as opposed to *executing* an installed dylint-driver against
/// user crates, where the crate names are the user's). Returns the
/// requested driver version when the registry source path reveals it.
pub(crate) fn nested_dylint_driver_build(rustc_args: &[String]) -> Option<String> {
    let mut is_driver_crate = false;
    for (index, arg) in rustc_args.iter().enumerate() {
        if arg == "--crate-name"
            && rustc_args.get(index + 1).map(String::as_str) == Some("dylint_driver")
        {
            is_driver_crate = true;
            break;
        }
        if arg == "--crate-name=dylint_driver" {
            is_driver_crate = true;
            break;
        }
    }
    if !is_driver_crate {
        return None;
    }
    let version = rustc_args
        .iter()
        .find_map(|arg| {
            let start = arg.find("dylint_driver-")?;
            let tail = &arg[start + "dylint_driver-".len()..];
            let end = tail
                .find(|c: char| c != '.' && !c.is_ascii_digit())
                .unwrap_or(tail.len());
            let candidate = &tail[..end];
            candidate
                .contains('.')
                .then(|| candidate.trim_end_matches('.').to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());
    Some(version)
}

fn nested_dylint_driver_diagnostic(requested_version: &str) -> String {
    format!(
        concat!(
            "soldr: refusing a nested dylint-driver source build ",
            "(requested dylint_driver v{}). Soldr's contract is ",
            "binary-or-exit (soldr#2432/#2484): an uncached compiler ",
            "plugin must not silently turn a lint/test run into a long ",
            "source build. Prepare the prebuilt driver first by running ",
            "`soldr cargo dylint --all` once in this workspace (it ",
            "resolves and verifies the exact version + nightly + host ",
            "driver under DYLINT_DRIVER_PATH), or explicitly permit the ",
            "one-time source build with {}=1."
        ),
        requested_version, ALLOW_DYLINT_DRIVER_BUILD_ENV_VAR
    )
}

pub(crate) fn run_rustc_wrapper(
    raw_args: &[String],
    mut profile: WrapperProfile,
) -> Result<i32, SoldrError> {
    let tool_arg = raw_args
        .get(1)
        .ok_or_else(|| SoldrError::Other("missing tool path in wrapper mode".into()))?;

    let tool_stem = std::path::Path::new(tool_arg.as_str())
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(tool_arg);

    profile.mark("tool_resolved");

    // soldr#2484: nested Dylint flows (dylint_testing and friends) launch
    // their own cargo build of the dylint-driver crate. The top-level
    // front door already refuses a missing/mismatched driver
    // (require_prebuilt_driver, soldr#2432), but a nested build re-enters
    // here as an ordinary rustc unit and would silently turn a lint/test
    // run into a long uncached source build. Fail closed before any
    // daemon contact; the one-time build needs an explicit opt-in.
    if let Some(requested) = nested_dylint_driver_build(&raw_args[2..]) {
        if !allow_dylint_driver_build() {
            return Err(SoldrError::Other(nested_dylint_driver_diagnostic(
                &requested,
            )));
        }
    }

    // Per-build target/ tracking for `soldr gc`. Best-effort: if we
    // can't resolve a workspace target dir cheaply, or the redb
    // upsert fails for any reason, skip silently — never fail a build.
    //
    // The phase emitted afterwards tells SOLDR_PROFILE_STARTUP=1
    // consumers which routing path fired — explicitly distinguishing
    // the daemon path from the fast direct-redb path proves the
    // Option-A invariant from #474: outside a soldr-cargo session,
    // no `daemon`/`is_live`/`socket`/`record_target_touch_or_fallback`
    // phase appears in the profile.
    if tool_stem == "rustc" {
        let path = record_target_dir_in_registry(&raw_args[2..]);
        let mark = match path {
            TargetTouchPath::NoTarget => "target_dir_recorded_no_target",
            TargetTouchPath::NoPaths => "target_dir_recorded_no_paths",
            TargetTouchPath::DaemonFirst => "target_dir_recorded_daemon",
            TargetTouchPath::MemoSkipped => "target_dir_recorded_memo",
        };
        profile.mark(mark);
    } else {
        profile.mark("target_dir_recorded");
    }

    // When the source argument is "-" (stdin), rustc reads the source from
    // the process's stdin. If we pass this invocation to zccache as-is,
    // zccache reads stdin to hash the source content, exhausting the pipe
    // before rustc is spawned. Rustc then receives an empty stdin, compiles
    // nothing, and exits 0 — masking any real compile error (e.g. E0554 from
    // build-script feature probes like rustix 0.37's `can_compile()`).
    //
    // Fix: spill stdin to a content-addressed temp file so both zccache and
    // rustc see a stable real path. This keeps zccache in the loop (it can
    // hash the file normally) while preserving the correct exit code, and it
    // lets identical feature probes converge on the same cache key.
    let stdin_tempfile = if raw_args[2..].iter().any(|a| a == "-") {
        Some(spill_stdin_to_content_addressed_file()?)
    } else {
        None
    };
    profile.mark("stdin_handled");

    // Build the effective arg list, replacing "-" with the temp file path.
    let effective_args: std::borrow::Cow<[String]> = if let Some(ref tmp) = stdin_tempfile {
        let tmp_str = tmp.path().to_string_lossy().into_owned();
        let replaced: Vec<String> = raw_args
            .iter()
            .cloned()
            .map(|a| if a == "-" { tmp_str.clone() } else { a })
            .collect();
        std::borrow::Cow::Owned(replaced)
    } else {
        std::borrow::Cow::Borrowed(raw_args)
    };
    let compile_args = normalize_nested_workspace_wrapper_args(&effective_args, tool_stem);
    // soldr#1992: `SOLDR_LINKER=fast` injects rust-lld through
    // `CARGO_TARGET_<TRIPLE>_LINKER`, which cargo applies to every crate --
    // including proc-macros, which build as DLLs and reliably fail that link
    // on MSVC with a bare `exit code: 1`. Cargo decides who receives the flag,
    // so this is the first point that sees a per-crate argv and can decline it.
    let compile_args = crate::linker::strip_fast_linker_for_proc_macro(
        &compile_args,
        crate::pyo3_detect::host_triple(),
    );

    // L2 cold-build skip (issue #980): some rustc invocations cargo
    // issues through RUSTC_WRAPPER can NEVER hit the cache — `--print
    // sysroot` / `--print cfg` probes do not compile anything, and
    // `--emit=dep-info`-only runs only refresh the dep graph. Routing
    // those through zccache pays the full wrapper startup tax (process
    // spawn, daemon IPC) for guaranteed-zero benefit. Detect them here
    // and let the existing direct-exec tool-spawn path below handle
    // them instead of the zccache routing block.
    let zccache_routed_tool = routes_through_embedded_zccache(tool_stem);
    let non_cacheable = zccache_routed_tool && is_non_cacheable_rustc(&compile_args);
    if non_cacheable {
        tracing::debug!("soldr: {tool_stem} invocation is non-cacheable; bypassing zccache");
        profile.mark("non_cacheable_bypass");
    }
    // Route rustc-like compiler invocations through the daemon's
    // embedded zccache compile service. This includes `clippy-driver`
    // when cargo nests `RUSTC_WORKSPACE_WRAPPER=clippy-driver` inside
    // soldr's `RUSTC_WRAPPER` for workspace crates.
    if zccache_routed_tool && !non_cacheable && crate::cache_lib::cache_enabled_in_current_process()
    {
        // L1 (issue #977 / #980 L1): dispatch the rustc invocation to
        // the daemon's embedded zccache service over IPC. The legacy
        // `zccache.exe` fork was deleted in the L1 second pass.
        profile.finish("before_embedded_compile_ipc");
        // soldr#1081 — lifted to `crate::compile_dispatch` so the
        // dedicated `zccache-soldr` shim binary can share the same
        // hang-safe retry logic.
        //
        // Cacheable rustc invocations always use the broker SESSION route.
        // The broker owns daemon acquisition and placement; failure is hard.
        return crate::compile_dispatch::compile_via_daemon(&compile_args[1..]);
    }

    direct_exec_tool(tool_arg, tool_stem, &compile_args, Some(profile))
}

/// Direct (uncached) exec of the wrapped tool — the non-daemon path.
/// Used only for clippy-driver and non-cacheable rustc invocations.
/// Preserves rustc's stdout/stderr/exit-code passthrough exactly: the
/// child inherits the wrapper's stdio and the exit code is returned
/// to cargo unchanged.
fn direct_exec_tool(
    tool_arg: &str,
    tool_stem: &str,
    effective_args: &[String],
    profile: Option<WrapperProfile>,
) -> Result<i32, SoldrError> {
    let tool_path = resolve_wrapper_tool_path(tool_arg, tool_stem)?;

    let mut command = std::process::Command::new(&tool_path);
    command.args(&effective_args[2..]);
    crate::binaries::apply_resolved_toolchain_homes(&mut command, &tool_path);
    suppress_windows_console_window(&mut command);
    if let Some(profile) = profile {
        profile.finish("before_tool_spawn");
    }
    let status =
        crate::platform::wait::status_for(&mut command, crate::platform::wait::LONG_CHILD)?;

    let exit_code = status.code().unwrap_or(1);
    // soldr#1974. The child inherits stdio here, so a process that dies at
    // DLL-init produces no diagnostics of its own and cargo reports only the
    // raw NTSTATUS. Name the condition before it gets mistaken for a broken
    // toolchain.
    crate::host_pressure::report_process_init_failure_to_stderr(tool_stem, exit_code);
    Ok(exit_code)
}

/// Resolve a wrapper compiler identity before deriving its toolchain homes.
/// Cargo may pass an absolute path, a relative path with components, or a bare
/// tool name. Every wrapper execution path must classify and execute the same
/// compiler Cargo supplied.
fn resolve_wrapper_tool_path(
    tool_arg: &str,
    tool_stem: &str,
) -> Result<std::path::PathBuf, SoldrError> {
    let tool_path = std::path::Path::new(tool_arg);
    if tool_path.is_absolute() {
        Ok(tool_path.to_path_buf())
    } else if tool_path.components().count() > 1 {
        Ok(std::env::current_dir()?.join(tool_path))
    } else {
        resolve_toolchain_binary(tool_stem)
    }
}

struct StdinSourceFile {
    path: std::path::PathBuf,
}

impl StdinSourceFile {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// Read all of stdin into a content-addressed source file and return it.
///
/// The file has a `.rs` extension so rustc accepts it without flags, and
/// lives in the system temp directory as `soldr-stdin-<short_blake3>.rs`.
/// It is intentionally retained so concurrent identical probes can share the
/// same stable path.
fn spill_stdin_to_content_addressed_file() -> Result<StdinSourceFile, SoldrError> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buf)
        .map_err(|e| SoldrError::Other(format!("failed to read stdin: {e}")))?;
    materialize_stdin_source(&buf)
}

fn materialize_stdin_source(bytes: &[u8]) -> Result<StdinSourceFile, SoldrError> {
    // soldr#1900: relocated off the OS temp dir (tmpfs on Linux). Note this
    // file is deliberately *reused* across invocations -- it is keyed by
    // content hash and `ensure_stdin_source_path` reports whether it already
    // existed -- so it is not a TempDir and must not auto-delete.
    materialize_stdin_source_in(&crate::core::ensure_temp_root(), bytes)
}

/// [`materialize_stdin_source`] against an explicit scratch root.
///
/// soldr#2006: `temp_root()` is memoized process-wide, so the first caller in
/// a test binary fixes the root for every later one. When that first call
/// happened while another test had `SOLDR_CACHE_DIR` pointed at its own
/// `TempDir`, the whole process's scratch root lived inside that `TempDir` and
/// vanished when it dropped -- failing unrelated tests that merely *read* the
/// global. `env_lock_lint` cannot see that: it tracks mutation sites, and
/// these tests mutate nothing.
///
/// Taking the root as a parameter removes the shared dependency rather than
/// synchronising around it, which is the same fix `build_env_block_in` made
/// for the ambient cwd in soldr#1929.
fn materialize_stdin_source_in(
    temp_dir: &std::path::Path,
    bytes: &[u8],
) -> Result<StdinSourceFile, SoldrError> {
    let hex = zccache::hash::hash_bytes(bytes).to_hex();
    let short_path = temp_dir.join(format!("soldr-stdin-{}.rs", &hex[..16]));
    if ensure_stdin_source_path(&short_path, bytes)? {
        return Ok(StdinSourceFile { path: short_path });
    }

    let full_path = temp_dir.join(format!("soldr-stdin-{hex}.rs"));
    if ensure_stdin_source_path(&full_path, bytes)? {
        return Ok(StdinSourceFile { path: full_path });
    }

    Err(SoldrError::Other(format!(
        "stdin temp path hash collision at {}",
        full_path.display()
    )))
}

/// How many times to rebuild the scratch file when the directory under it is
/// reclaimed mid-operation (soldr#2006).
///
/// One retry, not a loop: a single disappearance is a reclaimed scratch root
/// and is worth surviving; a second in immediate succession means something is
/// actively deleting the tree, and spinning would hide that rather than fix it.
const STDIN_PUBLISH_ATTEMPTS: u32 = 2;

fn ensure_stdin_source_path(path: &std::path::Path, bytes: &[u8]) -> Result<bool, SoldrError> {
    let mut last_err = None;
    for _ in 0..STDIN_PUBLISH_ATTEMPTS {
        match try_publish_stdin_source(path, bytes) {
            // soldr#2006: the scratch root can be reclaimed between creating
            // the temp file and renaming it into place -- `new_in` succeeds,
            // then `persist` reports the path is gone. Scratch is reclaimable
            // by design, so a vanished directory is a condition to recover
            // from, not a reason to fail the compile.
            Err(PublishError::RootVanished) => {
                let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
                let _ = std::fs::create_dir_all(parent);
                last_err = Some(PublishError::RootVanished);
            }
            other => return other.map_err(PublishError::into_soldr_error),
        }
    }
    Err(last_err
        .unwrap_or(PublishError::RootVanished)
        .into_soldr_error())
}

/// Failure modes of a single publish attempt.
enum PublishError {
    /// The scratch directory disappeared mid-operation -- retryable.
    RootVanished,
    /// Anything else -- not retryable.
    Fatal(SoldrError),
}

impl PublishError {
    fn into_soldr_error(self) -> SoldrError {
        match self {
            PublishError::Fatal(err) => err,
            PublishError::RootVanished => SoldrError::Other(
                "the soldr scratch directory was repeatedly removed while publishing the                  stdin source file; something is deleting it concurrently (soldr#2006)"
                    .to_string(),
            ),
        }
    }
}

fn try_publish_stdin_source(path: &std::path::Path, bytes: &[u8]) -> Result<bool, PublishError> {
    use std::io::Write as _;

    match std::fs::read(path) {
        Ok(existing) => return Ok(existing == bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(PublishError::Fatal(SoldrError::Other(format!(
                "failed to read existing stdin temp file {}: {err}",
                path.display()
            ))));
        }
    }

    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            return PublishError::RootVanished;
        }
        PublishError::Fatal(SoldrError::Other(format!(
            "failed to create stdin temp file in {}: {e}",
            parent.display()
        )))
    })?;
    tmp.write_all(bytes).map_err(|e| {
        PublishError::Fatal(SoldrError::Other(format!(
            "failed to write stdin temp file: {e}"
        )))
    })?;
    let _ = tmp.as_file().sync_all();

    match tmp.persist_noclobber(path) {
        Ok(_) => Ok(true),
        Err(err) if err.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = err.file.close();
            let existing = std::fs::read(path).map_err(|e| {
                PublishError::Fatal(SoldrError::Other(format!(
                    "failed to read raced stdin temp file {}: {e}",
                    path.display()
                )))
            })?;
            Ok(existing == bytes)
        }
        // Windows reports the missing directory as NotFound; some layers
        // surface it as os error 3 (path) rather than 2 (file). Both mean the
        // scratch root went away under us.
        Err(err) if err.error.kind() == std::io::ErrorKind::NotFound => {
            Err(PublishError::RootVanished)
        }
        Err(err) => Err(PublishError::Fatal(SoldrError::Other(format!(
            "failed to publish stdin temp file {}: {}",
            path.display(),
            err.error
        )))),
    }
}

// Routing logic + `TargetTouchPath` live in `wrapper_target.rs` so the
// integration tests in `tests/cargo_front_door/cli_wrapper_perf.rs` can drive
// `record_target_dir_in_registry` in-process via the lib's
// `pub mod wrapper_target;` declaration. Re-exported here so existing
// bin-side call sites in this file keep working unchanged.
pub(crate) use crate::wrapper_target::{record_target_dir_in_registry, TargetTouchPath};

// =========================================================================
// L1 daemon-embedded compile path (issue #977 / #980 L1)
// =========================================================================
//
// As of issue #1081 the `compile_via_daemon` body and the
// `is_compile_env_var` env filter were lifted into
// `crate::compile_dispatch` so multicall `zccache-soldr` dispatch can
// share them. The wrapper-entry dispatch site above now calls
// `compile_dispatch::compile_via_daemon` directly. Nothing remains in
// this file beyond the cargo-arg-shape predicates + stdin spill
// helper.

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn nested_driver_build_is_detected_with_version_from_registry_path() {
        let detected = nested_dylint_driver_build(&args(&[
            "--crate-name",
            "dylint_driver",
            "--edition=2021",
            "/home/u/.cargo/registry/src/index.crates.io-abc/dylint_driver-6.0.2/src/main.rs",
            "--emit=link",
        ]));
        assert_eq!(detected.as_deref(), Some("6.0.2"));

        // `--crate-name=<value>` spelling.
        let detected =
            nested_dylint_driver_build(&args(&["--crate-name=dylint_driver", "src/main.rs"]));
        assert_eq!(detected.as_deref(), Some("unknown"));
    }

    #[test]
    fn executing_the_driver_against_user_crates_is_not_a_driver_build() {
        // dylint RUNS the installed driver with the USER's crate names;
        // that must never trip the build guard (soldr#2484).
        assert_eq!(
            nested_dylint_driver_build(&args(&[
                "--crate-name",
                "my_workspace_crate",
                "src/lib.rs",
            ])),
            None
        );
        // A crate merely depending on a path containing the string is
        // not the driver crate either.
        assert_eq!(
            nested_dylint_driver_build(&args(&[
                "--crate-name",
                "other",
                "/x/dylint_driver-6.0.2/vendored.rs",
            ])),
            None
        );
    }

    #[test]
    fn nested_driver_diagnostic_names_version_preparation_and_opt_in() {
        let message = nested_dylint_driver_diagnostic("6.0.2");
        assert!(message.contains("dylint_driver v6.0.2"), "{message}");
        assert!(message.contains("soldr cargo dylint --all"), "{message}");
        assert!(
            message.contains("SOLDR_ALLOW_DYLINT_DRIVER_BUILD=1"),
            "{message}"
        );
        assert!(message.contains("DYLINT_DRIVER_PATH"), "{message}");
    }

    #[test]
    fn stdin_source_path_is_content_addressed() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let bytes = format!("fn main() {{ let _ = {nonce}; }}\n");
        // soldr#2006: an explicit root, not the process-wide memoized one --
        // otherwise another test's TempDir can own it and delete it under us.
        let root = tempfile::tempdir().expect("scratch root");
        let file = materialize_stdin_source_in(root.path(), bytes.as_bytes()).unwrap();
        let hash = zccache::hash::hash_bytes(bytes.as_bytes()).to_hex();
        let name = file.path().file_name().unwrap().to_string_lossy();

        assert_eq!(name.as_ref(), format!("soldr-stdin-{}.rs", &hash[..16]));
        assert_eq!(std::fs::read(file.path()).unwrap(), bytes.as_bytes());

        let same = materialize_stdin_source_in(root.path(), bytes.as_bytes()).unwrap();
        assert_eq!(same.path(), file.path());
    }

    #[test]
    fn stdin_source_paths_do_not_collide_for_distinct_content() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // soldr#2006: an explicit root, not the process-wide memoized one --
        // otherwise another test's TempDir can own it and delete it under us.
        let root = tempfile::tempdir().expect("scratch root");
        let a = materialize_stdin_source_in(
            root.path(),
            format!("const A: u128 = {nonce};\n").as_bytes(),
        )
        .unwrap();
        let b = materialize_stdin_source_in(
            root.path(),
            format!("const B: u128 = {};\n", nonce + 1).as_bytes(),
        )
        .unwrap();

        assert_ne!(a.path(), b.path());
    }

    // -------- is_non_cacheable_rustc (issue #980, L2) --------

    /// Build a wrapper-shaped argv: argv[0] is the soldr binary, argv[1]
    /// is the tool path, argv[2..] are the rustc args under test.
    fn wrapper_argv(rustc_args: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = vec!["soldr".into(), "rustc".into()];
        v.extend(rustc_args.iter().map(|s| (*s).to_string()));
        v
    }

    #[test]
    fn nested_workspace_wrapper_drops_real_compiler_path() {
        let workspace_tool = format!("{}-{}", "clippy", "driver");
        let argv = vec![
            "soldr".into(),
            format!("C:/toolchain/{workspace_tool}.exe"),
            "C:/toolchain/rustc.exe".into(),
            "--crate-name".into(),
            "demo".into(),
            "src/lib.rs".into(),
        ];

        let normalized = normalize_nested_workspace_wrapper_args(&argv, &workspace_tool);

        assert_eq!(
            normalized.as_ref(),
            &[
                "soldr",
                &format!("C:/toolchain/{workspace_tool}.exe"),
                "--crate-name",
                "demo",
                "src/lib.rs",
            ]
        );
    }

    #[test]
    fn wrapper_normalization_preserves_other_argument_shapes() {
        let workspace_tool = format!("{}-{}", "clippy", "driver");
        let direct_workspace_argv = vec![
            "soldr".into(),
            workspace_tool.clone(),
            "--crate-name".into(),
            "demo".into(),
            "src/lib.rs".into(),
        ];
        let rustc_argv = wrapper_argv(&["--crate-name", "demo", "src/lib.rs"]);
        let compiler_named_source_argv = vec![
            "soldr".into(),
            workspace_tool.clone(),
            format!("{}.rs", WRAPPER_PASSTHROUGH_TOOLS[0]),
            "--crate-name".into(),
            "demo".into(),
        ];

        assert!(matches!(
            normalize_nested_workspace_wrapper_args(&direct_workspace_argv, &workspace_tool),
            std::borrow::Cow::Borrowed(_)
        ));
        assert!(matches!(
            normalize_nested_workspace_wrapper_args(&rustc_argv, "rustc"),
            std::borrow::Cow::Borrowed(_)
        ));
        assert!(matches!(
            normalize_nested_workspace_wrapper_args(&compiler_named_source_argv, &workspace_tool),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn non_cacheable_print_mode_detected() {
        // `--print=cfg` is a metadata probe — never compiles, never
        // benefits from zccache.
        let argv = wrapper_argv(&["--print=cfg"]);
        assert!(is_non_cacheable_rustc(&argv));
    }

    #[test]
    fn non_cacheable_emit_dep_info_only_detected() {
        // Cargo's dep-graph refresh: `--emit=dep-info` produces no
        // object code, so wrapper round-trip is wasted.
        let argv = wrapper_argv(&["--emit=dep-info", "--crate-name=foo"]);
        assert!(is_non_cacheable_rustc(&argv));
    }

    #[test]
    fn cacheable_emit_link_metadata_not_flagged() {
        // A normal compile that emits link + metadata MUST still flow
        // through zccache.
        let argv = wrapper_argv(&["--emit=link,metadata"]);
        assert!(!is_non_cacheable_rustc(&argv));
    }

    #[test]
    fn cacheable_no_emit_no_print_not_flagged() {
        // No `--emit`, no `--print` → normal compile, must remain
        // cacheable.
        let argv = wrapper_argv(&["src/lib.rs"]);
        assert!(!is_non_cacheable_rustc(&argv));
    }

    #[test]
    fn clippy_driver_routes_through_embedded_zccache() {
        assert!(routes_through_embedded_zccache("rustc"));
        assert!(routes_through_embedded_zccache("clippy-driver"));
        assert!(!routes_through_embedded_zccache("rustfmt"));
        assert!(!routes_through_embedded_zccache("rustdoc"));
    }
}
