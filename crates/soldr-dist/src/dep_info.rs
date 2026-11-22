//! Validate the compiler's observed dependencies before any remote output is published.
use crate::{sandbox, DistJob, Result};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub(crate) struct InvalidProof(pub String);
impl std::fmt::Display for InvalidProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for InvalidProof {}

pub fn paths(text: &str, cwd: &Path) -> Result<BTreeSet<PathBuf>> {
    let text = text.replace("\\\r\n", " ").replace("\\\n", " ");
    let mut paths = BTreeSet::new();
    let mut rules = 0;
    for line in text.lines().filter(|line| !line.starts_with('#')) {
        let Some((_, dependencies)) = line.split_once(": ") else {
            continue;
        };
        rules += 1;
        let (mut word, mut escaped) = (String::new(), false);
        for c in dependencies.chars().chain(std::iter::once(' ')) {
            if escaped {
                word.push(c);
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c.is_whitespace() {
                if !word.is_empty() {
                    paths.insert(sandbox::absolute(cwd, Path::new(&word))?);
                    word.clear();
                }
            } else {
                word.push(c);
            }
        }
        if escaped || !word.is_empty() {
            return Err("unterminated dep-info escape".into());
        }
    }
    if rules == 0 {
        return Err("missing Rust dependency proof".into());
    }
    Ok(paths)
}

pub fn validate(job: &DistJob, text: &str) -> Result<()> {
    let cwd = Path::new(&job.cwd);
    let shipped = job
        .inputs
        .iter()
        .map(|i| sandbox::absolute(cwd, Path::new(&i.abs_path)))
        .collect::<Result<BTreeSet<_>>>()?;
    let observed = paths(text, cwd)?;
    if observed.is_empty() && job.stdin.is_empty() {
        return Err("missing Rust dependency proof inputs".into());
    }
    for path in observed {
        if !shipped.contains(&path) {
            return Err(format!("dep-info path outside shipped inputs: {}", path.display()).into());
        }
    }
    for line in text.lines().filter_map(|l| l.strip_prefix("# env-dep:")) {
        let (key, value) = line
            .split_once('=')
            .map_or((line, None), |(k, v)| (k, Some(v)));
        if job.env.get(key).map(String::as_str) != value {
            return Err(format!("dep-info environment outside shipped values: {key}").into());
        }
    }
    Ok(())
}

/// A failed proof is a reported decline before fetching or publishing any output.
pub(crate) fn decline(
    job: &DistJob,
    done: &crate::proto::Done,
    worker: &str,
) -> Option<crate::DistOutcome> {
    if job.compiler_kind != "rustc" {
        return None;
    }
    let result = std::str::from_utf8(&done.dependency_info)
        .map_err(|_| "non-UTF8 Rust dependency proof".into())
        .and_then(|text| validate(job, text));
    let error = result.err()?;
    let reason = format!("Rust input backstop: {error}");
    tracing::warn!(%worker, %reason, "discarding remote outputs");
    Some(crate::DistOutcome::Declined {
        reason,
        metrics: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escaped_paths_are_checked_before_output_publication() {
        let job = DistJob {
            cwd: "/package".into(),
            inputs: vec![crate::InputFile {
                abs_path: "/package/src/a b.rs".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(validate(&job, "a.o: src/a\\ b.rs\n").is_ok());
        assert!(validate(&job, "a.o: src/a\\ b.rs ../outside.rs\n")
            .unwrap_err()
            .to_string()
            .contains("outside shipped inputs"));
        assert!(validate(&job, "a.o: src/a\\ b.rs\n# env-dep:MISSING=unexpected\n").is_err());
        assert!(validate(&job, "").is_err());
    }
}
