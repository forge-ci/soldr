//! Keep Windows startup failure ownership visible on every CI host.

#[test]
fn contained_windows_spawn_owns_resources_before_fallible_setup() {
    let source = include_str!("../../src/platform_win/sync_spawn.rs");
    let start = source.find("pub fn spawn_sync(").unwrap();
    let end = source[start..]
        .find("/// Private containment stages")
        .unwrap()
        + start;
    let spawn = &source[start..end];
    let job = spawn.find("let job = create_job_object()?").unwrap();
    let create = spawn.find("create_process_inner(").unwrap();
    assert!(
        job < create,
        "job creation failure must not strand a suspended child"
    );
    assert!(spawn.contains("let process = OwnedHandle(process);"));
    assert!(spawn.contains("let thread = OwnedHandle(thread);"));
    assert!(spawn.contains("resume_contained_thread(&thread)"));
    assert!(
        !spawn.contains("ResumeThread(thread);"),
        "resume failure must be checked"
    );
}
