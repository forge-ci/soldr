use super::*;

#[tokio::test]
async fn finished_output_survives_a_disconnected_stream() {
    let root = tempfile::tempdir().unwrap();
    retention::prepare(root.path()).unwrap();
    let connection = retention::Connection::default();
    let id = "a".repeat(32);
    let archive = connection.allocate(root.path(), &id).unwrap();
    let output = archive.path().join("output");
    fs::create_dir(&output).unwrap();
    let (start, wait) = tokio::sync::oneshot::channel();
    let (done, finished) = tokio::sync::oneshot::channel();
    let session = id.clone();
    let response = control::stream_run(move |sink| {
        wait.blocking_recv().unwrap();
        let transcript = Transcript::new(archive.path(), Some(sink))?;
        let result = ResultFrame {
            attestation: b"{}".to_vec(),
            ..ResultFrame::default()
        };
        let publication = publish_result(&archive, &result, &transcript, &session, None, 0);
        if publication.is_err() {
            archive.discard();
        }
        let _ = done.send(publication);
        Ok(())
    });
    drop(response);
    start.send(()).unwrap();
    finished.await.unwrap().unwrap();
    let retained = finished_output(root.path(), &id, &connection).unwrap();
    assert_eq!(
        fs::read(retained.path.join("attestation.json")).unwrap(),
        b"{}"
    );
    assert!(retained.path.join("stdout-stderr.jsonl").is_file());
}

#[test]
fn a_failed_session_is_distinct_from_a_pending_transcript() {
    let root = tempfile::tempdir().unwrap();
    let id = "a".repeat(32);
    let session = root.path().join("sessions").join(&id);
    fs::create_dir_all(&session).unwrap();
    let connection = retention::Connection::default();
    let readiness = || {
        finished_output(root.path(), &id, &connection)
            .err()
            .unwrap()
    };
    assert_eq!(readiness().kind(), io::ErrorKind::NotFound);
    fs::File::create_new(session.join("started")).unwrap();
    assert_eq!(readiness().kind(), io::ErrorKind::Other);
    retention::prepare(root.path()).unwrap();
    let _active_output = connection.allocate(root.path(), &id).unwrap();
    assert_eq!(readiness().kind(), io::ErrorKind::NotFound);
    publish_session_failure(root.path(), &id, &io::Error::other("build stopped")).unwrap();
    assert_eq!(readiness().to_string(), "build stopped");
}

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
fn remote_work_directory_requires_a_relative_worktree_path() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("sessions")).unwrap();
    let mut request = Start {
        commit: "a".repeat(40),
        working_tree: true,
        work_dir: Some("crates/cli".into()),
        ..Start::default()
    };
    assert!(start(root.path(), request.clone(), &SessionAccess::default()).is_ok());
    request.working_tree = false;
    assert!(start(root.path(), request.clone(), &SessionAccess::default()).is_err());
    request.working_tree = true;
    for path in ["../outside", "/outside", "crates/../cli"] {
        request.work_dir = Some(path.into());
        assert!(start(root.path(), request.clone(), &SessionAccess::default()).is_err());
    }
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
