//! Link-only `-fuse-ld=lld` for Darwin cross C/C++ (soldr#3776).
//!
//! cmake-based `-sys` crates try-compile with a compile *and link* round
//! trip, so the C/C++ flags must select lld (the host's GNU ld cannot write
//! Mach-O). Passing `-fuse-ld=lld` directly in `CFLAGS_<target>` makes every
//! compile-only cc-rs invocation warn `argument unused during compilation`,
//! fatal under `-Werror`. Clang does not report options that come from a
//! configuration file as unused, and still applies them when it links, so the
//! flag lives in a soldr-managed `.cfg` passed with `--config=`.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

pub(crate) const LLD_CONFIG_BODY: &str = "-fuse-ld=lld\n";
const LLD_CONFIG_NAME: &str = "soldr-darwin-link-lld-v1.cfg";

/// Materialize the managed clang configuration file and return its path.
pub(crate) fn ensure_lld_config(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    let dir = paths.bin.join("clang-config");
    let file = dir.join(LLD_CONFIG_NAME);
    if std::fs::read_to_string(&file).is_ok_and(|body| body == LLD_CONFIG_BODY) {
        return Ok(file);
    }
    std::fs::create_dir_all(&dir)?;
    let staged = dir.join(format!(".{LLD_CONFIG_NAME}.{}.tmp", std::process::id()));
    std::fs::write(&staged, LLD_CONFIG_BODY)?;
    std::fs::rename(&staged, &file)?;
    Ok(file)
}

/// The C/C++ flag fragment (leading space) that selects lld link-only.
pub(crate) fn link_only_lld_flag(config: &Path) -> String {
    format!(" --config={}", config.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file_holds_only_the_linker_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let cfg = ensure_lld_config(&paths).expect("config written");
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), LLD_CONFIG_BODY);
        // Idempotent.
        assert_eq!(ensure_lld_config(&paths).unwrap(), cfg);
    }

    #[test]
    fn compile_flags_carry_the_config_not_the_raw_linker_flag() {
        let flag = link_only_lld_flag(Path::new("/s/soldr-darwin-link-lld-v1.cfg"));
        assert_eq!(flag, " --config=/s/soldr-darwin-link-lld-v1.cfg");
        assert!(!flag.contains("-fuse-ld"));
    }

    /// With a real clang: a compile-only `-Werror` run is clean, and a link
    /// still selects lld. Runs against `SOLDR_LLVM_DIR`'s clang; skipped without it.
    #[test]
    fn real_clang_is_warning_free_on_compile_and_uses_lld_on_link() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(tmp.path().join("soldr"));
        let cfg = ensure_lld_config(&paths).expect("config written");
        let src = tmp.path().join("f.c");
        std::fs::write(&src, "int f(void) { return 0; }\n").unwrap();
        let config = format!("--config={}", cfg.display());
        // Only the managed (unwrapped) clang is meaningful here: distro and
        // nix wrappers inject flags of their own.
        let Some(dir) = std::env::var_os("SOLDR_LLVM_DIR") else {
            return;
        };
        let clang = std::path::Path::new(&dir).join("clang");
        let Ok(compile) = std::process::Command::new(&clang)
            .args([
                config.as_str(),
                "--target=arm64-apple-darwin",
                "-Werror",
                "-c",
            ])
            .arg(&src)
            .arg("-o")
            .arg(tmp.path().join("f.o"))
            .output()
        else {
            return;
        };
        let stderr = String::from_utf8_lossy(&compile.stderr);
        assert!(compile.status.success(), "compile failed: {stderr}");
        assert!(!stderr.contains("unused"), "{stderr}");
        let link = std::process::Command::new(&clang)
            .args([config.as_str(), "--target=x86_64-unknown-linux-gnu", "-###"])
            .arg(tmp.path().join("f.o"))
            .output()
            .expect("clang -###");
        let plan = String::from_utf8_lossy(&link.stderr);
        assert!(plan.contains("ld.lld"), "link must select lld: {plan}");
    }
}
