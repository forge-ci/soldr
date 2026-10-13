use super::{classify, probe_with};
use std::path::Path;

#[test]
fn a_target_inside_another_checkout_is_shared() {
    let probe = classify(
        Path::new("/repo/.worktrees/agent-1"),
        Path::new("/repo/target"),
        Some(Path::new("/repo")),
    );
    assert_eq!(probe.shared_with.as_deref(), Some(Path::new("/repo")));
}

#[test]
fn a_target_inside_this_checkout_is_not_shared() {
    let probe = classify(
        Path::new("/repo"),
        Path::new("/repo/target"),
        Some(Path::new("/repo")),
    );
    assert_eq!(probe.shared_with, None);
}

#[test]
fn a_target_outside_every_checkout_is_not_shared() {
    let probe = classify(Path::new("/repo"), Path::new("/cache/targets/repo"), None);
    assert_eq!(probe.shared_with, None);
}

#[test]
fn a_nested_worktree_inheriting_the_parent_target_dir_is_detected() {
    // The soldr#3092 shape: a worktree nested under the main checkout with no
    // config of its own picks up the parent's `[build] target-dir`.
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("repo");
    let nested = parent.join(".worktrees").join("agent-1");
    std::fs::create_dir_all(parent.join(".git")).unwrap();
    std::fs::create_dir_all(parent.join(".cargo")).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join(".git"),
        "gitdir: ../../.git/worktrees/agent-1\n",
    )
    .unwrap();
    let shared = parent.join("shared-target");
    std::fs::write(
        parent.join(".cargo").join("config.toml"),
        format!("[build]\ntarget-dir = {:?}\n", shared.display().to_string()),
    )
    .unwrap();

    let inputs = crate::core::cargo_target_dir::CargoTargetDirInputs {
        cwd: nested.clone(),
        ..Default::default()
    };
    let found = probe_with(&inputs).expect("inside a checkout");
    assert_eq!(found.worktree_root, nested);
    assert_eq!(found.shared_with.as_deref(), Some(parent.as_path()));
}
