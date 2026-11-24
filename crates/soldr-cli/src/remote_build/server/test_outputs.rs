use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Action, Start, Transcript};

const TEST_LIST: &str = "test-executables.json";

pub(super) fn requested(request: &Start) -> bool {
    request.working_tree
        && request.action == Action::CargoTestCompile as i32
        && request
            .args
            .iter()
            .any(|arg| arg == "--message-format=json")
}

pub(super) fn save(
    transcript: &Transcript,
    source: &Path,
    target: &Path,
    output: &Path,
    mut listing: std::process::Command,
    arguments: &[String],
) -> io::Result<()> {
    transcript.writer.lock().flush()?;
    let file = fs::File::open(transcript.file.path())?;
    let entries = BufReader::new(file)
        .lines()
        .filter_map(|line| match line {
            Ok(line) => read_entry(&line, source, target).transpose(),
            Err(error) => Some(Err(error)),
        })
        .collect::<io::Result<Vec<_>>>()?;
    // Cargo emits compiler artifacts as they finish. Its human-readable
    // no-run listing is ordered exactly as Cargo would start the tests.
    listing.args(
        arguments
            .iter()
            .filter(|arg| *arg != "--message-format=json"),
    );
    listing
        .env("CARGO_TERM_COLOR", "never")
        .env("SOLDR_TIMESTAMP_LINES", "0");
    let result =
        crate::platform::wait::output_for(&mut listing, crate::platform::wait::WHOLE_COMMAND)?;
    if !result.status.success() {
        return Err(io::Error::other(format!(
            "could not list compiled Forge CI tests: {}",
            String::from_utf8_lossy(&result.stderr).trim(),
        )));
    }
    let entries = order_tests(entries, target, &String::from_utf8_lossy(&result.stderr))?;
    fs::write(output.join(TEST_LIST), serde_json::to_vec(&entries)?)
}

fn order_tests(
    entries: Vec<TestExecutable>,
    target: &Path,
    listing: &str,
) -> io::Result<Vec<TestExecutable>> {
    let mut by_path: BTreeMap<_, _> = entries
        .into_iter()
        .map(|entry| {
            (
                target.parent().unwrap_or(target).join(&entry.executable),
                entry,
            )
        })
        .collect();
    let mut ordered = Vec::with_capacity(by_path.len());
    for line in listing
        .lines()
        .filter(|line| line.trim_start().starts_with("Executable "))
    {
        let Some(line) = line.strip_suffix(')') else {
            continue;
        };
        let entry = line
            .match_indices(" (")
            .find_map(|(offset, prefix)| by_path.remove(Path::new(&line[offset + prefix.len()..])));
        let entry = entry.ok_or_else(|| {
            io::Error::other("Cargo listed a test executable missing from the compile inventory")
        })?;
        ordered.push(entry);
    }
    if !by_path.is_empty() {
        return Err(io::Error::other(
            "Cargo did not list every compiled test executable",
        ));
    }
    Ok(ordered)
}

#[derive(Deserialize)]
struct Record {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    v: String,
}

#[derive(Deserialize)]
struct CargoArtifact {
    reason: String,
    executable: Option<PathBuf>,
    manifest_path: Option<PathBuf>,
    profile: Option<CargoProfile>,
}

#[derive(Deserialize)]
struct CargoProfile {
    test: bool,
}

#[derive(Serialize)]
struct TestExecutable {
    executable: String,
    manifest: String,
}

fn read_entry(line: &str, source: &Path, target: &Path) -> io::Result<Option<TestExecutable>> {
    let record: Record = serde_json::from_str(line)?;
    let json = record.v.trim();
    if record.kind != "o" || !json.starts_with('{') {
        return Ok(None);
    }
    let artifact: CargoArtifact = match serde_json::from_str(json) {
        Ok(artifact) => artifact,
        Err(_) => return Ok(None),
    };
    if artifact.reason != "compiler-artifact" || !artifact.profile.is_some_and(|p| p.test) {
        return Ok(None);
    }
    let (Some(executable), Some(manifest)) = (artifact.executable, artifact.manifest_path) else {
        return Ok(None);
    };
    Ok(Some(TestExecutable {
        executable: format!("target/{}", under(&executable, target)?),
        manifest: under(&manifest, source)?,
    }))
}

fn under(path: &Path, root: &Path) -> io::Result<String> {
    let path = path
        .strip_prefix(root)
        .ok()
        .and_then(Path::to_str)
        .ok_or_else(|| io::Error::other("Cargo reported a test path outside the build"))?
        .replace(std::path::MAIN_SEPARATOR, "/");
    super::super::snapshot::relative_path(&path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inventory_uses_cargo_listing_order() {
        let entries = ["integration", "unit"]
            .map(|name| TestExecutable {
                executable: format!("target/debug/deps/{name}"),
                manifest: "Cargo.toml".into(),
            })
            .into();
        let target = Path::new("build (test)/target");
        let listing = "  Executable unittests src/main.rs (build (test)/target/debug/deps/unit)\n\
                       Executable tests/integration.rs (build (test)/target/debug/deps/integration)\n";
        let ordered = order_tests(entries, target, listing).unwrap();
        assert_eq!(ordered[0].executable, "target/debug/deps/unit");
        assert_eq!(ordered[1].executable, "target/debug/deps/integration");
    }
}
