//! Workspace capability declarations and tree-bound lint evidence.
use crate::core::{SoldrError, SoldrPaths};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(Debug, Deserialize)]
struct Declaration {
    schema: u32,
    checks: Vec<Check>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Deserialize)]
struct Check {
    id: String,
    step: String,
    scope: Option<String>,
    targets: Option<String>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct ClientCheck {
    id: String,
    hosts: Vec<String>,
}

#[derive(Debug)]
struct Work {
    check: ClientCheck,
    args: Vec<String>,
}

#[derive(Serialize)]
struct ClientWork {
    v: u32,
    kind: &'static str,
    checks: Vec<ClientCheck>,
    digest: String,
}

#[derive(Debug, Serialize)]
struct ResultRow {
    id: String,
    ok: bool,
    secs: f64,
    key: String,
    // Absent for ordinary results; explicitly identifies an inapplicable lane.
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
}

#[derive(Serialize)]
struct Evidence {
    v: u32,
    tree: String,
    host: String,
    soldr: &'static str,
    results: Vec<ResultRow>,
    autofixed: Vec<String>,
}

fn error(message: impl std::fmt::Display) -> SoldrError {
    SoldrError::Other(format!("lint attest: {message}"))
}

/// Read the nearest workspace, even when invoked from a member directory.
/// This deliberately avoids Cargo metadata, toolchain probes and compilers.
fn workspace(start: &Path) -> Result<(PathBuf, toml::Value), SoldrError> {
    for dir in start.ancestors() {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file() {
            let text = std::fs::read_to_string(&manifest).map_err(error)?;
            let value: toml::Value = toml::from_str(&text).map_err(error)?;
            if value.get("workspace").is_some() {
                return Ok((dir.to_path_buf(), value));
            }
        }
    }
    Err(error("a workspace Cargo.toml is required"))
}

fn parse(value: &toml::Value, host: &str) -> Result<(Vec<Work>, Vec<String>), SoldrError> {
    let metadata = value
        .get("workspace")
        .and_then(|v| v.get("metadata"))
        .and_then(|v| v.get("soldr"));
    let Some(table) = metadata.and_then(|v| v.get("attest")) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let declaration: Declaration = table.clone().try_into().map_err(error)?;
    if declaration.schema != 1 {
        return Err(error("attest.schema must be 1"));
    }
    let targets: Vec<String> = match metadata.and_then(|v| v.get("targets")) {
        Some(v) => v.clone().try_into().map_err(error)?,
        None => Vec::new(),
    };
    let targets: Vec<String> = targets
        .iter()
        .map(|target| {
            crate::target_alias::resolve_soldr_target_for_host(target, host)
                .map(|v| v.rust_triple)
                .map_err(|err| err.into_soldr_error(crate::target_alias::TargetSurface::Lint))
        })
        .collect::<Result<_, _>>()?;
    let mut warnings: Vec<String> = declaration
        .unknown
        .keys()
        .map(|key| format!("unknown attest key {key:?}"))
        .collect();
    let mut work = Vec::new();
    let mut ids = BTreeSet::new();
    for check in declaration.checks {
        warnings.extend(
            check
                .unknown
                .keys()
                .map(|key| format!("unknown attest check key {key:?} in {:?}", check.id)),
        );
        if check.id.is_empty()
            || check.id.chars().any(char::is_whitespace)
            || !matches!(check.step.as_str(), "fmt" | "clippy" | "dylint" | "test")
            || check.scope.as_deref().is_some_and(|v| v != "all-targets")
            || check.targets.as_deref().is_some_and(|v| v != "cross")
        {
            return Err(error(format!("malformed check {:?}", check.id)));
        }
        let target_id = check.id.contains("{target}");
        let host_id = check.id.contains("{host}");
        if target_id != (check.targets.as_deref() == Some("cross"))
            || (target_id && host_id)
            || (check.step == "fmt" && (target_id || check.scope.is_some()))
        {
            return Err(error(format!(
                "incompatible scope/targets in {:?}",
                check.id
            )));
        }
        let expanded = check
            .id
            .replace("{host}", host)
            .replace("{target}", "target");
        if expanded.contains(['{', '}']) {
            return Err(error(format!("unknown id placeholder in {:?}", check.id)));
        }
        let expansions: Vec<Option<&String>> = if target_id {
            targets.iter().map(Some).collect()
        } else {
            vec![None]
        };
        for target in expansions {
            let id = check
                .id
                .replace("{host}", host)
                .replace("{target}", target.map_or("", String::as_str));
            if !ids.insert(id.clone()) {
                return Err(error(format!("duplicate expanded check id {id:?}")));
            }
            // Cross compilation is possible anywhere; target tests require a native host.
            let hosts = match (check.step.as_str(), target, host_id) {
                ("test", Some(target), _) => vec![target.clone()],
                (_, _, true) => vec![host.to_owned()],
                _ => vec!["any".into()],
            };
            work.push(Work {
                check: ClientCheck { id, hosts },
                args: check_args(&check, target),
            });
        }
    }
    work.sort_by(|a, b| a.check.id.cmp(&b.check.id));
    Ok((work, warnings))
}

fn check_args(check: &Check, target: Option<&String>) -> Vec<String> {
    let mut args: Vec<String> = match check.step.as_str() {
        "fmt" => vec!["fmt", "--all", "--", "--check"],
        "clippy" => vec!["clippy", "--workspace"],
        "dylint" => vec!["dylint", "--all", "--", "--workspace"],
        "test" => vec!["nextest", "run", "--workspace"],
        _ => unreachable!("validated step"),
    }
    .into_iter()
    .map(str::to_owned)
    .collect();
    if check.scope.is_some() {
        args.push("--all-targets".into());
    }
    if let Some(target) = target {
        args.extend(["--target".into(), target.clone()]);
    }
    if check.step == "clippy" {
        args.extend(["--".into(), "-D".into(), "warnings".into()]);
    }
    args
}

fn load(start: &Path) -> Result<(PathBuf, Vec<Work>), SoldrError> {
    let (root, value) = workspace(start)?;
    let (work, warnings) = parse(&value, crate::pyo3_detect::host_triple())?;
    for warning in warnings {
        eprintln!("soldr: warning: {warning}");
    }
    validate_contract(&root, &work)?;
    Ok((root, work))
}

/// If a gate contract is present, fail drift before running any checks.
fn validate_contract(root: &Path, work: &[Work]) -> Result<(), SoldrError> {
    let path = root.join("ci-attestations.yml");
    if !path.is_file() {
        return Ok(());
    }
    let contract = std::fs::read_to_string(path).map_err(error)?;
    // Gate keys are the two-space-indented mapping directly under `gates:`.
    let mut gates = BTreeSet::new();
    let mut inside = false;
    for line in contract.lines() {
        if line == "gates:" {
            inside = true;
            continue;
        }
        if inside && !line.is_empty() && !line.starts_with([' ', '#']) {
            break;
        }
        if inside && line.starts_with("  ") && !line.starts_with("   ") {
            if let Some((id, _)) = line.trim().split_once(':') {
                gates.insert(id.trim_matches(['\"', '\'']).to_owned());
            }
        }
    }
    for item in work {
        if !gates.contains(&item.check.id) {
            return Err(error(format!(
                "check {:?} is absent from ci-attestations.yml gates",
                item.check.id
            )));
        }
    }
    Ok(())
}

fn client_work(work: &[Work]) -> Result<ClientWork, SoldrError> {
    let checks: Vec<ClientCheck> = work.iter().map(|w| w.check.clone()).collect();
    // Canonical bytes: sorted by id, struct field order id/hosts, compact JSON.
    let bytes = serde_json::to_vec(&checks).map_err(error)?;
    Ok(ClientWork {
        v: 1,
        kind: "client-work",
        checks,
        digest: hex::encode(Sha256::digest(bytes)),
    })
}

pub(crate) fn print_plan(args: &[String]) -> Result<i32, SoldrError> {
    if args != ["--format", "json"] && args != ["--format=json"] {
        eprintln!("soldr: lint plan requires --format json");
        return Ok(2);
    }
    let result = std::env::current_dir()
        .map_err(error)
        .and_then(|cwd| load(&cwd));
    match result {
        Ok((_, work)) => {
            println!(
                "{}",
                serde_json::to_string(&client_work(&work)?).map_err(error)?
            );
            Ok(0)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(2)
        }
    }
}

fn git(root: &Path, args: &[&str]) -> Result<String, SoldrError> {
    let mut command = Command::new("git");
    command.current_dir(root).args(args);
    let output = crate::core::command_output_with_timeout_duration(
        &mut command,
        "attestation git",
        std::time::Duration::from_secs(10),
    )?;
    if !output.status.success() {
        return Err(error(String::from_utf8_lossy(&output.stderr)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn clean_tree(root: &Path) -> Result<Option<String>, SoldrError> {
    if !git(root, &["status", "--porcelain=v1", "--untracked-files=all"])?.is_empty() {
        return Ok(None);
    }
    git(root, &["rev-parse", "HEAD^{tree}"]).map(Some)
}

// Resolve nonexistent tails through their nearest existing ancestor before any
// write, so both direct paths and symlinked paths inside a checkout are refused.
fn resolve_destination(path: &Path) -> Result<PathBuf, SoldrError> {
    if path.exists() {
        return path.canonicalize().map_err(error);
    }
    let parent = path
        .parent()
        .ok_or_else(|| error("invalid evidence path"))?;
    let name = path
        .file_name()
        .ok_or_else(|| error("invalid evidence path"))?;
    Ok(resolve_destination(parent)?.join(name))
}

fn evidence_dir(root: &Path) -> Result<PathBuf, SoldrError> {
    let home = match std::env::var_os("SOLDR_HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home),
        _ => SoldrPaths::new()?.root,
    };
    let home = if home.is_absolute() {
        home
    } else {
        std::env::current_dir().map_err(error)?.join(home)
    };
    let dir = resolve_destination(&home.join("attest"))?;
    let repo = PathBuf::from(git(root, &["rev-parse", "--show-toplevel"])?)
        .canonicalize()
        .map_err(error)?;
    if dir.starts_with(repo) {
        return Err(error("SOLDR_HOME/attest must be outside the repository"));
    }
    // Refuse storage in any other checkout as well.
    let existing = dir
        .ancestors()
        .find(|path| path.exists())
        .ok_or_else(|| error("invalid evidence path"))?;
    if git(existing, &["rev-parse", "--absolute-git-dir"]).is_ok() {
        return Err(error("SOLDR_HOME/attest must be outside every repository"));
    }
    std::fs::create_dir_all(&dir).map_err(error)?;
    Ok(dir)
}

fn aggregate(results: &[ResultRow], normal: i32) -> i32 {
    if normal != 0 && normal != 75 {
        return 1;
    }
    if results.iter().any(|r| !r.ok && r.exit_code != Some(75)) {
        return 1;
    }
    if !results.is_empty() && results.iter().all(|r| r.exit_code == Some(75)) {
        return 75;
    }
    0
}

pub(crate) async fn run(args: &[String], cache: bool, trust: bool) -> Result<i32, SoldrError> {
    let mut options = args.iter().skip(1).map(String::as_str);
    let mut valid = args.first().map(String::as_str) == Some("rust");
    while let Some(option) = options.next() {
        valid &= match option {
            "--attest" | "--check" | "--format=json" => true,
            "--format" => options.next() == Some("json"),
            _ => false,
        };
    }
    if !valid {
        eprintln!("soldr: --attest requires lint rust with canonical workspace scope");
        return Ok(2);
    }
    let cwd = std::env::current_dir().map_err(error)?;
    run_at(&cwd, cache, trust).await
}

async fn run_at(cwd: &Path, cache: bool, trust: bool) -> Result<i32, SoldrError> {
    let (root, work) = match load(cwd) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("{err}");
            return Ok(2);
        }
    };
    let Some(tree) = clean_tree(&root)? else {
        eprintln!("soldr: --attest requires a clean committed tree");
        return Ok(3);
    };
    let dir = match evidence_dir(&root) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("{err}");
            return Ok(2);
        }
    };
    let normal_steps = match crate::lint_cmd::attestation_plan(&root) {
        Ok(steps) => steps,
        Err(err) => {
            eprintln!("{err}");
            return Ok(2);
        }
    };
    let soldr = crate::current_soldr_binary()?;
    execute(
        &root,
        normal_steps,
        work,
        tree,
        &dir,
        &soldr,
        (cache, trust),
    )
}

fn offline_child(command: &mut Command) {
    command
        .env("CARGO_NET_OFFLINE", "true")
        .env("SOLDR_ATTEST_OFFLINE", "1")
        .env("SOLDR_NO_AUTO_COMPONENT", "1")
        .env("RUSTUP_DIST_SERVER", "file:///soldr-offline")
        .env("RUSTUP_UPDATE_ROOT", "file:///soldr-offline")
        .stdout(Stdio::from(std::io::stderr()))
        .stderr(Stdio::inherit());
}

fn execute(
    root: &Path,
    normal_steps: Vec<crate::lint_steps::PlannedStep>,
    work: Vec<Work>,
    tree: String,
    dir: &Path,
    soldr: &Path,
    flags: (bool, bool),
) -> Result<i32, SoldrError> {
    let (cache, trust) = flags;
    // Reuse the ordinary plan without recursively entering the lint command.
    // Every child uses stderr, leaving stdout for the single evidence object.
    let mut normal_code = 0;
    for step in normal_steps {
        let mut command = match step {
            crate::lint_steps::PlannedStep::Cargo { args, .. } => {
                crate::lint_steps::cargo_step_command(soldr, &args, cache, trust)
            }
            crate::lint_steps::PlannedStep::Extra(extra) => {
                crate::lint_steps::extra_command(&extra)
            }
        };
        command
            .current_dir(root)
            .env_remove("SOLDR_ATTEST_KEY_FILE");
        crate::cargo_front_door::porcelain_lint::scrub_child_env(&mut command);
        offline_child(&mut command);
        normal_code = wait(&mut command)?;
        if normal_code != 0 {
            break;
        }
    }
    let host = crate::pyo3_detect::host_triple();
    let mut results = Vec::new();
    for item in work {
        if !item.check.hosts.iter().any(|v| v == "any" || v == host) {
            results.push(ResultRow {
                id: item.check.id,
                ok: false,
                secs: 0.0,
                key: String::new(),
                exit_code: Some(75),
            });
            continue;
        }
        let key_file = tempfile::NamedTempFile::new_in(dir).map_err(error)?;
        let mut command = crate::lint_steps::cargo_step_command(soldr, &item.args, cache, trust);
        command
            .current_dir(root)
            .env("SOLDR_ATTEST_KEY_FILE", key_file.path());
        offline_child(&mut command);
        // No target installation here: offline evidence uses existing toolchains.
        let start = Instant::now();
        let code = wait(&mut command)?;
        let key = std::fs::read_to_string(key_file.path()).map_err(error)?;
        let code = if code == 0 && key.is_empty() {
            eprintln!(
                "soldr: no verdict key for {:?}; evidence cannot claim a pass",
                item.check.id
            );
            1
        } else {
            code
        };
        results.push(ResultRow {
            id: item.check.id,
            ok: code == 0,
            secs: start.elapsed().as_secs_f64(),
            key,
            exit_code: (code == 75).then_some(75),
        });
    }
    if clean_tree(root)?.as_deref() != Some(&tree) {
        eprintln!("soldr: tree changed during attestation");
        return Ok(3);
    }
    let code = aggregate(&results, normal_code);
    let evidence = Evidence {
        v: 1,
        tree: tree.clone(),
        host: host.into(),
        soldr: env!("CARGO_PKG_VERSION"),
        results,
        autofixed: Vec::new(),
    };
    let json = serde_json::to_string(&evidence).map_err(error)?;
    let mut temp = tempfile::NamedTempFile::new_in(dir).map_err(error)?;
    use std::io::Write;
    writeln!(temp, "{json}").map_err(error)?;
    temp.persist(dir.join(format!("{tree}.json")))
        .map_err(error)?;
    println!("{json}");
    Ok(code)
}

fn wait(command: &mut Command) -> Result<i32, SoldrError> {
    crate::exit_guard::mark_spoke();
    let mut child = crate::lint_steps::spawn_guarded(command).map_err(error)?;
    crate::platform::wait::wait_for(&mut child, crate::platform::wait::WHOLE_COMMAND)
        .map(|status| status.code().unwrap_or(1))
        .map_err(error)
}

#[cfg(test)]
#[path = "lint_attest_tests.rs"]
mod tests;
