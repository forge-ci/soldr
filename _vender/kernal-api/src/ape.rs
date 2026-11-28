//! Actually Portable Executable (APE / cosmocc) launch support.
//!
//! A Cosmopolitan APE binary starts with a shell prologue rather than a native
//! image header. A host with no `binfmt_misc` registration for it -- stock
//! NixOS, most containers, macOS -- refuses it with "Exec format error", and
//! only a shell knows to retry.
//!
//! This crate launches such an image through a loader instead
//! (`<loader> <image> <args...>`), chosen by [`plan_launch`]: an explicit
//! loader ([`LOADER_ENV`] or [`ApeOptions::loader`]), else the loader
//! embedded in the image (Linux; installed into a private, exec-capable cache
//! directory or a sealed memfd), else an installed `ape`, else the host
//! shell. The embedded loader needs no `PATH`, `dd`, `gzip` or `$TMPDIR` in
//! the child, so a cleared environment works.
//!
//! Every process this crate spawns gets this automatically:
//! [`SpawnSpec`](crate::SpawnSpec) spawns and sessions and
//! [`run_bounded_command`](crate::run_bounded_command) plan the loader before
//! the spawn, and
//! [`platform::process::spawn_sync`](crate::platform::process::spawn_sync)
//! retries a caller-built command once after a refusal. [`command`] gives a
//! caller that builds its own `std::process::Command` the same plan. On a
//! host that runs APE images natively (Windows) nothing is ever planned.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Environment variable naming an explicit APE loader (an `ape` binary or a
/// POSIX shell). Read from the child environment first, then this process's.
pub const LOADER_ENV: &str = "RUNNING_PROCESS_APE_LOADER";

/// Environment variable naming the preferred directory for loaders extracted
/// from APE images; used only when private to this user and exec-capable.
pub const CACHE_DIR_ENV: &str = "RUNNING_PROCESS_APE_CACHE_DIR";

/// Whether this host starts an APE image as a native executable, so no
/// loader is ever involved (Windows, where the image is a PE).
pub fn runs_natively() -> bool {
    !running_process::ape::NEEDS_LOADER
}

/// Whether `header` begins with one of the APE magics (`MZqFpD='`,
/// `jartsr='`, `APEDBG='`).
pub fn is_ape_header(header: &[u8]) -> bool {
    running_process::ape::is_ape_header(header)
}

/// Whether the file at `path` is an APE image. Unreadable and missing files
/// are not.
pub fn is_ape_file(path: &Path) -> bool {
    running_process::ape::is_ape_file(path)
}

/// What a launch plan consults, as the child will see it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ApeOptions {
    /// The child's `PATH`: locates a bare program name, `ape` and `sh`.
    pub path: Option<OsString>,
    /// An explicit loader, path or bare name; wins over every other choice.
    pub loader: Option<OsString>,
    /// Directories to install an extracted loader in, in order. The first
    /// private, exec-capable one wins; with none, a sealed memfd (Linux).
    pub cache_dirs: Vec<PathBuf>,
}

impl ApeOptions {
    /// The values a child inherits from this process: `PATH`,
    /// [`LOADER_ENV`], then [`CACHE_DIR_ENV`] ahead of the host default
    /// cache directories.
    pub fn inherited() -> Self {
        Self::from_backend(running_process::ape::ApeOptions::inherited())
    }

    /// Apply a child's environment edits over the inherited values; `None`
    /// removes a variable and `clear` starts from an empty environment.
    pub fn with_overrides<'a>(
        clear: bool,
        overrides: impl IntoIterator<Item = (&'a OsStr, Option<&'a OsStr>)>,
    ) -> Self {
        Self::from_backend(running_process::ape::ApeOptions::with_overrides(
            clear, overrides,
        ))
    }

    fn from_backend(options: running_process::ape::ApeOptions) -> Self {
        Self {
            path: options.path,
            loader: options.loader,
            cache_dirs: options.cache_dirs,
        }
    }

    fn to_backend(&self) -> running_process::ape::ApeOptions {
        running_process::ape::ApeOptions {
            path: self.path.clone(),
            loader: self.loader.clone(),
            cache_dirs: self.cache_dirs.clone(),
        }
    }
}

/// Which loader runs an APE image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ApeLoaderKind {
    /// The loader named by [`LOADER_ENV`] or [`ApeOptions::loader`].
    Explicit,
    /// The loader embedded in the image, extracted by this crate.
    Embedded,
    /// An `ape` loader installed on the host.
    Installed,
    /// A POSIX shell, running the image's own prologue.
    Shell,
}

/// How to run one APE image: `loader image args...`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApeLaunch {
    kind: ApeLoaderKind,
    loader: PathBuf,
    image: PathBuf,
    ape_path_dir: Option<PathBuf>,
}

impl ApeLaunch {
    /// Which loader this launch uses.
    pub fn kind(&self) -> ApeLoaderKind {
        self.kind
    }

    /// The program to execute in place of the image.
    pub fn loader(&self) -> &Path {
        &self.loader
    }

