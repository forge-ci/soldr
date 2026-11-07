//! Bounded pruning with backlinks to every target directory Soldr has linked.
use crate::core::{SoldrError, SoldrPaths};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub(super) fn lock_store(root: &Path) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(root)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(".lock"))?;
    crate::platform::wait::lock_exclusive_for(&file, crate::platform::wait::SHORT_LOCK)?;
    Ok(file)
}

pub(super) fn register_link(target: &Path, store: &Path) -> std::io::Result<()> {
    let target = target.canonicalize()?;
    let links = store.join(".soldr-links");
    std::fs::create_dir_all(&links)?;
    let digest = Sha256::digest(target.to_string_lossy().as_bytes());
    let name: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let backlink = links.join(name);
    if !backlink.exists() {
        crate::platform::fs::links::create(&target.to_string_lossy(), &backlink, true)?;
    }
    std::fs::write(store.join(".last-used"), [])
}

fn in_use(store: &Path) -> std::io::Result<bool> {
    for entry in std::fs::read_dir(store.join(".soldr-links"))? {
        let target = entry?.path().join("dylint/libraries");
        match target.canonicalize() {
            Ok(current) if current == store => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

/// Retain the N newest keys plus every live key. Unknown legacy entries are
/// retained: without backlinks we cannot establish that they are unused.
pub(super) fn prune(root: &Path, keep: usize, dry_run: bool) -> std::io::Result<Vec<PathBuf>> {
    let _lock = lock_store(root)?;
    let mut keys = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let marker = entry.path().join(".last-used");
        if marker.is_file() {
            keys.push((marker.metadata()?.modified()?, entry.path().canonicalize()?));
        }
    }
    keys.sort_by(|a, b| b.cmp(a));
    let mut removed = Vec::new();
    for (_, key) in keys.into_iter().skip(keep) {
        if in_use(&key)? {
            continue;
        }
        if !dry_run {
            std::fs::remove_dir_all(&key)?;
        }
        removed.push(key);
    }
    Ok(removed)
}

#[derive(clap::Parser)]
#[command(
    name = "soldr dylint store",
    about = "Manage shared Dylint library builds"
)]
struct StoreCli {
    #[command(subcommand)]
    command: StoreCommand,
}
#[derive(clap::Subcommand)]
enum StoreCommand {
    /// Keep the newest keys and all keys linked by live target directories
    Gc {
        #[arg(long, default_value_t = 4)]
        keep: usize,
        #[arg(long)]
        dry_run: bool,
    },
}
pub(crate) fn run(args: &[String]) -> Result<i32, SoldrError> {
    use clap::Parser;
    let cli = match StoreCli::try_parse_from(
        std::iter::once("soldr dylint store".to_string()).chain(args.iter().cloned()),
    ) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            error
                .print()
                .map_err(|e| SoldrError::Other(e.to_string()))?;
            return Ok(code);
        }
    };
    let StoreCommand::Gc { keep, dry_run } = cli.command;
    let root = SoldrPaths::new()?.root.join("dylint/libraries");
    for key in prune(&root, keep, dry_run).map_err(|e| SoldrError::Other(e.to_string()))? {
        println!(
            "{} {}",
            if dry_run { "Would remove" } else { "Removed" },
            key.display()
        );
    }
    Ok(0)
}
