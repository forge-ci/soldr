//! `soldr verdict ...` (soldr#3895).
//!
//! ```text
//! soldr verdict key [--json] -- <cargo args>      key for an invocation here
//! soldr verdict explain -- <cargo args>           inputs, key, hit or miss
//! soldr verdict rekey [--json] <seen.jsonl>       re-derive keys for a prior
//!                                                 job's invocations (setup-soldr)
//! soldr verdict ls [--json]                       stored verdicts
//! soldr verdict gc [--max N]                      keep the newest N
//! soldr verdict paths [--json]                    store layout
//! soldr verdict profiles [--json]                 the effective adapters
//! ```

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;

use super::context::Context;
use super::store::{Seen, Store, DEFAULT_MAX_RECORDS};
use crate::core::SoldrError;

const USAGE: &str =
    "usage: soldr verdict <key|explain|rekey|ls|gc|paths|profiles> [--json] [-- <cargo args>]";

/// Exit code for "this invocation would run, not skip".
const EXIT_NO_SKIP: i32 = 2;

pub fn run(args: &[String]) -> Result<i32, SoldrError> {
    let Some((sub, rest)) = args.split_first() else {
        eprintln!("{USAGE}");
        return Ok(2);
    };
    let (flags, cargo_args) = match rest.iter().position(|a| a == "--") {
        Some(index) => (&rest[..index], &rest[index + 1..]),
        None => (rest, &[][..]),
    };
    let json = flags.iter().any(|f| f == "--json");
    let store = Store::default_location()
        .ok_or_else(|| SoldrError::Other("cannot locate the soldr root".into()))?;
    match sub.as_str() {
        "key" => key(cargo_args, json),
        "explain" => explain(cargo_args, &store),
        "rekey" => rekey(flags, json),
        "ls" => ls(&store, json),
        "gc" => gc(flags, &store),
        "paths" => paths(&store, json),
        "profiles" => profiles(json),
        _ => {
            eprintln!("{USAGE}");
            Ok(2)
        }
    }
}

fn current_dir() -> Result<std::path::PathBuf, SoldrError> {
    Ok(std::env::current_dir()?)
}

fn key(cargo_args: &[String], json: bool) -> Result<i32, SoldrError> {
    let cwd = current_dir()?;
    let result = Context::discover(&cwd).and_then(|ctx| ctx.derive(cargo_args, &cwd));
    match (result, json) {
        (Ok(derivation), false) => {
            println!("{}", derivation.key);
            Ok(0)
        }
        (Ok(derivation), true) => {
            println!(
                "{}",
                serde_json::json!({ "key": derivation.key, "tool": derivation.tool })
            );
            Ok(0)
        }
        (Err(reason), false) => {
            eprintln!("no skip: {reason}");
            Ok(EXIT_NO_SKIP)
        }
        (Err(reason), true) => {
            println!("{}", serde_json::json!({ "key": null, "reason": reason }));
            Ok(EXIT_NO_SKIP)
        }
    }
}

fn explain(cargo_args: &[String], store: &Store) -> Result<i32, SoldrError> {
    let cwd = current_dir()?;
    let derivation = match Context::discover(&cwd).and_then(|ctx| ctx.derive(cargo_args, &cwd)) {
        Ok(derivation) => derivation,
        Err(reason) => {
            println!("runs every time: {reason}");
            return Ok(EXIT_NO_SKIP);
        }
    };
    println!("tool       {}", derivation.tool);
    println!("key        {}", derivation.key);
    println!(
        "files      {} tracked, {} uncommitted",
        derivation.files, derivation.dirty
    );
    for (label, value) in &derivation.components {
        println!("  {label:<22} {value}");
    }
    match store.lookup(&derivation.key) {
        Some(record) => println!(
            "verdict    HIT (recorded by soldr {} at {})",
            record.soldr, record.created
        ),
        None => println!("verdict    miss: the check will run and record a pass"),
    }
    Ok(0)
}

