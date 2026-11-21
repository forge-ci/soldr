//! Compiler commands are worker-owned; a peer cannot select an executable path.
use crate::{
    proto::{Done, OutputFile},
    sandbox, DistJob, Result,
};
use running_process::AsyncProcessBuilder;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Compiler {
    pub kind: String,
    pub identity: String,
    pub program: PathBuf,
    pub prefix: Vec<String>,
    pub toolchain: Option<PathBuf>,
}
impl Compiler {
    /// Rust always enters the Soldr front door, which resolves the pinned compiler.
    pub async fn rustc() -> Result<Self> {
        let resolved = AsyncProcessBuilder::new("soldr")
            .args(["rustup", "which", "rustc"])
            .capture()
            .await?;
        if !resolved.status.success() {
            return Err(format!(
                "rustc resolution failed: {}",
                String::from_utf8_lossy(&resolved.stderr)
            )
            .into());
        }
        let path = PathBuf::from(String::from_utf8(resolved.stdout)?.trim());
        let toolchain = path
            .parent()
            .and_then(Path::parent)
            .ok_or("invalid resolved rustc path")?
            .to_path_buf();
        let output = AsyncProcessBuilder::new("soldr")
            .env("RUSTUP_TOOLCHAIN", &toolchain)
            .args(["rustc", "-vV"])
            .capture()
            .await?;
        if !output.status.success() {
            return Err(format!(
                "rustc identity failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        let text = String::from_utf8(output.stdout)?;
        let identity = text
            .lines()
            .filter(|s| s.starts_with("commit-hash:") || s.starts_with("host:"))
            .collect::<Vec<_>>()
            .join("\n");
        if identity.lines().count() != 2 {
            return Err("rustc -vV missing commit-hash/host".into());
        }
        Ok(Self {
            kind: "rustc".into(),
            identity,
            program: "soldr".into(),
            prefix: vec!["rustc".into()],
            toolchain: Some(toolchain),
        })
    }
    pub fn cc(path: &Path) -> Result<Self> {
        let program = std::fs::canonicalize(path)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(std::fs::File::open(&program)?)?;
        Ok(Self {
            kind: "cc".into(),
            identity: hasher.finalize().to_hex().to_string(),
            program,
            prefix: vec![],
            toolchain: None,
        })
    }
    /// Runtime/toolchain environment cannot be relocated as an arbitrary Rust string.
    pub fn environment_decline(&self, job: &DistJob) -> Option<String> {
        for (key, value) in &job.env {
            if key == "RUSTUP_TOOLCHAIN"
                && self
                    .toolchain
                    .as_ref()
                    .is_some_and(|path| path.to_str() != Some(value.as_str()))
            {
                return Some("worker runtime environment differs: RUSTUP_TOOLCHAIN".into());
            }
            if [
                "OUT_DIR",
                "CPATH",
                "C_INCLUDE_PATH",
                "CPLUS_INCLUDE_PATH",
                "LIBRARY_PATH",
            ]
            .contains(&key.as_str())
                && !value.is_empty()
            {
                return Some(format!(
                    "path-dependent environment {key} requires namespace isolation"
                ));
            }
            if [
                "HOME",
                "PATH",
                "RUSTUP_HOME",
                "CARGO_HOME",
                "RUSTUP_TOOLCHAIN",
            ]
            .contains(&key.as_str())
                && std::env::var(key).ok().as_ref() != Some(value)
            {
                return Some(format!("worker runtime environment differs: {key}"));
            }
        }
        None
    }
    pub async fn run(&self, job: &DistJob, root: &Path) -> Result<Done> {
        let mut command = AsyncProcessBuilder::new(&self.program)
            .args(&self.prefix)
            .clear_env(true)
            .kill_on_drop(true)
            .create_process_group(true);
        for (key, value) in &job.env {
            command = command.env(key, value);
        }
        // PATH and toolchain homes are worker configuration, never sandbox-rewritten.
        for key in ["PATH", "HOME", "RUSTUP_HOME", "CARGO_HOME"] {
            if let Some(value) = std::env::var_os(key) {
                command = command.env(key, value);
            }
        }
        if let Some(toolchain) = &self.toolchain {
            command = command.env("RUSTUP_TOOLCHAIN", toolchain);
        }
        let mirror_root = root.join("root");
        if self.kind == "rustc" {
            // Restore non-workspace absolute paths, before more-specific caller remaps.
            command = command.arg(format!("--remap-path-prefix={}=", mirror_root.display()));
        } else {
            command = command.arg(format!("-ffile-prefix-map={}=", mirror_root.display()));
        }
        for arg in &job.argv {
            command = command.arg(sandbox::rewrite(root, job, arg)?);
        }
        if self.kind == "rustc"
            && !job
                .argv
                .iter()
                .any(|arg| arg.starts_with("--remap-path-prefix"))
        {
            let cwd = sandbox::mirrored(root, &job.cwd)?;
            command = command.arg(format!("--remap-path-prefix={}=.", cwd.display()));
        }
        let mut child = command
            .current_dir(sandbox::mirrored(root, &job.cwd)?)
            .build();
        child.start().await?;
        if !job.stdin.is_empty() {
            child.write_stdin(&job.stdin).await?;
        }
        child.close_stdin().await?;
        let output = child.capture().await?;
        Ok(Done {
            exit_code: output.status.code().unwrap_or(1),
            stdout: output.stdout,
            stderr: output.stderr,
            outputs: vec![],
        })
    }
}
pub fn outputs(
    job: &DistJob,
    root: &Path,
    lease: &mut crate::cas::CasLease,
) -> Result<Vec<OutputFile>> {
    let mut outputs = vec![];
    for path in &job.outputs {
        let source = sandbox::mirrored(root, path)?;
        let (digest, size) = lease.ingest(&source)?;
        let mode = soldr_platform::fs::permissions::mode(&source).unwrap_or(0o644);
        outputs.push(OutputFile {
            abs_path: path.clone(),
            blake3: digest,
            size,
            mode,
        });
    }
    Ok(outputs)
}
