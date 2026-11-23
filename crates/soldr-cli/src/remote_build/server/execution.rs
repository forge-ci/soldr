use std::{fs, io, path::Path};

use super::{
    compile_args, relative_path, runner_soldr, runtime, test_outputs, Action, Start, Transcript,
};

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