#[derive(Serialize)]
struct Rekeyed {
    tool: String,
    args: Vec<String>,
    cwd: String,
    key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// Re-derive the keys of the invocations a previous job logged. Used by a
/// remote tier to fetch exactly the verdicts the coming job can use.
fn rekey(flags: &[String], json: bool) -> Result<i32, SoldrError> {
    let Some(file) = flags.iter().find(|f| !f.starts_with("--")) else {
        eprintln!("usage: soldr verdict rekey [--json] <seen.jsonl>");
        return Ok(2);
    };
    let text = std::fs::read_to_string(file)?;
    let cwd = current_dir()?;
    let context = Context::discover(&cwd);
    let mut done = BTreeSet::new();
    for line in text.lines() {
        let Ok(seen) = serde_json::from_str::<Seen>(line) else {
            continue;
        };
        if seen.tool.is_empty() {
            // An ungated run: report it so the caller knows the job needs
            // its toolchain and caches.
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "tool": null, "args": seen.args, "ungated": true })
                );
            }
            continue;
        }
        if !done.insert((seen.args.clone(), seen.cwd.clone())) {
            continue;
        }
        let row = rekey_one(&context, &seen);
        if json {
            println!("{}", serde_json::to_string(&row).unwrap_or_default());
        } else if let Some(key) = &row.key {
            println!("{key}");
        }
    }
    Ok(0)
}

fn rekey_one(context: &Result<Context, String>, seen: &Seen) -> Rekeyed {
    let derived = match context {
        Ok(ctx) => {
            let dir = if seen.cwd == "." {
                ctx.snapshot.root.clone()
            } else {
                ctx.snapshot.root.join(Path::new(&seen.cwd))
            };
            ctx.derive(&seen.args, &dir)
        }
        Err(reason) => Err(reason.clone()),
    };
    let (key, reason) = match derived {
        Ok(d) => (Some(d.key), None),
        Err(reason) => (None, Some(reason)),
    };
    Rekeyed {
        tool: seen.tool.clone(),
        args: seen.args.clone(),
        cwd: seen.cwd.clone(),
        key,
        reason,
    }
}

fn ls(store: &Store, json: bool) -> Result<i32, SoldrError> {
    for (_, record) in store.list() {
        if json {
            println!("{}", serde_json::to_string(&record).unwrap_or_default());
        } else {
            println!("{}  {}  {}", record.key, record.cwd, record.argv.join(" "));
        }
    }
    Ok(0)
}

fn gc(flags: &[String], store: &Store) -> Result<i32, SoldrError> {
    let max = flags
        .iter()
        .position(|f| f == "--max")
        .and_then(|i| flags.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_RECORDS);
    println!("removed {} verdict(s)", store.gc(max));
    Ok(0)
}

fn paths(store: &Store, json: bool) -> Result<i32, SoldrError> {
    let rows = [
        ("root", store.root().to_path_buf()),
        ("records", store.records_dir()),
        ("spool", store.spool_dir()),
        ("seen", store.seen_path()),
    ];
    if json {
        let map: serde_json::Map<String, serde_json::Value> = rows
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.display().to_string().into()))
            .collect();
        println!("{}", serde_json::Value::Object(map));
    } else {
        for (label, path) in rows {
            println!("{label:<8} {}", path.display());
        }
    }
    Ok(0)
}

fn profiles(json: bool) -> Result<i32, SoldrError> {
    let cwd = current_dir()?;
    let root = cwd
        .ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(&cwd)
        .to_path_buf();
    let config = super::profiles::workspace_config(&super::workspace_root(&cwd, &root));
    for profile in super::profiles::effective(&config) {
        if json {
            println!("{}", serde_json::to_string(&profile).unwrap_or_default());
        } else {
            let state = if profile.pure {
                ""
            } else {
                "  (impure: never skipped)"
            };
            println!("{:<10} cargo {}{state}", profile.name, profile.subcommand);
        }
    }
    Ok(0)
}
