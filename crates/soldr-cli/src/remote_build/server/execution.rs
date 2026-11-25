use std::{
    fs, io,
    path::{Path, PathBuf},
};

use prost::Message;

use crate::attestation_stats::WorkerTimer;

use super::{
    compile_args, lock_session, manifest, prepare_sources, relative_path, runner_soldr, runtime,
    slots, test_outputs, Action, Start, Transcript,
};

pub(super) struct PreparedRun {
    pub(super) request: Start,
    pub(super) runner_lock: fs::File,
    pub(super) cache_root: PathBuf,
    pub(super) manifest: Option<manifest::Manifest>,
    pub(super) lease: Option<slots::Lease>,
    pub(super) source: PathBuf,
    pub(super) build_output: PathBuf,
    pub(super) changes: Option<slots::OutputChanges>,
}

pub(super) fn prepare_run(
    root: &Path,
    session: &Path,
    output: &Path,
    id: &str,
    slot_limit: Option<u32>,
    timer: &mut WorkerTimer,
) -> io::Result<PreparedRun> {
    let request = Start::decode(fs::read(session.join("start.pb"))?.as_slice())?;
    let runner_lock = lock_session(root, session, &request)?;
    timer.locked();
    let manifest = manifest::decode(request.manifest_json.as_deref(), &request.commit)?;
    let lease = manifest
        .as_ref()
        .filter(|_| slot_limit != Some(0))
        .map(|manifest| slots::Lease::acquire(root, id, manifest, slot_limit))
        .transpose()?;
    let workspace = lease
        .as_ref()
        .map_or(session, |lease| lease.workspace.as_path());
    let (source, build_output) =
        prepare_sources(root, session, workspace, output, &request, lease.is_some())?;
    let changes = lease
        .as_ref()
        .map(|lease| slots::OutputChanges::capture(&lease.workspace, &request))
        .transpose()?;
    Ok(PreparedRun {
        request,
        runner_lock,
        cache_root: root.join("compiler"),
        manifest,
        lease,
        source,
        build_output,
        changes,
    })
}

pub(super) fn execute_action(
    soldr: &Path,
    cache_root: &Path,
    source: &Path,
    build_output: &Path,
    request: &Start,
    transcript: &Transcript,
    runtime_requests: &Path,
) -> io::Result<(std::process::Output, &'static str, bool)> {
    let action = Action::try_from(request.action).map_err(io::Error::other)?;
    let mut arguments = compile_args(action, &request.args, build_output)?;
    if action == Action::CargoTestRuntime {
        if transcript.sink.is_none() || !request.working_tree {
            return Err(io::Error::other(
                "test runtime requires a streamed worktree build",
            ));
        }
        runtime::configure_runner(&mut arguments, soldr, source, runtime_requests)?;
    }
    let full_lint = action == Action::Lint && arguments == ["rust", "--check"];
    let action = action.worker_command(&mut arguments);
    let directory = match &request.work_dir {
        Some(directory) => {
            let directory = fs::canonicalize(source.join(relative_path(directory)?))?;
            if !directory.starts_with(fs::canonicalize(source)?) || !directory.is_dir() {
                return Err(io::Error::other("work directory is outside the snapshot"));
            }
            directory
        }
        None => source.to_path_buf(),
    };
    let target = build_output.join("target");
    let command = || {
        let mut command = runner_soldr(soldr, cache_root);
        command
            .current_dir(&directory)
            .env("CARGO_TARGET_DIR", &target)
            .arg(action);
        command
    };
    let result = transcript.run(command().args(&arguments))?;
    if result.status.success() && test_outputs::requested(request) {
        test_outputs::save(
            transcript,
            source,
            &target,
            build_output,
            command(),
            &arguments,
        )?;
    }
    Ok((result, action, full_lint))
}
