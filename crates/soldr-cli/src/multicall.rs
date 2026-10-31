//! Busybox-style argv[0] dispatch for shim identities.
//!
//! The release archive no longer ships `soldr-shim`,
//! `soldr-clang-shim`, or `zccache-soldr` sidecar binaries. Instead,
//! shim installers hardlink/copy the main `soldr` binary under each
//! shim name and this module selects the tiny fast path before
//! clap/tokio startup.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const TOOLCHAIN_TOOLS: &[&str] = &[
    "cargo",
    "rustc",
    "rustfmt",
    "clippy-driver",
    "rustdoc",
    "rustup",
];
const ZCCACHE_SOLDR: &str = "zccache-soldr";
const SOLDR_DAEMON: &str = "soldr-daemon";
const SOLDR_DYLINT: &str = "soldr-dylint";

#[derive(Debug, PartialEq)]
pub(crate) enum MulticallDispatch {
    SoldrArgs(Vec<String>),
    Exit(i32),
    ExitCode(ExitCode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShimIdentity {
    Toolchain(&'static str),
    Clang(ClangTool),
    ZccacheSoldr,
    SoldrDaemon,
    SoldrDylint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClangTool {
    Clang,
    ClangPP,
}

impl ClangTool {
    fn msvc_downstream(self) -> &'static str {
        "clang-cl"
    }

    fn passthrough_downstream(self) -> &'static str {
        match self {
            ClangTool::Clang => "clang",
            ClangTool::ClangPP => "clang++",
        }
    }
}

/// Out-of-band carrier for a shim's argv[0] identity (soldr#1934).
///
/// Set by the `sh` trampoline that wheel-repaired installs use in place of a
/// hardlinked shim (see [`crate::shim_dir::trampoline_shim_body`]). A
/// trampoline cannot set argv[0] portably — `exec -a` is a bashism and
/// `/bin/sh` is `dash` on most Linux distributions — so it hands the shim path
/// over in the environment instead.
pub(crate) const SHIM_ARGV0_ENV: &str = "SOLDR_ARGV0_SHIM";

/// Rewrite argv[0] from [`SHIM_ARGV0_ENV`] so a trampoline invocation is
/// indistinguishable from a hardlinked one.
///
/// This must run before *any* argv inspection. The whole point is that
/// `toolchain_shim_should_defer_to_rustc_wrapper`, `classify_argv0`, the
/// `RUSTC_WRAPPER` positional contract, and `strip_self_from_path` all keep
/// reading the same argv they were written against — a shape-specific branch
/// in each of them is what 0.8.26 got wrong, one caller at a time.
///
/// The variable is removed on the way through: it describes *this* process's
/// invocation, and a child that inherited it would misidentify itself.
pub(crate) fn apply_shim_argv0_override(raw_args: Vec<String>) -> Vec<String> {
    let Some(shim_path) = env::var_os(SHIM_ARGV0_ENV) else {
        return raw_args;
    };
    env::remove_var(SHIM_ARGV0_ENV);
    let Some(shim_path) = shim_path.to_str().filter(|value| !value.is_empty()) else {
        return raw_args;
    };
    // Only argv[0] moves. `"$@"` reached us untouched, so every remaining
    // index already matches the hardlink shape.
    let mut rebuilt = Vec::with_capacity(raw_args.len().max(1));
    rebuilt.push(shim_path.to_string());
    rebuilt.extend(raw_args.into_iter().skip(1));
    rebuilt
}

/// Whether argv0 selects a multicall shim identity (rustc/cargo/clang/...)
/// rather than the plain `soldr` front door. Shim entries are by definition
/// sanctioned re-entrancy (soldr#2547).
pub(crate) fn is_shim_identity(argv0: &str) -> bool {
    classify_argv0(argv0).is_some()
}

pub(crate) fn maybe_dispatch(raw_args: &[String]) -> Option<MulticallDispatch> {
    let argv0 = raw_args.first().map(String::as_str).unwrap_or("");
    match classify_argv0(argv0)? {
        ShimIdentity::Toolchain(tool) => {
            strip_self_from_path(argv0);
            let mut args = Vec::with_capacity(raw_args.len());
            args.push(tool.to_string());
            args.extend(raw_args.iter().skip(1).cloned());
            Some(MulticallDispatch::SoldrArgs(args))
        }
        ShimIdentity::Clang(tool) => Some(MulticallDispatch::Exit(run_clang_shim(tool))),
        ShimIdentity::ZccacheSoldr => Some(MulticallDispatch::Exit(run_zccache_soldr())),
        ShimIdentity::SoldrDaemon => Some(MulticallDispatch::Exit(crate::daemon_entry::run())),
        ShimIdentity::SoldrDylint => Some(MulticallDispatch::Exit(run_soldr_dylint(raw_args))),
    }
}

pub(crate) fn toolchain_shim_should_defer_to_rustc_wrapper(raw_args: &[String]) -> bool {
    let Some(rustc_path) = raw_args.get(1) else {
        return false;
    };
    if !is_rustc_wrapper_passthrough_tool(rustc_path) {
        return false;
    }
    let argv0 = raw_args.first().map(String::as_str).unwrap_or("");
    matches!(
        classify_argv0(argv0),
        Some(ShimIdentity::Toolchain("rustc")) | Some(ShimIdentity::Toolchain("clippy-driver"))
    )
}

fn is_rustc_wrapper_passthrough_tool(arg: &str) -> bool {
    let stem = Path::new(arg)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(arg);
    matches!(stem, "rustc" | "clippy-driver")
}

fn classify_argv0(argv0: &str) -> Option<ShimIdentity> {
    let stem = argv0_stem(argv0)?;
    for &tool in TOOLCHAIN_TOOLS {
        if stem == tool {
            return Some(ShimIdentity::Toolchain(tool));
        }
    }
    match stem.as_str() {
        "clang" => Some(ShimIdentity::Clang(ClangTool::Clang)),
        "clang++" => Some(ShimIdentity::Clang(ClangTool::ClangPP)),
        ZCCACHE_SOLDR => Some(ShimIdentity::ZccacheSoldr),
        SOLDR_DAEMON => Some(ShimIdentity::SoldrDaemon),
        SOLDR_DYLINT => Some(ShimIdentity::SoldrDylint),
        _ => None,
    }
}

fn run_soldr_dylint(raw_args: &[String]) -> i32 {
    match crate::wrapper::run_rustc_wrapper(raw_args, crate::startup_profile::WrapperProfile::new())
    {
        Ok(code) => {
            let code = normalize_exit_code(code);
            if code == 0 {
                // soldr#3989: cargo-dylint reads the suffixed copy from the
                // profile dir, not the build-dir out-dir it lands in.
                crate::dylint_library_uplift::uplift_suffixed_library(raw_args);
            }
            code
        }
        Err(err) => {
            eprintln!("soldr-dylint: wrapper dispatch failed: {err}");
            // This line IS the explanation for the non-zero exit; without
            // the mark, the soldr#2024 exit annotation would follow it
            // claiming "soldr emitted no diagnostic" — actively misleading
            // right under a printed diagnostic (soldr#2634 finding 3).
            crate::exit_guard::mark_spoke();
            101
        }
    }
}

fn argv0_stem(argv0: &str) -> Option<String> {
    let file_name = Path::new(argv0).file_name()?.to_string_lossy();
    let file_name = file_name.as_ref();
    let suffix_start = file_name.len().saturating_sub(4);
    let stem = if file_name
        .get(suffix_start..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".exe"))
    {
        file_name.get(..suffix_start).unwrap_or(file_name)
    } else {
        file_name
    };
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

fn strip_self_from_path(argv0: &str) {
    let Some(shim_dir) = current_shim_dir(argv0) else {
        return;
    };
    let Some(existing) = env::var_os("PATH") else {
        return;
    };
    let separator =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            ';'
        } else {
            ':'
        };
    let existing = existing.to_string_lossy();
    let new_path = filter_path(&existing, &shim_dir, separator);
    env::set_var("PATH", new_path);
}

/// Which directory to drop from `PATH` — the one the *shim* lives in.
///
/// argv[0] leads (soldr#1934). For a hardlinked shim both sources agree, since
/// the shim is a copy of the soldr binary and `current_exe()` is the shim
/// itself. For a trampoline they do not: `current_exe()` is the real soldr
/// binary, off in the install tree, and scrubbing *that* directory would leave
/// the shim dir on `PATH` while removing something that belonged there.
fn current_shim_dir(argv0: &str) -> Option<PathBuf> {
    Path::new(argv0)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(canonicalize_or_self)
        .or_else(|| {
            env::current_exe()
                .ok()
                .as_deref()
                .and_then(Path::parent)
                .map(canonicalize_or_self)
        })
}

fn canonicalize_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn filter_path(existing: &str, shim_dir: &Path, separator: char) -> String {
    let shim_str = shim_dir.to_string_lossy();
    let mut out = String::with_capacity(existing.len());
    let mut first = true;
    for entry in existing.split(separator) {
        if entry.is_empty() {
            continue;
        }
        let matches = if crate::platform::host::facts::os()
            == crate::platform::host::facts::HostOs::Windows
        {
            entry.eq_ignore_ascii_case(&shim_str)
        } else {
            entry == shim_str
        };
        if matches {
            continue;
        }
        if !first {
            out.push(separator);
        }
        out.push_str(entry);
        first = false;
    }
    out
}

fn run_zccache_soldr() -> i32 {
    let rustc_argv: Vec<String> = env::args().skip(1).collect();
    if rustc_argv.is_empty() {
        eprintln!(
            "zccache-soldr: missing compiler-path argument (wrapper contract is \
             `[wrapper_path, compiler_path, ...compiler_args]`)"
        );
        crate::exit_guard::mark_spoke();
        return 2;
    }

    match crate::compile_dispatch::dispatch_compile(
        &rustc_argv,
        std::io::stdout(),
        std::io::stderr(),
    ) {
        Ok(code) => normalize_exit_code(code),
        Err(failure) => {
            eprintln!("zccache-soldr: dispatch failed: {failure}");
            crate::exit_guard::mark_spoke();
            101
        }
    }
}

fn run_clang_shim(tool: ClangTool) -> i32 {
    let argv: Vec<OsString> = env::args_os().collect();
    let args = argv.get(1..).unwrap_or(&[]);
    let downstream = if argv_has_msvc_target(args) {
        tool.msvc_downstream()
    } else {
        tool.passthrough_downstream()
    };

    match locate_in_path(downstream, true) {
        Some(path) => exec_with_argv(&path, args),
        None => {
            if downstream == "clang-cl" {
                eprintln!(
                    "clang shim: detected --target=*-pc-windows-msvc \
                     in argv but `clang-cl` is not on PATH"
                );
            } else {
                eprintln!(
                    "clang shim: passthrough mode but `{downstream}` \
                     is not on PATH after stripping the shim's own directory"
                );
            }
            crate::exit_guard::mark_spoke();
            127
        }
    }
}

fn argv_has_msvc_target(args: &[OsString]) -> bool {
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let arg = arg.to_string_lossy();
        if let Some(triple) = arg.strip_prefix("--target=") {
            if triple.ends_with("-pc-windows-msvc") {
                return true;
            }
        }
        if arg == "--target" {
            if let Some(next) = iter.peek() {
                if next.to_string_lossy().ends_with("-pc-windows-msvc") {
                    return true;
                }
            }
        }
    }
    false
}

