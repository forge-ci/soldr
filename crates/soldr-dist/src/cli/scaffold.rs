//! Deliberately narrow debug input discovery; the real manifest comes from zccache.
use crate::{cas::DigestMemo, compiler::Compiler, DistJob, Result};
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
};

pub fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}
pub async fn job(argv: &[String]) -> Result<DistJob> {
    let (program, args) = argv.split_first().ok_or("compiler argv required")?;
    let cwd = std::env::current_dir()?;
    let rust = Path::new(program).file_stem().is_some_and(|s| s == "rustc");
    let compiler = if rust {
        Compiler::rustc().await?
    } else {
        Compiler::cc(&find_program(program).unwrap_or_else(|| PathBuf::from(program)))?
    };
    // The scaffolding exposes object/metadata compilation only. Linking stays local.
    if rust
        && !args
            .iter()
            .any(|a| a == "--emit=obj" || a == "--emit=metadata")
    {
        return Err(
            "Dist debug rustc requires --emit=obj or --emit=metadata; linking stays local".into(),
        );
    }
    if !rust && !args.iter().any(|a| a == "-c") {
        return Err("Dist debug cc requires -c".into());
    }
    if args.iter().any(|a| a == "-") {
        return Err(
            "dist debug requires file inputs/outputs; the library API supports stdin".into(),
        );
    }
    if args.iter().any(|a| a.starts_with('@')) {
        return Err("Dist debug response files are unsupported".into());
    }
    let output = args
        .windows(2)
        .find(|a| a[0] == "-o")
        .map(|a| crate::sandbox::absolute(&cwd, Path::new(&a[1])))
        .ok_or("Dist debug requires explicit -o OUTPUT")??;
    let temp = tempfile::tempdir()?;
    let depfile = temp.path().join("inputs.d");
    let mut command = compiler.command();
    if let Some(toolchain) = &compiler.toolchain {
        command = command.env("RUSTUP_TOOLCHAIN", toolchain);
    }
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if arg == "-o" || arg == "--emit" {
            skip = true;
            continue;
        }
        if arg.starts_with("--emit=") {
            continue;
        }
        command = command.arg(arg);
    }
    if rust {
        command = command.arg(format!("--emit=dep-info={}", depfile.display()));
    } else {
        command = command.args(["-MM", "-MF"]).arg(&depfile);
    }
    let result = command.capture().await?;
    if !result.status.success() {
        return Err(format!(
            "Dist input scan failed: {}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        )
        .into());
    }
    let text = std::fs::read_to_string(depfile)?;
    let mut paths = dep_paths(&text, &cwd)?;
    extra_inputs(args, &cwd, &mut paths)?;
    let mut memo = DigestMemo::default();
    let inputs = paths
        .iter()
        .map(|path| memo.input(path))
        .collect::<Result<_>>()?;
    let env = dependency_environment(&text)?;
    Ok(DistJob {
        rewrite_env: vec![],
        compiler_kind: compiler.kind,
        compiler_identity: compiler.identity,
        argv: args.to_vec(),
        env,
        cwd: cwd.to_str().ok_or("non-UTF8 cwd")?.into(),
        stdin: vec![],
        inputs,
        outputs: vec![output.to_str().ok_or("non-UTF8 output")?.into()],
    })
}
fn absolute(cwd: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.into()
    } else {
        cwd.join(path)
    }
}
fn dep_paths(text: &str, cwd: &Path) -> Result<BTreeSet<PathBuf>> {
    let text = text.replace("\\\n", " ");
    let line = text
        .lines()
        .find(|line| !line.starts_with('#') && line.contains(": "))
        .ok_or("unrecognised compiler dep-info")?;
    let (_, dependencies) = line
        .split_once(": ")
        .ok_or("dep-info missing dependencies")?;
    let mut paths = BTreeSet::new();
    let mut word = String::new();
    let mut escaped = false;
    for c in dependencies.chars().chain(std::iter::once(' ')) {
        if escaped {
            word.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c.is_whitespace() {
            if !word.is_empty() {
                paths.insert(absolute(cwd, &word));
                word.clear();
            }
        } else {
            word.push(c);
        }
    }
    if paths.is_empty() {
        return Err("dep-info has no inputs".into());
    }
    Ok(paths)
}
fn extra_inputs(args: &[String], cwd: &Path, paths: &mut BTreeSet<PathBuf>) -> Result<()> {
    for (index, arg) in args.iter().enumerate() {
        if arg == "--extern" {
            if let Some((_, path)) = args.get(index + 1).and_then(|s| s.split_once('=')) {
                paths.insert(absolute(cwd, path));
            }
        } else if let Some(value) = arg.strip_prefix("--extern=") {
            if let Some((_, path)) = value.split_once('=') {
                paths.insert(absolute(cwd, path));
            }
        } else if let Some(value) = arg.strip_prefix("-L") {
            let value = if value.is_empty() {
                args.get(index + 1).ok_or("-L missing path")?.as_str()
            } else {
                value
            };
            let value = value.split_once('=').map_or(value, |(_, path)| path);
            for entry in std::fs::read_dir(absolute(cwd, value))? {
                let path = entry?.path();
                if path.is_file() {
                    paths.insert(path);
                }
            }
        }
    }
    Ok(())
}

fn dependency_environment(text: &str) -> Result<HashMap<String, String>> {
    for dependency in text
        .lines()
        .filter_map(|line| line.strip_prefix("# env-dep:"))
    {
        let key = dependency
            .split_once('=')
            .map_or(dependency, |(key, _)| key);
        if crate::compiler::RUNTIME_ENV.contains(&key) {
            return Err(
                format!("dist debug cannot preserve worker-runtime env dependency {key}").into(),
            );
        }
    }
    // dep-info names the env! inputs; avoid exporting unrelated host credentials.
    let keys: BTreeSet<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix("# env-dep:"))
        .map(|line| {
            line.split_once('=')
                .map_or(line, |(key, _)| key)
                .to_string()
        })
        .chain(
            [
                "CPATH",
                "C_INCLUDE_PATH",
                "CPLUS_INCLUDE_PATH",
                "LANG",
                "LC_ALL",
                "SOURCE_DATE_EPOCH",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .collect();
    Ok(keys
        .into_iter()
        .filter_map(|key| std::env::var(&key).ok().map(|value| (key, value)))
        .collect())
}
