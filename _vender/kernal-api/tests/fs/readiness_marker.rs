use std::io::Write;

#[test]
fn marker_is_invisible_until_payload_is_complete() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ready.marker");
    crate::marker_support::publish_with(&path, |file| {
        file.write_all(b"ready:")?;
        assert!(
            !path.exists(),
            "reader can observe a partial readiness marker"
        );
        file.write_all(b"123")
    })
    .unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "ready:123");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn failed_marker_write_is_cleaned_up_and_existing_marker_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ready.marker");
    assert!(crate::marker_support::publish_with(&path, |file| {
        file.write_all(b"partial")?;
        Err(std::io::Error::other("fixture write failure"))
    })
    .is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    crate::marker_support::publish(&path, "ready").unwrap();
    assert!(crate::marker_support::publish(&path, "overwrite").is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "ready");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}