fn locate_in_path(binary: &str, strip_self: bool) -> Option<PathBuf> {
    let original_path = env::var_os("PATH").unwrap_or_default();
    let strip_dir = if strip_self {
        env::current_exe()
            .ok()
            .as_deref()
            .and_then(Path::parent)
            .map(canonicalize_or_self)
    } else {
        None
    };

    for dir in env::split_paths(&original_path) {
        if let Some(strip) = strip_dir.as_ref() {
            if canonicalize_or_self(&dir) == *strip {
                continue;
            }
        }
        let candidate = dir.join(exe_filename(binary));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn exe_filename(binary: &str) -> String {
    crate::platform::executable::name::native(binary)
}

/// Run `binary` as the current process: Unix execs (replacing this image),
/// Windows spawns and waits. Only the failure path returns here on Unix.
fn exec_with_argv(binary: &Path, args: &[OsString]) -> i32 {
    let mut command = std::process::Command::new(binary);
    command.args(args);
    match crate::platform::process::spawn::exec_or_status(&mut command) {
        Ok(status) => status.code().map(normalize_exit_code).unwrap_or(1),
        Err(err) => {
            eprintln!("clang shim: exec({binary:?}) failed: {err}");
            crate::exit_guard::mark_spoke();
            126
        }
    }
}

fn normalize_exit_code(code: i32) -> i32 {
    if (0..=255).contains(&code) {
        code
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RAII snapshot/restore of `PATH`, held for the duration of a test that
    /// calls into code which mutates it (#1663).
    ///
    /// Restoring in a `Drop` rather than inline is the whole point: the
    /// previous hand-rolled restore in
    /// `cargo_rustc_multicall_preserves_every_argument` ran only if
    /// `maybe_dispatch` returned normally, so a panic there permanently
    /// rewrote `PATH` for every subsequent test in the binary. Holding
    /// `TEST_PROCESS_ENV_LOCK` additionally stops a parallel test in this
    /// binary from observing the mutated value.
    struct PathEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        original: Option<std::ffi::OsString>,
    }

    impl PathEnvGuard {
        fn capture() -> Self {
            // A poisoned lock means some other test panicked while holding it;
            // the env is still ours to restore, so recover rather than cascade.
            let lock = crate::TEST_PROCESS_ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Self {
                _lock: lock,
                original: env::var_os("PATH"),
            }
        }
    }

    impl Drop for PathEnvGuard {
        fn drop(&mut self) {
            match self.original.take() {
                Some(path) => env::set_var("PATH", path),
                None => env::remove_var("PATH"),
            }
        }
    }

    fn os_args(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    /// RAII guard for [`SHIM_ARGV0_ENV`], sharing the process-wide env lock so
    /// a parallel test in this binary cannot observe the mutation.
    struct ShimEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl ShimEnvGuard {
        fn set(value: &str) -> Self {
            let lock = crate::TEST_PROCESS_ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            env::set_var(SHIM_ARGV0_ENV, value);
            Self { _lock: lock }
        }
    }

    impl Drop for ShimEnvGuard {
        fn drop(&mut self) {
            env::remove_var(SHIM_ARGV0_ENV);
        }
    }

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    // soldr#1934. This is the regression the 0.8.26 trampoline caused, and it
    // is asserted on the argv the wrapper *receives* -- not on "the shim ran",
    // which was true throughout the outage.
    #[test]
    fn a_trampoline_rustc_wrapper_invocation_matches_the_hardlink_shape() {
        // What cargo hands a hardlinked shim: the tool identity is argv[0] and
        // argv[1] is the real compiler.
        let hardlink = args(&[
            "/wheel/shims/rustc",
            "/toolchains/1.94.1/bin/rustc",
            "-",
            "--crate-name",
            "___",
        ]);
        // What the trampoline forwards: identical, minus argv[0], which the
        // script could not set and passed in the environment instead.
        let trampoline = args(&[
            "/wheel/lib/soldr",
            "/toolchains/1.94.1/bin/rustc",
            "-",
            "--crate-name",
            "___",
        ]);

        let _guard = ShimEnvGuard::set("/wheel/shims/rustc");
        let restored = apply_shim_argv0_override(trampoline);

        assert_eq!(
            restored, hardlink,
            "the trampoline shape must be byte-identical to the hardlink shape"
        );
        // The consequence that actually broke builds: without the override the
        // deferral gate never fires, `wrapper.rs` reads a tool name out of
        // argv[1] instead of the compiler path, and the real rustc path stays
        // in the compile args as a second source file.
        assert!(toolchain_shim_should_defer_to_rustc_wrapper(&restored));
        assert_eq!(restored[1], "/toolchains/1.94.1/bin/rustc");

        // And the variable does not survive into any child process.
        assert!(env::var_os(SHIM_ARGV0_ENV).is_none());
    }

    #[test]
    fn shim_argv0_override_is_inert_without_the_env_var() {
        let _lock = crate::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        env::remove_var(SHIM_ARGV0_ENV);
        // A hardlinked shim -- the overwhelmingly common case -- must be
        // untouched, including argv[0].
        let hardlink = args(&["/wheel/shims/cargo", "build", "--release"]);
        assert_eq!(apply_shim_argv0_override(hardlink.clone()), hardlink);
    }

    #[test]
    fn an_empty_shim_override_is_ignored_rather_than_trusted() {
        // A truncated or hand-edited trampoline must not be able to blank
        // argv[0] and send dispatch somewhere unintended.
        let original = args(&["/wheel/lib/soldr", "--version"]);
        let _guard = ShimEnvGuard::set("");
        assert_eq!(apply_shim_argv0_override(original.clone()), original);
    }

    #[test]
    fn the_scrubbed_path_entry_is_the_shim_dir_not_the_soldr_dir() {
        // A trampoline's `current_exe()` is the real soldr binary, which lives
        // somewhere else entirely; argv[0] is the only source that names the
        // directory actually on PATH (soldr#1934).
        let guard = PathEnvGuard::capture();
        let shim_dir = if crate::platform::host::facts::os()
            == crate::platform::host::facts::HostOs::Windows
        {
            "C:\\wheel\\shims"
        } else {
            "/wheel/shims"
        };
        let other = if crate::platform::host::facts::os()
            == crate::platform::host::facts::HostOs::Windows
        {
            "C:\\usr\\bin"
        } else {
            "/usr/bin"
        };
        let separator = if crate::platform::host::facts::os()
            == crate::platform::host::facts::HostOs::Windows
        {
            ';'
        } else {
            ':'
        };
        env::set_var("PATH", format!("{shim_dir}{separator}{other}"));

        strip_self_from_path(&format!("{shim_dir}{}rustc", std::path::MAIN_SEPARATOR));

        let path = env::var("PATH").unwrap();
        assert!(
            !path.split(separator).any(|entry| entry == shim_dir),
            "{path}"
        );
        assert!(path.split(separator).any(|entry| entry == other), "{path}");
        drop(guard);
    }

    #[test]
    fn classify_argv0_recognizes_toolchain_shims() {
        assert_eq!(
            classify_argv0("/tmp/shims/cargo"),
            Some(ShimIdentity::Toolchain("cargo"))
        );
        assert_eq!(
            classify_argv0("clippy-driver.exe"),
            Some(ShimIdentity::Toolchain("clippy-driver"))
        );
        assert_eq!(classify_argv0("soldr"), None);
    }

    #[test]
    fn classify_argv0_recognizes_clang_and_zccache_shims() {
        assert_eq!(
            classify_argv0("/tmp/clang-shim/clang"),
            Some(ShimIdentity::Clang(ClangTool::Clang))
        );
        assert_eq!(
            classify_argv0("clang++"),
            Some(ShimIdentity::Clang(ClangTool::ClangPP))
        );
        assert_eq!(
            classify_argv0("zccache-soldr"),
            Some(ShimIdentity::ZccacheSoldr)
        );
    }

    #[test]
    fn classify_argv0_recognizes_runtime_aliases() {
        assert_eq!(
            classify_argv0("/opt/soldr/soldr-daemon"),
            Some(ShimIdentity::SoldrDaemon)
        );
        assert_eq!(
            classify_argv0("soldr-daemon.exe"),
            Some(ShimIdentity::SoldrDaemon)
        );
        assert_eq!(
            classify_argv0("/opt/soldr/shims/soldr-dylint"),
            Some(ShimIdentity::SoldrDylint)
        );
        assert_eq!(
            classify_argv0("soldr-dylint.exe"),
            Some(ShimIdentity::SoldrDylint)
        );
        assert_eq!(classify_argv0("zccache"), None);
        assert_eq!(classify_argv0("zccache.exe"), None);
    }

    #[test]
    fn filter_path_drops_self_dir() {
        let shim = PathBuf::from("/opt/.soldr/v0.8/shims");
        let path = "/usr/bin:/opt/.soldr/v0.8/shims:/usr/local/bin";
        assert_eq!(filter_path(path, &shim, ':'), "/usr/bin:/usr/local/bin");
    }

    #[test]
    fn argv_has_msvc_target_detects_single_and_split_forms() {
        assert!(argv_has_msvc_target(&os_args(&[
            "-O2",
            "--target=aarch64-pc-windows-msvc",
        ])));
        assert!(argv_has_msvc_target(&os_args(&[
            "--target",
            "x86_64-pc-windows-msvc",
        ])));
        assert!(!argv_has_msvc_target(&os_args(&[
            "--target=aarch64-unknown-linux-gnu",
        ])));
    }

    #[test]
    fn normalize_exit_code_collapses_out_of_range_values() {
        assert_eq!(normalize_exit_code(0), 0);
        assert_eq!(normalize_exit_code(255), 255);
        assert_eq!(normalize_exit_code(256), 1);
        assert_eq!(normalize_exit_code(-1), 1);
    }

    #[test]
    fn only_compiler_shims_defer_rustc_wrapper_contract() {
        for argv in [
            ["rustc", "/opt/rust/bin/rustc", "-vV"],
            ["clippy-driver.exe", "/opt/rust/bin/rustc", "-vV"],
        ] {
            assert!(
                toolchain_shim_should_defer_to_rustc_wrapper(&argv.map(str::to_string)),
                "compiler shim should preserve the wrapper contract: {argv:?}"
            );
        }

        for argv in [
            ["cargo", "rustc", "--profile"],
            ["cargo.exe", "clippy-driver.exe", "-vV"],
            ["rustfmt", "/opt/rust/bin/rustc", "-vV"],
            ["rustdoc", "/opt/rust/bin/rustc", "-vV"],
            ["clang", "/opt/rust/bin/rustc", "-vV"],
            ["zccache-soldr", "/opt/rust/bin/rustc", "-vV"],
        ] {
            assert!(
                !toolchain_shim_should_defer_to_rustc_wrapper(&argv.map(str::to_string)),
                "non-compiler shim must use normal multicall dispatch: {argv:?}"
            );
        }
    }

    #[test]
    fn cargo_rustc_multicall_preserves_every_argument() {
        let raw_args = [
            "/tmp/soldr-shims/cargo",
            "rustc",
            "--profile",
            "release",
            "--message-format",
            "json-render-diagnostics",
        ]
        .map(str::to_string);
        // RAII, not a manual restore (#1663). `maybe_dispatch` mutates PATH,
        // and the previous hand-rolled snapshot/restore only ran on the success
        // path — a panic inside `maybe_dispatch` left this process's PATH
        // rewritten for every later test in the binary. The guard also holds
        // `TEST_PROCESS_ENV_LOCK`, so a concurrently-running test cannot
        // observe the mutated PATH mid-flight.
        let _path_guard = PathEnvGuard::capture();

        let dispatch = maybe_dispatch(&raw_args);

        assert_eq!(
            dispatch,
            Some(MulticallDispatch::SoldrArgs(
                [
                    "cargo",
                    "rustc",
                    "--profile",
                    "release",
                    "--message-format",
                    "json-render-diagnostics",
                ]
                .map(str::to_string)
                .into()
            ))
        );
    }
}