    /// Absolute path of the APE image.
    pub fn image(&self) -> &Path {
        &self.image
    }

    /// A private directory holding this launch's loader as `ape`. Put it first
    /// on the child's search path ([`Self::child_path`]) so APE programs the
    /// child spawns in turn (a cosmocc `gcc` running `cc1`) find a loader.
    /// `None` for a shell loader.
    pub fn ape_path_dir(&self) -> Option<&Path> {
        self.ape_path_dir.as_deref()
    }

    /// The child's search path with [`Self::ape_path_dir`] first, given the
    /// one it would otherwise get. `None` when there is nothing to add.
    pub fn child_path(&self, inherited: Option<&OsStr>) -> Option<OsString> {
        let dir = self.ape_path_dir.as_ref()?;
        let rest = inherited.filter(|path| !path.is_empty());
        std::env::join_paths(
            std::iter::once(dir.clone()).chain(rest.into_iter().flat_map(std::env::split_paths)),
        )
        .ok()
    }

    /// The loader's arguments: the image, then `args` unchanged.
    pub fn args<I, S>(&self, args: I) -> Vec<OsString>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        std::iter::once(self.image.clone().into_os_string())
            .chain(args.into_iter().map(|arg| arg.as_ref().to_os_string()))
            .collect()
    }

    /// A spawn description that runs the image with `args` through this
    /// launch's loader, with [`Self::ape_path_dir`] first on the inherited
    /// search path. Every other [`SpawnSpec`](crate::SpawnSpec) setting
    /// starts at its default.
    pub fn spawn_spec<I, S>(&self, args: I) -> crate::SpawnSpec
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let spec = crate::SpawnSpec::new(&self.loader).args(self.args(args));
        match self.child_path(ApeOptions::inherited().path.as_deref()) {
            Some(path) => spec.env("PATH", path),
            None => spec,
        }
    }
}

/// Plan how to run `program` if it resolves to an APE image.
///
/// `program` resolves as the child's `execvp` would: a path with a separator
/// is relative to `current_dir` when one is given; a bare name is searched in
/// `options.path`. Planning may install the image's embedded loader.
///
/// `None` means `program` is not an APE image, the host runs APE images
/// natively, or no loader is available; spawn the program as it is.
pub fn plan_launch(
    program: &OsStr,
    current_dir: Option<&Path>,
    options: &ApeOptions,
) -> Option<ApeLaunch> {
    let launch = running_process::ape::plan_launch(program, current_dir, &options.to_backend())?;
    let kind = match launch.kind {
        running_process::ape::LoaderKind::Explicit => ApeLoaderKind::Explicit,
        running_process::ape::LoaderKind::Embedded => ApeLoaderKind::Embedded,
        running_process::ape::LoaderKind::System => ApeLoaderKind::Installed,
        running_process::ape::LoaderKind::Shell => ApeLoaderKind::Shell,
    };
    Some(ApeLaunch {
        kind,
        loader: launch.loader,
        image: launch.image,
        ape_path_dir: launch.ape_path_dir,
    })
}

/// A `std::process::Command` for `program` that runs an APE image through
/// its planned loader, with this process's environment as the child's. Use in
/// place of `Command::new` for an external tool. A relative `program` path
/// resolves against this process's working directory.
///
/// Call it without holding a [`ForkGuard`]: planning may install a loader.
pub fn command(program: impl AsRef<OsStr>) -> std::process::Command {
    running_process::ape::command(program)
}

/// The shared half of the process-wide fork lock.
///
/// Hold it across a spawn this crate does not perform, so a loader being
/// written by another thread cannot leak into the forked child and make the
/// loader's own launch fail with "Text file busy".
///
/// Never hold it while planning a launch ([`plan_launch`], [`command`]):
/// planning may install a loader under the exclusive half of the same lock,
/// and the thread would deadlock waiting on itself. Build the command first,
/// then take the guard for the spawn alone.
pub struct ForkGuard {
    _guard: std::sync::RwLockReadGuard<'static, ()>,
}

/// Take the shared half of the process-wide fork lock; see [`ForkGuard`].
pub fn fork_guard() -> ForkGuard {
    ForkGuard {
        _guard: running_process::ape::fork_guard(),
    }
}

/// Spawn a caller-built command under the fork lock, retrying once through
/// the host's shell convention if the host refused it as an APE image.
#[allow(
    dead_code,
    reason = "only the Unix sync spawn layer builds commands a host can refuse as APE"
)]
pub(crate) fn spawn_std<T>(
    command: &mut std::process::Command,
    spawn: impl FnMut(&mut std::process::Command) -> std::io::Result<T>,
) -> std::io::Result<T> {
    running_process::ape::spawn_std(command, spawn)
}

#[cfg(test)]
mod tests {
    /// The facade spells the variables itself; they must stay the ones the
    /// substrate reads.
    #[test]
    fn environment_names_match_the_substrate() {
        assert_eq!(super::LOADER_ENV, running_process::ape::LOADER_ENV);
        assert_eq!(super::CACHE_DIR_ENV, running_process::ape::CACHE_DIR_ENV);
    }
}
