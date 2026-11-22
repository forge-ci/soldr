//! Compiler commands are worker-owned; a peer cannot select an executable path.
use crate::{
    proto::{Done, OutputFile},
    sandbox, DistJob, Result,
};
use running_process::AsyncProcessBuilder;
use std::path::{Path, PathBuf};

/// Worker-owned process context, including the three nextest isolation roots.
pub(crate) const RUNTIME_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "RUSTUP_HOME",
    "CARGO_HOME",
    "RUSTUP_TOOLCHAIN",
    "XDG_RUNTIME_DIR",
    "TMPDIR",
    "TMP",
    "TEMP",
    "SOLDR_CACHE_DIR",
    "RUSTUP_AUTO_INSTALL",
];

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
        let resolved = soldr_command()
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
        let output = soldr_command()
            .env("RUSTUP_TOOLCHAIN", &toolchain)
            .args(["--no-cache", "rustc", "-vV"])
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
            // Remote joblets execute as owned children after the client's cache miss.
            // Disable nested cache/executor routing; worker slots and host-load
            // admission govern these direct children without a global Cargo cap.
            prefix: vec!["--no-cache".into(), "rustc".into()],
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
        if self.kind == "rustc"
            && job.inputs.iter().any(|input| {
                Path::new(&input.abs_path).extension().is_some_and(|ext| {
                    ["so", "dylib", "dll"]
                        .iter()
                        .any(|candidate| ext == *candidate)
                })
            })
        {
            let host = self
                .identity
                .lines()
                .find_map(|line| line.strip_prefix("host: "));
            for (index, arg) in job.argv.iter().enumerate() {
                let target = if arg == "--target" {
                    job.argv.get(index + 1).map(String::as_str)
                } else {
                    arg.strip_prefix("--target=")
                };
                if target.is_some() && target != host {
                    return Some("cross-triple proc macro input".into());
                }
            }
        }
        if job
            .rewrite_env
            .iter()
            .any(|key| !sandbox::path_environment(key))
        {
            return Some("invalid Rust path environment rewrite".into());
        }
        for (key, value) in &job.env {
            if self.kind == "rustc" {
                continue;
            }
            if self.kind == "rustc" && sandbox::path_environment(key) {
                continue;
            }
            if Path::new(value).is_absolute()
                && sandbox::absolute(Path::new(&job.cwd), Path::new(value)).map_or(true, |path| {
                    job.inputs
                        .iter()
                        .any(|input| Path::new(&input.abs_path).starts_with(&path))
                })
            {
                return Some(format!(
                    "input-path environment {key} requires namespace isolation"
                ));
            }
            if self.kind == "rustc" && RUNTIME_ENV.contains(&key.as_str()) {
                continue; // Worker configuration is applied separately; proof verifies observed reads.
            }
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
            if RUNTIME_ENV.contains(&key.as_str())
                && std::env::var(key).ok().as_ref() != Some(value)
            {
                return Some(format!("worker runtime environment differs: {key}"));
            }
        }
        None
    }
    /// The sanctioned one-hop edge is consumed by Soldr before it launches
    /// rustc, so strict re-entry remains enforced for compiler descendants.
    pub fn command(&self) -> AsyncProcessBuilder {
        let mut command = AsyncProcessBuilder::new(&self.program).args(&self.prefix);
        if self.kind == "rustc" {
            command = command.env(soldr_core::self_relocate::SELF_SPAWN_EDGE_ENV_VAR, "1");
        }
        if let Some(toolchain) = &self.toolchain {
            command = command.env("RUSTUP_TOOLCHAIN", toolchain);
        }
        command
    }
    pub async fn run(&self, job: &DistJob, root: &Path) -> Result<Done> {
        let mut command = self
            .command()
            .clear_env(true)
            .kill_on_drop(true)
            .create_process_group(true);
        for (key, value) in &job.env {
            if RUNTIME_ENV.contains(&key.as_str())
                || key.starts_with("SOLDR_")
                || key.starts_with("ZCCACHE_")
                || key.starts_with("RUNNING_PROCESS_")
                || [
                    "CARGO_MAKEFLAGS",
                    "MAKEFLAGS",
                    "MFLAGS",
                    "RUSTC_WRAPPER",
                    "RUSTC_WORKSPACE_WRAPPER",
                ]
                .contains(&key.as_str())
            {
                continue;
            }
            let value = if self.kind == "rustc" && job.rewrite_env.contains(key) {
                if Path::new(value).is_absolute() {
                    sandbox::mirrored(root, value)?
                        .to_str()
                        .ok_or("non-UTF8 Rust environment path")?
                        .to_owned()
                } else {
                    value.clone()
                }
            } else {
                value.clone()
            };
            command = command.env(key, value);
        }
        // PATH and toolchain homes are worker configuration, never sandbox-rewritten.
        for &key in RUNTIME_ENV {
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
        let (args, proof) = crate::rust_output::proof_args(job, root)?;
        for arg in args {
            command = command.arg(arg);
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
            if let Err(error) = child.write_stdin(&job.stdin).await {
                eprintln!("dist compiler stdin: {error}");
            }
        }
        if let Err(error) = child.close_stdin().await {
            eprintln!("dist compiler stdin close: {error}");
        }
        let output = child.capture().await?;
        Ok(Done {
            exit_code: output.status.code().unwrap_or(1),
            stdout: output.stdout,
            stderr: output.stderr,
            outputs: vec![],
            dependency_info: crate::rust_output::proof(job, root, proof.as_deref())
                .map_err(|error| crate::dep_info::InvalidProof(error.to_string()))?,
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

fn soldr_command() -> AsyncProcessBuilder {
    AsyncProcessBuilder::new("soldr").env(soldr_core::self_relocate::SELF_SPAWN_EDGE_ENV_VAR, "1")
}
