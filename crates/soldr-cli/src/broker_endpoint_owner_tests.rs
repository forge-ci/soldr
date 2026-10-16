use super::*;

/// soldr#3815: a successor that rebinds the path orphans the first broker,
/// which must notice, and must not unlink the successor's socket on exit.
#[test]
fn rebinding_the_path_orphans_the_first_claim_and_spares_the_successor() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        // Named pipes have no file to rebind; the claim is always owned there.
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let endpoint = dir.path().join("broker.sock");
    let endpoint_text = endpoint.to_string_lossy().into_owned();

    std::fs::write(&endpoint, b"first").expect("first socket file");
    let first = EndpointClaim::capture(&endpoint_text);
    assert!(first.still_owned());

    // A successor recovers the path: unlink, then bind a new file.
    std::fs::remove_file(&endpoint).expect("unlink");
    std::fs::write(&endpoint, b"second").expect("successor socket file");
    let second = EndpointClaim::capture(&endpoint_text);

    assert!(!first.still_owned(), "the first broker is now orphaned");
    assert!(second.still_owned());
    assert!(first.orphaned_message().contains("orphaned"));

    first.retire();
    assert!(
        endpoint.exists(),
        "an orphan must not unlink its successor's socket"
    );
    second.retire();
    assert!(!endpoint.exists(), "the owner retires its own socket");
}

#[test]
fn a_missing_endpoint_is_never_owned() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let claim = EndpointClaim::capture(&dir.path().join("absent.sock").to_string_lossy());
    assert!(!claim.still_owned());
}
