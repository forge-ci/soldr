use super::*;

#[test]
fn remote_worktree_session_acknowledges_snapshot_without_a_manifest() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("sessions")).unwrap();
    let mut request = Start {
        commit: "a".repeat(40),
        working_tree: true,
        ..Start::default()
    };
    let result = start(root.path(), request.clone(), &SessionAccess::default()).unwrap();
    let session: Session = wire::decode(&result).unwrap();
    assert!(session.working_tree_accepted);
    assert!(!session.manifest_accepted);
    request.manifest_json = Some(b"null".to_vec());
    assert!(start(root.path(), request, &SessionAccess::default()).is_err());
}

#[test]
fn every_session_persists_its_manifest_before_source_upload() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("sessions")).unwrap();
    let commit = "a".repeat(40);
    let populated = serde_json::json!({
        "github": {"url": "https://github.com/forge-ci/soldr"},
        "user_id": 1234,
        "parent-commit-hash": "b".repeat(40),
        "last-commit-hash": commit,
    });
    for (value, accepted) in [(serde_json::Value::Null, false), (populated, true)] {
        let response = start(
            root.path(),
            Start {
                commit: commit.clone(),
                manifest_json: accepted.then(|| serde_json::to_vec(&value).unwrap()),
                ..Start::default()
            },
            &SessionAccess::default(),
        )
        .unwrap();
        let session: Session = wire::decode(&response).unwrap();
        assert_eq!(session.manifest_accepted, accepted);
        let path = root
            .path()
            .join("sessions")
            .join(session.id)
            .join("manifest.json");
        let stored: serde_json::Value =
            serde_json::from_reader(fs::File::open(path).unwrap()).unwrap();
        assert_eq!(stored, value);
    }
}
